//! 生きているワーカーと在庫の台帳。
//!
//! **このファイルは tmux コマンドを1つも打たない。** 「どのスレッドにどのワーカーが居るか」の
//! 記録だけを持つ。実際に窓を叩くのは [`crate::agent::Agent`] 越し。
//!
//! Slack へ投稿するもの・threads.json を書くものはここに置かない — あれは「台帳を見て
//! Slack と agent に指示を出す」ので Bridge の仕事。

use crate::agent::Agent;
use crate::bridge::state::{PoolKey, ThreadEntry, ThreadKey, WorkerState};
use std::collections::{HashMap, HashSet};

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

/// 生きているワーカーと在庫の台帳。tmux は叩かない — 叩くのは [`Agent`]。
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
        agent: &Agent,
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
    fn live_agent() -> Agent {
        Agent::Claude(Claude::new(Tmux {
            run: Box::new(|args: &[&str]| {
                if args.first() == Some(&"list-windows") {
                    Ok("@7 4242 claude w-sid-1\n".to_string())
                } else {
                    Ok(String::new())
                }
            }),
        }))
    }

    /// 窓が無い fake tmux。
    fn dead_agent() -> Agent {
        Agent::Claude(Claude::new(Tmux {
            run: Box::new(|_args: &[&str]| Ok(String::new())),
        }))
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
