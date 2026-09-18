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
use crate::agent::claude::Claude;
use crate::agent::tmux::Window;
use crate::agent::{
    Envelope, HookEvent, ProbeErr, SessionId,
};
use crate::bridge::command::CmdFx;
use crate::bridge::render::RestartPhase;
use crate::bridge::state as bridge;
use crate::bridge::inbound::InboundMsg;
use crate::bridge::state::{Disposition, LogCtx, ThreadKey};
use crate::{mcp, ports, slack};
use std::collections::HashMap;
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

impl Bridge {

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
    fn post(&self, channel: &str, thread_ts: &str, text: String, key: &ThreadKey) {
        let (api, channel, thread_ts, key) = (
            self.deps.slack.clone(),
            channel.to_string(),
            thread_ts.to_string(),
            key.clone(),
        );
        tokio::spawn(async move {
            api.post_now(&channel, &thread_ts, text, &key).await;
        });
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

    /// スレッドが引けないうちは milestone を出さない(現行4103 と同じ)。
    fn milestone(&mut self, key: Option<&ThreadKey>, event: &str, ctx: &LogCtx) {
        let Some(key) = key else {
            ctx.debug("bridge", &format!("{event} before its thread is known"));
            return;
        };
        let m = self.lifecycle.record(key, event, self.deps.clock.now_ms());
        ctx.info("lifecycle", &m.message(key, event));
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
        let api = self.deps.slack.clone();
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

        let api: ports::Slack = Arc::new(slack::Api::new(&bot_token)?);
        let (hook_tx, mut hook_rx) = mpsc::channel(64);
        let (dispo_tx, mut dispo_rx) = mpsc::channel(64);
        let (hook_port, hook_token) = HookIntake::serve(&dir, hook_tx.clone()).await?;
        let (mcp_port, mcp_token) = mcp::Mcp::serve(
            &dir,
            Arc::new(slack::ToolExec {
                slack: api.clone(),
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
    use crate::agent::tmux as tmux_mod;
    use std::collections::HashSet;
    use crate::bridge::inbound::{ForeignReaction, dedup_key, foreign_reaction, is_own_reaction};

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

    const ROOT: &str = "1782000000.000100";

    /// The first `user_prompt` hook: what lifts the "still starting" latch.
    async fn on_hook_user_prompt_for_test(b: &mut Bridge, sid: &str) {
        b.on_hook(HookEvent {
            kind: "user_prompt".into(),
            session_id: sid.into(),
            payload: serde_json::json!({}),
            respond: None,
        })
        .await;
    }

    /// Lets the tasks Bridge spawned (posts, the status line, reaction flips) run.
    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    fn in_thread(ts: &str, text: &str) -> InboundMsg {
        let mut m = channel_msg(ts, "U_OWNER", text);
        m.thread_ts = Some(ROOT.into());
        m
    }

    /// Starts a thread at ROOT and lets its agent take the first turn. Returns the session id.
    async fn running_thread(b: &mut Bridge, agent: &FakeAgent) -> String {
        b.on_inbound(&channel_msg(ROOT, "U_OWNER", "<@U_BOT> fix the tests"))
            .await;
        let sid = agent.spawned.lock().unwrap()[0].session_id.as_str().to_string();
        on_hook_user_prompt_for_test(b, &sid).await;
        sid
    }

    #[tokio::test]
    async fn a_reply_in_a_running_thread_reaches_the_same_agent() {
        // Observed: once user_prompt clears the latch, the entry's session is Ready and
        // Dispatch::Deliver types the envelope into the window the spawn returned (@0).
        let (d, _slack, agent, _clock) = flow_deps("reply");
        let (mut b, _fx) = Bridge::for_test(d);
        running_thread(&mut b, &agent).await;
        b.on_inbound(&in_thread("1782000000.000200", "and also this"))
            .await;
        assert_eq!(agent.spawned.lock().unwrap().len(), 1, "no second agent");
        let delivered = agent.delivered.lock().unwrap().clone();
        assert_eq!(delivered.len(), 1, "{delivered:?}");
        assert_eq!(delivered[0].0, "@0");
        assert!(delivered[0].1.contains("and also this"), "{delivered:?}");
    }

    #[tokio::test]
    async fn stop_interrupts_the_running_agent() {
        // Observed: `stop` from the Owner with a request still unanswered sends Escape to the
        // worker's window id and posts nothing.
        let (d, slack, agent, _clock) = flow_deps("stop");
        let (mut b, _fx) = Bridge::for_test(d);
        running_thread(&mut b, &agent).await;
        settle().await;
        let before = slack.calls().len();
        b.on_inbound(&in_thread("1782000000.000200", "stop")).await;
        settle().await;
        assert_eq!(*agent.interrupted.lock().unwrap(), vec!["@0".to_string()]);
        assert!(
            !slack.calls()[before..].iter().any(|c| c.starts_with("post")),
            "{:?}",
            slack.calls()
        );
    }

    #[tokio::test]
    async fn a_tool_prompt_is_posted_and_answered_on_allow() {
        // Observed: a tool no standing rule covers (Bash) gets one prompt in the thread; Allow
        // answers the worker with "allow" and rewrites the prompt to ✅ (it is not deleted).
        let (d, slack, agent, _clock) = flow_deps("perm");
        let (mut b, _fx) = Bridge::for_test(d);
        let sid = running_thread(&mut b, &agent).await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        b.on_hook(HookEvent {
            kind: "perm".into(),
            session_id: sid,
            payload: serde_json::json!({
                "tool_name": "Bash",
                "tool_input": {"command": "cargo test"},
                "tool_use_id": "toolu_1",
            }),
            respond: Some(tx),
        })
        .await;
        let perms: Vec<String> = slack
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("perm "))
            .collect();
        assert_eq!(perms.len(), 1, "{:?}", slack.calls());
        let req_id = perms[0].split(' ').nth(3).unwrap().to_string();
        assert_eq!(perms[0], format!("perm C1 {ROOT} {req_id} Bash"));
        let prompt_ts = b.perm_pending[&req_id].prompt_ts.clone();

        b.on_perm_click(slack::PermClick {
            req_id: req_id.clone(),
            action: "allow".into(),
            by: "U_OWNER".into(),
        })
        .await;
        let answer = rx.await.unwrap();
        assert!(answer.to_string().contains("\"allow\""), "{answer}");
        assert!(b.perm_pending.is_empty());
        assert!(
            slack
                .calls()
                .contains(&format!("update C1 {prompt_ts} ✅ `Bash` — allow by <@U_OWNER>")),
            "{:?}",
            slack.calls()
        );
    }

    #[tokio::test]
    async fn silence_shows_the_thinking_status_once() {
        // Observed: the user_prompt hook arms the watchdog quietly; after SILENCE_MS with the
        // request unanswered, stall_tick sets "is thinking…" once and a second tick is a no-op.
        let (d, slack, agent, clock) = flow_deps("stall");
        let (mut b, _fx) = Bridge::for_test(d);
        running_thread(&mut b, &agent).await;
        clock.advance(slack::SILENCE_MS + 1);
        b.stall_tick();
        b.stall_tick();
        settle().await;
        let thinking = format!("status C1 {ROOT} {}", slack::THINKING_STATUS);
        let shown = slack.calls().iter().filter(|c| **c == thinking).count();
        assert_eq!(shown, 1, "{:?}", slack.calls());
    }

    #[tokio::test]
    async fn the_usage_limit_gate_refuses_a_new_thread() {
        // Observed: while limited_until_ms is ahead of the clock, a new request gets the
        // Limited notice in its thread — no ack reaction, no agent.
        let (d, slack, agent, clock) = flow_deps("limit");
        let (mut b, _fx) = Bridge::for_test(d);
        let until_ms = ports::Clock::now_ms(clock.as_ref()) + 3_600_000;
        b.limited_until_ms = until_ms;
        b.on_inbound(&channel_msg(ROOT, "U_OWNER", "<@U_BOT> fix the tests"))
            .await;
        settle().await;
        assert!(agent.spawned.lock().unwrap().is_empty());
        let notice = crate::bridge::render::Notice::Limited { until_ms }.render();
        assert_eq!(slack.calls(), vec![format!("post C1 {ROOT} {notice}")]);
    }
}
