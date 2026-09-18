//! 生きているワーカーと在庫の台帳。
//!
//! **このファイルは tmux コマンドを1つも打たない。** 「どのスレッドにどのワーカーが居るか」の
//! 記録だけを持つ。実際に窓を叩くのは [`crate::ports::AgentPort`] 越し。
//!
//! Slack へ投稿するもの・threads.json を書くものはここに置かない — あれは「台帳を見て
//! Slack と agent に指示を出す」ので Bridge の仕事。

use crate::agent::screen::SpawnOutcome;
use crate::agent::tmux::{self as tmux_mod, Pid, Window};
use crate::agent::{SessionId, SpawnReq};
use crate::bridge::{inbound, worker};
use crate::bridge::state as bridge;
use crate::bridge::state::LogCtx;
use crate::bridge::{Bridge, CmdFx, Host};
use crate::{mcp, slack};
use crate::ports::AgentPort;
use crate::bridge::inbound::WorkerState;
use crate::bridge::state::{PoolKey, ThreadEntry, ThreadKey};
use std::collections::{HashMap, HashSet};

/// 冷えたワーカーを畳む規則(2026-08-02 ユーザー決裁の4つの数字)。
/// 現行 Bun は環境変数で動かせるが、こちらは決め打ち — 動かしたくなったら足す。
const CLEANUP: CleanupPolicy = CleanupPolicy {
    idle_ttl_ms: 30 * 60_000,
    idle_slots: 5,
    idle_max_ms: 60 * 60_000,
    max_concurrent: 10,
};

/// 起動画面を見張る時間。現行の 30秒(同期)+ 120秒(linger)= 150秒に合わせた。
/// 現行の linger は `FIRST_PROMPT_TIMEOUT_MS`(60s、p99 45.6s)の2倍。
const SPAWN_SCREEN_BUDGET_MS: u64 = 150_000;

const SPAWN_SCREEN_POLL_MS: u64 = 1_000;

/// プール worker が MCP を上げるまでの猶予(worker.ts の `MCP_INIT_TIMEOUT_MS` と同値)。
/// これを超えたら諦める(= 実体ごと畳んで在庫から消す)。
const POOL_MCP_INIT_TIMEOUT_MS: u64 = 50_000;

/// 在庫を数え直す間隔。tick は 500ms 刻みなので、ここでスイープを間引く。
pub const POOL_SWEEP_INTERVAL_MS: u64 = 5_000;

/// 冷えたワーカーを畳む頃合いを見る間隔(`WORKER_CLEANUP_MS` と同値)。
pub const CLEANUP_INTERVAL_MS: u64 = 60_000;

/// ワーカー1本の暖機の印。hook が来るたびに更新される。
#[derive(Clone, Default, Debug)]
pub struct Hooked {
    pub ended: bool,
    /// MCP の initialize を観測したか(= disposition ツールを呼べる)。Stop の fail-open 判定に使う。
    pub mcp_ready: bool,
    pub window_id: Option<String>,
    /// ワーカーの .jsonl(hook payload が毎回運んでくる)と、そこまで読んだ位置。
    pub transcript_path: Option<String>,
    pub transcript_offset: u64,
}

/// 生死判定の材料。これだけで [`WorkerState`] が決まる。
pub struct WorkerFacts {
    pub pid: Option<u32>,
    /// **この Bridge が起こして、まだ最初のターンを踏んでいない**([`Workers::starting`])。
    pub starting: bool,
    pub ended: bool,
}

/// 在庫1本。cwd は起動時に固定される。
///
/// **窓と ready は持たない** — どちらも同じ session_id の [`Hooked`] が既に持っている
/// (`window_id` と `mcp_ready`)。在庫側にも置くと同じ事実の写しが2つになり、
/// 継承経路のように片方だけ埋まる状態が作れてしまう。引くのは [`Workers`] のメソッド。
pub struct PoolWorker {
    pub session_id: String,
    pub spawned_at_ms: u64,
    /// 起動時に固定した作業ディレクトリ(status の Warm Pool 節がそのまま出す)。
    pub cwd: String,
    /// 指名済みセッションの `--resume` で起こしたか。**猶予切れの扱いが変わる** —
    /// resume は「セッションが消えている/壊れている」で失敗しうるので、諦め札を立てる前に
    /// 指名を捨てて新規 ID で1回だけやり直す(`give_up_stale_pools`)。
    pub resumed: bool,
}

/// 生きているワーカーと在庫の台帳。tmux は叩かない — 叩くのは [`AgentPort`]。
#[derive(Default)]
pub struct Workers {
    /// session_id → 暖機の印
    hooked: HashMap<String, Hooked>,
    /// **この Bridge が起こして、まだ最初のターンを踏んでいない**セッション(bridge.ts の
    /// `awaitingPushReady` と同じ掛け金)。claude の TUI が立ち上がりきる前に send-keys すると
    /// 入力が消えるので、その数秒だけ配達を queue に回すためだけに存在する。
    ///
    /// **spawn した瞬間にだけ入れ、最初の user_prompt で外す。** hook から「起動した」を
    /// 推測しない — session_start は compact / clear でも飛んでくるので、そこで起動中に
    /// 戻すと、動いているワーカー宛の配達が queue に詰まったまま二度と流れない
    /// (queue を流す user_prompt は、その queue の中の依頼を渡さないと発火しない)。
    /// 2026-08-01 実機。前身の `started_here` は掛け金を裏返しに持っていて、この罠を作った。
    ///
    /// 永続化しない。Bridge を再起動したら誰も「起動中」ではない — 生きているワーカーは
    /// 前世代が起こしきったもの = そのまま渡してよい、が正しい。
    starting: HashSet<String>,
    /// `PoolKey::of_cwd(cwd)` → 在庫中のプール worker
    pools: HashMap<PoolKey, PoolWorker>,
    /// 諦めた pool_key。在庫から消しても「二度と起こさない」を覚えておくための**一方通行の札**
    /// (これが無いと次のスイープが同じ枠を spawn し直して無限リトライになる)
    gave_up: HashSet<PoolKey>,
    /// 返信待ちの終了予約。満期は tick が見る。
    drains: Vec<DrainJob>,
    /// 最後に在庫を数え直した時刻。
    last_sweep_ms: u64,
    /// 最後に冷えたワーカーを見に行った時刻。
    last_cleanup_ms: u64,
}

impl Workers {
    // ── 暖機の印 ────────────────────────────────────────────────────────────

    /// 起こした = 最初のターンまで配達を待たせる(掛け金の説明は [`Workers::starting`])。
    pub fn mark_starting(&mut self, session_id: &str) {
        self.starting.insert(session_id.to_string());
    }

    /// 最初のターンが来た = TUI がキーを取れている。以後は素通しで配達してよい。
    ///
    /// queue を流すのはここではなく呼び手(`flush_queued`)で、**毎ターン試す**。
    /// 掛け金が外れる1回に紐づけると、Bridge 再起動をまたいで pending.json に残った分を
    /// 拾う機会が無くなる(再起動後のワーカーは誰も掛け金に入っていない)。
    pub fn clear_starting(&mut self, session_id: &str) {
        self.starting.remove(session_id);
    }

    pub fn is_starting(&self, session_id: &str) -> bool {
        self.starting.contains(session_id)
    }

    pub fn warm(&self, session_id: &str) -> Option<&Hooked> {
        self.hooked.get(session_id)
    }

    /// 無ければ既定で作ってから返す(hook が最初に触った時点で席ができる)。
    pub fn warm_mut(&mut self, session_id: &str) -> &mut Hooked {
        self.hooked.entry(session_id.to_string()).or_default()
    }

    pub fn forget(&mut self, session_id: &str) {
        self.hooked.remove(session_id);
        self.starting.remove(session_id);
    }

    /// 配達先の窓 id。窓名は改名されうるので、覚えている `@N` を優先する。
    pub fn window_of(&self, session_id: &str) -> Option<String> {
        self.hooked
            .get(session_id)
            .and_then(|h| h.window_id.clone())
    }

    pub fn is_empty(&self) -> bool {
        self.hooked.is_empty()
    }

    /// 現在の transcript の読み位置たち `(session_id, path, offset)`。
    pub fn transcripts(&self) -> Vec<(String, String, u64)> {
        self.hooked
            .iter()
            .filter(|(_, h)| !h.ended)
            .filter_map(|(sid, h)| {
                h.transcript_path
                    .clone()
                    .map(|p| (sid.clone(), p, h.transcript_offset))
            })
            .collect()
    }

    /// このスレッドのワーカーの生死。**問い合わせるだけで、何も書き換えない** — 書き換えると
    /// 呼ばれた順番で答えが変わる(前身はここで「継承ワーカー」を検出して印を戻していたので、
    /// 呼ばれないまま session_start が先に来ると誤判定した。2026-08-01 実機)。
    ///
    /// 前の Bridge から継承したワーカーは掛け金に**入っていない**ので、推測なしで Ready。
    pub fn state_of(
        &self,
        entry: Option<&ThreadEntry>,
        window: &str,
        agent: &dyn AgentPort,
    ) -> WorkerState {
        let Some(sid) = entry.and_then(|e| e.agent_id.as_deref()) else {
            return WorkerState::Absent;
        };
        let h = self.hooked.get(sid);
        Self::state_from(&WorkerFacts {
            pid: agent
                .pid_of(h.and_then(|h| h.window_id.as_deref()), window)
                .map(|p| p.0),
            starting: self.is_starting(sid),
            ended: h.is_some_and(|h| h.ended),
        })
    }

    /// 生きているか。順序は優先度 — **終わったセッションは pid が残っていても居ない**
    /// (Starting にすると `Action::decide` が Queue を返し、respawn されないまま
    /// queue が永久に溜まる)。
    fn state_from(facts: &WorkerFacts) -> WorkerState {
        if facts.ended || facts.pid.is_none() {
            return WorkerState::Absent;
        }
        if facts.starting {
            WorkerState::Starting
        } else {
            WorkerState::Ready
        }
    }

    // ── 在庫 ────────────────────────────────────────────────────────────────

    pub fn pool(&self, pool_key: &PoolKey) -> Option<&PoolWorker> {
        self.pools.get(pool_key)
    }

    pub fn has_pool(&self, pool_key: &PoolKey) -> bool {
        self.pools.contains_key(pool_key)
    }

    pub fn insert_pool(&mut self, pool_key: PoolKey, p: PoolWorker) {
        self.pools.insert(pool_key, p);
    }

    pub fn remove_pool(&mut self, pool_key: &PoolKey) -> Option<PoolWorker> {
        self.pools.remove(pool_key)
    }

    /// 在庫を丸ごと引き取る(logout / restart の一斉畳み)。
    pub fn take_pools(&mut self) -> HashMap<PoolKey, PoolWorker> {
        std::mem::take(&mut self.pools)
    }

    /// 在庫が「使える」か。**MCP を握った瞬間がその唯一の証拠**なので、
    /// [`Hooked::mcp_ready`] をそのまま在庫の ready として読む。
    pub fn pool_ready(&self, session_id: &str) -> bool {
        self.hooked.get(session_id).is_some_and(|h| h.mcp_ready)
    }

    /// `(pool_key, session_id, spawned_at_ms, ready)` の一覧。期限判定と status 表示に使う。
    pub fn pool_rows(&self) -> Vec<(PoolKey, String, u64, bool)> {
        self.pools
            .iter()
            .map(|(k, p)| {
                (
                    k.clone(),
                    p.session_id.clone(),
                    p.spawned_at_ms,
                    self.pool_ready(&p.session_id),
                )
            })
            .collect()
    }

    /// 在庫の `(session_id, window_id)` 一覧。一斉畳みの pid 集めに使う。
    pub fn pool_pid_rows(&self) -> Vec<(String, Option<String>)> {
        self.pools
            .values()
            .map(|p| (p.session_id.clone(), self.window_of(&p.session_id)))
            .collect()
    }

    /// status の Warm Pool 節がそのまま出す `(cwd, ready)`。
    pub fn pool_summary(&self) -> Vec<(String, bool)> {
        self.pools
            .values()
            .map(|p| (p.cwd.clone(), self.pool_ready(&p.session_id)))
            .collect()
    }

    /// この session_id の在庫を登録簿から**落とすだけ**。実体は既に死んでいる前提なので
    /// 窓は畳まない。`gave_up` には**入れない** — 死は「諦めた」ではなく、次のスイープで
    /// 立て直してよい枠だから。落とせたら pool_key を返す。
    pub fn drop_pool_of_session(&mut self, session_id: &str) -> Option<PoolKey> {
        // ponytail: プールはせいぜい数個 — session_id からの逆引き表は要らない
        let key = self
            .pools
            .iter()
            .find(|(_, p)| p.session_id == session_id)
            .map(|(k, _)| k.clone())?;
        self.pools.remove(&key);
        Some(key)
    }

    // ── 諦めた枠の札 ─────────────────────────────────────────────────────────

    pub fn gave_up_on(&self, pool_key: &PoolKey) -> bool {
        self.gave_up.contains(pool_key)
    }

    pub fn mark_gave_up(&mut self, pool_key: PoolKey) {
        self.gave_up.insert(pool_key);
    }

    pub fn gave_up_count(&self) -> usize {
        self.gave_up.len()
    }

    /// 札を全部剥がす(access 再読込 = 設定が変わったので、諦めた判断も無効になる)。
    pub fn clear_gave_up(&mut self) {
        self.gave_up.clear();
    }

    // ── スイープの間引き ─────────────────────────────────────────────────────

    /// 在庫を数え直す頃合いなら true(その時点で時計を進める)。
    pub fn sweep_due(&mut self, now_ms: u64) -> bool {
        if now_ms.saturating_sub(self.last_sweep_ms) < POOL_SWEEP_INTERVAL_MS {
            return false;
        }
        self.last_sweep_ms = now_ms;
        true
    }

    /// 冷えたワーカーを見に行く頃合いなら true(その時点で時計を進める)。
    ///
    /// **起動直後の1回は必ず見送る**(現行も初回は間隔ぶん待つ)。掃除は窓を閉じる側なので、
    /// 在庫の起こし直しが済む前に走らせない — 生きている在庫が「持ち主なし」に見える。
    pub fn cleanup_due(&mut self, now_ms: u64) -> bool {
        if self.last_cleanup_ms == 0 {
            self.last_cleanup_ms = now_ms;
            return false;
        }
        if now_ms.saturating_sub(self.last_cleanup_ms) < CLEANUP_INTERVAL_MS {
            return false;
        }
        self.last_cleanup_ms = now_ms;
        true
    }

    // ── 終了予約 ────────────────────────────────────────────────────────────

    pub fn is_draining(&self, key: &ThreadKey) -> bool {
        self.drains.iter().any(|j| &j.key == key)
    }

    pub fn push_drain(&mut self, job: DrainJob) {
        self.drains.push(job);
    }

    /// 満期の来た予約の位置(未応答が空になったか、期限切れ)。
    pub fn due_drain(&self, now_ms: u64, is_settled: impl Fn(&ThreadKey) -> bool) -> Option<usize> {
        self.drains
            .iter()
            .position(|j| is_settled(&j.key) || now_ms >= j.deadline_ms)
    }

    pub fn take_drain(&mut self, i: usize) -> DrainJob {
        self.drains.remove(i)
    }
}

// ── 冷えたワーカーの回収 ────────────────────────────────────────────────────
//
// 現行 Bun の `selectCleanupKeys`と**同じ骨格・違う規則**。
// 現行は「上限を超えたときだけ、冷えたものを落とす」で、席が空いていれば何時間冷えていても
// 残す。こちらは「冷えたまま置ける本数」自体に枠を設ける(2026-08-02 ユーザー決裁)。

/// 判定の材料1本ぶん。**tmux も transcript もここには出てこない** — 呼び手が実物を見て
/// 数字にしてから渡す(この関数を純粋に保つ = テストが実機を要らない)。
pub struct IdleSnapshot {
    pub key: ThreadKey,
    /// 最後に**本当に働いた**時刻からの経過(= transcript の mtime との差)。
    pub idle_ms: u64,
    /// 落としてよいか。起動中 / 返事をまだ返していない / 人のクリック待ち /
    /// 既に終了予約が積まれている、はすべて false。
    /// **transcript が読めず idle が測れないものも false** — 判断材料が無いものは触らない。
    pub eligible: bool,
}

/// 何本まで・どれだけ冷えたら畳むか。
pub struct CleanupPolicy {
    /// これを**超えた**ら「冷えている」。
    pub idle_ttl_ms: u64,
    /// 冷えたまま置いておける本数。
    pub idle_slots: usize,
    /// これを**超えた**ら席の空きに関係なく畳む。
    pub idle_max_ms: u64,
    /// 同時に生かしておける本数(冷えていなくてもこれを超えたら畳む)。
    pub max_concurrent: usize,
}

/// 畳むべきスレッド。**規則は3つで、上から順に適用する**(先に落ちたものは後の分母から抜ける)。
///
/// 1. 放置の絶対上限を超えたもの — 席が余っていても落とす
/// 2. 冷えたもの(TTL 超え)が枠を超えた分 — 冷えている順に、枠ちょうどまで
/// 3. それでも同時上限を超えていたら — 冷えている順に、**冷えていなくても**上限まで
///
/// どの規則でも `eligible` でないものには手を出さない。落とせないものが多くて上限に
/// 戻れないときは、戻れないまま返す(次の回で改めて見る)。
pub fn select_cleanup_keys(snaps: Vec<IdleSnapshot>, p: &CleanupPolicy) -> Vec<ThreadKey> {
    let mut snaps = snaps;
    snaps.sort_by(|a, b| b.idle_ms.cmp(&a.idle_ms)); // 冷えている順。3つの規則すべてこの順で落とす
    let mut evict = Vec::new();

    // 1. 放置の絶対上限
    let mut kept: Vec<IdleSnapshot> = Vec::new();
    for s in snaps {
        if s.eligible && s.idle_ms > p.idle_max_ms {
            evict.push(s.key);
        } else {
            kept.push(s);
        }
    }

    // 2. 冷えたものの枠。冷えていないものは数にも入らない。**残すのは温かい方** —
    //    次に話しかけられる見込みが高いものから席を守る(現行 Bun も warm-preserving)
    let idle_now = kept.iter().filter(|s| s.idle_ms > p.idle_ttl_ms).count();
    let mut surplus = idle_now.saturating_sub(p.idle_slots);
    let mut kept2: Vec<IdleSnapshot> = Vec::new();
    for s in kept {
        if surplus > 0 && s.idle_ms > p.idle_ttl_ms && s.eligible {
            evict.push(s.key);
            surplus -= 1;
            continue;
        }
        kept2.push(s);
    }

    // 3. 同時上限。ここまで残ったものは冷えていない = 働いている最中かもしれないが、
    //    席が足りないので一番冷えたものから譲ってもらう
    let mut over = kept2.len().saturating_sub(p.max_concurrent);
    for s in kept2 {
        if over == 0 {
            break;
        }
        if s.eligible {
            evict.push(s.key);
            over -= 1;
        }
    }
    evict
}

/// exit / resume が積む「返信が着いてから殺す」予約。
///
/// 走っているターンは**切らない**(それは stop の仕事) —
/// 最後の返信が Slack に着くのを待ってからワーカーを終わらせる。
pub struct DrainJob {
    pub key: ThreadKey,
    pub session_id: String,
    pub deadline_ms: u64,
    /// 別れの挨拶を出す先 (channel, thread_ts)。exit だけが告げる —
    /// resume は報告文が「本セッションは終了します。」と言い終えている。
    pub farewell: Option<(String, String)>,
    /// ドレイン中ずっと出しておく shimmer(`resume` だけが預ける)。job を drains から
    /// 取り出した時点で Drop = クリア。待っているのはこの予約なので、guard もここに置く。
    pub thinking: Option<crate::slack::Thinking>,
}

// ── starting, pooling and reaping agents ──

impl Bridge {
    /// ワーカーを終わらせる本体(順序をそのまま)。
    /// **threads.json の entry は触らない** — セッションの紐付けが残るからこそ resume できる。
    pub(super) async fn terminate(
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

    pub(super) fn write_mcp(&self, sid: &str, ctx: &LogCtx) -> Option<String> {
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
    pub(super) async fn restore_pools(&mut self, ctx: &LogCtx) {
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

    /// 在庫が欠けているプールを起動する。owner が居ないうちは `pool_targets` が空なので何もしない。
    /// **`milestone()` は呼ばない** — プール worker はまだどのスレッドのものでもない。
    pub(super) fn start_missing_pool_workers(&mut self, ctx: &LogCtx) {
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

    /// 生きているワーカーを**全部**畳む。ドレインはしない — 走らせるターンには行き先が無い
    /// (teardownAllWorkers)。`logout`(認証が外れた)と
    /// `shutdown`(孤児を残さない)の両方から呼ぶ。`detail` は行末に足す補足で、無いときは空文字。
    pub(super) async fn teardown_all_workers(&mut self, why: &str, detail: &str, ctx: &LogCtx) {
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
    pub(super) async fn give_up_stale_pools(&mut self, ctx: &LogCtx) {
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
    pub(super) fn sweep_pools(&mut self, ctx: &LogCtx) {
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
    pub(super) async fn cleanup_workers(&mut self) {
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
    pub(super) fn safe_to_reap(owned: &HashSet<String>, rows: &[tmux_mod::WindowRow]) -> bool {
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
    pub(super) fn drop_pool_worker(&mut self, sid: &str, why: &str, ctx: &LogCtx) {
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
    pub(super) fn claim_pool_worker(&mut self, pool_key: &PoolKey) -> Option<worker::PoolWorker> {
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
    pub(super) fn assign_pool_worker(
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

    pub(super) fn spawn_worker(&mut self, req: &SpawnReq, key: &ThreadKey, ctx: &LogCtx, sid: &str) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude::Claude;
    use crate::agent::tmux::Tmux;

    // ── 冷えたワーカーの回収 ────────────────────────────────────────────────

    const MIN: u64 = 60_000;

    /// 実機の決裁値(30分 / 5本 / 60分 / 10本)。
    const P: CleanupPolicy = CleanupPolicy {
        idle_ttl_ms: 30 * MIN,
        idle_slots: 5,
        idle_max_ms: 60 * MIN,
        max_concurrent: 10,
    };

    /// `idle_min` 分だけ冷えた、落としてよいスレッド。
    fn snap(n: u32, idle_min: u64) -> IdleSnapshot {
        IdleSnapshot {
            key: ThreadKey::new("C1", &n.to_string()),
            idle_ms: idle_min * MIN,
            eligible: true,
        }
    }

    fn keys(v: Vec<ThreadKey>) -> Vec<String> {
        v.into_iter().map(|k| k.as_str().to_string()).collect()
    }

    /// 席が余っていて、冷えたものも枠の内なら何も畳まない。
    #[test]
    fn nothing_is_evicted_while_within_every_limit() {
        // 5本冷えている(枠ちょうど)+ 温かいのが4本 = 9本(上限10の内)
        let mut v: Vec<IdleSnapshot> = (0..5).map(|n| snap(n, 45)).collect();
        v.extend((5..9).map(|n| snap(n, 1)));
        assert!(select_cleanup_keys(v, &P).is_empty());
    }

    /// 放置の絶対上限は席の空きに関係しない — 1本だけでも畳む。
    #[test]
    fn a_thread_past_the_hard_idle_ceiling_goes_even_with_seats_to_spare() {
        assert_eq!(
            keys(select_cleanup_keys(vec![snap(1, 61)], &P)),
            ["C1:1"],
            "60分を超えたら、他に誰も居なくても畳む"
        );
        assert!(
            select_cleanup_keys(vec![snap(1, 60)], &P).is_empty(),
            "ちょうど60分は「超えて」いない"
        );
    }

    /// 冷えたものの枠(5本)を超えた分だけ、**冷えている順に**畳む。
    #[test]
    fn only_the_coldest_surplus_leaves_the_idle_slots() {
        // 冷えたのが7本(31〜37分)。枠は5なので2本落ちる — 落ちるのは冷えている方
        let v: Vec<IdleSnapshot> = (1..=7).map(|n| snap(n, 30 + n as u64)).collect();
        assert_eq!(keys(select_cleanup_keys(v, &P)), ["C1:7", "C1:6"]);
    }

    /// 同時上限を超えたら、冷えていなくても一番冷えたものから畳む。
    #[test]
    fn the_concurrency_cap_evicts_even_warm_threads() {
        // 12本すべて温かい(TTL 未満)= 冷えた枠は使っていない。上限10なので2本落ちる
        let v: Vec<IdleSnapshot> = (1..=12).map(|n| snap(n, n as u64)).collect();
        assert_eq!(keys(select_cleanup_keys(v, &P)), ["C1:12", "C1:11"]);
    }

    /// 触ってはいけないものは、どの規則でも畳まれない(上限に戻れなくても諦める)。
    #[test]
    fn ineligible_threads_are_never_evicted() {
        let ineligible = |n: u32, idle_min: u64| IdleSnapshot {
            eligible: false,
            ..snap(n, idle_min)
        };
        // 3時間放置でも、返事待ち・起動中・クリック待ちなら残る
        assert!(select_cleanup_keys(vec![ineligible(1, 180)], &P).is_empty());
        // 上限超過の巻き添えにもしない — 落ちるのは eligible な中で一番冷えたもの
        let mut v: Vec<IdleSnapshot> = (1..=10).map(|n| snap(n, n as u64)).collect();
        v.push(ineligible(99, 200));
        assert_eq!(keys(select_cleanup_keys(v, &P)), ["C1:10"]);
    }

    /// 窓は生きている(pane_pid が返る)fake tmux。
    fn live_agent() -> Claude {
        Claude::new(Tmux {
            run: Box::new(|args: &[&str]| {
                if args.first() == Some(&"list-windows") {
                    Ok("@7 4242 claude w-sid-1\n".to_string())
                } else {
                    Ok(String::new())
                }
            }),
        })
    }

    /// 窓が無い fake tmux。
    fn dead_agent() -> Claude {
        Claude::new(Tmux {
            run: Box::new(|_args: &[&str]| Ok(String::new())),
        })
    }

    fn entry_of(sid: &str) -> ThreadEntry {
        ThreadEntry {
            channel_id: Some("C1".into()),
            agent_id: Some(sid.into()),
            ..Default::default()
        }
    }

    /// 掛け金の一生: spawn で入り、最初のターンで外れる。**外れるのは1回だけ** —
    /// queue を流すのはその1回でよい。
    #[test]
    fn the_latch_goes_on_at_spawn_and_comes_off_at_the_first_turn() {
        let entry = entry_of("sid-1");
        let mut w = Workers::default();

        w.mark_starting("sid-1");
        assert_eq!(
            w.state_of(Some(&entry), "w-sid-1", &live_agent()),
            WorkerState::Starting,
            "起こした直後は TUI がキーを取れない — 配達は queue へ"
        );

        w.clear_starting("sid-1");
        assert!(!w.is_starting("sid-1"), "最初のターンで外れる");
        assert_eq!(
            w.state_of(Some(&entry), "w-sid-1", &live_agent()),
            WorkerState::Ready
        );
    }

    /// **前の Bridge から継承したワーカーは、推測なしで Ready。** 掛け金はこのプロセスが
    /// 起こしたときにしか入らないので、入っていない = 前世代が起こしきった = 渡してよい。
    #[test]
    fn a_worker_inherited_from_a_previous_bridge_is_ready_not_starting() {
        let entry = entry_of("sid-1");

        let w = Workers::default();
        assert_eq!(
            w.state_of(Some(&entry), "w-sid-1", &live_agent()),
            WorkerState::Ready
        );

        let gone = Workers::default();
        assert_eq!(
            gone.state_of(Some(&entry), "w-sid-1", &dead_agent()),
            WorkerState::Absent
        );
        assert!(gone.is_empty(), "死んだ窓に暖機印を付けてはいけない");
    }

    /// hook が席を触っても掛け金は動かない。**session_start は compact / clear でも飛ぶ**ので、
    /// あれで「起動中」に戻すと、動いているワーカー宛の配達が queue に詰まったまま
    /// 二度と流れない(2026-08-01 実機。前身は `started_here` をここで立てていた)。
    #[test]
    fn a_hook_touching_the_seat_never_puts_the_worker_back_into_starting() {
        let entry = entry_of("sid-1");
        let mut w = Workers::default();

        // compact 明けの session_start が通る道(ended を戻し、transcript を覚え直す)
        let h = w.warm_mut("sid-1");
        h.ended = false;
        h.transcript_path = Some("/tmp/sid-1.jsonl".into());

        assert_eq!(
            w.state_of(Some(&entry), "w-sid-1", &live_agent()),
            WorkerState::Ready,
            "動いているワーカーが Starting になると配達が queue で詰まる"
        );
    }

    #[test]
    fn sweep_is_throttled_to_the_interval() {
        let mut w = Workers::default();
        assert!(w.sweep_due(POOL_SWEEP_INTERVAL_MS), "初回は必ず走る");
        assert!(!w.sweep_due(POOL_SWEEP_INTERVAL_MS + 1), "間隔内は間引く");
        assert!(w.sweep_due(2 * POOL_SWEEP_INTERVAL_MS + 1));
    }
    /// 在庫の ready は在庫側に**持たない** — 同じ session の `mcp_ready` がそのまま出る。
    #[test]
    fn a_pool_worker_is_ready_exactly_when_its_session_took_mcp() {
        let mut w = Workers::default();
        let key = PoolKey::of_cwd("/repo/a");
        w.insert_pool(
            key.clone(),
            PoolWorker {
                session_id: "sid-1".into(),
                spawned_at_ms: 0,
                cwd: "/repo/a".into(),
                resumed: false,
            },
        );
        assert_eq!(w.pool_summary(), vec![("/repo/a".to_string(), false)]);
        assert!(!w.pool_rows()[0].3, "MCP 前は使えない");

        w.warm_mut("sid-1").mcp_ready = true;
        assert!(w.pool_ready("sid-1"), "MCP を握った = 在庫が使える");
        assert_eq!(w.pool_summary(), vec![("/repo/a".to_string(), true)]);
        assert!(w.pool_rows()[0].3);

        // 窓も同じ場所から引く(在庫側に写しを置かない)
        w.warm_mut("sid-1").window_id = Some("@7".into());
        assert_eq!(
            w.pool_pid_rows(),
            vec![("sid-1".to_string(), Some("@7".to_string()))]
        );
    }

    fn facts(pid: Option<u32>, starting: bool, ended: bool) -> WorkerFacts {
        WorkerFacts {
            pid,
            starting,
            ended,
        }
    }

    #[test]
    fn state_table() {
        assert_eq!(
            Workers::state_from(&facts(None, true, false)),
            WorkerState::Absent
        );
        assert_eq!(
            Workers::state_from(&facts(Some(1), true, false)),
            WorkerState::Starting
        );
        assert_eq!(
            Workers::state_from(&facts(Some(1), false, false)),
            WorkerState::Ready
        );
        // pid が消えたら、過去に働いていても居ない
        assert_eq!(
            Workers::state_from(&facts(None, false, false)),
            WorkerState::Absent
        );
        // session_end のあとは pid が残っていても居ない
        assert_eq!(
            Workers::state_from(&facts(Some(1), false, true)),
            WorkerState::Absent
        );
    }
}
