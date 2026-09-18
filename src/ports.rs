//! The edges between agentgw's core and the outside world.
//!
//! The core (`bridge`) talks to Slack, to the coding agent and to the clock only through
//! these traits. `Bridge::run()` wires the real implementations; tests wire the fakes in
//! `fake` and drive the same code.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::agent::screen::SpawnOutcome;
use crate::agent::tmux::{Pid, Window, WindowRow};
use crate::agent::{
    CompactOutcome, CompactProgress, ContextReport, LoginOutcome, ProbeErr, SpawnReq, UsageRow,
};
use crate::agent::LimitHit;
use crate::bridge::state::{LogCtx, ThreadKey};

// ── what Slack reads return ──

/// 既にある1件の投稿について、**イベントからは分からないこと**だけ。
/// [`SlackPort::message_at`] が返す。
pub struct MessageAt {
    /// 書き手(bot が投げたものには入らないことがある)。
    pub user: Option<String>,
    /// bot が投げたもの。
    pub is_bot: bool,
    /// 属するスレッドの根。返信でなければ自分自身の ts。
    pub thread_ts: String,
}

/// history/replies の1行。
#[derive(Clone)]
pub struct FetchedMsg {
    pub ts: String,
    pub user: String,
    pub text: String,
    /// スレッドの根。返信なら親の ts、根自身なら自分の ts、スレッド外なら None。
    pub thread_ts: Option<String>,
}

impl FetchedMsg {
    /// oldest-first の `[ts] user: text`。
    pub fn render_all(msgs: &[FetchedMsg]) -> String {
        let mut rows: Vec<&FetchedMsg> = msgs.iter().collect();
        rows.sort_by(|a, b| a.ts.cmp(&b.ts));
        rows.iter()
            .map(|m| format!("[{}] {}: {}", m.ts, m.user, m.text))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Writing to and reading from Slack. Implemented by `slack::Api`.
#[async_trait]
pub trait SlackPort: Send + Sync + 'static {
    async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String>;
    async fn post_message_no_unfurl(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String>;
    async fn post_markdown(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String>;
    async fn update_message(&self, channel: &str, ts: &str, text: &str) -> Result<(), String>;
    async fn update_markdown(&self, channel: &str, ts: &str, text: &str) -> Result<(), String>;
    async fn delete_message(&self, channel: &str, ts: &str) -> Result<(), String>;
    async fn add_reaction(&self, channel: &str, ts: &str, emoji: &str) -> Result<(), String>;
    async fn remove_reaction(&self, channel: &str, ts: &str, emoji: &str) -> Result<(), String>;
    /// Marks a delivered message as received: drops the ack and ⟳, adds 🤖. Best-effort.
    async fn flip_to_received(&self, channel: &str, message_ts: &str, ack: &str);
    async fn post_perm_prompt(
        &self,
        channel: &str,
        thread_ts: &str,
        req_id: &str,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> Result<String, String>;
    async fn open_dm(&self, user_id: &str) -> Result<String, String>;
    async fn history(&self, channel: &str, limit: u16) -> Result<Vec<FetchedMsg>, String>;
    async fn replies(
        &self,
        channel: &str,
        thread_ts: &str,
        limit: u16,
    ) -> Result<Vec<FetchedMsg>, String>;
    async fn parent_thread_of(&self, channel: &str, ts: &str) -> Option<String>;
    async fn message_at(&self, channel: &str, ts: &str) -> Option<MessageAt>;
    async fn thread_root_gone(&self, channel: &str, thread_ts: &str) -> bool;
    async fn get_permalink(&self, channel: &str, ts: &str) -> Result<String, String>;
    async fn channel_display_name(&self, channel: &str) -> Option<String>;
    async fn user_display_name(&self, user: &str) -> Option<String>;
    async fn auth_test(&self) -> Result<(String, Option<String>), String>;
    async fn resolve_bot_id(&self, user_id: &str) -> Result<Option<String>, String>;
    async fn file_info(&self, file_id: &str) -> Result<(String, String, u64), String>;
    async fn download_to(&self, url: &str, dest: &Path) -> Result<(), String>;
    /// file_id → the local path it was saved to under `state_dir/inbox`.
    async fn download_attachment(&self, file_id: &str, state_dir: &Path)
    -> Result<String, String>;
    async fn upload_file(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        path: &Path,
    ) -> Result<(), String>;
    async fn set_thinking_status(
        &self,
        channel: &str,
        thread_ts: &str,
        status: &str,
    ) -> Result<(), String>;
}

/// Starting and driving the coding agent. Implemented by `agent::claude::Claude`.
///
/// Mostly synchronous because the implementation drives tmux with blocking commands.
/// The optional features answer `None` when the agent does not support them — whether to
/// say so is the caller's call.
#[async_trait]
pub trait AgentPort: Send + Sync + 'static {
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
    fn model_alias(&self, model_id: &str) -> Option<&'static str>;
    /// Reads the history from `offset` on: `(text, new offset)`.
    fn new_history_lines(&self, path: String, offset: u64, ctx: &LogCtx)
    -> Option<(String, u64)>;

    fn context_argv(&self, session_id: &str) -> Vec<String>;
    fn usage_argv(&self) -> Vec<String>;
    async fn probe(&self, argv: Vec<String>, cwd: String) -> Result<String, ProbeErr>;
    fn context_report(&self, raw: &str) -> Option<ContextReport>;
    fn usage_rows(&self, raw: &str) -> Option<Vec<UsageRow>>;

    fn login_begin(&self, cwd: &str) -> Result<(), String>;
    fn login_url(&self) -> Option<String>;
    fn login_submit_code(&self, code: &str) -> Result<(), String>;
    fn login_outcome(&self) -> LoginOutcome;
    async fn logout(&self) -> Result<(), ProbeErr>;
}

/// The time. Implemented by [`SystemClock`]; tests move a `fake::FakeClock` by hand.
pub trait Clock: Send + Sync + 'static {
    fn now_ms(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        crate::bridge::state::now_ms()
    }
}

pub type Slack = Arc<dyn SlackPort>;
pub type AgentRef = Arc<dyn AgentPort>;
pub type ClockRef = Arc<dyn Clock>;

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Every Slack call, in order: `"post C1 1.0 hello"` / `"react C1 1.1 eyes"` / ...
    ///
    /// Markdown goes on record as `post_md` / `update_md` so a test can tell it from mrkdwn.
    #[derive(Default)]
    pub struct FakeSlack {
        pub calls: Mutex<Vec<String>>,
        next_ts: AtomicU64,
        /// What `history` and `replies` answer.
        pub msgs: Vec<FetchedMsg>,
        /// Every recorded call fails (after being recorded).
        pub fail: bool,
        /// Only `replies` fails.
        pub replies_fail: bool,
        /// The size `file_info` reports.
        pub file_size: u64,
    }

    impl FakeSlack {
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn record(&self, s: String) -> Result<(), String> {
            self.calls.lock().unwrap().push(s);
            if self.fail {
                Err("slack said no".into())
            } else {
                Ok(())
            }
        }
        fn ts(&self) -> String {
            format!("9.{}", self.next_ts.fetch_add(1, Ordering::SeqCst))
        }
    }

    #[async_trait]
    impl SlackPort for FakeSlack {
        async fn post_message(&self, c: &str, t: &str, th: Option<&str>) -> Result<String, String> {
            self.record(format!("post {c} {} {t}", th.unwrap_or("-")))?;
            Ok(self.ts())
        }
        async fn post_message_no_unfurl(
            &self,
            c: &str,
            t: &str,
            th: Option<&str>,
        ) -> Result<String, String> {
            self.post_message(c, t, th).await
        }
        async fn post_markdown(&self, c: &str, t: &str, th: Option<&str>) -> Result<String, String> {
            self.record(format!("post_md {c} {} {t}", th.unwrap_or("-")))?;
            Ok(self.ts())
        }
        async fn update_message(&self, c: &str, ts: &str, t: &str) -> Result<(), String> {
            self.record(format!("update {c} {ts} {t}"))
        }
        async fn update_markdown(&self, c: &str, ts: &str, t: &str) -> Result<(), String> {
            self.record(format!("update_md {c} {ts} {t}"))
        }
        async fn delete_message(&self, c: &str, ts: &str) -> Result<(), String> {
            self.record(format!("delete {c} {ts}"))
        }
        async fn add_reaction(&self, c: &str, ts: &str, e: &str) -> Result<(), String> {
            self.record(format!("react {c} {ts} {e}"))
        }
        async fn remove_reaction(&self, c: &str, ts: &str, e: &str) -> Result<(), String> {
            self.record(format!("unreact {c} {ts} {e}"))
        }
        async fn flip_to_received(&self, c: &str, ts: &str, _ack: &str) {
            let _ = self.record(format!("received {c} {ts}"));
        }
        async fn post_perm_prompt(
            &self,
            c: &str,
            th: &str,
            id: &str,
            tool: &str,
            _i: &serde_json::Value,
        ) -> Result<String, String> {
            self.record(format!("perm {c} {th} {id} {tool}"))?;
            Ok(self.ts())
        }
        async fn open_dm(&self, u: &str) -> Result<String, String> {
            Ok(format!("D-{u}"))
        }
        async fn history(&self, _c: &str, _l: u16) -> Result<Vec<FetchedMsg>, String> {
            if self.fail {
                return Err("slack said no".into());
            }
            Ok(self.msgs.clone())
        }
        async fn replies(&self, _c: &str, _t: &str, _l: u16) -> Result<Vec<FetchedMsg>, String> {
            if self.fail || self.replies_fail {
                return Err("channel_not_found".into());
            }
            Ok(self.msgs.clone())
        }
        async fn parent_thread_of(&self, _c: &str, _t: &str) -> Option<String> {
            None
        }
        async fn message_at(&self, _c: &str, _t: &str) -> Option<MessageAt> {
            None
        }
        async fn thread_root_gone(&self, _c: &str, _t: &str) -> bool {
            false
        }
        async fn get_permalink(&self, c: &str, ts: &str) -> Result<String, String> {
            self.record(format!("permalink {c} {ts}"))?;
            Ok(format!("https://slack/{c}/{ts}"))
        }
        async fn channel_display_name(&self, c: &str) -> Option<String> {
            self.record(format!("channel_name {c}")).ok()?;
            Some(format!("#{c}"))
        }
        async fn user_display_name(&self, u: &str) -> Option<String> {
            Some(u.to_string())
        }
        async fn auth_test(&self) -> Result<(String, Option<String>), String> {
            Ok(("U_BOT".into(), Some("agentgw".into())))
        }
        /// `UB…` is a bot (→ `B…`); anyone else is a person.
        async fn resolve_bot_id(&self, u: &str) -> Result<Option<String>, String> {
            self.record(format!("bot_id {u}"))?;
            Ok(u.strip_prefix("UB").map(|rest| format!("B{rest}")))
        }
        async fn file_info(&self, _f: &str) -> Result<(String, String, u64), String> {
            Ok(("https://slack/f".into(), "shot.png".into(), self.file_size))
        }
        async fn download_to(&self, _u: &str, _d: &Path) -> Result<(), String> {
            Err("no files in fake".into())
        }
        async fn download_attachment(&self, _f: &str, _d: &Path) -> Result<String, String> {
            Err("no files in fake".into())
        }
        /// Fails like the real one when the file can't be read.
        async fn upload_file(&self, c: &str, th: Option<&str>, p: &Path) -> Result<(), String> {
            std::fs::metadata(p).map_err(|e| format!("read {}: {e}", p.display()))?;
            self.record(format!("upload {c} {} {}", th.unwrap_or("-"), p.display()))
        }
        async fn set_thinking_status(&self, c: &str, th: &str, s: &str) -> Result<(), String> {
            self.record(format!("status {c} {th} {s}"))
        }
    }

    /// Records spawns and deliveries; windows live until terminated.
    #[derive(Default)]
    pub struct FakeAgent {
        pub spawned: Mutex<Vec<SpawnReq>>,
        /// (window id, text)
        pub delivered: Mutex<Vec<(String, String)>>,
        /// Window ids that got an interrupt (Escape).
        pub interrupted: Mutex<Vec<String>>,
        windows: Mutex<Vec<WindowRow>>,
        next: AtomicU64,
    }

    #[async_trait]
    impl AgentPort for FakeAgent {
        fn spawn(&self, req: &SpawnReq) -> Result<Window, String> {
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
        fn session_history_exists(&self, _r: Option<&str>, _s: &str) -> bool {
            false
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
        fn model_alias(&self, _m: &str) -> Option<&'static str> {
            None
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
        fn login_submit_code(&self, _c: &str) -> Result<(), String> {
            Ok(())
        }
        fn login_outcome(&self) -> LoginOutcome {
            LoginOutcome::Success
        }
        async fn logout(&self) -> Result<(), ProbeErr> {
            Ok(())
        }
    }

    pub struct FakeClock(pub AtomicU64);

    impl FakeClock {
        pub fn at(ms: u64) -> Arc<FakeClock> {
            Arc::new(FakeClock(AtomicU64::new(ms)))
        }
        pub fn advance(&self, ms: u64) {
            self.0.fetch_add(ms, Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    #[tokio::test]
    async fn fake_slack_records_calls_in_order() {
        let s = FakeSlack::default();
        let ts = s.post_message("C1", "hello", Some("1.0")).await.unwrap();
        s.add_reaction("C1", &ts, "eyes").await.unwrap();
        assert_eq!(
            s.calls(),
            vec!["post C1 1.0 hello".to_string(), format!("react C1 {ts} eyes")]
        );
    }

    #[test]
    fn fake_clock_moves_only_when_told() {
        let c = FakeClock::at(1_000);
        assert_eq!(c.now_ms(), 1_000);
        c.advance(500);
        assert_eq!(c.now_ms(), 1_500);
    }
}
