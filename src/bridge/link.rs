//! Bridge 側の口 — Relay Server へ**外向きに** dial し、自分の担当チャンネルのイベントを受け取る。
//!
//! 外向きなのが要点で、だから家庭用ルータのポート開放も VPN も要らない(カフェの Wi-Fi でも
//! テザリングでも繋がる)。
//!
//! > **リンクが切れても、ワーカーには一切触らない。**
//! > この module はワーカーへの口を**渡されていない** — それは意図的な設計。Bridge のもう一方の
//! > リンク(ワーカーの生死)は落ちたら本当に「そのワーカーが死んだ」を意味するが、その反射を
//! > ここに持ち込むと、2秒の Wi-Fi の瞬きが、生きて作業中のワーカーを殺すことになる。
//! > Relay を失って起きるのはこれだけ: **戻るまで新しい Slack メッセージが来ない。**
//! > 走っているものは走り続け、その下で繋ぎ直す。壊れた電話線は死んだ人ではない。
//!
//! 話し合いでは解決しない断り(トークンが違う・版が違う)は**大きな声で報告する**。ただし
//! **プロセスは落とさない** — 落ちると supervisor がすぐ起こし直し、その連打が systemd の
//! 起動レート制限(既定 10秒に5回)を踏んで unit を `failed` のまま放置する。実際 2026-08-03 に
//! 子がこれで5時間15分止まった。デプロイ中の一瞬の 401 で子が恒久的に上がってこなくなる。
//! 直らない断りでも間を空けて繋ぎ直し続け、直された瞬間に自力で戻る。

use crate::bridge::relay::wire::{self, LinkFrame};
use crate::bridge::state::LogCtx;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const RECONNECT_MIN_MS: u64 = 1_000;
pub const RECONNECT_MAX_MS: u64 = 30_000;

/// 無通信がこれだけ続いたら Ping を1発。次の窓でも無音なら死んだものとして切る。
pub const LINK_IDLE_MS: u64 = 30_000;

/// 無通信の窓が閉じたときにやること。
#[derive(Debug, PartialEq, Eq)]
pub enum Idle {
    /// 生きているか確かめる。
    Ping,
    /// 前の Ping に返事が無かった。切って繋ぎ直す。
    Dead,
}

/// リンクが死んでいないかを見る番人。**half-open を見つける唯一の手段。**
///
/// 相手が FIN を出さずに消えると(ネット断・サスペンド・NAT のエントリ落ち)、ソケットは
/// `ESTAB` のまま残り、受信は**永久に返らない**。Close も Err も来ないので、どの受信ループも
/// 抜けられず、その先にある再接続に一生たどり着かない(2026-08-03 に子が74分沈黙した)。
/// WebSocket の Ping は tungstenite も axum も**受けた分に Pong を返すだけ**で、自分からは
/// 打たない。TCP keepalive も既定では off。だから、こちらから叩いて確かめるしかない。
#[derive(Default)]
pub struct IdleWatch {
    pinged: bool,
}

impl IdleWatch {
    /// 何か届いた / 送れた。生きている。
    pub fn on_traffic(&mut self) {
        self.pinged = false;
    }

    /// 無通信のまま窓が閉じた。
    pub fn on_idle(&mut self) -> Idle {
        if self.pinged {
            Idle::Dead
        } else {
            self.pinged = true;
            Idle::Ping
        }
    }
}

/// link から1フレーム読んだ結果。[`LinkRead`] を埋める側が返す。
pub enum Frame {
    /// 本文。
    Text(String),
    /// 本文以外(Pong など)。中身は要らないが、**届いた事実**が生存の証拠。
    Other,
    /// 相手が閉じた / 壊れた。理由は人に見せる1行。
    Closed(String),
}

/// 1フレーム読む口。**タイムアウトは付けない** — 番人([`beat`])が外から被せる。
///
/// axum と tungstenite で読み口の名前が違うだけなので、実装はその差を埋めるだけにする。
/// **見張りの時計と状態機械は [`beat`] に1つしか無い**(4か所に写していた頃の再発防止)。
pub trait LinkRead {
    fn read_frame(&mut self) -> impl std::future::Future<Output = Frame> + Send;
}

/// 番人つきの受信の結果。
///
/// **`Ping` を握り潰さないこと。** ここを無視すると、相手が黙って消えた link
/// (half-open)を永久に見逃す。match の網羅がそれを強制する。
pub enum Beat {
    /// 本文が届いた。
    Text(String),
    /// 本文以外が届いた。生きている。
    Alive,
    /// 無通信が続いた。**呼び手は Ping を1発打って、次を待つ。**
    Ping,
    /// 切れた。理由つき。呼び手はループを抜ける。
    Gone(String),
}

/// **link の受信は、必ずこれを通す。** 素の `next()` / `recv()` を直に待つと、
/// 相手が FIN を出さずに消えたとき Close も Err も来ないまま永久に返らない
/// (2026-08-03、子が74分沈黙)。
pub async fn beat<S: LinkRead>(socket: &mut S, watch: &mut IdleWatch) -> Beat {
    beat_within(socket, watch, LINK_IDLE_MS).await
}

/// [`beat`] の中身。窓の広さを渡せるのは**テストのため**だけ — 呼び手は `beat` を使う。
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
            Some(Ok(_)) => Frame::Other, // ping/pong などは tungstenite が処理する
            Some(Err(e)) => Frame::Closed(e.to_string()),
        }
    }
}

/// 握手が拒まれた理由のうち、**呼ぶ側が態度を変えるべきもの**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fatal {
    /// 401 — api トークンが違う。再起動しても直らない。人が設定を直すしかない。
    BadToken,
    /// 426 — 版が違う。**このマシンの新しいコードで再起動すれば治る**種類。
    /// (Bun が `RejectReason` で区別していた唯一の中身がこれ。)
    WrongVersion,
    /// 400 — 名乗りが通らない(Bridge ID が空 / 使えない字)。設定を直すしかない。
    BadBridgeId,
}

impl Fatal {
    /// HTTP のステータスから。**それ以外は fatal ではない** — 繋がらないだけなので黙って再試行する。
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

/// Relay から届いたもののうち、Bridge が使うもの。
pub enum FromRelay {
    /// 受理された。bot トークン(**メモリだけ**)と、いまの home。
    Ready {
        bot_token: String,
        home: Option<String>,
    },
    /// Slack のイベント1つ。
    Event {
        name: String,
        event: serde_json::Value,
    },
    /// ボタン1押し。
    Action {
        action: serde_json::Value,
        body: serde_json::Value,
    },
    /// Owner がこのマシンをある場所の担当に決めた。
    Linked {
        owner_user_id: String,
        channel: String,
        thread_ts: String,
    },
    /// 話し合いで解決しない断り。**再試行しない。**
    Fatal(Fatal),
}

/// 親から届いたフレームは、そのまま上流の知らせになる。**dial した側でも迎えられた側でも
/// 同じ受け皿**に流すための橋(`Fatal` だけはこちら側の事情なので `From` に無い)。
impl From<crate::bridge::relay::wire::LinkFrame> for FromRelay {
    fn from(frame: crate::bridge::relay::wire::LinkFrame) -> Self {
        use crate::bridge::relay::wire::LinkFrame as F;
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

    /// 繋ぎ続ける。届いたものを `tx` に流す。**戻ってこない**(`stop()` されたときだけ戻る)。
    pub async fn run(&self, tx: tokio::sync::mpsc::Sender<FromRelay>) {
        let mut backoff = RECONNECT_MIN_MS;
        while !self.stopped.load(Ordering::SeqCst) {
            match self.connect_once(&tx).await {
                // 握手が通った回。次の再接続は短い待ちから始めてよい
                Ok(true) => backoff = RECONNECT_MIN_MS,
                Ok(false) => {}
                // 話し合いでは解決しない断り。**大きな声で言うが、諦めない** — ここで
                // 抜けるとプロセスが落ち、supervisor の連打が systemd の起動レート制限を
                // 踏んで unit が `failed` のまま残る(2026-08-03、子が5時間15分停止)。
                // 人が直すまで間を空けて繋ぎ直し続ければ、直された瞬間に自力で戻る
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

    /// 1回の接続。`Ok(true)` = 握手まで通った(その後切れた)、`Ok(false)` = 繋がらなかった。
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
                // **断られた**のか、**繋がらなかった**のかを分ける。後者は黙って再試行する
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
            // **受信は `beat` 経由だけ。** 直に `next()` を待つと half-open で永久に止まる
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
                return Ok(true); // Bridge 本体が終わった
            }
        }
        // ここが「反射でワーカーを片付けたくなる」場所。**やらない。**
        // リンクが落ちたのは、Slack の流れが止まったという意味でしかない。
        Ok(true)
    }
}

/// upgrade の3点(パス / `Authorization` / `Sec-WebSocket-Protocol`)を載せた要求を組む。
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

/// 次の再接続までの待ち。握手が通ったら短い待ちに戻す。
pub fn next_backoff(current_ms: u64, handshake_succeeded: bool) -> u64 {
    if handshake_succeeded {
        RECONNECT_MIN_MS
    } else {
        (current_ms * 2).min(RECONNECT_MAX_MS)
    }
}

// ── どちらのモードで動くか(排他) ─────────────────────────────────────

/// Bridge の入口は2つあり、**同時に開いてはいけない**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// 直結 — 自分で Slack の Socket Mode を開く。
    Direct {
        app_token: String,
        bot_token: String,
    },
    /// 親経由 — Slack には触らず、親へ dial する。bot トークンは握手で貰う。
    Relay {
        url: String,
        api_token: String,
        bridge_id: String,
    },
    /// 親経由(**迎えに来てもらう**)— こちらからは dial せず、親の接続を口で待つ。
    /// 親が NAT の内側にいて子から繋ぎに行けない構成のためだけに在る。
    /// フレームの向きは変わらない(親→子)。
    AwaitParent,
}

impl Mode {
    /// env からモードを決める。**両方揃っていたら起動を拒否する。**
    ///
    /// 黙って両方繋ぐと、Slack が同じ app トークンの2人目に対して負荷分散を始め、
    /// この設計が防いでいる split-brain がそのまま戻る。だから既定でどちらかに寄せるのではなく、
    /// **大きな声で断る**。
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

        // **1つの子につき link は1本。** 両方向あると同じイベントが二重に配られる
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
            // **自動命名はしない。** ホスト名にも `default` にも落とさない —
            // 名前が衝突したマシンは、互いの Slack メッセージを奪い合う
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
        // Slack のトークンが無く、口だけがある = 親に迎えに来てもらう子
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

/// 子を迎える口。既定は `0.0.0.0`(どのインターフェースでも受ける)。
///
/// この Bridge は TLS を自分で終端しない。前段(Tailscale / SSH トンネル / reverse proxy)を
/// 置くならその前段が終端し、置かないなら **`ws://` の平文**で受ける。この link は Slack の
/// bot トークンを子に配るので、平文で外に出すぶんはそのまま危険度になる。
/// 判断は運用の側 — [`Listen::is_exposed`] が真のときは起動時に1行警告を出す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listen {
    pub addr: std::net::SocketAddr,
    /// 子が提示する共有の秘密。`add-child` が生成する。
    pub token: String,
}

impl Listen {
    /// loopback の外に出ているか。**警告を出すためだけの判定**で、拒否はしない。
    pub fn is_exposed(&self) -> bool {
        !self.addr.ip().is_loopback()
    }
}

/// このプロセスの配線 — 上流(どこから貰うか)と下流(誰に渡すか)。
///
/// **モードは2軸で、掛け算ではない。** 上流が親(= 自分は子)なら下流は持てない
/// (3階層になる)。この検査が「多段は作らない」という裁定の置き場所。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wiring {
    pub upstream: Mode,
    /// 自分の名前。子はこれを名乗り、親は `route <自分の id>` の指名先として使う。
    pub self_id: Option<String>,
    /// 子を迎えるなら。**無ければ今までどおりの単独 Bridge**。
    pub children: Option<Listen>,
    /// 親に迎えに来てもらう子の口。`children` とは排他(どちらも `AGENTGW_LINK_LISTEN` を読むが、
    /// 開ける相手が違う — こちらは**上流**が入ってくる口)。
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
        // 親に迎えに来てもらう子は、この口を**親のために**開ける(子を持つのではない)。
        // 3階層(親へ dial しながら子を持つ)は Mode::resolve が既に断っている
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
        // 名前が無い親は `route <自分の id>` の指名先になれない。自動では決めない
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

/// `host:port` を読む。**どのインターフェースでも受ける**(既定は `0.0.0.0`)。
///
/// 以前は loopback しか許さなかった。TLS を自分で終端しないので、前段(Tailscale /
/// SSH トンネル / reverse proxy)を必ず通す作りにしていたためだ。**2026-08-02 に
/// ユーザー判断で開けた** — 前段を置くかどうかは運用の側で決める。開けたときは
/// [`Listen::is_exposed`] が真になり、起動時に1行警告が出る。
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

/// 親から**迎えに行く**子の一覧。`AGENTGW_CHILD_URLS=desktop=wss://a,laptop=wss://b`。
///
/// **既定は子から dial** — この設定を書くのは、親が NAT の内側にいて子から繋ぎに行けない
/// ときだけ。**自動フォールバックにしない**(親が子の URL を持っている時点で選択は済んでいる。
/// 「まず待って駄目なら繋ぐ」にすると、繋がらない子と設定していない子を区別できなくなる)。
pub fn child_urls(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|pair| {
            let (id, url) = pair.split_once('=')?;
            let (id, url) = (id.trim(), url.trim().trim_end_matches('/'));
            (!id.is_empty() && !url.is_empty()).then(|| (id.to_string(), url.to_string()))
        })
        .collect()
}

/// `.env` に Relay の設定を書き、**直結のトークンを畳む**。
///
/// 3つを手で書かせるのは、間違いを作る機会が3回あるということ。接続文字列1本から起こす。
/// `SLACK_APP_TOKEN` をコメントアウトするのが要 — 残っていると [`Mode::resolve`] が
/// 起動を拒否する(そしてそれは正しい)。
pub fn apply_connection(env_text: &str, conn: &wire::Invite, bridge_id: &str) -> String {
    // 直結の口を閉じる。**消さずにコメントにする** — 戻したくなる日のために
    let folded: Vec<String> = env_text
        .lines()
        .map(|line| match line.split_once('=').map(|(k, _)| k.trim()) {
            Some("SLACK_APP_TOKEN") => format!("# (relay mode) {line}"),
            _ => line.to_string(),
        })
        .collect();
    set_env_keys(
        &folded.join("\n"),
        &[
            ("AGENTGW_RELAY_URL", conn.url.clone()),
            ("AGENTGW_RELAY_TOKEN", conn.api_token.clone()),
            ("AGENTGW_BRIDGE_ID", bridge_id.to_string()),
        ],
    )
}

/// `.env` のキーを**置換で**書き入れる(無ければ末尾に足す)。他の行は1文字も触らない。
///
/// 積み増しにすると `load_env` は後勝ちで読むので、消したはずの値が残り続ける。
pub fn set_env_keys(env_text: &str, pairs: &[(&str, String)]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut seen = vec![false; pairs.len()];
    for line in env_text.lines() {
        let key = line.split_once('=').map(|(k, _)| k.trim());
        match pairs.iter().position(|(k, _)| Some(*k) == key) {
            Some(i) => {
                seen[i] = true;
                out.push(format!("{}={}", pairs[i].0, pairs[i].1));
            }
            None => out.push(line.to_string()),
        }
    }
    for (i, (k, v)) in pairs.iter().enumerate() {
        if !seen[i] {
            out.push(format!("{k}={v}"));
        }
    }
    let mut text = out.join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// `agentgw link [--name <名前>] [<接続文字列>|-]` — 接続文字列1本を `.env` に落とす。
///
/// **引数が `-`、または引数が無く stdin が端末でないときは stdin から読む。** ssh 越しに
/// 渡すとき argv に置くと、相手の `ps` とシェル履歴に秘密が残る。
///
/// `--name` が要るのは、その stdin から読む経路だ — 接続文字列がパイプで来ていると、
/// 名前を訊く口([`prompt_bridge_id`])が塞がっている。名前は秘密ではないので argv でよい。
pub fn cli(args: &[String], dir: &crate::bridge::state::StateDir) -> i32 {
    use std::io::{IsTerminal, Read};
    let mut named: Option<String> = None;
    let mut rest: Vec<&str> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => named = it.next().map(|s| s.trim().to_string()),
            s if s.starts_with("--name=") => {
                named = Some(s.trim_start_matches("--name=").trim().to_string());
            }
            s => rest.push(s),
        }
    }
    let named = named.filter(|s| !s.is_empty());
    let arg = rest.first().copied();
    let raw = match arg {
        Some("-") | None if !std::io::stdin().is_terminal() => {
            let mut buf = String::new();
            if std::io::stdin().read_to_string(&mut buf).is_err() {
                eprintln!("link: {}", crate::t!("couldn't read standard input", "標準入力を読めません"));
                return 1;
            }
            buf
        }
        Some(s) if s != "-" => s.to_string(),
        _ => {
            eprintln!(
                "{}",
                crate::t!(
                    "usage: agentgw link <connection string>\n       \
                     agentgw link --name <name> -   (read from standard input)",
                    "usage: agentgw link <接続文字列>\n       \
                     agentgw link --name <名前> -   (標準入力から読む)"
                )
            );
            return 2;
        }
    };
    let conn = match wire::decode_connection(&raw) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("link: {e}");
            return 1;
        }
    };
    // 名前は明示。**自動命名はしない**(衝突したマシンは互いの Slack メッセージを奪い合う)
    let bridge_id = match named
        .or_else(|| std::env::var("AGENTGW_BRIDGE_ID").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(id) => id,
        None => match prompt_bridge_id() {
            Some(id) => id,
            None => {
                eprintln!(
                    "link: {}",
                    crate::t!(
                        "this machine needs a name. Pass `--name <name>` or set AGENTGW_BRIDGE_ID.",
                        "このマシンに名前が必要です。`--name <名前>` を付けるか、AGENTGW_BRIDGE_ID を設定してください。"
                    )
                );
                return 1;
            }
        },
    };
    let env_path = dir.path().join(".env");
    let before = std::fs::read_to_string(&env_path).unwrap_or_default();
    let after = apply_connection(&before, &conn, &bridge_id);
    if let Err(e) = crate::bridge::state::write_atomic_mode(&env_path, &after, Some(0o600)) {
        eprintln!("link: {}", crate::t!("couldn't write {}: {e}", "{} に書けません: {e}", env_path.display()));
        return 1;
    }
    println!("{}", crate::t!("Saved: {}", "保存しました: {}", env_path.display()));
    println!("  AGENTGW_RELAY_URL={}", conn.url);
    println!("  AGENTGW_BRIDGE_ID={bridge_id}");
    if before.lines().any(|l| l.starts_with("SLACK_APP_TOKEN=")) {
        println!(
            "{}",
            crate::t!(
                "  Commented out SLACK_APP_TOKEN: this machine now goes through the gateway instead of connecting to Slack itself.",
                "  SLACK_APP_TOKEN をコメントアウトしました。このマシンは自分で Slack につながず、ゲートウェイを通します。"
            )
        );
    }
    0
}

pub(crate) fn prompt_bridge_id() -> Option<String> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let host = std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let default = if host.is_empty() {
        String::new()
    } else {
        format!(" [{host}]")
    };
    print!("{}", crate::t!("Name for this machine{default}: ", "このマシンの名前{default}: "));
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok()?;
    let typed = line.trim();
    // 空 Enter は「表示した候補でよい」の意思表示。何も候補が無ければ None
    let id = if typed.is_empty() {
        host
    } else {
        typed.to_string()
    };
    (!id.is_empty()).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 断られた理由のうち、**態度を変えるべきものだけ**が fatal。

    #[test]
    fn only_a_refusal_we_cannot_talk_our_way_out_of_is_fatal() {
        assert_eq!(Fatal::of(401), Some(Fatal::BadToken));
        assert_eq!(Fatal::of(426), Some(Fatal::WrongVersion));
        assert_eq!(Fatal::of(400), Some(Fatal::BadBridgeId));
        // 繋がらないだけ / 相手が落ちている = 黙って再試行する
        for status in [404, 500, 502, 503, 301, 200] {
            assert_eq!(Fatal::of(status), None, "{status}");
        }
    }

    /// 無通信2回で死んだと見なす。**1回目では切らない** — 静かなだけの link を
    /// 30秒ごとに切ると、繋ぎ直しの嵐になる。
    #[test]
    fn two_silent_windows_mean_the_link_is_dead() {
        let mut w = IdleWatch::default();
        assert_eq!(w.on_idle(), Idle::Ping); // 1回目: 叩いて確かめる
        assert_eq!(w.on_idle(), Idle::Dead); // 2回目: 返事が無い = 死んでいる
    }

    /// Pong でも配達でも、**何か届けば生きている**。番人はそこで数え直す。
    #[test]
    fn any_traffic_clears_the_watch() {
        let mut w = IdleWatch::default();
        assert_eq!(w.on_idle(), Idle::Ping);
        w.on_traffic(); // Pong が返ってきた
        assert_eq!(w.on_idle(), Idle::Ping); // また1回目から
        w.on_traffic();
        w.on_traffic();
        assert_eq!(w.on_idle(), Idle::Ping);
        assert_eq!(w.on_idle(), Idle::Dead);
    }

    /// 何も返さないソケット = 相手が黙って消えた状態(half-open)。
    struct SilentSocket;
    impl LinkRead for SilentSocket {
        async fn read_frame(&mut self) -> Frame {
            std::future::pending().await // 永久に返らない — これが74分の沈黙の正体
        }
    }

    /// 2回目の読みだけ本文を返し、あとは黙るソケット(黙る→届く→黙る、の順を作る)。
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

    /// **`beat` は永久に待たない。** 黙ったままなら Ping を促し、次の窓で切ると言う。
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

    /// 届いたら本文を返し、**番人を数え直す** — 生きている link を切らないための要。
    /// 数え直しが無いと、Ping 済みの直後に1窓黙っただけで切ってしまう。
    #[tokio::test]
    async fn a_frame_arrives_and_resets_the_watch() {
        let mut s = SilentThenText(0);
        let mut w = IdleWatch::default();
        // 1窓目は無音 → Ping を促す
        assert!(matches!(beat_within(&mut s, &mut w, 10).await, Beat::Ping));
        // そこへ本文が届く(Pong でも同じ) → 生きているので数え直す
        match beat_within(&mut s, &mut w, 10).await {
            Beat::Text(t) => assert_eq!(t, "hello"),
            _ => panic!("本文が来るはず"),
        }
        // **数え直したので、また Ping から。** ここが Gone なら数え直せていない
        assert!(matches!(beat_within(&mut s, &mut w, 10).await, Beat::Ping));
        assert!(matches!(
            beat_within(&mut s, &mut w, 10).await,
            Beat::Gone(_)
        ));
    }

    /// 版違いだけは「このマシンを新しくして再起動すれば治る」と言い分ける。
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
        // 一度でも握手が通れば、次の待ちは短いところから
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

    // ── モード選択 ──────────────────────────────────────────────────────────

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
        // URL だけ / トークンだけ = 設定が途中。分かる文面で断る
        let half = Mode::resolve(env(&[("AGENTGW_RELAY_URL", "wss://r")])).unwrap_err();
        assert!(half.contains("half set up"), "{half}");
    }

    /// **自動命名しない。** `default` に落ちると、複数マシンが揃って衝突する。
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

    /// ** **: 両方揃った設定は起動しない。黙って両方繋ぐと split-brain が戻る。
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

    /// 空文字は「無い」と同じ。`SLACK_APP_TOKEN=` だけ残った .env で起動を拒まれない。
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

    // ── 配線(上流 × 下流) ─────────────────────────────────────────────────

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

    /// 子を迎える設定が無いものは、**今までどおりの単独 Bridge**。ここが回帰の本丸。
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

    /// **公開インターフェースにも bind する**(2026-08-02、ユーザー判断)。拒否はしないが、
    /// loopback の外に出たことは `is_exposed` が立てて起動時に1行警告になる。
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

    /// 名前の無い親は `route <自分の id>` の指名先になれない。**自動では決めない。**
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

    /// **1つの子につき link は1本。** 自分から dial しながら迎えにも来てもらう設定は、
    /// 同じイベントが二重に届く(= 3階層を作ろうとした設定もここで止まる)。
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

    /// Slack のトークンが無く、口だけある = **親に迎えに来てもらう子**。
    #[test]
    fn a_bridge_with_only_an_inlet_waits_for_its_parent() {
        let w = wire(&[
            ("AGENTGW_BRIDGE_ID".into(), "desktop".into()),
            ("AGENTGW_LINK_LISTEN".into(), "127.0.0.1:8787".into()),
            ("AGENTGW_LINK_TOKEN".into(), "k".into()),
        ])
        .unwrap();
        assert!(matches!(w.upstream, Mode::AwaitParent));
        // 開けた口は**上流のため**で、子を持つのではない
        assert!(w.children.is_none());
        assert_eq!(w.inlet.unwrap().addr.to_string(), "127.0.0.1:8787");
    }

    /// 迎えに行く先の表。**空白と末尾の / は落とす。壊れた組は黙って捨てる**
    /// (半端な行き先を作るより、その子が居ないことにする方が安全)。
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

    // ── .env の書き換え ─────────────────────────────────────────────────────

    fn conn() -> wire::Invite {
        wire::Invite {
            url: "wss://relay.example".into(),
            api_token: "s3cret".into(),
        }
    }

    /// 直結の口を**コメントにして**閉じ、Relay の3つを書く。他の行は1文字も変えない。
    #[test]
    fn applying_a_connection_folds_the_direct_token_and_keeps_everything_else() {
        let before = "SLACK_BOT_TOKEN=xoxb-1\nSLACK_APP_TOKEN=xapp-1\nOTHER=keep me\n";
        let after = apply_connection(before, &conn(), "desktop");
        assert!(
            after.contains("# (relay mode) SLACK_APP_TOKEN=xapp-1"),
            "{after}"
        );
        assert!(!after.contains("\nSLACK_APP_TOKEN="), "{after}");
        assert!(after.contains("SLACK_BOT_TOKEN=xoxb-1"), "{after}");
        assert!(after.contains("OTHER=keep me"), "{after}");
        assert!(
            after.contains("AGENTGW_RELAY_URL=wss://relay.example"),
            "{after}"
        );
        assert!(after.contains("AGENTGW_RELAY_TOKEN=s3cret"), "{after}");
        assert!(after.contains("AGENTGW_BRIDGE_ID=desktop"), "{after}");
    }

    /// 貼り直しは**上書き**で、行を増やさない。
    #[test]
    fn re_applying_replaces_rather_than_appends() {
        let once = apply_connection("", &conn(), "desktop");
        let twice = apply_connection(
            &once,
            &wire::Invite {
                url: "wss://new".into(),
                api_token: "new".into(),
            },
            "laptop",
        );
        assert_eq!(twice.matches("AGENTGW_RELAY_URL=").count(), 1, "{twice}");
        assert!(twice.contains("AGENTGW_RELAY_URL=wss://new"), "{twice}");
        assert!(twice.contains("AGENTGW_BRIDGE_ID=laptop"), "{twice}");
    }

    /// 書いた .env が、そのまま Relay モードとして読めること(往復)。
    #[test]
    fn what_it_writes_is_what_resolve_reads() {
        let text = apply_connection("SLACK_APP_TOKEN=xapp-1\n", &conn(), "desktop");
        let pairs: Vec<(String, String)> = text
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| {
                l.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect();
        let got = Mode::resolve(move |k: &str| {
            pairs.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone())
        })
        .unwrap();
        assert_eq!(
            got,
            Mode::Relay {
                url: "wss://relay.example".into(),
                api_token: "s3cret".into(),
                bridge_id: "desktop".into()
            }
        );
    }

    /// **経路の合流点の実データ確認**: Relay が載せる形の JSON が、直結と同じ `InboundMsg`
    /// になること。ここが崩れると、Relay 経由だけメッセージが届かなくなる。
    #[test]
    fn an_event_the_relay_forwards_becomes_the_same_inbound_message() {
        let event = serde_json::json!({
            "type": "message",
            "ts": "1700000000.000100",
            "channel": "C1",
            "user": "U1",
            "text": "<@U_BOT> hello"
        });
        let msg = crate::slack::inbound_from_relay("message", &event).expect("読めること");
        assert_eq!(msg.channel, "C1");
        assert_eq!(msg.user.as_deref(), Some("U1"));
        assert_eq!(msg.text, "<@U_BOT> hello");
        assert_eq!(msg.ts, "1700000000.000100");
    }

    #[test]
    fn an_event_the_relay_could_not_encode_is_dropped_not_guessed() {
        assert!(crate::slack::inbound_from_relay("message", &serde_json::json!({})).is_none());
        assert!(
            crate::slack::inbound_from_relay("app_mention", &serde_json::json!({"channel": "C1"}))
                .is_none()
        );
    }

    /// ボタンは `perm:<動作>:<reqId>` のものだけ拾う。それ以外は Bridge の関心事ではない。
    #[test]
    fn a_forwarded_button_becomes_a_perm_click() {
        let click = crate::slack::perm_click_from_relay(
            &serde_json::json!({"action_id": "perm:allow-channel:req-7"}),
            &serde_json::json!({"user": {"id": "U_OWNER"}}),
        )
        .expect("読めること");
        assert_eq!(click.req_id, "req-7");
        assert_eq!(click.action, "allow-channel");
        assert_eq!(click.by, "U_OWNER");

        for other in ["something_else", "perm", "perm:allow"] {
            assert!(
                crate::slack::perm_click_from_relay(
                    &serde_json::json!({ "action_id": other }),
                    &serde_json::json!({})
                )
                .is_none(),
                "{other}"
            );
        }
    }

    /// dial 先はパスまで組み立てる — 接続文字列の URL にはパスが付いていない。
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
