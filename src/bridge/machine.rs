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

use crate::bridge::gateway::link::{self, LinkFrame};
use crate::bridge::gateway::{Beat, IdleWatch, RECONNECT_MAX_MS, RECONNECT_MIN_MS, beat};
use crate::log::LogCtx;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;


use crate::bridge::gateway;

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
        /// Who the gateway is (`status` says so).
        gateway: Option<String>,
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
    /// The notice channel decided by the gateway.
    Home { channel: String },
    /// `pwd <this machine>:<path>` — check the folder here, store it, answer up the link.
    SetProject {
        channel: String,
        thread_ts: String,
        path: String,
    },
    /// A refusal that talking won't fix. **No retry.**
    Fatal(Fatal),
}

/// Answers a machine sends up the link (see [`LinkFrame::ProjectSet`]). Shared by every connection this
/// machine has had, so an answer queued while the link was down goes out on the next one.
pub type Uplink = Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<LinkFrame>>>;

/// What woke a machine's link loop: something from the gateway, or an answer of ours to send up.
enum Wake {
    Beat(Beat),
    Up(LinkFrame),
}

async fn wake<S: crate::bridge::gateway::LinkRead + Send>(
    socket: &mut S,
    watch: &mut IdleWatch,
    up: &Uplink,
) -> Wake {
    tokio::select! {
        b = beat(socket, watch) => Wake::Beat(b),
        Some(f) = async { up.lock().await.recv().await } => Wake::Up(f),
    }
}

/// A frame from the gateway becomes an upstream message as-is. The bridge that lets **both the dialing side and the
/// accepted side use the same sink** (`Fatal` is a local matter, so it never comes from a frame).
/// `None` for [`LinkFrame::ProjectSet`] — that one only ever goes the other way.
impl FromRelay {
    fn of(frame: LinkFrame) -> Option<Self> {
        use LinkFrame as F;
        Some(match frame {
            F::Ready {
                bot_token,
                home,
                gateway,
            } => FromRelay::Ready {
                bot_token,
                home,
                gateway,
            },
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
            F::SetProject {
                channel,
                thread_ts,
                path,
            } => FromRelay::SetProject {
                channel,
                thread_ts,
                path,
            },
            F::Home { channel } => FromRelay::Home { channel },
            // Answers and asks only ever go the other way
            F::ProjectSet { .. }
            | F::Channels { .. }
            | F::PwdOn { .. }
            | F::SetHome { .. }
            | F::MachineHome { .. }
            | F::MachineHost { .. }
            | F::Machines { .. } => {
                return None;
            }
        })
    }
}

pub struct RelayLink {
    url: String,
    api_token: String,
    bridge_id: String,
    stopped: Arc<AtomicBool>,
    up: Uplink,
    /// Whether the link to the gateway is up right now (`status` says so).
    live: Arc<AtomicBool>,
}

impl RelayLink {
    pub fn new(url: &str, api_token: &str, bridge_id: &str, up: Uplink, live: Arc<AtomicBool>) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            api_token: api_token.to_string(),
            bridge_id: bridge_id.to_string(),
            stopped: Arc::new(AtomicBool::new(false)),
            up,
            live,
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
        let target = format!("{}{}", self.url, link::path_for(&self.bridge_id));
        LogCtx::default().info(
            "bridge",
            &format!(
                "remote link: connecting to {target} as \"{}\"",
                self.bridge_id
            ),
        );
        let request = match link::build_request(&target, &self.api_token) {
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
        self.live.store(true, Ordering::SeqCst);

        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::protocol::Message as M;
        let mut watch = IdleWatch::default();
        loop {
            // **Receive only via `beat`.** Waiting on `next()` directly hangs forever on half-open
            let raw = match wake(&mut socket, &mut watch, &self.up).await {
                Wake::Up(frame) => {
                    watch.on_traffic();
                    if socket.send(M::Text(link::encode(&frame).into())).await.is_err() {
                        break;
                    }
                    continue;
                }
                Wake::Beat(Beat::Text(t)) => t,
                Wake::Beat(Beat::Alive) => continue,
                Wake::Beat(Beat::Ping) => {
                    if socket.send(M::Ping(Default::default())).await.is_err() {
                        break;
                    }
                    continue;
                }
                Wake::Beat(Beat::Gone(why)) => {
                    LogCtx::default().info("bridge", &format!("remote link: link closed ({why})"));
                    break;
                }
            };
            let Some(out) = link::decode(&raw).and_then(FromRelay::of) else {
                LogCtx::default().info(
                    "bridge",
                    &format!("remote link: dropped an unrecognised frame: {}", {
                        let head: String = raw.chars().take(120).collect();
                        head
                    }),
                );
                continue;
            };
            match &out {
                FromRelay::Ready { home, .. } => LogCtx::default().info(
                    "bridge",
                    &format!(
                        "remote link: ready (Slack token received; kept in memory only){}",
                        home.as_deref()
                            .map(|h| format!(" — home={h}"))
                            .unwrap_or_default()
                    ),
                ),
                FromRelay::Linked {
                    owner_user_id,
                    channel,
                    ..
                } => LogCtx::default().info(
                    "bridge",
                    &format!(
                        "remote link: Owner put this machine in charge — owner={owner_user_id} channel={channel}"
                    ),
                ),
                _ => {}
            }
            if tx.send(out).await.is_err() {
                return Ok(true); // the Bridge itself has ended
            }
        }
        self.live.store(false, Ordering::SeqCst);
        // This is where you'd be tempted to clean up agents by reflex. **Don't.**
        // A dropped link only means the flow from Slack stopped.
        Ok(true)
    }
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
}

impl Mode {
    /// Which side of a link this Bridge is, from `.env`. **The role is written down, not guessed** —
    /// it used to be inferred from which keys happened to be present, and a half-finished `.env` then
    /// read as a different role rather than as an error.
    pub fn resolve(
        get: impl Fn(&str) -> Option<String>,
        access: &crate::bridge::state::Access,
    ) -> Result<Mode, String> {
        let v = |k: &str| {
            get(k)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        // **No automatic naming.** Don't fall back to the hostname or `default` — machines with
        // colliding names steal each other's Slack messages
        let id = v("AGENTGW_BRIDGE_ID");
        let role = v("AGENTGW_BRIDGE_ROLE").unwrap_or_default();
        match role.to_lowercase().as_str() {
            "machine" => {
                let Some(link) = access.gateway.as_ref().filter(|l| !l.link_url.is_empty()) else {
                    return Err(crate::t!(
                        "This machine doesn't know its gateway: access.json has no `gateway` record.                          Run `agentgw add-machine` for it on the gateway.",
                        "このマシンはゲートウェイを知りません(access.json に `gateway` がありません)。                         ゲートウェイで `agentgw add-machine` を実行してください。"
                    ));
                };
                let Some(api_token) = v("AGENTGW_LINK_TOKEN") else {
                    return Err(crate::t!(
                        ".env has no AGENTGW_LINK_TOKEN, so this machine has no key to present to the                          gateway. Run `agentgw add-machine` for it on the gateway.",
                        ".env に AGENTGW_LINK_TOKEN がありません。ゲートウェイに示す鍵が無い状態です。                         ゲートウェイで `agentgw add-machine` を実行してください。"
                    ));
                };
                let Some(bridge_id) = id else {
                    return Err(crate::t!(
                        "This machine has no name. Set AGENTGW_BRIDGE_ID in .env. It isn't chosen                          automatically: two machines with the same name would take each other's messages.",
                        "このマシンに名前がありません。.env に AGENTGW_BRIDGE_ID を書いてください。                         名前は自動では決めません。同じ名前のマシンが2台あると、互いのメッセージを取り合うためです。"
                    ));
                };
                Ok(Mode::Relay {
                    url: link.link_url.clone(),
                    api_token,
                    bridge_id,
                })
            }
            "gateway" => match (v("SLACK_APP_TOKEN"), v("SLACK_BOT_TOKEN")) {
                (Some(app_token), Some(bot_token)) => Ok(Mode::Direct {
                    app_token,
                    bot_token,
                }),
                _ => Err(crate::t!(
                    "This is the gateway, but .env has no Slack tokens. Run `agentgw install` and                      paste them.",
                    "ここはゲートウェイですが、.env に Slack のトークンがありません。`agentgw install` を                     実行して貼り付けてください。"
                )),
            },
            _ => Err(match role.is_empty() {
                true => crate::t!(
                    ".env doesn't say what this Bridge is: set AGENTGW_BRIDGE_ROLE to `gateway` or                      `machine`. `agentgw install` writes it.",
                    ".env にこの Bridge の役割がありません。AGENTGW_BRIDGE_ROLE に `gateway` か `machine` を                     書いてください(`agentgw install` が書きます)。"
                ),
                false => crate::t!(
                    "AGENTGW_BRIDGE_ROLE in .env is `{role}`, which is neither `gateway` nor `machine`.",
                    ".env の AGENTGW_BRIDGE_ROLE が `{role}` です。`gateway` か `machine` のどちらかです。"
                ),
            }),
        }
    }
}

/// The port machines link on. `AGENTGW_BRIDGE_PORT` when someone wanted a different one.
pub fn link_port(get: impl Fn(&str) -> Option<String>) -> String {
    get("AGENTGW_BRIDGE_PORT")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| gateway::DEFAULT_PORT.to_string())
}

/// The address a name has on this machine, asked of the OS. `None` when nothing answers.
///
/// Used at startup to turn the names in the links' dial URLs into addresses to open. Those names are
/// **ours** — the gateway's own LAN or tailnet name — so this resolver is the right one to ask.
pub fn address_of(name: &str) -> Option<String> {
    if name.is_empty() || name.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    let ask = |program: &str, args: &[&str]| -> Option<String> {
        let out = std::process::Command::new(program).args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).to_string())
    };
    // No resolver crate for one lookup: `getent` where there is one, `host` elsewhere (macOS has it)
    let said = match ask("getent", &["hosts", name]) {
        // `192.0.2.20   build-box.lan` — the address comes first
        Some(out) => out.lines().next()?.split_whitespace().next().map(str::to_string),
        // `build-box.lan has address 192.0.2.20`
        None => ask("host", &["-W", "2", name])?
            .lines()
            .find_map(|l| l.rsplit_once("has address "))
            .map(|(_, ip)| ip.trim().to_string()),
    };
    said.filter(|a| !a.is_empty())
}

/// Every address the gateway accepts machines on: **loopback always**, plus whatever each link asks
/// for. **Derived, never stored** — a machine that goes away takes its address with it, so nothing
/// piles up and nothing has to be swept.
///
/// Loopback is not optional: it is where a front that terminates TLS forwards to, and where an ssh
/// tunnel comes out.
pub fn listen_addrs(
    port: &str,
    access: &crate::bridge::state::Access,
    resolve: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    // **No machines, nothing to accept.** A lone Bridge opens no port at all
    if access.machines.is_empty() {
        return Vec::new();
    }
    let mut out = vec![format!("127.0.0.1:{port}")];
    for link in access.machines.values() {
        if let Some(addr) = link.required_address(&resolve)
            && !out.contains(&addr)
        {
            out.push(addr);
        }
    }
    out
}

/// The listener that accepts machines. Default is `0.0.0.0` (accept on any interface).
///
/// This Bridge does not terminate TLS itself. If a front (Tailscale / SSH tunnel / reverse proxy) is
/// in place, it terminates; if not, it accepts **plain `ws://`**. This link hands the Slack
/// bot token to machines, so exposing it in plain text is exactly that much risk.
/// The call is operational — when [`Listen::is_exposed`] is true, one warning line is logged at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listen {
    /// Every address to accept on. **More than one on purpose**: loopback for whatever terminates TLS in
    /// front (tailscale serve), the LAN address for machines on the same network — without opening
    /// everything the way `0.0.0.0` does.
    pub addrs: Vec<std::net::SocketAddr>,
    /// The shared secret machines present. Generated by `add-child`.
    pub token: String,
}

impl Listen {
    /// The first address, for the lines that name one place (a machine's own inlet has exactly one).
    pub fn addr(&self) -> std::net::SocketAddr {
        self.addrs[0]
    }

    /// Whether any of them is beyond loopback. **Only decides whether to warn**; nothing is refused.
    pub fn is_exposed(&self) -> bool {
        self.addrs.iter().any(|a| !a.ip().is_loopback())
    }
}

/// This process's wiring — upstream (where it gets from) and downstream (who it hands to).
///
/// **The mode has two axes, not a product.** If upstream is a gateway (= we are a machine), there can be no downstream
/// (that would make three tiers). This check is where the "no multi-tier" ruling lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wiring {
    pub upstream: Mode,
    /// Our own name. A machine announces it; a gateway uses it as the target of `pwd <own id>`.
    pub self_id: Option<String>,
    /// Set when accepting machines. **Without it, the usual standalone Bridge**.
    pub children: Option<Listen>,
}

impl Wiring {
    /// `resolve_name` turns a name in a link's dial URL into an address. Those names are **ours**, so
    /// this machine's resolver is the right one to ask; it is passed in so tests need no DNS.
    pub fn resolve(
        get: impl Fn(&str) -> Option<String>,
        access: &crate::bridge::state::Access,
        resolve_name: impl Fn(&str) -> Option<String>,
    ) -> Result<Wiring, String> {
        let v = |k: &str| {
            get(k)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let upstream = Mode::resolve(&get, access)?;
        let self_id = v("AGENTGW_BRIDGE_ID");
        if !matches!(upstream, Mode::Direct { .. }) {
            return Ok(Wiring {
                upstream,
                self_id,
                children: None,
            });
        }
        let addrs = listen_addrs(&link_port(&get), access, resolve_name);
        // **Without machines it is the usual standalone Bridge** — nothing to accept, nothing to open
        if addrs.is_empty() {
            return Ok(Wiring {
                upstream,
                self_id,
                children: None,
            });
        }
        let Some(token) = v("AGENTGW_LINK_TOKEN") else {
            return Err(crate::t!(
                "This is the gateway, but .env has no AGENTGW_LINK_TOKEN. Machines can't connect                  without a secret key; `agentgw add-machine` creates one.",
                "ここはゲートウェイですが、.env に AGENTGW_LINK_TOKEN がありません。秘密鍵が無いと                 マシンはつながれません。`agentgw add-machine` を実行すると作られます。"
            ));
        };
        // A gateway with no name can't be the target of `pwd <own id>`. It is not decided automatically
        if self_id.is_none() {
            return Err(crate::t!(
                "To accept machines, this gateway needs a name: set AGENTGW_BRIDGE_ID in .env.                  `pwd <name>:<path>` uses it, so it isn't chosen automatically.",
                "マシンを受け入れるには、このゲートウェイに名前が必要です。.env に AGENTGW_BRIDGE_ID を                 書いてください。`pwd <名前>:<パス>` で使う名前なので、自動では決めません。"
            ));
        }
        Ok(Wiring {
            upstream,
            self_id,
            children: Some(Listen {
                addrs: parse_listens(&addrs.join(","))?,
                token,
            }),
        })
    }
}

/// Read `host:port`. **Accepts on any interface** (default `0.0.0.0`).
///
/// It used to allow only loopback. Since it doesn't terminate TLS itself, it was built to always go through
/// a front (Tailscale / SSH tunnel / reverse proxy). **Opened up on 2026-08-02 by user decision** —
/// whether to put a front in is an operational choice. When opened,
/// [`Listen::is_exposed`] is true and one warning line is logged at startup.
/// One or more `host:port`, comma separated. **Every one of them is opened.**
fn parse_listens(listen: &str) -> Result<Vec<std::net::SocketAddr>, String> {
    let addrs = listen
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(parse_listen)
        .collect::<Result<Vec<_>, String>>()?;
    if addrs.is_empty() {
        return Err(crate::t!(
            "there is no address to accept machines on",
            "マシンを受け付けるアドレスがありません"
        ));
    }
    Ok(addrs)
}

fn parse_listen(listen: &str) -> Result<std::net::SocketAddr, String> {
    let (host, port) = listen.rsplit_once(':').ok_or_else(|| {
        crate::t!(
            "an address to accept machines on must be host:port, not {listen}",
            "マシンを受け付けるアドレスは host:port の形です。今の値: {listen}"
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

/// The role line of `status` — **always shown** (it's the first place to look when the config is wrong,
/// so staying silent on a machine leaves no clue). **The role is decided by `.env` alone** — it doesn't look at processes, so
/// it shows even when the Bridge isn't running (which is exactly when you want to read it).
/// If `Wiring::resolve` rejects the config, its reason is shown as is — the only place to learn why
/// a Bridge can't start without opening the logs.
pub fn role_line(
    env: &std::collections::HashMap<String, String>,
    access: &crate::bridge::state::Access,
) -> String {
    let wiring = match Wiring::resolve(|k| env.get(k).cloned(), access, address_of) {
        Ok(w) => w,
        Err(why) => {
            let why = why.lines().next().unwrap_or("");
            return crate::t!("Role: can't tell — {why}", "役割: 判定できません — {why}");
        }
    };
    let name = wiring
        .self_id
        .as_deref()
        .map(|n| crate::t!(" \"{n}\"", "「{n}」"))
        .unwrap_or_default();
    match (&wiring.upstream, &wiring.children) {
        (Mode::Direct { .. }, Some(l)) => {
            // Every address, not just the first: a gateway opens one per way in that a machine needs
            let addr = l.addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ");
            crate::t!(
                "Role: gateway{name} — connected to Slack, accepts machines on {addr}",
                "役割: ゲートウェイ{name} — Slack に接続、マシンを {addr} で受け付け"
            )
        }
        (Mode::Direct { .. }, None) => crate::t!(
            "Role: gateway{name} — connected to Slack, no other machines",
            "役割: ゲートウェイ{name} — Slack に接続、ほかのマシンなし"
        ),
        (Mode::Relay { url, .. }, _) => crate::t!(
            "Role: machine{name} — connects to the gateway at {url}",
            "役割: マシン{name} — ゲートウェイ {url} につなぐ"
        ),
    }
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

    // ── Mode selection ──────────────────────────────────────────────────────

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    use crate::bridge::state::{Access, Link};

    /// A gateway with these machines recorded.
    fn with_machines(pairs: &[(&str, Link)]) -> Access {
        Access {
            machines: pairs.iter().map(|(k, l)| (k.to_string(), l.clone())).collect(),
            ..Default::default()
        }
    }

    fn a_link(kind: &str, url: &str) -> Link {
        Link {
            kind: kind.into(),
            link_url: url.into(),
            ..Default::default()
        }
    }

    /// `hub.lan` is ours, and it is the only name this test's resolver knows.
    fn here(name: &str) -> Option<String> {
        (name == "hub.lan").then(|| "192.0.2.10".to_string())
    }

    #[test]
    fn the_gateway_is_the_one_holding_the_slack_tokens() {
        assert_eq!(
            Mode::resolve(
                env(&[
                    ("AGENTGW_BRIDGE_ROLE", "gateway"),
                    ("SLACK_APP_TOKEN", "xapp-1"),
                    ("SLACK_BOT_TOKEN", "xoxb-1")
                ]),
                &Access::default()
            )
            .unwrap(),
            Mode::Direct {
                app_token: "xapp-1".into(),
                bot_token: "xoxb-1".into()
            }
        );
        // Half the tokens is not a different role, it is a broken gateway
        let e = Mode::resolve(
            env(&[("AGENTGW_BRIDGE_ROLE", "gateway"), ("SLACK_APP_TOKEN", "xapp-1")]),
            &Access::default(),
        )
        .unwrap_err();
        assert!(e.contains("no Slack tokens"), "{e}");
    }

    /// **The role is written down, not guessed.** It used to be inferred from which keys happened to be
    /// present, so a half-finished `.env` read as a different role instead of as an error.
    #[test]
    fn a_bridge_that_does_not_say_what_it_is_refuses_to_start() {
        let e = Mode::resolve(env(&[("SLACK_APP_TOKEN", "xapp-1")]), &Access::default()).unwrap_err();
        assert!(e.contains("AGENTGW_BRIDGE_ROLE"), "{e}");
        // **`Gateway` still works** — only a real typo is refused, and it is quoted back as written
        assert!(
            Mode::resolve(
                env(&[
                    ("AGENTGW_BRIDGE_ROLE", "Gateway"),
                    ("SLACK_APP_TOKEN", "xapp-1"),
                    ("SLACK_BOT_TOKEN", "xoxb-1")
                ]),
                &Access::default()
            )
            .is_ok()
        );
        let typo = Mode::resolve(env(&[("AGENTGW_BRIDGE_ROLE", "Gatway")]), &Access::default())
            .unwrap_err();
        assert!(typo.contains("`Gatway`"), "{typo}");
    }

    #[test]
    fn a_machine_dials_the_gateway_its_record_names() {
        let access = Access {
            gateway: Some(a_link(Link::TAILSCALE, "wss://r")),
            ..Default::default()
        };
        assert_eq!(
            Mode::resolve(
                env(&[
                    ("AGENTGW_BRIDGE_ROLE", "machine"),
                    ("AGENTGW_LINK_TOKEN", "s"),
                    ("AGENTGW_BRIDGE_ID", "desktop"),
                ]),
                &access
            )
            .unwrap(),
            Mode::Relay {
                url: "wss://r".into(),
                api_token: "s".into(),
                bridge_id: "desktop".into()
            }
        );
        // No record = it doesn't know where its gateway is
        let lost = Mode::resolve(
            env(&[
                ("AGENTGW_BRIDGE_ROLE", "machine"),
                ("AGENTGW_LINK_TOKEN", "s"),
                ("AGENTGW_BRIDGE_ID", "desktop"),
            ]),
            &Access::default(),
        )
        .unwrap_err();
        assert!(lost.contains("doesn't know its gateway"), "{lost}");
    }

    /// **No automatic naming.** Falling back to `default` makes multiple machines all collide.
    #[test]
    fn a_machine_without_a_name_refuses_to_start() {
        let access = Access {
            gateway: Some(a_link(Link::TAILSCALE, "wss://r")),
            ..Default::default()
        };
        let e = Mode::resolve(
            env(&[("AGENTGW_BRIDGE_ROLE", "machine"), ("AGENTGW_LINK_TOKEN", "s")]),
            &access,
        )
        .unwrap_err();
        assert!(e.contains("AGENTGW_BRIDGE_ID"), "{e}");
        assert!(e.contains("take each other's messages"), "{e}");
    }

    /// An empty string is the same as absent. A .env left with just `SLACK_APP_TOKEN=` doesn't block startup.
    #[test]
    fn an_empty_value_counts_as_absent() {
        let access = Access {
            gateway: Some(a_link(Link::TAILSCALE, "wss://r")),
            ..Default::default()
        };
        assert!(matches!(
            Mode::resolve(
                env(&[
                    ("AGENTGW_BRIDGE_ROLE", "machine"),
                    ("SLACK_APP_TOKEN", "   "),
                    ("AGENTGW_LINK_TOKEN", "s"),
                    ("AGENTGW_BRIDGE_ID", "desktop"),
                ]),
                &access
            ),
            Ok(Mode::Relay { .. })
        ));
    }

    // ── Wiring (upstream × downstream) ──────────────────────────────────────

    fn parent(extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = [
            ("AGENTGW_BRIDGE_ROLE", "gateway"),
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

    fn wire(pairs: &[(String, String)], access: &Access) -> Result<Wiring, String> {
        let owned: Vec<(String, String)> = pairs.to_vec();
        Wiring::resolve(
            move |k| {
                owned
                    .iter()
                    .find(|(key, _)| key == k)
                    .map(|(_, v)| v.clone())
            },
            access,
            here,
        )
    }

    /// With no machines recorded there is nothing to accept — **the usual standalone Bridge**, and no
    /// port opened at all. This is the main regression guard.
    #[test]
    fn a_bridge_with_no_machines_opens_nothing() {
        let w = wire(&parent(&[]), &Access::default()).unwrap();
        assert!(w.children.is_none());
        assert!(matches!(w.upstream, Mode::Direct { .. }));
    }

    /// **Loopback always, plus what each link asks for.** Derived from the records every start-up, so a
    /// machine that goes away takes its address with it.
    #[test]
    fn the_open_addresses_come_from_the_machines() {
        let access = with_machines(&[
            // A front terminates this one and hands it to our loopback: nothing extra to open
            ("fronted", a_link(Link::TAILSCALE, "wss://hub.example.ts.net")),
            ("lan", a_link(Link::LAN, "ws://hub.lan:8787")),
            // Its exit is our loopback too
            ("tunnelled", a_link(Link::TUNNEL, "ws://127.0.0.1:8799")),
        ]);
        let w = wire(&parent(&[("AGENTGW_LINK_TOKEN", "s3cret")]), &access).unwrap();
        let l = w.children.unwrap();
        let addrs: Vec<String> = l.addrs.iter().map(|a| a.to_string()).collect();
        assert_eq!(addrs, vec!["127.0.0.1:8787", "192.0.2.10:8787"]);
        assert_eq!(l.token, "s3cret");
        assert_eq!(w.self_id.as_deref(), Some("vps"));
        assert!(l.is_exposed());

        // Take the LAN machine away and its address goes with it
        let alone = with_machines(&[("fronted", a_link(Link::TAILSCALE, "wss://hub.example.ts.net"))]);
        let w = wire(&parent(&[("AGENTGW_LINK_TOKEN", "s3cret")]), &alone).unwrap();
        let l = w.children.unwrap();
        assert_eq!(l.addr().to_string(), "127.0.0.1:8787");
        assert!(!l.is_exposed());
    }

    /// A different port is asked for in one place, not spelled into every address.
    #[test]
    fn the_port_is_asked_for_once() {
        let access = with_machines(&[("lan", a_link(Link::LAN, "ws://hub.lan:8788"))]);
        let w = wire(
            &parent(&[("AGENTGW_LINK_TOKEN", "s3cret"), ("AGENTGW_BRIDGE_PORT", "8788")]),
            &access,
        )
        .unwrap();
        let addrs: Vec<String> = w.children.unwrap().addrs.iter().map(|a| a.to_string()).collect();
        assert_eq!(addrs, vec!["127.0.0.1:8788", "192.0.2.10:8788"]);
    }

    #[test]
    fn a_gateway_with_machines_but_no_key_refuses_to_start() {
        let access = with_machines(&[("lan", a_link(Link::LAN, "ws://hub.lan:8787"))]);
        let e = wire(&parent(&[]), &access).unwrap_err();
        assert!(e.contains("AGENTGW_LINK_TOKEN"), "{e}");
    }

    /// A gateway with no name can't be the target of `pwd <own id>`. **Not decided automatically.**
    #[test]
    fn a_parent_without_a_name_refuses_to_start() {
        let mut pairs = parent(&[("AGENTGW_LINK_TOKEN", "s3cret")]);
        pairs.retain(|(k, _)| k != "AGENTGW_BRIDGE_ID");
        let access = with_machines(&[("lan", a_link(Link::LAN, "ws://hub.lan:8787"))]);
        let e = wire(&pairs, &access).unwrap_err();
        assert!(e.contains("AGENTGW_BRIDGE_ID"), "{e}");
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
        let (_up, up_rx) = tokio::sync::mpsc::unbounded_channel();
        let l = RelayLink::new(
            "wss://relay.example/",
            "tok",
            "desktop",
            Arc::new(tokio::sync::Mutex::new(up_rx)),
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(l.url, "wss://relay.example");
        assert_eq!(
            format!("{}{}", l.url, link::path_for(&l.bridge_id)),
            "wss://relay.example/bridge/desktop"
        );
    }

    /// The first line of `status`. **Staying silent with the role misread is the worst**, so every shape
    /// and "can't tell" are pinned.
    #[test]
    fn role_line_names_the_role() {
        let env = |pairs: &[(&str, &str)]| -> std::collections::HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let line = |pairs: &[(&str, &str)], access: &Access| role_line(&env(pairs), access);

        let solo = line(
            &[
                ("AGENTGW_BRIDGE_ROLE", "gateway"),
                ("SLACK_APP_TOKEN", "xapp-1"),
                ("SLACK_BOT_TOKEN", "xoxb-1"),
            ],
            &Access::default(),
        );
        assert!(solo.starts_with("Role: gateway — connected to Slack, no other machines"), "{solo}");

        let parent = line(
            &[
                ("AGENTGW_BRIDGE_ROLE", "gateway"),
                ("SLACK_APP_TOKEN", "xapp-1"),
                ("SLACK_BOT_TOKEN", "xoxb-1"),
                ("AGENTGW_BRIDGE_ID", "mac"),
                ("AGENTGW_LINK_TOKEN", "k"),
            ],
            &with_machines(&[("lan", a_link(Link::LAN, "ws://hub.lan:8787"))]),
        );
        assert!(parent.contains("Role: gateway \"mac\""), "{parent}");
        // `role_line` asks the real resolver, which knows nothing of this test's names — loopback is
        // what is certain, and that it says every address rather than one is what matters here
        assert!(parent.contains("accepts machines on 127.0.0.1:8787"), "{parent}");

        let dialing = line(
            &[
                ("AGENTGW_BRIDGE_ROLE", "machine"),
                ("AGENTGW_LINK_TOKEN", "k"),
                ("AGENTGW_BRIDGE_ID", "laptop"),
            ],
            &Access {
                gateway: Some(a_link(Link::TAILSCALE, "wss://p.example")),
                ..Default::default()
            },
        );
        assert!(dialing.contains("Role: machine \"laptop\""), "{dialing}");
        assert!(dialing.contains("wss://p.example"), "{dialing}");

        // A config that can't start is exactly when status needs to give the reason (without opening the logs)
        let broken = line(&[], &Access::default());
        assert!(broken.starts_with("Role: can't tell"), "{broken}");
    }
}
