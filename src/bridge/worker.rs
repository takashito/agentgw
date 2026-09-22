//! The ledger of live agents and the pool.
//!
//! **This file never runs a single tmux command.** It only records which agent lives in
//! which thread. Actually poking windows goes through [`crate::agent::Agent`].
//!
//! Nothing that posts to Slack or writes threads.json lives here — that is "read the
//! ledger, then instruct Slack and the agent", which is the Bridge's job.

use crate::agent::SpawnOutcome;
use crate::agent::{KILL_GRACE_MS, Pid, Window, WindowRow};
use crate::agent::{SessionId, SpawnReq};
use crate::bridge::worker;
use crate::bridge::state as bridge;
use crate::log::LogCtx;
use crate::bridge::{Bridge, CmdFx, Host};
use crate::chat::slack;
use crate::mcp;
use crate::agent::Agent;
use crate::agent::WorkerState;
use crate::bridge::state::{PoolKey, ThreadEntry};
use crate::chat::ThreadKey;
use std::collections::{HashMap, HashSet};

/// Rules for tearing down cold agents (four numbers the user decided on 2026-08-02; the
/// concurrency cap went 10 → 8 on 2026-09-22).
/// Hard-coded rather than read from env vars — add that when someone needs to tune them.
const CLEANUP: CleanupPolicy = CleanupPolicy {
    idle_ttl_ms: 30 * 60_000,
    idle_slots: 5,
    idle_max_ms: 60 * 60_000,
    max_concurrent: 8,
};

/// How long to watch the startup screen: 30s (sync) + 120s (linger) = 150s.
/// The linger is twice `FIRST_PROMPT_TIMEOUT_MS` (60s, p99 45.6s).
const SPAWN_SCREEN_BUDGET_MS: u64 = 150_000;

const SPAWN_SCREEN_POLL_MS: u64 = 1_000;

/// Grace period for a pool agent to bring up MCP (same value as `MCP_INIT_TIMEOUT_MS`).
/// Past this we give up (tear it down and remove it from the pool).
const POOL_MCP_INIT_TIMEOUT_MS: u64 = 50_000;

/// How often to recount the pool. The tick runs every 500ms, so this thins out the sweeps.
pub const POOL_SWEEP_INTERVAL_MS: u64 = 5_000;

/// How often to check for cold agents to tear down (same value as `WORKER_CLEANUP_MS`).
pub const CLEANUP_INTERVAL_MS: u64 = 60_000;

/// Warm-up markers for one agent. Updated on every hook.
#[derive(Clone, Default, Debug)]
pub struct Hooked {
    pub ended: bool,
    /// Whether we saw MCP initialize (= it can call the disposition tools). Used for Stop's fail-open decision.
    pub mcp_ready: bool,
    pub window_id: Option<String>,
    /// The agent's .jsonl (every hook payload carries it) and how far we have read it.
    pub transcript_path: Option<String>,
    pub transcript_offset: u64,
}

/// Inputs for the liveness check. These alone decide the [`WorkerState`].
pub struct WorkerFacts {
    pub pid: Option<u32>,
    /// **Started by this Bridge and has not reached its first turn yet** ([`Workers::starting`]).
    pub starting: bool,
    pub ended: bool,
}

/// One pool entry. Its cwd is fixed at startup.
///
/// **It holds no window and no ready flag** — the [`Hooked`] for the same session_id already
/// has both (`window_id` and `mcp_ready`). Keeping them here too would make two copies of one
/// fact, and paths like inheritance could fill in only one of them. Look them up through [`Workers`].
pub struct PoolWorker {
    pub session_id: String,
    pub spawned_at_ms: u64,
    /// Working directory fixed at startup (the status Warm Pool section prints it as is).
    pub cwd: String,
    /// Whether it was started with `--resume` of a reserved session. **This changes how the timeout
    /// is handled** — resume can fail because the session is gone or broken, so before marking
    /// the slot as given up we drop the reservation and retry once with a fresh ID (`give_up_stale_pools`).
    pub resumed: bool,
}

/// The ledger of live agents and the pool. It never touches tmux — [`Agent`] does.
#[derive(Default)]
pub struct Workers {
    /// session_id → warm-up markers
    hooked: HashMap<String, Hooked>,
    /// Sessions **started by this Bridge that have not reached their first turn yet** (the same
    /// latch as `awaitingPushReady`). send-keys before claude's TUI has fully come up loses the
    /// input, so this exists only to route deliveries to the queue for those few seconds.
    ///
    /// **Set only at the moment of spawn, cleared on the first user_prompt.** Never infer
    /// "started" from hooks — session_start also fires on compact / clear, and flipping back to
    /// starting there leaves deliveries to a running agent stuck in the queue forever
    /// (the user_prompt that flushes the queue only fires once a request from that queue is handed over).
    /// Seen on a real machine 2026-08-01. The predecessor, `started_here`, held the latch inverted and caused this trap.
    ///
    /// Not persisted. After a Bridge restart nobody is "starting" — live agents were fully
    /// started by the previous generation, so handing them work directly is correct.
    starting: HashSet<String>,
    /// `PoolKey::of_cwd(cwd)` → the pool agent in stock
    pools: HashMap<PoolKey, PoolWorker>,
    /// Pool keys we gave up on. A **one-way marker** that remembers "never start this again" after it
    /// leaves the pool (without it the next sweep respawns the same slot and retries forever)
    gave_up: HashSet<PoolKey>,
    /// Pending terminations waiting for replies. The tick checks when they are due.
    drains: Vec<DrainJob>,
    /// When the pool was last recounted.
    last_sweep_ms: u64,
    /// When we last checked for cold agents.
    last_cleanup_ms: u64,
}

impl Workers {
    // ── Warm-up markers ─────────────────────────────────────────────────────

    /// Just started = hold deliveries until the first turn (see [`Workers::starting`] for the latch).
    pub fn mark_starting(&mut self, session_id: &str) {
        self.starting.insert(session_id.to_string());
    }

    /// The first turn arrived = the TUI accepts keys. From now on deliver straight through.
    ///
    /// Flushing the queue is not done here but by the caller (`flush_queued`), and **it is tried every turn**.
    /// Tying it to the single latch release would lose the chance to pick up what stayed in
    /// pending.json across a Bridge restart (no agent is in the latch after a restart).
    pub fn clear_starting(&mut self, session_id: &str) {
        self.starting.remove(session_id);
    }

    pub fn is_starting(&self, session_id: &str) -> bool {
        self.starting.contains(session_id)
    }

    pub fn warm(&self, session_id: &str) -> Option<&Hooked> {
        self.hooked.get(session_id)
    }

    /// Creates a default entry if missing, then returns it (the seat exists from the first hook that touches it).
    pub fn warm_mut(&mut self, session_id: &str) -> &mut Hooked {
        self.hooked.entry(session_id.to_string()).or_default()
    }

    pub fn forget(&mut self, session_id: &str) {
        self.hooked.remove(session_id);
        self.starting.remove(session_id);
    }

    /// Window id to deliver to. Window names can be renamed, so prefer the remembered `@N`.
    pub fn window_of(&self, session_id: &str) -> Option<String> {
        self.hooked
            .get(session_id)
            .and_then(|h| h.window_id.clone())
    }

    pub fn is_empty(&self) -> bool {
        self.hooked.is_empty()
    }

    /// Current transcript read positions `(session_id, path, offset)`.
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

    /// Liveness of this thread's agent. **It only queries and never writes** — writing would make
    /// the answer depend on call order (the predecessor detected "inherited agents" here and reset
    /// markers, so it misjudged when session_start arrived before any call. Real machine, 2026-08-01).
    ///
    /// Agents inherited from the previous Bridge are **not** in the latch, so they are Ready without guessing.
    pub fn state_of(
        &self,
        entry: Option<&ThreadEntry>,
        window: &str,
        agent: &dyn Agent,
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

    /// Whether it is alive. The order is the priority — **an ended session is gone even if its pid remains**
    /// (making it Starting makes `Action::decide` return Queue, and the queue grows forever
    /// without a respawn).
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

    // ── Pool ─────────────────────────────────────────────────────────────────

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

    /// Takes over the whole pool (bulk teardown on logout / restart).
    pub fn take_pools(&mut self) -> HashMap<PoolKey, PoolWorker> {
        std::mem::take(&mut self.pools)
    }

    /// Whether a pool entry is usable. **Grabbing MCP is the only proof of that**, so
    /// [`Hooked::mcp_ready`] is read directly as the pool's ready flag.
    pub fn pool_ready(&self, session_id: &str) -> bool {
        self.hooked.get(session_id).is_some_and(|h| h.mcp_ready)
    }

    /// List of `(pool_key, session_id, spawned_at_ms, ready)`. Used for timeout checks and status.
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

    /// List of pool `(session_id, window_id)`. Used to collect pids for bulk teardown.
    pub fn pool_pid_rows(&self) -> Vec<(String, Option<String>)> {
        self.pools
            .values()
            .map(|p| (p.session_id.clone(), self.window_of(&p.session_id)))
            .collect()
    }

    /// `(cwd, ready)` printed as is by the status Warm Pool section.
    ///每 pool slot as (cwd, session_id, ready). `status` needs the session to say how long it has waited.
    pub fn pool_entries(&self) -> Vec<(String, String, bool)> {
        self.pools
            .values()
            .map(|p| (p.cwd.clone(), p.session_id.clone(), self.pool_ready(&p.session_id)))
            .collect()
    }

    pub fn pool_summary(&self) -> Vec<(String, bool)> {
        self.pools
            .values()
            .map(|p| (p.cwd.clone(), self.pool_ready(&p.session_id)))
            .collect()
    }

    /// **Only drops** this session_id's pool entry from the registry. The process is assumed to be
    /// dead already, so the window is not closed. It is **not** added to `gave_up` — dying is not
    /// "giving up"; the next sweep may rebuild the slot. Returns the pool_key if something was dropped.
    pub fn drop_pool_of_session(&mut self, session_id: &str) -> Option<PoolKey> {
        // ponytail: a pool holds a few entries at most — no reverse index from session_id needed
        let key = self
            .pools
            .iter()
            .find(|(_, p)| p.session_id == session_id)
            .map(|(k, _)| k.clone())?;
        self.pools.remove(&key);
        Some(key)
    }

    // ── Given-up slot markers ─────────────────────────────────────────────────

    pub fn gave_up_on(&self, pool_key: &PoolKey) -> bool {
        self.gave_up.contains(pool_key)
    }

    pub fn mark_gave_up(&mut self, pool_key: PoolKey) {
        self.gave_up.insert(pool_key);
    }

    pub fn gave_up_count(&self) -> usize {
        self.gave_up.len()
    }

    /// Clears all markers (access reload = settings changed, so earlier give-ups no longer apply).
    pub fn clear_gave_up(&mut self) {
        self.gave_up.clear();
    }

    // ── Sweep throttling ─────────────────────────────────────────────────────

    /// True when it is time to recount the pool (and advances the clock at that point).
    pub fn sweep_due(&mut self, now_ms: u64) -> bool {
        if now_ms.saturating_sub(self.last_sweep_ms) < POOL_SWEEP_INTERVAL_MS {
            return false;
        }
        self.last_sweep_ms = now_ms;
        true
    }

    /// True when it is time to check for cold agents (and advances the clock at that point).
    ///
    /// **Always skips the first round right after startup** (the first run waits one interval). Cleanup
    /// closes windows, so it must not run before the pool is re-adopted — live pool agents would look ownerless.
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

    // ── Pending terminations ─────────────────────────────────────────────────

    pub fn is_draining(&self, key: &ThreadKey) -> bool {
        self.drains.iter().any(|j| &j.key == key)
    }

    pub fn push_drain(&mut self, job: DrainJob) {
        self.drains.push(job);
    }

    /// Positions of reservations that are due (no unanswered messages left, or expired).
    pub fn due_drain(&self, now_ms: u64, is_settled: impl Fn(&ThreadKey) -> bool) -> Option<usize> {
        self.drains
            .iter()
            .position(|j| is_settled(&j.key) || now_ms >= j.deadline_ms)
    }

    pub fn take_drain(&mut self, i: usize) -> DrainJob {
        self.drains.remove(i)
    }
}

// ── Reclaiming cold agents ───────────────────────────────────────────────────
//
// The other approach only drops cold agents once the cap is exceeded, keeping them however
// long they sit cold while seats are free. Here the number of agents allowed to sit cold
// has its own cap (user decision, 2026-08-02).

/// Input for one decision. **Neither tmux nor transcripts appear here** — the caller looks at the
/// real things and turns them into numbers first (keeping this function pure = tests need no real machine).
pub struct IdleSnapshot {
    pub key: ThreadKey,
    /// Time since it last **actually worked** (= difference from the transcript's mtime).
    pub idle_ms: u64,
    /// Whether it may be dropped. False while starting / not yet replied / waiting for a human click /
    /// already having a pending termination.
    /// **Also false when the transcript cannot be read and idle cannot be measured** — never touch what we cannot judge.
    pub eligible: bool,
}

/// How many to keep, and how cold before tearing down.
pub struct CleanupPolicy {
    /// **Beyond** this, it counts as "cold".
    pub idle_ttl_ms: u64,
    /// How many may stay cold.
    pub idle_slots: usize,
    /// **Beyond** this, tear down regardless of free seats.
    pub idle_max_ms: u64,
    /// How many may live at once (beyond this, tear down even if not cold).
    pub max_concurrent: usize,
}

/// Threads to tear down. **Three rules, applied top to bottom** (whatever an earlier rule drops leaves the later counts).
///
/// 1. Past the absolute idle limit — drop even if seats are free
/// 2. Cold ones (past TTL) beyond the cold cap — coldest first, down to exactly the cap
/// 3. If still above the concurrency cap — coldest first, **even if not cold**, down to the cap
///
/// No rule touches anything that is not `eligible`. If too many cannot be dropped to get back
/// under the cap, return as is (the next round looks again).
pub fn select_cleanup_keys(snaps: Vec<IdleSnapshot>, p: &CleanupPolicy) -> Vec<ThreadKey> {
    let mut snaps = snaps;
    snaps.sort_by(|a, b| b.idle_ms.cmp(&a.idle_ms)); // coldest first; all three rules drop in this order
    let mut evict = Vec::new();

    // 1. Absolute idle limit
    let mut kept: Vec<IdleSnapshot> = Vec::new();
    for s in snaps {
        if s.eligible && s.idle_ms > p.idle_max_ms {
            evict.push(s.key);
        } else {
            kept.push(s);
        }
    }

    // 2. Cold cap. Agents that are not cold do not count. **Keep the warmer ones** —
    //    protect the seats most likely to be spoken to next (warm-preserving)
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

    // 3. Concurrency cap. What remains is not cold = may be mid-work, but there
    //    are not enough seats, so the coldest gives way first
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

/// A "kill after the reply lands" reservation queued by exit / resume.
///
/// A running turn is **not cut** (that is stop's job) —
/// wait for the last reply to reach Slack, then end the agent.
pub struct DrainJob {
    pub key: ThreadKey,
    pub session_id: String,
    pub deadline_ms: u64,
    /// Where to post the farewell (channel, thread_ts). Only exit announces it —
    /// for resume the report text already says the session is ending.
    pub farewell: Option<(String, String)>,
    /// Shimmer shown for the whole drain (only `resume` hands one over). Dropped = cleared when
    /// the job is taken out of drains. This reservation is what we wait on, so the guard lives here too.
    pub thinking: Option<crate::chat::slack::Thinking>,
}

// ── starting, pooling and reaping agents ──

impl Bridge {
    /// The body that ends the agent (keep the order as is).
    /// **Does not touch the threads.json entry** — the session binding staying is what makes resume possible.
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
        // 1. Drop unanswered messages **before** kill. Kill with them left and the agent-reclaim path
        // that sees the disconnect reads it as "lost a reply" and cold-spawns a replacement
        self.ledger.dispose_all(key);
        let (_, root) = key.split();
        let root_ts = root.unwrap_or_default();
        // 2. Kill the real process by pid, then close the window. Closing only the window without a pid
        // leaves claude alive as an orphan (F5)
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
                    pid.kill_graceful(KILL_GRACE_MS).await;
                }
                None => ctx.info(
                    "bridge",
                    &format!(
                        "exit: no live worker process for session={sid} — closing window only \
                         key={key}"
                    ),
                ),
            }
            // There is a path that arrives by window name (inherited agents do not remember window_id). Passing
            // a bare name to -t makes tmux target **the current session** — always qualify with the session
            let target = Window::of(window_id.as_deref().unwrap_or(&name));
            if let Err(e) = self.deps.agent.terminate(&target) {
                // **Killing the process usually closes the window with it**, so "can't find window"
                // here is the normal ending, not a fault. Logged as one, it made every clean
                // teardown look like a failure
                let gone = e.contains("can't find window");
                let line = format!("exit: kill-window failed: {e}");
                match gone {
                    true => ctx.debug("bridge", &line),
                    false => ctx.error("bridge", &line),
                }
            }
            // 3. Drop only the session-key memory (threads.json intact → the next message uses --resume)
            self.workers.forget(sid);
        }
        // 4. Discard what was waiting. Keeping it would feed old messages nobody waits for
        // into the next agent's user_prompt
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
                crate::t!("Thanks for your work! Ending the Claude Code session.", "お疲れさまでした。Claude Code のセッションを終了します。"),
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

    /// Re-adopts the pool the previous Bridge left behind (once at startup). restart exits without
    /// tearing down the pool, so warm processes are still in tmux. This revives them as the pool.
    ///
    /// Same reasoning as inheriting thread agents: **alive = MCP initialize already happened under the
    /// previous process**, so the marker is set again. Without it the `claim_pool_worker` condition
    /// (`not in the latch && mcp_ready && !ended`) is never met, and the pool entry sits there
    /// without ever being claimed (as for the latch, inherited agents were never in it).
    ///
    /// Dead reservations are left alone — [`Self::start_missing_pool_workers`], called right after,
    /// starts the same session_id with `--resume`.
    pub(super) async fn restore_pools(&mut self, ctx: &LogCtx) {
        let targets: Vec<String> = self.access.pool_targets(&Host::home(), &self.machine_name, self.link.is_none());
        let mut dirty = false;
        for (cwd, sid) in self.pools.rows() {
            let name = SessionId::from(sid.clone()).window_name();
            // Like inherited agents it does not remember window_id — look up by window name (automatic-rename
            // is turned off right after spawn, so the name stays)
            let alive = self.deps.agent.pid_of(None, &name).is_some();
            let key = bridge::PoolKey::of_cwd(&cwd);
            let ctx = LogCtx {
                session_id: Some(sid.clone()),
                thread_key: None,
            };
            match bridge::PoolRestore::decide(targets.contains(&cwd), alive) {
                // A cwd no longer in the pool targets (settings changed). Keeping it leaves a pool entry nobody claims
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
                // Keep the reservation — start_missing_pool_workers right after starts it with `--resume`
                bridge::PoolRestore::Respawn => continue,
                bridge::PoolRestore::Adopt => {}
            }
            ctx.info(
                "bridge",
                &format!(
                    "pool: inherited a live worker from a previous bridge process (key={key})"
                ),
            );
            // An agent fully started by the previous generation = not put in the latch (claimable as is).
            // Only MCP gets its marker back — initialize happened under the previous process and will not come again
            self.workers.warm_mut(&sid).mcp_ready = true;
            self.workers.insert_pool(
                key,
                worker::PoolWorker {
                    session_id: sid,
                    // The grace period restarts from this process (do not abandon it by the old clock)
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

    /// Starts pools that are missing entries. While there is no owner, `pool_targets` is empty so this does nothing.
    /// **Does not call `milestone()`** — a pool agent does not belong to any thread yet.
    pub(super) fn start_missing_pool_workers(&mut self, ctx: &LogCtx) {
        let targets = self.access.pool_targets(&Host::home(), &self.machine_name, self.link.is_none());
        // Converge **downward too**. Pool entries for slots no longer targeted sit idle in seats
        // nobody wants. Without this, `warm off` frees nothing until those entries die.
        // Claimed agents have already left the pool roster, so this never stops someone's work
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
        // While at the cap, **only stop starting new ones** (releasing is always safe and was done above)
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
            // A warm agent for a folder that isn't there would sit in the home directory instead
            if !self.deps.agent.workdir_exists(&cwd) {
                ctx.debug("bridge", &format!("pool: {cwd} is missing — not warming an agent (key={key})"));
                continue;
            }
            // Given-up slots have been removed from the pool — looking only at "missing" would respawn them,
            // so also check the one-way marker (`gave_up_pool_keys`)
            let status = Some(bridge::PoolStatus {
                present: self.workers.has_pool(&key),
                gave_up: self.workers.gave_up_on(&key),
            });
            if !bridge::PoolStatus::needs_launch(status) {
                continue;
            }
            // If a session is reserved, **resume it** — cutting a new ID would pile up a throwaway
            // session in claude's history every time the pool is rebuilt.
            // **But only while a conversation exists.** A pool agent that died before its first idle
            // prompt has no saved conversation, and resume exits at once ("No conversation found"). Keeping
            // the reservation loops exit → same resume 5s later, forever (found 2026-09-18; it had run on
            // one machine since August 2). Starting fresh re-reserves below
            let nominated = self
                .pools
                .session_of(&cwd)
                .map(str::to_string)
                .filter(|sid| {
                    let resumable = self.deps.agent.session_history_exists(None, sid);
                    if !resumable {
                        ctx.info(
                            "bridge",
                            &format!(
                                "pool: nominated session {sid} has no conversation to resume — \
                                 starting a new one (key={key})"
                            ),
                        );
                    }
                    resumable
                });
            let sid = nominated
                .clone()
                .unwrap_or_else(|| SessionId::new().as_str().to_string());
            let how = if nominated.is_some() {
                "resuming"
            } else {
                "launching"
            };
            ctx.info("bridge", &format!("pool: {how} {} (key={key})", cwd));
            // From here on only "which agent" matters. There is no thread yet, so thread_key is None
            let ctx = LogCtx {
                session_id: Some(sid.clone()),
                thread_key: None,
            };
            let Some(mcp) = self.write_mcp(&sid, &ctx) else {
                continue;
            };
            // Pool and assigned agents follow the same window-name rule — no rename on claim
            let window_id = match self.deps.agent.spawn(&SpawnReq {
                session_id: sid.clone().into(),
                cwd: cwd.clone(),
                // No body waiting for delivery = start on the agent's idle prompt
                prompt: None,
                resume_from: nominated.clone().map(SessionId::from),
                window: SessionId::from(sid.clone()).window_name(),
                // Reaching here means no pool entry = no window either
                state: crate::agent::WorkerState::Absent,
                hooks_file: self.hooks_file.clone(),
                mcp_config: mcp,
            }) {
                Ok(window) => {
                    self.start_spawn_screen_watch(&window, format!("pool session={sid}"), None, &sid, &ctx);
                    Some(window.as_str().to_string())
                }
                Err(e) => {
                    ctx.error("bridge", &format!("pool spawn failed: {e}"));
                    continue;
                }
            };
            // Reserve a freshly cut ID here (so the next startup can pick it up with resume)
            if nominated.is_none() {
                self.pools.nominate(&cwd, &sid);
                self.save_pools(&ctx);
            }
            // Put the window in the same place as thread agents — no move needed on claim
            self.workers.warm_mut(&sid).window_id = window_id;
            // A pool agent is starting until it passes its idle prompt once (then it becomes claimable)
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

    /// Writes the reservation table (pools.json). Keeps running on failure — all that is lost is
    /// "resume on next startup", in which case the pool comes up with fresh IDs.
    fn save_pools(&self, ctx: &LogCtx) {
        if let Err(e) = self.pools.save() {
            ctx.error("bridge", &format!("pools.json save failed: {e}"));
        }
    }

    /// Tears down one pool entry with its process. Window name is `w-<session_id>`; kill by pid → close the window,
    /// in the same order as `terminate`. `why` is the context word at the head of the log (logout / restart / pool give-up).
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
                pid.kill_graceful(KILL_GRACE_MS).await;
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

    /// Tears down **all** live agents. No drain — the turns they would run have nowhere to go.
    /// Called from both `logout` (auth was lost) and
    /// `shutdown` (leave no orphans). `detail` is extra text appended to the line; empty when none.
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
        // Collect the pool pids too (the ones teardown_pools below tears down)
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
        // **SIGTERM everyone first.** The teardown below waits one at a time, so without firing first
        // the grace periods (worst case TERM 1.5s + KILL 1.5s each) add up N times, the 5s shutdown
        // backstop cuts in and survivors remain (it really left 4 behind on a real machine). Firing
        // first lets the grace periods **overlap**. Parallelizing teardown itself would solve the same problem
        // ponytail: the SIGKILL wait stays serial. claude exits on SIGTERM, so in practice this is enough.
        //           If it stops being enough, parallelize terminate itself with a JoinSet
        for (_, _, pid) in &live {
            pid.term();
        }
        for pid in &pool_pids {
            pid.term();
        }
        // Unlike run_drains' "one per tick", kill them all serially here — the ones kept
        // waiting are the very agents being killed, so breaking the Stop contract's 5s window
        // hurts nobody (that agent will not answer anymore)
        for (key, sid, _) in &live {
            self.terminate(key, Some(sid.as_str()), None).await;
        }
        // The pool is not covered by `terminate` (it does not belong to any thread yet)
        self.teardown_pools(why).await;
    }

    /// Empties the pool and tears everything down. From `logout` (auth was lost) and `shutdown` (leave no orphans).
    /// **Not called from restart** — restart exits with the pool alive for the successor to inherit.
    ///
    /// Only the processes go; the **reservations** in pools.json stay. The next startup starts the same
    /// session_id with `--resume`, so sessions do not multiply across stops.
    async fn teardown_pools(&mut self, why: &str) {
        for (key, p) in self.workers.take_pools() {
            self.teardown_pool(&key, &p, why).await;
        }
    }

    /// Gives up on pool entries that passed the grace period without bringing up MCP:
    /// record the key in `gave_up_pool_keys` and **tear down the process and remove it from the pool**.
    /// The record stops respawns; the removal keeps status from counting a pool entry that does not exist
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
            // A pool agent started with resume that never warms up most likely has a gone/broken session.
            // Setting the give-up marker here would mean that pool is never built again because of a stale reservation.
            // Drop only the reservation and bet once more on a fresh ID (if that fails too, the marker below is set)
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

    /// Recounts the pool periodically. Dead entries are dropped from the registry by `session_end` and by the
    /// liveness check at claim time, so this only "fills empty slots" (converging on the missing target keys).
    /// **Throttling is required** — the tick runs every 500ms, and unthrottled it would query tmux every time.
    pub(super) fn sweep_pools(&mut self, ctx: &LogCtx) {
        let now = self.deps.clock.now_ms();
        if !self.workers.sweep_due(now) {
            return;
        }
        self.start_missing_pool_workers(ctx);
    }

    /// Tears down cold agents. Once every 60 seconds.
    ///
    /// Only **agents bound to a thread** are considered. The pool is not in `threads.json`, so it
    /// never shows up here (tearing it down is `sweep_pools`' job).
    ///
    /// **Does not touch threads.json** — the session binding stays, so when the next message comes to
    /// a torn-down thread it wakes up again with `--resume`. From a human's view nothing happened.
    ///
    /// ponytail: tears down only one per round. kill can stall the main loop up to 3s, so
    /// for the same reason as `run_drains` never two in one tick. If one per 60s is too slow,
    /// move the kill into a spawned task.
    pub(super) async fn cleanup_workers(&mut self) {
        if !self.workers.cleanup_due(self.deps.clock.now_ms()) {
            return;
        }
        self.reap_stray_windows().await;
        self.evict_idle_worker().await;
    }

    /// Closes windows that belong to nobody (empty and orphaned windows).
    ///
    /// **The tmux window list is the authority.** The Bridge's memory is lost on restart but windows stay, so
    /// the owner is looked up again from the `w-<session_id>` baked into the window name. No owner = nobody can deliver to it and nobody tears it down.
    ///
    /// Left alone: windows that are not agent windows (the anchor window, windows a human opened) and those **starting up**
    /// (`starting`). spawn sets the latch right after creating the window, inside the same function, so there is
    /// no gap where a just-started agent looks "owned by nobody yet".
    /// Whether this round may clean up: **if agent windows exist while nobody is assigned**, suspect the side
    /// that lost its memory (not the windows). If there are no windows, cleaning does nothing, so let it through.
    pub(super) fn safe_to_reap(owned: &HashSet<String>, rows: &[WindowRow]) -> bool {
        !owned.is_empty() || !rows.iter().any(|r| r.session_id().is_some())
    }

    async fn reap_stray_windows(&mut self) {
        // session_ids held by threads and the pool = windows that have an owner
        let owned: HashSet<String> = self
            .threads
            .entries
            .values()
            .filter_map(|e| e.agent_id.clone())
            .chain(self.workers.pool_pid_rows().into_iter().map(|(sid, _)| sid))
            // pools.json **reservations** count as owners too. Even before the pool entry is up,
            // a reserved session's window is "a pool entry about to be claimed", not litter
            .chain(self.pools.sessions().map(str::to_string))
            .collect();
        let rows = self.deps.agent.windows();
        // **Agent windows exist while nobody owns anything = not garbage windows, but we lost
        // track of assignments** (threads.json could not be read, etc.). Falling through to the check below
        // would make "every window ownerless" = close them all. On 2026-08-03 this path closed
        // 3 running agents on a real machine. **Do nothing this round** — real garbage windows get
        // closed normally in a round where the assignment table is healthy (every 60s).
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
                continue; // not an agent window
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
            // If it is not an empty shell, claude may be alive. Closing only the window leaves it
            // as an orphan, so **kill by pid first**, then close (same order as terminate)
            row.pid.kill_graceful(KILL_GRACE_MS).await;
            if let Err(e) = self.deps.agent.terminate(&Window::of(&row.id)) {
                ctx.debug("bridge", &format!("cleanup: kill-window failed: {e}"));
            }
            self.workers.forget(&sid);
            return; // one per round (do not stall the main loop too long on kill waits)
        }
    }

    /// Drops thread records whose agent can never come back: **no transcript on disk means
    /// `--resume` has nothing to resume**, which is the only reason the record outlives its window.
    ///
    /// Run once at start-up, because that is when a record can have gone stale unnoticed. Measured on
    /// a real machine 2026-09-22: 78 records, 21 resumable, so 57 rows nobody could ever use.
    ///
    /// **A record with a live window is never dropped**, whatever the disk says: an agent that has
    /// not taken its first turn has no transcript yet, and forgetting it would strand every delivery.
    pub(super) fn forget_unresumable_threads(&mut self, ctx: &LogCtx) {
        let before = self.threads.entries.len();
        let agent = self.deps.agent.clone();
        self.threads.entries.retain(|_, e| {
            let Some(sid) = e.agent_id.as_deref() else {
                return true; // no agent yet — the thread is waiting for one
            };
            let window = SessionId::from(sid.to_string()).window_name();
            agent.session_history_exists(None, sid) || agent.pid_of(None, &window).is_some()
        });
        let dropped = before - self.threads.entries.len();
        if dropped == 0 {
            return;
        }
        ctx.info(
            "bridge",
            &format!(
                "start-up: dropped {dropped} thread record(s) with no history left to resume \
                 ({} kept)",
                self.threads.entries.len()
            ),
        );
        if let Err(e) = self.threads.save() {
            ctx.error("bridge", &format!("threads.json save failed: {e}"));
        }
    }

    /// Tears down one cold thread agent.
    async fn evict_idle_worker(&mut self) {
        let now = self.deps.clock.now_ms();
        let mut snaps: Vec<worker::IdleSnapshot> = Vec::new();
        let mut sid_of: HashMap<ThreadKey, String> = HashMap::new();
        // Threads waiting for a human click (perm_pending is keyed by reqId, so convert to the thread key)
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
            // The tmux pid is the only answer for liveness (the real thing, not memory — same approach as status)
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
            // Idle is measured from the transcript's mtime = when it last **actually worked**. Broader than
            // narration or reply times; it also updates while thinking silently or running a long tool
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
        // One, coldest first (select stacks them coldest first)
        let key = evict.remove(0);
        let sid = sid_of.get(&key).cloned();
        // **Log the teardown reason to that thread's log** (the caller's ctx is plain, so it would go to
        // plugin-debug.log and get buried). Anyone later chasing "the agent is gone" first opens
        // the by-thread bridge.log, where the following `exit: terminating …` also appears
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

    /// **Only drops** this session_id's pool entry from the registry (called from the per-session
    /// cleanup). The process is assumed to be dead already, so the window is not closed.
    /// It is **not** added to `gave_up_pool_keys` — dying is not "giving up";
    /// the next sweep may rebuild the slot.
    ///
    /// The pools.json **reservation stays too**. We want to rebuild with `--resume` of the same session_id
    /// (cutting a new ID would add a session every time a pool agent dies). The escape hatch for a session
    /// too broken to resume lives in `give_up_stale_pools`.
    pub(super) fn drop_pool_worker(&mut self, sid: &str, why: &str, ctx: &LogCtx) {
        let Some(key) = self.workers.drop_pool_of_session(sid) else {
            return;
        };
        ctx.info(
            "bridge",
            &format!("pool: worker session={sid} {why} — dropped from the pool (key={key})"),
        );
    }

    /// Takes one agent out of the pool. Only **push-ready** ones qualify
    /// (mcp_initialized ∧ first user_prompt ∧ not ended), and only if **the real process is alive**.
    /// Dead pool entries are not claimed and are removed from the registry — kept, they are never refilled, and the next new
    /// thread claims the corpse again and loses the delivery with it (measured in E2E).
    /// Once taken it is no longer in the pool — refilling is left to the next `start_missing_pool_workers`.
    pub(super) fn claim_pool_worker(&mut self, pool_key: &PoolKey) -> Option<worker::PoolWorker> {
        let p = self.workers.pool(pool_key)?;
        let sid = p.session_id.clone();
        let push_ready = !self.workers.is_starting(&sid)
            && self
                .workers
                .warm(&sid)
                .is_some_and(|h| h.mcp_ready && !h.ended);
        if !push_ready {
            // Maybe it just has not warmed up yet — leave it alone (give-up watches the deadline)
            return None;
        }
        // Without a connector, the claude pid in tmux is the only proof of life
        // (a pool agent killed by kill -9, which sends no session_end, can only be noticed here)
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

    /// Binds the claimed agent to the thread, delivers the envelope, and refills what was taken.
    /// The window is reused — only the thread key is attached afterward.
    /// **Does not call `milestone()`** — the pool already emitted spawn / mcp_initialized.
    /// This single `pool: assigned` line is the only record of the assignment.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn assign_pool_worker(
        &mut self,
        claimed: worker::PoolWorker,
        root_ts: &str,
        channel: &str,
        cwd: &str,
        topic: &str,
        message_id: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) -> Result<(), String> {
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
        // Graduating from the pool — this session belongs to this thread from now on. Without dropping the reservation,
        // the refill would start the thread's session a second time with `--resume`
        self.pools.release(&claimed.cwd);
        self.save_pools(&ctx);

        // Not put in the latch — being claimable already required the pool agent to pass its first turn.
        // The window has been in self.hooked since startup (the pool holds no copy)
        ctx.info(
            "bridge",
            &format!("pool: assigned {key} to pool worker session={sid}"),
        );

        let target = self
            .workers
            .window_of(&sid)
            .unwrap_or_else(|| SessionId::from(sid.clone()).window_name());
        // Off the loop, like every other delivery. The caller has already put the message in the queue,
        // so a failure leaves it there and the report posts the error frame
        self.start_delivery(&target, key, root_ts, &ctx);
        ctx.info(
            "bridge",
            &format!(
                "slack-events: hand over message_id={message_id} chat={channel} \
                 thread={root_ts} new=true -> worker {channel}"
            ),
        );
        // Same as Dispatch::Deliver — show the shimmer the moment it is handed over
        self.touch_thread(key, slack::TYPING_STATUS);
        let delivered: Result<(), String> = Ok(());

        // The refill starts a different session — do not let it carry the assigned session_id
        self.start_missing_pool_workers(&LogCtx {
            session_id: None,
            thread_key: ctx.thread_key.clone(),
        });
        delivered
    }

    /// Starts a background task watching the window right after startup. **Always call right after spawn** —
    /// this is the only responder, and there is no other startup deadline (`Claude::watch_spawn_screens`).
    fn start_spawn_screen_watch(
        &self,
        w: &Window,
        what: String,
        key: Option<ThreadKey>,
        session_id: &str,
        ctx: &LogCtx,
    ) {
        let (tx, w, ctx) = (self.cmd_tx.clone(), w.clone(), ctx.clone());
        let session_id = session_id.to_string();
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let out = agent
                .watch_spawn_screens(&w, SPAWN_SCREEN_BUDGET_MS, SPAWN_SCREEN_POLL_MS, &ctx)
                .await;
            // Windows that start normally come here every time — only a rejection or an exit is worth waking main for
            if matches!(
                out,
                SpawnOutcome::LoginRequired | SpawnOutcome::UsageLimited | SpawnOutcome::Exited
            ) {
                let _ = tx
                    .send(CmdFx::SpawnScreen { outcome: out, what, key, session_id })
                    .await;
            }
        });
    }

    pub(super) fn spawn_worker(&mut self, req: &SpawnReq, key: &ThreadKey, ctx: &LogCtx, sid: &str) {
        match self.deps.agent.spawn(req) {
            Ok(window) => {
                self.start_spawn_screen_watch(&window, format!("thread={key}"), Some(key.clone()), sid, ctx);
                let id = window.as_str().to_string();
                // Window names can be renamed, so track by this window_id from now on
                self.workers.warm_mut(sid).window_id = Some(id.clone());
                // Just started = hold deliveries until the first turn (the TUI cannot take keys yet)
                self.workers.mark_starting(sid);
                ctx.debug("bridge", &format!("spawned window={id}"));
                self.milestone(Some(key), "spawn", ctx);
            }
            Err(e) => {
                ctx.error("bridge", &format!("spawn failed: {e}"));
                self.agent_never_started(key, sid, ctx);
            }
        }
    }

    /// The agent for `key` stopped before it read its first message, or never started. Say so in the
    /// thread and settle it — otherwise the message sits under 👀 with nobody to answer it. A session
    /// that never began is dropped from the thread, so the next message starts a fresh agent instead
    /// of resuming a conversation that doesn't exist (which would exit at once, just as silently).
    pub(super) fn agent_never_started(&mut self, key: &ThreadKey, sid: &str, ctx: &LogCtx) {
        self.workers.clear_starting(sid);
        let (channel, root) = key.split();
        let Some(root_ts) = root else { return };
        if !self.deps.agent.session_history_exists(None, sid)
            && let Some(mut e) = self.threads.get(&root_ts).cloned()
            && e.agent_id.as_deref() == Some(sid)
        {
            e.agent_id = None;
            self.threads.upsert(&root_ts, e);
            if let Err(err) = self.threads.save() {
                ctx.error("bridge", &format!("threads.json save failed: {err}"));
            }
        }
        // Messages queued behind the start-up would wait for a first turn that never comes
        self.pending.remove(&root_ts);
        self.settle_told(key);
        ctx.error(
            "bridge",
            &format!("agent session={sid} stopped before its first message — told {key}"),
        );
        let machine = &self.machine_name;
        self.post_error_frame(
            channel,
            root_ts,
            crate::t!(
                "Claude Code on *{machine}* stopped before it could read this message, so no reply was written. Send it again.",
                "*{machine}* の Claude Code が、このメッセージを読む前に止まったため、返信を書けませんでした。もう一度送ってください。"
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude::Claude;
    use crate::agent::tmux::Tmux;

    // ── Reclaiming cold agents ───────────────────────────────────────────────

    const MIN: u64 = 60_000;

    /// The values decided for real use (30 min / 5 / 60 min / 10).
    const P: CleanupPolicy = CleanupPolicy {
        idle_ttl_ms: 30 * MIN,
        idle_slots: 5,
        idle_max_ms: 60 * MIN,
        max_concurrent: 10,
    };

    /// A thread that has been cold for `idle_min` minutes and may be dropped.
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

    /// With free seats and cold ones within the cap, nothing is torn down.
    #[test]
    fn nothing_is_evicted_while_within_every_limit() {
        // 5 cold (exactly the cap) + 4 warm = 9 (within the cap of 10)
        let mut v: Vec<IdleSnapshot> = (0..5).map(|n| snap(n, 45)).collect();
        v.extend((5..9).map(|n| snap(n, 1)));
        assert!(select_cleanup_keys(v, &P).is_empty());
    }

    /// The absolute idle limit ignores free seats — even a single one is torn down.
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

    /// Only the cold ones beyond the cold cap (5) are torn down, **coldest first**.
    #[test]
    fn only_the_coldest_surplus_leaves_the_idle_slots() {
        // 7 cold (31–37 min). The cap is 5, so 2 go — the colder ones
        let v: Vec<IdleSnapshot> = (1..=7).map(|n| snap(n, 30 + n as u64)).collect();
        assert_eq!(keys(select_cleanup_keys(v, &P)), ["C1:7", "C1:6"]);
    }

    /// Above the concurrency cap, the coldest go first even if not cold.
    #[test]
    fn the_concurrency_cap_evicts_even_warm_threads() {
        // All 12 warm (under TTL) = the cold cap is unused. Cap is 10, so 2 go
        let v: Vec<IdleSnapshot> = (1..=12).map(|n| snap(n, n as u64)).collect();
        assert_eq!(keys(select_cleanup_keys(v, &P)), ["C1:12", "C1:11"]);
    }

    /// What must not be touched is never torn down by any rule (give up on getting back under the cap).
    #[test]
    fn ineligible_threads_are_never_evicted() {
        let ineligible = |n: u32, idle_min: u64| IdleSnapshot {
            eligible: false,
            ..snap(n, idle_min)
        };
        // Even after 3 hours idle, they stay if waiting for a reply, starting, or waiting for a click
        assert!(select_cleanup_keys(vec![ineligible(1, 180)], &P).is_empty());
        // Not collateral for exceeding the cap either — what goes is the coldest eligible one
        let mut v: Vec<IdleSnapshot> = (1..=10).map(|n| snap(n, n as u64)).collect();
        v.push(ineligible(99, 200));
        assert_eq!(keys(select_cleanup_keys(v, &P)), ["C1:10"]);
    }

    /// Fake tmux where the window is alive (returns a pane_pid).
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

    /// Fake tmux with no window.
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

    /// The latch lifecycle: set on spawn, cleared on the first turn. **It clears only once** —
    /// flushing the queue on that one time is enough.
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

    /// **Agents inherited from the previous Bridge are Ready without guessing.** The latch is only set
    /// when this process starts an agent, so not set = fully started by the previous generation = safe to hand work.
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

    /// Hooks touching the seat do not move the latch. **session_start also fires on compact / clear**, so
    /// flipping back to "starting" there would leave deliveries to a running agent stuck in the queue
    /// forever (real machine, 2026-08-01. The predecessor set `started_here` here).
    #[test]
    fn a_hook_touching_the_seat_never_puts_the_worker_back_into_starting() {
        let entry = entry_of("sid-1");
        let mut w = Workers::default();

        // The path session_start takes after a compact (resets ended, re-learns the transcript)
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
    /// The pool does **not** hold its own ready flag — the same session's `mcp_ready` shows through.
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

        // The window is looked up from the same place too (no copy on the pool side)
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
        // Once the pid is gone, it is gone even if it worked in the past
        assert_eq!(
            Workers::state_from(&facts(None, false, false)),
            WorkerState::Absent
        );
        // After session_end it is gone even if the pid remains
        assert_eq!(
            Workers::state_from(&facts(Some(1), false, true)),
            WorkerState::Absent
        );
    }
}
