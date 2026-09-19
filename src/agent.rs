//! The agent abstraction layer. The Bridge knows nothing below this directly.
//!
//! Bridge talks to the agent through `crate::agent::Agent`; the only implementation is
//! [`claude::Claude`]. This module keeps the types that cross that edge.

pub mod claude;
pub mod screen;
pub mod tmux;


use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::log::LogCtx;
use crate::chat::ThreadKey;

/// A window already resolved to a form `-t` accepts.
///
/// Handling raw window names (`1-1`) and resolved targets (`agentgw-workers:1-1`) as the
/// same `&str` bred mix-ups — a raw name passed to `capture-pane -t` doesn't error, it reads
/// "the window of that name in the current session". Going through this type makes that unwritable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window(String);

impl Window {
    /// From a raw window (`@N` or a window name). `@N` (window_id) can be used as is — unlike
    /// a window name it doesn't move on rename. Only window names get qualified with the
    /// session (a bare name makes tmux pick "the current session").
    pub fn of(window: &str) -> Self {
        Window(tmux::target(window))
    }

    /// From a string that can already go straight to `-t` (the sign-in session and other
    /// things that aren't windows in the agent session).
    pub(crate) fn raw(target: impl Into<String>) -> Self {
        Window(target.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// So it can be embedded in log lines as is — the logs print the resolved target.
impl std::fmt::Display for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `WORKER_KILL_GRACE_MS` / `WORKER_KILL_HARD_MS` = 1500ms,
/// polling interval 100ms (`pollMs`).
pub const KILL_GRACE_MS: u64 = 1_500;
const KILL_HARD_MS: u64 = 1_500;
const KILL_POLL_MS: u64 = 100;

/// The agent's own process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pid(pub u32);

/// So it can be embedded in log lines as is — the logs print the bare pid.
impl std::fmt::Display for Pid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Pid {
    /// Kill by pid. **Never kill by pattern (pkill -f): it takes production agents with it.**
    /// TERM → poll for liveness up to grace_ms → -9 if still there → another 1500ms.
    /// Returns whether a live pid was actually taken down. Already gone, or surviving -9, are both false.
    pub async fn kill_graceful(&self, grace_ms: u64) -> bool {
        let pid = self.0;
        if !self.alive() {
            return false;
        }
        self.signal("-TERM");
        if self.wait_until_gone(grace_ms).await {
            return true;
        }
        crate::log::LogCtx::default().info(
            "worker",
            &format!("SIGKILL pid {pid} — survived SIGTERM within {grace_ms}ms"),
        );
        self.signal("-9");
        let gone = self.wait_until_gone(KILL_HARD_MS).await;
        if !gone {
            crate::log::LogCtx::default()
                .error("worker", &format!("pid {pid} still alive after SIGKILL"));
        }
        gone
    }

    /// Sends SIGTERM only and **does not wait**. A pre-shot for tearing down several agents,
    /// so `kill_graceful`'s grace periods don't pile up in series (running `kill_graceful`
    /// afterwards lets the grace periods overlap).
    pub fn term(&self) -> bool {
        self.signal("-TERM")
    }

    /// Looks only at `kill`'s exit code. `output()` keeps `-0`'s "No such process" out of
    /// the log.
    fn signal(&self, sig: &str) -> bool {
        std::process::Command::new("kill")
            .args([sig, &self.0.to_string()])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// `kill -0` = liveness probe (no signal is sent).
    fn alive(&self) -> bool {
        self.signal("-0")
    }

    async fn wait_until_gone(&self, budget_ms: u64) -> bool {
        for _ in 0..budget_ms.div_ceil(KILL_POLL_MS).max(1) {
            tokio::time::sleep(std::time::Duration::from_millis(KILL_POLL_MS)).await;
            if !self.alive() {
                return true;
            }
        }
        false
    }
}

/// One window seen in the inventory.
#[derive(Clone)]
pub struct WindowRow {
    pub id: String,
    pub pid: Pid,
    /// The command running in the pane **right now** (`claude` / `zsh` …).
    pub command: String,
    pub name: String,
}

impl WindowRow {
    /// The session_id of the agent this window holds (`w-<sid>`).
    /// `None` = not an agent window (the anchor window, a window a person opened) → **don't touch it**.
    pub fn session_id(&self) -> Option<&str> {
        self.name.strip_prefix("w-").filter(|s| !s.is_empty())
    }

    /// Whether this is a husk where claude is gone and only the shell is left.
    /// Login shells carry a leading `-`, like `-zsh`.
    pub fn is_empty_shell(&self) -> bool {
        matches!(
            self.command.trim().trim_start_matches('-'),
            "zsh" | "bash" | "sh" | "fish" | "dash" | "ksh"
        )
    }
}

/// How the watch ended. **This is not "started successfully"**: that is expressed by the first
/// user_prompt releasing the `starting` latch, so this only says whether the screens that
/// needed an answer got one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnOutcome {
    /// Answered a screen that could be answered (or answered and kept watching until the deadline)
    Answered,
    /// Sign-in screen. **Nobody here can answer it**; the only option is to tell the Owner
    LoginRequired,
    /// Usage-limit modal. Same as above (confirming it is the caller's job)
    UsageLimited,
    /// Nothing showed up before the deadline. **Not a failure**: a window that started normally looks like this
    NoScreen,
    /// The window went away while being watched. Whether that matters (did the agent ever take its
    /// first message?) is the caller's call — this only says the window is gone
    Exited,
}

/// The session identifier of one agent.
///
/// **Single-use** — claude dies at once when started with an already-used ID.
/// Every spawn is new; continuing is left to the implementation's resume mechanism.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(String);

impl SessionId {
    /// A new ID per spawn. claude refuses to start with a used ID (measured in the spike).
    pub fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or_default();
        let pid = std::process::id();
        let h = format!("{nanos:016x}{pid:08x}{:08x}", nanos as u32);
        SessionId(format!(
            "{}-{}-4{}-8{}-{}",
            &h[0..8],
            &h[8..12],
            &h[13..16],
            &h[17..20],
            &h[20..32]
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The agent's tmux window name. **One rule for pooled and assigned agents alike**.
    /// Since the name depends only on session_id, it can be looked up by name after a Bridge restart that forgot the window_id
    /// (no pool-only names — the window is not renamed when taken from the pool).
    pub fn window_name(&self) -> String {
        let mut name = String::from("w-");
        name.extend(self.0.chars().map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-' => c,
            _ => '-',
        }));
        name
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        SessionId(s)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        SessionId(s.to_string())
    }
}

/// `new()` issues a **new** value every time — that is why `Default` means the same
/// (a single-use ID has no notion of a "default value"; default = issue a new one is the only reading).
impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether an agent is alive. Derived from facts by the Bridge's `Workers` (`bridge/worker.rs`).
///
/// Handed to the implementation via [`SpawnReq::state`] to check the seat (spawn into anything but `Absent` is refused).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WorkerState {
    Absent,
    Starting,
    Ready,
}

/// A request to start one agent.
#[derive(Clone)]
pub struct SpawnReq {
    pub session_id: SessionId,
    pub cwd: String,
    /// The first text to feed in. `None` means the implementation's idle prompt (warming the pool).
    /// The implementation adds its own orienting preamble, so put the **bare envelope** here.
    pub prompt: Option<String>,
    /// The previous session id when continuing.
    pub resume_from: Option<SessionId>,
    /// The seat's window name (rule of [`SessionId::window_name`]). It gets renamed, so not used for tracking.
    pub window: String,
    /// The seat's current state. The implementation refuses spawn into anything but `Absent` (spawn never kills).
    pub state: WorkerState,
    /// Absolute path of the hook settings file handed to the implementation.
    pub hooks_file: String,
    /// Absolute path of the MCP config file written for this session.
    pub mcp_config: String,
}

/// The envelope handed to an agent — one inbound message, in a form that can go straight into the prompt.
///
/// Attribute order follows the meta build order.
/// `message_id` is **the inbound message's ts**; `ts` is the now at delivery (filled in by the caller).
pub struct Envelope {
    pub channel_id: String,
    pub message_id: String,
    pub user: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub text: String,
    /// Local paths of attachments downloaded ahead. `file_count` is their count.
    pub file_paths: Vec<String>,
    /// Degradation notes for attachments that couldn't be downloaded. Not dropped silently: shown to the agent so it tells the person.
    pub file_errors: Vec<String>,
    /// A delivery the loop guard tripped on (`loop_guard="true"`). When present, it signals: don't answer
    /// that bot, **ask a person to decide**. `loop_notify` is who to call (the Owner's mention).
    pub loop_guard: Option<String>,
}

/// The source Claude Code uses when drawing a push notification. Using the same value makes
/// the injected first message byte-identical to a pushed one.
///
/// **The reader is the plugin's name.** As of 2026-09-18 there is no matching plugin in
/// `~/.claude/plugins`, so it matches the binary name. If a plugin is made later, keep
/// its name and this in sync.
const CHANNEL_SOURCE: &str = "plugin:agentgw:agentgw";

impl Envelope {
    /// Hands a cold-start agent its first message in the same form as a push-in.
    pub fn render(&self) -> String {
        let e = self;
        // No thread_ts means no attribute at all
        let thread_ts = e
            .thread_ts
            .as_deref()
            .map_or(String::new(), |t| format!(" thread_ts=\"{t}\""));
        // Attachments were downloaded on receipt. Successes give
        // file_count/file_paths, failures give file_errors. Missing ones leave the attribute out entirely
        let files = if e.file_paths.is_empty() {
            String::new()
        } else {
            format!(
                " file_count=\"{}\" file_paths=\"{}\"",
                e.file_paths.len(),
                e.file_paths.join(",")
            )
        };
        let file_errors = if e.file_errors.is_empty() {
            String::new()
        } else {
            format!(" file_errors=\"{}\"", e.file_errors.join("\n"))
        };
        // Loop guard — the attribute is only on deliveries where it tripped
        let loop_guard = match e.loop_guard.as_deref() {
            None => String::new(),
            Some("") => " loop_guard=\"true\"".to_string(),
            Some(who) => format!(" loop_guard=\"true\" loop_notify=\"{who}\""),
        };
        format!(
            "<channel source=\"{CHANNEL_SOURCE}\" channel_id=\"{}\" message_id=\"{}\" user=\"{}\" \
             user_id=\"{}\" ts=\"{}\"{thread_ts}{files}{file_errors}{loop_guard}>\n{}\n</channel>",
            e.channel_id, e.message_id, e.user, e.user, e.ts, e.text
        )
    }
}

/// One event reported by the implementation.
///
/// claude builds it from hooks, another implementation some other way — to the Bridge it's the same type.
/// `respond` is only on hooks waiting for an answer (stop) — the receiver decides by sending or dropping it
/// (dropping = `{}` = decline). Sender can't be cloned, so no Clone.
#[derive(Debug)]
pub struct HookEvent {
    pub kind: String,
    pub session_id: String,
    pub payload: serde_json::Value,
    pub respond: Option<tokio::sync::oneshot::Sender<serde_json::Value>>,
}

/// One table row. `(name, tokens, %)` — all strings **exactly as printed**.
pub type ContextCategory = (String, String, String);

/// Parsed `/context`.
///
/// Not converted to numbers (the printed text already has the digits needed for drawing).
#[derive(Debug)]
pub struct ContextReport {
    /// e.g. `claude-opus-4-8[1m]`
    pub model: String,
    /// The model as people read it, e.g. `Opus 4.8（1M context）`
    pub model_label: String,
    /// Tokens used (as printed, e.g. `43.8k`)
    pub used: String,
    /// Window size (as printed, e.g. `1m`)
    pub total: String,
    /// Usage percentage (as printed, e.g. `4%`)
    pub pct: String,
    pub categories: Vec<ContextCategory>,
}

/// A record in which Claude Code itself wrote "hit the limit".
#[derive(Debug, PartialEq, Eq)]
pub struct LimitHit {
    /// The record's text (logged as is — keeps what the decision was based on).
    pub detail: String,
    /// When the wall lifts (epoch ms).
    pub reset_ms: u64,
}

/// One limit line of `/usage` (e.g. `Current week (all models): 66% used · resets Jul 1 at 5pm`).
#[derive(Debug)]
pub struct UsageRow {
    pub label: String,
    pub pct: String,
    /// The content of `· resets <when>`. A 0% line has no such clause and this is empty
    pub reset: String,
}

/// What can be read from the live compaction spinner. Only seconds and % become numbers; tokens stay **as printed**.
#[derive(Debug, Default, PartialEq)]
pub struct CompactProgress {
    pub active: bool,
    /// Elapsed seconds (`(1m 4s)` → 64). None while the timer isn't shown yet
    pub seconds: Option<u32>,
    /// Token count as printed (e.g. `876` / `1.6k`)
    pub tokens: Option<String>,
    /// Token count arrow (`↑` / `↓`)
    pub tokens_dir: Option<char>,
    /// Completion percentage (0–100) read from the bar line right below the spinner
    pub percent: Option<u8>,
}

/// Reading of the pane **after** the code was sent.
#[derive(Debug, PartialEq, Eq)]
pub enum LoginOutcome {
    Success,
    Error,
    Pending,
}

/// How a compaction ended. **Holds no wording** — what to say is up to the Bridge.
#[derive(Debug, PartialEq, Eq)]
pub enum CompactOutcome {
    Done,
    /// The history was too short to have anything to compact.
    Nothing,
    Failed,
}

/// The two ways a headless probe fails. **The wording returned to the Owner differs**, so they are separate types.
///
/// "Ran but didn't answer" and "couldn't start at all" get different wording for the Owner;
/// only the former may suggest a retry.
pub enum ProbeErr {
    /// Non-zero exit / 60-second timeout
    Failed(String),
    /// Couldn't spawn (the implementation isn't on PATH, etc.)
    Errored(String),
}


// ── the agent as the core sees it ──

/// Starting and driving the coding agent. Implemented by [`claude::Claude`].
///
/// Mostly synchronous because the implementation drives tmux with blocking commands.
/// The optional features answer `None` when the agent does not support them — whether to
/// say so is the caller's call.
#[async_trait]
pub trait Agent: Send + Sync + 'static {
    fn spawn(&self, req: &SpawnReq) -> Result<Window, String>;
    /// Hands `text` to the agent as its next message and confirms it was submitted.
    /// Errs when the window can't take keys (a dialog is open) or the text never left the
    /// input box — the caller keeps the message instead of assuming it arrived.
    fn deliver(&self, w: &Window, text: &str) -> Result<(), String>;
    /// Answers the start-up screens of a freshly spawned window until `budget_ms` runs out.
    async fn watch_spawn_screens(
        &self,
        w: &Window,
        budget_ms: u64,
        poll_ms: u64,
        ctx: &LogCtx,
    ) -> SpawnOutcome;
    /// The worker's process. The only proof that it is alive.
    fn pid_of(&self, window_id: Option<&str>, window_name: &str) -> Option<Pid>;
    /// Every window the agent's session holds, workers or not.
    fn windows(&self) -> Vec<WindowRow>;
    fn terminate(&self, w: &Window) -> Result<(), String>;
    /// Cancels what the worker is doing (Escape).
    fn interrupt(&self, w: &Window) -> Result<(), String>;
    fn login_kill(&self);

    /// Compacts the conversation. Progress goes to `progress` one reading at a time;
    /// the sender is dropped when this returns.
    async fn compact(
        &self,
        w: &Window,
        key: &ThreadKey,
        session_id: &str,
        progress: mpsc::Sender<CompactProgress>,
    ) -> Option<CompactOutcome>;
    async fn effort(&self, w: &Window, key: &ThreadKey, session_id: &str)
    -> Option<&'static str>;
    async fn set_effort(
        &self,
        w: &Window,
        level: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) -> Option<bool>;
    fn mode(&self, w: &Window, ctx: &LogCtx) -> Option<&'static str>;
    async fn set_mode(&self, w: &Window, name: &str, key: &ThreadKey, ctx: &LogCtx)
    -> Option<bool>;
    async fn set_model(
        &self,
        w: &Window,
        name: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) -> Option<bool>;

    // `remembered` is the history path a hook carried (first candidate: a worker that
    // moved into a worktree takes its history with it).
    fn session_cwd(&self, remembered: Option<&str>, session_id: &str) -> Option<String>;
    fn session_history_exists(&self, remembered: Option<&str>, session_id: &str) -> bool;
    /// Whether `path` is a folder an agent can start in on this machine. tmux doesn't say when it
    /// isn't — it quietly starts in the home directory instead.
    fn workdir_exists(&self, path: &str) -> bool;
    fn last_activity_ms(&self, remembered: Option<&str>, session_id: &str) -> Option<u64>;
    fn current_model(
        &self,
        remembered: Option<&str>,
        session_id: &str,
    ) -> Option<std::io::Result<Option<String>>>;
    fn session_limit_error(
        &self,
        remembered: Option<&str>,
        session_id: &str,
        now_ms: u64,
    ) -> Option<std::io::Result<Option<LimitHit>>>;
    /// What a turn that ended with no `error_type` actually failed on, read from the agent's
    /// own record — in the same words as `error_type` (e.g. `authentication_failed`).
    /// `None` = can't tell (no record, or an error this agent doesn't recognise).
    fn failure_type(&self, remembered: Option<&str>, session_id: &str) -> Option<&'static str>;
    fn model_alias(&self, model_id: &str) -> Option<&'static str>;
    /// The values `model` / `effort` / `mode` accept, in the order `help` lists them.
    fn models(&self) -> &'static [&'static str];
    fn effort_levels(&self) -> &'static [&'static str];
    fn modes(&self) -> &'static [&'static str];
    /// A typed model name → the name the agent knows (`sonet` → `sonnet`), or `None` if unknown.
    fn canonical_model(&self, typed: &str) -> Option<String>;
    /// The shell command that continues `session_id` in a terminal (`claude --resume …`).
    fn resume_command(&self, cwd: Option<&str>, session_id: &str) -> String;
    /// Reads the history from `offset` on: `(text, new offset)`.
    fn new_history_lines(&self, path: String, offset: u64, ctx: &LogCtx)
    -> Option<(String, u64)>;

    fn context_argv(&self, session_id: &str) -> Vec<String>;
    fn usage_argv(&self) -> Vec<String>;
    async fn probe(&self, argv: Vec<String>, cwd: String) -> Result<String, ProbeErr>;
    /// Whether the agent is signed in right now. `None` = couldn't find out.
    async fn signed_in(&self) -> Option<bool>;
    fn context_report(&self, raw: &str) -> Option<ContextReport>;
    fn usage_rows(&self, raw: &str) -> Option<Vec<UsageRow>>;

    fn login_begin(&self, cwd: &str) -> Result<(), String>;
    fn login_url(&self) -> Option<String>;
    fn login_submit_code(&self, code: &str) -> Result<(), String>;
    fn login_outcome(&self) -> LoginOutcome;
    async fn logout(&self) -> Result<(), ProbeErr>;
}

pub type AgentRef = std::sync::Arc<dyn Agent>;

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Records spawns and deliveries; windows live until terminated.
    #[derive(Default)]
    pub struct FakeAgent {
        pub spawned: Mutex<Vec<SpawnReq>>,
        /// (window id, text)
        pub delivered: Mutex<Vec<(String, String)>>,
        /// Window ids that got an interrupt (Escape).
        pub interrupted: Mutex<Vec<String>>,
        /// When set, `deliver` fails the way a window with an open dialog does.
        pub fail_deliver: std::sync::atomic::AtomicBool,
        /// Sessions whose conversation history exists (others can't be resumed).
        pub histories: Mutex<Vec<String>>,
        /// What `failure_type` answers (what the agent's own record says a failed turn died of).
        pub failure_type: Mutex<Option<&'static str>>,
        /// What `signed_in` answers once set; unset means `Some(true)` (the usual case).
        pub signed_in: Mutex<Option<Option<bool>>>,
        /// Sign-in codes handed to `login_submit_code`.
        pub submitted_codes: Mutex<Vec<String>>,
        /// When set, `spawn` fails (the agent can't be started at all).
        pub fail_spawn: std::sync::atomic::AtomicBool,
        /// Folders `workdir_exists` says are missing (every other folder exists).
        pub missing_dirs: Mutex<Vec<String>>,
        windows: Mutex<Vec<WindowRow>>,
        next: AtomicU64,
    }

    #[async_trait]
    impl Agent for FakeAgent {
        fn spawn(&self, req: &SpawnReq) -> Result<Window, String> {
            if self.fail_spawn.load(Ordering::SeqCst) {
                return Err("tmux: no server running".into());
            }
            let n = self.next.fetch_add(1, Ordering::SeqCst);
            let id = format!("@{n}");
            self.spawned.lock().unwrap().push(req.clone());
            self.windows.lock().unwrap().push(WindowRow {
                id: id.clone(),
                pid: Pid(4242 + n as u32),
                command: "claude".into(),
                name: req.window.clone(),
            });
            Ok(Window::of(&id))
        }
        fn deliver(&self, w: &Window, text: &str) -> Result<(), String> {
            if self.fail_deliver.load(Ordering::SeqCst) {
                return Err(format!("{w}: a dialog is open"));
            }
            self.delivered
                .lock()
                .unwrap()
                .push((w.as_str().to_string(), text.to_string()));
            Ok(())
        }
        async fn watch_spawn_screens(
            &self,
            _w: &Window,
            _b: u64,
            _p: u64,
            _c: &LogCtx,
        ) -> SpawnOutcome {
            SpawnOutcome::NoScreen
        }
        fn pid_of(&self, id: Option<&str>, name: &str) -> Option<Pid> {
            let rows = self.windows.lock().unwrap();
            rows.iter()
                .find(|r| id == Some(r.id.as_str()))
                .or_else(|| rows.iter().find(|r| r.name == name))
                .map(|r| r.pid)
        }
        fn windows(&self) -> Vec<WindowRow> {
            self.windows.lock().unwrap().clone()
        }
        fn terminate(&self, w: &Window) -> Result<(), String> {
            self.windows.lock().unwrap().retain(|r| r.id != w.as_str());
            Ok(())
        }
        fn interrupt(&self, w: &Window) -> Result<(), String> {
            self.interrupted.lock().unwrap().push(w.as_str().to_string());
            Ok(())
        }
        fn login_kill(&self) {}
        async fn compact(
            &self,
            _w: &Window,
            _k: &ThreadKey,
            _s: &str,
            _p: mpsc::Sender<CompactProgress>,
        ) -> Option<CompactOutcome> {
            Some(CompactOutcome::Done)
        }
        async fn effort(&self, _w: &Window, _k: &ThreadKey, _s: &str) -> Option<&'static str> {
            None
        }
        async fn set_effort(&self, _w: &Window, _l: &str, _k: &ThreadKey, _c: &LogCtx) -> Option<bool> {
            Some(true)
        }
        fn mode(&self, _w: &Window, _c: &LogCtx) -> Option<&'static str> {
            None
        }
        async fn set_mode(&self, _w: &Window, _m: &str, _k: &ThreadKey, _c: &LogCtx) -> Option<bool> {
            Some(true)
        }
        async fn set_model(&self, _w: &Window, _m: &str, _k: &ThreadKey, _c: &LogCtx) -> Option<bool> {
            Some(true)
        }
        fn session_cwd(&self, _r: Option<&str>, _s: &str) -> Option<String> {
            None
        }
        fn session_history_exists(&self, _r: Option<&str>, s: &str) -> bool {
            self.histories.lock().unwrap().iter().any(|h| h == s)
        }
        fn workdir_exists(&self, path: &str) -> bool {
            !self.missing_dirs.lock().unwrap().iter().any(|d| d == path)
        }
        fn last_activity_ms(&self, _r: Option<&str>, _s: &str) -> Option<u64> {
            None
        }
        fn current_model(
            &self,
            _r: Option<&str>,
            _s: &str,
        ) -> Option<std::io::Result<Option<String>>> {
            None
        }
        fn session_limit_error(
            &self,
            _r: Option<&str>,
            _s: &str,
            _now: u64,
        ) -> Option<std::io::Result<Option<LimitHit>>> {
            None
        }
        fn failure_type(&self, _r: Option<&str>, _s: &str) -> Option<&'static str> {
            *self.failure_type.lock().unwrap()
        }
        fn model_alias(&self, _m: &str) -> Option<&'static str> {
            None
        }
        // The vocabulary is Claude's own, so the fake answers exactly what Claude does.
        fn models(&self) -> &'static [&'static str] {
            super::claude::Claude::real().models()
        }
        fn effort_levels(&self) -> &'static [&'static str] {
            super::claude::Claude::real().effort_levels()
        }
        fn modes(&self) -> &'static [&'static str] {
            super::claude::Claude::real().modes()
        }
        fn canonical_model(&self, typed: &str) -> Option<String> {
            super::claude::Claude::real().canonical_model(typed)
        }
        fn resume_command(&self, cwd: Option<&str>, session_id: &str) -> String {
            super::claude::Claude::real().resume_command(cwd, session_id)
        }
        fn new_history_lines(&self, _p: String, _o: u64, _c: &LogCtx) -> Option<(String, u64)> {
            None
        }
        fn context_argv(&self, _s: &str) -> Vec<String> {
            vec![]
        }
        fn usage_argv(&self) -> Vec<String> {
            vec![]
        }
        async fn probe(&self, _a: Vec<String>, _c: String) -> Result<String, ProbeErr> {
            Ok(String::new())
        }
        async fn signed_in(&self) -> Option<bool> {
            self.signed_in.lock().unwrap().unwrap_or(Some(true))
        }
        fn context_report(&self, _r: &str) -> Option<ContextReport> {
            None
        }
        fn usage_rows(&self, _r: &str) -> Option<Vec<UsageRow>> {
            None
        }
        fn login_begin(&self, _c: &str) -> Result<(), String> {
            Ok(())
        }
        fn login_url(&self) -> Option<String> {
            Some("https://claude.ai/oauth".into())
        }
        fn login_submit_code(&self, c: &str) -> Result<(), String> {
            self.submitted_codes.lock().unwrap().push(c.to_string());
            Ok(())
        }
        fn login_outcome(&self) -> LoginOutcome {
            LoginOutcome::Success
        }
        async fn logout(&self) -> Result<(), ProbeErr> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_window_name_is_w_plus_the_sanitized_session_id() {
        // An ordinary session_id (alphanumerics and `-`) just gets `w-` prefixed
        assert_eq!(
            SessionId::from("18c6a2a8-be4e-4a80-8000-8b08be4e7a80").window_name(),
            "w-18c6a2a8-be4e-4a80-8000-8b08be4e7a80"
        );
        // Each `[^A-Za-z0-9_-]` character becomes `-`
        assert_eq!(
            SessionId::from("a:b.c/d e_f").window_name(),
            "w-a-b-c-d-e_f"
        );
        assert_eq!(SessionId::from("あ").window_name(), "w--");
    }

    #[test]
    fn session_id_is_uuid_v4_shaped() {
        let id = SessionId::new();
        let s = id.as_str();
        let g: Vec<&str> = s.split('-').collect();
        assert_eq!(
            g.iter().map(|x| x.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "{s}"
        );
        assert!(g[2].starts_with('4'), "{s}");
        assert!(g[3].starts_with('8'), "{s}");
        assert!(s.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "{s}");
    }

    #[test]
    fn envelope_matches_current_shape() {
        let e = Envelope {
            channel_id: "C1".into(),
            message_id: "171.002".into(),
            user: "U1".into(),
            ts: "2026-07-28T00:00:00.000Z".into(),
            thread_ts: Some("171.001".into()),
            text: "hello\nworld".into(),
            file_paths: Vec::new(),
            file_errors: Vec::new(),
            loop_guard: None,
        };
        assert_eq!(
            e.render(),
            "<channel source=\"plugin:agentgw:agentgw\" channel_id=\"C1\" \
             message_id=\"171.002\" user=\"U1\" user_id=\"U1\" ts=\"2026-07-28T00:00:00.000Z\" \
             thread_ts=\"171.001\">\nhello\nworld\n</channel>"
        );
        // No thread_ts means no attribute at all
        let bare = Envelope {
            thread_ts: None,
            ..e
        };
        assert!(!bare.render().contains("thread_ts"));
        // Without attachments, the file_* attributes are left out entirely
        assert!(!bare.render().contains("file_"));
    }

    fn envelope_with(paths: Vec<String>, errors: Vec<String>) -> String {
        Envelope {
            channel_id: "C1".into(),
            message_id: "171.002".into(),
            user: "U1".into(),
            ts: "2026-07-28T00:00:00.000Z".into(),
            thread_ts: Some("171.001".into()),
            text: "check the screenshot".into(),
            file_paths: paths,
            file_errors: errors,
            loop_guard: None,
        }
        .render()
    }

    /// The attribute is only on deliveries where the loop guard tripped. With nobody to call, just the mark.
    #[test]
    fn the_loop_guard_rides_the_envelope_only_when_it_tripped() {
        let env = |guard: Option<&str>| {
            Envelope {
                channel_id: "C1".into(),
                message_id: "171.002".into(),
                user: "B1".into(),
                ts: "2026-07-28T00:00:00.000Z".into(),
                thread_ts: Some("171.001".into()),
                text: "また返してきた".into(),
                file_paths: Vec::new(),
                file_errors: Vec::new(),
                loop_guard: guard.map(str::to_string),
            }
            .render()
        };
        assert!(!env(None).contains("loop_guard"));
        assert!(env(Some("<@U1>")).contains(r#" loop_guard="true" loop_notify="<@U1>""#));
        let no_owner = env(Some(""));
        assert!(no_owner.contains(r#" loop_guard="true""#));
        assert!(!no_owner.contains("loop_notify"));
    }

    #[test]
    fn envelope_carries_downloaded_attachments() {
        // file_count is the number of **successful** downloads, file_paths is comma-joined
        let out = envelope_with(vec!["/i/a.png".into(), "/i/b.png".into()], Vec::new());
        assert!(
            out.contains(
                " thread_ts=\"171.001\" file_count=\"2\" file_paths=\"/i/a.png,/i/b.png\">"
            ),
            "{out}"
        );
        assert!(!out.contains("file_errors"), "{out}");
    }

    #[test]
    fn envelope_carries_failure_notes_without_paths() {
        // Even when nothing could be downloaded the text is delivered — only the degradation note is attached
        let note = "[attachment shot.png not downloaded: file too large]".to_string();
        let out = envelope_with(Vec::new(), vec![note.clone()]);
        assert!(out.contains(&format!(" file_errors=\"{note}\">")), "{out}");
        assert!(!out.contains("file_count"), "{out}");
    }

    #[test]
    fn envelope_mixes_successes_and_failures() {
        // Attribute order is the meta insertion order — file_count, file_paths, file_errors
        let out = envelope_with(
            vec!["/i/a.png".into()],
            vec!["[attachment b.png not downloaded: boom]".into()],
        );
        assert!(
            out.contains(
                " file_count=\"1\" file_paths=\"/i/a.png\" \
                 file_errors=\"[attachment b.png not downloaded: boom]\">"
            ),
            "{out}"
        );
        // Multiple notes are joined with newlines
        let two = envelope_with(Vec::new(), vec!["[a]".into(), "[b]".into()]);
        assert!(two.contains("file_errors=\"[a]\n[b]\">"), "{two}");
    }

    #[tokio::test]
    async fn kill_pid_graceful_kills_a_live_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pid = Pid(child.id());
        // A dead child stays in the pid table as a zombie until waited on and answers `kill -0`. In real runs
        // claude is tmux's child (not ours), so this doesn't happen — the test reaps it itself.
        let reaper = std::thread::spawn(move || child.wait().unwrap());
        assert!(pid.kill_graceful(KILL_GRACE_MS).await);
        reaper.join().unwrap();
    }
}
