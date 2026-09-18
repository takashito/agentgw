//! Tools for keeping processes in tmux. **The word claude does not appear here** —
//! every agent implementation creates and feeds its windows the same way.

/// The tmux session the agents live in. **tmux resolves names by prefix**, so type the
/// full name by hand too (`-t agentgw` hits `agentgw-workers`).
pub const TMUX_SESSION: &str = "agentgw-workers";

/// The pause between typing the text and sending Enter. **Do not shorten it**.
///
/// Sent right away, Enter (CR) lands in the same read as the tail of the text, and the TUI
/// treats it as "more of the paste" and swallows it as a newline. The text plus a trailing
/// newline stays in the input box unsent, UserPromptSubmit never fires, and the thread goes
/// silent (2026-07-30 on a real machine: happened with a 1.3KB envelope).
///
/// Measured (1253B, 5 runs each, with the reader's per-read backlog pinned at 60ms / 250ms):
/// - no pause    → CR mixed into the same read as the text 5/5 (both conditions)
/// - 200ms pause → arrives in its own read 5/5 (both conditions)
///
/// When the reader is idle, CR arrives on its own even without a pause — the race only
/// happens while the TUI is busy drawing a text larger than the pty buffer (1022B), so short
/// envelopes don't reproduce it.
const SETTLE_BEFORE_ENTER: std::time::Duration = std::time::Duration::from_millis(200);

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
        if window.starts_with('@') {
            Window(window.to_string())
        } else {
            Window(format!("{TMUX_SESSION}:{window}"))
        }
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
        crate::bridge::state::LogCtx::default().info(
            "worker",
            &format!("SIGKILL pid {pid} — survived SIGTERM within {grace_ms}ms"),
        );
        self.signal("-9");
        let gone = self.wait_until_gone(KILL_HARD_MS).await;
        if !gone {
            crate::bridge::state::LogCtx::default()
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

/// Injection point for running tmux. Tests use a fake, real runs the real thing.
pub struct Tmux {
    #[allow(clippy::type_complexity)]
    pub run: Box<dyn Fn(&[&str]) -> Result<String, String> + Send + Sync>,
}

impl Tmux {
    /// The real tmux.
    pub fn real() -> Tmux {
        Tmux {
            run: Box::new(|args| {
                let out = std::process::Command::new("tmux")
                    .args(args)
                    .output()
                    .map_err(|e| format!("tmux {args:?}: {e}"))?;
                if !out.status.success() {
                    return Err(format!(
                        "tmux {args:?} failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    ));
                }
                Ok(String::from_utf8_lossy(&out.stdout).to_string())
            }),
        }
    }

    /// Push-in: literal typing → a beat → Enter. 3KB in one go is measured to work.
    pub fn deliver(&self, w: &Window, text: &str) -> Result<(), String> {
        let t = w.as_str();
        // `--` guards against text starting with `-`
        (self.run)(&["send-keys", "-t", t, "-l", "--", text])?;
        std::thread::sleep(SETTLE_BEFORE_ENTER);
        (self.run)(&["send-keys", "-t", t, "Enter"])?;
        Ok(())
    }

    /// The visible text of the window. TUI state (/compact progress, model name) is read from here.
    pub fn capture(&self, w: &Window) -> Result<String, String> {
        (self.run)(&["capture-pane", "-p", "-t", w.as_str()])
    }

    /// Reads including scrollback. `claude auth login` returns to the shell right after printing
    /// "Login successful.", and the marker scrolls off screen before the next poll.
    pub fn capture_history(&self, w: &Window, lines: u32) -> Result<String, String> {
        (self.run)(&[
            "capture-pane",
            "-p",
            "-S",
            &format!("-{lines}"),
            "-t",
            w.as_str(),
        ])
    }

    /// One tmux key name (`Escape` / `Enter` / `BTab` = shift+tab).
    pub fn send_key(&self, w: &Window, key: &str) -> Result<(), String> {
        (self.run)(&["send-keys", "-t", w.as_str(), key]).map(|_| ())
    }

    /// One Escape (cancels the TUI prompt).
    pub fn send_escape(&self, w: &Window) -> Result<(), String> {
        self.send_key(w, "Escape")
    }

    /// One Enter.
    pub fn send_enter(&self, w: &Window) -> Result<(), String> {
        self.send_key(w, "Enter")
    }

    /// Types one line into the TUI (slash commands like `/compact`). Same literal + Enter as `deliver`.
    pub fn send_command(&self, w: &Window, cmd: &str) -> Result<(), String> {
        (self.run)(&["send-keys", "-t", w.as_str(), "-l", "--", cmd])?;
        self.send_enter(w)
    }

    /// Kills the whole window. **By window_id (`@N`)** — names move on rename.
    pub fn kill_window(&self, w: &Window) -> Result<(), String> {
        (self.run)(&["kill-window", "-t", w.as_str()]).map(|_| ())
    }

    /// Creates a window and runs `line` in it. Returns the window_id (`@N`).
    /// The window name gets renamed to the screen title of the program inside, so don't rely on it.
    pub fn spawn(&self, window: &str, cwd: &str, line: &str) -> Result<Window, String> {
        // Create the session first if missing (has-session returns non-zero for "missing", so ignore the Err)
        let id = if (self.run)(&["has-session", "-t", TMUX_SESSION]).is_err() {
            (self.run)(&[
                "new-session",
                "-d",
                "-s",
                TMUX_SESSION,
                "-n",
                window,
                "-c",
                cwd,
                "-P",
                "-F",
                "#{window_id}",
                line,
            ])?
        } else {
            (self.run)(&[
                "new-window",
                "-d",
                "-t",
                TMUX_SESSION,
                "-n",
                window,
                "-c",
                cwd,
                "-P",
                "-F",
                "#{window_id}",
                line,
            ])?
        };
        let id = id.trim().to_string();
        // Lock out renaming so the name fallback keeps working (spawn succeeds even if this fails)
        for opt in ["automatic-rename", "allow-rename"] {
            if let Err(e) = (self.run)(&["set-option", "-w", "-t", &id, opt, "off"]) {
                crate::bridge::state::LogCtx::default()
                    .error("worker", &format!("could not turn off {opt} on {id}: {e}"));
            }
        }
        Ok(Window::of(&id))
    }

    /// Inventory of windows. **tmux is the only authority** — the Bridge's memory is lost on restart, windows stay.
    ///
    /// **The window name goes last** — it can be renamed to a screen title containing spaces,
    /// so everything after the first three fields is read as the name.
    pub fn rows(&self) -> Vec<WindowRow> {
        let Ok(out) = (self.run)(&[
            "list-windows",
            "-t",
            TMUX_SESSION,
            "-F",
            "#{window_id} #{pane_pid} #{pane_current_command} #{window_name}",
        ]) else {
            return Vec::new();
        };
        out.lines()
            .filter_map(|line| {
                let mut f = line.splitn(4, ' ');
                Some(WindowRow {
                    id: f.next()?.to_string(),
                    pid: Pid(f.next()?.trim().parse().ok()?),
                    command: f.next()?.to_string(),
                    name: f.next()?.to_string(),
                })
            })
            .collect()
    }

    /// The pid of the process running in the window. **Looked up from the list-windows listing** —
    /// display-message answers for a nonexistent window with another window's answer and exit 0 (a known trap).
    ///
    /// window_id is the primary key. Name matching is the fallback — if a window can't be
    /// rediscovered after a Bridge restart forgot its id, every attempt respawns and old agents leak.
    pub fn pid_of(&self, window_id: Option<&str>, name: &str) -> Option<Pid> {
        let rows = self.rows();
        rows.iter()
            .find(|r| window_id.is_some_and(|id| id == r.id))
            .or_else(|| rows.iter().find(|r| r.name == name))
            .map(|r| r.pid)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn recording() -> (std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>, Tmux) {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let sink = calls.clone();
        let tmux = Tmux {
            run: Box::new(move |args| {
                sink.lock()
                    .unwrap()
                    .push(args.iter().map(|s| s.to_string()).collect());
                Ok(String::new())
            }),
        };
        (calls, tmux)
    }

    #[test]
    fn deliver_types_then_sends_enter() {
        let (calls, tmux) = recording();
        tmux.deliver(&Window::of("1-1"), "hello").unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "type then Enter: {calls:?}");
        // Put `--` so text starting with `-` still passes as an argument
        assert_eq!(
            calls[0],
            [
                "send-keys",
                "-t",
                "agentgw-workers:1-1",
                "-l",
                "--",
                "hello"
            ]
        );
        assert_eq!(
            calls[1],
            ["send-keys", "-t", "agentgw-workers:1-1", "Enter"]
        );
    }

    #[test]
    fn deliver_targets_a_window_id_directly() {
        let (calls, tmux) = recording();
        tmux.deliver(&Window::of("@42"), "hello").unwrap();
        assert_eq!(
            calls.lock().unwrap()[0][2],
            "@42",
            "window id needs no session prefix"
        );
    }

    /// A fake that just returns list-windows output.
    fn listing_tmux(out: &'static str) -> Tmux {
        Tmux {
            run: Box::new(move |_| Ok(out.to_string())),
        }
    }

    #[test]
    fn finds_the_worker_by_window_id_after_tmux_renames_the_window() {
        // The window name was renamed to claude's screen title (with spaces). Name matching no longer hits.
        let tmux = listing_tmux(
            "@22 3493 claude ✳ building the thing\n@23 5773 claude 1785161156-915759\n",
        );
        assert_eq!(tmux.pid_of(Some("@22"), "2-1-220"), Some(Pid(3493)));
    }

    /// The inventory must read "everything to the end is the name, even with spaces". If this
    /// drifts, a renamed window's session_id gets mixed up and a **live window may be cleaned up**.
    #[test]
    fn the_window_listing_keeps_a_renamed_name_whole() {
        let tmux = listing_tmux(
            "@22 3493 claude w-abc-123\n\
             @23 5773 zsh w-dead-9\n\
             @24 91 claude ✳ building the thing\n\
             @25 92 -bash _anchor\n",
        );
        let rows = tmux.rows();
        assert_eq!(rows.len(), 4);

        // Agent windows = only names starting with `w-`
        let ids: Vec<Option<&str>> = rows.iter().map(|r| r.session_id()).collect();
        assert_eq!(ids, [Some("abc-123"), Some("dead-9"), None, None]);

        // Is claude there, or is it a shell-only husk
        let shells: Vec<bool> = rows.iter().map(|r| r.is_empty_shell()).collect();
        assert_eq!(
            shells,
            [false, true, false, true],
            "ログインシェルの `-bash` も殻"
        );

        assert_eq!(rows[2].name, "✳ building the thing", "空白ごと名前");
        assert_eq!(rows[0].pid, Pid(3493));
    }

    #[test]
    fn falls_back_to_the_window_name_when_the_id_is_unknown() {
        // After a Bridge restart: if the window can't be found without its id, respawn leaks
        let tmux = listing_tmux("@22 3493 claude 2-1-220\n@23 5773 claude other\n");
        assert_eq!(tmux.pid_of(None, "2-1-220"), Some(Pid(3493)));
        assert_eq!(tmux.pid_of(Some("@99"), "nope"), None);
    }

    #[test]
    fn spawn_returns_the_window_id_and_forbids_renaming() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let sink = calls.clone();
        let tmux = Tmux {
            run: Box::new(move |args| {
                sink.lock()
                    .unwrap()
                    .push(args.iter().map(|s| s.to_string()).collect());
                Ok(if args[0] == "new-window" {
                    "@42\n".to_string()
                } else {
                    String::new()
                })
            }),
        };
        let w = tmux.spawn("1-1", "/repo", "the launch line").unwrap();
        assert_eq!(w.as_str(), "@42");
        let calls = calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|c| c == &["set-option", "-w", "-t", "@42", "automatic-rename", "off"]),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c == &["set-option", "-w", "-t", "@42", "allow-rename", "off"]),
            "{calls:?}"
        );
    }

    #[tokio::test]
    async fn kill_pid_graceful_returns_false_for_dead_pid() {
        // A pid that was never there is not "taken down"
        assert!(!Pid(4_000_000).kill_graceful(100).await);
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
