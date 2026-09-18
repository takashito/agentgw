//! Bridge — Slack と エージェントの間に座る本体。
//!
//! 実行時状態(起動/再起動/終了・受信配達・コマンド応答)はここ。
//! ディスクに残る状態(access.json / threads.json / ポート・トークンの記憶)と
//! ログは [`state`] に、ワーカーの台帳は [`worker`] に、コマンドの検出と文面は
//! [`command`] に居る。

pub mod command;
pub mod link;
pub mod gateway;
pub mod inbound;
pub mod render;
pub mod state;
pub mod worker;

use crate::agent::claude::HookIntake;
use crate::agent::screen::SpawnOutcome;
use crate::agent::claude::Claude;
use crate::agent::tmux::{self as tmux_mod, Pid, Window};
use crate::agent::{
    CompactOutcome, CompactProgress, Envelope, HookEvent, LoginOutcome, ProbeErr, SessionId,
    SpawnReq,
};
use crate::bridge::command::{Cmd, PwdMode};
use crate::bridge::render::RestartPhase;
use crate::bridge::state as bridge;
use crate::bridge::inbound::{Dispatch, GateVerdict, InboundMsg};
use crate::bridge::state::{Disposition, LogCtx, PoolKey, ThreadKey};
use crate::{mcp, ports, slack};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

const NARRATION_CAP: usize = 600;

/// ドレインを諦めてでも殺す上限。現行は受信確認のタイムアウトを流用する
/// (RECEIPT_TIMEOUT_MS = 30s)— 新しいつまみを増やさないため。
const DRAIN_TIMEOUT_MS: u64 = 30_000;

/// 付箋を Slack に反映する間隔。board 側が別途スレッド単位で1秒スロットルする。
const FLUSH_INTERVAL: Duration = Duration::from_millis(500);

/// 冷えたワーカーを畳む規則(2026-08-02 ユーザー決裁の4つの数字)。
/// 現行 Bun は環境変数で動かせるが、こちらは決め打ち — 動かしたくなったら足す。
const CLEANUP: worker::CleanupPolicy = worker::CleanupPolicy {
    idle_ttl_ms: 30 * 60_000,
    idle_slots: 5,
    idle_max_ms: 60 * 60_000,
    max_concurrent: 10,
};

/// Slack が再配達に刻む試行回数。slack-morphism 2.24.0 の Socket Mode envelope は
/// `envelope_id` と `accepts_response_payload` しか持たず、Slack が載せる `retry_attempt` は
/// push イベントのコールバックに渡る前に捨てられる(models/socket_mode/mod.rs:59-64)ので
/// 0 固定。同じ (channel, ts) の再到着は dedup が落とすため、stale の判定に残るのは
/// 「この Bridge が聞き始める前に投稿されたか」だけになる。
const RETRY_NUM: u32 = 0;

/// ターン失敗で1つのメッセージを再送してよい回数(現行 `TURN_FAILURE_RETRY_CAP`)。
/// **1回は意図** — 再送で直る失敗(混雑・API の瞬断・型無しの一発)は次の試行で晴れる。
/// 同じ失敗が2度出るなら本物なので、ループを見せるより人に伝える。
const TURN_FAILURE_RETRY_CAP: u32 = 1;

/// セッションの無いスレッドに返す1行(全コマンド共通)。
fn no_session() -> String {
    crate::t!(
        "This thread has no session running yet. Send a message to start one first.",
        "このスレッドには、まだ動いているセッションがありません。先にメッセージを送ってセッションを始めてください。"
    )
}

/// spawn したコマンドが main ループへ返す状態変更の便り。
///
/// サインイン・サインアウトは spawn したタスクの中で何十秒も走る(ブラウザの往復を待つ)ので、
/// 状態の書換えは main ループに**戻して**やる — tmux とポーリングはタスク、access.json と
/// login_pending は main、と持ち場を割る(dispo_rx と同じ形)。
enum CmdFx {
    /// サインインの結末。成否どちらでも login セッションを畳んで pending の席を空ける
    /// (**始まり**は main が同期で登録する — 席取りを spawn に任せると2本目に奪われる)。
    /// `bound` = Owner にする人
    LoginFinished {
        channel: String,
        bound: Option<String>,
    },
    LogoutFinished {
        ok: bool,
        channel: String,
        thread_ts: String,
    },
    /// 起動直後の窓に**誰も答えられない画面**が出ていた。tmux のポーリングはタスク、
    /// ゲートと通知は main、と持ち場を割る(LoginFinished と同じ形)。
    SpawnScreen {
        outcome: SpawnOutcome,
        /// ログに出す相手(`thread=…` / `pool session=…`)
        what: String,
    },
}

/// 人のクリックを待っているツール許可1件。
///
/// **ワーカーの hook はこの `respond` を握ったまま開いている** — 押されるか満期が来るまで返らない。
struct PermPending {
    /// hook へ返す口。押された/諦めた時にここへ答えを流す。
    respond: tokio::sync::oneshot::Sender<serde_json::Value>,
    channel: String,
    thread_ts: String,
    tool_name: String,
    /// Deny のときに**そのツールの行**を 🚫 にするための鍵。
    tool_use_id: String,
    /// 投稿したプロンプトの ts。満期のときに**消す**ためだけに持つ。
    prompt_ts: String,
    /// 満期(epoch ms)。hook 側の待ちより少し内側で切る。
    deadline_ms: u64,
}

/// 上限の見張りを始めるまでの猶予(立ち上がりに probe をぶつけない)。
const USAGE_MONITOR_STARTUP_DELAY_MS: u64 = 60_000;

/// 起動画面を見張る時間。現行の 30秒(同期)+ 120秒(linger)= 150秒に合わせた。
/// 現行の linger は `FIRST_PROMPT_TIMEOUT_MS`(60s、p99 45.6s)の2倍。
const SPAWN_SCREEN_BUDGET_MS: u64 = 150_000;
const SPAWN_SCREEN_POLL_MS: u64 = 1_000;

/// 人を待つ上限。hook の宣言(125s)と Bridge の待ち(120s)より内側で畳む。
const PERM_WAIT_MS: u64 = 115_000;

/// サインインのポーリング(定数どおり)。URL は普通 1〜3 秒で出る。
const LOGIN_POLL: Duration = Duration::from_secs(1);
const URL_POLL_MAX: u32 = 20;
const CODE_POLL_MAX: u32 = 30;

/// 未応答を抱えたスレッドに残す一言。
///
/// **Bun 原文から意図的に逸脱**: 原文は「再起動後に残りの処理を自動で
/// 再開します」だが、Rust の restart が降ろすのは Bridge だけで、ワーカーも在庫も畳まない
/// (後継が継承する)。止めていないものを「再開する」と言うのは二重に嘘 — 自動再開の仕組みも
/// まだ無い(`maintenance_restart` の ponytail 注記)。事実だけを言う。
fn restart_notice() -> String {
    crate::t!(
        "🙏 agentgw is restarting for a few seconds. Running agents aren't stopped, so their work continues.",
        "🙏 agentgw を数秒だけ再起動します。動いているエージェントは止めないので、作業はそのまま続きます。"
    )
}

/// プール worker が MCP を上げるまでの猶予(worker.ts の `MCP_INIT_TIMEOUT_MS` と同値)。
/// これを超えたら諦める(= 実体ごと畳んで在庫から消す)。
const POOL_MCP_INIT_TIMEOUT_MS: u64 = 50_000;

/// main ループが握る可変状態ひとまとめ。select の各腕はここのメソッドを呼ぶだけ。
pub struct Bridge {
    /// The outside world: Slack, the agent, the clock and the state directory.
    deps: Deps,
    // TODO(Task 12): `slack::Thinking` and `Api::post_now` still want the concrete Api.
    // Once they take `ports::Slack`, this goes and `deps.slack` does the job.
    slack_api: Arc<slack::Api>,
    access: bridge::Access,
    threads: bridge::Threads,
    /// cwd → 在庫に指名したセッション(pools.json)。**実体ではなく指名**なので Bridge を
    /// またいで残る。在庫を起こすときはここを見て `--resume` / 新規を決める。
    pools: bridge::Pools,
    dedup: inbound::RecentDeliveries,
    /// 生きているワーカーと在庫の台帳。
    workers: worker::Workers,
    /// 配達待ち(まだワーカーが暖まっていないスレッドの queue)。台帳ではないので Bridge 側。
    /// **root_ts** → 配達待ちの本文。スレッド鍵ではない(ワーカーがまだ暖まっていない
    /// 間に溜める queue で、引くのは常に同じチャンネルの中)。
    pending: HashMap<String, Vec<InboundMsg>>,
    lifecycle: bridge::Lifecycle,
    ledger: bridge::Ledger,
    sticky: slack::StickyBoard,
    /// 人のクリック待ちのツール許可。reqId → 待っているワーカーと、消すべきプロンプト。
    perm_pending: HashMap<String, PermPending>,
    /// `thread_key\0message_id` → まだ final が来ていない narration の断片。
    narration: HashMap<String, String>,
    hooks_file: String,
    mcp_port: u16,
    mcp_token: String,
    /// 自分の Slack user id。本文コマンドは自 mention を剥がしてから判定する。
    /// auth.test が落ちた起動では None(mention 付きのコマンドが素通しになるだけ)。
    bot_user_id: Option<String>,
    /// この Bridge が聞き始めた時刻。これより前に投稿されたコマンドは手遅れ。
    started_at_ms: u64,
    /// spawn したサインイン・サインアウトが状態変更を戻してくる口。
    cmd_tx: mpsc::Sender<CmdFx>,
    /// コード待ちのサインイン: channel → その sign-in を始めた人。**同時に1本だけ**
    /// (login セッションは1つ — 2本目を通すと後から来たコードで先の人が Owner になる)
    login_pending: HashMap<String, String>,
    /// 再起動を始めたか。exit(0) までの数百 ms を二重に走らせないための札
    restarting: bool,
    /// サインアウトが走っているか。`logout` の2連打で `claude auth logout` が2回走り、
    /// 2本目の teardown が1本目の後始末と噛み合わなくなるのを防ぐ
    signing_out: bool,
    /// usage 上限のリセット時刻(epoch ms)。いまの時刻がこれを下回る間は新規配達を遮断する。
    /// 0 = ゲート開放。実際に埋めるのは定期ポーリング。
    limited_until_ms: u64,
    /// 上限の見張り。最後に `/usage` を読んだ時刻・上限が見込まれるか・
    /// どこまで警告したか。
    usage_polled_at_ms: u64,
    usage_at_risk: bool,
    usage_warned_pct: u32,
    /// 沈黙見張り(現行の `armWatchdog` / `showStall`)。`thread_key` → [`Stall`]。
    stall: HashMap<ThreadKey, Stall>,
    /// 子を迎える口を持っているか。`help` に `route` の節を出すかだけに使う
    /// (実行するのは `relay::CommandCtx::route`。持っていないマシンで一覧に出しても
    /// 振り分ける相手が居ない)。
    fleet: bool,
}

/// The outside world. Only `Bridge::run()` wires the real ones; tests fill it with fakes
/// and hand it to `Bridge::new`.
#[derive(Clone)]
pub struct Deps {
    pub slack: ports::Slack,
    pub agent: ports::AgentRef,
    pub clock: ports::ClockRef,
    pub dir: bridge::StateDir,
}

/// Values fixed at start-up (the result of the wiring).
struct Config {
    hooks_file: String,
    mcp_port: u16,
    mcp_token: String,
    bot_user_id: Option<String>,
    /// When this Bridge started listening. Commands posted before it are stale.
    started_at_ms: u64,
    fleet: bool,
    cmd_tx: mpsc::Sender<CmdFx>,
    // TODO(Task 12): goes away with `Bridge::slack_api`.
    slack_api: Arc<slack::Api>,
}

impl Bridge {
    /// Loads the state files from `deps.dir`; everything else starts empty.
    fn new(deps: Deps, config: Config) -> Bridge {
        // 台帳は threads.json の中(entry の `inflight`)なので、読んだ後に組み立てる
        let threads = bridge::Threads::load(&deps.dir);
        Bridge {
            ledger: bridge::Ledger::load(&threads),
            threads,
            pools: bridge::Pools::load(&deps.dir),
            access: bridge::Access::load(&deps.dir),
            deps,
            slack_api: config.slack_api,
            dedup: inbound::RecentDeliveries::new(),
            workers: worker::Workers::default(),
            pending: HashMap::new(),
            lifecycle: bridge::Lifecycle::new(),
            sticky: slack::StickyBoard::default(),
            perm_pending: HashMap::new(),
            narration: HashMap::new(),
            hooks_file: config.hooks_file,
            mcp_port: config.mcp_port,
            mcp_token: config.mcp_token,
            bot_user_id: config.bot_user_id,
            started_at_ms: config.started_at_ms,
            cmd_tx: config.cmd_tx,
            login_pending: HashMap::new(),
            restarting: false,
            signing_out: false,
            limited_until_ms: 0,
            usage_polled_at_ms: 0,
            usage_at_risk: false,
            usage_warned_pct: 0,
            stall: HashMap::new(),
            fleet: config.fleet,
        }
    }

    #[cfg(test)]
    fn for_test(deps: Deps) -> (Bridge, mpsc::Receiver<CmdFx>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let config = Config {
            hooks_file: String::new(),
            mcp_port: 0,
            mcp_token: String::new(),
            bot_user_id: Some("U_BOT".into()),
            started_at_ms: deps.clock.now_ms(),
            fleet: false,
            cmd_tx,
            slack_api: Arc::new(slack::Api::new("xoxb-test").expect("slack client")),
        };
        (Bridge::new(deps, config), cmd_rx)
    }
}

/// 1スレッドぶんの沈黙見張り。
///
/// **そのスレッド宛のステータス送信口を丸ごと抱える**のが肝で、
/// 配達 / hook / 見張り発火 / 決着の送信が全て `thinking` の1本の直列タスクを通る = 互いに
/// 追い越さない。別々に `tokio::spawn` していた頃は「配達の `is typing…`」と「見張り解除の
/// クリア」が順不同に飛んで打ち消し合っていた。
///
/// entry を落とすと `slack::Thinking` の Drop が最後にクリアを流す — **キューの最後尾に並ぶ**ので、
/// 直前に流した set を必ず追い越さずに消える。
struct Stall {
    /// 最後に活動があった時刻(epoch ms)。
    last_activity_ms: u64,
    /// 見張りが `is thinking…` を出しているか(現行 `Entry.stalled`)。
    shown: bool,
    /// 許可プロンプトが出ていて、ワーカーは**意図的に**黙っている。
    /// 見張りを止める(待っていることは「Permission requested」のプロンプト自身が示す)。
    awaiting_perm: bool,
    thinking: slack::Thinking,
}

/// 「同じ出来事を2度処理しない」ための鍵。
///
/// 素の `ts` で足りるのは**本文が届いたとき**だけ。リアクション・書き換え・削除は `ts` が
/// **相手側のメッセージ**を指すので、そのままだと元の配達で覚えた鍵とぶつかって必ず
/// 「重複」で落ちる。種別ごとに名前空間を分ける。
fn dedup_key(msg: &InboundMsg) -> String {
    match (&msg.reaction, &msg.edited, &msg.deleted_ts) {
        // 同じ投稿への2つ目の絵文字を「重複」にしない — 絵文字と付/外を混ぜて数える
        (Some(r), _, _) => format!("{}#{}{}", msg.ts, r.emoji, if r.added { "+" } else { "-" }),
        // 書き換えは改訂ごとに1回だけ通す(同じ改訂の再配達は落とす)
        (_, Some(e), _) => format!("{}#edit#{}", msg.ts, e.revision),
        (_, _, Some(_)) => format!("{}#deleted", msg.ts),
        _ => msg.ts.clone(),
    }
}

/// 自分が付けた印(ack の 👀 / 🤖 / 受信確認の 🔄)は、Slack から自分に返ってくる。
///
/// **ワーカーには渡さない。** リアクションの合成本文は「黙っていないで返事を」と促すので、
/// 素直に従うワーカーは ack のたびに返事を増やす。実測(2026-08-02)では**スレッドの1通目**の
/// たびに3通入っていた — 1通目に限るのは、リアクションの通知がスレッドを教えてくれず、
/// 代用した ID がそのときだけ本物のスレッドと一致するため。
fn is_own_reaction(msg: &InboundMsg, bot_user_id: Option<&str>) -> bool {
    msg.reaction.is_some() && bot_user_id.is_some_and(|b| msg.user.as_deref() == Some(b))
}

/// **自分が書いたのではない**投稿に付いたリアクションの行き先。
#[derive(PartialEq, Debug)]
enum ForeignReaction {
    /// Owner が**自分の依頼**に stop を付けた — 止める(2026-08-02 のユーザー指定)。
    Stop,
    /// それ以外。現行はここを黙って捨てる — 他人のやりとりへの
    /// リアクションが、合成テキストとしてワーカーに流れ込まないように
    Drop,
}

fn foreign_reaction(
    r: &inbound::Reaction,
    reactor: Option<&str>,
    author: Option<&str>,
    owner: &str,
) -> ForeignReaction {
    let by_owner = !owner.is_empty() && reactor == Some(owner);
    // `author == reactor` = 自分の投稿に自分で付けた。**他人の投稿への stop は効かない** —
    // 誰の仕事を止めるのかが決まらない
    if r.is_stop() && by_owner && author == reactor {
        ForeignReaction::Stop
    } else {
        ForeignReaction::Drop
    }
}

impl Bridge {
    /// 受信メッセージの添付を先読みダウンロードし、結果を msg に書き戻す。
    /// **決して失敗を投げない** — 1ファイルごとに握りつぶして劣化ノートにする。壊れた/大きすぎる
    /// 1つが、ユーザーの本文や他の添付を巻き添えにしないため(現行)。
    async fn with_attachments(&self, mut msg: InboundMsg, ctx: &LogCtx) -> InboundMsg {
        let (mut paths, mut errors) = (Vec::new(), Vec::new());
        // ここは select ループの腕の中 — 止まった分だけ**全スレッド**の hook / disposition /
        // コマンド / sticky flush が待たされる。だから締切は1ファイルごとではなく
        // **メッセージ全体で1つ**。何枚貼られても最悪 DOWNLOAD_TIMEOUT で必ず抜ける。
        // ponytail: 逐次。並列化(tokio::spawn)は多数添付の待ち時間が実際に痛くなってから
        let deadline = tokio::time::Instant::now() + slack::DOWNLOAD_TIMEOUT;
        for f in &msg.files {
            let dl = self.deps.slack.download_attachment(&f.id, self.deps.dir.path());
            let reason = match tokio::time::timeout_at(deadline, dl).await {
                Ok(Ok(path)) => {
                    paths.push(path);
                    continue;
                }
                Ok(Err(e)) => e,
                // 締切超過。残りのファイルも同じ枝で即座にノートになる
                Err(_) => format!(
                    "timed out ({}s budget for this message's attachments)",
                    slack::DOWNLOAD_TIMEOUT.as_secs()
                ),
            };
            ctx.debug(
                "bridge",
                &format!("attachment download failed file={}: {reason}", f.id),
            );
            // 文言は原文コピー— ワーカーが人に伝える一次資料
            errors.push(format!("[attachment {} not downloaded: {reason}]", f.name));
        }
        ctx.debug("bridge", &format!(
                "slack-events: message accepted message_id={} channel={} type={} files={} dl={} err={}",
                msg.ts,
                msg.channel,
                match msg.channel_kind {
                    inbound::ChannelKind::Dm => "im",
                    inbound::ChannelKind::Channel => "channel",
                },
                msg.files.len(),
                paths.len(),
                errors.len()
            ));
        msg.file_paths = paths;
        msg.file_errors = errors;
        msg
    }

    async fn on_inbound(&mut self, msg: &InboundMsg) {
        // 自分の印は自分に返る。**dedup より前**で捨てる — 覚え書きを自分の絵文字で埋めない
        if is_own_reaction(msg, self.bot_user_id.as_deref()) {
            LogCtx::default().debug(
                "bridge",
                &format!(
                    "own reaction on {}:{} — dropped",
                    msg.channel,
                    msg.reaction
                        .as_ref()
                        .map(|r| r.emoji.as_str())
                        .unwrap_or("")
                ),
            );
            return;
        }
        // dedup は gate の前 — 1メッセージが複数イベントで届く
        let dedup_key = dedup_key(msg);
        if self.dedup.seen(&msg.channel, &dedup_key) {
            LogCtx::default().debug(
                "bridge",
                &format!("duplicate event {}:{} — dropped", msg.channel, msg.ts),
            );
            return;
        }
        // コマンドは**ボタン**: 押された時に効くか、さもなくば効かない。
        // Bridge が落ちていた間の投稿は Slack が再接続時にまとめて寄越すので、送り主がとうに
        // 諦めた後の `exit` が届いて二度目の別れを告げる。全コマンド経路の手前で落とす。
        // 普通のリクエストは無傷 — 遅れて配達される方が、本物の仕事を失うよりましだから
        let body = crate::bridge::command::Message::new(&msg.text, self.bot_user_id.as_deref());
        if let Some(cmd) = crate::bridge::command::Cmd::parse(&body)
            && let Some(stale) = cmd.stale_reason(&msg.ts, self.started_at_ms, RETRY_NUM)
        {
            LogCtx {
                session_id: None,
                thread_key: msg
                    .thread_ts
                    .as_deref()
                    .map(|t| ThreadKey::new(&msg.channel, t)),
            }
            .info(
                "bridge",
                &format!(
                    "slack-events: dropped: command arrived too late to honor ({stale}) — \
                     msg={} channel={} sender={} text={}",
                    msg.ts,
                    msg.channel,
                    msg.user.as_deref().unwrap_or(""),
                    serde_json::to_string(&msg.text.chars().take(40).collect::<String>())
                        .unwrap_or_default()
                ),
            );
            return;
        }
        // Owner がまだ居ないときのサインイン専用の抜け道。gate は
        // owner 空を全部落とすので、**その手前**でなければサインインは永久に始められない。
        // Owner が縛られた後はこの枝ごと素通りする(以後は普通の gate の世界)
        if self.access.owner.is_empty() && !msg.is_bot && self.login_carve_out(msg) {
            return;
        }
        // 編集・削除のイベントは**スレッドを教えてくれない**(ライブラリの型が
        // 入れ子の `thread_ts` を落とす)。まず未応答の台帳で引き、載っていなければ Slack に
        // 1件だけ問い合わせる。**返事し終わった過去のメッセージを直した場合はこちらしか無い** —
        // 台帳から消えているので、聞かないと別スレッド扱いで捨ててしまう
        // リアクションも同じで、**書き手もスレッドも教えてくれない**。元の投稿を1回だけ引く。
        // 省いて ts をスレッドの代用にしていたのが、
        // 「スレッドの1通目にだけ湧く誤配達」の正体だった(2026-08-02)。
        // ponytail: リアクション1つにつき1回問い合わせる。多すぎるなら `item_user` を
        // InboundMsg まで持ち上げて、stop でない他人宛のものを引く前に落とす
        let reacted = match &msg.reaction {
            Some(r) => match self.deps.slack.message_at(&msg.channel, &r.item_ts).await {
                Some(m) => Some(m),
                None => {
                    LogCtx::default().debug(
                        "bridge",
                        &format!(
                            "reaction dropped: could not read {}:{}",
                            msg.channel, r.item_ts
                        ),
                    );
                    return;
                }
            },
            None => None,
        };
        let touched = msg
            .deleted_ts
            .as_deref()
            .or(msg.edited.as_ref().map(|e| e.ts.as_str()));
        let root_ts = match (&reacted, touched) {
            (Some(m), _) => m.thread_ts.clone(),
            (None, None) => msg.thread_ts.clone().unwrap_or_else(|| msg.ts.clone()),
            (None, Some(id)) => match self.ledger.key_of_id(id).and_then(|k| k.split().1) {
                Some(root) => root,
                None => self
                    .deps.slack
                    .parent_thread_of(&msg.channel, id)
                    .await
                    .or_else(|| msg.thread_ts.clone())
                    .unwrap_or_else(|| id.to_string()),
            },
        };
        let key = ThreadKey::new(&msg.channel, &root_ts);
        let ctx = |sid: Option<&str>| LogCtx {
            session_id: sid.map(str::to_string),
            thread_key: Some(key.clone()),
        };

        // チャンネルでは**名指しか、もう動いているスレッド**でないと入れない
        // (DM は名指し扱い)。登録済みかどうかは見ない — 判断材料はこの2つだけ
        let dm = msg.channel_kind == inbound::ChannelKind::Dm;
        let is_mention = dm
            || crate::bridge::command::Message::new(&msg.text, self.bot_user_id.as_deref())
                .mentions_bot();
        let is_active_thread = !dm && self.threads.is_active(&root_ts);
        let context_only = match self.access.gate(msg, is_mention, is_active_thread) {
            GateVerdict::Drop(reason) => {
                ctx(None).info(
                    "bridge",
                    &format!("dropped {}:{} — {reason}", msg.channel, msg.ts),
                );
                return;
            }
            // Owner 以外が動いているスレッドで喋った — 流すが返事は期待しない
            GateVerdict::Context => {
                ctx(None).info(
                    "bridge",
                    &format!(
                        "slack-events: channel deliver: context (non-owner, silent-expected) — \
                         msg={} channel={} sender={}",
                        msg.ts,
                        msg.channel,
                        msg.user.as_deref().unwrap_or("")
                    ),
                );
                true
            }
            GateVerdict::Serve => false,
        };
        let _ = context_only;

        // ユーザーが自分の依頼を消した。**まだ処理中のものだけ**取り消しを伝える:
        // 既に答えた過去のメッセージを消しても、取り下げる仕事はもう無い
        if let Some(deleted) = msg.deleted_ts.clone() {
            self.on_message_deleted(msg, &key, &root_ts, &deleted, &ctx(None));
            return;
        }

        // ユーザーが依頼を書き換えた。**いま処理中のもの**なら中断して新しい方を
        // やり直させ、過去のものなら今の仕事は続けさせて「手が空いたら」と伝える
        if let Some(edited) = msg.edited.clone() {
            self.on_message_edited(msg, &key, &root_ts, &edited.ts, &ctx(None));
            return;
        }

        // **Owner が自分の依頼に付けた stop は止める合図**(2026-08-02 のユーザー指定。現行との
        // 意図的な差 — 現行は付箋に付いたものだけを見る)。付箋を探して押さなくても、
        // 止めたい依頼そのものに ✋ を付ければ止まる。
        //
        // その手前に、現行にあって移植されていなかった検査を置く: **自分が書いた投稿への
        // リアクションだけ拾う**。他人の投稿に付いたものまで拾うと、
        // 無関係なやりとりが合成テキストとしてワーカーに流れ込む
        if let (Some(r), Some(m)) = (&msg.reaction, &reacted)
            && !(m.is_bot || m.user.as_deref() == self.bot_user_id.as_deref())
        {
            if foreign_reaction(
                r,
                msg.user.as_deref(),
                m.user.as_deref(),
                &self.access.owner,
            ) == ForeignReaction::Stop
            {
                ctx(None).info(
                    "bridge",
                    &format!(
                        "slack-events: stop reaction :{}: on the requester's own message {}:{} \
                         (thread {root_ts})",
                        r.emoji, msg.channel, r.item_ts
                    ),
                );
                self.user_stop(msg, &key, &root_ts, &ctx(None));
                return;
            }
            ctx(None).debug(
                "bridge",
                &format!(
                    "reaction dropped: {}:{} is not our message",
                    msg.channel, r.item_ts
                ),
            );
            return;
        }

        // **進捗付箋に付いた** stop 絵文字は stop コマンドと同じ。他のメッセージへの
        // 同じ絵文字はただのリアクションとして下へ流す(付箋を狙って押した時だけ止める)
        if let Some(r) = &msg.reaction
            && r.is_stop()
            && self.sticky.sticky_ts(&key).as_deref() == Some(r.item_ts.as_str())
        {
            ctx(None).info(
                "bridge",
                &format!(
                    "slack-events: stop reaction :{}: on progress sticky {}:{} (thread {root_ts})",
                    r.emoji, msg.channel, r.item_ts
                ),
            );
            self.user_stop(msg, &key, &root_ts, &ctx(None));
            return;
        }

        // ループ遮断(チャンネルのみ)。許可した bot が居る以上、
        // bot 同士が延々と返し合う道が開いている。**人が入るまで**止めるのがここ
        let mut loop_guard: Option<String> = None;
        if !dm {
            if msg.is_bot {
                if self.threads.status_of(&root_ts) == "paused" {
                    ctx(None).info(
                        "bridge",
                        &format!(
                            "slack-events: dropped: thread paused (loop guard) — msg={} \
                             (awaiting human re-engagement)",
                            msg.ts
                        ),
                    );
                    return;
                }
                let streak = self.threads.bump_bot_streak(&root_ts);
                if streak >= bridge::LOOP_LIMIT {
                    self.threads.pause(&root_ts);
                    ctx(None).info(
                        "bridge",
                        &format!(
                            "slack-events: loop-guard: thread paused after {streak} consecutive \
                             bot msgs (≥ LOOP_LIMIT) — msg={}",
                            msg.ts
                        ),
                    );
                    // 呼ぶ相手が居なければ空(印だけ立てる)
                    loop_guard = Some(match self.access.owner.as_str() {
                        "" => String::new(),
                        o => format!("<@{o}>"),
                    });
                }
                if let Err(e) = self.threads.save() {
                    ctx(None).error("bridge", &format!("threads.json save failed: {e}"));
                }
            } else if self.threads.reset_bot_streak(&root_ts) {
                ctx(None).info(
                    "bridge",
                    "slack-events: thread resumed — human re-engaged (was paused)",
                );
                if let Err(e) = self.threads.save() {
                    ctx(None).error("bridge", &format!("threads.json save failed: {e}"));
                }
            }
        }

        // コマンドは Bridge が自分で答えて**ここで終わる** — ワーカーには決して渡さない。
        // ack より前に返すのは、コマンドに 👀 を付けないため
        if self.handle_command(msg, &key, &root_ts).await {
            // Bridge が**このスレッドで喋った**。status / usage のようにワーカーを起こさない
            // コマンドでも、以後そのスレッドは「動いているスレッド」— 続きはメンション無しで
            // 受ける(ユーザー指定。セッションの有無とは切り離す)
            self.mark_thread_seen(&msg.channel, &root_ts, &ctx(None));
            return;
        }

        // usage 上限中は新規の依頼を受けない。キューイングもしない。
        // コマンドの**後ろ**なのは現行どおり — 上限中でも stop/restart/logout は効く
        if self.deps.clock.now_ms() < self.limited_until_ms {
            let text = crate::bridge::render::Notice::Limited {
                until_ms: self.limited_until_ms,
            }
            .render();
            self.post(&msg.channel, &root_ts, text, &key);
            return;
        }

        // 「見た」の合図をすぐ返し、未応答として台帳に載せる(🤖 への切替は user_prompt hook)
        self.react(
            &msg.channel,
            &msg.ts,
            slack::Api::ack_emoji(&self.access),
            &key,
        )
        .await;
        self.ledger.track(&key, &msg.ts);

        // 添付は**受信した時に**落とし、封筒には
        // ローカルパスを載せる。ワーカーは Read するだけでよく、引き金のメッセージについて
        // download_attachment を呼ぶ必要が無い(呼ぶための file_id も知らされない)
        let msg = &self.with_attachments(msg.clone(), &ctx(None)).await;

        let entry = self.threads.get(&root_ts).cloned();
        let is_new = entry.is_none();
        let sid = entry.as_ref().and_then(|e| e.agent_id.as_deref());
        // 窓名は session_id だけで決まる。まだセッションの無いスレッドは
        // 窓も無い — worker_state は sid 無しで Absent を返すので空文字で害が無い
        let window = sid
            .map(|s| SessionId::from(s).window_name())
            .unwrap_or_default();
        let state = self.workers.state_of(entry.as_ref(), &window, self.deps.agent.as_ref());
        // 配達先は window_id を優先(窓名は改名されうる)
        let target = entry
            .as_ref()
            .and_then(|e| e.agent_id.as_deref())
            .and_then(|sid| self.workers.warm(sid))
            .and_then(|h| h.window_id.clone())
            .unwrap_or_else(|| window.clone());
        let envelope = Envelope::of_guarded(msg, &root_ts, loop_guard, self.deps.clock.now_ms());
        // ターン失敗の再送に要る。`track` は ack の直後(封筒がまだ無い時点)なので、
        // 封筒ができた**ここ**で台帳に預ける — これより下の配達経路は全部この後ろ
        self.ledger.remember_envelope(&key, &msg.ts, &envelope);
        // pwd と同じ解決を通す(ルート無し → Home フォールバック)
        let (cwd, _) = self.access.repo_path(&msg.channel, &Host::home());

        // 新規スレッドは cold spawn の前に在庫を覗く。無ければ下の decide にそのまま落ちる
        if bridge::Threads::should_claim_pool(entry.as_ref()) {
            let key_pool = bridge::PoolKey::of_cwd(&cwd);
            if let Some(claimed) = self.claim_pool_worker(&key_pool) {
                // SpawnNew と同じ話題の取り方(trim して頭 60 文字 — 文字単位)
                let topic: String = msg.text.trim().chars().take(60).collect();
                self.assign_pool_worker(
                    claimed,
                    &root_ts,
                    &msg.channel,
                    &cwd,
                    &envelope,
                    &topic,
                    &msg.ts,
                    &key,
                    &ctx(None),
                );
                return;
            }
        }

        // 配達できたら沈黙見張りを起こす。`entry` の借用が生きているうちは `&mut self` を
        // 取れないので、match の外まで持ち出す
        let mut delivered = false;
        match Dispatch::decide(entry.as_ref(), state) {
            Dispatch::SpawnNew => {
                let sid = SessionId::new().as_str().to_string();
                let mut e = entry.unwrap_or_default();
                e.agent_id = Some(sid.clone());
                e.channel_id = Some(msg.channel.clone());
                e.repo_path = Some(cwd.clone());
                // status のリンク文字列になる話題。現行と同じく trim して頭 60 文字
                // (字数は文字単位で数える — 日本語を割らないため)
                let topic: String = msg.text.trim().chars().take(60).collect();
                e.topic = (!topic.is_empty()).then_some(topic);
                self.threads.upsert(&root_ts, e);
                if let Err(err) = self.threads.save() {
                    ctx(Some(&sid)).error("bridge", &format!("threads.json save failed: {err}"));
                }
                let Some(mcp) = self.write_mcp(&sid, &ctx(Some(&sid))) else {
                    return;
                };
                let req = SpawnReq {
                    session_id: sid.clone().into(),
                    cwd: cwd.clone(),
                    prompt: Some(envelope.clone()),
                    resume_from: None,
                    // 窓名は**今決まった** session_id から作る(スレッドの窓名は存在しない)
                    window: SessionId::from(sid.clone()).window_name(),
                    state,
                    hooks_file: self.hooks_file.clone(),
                    mcp_config: mcp,
                };
                self.spawn_worker(&req, &key, &ctx(Some(&sid)), &sid);
            }
            Dispatch::SpawnResume(sid) => {
                let Some(mcp) = self.write_mcp(&sid, &ctx(Some(&sid))) else {
                    return;
                };
                let req = SpawnReq {
                    session_id: sid.clone().into(),
                    cwd: cwd.clone(),
                    prompt: Some(envelope.clone()),
                    // 継続は同じ session_id を `--resume` で開き直す
                    resume_from: Some(sid.clone().into()),
                    window: SessionId::from(sid.clone()).window_name(),
                    state,
                    hooks_file: self.hooks_file.clone(),
                    mcp_config: mcp,
                };
                self.spawn_worker(&req, &key, &ctx(Some(&sid)), &sid);
            }
            Dispatch::Deliver => {
                let sid = entry.as_ref().and_then(|e| e.agent_id.clone());
                // 暖まっているワーカーには prefix 抜きの素の封筒(push と同じ形)
                match self.deps.agent.send_text(&Window::of(&target), &envelope) {
                    // 1メッセージ1行の配達記録(現行)
                    Ok(()) => {
                        ctx(sid.as_deref()).info(
                            "bridge",
                            &format!(
                                "slack-events: deliver message_id={} chat={} thread={root_ts} \
                                 new={is_new} -> worker {}",
                                msg.ts, msg.channel, msg.channel
                            ),
                        );
                        delivered = true;
                    }
                    // 配達できなかった = このメッセージは**誰にも届いていない**。ログだけだと
                    // 👀 が付いたまま無言で終わる(2026-08-18: モーダルで詰まった2スレッドが
                    // それで18分沈黙した)ので、待っている人に1本言う。
                    // ponytail: 詰まっている間は1通ごとに1本出る。うるさければスレッド単位で
                    // 抑制する(`cannot_deliver` の cooldown と同じ形)
                    Err(e) => {
                        ctx(sid.as_deref()).error(
                            "bridge",
                            &format!("delivery failed: {e} — queued for retry"),
                        );
                        // 取っておかないと、覆いが晴れても**このメッセージは二度と届かない**
                        // (再配達の経路が無い理由は [`Bridge::retry_pending`])
                        self.pending
                            .entry(root_ts.clone())
                            .or_default()
                            .push(msg.clone());
                        self.post_error_frame(
                            msg.channel.clone(),
                            root_ts.clone(),
                            crate::t!("Couldn't hand this to the agent: {e}", "エージェントに渡せませんでした: {e}"),
                        );
                    }
                }
            }
            Dispatch::Queue => {
                self.pending
                    .entry(root_ts.clone())
                    .or_default()
                    .push(msg.clone());
                ctx(None).info("bridge", "worker still starting — queued");
            }
        }
        // 渡した = ここから先は返事待ち。shimmer で「受け取って動いている」を出す
        // 無音が続けば見張りが「思考中」に差し替える。
        // 見張りの張り直しと同じ1回の送信で出すので、既に出ている shimmer と喧嘩しない
        // リアクションは軽い合図 — 👀 だけが活動の印で、shimmer は立てない
        // (ユーザー判断)。
        // **他人宛の発言でも立てない** — 動いているスレッドには `<@誰か> おーい` も流れてくるが、
        // それはこちらへの用件ではない。出すと「返事を書いている」という嘘になる
        let for_someone_else = !is_mention
            && crate::bridge::command::Message::new(&msg.text, self.bot_user_id.as_deref())
                .mentions_someone_else();
        if delivered && msg.reaction.is_none() && !for_someone_else {
            self.touch_thread(&key, slack::TYPING_STATUS);
        }
    }

    /// Bridge が自分で答える本文コマンド。**true = 消費した** — 呼び手はそこで打ち切る。
    ///
    /// 認可は1本の規則だけ: 送信者が Owner か。チャンネルでも DM でも、
    /// mention の有無にも依らない。Owner でない誰かのコマンドは記録して捨てる — ワーカーに
    /// 落ちると「永遠に再spawn される未応答メッセージ」になってスレッドを詰まらせる。
    async fn handle_command(&mut self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str) -> bool {
        // 検出は Cmd::parse が全部やる — どれでもなければコマンドではない
        let msg_body = crate::bridge::command::Message::new(&msg.text, self.bot_user_id.as_deref());
        let Some(cmd) = crate::bridge::command::Cmd::parse(&msg_body) else {
            return false;
        };
        let label = cmd.label();

        let ctx = LogCtx {
            session_id: None,
            thread_key: Some(key.clone()),
        };
        let sender = msg.user.as_deref().unwrap_or("");
        // 今は gate が非 Owner を先に落とすのでここは通らない。それでも残す — 現行が
        // 非 Owner の発言をワーカーに「読ませる」context 配達を持っており、
        // それを移植した日に gate は非 Owner を通し始める。コマンドの認可はその時も**ここ**にある
        if self.access.owner.is_empty() || msg.user.as_deref() != Some(self.access.owner.as_str()) {
            // `login` だけは文脈が1つ増える — Owner が**既に居る**のに来た login だから
            // ここに落ちている(居なければ gate の手前の抜け道が捌く)
            let note = match cmd {
                Cmd::Login => " while an Owner is bound",
                _ => "",
            };
            ctx.info(
                "bridge",
                &format!(
                    "slack-events: {label} from non-owner {sender}{note} — \
                     ignoring (not delivered to worker) msg={}",
                    msg.ts
                ),
            );
            return true;
        }
        let dm = msg.channel_kind == inbound::ChannelKind::Dm;

        match cmd {
            // stop。ESC は tmux を通るので
            // MCP が死んでいても効く — 止めたくなるのは大抵まさにその状況
            Cmd::Stop => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: stop command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_stop(msg, key, root_ts, &ctx);
            }
            // exit。終わらせるのは**ワーカーだけ** — スレッドは残る
            Cmd::Exit => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: exit command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_exit(msg, key, root_ts, &ctx).await;
            }
            // resume。手元の端末に線を渡す
            Cmd::Resume => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: resume command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_resume(msg, key, root_ts, &ctx);
            }
            Cmd::Help => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: help command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.post(
                    &msg.channel,
                    root_ts,
                    crate::bridge::render::Notice::Help { fleet: self.fleet }.render(),
                    key,
                );
            }
            // status。ワーカーを一切通さないので、全員が
            // 固まっていても答えが返る
            Cmd::Status => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: status command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_status(msg, key, root_ts, &ctx);
            }
            Cmd::Context => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: context command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_context(msg, key, root_ts, &ctx);
            }
            Cmd::Usage => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: usage command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_usage(msg, key, root_ts, &ctx);
            }
            Cmd::Compact => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: compact command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_compact(msg, key, root_ts, &ctx);
            }
            // restart。この Bridge 自身を
            // 入れ替える唯一のコマンド — 進捗チェックリストだけが2つのプロセスをまたぐ
            Cmd::Restart => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: restart command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                let channel = msg.channel.clone();
                self.maintenance_restart("slack restart command", Some((&channel, root_ts)), &ctx)
                    .await;
            }
            // 既にサインイン済みでの `login`。ワーカーに落とすと
            // 「何にログインしますか?」と訊き返してくるので、ここで打ち止める
            Cmd::Login => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: login command from Owner while already signed in — \
                         replying already-signed-in msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.post(
                    &msg.channel,
                    root_ts,
                    {
                        let owner = &self.access.owner;
                        crate::t!(
                            "Already signed in (owner: <@{owner}>). To switch accounts, send `logout`, then `login`.",
                            "既にサインインしています(Owner: <@{owner}>)。アカウントを切り替えるには、`logout` のあとに `login` を送ってください。"
                        )
                    },
                    key,
                );
            }
            Cmd::Logout => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: logout command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_logout(&msg.channel, root_ts);
            }
            Cmd::Model(name) => {
                let what = match &name {
                    Some(n) => format!("model command ({n})"),
                    None => "model show command".to_string(),
                };
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: {what} msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_model(msg, name, key, root_ts, &ctx);
            }
            Cmd::Effort(level) => {
                let what = match &level {
                    Some(l) => format!("effort command ({l})"),
                    None => "effort show command".to_string(),
                };
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: {what} msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_effort(msg, level, key, root_ts, &ctx);
            }
            Cmd::Mode(name) => {
                let what = match &name {
                    Some(n) => format!("mode command ({n})"),
                    None => "mode show command".to_string(),
                };
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: {what} msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_mode(msg, name, key, root_ts, &ctx);
            }
            Cmd::Pwd(mode) => {
                let kind = match mode {
                    PwdMode::Current => "current",
                    PwdMode::Set(_) => "set",
                    PwdMode::All => "all",
                };
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: pwd command mode={kind} msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                let out = self.pwd_answer(msg, mode, dm, root_ts, &ctx);
                self.post(&msg.channel, root_ts, out, key);
            }
            Cmd::Owner(oc) => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: owner-command '{}' args=[{}] msg={} channel={} dm={dm}",
                        oc.verb,
                        oc.args.join(" "),
                        msg.ts,
                        msg.channel
                    ),
                );
                self.owner_command(msg, oc, dm, root_ts, key, &ctx).await;
            }
        }
        true
    }

    /// 走っているターンを ESC で断ち切る。生きているのは
    /// 「ワーカーのプロセスが在る」かつ「未応答が1件以上ある」ときだけ。
    /// 書き換えられた依頼をワーカーに渡す。分かれ道は1つだけ:
    /// **いま処理中の依頼が書き換わったのか**(= 未応答に載っていて、かつスレッドで最新)。
    ///
    /// - 処理中 → 付箋に「Interrupted by user.」を足し(既に出ているときだけ)、新しい本文を
    ///   押し込んでから ESC。古い言い回しのためにやっていた作業は捨てさせる
    /// - 過去の依頼 → **中断しない**。「手が空いてから直した依頼をやって」と伝えるだけ
    ///   (過去のメッセージを消したときに今の仕事へ手を出さないのと同じ扱い)
    ///
    /// ワーカーの居ないスレッドは best-effort で捨てる(中断する相手も渡す先も無い)。
    fn on_message_edited(
        &mut self,
        msg: &InboundMsg,
        key: &ThreadKey,
        root_ts: &str,
        edited_ts: &str,
        ctx: &LogCtx,
    ) {
        let Some(sid) = self.threads.get(root_ts).and_then(|e| e.agent_id.clone()) else {
            ctx.info(
                "bridge",
                &format!(
                    "message_changed chan={} ts={edited_ts} -> no active worker thread, dropped \
                     (best-effort)",
                    msg.channel
                ),
            );
            return;
        };
        // 「いま処理中」= 未応答に載っていて、かつスレッドの未応答の中で最新
        // (Slack の id は秒.マイクロ秒なので数値比較が新しさの比較になる)
        let pending = self.ledger.pending(key);
        let num = |id: &str| id.parse::<f64>().unwrap_or(0.0);
        let is_current = pending.iter().any(|id| id == edited_ts)
            && pending.iter().all(|id| num(id) <= num(edited_ts));

        let mut notice = msg.clone();
        notice.text = crate::bridge::inbound::edit_notice(
            edited_ts,
            &msg.text,
            !msg.files.is_empty(),
            is_current,
        );
        let target = self
            .workers
            .warm(&sid)
            .and_then(|h| h.window_id.clone())
            .unwrap_or_else(|| SessionId::from(sid.clone()).window_name());
        // 付箋が出ているときだけ中断の締め行を足す(まだ何も出ていないなら足さない)
        if is_current && self.sticky.sticky_ts(key).is_some() {
            self.sticky.on_interrupted(key);
        }
        ctx.info(
            "bridge",
            &format!(
                "message_changed chan={} ts={edited_ts} thread={root_ts} -> {}",
                msg.channel,
                if is_current {
                    "CURRENT work edited: interrupt + redo with new text"
                } else {
                    "past message revised: notify only (current work untouched)"
                }
            ),
        );
        let envelope = Envelope::of(&notice, root_ts, self.deps.clock.now_ms());
        if let Err(e) = self.deps.agent.send_text(&Window::of(&target), &envelope) {
            ctx.error("bridge", &format!("message_changed delivery failed: {e}"));
            return;
        }
        // 押し込んでから切る(削除と同じ順序)— 逆にすると、切られたワーカーが
        // 書き換えを知らないまま次の入力を待つ
        if is_current {
            self.user_stop(msg, key, root_ts, ctx);
        }
        // 書き換えられた依頼は**もう一度未応答**。載せ直しは user_stop の後(先に載せると
        // dispose が巻き込む)。これが無いと沈黙見張りが「決着済み」と見て即畳まれ、
        // ワーカーが作り直している間ずっと shimmer も「考え中」も出ない。
        // 封筒も差し替える — 覚えているのは書き換え**前**の本文で、ターン失敗の再送が
        // 古い言い回しを送り直してしまう
        self.ledger.track(key, edited_ts);
        self.ledger.remember_envelope(key, edited_ts, &envelope);
        self.touch_thread(key, slack::TYPING_STATUS);
    }

    /// 降りる前に、**まだ渡していないメッセージを threads.json へ逃がす**。
    /// これが無いと、起動待ちで queue に積まれた依頼は落ちた瞬間に消える。
    ///
    /// 逃がすのは queue の中身だけ(台帳の未応答は**渡し済み**で、ワーカーが抱えている)。
    fn flush_pending_to_disk(&mut self, ctx: &LogCtx) {
        let queued: Vec<(String, Vec<InboundMsg>)> = self.pending.drain().collect();
        let mut n = 0usize;
        for (root_ts, msgs) in queued {
            for msg in &msgs {
                self.threads.enqueue_pending(&root_ts, &msg.channel, msg);
                n += 1;
            }
            if !msgs.is_empty() {
                ctx.info(
                    "bridge",
                    &format!(
                        "re-enqueued {} undelivered message(s) to the durable queue thread={root_ts}",
                        msgs.len()
                    ),
                );
            }
        }
        if n > 0
            && let Err(e) = self.threads.save()
        {
            ctx.error("bridge", &format!("threads.json save failed: {e}"));
        }
    }

    /// 逃がしてあった分を配り直す(起動時に1回)。**普通の受信と同じ道**に流すので、
    /// ワーカーが生きていれば押し込み、居なければ起こして渡す(= 中断したスレッドの再開)。
    async fn resume_pending_from_disk(&mut self) {
        for root_ts in self.threads.threads_with_pending() {
            let msgs = self.threads.drain_pending(&root_ts);
            if msgs.is_empty() {
                continue;
            }
            LogCtx {
                session_id: None,
                thread_key: None,
            }
            .info(
                "bridge",
                &format!(
                    "resuming {} message(s) left undelivered by the previous bridge process \
                     thread={root_ts}",
                    msgs.len()
                ),
            );
            for msg in msgs {
                self.on_inbound(&msg).await;
            }
        }
        if let Err(e) = self.threads.save() {
            LogCtx::default().error("bridge", &format!("threads.json save failed: {e}"));
        }
    }

    /// 消された依頼をワーカーに取り下げさせる。
    ///
    /// 順序は Bun と同じ: **先に取り消しの通知を押し込み**、
    /// そのあと ESC で走っているターンを切る。逆にすると、切られたワーカーが取り消しを
    /// 知らないまま次の入力を待つ。
    ///
    /// 最後に台帳から落とす — 消した本人はもう返事を待っていないので、ここを残すと
    /// 「応答待ち」の見張りが取り消しの処理中ずっと居座る(塞いだ穴)。
    fn on_message_deleted(
        &mut self,
        msg: &InboundMsg,
        key: &ThreadKey,
        root_ts: &str,
        deleted: &str,
        ctx: &LogCtx,
    ) {
        if !self.ledger.pending(key).iter().any(|id| id == deleted) {
            ctx.info(
                "bridge",
                &format!(
                    "message_deleted chan={} ts={deleted} -> already answered / not in-flight, \
                     no action",
                    msg.channel
                ),
            );
            return;
        }
        let sid = self.threads.get(root_ts).and_then(|e| e.agent_id.clone());
        let target = sid.as_deref().map(|s| {
            self.workers
                .warm(s)
                .and_then(|h| h.window_id.clone())
                .unwrap_or_else(|| SessionId::from(s.to_string()).window_name())
        });
        match target {
            Some(w) => {
                let envelope = Envelope::of(msg, root_ts, self.deps.clock.now_ms());
                match self.deps.agent.send_text(&Window::of(&w), &envelope) {
                    Ok(()) => ctx.info(
                        "bridge",
                        &format!(
                            "message_deleted chan={} ts={deleted} thread={root_ts} \
                             -> interrupt notify",
                            msg.channel
                        ),
                    ),
                    Err(e) => ctx.error("bridge", &format!("message_deleted delivery failed: {e}")),
                }
                self.user_stop(msg, key, root_ts, ctx);
            }
            None => ctx.info(
                "bridge",
                &format!(
                    "message_deleted chan={} ts={deleted} -> no active worker thread, dropped \
                     (best-effort)",
                    msg.channel
                ),
            ),
        }
        // 取り消された依頼は未応答ではない(見張りの対象から外す)
        self.ledger.disposed(key, &[deleted.to_string()]);
    }

    fn user_stop(&mut self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let sid = self.threads.get(root_ts).and_then(|e| e.agent_id.clone());
        let pending = self.ledger.pending(key);
        let name = sid
            .as_deref()
            .map(|s| SessionId::from(s).window_name())
            .unwrap_or_default();
        let window_id = sid
            .as_deref()
            .and_then(|s| self.workers.warm(s))
            .and_then(|h| h.window_id.clone());
        // 順に短絡する — 未応答が無い stop で tmux を叩きに行かない
        let running = sid.is_some()
            && !pending.is_empty()
            && self.deps.agent.pid_of(window_id.as_deref(), &name).is_some();
        let Some(sid) = sid.as_deref().filter(|_| running) else {
            ctx.info(
                "bridge",
                &format!(
                    "user stop: key={key} nothing running (session={} pending={}) — no ESC",
                    sid.as_deref().unwrap_or("none"),
                    pending.len()
                ),
            );
            ctx.info(
                "bridge",
                &format!(
                    "slack-events: user stop → nothing running for {}:{root_ts}",
                    msg.channel
                ),
            );
            self.post(
                &msg.channel,
                root_ts,
                crate::t!("Nothing is running right now.", "いま止めるものはありません。"),
                key,
            );
            return;
        };
        // 台帳を**先に**落とす — そうしないと ESC で切られたターンが未応答を抱えたまま終わり、
        // stop hook が「応答待ち」の再プロンプトを撃つ
        self.ledger.disposed(key, &pending);
        let target = Window::of(window_id.as_deref().unwrap_or(&name));
        match self.deps.agent.interrupt(&target) {
            Ok(()) => ctx.info(
                "bridge",
                &format!(
                    "user stop: sent ESC to worker window {target} (session {sid}) key={key}, \
                     disposed pending=[{}]",
                    pending.join(",")
                ),
            ),
            Err(e) => ctx.error("bridge", &format!("user stop: ESC to {target} failed: {e}")),
        }
        self.sticky.on_interrupted(key);
        ctx.info(
            "bridge",
            &format!(
                "slack-events: user stop → interrupted running turn for {}:{root_ts}",
                msg.channel
            ),
        );
    }

    /// `exit` / `bye` / `done`。終わらせるのは
    /// **ワーカーであってスレッドではない** — threads.json の entry は残すので、次のメッセージが
    /// `--resume` で同じセッションを継ぐ。走っているターンは切らずに待つ(stop との違い):
    /// 別れの挨拶がワーカーの最後の返信より**下**に着くように。
    async fn user_exit(&mut self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let farewell = Some((msg.channel.clone(), root_ts.to_string()));
        let sid = self.threads.get(root_ts).and_then(|e| e.agent_id.clone());
        let name = sid
            .as_deref()
            .map(|s| SessionId::from(s).window_name())
            .unwrap_or_default();
        let window_id = sid
            .as_deref()
            .and_then(|s| self.workers.warm(s))
            .and_then(|h| h.window_id.clone());
        // 短絡する — セッションの無いスレッドの exit で tmux を叩きに行かない
        let live = sid.is_some() && self.deps.agent.pid_of(window_id.as_deref(), &name).is_some();
        // ワーカーが居ない exit も別れは告げる(現行 performUserExit は session 無しでも
        // farewell まで行く)。待つものが無いので予約せずその場で片付ける
        let Some(sid) = sid.filter(|_| live) else {
            self.terminate(key, None, farewell).await;
            return;
        };
        self.push_drain(key, sid, farewell, None, ctx); // exit は別れの挨拶を出すので shimmer 不要
    }

    /// `resume`。手元の端末で続きを開く1行を
    /// 出し、**線を本当に渡す** — ワーカーが生きていれば exit と同じドレイン後に終わらせる
    /// (別れの挨拶は無し。報告文が終了を告げ終えている)。
    fn user_resume(&mut self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let entry = self.threads.get(root_ts).cloned().unwrap_or_default();
        // セッション ID は**発行しない** — 走った覚えの無いスレッドに渡す id は端末で失敗するだけ
        let Some(sid) = entry.agent_id.filter(|s| !s.is_empty()) else {
            ctx.info(
                "bridge",
                &format!("resume: no bound session for thread tts={root_ts} — nothing to resume"),
            );
            let none = crate::bridge::render::ResumeInfo {
                session_id: None,
                cwd: None,
                transcript_missing: false,
                worker_running: false,
            };
            self.post(&msg.channel, root_ts, none.render(), key);
            return;
        };
        // 履歴の在処は hook が運んできた道が第一(worktree に入ったワーカーは transcript ごと
        // 別プロジェクトに移る — 記録した repo_path は当てにならない。)
        let remembered = self
            .workers
            .warm(&sid)
            .and_then(|h| h.transcript_path.clone());
        let history_cwd = self.deps.agent.session_cwd(remembered.as_deref(), &sid);
        let history_exists = self
            .deps.agent
            .session_history_exists(remembered.as_deref(), &sid);
        let name = SessionId::from(sid.clone()).window_name();
        let window_id = self.workers.window_of(&sid);
        let worker_running = self.deps.agent.pid_of(window_id.as_deref(), &name).is_some();
        let info = crate::bridge::render::ResumeInfo {
            // cwd は id と同じくらい大事 — claude は cwd ごとに履歴を仕舞うので、
            // 違う場所で --resume すると見つからない
            cwd: Some(history_cwd.or(entry.repo_path).unwrap_or_else(Host::home)),
            transcript_missing: !history_exists,
            worker_running,
            session_id: Some(sid.clone()),
        };
        // 他の6箇所と同じ文字単位の頭8字(バイト添字は非 ASCII の id で panic の芽 —
        // ここは select ループの中)。transcript は現行と同じく在処ではなく2値
        let short: String = sid.chars().take(8).collect();
        ctx.info(
            "bridge",
            &format!(
                "resume: session={short} cwd={} transcript={} worker={}",
                info.cwd.as_deref().unwrap_or(""),
                if history_exists { "found" } else { "MISSING" },
                if worker_running { "RUNNING" } else { "ABSENT" }
            ),
        );
        self.post(&msg.channel, root_ts, info.render(), key);
        if worker_running {
            ctx.info(
                "bridge",
                &format!(
                    "slack-events: resume command → terminating live worker for {key} \
                     (session handed to the Owner's terminal)"
                ),
            );
            // shimmer は**ドレインに預ける**。resume 本体は tmux を1回覗くだけで終わるので、
            // ここで guard を持っても set と clear が連続して飛ぶだけで一度も描画されない。
            // 実際に待つのは「返事が捌けるか30秒」を待つドレイン側(現行 Bun に原文なし)
            let thinking =
                slack::Thinking::new(&self.slack_api, &msg.channel, root_ts, &slack::Status::Resume.text()); // TODO(Task 12)
            self.push_drain(key, sid, None, Some(thinking), ctx);
        }
    }

    /// 終了予約を積む。同じスレッドの2本目は積まない — 積むと同じ窓を二度殺しに行き、
    /// 二度目の別れを告げる(現行の in-flight guard 相当)。
    fn push_drain(
        &mut self,
        key: &ThreadKey,
        session_id: String,
        farewell: Option<(String, String)>,
        thinking: Option<slack::Thinking>,
        ctx: &LogCtx,
    ) {
        if self.workers.is_draining(key) {
            ctx.debug(
                "bridge",
                &format!("exit: already draining key={key} — ignoring the second request"),
            );
            return;
        }
        let pending = self.ledger.pending(key);
        if !pending.is_empty() {
            ctx.info(
                "bridge",
                &format!(
                    "exit: waiting for in-flight reply before terminating key={key} pending=[{}]",
                    pending.join(",")
                ),
            );
        }
        self.workers.push_drain(worker::DrainJob {
            key: key.clone(),
            session_id,
            deadline_ms: self.deps.clock.now_ms() + DRAIN_TIMEOUT_MS,
            farewell,
            thinking,
        });
    }

    /// 満期の来た終了予約を実行する。待ちの終わりは「未応答が空になった」か 30 秒のどちらか早い方
    /// (見るのは**生死ではなく未応答**なので、
    /// ドレイン中の一瞬の respawn 隙間で早まって殺すことがない)。
    async fn run_drains(&mut self) {
        let now = self.deps.clock.now_ms();
        // ponytail: select 直列・1 tick 1本・最悪 ~3秒(SIGTERM 猶予 + SIGKILL 待ち)。この間
        // main ループは止まるので、Stop hook の 5 秒枠(endpoints.rs STOP_DECISION_CAP)を割らない
        // ように**同一 tick で2本殺さない** — 残りは次の tick(500ms 後)。並行 kill が要るなら
        // kill を spawn に逃がす(hooked 除去の順序に注意)
        let ledger = &self.ledger;
        let Some(i) = self
            .workers
            .due_drain(now, |key| ledger.pending(key).is_empty())
        else {
            return;
        };
        let job = self.workers.take_drain(i);
        let ctx = LogCtx {
            session_id: Some(job.session_id.clone()),
            thread_key: Some(job.key.clone()),
        };
        let remaining = self.ledger.pending(&job.key);
        if remaining.is_empty() {
            ctx.info(
                "bridge",
                &format!(
                    "exit: in-flight reply drained — terminating key={}",
                    job.key
                ),
            );
        } else {
            ctx.info(
                "bridge",
                &format!(
                    "exit: receipt timeout reached with undisposed=[{}] — terminating anyway \
                     key={}",
                    remaining.join(","),
                    job.key
                ),
            );
        }
        self.terminate(&job.key, Some(&job.session_id), job.farewell)
            .await;
        // 待っていたものが終わった**後**に消す(Drop = クリア)。scope 終端の暗黙 Drop でも
        // 同じ順序になるが、ここが消えるタイミングだと読めるように明示しておく
        drop(job.thinking);
    }

    /// ワーカーを終わらせる本体(順序をそのまま)。
    /// **threads.json の entry は触らない** — セッションの紐付けが残るからこそ resume できる。
    async fn terminate(
        &mut self,
        key: &ThreadKey,
        sid: Option<&str>,
        farewell: Option<(String, String)>,
    ) {
        let ctx = LogCtx {
            session_id: sid.map(str::to_string),
            thread_key: Some(key.clone()),
        };
        // 1. kill の**前に**未応答を落とす。残したまま殺すと、切断を見たワーカー回収経路が
        // 「返事を失った」と読んで代わりを cold spawn する
        self.ledger.dispose_all(key);
        let (_, root) = key.split();
        let root_ts = root.unwrap_or_default();
        // 2. 実プロセスを pid 指名で殺してから窓を閉じる。pid を引けないまま窓だけ閉じると
        // claude が孤児として生き残る(F5)
        if let Some(sid) = sid {
            let name = SessionId::from(sid.to_string()).window_name();
            let window_id = self.workers.window_of(sid);
            match self.deps.agent.pid_of(window_id.as_deref(), &name) {
                Some(pid) => {
                    ctx.info(
                        "bridge",
                        &format!(
                            "exit: terminating worker session={sid} by pid [{pid}] and closing \
                             window key={key}"
                        ),
                    );
                    pid.kill_graceful(tmux_mod::KILL_GRACE_MS).await;
                }
                None => ctx.info(
                    "bridge",
                    &format!(
                        "exit: no live worker process for session={sid} — closing window only \
                         key={key}"
                    ),
                ),
            }
            // 窓名で落ちてくる道がある(継承ワーカーは window_id を覚えていない)。素の名前を
            // -t に渡すと tmux が**今いるセッション**に当てる — 必ずセッション修飾を通す
            let target = Window::of(window_id.as_deref().unwrap_or(&name));
            if let Err(e) = self.deps.agent.terminate(&target) {
                ctx.error("bridge", &format!("exit: kill-window failed: {e}"));
            }
            // 3. セッション鍵の記憶だけ落とす(threads.json は無傷 → 次のメッセージが --resume)
            self.workers.forget(sid);
        }
        // 4. 待たせていた分は捨てる。残すと、次に立つワーカーの user_prompt で
        // もう誰も待っていない古いメッセージが流し込まれる
        if let Some(q) = self.pending.remove(&root_ts) {
            ctx.info(
                "bridge",
                &format!(
                    "exit: cleared {} stranded pending msg(s) so they aren't delivered to the \
                     next worker key={key}",
                    q.len()
                ),
            );
        }
        ctx.info(
            "bridge",
            &format!(
                "exit: worker terminated, thread kept for resume key={key} session={}",
                sid.unwrap_or("none")
            ),
        );
        if let Some((channel, thread_ts)) = farewell {
            self.post(
                &channel,
                &thread_ts,
                crate::t!("Ending this session. Your next message here starts it again.", "このセッションを終えます。次にこのスレッドに書けば、また始まります。"),
                key,
            );
            ctx.info(
                "bridge",
                &format!("slack-events: user exit → session ended for {channel}:{thread_ts}"),
            );
        }
    }

    /// `context` / `ctx`。このスレッド自身の
    /// セッションを `--fork-session` で複製して `/context` を訊く — 走っているワーカーには
    /// 触らないので、ターンの最中でも答えが出る。probe は10〜20秒かかるので投げっぱなし。
    fn user_context(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let entry = self.threads.get(root_ts);
        // セッションが無いスレッドには訊く先が無い
        let Some(sid) = entry.and_then(|e| e.agent_id.clone()) else {
            ctx.info(
                "bridge",
                &format!("context: no bound session for thread tts={root_ts} — nothing to probe"),
            );
            self.post(&msg.channel, root_ts, no_session(), key);
            return;
        };
        let cwd = entry
            .and_then(|e| e.repo_path.clone())
            .unwrap_or_else(Host::home);
        let short: String = sid.chars().take(8).collect();
        ctx.info(
            "bridge",
            &format!("context: probing /context tts={root_ts} session={short} cwd={cwd}"),
        );
        let argv = self.deps.agent.context_argv(&sid);
        let (api, channel, root, key) = (
            self.slack_api.clone(), // TODO(Task 12): post_now
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        let thinking =
            slack::Thinking::new(&self.slack_api, &msg.channel, root_ts, &slack::Status::Context.text()); // TODO(Task 12)
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア(probe が失敗しても消える)
            let ctx = LogCtx {
                session_id: Some(sid),
                thread_key: Some(key.clone()),
            };
            let out = match agent.probe(argv, cwd).await {
                Ok(raw) => {
                    ctx.info(
                        "bridge",
                        &format!(
                            "context: probe ok tts={root} session={short} bytes={}",
                            raw.len()
                        ),
                    );
                    match agent.context_report(&raw) {
                        Some(r) => r.render(),
                        None => {
                            ctx.error(
                                "bridge",
                                &format!(
                                    "slack-events: context command — could not parse /context \
                                     output for {channel}:{root}"
                                ),
                            );
                            crate::t!("Couldn't read the context usage.", "コンテキストの使用量を読み取れませんでした。")
                        }
                    }
                }
                // 失敗の理由は英語の内部文字列 — ログに置き、スレッドには流さない
                Err(ProbeErr::Failed(e)) => {
                    ctx.error(
                        "bridge",
                        &format!("context: probe failed tts={root} session={short}: {e}"),
                    );
                    ctx.error(
                        "bridge",
                        &format!(
                            "slack-events: context command probe failed for {channel}:{root}: {e}"
                        ),
                    );
                    crate::t!("Couldn't get the context usage. Try again.", "コンテキストの使用量を取得できませんでした。もう一度試してください。")
                }
                Err(ProbeErr::Errored(e)) => {
                    ctx.error(
                        "bridge",
                        &format!("slack-events: context command errored for {channel}:{root}: {e}"),
                    );
                    crate::t!("Something went wrong while getting the context usage.", "コンテキストの使用量を取得する途中でエラーが起きました。")
                }
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// `usage` / `usg`。アカウント全体の話
    /// なのでスレッドもセッションも要らない — Home で `claude -p /usage` を回すだけ。
    fn user_usage(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let cwd = Host::home();
        ctx.info("bridge", &format!("usage: probing /usage cwd={cwd}"));
        let argv = self.deps.agent.usage_argv();
        let (api, channel, root, key) = (
            self.slack_api.clone(), // TODO(Task 12): post_now
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        let thinking = slack::Thinking::new(&self.slack_api, &msg.channel, root_ts, &slack::Status::Usage.text()); // TODO(Task 12)
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア
            let ctx = LogCtx {
                session_id: None,
                thread_key: Some(key.clone()),
            };
            let out = match agent.probe(argv, cwd).await {
                Ok(raw) => {
                    ctx.info("bridge", &format!("usage: probe ok bytes={}", raw.len()));
                    match agent.usage_rows(&raw) {
                        // 300 = "Current session" の窓(5h)。"Current week" 行の窓は
                        // format_usage_report_with_projection が内側で差し替える
                        Some(rows) => crate::bridge::render::UsageReport {
                            rows: &rows,
                            projection: Some((crate::bridge::command::WallClock::now(), 300)),
                        }
                        .render(),
                        None => {
                            ctx.error(
                                "bridge",
                                &format!(
                                    "slack-events: usage command — could not parse /usage \
                                     output for {channel}:{root}"
                                ),
                            );
                            crate::t!("Couldn't read your usage.", "使用状況を読み取れませんでした。")
                        }
                    }
                }
                Err(ProbeErr::Failed(e)) => {
                    ctx.error("bridge", &format!("usage: probe failed: {e}"));
                    ctx.error(
                        "bridge",
                        &format!(
                            "slack-events: usage command probe failed for {channel}:{root}: {e}"
                        ),
                    );
                    crate::t!("Couldn't get your usage. Try again.", "使用状況を取得できませんでした。もう一度試してください。")
                }
                Err(ProbeErr::Errored(e)) => {
                    ctx.error(
                        "bridge",
                        &format!("slack-events: usage command errored for {channel}:{root}: {e}"),
                    );
                    crate::t!("Something went wrong while getting your usage.", "使用状況を取得する途中でエラーが起きました。")
                }
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// TUI を叩くコマンド(compact / model / effort)の共通の入口。
    /// セッションと窓を解決し、ターンが走っている間は断る — 走行中の TUI には打ち込めない。
    /// `Ok((target, session_id))` の時だけ tmux を叩いてよい。`Err` はそのまま返す1行。
    fn tui_guard(
        &self,
        label: &str,
        no_session_tail: &str,
        busy_tail: &str,
        key: &ThreadKey,
        root_ts: &str,
        ctx: &LogCtx,
    ) -> Result<(Window, String), String> {
        let Some(sid) = self.threads.get(root_ts).and_then(|e| e.agent_id.clone()) else {
            ctx.info(
                "bridge",
                &format!("{label}: no bound session for thread tts={root_ts} — {no_session_tail}"),
            );
            return Err(no_session());
        };
        let name = SessionId::from(sid.clone()).window_name();
        let window_id = self.workers.window_of(&sid);
        // 未応答を先に見る — 空なら tmux を叩きに行かない(user_stop と同じ短絡)
        let pending = self.ledger.pending(key);
        if !pending.is_empty() && self.deps.agent.pid_of(window_id.as_deref(), &name).is_some() {
            ctx.info(
                "bridge",
                &format!(
                    "{label}: turn in-flight for key={key} (pending={}) — {busy_tail}",
                    pending.len()
                ),
            );
            return Err(crate::t!(
                "The agent is busy. Send `stop` first, then `{label}`.",
                "エージェントが作業中です。`stop` で止めてから `{label}` を送ってください。"
            ));
        }
        Ok((Window::of(window_id.as_deref().unwrap_or(&name)), sid))
    }

    /// `compact`。**生きているセッション**の
    /// TUI に `/compact` を打ち込み、進捗スピナーを Slack の1本の付箋に流し込む
    /// (context/usage と違って使い捨ての probe ではない — 圧縮するのは走っている会話そのもの)。
    /// 最長6分かかるので select ループの外(spawn)で回す。
    fn user_compact(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let (target, sid) = match self.tui_guard(
            "compact",
            "nothing to compact",
            "refusing (busy)",
            key,
            root_ts,
            ctx,
        ) {
            Ok(v) => v,
            Err(text) => {
                self.post(&msg.channel, root_ts, text, key);
                return;
            }
        };
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        // compact に専用の thinking status は**付けない**(Bun からの逸脱 — ユーザー判断)。
        // 以前の実装はここで「考え中」を張り、tick ごとに秒数付きへ張り直していた。
        // こちらは進捗チェックリスト(run_compact が投稿して編集し続ける sticky)が同じことを
        // 見せているので、shimmer と二重で冗長という判断(**移植漏れではない**)。
        tokio::spawn(Self::run_compact(api, self.deps.agent.clone(), channel, root, key, target, sid));
    }

    /// `model`。名前付きは TUI に
    /// `/model <名前>` を打ち込む。素の `model` は transcript を読むだけ — ワーカーに触らない。
    fn user_model(
        &self,
        msg: &InboundMsg,
        name: Option<String>,
        key: &ThreadKey,
        root_ts: &str,
        ctx: &LogCtx,
    ) {
        let Some(name) = name else {
            self.model_show(msg, key, root_ts, ctx);
            return;
        };
        let (target, sid) = match self.tui_guard(
            "model",
            "nothing to switch",
            "refusing (busy)",
            key,
            root_ts,
            ctx,
        ) {
            Ok(v) => v,
            Err(text) => {
                self.post(&msg.channel, root_ts, text, key);
                return;
            }
        };
        let (api, channel, root, key) = (
            self.slack_api.clone(), // TODO(Task 12): post_now
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        let thinking = slack::Thinking::new(&self.slack_api, &msg.channel, root_ts, &slack::Status::Model.text()); // TODO(Task 12)
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア(TUI が確定しなくても消える)
            let ctx = LogCtx {
                session_id: Some(sid),
                thread_key: Some(key.clone()),
            };
            // None = このエージェントが model 切替に非対応。実体が1つの今は起きない
            let done = agent.set_model(&target, &name, &key, &ctx).await == Some(true);
            let out = if done {
                crate::t!("✅ Switched this thread's model to *{name}*.", "✅ このスレッドのモデルを *{name}* に切り替えました。")
            } else {
                crate::t!("Couldn't switch the model. Try again.", "モデルを切り替えられませんでした。もう一度試してください。")
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// 素の `model`。今のモデルを名乗るのは transcript の**最後の**
    /// assistant レコード。TUI も probe も要らないので select ループの中で答える
    /// (読むのは末尾 256KB だけ — 長いセッションの .jsonl は数十 MB ある)。
    fn model_show(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let Some(sid) = self.threads.get(root_ts).and_then(|e| e.agent_id.clone()) else {
            ctx.info(
                "bridge",
                &format!("model: no bound session for thread tts={root_ts} — no current model"),
            );
            self.post(&msg.channel, root_ts, no_session(), key);
            return;
        };
        let short: String = sid.chars().take(8).collect();
        // 履歴の在処は hook が運んできた道が第一(worktree に入ったワーカーは transcript ごと
        // 移る)。忘れていれば総当たりで探す — resume と同じ解決順
        let remembered = self
            .workers
            .warm(&sid)
            .and_then(|h| h.transcript_path.clone());
        let found = self.deps.agent.current_model(remembered.as_deref(), &sid);
        let failed = || crate::t!("Couldn't read the current model.", "今のモデルを読み取れませんでした。");
        let out = match found {
            None => {
                ctx.info(
                    "bridge",
                    &format!(
                        "model: no transcript for session={short} tts={root_ts} — \
                         cannot read current model"
                    ),
                );
                failed()
            }
            Some(read) => match read {
                Err(e) => {
                    ctx.error(
                        "bridge",
                        &format!("model: transcript read failed for session={short}: {e}"),
                    );
                    failed()
                }
                Ok(model) => match model {
                    None => {
                        ctx.info(
                            "bridge",
                            &format!("model: no model id in transcript tail of session={short}"),
                        );
                        failed()
                    }
                    Some(model) => {
                        ctx.info(
                            "bridge",
                            &format!("model: current={model} session={short} key={key}"),
                        );
                        match self.deps.agent.model_alias(&model) {
                            Some(alias) => crate::t!("Model: *{alias}* (`{model}`)", "モデル: *{alias}*(`{model}`)"),
                            None => crate::t!("Model: `{model}`", "モデル: `{model}`"),
                        }
                    }
                },
            },
        };
        self.post(&msg.channel, root_ts, out, key);
    }

    /// `effort`。level 付きは model と同じ
    /// TUI 駆動。素の `effort` は現在値を**どこにも記録が無い**ので TUI に訊く —
    /// スライダを開いて Escape で閉じ、TUI が出す状態行を読む。
    ///
    /// **現行 TUI との差分**(2026-07-29 実機・実弾で3経路とも確認): 履歴なしは即
    /// `Set effort level to …`、履歴ありは確認ダイアログを挟んで**状態行しか出さず**、
    /// 同じ level の選び直しは `Kept effort level as …`。移植元の Bun が知るのは1つ目だけ。
    fn user_effort(
        &self,
        msg: &InboundMsg,
        level: Option<String>,
        key: &ThreadKey,
        root_ts: &str,
        ctx: &LogCtx,
    ) {
        let (tail, busy) = match &level {
            Some(_) => ("nothing to set", "refusing (busy)"),
            None => ("no current level", "refusing read (busy)"),
        };
        let (target, sid) = match self.tui_guard("effort", tail, busy, key, root_ts, ctx) {
            Ok(v) => v,
            Err(text) => {
                self.post(&msg.channel, root_ts, text, key);
                return;
            }
        };
        let (api, channel, root, key) = (
            self.slack_api.clone(), // TODO(Task 12): post_now
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        // 素の `effort` は現在値を読むだけ — 現行も status を出さない(出典は set 側の 3182)
        let Some(level) = level else {
            tokio::spawn(Self::run_effort_show(
                api,
                self.deps.agent.clone(),
                channel,
                root,
                key,
                target,
                sid,
            ));
            return;
        };
        let thinking = slack::Thinking::new(&self.slack_api, &msg.channel, root_ts, &slack::Status::Effort.text()); // TODO(Task 12)
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア
            let ctx = LogCtx {
                session_id: Some(sid),
                thread_key: Some(key.clone()),
            };
            // None = このエージェントが effort に非対応。実体が1つの今は起きない
            let done = agent.set_effort(&target, &level, &key, &ctx).await == Some(true);
            let out = if done {
                crate::t!("✅ Set this thread's effort level to *{level}*.", "✅ このスレッドの effort を *{level}* にしました。")
            } else {
                crate::t!("Couldn't set the effort level. Try again.", "effort を設定できませんでした。もう一度試してください。")
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// `mode`(移植元に無い — Rust 版の新機能)。Claude Code の権限モードは TUI の
    /// shift+tab でしか変えられないので、`/effort` のようなスラッシュコマンドは使わず
    /// **キーを押して**目当てのモードに着くまで回す。素の `mode` はフッタを1回読むだけ。
    fn user_mode(
        &self,
        msg: &InboundMsg,
        name: Option<String>,
        key: &ThreadKey,
        root_ts: &str,
        ctx: &LogCtx,
    ) {
        let (tail, busy) = match &name {
            Some(_) => ("nothing to switch", "refusing (busy)"),
            None => ("no current mode", "refusing read (busy)"),
        };
        let (target, sid) = match self.tui_guard("mode", tail, busy, key, root_ts, ctx) {
            Ok(v) => v,
            Err(text) => {
                self.post(&msg.channel, root_ts, text, key);
                return;
            }
        };
        let ctx = LogCtx {
            session_id: Some(sid),
            thread_key: Some(key.clone()),
        };
        // 読むだけなら tmux を1回叩くだけ — spawn も shimmer も要らない
        let Some(name) = name else {
            let out = match self.deps.agent.mode(&target, &ctx) {
                Some(m) => crate::t!("Permission mode: *{m}*", "権限モード: *{m}*"),
                None => crate::t!("Couldn't read the permission mode.", "権限モードを読み取れませんでした。"),
            };
            self.post(&msg.channel, root_ts, out, key);
            return;
        };
        let (api, channel, root, key) = (
            self.slack_api.clone(), // TODO(Task 12): post_now
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        let thinking = slack::Thinking::new(&self.slack_api, &msg.channel, root_ts, &slack::Status::Mode.text()); // TODO(Task 12)
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア
            let done = agent.set_mode(&target, &name, &key, &ctx).await == Some(true);
            let out = if done {
                crate::t!("✅ Switched this thread's permission mode to *{name}*.", "✅ このスレッドの権限モードを *{name}* にしました。")
            } else {
                crate::t!("Couldn't switch the permission mode. Try again.", "権限モードを切り替えられませんでした。もう一度試してください。")
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// `status`。**本当に動いている**
    /// ワーカーだけを載せる — 生死は tmux の claude pid が唯一の答え(記憶ではなく実物)。
    /// Slack への問い合わせ(permalink / チャンネル名)は select ループの外でやる。
    fn user_status(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let mut threads: Vec<crate::bridge::render::StatusThread> = Vec::new();
        // ponytail: スレッド1本につき tmux list-windows 1回(claude_pid_of が都度呼ぶ)。
        // dev のスレッド数では十分 — 数百本に育ったら現行と同じく
        // 窓の列挙を1回にまとめ、生きた窓名の集合で先に篩う
        for (tts, e) in &self.threads.entries {
            let (Some(channel_id), Some(sid)) = (e.channel_id.clone(), e.agent_id.as_deref())
            else {
                continue;
            };
            let h = self.workers.warm(sid);
            let window = SessionId::from(sid.to_string()).window_name();
            let alive = self
                .deps.agent
                .pid_of(h.and_then(|h| h.window_id.as_deref()), &window);
            if alive.is_none() {
                continue;
            }
            // idle の基準は transcript の mtime = 最後に**本当に働いた**時刻。
            // hook がまだ来ていない継承ワーカーは session id から探す
            let remembered = h.and_then(|h| h.transcript_path.clone());
            let last_activity_ms = self
                .deps.agent
                .last_activity_ms(remembered.as_deref(), sid)
                .unwrap_or(0);
            threads.push(crate::bridge::render::StatusThread {
                channel_id,
                thread_ts: tts.clone(),
                last_activity_ms,
                repo_path: e.repo_path.clone(),
                permalink: None,
                topic: e.topic.clone(),
                channel_name: None,
            });
        }
        ctx.debug(
            "bridge",
            &format!(
                "status: {} live thread(s) of {} recorded",
                threads.len(),
                self.threads.entries.len()
            ),
        );
        let (api, channel, root, key) = (
            self.slack_api.clone(), // TODO(Task 12): post_now
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        // $HOME は `~` 畳みのため(`process.env.HOME`)。Host::home() は
        // 「ルート未設定のワーカーが立つ場所」で別物なので、ここで混ぜない
        let home = std::env::var("HOME").unwrap_or_default();
        // `self` は 'static な spawn へ持ち越せない — 在庫の cwd はここで取り出して move で渡す
        // 数えるのは**使える**在庫だけ(起動途中は「まだ無い」— 諦めた枠はそもそも消えている)
        let pools: Vec<String> = self
            .workers
            .pool_summary()
            .into_iter()
            .filter(|(_, ready)| *ready)
            .map(|(cwd, _)| cwd)
            .collect();
        // 集計は permalink とチャンネル名の解決でスレッド数ぶん Slack を叩く — 待つ間の shimmer
        let thinking =
            slack::Thinking::new(&self.slack_api, &msg.channel, root_ts, &slack::Status::Gathering.text()); // TODO(Task 12)
        let (slack, clock) = (self.deps.slack.clone(), self.deps.clock.clone());
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア。どの経路で抜けても消える
            let ctx = LogCtx {
                session_id: None,
                thread_key: Some(key.clone()),
            };
            // 名前はチャンネルごとに1回だけ解決する(同じ会話に何本もスレッドが立つ)
            let mut names: HashMap<String, Option<String>> = HashMap::new();
            for t in &mut threads {
                t.permalink = match slack.get_permalink(&t.channel_id, &t.thread_ts).await {
                    Ok(p) => Some(p),
                    Err(e) => {
                        ctx.debug(
                            "bridge",
                            &format!(
                                "status: getPermalink failed for {}/{}: {e}",
                                t.channel_id, t.thread_ts
                            ),
                        );
                        None
                    }
                };
                if !names.contains_key(&t.channel_id) {
                    let n = slack.channel_display_name(&t.channel_id).await;
                    names.insert(t.channel_id.clone(), n);
                }
                t.channel_name = names[&t.channel_id].clone();
            }
            let report = crate::bridge::render::StatusReport {
                bridge_version: env!("CARGO_PKG_VERSION").to_string(),
                now_ms: clock.now_ms(),
                home,
                // 繋がり方は1つしかない(Remote は作らない)
                mode: "local".to_string(),
                threads,
                pools,
            }
            .render();
            api.post_now(&channel, &root, report, &key).await;
        });
    }

    /// `pwd` の3形の答え。解決は spawn と同じ resolve_repo_path を
    /// 通すので、表示したパスは必ずワーカーが実際に立つ場所。
    fn pwd_answer(
        &mut self,
        msg: &InboundMsg,
        mode: PwdMode,
        dm: bool,
        root_ts: &str,
        ctx: &LogCtx,
    ) -> String {
        let home = Host::home();
        let entry = |access: &bridge::Access, ch: &str| {
            let (repo_path, is_fallback) = access.repo_path(ch, &home);
            crate::bridge::render::PwdEntry {
                channel_id: ch.to_string(),
                repo_path,
                label: access.routes.get(ch).and_then(|r| r.label.clone()),
                is_fallback,
            }
        };
        match mode {
            PwdMode::Current => {
                entry(&self.access, &msg.channel).render(&crate::t!("Project directory for this channel", "このチャンネルの作業ディレクトリ"))
            }
            PwdMode::All => {
                let all: Vec<_> = self
                    .access
                    .routes
                    .keys()
                    .map(|ch| entry(&self.access, ch))
                    .collect();
                crate::bridge::render::PwdEntry::render_all(&all, &home)
            }
            // DM にはルートが無い(そのワーカーは常に Home で立つ)ので、黙って記録する
            // 代わりにそう言う
            PwdMode::Set(_) if dm || !crate::bridge::command::SlackId::is_channel(&msg.channel) => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: pwd set refused — not a channel (dm={dm}) msg={}",
                        msg.ts
                    ),
                );
                crate::bridge::render::Notice::PwdDmSetRefusal.render()
            }
            PwdMode::Set(path) => {
                let op = bridge::AccessOp::SetRepo {
                    channel: msg.channel.clone(),
                    path: path.clone(),
                };
                match self.access.apply(op) {
                    Ok((access, message, warnings)) => {
                        self.adopt_access(access, ctx);
                        ctx.info(
                            "bridge",
                            &format!(
                                "slack-events: pwd set channel={} path={path} msg={}",
                                msg.channel, msg.ts
                            ),
                        );
                        crate::bridge::render::Notice::with_warnings(&message, &warnings)
                    }
                    // 検証に落ちたパスは、そのエラー文そのものが Owner の読むもの
                    Err(e) => {
                        ctx.error(
                            "bridge",
                            &format!(
                                "slack-events: pwd set failed for {}:{root_ts}: {e}",
                                msg.channel
                            ),
                        );
                        e
                    }
                }
            }
        }
    }

    /// アクセス管理の verb。これは MCP ツールではない
    /// Owner の素のメッセージを配達**前に**Bridge が実行するので、prompt injection の面がゼロ。
    async fn owner_command(
        &mut self,
        msg: &InboundMsg,
        oc: crate::bridge::command::OwnerCmd,
        dm: bool,
        root_ts: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) {
        let op = match oc.verb {
            // `warm on|off [<#channel>]`。引数なしは「今いるチャンネル」— Owner は大抵その
            // チャンネルに立っている。DM はルートが無く常に事前起動なので受けない
            "warm" => {
                // 第1引数が on/off であることは parse_owner_command が保証済み
                let on = oc
                    .args
                    .first()
                    .is_some_and(|a| a.eq_ignore_ascii_case("on"));
                let ch = match oc.args.get(1) {
                    Some(tok) => crate::bridge::command::SlackId::from_channel_mention(tok),
                    None if !dm => Some(msg.channel.clone()),
                    None => None,
                };
                let Some(channel) = ch else {
                    let usage = if dm && oc.args.len() < 2 {
                        crate::t!(
                            "The agent for DMs is always started ahead of time. Name a channel: `warm on|off <#channel>`",
                            "DM のエージェントは常に先に起動しています。チャンネルを指定してください: `warm on|off <#channel>`"
                        )
                    } else {
                        crate::t!("Usage: `warm on|off [<#channel>]`", "使い方: `warm on|off [<#channel>]`")
                    };
                    self.post(&msg.channel, root_ts, usage, key);
                    return;
                };
                bridge::AccessOp::SetWarm { channel, on }
            }
            // Home は実在のチャンネルでなければならない — 打たれたその場所が Home になる
            "set-home" => {
                if dm || !crate::bridge::command::SlackId::is_channel(&msg.channel) {
                    let refusal = crate::t!(
                        "Run `set-home` in the *channel* you want notices in. A DM can't be the notice channel.",
                        "`set-home` は、通知を出したい *チャンネル* で実行してください。DM は通知先にできません。"
                    );
                    self.post(&msg.channel, root_ts, refusal, key);
                    return;
                }
                bridge::AccessOp::SetHome(msg.channel.clone())
            }
            verb @ ("allow-bot" | "remove-bot") => {
                let raw = oc.args.first().map(String::as_str).unwrap_or("");
                let bot_id = if crate::bridge::command::SlackId::is_bot(raw) {
                    raw.to_string() // 素の B… id はそのまま受ける
                } else {
                    // bot は自分の USER id(`<@U…>`)で mention されるが、許可台帳の鍵は bot_id(B…)
                    let Some(uid) = crate::bridge::command::SlackId::from_user_mention(raw) else {
                        self.post(
                            &msg.channel,
                            root_ts,
                            crate::t!("Usage: `{verb} <@bot>`", "使い方: `{verb} <@bot>`"),
                            key,
                        );
                        return;
                    };
                    match self.deps.slack.resolve_bot_id(&uid).await {
                        Ok(Some(b)) => b,
                        Ok(None) => {
                            let human = crate::t!(
                                "`{verb}` needs a bot — that mention is a person.",
                                "`{verb}` にはボットを指定してください。今のメンションは人です。"
                            );
                            self.post(&msg.channel, root_ts, human, key);
                            return;
                        }
                        Err(e) => {
                            ctx.error("bridge", &format!(
                                    "slack-events: owner-command '{verb}' failed for {}:{root_ts}: {e}",
                                    msg.channel
                                ));
                            self.post(&msg.channel, root_ts, e, key);
                            return;
                        }
                    }
                };
                if verb == "allow-bot" {
                    bridge::AccessOp::BotAllow(bot_id)
                } else {
                    bridge::AccessOp::BotRemove(bot_id)
                }
            }
            // 今は parse_owner_command が知っている verb しか寄越さないので届かない枝。
            // それでも黙って消えない— 現行は throw を catch して**その文言をそのまま**
            // Owner に返すので、こちらもそう返す
            other => {
                let e = format!("unknown owner command: {other}");
                ctx.error(
                    "bridge",
                    &format!(
                        "slack-events: owner-command '{other}' failed for {}:{root_ts}: {e}",
                        msg.channel
                    ),
                );
                self.post(&msg.channel, root_ts, e, key);
                return;
            }
        };
        match self.access.apply(op) {
            Ok((access, message, warnings)) => {
                self.adopt_access(access, ctx);
                self.post(
                    &msg.channel,
                    root_ts,
                    crate::bridge::render::Notice::with_warnings(&message, &warnings),
                    key,
                );
            }
            // 検証エラーの文が Owner の読むもの
            Err(e) => {
                ctx.error(
                    "bridge",
                    &format!(
                        "slack-events: owner-command '{}' failed for {}:{root_ts}: {e}",
                        oc.verb, msg.channel
                    ),
                );
                self.post(&msg.channel, root_ts, e, key);
            }
        }
    }

    /// Owner がまだ居ないときだけ通る、サインインの抜け道。
    /// **true = 消費した** — 呼び手はそこで打ち切る。届く場所は2つ:
    ///   • 人間の DM — 進行中のサインインがあれば次の1通を貼り付けコードと読む。素の `login` は
    ///     新しいサインイン。それ以外には短い案内を返す
    ///   • Owner が route したチャンネル — 受けるのは**貼り付けコードだけ**、しかも
    ///     そのサインインを始めた本人からのものだけ(相席の第三者にコードプロンプトを触らせない)
    fn login_carve_out(&mut self, msg: &InboundMsg) -> bool {
        let dm = msg.channel_kind == inbound::ChannelKind::Dm;
        let sender = msg.user.as_deref().unwrap_or("");
        let pending = self.login_pending.get(&msg.channel).cloned();
        if !dm && pending.as_deref() != Some(sender) {
            return false; // 通りすがりはこの枝の外 — 普通に gate へ落とす
        }
        // 返信は「その人が喋ったメッセージ」の下に吊る。DM/チャンネルの根に出すと、
        // 本人が見ているスレッドと違う場所に着く
        let reply_ts = msg.thread_ts.clone().unwrap_or_else(|| msg.ts.clone());
        let key = ThreadKey::new(&msg.channel, &reply_ts);
        let venue = if dm { "dm" } else { "channel" };
        let ch = &msg.channel;
        let ctx = LogCtx::default();
        match pending {
            Some(user) => {
                ctx.info(
                    "bridge",
                    &format!("slack-events: login code from {sender} ({venue} {ch})"),
                );
                self.submit_code(ch.clone(), msg.text.trim().to_string(), reply_ts, user);
            }
            None if crate::bridge::command::Message::new(
                &msg.text,
                self.bot_user_id.as_deref(),
            )
            .is("login") =>
            {
                ctx.info(
                    "bridge",
                    &format!("slack-events: login command from {sender} ({venue} {ch})"),
                );
                self.start_login(ch.clone(), sender.to_string(), reply_ts);
            }
            None => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: owner-less {venue} from {sender} — login guidance \
                         ({venue} {ch})"
                    ),
                );
                self.post(
                    ch,
                    &reply_ts,
                    crate::t!(
                        "Claude Code isn't signed in yet. Send `login` to sign in.",
                        "Claude Code はまだサインインしていません。`login` と送るとサインインを始めます。"
                    ),
                    &key,
                );
            }
        }
        true
    }

    /// サインインの開始。専用の tmux セッションで `claude auth login` を
    /// 回し、印字される認証 URL を掬って返す。ここから先はコード待ち。
    fn start_login(&mut self, channel: String, user: String, reply_ts: String) {
        let key = ThreadKey::new(&channel, &reply_ts);
        let ctx = LogCtx::default();
        // SECURITY: サインインは**全員で1つの** tmux セッションを使う。
        // 別チャンネルの2本目にセッションを作り直させると、1人目のポーリングが2人目の pane を
        // 読み、2人目の成功で**1人目**が Owner になる(他人の認証への相乗り)。同じチャンネルの
        // 撃ち直しは自分の流れをやり直すだけなので通す
        if let Some(other) = self.login_pending.keys().find(|k| **k != channel) {
            ctx.info(
                "bridge",
                &format!(
                    "login: refusing concurrent sign-in for {user} (channel {channel}) — \
                     a sign-in is already in progress (dm {other})"
                ),
            );
            self.post(
                &channel,
                &reply_ts,
                crate::t!(
                    "Another sign-in is in progress. Wait a moment, then send `login` again.",
                    "別のサインインが進行中です。少し待ってから、もう一度 `login` と送ってください。"
                ),
                &key,
            );
            return;
        }
        ctx.info(
            "bridge",
            &format!("login: starting sign-in for {user} (channel {channel})"),
        );
        // 席は判定の**直後**に取る。取るのを spawn の中(URL が出た後)にすると、URL を待つ
        // 最大20秒の間に別チャンネルの2本目が同じ判定をすり抜け、セッションを作り直して
        // 1本目の pane を奪う。席は成否どちらでも LoginFinished が外す。
        // 代償: URL が届くまでの間にこのチャンネルへ来た1通はコード扱いになり失敗の返事になる
        // (Owner は `login` を撃ち直せばよい)
        self.login_pending.insert(channel.clone(), user.clone());
        let (api, cmd_tx, home) = (self.slack_api.clone(), self.cmd_tx.clone(), Host::home()); // TODO(Task 12): post_now
        // サインインは URL を出してからコードを待つ数十秒 — その間ずっと shimmer を出す
        let thinking = slack::Thinking::new(&self.slack_api, &channel, &reply_ts, &slack::Status::Login.text()); // TODO(Task 12)
        // `thinking` は本文で触るので async move が丸ごと持っていく(どの経路で抜けても Drop = クリア)
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let ctx = LogCtx::default();
            let started = agent.login_begin(&home);
            let mut url = None;
            if let Err(e) = started {
                ctx.error(
                    "bridge",
                    &format!(
                        "login: could not start the sign-in session for {user} \
                         (channel {channel}): {e}"
                    ),
                );
            } else {
                // 待つのは URL **だけ**。ここで広いエラー判定を回すと、URL より前の飾りに
                // 紛れた "error/failed" で誤って諦める。本物の早期失敗は
                // 「URL が出ないまま時間切れ」という形で下に現れる
                for _ in 0..URL_POLL_MAX {
                    tokio::time::sleep(LOGIN_POLL).await;
                    // URL が出るまで最大 URL_POLL_MAX 秒 — Slack はそれより早く status を
                    // 失効させるので tick ごとに張り直す(compact と同じ理由)。
                    // 張り直さないと途中で shimmer が消えて「止まった」に見える
                    thinking.set(&slack::Status::Login.text());
                    url = agent.login_url();
                    if url.is_some() {
                        break;
                    }
                }
            }
            let Some(url) = url else {
                ctx.error(
                    "bridge",
                    &format!(
                        "login: no sign-in URL captured within {URL_POLL_MAX}s for {user} \
                         (channel {channel}) — aborting"
                    ),
                );
                // 席を空けるのは main(セッションの kill も向こうがやる)。ここで返さないと
                // 二度と `login` を受け付けなくなる
                let _ = cmd_tx
                    .send(CmdFx::LoginFinished {
                        channel: channel.clone(),
                        bound: None,
                    })
                    .await;
                api.post_now(&channel, &reply_ts, crate::t!(
                        "Couldn't start the sign-in. Wait a moment, then send `login` again.",
                        "サインインを始められませんでした。少し待ってから、もう一度 `login` と送ってください。"
                    ), &key)
                .await;
                return;
            };
            api.post_now(&channel, &reply_ts, crate::t!(
                    "🔐 Open this link in your browser and sign in to Claude, then paste the code it shows as your *next message* in this thread:\n{url}",
                    "🔐 このリンクをブラウザで開いて Claude にサインインし、表示されたコードを、このスレッドの *次のメッセージ* として貼ってください:\n{url}"
                ), &key)
            .await;
            ctx.info(
                "bridge",
                &format!(
                    "login: sign-in URL sent to {user} (channel {channel}) — awaiting pasted code"
                ),
            );
        });
    }

    /// 貼られたコードを待っている CLI に流し込み、画面の判定を待つ。
    /// Owner が縛られるのは**明示の成功マーカーを見たときだけ** — 沈黙も中断も成功ではない。
    ///
    /// ponytail: CLI がシェルに戻ったことの検知(現行の paneCommand)は持たない — 30 秒の
    /// 時間切れで代替する(最悪、失敗の返事が最大 30 秒遅れるだけ)
    fn submit_code(&self, channel: String, code: String, reply_ts: String, user: String) {
        let key = ThreadKey::new(&channel, &reply_ts);
        LogCtx::default().info(
            "bridge",
            &format!("login: submitting pasted code for {user} (channel {channel})"),
        );
        let (api, cmd_tx) = (self.slack_api.clone(), self.cmd_tx.clone()); // TODO(Task 12): post_now
        // サインインの後半(貼られたコードの判定、最大 CODE_POLL_MAX 秒)も待ち時間 —
        // login_start の guard は URL を出した時点で落ちているので、ここで張り直す
        let thinking = slack::Thinking::new(&self.slack_api, &channel, &reply_ts, &slack::Status::Login.text()); // TODO(Task 12)
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let ctx = LogCtx::default();
            let mut outcome = "timeout";
            if let Err(e) = agent.login_submit_code(&code) {
                ctx.error(
                    "bridge",
                    &format!(
                        "login: could not submit the pasted code for {user} \
                         (channel {channel}): {e}"
                    ),
                );
                outcome = "error";
            } else {
                for _ in 0..CODE_POLL_MAX {
                    tokio::time::sleep(LOGIN_POLL).await;
                    thinking.set(&slack::Status::Login.text()); // 失効させない(URL 待ちと同じ)
                    // scrollback ごと読む: "Login successful." を出した直後に CLI はシェルへ
                    // 戻り、次のポーリングまでに印が画面外へ流れる
                    match agent.login_outcome() {
                        LoginOutcome::Success => {
                            outcome = "success";
                            break;
                        }
                        LoginOutcome::Error => {
                            outcome = "error";
                            break;
                        }
                        LoginOutcome::Pending => {}
                    }
                }
            }
            // セッションの片付けと pending の削除、Owner の書き込みは main の仕事
            let (text, bound) = if outcome == "success" {
                (
                    crate::t!(
                        "Signed in ✅ — you're now the owner of this bot.",
                        "サインインしました ✅ — あなたがこのボットの Owner になりました。"
                    ),
                    Some(user),
                )
            } else {
                ctx.error(
                    "bridge",
                    &format!(
                        "login: sign-in {outcome} for {user} (channel {channel}) — no Owner bound"
                    ),
                );
                (
                    crate::t!(
                        "Sign-in failed — the code may be wrong or expired. Send `login` to try again.",
                        "サインインできませんでした。コードが違うか、期限が切れた可能性があります。`login` と送ってやり直してください。"
                    ),
                    None,
                )
            };
            let _ = cmd_tx
                .send(CmdFx::LoginFinished {
                    channel: channel.clone(),
                    bound,
                })
                .await;
            api.post_now(&channel, &reply_ts, text.to_string(), &key)
                .await;
        });
    }

    /// `logout`。CLI のサインアウトだけを
    /// spawn で回し、ワーカーの後片付けと Owner の解除は main に戻してやる。
    fn user_logout(&mut self, channel: &str, root_ts: &str) {
        // restart の札と違ってこれは**本当に効く** — user_logout は spawn を撒いてすぐ返るので、
        // `claude auth logout` が走っている数秒の間に2通目の logout が届きうる
        if self.signing_out {
            LogCtx {
                session_id: None,
                thread_key: Some(ThreadKey::new(channel, root_ts)),
            }
            .info("bridge", "logout ignored — already signing out");
            return;
        }
        self.signing_out = true;
        // shimmer は `claude auth logout` が返るまで。この後のワーカー畳みは main 側
        // (CmdFx::LogoutFinished)なので、ここで持たせておけば **必ず** 消える
        let thinking = slack::Thinking::new(&self.slack_api, channel, root_ts, &slack::Status::Logout.text()); // TODO(Task 12)
        let (cmd_tx, channel, thread_ts) = (
            self.cmd_tx.clone(),
            channel.to_string(),
            root_ts.to_string(),
        );
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア
            let ctx = LogCtx::default();
            ctx.info("bridge", "logout: signing out (claude auth logout)");
            let ok = match agent.logout().await {
                Ok(()) => {
                    ctx.info("bridge", "logout: claude auth logout OK");
                    true
                }
                Err(ProbeErr::Failed(status)) => {
                    ctx.error(
                        "bridge",
                        &format!("logout: 'claude auth logout' exited {status}"),
                    );
                    false
                }
                Err(ProbeErr::Errored(e)) => {
                    ctx.error(
                        "bridge",
                        &format!("logout: 'claude auth logout' threw: {e}"),
                    );
                    false
                }
            };
            // サインアウトの成否に関わらず片付けは走らせる — 半端にワーカーだけ生き残る方が悪い
            let _ = cmd_tx
                .send(CmdFx::LogoutFinished {
                    ok,
                    channel,
                    thread_ts,
                })
                .await;
        });
    }

    /// 起動/終了の home 通知を1回投げる。**失敗はログだけ**
    /// 起動シーケンスも restart も止めない。DM フォールバックは未実装(沈黙はしない)。
    /// home に「online」を出す。**起動のときと、親との link を張り直したとき**の両方で使う
    /// (子にとって「繋がった」を人に知らせるのはこの1行だけ — 親側の presence は 🔴 だけを言う)。
    async fn announce_online(&mut self, connected_as: &str) {
        let pools: Vec<String> = self.access.pool_targets(&Host::home());
        let text = crate::bridge::render::Notice::Online {
            label: Host::name().await,
            // 人が読む通知なので**名前**を出す(現行と同じ)
            connected_as: connected_as.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            pools,
            pending: 0,
        }
        .render();
        self.post_notice(&text, &LogCtx::default()).await;
    }

    async fn post_notice(&self, text: &str, ctx: &LogCtx) {
        match bridge::NoticeTarget::of(self.access.home_channel.as_deref(), &self.access.owner) {
            bridge::NoticeTarget::Home(ch) => {
                slack::Api::brief_call(
                    &format!("home notice post failed for {ch}"),
                    self.deps.slack.post_message_no_unfurl(&ch, text, None),
                    ctx,
                )
                .await;
            }
            // home チャンネルが無ければ Owner の DM に落とす。
            // DM は開き直しても同じ id が返るので、その場で開いて投げる
            bridge::NoticeTarget::OwnerDm(owner) => match self.deps.slack.open_dm(&owner).await {
                Ok(ch) => {
                    slack::Api::brief_call(
                        &format!("owner DM notice post failed for {owner}"),
                        self.deps.slack.post_message_no_unfurl(&ch, text, None),
                        ctx,
                    )
                    .await;
                }
                Err(e) => ctx.error(
                    "bridge",
                    &format!("home notice skipped: could not open a DM with {owner}: {e}"),
                ),
            },
            bridge::NoticeTarget::None_ => ctx.info(
                "bridge",
                "home notice skipped: no home_channel and no owner — nothing to notify",
            ),
        }
    }

    /// `restart`(簡約)。この Bridge を
    /// **降ろす**のが仕事 — 起こし直すのは supervisor で、チェックリストは後継が閉じる。
    ///
    /// `req` = 要求者スレッド `(channel, root_ts)`。Slack の `restart` は Some、運用者の
    /// SIGUSR1 は None(答える相手が居ないので、チェックリストも thinking status も marker も
    /// 出さない — の `req ? … : undefined` と同じ分岐)。
    ///
    /// ponytail: プラグイン更新は持たない(Rust はバイナリ1個 — 差し替えは install script の仕事)。
    /// 未応答は「予告を出して置いていく」— 自動再開はまだ無いので、続きは Owner が押し直す
    async fn maintenance_restart(&mut self, source: &str, req: Option<(&str, &str)>, ctx: &LogCtx) {
        // select の1腕は最後まで走るので今の実装で2本目は入らないが、順序の約束として置く
        if self.restarting {
            ctx.info(
                "bridge",
                &format!("maintenance restart ignored — already in flight ({source})"),
            );
            return;
        }
        self.restarting = true;
        // exit(0) は Drop を走らせない — restart だけは slack::Thinking guard を使わず、
        // set も clear も**その場で await** する(投げっぱなしだと exitがタスクごと殺す)
        if let Some((channel, root_ts)) = req {
            slack::Api::brief_call(
                "restart: thinking status set failed",
                self.deps.slack
                    .set_thinking_status(channel, root_ts, &slack::Status::Restart.text()),
                ctx,
            )
            .await;
        }
        // (a) 進捗チェックリストを1本投稿して ts を控える(以後これを編集し続ける)。**投稿に
        // 失敗しても再起動は止めない** — 表は飾り
        let progress_ts = match req {
            Some((channel, root_ts)) => {
                let first = RestartPhase::Received.render(None);
                slack::Api::brief_call(
                    &format!("slack-events: restart progress post failed for {channel}:{root_ts}"),
                    self.deps.slack
                        .post_message_no_unfurl(channel, &first, Some(root_ts)),
                    ctx,
                )
                .await
            }
            None => None,
        };
        ctx.info(
            "bridge",
            &format!("maintenance restart triggered ({source})"),
        );
        // (b) まだ返事を借りているスレッドには断りを入れる
        for key in self.ledger.pending_keys() {
            let (channel, thread) = key.split();
            slack::Api::brief_call(
                &format!("restart notice failed key={key}"),
                self.deps.slack
                    .post_message_no_unfurl(&channel, &restart_notice(), thread.as_deref()),
                ctx,
            )
            .await;
        }
        // (c) 後継への引き継ぎ。これが無いとチェックリストは「◌ …」のまま凍る。
        //     要求者が居ないときは書かない — ✅ を返す宛先が無いのに marker を残すと、
        // 後の無関係な起動が拾って的外れな「完了」を出す(不変条件)
        if let Some((channel, root_ts)) = req {
            let marker = serde_json::json!({
                "channel": channel,
                "thread_ts": root_ts,
                "progress_ts": progress_ts,
            });
            match self.deps.dir.write_json_atomic("restart-marker.json", &marker) {
                Ok(()) => ctx.info("bridge", &format!(
                        "wrote restart marker (requester carrier for the ✅ reply) \
                         (requester {channel}:{root_ts})"
                    )),
                Err(e) => ctx.error("bridge", &format!(
                        "could not write restart marker (the ✅ back-online reply may be skipped): {e}"
                    )),
            }
        }
        // (d) 降りることを home に1回知らせる(後継が online 通知を出す)
        let offline = crate::bridge::render::Notice::Offline {
            label: Host::name().await,
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            reason: "restart".to_string(),
        }
        .render();
        self.post_notice(&offline, ctx).await;
        // (e) 「Bridge を停止」まで done にしてから降りる。後継が繋がるまでの数秒、表は凍る
        if let (Some((channel, _)), Some(ts)) = (req, &progress_ts) {
            let switching = RestartPhase::Switching.render(None);
            slack::Api::brief_call(
                &format!("restart: progress checklist update failed for {channel}:{ts}"),
                self.deps.slack.update_message(channel, ts, &switching),
                ctx,
            )
            .await;
        }
        // (f) 在庫は**畳まない**。畳むと後継が新しいセッションを切り直すことになり、
        //     再起動のたびに使い捨ての在庫セッションが claude の履歴に積まれる。
        //     後継は pools.json の指名を頼りに、生き残りを `restore_pools` で拾い直す
        //     (死んでいた枠だけ同じ session_id を `--resume` で起こす)
        // 降りる前に必ず消す。残すとこのスレッドの shimmer は誰にも消されず居座る
        // (後継は自分が張っていない status を知らない)
        if let Some((channel, root_ts)) = req {
            slack::Api::brief_call(
                "restart: thinking status clear failed",
                self.deps.slack.set_thinking_status(channel, root_ts, ""),
                ctx,
            )
            .await;
        }
        ctx.info(
            "bridge",
            "maintenance restart: stepping down now (the supervisor brings the successor up)",
        );
        // 最後の tick 以降の変化を落としてから降りる — ワーカーは生き残るので、後継が
        // 未応答を拾い直せないと、そのスレッドは見張りの外に落ちる
        self.save_ledger();
        self.flush_pending_to_disk(ctx);
        std::process::exit(0);
    }

    /// spawn したサインイン・サインアウトが戻してきた状態変更を、main の側で1つずつ適用する。
    async fn on_cmd_fx(&mut self, fx: CmdFx) {
        let ctx = LogCtx::default();
        match fx {
            CmdFx::LoginFinished { channel, bound } => {
                self.deps.agent.login_kill();
                self.login_pending.remove(&channel);
                let Some(user) = bound else { return };
                let mut access = self.access.clone();
                access.owner.clone_from(&user);
                self.adopt_access(access, &ctx);
                ctx.info(
                    "bridge",
                    &format!("login: SUCCESS — {user} bound as Owner (channel {channel})"),
                );
                // サインインは仕切り直し — 前の認証状態で諦めた枠をもう一度試す
                // (これが無いと一時的な失敗で枠が Bridge の寿命いっぱい空く)
                if !self.workers.gave_up_count() == 0 {
                    ctx.info(
                        "bridge",
                        &format!(
                            "login: retrying {} pool(s) given up on earlier",
                            self.workers.gave_up_count()
                        ),
                    );
                }
                self.workers.clear_gave_up();
                // Owner が決まって初めて pool_targets が実体を持つ — ここが初回の在庫作り
                self.start_missing_pool_workers(&ctx);
            }
            CmdFx::LogoutFinished {
                ok,
                channel,
                thread_ts,
            } => {
                self.teardown_all_workers(
                    "logout",
                    &format!(" (claude auth logout ok={ok})"),
                    &ctx,
                )
                .await;
                let mut access = self.access.clone();
                access.owner.clear();
                self.adopt_access(access, &ctx);
                self.login_pending.clear();
                self.signing_out = false;
                ctx.info(
                    "bridge",
                    "logout: Owner cleared — bot is now Owner-less (login required)",
                );
                let key = ThreadKey::new(&channel, &thread_ts);
                self.post(
                    &channel,
                    &thread_ts,
                    crate::t!(
                        "Signed out. Send `login` to sign in again.",
                        "サインアウトしました。また使うときは `login` と送ってサインインしてください。"
                    ),
                    &key,
                );
            }
            CmdFx::SpawnScreen { outcome, what } => {
                let ctx = LogCtx::default();
                match outcome {
                    // pane は**疑い**でしかない。裏を取るのは既存の /usage 見張りの仕事なので、
                    // 次の flush で見に行くよう期限を過去に倒すだけ。**0 は使わない** —
                    // `usage_tick` が「初回なので少し待つ」の合図に使っているので、0 を書くと
                    // 確認が走らないどころか次の probe が 60 秒先送りになる
                    SpawnOutcome::UsageLimited => {
                        ctx.error(
                            "bridge",
                            &format!(
                                "spawn screen of {what} reads like the USAGE-LIMIT modal — \
                                 asking the usage monitor to confirm against /usage now \
                                 (the pane alone does NOT gate the fleet)"
                            ),
                        );
                        self.usage_polled_at_ms = 1;
                    }
                    // フリートゲートは張らない(`claude auth status` の裏取りが無い)。
                    // 言うだけ。Owner は `login` を送れる
                    SpawnOutcome::LoginRequired => {
                        ctx.error(
                            "bridge",
                            &format!(
                                "spawn screen of {what} reads like the LOGIN screen — the worker \
                                 is stuck at it and no key clears it. NOT gating the fleet \
                                 (no auth-status confirmation here yet); telling the Owner"
                            ),
                        );
                        self.post_notice(
                            &crate::t!(
                                "⚠️ An agent stopped at Claude's sign-in screen. Send `login` to sign in again.",
                                "⚠️ エージェントが Claude のサインイン画面で止まりました。`login` と送ってサインインし直してください。"
                            ),
                            &ctx,
                        )
                        .await;
                    }
                    SpawnOutcome::Answered | SpawnOutcome::NoScreen => {}
                }
            }
        }
    }

    /// 変異後の access を採用して落とす。以後の gate / resolve_repo_path はこれを読む
    /// (保存に失敗しても採用はする — 今の答えと食い違う方が混乱する)。
    fn adopt_access(&mut self, access: bridge::Access, ctx: &LogCtx) {
        if let Err(e) = access.save(&self.deps.dir) {
            ctx.error("bridge", &format!("access.json save failed: {e}"));
        }
        self.access = access;
        // 設定が変わったら在庫を**その場で**合わせる。次の死亡や再起動を待つと
        // `warm off` が何も解放しないまま居座る
        self.start_missing_pool_workers(ctx);
    }

    /// Bridge 直答の1本。呼び手は待たない — Slack の返事は台帳の外の出来事。
    /// このスレッドで一度でも喋ったことを threads.json に残す。**セッションは紐付けない** —
    /// 印だけ(`agent_id` 無し = ワーカーはまだ居ない)。次にここへ来たメッセージは
    /// 「動いているスレッドの続き」として、名指し無しでも通る。既にエントリがあれば触らない。
    fn mark_thread_seen(&mut self, channel: &str, root_ts: &str, ctx: &LogCtx) {
        if self.threads.get(root_ts).is_some() {
            return;
        }
        self.threads.upsert(
            root_ts,
            bridge::ThreadEntry {
                channel_id: Some(channel.to_string()),
                ..Default::default()
            },
        );
        if let Err(e) = self.threads.save() {
            ctx.error("bridge", &format!("threads.json save failed: {e}"));
        }
    }

    fn post(&self, channel: &str, thread_ts: &str, text: String, key: &ThreadKey) {
        let (api, channel, thread_ts, key) = (
            self.slack_api.clone(), // TODO(Task 12): post_now
            channel.to_string(),
            thread_ts.to_string(),
            key.clone(),
        );
        tokio::spawn(async move {
            api.post_now(&channel, &thread_ts, text, &key).await;
        });
    }

    fn write_mcp(&self, sid: &str, ctx: &LogCtx) -> Option<String> {
        match mcp::Mcp::write_config(&self.deps.dir, self.mcp_port, sid, &self.mcp_token) {
            Ok(p) => Some(p.to_string_lossy().to_string()),
            Err(e) => {
                ctx.error("bridge", &format!("mcp config write failed: {e}"));
                None
            }
        }
    }

    /// 前の Bridge が残した在庫を拾い直す(起動時に1回)。restart は在庫を畳まずに降りるので、
    /// tmux には暖まった実体がそのまま残っている。それを在庫として復活させるのがここ。
    ///
    /// スレッドワーカーの「継承」と同じ論法で、**生きている = MCP initialize は前プロセス時代に
    /// 済んでいる**とみなして印を立て直す。立てないと `claim_pool_worker` の条件
    /// (`掛け金に入っていない && mcp_ready && !ended`)を永久に満たさず、在庫が居るのに
    /// 誰にも引き当てられないまま残る(掛け金のほうは、継承ワーカーは元から入っていない)。
    ///
    /// 死んでいた指名には触らない — 直後の [`Self::start_missing_pool_workers`] が同じ
    /// session_id を `--resume` で起こす。
    async fn restore_pools(&mut self, ctx: &LogCtx) {
        let targets: Vec<String> = self.access.pool_targets(&Host::home());
        let mut dirty = false;
        for (cwd, sid) in self.pools.rows() {
            let name = SessionId::from(sid.clone()).window_name();
            // 継承ワーカーと同じく window_id は覚えていない — 窓名で引く(spawn 直後に
            // automatic-rename を切ってあるので名前は残る)
            let alive = self.deps.agent.pid_of(None, &name).is_some();
            let key = bridge::PoolKey::of_cwd(&cwd);
            let ctx = LogCtx {
                session_id: Some(sid.clone()),
                thread_key: None,
            };
            match bridge::PoolRestore::decide(targets.contains(&cwd), alive) {
                // プール対象から外れた cwd(設定変更)。残すと誰も引き当てない在庫が居座る
                bridge::PoolRestore::Discard => {
                    if alive {
                        let stale = worker::PoolWorker {
                            session_id: sid.clone(),
                            spawned_at_ms: self.deps.clock.now_ms(),
                            cwd: cwd.clone(),
                            resumed: false,
                        };
                        self.teardown_pool(&key, &stale, "pool no longer configured")
                            .await;
                    }
                    self.pools.release(&cwd);
                    dirty = true;
                    continue;
                }
                // 指名はそのまま — 直後の start_missing_pool_workers が `--resume` で起こす
                bridge::PoolRestore::Respawn => continue,
                bridge::PoolRestore::Adopt => {}
            }
            ctx.info(
                "bridge",
                &format!(
                    "pool: inherited a live worker from a previous bridge process (key={key})"
                ),
            );
            // 前世代が起こしきったワーカー = 掛け金には入れない(そのまま引き当ててよい)。
            // MCP だけは印を戻す — initialize は前プロセス時代に済んでいて二度と来ない
            self.workers.warm_mut(&sid).mcp_ready = true;
            self.workers.insert_pool(
                key,
                worker::PoolWorker {
                    session_id: sid,
                    // 猶予はこのプロセスから数え直す(前世代の時計で見捨てない)
                    spawned_at_ms: self.deps.clock.now_ms(),
                    cwd,
                    resumed: false,
                },
            );
        }
        if dirty {
            self.save_pools(ctx);
        }
    }

    /// 前の Bridge が答えを待っていたスレッドを拾い直す(起動時に1回)。
    ///
    /// 台帳は pending.json に落ちているが、**そのまま全部載せない**。ワーカーが死んでいる
    /// スレッドの未応答は誰も応えないので、載せると沈黙の見張りが永久に居座る。在庫の
    /// [`Self::restore_pools`] と同じ論法で、tmux に実体が残っている分だけを残す。
    ///
    /// **生き残りを数え上げに行かない** — 掛け金は元から空だし(= 継承ワーカーはそのまま
    /// 渡してよい)、MCP の印は最初のツール呼び出しが事実として立てる(`"mcp_ready"` hook)。
    /// 起動時に threads.json を全件 tmux に問い合わせても、その2つは何も早くならない。
    ///
    /// 載せ直したら見張りも張り直す(`touch_thread` の空文字 = 何も出さずに時計だけ始める)。
    /// これが無いと台帳だけ復活して、無音になっても「考え中」が出ない。
    fn restore_pending(&mut self, ctx: &LogCtx) {
        let all = self.ledger.pending_keys();
        let alive = self.threads.surviving(&all, |sid| {
            let name = SessionId::from(sid.to_string()).window_name();
            self.deps.agent.pid_of(None, &name).is_some()
        });
        self.ledger.retain_keys(&alive);
        if all.is_empty() {
            return;
        }
        ctx.info(
            "bridge",
            &format!(
                "restored {}/{} thread(s) with undisposed messages from the previous bridge process",
                alive.len(),
                all.len()
            ),
        );
        for key in alive {
            self.touch_thread(&key, "");
        }
    }

    /// 在庫が欠けているプールを起動する。owner が居ないうちは `pool_targets` が空なので何もしない。
    /// **`milestone()` は呼ばない** — プール worker はまだどのスレッドのものでもない。
    fn start_missing_pool_workers(&mut self, ctx: &LogCtx) {
        let targets = self.access.pool_targets(&Host::home());
        // **下方向にも**収束させる。プール対象から外れた枠の在庫は、誰も欲しがらない
        // 席を占めたまま遊んでいる。これが無いと `warm off` はその在庫が死ぬまで何も解放しない。
        // 引き当て済みのワーカーは在庫の名簿から抜けているので、ここが人の仕事を止めることはない
        let wanted: Vec<bridge::PoolKey> = targets
            .iter()
            .map(|cwd| bridge::PoolKey::of_cwd(cwd))
            .collect();
        let stale: Vec<(bridge::PoolKey, String)> = self
            .workers
            .pool_rows()
            .into_iter()
            .filter(|(key, ..)| !wanted.contains(key))
            .map(|(key, sid, ..)| (key, sid))
            .collect();
        for (key, sid) in stale {
            ctx.info(
                "bridge",
                &format!("pool {key} is no longer a warm-pool target — discarding its worker"),
            );
            self.drop_pool_worker(&sid, "de-targeted", ctx);
        }
        // 上限中は**起こす方だけ**やめる(解放は常に安全なので上で済ませてある)
        if self.deps.clock.now_ms() < self.limited_until_ms {
            ctx.info(
                "bridge",
                &format!(
                    "usage limit active — not starting missing warm-pool workers \
                     (limitedUntil={})",
                    self.limited_until_ms
                ),
            );
            return;
        }
        for cwd in targets {
            let key = bridge::PoolKey::of_cwd(&cwd);
            // 諦めた枠は在庫から消えている — 「居ない」だけを見ると再 spawn してしまうので、
            // 一方通行の札(`gave_up_pool_keys`)も一緒に見る
            let status = Some(bridge::PoolStatus {
                present: self.workers.has_pool(&key),
                gave_up: self.workers.gave_up_on(&key),
            });
            if !bridge::PoolStatus::needs_launch(status) {
                continue;
            }
            // 指名済みのセッションがあれば**それを resume** — 新規 ID を切ると、在庫を作り直す
            // たびに使い捨てのセッションが claude の履歴に積まれる
            let nominated = self.pools.session_of(&cwd).map(str::to_string);
            let sid = nominated
                .clone()
                .unwrap_or_else(|| SessionId::new().as_str().to_string());
            let how = if nominated.is_some() {
                "resuming"
            } else {
                "launching"
            };
            ctx.info("bridge", &format!("pool: {how} {} (key={key})", cwd));
            // 以後は「どの worker の話か」だけが要る。スレッドはまだ無いので thread_key は None
            let ctx = LogCtx {
                session_id: Some(sid.clone()),
                thread_key: None,
            };
            let Some(mcp) = self.write_mcp(&sid, &ctx) else {
                continue;
            };
            // 窓名はプールも割当済みも同じ規則— 引き当てで改名しない
            let window_id = match self.deps.agent.spawn(&SpawnReq {
                session_id: sid.clone().into(),
                cwd: cwd.clone(),
                // 配達待ちの本文を持たない = 実体の待受プロンプトで起動する
                prompt: None,
                resume_from: nominated.clone().map(SessionId::from),
                window: SessionId::from(sid.clone()).window_name(),
                // ここに来た時点で在庫は居ない = 窓も無い
                state: inbound::WorkerState::Absent,
                hooks_file: self.hooks_file.clone(),
                mcp_config: mcp,
            }) {
                Ok(window) => {
                    self.start_spawn_screen_watch(&window, format!("pool session={sid}"), &ctx);
                    Some(window.as_str().to_string())
                }
                Err(e) => {
                    ctx.error("bridge", &format!("pool spawn failed: {e}"));
                    continue;
                }
            };
            // 新規で切った ID はここで指名する(次の起動が resume で拾えるように)
            if nominated.is_none() {
                self.pools.nominate(&cwd, &sid);
                self.save_pools(&ctx);
            }
            // 窓はスレッドワーカーと同じ場所に置く — 引き当てで持ち替えが要らなくなる
            self.workers.warm_mut(&sid).window_id = window_id;
            // 在庫も待受プロンプトを1回踏むまでは起動中(踏んだ時点で引き当ての対象になる)
            self.workers.mark_starting(&sid);
            self.workers.insert_pool(
                key,
                worker::PoolWorker {
                    session_id: sid,
                    spawned_at_ms: self.deps.clock.now_ms(),
                    cwd,
                    resumed: nominated.is_some(),
                },
            );
        }
    }

    /// 指名表(pools.json)を書き出す。失敗しても走り続ける — 失うのは「次の起動で resume できる」
    /// だけで、その場合は新規 ID で在庫が立つ(現行と同じ挙動に落ちる)。
    fn save_pools(&self, ctx: &LogCtx) {
        if let Err(e) = self.pools.save() {
            ctx.error("bridge", &format!("pools.json save failed: {e}"));
        }
    }

    /// 在庫1本を実体ごと畳む。窓名は `w-<session_id>`、pid 指名 kill → 窓を閉じる、の順序は
    /// `terminate` と揃える。`why` はログの頭に出る文脈語(logout / restart / pool give-up)。
    async fn teardown_pool(&mut self, key: &PoolKey, p: &worker::PoolWorker, why: &str) {
        let sid = &p.session_id;
        let ctx = LogCtx {
            session_id: Some(sid.clone()),
            thread_key: None,
        };
        let name = SessionId::from(sid.to_string()).window_name();
        let window_id = self.workers.window_of(sid);
        match self.deps.agent.pid_of(window_id.as_deref(), &name) {
            Some(pid) => {
                ctx.info(
                    "bridge",
                    &format!(
                        "{why}: tearing down pool worker session={sid} by pid [{pid}] (key={key})"
                    ),
                );
                pid.kill_graceful(tmux_mod::KILL_GRACE_MS).await;
            }
            None => ctx.info(
                "bridge",
                &format!(
                    "{why}: no live process for pool worker session={sid} — \
                     closing window only (key={key})"
                ),
            ),
        }
        let target = Window::of(window_id.as_deref().unwrap_or(&name));
        if let Err(e) = self.deps.agent.terminate(&target) {
            ctx.error("bridge", &format!("{why}: pool kill-window failed: {e}"));
        }
        self.workers.forget(sid);
    }

    /// SIGTERM / SIGINT — **本当に止める**。後継は来ない。
    /// restart(SIGUSR1 / Slack の `restart`)と違い、ワーカーは1本も残さない:
    /// Bridge の居ない claude + tmux 窓は誰にも掃除されない孤児になる。
    ///
    /// なお slack-morphism は Socket Mode を張ると自前で TERM_SIGNALS を握る
    /// (tokio_clients_manager.rs:151-161 — debug ログを出すだけでプロセスを終わらせない)。
    /// signal-hook のレジストリは1シグナルに複数の受け手を許すので、この腕とは共存する。
    async fn shutdown(&mut self, reason: &str) -> ! {
        let ctx = LogCtx::default();
        ctx.info(
            "bridge",
            &format!("shutting down ({reason}) pid={}", std::process::id()),
        );
        eprintln!("slack bridge: shutting down ({reason})");
        // 下のどれかが詰まっても必ず降りる(5秒)。
        // teardown 側は tmux の SIGTERM 猶予を待つので、これが唯一の上限
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            std::process::exit(0);
        });
        // offline を**先に**出す。teardown は数秒かかるので、後回しにすると
        // 上のハード exit に食われて通知が落ちる(同じ事故を書いている)
        let offline = crate::bridge::render::Notice::Offline {
            label: Host::name().await,
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            reason: reason.to_string(),
        }
        .render();
        self.post_notice(&offline, &ctx).await;
        // 畳む前に逃がす — ワーカーごと落とすので、queue に残った依頼はここでしか救えない
        self.flush_pending_to_disk(&ctx);
        self.save_ledger();
        self.teardown_all_workers("shutdown", "", &ctx).await;
        ctx.info("bridge", &format!("shutdown complete ({reason})"));
        std::process::exit(0);
    }

    /// SIGHUP。access.json / threads.json は**メモリ側が正**なので、
    /// 手で編集したものを取り込む口はここだけ。
    fn reload_from_disk(&mut self) {
        let ctx = LogCtx::default();
        let access = bridge::Access::load(&self.deps.dir);
        self.adopt_access(access, &ctx);
        self.threads = bridge::Threads::load(&self.deps.dir);
        ctx.info(
            "bridge",
            "access.json + threads.json reloaded from disk (SIGHUP)",
        );
    }

    /// 生きているワーカーを**全部**畳む。ドレインはしない — 走らせるターンには行き先が無い
    /// (teardownAllWorkers)。`logout`(認証が外れた)と
    /// `shutdown`(孤児を残さない)の両方から呼ぶ。`detail` は行末に足す補足で、無いときは空文字。
    async fn teardown_all_workers(&mut self, why: &str, detail: &str, ctx: &LogCtx) {
        let live: Vec<(ThreadKey, String, Pid)> = self
            .threads
            .entries
            .iter()
            .filter_map(|(tts, e)| {
                let (ch, sid) = (e.channel_id.as_deref()?, e.agent_id.as_deref()?);
                let window_id = self.workers.window_of(sid);
                let window = SessionId::from(sid.to_string()).window_name();
                let pid = self.deps.agent.pid_of(window_id.as_deref(), &window)?;
                Some((ThreadKey::new(ch, tts), sid.to_string(), pid))
            })
            .collect();
        // 在庫の pid も一緒に集める(下の teardown_pools が畳む相手)
        let pool_pids: Vec<Pid> = self
            .workers
            .pool_pid_rows()
            .into_iter()
            .filter_map(|(sid, window_id)| {
                let name = SessionId::from(sid).window_name();
                self.deps.agent.pid_of(window_id.as_deref(), &name)
            })
            .collect();
        ctx.info(
            "bridge",
            &format!("{why}: tearing down {} live worker(s){detail}", live.len()),
        );
        // **先に全員へ SIGTERM を撃つ。** 下の teardown は1本ずつ直列に待つので、先撃ちしないと
        // 猶予(1本あたり最悪 TERM 1.5s + KILL 1.5s)が N 本ぶん積み上がり、shutdown の5秒
        // バックストップに切られて生き残りが出る(実機で実際に4本残した)。先に撃って
        // おけば猶予は**重なって**消化される。Bun は teardown 自体を並列化して同じ問題を解いた
        // ponytail: SIGKILL 側の待ちは直列のまま。claude は SIGTERM で降りるので実測上これで足りる。
        //           足りなくなったら terminate ごと JoinSet で並列化する
        for (_, _, pid) in &live {
            pid.term();
        }
        for pid in &pool_pids {
            pid.term();
        }
        // run_drains の「1 tick に1本」と非対称に、ここは直列で全部殺す — 待たせる
        // 相手は今まさに殺しているワーカー自身なので、Stop 契約の5秒枠を割っても
        // 困る者が居ない(そのワーカーはもう答えない)
        for (key, sid, _) in &live {
            self.terminate(key, Some(sid.as_str()), None).await;
        }
        // 在庫は `terminate` に載らない(まだどのスレッドのものでもない)
        self.teardown_pools(why).await;
    }

    /// 在庫を空にして全部畳む。`logout`(認証が外れた)と `shutdown`(孤児を残さない)から。
    /// **restart からは呼ばない** — あちらは在庫を生かしたまま降りて後継に継承させる。
    ///
    /// 畳むのは実体だけで、pools.json の**指名は残す**。次の起動が同じ session_id を
    /// `--resume` で起こすので、停止をまたいでもセッションは増えない。
    async fn teardown_pools(&mut self, why: &str) {
        for (key, p) in self.workers.take_pools() {
            self.teardown_pool(&key, &p, why).await;
        }
    }

    /// MCP を上げられないまま猶予を過ぎた在庫を諦める。諦め方は Bun 版と同型
    /// 鍵を `gave_up_pool_keys` に記録し、**実体を畳んで在庫から消す**。
    /// 記録が再 spawn の止め金、削除が「存在しない在庫を status が数える」の防ぎ
    async fn give_up_stale_pools(&mut self, ctx: &LogCtx) {
        let now = self.deps.clock.now_ms();
        let stale: Vec<PoolKey> = self
            .workers
            .pool_rows()
            .into_iter()
            .filter(|(_, _, spawned_at_ms, ready)| {
                !ready
                    && bridge::PoolStatus::should_give_up(
                        *spawned_at_ms,
                        now,
                        POOL_MCP_INIT_TIMEOUT_MS,
                    )
            })
            .map(|(k, _, _, _)| k)
            .collect();
        for key in stale {
            let Some(p) = self.workers.remove_pool(&key) else {
                continue;
            };
            let ctx = LogCtx {
                session_id: Some(p.session_id.clone()),
                thread_key: ctx.thread_key.clone(),
            };
            // resume で起こした在庫が暖まらないのは「セッションが消えた/壊れた」が第一候補。
            // ここで諦め札を立てると、指名が腐ったせいでそのプールが二度と作られなくなる。
            // 指名だけ捨てて、新規 ID の1回に賭け直す(それも暖まらなければ下の札が立つ)
            if p.resumed {
                self.pools.release(&p.cwd);
                self.save_pools(&ctx);
                ctx.info(
                    "bridge",
                    &format!(
                        "pool: resumed session did not warm up within \
                         {POOL_MCP_INIT_TIMEOUT_MS}ms — dropping the nomination for {key} \
                         and retrying once with a fresh session"
                    ),
                );
                self.teardown_pool(&key, &p, "pool resume failed").await;
                continue;
            }
            self.workers.mark_gave_up(key.clone());
            ctx.info(
                "bridge",
                &format!(
                    "pool: give up on {key} — no mcp_initialized within \
                     {POOL_MCP_INIT_TIMEOUT_MS}ms"
                ),
            );
            self.teardown_pool(&key, &p, "pool give-up").await;
        }
    }

    /// 定期的に在庫を数え直す。死んだ在庫は `session_end` と引き当て時の生存確認が登録簿から
    /// 落とすので、ここは「空いた枠を埋める」だけ(Bun の `missing(targetKeys)` 収束と同じ役)。
    /// **間引きは必須** — tick は 500ms 刻みで、素通しすると毎回 tmux に問い合わせに行く。
    fn sweep_pools(&mut self, ctx: &LogCtx) {
        let now = self.deps.clock.now_ms();
        if !self.workers.sweep_due(now) {
            return;
        }
        self.start_missing_pool_workers(ctx);
    }

    /// 冷えたワーカーを畳む(現行`cleanup.run` に相当)。60秒に1回。
    ///
    /// 見るのは**スレッドに紐づいたワーカーだけ**。在庫は `threads.json` に載っていないので
    /// ここには最初から出てこない(畳むのは `sweep_pools` 側の仕事)。
    ///
    /// **threads.json は触らない** — セッションの紐付けが残るので、畳んだスレッドに次の
    /// メッセージが来れば `--resume` で続きから起き直る。人から見ると何も起きていない。
    ///
    /// ponytail: 1回につき1本しか畳まない。kill は最悪3秒 main ループを止めるので、
    /// `run_drains` と同じ理由で同一 tick に2本入れない。60秒で1本ずつ減る速さで足りなく
    /// なったら、kill を spawn に逃がす。
    async fn cleanup_workers(&mut self) {
        if !self.workers.cleanup_due(self.deps.clock.now_ms()) {
            return;
        }
        self.reap_stray_windows().await;
        self.evict_idle_worker().await;
    }

    /// 誰のものでもない窓を閉じる(現行EMPTY と 2389 の ORPHAN)。
    ///
    /// **tmux の窓一覧が権威**。Bridge の記憶は再起動で消えるが窓は残るので、窓名に焼かれた
    /// `w-<session_id>` から持ち主を引き直す。持ち主が居ない = 誰も配達できず、誰も畳まない。
    ///
    /// 触らないもの: ワーカーの窓でないもの(アンカー窓・人が開いた窓)と、**立ち上げ中**
    /// (`starting`)。spawn は窓を作った直後・同じ関数の中で掛け金を立てるので、
    /// 起こしたばかりのワーカーが「まだ誰のものでもない」に見える隙間は無い。
    /// 掃除してよい回か。**担当が1人も居ないのにワーカーの窓がある**なら、記憶を失って
    /// いる側を疑う(窓を疑わない)。窓が無ければ掃除しても何も起きないので通してよい。
    fn safe_to_reap(owned: &HashSet<String>, rows: &[tmux_mod::WindowRow]) -> bool {
        !owned.is_empty() || !rows.iter().any(|r| r.session_id().is_some())
    }

    async fn reap_stray_windows(&mut self) {
        // スレッドと在庫が握っている session_id = 持ち主の居る窓
        let owned: HashSet<String> = self
            .threads
            .entries
            .values()
            .filter_map(|e| e.agent_id.clone())
            .chain(self.workers.pool_pid_rows().into_iter().map(|(sid, _)| sid))
            // pools.json の**指名**も持ち主として数える。在庫はまだ立っていなくても、
            // 指名されたセッションの窓は「これから引き当てられる在庫」なので litter ではない
            .chain(self.pools.sessions().map(str::to_string))
            .collect();
        let rows = self.deps.agent.windows();
        // **持ち主が1人も居ないのにワーカーの窓がある = 窓がゴミなのではなく、こちらが
        // 担当を見失っている**(threads.json が読めなかった等)。そのまま下の判定に落ちると
        // 「全部の窓が持ち主不明」= 全部閉じる、になる。2026-08-03 に実機で動いている
        // ワーカーを3本閉じた道がこれ。**この回は何もしない** — 本物のゴミ窓なら、担当表が
        // 正常な回(60秒ごと)に普通に閉じられる。
        if !Self::safe_to_reap(&owned, &rows) {
            LogCtx::default().error(
                "bridge",
                "cleanup: skipped — worker windows exist but no thread or pool owns any of them \
                 (the registry is probably lost; refusing to close live windows)",
            );
            return;
        }
        for row in rows {
            let Some(sid) = row.session_id().map(str::to_string) else {
                continue; // ワーカーの窓ではない
            };
            if self.workers.is_starting(&sid) {
                continue;
            }
            let why = if row.is_empty_shell() {
                "claude exited and left an empty window"
            } else if !owned.contains(&sid) {
                "belongs to no thread and no warm pool"
            } else {
                continue;
            };
            let ctx = LogCtx {
                session_id: Some(sid.clone()),
                thread_key: None,
            };
            ctx.info(
                "bridge",
                &format!(
                    "cleanup: stray window {} ({why}) — closing (command={})",
                    row.id, row.command
                ),
            );
            // 殻でないなら claude が生きている可能性がある。窓だけ閉じると孤児として
            // 残るので、**pid 指名で落としてから**閉じる(terminate と同じ順序)
            row.pid.kill_graceful(tmux_mod::KILL_GRACE_MS).await;
            if let Err(e) = self.deps.agent.terminate(&Window::of(&row.id)) {
                ctx.debug("bridge", &format!("cleanup: kill-window failed: {e}"));
            }
            self.workers.forget(&sid);
            return; // 1回につき1つ(kill の待ちで main ループを止めすぎない)
        }
    }

    /// 冷えたスレッドのワーカーを1本畳む。
    async fn evict_idle_worker(&mut self) {
        let now = self.deps.clock.now_ms();
        let mut snaps: Vec<worker::IdleSnapshot> = Vec::new();
        let mut sid_of: HashMap<ThreadKey, String> = HashMap::new();
        // 人のクリックを待っているスレッド(perm_pending は reqId 引きなので鍵に直す)
        let waiting: HashSet<ThreadKey> = self
            .perm_pending
            .values()
            .map(|p| ThreadKey::new(&p.channel, &p.thread_ts))
            .collect();
        for (tts, e) in &self.threads.entries {
            let (Some(channel_id), Some(sid)) = (e.channel_id.as_deref(), e.agent_id.as_deref())
            else {
                continue;
            };
            // 生死は tmux の pid が唯一の答え(記憶ではなく実物 — status と同じ流儀)
            let h = self.workers.warm(sid);
            let window = SessionId::from(sid.to_string()).window_name();
            if self
                .deps.agent
                .pid_of(h.and_then(|h| h.window_id.as_deref()), &window)
                .is_none()
            {
                continue;
            }
            let key = ThreadKey::new(channel_id, tts);
            // idle の基準は transcript の mtime = 最後に**本当に働いた**時刻。ナレーションや
            // reply の時刻より広く、黙って考えている / 長いツールを回している最中も更新される
            let last = self
                .deps.agent
                .last_activity_ms(h.and_then(|h| h.transcript_path.as_deref()), sid);
            let eligible = last.is_some()
                && !self.workers.is_starting(sid)
                && !self.workers.is_draining(&key)
                && self.ledger.pending(&key).is_empty()
                && !waiting.contains(&key);
            snaps.push(worker::IdleSnapshot {
                key: key.clone(),
                idle_ms: last.map_or(0, |t| now.saturating_sub(t)),
                eligible,
            });
            sid_of.insert(key, sid.to_string());
        }
        let live = snaps.len();
        let mut evict = worker::select_cleanup_keys(snaps, &CLEANUP);
        if evict.is_empty() {
            return;
        }
        // 一番冷えたものから1本(select は冷えている順に積む)
        let key = evict.remove(0);
        let sid = sid_of.get(&key).cloned();
        // **畳んだ理由はそのスレッドのログに残す**(呼び手の ctx は素なので plugin-debug.log
        // 行き = 雑多なログに埋もれる)。後から「ワーカーが消えている」を追う人が最初に開くのは
        // by-thread の bridge.log で、直後の `exit: terminating …` もそこに出る
        let ctx = LogCtx {
            session_id: sid.clone(),
            thread_key: Some(key.clone()),
        };
        ctx.info(
            "bridge",
            &format!(
                "cleanup: idle worker key={key} session={} — terminating ({live} live, \
                 {} more queued for the next pass)",
                sid.as_deref().unwrap_or("none"),
                evict.len()
            ),
        );
        self.terminate(&key, sid.as_deref(), None).await;
    }

    /// この session_id の在庫を登録簿から**落とすだけ**(Bun の `PoolRegistry.remove`。
    /// の `clearForSession` から呼ばれるのと同じ役)。実体は既に死んでいる
    /// 前提なので窓は畳まない。`gave_up_pool_keys` には**入れない** — 死は「諦めた」ではなく、
    /// 次のスイープで立て直してよい枠だから。
    ///
    /// pools.json の**指名も外さない**。立て直しは同じ session_id の `--resume` でやりたい
    /// (新規 ID を切ると、在庫が死ぬたびにセッションが増える)。resume できないほど壊れて
    /// いたときの逃げ道は `give_up_stale_pools` 側にある。
    fn drop_pool_worker(&mut self, sid: &str, why: &str, ctx: &LogCtx) {
        let Some(key) = self.workers.drop_pool_of_session(sid) else {
            return;
        };
        ctx.info(
            "bridge",
            &format!("pool: worker session={sid} {why} — dropped from the pool (key={key})"),
        );
    }

    /// 在庫から1本抜き取る。抜けるのは Bun の `claimReadyWorker` と同じ **push-ready**
    /// (mcp_initialized ∧ 最初の user_prompt ∧ 未終了)で、かつ**実プロセスが生きている**もの
    /// だけ。生きていない在庫は引き当てず登録簿からも消す — 残すと補充もされず、次の新規
    /// スレッドがまた死体を引いて配達ごと失う(E2E で実測)。
    /// 抜いた時点でそれはもう在庫ではない — 補充は次の `start_missing_pool_workers` に任せる。
    fn claim_pool_worker(&mut self, pool_key: &PoolKey) -> Option<worker::PoolWorker> {
        let p = self.workers.pool(pool_key)?;
        let sid = p.session_id.clone();
        let push_ready = !self.workers.is_starting(&sid)
            && self
                .workers
                .warm(&sid)
                .is_some_and(|h| h.mcp_ready && !h.ended);
        if !push_ready {
            // まだ暖まっていないだけかもしれない — 触らず置いておく(give-up が期限を見る)
            return None;
        }
        // connector の無い Rust では tmux の claude pid が唯一の生存証明
        // (session_end の飛ばない kill -9 で死んだ在庫はここでしか気付けない)
        if self
            .deps.agent
            .pid_of(
                self.workers.window_of(&sid).as_deref(),
                &SessionId::from(sid.clone()).window_name(),
            )
            .is_none()
        {
            self.drop_pool_worker(
                &sid,
                "has no live process",
                &LogCtx {
                    session_id: Some(sid.clone()),
                    thread_key: None,
                },
            );
            return None;
        }
        self.workers.remove_pool(pool_key)
    }

    /// 引き当てた worker をスレッドに縛り、封筒を配達し、抜いた分を補充する
    /// 窓は再利用 — スレッドキーだけが後から付く。
    /// **`milestone()` は呼ばない** — プールが既に spawn / mcp_initialized を出している。
    /// この `pool: assigned` の1行だけが「割当」の記録。
    #[allow(clippy::too_many_arguments)]
    fn assign_pool_worker(
        &mut self,
        claimed: worker::PoolWorker,
        root_ts: &str,
        channel: &str,
        cwd: &str,
        envelope: &str,
        topic: &str,
        message_id: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) {
        let sid = claimed.session_id.clone();
        let ctx = LogCtx {
            session_id: Some(sid.clone()),
            thread_key: ctx.thread_key.clone(),
        };

        let mut e = bridge::ThreadEntry::for_pool_assignment(
            channel,
            cwd,
            (!topic.is_empty()).then(|| topic.to_string()),
        );
        e.agent_id = Some(sid.clone());
        self.threads.upsert(root_ts, e);
        if let Err(err) = self.threads.save() {
            ctx.error("bridge", &format!("threads.json save failed: {err}"));
        }
        // 在庫の卒業 — このセッションは以後このスレッドのもの。指名を外さないと、
        // 補充がスレッドの持ち物を `--resume` で二重に起こしにいく
        self.pools.release(&claimed.cwd);
        self.save_pools(&ctx);

        // 掛け金には入れない — 在庫は引き当ての条件として既に最初のターンを踏んでいる。
        // 窓は起動時から self.hooked にある(在庫は写しを持たない)
        ctx.info(
            "bridge",
            &format!("pool: assigned {key} to pool worker session={sid}"),
        );

        let target = self
            .workers
            .window_of(&sid)
            .unwrap_or_else(|| SessionId::from(sid.clone()).window_name());
        match self.deps.agent.send_text(&Window::of(&target), envelope) {
            // Dispatch::Deliver と同型の配達記録。
            // このパスは新規スレッドの割当てなので new は常に true
            Ok(()) => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: deliver message_id={message_id} chat={channel} \
                         thread={root_ts} new=true -> worker {channel}"
                    ),
                );
                // Dispatch::Deliver と同じ — 渡した瞬間に shimmer を出す
                self.touch_thread(key, slack::TYPING_STATUS);
            }
            Err(e) => ctx.error("bridge", &format!("delivery failed: {e}")),
        }

        // 補充で起動するのは別セッション — 割当てた session_id を引きずらせない
        self.start_missing_pool_workers(&LogCtx {
            session_id: None,
            thread_key: ctx.thread_key.clone(),
        });
    }

    /// 起動直後の窓を見張る背景タスクを起こす。**spawn した直後に必ず呼ぶ** —
    /// これが唯一の答え手で、立ち上がりの期限は他に無い(`Claude::watch_spawn_screens`)。
    fn start_spawn_screen_watch(&self, w: &Window, what: String, ctx: &LogCtx) {
        let (tx, w, ctx) = (self.cmd_tx.clone(), w.clone(), ctx.clone());
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let out = agent
                .watch_spawn_screens(&w, SPAWN_SCREEN_BUDGET_MS, SPAWN_SCREEN_POLL_MS, &ctx)
                .await;
            // 普通に立ち上がった窓は毎回ここに来る — main を起こす価値があるのは拒絶だけ
            if matches!(
                out,
                SpawnOutcome::LoginRequired | SpawnOutcome::UsageLimited
            ) {
                let _ = tx.send(CmdFx::SpawnScreen { outcome: out, what }).await;
            }
        });
    }

    fn spawn_worker(&mut self, req: &SpawnReq, key: &ThreadKey, ctx: &LogCtx, sid: &str) {
        match self.deps.agent.spawn(req) {
            Ok(window) => {
                self.start_spawn_screen_watch(&window, format!("thread={key}"), ctx);
                let id = window.as_str().to_string();
                // 窓名は改名されうるので、以後はこの window_id で追う
                self.workers.warm_mut(sid).window_id = Some(id.clone());
                // 起こした = 最初のターンまでは配達を待たせる(TUI がまだキーを取れない)
                self.workers.mark_starting(sid);
                ctx.debug("bridge", &format!("spawned window={id}"));
                self.milestone(Some(key), "spawn", ctx);
            }
            Err(e) => ctx.error("bridge", &format!("spawn failed: {e}")),
        }
    }

    /// スレッドが引けないうちは milestone を出さない(現行4103 と同じ)。
    fn milestone(&mut self, key: Option<&ThreadKey>, event: &str, ctx: &LogCtx) {
        let Some(key) = key else {
            ctx.debug("bridge", &format!("{event} before its thread is known"));
            return;
        };
        let m = self.lifecycle.record(key, event, self.deps.clock.now_ms());
        ctx.info("lifecycle", &m.message(key, event));
    }

    /// ack の付与は**待つ** — 投げっぱなしだと受領時の flip(👀 remove → 🤖 add)が
    /// 飛行中の add を追い越し、👀 が後から付き直して残る。失敗は握る(best-effort)。
    async fn react(&self, channel: &str, ts: &str, emoji: &str, key: &ThreadKey) {
        if let Err(e) = self.deps.slack.add_reaction(channel, ts, emoji).await {
            LogCtx {
                session_id: None,
                thread_key: Some(key.clone()),
            }
            .debug("bridge", &format!("ack reaction '{emoji}' failed: {e}"));
        }
    }

    async fn on_hook(&mut self, mut ev: HookEvent) {
        // hook はセッション ID しか運ばない — スレッドは threads.json から引き戻す
        let owning = self
            .threads
            .find_by_session(&ev.session_id)
            .map(|(ts, _)| ts.clone());
        let key = self.key_of_session(&ev.session_id);
        let ctx = LogCtx {
            session_id: Some(ev.session_id.clone()),
            thread_key: key.clone(),
        };
        // hook が来た = ワーカーは生きて動いている。沈黙見張りを張り直す(現行は turn 開始と
        // progress で `armWatchdog` — hook 全種はその上位集合で、どれも「活動の証拠」)
        if let Some(k) = key.clone() {
            self.touch_thread(&k, ""); // 活動 = 見張り解除。配達と違い新しく出すものは無い
        }
        // どの hook も transcript の在処を運んでくる — 最初に来たもので受信確認の tail を張る
        if let Some(path) = ev.payload["transcript_path"].as_str() {
            let h = self.workers.warm_mut(&ev.session_id);
            if h.transcript_path.as_deref() != Some(path) {
                h.transcript_path = Some(path.to_string());
                h.transcript_offset = 0; // 別ファイルになった(resume 等)なら読み直す
            }
        }
        match ev.kind.as_str() {
            "session_start" => {
                // **掛け金には触らない。** この合図は起動だけでなく compact / clear でも飛ぶので、
                // ここで「起動中」に戻すと動いているワーカー宛の配達が queue で詰まる
                // (2026-08-01 実機。詳しくは `Workers::starting`)
                self.workers.warm_mut(&ev.session_id).ended = false;
                self.milestone(key.as_ref(), "session_start", &ctx);
            }
            "session_end" => {
                self.workers.warm_mut(&ev.session_id).ended = true;
                // 在庫が死んだ = もう在庫ではない(clearForSession)。
                // 残すと ready:true のまま次の新規スレッドに引き当てられ、配達ごと失う
                self.drop_pool_worker(&ev.session_id, "ended", &ctx);
                self.milestone(key.as_ref(), "session_end", &ctx);
            }
            // ツールが呼べた = MCP を握っている(継承ワーカーは initialize を見せる機会が
            // 二度と無い — mcp.rs の call_tool が唯一の証拠を送ってくる)。milestone は出さない
            "mcp_ready" => self.workers.warm_mut(&ev.session_id).mcp_ready = true,
            // MCP を握った = disposition ツールが呼べる(Stop を強制してよい唯一の証拠)
            "mcp_initialized" => {
                // 在庫が「使える」になる唯一の瞬間でもある(`Workers::pool_ready` がこれを読む)
                self.workers.warm_mut(&ev.session_id).mcp_ready = true;
                self.milestone(key.as_ref(), "mcp_initialized", &ctx);
            }
            "user_prompt" => {
                // 最初のターンを踏んだ = TUI がキーを取れている。掛け金を外して queue を流す
                self.workers.clear_starting(&ev.session_id);
                self.milestone(key.as_ref(), "user_prompt", &ctx);
                self.on_turn_start(key.as_ref(), &ctx);
                self.flush_queued(&ev.session_id, owning, key.as_ref(), &ctx);
            }
            "perm" => self.on_perm(&mut ev, key.as_ref(), &ctx).await,
            "error" => self.on_turn_failure(&ev, key.as_ref(), &ctx),
            "progress" => self.on_progress(key.as_ref(), &ev.payload),
            "narration" => self.on_narration(key.as_ref(), &ev.payload, &ctx),
            "stop" => {
                let out = self.stop_decision(key.as_ref(), &ev, &ctx);
                // block したならターンは**続く**(再プロンプト)ので、まだ終わりではない
                if let Some(key) = key.as_ref().filter(|_| out.get("decision").is_none()) {
                    self.sticky.on_turn_end(key);
                }
                if let Some(respond) = ev.respond.take() {
                    let _ = respond.send(out);
                }
            }
            other => ctx.debug("bridge", &format!("unhandled hook {other}")),
        }
    }

    /// ターンが失敗した(`StopFailure`)。理由を分類し、スレッドで待っている人に**日本語で**伝える
    /// (`case 'error'`)。黙って失敗すると、人からは
    /// 「ボットが無視した」ようにしか見えない。
    ///
    /// **ログはスレッドが引ける前に、無条件で出す** — ターン失敗は決して黙る経路ではない。
    /// 生の payload も一緒に残す: `error_type` は実機で**空のまま**届いたことがあり、記録が
    /// 無いと「Claude が型を送らなかった」と「こちらが落とした」を区別できない。
    ///
    /// 手順は現行のまま崩さない: ログ → スレッド門番 → **上限ゲート** → **再配達** → 文面。
    fn on_turn_failure(&mut self, ev: &HookEvent, key: Option<&ThreadKey>, ctx: &LogCtx) {
        let reason = ev.payload["error_type"].as_str().unwrap_or("");
        let (klass, text) = crate::bridge::render::TurnFailureClass::of(reason);
        let raw = serde_json::json!({
            "hook_event_name": ev.payload["hook_event_name"],
            "error_type": ev.payload["error_type"],
            "session_id": ev.session_id,
        });
        ctx.info(
            "bridge",
            &format!(
                "slack-events: disposition=turn_failure thread={} reason={} class={klass} raw={raw}",
                key.map_or("?", ThreadKey::as_str),
                if reason.is_empty() {
                    "(none sent)"
                } else {
                    reason
                },
            ),
        );
        // スレッドが引けなければここまで。home チャンネルへの写しは出さない
        // 失敗は1つのスレッドのもので、そこで待っている人には下で伝わる
        let Some((channel, Some(thread_ts))) = key.map(ThreadKey::split) else {
            return;
        };
        let key = key.expect("thread split above implies a key");
        // 何よりも先に「これは上限か」。`error_type` は答えられない(実機で空のまま届いたし、
        // `rate_limit` は混雑にも使われる)ので、ワーカー自身の履歴に訊く。上限への再送は
        // **唯一絶対に効かない答え**で、しかも人が必要としている1文を沈黙に置き換える
        if !ev.session_id.is_empty() && self.turn_hit_usage_limit(&ev.session_id, key, ctx) {
            return;
        }
        // retry 級は「もう一度配達する」で晴れるもの。配達できたら何も投稿しない —
        // 人が見るべきは再送したターンの結末そのもの
        if klass == crate::bridge::render::TurnFailureClass::Retry
            && self.retry_turn_failure(key, reason, ctx)
        {
            return;
        }
        self.post_error_frame(channel, thread_ts, text);
    }

    /// このターンは「上限に当たった」で死んだのか(`turnHitUsageLimit`)。再送の**手前**で訊く —
    /// 再送だけは絶対に効かないから(壁はリセットまで動かない)。
    ///
    /// 現行がこれを入れた実測(2026-07-17): 上限に当たった1秒後に StopFailure が届いた。
    /// `error_type` は**空**だったので `retry` と読まれ、モーダルで固まったワーカーへ静かに
    /// 再送された。Claude Code はその答え(リセット時刻つき)をそのワーカーの履歴に**既に
    /// 書いていた**。誰も読まず、沈黙は90分続いた。
    ///
    /// true = ゲートを立ててスレッドにも伝えた(呼び手は再送してはいけない)。ゲートは文面と
    /// 同じくらい大事で、立っていれば**次の**メッセージは入口でリセット時刻を貰える。
    fn turn_hit_usage_limit(&mut self, session_id: &str, key: &ThreadKey, ctx: &LogCtx) -> bool {
        let remembered = self
            .workers
            .warm(session_id)
            .and_then(|h| h.transcript_path.clone());
        let hit =
            match self
                .deps.agent
                .session_limit_error(remembered.as_deref(), session_id, self.deps.clock.now_ms())
            {
                Some(Ok(hit)) => hit,
                // 履歴が読めない・見つからないのは「上限ではない」の証拠にならないが、これ以上
                // 訊く先が無い。現行と同じく通常のターン失敗の扱いに落とす
                Some(Err(e)) => {
                    ctx.info(
                        "bridge",
                        &format!("could not read the transcript tail of session={session_id}: {e}"),
                    );
                    return false;
                }
                None => return false,
            };
        let Some(hit) = hit else { return false };
        self.limited_until_ms = hit.reset_ms;
        ctx.info(
            "bridge",
            &format!(
                "turn failure for {key} is the USAGE LIMIT, by Claude Code's own record \
                 (\"{}\") — not re-sending (the wall does not move); hard-limit gate set \
                 until {}",
                hit.detail, hit.reset_ms
            ),
        );
        let (channel, thread_ts) = key.split();
        if let Some(ts) = thread_ts {
            let text = crate::bridge::render::Notice::Limited {
                until_ms: hit.reset_ms,
            }
            .render();
            self.post(&channel, &ts, text, key);
        }
        true
    }

    /// ターンが失敗し、答えられなかったメッセージがまだ未応答 — 「諦めました」と人に言う前に
    /// **同じワーカーへもう一度配達する**(`retryTurnFailure`)。
    ///
    /// なぜタイマーではなくここか: 失敗は**もう分かっている**(Claude Code がターンの死んだ
    /// 瞬間に報告する)。これまで未応答を配り直す唯一の道はワーカーの**死**だったので、
    /// 生きたワーカーの下で失敗したターンは、誰にも答えられないまま台帳に残り続けた。
    ///
    /// 再送するのはワーカーが**本当に聞こえる**とき(Ready)だけ。それ以外は受領タイマーと
    /// 回収経路が既にそのメッセージの持ち主で、押し込んでも2度目の黙殺になる。
    /// 戻り値 true = 配達した(呼び手は何も投稿しない)。
    fn retry_turn_failure(&mut self, key: &ThreadKey, reason: &str, ctx: &LogCtx) -> bool {
        let why = if reason.is_empty() {
            "no type sent"
        } else {
            reason
        };
        let undisposed = self.ledger.undisposed(key);
        if undisposed.is_empty() {
            ctx.info(
                "bridge",
                &format!(
                    "turn failure ({why}) for {key} — nothing undisposed to re-send; \
                     telling the user"
                ),
            );
            return false;
        }
        let (_, root) = key.split();
        let root_ts = root.unwrap_or_default();
        let entry = self.threads.get(&root_ts).cloned();
        let sid = entry.as_ref().and_then(|e| e.agent_id.clone());
        let window = sid
            .as_deref()
            .map(|s| SessionId::from(s.to_string()).window_name())
            .unwrap_or_default();
        let state = self.workers.state_of(entry.as_ref(), &window, self.deps.agent.as_ref());
        if state != inbound::WorkerState::Ready {
            ctx.info(
                "bridge",
                &format!(
                    "turn failure ({why}) for {key} — worker is {state:?}, not READY: leaving \
                     the message to the receipt/recovery paths and telling the user"
                ),
            );
            return false;
        }
        let ids: Vec<String> = undisposed.iter().map(|(id, _)| id.clone()).collect();
        if !self.ledger.spend_retry(key, &ids, TURN_FAILURE_RETRY_CAP) {
            ctx.info(
                "bridge",
                &format!(
                    "turn failure ({why}) for {key} message_id=[{}] — the re-send budget \
                     ({TURN_FAILURE_RETRY_CAP}) is spent; telling the user",
                    ids.join(",")
                ),
            );
            return false;
        }
        // 配達先は window_id を優先(窓名は改名されうる)— Dispatch::Deliver と同じ引き方
        let target = sid
            .as_deref()
            .and_then(|s| self.workers.warm(s))
            .and_then(|h| h.window_id.clone())
            .unwrap_or(window);
        for (id, envelope) in &undisposed {
            if let Err(e) = self.deps.agent.send_text(&Window::of(&target), envelope) {
                ctx.error(
                    "bridge",
                    &format!("turn-failure re-send failed for {id}: {e} — telling the user"),
                );
                return false;
            }
            // 同じ message_id → 台帳は1メッセージ1件なので冪等。received が倒れて受信確認が
            // 張り直る = これは**新しい配達**なので、それが正しい
            self.ledger.track(key, id);
        }
        ctx.info(
            "bridge",
            &format!(
                "turn failure ({why}) for {key} — the worker is READY, so re-sending ids=[{}] \
                 (attempt 1/{TURN_FAILURE_RETRY_CAP}) instead of telling the user the bot gave up",
                ids.join(",")
            ),
        );
        self.touch_thread(key, slack::TYPING_STATUS);
        true
    }

    /// `error` フレームの ⚠️ を1本投げる。perm プロンプトと同じ**消えたスレッド根の門番**を
    /// 通す(消えた根に `thread_ts` 付きで投げると Slack がチャンネル直下に落とす)。
    /// 呼び手は待たない — probe は数秒かかることがあり、main ループを吊ってはならない。
    fn post_error_frame(&self, channel: String, thread_ts: String, text: String) {
        let api = self.slack_api.clone(); // TODO(Task 12): post_now
        tokio::spawn(async move {
            let key = ThreadKey::new(&channel, &thread_ts);
            let ctx = LogCtx {
                session_id: None,
                thread_key: Some(key.clone()),
            };
            if !channel.starts_with('D') && api.thread_root_gone(&channel, &thread_ts).await {
                ctx.info(
                    "bridge",
                    &format!(
                        "error frame suppressed: thread-root {thread_ts} gone in chan {channel} \
                         (no channel spam)"
                    ),
                );
                return;
            }
            api.post_now(&channel, &thread_ts, format!("⚠️ {text}"), &key)
                .await;
        });
    }

    /// 活動があった — 沈黙タイマーを張り直し、ステータスを `status` に差し替える
    /// (現行の `armWatchdog` + `clearStall`)。冪等。
    ///
    /// `status` は「活動の結果いま出したいもの」: 配達は `is typing…`、hook は `""`(解除)。
    /// **差し替えは1回の送信にまとめる** — 「出す」と「消す」を別々に投げると順序が無いので
    /// 互いを打ち消す(既に `is thinking…` が出ているスレッドへの追撃配達で実際に起きる)。
    fn touch_thread(&mut self, key: &ThreadKey, status: &str) {
        // 未応答が1件も無いスレッドに見張りは要らない — 立てても撃たないうえ、次の tick が
        // 「決着済み」として畳むだけ(答えた後も流れてくる hook で毎回それをやるのは無駄)
        if !self.stall.contains_key(key) && self.ledger.pending(key).is_empty() {
            return;
        }
        let e = match self.stall.get_mut(key) {
            Some(e) => e,
            None => {
                let (channel, thread) = key.split();
                // API は thread_ts 必須 — 根の引けない鍵は見張らない
                let Some(ts) = thread else { return };
                // **黙って**作る(空文字 = 初回送信なし)。`status` を渡すとここで1回送り、
                // 下の共通処理でもう1回同じものを送ってしまう。送信口は下の1箇所に統一する
                self.stall.entry(key.clone()).or_insert(Stall {
                    last_activity_ms: 0,
                    shown: false,
                    awaiting_perm: false,
                    thinking: slack::Thinking::new(&self.slack_api, &channel, &ts, ""), // TODO(Task 12)
                })
            }
        };
        e.last_activity_ms = self.deps.clock.now_ms();
        // 送るのは**見た目が変わるときだけ**。出ていた見張りを解除する(shown)か、
        // 新しく何かを出す(status 非空)か。turn 中の hook 連打は既に何も出ていなければ無送信
        let was_shown = std::mem::replace(&mut e.shown, false);
        if was_shown || !status.is_empty() {
            e.thinking.set(status);
        }
    }

    /// 上限の見張り。**これが無いと上限ゲートは自然に発火しない**
    /// (ターン失敗の文面から気づく道しか無い)。
    ///
    /// 間隔は平常 60分・上限が見込まれるときは 15分。読むのは Home で `/usage` を1回回すだけで、
    /// スレッドもセッションも要らない(`user_usage` と同じ道)。
    ///
    /// やることは2つ:
    /// - **上限に達している窓があればゲートを立てる**。見るのは全部の窓で、解ける時刻は
    ///   いちばん遅いものを採る(週の壁はセッションが低くても効く)
    /// - 80% / 90% を新しく跨いだら、生きているスレッドに1回ずつ警告する
    async fn usage_tick(&mut self) {
        let now = self.deps.clock.now_ms();
        let due = self.usage_polled_at_ms
            + if self.usage_at_risk {
                crate::bridge::command::UsageWatch::POLL_AT_RISK_MS
            } else {
                crate::bridge::command::UsageWatch::POLL_MS
            };
        // 起動直後は少し待つ(立ち上がりに probe をぶつけない)
        if self.usage_polled_at_ms == 0 {
            self.usage_polled_at_ms = self.started_at_ms + USAGE_MONITOR_STARTUP_DELAY_MS;
            return;
        }
        if now < due || self.access.owner.is_empty() {
            return;
        }
        self.usage_polled_at_ms = now;
        let ctx = LogCtx::default();
        let raw = match self.deps.agent
            .probe(self.deps.agent.usage_argv(), Host::home())
            .await
        {
            Ok(raw) => raw,
            Err(ProbeErr::Failed(m) | ProbeErr::Errored(m)) => {
                ctx.error(
                    "bridge",
                    &format!("usage-monitor: /usage probe failed: {m}"),
                );
                return;
            }
        };
        let Some(rows) = self.deps.agent.usage_rows(&raw) else {
            ctx.info(
                "bridge",
                "usage-monitor: /usage had no readable row — holding state",
            );
            return;
        };
        let session = rows
            .iter()
            .find(|r| r.label.to_lowercase().contains("current session"));
        let pct = session
            .and_then(|r| r.pct.parse::<f64>().ok())
            .unwrap_or(0.0);
        let reset_text = session.map(|r| r.reset.clone()).unwrap_or_default();

        // 使用率が下がった = 窓が変わった。警告の掛け金を戻す
        if (pct as u32) < self.usage_warned_pct {
            ctx.info(
                "bridge",
                &format!(
                    "usage-monitor: usage dropped {}%→{}% (window reset) — re-arming warnings",
                    self.usage_warned_pct, pct as u32
                ),
            );
            self.usage_warned_pct = 0;
        }
        // 予測(この先どれくらいで上限に当たるか)。間隔もこれで決まる
        let w = crate::bridge::command::WallClock::now();
        let projection = crate::bridge::command::WallClock::parse_reset(&reset_text, &w)
            .map(|reset| crate::bridge::render::UsageProjection::of(pct, &reset, &w, 300));
        self.usage_at_risk = projection
            .as_ref()
            .is_some_and(|p| p.enough_data && p.at_risk);
        ctx.info(
            "bridge",
            &format!(
                "usage-monitor: session {}% used, reset={}, atRisk={}",
                pct as u32,
                if reset_text.is_empty() {
                    "?"
                } else {
                    &reset_text
                },
                self.usage_at_risk
            ),
        );

        // 上限のゲート — 達している窓があれば、いちばん遅いリセットまで塞ぐ
        if let Some(reset) =
            crate::bridge::command::UsageWatch::binding_limit_reset(&rows, now, 100.0)
            && reset != self.limited_until_ms
        {
            ctx.info(
                "bridge",
                &format!(
                    "usage-monitor: hard limit — gating spawn/delivery/pre-warm until epoch={reset}"
                ),
            );
            self.limited_until_ms = reset;
        }

        let crossed =
            crate::bridge::command::UsageWatch::newly_crossed(pct as u32, self.usage_warned_pct);
        let Some(highest) = crossed.iter().max().copied() else {
            return;
        };
        let text = crate::bridge::render::Notice::UsageWarning {
            pct: pct as u32,
            reset: reset_text,
            projected_hit: projection
                .filter(|p| p.enough_data && p.at_risk)
                .and_then(|p| p.projected_hit),
        }
        .render();
        // 生きているワーカーを抱えたスレッドにだけ1回ずつ
        let targets: Vec<ThreadKey> = self.live_thread_keys();
        ctx.info(
            "bridge",
            &format!(
                "usage-monitor: crossed {}% — warning {} live thread(s)",
                crossed
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join("/"),
                targets.len()
            ),
        );
        for key in targets {
            let (channel, thread) = key.split();
            if let Some(t) = thread {
                self.post(&channel, &t, text.clone(), &key);
            }
        }
        self.usage_warned_pct = highest;
    }

    /// 生きたワーカーを抱えているスレッドの鍵。上限警告の宛先(`liveThreads`)。
    fn live_thread_keys(&self) -> Vec<ThreadKey> {
        self.threads
            .entries
            .iter()
            .filter_map(|(root_ts, e)| {
                let sid = e.agent_id.clone()?;
                let channel = e.channel_id.clone()?;
                let name = SessionId::from(sid).window_name();
                self.deps.agent
                    .pid_of(None, &name)
                    .map(|_| ThreadKey::new(&channel, root_ts))
            })
            .collect()
    }

    /// 沈黙見張りの一撃(500ms tick から。現行はスレッドごとの setTimeout)。
    /// 判定は [`bridge::stall_action`] — 立てる / 畳む / 何もしない の3値。
    ///
    /// 畳むのは未応答が空になった鍵。`on_disposition` が即座に落とすのが本筋だが、台帳は
    /// `terminate`(exit / logout / resume)でも空になる — **台帳を空にした全経路**をここ1箇所で
    /// 受けるので、呼び手ごとに後始末を書き足さなくても shimmer が居座らない。
    ///
    /// ponytail: 一度立てたら張り直さない(現行 `showStall` の冪等と同じ)。Slack が失効させれば
    /// shimmer は静かに消えるだけで、消し忘れの居座りより害が小さい。張り直すなら compact と
    /// 同じく tick ごとに `set` を撃つ形にする
    fn stall_tick(&mut self) {
        self.save_ledger();
        let now = self.deps.clock.now_ms();
        let acts: Vec<(ThreadKey, bridge::StallAction)> = self
            .stall
            .iter()
            .map(|(key, s)| {
                // 許可待ちは沈黙ではない — 撃たない。ただし決着(Settle)は通す
                let act = bridge::StallAction::of(
                    !self.ledger.pending(key).is_empty(),
                    s.last_activity_ms,
                    now,
                    s.shown || s.awaiting_perm,
                    slack::SILENCE_MS,
                );
                (key.clone(), act)
            })
            .filter(|(_, act)| *act != bridge::StallAction::Nothing)
            .collect();
        for (key, act) in acts {
            match act {
                // 落とすだけでよい — slack::Thinking の Drop がキューの最後尾からクリアを流す
                bridge::StallAction::Settle => drop(self.stall.remove(&key)),
                bridge::StallAction::Fire => {
                    let Some(e) = self.stall.get_mut(&key) else {
                        continue;
                    };
                    e.shown = true;
                    e.thinking.set(slack::THINKING_STATUS);
                    LogCtx {
                        session_id: None,
                        thread_key: Some(key.clone()),
                    }
                    .info(
                        "bridge",
                        "slack-events: stall watchdog fired — setting the thinking status",
                    );
                }
                bridge::StallAction::Nothing => {}
            }
        }
    }

    /// 台帳が変わっていれば pending.json に落とす。tick と、降りる直前から呼ぶ1箇所きり —
    /// 台帳を触る側に save を撒かないので、経路が増えても書き忘れが起きない。
    fn save_ledger(&mut self) {
        if let Some(Err(e)) = self.ledger.flush(&mut self.threads) {
            LogCtx::default().error("bridge", &format!("pending.json save failed: {e}"));
        }
    }

    /// ワーカーが入力を取り込んだ瞬間。ここが**受領の証拠** — 配達済みを 👀 から 🤖 に替え、
    /// 前ラウンドの付箋を畳んで新しい付箋を始める。
    fn on_turn_start(&mut self, key: Option<&ThreadKey>, ctx: &LogCtx) {
        let Some(key) = key else { return };
        let ids = self.ledger.mark_received(key);
        self.received(key, ids, ctx);
        self.sticky.on_turn_start(key);
    }

    /// 受領した id を milestone に出し、👀 を 🤖 に替える(投げっぱなし)。
    fn received(&mut self, key: &ThreadKey, ids: Vec<String>, ctx: &LogCtx) {
        if ids.is_empty() {
            return;
        }
        self.milestone(Some(key), "received", ctx);
        let (channel, _) = key.split();
        let ack = slack::Api::ack_emoji(&self.access).to_string();
        for id in ids {
            let (api, channel, ack) = (self.deps.slack.clone(), channel.clone(), ack.clone());
            tokio::spawn(async move {
                api.flip_to_received(&channel, &id, &ack).await;
            });
        }
    }

    fn key_of_session(&self, session_id: &str) -> Option<ThreadKey> {
        self.threads
            .find_by_session(session_id)
            .and_then(|(ts, e)| e.channel_id.as_ref().map(|ch| ThreadKey::new(ch, ts)))
    }

    /// 受信確認のバックストップ。**ターン中に押し込んだ分は UserPromptSubmit が発火しない**
    /// (ステアリングとして消費される)ので、ワーカーの transcript に封筒の message_id が
    /// 現れたことを受領の証拠にする。移植元 (transcript-watch)。
    ///
    /// ponytail: 500ms ポーリングの素朴な tail。fs 通知や JSON 行パースが要るなら
    /// Bun connector の transcript-watch(実装)を移植
    fn scan_transcripts(&mut self) {
        let tails = self.workers.transcripts();
        for (sid, path, offset) in tails {
            let Some(key) = self.key_of_session(&sid) else {
                continue;
            };
            let unreceived = self.ledger.unreceived(&key);
            if unreceived.is_empty() {
                // 探すものが無い間の出力は読まずに飛ばす(封筒が書かれるのは配達より後 =
                // track より後なので取りこぼさない)。据え置くと最初のターン中配達で
                // ターン1回分を一括同期読みして main ループが止まる
                if let Ok(m) = std::fs::metadata(&path) {
                    self.workers.warm_mut(&sid).transcript_offset = m.len();
                }
                continue;
            }
            let ctx = LogCtx {
                session_id: Some(sid.clone()),
                thread_key: Some(key.clone()),
            };
            let Some((text, next)) = self.deps.agent.new_history_lines(path, offset, &ctx) else {
                continue;
            };
            self.workers.warm_mut(&sid).transcript_offset = next;
            let found = bridge::Ledger::find_received_ids(&text, &unreceived);
            let ids = self.ledger.mark_received_ids(&key, &found);
            let taken_in = !ids.is_empty();
            self.received(&key, ids, &ctx);
            // 受領は新しいラウンドの始まり。UserPromptSubmit が来る道と同じ扱いにする —
            // ここで畳まないと、フォローアップより**上**にある古い付箋に、その後の
            // ナレーションとツール行が足され続ける(見た目には「返事が過去へ遡って伸びる」)
            if taken_in {
                self.sticky.on_turn_start(&key);
            }
        }
    }

    /// queue に残っている分を tick ごとに押し直す。**Rust 版には受領タイムアウトの再配達が
    /// 無い**ので、これが無いと詰まりから自力で戻れない — 既存の再送経路は2つとも hook 起点で、
    /// どちらもワーカーが止まっている間は永久に飛ばない:
    /// [`Bridge::retry_turn_failure`] は StopFailure(ターンが始まらないので出ない)、
    /// [`Bridge::flush_queued`] は user_prompt(何も送信できていないので出ない)。
    ///
    /// 押し直しが安全なのは [`crate::agent::claude::Claude::deliver`] が**打つ前に**入力欄を
    /// 見るから — 覆われていれば1文字も送らずに Err を返す。だから「人がダイアログに答えた
    /// 次の tick で流れる」が、余計な打鍵なしで成立する。2026-08-18 の18分沈黙への答え。
    fn retry_pending(&mut self, ctx: &LogCtx) {
        let roots: Vec<String> = self.pending.keys().cloned().collect();
        for root_ts in roots {
            let entry = self.threads.get(&root_ts).cloned();
            let Some(sid) = entry.as_ref().and_then(|e| e.agent_id.clone()) else {
                continue;
            };
            let window = SessionId::from(sid.clone()).window_name();
            // 聞こえる相手にだけ押す。Starting の分は user_prompt が流す道が生きている
            if self.workers.state_of(entry.as_ref(), &window, self.deps.agent.as_ref())
                != inbound::WorkerState::Ready
            {
                continue;
            }
            let key = entry
                .as_ref()
                .and_then(|e| e.channel_id.as_ref())
                .map(|ch| ThreadKey::new(ch, &root_ts));
            self.flush_queued(&sid, Some(root_ts), key.as_ref(), ctx);
        }
    }

    /// Ready になった瞬間に、待たせていた分を押し込む。
    fn flush_queued(
        &mut self,
        session_id: &str,
        owning: Option<String>,
        key: Option<&ThreadKey>,
        ctx: &LogCtx,
    ) {
        let Some(root_ts) = owning else { return };
        let Some(mut queued) = self.pending.remove(&root_ts) else {
            return;
        };
        let window = self
            .workers
            .window_of(session_id)
            .unwrap_or_else(|| SessionId::from(session_id.to_string()).window_name());
        let mut delivered = false;
        let mut i = 0;
        while i < queued.len() {
            let text = Envelope::of(&queued[i], &root_ts, self.deps.clock.now_ms());
            match self.deps.agent.send_text(&Window::of(&window), &text) {
                Ok(()) => {
                    ctx.info("bridge", "flushed queued message");
                    delivered = true;
                    i += 1;
                }
                // 渡せなかった分は queue に**戻す**(捨てると二度と届かない)。同じ窓宛なので
                // 1通目が詰まれば残りも詰まる — そこで止めて、順序のまま先頭に返す
                Err(e) => {
                    let back: Vec<InboundMsg> = queued.split_off(i);
                    ctx.error(
                        "bridge",
                        &format!("flush failed: {e} — {} message(s) re-queued", back.len()),
                    );
                    self.pending
                        .entry(root_ts.clone())
                        .or_default()
                        .splice(0..0, back);
                    break;
                }
            }
        }
        // 押し込みも配達 — 渡した瞬間に shimmer を出す(直接配達の 446-448 と同じ)。
        // ここを落とすと、起動待ちで queue に積まれたメッセージは最後まで無印のままになる
        if delivered {
            if let Some(key) = key {
                self.touch_thread(key, slack::TYPING_STATUS);
            }
        }
    }

    /// PermissionRequest hook の答え。Bridge は**この判断の唯一の権限者**ではなく、
    /// 常設規則で決まるものだけ即答し、決まらないものは辞退して Claude Code 自身の
    /// 許可経路に任せる(`{}` = 口を出さない)。
    ///
    /// **常設規則が最初に効く**のが肝: これが無いとワーカーは自分の返信ツールの許可を
    /// 人に訊きにいき、誰も押さないまま止まる(警句)。
    fn perm_decision(&mut self, ev: &HookEvent, ctx: &LogCtx) -> Option<serde_json::Value> {
        let p = &ev.payload;
        let tool = p["tool_name"].as_str().unwrap_or_default();
        let input = &p["tool_input"];
        Some(
            match crate::bridge::command::ToolPermission::decide(tool, input, self.deps.dir.path()) {
                crate::bridge::command::ToolPermission::Allow(because) => {
                    ctx.debug(
                        "bridge",
                        &format!("perm tool={tool} -> auto-allow ({because})"),
                    );
                    crate::bridge::command::ToolPermission::decision_output(
                        "allow",
                        &format!("Slack bridge ({because})"),
                    )
                }
                crate::bridge::command::ToolPermission::Deny(because) => {
                    ctx.info("bridge", &format!("perm tool={tool} -> DENIED: {because}"));
                    crate::bridge::command::ToolPermission::decision_output("deny", because)
                }
                // 規則では決まらない — 人に訊く。答えは後から来るのでここでは返さない
                crate::bridge::command::ToolPermission::Ask => return None,
            },
        )
    }

    /// 常設規則で決まればその場で答え、決まらなければ Slack に訊きにいって `respond` を預かる。
    /// スレッドが引けない・投稿に失敗した場合は辞退(`{}`)して Claude Code 自身の経路に落とす —
    /// 黙って拒むより手が残る。
    async fn on_perm(&mut self, ev: &mut HookEvent, key: Option<&ThreadKey>, ctx: &LogCtx) {
        let Some(respond) = ev.respond.take() else {
            return; // 答えを待っていない = 何もしない
        };
        if let Some(out) = self.perm_decision(ev, ctx) {
            let _ = respond.send(out);
            return;
        }
        let tool = ev.payload["tool_name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let Some((channel, Some(thread_ts))) = key.map(|k| k.split()) else {
            ctx.info(
                "bridge",
                &format!("perm tool={tool} — no thread for this session; standing aside"),
            );
            let _ = respond.send(serde_json::json!({}));
            return;
        };
        // 人が前に押した範囲なら訊かない。狭い方(スレッド)から見る
        for (granted, scope) in [
            (
                self.threads.thread_tool_allowed(&thread_ts, &tool),
                "thread-grant",
            ),
            (
                self.access.channel_tool_allowed(&channel, &tool),
                "channel-grant",
            ),
        ] {
            if granted {
                ctx.debug("bridge", &format!("perm tool={tool} -> {scope} auto-allow"));
                let _ = respond.send(crate::bridge::command::ToolPermission::decision_output(
                    "allow",
                    &format!("Slack bridge ({scope})"),
                ));
                return;
            }
        }
        // 消えたスレッド根に thread_ts 付きで投げると、Slack はそれを**チャンネル直下の
        // 発言**として落とす = チャンネルが荒れる。投げる前に根の生存を確かめ、消えていれば
        // プロンプトを出さずに deny する(DM は根がそうやって消えないので確認しない)。
        if !channel.starts_with('D') && self.deps.slack.thread_root_gone(&channel, &thread_ts).await {
            ctx.info(
                "bridge",
                &format!(
                    "perm tool={tool} -> thread-root {thread_ts} gone in chan {channel}; \
                     suppressing prompt (deny, no channel spam)"
                ),
            );
            let _ = respond.send(crate::bridge::command::ToolPermission::decision_output(
                "deny",
                "Slack bridge (thread-gone)",
            ));
            return;
        }
        // reqId は Bridge の中だけで意味を持つ札。時刻 + pid + 連番で十分に一意
        let req_id = format!("{:x}-{}", self.deps.clock.now_ms(), self.perm_pending.len());
        let input = ev.payload["tool_input"].clone();
        let prompt_ts = match self
            .deps.slack
            .post_perm_prompt(&channel, &thread_ts, &req_id, &tool, &input)
            .await
        {
            Ok(ts) => ts,
            Err(e) => {
                ctx.error(
                    "bridge",
                    &format!("perm tool={tool}: prompt post failed: {e}"),
                );
                let _ = respond.send(serde_json::json!({}));
                return;
            }
        };
        ctx.info(
            "bridge",
            &format!(
                "perm_request reqId={req_id} tool={tool} chan={channel} -> posted Slack prompt, \
                 awaiting click"
            ),
        );
        // ここから先ワーカーは人待ちで黙る。見張りを止める
        self.suspend_stall_for_perm(&ThreadKey::new(&channel, &thread_ts));
        self.perm_pending.insert(
            req_id,
            PermPending {
                respond,
                channel,
                thread_ts,
                tool_name: tool,
                tool_use_id: ev.payload["tool_use_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                prompt_ts,
                deadline_ms: self.deps.clock.now_ms() + PERM_WAIT_MS,
            },
        );
    }

    /// 許可プロンプトを出した = ワーカーは**意図的に**黙る。見張りを止め、
    /// 既に出ている `is thinking…` も消す(待っていることはプロンプト自身が示している)。
    fn suspend_stall_for_perm(&mut self, key: &ThreadKey) {
        let Some(e) = self.stall.get_mut(key) else {
            return; // このラウンドの見張りがまだ無い(初回ターン)— 止めるものが無い
        };
        e.awaiting_perm = true;
        if std::mem::replace(&mut e.shown, false) {
            e.thinking.set("");
        }
    }

    /// 許可が決着した。`rearm` = 人が押した(沈黙の計測をやり直す)。
    /// 満期のときは **false** — 今まさに「時間切れ」と言ったのに見張りを張り直さない
    /// (本当の活動が来たら touch_thread が張り直す)。
    fn resume_stall_after_perm(&mut self, key: &ThreadKey, rearm: bool) {
        let Some(e) = self.stall.get_mut(key) else {
            return;
        };
        e.awaiting_perm = false;
        if std::mem::replace(&mut e.shown, false) {
            e.thinking.set("");
        }
        if rearm {
            e.last_activity_ms = self.deps.clock.now_ms();
        }
    }

    /// 承認ボタンが押された。**押した瞬間にワーカーへ答える** — その後でプロンプトを
    /// 押された結果に描き替える(投稿の書き替えが遅れてもワーカーは待たない)。
    async fn on_perm_click(&mut self, click: slack::PermClick) {
        let Some(p) = self.perm_pending.remove(&click.req_id) else {
            // 既に押された / 満期で畳んだ。二度目のクリックは何もしない
            return;
        };
        let ctx = LogCtx {
            session_id: None,
            thread_key: Some(ThreadKey::new(&p.channel, &p.thread_ts)),
        };
        let allow = click.action != "deny";
        let by = &click.action;
        ctx.info(
            "bridge",
            &format!(
                "perm reqId={} tool={} -> {} (by {} {})",
                click.req_id,
                p.tool_name,
                if allow { "allow" } else { "deny" },
                click.by,
                by
            ),
        );
        let _ = p
            .respond
            .send(crate::bridge::command::ToolPermission::decision_output(
                if allow { "allow" } else { "deny" },
                &format!("Slack bridge ({by})"),
            ));
        let key = ThreadKey::new(&p.channel, &p.thread_ts);
        self.resume_stall_after_perm(&key, true);
        if !allow {
            // 断ったツールの行を 🚫 に。満期の注記行とは**別の道**
            self.sticky.on_perm_denied(&key, &p.tool_use_id);
        }
        // 以後この範囲では訊かない、を覚える。**人が押したときにしか書かれない**
        match click.action.as_str() {
            "allow-thread" => self.grant_tool(&p.thread_ts, &p.tool_name, true, &ctx),
            "allow-channel" => self.grant_tool(&p.channel, &p.tool_name, false, &ctx),
            _ => {}
        }
        let mark = if allow { "✅" } else { "🚫" };
        let done = format!(
            "{mark} `{}` — {} by <@{}>",
            p.tool_name, click.action, click.by
        );
        if let Err(e) = self
            .deps.slack
            .update_message(&p.channel, &p.prompt_ts, &done)
            .await
        {
            ctx.debug("bridge", &format!("perm prompt update failed: {e}"));
        }
    }

    /// 人が押さないまま満期。ワーカーは既に諦めて次へ進んでいるので、**残ったプロンプトを消す** —
    /// 残すと後から押された Allow が「効いたのに何も起きない」になる(#2)。
    async fn expire_perm_prompts(&mut self) {
        let now = self.deps.clock.now_ms();
        let expired: Vec<String> = self
            .perm_pending
            .iter()
            .filter(|(_, p)| now >= p.deadline_ms)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            let Some(p) = self.perm_pending.remove(&id) else {
                continue;
            };
            let ctx = LogCtx {
                session_id: None,
                thread_key: Some(ThreadKey::new(&p.channel, &p.thread_ts)),
            };
            ctx.info(
                "bridge",
                &format!(
                    "perm prompt reqId={id} EXPIRED (no click) tool={} — deleting stale prompt",
                    p.tool_name
                ),
            );
            let _ = p.respond.send(serde_json::json!({}));
            let key = ThreadKey::new(&p.channel, &p.thread_ts);
            // 張り直さない — いま「時間切れ」と言ったばかりなので、本当の活動が来るまで黙る
            self.resume_stall_after_perm(&key, false);
            self.sticky.on_perm_timeout(&key);
            if let Err(e) = self.deps.slack.delete_message(&p.channel, &p.prompt_ts).await {
                ctx.debug("bridge", &format!("perm prompt delete failed: {e}"));
            }
        }
    }

    /// 「以後このスレッド/チャンネルでは訊かない」を access.json に書く。
    /// **人がボタンを押したときにしか呼ばれない** — セッションが自分で書く道は無い。
    fn grant_tool(&mut self, scope: &str, tool: &str, thread: bool, ctx: &LogCtx) {
        let (where_, saved) = if thread {
            self.threads.grant_thread_tool(scope, tool);
            ("thread", self.threads.save().map_err(|e| e.to_string()))
        } else {
            self.access.grant_channel_tool(scope, tool);
            (
                "channel",
                self.access.save(&self.deps.dir).map_err(|e| e.to_string()),
            )
        };
        match saved {
            Ok(()) => ctx.info("bridge", &format!("perm: granted {tool} for this {where_}")),
            Err(e) => ctx.error("bridge", &format!("perm: {where_}-grant save failed: {e}")),
        }
    }

    /// PreToolUse / PostToolUse → 付箋のツール行。行に**しない**ツールの判定は board 側の不変条件。
    fn on_progress(&mut self, key: Option<&ThreadKey>, p: &serde_json::Value) {
        let Some(key) = key else { return };
        let name = p["tool_name"].as_str().unwrap_or_default();
        // ツール名の無い progress は「ツールの素性がまだ決まっていない」活動 ping。
        // **行にしない**(見張りの張り直しは呼び手が hook 種別に関わらず済ませている)。
        // 行にすると名前が空の行が生まれ、畳めないので**連続した Read/検索の run を分断する**
        // (位置の門番)。
        if name.is_empty() {
            return;
        }
        let content = &p["tool_response"]["content"];
        let result_text = content
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| serde_json::to_string(content).unwrap_or_default());
        let status = slack::ToolStatus::of(
            p["hook_event_name"].as_str().unwrap_or_default(),
            p["tool_response"]["is_error"].as_bool().unwrap_or(false),
            &result_text,
        );
        let summary = slack::StickyBoard::summarize(name, &p["tool_input"]);
        // id が無いと全ツールが1行を上書きし合う — 名前+引数で代用する
        let id = match p["tool_use_id"].as_str().unwrap_or_default() {
            "" => format!("{name}:{summary}"),
            id => id.to_string(),
        };
        // subagent の素性。top-level の agent_id/agent_type は
        // **subagent の中で走ったツールにだけ**付く(main セッションでは無い)。
        // 起こした側の id は tool_response に載り、foreground は camelCase、
        // background/teammate は snake_case で来る。両方受ける。
        let agent = slack::sticky::AgentRef {
            agent_id: p["agent_id"].as_str().map(str::to_string),
            agent_type: p["agent_type"].as_str().map(str::to_string),
            // 拾うのは **Agent/Task の PostToolUse だけ**。他のツールの結果に同名の
            // フィールドがあっても掴まない。3つの形があり、
            // background/teammate は content が null なので最後の落穂拾いが効かない
            spawned_agent_id: ((name == "Agent" || name == "Task")
                && p["hook_event_name"].as_str() == Some("PostToolUse"))
            .then(|| {
                p["tool_response"]["agentId"]
                    .as_str()
                    .or_else(|| p["tool_response"]["agent_id"].as_str())
                    .map(str::to_string)
                    .or_else(|| Self::agent_id_in_text(&result_text))
            })
            .flatten(),
            // background/teammate を名前で結ぶための起動名(`input.name`)
            launched_name: p["tool_input"]["name"].as_str().map(str::to_string),
        };
        self.sticky
            .upsert_tool(key, &id, name, &summary, status, &agent, &p["tool_input"]);
    }

    /// 結果テキストに素で書かれた `agentId: <id>`(最後の落穂拾い。`/agentId:\s*(\w+)/`)。
    fn agent_id_in_text(result_text: &str) -> Option<String> {
        let rest = result_text.split_once("agentId:")?.1.trim_start();
        let id: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        (!id.is_empty()).then_some(id)
    }

    /// MessageDisplay の delta を message_id ごとに繋ぎ、`final` で1行に確定する。
    fn on_narration(&mut self, key: Option<&ThreadKey>, p: &serde_json::Value, ctx: &LogCtx) {
        let Some(key) = key else { return };
        let msg_id = p["message_id"].as_str().unwrap_or_default();
        // message_id は空で届きうる — スレッドで名前空間を切らないと全スレッドが1本の
        // バッファを共有して混線する(NUL は Slack の id に現れない区切り)
        let slot = format!("{key}\u{0}{msg_id}");
        let buf = self.narration.entry(slot.clone()).or_default();
        // 蓄積は放っておくと無限に伸びる — 1行の予算を超えたらもう足さない
        if buf.len() < NARRATION_CAP {
            buf.push_str(p["delta"].as_str().unwrap_or_default());
        }
        if !p["final"].as_bool().unwrap_or(false) {
            return;
        }
        let Some(text) = self.narration.remove(&slot) else {
            return;
        };
        let text = if text.len() > NARRATION_CAP {
            ctx.debug(
                "bridge",
                &format!("narration clipped msg={msg_id} len={}", text.len()),
            );
            slack::StickyBoard::clip(&text, NARRATION_CAP)
        } else {
            text
        };
        if !text.is_empty() {
            self.sticky.push_narration(key, &text);
        }
    }

    /// ターンを終わらせてよいか。答えないとワーカーが最大5秒吊るので、必ず値を返す。
    fn stop_decision(
        &self,
        key: Option<&ThreadKey>,
        ev: &HookEvent,
        ctx: &LogCtx,
    ) -> serde_json::Value {
        let pending = key.map(|k| self.ledger.pending(k)).unwrap_or_default();
        let mcp_ready = self
            .workers
            .warm(&ev.session_id)
            .is_some_and(|h| h.mcp_ready);
        let block = bridge::Disposition::should_block_stop(
            key.is_some(),
            ev.payload["stop_hook_active"].as_bool().unwrap_or(false),
            mcp_ready,
            pending.len(),
        );
        if let Some(key) = key {
            if block {
                ctx.info(
                    "bridge",
                    &format!(
                        "stop blocked: thread={key} ended with undisposed message(s) [{}] \
                         → re-prompting worker to reply/react/no_reply",
                        pending.join(",")
                    ),
                );
            } else if !mcp_ready && !pending.is_empty() {
                ctx.info(
                    "bridge",
                    &format!(
                        "stop allowed (fail-open): thread={key} has undisposed message(s) [{}] \
                         but MCP not ready (session={}) — leaving it for warm-push/recovery, \
                         not re-prompting",
                        pending.join(","),
                        ev.session_id
                    ),
                );
            }
        }
        if block {
            bridge::Disposition::block_output(&pending)
        } else {
            serde_json::json!({})
        }
    }

    /// ツールが「答えた」— 台帳から落とし、付箋を決着させる。
    async fn on_disposition(&mut self, d: Disposition) {
        // reply / no_reply / edit は thread_ts を運ばないことがある — セッションから根を引き戻す
        let root = d.thread_ts.clone().or_else(|| {
            self.threads
                .find_by_session(&d.session_id)
                .map(|(ts, _)| ts.clone())
        });
        let Some(root) = root else {
            LogCtx {
                session_id: Some(d.session_id.clone()),
                thread_key: None,
            }
            .info(
                "bridge",
                &format!(
                    "disposition={} thread unresolved — ledger and sticky left untouched",
                    d.kind
                ),
            );
            return;
        };
        let key = ThreadKey::new(&d.channel_id, &root);
        let ctx = LogCtx {
            session_id: Some(d.session_id.clone()),
            thread_key: Some(key.clone()),
        };
        // 答えが出た = shimmer の役目は終わり(reply / no_reply / edit_message のどれでも)。
        // 落とすだけでよい — slack::Thinking の Drop がキューの最後尾からクリアを流すので、
        // 直前に見張りが投げた `is thinking…` を追い越さずに必ず後から消える
        self.stall.remove(&key);
        let before = self.ledger.pending(&key).len();
        if d.message_ids.is_empty() {
            self.ledger.dispose_all(&key); // ids 無しはスレッド全消化
        } else {
            self.ledger.disposed(&key, &d.message_ids);
        }
        ctx.debug(
            "bridge",
            &format!(
                "ledger: {before} → {} undisposed after {}",
                self.ledger.pending(&key).len(),
                d.kind
            ),
        );
        // 返信・編集は最後の絵を出してから記録として残す。沈黙とリアクションは付箋ごと消す
        if matches!(d.kind, "reply" | "edit")
            && let Some((posted, body)) = self.sticky.take_final(&key)
        {
            self.flush_sticky(&key, posted, body).await;
        }
        if let slack::StickyAction::Delete(ts) = self.sticky.settle(&key, d.kind) {
            let (api, channel) = (self.deps.slack.clone(), d.channel_id.clone());
            tokio::spawn(async move {
                if let Err(e) = api.delete_message(&channel, &ts).await {
                    ctx.debug("bridge", &format!("sticky delete failed: {e}"));
                }
            });
        }
    }

    async fn flush_stickies(&mut self) {
        for (key, posted, body) in self.sticky.take_dirty(self.deps.clock.now_ms()) {
            self.flush_sticky(&key, posted, body).await;
        }
    }

    /// 付箋の1枚を Slack に反映する。post だけは ts を覚えるので待つ(update は投げっぱなし)。
    async fn flush_sticky(&mut self, key: &ThreadKey, posted: Option<String>, body: String) {
        if body.is_empty() {
            return; // Slack は空 text の update を弾く
        }
        let (channel, root) = key.split();
        let ctx = LogCtx {
            session_id: None,
            thread_key: Some(key.clone()),
        };
        match posted {
            Some(ts) => {
                let api = self.deps.slack.clone();
                tokio::spawn(async move {
                    if let Err(e) = api.update_message(&channel, &ts, &body).await {
                        ctx.error("bridge", &format!("sticky update failed: {e}"));
                    }
                });
            }
            None => match self
                .deps.slack
                .post_message(&channel, &body, root.as_deref())
                .await
            {
                Ok(ts) => self.sticky.set_posted(key, &ts),
                Err(e) => ctx.error("bridge", &format!("sticky post failed: {e}")),
            },
        }
    }
}

/// 前のプロセスが `restart` で降りるときに残したマーカーを**消費**する(読んで消す)。
/// 残すと、次の起動が身に覚えの無い「✅ 再起動が完了しました」を出す。
/// 中断スレッドの自動再開はこの実装に無いので、再開の行は 0 件で閉じる。
async fn consume_restart_marker(dir: &bridge::StateDir, api: &dyn ports::SlackPort) {
    let ctx = LogCtx::default();
    let path = dir.restart_marker();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    if let Err(e) = std::fs::remove_file(&path) {
        ctx.error(
            "bridge",
            &format!(
                "could not remove the restart marker (a later start may post a false ✅): {e}"
            ),
        );
    }
    let m: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    let (Some(channel), Some(ts)) = (m["channel"].as_str(), m["progress_ts"].as_str()) else {
        ctx.info(
            "bridge",
            "restart marker consumed — no progress checklist to finish",
        );
        return;
    };
    let done = RestartPhase::Done.render(None);
    match api.update_message(channel, ts, &done).await {
        Ok(()) => ctx.info(
            "bridge",
            &format!("restart: checklist completed for {channel}:{ts} (marker consumed)"),
        ),
        Err(e) => ctx.error(
            "bridge",
            &format!("restart: could not finish the progress checklist for {channel}:{ts}: {e}"),
        ),
    }
}

/// このマシンについて外のコマンドに訊くこと。
pub struct Host;

impl Host {
    /// home 通知に出すホスト名を `hostname` 1回で取る(`now_wallclock` と同じ流儀)。
    /// 飾りなので失敗しても落とさない — `unknown` で通す。
    pub async fn name() -> String {
        let out = tokio::process::Command::new("hostname").output().await;
        match out {
            Ok(o) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if s.is_empty() {
                    "unknown".to_string()
                } else {
                    s
                }
            }
            _ => "unknown".to_string(),
        }
    }

    /// ルート未設定のチャンネル / DM のワーカーが立つ場所。**pwd と spawn は同じこれを読む**
    /// (usage の probe・context/resume の cwd フォールバック・login セッションの `-c` も全部ここ)。
    /// 現行の`workerHomeOf` = `access.workerHome ?? homedir()` — Bridge を
    /// どこから起動したかで変わってはいけない。カレントに落ちるのは HOME が読めない時だけ。
    ///
    /// ponytail: `access.workerHome` の型付けはまだ無い(未知フィールドとして往復保存はされている)。
    /// setup がそれを書き始めたら、ここで先に読む
    pub fn home() -> String {
        match std::env::var("HOME") {
            Ok(h) if !h.is_empty() => h,
            _ => std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".to_string()),
        }
    }

    /// Wall-clock epoch ms for the wiring in `run()`. Bridge methods read `deps.clock` instead.
    pub fn now_ms() -> u64 {
        bridge::now_ms()
    }
}

/// Relay から来たものを、直結モードと**同じ2本の口**に流し込む。
///
/// ここが「Relay 経由」と「直結」の合流点。イベントは slack-morphism の型に戻してから
/// `InboundMsg` にする — **変換ロジックを複製しない**のが要で、2本目を書くと、直結と
/// Relay 経由で門番の判断がいつか食い違う。
///
/// `reload` は「access.json を書いたから読み直せ」の合図。**書きっぱなしにすると、動いている
/// Bridge は古い値(Owner 空)を見続けて、届いたものを全部 `no-owner` で捨てる** — 入口で
/// 捨てるので Owner を直すコマンドも入らず、再起動するまで抜けられない(2026-08-02 実機で発生)。
///
/// ponytail: 合図と本文は別の channel なので、同じ瞬間に両方届いた1回分だけ順序が入れ替わりうる
/// (握手と本文が同時に来たときだけ)。気になったら oneshot で ack を待つ
/// 握手で来た home を、こちらの現在値に反映する。**変わったら true**(呼ぶ側が読み直しの
/// 合図を出す)。
///
/// **起動時の「最初の `Ready` 待ち」もここを通す。** 待ちループは bot トークンだけ取って
/// home を捨てていたので、親が `attach()` で1回だけ載せてくる home が誰にも読まれず、
/// 起動したての子は自分の古い home(または未設定 → Owner DM)に通知を出していた
/// (2026-08-02 実機: 子の online 通知が親の home と違うチャンネルに出た)。
fn adopt_home(dir: &bridge::StateDir, home: Option<String>) -> bool {
    let Some(home) = home else { return false };
    let mut access = bridge::Access::load(dir);
    if access.home_channel.as_deref() == Some(home.as_str()) {
        return false;
    }
    access.home_channel = Some(home.clone());
    if let Err(e) = access.save(dir) {
        LogCtx::default().error("bridge", &format!("could not save the home channel: {e}"));
        return false;
    }
    LogCtx::default().info(
        "bridge",
        &format!("remote link: home channel is now {home}"),
    );
    true
}

async fn pump_relay(
    item: link::FromRelay,
    msg_tx: &mpsc::Sender<InboundMsg>,
    click_tx: &mpsc::Sender<slack::PermClick>,
    dir: &bridge::StateDir,
    reload: &mpsc::Sender<()>,
    relink: &mpsc::Sender<()>,
) {
    match item {
        link::FromRelay::Event { name, event } => {
            if let Some(msg) = slack::inbound_from_relay(&name, &event)
                && msg_tx.send(msg).await.is_err()
            {
                return;
            }
        }
        link::FromRelay::Action { action, body } => {
            if let Some(click) = slack::perm_click_from_relay(&action, &body)
                && click_tx.send(click).await.is_err()
            {
                return;
            }
        }
        // 握手のたびに来る。Relay が持っている home を、こちらの現在値に反映する
        // (`set-home` を聞き逃していたマシンが、繋ぎ直しで追いつく)
        link::FromRelay::Ready { home, .. } => {
            if adopt_home(dir, home) {
                let _ = reload.send(()).await;
            }
            // **起動時の1本目はここを通らない**(構築前の待ちループが食う)。ここに来るのは
            // 張り直しだけなので、そのたびに online を出す
            let _ = relink.send(()).await;
        }
        // Owner がこのマシンを担当に決めた。**Owner を記録する** — Relay 経由の Bridge は
        // これが来るまで Slack 上の自分の身元を何も知らない
        link::FromRelay::Linked {
            owner_user_id,
            channel,
            ..
        } => {
            let mut access = bridge::Access::load(dir);
            if access.owner != owner_user_id {
                access.owner = owner_user_id.clone();
                if let Err(e) = access.save(dir) {
                    LogCtx::default().error("bridge", &format!("could not save the owner: {e}"));
                } else {
                    LogCtx::default().info(
                        "bridge",
                        &format!("remote link: owner is {owner_user_id} (in charge of {channel})"),
                    );
                    let _ = reload.send(()).await;
                }
            }
        }
        // ここまで来たらリンクは諦めている。**ワーカーには触らない** — 走っているものは
        // 走り続ける。人が設定を直して再起動するまで、新しい Slack メッセージが来ないだけ
        link::FromRelay::Fatal(f) => {
            LogCtx::default().error("bridge", &format!("remote link: {} — no new messages will arrive (running workers keep going)", f.message()));
        }
    }
}

impl Bridge {
    /// 配線して select ループを回す。シグナル契約は
    /// SIGTERM/SIGINT=graceful shutdown / SIGUSR1=maintenance restart / SIGHUP=再読込。
    pub async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let dir = bridge::StateDir::resolve();
        for (k, v) in dir.load_env()? {
            // 起動時・spawn 前の単スレッド区間なので安全
            unsafe { std::env::set_var(k, v) };
        }
        // 上流(直結か親経由か)と下流(子を迎えるか)。**両方揃った設定は起動しない**
        // (黙って両方繋ぐと Slack が負荷分散を始め、split-brain がそのまま戻る)
        let wiring = link::Wiring::resolve(|k| std::env::var(k).ok())?;
        let mode = wiring.upstream.clone();
        LogCtx::default().info(
            "bridge",
            &format!("starting — state dir {}", dir.path().display()),
        );

        let (msg_tx, mut msg_rx) = mpsc::channel(64);
        let (click_tx, mut click_rx) = mpsc::channel(16);
        // 聞き始める**前**に打つ。この後ろで取ると、接続と
        // auth.test にかかった数百 ms の間に届いた生きたコマンドが「起動前の投稿」に見えて黙って落ちる
        let started_at_ms = Host::now_ms();

        // Relay 経由では bot トークンが**握手で来る**ので、Api はその後にしか作れない。
        // ここが直結との唯一の順序の違い。
        let (bot_token, link_home, relay_rx) = match &mode {
            link::Mode::Direct { bot_token, .. } => (bot_token.clone(), None, None),
            link::Mode::Relay {
                url,
                api_token,
                bridge_id,
            } => {
                let (tx, mut rx) = mpsc::channel(64);
                let l = Arc::new(link::RelayLink::new(url, api_token, bridge_id));
                tokio::spawn({
                    let l = l.clone();
                    async move { l.run(tx).await }
                });
                // 最初の Ready が来るまでは Slack に何も書けない。**待つ**
                let (token, home) = loop {
                    match rx.recv().await {
                        Some(link::FromRelay::Ready { bot_token, home }) => {
                            break (bot_token, home);
                        }
                        // **握手を断られても落ちない。** 落ちると supervisor がすぐ起こし直し、
                        // その連打が systemd の起動レート制限(既定 10秒に5回)を踏んで
                        // unit を `failed` のまま置き去りにする — デプロイ中の一瞬の 401 で
                        // 子が恒久的に上がってこなくなる(2026-08-03、子が5時間15分停止)。
                        // `RelayLink::run` は間を空けて繋ぎ直し続けるので、ここでは待つ
                        Some(link::FromRelay::Fatal(f)) => {
                            LogCtx::default().error(
                                "bridge",
                                &format!("remote link: {} — retrying until it is fixed", f.message()),
                            );
                            continue;
                        }
                        Some(_) => continue, // 受理前に何か来ても捨てる
                        None => return Err("relay link ended before the handshake".into()),
                    }
                };
                (token, home, Some(rx))
            }
            // 親が迎えに来る。**待つのは同じ** — 最初の Ready まで Slack には何も書けない
            link::Mode::AwaitParent => {
                let Some(listen) = wiring.inlet.clone() else {
                    return Err("AGENTGW_LINK_LISTEN is not set, so the gateway has nowhere to connect".into());
                };
                let (tx, mut rx) = mpsc::channel(64);
                let inlet = Arc::new(link::GatewayInlet {
                    token: listen.token,
                    tx,
                });
                tokio::spawn(inlet.serve(listen.addr));
                let (token, home) = loop {
                    match rx.recv().await {
                        Some(link::FromRelay::Ready { bot_token, home }) => {
                            break (bot_token, home);
                        }
                        Some(_) => continue,
                        None => return Err("the inlet closed before the parent arrived".into()),
                    }
                };
                (token, home, Some(rx))
            }
        };
        // 握手の1本目に載っている home をここで反映する。この後 `Access::load` で読み直すので、
        // 起動通知(online)は最初から親と同じチャンネルに出る
        adopt_home(&dir, link_home);

        let api = Arc::new(slack::Api::new(&bot_token)?);
        let (hook_tx, mut hook_rx) = mpsc::channel(64);
        let (dispo_tx, mut dispo_rx) = mpsc::channel(64);
        let (hook_port, hook_token) = HookIntake::serve(&dir, hook_tx.clone()).await?;
        let (mcp_port, mcp_token) = mcp::Mcp::serve(
            &dir,
            Arc::new(slack::ToolExec {
                api: api.clone(),
                state_dir: dir.path().to_path_buf(),
                dispo: dispo_tx,
            }),
            hook_tx,
        )
        .await?;
        let hooks_file = HookIntake::write_settings(&dir, hook_port, &hook_token)?;
        let hooks_file = hooks_file.to_string_lossy().to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (reload_tx, mut reload_rx) = mpsc::channel(4);
        // 親との link を張り直したときの合図(子だけが使う)
        let (relink_tx, mut relink_rx) = mpsc::channel(4);
        consume_restart_marker(&dir, api.as_ref()).await;

        // 子を迎えるなら、Slack のイベントは**畳む前に**こちらへ回す。誰の担当かを決めてから、
        // 自分の分だけ msg_tx / click_tx に戻る(= ローカル配達は直結と同じ変換を通る)
        let fleet = wiring.children.map(|listen| {
            // loopback の外に出したなら1行残す。**拒否はしない**(2026-08-02 の判断)が、
            // 前段の TLS を忘れたまま動いている Bridge は、ログにも痕跡が無いと気づけない
            if listen.is_exposed() {
                LogCtx::default().info(
                    "bridge",
                    &format!(
                        "children port {} is outside the loopback — \
                         put TLS in front (tailscale serve / reverse proxy) or the bot token \
                         crosses the network in the clear",
                        listen.addr
                    ),
                );
            }
            let fleet = Arc::new(crate::bridge::gateway::Fleet {
                links: crate::bridge::gateway::LinkServer::new(),
                token: listen.token,
                self_id: wiring.self_id.clone().unwrap_or_default(),
                bot_token: bot_token.clone(),
                api: api.clone(),
                dir: dir.clone(),
                cooldown: Default::default(),
                presence: Default::default(),
                pending_selection: Default::default(),
                bot_user_id: Default::default(),
                msg_tx: msg_tx.clone(),
                click_tx: click_tx.clone(),
                reload: reload_tx.clone(),
                tunnels: Default::default(),
            });
            tokio::spawn(crate::bridge::gateway::serve_children(
                fleet.clone(),
                listen.addr,
            ));
            tokio::spawn(fleet.clone().watch_presence());
            fleet
        });
        // 親が NAT の内側にいる構成でだけ、こちらから子へ迎えに行く
        if let Some(fleet) = &fleet
            && let Ok(raw) = std::env::var("AGENTGW_CHILD_URLS")
        {
            let targets = link::child_urls(&raw);
            if !targets.is_empty() {
                LogCtx::default().info(
                    "relay",
                    &format!(
                        "dialling {} child(ren) from AGENTGW_CHILD_URLS: {}",
                        targets.len(),
                        targets
                            .iter()
                            .map(|(id, _)| id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
                fleet.dial_children(targets);
            }
        }
        // 直結で届かない子には、親が ssh トンネルを張る(`add-child` が書く一覧)。
        // **agentgw の子プロセスとして持つ** — 動いている間だけ繋がっていればよい
        if let Some(fleet) = &fleet
            && let Ok(raw) = std::env::var("AGENTGW_TUNNELS")
        {
            // 出口は親の口。0.0.0.0 で待っていても、トンネルの出口は loopback で足りる
            let port = std::env::var("AGENTGW_LINK_LISTEN")
                .ok()
                .and_then(|l| l.rsplit(':').next().map(str::to_string))
                .unwrap_or_else(|| "8787".to_string());
            for (child, target) in link::child_urls(&raw) {
                tokio::spawn(crate::fleet::keep_tunnel(
                    fleet.clone(),
                    child,
                    target,
                    format!("127.0.0.1:{port}"),
                ));
            }
        }
        let fleet_tx = fleet.as_ref().map(|fleet| {
            let (tx, mut rx) = mpsc::channel::<slack::FleetEvent>(64);
            let fleet = fleet.clone();
            tokio::spawn(async move {
                while let Some(item) = rx.recv().await {
                    fleet.on_fleet_event(item).await;
                }
            });
            tx
        });

        match (mode, relay_rx) {
            (link::Mode::Direct { app_token, .. }, _) => {
                tokio::spawn(async move {
                    if let Err(e) = slack::Api::listen(&app_token, msg_tx, click_tx, fleet_tx).await
                    {
                        LogCtx::default().error("slack", &format!("socket mode stopped: {e}"));
                    }
                });
            }
            // 親経由(こちらから dial / 迎えに来てもらう のどちらでも)。**同じ2本に
            // 流し込む**ので、この下流は1行も変わらない
            (link::Mode::Relay { .. } | link::Mode::AwaitParent, Some(mut rx)) => {
                let dir2 = dir.clone();
                let reload = reload_tx.clone();
                let relink = relink_tx.clone();
                tokio::spawn(async move {
                    while let Some(item) = rx.recv().await {
                        pump_relay(item, &msg_tx, &click_tx, &dir2, &reload, &relink).await;
                    }
                });
            }
            (link::Mode::Relay { .. } | link::Mode::AwaitParent, None) => {
                unreachable!("a machine behind a gateway always has a receiver")
            }
        }

        if bridge::Access::load(&dir).owner.is_empty() {
            LogCtx::default().info("bridge", "no owner in access.json — serving nobody");
        }
        // 起動時に1回だけ訊く。落ちても起動は止めない — コマンド判定が mention 抜きの形だけになる
        let (bot_user_id, bot_name) = match api.auth_test().await {
            Ok((id, name)) => {
                LogCtx::default().info(
                    "bridge",
                    &format!("bot user id {id} ({})", name.as_deref().unwrap_or("?")),
                );
                (Some(id), name)
            }
            Err(e) => {
                LogCtx::default().error("bridge", &format!(
                        "auth.test failed: {e} — commands written with an @mention won't be recognized"
                    ));
                (None, None)
            }
        };
        // フリートのコマンド判定にも同じ id が要る(`@ボット route …`)
        if let Some(fleet) = &fleet {
            *fleet.bot_user_id.lock().await = bot_user_id.clone();
        }
        let mut b = Bridge::new(
            Deps {
                slack: api.clone(),
                agent: Arc::new(Claude::real()),
                clock: Arc::new(ports::SystemClock),
                dir,
            },
            Config {
                hooks_file,
                mcp_port,
                mcp_token,
                bot_user_id,
                started_at_ms,
                fleet: fleet.is_some(),
                cmd_tx,
                slack_api: api, // TODO(Task 12)
            },
        );
        // 前の Bridge が落ちる前に残した login セッションを掃く
        b.deps.agent.login_kill();
        // 再起動でも Owner は access.json に残る。restart は在庫を畳まずに降りるので、
        // まず生き残りを拾い直し(restore_pools)、欠けた枠だけを起こす。指名済みの
        // セッションがある枠は新規 ID ではなく `--resume` で立ち上がる
        if !b.access.owner.is_empty() {
            b.restore_pools(&LogCtx::default()).await;
            b.start_missing_pool_workers(&LogCtx::default());
        }
        // 前プロセスが答えを待っていたスレッドを拾い直す(生きているワーカーの分だけ)
        b.restore_pending(&LogCtx::default());
        // 渡しそびれた依頼を配り直す(ワーカーが居なければ起こして渡す)
        b.resume_pending_from_disk().await;
        // 起動を home に1回知らせる。pending は常に 0 — Rust 版は未完スレッドの自動再開を持たない
        let online_as = bot_name
            .or_else(|| b.bot_user_id.clone())
            .unwrap_or_else(|| "?".to_string());
        b.announce_online(&online_as).await;
        // tokio の signal は features = ["full"] に含まれる(依存は増えない)。
        // slack-morphism が signal-hook で TERM_SIGNALS を先に握っているが、レジストリは
        // 1シグナルに複数の受け手を許すので両方に配送される
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sighup = signal(SignalKind::hangup())?;
        let mut sigusr1 = signal(SignalKind::user_defined1())?;
        let mut flush = tokio::time::interval(FLUSH_INTERVAL);

        loop {
            tokio::select! {
                Some(msg) = msg_rx.recv() => b.on_inbound(&msg).await,
                Some(ev) = hook_rx.recv() => b.on_hook(ev).await,
                // 詰まると MCP ツール呼び出しごとワーカーが固まる — 必ず引き取る
                Some(d) = dispo_rx.recv() => b.on_disposition(d).await,
                Some(fx) = cmd_rx.recv() => b.on_cmd_fx(fx).await,
                Some(c) = click_rx.recv() => b.on_perm_click(c).await,
                _ = flush.tick() => {
                    b.scan_transcripts();
                    b.stall_tick();
                    b.run_drains().await;
                    b.flush_stickies().await;
                    b.expire_perm_prompts().await;
                    b.retry_pending(&LogCtx::default());
                    b.give_up_stale_pools(&LogCtx::default()).await;
                    b.sweep_pools(&LogCtx::default());
                    b.cleanup_workers().await;
                    b.usage_tick().await;
                }
                _ = sigterm.recv() => b.shutdown("signal:SIGTERM").await,
                _ = sigint.recv() => b.shutdown("signal:SIGINT").await,
                _ = sighup.recv() => b.reload_from_disk(),
                // フリート側が access.json を書いた(route / set-home / owner)。SIGHUP と同じ
                Some(()) = reload_rx.recv() => b.reload_from_disk(),
                // 親との link を張り直した。**もう一度 online を出す** — 子にとって
                // 「繋がった」を人に知らせるのはこの1行しかない(親側の presence は
                // 🔴 だけを言う。2か所で同じことを言わない)
                Some(()) = relink_rx.recv() => b.announce_online(&online_as).await,
                // 運用者からの再起動要求。Slack の `restart` と同じ経路に合流し、
                // 要求者スレッドが無いぶんだけチェックリスト・status・marker を省く
                _ = sigusr1.recv() => b.maintenance_restart("SIGUSR1", None, &LogCtx::default()).await,
                else => break,
            }
        }
        Ok(())
    }

    /// `/compact` を打ち込み、pane のスピナーを Slack の付箋に流す。付箋は**最初の進捗が出てから**作る — busy / no-session の
    /// 断りが1本で済むのはそのため。最後の1行は付箋があれば書き換え、無ければ新規投稿。
    ///
    /// TUI を回すのはエージェント側(`AgentPort::compact`)。ここは**進捗を Slack に描く側**だけ —
    /// 最長6分かかるので select ループの外(spawn)で回す。
    async fn run_compact(
        api: ports::Slack,
        agent: ports::AgentRef,
        channel: String,
        root: String,
        key: ThreadKey,
        target: Window,
        sid: String,
    ) {

        let ctx = LogCtx {
            session_id: Some(sid.clone()),
            thread_key: Some(key.clone()),
        };
        // The agent sends each reading; this task draws them one at a time, in order.
        // Capacity 1 keeps the agent at most one reading ahead of the drawing.
        let (tx, mut rx) = mpsc::channel::<CompactProgress>(1);
        let draw = async {
            let mut progress_ts: Option<String> = None;
            let mut last_rendered = String::new();
            while let Some(st) = rx.recv().await {
                let rendered = st.render();
                // 同じ絵を描き直さない(Slack の編集回数はタダではない)
                if rendered == last_rendered {
                    continue;
                }
                last_rendered.clone_from(&rendered);
                let posted = match &progress_ts {
                    Some(ts) => api
                        .update_message(&channel, ts, &rendered)
                        .await
                        .map(|()| None),
                    None => api
                        .post_message_no_unfurl(&channel, &rendered, Some(&root))
                        .await
                        .map(Some),
                };
                match posted {
                    Ok(Some(ts)) => progress_ts = Some(ts),
                    Ok(None) => {}
                    // 描き損ねても圧縮は続く
                    Err(e) => ctx.error("bridge", &format!(
                        "slack-events: compact progress render failed for {channel}:{root}: {e}"
                    )),
                }
            }
            progress_ts
        };
        let (outcome, progress_ts) = tokio::join!(agent.compact(&target, &key, &sid, tx), draw);
        // None = このエージェントが compact に非対応。実体が1つの今は起きない
        let final_text = match outcome {
            Some(CompactOutcome::Done) => crate::t!("✅ Compacted the context.", "✅ コンテキストを圧縮しました。"),
            Some(CompactOutcome::Nothing) => crate::t!(
                "There isn't enough history to compact yet.",
                "圧縮するほどの履歴がまだありません。"
            ),
            Some(CompactOutcome::Failed) | None => crate::t!(
                "Couldn't compact the context. Try again.",
                "コンテキストを圧縮できませんでした。もう一度試してください。"
            ),
        };
        let posted = match &progress_ts {
            Some(ts) => api.update_message(&channel, ts, &final_text).await,
            None => api
                .post_message_no_unfurl(&channel, &final_text, Some(&root))
                .await
                .map(|_| ()),
        };
        if let Err(e) = posted {
            ctx.error(
                "bridge",
                &format!("slack-events: compact final post failed for {channel}:{root}: {e}"),
            );
        }
    }

    /// 素の `effort`。今の level を TUI に訊くのはエージェント側
    /// (`AgentPort::effort`)。ここは答えを1行にして投げるだけ。
    async fn run_effort_show(
        api: Arc<slack::Api>, // TODO(Task 12)
        agent: ports::AgentRef,
        channel: String,
        root: String,
        key: ThreadKey,
        target: Window,
        sid: String,
    ) {
        let failed = || crate::t!("Couldn't read the current effort level.", "今の effort を読み取れませんでした。");
        // None = 非対応か、状態行が読めなかったか — どちらも同じ断りを返す
        let out = match agent.effort(&target, &key, &sid).await {
            Some(level) => crate::t!("Effort level: *{level}*", "effort: *{level}*"),
            None => failed(),
        };
        api.post_now(&channel, &root, out, &key).await;
    }
}

impl Envelope {
    /// 受信メッセージ → 封筒テキスト。`ts` は**配達時点**の now(`now_ms`)、`thread_ts` は解決済みの根
    /// (現行`threadTs || msg.ts` を渡す)。
    pub fn of(msg: &InboundMsg, root_ts: &str, now_ms: u64) -> String {
        Self::of_guarded(msg, root_ts, None, now_ms)
    }

    /// ループ遮断が立った配達だけ `loop_guard` を載せる(空文字 = 呼ぶ相手が居ない)。
    pub fn of_guarded(
        msg: &InboundMsg,
        root_ts: &str,
        loop_guard: Option<String>,
        now_ms: u64,
    ) -> String {
        Envelope {
            loop_guard,
            channel_id: msg.channel.clone(),
            message_id: msg.ts.clone(),
            user: msg.user.clone().unwrap_or_else(|| "unknown".to_string()),
            ts: bridge::iso8601(now_ms),
            thread_ts: Some(root_ts.to_string()),
            text: msg.text.clone(),
            // 先読みダウンロードの結果はメッセージが持っている(queue 経由でも持ち越す)
            file_paths: msg.file_paths.clone(),
            file_errors: msg.file_errors.clone(),
        }
        .render()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 削除の鍵が元メッセージの配達とぶつかると、取り消しが**一度も**通らない
    /// (実機で 100% 落ちていた)。種別ごとに別の鍵になることだけ確かめる。
    #[test]
    fn dedup_key_separates_a_deletion_from_the_message_it_removes() {
        let base = InboundMsg {
            channel: "C1".into(),
            channel_kind: inbound::ChannelKind::Channel,
            ts: "1.1".into(),
            thread_ts: None,
            user: Some("U1".into()),
            is_bot: false,
            bot_id: None,
            text: String::new(),
            files: Vec::new(),
            file_paths: Vec::new(),
            file_errors: Vec::new(),
            reaction: None,
            deleted_ts: None,
            edited: None,
        };
        let deleted = InboundMsg {
            deleted_ts: Some("1.1".into()),
            ..base.clone()
        };
        assert_eq!(dedup_key(&base), "1.1");
        assert_ne!(dedup_key(&deleted), dedup_key(&base));

        // 自分が付けた印だけ捨てる。人が付けたものは今までどおり通す(stop の ✋ が死ぬ)
        let react = |by: &str| InboundMsg {
            user: Some(by.into()),
            reaction: Some(inbound::Reaction {
                emoji: "eyes".into(),
                item_ts: "1.1".into(),
                added: true,
            }),
            ..base.clone()
        };
        assert!(is_own_reaction(&react("UBOT"), Some("UBOT")));
        assert!(!is_own_reaction(&react("U1"), Some("UBOT")));
        // 自分の id をまだ知らない起動直後に、人のリアクションを巻き込まない
        assert!(!is_own_reaction(&react("UBOT"), None));
        assert!(!is_own_reaction(&base, Some("UBOT")), "本文は素通し");
    }

    /// 自分が書いたのではない投稿への stop は、**Owner が自分の依頼に付けたときだけ**通す。
    #[test]
    fn only_the_owner_stopping_their_own_request_counts() {
        let r = |emoji: &str| inbound::Reaction {
            emoji: emoji.into(),
            item_ts: "1.1".into(),
            added: true,
        };
        let v = |emoji, reactor, author| foreign_reaction(&r(emoji), reactor, author, "UOWNER");
        assert_eq!(
            v("raised_hand", Some("UOWNER"), Some("UOWNER")),
            ForeignReaction::Stop,
            "Owner が自分の依頼に付けた"
        );
        assert_eq!(
            v("raised_hand", Some("UOWNER"), Some("U2")),
            ForeignReaction::Drop,
            "他人の投稿 — 誰の仕事を止めるか決まらない"
        );
        assert_eq!(
            v("raised_hand", Some("U2"), Some("U2")),
            ForeignReaction::Drop,
            "Owner ではない"
        );
        assert_eq!(
            v("thumbsup", Some("UOWNER"), Some("UOWNER")),
            ForeignReaction::Drop,
            "stop の絵文字ではない"
        );
        assert_eq!(
            foreign_reaction(&r("raised_hand"), Some("UOWNER"), Some("UOWNER"), ""),
            ForeignReaction::Drop,
            "Owner がまだ居ない"
        );
        // 外したときは止めない(`is_stop` は added のときだけ真)
        let removed = inbound::Reaction {
            added: false,
            ..r("raised_hand")
        };
        assert_eq!(
            foreign_reaction(&removed, Some("UOWNER"), Some("UOWNER"), "UOWNER"),
            ForeignReaction::Drop,
            "外した"
        );
    }

    /// 親から「君が担当」と言われた子は、access.json に書くだけでなく**読み直しの合図まで出す**。
    /// 出さないと動いている門番は Owner 空のままで、以後の配達を全部 `no-owner` で捨てる
    /// (2026-08-02 実機。入口で捨てるので直すコマンドも入らず再起動でしか抜けられなかった)。
    #[tokio::test]
    async fn being_put_in_charge_asks_the_running_bridge_to_reread_access() {
        let dir = bridge::StateDir::at(
            std::env::temp_dir().join(format!("sc-linked-{}", std::process::id())),
        );
        let _ = std::fs::remove_dir_all(dir.path());
        let (msg_tx, _msg_rx) = mpsc::channel(4);
        let (click_tx, _click_rx) = mpsc::channel(4);
        let (reload_tx, mut reload_rx) = mpsc::channel(4);
        let (relink_tx, _relink_rx) = mpsc::channel(4);

        pump_relay(
            link::FromRelay::Linked {
                owner_user_id: "U0OWNER".into(),
                channel: "C1".into(),
                thread_ts: "1.1".into(),
            },
            &msg_tx,
            &click_tx,
            &dir,
            &reload_tx,
            &relink_tx,
        )
        .await;

        assert_eq!(
            bridge::Access::load(&dir).owner,
            "U0OWNER",
            "ディスクに残る"
        );
        assert_eq!(reload_rx.try_recv(), Ok(()), "読み直しの合図が出る");
        let _ = std::fs::remove_dir_all(dir.path());
    }

    /// 握手で来た home はディスクに残る。**同じ値なら false** — 変わっていないのに
    /// 読み直しの合図を出すと、繋ぎ直すたびに門番が access.json を読み直すことになる。
    #[test]
    fn a_home_from_the_handshake_is_kept_on_disk() {
        let dir = bridge::StateDir::at(
            std::env::temp_dir().join(format!("sc-adopt-home-{}", std::process::id())),
        );
        let _ = std::fs::remove_dir_all(dir.path());

        assert!(!adopt_home(&dir, None), "home が無い握手は何もしない");
        assert!(adopt_home(&dir, Some("C_HOME".into())), "初回は変わる");
        assert_eq!(
            bridge::Access::load(&dir).home_channel.as_deref(),
            Some("C_HOME"),
            "ディスクに残る"
        );
        assert!(
            !adopt_home(&dir, Some("C_HOME".into())),
            "同じ値なら変わらない"
        );
        let _ = std::fs::remove_dir_all(dir.path());
    }

    /// 2026-08-03 の事故そのもの: 担当表が空のまま掃除役が回ると、**動いているワーカーの窓が
    /// 全部「持ち主不明」に見えて閉じられる**。持ち主ゼロ + ワーカーの窓あり、の回は触らない。
    #[test]
    fn the_sweeper_does_not_run_when_it_knows_no_owner_but_worker_windows_exist() {
        let row = |name: &str| tmux_mod::WindowRow {
            id: "@1".into(),
            pid: crate::agent::tmux::Pid(1),
            command: "claude".into(),
            name: name.into(),
        };
        let none: HashSet<String> = HashSet::new();
        let some: HashSet<String> = ["S-1".to_string()].into_iter().collect();
        let worker = [row("w-S-1")];
        let human = [row("scratch")];

        assert!(
            !Bridge::safe_to_reap(&none, &worker),
            "持ち主ゼロ + 生きたワーカーの窓 = 記憶を失っている。閉じてはいけない"
        );
        assert!(
            Bridge::safe_to_reap(&some, &worker),
            "持ち主が1人でも居れば、いつもどおり掃除する"
        );
        assert!(
            Bridge::safe_to_reap(&none, &human),
            "ワーカーの窓が無いなら掃除しても何も起きない"
        );
    }

    // ── flows through Bridge, with fakes for Slack, the agent and the clock ──

    use crate::ports::fake::{FakeAgent, FakeClock, FakeSlack};

    /// A fresh state dir with only the owner in access.json.
    fn flow_deps(name: &str) -> (Deps, Arc<FakeSlack>, Arc<FakeAgent>, Arc<FakeClock>) {
        let path = std::env::temp_dir().join(format!("agentgw-flow-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("access.json"), r#"{"owner":"U_OWNER"}"#).unwrap();
        let slack = Arc::new(FakeSlack::default());
        let agent = Arc::new(FakeAgent::default());
        let clock = FakeClock::at(1_782_000_000_000);
        let deps = Deps {
            slack: slack.clone(),
            agent: agent.clone(),
            clock: clock.clone(),
            dir: bridge::StateDir::at(path),
        };
        (deps, slack, agent, clock)
    }

    fn channel_msg(ts: &str, user: &str, text: &str) -> InboundMsg {
        InboundMsg {
            channel: "C1".into(),
            channel_kind: inbound::ChannelKind::Channel,
            ts: ts.into(),
            thread_ts: None,
            user: Some(user.into()),
            is_bot: false,
            bot_id: None,
            text: text.into(),
            files: vec![],
            file_paths: vec![],
            file_errors: vec![],
            reaction: None,
            deleted_ts: None,
            edited: None,
        }
    }

    #[tokio::test]
    async fn an_owner_message_in_a_new_thread_starts_an_agent_and_acks() {
        let (d, slack, agent, _clock) = flow_deps("owner");
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_inbound(&channel_msg("1782000000.000100", "U_OWNER", "<@U_BOT> fix the tests"))
            .await;
        assert_eq!(agent.spawned.lock().unwrap().len(), 1, "one agent for the new thread");
        assert!(
            slack.calls().contains(&"react C1 1782000000.000100 eyes".to_string()),
            "{:?}",
            slack.calls()
        );
    }

    #[tokio::test]
    async fn a_stranger_cannot_start_work() {
        let (d, slack, agent, _clock) = flow_deps("stranger");
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_inbound(&channel_msg("1782000000.000100", "U_STRANGER", "<@U_BOT> rm -rf /"))
            .await;
        assert!(agent.spawned.lock().unwrap().is_empty());
        assert!(slack.calls().is_empty(), "{:?}", slack.calls());
    }

    #[tokio::test]
    async fn the_second_event_for_the_same_message_is_dropped() {
        let (d, _slack, agent, _clock) = flow_deps("dedup");
        let (mut b, _fx) = Bridge::for_test(d);
        let m = channel_msg("1782000000.000100", "U_OWNER", "<@U_BOT> hi");
        b.on_inbound(&m).await;
        b.on_inbound(&m).await;
        assert_eq!(agent.spawned.lock().unwrap().len(), 1);
    }
}
