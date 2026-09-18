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
pub mod turn;
pub mod state;
pub mod worker;

use crate::agent::claude::HookIntake;
use crate::agent::claude::Claude;
use crate::agent::Envelope;
use crate::bridge::command::CmdFx;
use crate::bridge::render::RestartPhase;
use crate::bridge::state as bridge;
use crate::bridge::inbound::InboundMsg;
use crate::bridge::state::{LogCtx, ThreadKey};
use crate::bridge::turn::{PermPending, Stall};
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
    use crate::agent::HookEvent;
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
