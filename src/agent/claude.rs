//! The claude process — launch line / liveness / sign-in session / reading the screen and output.
//! **This is the only file where the word claude may appear.**
//!
//! The hooks / mcp settings files are written by the Bridge on every spawn — this type only holds
//! its tool (tmux) and receives the paths through [`SpawnReq`].
//!
//! Reading the transcript (`Transcript`) lives here too. The screen (`Pane`) and model id (`ModelId`) are in `screen.rs`.
//! **It holds no Slack-facing text at all** — rendering is the Bridge's job (`command.rs`).

use super::SpawnOutcome;
use super::screen::{Pane, SpawnScreen};
use super::tmux::Tmux;
use super::{Pid, Window, WindowRow};
use super::{CompactOutcome, CompactProgress, LimitHit, LoginOutcome, ProbeErr, SpawnReq};
use crate::agent::WorkerState;
use crate::log::LogCtx;
use crate::state_dir::StateDir;
use crate::chat::ThreadKey;
use crate::clock::WallClock;
use std::time::Duration;

/// A fresh ID per spawn. claude refuses to start with an ID that was already used (measured in the spike).
pub enum SessionMode {
    New(String),
    Resume(String),
}

impl SessionMode {
    fn session_id(&self) -> &str {
        match self {
            SessionMode::New(s) | SessionMode::Resume(s) => s,
        }
    }

    fn flag(&self) -> String {
        match self {
            SessionMode::New(s) => format!("--session-id {s}"),
            SessionMode::Resume(s) => format!("--resume {s}"),
        }
    }
}

/// Session name that does not collide with the old `slack-login`.
const LOGIN_SESSION: &str = "slack-login-rs";

/// Orientation for a NEW thread. Verbatim copy — do not change a single character.
const NEW_PENDING_PREFIX: &str = "This is a NEW Slack thread; the message(s) below are the first you have received — \
     treat them as one burst (a single reply may cover several; a later one may correct an \
     earlier). Reply to them now, then wait for pushed messages; do not poll.";

/// Orientation for RESUME. Verbatim copy — do not change a single character.
const RESUME_PENDING_PREFIX: &str = "You are RESUMING this Slack thread; the previous worker process was replaced and your \
     bridge/MCP is freshly reconnected, so agentgw tools work NOW. The message(s) below \
     arrived while the thread was down and are the ones to handle now — treat them as one burst \
     (a single reply may cover several; a later one may correct an earlier). Reply to them now, \
     then wait for pushed messages; do not poll.";

/// Idle prompt for an agent started with an empty backlog. Verbatim copy — do not change a single character.
const WORKER_STARTUP_PROMPT: &str = "You are a Slack thread worker. There is no message waiting right now — wait for messages \
     to be pushed to you and reply when they arrive. Do not poll and do not call any startup tool.";

/// How many times to press Enter again when the delivered body is still in the input box, and the pause between.
/// Same idea as the slash commands' `SUBMIT_RETRY_CAP`, only the target is the body.
///
/// **Four presses (≈1s) is less than a busy TUI takes.** A pane measured on 2026-09-22 while a turn ran
/// took longer than that to swallow a body, and the box still holding the text was then read as a failed
/// delivery: the message stayed queued and the tick **typed the whole body in again**, stacking copies
/// (one thread received the same request four times).
const DELIVER_SUBMIT_RETRIES: u32 = 9;
const DELIVER_SUBMIT_POLL: Duration = Duration::from_millis(200);

/// How many cursor moves an answer to a dialog may take, and the pause after each press.
///
/// One press per read: the cursor is the only proof the key landed. The cap is a few more than
/// any list seen so far -- if the cursor is not where it was aimed by then the screen is not
/// what we think it is, and the answer is refused rather than confirmed blind.
const DIALOG_MOVE_CAP: usize = 12;
const DIALOG_MOVE_POLL: Duration = Duration::from_millis(80);

/// How much of a transcript's end to read when asking when it last moved. One entry is a few hundred
/// bytes at most, so 64KB always spans several.
const ACTIVITY_TAIL: u64 = 64 * 1024;

/// Levels `/effort` accepts (checked on a real machine 2026-07-17: the slider's 5 steps + `ultracode` / `auto`).
const EFFORT_LEVELS: [&str; 7] = ["low", "medium", "high", "xhigh", "max", "ultracode", "auto"];

/// Permission modes `mode` accepts. Only the 4 reachable by cycling shift+tab are arguments —
/// `bypass` / `don't ask` are only entered through settings, and nothing we want Slack to trigger.
const MODE_NAMES: [&str; 4] = ["manual", "plan", "edit", "auto"];

/// Keeps claude in a tmux window.
pub struct Claude {
    tmux: Tmux,
}

impl Claude {
    pub fn new(tmux: Tmux) -> Self {
        Self { tmux }
    }

    /// Claude on the real tmux.
    pub fn real() -> Self {
        Self::new(Tmux::real())
    }

    /// Only for the argv-pinning tests in this file. **The production path never goes through here** — only
    /// `Claude`'s methods (`deliver` / `capture` / `login_*` / `drive` …) poke the window.
    #[cfg(test)]
    fn tmux(&self) -> &Tmux {
        &self.tmux
    }

    /// The launch line of the claude to spawn.
    fn launch_line(
        &self,
        mode: &SessionMode,
        hooks_file: &str,
        mcp_config: &str,
        prompt: &str,
    ) -> String {
        format!(
            // --strict-mcp-config: do not load MCP from the production plugin (isolates dev agents)
            "AGENTGW_SESSION_ID={} claude --settings {hooks_file} --mcp-config {mcp_config} \
             --strict-mcp-config {} {}",
            mode.session_id(),
            mode.flag(),
            Self::single_quote(prompt)
        )
    }

    /// Shell single-quoting. A `'` inside is closed and reopened as `'\''`.
    fn single_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    /// Bundles the first message into the spawn prompt (structurally avoids the delivery race).
    /// Envelopes are joined with one blank line (`\n\n`).
    fn spawn_prompt(&self, envelope: &str, mode: &SessionMode) -> String {
        let prefix = match mode {
            SessionMode::New(_) => NEW_PENDING_PREFIX,
            SessionMode::Resume(_) => RESUME_PENDING_PREFIX,
        };
        format!("{prefix}\n\n{envelope}")
    }

    /// What a conversation carried here from another machine is told first.
    ///
    /// **Every path in its own record is the old machine's.** It will read that record as its own
    /// memory and act on what it says about files, so it has to know the ground under those paths
    /// has changed — a checkout here may be at a different commit, or missing entirely.
    fn moved_notice(m: &crate::bridge::state::MovedFrom, here: &str) -> String {
        format!(
            "This conversation was running on the machine `{}`, in `{}`. It has been moved: you are \
             on a different machine now, working in `{here}`. Everything written above happened in \
             the old place, so **every path in it belongs to that machine** — the same file is not \
             promised to exist here, or to hold the same thing if it does. Before acting on anything \
             the record says about a file, look at it here first, and say plainly if what you find \
             does not match.",
            m.machine, m.path,
        )
    }

    /// A pool agent has no body waiting for delivery — it always starts on the idle prompt.
    fn pool_prompt(&self) -> &'static str {
        WORKER_STARTUP_PROMPT
    }

    /// Starts claude in a window. **Only when Absent** — spawn never kills.
    /// Returns the window_id (`@N`). The window name gets renamed by claude's screen title, so it is not relied on.
    pub fn spawn(&self, req: &SpawnReq) -> Result<Window, String> {
        if req.state != WorkerState::Absent {
            return Err(format!(
                "seat occupied ({:?}) — refusing to spawn over a live worker",
                req.state
            ));
        }
        let mode = match &req.resume_from {
            Some(id) => SessionMode::Resume(id.as_str().to_string()),
            None => SessionMode::New(req.session_id.as_str().to_string()),
        };
        // No body (= warming a pool agent) uses the idle prompt; otherwise an envelope with the orientation prepended
        let mut prompt = match &req.prompt {
            Some(envelope) => self.spawn_prompt(envelope, &mode),
            None => self.pool_prompt().to_string(),
        };
        // **Before the first message, not after.** It has to know the paths in its own record belong
        // to another machine before it acts on any of them
        if let Some(m) = &req.moved_from {
            prompt = format!(
                "{}\n\n{prompt}",
                Self::moved_notice(m, &req.cwd)
            );
        }
        let line = self.launch_line(&mode, &req.hooks_file, &req.mcp_config, &prompt);
        // Settle the trust confirmation without answering on screen (the official way). Failing to write does not stop startup —
        // if the confirmation appears, the startup-screen watcher answers it
        match Self::pre_answer_dialogs(&req.cwd) {
            Ok(written) if !written.is_empty() => LogCtx::default().info(
                "spawn",
                &format!("recorded in ~/.claude.json (no dialog): {}", written.join(", ")),
            ),
            Ok(_) => {}
            Err(e) => LogCtx::default().error("spawn", &format!("could not pre-answer dialogs for {}: {e}", req.cwd)),
        }
        self.tmux.spawn(&req.window, &req.cwd, &line)
    }

    /// Records in `~/.claude.json` the answers to the dialogs Claude Code would otherwise put on
    /// the startup screen, the way Claude Code itself records them:
    ///
    /// - `projects["<repo root>"].hasTrustDialogAccepted` — the workspace-trust confirmation
    ///   (the documented way; keyed on the repository root, and never persisted for the home directory)
    /// - `autoModeEnvSetup.dismissed` — "Teach auto mode about your environment?". Measured on a
    ///   real machine 2026-09-22: an older `dismissedAt` is no longer read and the modal came back,
    ///   holding a queued message for minutes; answering "Don't show again" writes `dismissed`.
    ///
    /// Returns what it wrote, for the log. These are Claude Code's own keys and it may rename them
    /// again — when that happens the dialog simply shows up as before, and the startup-screen
    /// watcher is still there to answer it.
    fn pre_answer_dialogs(cwd: &str) -> Result<Vec<String>, String> {
        // Tests spawn with made-up folders: never touch the real user's config from a test
        if cfg!(test) {
            return Ok(Vec::new());
        }
        let home = std::env::var("HOME").map_err(|e| format!("HOME: {e}"))?;
        let path = std::path::Path::new(&home).join(".claude.json");
        let config: serde_json::Value = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let (mut updated, mut written) = (config.clone(), Vec::new());
        if let Some(key) = Self::trust_key(std::path::Path::new(cwd), std::path::Path::new(&home))
            && let Some(next) = Self::with_trust(&updated, &key.to_string_lossy())
        {
            updated = next;
            written.push(format!("trust for {}", key.display()));
        }
        if let Some(next) = Self::with_auto_mode_dismissed(&updated) {
            updated = next;
            written.push("auto mode environment setup dismissed".to_string());
        }
        if written.is_empty() {
            return Ok(Vec::new());
        }
        // Claude Code rewrites this file too: write a sibling and rename, so a reader never
        // sees half a file. Keep the original's permissions (it is private to the user).
        let tmp = path.with_extension("json.agentgw-tmp");
        let body = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
        if let Ok(meta) = std::fs::metadata(&path) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(written)
    }

    /// Where Claude Code keys the trust for `cwd`: the git repository root when `cwd` is inside
    /// one, else `cwd` itself. `None` for the home directory (never persisted).
    pub fn trust_key(cwd: &std::path::Path, home: &std::path::Path) -> Option<std::path::PathBuf> {
        if cwd == home {
            return None;
        }
        let root = cwd.ancestors().find(|dir| dir.join(".git").exists()).unwrap_or(cwd);
        (root != home).then(|| root.to_path_buf())
    }

    /// `config` with `projects[key].hasTrustDialogAccepted = true`, or `None` if already so.
    pub fn with_trust(config: &serde_json::Value, key: &str) -> Option<serde_json::Value> {
        if config["projects"][key]["hasTrustDialogAccepted"] == serde_json::Value::Bool(true) {
            return None;
        }
        let mut updated = config.clone();
        let root = updated.as_object_mut()?;
        let projects = root.entry("projects").or_insert_with(|| serde_json::json!({}));
        let project = projects.as_object_mut()?.entry(key).or_insert_with(|| serde_json::json!({}));
        project.as_object_mut()?.insert("hasTrustDialogAccepted".into(), serde_json::Value::Bool(true));
        Some(updated)
    }

    /// `config` with `autoModeEnvSetup.dismissed = true`, or `None` if it already says so.
    /// The sibling `denials` / `dismissedAt` that Claude Code keeps there are left untouched.
    pub fn with_auto_mode_dismissed(config: &serde_json::Value) -> Option<serde_json::Value> {
        if config["autoModeEnvSetup"]["dismissed"] == serde_json::Value::Bool(true) {
            return None;
        }
        let mut updated = config.clone();
        let root = updated.as_object_mut()?;
        let setup = root
            .entry("autoModeEnvSetup")
            .or_insert_with(|| serde_json::json!({}));
        setup
            .as_object_mut()?
            .insert("dismissed".into(), serde_json::Value::Bool(true));
        Some(updated)
    }

    /// Pushing the keys in is tmux's job. **Confirming it was submitted is ours** — only this file
    /// knows the input box (`❯`).
    ///
    /// The pause in `Tmux::deliver` is 200ms, measured with 1.3KB, and **not enough for long envelopes**
    /// (real machine 2026-08-02: at 4017B / 76 lines the Enter was swallowed by the TUI still ingesting,
    /// became a trailing newline of the body and stayed in the input box). Nothing was submitted, so
    /// UserPromptSubmit never fired, and the Bridge recorded send-keys success as delivery success and went silent for 40 minutes.
    ///
    /// Lengthening the pause is only guesswork — what stalls is the TUI's rendering, which varies with length and load.
    /// **Pressing Enter again until the input box is empty** is the only verifiable form (same as `send_command`).
    pub fn deliver(&self, w: &Window, text: &str) -> Result<(), String> {
        // **Before typing, make sure the window can take keys.** send-keys into a window showing a modal
        // not only fails to arrive, the body becomes input to the modal — in a choice list, digits in the body select,
        // and the following Enter confirms. Counting the retries (the tick's redelivery), it picks before a human answers.
        // If the screen cannot be read, proceed (unreadable is not evidence of a modal; as before).
        if let Ok(pane) = self.tmux.capture(w)
            && let Some(why) = Self::not_accepting_keys(&pane)
        {
            return Err(why.to_string());
        }
        self.tmux.deliver(w, text)?;
        for attempt in 0..=DELIVER_SUBMIT_RETRIES {
            std::thread::sleep(DELIVER_SUBMIT_POLL);
            // If the screen cannot be read, do not press — dropping a stray Enter into a window we cannot see is riskier
            let Ok(pane) = self.tmux.capture(w) else {
                return Ok(());
            };
            // A modal that appears mid-send is caught here too. Pressing again would **confirm the dialog**,
            // so back out without firing a single key
            if let Some(why) = Self::not_accepting_keys(&pane) {
                return Err(why.to_string());
            }
            if Pane::new(&pane).input_box_empty() {
                return Ok(());
            }
            if attempt < DELIVER_SUBMIT_RETRIES {
                self.tmux.send_enter(w)?;
            }
        }
        // **A full box is not proof of failure, so it is not reported as one.** A running turn takes keys
        // (measured 2026-09-22: a pane mid-turn showed `✶ …ing… (2m 7s …)` over an *empty* box, the text
        // having gone in), it is only slower to swallow them. Reporting failure here kept the message
        // queued, and the queue types the body in again — which is how the same request arrived four times.
        //
        // **Receipt is settled where it is actually known**: the `user_prompt` hook, or, for a body taken
        // as mid-turn steering (which fires no hook), `Bridge::scan_transcripts`. A body that never gets
        // in is caught by [`crate::bridge::Bridge::warn_unreceived`] — once, without retyping.
        Ok(())
    }

    /// Whether this window can accept typing. If not, returns **a reason a human can be shown**.
    ///
    /// **The presence of `❯` does not decide it.** TUI shapes checked against 3 real screens on 2026-08-18:
    ///
    /// | Screen | Last `❯` line | `Esc to cancel` |
    /// |---|---|---|
    /// | Normal (idle) | `❯ ` (empty) | none |
    /// | auto mode onboarding | `❯ ` (empty — the modal comes up **with the box still alive**) | present |
    /// | `/model` selector | `❯ 2. Opus …` (**`❯` is taken over by the selection cursor**) | present |
    ///
    /// The first implementation assumed "the `❯` line disappears" and **fired for neither modal**.
    /// Worse, for the `/model` kind `input_box_empty()` reads "body still there", so the retry
    /// Enter confirms the dialog. The only thing both share is the cancel hint line — a different string
    /// from the running `esc to interrupt`, so it tells them apart.
    ///
    /// A screen with no `❯` at all is refused too. Without a visible box we cannot say it was sent (an unknown modal).
    fn not_accepting_keys(pane: &str) -> Option<String> {
        let p = Pane::new(pane);
        if let Some(footer) = p.modal_footer() {
            // **Written for the person waiting in the thread**, not for the log: they can act on
            // "answer the dialog", not on a window id or a footer string. The wording is the
            // screen's own — modals we don't know by name are exactly the ones that get stuck
            let title = p
                .dialog()
                .map_or_else(|| footer.trim().to_string(), |d| d.title);
            return Some(crate::t!(
                "the agent is waiting on a dialog ({title}) — answer it on its screen and this goes through",
                "エージェントの画面で確認待ちになっています（{title}）。画面で答えると、これはそのまま渡ります"
            ));
        }
        p.input_line().is_empty().then(|| {
            crate::t!(
                "the agent's input box is not on screen",
                "エージェントの入力欄が画面に見当たりません"
            )
        })
    }

    /// The modal on this window's screen, if any. Reads it; presses nothing.
    pub fn dialog(&self, w: &Window) -> Option<crate::agent::Dialog> {
        Pane::new(&self.tmux.capture(w).ok()?).dialog()
    }

    /// Walks the cursor onto `choice` and confirms it, or cancels with Escape (`None`).
    ///
    /// **One press, then look.** The screen is read again after every key, so a modal that has
    /// gone away — someone answered it on the machine — stops this instead of leaking an Enter
    /// into the input box underneath. Up and Down are both used rather than running off the end
    /// of the list, which not every list wraps around.
    pub async fn answer_dialog(
        &self,
        w: &Window,
        choice: Option<usize>,
        ctx: &LogCtx,
    ) -> Result<(), String> {
        let Some(want) = choice else {
            ctx.info("worker", &format!("{w}: dialog cancelled (Escape)"));
            return self.tmux.send_escape(w);
        };
        for _ in 0..DIALOG_MOVE_CAP {
            let Some(d) = self.dialog(w) else {
                return Err(crate::t!(
                    "the dialog is no longer on the agent's screen",
                    "エージェントの画面にその確認はもうありません"
                ));
            };
            if want >= d.options.len() {
                return Err(crate::t!(
                    "that choice is not on the dialog any more",
                    "その選択肢はもう画面にありません"
                ));
            }
            if d.selected == want {
                ctx.info(
                    "worker",
                    &format!("{w}: dialog answered with {:?}", d.options[want]),
                );
                return self.tmux.send_enter(w);
            }
            let key = if want > d.selected { "Down" } else { "Up" };
            self.tmux.send_key(w, key)?;
            tokio::time::sleep(DIALOG_MOVE_POLL).await;
        }
        Err(crate::t!(
            "the dialog's cursor would not move onto that choice",
            "画面の選択位置をそこまで動かせませんでした"
        ))
    }

    pub fn terminate(&self, w: &Window) -> Result<(), String> {
        self.tmux.kill_window(w)
    }

    /// The agent's own process. **The only proof of life** — without a connector, an agent
    /// killed by `kill -9`, which sends no session_end, can only be noticed here.
    pub fn pid_of(&self, window_id: Option<&str>, window_name: &str) -> Option<Pid> {
        self.tmux.pid_of(window_id, window_name)
    }

    /// Captures one pane. On failure, logs and returns an empty string — a capture failure is swallowed
    /// and the check continues (do not give up a command over one miss).
    fn capture(&self, w: &Window, label: &str, ctx: &LogCtx) -> String {
        self.tmux.capture(w).unwrap_or_else(|e| {
            ctx.error("bridge", &format!("{label}: capture-pane {w} failed: {e}"));
            String::new()
        })
    }

    /// Target of the sign-in session. It is not an agent-session window, so it is not qualified.
    fn login_window(&self) -> Window {
        Window::raw(LOGIN_SESSION)
    }

    /// The sign-in session. The wide `-x 400` pane makes the auth URL print on one line
    /// (wrapped, the URL cannot be picked up).
    fn login_start(&self, cwd: &str) -> Result<(), String> {
        (self.tmux.run)(&[
            "new-session",
            "-d",
            "-s",
            LOGIN_SESSION,
            "-x",
            "400",
            "-y",
            "50",
            "-c",
            cwd,
        ])
        .map(|_| ())
    }

    /// Cleanup. If absent tmux just exits non-zero — that means "nothing to clean", so stay quiet.
    pub fn login_kill(&self) {
        let _ = (self.tmux.run)(&["kill-session", "-t", LOGIN_SESSION]);
    }

    /// Types one line into the TUI and confirms on screen that it took effect.
    ///
    /// Lesson learned: getting both questions wrong costs a 20-second miss per command.
    /// Ask "is it still in the input?" of the **input box**, not the whole pane (a submitted command
    /// stays on screen as echo). For "did the confirmation appear?", ignore freshness and ask only **whether a line
    /// naming the requested target is on screen** (if an old line already names that target, the agent is already there).
    ///
    /// **TUI drift** (real machine 2026-07-29): the TUI may insert a confirmation dialog before `/effort <level>`
    /// (sessions with history). Waiting shows nothing until Enter confirms it,
    /// so it is pressed here. TUIs as of 2026-07-17 did not have this screen.
    async fn drive(
        &self,
        target: &Window,
        cmd: &str,
        confirmed: impl Fn(&str) -> bool,
        label: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) -> bool {
        const MAX_MS: u64 = 20_000;
        const POLL: Duration = Duration::from_millis(400);
        const SUBMIT_RETRY_CAP: u32 = 4;
        /// Enter on a dialog is counted separately from submit retries — it presses a button, not the input,
        /// with slack for the one miss during the redraw after confirming.
        const DIALOG_CONFIRM_CAP: u32 = 2;
        let tmux = &self.tmux;
        ctx.info(
            "bridge",
            &format!("{label}: sending {cmd} to worker window {target} key={key}"),
        );
        if let Err(e) = tmux.send_command(target, cmd) {
            ctx.error(
                "bridge",
                &format!("{label}: send-keys {cmd} to {target} failed: {e}"),
            );
            return false;
        }
        // If anything stays in the input box, it is this word — `/effort`
        let word = cmd.split(' ').next().unwrap_or(cmd);
        let started = std::time::Instant::now();
        let mut retries = 0;
        let mut dialog_confirms = 0;
        // Do not check before the TUI has finished processing the Enter send_command already sent
        tokio::time::sleep(Duration::from_millis(800)).await;
        while started.elapsed().as_millis() as u64 <= MAX_MS {
            let pane = self.capture(target, label, ctx);
            let unsubmitted = Pane::new(&pane).still_has_command(word);
            if !unsubmitted && confirmed(&pane) {
                ctx.info(
                    "bridge",
                    &format!(
                        "{label}: done key={key} after {}ms",
                        started.elapsed().as_millis()
                    ),
                );
                return true;
            }
            if unsubmitted && retries < SUBMIT_RETRY_CAP {
                retries += 1;
                ctx.info(
                    "bridge",
                    &format!(
                        "{label}: {word} still un-submitted — pressing Enter \
                         ({retries}/{SUBMIT_RETRY_CAP}) key={key}"
                    ),
                );
                if let Err(e) = tmux.send_enter(target) {
                    ctx.debug(
                        "bridge",
                        &format!("{label}: submit Enter failed key={key}: {e}"),
                    );
                }
            } else if Pane::new(&pane).effort_confirm_dialog_open()
                && dialog_confirms < DIALOG_CONFIRM_CAP
            {
                // Confirmation dialog (real machine 2026-07-29). The default choice is "Yes", so Enter confirms.
                // When we get here `unsubmitted` is always false — the input is already empty and the `❯` line is a choice
                // (`/model` does not show a dialog for now, but if it does it is caught in the same place)
                dialog_confirms += 1;
                ctx.info(
                    "bridge",
                    &format!(
                        "{label}: confirming the effort-change dialog (Enter) \
                         ({dialog_confirms}/{DIALOG_CONFIRM_CAP}) key={key}"
                    ),
                );
                if let Err(e) = tmux.send_enter(target) {
                    ctx.debug(
                        "bridge",
                        &format!("{label}: dialog Enter failed key={key}: {e}"),
                    );
                }
            }
            tokio::time::sleep(POLL).await;
        }
        // Restore the screen before giving up — leaving a half-typed command or an open dialog
        // lets the next message get eaten by the choices
        if let Err(e) = tmux.send_escape(target) {
            ctx.debug(
                "bridge",
                &format!("{label}: cleanup Escape failed key={key}: {e}"),
            );
        }
        ctx.error(
            "bridge",
            &format!(
                "{label}: timed out after {MAX_MS}ms without a confirmation for {cmd} key={key}"
            ),
        );
        false
    }

    /// Watches a freshly started window and answers the screens it can answer.
    ///
    /// **This is the only responder.** There is no other startup deadline (only the user_prompt hook
    /// clears the `starting` latch), so a window stopped in front of a screen stays "starting" forever and
    /// piles deliveries into the queue. On a real machine on 2026-08-02: messages piled up behind the trust dialog and
    /// the Bridge went silent.
    ///
    /// It **keeps watching** until the deadline because screens sometimes appear late
    /// (the reason for the linger). This runs as a background task from the start,
    /// so a single watch covers what could otherwise be split into a sync wait and a background watch.
    pub async fn watch_spawn_screens(
        &self,
        w: &Window,
        budget_ms: u64,
        poll_ms: u64,
        ctx: &LogCtx,
    ) -> SpawnOutcome {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(budget_ms);
        let mut answered = false;
        let mut trust_answered = false;
        // How many times Down moved to Yes. A cap so we do not keep selecting if the screen layout changes again
        let mut trust_moves = 0;
        // **confirm needs a latch too.** The watch is not folded up after answering confirm,
        // so without a latch it would keep firing Enter every second
        // until the screen goes away
        let mut confirm_answered = false;
        while std::time::Instant::now() < deadline {
            // **Do not watch a window we cannot read.** If the window is gone (the agent exited), no screen to answer
            // will appear. Reading on would pile up one error line per round
            let pane = match self.tmux.capture(w) {
                Ok(pane) => pane,
                Err(e) => {
                    ctx.info("spawn", &format!("{w}: window is gone ({e}) — ending the watch"));
                    return SpawnOutcome::Exited;
                }
            };
            match Pane::new(&pane).spawn_screen() {
                // A screen that firing does not clear. Firing would send the input box's contents, so back out without firing
                SpawnScreen::LoginRequired => {
                    ctx.error(
                        "spawn",
                        &format!(
                            "{w}: pane reads like the LOGIN screen — abandoning the watch \
                             (no key clears it)"
                        ),
                    );
                    return SpawnOutcome::LoginRequired;
                }
                SpawnScreen::UsageLimited => {
                    ctx.error(
                        "spawn",
                        &format!(
                            "{w}: pane reads like the USAGE-LIMIT modal — abandoning the watch \
                             (no key clears it)"
                        ),
                    );
                    return SpawnOutcome::UsageLimited;
                }
                // Do not fire repeatedly while the same screen stays.
                // **Before Enter, confirm Yes is the one selected** — 2.1.276 shows it with No selected first,
                // so Enter alone chooses exit (2026-09-18: on one machine it exited immediately every time)
                SpawnScreen::Trust if !trust_answered => {
                    match Pane::new(&pane).trust_selected_is_yes() {
                        Some(false) if trust_moves < TRUST_MOVES_MAX => {
                            trust_moves += 1;
                            if let Err(e) = self.tmux.send_key(w, "Down") {
                                ctx.error("spawn", &format!("{w}: trust dialog Down failed: {e}"));
                            }
                            ctx.info("spawn", &format!("{w}: workspace-trust dialog has No selected — moving to Yes"));
                        }
                        Some(false) => {
                            ctx.error(
                                "spawn",
                                &format!("{w}: workspace-trust dialog: could not select Yes — not answering"),
                            );
                            trust_answered = true;
                        }
                        _ => {
                            if let Err(e) = self.tmux.send_enter(w) {
                                ctx.error("spawn", &format!("{w}: trust dialog Enter failed: {e}"));
                            }
                            trust_answered = true;
                            answered = true;
                            ctx.info(
                                "spawn",
                                &format!("{w}: workspace-trust dialog seen — accepted"),
                            );
                        }
                    }
                }
                SpawnScreen::Trust => {}
                SpawnScreen::Confirm => {
                    if !confirm_answered {
                        if let Err(e) = self.tmux.send_enter(w) {
                            ctx.error("spawn", &format!("{w}: confirm prompt Enter failed: {e}"));
                        }
                        confirm_answered = true;
                        answered = true;
                        ctx.info("spawn", &format!("{w}: dev-channels confirm prompt seen"));
                    }
                }
                SpawnScreen::None_ => {}
            }
            tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
        }
        if answered {
            SpawnOutcome::Answered
        } else {
            SpawnOutcome::NoScreen
        }
    }

    /// Types `/model <name>`. Returns whether the screen said it is on the requested model.
    /// Move a running agent to another folder **on this machine**, with Claude Code's own `/cd`.
    ///
    /// It keeps the conversation, loads the new folder's `CLAUDE.md` and settings, and files the
    /// session where `--resume` finds it — all the work a hand-rolled move would have to redo, badly
    /// (the record would have to be copied to a directory whose name we can only guess at).
    ///
    /// Confirmed by the folder appearing on screen. A `/cd` that is refused — an untrusted folder
    /// the person declines, or one a `Cd` permission rule forbids — leaves the agent where it is,
    /// and so leaves this `false`.
    pub async fn cd(&self, target: &Window, path: &str, key: &ThreadKey, ctx: &LogCtx) -> bool {
        // The tail is enough and survives the TUI shortening a long path from the left
        let tail: String = path.chars().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect();
        self.drive(
            target,
            &format!("/cd {path}"),
            |pane| pane.contains(&tail),
            "cd",
            key,
            ctx,
        )
        .await
    }

    pub async fn set_model(
        &self,
        target: &Window,
        name: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) -> bool {
        self.drive(
            target,
            &format!("/model {name}"),
            |pane| Pane::new(pane).model_confirmed(name),
            "model",
            key,
            ctx,
        )
        .await
    }

    /// Types `/effort <level>`.
    ///
    /// The path through the dialog (sessions with history) does not print `Set effort level to …` after
    /// confirming — only the status line appears, so if that line names the requested level we read it as applied
    /// (real machine 2026-07-29).
    pub async fn set_effort(
        &self,
        target: &Window,
        level: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) -> bool {
        self.drive(
            target,
            &format!("/effort {level}"),
            |pane| {
                let p = Pane::new(pane);
                p.effort_confirmed(level) || p.effort_status() == Some(level)
            },
            "effort",
            key,
            ctx,
        )
        .await
    }

    /// The current permission mode. Reads the footer once — fires no keys, so safe mid-turn.
    pub fn mode(&self, target: &Window, ctx: &LogCtx) -> &'static str {
        Pane::new(&self.capture(target, "mode", ctx)).mode_status()
    }

    /// Presses shift+tab (tmux `BTab`) until reaching the target mode.
    ///
    /// **Do not rely on the cycle's order** — how many steps there are depends on settings (bypass / don't ask
    /// come and go). Just press once and read, for one full cycle.
    pub async fn set_mode(
        &self,
        target: &Window,
        want: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) -> bool {
        /// The cycle has at most 6 steps (manual/plan/edit/auto/bypass/don't ask) — stop after one full round.
        const MAX_PRESSES: u32 = 6;
        const SETTLE: Duration = Duration::from_millis(400);
        for i in 0..=MAX_PRESSES {
            let now = self.mode(target, ctx);
            if now == want {
                ctx.info(
                    "bridge",
                    &format!("mode: {want} after {i} shift+tab press(es) key={key}"),
                );
                return true;
            }
            if i == MAX_PRESSES {
                ctx.error(
                    "bridge",
                    &format!(
                        "mode: cycled {MAX_PRESSES} times without reaching {want} \
                         (stuck at {now}) key={key}"
                    ),
                );
                return false;
            }
            if let Err(e) = self.tmux.send_key(target, "BTab") {
                ctx.error(
                    "bridge",
                    &format!("mode: send-keys BTab to {target} failed: {e}"),
                );
                return false;
            }
            tokio::time::sleep(SETTLE).await;
        }
        unreachable!("the loop always returns at i == MAX_PRESSES")
    }

    /// Types `/compact` and forwards the pane's spinner to `progress` one by one
    ///
    /// **Posts nothing to Slack** — whether to create or edit a progress message, and its text, is the Bridge's call.
    /// This returns only the outcome. It can take up to 6 minutes, so the caller runs it outside the select loop.
    ///
    /// The sender is dropped when this returns, which ends the caller's receiving loop.
    pub async fn compact(
        &self,
        target: &Window,
        key: &ThreadKey,
        sid: &str,
        progress: tokio::sync::mpsc::Sender<CompactProgress>,
    ) -> CompactOutcome {
        const MAX_MS: u64 = 6 * 60_000; // ceiling so a stuck compaction does not poll forever
        const POLL: Duration = Duration::from_millis(800);
        const SUBMIT_RETRY_CAP: u32 = 4;
        let ctx = LogCtx {
            session_id: Some(sid.to_string()),
            thread_key: Some(key.clone()),
        };
        let tmux = &self.tmux;
        let short: String = sid.chars().take(8).collect();
        // A `Compacted (ctrl+o …)` left by an earlier compaction stays on screen — note it first and only read a **new**
        // marker as completion
        let baseline = tmux.capture(target).unwrap_or_default();
        let stale_done = baseline.to_lowercase().contains("compacted (ctrl+o");
        ctx.info(
            "bridge",
            &format!(
                "compact: sending /compact to worker window {target} (session {short}) key={key}"
            ),
        );
        if let Err(e) = tmux.send_command(target, "/compact") {
            ctx.error(
                "bridge",
                &format!("compact: send-keys /compact to {target} failed: {e}"),
            );
            return CompactOutcome::Failed;
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
        let started = std::time::Instant::now();
        let mut retries = 0;
        let mut seen = false;
        loop {
            if started.elapsed().as_millis() as u64 > MAX_MS {
                ctx.error(
                    "bridge",
                    &format!("compact: timed out after {MAX_MS}ms (seen={seen}) key={key}"),
                );
                break CompactOutcome::Failed;
            }
            let pane = self.capture(target, "compact", &ctx);
            let lower = pane.to_lowercase();
            if lower.contains("not enough messages to compact") {
                ctx.info(
                    "bridge",
                    &format!("compact: nothing to compact (session too small) key={key}"),
                );
                break CompactOutcome::Nothing;
            }
            let st = Pane::new(&pane).compact_progress();
            if st.active {
                seen = true;
                // Nothing to do here: compact has no dedicated status, and progress
                // is shown by the caller's progress message (see the user_compact comment; deliberate).
                // A receiver that went away only stops the drawing, not the compaction
                let _ = progress.send(st).await;
            } else if seen || (lower.contains("compacted (ctrl+o") && !stale_done) {
                // Only **positive signals** announce the end: we saw the spinner disappear, or
                // a new `Compacted` marker appeared (when it was too fast to ever catch the spinner).
                // Do not close on "no spinner yet" — that bug folded the bar in the middle of a late-starting compaction
                ctx.info(
                    "bridge",
                    &format!(
                        "compact: done key={key} ({}) after {}ms",
                        if seen {
                            "spinner cleared"
                        } else {
                            "Compacted marker"
                        },
                        started.elapsed().as_millis()
                    ),
                );
                break CompactOutcome::Done;
            } else if Pane::new(&pane).still_has_command("/compact") && retries < SUBMIT_RETRY_CAP {
                // The slash-command completion menu can eat the submit Enter
                retries += 1;
                ctx.info(
                    "bridge",
                    &format!(
                        "compact: /compact still un-submitted — pressing Enter \
                         ({retries}/{SUBMIT_RETRY_CAP}) key={key}"
                    ),
                );
                if let Err(e) = tmux.send_enter(target) {
                    ctx.debug(
                        "bridge",
                        &format!("compact: submit Enter failed key={key}: {e}"),
                    );
                }
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Asks the TUI for the current effort level. The level is recorded nowhere, so
    /// the only source of truth is the TUI itself: open the slider with `/effort`, close it with Escape, and read the status
    /// line the TUI then prints above the input box (`● high · /effort`). The slider itself is not parsed.
    /// None if it cannot be read (the caller decides the refusal text).
    pub async fn effort(
        &self,
        target: &Window,
        key: &ThreadKey,
        sid: &str,
    ) -> Option<&'static str> {
        const MAX_MS: u64 = 15_000;
        const POLL: Duration = Duration::from_millis(800);
        const READ_MS: u64 = 5_000;
        const SUBMIT_RETRY_CAP: u32 = 4;
        let ctx = LogCtx {
            session_id: Some(sid.to_string()),
            thread_key: Some(key.clone()),
        };
        let tmux = &self.tmux;
        let short: String = sid.chars().take(8).collect();
        ctx.info(
            "bridge",
            &format!(
                "effort: opening /effort on worker window {target} (session {short}) key={key}"
            ),
        );
        if let Err(e) = tmux.send_command(target, "/effort") {
            ctx.error(
                "bridge",
                &format!("effort: send-keys /effort to {target} failed: {e}"),
            );
            return None;
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
        let started = std::time::Instant::now();
        let mut retries = 0;
        // Act 1: wait for the slider to open (press again while the completion menu eats the Enter)
        let mut slider_seen = false;
        while started.elapsed().as_millis() as u64 <= MAX_MS {
            let pane = self.capture(target, "effort", &ctx);
            if Pane::new(&pane).effort_slider_open() {
                slider_seen = true;
                break;
            }
            if Pane::new(&pane).still_has_command("/effort") && retries < SUBMIT_RETRY_CAP {
                retries += 1;
                ctx.info(
                    "bridge",
                    &format!(
                        "effort: /effort still un-submitted — pressing Enter \
                         ({retries}/{SUBMIT_RETRY_CAP}) key={key}"
                    ),
                );
                if let Err(e) = tmux.send_enter(target) {
                    ctx.debug(
                        "bridge",
                        &format!("effort: submit Enter failed key={key}: {e}"),
                    );
                }
            }
            tokio::time::sleep(POLL).await;
        }
        if !slider_seen {
            // Clear the half-typed input from the input box before giving up
            if let Err(e) = tmux.send_escape(target) {
                ctx.debug(
                    "bridge",
                    &format!("effort: cleanup Escape failed key={key}: {e}"),
                );
            }
            ctx.error(
                "bridge",
                &format!(
                    "effort: timed out after {MAX_MS}ms without seeing the /effort slider key={key}"
                ),
            );
            return None;
        }
        // Act 2: close the slider — the TUI answers with a status line naming the current level
        if let Err(e) = tmux.send_escape(target) {
            ctx.error(
                "bridge",
                &format!("effort: closing Escape failed key={key}: {e}"),
            );
            return None;
        }
        let read_started = std::time::Instant::now();
        while read_started.elapsed().as_millis() as u64 <= READ_MS {
            let pane = self.capture(target, "effort", &ctx);
            if let Some(level) = Pane::new(&pane).effort_status() {
                ctx.info(
                    "bridge",
                    &format!("effort: current={level} session={short} key={key}"),
                );
                return Some(level);
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        // The status line is a transient notice — if another notice ("Plugins updated", etc.) takes its slot it cannot be read.
        // Observed on a real machine. Retrying is left to the Owner sending the message again
        ctx.error(
            "bridge",
            &format!(
                "effort: slider closed but no status line appeared within {READ_MS}ms key={key}"
            ),
        );
        None
    }

    /// Runs headless `claude` once and returns stdout (`probe`).
    ///
    /// Removing `AGENTGW_SESSION_ID` is the key — if it stays, the child process's hooks claim to be the agent
    /// and the probe is treated as the real session. stderr is discarded.
    /// `kill_on_drop` cleans up after a timeout.
    /// argv asking for `/context` (`--fork-session` keeps the main session clean).
    pub fn context_argv(session_id: &str) -> Vec<String> {
        [
            "claude",
            "--resume",
            session_id,
            "--fork-session",
            "-p",
            "/context",
        ]
        .map(str::to_string)
        .to_vec()
    }

    /// argv asking for `/usage`. It is about the whole account, so no session is needed.
    pub fn usage_argv() -> Vec<String> {
        ["claude", "-p", "/usage"].map(str::to_string).to_vec()
    }

    /// Starts sign-in. The caller waits with [`Claude::login_url`] **until the URL appears**.
    pub fn login_begin(&self, cwd: &str) -> Result<(), String> {
        // Start clean every time — drop leftovers, then start in a wide pane (so the URL prints on one line)
        self.login_kill();
        self.login_start(cwd).and_then(|()| {
            self.tmux
                .send_command(&self.login_window(), "claude auth login")
        })
    }

    /// Returns the auth URL if it is on screen.
    pub fn login_url(&self) -> Option<String> {
        self.tmux
            .capture(&self.login_window())
            .ok()
            .and_then(|pane| Pane::new(&pane).auth_login_url())
    }

    /// Sends in the pasted code.
    pub fn login_submit_code(&self, code: &str) -> Result<(), String> {
        self.tmux.send_command(&self.login_window(), code)
    }

    /// The outcome of sign-in. Reads the whole scrollback: right after printing "Login successful." the CLI
    /// returns to the shell, and the marker scrolls off screen before the next poll.
    pub fn login_outcome(&self) -> LoginOutcome {
        let pane = self
            .tmux
            .capture_history(&self.login_window(), 200)
            .unwrap_or_default();
        Pane::new(&pane).login_outcome()
    }

    /// Signs out. Runs the real command once. `Failed` is a non-zero exit (its content describes the
    /// exit status), `Errored` is a failure to launch at all — the caller shows different text for each.
    pub async fn logout(&self) -> Result<(), ProbeErr> {
        let run = tokio::process::Command::new("claude")
            .args(["auth", "logout"])
            .stdin(std::process::Stdio::null())
            .output()
            .await;
        match run {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(ProbeErr::Failed(
                o.status
                    .code()
                    .map_or_else(|| "by signal".to_string(), |c| c.to_string()),
            )),
            Err(e) => Err(ProbeErr::Errored(e.to_string())),
        }
    }

    /// Whether Claude Code is signed in on this machine, by `claude auth status`. Reads the
    /// JSON it prints, not its exit code (it exits 1 when signed out). `None` = no answer.
    pub async fn signed_in(&self) -> Option<bool> {
        let run = tokio::process::Command::new("claude")
            .args(["auth", "status"])
            .env_remove("AGENTGW_SESSION_ID")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        // The watcher is called inside the Bridge's select loop — if it hangs, do not stall long (normally under 1 second)
        let out = tokio::time::timeout(AUTH_STATUS_TIMEOUT, run).await.ok()?.ok()?;
        Self::auth_status_signed_in(&String::from_utf8_lossy(&out.stdout))
    }

    /// `loggedIn` of `claude auth status`'s JSON.
    pub fn auth_status_signed_in(json: &str) -> Option<bool> {
        serde_json::from_str::<serde_json::Value>(json).ok()?["loggedIn"].as_bool()
    }

    pub async fn probe(&self, argv: Vec<String>, cwd: String) -> Result<String, ProbeErr> {
        let Some((bin, args)) = argv.split_first() else {
            return Err(ProbeErr::Errored("empty probe argv".to_string()));
        };
        let run = tokio::process::Command::new(bin)
            .args(args)
            .current_dir(&cwd)
            .env_remove("AGENTGW_SESSION_ID")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(PROBE_TIMEOUT, run).await {
            Err(_) => Err(ProbeErr::Failed(format!(
                "probe timed out after {}ms",
                PROBE_TIMEOUT.as_millis()
            ))),
            // A failure of spawn itself (no claude / no cwd) would be the same on retry
            Ok(Err(e)) => Err(ProbeErr::Errored(format!(
                "probe could not start in {cwd}: {e}"
            ))),
            Ok(Ok(o)) if !o.status.success() => Err(ProbeErr::Failed(format!(
                "probe: claude exited {}",
                o.status
                    .code()
                    .map_or_else(|| "by signal".to_string(), |c| c.to_string())
            ))),
            Ok(Ok(o)) => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        }
    }
}

#[async_trait::async_trait]
impl crate::agent::Agent for Claude {
    fn spawn(&self, req: &SpawnReq) -> Result<Window, String> {
        Claude::spawn(self, req)
    }

    fn deliver(&self, w: &Window, text: &str) -> Result<(), String> {
        // The inherent `Claude::deliver`: checks for a dialog, then presses Enter again until
        // the input box is empty.
        Claude::deliver(self, w, text)
    }

    async fn watch_spawn_screens(
        &self,
        w: &Window,
        budget_ms: u64,
        poll_ms: u64,
        ctx: &LogCtx,
    ) -> SpawnOutcome {
        Claude::watch_spawn_screens(self, w, budget_ms, poll_ms, ctx).await
    }

    fn pid_of(&self, window_id: Option<&str>, window_name: &str) -> Option<Pid> {
        Claude::pid_of(self, window_id, window_name)
    }

    fn windows(&self) -> Vec<WindowRow> {
        self.tmux.rows()
    }

    fn terminate(&self, w: &Window) -> Result<(), String> {
        Claude::terminate(self, w)
    }

    fn interrupt(&self, w: &Window) -> Result<(), String> {
        self.tmux.send_escape(w)
    }

    fn dialog(&self, w: &Window) -> Option<crate::agent::Dialog> {
        Claude::dialog(self, w)
    }

    async fn answer_dialog(
        &self,
        w: &Window,
        choice: Option<usize>,
        ctx: &LogCtx,
    ) -> Result<(), String> {
        Claude::answer_dialog(self, w, choice, ctx).await
    }

    fn login_kill(&self) {
        Claude::login_kill(self)
    }

    async fn compact(
        &self,
        w: &Window,
        key: &ThreadKey,
        session_id: &str,
        progress: tokio::sync::mpsc::Sender<CompactProgress>,
    ) -> Option<CompactOutcome> {
        Some(Claude::compact(self, w, key, session_id, progress).await)
    }

    async fn effort(&self, w: &Window, key: &ThreadKey, session_id: &str) -> Option<&'static str> {
        Claude::effort(self, w, key, session_id).await
    }

    async fn set_effort(
        &self,
        w: &Window,
        level: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) -> Option<bool> {
        Some(Claude::set_effort(self, w, level, key, ctx).await)
    }

    fn mode(&self, w: &Window, ctx: &LogCtx) -> Option<&'static str> {
        Some(Claude::mode(self, w, ctx))
    }

    async fn set_mode(&self, w: &Window, name: &str, key: &ThreadKey, ctx: &LogCtx) -> Option<bool> {
        Some(Claude::set_mode(self, w, name, key, ctx).await)
    }
    async fn cd(&self, w: &Window, path: &str, key: &ThreadKey, ctx: &LogCtx) -> Option<bool> {
        Some(Claude::cd(self, w, path, key, ctx).await)
    }
    fn pre_answer_dialogs(&self, cwd: &str) -> Result<Vec<String>, String> {
        Self::pre_answer_dialogs(cwd)
    }

    async fn set_model(
        &self,
        w: &Window,
        name: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) -> Option<bool> {
        Some(Claude::set_model(self, w, name, key, ctx).await)
    }

    fn session_cwd(&self, remembered: Option<&str>, session_id: &str) -> Option<String> {
        Transcript::locate(remembered, session_id).and_then(|t| t.cwd())
    }

    fn workdir_exists(&self, path: &str) -> bool {
        std::path::Path::new(path).is_dir()
    }
    fn remove_worktree(&self, path: &str) -> Option<Result<(), String>> {
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(["-C", path])
                .args(args)
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        // **Resolve both before comparing.** `--git-common-dir` answers relative to the directory
        // asked (a plain `.git` inside a checkout) while `--absolute-git-dir` never does, so comparing
        // the two strings calls every checkout a worktree — and would hand the project itself to
        // `worktree remove`. Canonicalising settles the symlinks on the way (`/tmp` is one on macOS).
        let resolve = |p: &str| {
            let named = std::path::Path::new(p);
            let abs = if named.is_absolute() {
                named.to_path_buf()
            } else {
                std::path::Path::new(path).join(named)
            };
            std::fs::canonicalize(abs).ok()
        };
        let own = resolve(&git(&["rev-parse", "--absolute-git-dir"])?)?;
        let shared = resolve(&git(&["rev-parse", "--git-common-dir"])?)?;
        if own == shared {
            return None; // the checkout itself — never ours to remove
        }
        // Run it from the checkout the worktree belongs to: git refuses to remove the worktree it is
        // standing in. `--force` is deliberately absent, so one with work in it comes back as Err.
        let main = shared.parent()?.to_path_buf();
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["worktree", "remove", path])
            .output()
            .map_err(|e| e.to_string());
        Some(out.and_then(|o| {
            if o.status.success() {
                Ok(())
            } else {
                Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
            }
        }))
    }
    fn session_history_exists(&self, remembered: Option<&str>, session_id: &str) -> bool {
        Transcript::locate(remembered, session_id).is_some()
    }
    fn read_session(&self, session_id: &str) -> Option<String> {
        let t = Transcript::locate(None, session_id)?;
        std::fs::read_to_string(&t.path).ok()
    }

    fn last_activity_ms(&self, remembered: Option<&str>, session_id: &str) -> Option<u64> {
        let t = Transcript::locate(remembered, session_id)?;
        t.last_entry_ms(ACTIVITY_TAIL).or_else(|| t.mtime_ms())
    }

    fn current_model(
        &self,
        remembered: Option<&str>,
        session_id: &str,
    ) -> Option<std::io::Result<Option<String>>> {
        Transcript::locate(remembered, session_id).map(|t| t.model_id(256 * 1024))
    }

    fn session_limit_error(
        &self,
        remembered: Option<&str>,
        session_id: &str,
        now_ms: u64,
    ) -> Option<std::io::Result<Option<crate::agent::LimitHit>>> {
        Transcript::locate(remembered, session_id).map(|t| t.limit_error(256 * 1024, now_ms))
    }

    fn failure_type(&self, remembered: Option<&str>, session_id: &str) -> Option<&'static str> {
        let tail = Transcript::locate(remembered, session_id)?.tail(256 * 1024).ok()?;
        Transcript::failure_type(&tail)
    }

    fn model_alias(&self, model_id: &str) -> Option<&'static str> {
        super::screen::ModelId::new(model_id).alias()
    }

    fn models(&self) -> &'static [&'static str] {
        &super::screen::MODEL_NAMES
    }

    fn effort_levels(&self) -> &'static [&'static str] {
        &EFFORT_LEVELS
    }

    fn modes(&self) -> &'static [&'static str] {
        &MODE_NAMES
    }

    /// Case-insensitive. `sonet` is accepted as a misspelling of `sonnet`.
    fn canonical_model(&self, typed: &str) -> Option<String> {
        let name = match typed.to_lowercase().as_str() {
            "sonet" => "sonnet".to_string(),
            v => v.to_string(),
        };
        super::screen::MODEL_NAMES
            .contains(&name.as_str())
            .then_some(name)
    }

    /// The cwd matters as much as the id — claude stores history per cwd, so
    /// --resume from a different place cannot find it.
    fn resume_command(&self, cwd: Option<&str>, session_id: &str) -> String {
        match cwd {
            Some(cwd) => format!("cd {cwd} && claude --resume {session_id}"),
            None => format!("claude --resume {session_id}"),
        }
    }

    fn new_history_lines(&self, path: String, offset: u64, ctx: &LogCtx) -> Option<(String, u64)> {
        let mut t = Transcript::at_offset(path, offset);
        t.new_lines(ctx).map(|text| (text, t.offset()))
    }

    fn context_argv(&self, session_id: &str) -> Vec<String> {
        Claude::context_argv(session_id)
    }

    fn usage_argv(&self) -> Vec<String> {
        Claude::usage_argv()
    }

    async fn probe(&self, argv: Vec<String>, cwd: String) -> Result<String, ProbeErr> {
        Claude::probe(self, argv, cwd).await
    }

    async fn signed_in(&self) -> Option<bool> {
        Claude::signed_in(self).await
    }

    fn context_report(&self, raw: &str) -> Option<super::ContextReport> {
        Pane::new(raw).context_report()
    }

    fn usage_rows(&self, raw: &str) -> Option<Vec<super::UsageRow>> {
        Pane::new(raw).usage_rows()
    }

    fn login_begin(&self, cwd: &str) -> Result<(), String> {
        Claude::login_begin(self, cwd)
    }

    fn login_url(&self) -> Option<String> {
        Claude::login_url(self)
    }

    fn login_submit_code(&self, code: &str) -> Result<(), String> {
        Claude::login_submit_code(self, code)
    }

    fn login_outcome(&self) -> LoginOutcome {
        Claude::login_outcome(self)
    }

    async fn logout(&self) -> Result<(), ProbeErr> {
        Claude::logout(self).await
    }
}

/// Deadline for giving up on the headless probe (`ms = 60_000`).
/// Down presses allowed on the trust dialog before giving up (No, then Yes, has needed one).
const TRUST_MOVES_MAX: u32 = 3;
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// `claude auth status` is a local check; it answers in well under a second.
const AUTH_STATUS_TIMEOUT: Duration = Duration::from_secs(10);

/// The agent's `.jsonl` and how far we have read it.
///
/// Every hook payload carries the path, so it follows the file automatically if it moves (no need to search for the file).
pub struct Transcript {
    path: String,
    /// Where the last read stopped. `new_lines` advances it.
    offset: u64,
}

impl Transcript {
    /// The path the hook carried is the first candidate; otherwise search exhaustively by session id
    /// (resume / model / status use the same resolution order). **This is the only way to open one from outside.**
    pub fn locate(remembered: Option<&str>, session_id: &str) -> Option<Self> {
        remembered
            .filter(|p| std::path::Path::new(p).is_file())
            .map(|p| Self::at(p.to_string()))
            .or_else(|| Self::find(session_id))
    }

    /// Opens carrying over the previous position. The offset is held by the ledger (`Hooked`), so
    /// whoever reads on hands it in when opening.
    pub fn at_offset(path: String, offset: u64) -> Self {
        Self { path, offset }
    }

    fn at(path: String) -> Self {
        Self { path, offset: 0 }
    }

    /// Where a conversation started in `cwd` is kept: `~/.claude/projects/<cwd with / and . as ->`.
    ///
    /// **`--resume` only looks under the folder it was started in**, so a conversation carried to
    /// another machine has to land under the new folder's name, not the old one's.
    ///
    /// ponytail: the encoding is read off real directories on a real machine (`/Users/taito` →
    /// `-Users-taito`, `/Users/taito/.config` → `-Users-taito--config`), not from any documentation.
    /// Claude Code could change it. If resuming a moved conversation ever starts a blank one, compare
    /// this against the directory names under `~/.claude/projects` again.
    pub fn project_dir(cwd: &str) -> Option<std::path::PathBuf> {
        let name: String = cwd
            .chars()
            .map(|c| if c == '/' || c == '.' { '-' } else { c })
            .collect();
        Some(
            std::path::Path::new(&std::env::var("HOME").ok()?)
                .join(".claude/projects")
                .join(name),
        )
    }

    /// Exhaustive search of `~/.claude/projects/<project>/<sid>.jsonl`.
    /// Projects are only one level deep, so two readdirs are enough.
    fn find(sid: &str) -> Option<Self> {
        let root = std::path::Path::new(&std::env::var("HOME").ok()?).join(".claude/projects");
        for e in std::fs::read_dir(root).ok()? {
            let Ok(e) = e else { continue };
            let p = e.path().join(format!("{sid}.jsonl"));
            if p.is_file() {
                return Some(Self::at(p.to_string_lossy().into_owned()));
            }
        }
        None
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Reads **only complete lines** after `offset` and advances the offset. A partial line waits for next time
    /// (so an id cut in half is not missed). Unreadable files are swallowed at debug — the receipt check is
    /// an observation signal, not a ledger.
    pub fn new_lines(&mut self, ctx: &LogCtx) -> Option<String> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&self.path)
            .inspect_err(|e| ctx.debug("bridge", &format!("transcript open failed: {e}")))
            .ok()?;
        // If it got shorter it was replaced by something else — read again from the start
        let start = if f.metadata().ok()?.len() < self.offset {
            0
        } else {
            self.offset
        };
        let mut buf = Vec::new();
        f.seek(SeekFrom::Start(start)).ok()?;
        f.read_to_end(&mut buf).ok()?;
        let end = buf.iter().rposition(|&b| b == b'\n')? + 1;
        self.offset = start + end as u64;
        Some(String::from_utf8_lossy(&buf[..end]).into_owned())
    }

    /// Where the agent is working **now**: the `cwd` of the last entry that carries one.
    ///
    /// **Read from the end, not the start.** Every entry carries the directory the agent was in when
    /// it was written, and one that enters a worktree keeps writing to the same file from its new
    /// place — so the last is where it is, and the first is only where it began.
    ///
    /// Reading the first line alone used to be the whole of this function, on the grounds that the
    /// file grows to several MB. It stopped working: a transcript now opens with bookkeeping entries
    /// (`last-prompt`, `queue-operation`, `mode`) carrying no `cwd` at all, so it returned `None` for
    /// every transcript measured on a real machine (20 of 20, 2026-09-23), and `resume` had been
    /// quietly falling back to the recorded repo path ever since.
    ///
    /// The tail keeps the original promise — a fixed read, never the whole file.
    pub fn cwd(&self) -> Option<String> {
        let tail = self.tail(ACTIVITY_TAIL).ok()?;
        // Backwards, whole entries only: a tail's first line is usually cut in half, and a
        // half-parsed one is simply skipped
        tail.lines().rev().find_map(|l| {
            let entry: serde_json::Value = serde_json::from_str(l).ok()?;
            entry.get("cwd")?.as_str().map(str::to_string)
        })
    }

    /// Modification time (epoch ms). None if unreadable.
    pub fn mtime_ms(&self) -> Option<u64> {
        std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_millis() as u64)
    }

    /// When the conversation itself last moved: the `timestamp` of the final entry in the last `n`
    /// bytes. `None` if the tail holds no readable timestamp (an empty or brand-new file).
    ///
    /// **Not the file's mtime.** Measured on a real machine 2026-09-22: transcripts of idle agents
    /// are rewritten long after the conversation stops, with the same bytes — seven live agents were
    /// 0, 17min, 2.5h, 3.3h, 4.4h, 14.8h and 15.0h past their last entry. Idle measured by mtime
    /// therefore kept resetting below the hour that tears an agent down, and nothing was ever
    /// reclaimed. What is written inside the file cannot be moved by whatever touches it.
    pub fn last_entry_ms(&self, n: u64) -> Option<u64> {
        let tail = self.tail(n).ok()?;
        // Backwards, whole entries only: the first line of a tail is usually cut in half, and a
        // half-parsed one is simply skipped
        tail.lines().rev().find_map(|l| {
            let entry: serde_json::Value = serde_json::from_str(l).ok()?;
            let ts = entry.get("timestamp")?.as_str()?;
            chrono::DateTime::parse_from_rfc3339(ts)
                .ok()
                .map(|t| t.timestamp_millis() as u64)
        })
    }

    /// The **last** model id named in the final `n` bytes. `Ok(None)` = readable but no id.
    pub fn model_id(&self, n: u64) -> std::io::Result<Option<String>> {
        Ok(Pane::new(&self.tail(n)?).last_model_id())
    }

    /// The **last** limit error named in the final `n` bytes (`limitErrorForSession`).
    /// Only called on the rare path where "the turn already failed", so one tail read is enough.
    pub fn limit_error(
        &self,
        n: u64,
        now_ms: u64,
    ) -> std::io::Result<Option<crate::agent::LimitHit>> {
        Ok(Transcript::limit_hit(
            &self.tail(n)?,
            now_ms,
        ))
    }

    /// The final `n` bytes of the file. A long session's transcript runs to tens of MB, and
    /// the answer is always at the end.
    fn tail(&self, n: u64) -> std::io::Result<String> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&self.path)?;
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(len.saturating_sub(n)))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    // ── usage limit recorded in the transcript ──

    /// Picks **only limit errors** from the end of the history (`limitErrorInTranscriptTail`).
    ///
    /// An unrelated api-error that came later (a transient overload) must not hide a limit that is still in effect, so
    /// only limit records are kept. An unreadable reset time is bound **conservatively** to the error time + 1 hour;
    /// if even that has passed, the window has already reopened = mere history, so `None`.
    /// The `error_type` the **last** API error in `tail` stands for, when Claude Code wrote the
    /// error but sent no type with the turn failure. Only what we recognise; the rest is `None`.
    pub fn failure_type(tail: &str) -> Option<&'static str> {
        let text = tail
            .lines()
            .filter(|l| l.contains("\"isApiErrorMessage\""))
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|rec| rec["isApiErrorMessage"] == serde_json::Value::Bool(true))
            .map(|rec| match &rec["message"]["content"] {
                serde_json::Value::Array(a) => a
                    .iter()
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                serde_json::Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .last()?;
        // Text seen on a real machine (2026-09-18): `Login expired · Please run /login`
        (text.contains("Login expired") || text.contains("run /login"))
            .then_some("authentication_failed")
    }

    pub fn limit_hit(tail: &str, now_ms: u64) -> Option<LimitHit> {
        let mut latest: Option<(String, u64)> = None;
        for line in tail.lines() {
            // A cheap first-pass filter — parsing 256KB as JSON is expensive. This flag is rarely set
            if !line.contains("\"isApiErrorMessage\"") {
                continue;
            }
            // The tail slice starts mid-line, and the line being written is partial — skip both
            let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if rec["isApiErrorMessage"] != serde_json::Value::Bool(true) {
                continue;
            }
            let text = match &rec["message"]["content"] {
                serde_json::Value::Array(a) => a
                    .iter()
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                serde_json::Value::String(s) => s.clone(),
                _ => String::new(),
            };
            if !Self::says_limit_reached(&text) {
                continue;
            }
            let at = rec["timestamp"]
                .as_str()
                .and_then(WallClock::parse_iso8601_ms);
            latest = Some((text, at.unwrap_or(now_ms)));
        }
        let (detail, at) = latest?;
        let reset_ms = WallClock::parse_reset_epoch(&detail, at).unwrap_or(at + 3_600_000);
        (reset_ms > now_ms).then_some(LimitHit { detail, reset_ms })
    }

    /// Literals equivalent to an **unanchored substring match** of `LIMIT_MODAL_PROMPTS`.
    /// The leading `(?:…)?` may be empty, so it does not change the substring-match result — hence it can be dropped.
    ///
    /// ⚠️ **Do not reuse this for pane detection.** The same table is also used **anchored at line start**
    /// (`paneShowsPrompt` — the screen prints its own prompts as lines, which tells them apart from
    /// "content" that mentions the same words mid-sentence). Porting that detection means redoing the reduction.
    fn says_limit_reached(text: &str) -> bool {
        let t = text.to_ascii_lowercase();
        if t.contains("wait for limit to reset") || t.contains("wait for the limit to reset") {
            return true;
        }
        ["session", "usage", "weekly"].iter().any(|w| {
            t.contains(&format!("hit your {w} limit"))
                || t.contains(&format!("{w} limit reached"))
                || t.contains(&format!("you've reached your {w} limit"))
                || t.contains(&format!("youve reached your {w} limit"))
        })
    }
}

// ─── Hook endpoint ─────────────────────────────────────────────────────────

use crate::agent::HookEvent;
use axum::{
    Router, extract::State, http::HeaderMap, http::StatusCode, http::header::CONTENT_TYPE,
    routing::post,
};
use std::path::PathBuf;
use tokio::sync::mpsc;

/// Shared state the hook endpoint gives axum.
///
/// `token` is a password cut per startup that rejects POSTs from anything but agents (the endpoint is bound to 127.0.0.1, but
/// other processes on the same machine could still hit it).
#[derive(Clone)]
struct HookState {
    tx: mpsc::Sender<HookEvent>,
    token: String,
}

/// The HTTP endpoint that receives claude's hooks.
///
/// **UserPromptSubmit does not fire when steering input is consumed mid-turn**, so the receipt check
/// relies on also scanning the transcript via the payload's `transcript_path`.
/// Do not break that assumption when touching this.
pub struct HookIntake;

impl HookIntake {
    /// Upper bound for hooks that wait for an answer.
    /// perm waits for a human in Slack, so it is long. stop must not hang the end of a turn, so it is short.
    fn decision_cap(kind: &str) -> Option<Duration> {
        match kind {
            "perm" => Some(Duration::from_secs(120)),
            "stop" => Some(Duration::from_secs(5)),
            _ => None,
        }
    }

    /// Brings up the endpoint and returns `(port, token)`. The port is remembered and reused —
    /// agents bake the URL in, so it has to survive a Bridge restart.
    pub async fn serve(
        state_dir: &StateDir,
        tx: mpsc::Sender<HookEvent>,
    ) -> std::io::Result<(u16, String)> {
        let port = state_dir.remembered_port("hook", StateDir::free_port);
        let token = state_dir.remembered_token("hook");
        let app = Router::new()
            .route("/hook/{kind}", post(Self::on_hook))
            .with_state(HookState {
                tx,
                token: token.clone(),
            });
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        tokio::spawn(async move {
            // Same as the MCP endpoint: agents hold this address, so a server that returns leaves every
            // hook — receipts, permissions, turn failures — unanswered, in silence
            let why = match axum::serve(listener, app).await {
                Err(e) => format!("hook endpoint stopped: {e}"),
                Ok(()) => "hook endpoint stopped".to_string(),
            };
            LogCtx::default().error("hooks", &why);
            std::process::exit(1);
        });
        LogCtx::default().info("hooks", &format!("hook endpoint on 127.0.0.1:{port}"));
        Ok((port, token))
    }

    /// Builds the hooks settings for an agent.
    pub fn settings_json(port: u16, token: &str) -> serde_json::Value {
        let base = format!("http://127.0.0.1:{port}/hook");
        let http_ = |kind: &str, timeout: u32| {
            serde_json::json!([{ "matcher": "", "hooks": [{
                "type": "http",
                "url": format!("{base}/{kind}"),
                "timeout": timeout,
                // Claude Code expands ${VAR} only for declared variables
                "headers": { "x-agentgw-token": token, "x-agentgw-session": "${AGENTGW_SESSION_ID}" },
                "allowedEnvVars": ["AGENTGW_SESSION_ID"],
            }]}])
        };
        serde_json::json!({
            // Agents are Slack-driven sessions. Only one --settings takes effect, so it lives here too.
            "disableRemoteControl": true,
            "hooks": {
                // Only SessionStart has Claude Code ignore http, so it uses curl. Headers are not carried,
                // so session_id is taken from the body (measured in the spike).
                "SessionStart": [{ "matcher": "", "hooks": [{
                    "type": "command",
                    "timeout": 10,
                    "command": format!(
                        "/usr/bin/curl -sS --max-time 5 -X POST -H 'content-type: application/json' \
                         -H 'x-agentgw-token: {token}' --data-binary @- {base}/session_start || true"
                    ),
                }]}],
                "UserPromptSubmit": http_("user_prompt", 5),
                // Reporting hooks get a short timeout — so a slow Bridge does not stall the agent's turn.
                "PreToolUse": http_("progress", 3),
                "PostToolUse": http_("progress", 3),
                // A tool call that fails gets no PostToolUse — without this the row stays ◌ for good
                "PostToolUseFailure": http_("progress", 3),
                "MessageDisplay": http_("narration", 3),
                // Why the turn failed (`error_type`). **Not a decision hook** — it only takes the report and
                // tells the human, so it returns `{}` right away.
                "StopFailure": http_("error", 5),
                // SessionEnd only gets 1500ms by default. Declaring 30s widens the interruption window too.
                "SessionEnd": http_("session_end", 30),
                // The two that must answer. perm waits for a human in Slack, and stop must never hang
                // the end of a turn. 125s is a bit wider than the Bridge's 120s wait
                "PermissionRequest": http_("perm", 125),
                "Stop": http_("stop", 15),
            }
        })
    }

    pub fn write_settings(dir: &StateDir, port: u16, token: &str) -> std::io::Result<PathBuf> {
        // Generated files, so they go to a temporary area, not state ([`StateDir::runtime_dir`])
        dir.write_runtime_json("worker-hooks.json", &Self::settings_json(port, token))
    }

    /// Hook responses have a JSON body.
    fn json_body(body: String) -> impl axum::response::IntoResponse {
        ([(CONTENT_TYPE, "application/json")], body)
    }

    /// Hands off to the receiver and waits for the answer. No answer, too slow, or the receiver gone all mean `{}` (= abstain).
    async fn decide_or_default(
        tx: &mpsc::Sender<HookEvent>,
        mut ev: HookEvent,
        cap: Duration,
    ) -> String {
        let (respond, answer) = tokio::sync::oneshot::channel();
        ev.respond = Some(respond);
        if tx.send(ev).await.is_err() {
            return "{}".into();
        }
        match tokio::time::timeout(cap, answer).await {
            Ok(Ok(v)) => v.to_string(),
            _ => "{}".into(),
        }
    }

    async fn on_hook(
        State(st): State<HookState>,
        axum::extract::Path(kind): axum::extract::Path<String>,
        headers: HeaderMap,
        body: String,
    ) -> Result<impl axum::response::IntoResponse, StatusCode> {
        if headers.get("x-agentgw-token").and_then(|v| v.to_str().ok()) != Some(st.token.as_str()) {
            LogCtx::default().info("hooks", &format!("rejected {kind}: bad token"));
            return Err(StatusCode::UNAUTHORIZED);
        }
        let payload: serde_json::Value =
            serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        // SessionStart uses curl and carries no headers — use session_id from the body (measured in the spike)
        let session_id = headers
            .get("x-agentgw-session")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .or_else(|| payload["session_id"].as_str().map(str::to_string))
            .unwrap_or_default();
        if session_id.is_empty() {
            LogCtx::default().info("hooks", &format!("{kind} without session id — ignored"));
            return Ok(Self::json_body("{}".into()));
        }
        let ctx = LogCtx {
            session_id: Some(session_id.clone()),
            thread_key: None,
        };
        ctx.debug("hooks", &format!("hook {kind}"));
        let ev = HookEvent {
            kind,
            session_id,
            payload,
            respond: None,
        };
        // perm and stop need an answer. The rest are reports, so return `{}` at once and do not hold the turn.
        if let Some(cap) = Self::decision_cap(&ev.kind) {
            return Ok(Self::json_body(
                Self::decide_or_default(&st.tx, ev, cap).await,
            ));
        }
        if let Err(e) = st.tx.send(ev).await {
            ctx.error("hooks", &format!("hook queue closed: {e}"));
        }
        Ok(Self::json_body("{}".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::agent::SessionId;
    use crate::agent::screen::tests::{LOGIN_PANE, TRUST_PANE};

    fn claude_with(
        run: impl Fn(&[&str]) -> Result<String, String> + Send + Sync + 'static,
    ) -> Claude {
        Claude::new(Tmux { run: Box::new(run) })
    }

    /// The launch line and prompts do not touch tmux — a fake that quietly succeeds is enough.
    fn quiet() -> Claude {
        claude_with(|_| Ok(String::new()))
    }

    fn req(session_id: &str, prompt: Option<&str>, state: WorkerState) -> SpawnReq {
        SpawnReq {
            session_id: SessionId::from(session_id.to_string()),
            cwd: "/repo".to_string(),
            prompt: prompt.map(str::to_string),
            resume_from: None,
            window: "1-1".to_string(),
            state,
            hooks_file: "/st/h.json".to_string(),
            mcp_config: "/st/m.json".to_string(),
            moved_from: None,
        }
    }

    #[test]
    fn launch_line_snapshot() {
        let line = quiet().launch_line(
            &SessionMode::New("sid-1".into()),
            "/st/worker-hooks.json",
            "/st/mcp/sid-1.json",
            "it's here",
        );
        assert_eq!(
            line,
            "AGENTGW_SESSION_ID=sid-1 claude --settings /st/worker-hooks.json \
             --mcp-config /st/mcp/sid-1.json --strict-mcp-config --session-id sid-1 \
             'it'\\''s here'"
        );
    }

    /// The launch line built from `SpawnReq` has the same shape as the string the old `src/worker.rs`
    /// spawn passed to tmux.
    /// `--resume` if `resume_from` is set, otherwise `--session-id` (IDs are single-use).
    #[test]
    fn spawn_hands_tmux_the_launch_line() {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = lines.clone();
        let c = claude_with(move |args| {
            if args[0] == "new-window" || args[0] == "new-session" {
                sink.lock().unwrap().push(args[args.len() - 1].to_string());
            }
            Ok("@7\n".to_string())
        });
        let sp = c
            .spawn(&req(
                "sid-1",
                Some("<channel …>x</channel>"),
                WorkerState::Absent,
            ))
            .unwrap();
        assert_eq!(sp.as_str(), "@7");
        let got = lines.lock().unwrap()[0].clone();
        assert!(
            got.starts_with(
                "AGENTGW_SESSION_ID=sid-1 claude --settings /st/h.json --mcp-config /st/m.json \
                 --strict-mcp-config --session-id sid-1 'This is a NEW Slack thread;"
            ),
            "{got}"
        );
        assert!(got.ends_with("<channel …>x</channel>'"), "{got}");
        // No body = warming a pool agent — start on the idle prompt
        let mut pool = req("sid-2", None, WorkerState::Absent);
        pool.resume_from = Some(SessionId::from("sid-2".to_string()));
        c.spawn(&pool).unwrap();
        let got = lines.lock().unwrap()[1].clone();
        assert_eq!(
            got,
            format!(
                "AGENTGW_SESSION_ID=sid-2 claude --settings /st/h.json --mcp-config /st/m.json \
                 --strict-mcp-config --resume sid-2 '{WORKER_STARTUP_PROMPT}'"
            )
        );
    }

    #[test]
    fn tmux_helpers_compose_correct_argv() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let c2 = calls.clone();
        let claude = claude_with(move |args| {
            c2.lock().unwrap().push(args.join(" "));
            Ok(String::new())
        });
        let tmux = claude.tmux();
        let w = Window::of("@5");
        tmux.capture(&w).unwrap();
        tmux.send_escape(&w).unwrap();
        tmux.send_command(&w, "/compact").unwrap();
        tmux.kill_window(&w).unwrap();
        claude.login_start("/home").unwrap();
        tmux.capture_history(&claude.login_window(), 200).unwrap();
        tmux.send_enter(&claude.login_window()).unwrap();
        claude.login_kill();
        let got = calls.lock().unwrap().clone();
        assert_eq!(got[0], "capture-pane -p -t @5");
        assert_eq!(got[1], "send-keys -t @5 Escape");
        assert_eq!(got[2], "send-keys -t @5 -l -- /compact");
        assert_eq!(got[3], "send-keys -t @5 Enter");
        assert_eq!(got[4], "kill-window -t @5");
        assert_eq!(
            got[5],
            "new-session -d -s slack-login-rs -x 400 -y 50 -c /home"
        );
        // The success marker scrolls away when it drops back to the shell — read the whole scrollback
        assert_eq!(got[6], "capture-pane -p -S -200 -t slack-login-rs");
        assert_eq!(got[7], "send-keys -t slack-login-rs Enter");
        assert_eq!(got[8], "kill-session -t slack-login-rs");
    }

    #[test]
    fn spawn_refuses_when_the_seat_is_taken() {
        let err = quiet()
            .spawn(&req("sid", Some("hi"), WorkerState::Ready))
            .unwrap_err();
        assert!(
            err.contains("occupied"),
            "a spawn must never be a kill: {err}"
        );
    }

    /// Same "read the final n bytes and hand them to a pure function" shape as `model_id`.
    #[test]
    fn transcript_limit_error_reads_the_tail() {
        let dir = std::env::temp_dir().join(format!("scr-limit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let rec = serde_json::json!({
            "isApiErrorMessage": true,
            "timestamp": "2026-07-30T05:00:00.000Z",
            "message": { "content": [{ "text": "Claude usage limit reached. resets at 11pm" }] },
        });
        std::fs::write(&path, format!("{rec}\n")).unwrap();
        let t = Transcript::at_offset(path.to_string_lossy().into_owned(), 0);
        let now = 1_785_387_600_000u64; // 2026-07-30T05:00:00Z = 14:00 JST on the 30th
        let hit = t.limit_error(256 * 1024, now).unwrap().expect("limit hit");
        assert!(hit.detail.contains("usage limit reached"), "{hit:?}");
        assert_eq!(hit.reset_ms, 1_785_420_000_000); // 23:00 JST on the 30th
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **The file's own mtime is not activity.** Real transcripts of idle agents are rewritten hours
    /// after their last entry (measured 2026-09-22: up to 15 hours), which kept resetting the idle
    /// clock below the hour that reclaims an agent, so none ever was.
    #[test]
    fn activity_is_the_last_entry_not_the_file_mtime() {
        let dir = std::env::temp_dir().join(format!("scr-activity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        // Two real shapes: an assistant turn, then the system entry Claude Code writes after it
        let lines = [
            serde_json::json!({"type": "assistant", "timestamp": "2026-09-22T11:15:29.850Z"}),
            serde_json::json!({"type": "system", "timestamp": "2026-09-22T11:15:31.433Z", "hookCount": 2}),
        ];
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, &body).unwrap();
        let t = Transcript::at_offset(path.to_string_lossy().into_owned(), 0);

        // 2026-09-22T11:15:31.433Z — the last entry, whatever the file was touched at since
        assert_eq!(t.last_entry_ms(64 * 1024), Some(1_790_075_731_433));
        // Nothing to read = nothing claimed; the caller falls back to the file
        std::fs::write(&path, "\n").unwrap();
        assert_eq!(t.last_entry_ms(64 * 1024), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_input_box_means_the_text_was_submitted() {
        // What an empty box really looks like: after `❯` comes U+00A0 (capture-pane on a real machine, 2026-08-02)
        assert!(Pane::new("← U012ABC3DEF: やって\n❯ \u{a0}").input_box_empty());
        assert!(Pane::new("dialog with no prompt line").input_box_empty()); // no `❯` = do not type
        // Body left unsubmitted. The box wraps and the `❯` line shows the visible first line
        assert!(!Pane::new("❯ ==> 状態\n  </channel>\n").input_box_empty());

        // **The trap itself**: the auto mode modal comes up with the box still alive, so this reads "empty" =
        // "submitted". This function alone cannot decide whether delivery is possible
        // ([`Claude::not_accepting_keys`] tells them apart by the hint line). The shape of the 18-minute silence on 2026-08-18.
        let (_, onboarding, _) = REAL_MODALS[0];
        assert!(Pane::new(onboarding).input_box_empty());
        assert!(Pane::new(onboarding).modal_footer().is_some());
        // The `/model` kind conversely has `❯` taken by the selection cursor, so it looks like "body still there" —
        // the trap where the retry Enter confirms the dialog
        let (_, selector, _) = REAL_MODALS[1];
        assert!(!Pane::new(selector).input_box_empty());
        assert!(Pane::new(selector).modal_footer().is_some());
    }

    /// **Real** panes (2026-08-18, captured in `slack-workers-rs`). Decorations and wrapping kept as is.
    /// Do not rewrite these by guesswork — the first fix did not work because the behavior of `❯` was
    /// assumed without looking at the real thing.
    ///
    /// 1st: auto mode onboarding. The modal comes up **with the input box (`❯`) still alive**.
    /// 2nd: `/model` selector. **`❯` is taken over by the selection cursor** (the box line disappears).
    const REAL_MODALS: &[(&str, &str, &str)] = &[
        (
            "auto mode onboarding",
            " Set up auto mode for your environment?\n\
             \n\
             Auto mode lets Claude act without asking first. Telling it which repos you trust\n\
             and what data is sensitive gives it clearer guardrails on what's safe to run.\n\
             \n\
             \u{276f} 1. Set it up\n\
             \u{a0} 2. Not now\n\
             \u{a0} 3. Don't show again\n\
             \n\
             Enter to confirm \u{b7} Esc to cancel\n\
             \n\
             Message #slack-multi-ch\n\
             \u{276f} \u{a0}\n\
             \u{a0} ctx:4%  15:08  Opus 5 (1M context)\n\
             \u{23f5}\u{23f5} auto mode on (shift+tab to cycle) \u{b7} \u{2190} 1 agent\n",
            "Set up auto mode for your environment?",
        ),
        (
            "/model selector",
            "   Select model\n\
             \n\
             \u{a0} 1. Default (recommended)  Opus 5 with 1M context\n\
             \u{276f} 2. Opus (1M context) \u{2714}    Opus 5 with 1M context\n\
             \u{a0} 3. Fable                  Fable 5\n\
             \n\
             \u{a0} \u{25cf} High effort (default) \u{2190}/\u{2192} to adjust\n\
             \n\
             Enter to set as default \u{b7} s to use this session only \u{b7} Esc to cancel\n",
            "Select model",
        ),
    ];

    /// The real idle screen (same day, same session). The `❯` line is empty and no hint line is shown.
    const IDLE_PANE: &str = "\u{2726} Cooked for 2s\n\
                             \n\
                             \u{2500}\u{2500}\u{2500}\u{2500}\n\
                             \u{276f} \u{a0}\n\
                             \u{2500}\u{2500}\u{2500}\u{2500}\n\
                             \u{a0} ~  ctx:4%  17:54  Opus 5 (1M context)\n\
                             \u{23f5}\u{23f5} auto mode on (shift+tab to cycle) \u{b7} \u{2190} 1 agent\n";

    /// A fake that sends body and Enter. `capture-pane` returns `panes` one at a time from the front.
    fn deliver_probe(
        panes: Vec<&'static str>,
    ) -> (std::sync::Arc<std::sync::Mutex<Vec<String>>>, Claude) {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = calls.clone();
        let shown = std::sync::Mutex::new(0usize);
        let c = claude_with(move |args| {
            sink.lock().unwrap().push(args.join(" "));
            if args[0] == "capture-pane" {
                let mut i = shown.lock().unwrap();
                let pane = panes[(*i).min(panes.len() - 1)];
                *i += 1;
                return Ok(pane.to_string());
            }
            Ok(String::new())
        });
        (calls, c)
    }

    fn enters(calls: &[String]) -> usize {
        calls.iter().filter(|c| c.ends_with(" Enter")).count()
    }

    #[test]
    fn deliver_presses_enter_again_while_the_box_still_holds_the_text() {
        // The shape where Enter was swallowed by the ingesting TUI with a 4KB envelope (2026-08-02). Pressing again gets it through
        // The 1st pane is consumed by the pre-typing check (the box is there = OK to type)
        let (calls, c) = deliver_probe(vec!["❯ \u{a0}", "❯ </channel>\n", "❯ \u{a0}"]);
        c.deliver(&Window::of("@42"), "hello").unwrap();
        assert_eq!(enters(&calls.lock().unwrap()), 2, "最初の1発 + 押し直し1回");
    }

    /// **A box that will not empty is not a failed delivery.** A busy TUI is only slower to swallow
    /// keys (measured on a real pane 2026-09-22: mid-turn, the body went in and the box came back
    /// empty), and reporting failure here left the message queued — where the tick typed the whole
    /// body in again, four copies of one request. Enter is pressed up to the cap, and whether the body
    /// truly went in is settled by the ledger (`user_prompt` / the transcript), not by this screen.
    #[test]
    fn a_box_that_never_empties_is_not_reported_as_a_failure() {
        let (calls, c) = deliver_probe(vec!["❯ </channel>\n"]); // stays there forever
        c.deliver(&Window::of("@42"), "hello").unwrap();
        assert_eq!(
            enters(&calls.lock().unwrap()),
            1 + DELIVER_SUBMIT_RETRIES as usize,
            "the pressing stops at the cap"
        );
    }

    /// The stall seen on a real machine on 2026-08-18. A modal covering `❯` looks **empty** to `input_box_empty()`,
    /// but the keystrokes are eaten by the modal. For the 18 minutes this returned `Ok`, the Bridge recorded
    /// delivery success while 2 threads went silent.
    ///
    /// The key is **sending not a single character** — typing the body into a choice list makes its digits select and the following Enter
    /// confirm. The tick presses again (`Bridge::retry_pending`), so a leak here picks an option
    /// before a human answers.
    #[test]
    fn deliver_refuses_to_call_a_covered_input_box_a_delivery() {
        for (label, modal, want) in REAL_MODALS {
            let (calls, c) = deliver_probe(vec![modal]);
            let err = c.deliver(&Window::of("@42"), "hello").unwrap_err();
            assert!(err.contains("waiting on a dialog"), "{label}: {err}");
            // What the person waiting sees: which dialog, and that answering it lets the message through.
            // No window id, no footer string — they can act on neither
            assert!(err.contains(want), "{label}: {err}");
            assert!(!err.contains("@42"), "{label}: internal id leaked into the notice: {err}");
            let calls = calls.lock().unwrap();
            assert_eq!(enters(&calls), 0, "{label}: Enter を撃たない");
            assert!(
                !calls
                    .iter()
                    .any(|c| c.contains("send-keys") && c.contains(" -l ")),
                "{label}: 本文も1文字も送らない: {calls:?}"
            );
        }
    }

    /// An idle window passes straight through. A false positive here **stops every delivery**, so it is pinned
    /// by the same tests as the modal check (`esc to interrupt` is the running word, not a modal).
    #[test]
    fn an_idle_worker_is_not_mistaken_for_a_dialog() {
        for (label, pane) in [
            ("待機中", IDLE_PANE),
            (
                "走行中",
                "✽ Fermenting… (1m 22s · ↓ 2.0k tokens)\n  (esc to interrupt)\n\
                 ────\n❯ \u{a0}\n────\n  ⏵⏵ auto mode on (shift+tab to cycle)\n",
            ),
        ] {
            assert!(
                Pane::new(pane).modal_footer().is_none(),
                "{label}: モーダル扱いされた"
            );
            let (calls, c) = deliver_probe(vec![pane, "❯ \u{a0}"]);
            c.deliver(&Window::of("@42"), "hello")
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(enters(&calls.lock().unwrap()), 1, "{label}");
        }
    }

    /// **Against real tmux and a real claude**. Unit tests only pin panes I captured, so the end-to-end
    /// "capture, judge, do not type" can only be checked here
    /// (0.17.3 was green in unit tests yet did not work on a real machine).
    ///
    /// How to run: `cargo test -- --ignored real_tmux`. It starts a claude, so it does not run by default.
    #[test]
    #[ignore = "本物の tmux と claude を起こす"]
    fn real_tmux_deliver_refuses_a_live_claude_dialog() {
        // Use a name that is not a prefix match for the production session `agentgw-workers` (a trap hit on 2026-08-18)
        const SESSION: &str = "deliver-probe-rs";
        let t = Tmux::real();
        let sh = |args: &[&str]| (t.run)(args);
        let _ = sh(&["kill-session", "-t", SESSION]);
        sh(&[
            "new-session",
            "-d",
            "-s",
            SESSION,
            "-x",
            "200",
            "-y",
            "50",
            "-c",
            "/home/me",
            "claude",
        ])
        .expect("tmux new-session");

        let c = Claude::new(Tmux::real());
        let w = Window::raw(SESSION);
        let pane =
            || (Tmux::real().run)(&["capture-pane", "-p", "-t", SESSION]).unwrap_or_default();
        let wait = |label: &str, ok: &dyn Fn(&str) -> bool| {
            for _ in 0..120 {
                if ok(&pane()) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            let _ = (Tmux::real().run)(&["kill-session", "-t", SESSION]);
            panic!("{label} を60秒待って出なかった:\n{}", pane());
        };

        wait("入力欄", &|p| !Pane::new(p).input_line().is_empty());
        // An idle window passes straight through = no false positive (if this fails, every delivery stops)
        assert!(Pane::new(&pane()).modal_footer().is_none(), "{}", pane());
        c.deliver(&w, "say ok").expect("待機中の窓には配達できる");

        // `/model` **only once**. Resending inside the wait would type into the open dialog
        wait("入力欄(応答後)", &|p| Pane::new(p).input_box_empty());
        sh(&["send-keys", "-t", SESSION, "-l", "--", "/model"]).expect("send /model");
        std::thread::sleep(Duration::from_millis(400));
        sh(&["send-keys", "-t", SESSION, "Enter"]).expect("send Enter");
        wait("ダイアログ", &|p| {
            Pane::new(p).modal_footer().is_some()
        });

        let before = pane();
        let err = c
            .deliver(&w, "この本文は1文字も入ってはいけない")
            .unwrap_err();
        let after = pane();
        let _ = sh(&["send-keys", "-t", SESSION, "Escape"]);
        std::thread::sleep(Duration::from_millis(800));
        let closed = pane();
        let _ = sh(&["kill-session", "-t", SESSION]);

        assert!(err.contains("a dialog has the keyboard"), "{err}");
        // Nothing typed = the dialog is still open and the selection has not moved
        assert!(
            Pane::new(&after).modal_footer().is_some(),
            "ダイアログが閉じた(= Enter を撃った):\n{after}"
        );
        assert_eq!(
            Pane::new(&before).input_line(),
            Pane::new(&after).input_line(),
            "選択カーソルが動いた(= 本文が操作になった)"
        );
        assert!(
            Pane::new(&closed).modal_footer().is_none(),
            "Escape で閉じられなかった:\n{closed}"
        );
    }

    fn hook_tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sc-hooks-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn worker_hooks_has_the_load_bearing_shape() {
        let v = HookIntake::settings_json(8791, "tok");
        // Only one --settings takes effect → disableRemoteControl must live alongside
        assert_eq!(v["disableRemoteControl"], true);
        // SessionStart ignores http, so it is a curl command
        let ss = &v["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(ss["type"], "command");
        assert!(
            ss["command"].as_str().unwrap().contains("curl"),
            "SessionStart must be curl: {ss}"
        );
        assert!(
            ss["command"]
                .as_str()
                .unwrap()
                .contains("x-agentgw-token: tok")
        );
        // UserPromptSubmit / SessionEnd use http + session header
        for kind in ["UserPromptSubmit", "SessionEnd"] {
            let h = &v["hooks"][kind][0]["hooks"][0];
            assert_eq!(h["type"], "http", "{kind}");
            assert_eq!(h["headers"]["x-agentgw-token"], "tok", "{kind}");
            assert_eq!(
                h["headers"]["x-agentgw-session"], "${AGENTGW_SESSION_ID}",
                "{kind}"
            );
            assert_eq!(h["allowedEnvVars"][0], "AGENTGW_SESSION_ID", "{kind}");
            assert!(
                h["url"]
                    .as_str()
                    .unwrap()
                    .starts_with("http://127.0.0.1:8791/hook/"),
                "{kind}"
            );
        }
        // A failed tool call only reports through PostToolUseFailure — without it the row stays ◌
        assert_eq!(
            v["hooks"]["PostToolUseFailure"][0]["hooks"][0]["url"],
            "http://127.0.0.1:8791/hook/progress"
        );
    }

    #[tokio::test]
    async fn stop_decision_answers_or_declines() {
        use std::time::Duration;
        // The case where the main-loop stand-in returns block
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            let ev: HookEvent = rx.recv().await.unwrap();
            ev.respond
                .unwrap()
                .send(serde_json::json!({"decision": "block"}))
                .unwrap();
        });
        let ev = HookEvent {
            kind: "stop".into(),
            session_id: "s1".into(),
            payload: serde_json::Value::Null,
            respond: None,
        };
        let body = HookIntake::decide_or_default(&tx, ev, Duration::from_secs(1)).await;
        assert_eq!(body, r#"{"decision":"block"}"#);
        // The endpoint itself is closed (main went down) → cannot send, so abstain with {}
        let (tx2, rx2) = tokio::sync::mpsc::channel(1);
        drop(rx2);
        let ev = HookEvent {
            kind: "stop".into(),
            session_id: "s1".into(),
            payload: serde_json::Value::Null,
            respond: None,
        };
        let body = HookIntake::decide_or_default(&tx2, ev, Duration::from_millis(50)).await;
        assert_eq!(body, "{}");
    }

    /// When the receiver drops it without sending a respond. The oneshot errors at once, so the
    /// requirement is abstaining without waiting out the 5-second cap (do not hang the turn).
    #[tokio::test]
    async fn stop_declines_at_once_when_the_answer_is_dropped() {
        use std::time::Duration;
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move { drop(rx.recv().await.unwrap()) });
        let ev = HookEvent {
            kind: "stop".into(),
            session_id: "s1".into(),
            payload: serde_json::Value::Null,
            respond: None,
        };
        let t = std::time::Instant::now();
        assert_eq!(
            HookIntake::decide_or_default(&tx, ev, Duration::from_secs(5)).await,
            "{}"
        );
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "respond drop は即返るはず: {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn config_writers_land_on_disk() {
        let dir = hook_tmp("write");
        let dir = crate::state_dir::StateDir::at(dir);
        let hooks = HookIntake::write_settings(&dir, 8791, "tok").unwrap();
        let mcp = crate::mcp::Mcp::write_config(&dir, 8790, "sid-1", "secret").unwrap();
        assert!(hooks.exists() && mcp.exists());
        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&mcp).unwrap()).unwrap();
        assert_eq!(
            back["mcpServers"]["agentgw"]["headers"]["X-Agentgw-Session"],
            "sid-1"
        );
    }

    /// A `Claude` whose screen can be swapped per call, and a record of the keys sent.
    /// The watch has no early exit — it polls until its budget runs out, so the budget **is** how
    /// long these tests take. It used to be 30ms, which is not a budget but a coin flip: on a busy
    /// machine the runtime got through one poll of three and the test failed (four times in one
    /// afternoon). A tenth of a second is still cheap and leaves room for a loaded machine.
    const WATCH_MS: u64 = 300;

    fn claude_showing(
        panes: Vec<&'static str>,
    ) -> (std::sync::Arc<std::sync::Mutex<Vec<String>>>, Claude) {
        let keys = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = keys.clone();
        let seq = std::sync::Mutex::new(panes.into_iter());
        let last = std::sync::Mutex::new("");
        let c = claude_with(move |args| {
            if args.first() == Some(&"capture-pane") {
                let mut cur = last.lock().unwrap();
                if let Some(next) = seq.lock().unwrap().next() {
                    *cur = next;
                }
                return Ok(cur.to_string());
            }
            sink.lock().unwrap().push(args.join(" "));
            Ok(String::new())
        });
        (keys, c)
    }

    /// Claude Code 2.1.276 selects "No, exit" first. Enter alone would quit; move to Yes first.
    /// Trust is keyed on the repository root inside a git repository and on the folder itself
    /// outside one; the home directory is never persisted (Claude Code's permissions docs).
    #[test]
    fn the_trust_key_is_the_repo_root_or_the_folder_but_never_home() {
        let base = std::env::temp_dir().join(format!("agentgw-trust-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        let repo = home.join("repo");
        let plain = home.join("notes");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(Claude::trust_key(&home, &home), None, "home is never persisted");
        assert_eq!(Claude::trust_key(&repo.join("src/deep"), &home), Some(repo.clone()));
        assert_eq!(Claude::trust_key(&repo, &home), Some(repo.clone()));
        assert_eq!(Claude::trust_key(&plain, &home), Some(plain.clone()));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn trusting_a_folder_sets_only_its_flag_and_skips_when_already_set() {
        let config = serde_json::json!({
            "userID": "u",
            "projects": { "/srv/app": { "allowedTools": ["Read"] } }
        });
        let updated = Claude::with_trust(&config, "/srv/app").expect("needs writing");
        assert_eq!(updated["projects"]["/srv/app"]["hasTrustDialogAccepted"], true);
        assert_eq!(updated["projects"]["/srv/app"]["allowedTools"][0], "Read", "other fields kept");
        assert_eq!(updated["userID"], "u");
        assert_eq!(Claude::with_trust(&updated, "/srv/app"), None, "already trusted: no write");
        let fresh = Claude::with_trust(&serde_json::json!({}), "/x").unwrap();
        assert_eq!(fresh["projects"]["/x"]["hasTrustDialogAccepted"], true);
    }

    /// The modal came back on a real machine although `dismissedAt` was set: the current Claude Code
    /// reads `dismissed`. Writing it keeps the queue moving instead of holding a message on a modal.
    #[test]
    fn auto_mode_env_setup_is_dismissed_without_touching_its_siblings() {
        let old = serde_json::json!({"autoModeEnvSetup": {"dismissedAt": 1786800743891i64, "denials": 5}});
        let updated = Claude::with_auto_mode_dismissed(&old).expect("needs writing");
        assert_eq!(updated["autoModeEnvSetup"]["dismissed"], true);
        assert_eq!(updated["autoModeEnvSetup"]["denials"], 5, "siblings kept");
        assert_eq!(updated["autoModeEnvSetup"]["dismissedAt"], 1786800743891i64);
        assert_eq!(Claude::with_auto_mode_dismissed(&updated), None, "already dismissed: no write");
        let fresh = Claude::with_auto_mode_dismissed(&serde_json::json!({})).unwrap();
        assert_eq!(fresh["autoModeEnvSetup"]["dismissed"], true);
    }

    /// Answering a dialog from the thread: walk the cursor a row at a time, reading the screen
    /// after each press, and only then confirm. **The screen is the proof the key landed** —
    /// counting presses instead would confirm whatever row the cursor happened to be on.
    #[tokio::test]
    async fn a_dialog_is_answered_by_moving_the_cursor_then_confirming() {
        use crate::agent::screen::tests::{TRUST_PANE_NO_FIRST, TRUST_PANE_YES_SECOND};
        // First read: "No, exit" selected. After one Down the second read shows Yes selected
        let (calls, c) = deliver_probe(vec![TRUST_PANE_NO_FIRST, TRUST_PANE_YES_SECOND]);
        c.answer_dialog(&Window::of("@42"), Some(1), &LogCtx::default())
            .await
            .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.iter().filter(|c| c.ends_with(" Down")).count(), 1);
        assert_eq!(enters(&calls), 1);
        assert!(
            calls.iter().all(|c| !c.contains("send-keys -l")),
            "not one character is typed at a dialog: {calls:?}"
        );
    }

    /// Someone answered it on the machine while the buttons sat in the thread. Pressing Enter
    /// now would land in the input box underneath, so the answer is refused instead.
    #[tokio::test]
    async fn answering_a_dialog_that_has_gone_is_refused_rather_than_pressed() {
        let (calls, c) = deliver_probe(vec!["\u{276f} \u{a0}"]);
        let err = c
            .answer_dialog(&Window::of("@42"), Some(1), &LogCtx::default())
            .await
            .unwrap_err();
        assert!(err.contains("no longer on"), "{err}");
        assert_eq!(enters(&calls.lock().unwrap()), 0);
    }

    #[tokio::test]
    async fn a_trust_dialog_with_no_selected_is_moved_to_yes_before_enter() {
        use crate::agent::screen::tests::{TRUST_PANE_NO_FIRST, TRUST_PANE_YES_SECOND};
        let (keys, c) = claude_showing(vec![TRUST_PANE_NO_FIRST, TRUST_PANE_YES_SECOND, ""]);
        let out = c
            .watch_spawn_screens(&Window::of("1-1"), WATCH_MS, 1, &LogCtx::default())
            .await;
        assert_eq!(out, SpawnOutcome::Answered);
        let keys = keys.lock().unwrap();
        let sent: Vec<&str> = keys.iter().map(|k| k.rsplit(' ').next().unwrap_or("")).collect();
        assert_eq!(sent, ["Down", "Enter"], "{keys:?}");
    }

    #[tokio::test]
    async fn the_trust_dialog_is_answered_once_and_the_watch_keeps_going() {
        // Even if the same screen shows twice, Enter only once
        let (keys, c) = claude_showing(vec![TRUST_PANE, TRUST_PANE, ""]);
        let out = c
            .watch_spawn_screens(&Window::of("1-1"), WATCH_MS, 1, &LogCtx::default())
            .await;
        assert_eq!(out, SpawnOutcome::Answered);
        let keys = keys.lock().unwrap();
        assert_eq!(
            keys.iter().filter(|k| k.ends_with("Enter")).count(),
            1,
            "trust に撃つ Enter は1回だけ: {keys:?}"
        );
    }

    #[tokio::test]
    async fn the_login_screen_aborts_the_watch_without_pressing_anything() {
        let (keys, c) = claude_showing(vec![LOGIN_PANE]);
        let out = c
            .watch_spawn_screens(&Window::of("1-1"), WATCH_MS, 1, &LogCtx::default())
            .await;
        assert_eq!(out, SpawnOutcome::LoginRequired);
        // A screen Enter does not clear — firing would send the input box's contents, so do not fire
        let keys = keys.lock().unwrap();
        assert!(keys.is_empty(), "拒絶画面にはキーを送らない: {keys:?}");
    }

    /// A window that is gone (the agent exited, tmux has no server) can't show a start-up
    /// screen any more. Keep polling it and every poll logs an error: with a pool agent that
    /// died and was re-launched every 5 s, 30 watches at once filled a 17 GB log.
    #[tokio::test]
    async fn the_watch_ends_when_the_window_is_gone() {
        let captures = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = captures.clone();
        let c = claude_with(move |args| {
            if args.first() == Some(&"capture-pane") {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Err("no server running on /tmp/tmux-0/default".to_string());
            }
            Ok(String::new())
        });
        let out = c
            .watch_spawn_screens(&Window::of("1-1"), 2_000, 1, &LogCtx::default())
            .await;
        assert_eq!(out, SpawnOutcome::Exited, "the caller is told the window went away");
        assert_eq!(captures.load(std::sync::atomic::Ordering::SeqCst), 1, "one failed read is enough");
    }

    #[tokio::test]
    async fn a_quiet_pane_runs_out_the_budget_and_says_so() {
        let (keys, c) = claude_showing(vec!["still booting\u{2026}"]);
        let out = c
            .watch_spawn_screens(&Window::of("1-1"), WATCH_MS, 1, &LogCtx::default())
            .await;
        assert_eq!(out, SpawnOutcome::NoScreen);
        assert!(keys.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_confirm_screen_is_answered_but_the_watch_does_not_stop() {
        // Answering confirm does not mean "started". The latch is cleared by user_prompt
        let (keys, c) = claude_showing(vec!["Yes, proceed with local development", TRUST_PANE, ""]);
        let out = c
            .watch_spawn_screens(&Window::of("1-1"), WATCH_MS, 1, &LogCtx::default())
            .await;
        assert_eq!(out, SpawnOutcome::Answered);
        let keys = keys.lock().unwrap();
        assert_eq!(
            keys.iter().filter(|k| k.ends_with("Enter")).count(),
            2,
            "confirm と trust に1回ずつ: {keys:?}"
        );
    }

    #[test]
    fn auth_status_is_read_from_its_json_not_its_exit_code() {
        // `claude auth status` exits 1 when signed out but still prints the JSON
        assert_eq!(Claude::auth_status_signed_in(r#"{"loggedIn": true, "authMethod": "claude.ai"}"#), Some(true));
        assert_eq!(Claude::auth_status_signed_in(r#"{"loggedIn": false, "authMethod": "none"}"#), Some(false));
        assert_eq!(Claude::auth_status_signed_in("not json"), None);
        assert_eq!(Claude::auth_status_signed_in(r#"{"authMethod": "none"}"#), None);
    }

    #[test]
    fn a_login_error_in_the_transcript_reads_as_authentication_failed() {
        let line = |text: &str| {
            format!(
                "{}\n",
                serde_json::json!({
                    "isApiErrorMessage": true,
                    "timestamp": "2026-09-18T13:52:53.000Z",
                    "message": { "content": [{ "type": "text", "text": text }] },
                })
            )
        };
        let login = line("Login expired · Please run /login");
        assert_eq!(Transcript::failure_type(&login), Some("authentication_failed"));
        // Only the **last** error is looked at
        let later = format!("{login}{}", line("API Error: overloaded_error"));
        assert_eq!(Transcript::failure_type(&later), None);
        assert_eq!(Transcript::failure_type(""), None);
    }

    /// LIMIT_MODAL_PROMPTS reduced to unanchored literals.
    /// api-errors other than "limit" are not picked up (no wall over a transient overload error).
    #[test]
    fn limit_error_is_read_from_the_transcript_tail() {
        let now = 1_785_000_000_000u64;
        let line = |ts: &str, text: &str| {
            format!(
                "{}\n",
                serde_json::json!({
                    "isApiErrorMessage": true,
                    "timestamp": ts,
                    "message": { "content": [{ "text": text }] },
                })
            )
        };
        let tail = line(
            "2026-07-30T14:00:00.000Z",
            "Claude usage limit reached. Your limit will reset at 11pm",
        );
        let hit = Transcript::limit_hit(&tail, now).expect("limit hit");
        assert!(hit.detail.contains("usage limit reached"), "{hit:?}");
        assert!(hit.reset_ms > now);

        // api-errors that are not limits are not picked up
        let other = line("2026-07-30T14:00:00.000Z", "API Error: overloaded_error");
        assert!(Transcript::limit_hit(&other, now).is_none());

        // The tail slice starts mid-line — a partial line must neither crash nor stop it
        let sliced = format!("Message\",\"isApiErrorMessage\":true}}\n{tail}");
        assert!(Transcript::limit_hit(&sliced, now).is_some());

        // If the reset time is unreadable, error time + 1h. If even that has passed it is history, so None
        let at = 1_785_420_000_000u64; // 2026-07-30T14:00:00Z
        let no_time = line("2026-07-30T14:00:00.000Z", "Claude usage limit reached.");
        assert_eq!(
            Transcript::limit_hit(&no_time, at + 1_800_000).map(|h| h.reset_ms),
            Some(at + 3_600_000)
        );
        assert!(Transcript::limit_hit(&no_time, at + 2 * 3_600_000).is_none());
    }

    /// **A moved conversation is told so before its first message.** Its own record is the other
    /// machine's, and it will read that record as memory and act on the paths in it.
    #[test]
    fn a_moved_conversation_is_told_where_it_came_from() {
        let mut r = req("sid-2", Some("<channel …>hello</channel>"), WorkerState::Absent);
        r.resume_from = Some(SessionId::from("sid-2".to_string()));
        r.moved_from = Some(crate::bridge::state::MovedFrom {
            machine: "tyo-mpv5l".into(),
            path: "/Users/t/dev/agentgw".into(),
        });
        let mut seen = String::new();
        let claude = claude_with(move |args| {
            Ok(args.join(" "))
        });
        // The notice has to be in the launch line, ahead of the message
        let line = claude.launch_line(
            &SessionMode::Resume("sid-2".into()),
            &r.hooks_file,
            &r.mcp_config,
            &format!(
                "{}\n\n{}",
                Claude::moved_notice(r.moved_from.as_ref().unwrap(), &r.cwd),
                "<channel …>hello</channel>"
            ),
        );
        seen.push_str(&line);
        assert!(seen.contains("tyo-mpv5l"), "the old machine is not named: {seen}");
        assert!(seen.contains("/Users/t/dev/agentgw"), "the old folder is not named");
        assert!(seen.contains("/repo"), "where it is now is not named");
        let notice_at = seen.find("has been moved").expect("no notice");
        let msg_at = seen.find("hello").expect("no message");
        assert!(notice_at < msg_at, "the message comes before the notice");
    }

    /// A conversation that was not moved says nothing extra — the usual startup, unchanged.
    #[test]
    fn a_conversation_that_stayed_put_is_told_nothing_extra() {
        let r = req("sid-1", Some("<channel …>hi</channel>"), WorkerState::Absent);
        assert!(r.moved_from.is_none());
        let line = quiet().launch_line(
            &SessionMode::New("sid-1".into()),
            &r.hooks_file,
            &r.mcp_config,
            r.prompt.as_deref().unwrap(),
        );
        assert!(!line.contains("has been moved"), "{line}");
    }

    /// **Against real git, because git's own answer is the thing being relied on.** A fake would only
    /// repeat the belief under test: that a worktree and the checkout it belongs to can be told apart,
    /// and that removal refuses when work would be lost.
    ///
    /// The checkout case is the one that matters. Getting it wrong deletes the project. It was already
    /// wrong once — the two git answers are not both absolute, and comparing them as strings made
    /// every checkout look like a worktree.
    #[test]
    fn a_worktree_comes_down_but_the_checkout_it_belongs_to_never_does() {
        let base = std::env::temp_dir().join(format!("agentgw-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "T"]);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-qm", "first"]);

        // Nothing here goes near tmux — it only shells out to git
        let claude = quiet();
        let at = |p: &std::path::Path| p.to_str().unwrap().to_string();

        // The checkout itself: not a worktree, and nothing is touched
        assert!(
            claude.remove_worktree(&at(&repo)).is_none(),
            "called the checkout a worktree"
        );
        assert!(repo.join("a.txt").exists(), "the checkout was disturbed");
        // Somewhere that is no repository at all
        assert!(claude.remove_worktree(&at(&base)).is_none());

        // A clean worktree comes down
        let clean = base.join("clean");
        git(&repo, &["worktree", "add", "-q", "-b", "clean", clean.to_str().unwrap()]);
        assert!(matches!(claude.remove_worktree(&at(&clean)), Some(Ok(()))));
        assert!(!clean.exists(), "it said it removed the worktree and did not");

        // One with work in it is refused, and left where it is
        let dirty = base.join("dirty");
        git(&repo, &["worktree", "add", "-q", "-b", "dirty", dirty.to_str().unwrap()]);
        std::fs::write(dirty.join("unsaved.txt"), "work").unwrap();
        assert!(matches!(claude.remove_worktree(&at(&dirty)), Some(Err(_))));
        assert!(dirty.join("unsaved.txt").exists(), "unsaved work was destroyed");

        let _ = std::fs::remove_dir_all(&base);
    }
}
