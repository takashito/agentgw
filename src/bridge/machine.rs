//! The machine-side connection — dials **outward** to the gateway and receives events for the channels it handles.
//! Also home to the listener (`GatewayInlet`) for setups where the gateway dials the machine.
//!
//! Dialing outward is the point: no port forwarding on a home router and no VPN needed (it connects from
//! café Wi-Fi or tethering too).
//!
//! > **When the connection drops, agents are never touched.**
//! > This module is deliberately **not given** a handle to the agents. A dropped agent
//! > liveness signal really does mean "that agent died", but bringing that reflex in
//! > here would let a 2-second Wi-Fi blink kill agents that are alive and working.
//! > Losing the gateway causes only this: **no new Slack messages until it comes back.**
//! > What is running keeps running, and the link reconnects underneath.
//!
//! Refusals that talking can't fix (wrong key, wrong version) are **reported loudly**, but
//! **the process does not exit** — if it did, the supervisor would restart it at once, the rapid restarts would hit
//! systemd's start rate limit (default 5 in 10 seconds) and leave the unit `failed`, and a momentary
//! 401 during a deploy would keep the machine down for good. Even for an unfixable refusal it keeps
//! reconnecting at intervals, and recovers on its own the moment it is fixed.

use crate::bridge::gateway::wire::{self, LinkFrame};
use crate::log::LogCtx;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::routing::get;

use crate::bridge::gateway;

pub const RECONNECT_MIN_MS: u64 = 1_000;
pub const RECONNECT_MAX_MS: u64 = 30_000;

/// After this much silence, send one Ping. If the next window is silent too, treat it as dead and disconnect.
pub const LINK_IDLE_MS: u64 = 30_000;

/// What to do when a silent window closes.
#[derive(Debug, PartialEq, Eq)]
pub enum Idle {
    /// Check that it's alive.
    Ping,
    /// The previous Ping got no answer. Disconnect and reconnect.
    Dead,
}

/// The watchman checking the link isn't dead. **The only way to detect half-open.**
///
/// When the peer vanishes without a FIN (network loss, suspend, a dropped NAT entry), the socket stays
/// `ESTAB` and receiving **never returns**. No Close and no Err arrive, so no receive loop can
/// exit, and the reconnect beyond it is never reached (2026-08-03: a machine went silent for 74 minutes).
/// For WebSocket Ping, both tungstenite and axum **only answer received Pings with Pong** and never
/// send their own. TCP keepalive is off by default too. So the only way is to poke from our side.
#[derive(Default)]
pub struct IdleWatch {
    pinged: bool,
}

impl IdleWatch {
    /// Something arrived / was sent. Alive.
    pub fn on_traffic(&mut self) {
        self.pinged = false;
    }

    /// The window closed with no traffic.
    pub fn on_idle(&mut self) -> Idle {
        if self.pinged {
            Idle::Dead
        } else {
            self.pinged = true;
            Idle::Ping
        }
    }
}

/// The result of reading one frame from the link. Returned by the implementor of [`LinkRead`].
pub enum Frame {
    /// A payload.
    Text(String),
    /// Not a payload (Pong etc.). Its content is irrelevant, but **the fact it arrived** proves liveness.
    Other,
    /// The peer closed / broke. The reason is a line for humans.
    Closed(String),
}

/// A reader of one frame. **No timeout here** — the watchman ([`beat`]) wraps it from outside.
///
/// axum and tungstenite differ only in the name of their read method, so implementations just bridge that.
/// **The watch clock and state machine exist only once, in [`beat`]** (to prevent a repeat of when they were copied to 4 places).
pub trait LinkRead {
    fn read_frame(&mut self) -> impl std::future::Future<Output = Frame> + Send;
}

/// The result of a watched receive.
///
/// **Don't swallow `Ping`.** Ignoring it means never noticing a link whose peer silently vanished
/// (half-open). Exhaustive matching enforces that.
pub enum Beat {
    /// A payload arrived.
    Text(String),
    /// Something other than a payload arrived. Alive.
    Alive,
    /// Silence continued. **The caller sends one Ping and waits for the next.**
    Ping,
    /// Disconnected, with a reason. The caller leaves the loop.
    Gone(String),
}

/// **Every link receive goes through this.** Waiting on a bare `next()` / `recv()` directly
/// never returns when the peer vanishes without a FIN, since neither Close nor Err arrives
/// (2026-08-03: a machine went silent for 74 minutes).
pub async fn beat<S: LinkRead>(socket: &mut S, watch: &mut IdleWatch) -> Beat {
    beat_within(socket, watch, LINK_IDLE_MS).await
}

/// The body of [`beat`]. The window size is passable **only for tests** — callers use `beat`.
async fn beat_within<S: LinkRead>(socket: &mut S, watch: &mut IdleWatch, window_ms: u64) -> Beat {
    match tokio::time::timeout(Duration::from_millis(window_ms), socket.read_frame()).await {
        Ok(Frame::Text(t)) => {
            watch.on_traffic();
            Beat::Text(t)
        }
        Ok(Frame::Other) => {
            watch.on_traffic();
            Beat::Alive
        }
        Ok(Frame::Closed(why)) => Beat::Gone(why),
        Err(_) => match watch.on_idle() {
            Idle::Ping => Beat::Ping,
            Idle::Dead => Beat::Gone(format!("no reply to ping within {window_ms}ms")),
        },
    }
}

impl<S> LinkRead for tokio_tungstenite::WebSocketStream<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    async fn read_frame(&mut self) -> Frame {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::protocol::Message as M;
        match self.next().await {
            Some(Ok(M::Text(t))) => Frame::Text(t.to_string()),
            Some(Ok(M::Close(_))) | None => Frame::Closed("closed by the other side".to_string()),
            Some(Ok(_)) => Frame::Other, // tungstenite handles ping/pong and the like
            Some(Err(e)) => Frame::Closed(e.to_string()),
        }
    }
}

/// Reasons the handshake was refused where **the caller should change its behaviour**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fatal {
    /// 401 — wrong api token. A restart won't fix it. Only a human fixing the config will.
    BadToken,
    /// 426 — version mismatch. The kind that **is fixed by restarting with this machine's new code**.
    WrongVersion,
    /// 400 — the name is rejected (Bridge ID empty / invalid characters). Only fixing the config helps.
    BadBridgeId,
}

impl Fatal {
    /// From an HTTP status. **Anything else is not fatal** — it just didn't connect, so retry quietly.
    fn of(status: u16) -> Option<Fatal> {
        match status {
            401 => Some(Fatal::BadToken),
            426 => Some(Fatal::WrongVersion),
            400 => Some(Fatal::BadBridgeId),
            _ => None,
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            Fatal::BadToken => {
                "the gateway rejected the key; add this machine again with `agentgw add-machine`"
            }
            Fatal::WrongVersion => {
                "this machine and the gateway run incompatible versions; upgrade both"
            }
            Fatal::BadBridgeId => {
                "the gateway rejected this machine's name; check AGENTGW_BRIDGE_ID"
            }
        }
    }
}

/// What the Bridge uses out of what arrives from the Relay.
pub enum FromRelay {
    /// Accepted. The bot token (**in memory only**) and the current home.
    Ready {
        bot_token: String,
        home: Option<String>,
    },
    /// One Slack event.
    Event {
        name: String,
        event: serde_json::Value,
    },
    /// One button press.
    Action {
        action: serde_json::Value,
        body: serde_json::Value,
    },
    /// The Owner assigned this machine to some place.
    Linked {
        owner_user_id: String,
        channel: String,
        thread_ts: String,
    },
    /// A refusal that talking won't fix. **No retry.**
    Fatal(Fatal),
}

/// A frame from the gateway becomes an upstream message as-is. The bridge that lets **both the dialing side and the
/// accepted side use the same sink** (`Fatal` alone is a local matter, so it isn't in `From`).
impl From<crate::bridge::gateway::wire::LinkFrame> for FromRelay {
    fn from(frame: crate::bridge::gateway::wire::LinkFrame) -> Self {
        use crate::bridge::gateway::wire::LinkFrame as F;
        match frame {
            F::Ready { bot_token, home } => FromRelay::Ready { bot_token, home },
            F::Event { name, event } => FromRelay::Event { name, event },
            F::Action { action, body } => FromRelay::Action { action, body },
            F::Linked {
                owner_user_id,
                channel,
                thread_ts,
            } => FromRelay::Linked {
                owner_user_id,
                channel,
                thread_ts,
            },
        }
    }
}

pub struct RelayLink {
    url: String,
    api_token: String,
    bridge_id: String,
    stopped: Arc<AtomicBool>,
}

impl RelayLink {
    pub fn new(url: &str, api_token: &str, bridge_id: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            api_token: api_token.to_string(),
            bridge_id: bridge_id.to_string(),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Keep connected, forwarding what arrives to `tx`. **Does not return** (only when `stop()` is called).
    pub async fn run(&self, tx: tokio::sync::mpsc::Sender<FromRelay>) {
        let mut backoff = RECONNECT_MIN_MS;
        while !self.stopped.load(Ordering::SeqCst) {
            match self.connect_once(&tx).await {
                // The handshake succeeded this time. The next reconnect may start from a short wait
                Ok(true) => backoff = RECONNECT_MIN_MS,
                Ok(false) => {}
                // A refusal talking won't fix. **Say it loudly, but don't give up** — leaving here
                // makes the process exit, and the supervisor's rapid restarts hit systemd's start rate limit
                // and leave the unit `failed` (2026-08-03: a machine was down for 5 hours 15 minutes).
                // Keep reconnecting at intervals until a human fixes it, and it recovers the moment it is fixed
                Err(fatal) => {
                    LogCtx::default().error("bridge", &format!("remote link: {}", fatal.message()));
                    let _ = tx.send(FromRelay::Fatal(fatal)).await;
                }
            }
            if self.stopped.load(Ordering::SeqCst) {
                return;
            }
            LogCtx::default().info(
                "bridge",
                &format!("remote link: reconnecting in {backoff}ms (workers keep running)"),
            );
            tokio::time::sleep(Duration::from_millis(backoff)).await;
            backoff = (backoff * 2).min(RECONNECT_MAX_MS);
        }
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    /// One connection. `Ok(true)` = got through the handshake (then dropped), `Ok(false)` = didn't connect.
    async fn connect_once(&self, tx: &tokio::sync::mpsc::Sender<FromRelay>) -> Result<bool, Fatal> {
        let target = format!("{}{}", self.url, wire::path_for(&self.bridge_id));
        LogCtx::default().info(
            "bridge",
            &format!(
                "remote link: connecting to {target} as \"{}\"",
                self.bridge_id
            ),
        );
        let request = match build_request(&target, &self.api_token) {
            Ok(r) => r,
            Err(e) => {
                LogCtx::default().error("bridge", &format!("remote link: {e}"));
                return Ok(false);
            }
        };

        let (mut socket, _) = match tokio_tungstenite::connect_async(request).await {
            Ok(ok) => ok,
            Err(e) => {
                // Tell **refused** apart from **couldn't connect**. The latter is retried quietly
                if let tokio_tungstenite::tungstenite::Error::Http(resp) = &e
                    && let Some(fatal) = Fatal::of(resp.status().as_u16())
                {
                    return Err(fatal);
                }
                LogCtx::default().info("bridge", &format!("remote link: {e}"));
                return Ok(false);
            }
        };
        LogCtx::default().info(
            "bridge",
            &format!("remote link: linked as \"{}\"", self.bridge_id),
        );

        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::protocol::Message as M;
        let mut watch = IdleWatch::default();
        loop {
            // **Receive only via `beat`.** Waiting on `next()` directly hangs forever on half-open
            let raw = match beat(&mut socket, &mut watch).await {
                Beat::Text(t) => t,
                Beat::Alive => continue,
                Beat::Ping => {
                    if socket.send(M::Ping(Default::default())).await.is_err() {
                        break;
                    }
                    continue;
                }
                Beat::Gone(why) => {
                    LogCtx::default().info("bridge", &format!("remote link: link closed ({why})"));
                    break;
                }
            };
            let Some(frame) = wire::decode(&raw) else {
                LogCtx::default().info(
                    "bridge",
                    &format!("remote link: dropped an unrecognised frame: {}", {
                        let head: String = raw.chars().take(120).collect();
                        head
                    }),
                );
                continue;
            };
            let out = match frame {
                LinkFrame::Ready { bot_token, home } => {
                    LogCtx::default().info(
                        "bridge",
                        &format!(
                            "remote link: ready (Slack token received; kept in memory only){}",
                            home.as_deref()
                                .map(|h| format!(" — home={h}"))
                                .unwrap_or_default()
                        ),
                    );
                    FromRelay::Ready { bot_token, home }
                }
                LinkFrame::Event { name, event } => FromRelay::Event { name, event },
                LinkFrame::Action { action, body } => FromRelay::Action { action, body },
                LinkFrame::Linked {
                    owner_user_id,
                    channel,
                    thread_ts,
                } => {
                    LogCtx::default().info(
                        "bridge",
                        &format!(
                            "remote link: Owner put this machine in charge — owner={owner_user_id} channel={channel}"
                        ),
                    );
                    FromRelay::Linked {
                        owner_user_id,
                        channel,
                        thread_ts,
                    }
                }
            };
            if tx.send(out).await.is_err() {
                return Ok(true); // the Bridge itself has ended
            }
        }
        // This is where you'd be tempted to clean up agents by reflex. **Don't.**
        // A dropped link only means the flow from Slack stopped.
        Ok(true)
    }
}

/// Build the request carrying the three upgrade parts (path / `Authorization` / `Sec-WebSocket-Protocol`).
pub(crate) fn build_request(
    target: &str,
    api_token: &str,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = target
        .into_client_request()
        .map_err(|e| format!("not a usable address ({target}): {e}"))?;
    let headers = request.headers_mut();
    headers.insert(
        "authorization",
        format!("Bearer {api_token}")
            .parse()
            .map_err(|_| "the key can't go in a header".to_string())?,
    );
    headers.insert(
        "sec-websocket-protocol",
        wire::LINK_SUBPROTOCOL
            .parse()
            .map_err(|_| "the subprotocol can't go in a header".to_string())?,
    );
    Ok(request)
}

/// The wait before the next reconnect. Reset to the short wait once a handshake succeeds.
pub fn next_backoff(current_ms: u64, handshake_succeeded: bool) -> u64 {
    if handshake_succeeded {
        RECONNECT_MIN_MS
    } else {
        (current_ms * 2).min(RECONNECT_MAX_MS)
    }
}

// ── Which mode to run in (mutually exclusive) ────────────────────────────

/// The Bridge has two entrances, and they **must never be open at once**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Direct — open Slack's Socket Mode ourselves.
    Direct {
        app_token: String,
        bot_token: String,
    },
    /// Via the gateway — don't touch Slack, dial the gateway. The bot token comes in the handshake.
    Relay {
        url: String,
        api_token: String,
        bridge_id: String,
    },
    /// Via the gateway (**being picked up**) — don't dial; wait at a listener for the gateway's connection.
    /// Exists only for setups where the gateway is behind NAT and the machine can't reach it.
    /// The frame direction doesn't change (gateway → machine).
    AwaitParent,
}

impl Mode {
    /// Decide the mode from env. **Refuse to start if both are set.**
    ///
    /// Silently connecting both makes Slack start load-balancing across two consumers of the same app token,
    /// bringing back exactly the split-brain this design prevents. So rather than defaulting to one,
    /// **refuse loudly**.
    pub fn resolve(get: impl Fn(&str) -> Option<String>) -> Result<Mode, String> {
        let v = |k: &str| {
            get(k)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let (app, bot) = (v("SLACK_APP_TOKEN"), v("SLACK_BOT_TOKEN"));
        let (url, token, id) = (
            v("AGENTGW_RELAY_URL"),
            v("AGENTGW_RELAY_TOKEN"),
            v("AGENTGW_BRIDGE_ID"),
        );
        let wants_relay = url.is_some() || token.is_some();
        let listens = v("AGENTGW_LINK_LISTEN").is_some();

        // **One link per machine.** With both directions, the same event is delivered twice
        if wants_relay && listens {
            return Err(crate::t!(
                "Both AGENTGW_RELAY_URL and AGENTGW_LINK_LISTEN are set in .env. Keep one: \
                 AGENTGW_RELAY_URL if this machine connects to the gateway, AGENTGW_LINK_LISTEN if \
                 the gateway connects to this machine. With both, every message arrives twice.",
                ".env に AGENTGW_RELAY_URL と AGENTGW_LINK_LISTEN の両方があります。どちらか一方にしてください。\
                 このマシンからゲートウェイにつなぐなら AGENTGW_RELAY_URL、ゲートウェイからこのマシンに\
                 つなぐなら AGENTGW_LINK_LISTEN です。両方あると、メッセージが2回ずつ届きます。"
            ));
        }
        if app.is_some() && wants_relay {
            return Err(crate::t!(
                ".env has both SLACK_APP_TOKEN and AGENTGW_RELAY_URL. This machine can either connect \
                 to Slack itself (as the gateway) or go through a gateway, not both — Slack would split \
                 messages between the two, and some would never be answered. To go through a gateway, \
                 comment out SLACK_APP_TOKEN.",
                ".env に SLACK_APP_TOKEN と AGENTGW_RELAY_URL の両方があります。このマシンは、自分で Slack に\
                 つなぐ(ゲートウェイになる)か、ゲートウェイを通すかのどちらかです。両方だと Slack が\
                 メッセージを振り分けてしまい、返事の来ないものが出ます。ゲートウェイを通すなら、\
                 SLACK_APP_TOKEN をコメントアウトしてください。"
            ));
        }
        if wants_relay {
            let (Some(url), Some(api_token)) = (url, token) else {
                return Err(crate::t!(
                    "The connection to the gateway is only half set up: .env needs both \
                     AGENTGW_RELAY_URL and AGENTGW_RELAY_TOKEN. Run `agentgw add-machine` on the \
                     gateway to set both.",
                    "ゲートウェイへの接続の設定が途中です。.env に AGENTGW_RELAY_URL と \
                     AGENTGW_RELAY_TOKEN の両方が必要です。ゲートウェイで `agentgw add-machine` を\
                     実行すると両方が書かれます。"
                ));
            };
            // **No automatic naming.** Don't fall back to the hostname or `default` —
            // machines with colliding names steal each other's Slack messages
            let Some(bridge_id) = id else {
                return Err(crate::t!(
                    "This machine has no name. Set AGENTGW_BRIDGE_ID in .env. It isn't chosen \
                     automatically: two machines with the same name would take each other's messages.",
                    "このマシンに名前がありません。.env に AGENTGW_BRIDGE_ID を書いてください。\
                     名前は自動では決めません。同じ名前のマシンが2台あると、互いのメッセージを取り合うためです。"
                ));
            };
            return Ok(Mode::Relay {
                url,
                api_token,
                bridge_id,
            });
        }
        // No Slack token and only a listener = a machine that the gateway comes to pick up
        if app.is_none() && listens {
            return Ok(Mode::AwaitParent);
        }
        match (app, bot) {
            (Some(app_token), Some(bot_token)) => Ok(Mode::Direct {
                app_token,
                bot_token,
            }),
            _ => Err(crate::t!(
                "There are no Slack tokens in .env. For the gateway, run `agentgw install` and paste \
                 them; for any other machine, run `agentgw add-machine` on the gateway.",
                ".env に Slack のトークンがありません。ゲートウェイなら `agentgw install` でトークンを\
                 入れてください。ほかのマシンは、ゲートウェイで `agentgw add-machine` を実行して加えます。"
            )),
        }
    }
}

/// The listener that accepts machines. Default is `0.0.0.0` (accept on any interface).
///
/// This Bridge does not terminate TLS itself. If a front (Tailscale / SSH tunnel / reverse proxy) is
/// in place, it terminates; if not, it accepts **plain `ws://`**. This link hands the Slack
/// bot token to machines, so exposing it in plain text is exactly that much risk.
/// The call is operational — when [`Listen::is_exposed`] is true, one warning line is logged at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listen {
    pub addr: std::net::SocketAddr,
    /// The shared secret machines present. Generated by `add-child`.
    pub token: String,
}

impl Listen {
    /// Whether it is exposed beyond loopback. **Only decides whether to warn**; nothing is refused.
    pub fn is_exposed(&self) -> bool {
        !self.addr.ip().is_loopback()
    }
}

/// This process's wiring — upstream (where it gets from) and downstream (who it hands to).
///
/// **The mode has two axes, not a product.** If upstream is a gateway (= we are a machine), there can be no downstream
/// (that would make three tiers). This check is where the "no multi-tier" ruling lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wiring {
    pub upstream: Mode,
    /// Our own name. A machine announces it; a gateway uses it as the target of `route <own id>`.
    pub self_id: Option<String>,
    /// Set when accepting machines. **Without it, the usual standalone Bridge**.
    pub children: Option<Listen>,
    /// The listener of a machine that the gateway picks up. Mutually exclusive with `children` (both read `AGENTGW_LINK_LISTEN`,
    /// but for different peers — this one is where **upstream** comes in).
    pub inlet: Option<Listen>,
}

impl Wiring {
    pub fn resolve(get: impl Fn(&str) -> Option<String>) -> Result<Wiring, String> {
        let v = |k: &str| {
            get(k)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let upstream = Mode::resolve(&get)?;
        let self_id = v("AGENTGW_BRIDGE_ID");

        let Some(listen) = v("AGENTGW_LINK_LISTEN") else {
            return Ok(Wiring {
                upstream,
                self_id,
                children: None,
                inlet: None,
            });
        };
        // A machine the gateway picks up opens this listener **for the gateway** (it doesn't take machines).
        // Three tiers (dialing a gateway while having machines) are already refused by Mode::resolve
        if matches!(upstream, Mode::AwaitParent) {
            return Ok(Wiring {
                upstream,
                self_id,
                children: None,
                inlet: Some(Listen {
                    addr: parse_listen(&listen)?,
                    token: v("AGENTGW_LINK_TOKEN").ok_or_else(|| {
                        crate::t!(
                            ".env has AGENTGW_LINK_LISTEN but no AGENTGW_LINK_TOKEN. The gateway's \
                             secret key is required to accept its connection.",
                            ".env に AGENTGW_LINK_LISTEN はありますが、AGENTGW_LINK_TOKEN がありません。\
                             ゲートウェイからの接続を受けるには、ゲートウェイの秘密鍵が必要です。"
                        )
                    })?,
                }),
            });
        }
        let Some(token) = v("AGENTGW_LINK_TOKEN") else {
            return Err(crate::t!(
                ".env has AGENTGW_LINK_LISTEN but no AGENTGW_LINK_TOKEN. Machines can't connect \
                 without a secret key; `agentgw add-machine` creates one.",
                ".env に AGENTGW_LINK_LISTEN はありますが、AGENTGW_LINK_TOKEN がありません。秘密鍵が\
                 無いとマシンはつながれません。`agentgw add-machine` を実行すると作られます。"
            ));
        };
        // A gateway with no name can't be the target of `route <own id>`. It is not decided automatically
        if self_id.is_none() {
            return Err(crate::t!(
                "To accept machines, this gateway needs a name: set AGENTGW_BRIDGE_ID in .env. \
                 `route` uses it, so it isn't chosen automatically.",
                "マシンを受け入れるには、このゲートウェイに名前が必要です。.env に AGENTGW_BRIDGE_ID を\
                 書いてください。`route` で使う名前なので、自動では決めません。"
            ));
        }
        Ok(Wiring {
            upstream,
            self_id,
            children: Some(Listen {
                addr: parse_listen(&listen)?,
                token,
            }),
            inlet: None,
        })
    }
}

// ── waiting for the gateway (machines the gateway dials)  ─────────────────────

/// A machine's kit for waiting for the gateway's connection. It is **where upstream comes in**, so it differs from [`gateway::Fleet`](crate::bridge::gateway::Fleet) (which accepts machines).
pub struct GatewayInlet {
    pub token: String,
    /// Where received frames go. **The same sink** as when the machine dials.
    pub tx: tokio::sync::mpsc::Sender<FromRelay>,
}

async fn on_parent_upgrade(
    State(inlet): State<Arc<GatewayInlet>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    match gateway::admit_upgrade(&headers, &uri, &inlet.token, "a parent", ws) {
        Ok((parent_id, ws)) => ws
            .protocols([wire::LINK_SUBPROTOCOL])
            .on_upgrade(move |socket| on_parent_socket(inlet, parent_id, socket)),
        Err(response) => response,
    }
}

/// Keep reading frames from the gateway. **Agents are not touched** — losing the gateway
/// only means "no new Slack messages until it comes back".
async fn on_parent_socket(inlet: Arc<GatewayInlet>, parent_id: String, mut socket: WebSocket) {
    gateway::rlog("info", &format!("parent \"{parent_id}\" connected"));
    // If the gateway silently vanishes, we'd stay stuck in receive forever without noticing. Poke to check
    let mut watch = IdleWatch::default();
    loop {
        // **Receive only via `beat`.** Waiting on `recv()` directly hangs forever on half-open
        let raw = match beat(&mut socket, &mut watch).await {
            Beat::Text(t) => t,
            Beat::Alive => continue,
            Beat::Ping => {
                if socket
                    .send(Message::Ping(Default::default()))
                    .await
                    .is_err()
                {
                    break;
                }
                continue;
            }
            Beat::Gone(why) => {
                gateway::rlog(
                    "info",
                    &format!("parent \"{parent_id}\": link closed ({why})"),
                );
                break;
            }
        };
        let Some(frame) = wire::decode(&raw) else {
            gateway::rlog("info", "dropped an unrecognised frame from the parent");
            continue;
        };
        if inlet.tx.send(frame.into()).await.is_err() {
            break;
        }
    }
    gateway::rlog("info", &format!("parent \"{parent_id}\" disconnected"));
}

/// Open the listener that accepts the gateway. **Does not return.**
impl GatewayInlet {
    /// Open the listener that accepts the gateway. **Does not return.**
    pub async fn serve(self: Arc<Self>, addr: std::net::SocketAddr) {
        let app = Router::new()
            .route(wire::PROBE_PATH, get(on_parent_upgrade))
            .route("/bridge/{id}", get(on_parent_upgrade))
            .with_state(self);
        let Some(listener) = gateway::bind_link_port(addr, "parent").await else {
            return;
        };
        gateway::rlog(
            "info",
            &format!("waiting for the parent on {addr}/bridge/<id>"),
        );
        if let Err(e) = axum::serve(listener, app).await {
            gateway::rlog("error", &format!("the inlet stopped: {e}"));
        }
    }
}

/// Read `host:port`. **Accepts on any interface** (default `0.0.0.0`).
///
/// It used to allow only loopback. Since it doesn't terminate TLS itself, it was built to always go through
/// a front (Tailscale / SSH tunnel / reverse proxy). **Opened up on 2026-08-02 by user decision** —
/// whether to put a front in is an operational choice. When opened,
/// [`Listen::is_exposed`] is true and one warning line is logged at startup.
fn parse_listen(listen: &str) -> Result<std::net::SocketAddr, String> {
    let (host, port) = listen.rsplit_once(':').ok_or_else(|| {
        crate::t!(
            "AGENTGW_LINK_LISTEN must be host:port (e.g. 0.0.0.0:8787), not {listen}",
            "AGENTGW_LINK_LISTEN は host:port の形で書いてください(例: 0.0.0.0:8787)。今の値: {listen}"
        )
    })?;
    let host = host.trim_matches(['[', ']']);
    let port: u16 = port
        .parse()
        .map_err(|_| crate::t!("AGENTGW_LINK_LISTEN has an invalid port: {port}", "AGENTGW_LINK_LISTEN のポートが正しくありません: {port}"))?;
    let ip: std::net::IpAddr = if host == "localhost" {
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    } else {
        host.parse()
            .map_err(|_| crate::t!("AGENTGW_LINK_LISTEN has an invalid host: {host}", "AGENTGW_LINK_LISTEN のホストが正しくありません: {host}"))?
    };
    Ok(std::net::SocketAddr::new(ip, port))
}

/// The machines the gateway **goes to pick up**. `AGENTGW_CHILD_URLS=desktop=wss://a,laptop=wss://b`.
///
/// **The default is the machine dialing** — set this only when the gateway is behind NAT and machines can't
/// reach it. **Not an automatic fallback** (once the gateway holds a machine's URL, the choice is made;
/// "wait first, then dial if that fails" would make an unreachable machine indistinguishable from an unconfigured one).
pub fn child_urls(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|pair| {
            let (id, url) = pair.split_once('=')?;
            let (id, url) = (id.trim(), url.trim().trim_end_matches('/'));
            (!id.is_empty() && !url.is_empty()).then(|| (id.to_string(), url.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Of the refusal reasons, **only those that call for a change of behaviour** are fatal.

    #[test]
    fn only_a_refusal_we_cannot_talk_our_way_out_of_is_fatal() {
        assert_eq!(Fatal::of(401), Some(Fatal::BadToken));
        assert_eq!(Fatal::of(426), Some(Fatal::WrongVersion));
        assert_eq!(Fatal::of(400), Some(Fatal::BadBridgeId));
        // Just didn't connect / the peer is down = retry quietly
        for status in [404, 500, 502, 503, 301, 200] {
            assert_eq!(Fatal::of(status), None, "{status}");
        }
    }

    /// Two silent windows mean dead. **Don't disconnect on the first** — cutting a link
    /// that is merely quiet every 30 seconds causes a reconnect storm.
    #[test]
    fn two_silent_windows_mean_the_link_is_dead() {
        let mut w = IdleWatch::default();
        assert_eq!(w.on_idle(), Idle::Ping); // first: poke to check
        assert_eq!(w.on_idle(), Idle::Dead); // second: no answer = dead
    }

    /// A Pong or a delivery — **anything arriving means alive**. The watchman restarts its count there.
    #[test]
    fn any_traffic_clears_the_watch() {
        let mut w = IdleWatch::default();
        assert_eq!(w.on_idle(), Idle::Ping);
        w.on_traffic(); // a Pong came back
        assert_eq!(w.on_idle(), Idle::Ping); // back to the first one
        w.on_traffic();
        w.on_traffic();
        assert_eq!(w.on_idle(), Idle::Ping);
        assert_eq!(w.on_idle(), Idle::Dead);
    }

    /// A socket that returns nothing = the peer silently vanished (half-open).
    struct SilentSocket;
    impl LinkRead for SilentSocket {
        async fn read_frame(&mut self) -> Frame {
            std::future::pending().await // never returns — this is what the 74-minute silence was
        }
    }

    /// A socket that returns a payload only on the second read and is silent otherwise (silent → arrives → silent).
    struct SilentThenText(u32);
    impl LinkRead for SilentThenText {
        async fn read_frame(&mut self) -> Frame {
            self.0 += 1;
            if self.0 == 2 {
                Frame::Text("hello".to_string())
            } else {
                std::future::pending().await
            }
        }
    }

    /// **`beat` never waits forever.** If it stays silent, it asks for a Ping, then says to disconnect in the next window.
    #[tokio::test]
    async fn a_silent_socket_gets_pinged_then_declared_gone() {
        let mut s = SilentSocket;
        let mut w = IdleWatch::default();
        assert!(matches!(beat_within(&mut s, &mut w, 10).await, Beat::Ping));
        let Beat::Gone(why) = beat_within(&mut s, &mut w, 10).await else {
            panic!("2窓目は Gone のはず");
        };
        assert!(why.contains("no reply to ping"), "{why}");
    }

    /// When something arrives, return the payload and **restart the watchman's count** — the key to not cutting a live link.
    /// Without the reset, a single silent window right after a Ping would disconnect.
    #[tokio::test]
    async fn a_frame_arrives_and_resets_the_watch() {
        let mut s = SilentThenText(0);
        let mut w = IdleWatch::default();
        // First window is silent → ask for a Ping
        assert!(matches!(beat_within(&mut s, &mut w, 10).await, Beat::Ping));
        // Then a payload arrives (a Pong works the same) → alive, so restart the count
        match beat_within(&mut s, &mut w, 10).await {
            Beat::Text(t) => assert_eq!(t, "hello"),
            _ => panic!("本文が来るはず"),
        }
        // **The count was reset, so it's Ping again.** Gone here would mean the reset didn't happen
        assert!(matches!(beat_within(&mut s, &mut w, 10).await, Beat::Ping));
        assert!(matches!(
            beat_within(&mut s, &mut w, 10).await,
            Beat::Gone(_)
        ));
    }

    /// Only a version mismatch is worded as "update this machine and restart to fix".
    #[test]
    fn a_version_mismatch_says_a_restart_can_heal_it() {
        assert!(Fatal::WrongVersion.message().contains("upgrade"));
        assert!(Fatal::BadToken.message().contains("add-machine"));
        assert!(Fatal::BadBridgeId.message().contains("AGENTGW_BRIDGE_ID"));
    }

    #[test]
    fn the_backoff_grows_then_resets_on_a_good_handshake() {
        let mut b = RECONNECT_MIN_MS;
        for expected in [2_000, 4_000, 8_000, 16_000, 30_000, 30_000] {
            b = next_backoff(b, false);
            assert_eq!(b, expected);
        }
        // Once a handshake has succeeded, the next wait starts short
        assert_eq!(next_backoff(b, true), RECONNECT_MIN_MS);
    }

    #[test]
    fn the_request_carries_the_three_things_the_upgrade_needs() {
        let req = build_request("wss://relay.example/bridge/desktop", "s3cret").unwrap();
        assert_eq!(req.uri().path(), "/bridge/desktop");
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer s3cret");
        assert_eq!(
            req.headers().get("sec-websocket-protocol").unwrap(),
            wire::LINK_SUBPROTOCOL
        );
    }

    #[test]
    fn a_url_that_is_not_a_websocket_target_is_refused_rather_than_dialled() {
        assert!(build_request("not a url", "s").is_err());
        assert!(build_request("", "s").is_err());
    }

    // ── Mode selection ──────────────────────────────────────────────────────

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn direct_mode_needs_both_slack_tokens() {
        assert_eq!(
            Mode::resolve(env(&[
                ("SLACK_APP_TOKEN", "xapp-1"),
                ("SLACK_BOT_TOKEN", "xoxb-1")
            ]))
            .unwrap(),
            Mode::Direct {
                app_token: "xapp-1".into(),
                bot_token: "xoxb-1".into()
            }
        );
        assert!(Mode::resolve(env(&[("SLACK_APP_TOKEN", "xapp-1")])).is_err());
        assert!(Mode::resolve(env(&[])).is_err());
    }

    #[test]
    fn relay_mode_needs_the_url_the_token_and_a_name() {
        assert_eq!(
            Mode::resolve(env(&[
                ("AGENTGW_RELAY_URL", "wss://r"),
                ("AGENTGW_RELAY_TOKEN", "s"),
                ("AGENTGW_BRIDGE_ID", "desktop"),
            ]))
            .unwrap(),
            Mode::Relay {
                url: "wss://r".into(),
                api_token: "s".into(),
                bridge_id: "desktop".into()
            }
        );
        // Only the URL / only the token = half-finished config. Refuse with a clear message
        let half = Mode::resolve(env(&[("AGENTGW_RELAY_URL", "wss://r")])).unwrap_err();
        assert!(half.contains("half set up"), "{half}");
    }

    /// **No automatic naming.** Falling back to `default` makes multiple machines all collide.
    #[test]
    fn a_relay_bridge_without_a_name_refuses_to_start() {
        let e = Mode::resolve(env(&[
            ("AGENTGW_RELAY_URL", "wss://r"),
            ("AGENTGW_RELAY_TOKEN", "s"),
        ]))
        .unwrap_err();
        assert!(e.contains("AGENTGW_BRIDGE_ID"), "{e}");
        assert!(e.contains("take each other's messages"), "{e}");
    }

    /// A config with both set doesn't start. Silently connecting both brings split-brain back.
    #[test]
    fn having_both_modes_configured_refuses_to_start() {
        let e = Mode::resolve(env(&[
            ("SLACK_APP_TOKEN", "xapp-1"),
            ("SLACK_BOT_TOKEN", "xoxb-1"),
            ("AGENTGW_RELAY_URL", "wss://r"),
            ("AGENTGW_RELAY_TOKEN", "s"),
            ("AGENTGW_BRIDGE_ID", "desktop"),
        ]))
        .unwrap_err();
        assert!(e.contains("not both"), "{e}");
        assert!(e.contains("SLACK_APP_TOKEN"), "{e}");
    }

    /// An empty string is the same as absent. A .env left with just `SLACK_APP_TOKEN=` doesn't block startup.
    #[test]
    fn an_empty_value_counts_as_absent() {
        assert!(matches!(
            Mode::resolve(env(&[
                ("SLACK_APP_TOKEN", "   "),
                ("AGENTGW_RELAY_URL", "wss://r"),
                ("AGENTGW_RELAY_TOKEN", "s"),
                ("AGENTGW_BRIDGE_ID", "desktop"),
            ])),
            Ok(Mode::Relay { .. })
        ));
    }

    // ── Wiring (upstream × downstream) ──────────────────────────────────────

    fn parent(extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = [
            ("SLACK_APP_TOKEN", "xapp-1"),
            ("SLACK_BOT_TOKEN", "xoxb-1"),
            ("AGENTGW_BRIDGE_ID", "vps"),
        ]
        .iter()
        .map(|(k, x)| (k.to_string(), x.to_string()))
        .collect();
        v.extend(extra.iter().map(|(k, x)| (k.to_string(), x.to_string())));
        v
    }

    fn wire(pairs: &[(String, String)]) -> Result<Wiring, String> {
        let owned: Vec<(String, String)> = pairs.to_vec();
        Wiring::resolve(move |k| {
            owned
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        })
    }

    /// Without a config for accepting machines, it is **the usual standalone Bridge**. This is the main regression guard.
    #[test]
    fn a_bridge_without_a_listen_is_the_lone_bridge_we_already_had() {
        let w = wire(&parent(&[])).unwrap();
        assert!(w.children.is_none());
        assert!(matches!(w.upstream, Mode::Direct { .. }));
    }

    #[test]
    fn a_parent_listens_on_the_loopback_port_it_was_given() {
        let w = wire(&parent(&[
            ("AGENTGW_LINK_LISTEN", "127.0.0.1:8787"),
            ("AGENTGW_LINK_TOKEN", "s3cret"),
        ]))
        .unwrap();
        let l = w.children.unwrap();
        assert_eq!(l.addr.to_string(), "127.0.0.1:8787");
        assert_eq!(l.token, "s3cret");
        assert_eq!(w.self_id.as_deref(), Some("vps"));
    }

    /// **Binds on public interfaces too** (2026-08-02, user decision). Nothing is refused, but
    /// being exposed beyond loopback sets `is_exposed`, which logs one warning line at startup.
    #[test]
    fn a_listen_outside_the_loopback_is_allowed_but_marked_exposed() {
        let w = wire(&parent(&[
            ("AGENTGW_LINK_LISTEN", "0.0.0.0:8787"),
            ("AGENTGW_LINK_TOKEN", "s3cret"),
        ]))
        .unwrap();
        let l = w.children.unwrap();
        assert_eq!(l.addr.to_string(), "0.0.0.0:8787");
        assert!(l.is_exposed());

        let loop_back = wire(&parent(&[
            ("AGENTGW_LINK_LISTEN", "127.0.0.1:8787"),
            ("AGENTGW_LINK_TOKEN", "s3cret"),
        ]))
        .unwrap();
        assert!(!loop_back.children.unwrap().is_exposed());
    }

    #[test]
    fn a_listen_without_a_key_refuses_to_start() {
        let e = wire(&parent(&[("AGENTGW_LINK_LISTEN", "127.0.0.1:8787")])).unwrap_err();
        assert!(e.contains("AGENTGW_LINK_TOKEN"), "{e}");
    }

    /// A gateway with no name can't be the target of `route <own id>`. **Not decided automatically.**
    #[test]
    fn a_parent_without_a_name_refuses_to_start() {
        let mut pairs = parent(&[
            ("AGENTGW_LINK_LISTEN", "127.0.0.1:8787"),
            ("AGENTGW_LINK_TOKEN", "s3cret"),
        ]);
        pairs.retain(|(k, _)| k != "AGENTGW_BRIDGE_ID");
        let e = wire(&pairs).unwrap_err();
        assert!(e.contains("AGENTGW_BRIDGE_ID"), "{e}");
    }

    /// **One link per machine.** A config that dials out while also being picked up
    /// gets every event twice (= a config trying to build three tiers is stopped here too).
    #[test]
    fn a_child_cannot_face_both_ways() {
        let e = wire(&[
            ("AGENTGW_RELAY_URL".into(), "wss://r".into()),
            ("AGENTGW_RELAY_TOKEN".into(), "s".into()),
            ("AGENTGW_BRIDGE_ID".into(), "desktop".into()),
            ("AGENTGW_LINK_LISTEN".into(), "127.0.0.1:8787".into()),
            ("AGENTGW_LINK_TOKEN".into(), "k".into()),
        ])
        .unwrap_err();
        assert!(e.contains("Keep one"), "{e}");
    }

    /// No Slack token and only a listener = **a machine that the gateway picks up**.
    #[test]
    fn a_bridge_with_only_an_inlet_waits_for_its_parent() {
        let w = wire(&[
            ("AGENTGW_BRIDGE_ID".into(), "desktop".into()),
            ("AGENTGW_LINK_LISTEN".into(), "127.0.0.1:8787".into()),
            ("AGENTGW_LINK_TOKEN".into(), "k".into()),
        ])
        .unwrap();
        assert!(matches!(w.upstream, Mode::AwaitParent));
        // The opened listener is **for upstream**; it doesn't take machines
        assert!(w.children.is_none());
        assert_eq!(w.inlet.unwrap().addr.to_string(), "127.0.0.1:8787");
    }

    /// The table of machines to pick up. **Whitespace and a trailing / are stripped. Broken pairs are silently dropped**
    /// (safer to treat that machine as absent than to build a half-valid destination).
    #[test]
    fn the_child_url_table_is_read_pair_by_pair() {
        assert_eq!(
            child_urls("desktop=wss://a.example/ , laptop=wss://b.example"),
            vec![
                ("desktop".to_string(), "wss://a.example".to_string()),
                ("laptop".to_string(), "wss://b.example".to_string()),
            ]
        );
        assert!(child_urls("").is_empty());
        assert!(child_urls("desktop").is_empty());
        assert!(child_urls("=wss://a").is_empty());
        assert!(child_urls("desktop=").is_empty());
    }

    /// **A real-data check at the point where the routes merge**: JSON in the shape the Relay sends must become
    /// the same `InboundMsg` as a direct connection. If this breaks, messages stop arriving only via the Relay.
    #[test]
    fn an_event_the_relay_forwards_becomes_the_same_inbound_message() {
        let event = serde_json::json!({
            "type": "message",
            "ts": "1700000000.000100",
            "channel": "C1",
            "user": "U1",
            "text": "<@U_BOT> hello"
        });
        let msg = crate::chat::slack::inbound_from_relay("message", &event).expect("読めること");
        assert_eq!(msg.channel, "C1");
        assert_eq!(msg.user.as_deref(), Some("U1"));
        assert_eq!(msg.text, "<@U_BOT> hello");
        assert_eq!(msg.ts, "1700000000.000100");
    }

    #[test]
    fn an_event_the_relay_could_not_encode_is_dropped_not_guessed() {
        assert!(crate::chat::slack::inbound_from_relay("message", &serde_json::json!({})).is_none());
        assert!(
            crate::chat::slack::inbound_from_relay("app_mention", &serde_json::json!({"channel": "C1"}))
                .is_none()
        );
    }

    /// Only buttons of the form `perm:<action>:<reqId>` are picked up. Anything else isn't the Bridge's concern.
    #[test]
    fn a_forwarded_button_becomes_a_perm_click() {
        let click = crate::chat::slack::perm_click_from_relay(
            &serde_json::json!({"action_id": "perm:allow-channel:req-7"}),
            &serde_json::json!({"user": {"id": "U_OWNER"}}),
        )
        .expect("読めること");
        assert_eq!(click.req_id, "req-7");
        assert_eq!(click.action, "allow-channel");
        assert_eq!(click.by, "U_OWNER");

        for other in ["something_else", "perm", "perm:allow"] {
            assert!(
                crate::chat::slack::perm_click_from_relay(
                    &serde_json::json!({ "action_id": other }),
                    &serde_json::json!({})
                )
                .is_none(),
                "{other}"
            );
        }
    }

    /// The dial target is built including the path — the connection string's URL has no path.
    #[test]
    fn the_path_is_built_from_the_bridge_id() {
        let l = RelayLink::new("wss://relay.example/", "tok", "desktop");
        assert_eq!(l.url, "wss://relay.example");
        assert_eq!(
            format!("{}{}", l.url, wire::path_for(&l.bridge_id)),
            "wss://relay.example/bridge/desktop"
        );
    }
}
