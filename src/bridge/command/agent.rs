//! Commands aimed at the thread's agent: stop / exit / resume / context / usage / compact /
//! model / effort / mode, and signing in and out (login / logout).
//!
//! The one place allowed to implement methods on agent types (`ContextReport`,
//! `CompactProgress`, `UsageRow`): how they look in Slack is this file's concern.

use super::CmdFx;
use crate::agent::SpawnOutcome;
use crate::agent::Window;
use crate::agent::Agent;
use crate::agent::{
    CompactOutcome, CompactProgress, ContextCategory, ContextReport, LoginOutcome, ProbeErr,
    SessionId, UsageRow,
};
use crate::chat::InboundMsg;
use crate::log::LogCtx;
use crate::chat::ThreadKey;
use crate::clock::WallClock;
use crate::bridge::turn::UsageProjection;
use crate::bridge::{Bridge, Host};
use crate::chat::slack;
use tokio::sync::mpsc;

/// Sign-in polling (per the constants). The URL normally appears within 1–3 seconds.
const LOGIN_POLL: std::time::Duration = std::time::Duration::from_secs(1);

const URL_POLL_MAX: u32 = 20;

const CODE_POLL_MAX: u32 = 30;

/// The one line returned to a thread with no session (shared by all commands).
fn no_session() -> String {
    crate::t!(
        "This thread has no agent running. Send a message to start one.",
        "このスレッドには、動いているエージェントがありません。メッセージを送ると始まります。"
    )
}

impl Bridge {
    pub(in crate::bridge) fn user_stop(&mut self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
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
        // Short-circuit in order — a stop with nothing unanswered doesn't go poke tmux
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
        // Drop the ledger **first** — otherwise the turn cut by ESC ends still holding an unanswered message,
        // and the stop hook fires an "awaiting reply" re-prompt
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

    /// `exit` / `bye` / `done`. What ends is
    /// **the agent, not the thread** — the threads.json entry stays, so the next message
    /// continues the same session with `--resume`. A running turn is waited for, not cut (unlike stop):
    /// so the farewell lands **below** the agent's last reply.
    pub(super) async fn user_exit(&mut self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
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
        // Short-circuit — an exit on a thread with no session doesn't go poke tmux
        let live = sid.is_some() && self.deps.agent.pid_of(window_id.as_deref(), &name).is_some();
        // An exit with no agent still says goodbye.
        // There is nothing to wait for, so clean up on the spot instead of scheduling it
        let Some(sid) = sid.filter(|_| live) else {
            self.terminate(key, None, farewell).await;
            return;
        };
        self.push_drain(key, sid, farewell, None, ctx); // exit posts a farewell, so no shimmer is needed
    }

    /// `resume`. Posts the one line that continues the session in a local terminal,
    /// and **really hands the line over** — if the agent is alive it ends after the same drain as exit
    /// (no farewell; the report already says it is ending).
    pub(super) fn user_resume(&mut self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let entry = self.threads.get(root_ts).cloned().unwrap_or_default();
        // **Never issue** a session ID — an id handed to a thread that never ran just fails in the terminal
        let Some(sid) = entry.agent_id.filter(|s| !s.is_empty()) else {
            ctx.info(
                "bridge",
                &format!("resume: no bound session for thread tts={root_ts} — nothing to resume"),
            );
            let none = ResumeInfo {
                session_id: None,
                cwd: None,
                transcript_missing: false,
                worker_running: false,
            };
            self.post(&msg.channel, root_ts, none.render(self.deps.agent.as_ref()), key);
            return;
        };
        // Where the history lives: the path the hook brought wins (an agent that entered a worktree moves
        // to another project along with its transcript — the recorded repo_path can't be trusted.)
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
        let info = ResumeInfo {
            // The cwd matters as much as the id — claude stores history per cwd,
            // so --resume from a different place won't find it
            cwd: Some(history_cwd.or(entry.repo_path).unwrap_or_else(Host::home)),
            transcript_missing: !history_exists,
            worker_running,
            session_id: Some(sid.clone()),
        };
        // First 8 characters by char, as in the other six places (a byte index panics on a non-ASCII id —
        // and this is inside the select loop). The transcript is a yes/no, not a location
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
        self.post(&msg.channel, root_ts, info.render(self.deps.agent.as_ref()), key);
        if worker_running {
            ctx.info(
                "bridge",
                &format!(
                    "slack-events: resume command → terminating live worker for {key} \
                     (session handed to the Owner's terminal)"
                ),
            );
            // **Hand the shimmer to the drain.** The resume itself only peeks at tmux once,
            // so holding the guard here would just fire set and clear back to back and never render.
            // The actual waiting happens in the drain, which waits up to 30 seconds for the reply to go out
            let thinking =
                slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, true);
            self.push_drain(key, sid, None, Some(thinking), ctx);
        }
    }

    /// `context` / `ctx`. Duplicates this thread's own
    /// session with `--fork-session` and asks `/context` — the running agent is not
    /// touched, so it answers even mid-turn. The probe takes 10–20 seconds, so it is fire-and-forget.
    pub(super) fn user_context(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let entry = self.threads.get(root_ts);
        // A thread with no session has nothing to ask
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
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        let thinking =
            slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, true);
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = clear (goes away even if the probe fails)
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
                // The failure reason is an internal English string — keep it in the log, not the thread
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

    /// `usage` / `usg`. It is about the whole account,
    /// so no thread or session is needed — just run `claude -p /usage` at Home.
    pub(super) fn user_usage(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let cwd = Host::home();
        ctx.info("bridge", &format!("usage: probing /usage cwd={cwd}"));
        let argv = self.deps.agent.usage_argv();
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, true);
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = clear
            let ctx = LogCtx {
                session_id: None,
                thread_key: Some(key.clone()),
            };
            let out = match agent.probe(argv, cwd).await {
                Ok(raw) => {
                    ctx.info("bridge", &format!("usage: probe ok bytes={}", raw.len()));
                    match agent.usage_rows(&raw) {
                        // 300 = the "Current session" window (5h). The "Current week" row's window
                        // is swapped in inside format_usage_report_with_projection
                        Some(rows) => UsageReport {
                            rows: &rows,
                            projection: Some((crate::clock::WallClock::now(), 300)),
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

    /// Shared entry for commands that drive the TUI (compact / model / effort).
    /// Resolves the session and window, and refuses while a turn is running — you can't type into a busy TUI.
    /// Poke tmux only on `Ok((target, session_id))`. `Err` is the line to return as-is.
    pub(super) fn tui_guard(
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
        // **The record isn't the agent.** A thread keeps its session id after its agent is gone, and typing
        // into a window that isn't there answered with a raw tmux error ("can't find window")
        if self.deps.agent.pid_of(window_id.as_deref(), &name).is_none() {
            ctx.info(
                "bridge",
                &format!("{label}: session {sid} is not running — {no_session_tail}"),
            );
            return Err(no_session());
        }
        // Check unanswered messages first — if empty, don't go poke tmux (the same short-circuit as user_stop)
        let pending = self.ledger.pending(key);
        if !pending.is_empty() {
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

    /// `compact`. Types `/compact` into the TUI of the **live session**
    /// and streams the progress spinner into one Slack progress message
    /// (unlike context/usage this is not a throwaway probe — it compresses the running conversation itself).
    /// It can take up to 6 minutes, so it runs outside the select loop (spawned).
    pub(super) fn user_compact(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
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
        // compact gets **no** dedicated thinking status (a user decision).
        // An earlier version set "thinking" here and re-set it each tick with the seconds.
        // The progress checklist (the sticky that run_compact posts and keeps editing) already shows
        // the same thing, so a shimmer on top was judged redundant (**not a porting omission**).
        tokio::spawn(Self::run_compact(api, self.deps.agent.clone(), channel, root, key, target, sid));
    }

    /// `model`. With a name, types
    /// `/model <name>` into the TUI. Bare `model` only reads the transcript — the agent is not touched.
    pub(super) fn user_model(
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
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, true);
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = clear (goes away even if the TUI never confirms)
            let ctx = LogCtx {
                session_id: Some(sid),
                thread_key: Some(key.clone()),
            };
            // None = this agent doesn't support switching model. Can't happen while there is only one implementation
            let done = agent.set_model(&target, &name, &key, &ctx).await == Some(true);
            let out = if done {
                crate::t!("✅ Switched this thread's model to *{name}*.", "✅ このスレッドのモデルを *{name}* に切り替えました。")
            } else {
                crate::t!("Couldn't switch the model. Try again.", "モデルを切り替えられませんでした。もう一度試してください。")
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// Bare `model`. The current model is named by the **last** assistant record in the
    /// transcript. No TUI and no probe needed, so it answers inside the select loop
    /// (it reads only the last 256KB — a long session's .jsonl can be tens of MB).
    pub(super) fn model_show(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let Some(sid) = self.threads.get(root_ts).and_then(|e| e.agent_id.clone()) else {
            ctx.info(
                "bridge",
                &format!("model: no bound session for thread tts={root_ts} — no current model"),
            );
            self.post(&msg.channel, root_ts, no_session(), key);
            return;
        };
        let short: String = sid.chars().take(8).collect();
        // Where the history lives: the path the hook brought wins (an agent that entered a worktree moves
        // along with its transcript). If forgotten, search exhaustively — the same resolution order as resume
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

    /// `effort`. With a level, it drives the TUI
    /// like model. The current value of bare `effort` is **recorded nowhere**, so it asks the TUI —
    /// open the slider, close it with Escape, and read the status line the TUI prints.
    ///
    /// **How the TUI behaves** (all three paths confirmed on a real machine, 2026-07-29): without history it immediately says
    /// `Set effort level to …`; with history it shows a confirm dialog and then **only the status line**;
    /// re-picking the same level says `Kept effort level as …`.
    pub(super) fn user_effort(
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
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        // Bare `effort` only reads the current value — no status is shown
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
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, true);
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = clear
            let ctx = LogCtx {
                session_id: Some(sid),
                thread_key: Some(key.clone()),
            };
            // None = this agent doesn't support effort. Can't happen while there is only one implementation
            let done = agent.set_effort(&target, &level, &key, &ctx).await == Some(true);
            let out = if done {
                crate::t!("✅ Set this thread's effort level to *{level}*.", "✅ このスレッドの effort を *{level}* にしました。")
            } else {
                crate::t!("Couldn't set the effort level. Try again.", "effort を設定できませんでした。もう一度試してください。")
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// `mode` (new in this implementation). Claude Code's permission mode can only be changed with shift+tab
    /// in the TUI, so there is no slash command like `/effort` — it **presses the key** repeatedly until
    /// it lands on the wanted mode. Bare `mode` just reads the footer once.
    pub(super) fn user_mode(
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
        // Reading only pokes tmux once — no spawn or shimmer needed
        let Some(name) = name else {
            let out = match self.deps.agent.mode(&target, &ctx) {
                Some(m) => crate::t!("Permission mode: *{m}*", "権限モード: *{m}*"),
                None => crate::t!("Couldn't read the permission mode.", "権限モードを読み取れませんでした。"),
            };
            self.post(&msg.channel, root_ts, out, key);
            return;
        };
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, true);
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = clear
            let done = agent.set_mode(&target, &name, &key, &ctx).await == Some(true);
            let out = if done {
                crate::t!("✅ Switched this thread's permission mode to *{name}*.", "✅ このスレッドの権限モードを *{name}* にしました。")
            } else {
                crate::t!("Couldn't switch the permission mode. Try again.", "権限モードを切り替えられませんでした。もう一度試してください。")
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// Type `/compact` and stream the pane's spinner into a Slack progress message. The progress message is created **only after the first progress appears** — that is why
    /// a busy / no-session refusal stays a single message. The last line edits the progress message if there is one, otherwise posts anew.
    ///
    /// Driving the TUI is the agent's job (`Agent::compact`). This is **only the side that draws progress on Slack** —
    /// it can take up to 6 minutes, so it runs outside the select loop (spawned).
    pub(super) async fn run_compact(
        api: crate::chat::ChatRef,
        agent: crate::agent::AgentRef,
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
                // Don't redraw the same picture (Slack edits aren't free)
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
                    // Compaction continues even if a draw fails
                    Err(e) => ctx.error("bridge", &format!(
                        "slack-events: compact progress render failed for {channel}:{root}: {e}"
                    )),
                }
            }
            progress_ts
        };
        let (outcome, progress_ts) = tokio::join!(agent.compact(&target, &key, &sid, tx), draw);
        // None = this agent doesn't support compact. Can't happen while there is only one implementation
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

    /// Bare `effort`. Asking the TUI for the current level is the agent's side
    /// (`Agent::effort`). This only turns the answer into one line and posts it.
    pub(super) async fn run_effort_show(
        api: crate::chat::ChatRef,
        agent: crate::agent::AgentRef,
        channel: String,
        root: String,
        key: ThreadKey,
        target: Window,
        sid: String,
    ) {
        let failed = || crate::t!("Couldn't read the current effort level.", "今の effort を読み取れませんでした。");
        // None = unsupported, or the status line couldn't be read — both get the same refusal
        let out = match agent.effort(&target, &key, &sid).await {
            Some(level) => crate::t!("Effort level: *{level}*", "effort: *{level}*"),
            None => failed(),
        };
        api.post_now(&channel, &root, out, &key).await;
    }

    /// The sign-in shortcut, taken only while there is no Owner yet.
    /// **true = consumed** — the caller stops there. It is reached in two places:
    ///   • A human's DM — if a sign-in is in progress, the next message is read as the pasted code. Bare `login`
    ///     starts a new sign-in. Anything else gets a short hint
    ///   • A channel the Owner routed — it accepts **only the pasted code**, and only from
    ///     the person who started that sign-in (a bystander never gets to touch the code prompt)
    pub(in crate::bridge) fn login_carve_out(&mut self, msg: &InboundMsg) -> bool {
        let dm = msg.channel_kind == crate::chat::ChannelKind::Dm;
        let sender = msg.user.as_deref().unwrap_or("");
        let pending = self.sign_in.pending.get(&msg.channel).cloned();
        if !dm && pending.as_deref() != Some(sender) {
            return false; // passers-by are outside this branch — fall through to the gate as usual
        }
        // Hang the reply under "the message that person sent". Posting it at the DM/channel root would land
        // somewhere other than the thread they are watching
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

    /// Start a sign-in. Runs `claude auth login` in a dedicated tmux session,
    /// scoops up the auth URL it prints and returns it. From here on it waits for the code.
    pub(super) fn start_login(&mut self, channel: String, user: String, reply_ts: String) {
        let key = ThreadKey::new(&channel, &reply_ts);
        let ctx = LogCtx::default();
        // SECURITY: sign-in uses **one shared** tmux session for everyone.
        // If a second attempt from another channel recreated the session, the first person's polling would read
        // the second person's pane, and the second one's success would make **the first person** Owner (piggybacking
        // on someone else's auth). Retrying in the same channel only restarts your own flow, so it is allowed
        if let Some(other) = self.sign_in.pending.keys().find(|k| **k != channel) {
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
        // Take the seat **right after** the check. Taking it inside the spawn (after the URL appears) would let a
        // second attempt from another channel slip past the same check during the up-to-20 seconds of waiting for the URL,
        // recreate the session and steal the first one's pane. LoginFinished frees the seat on success or failure.
        // Cost: a message arriving in this channel before the URL shows up is treated as the code and gets a failure reply
        // (the Owner can just send `login` again)
        self.sign_in.pending.insert(channel.clone(), user.clone());
        let (api, cmd_tx, home) = (self.deps.slack.clone(), self.cmd_tx.clone(), Host::home());
        // Sign-in takes tens of seconds (show the URL, then wait for the code) — keep the shimmer up the whole time
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &channel, &reply_ts, true);
        // `thinking` is used in the body, so the async move takes it whole (Drop = clear whichever path exits)
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
                // Wait **only** for the URL. A broad error check here would give up by mistake on an "error/failed"
                // buried in the decoration before the URL. A real early failure shows up below
                // as "timed out without a URL"
                for _ in 0..URL_POLL_MAX {
                    tokio::time::sleep(LOGIN_POLL).await;
                    // Up to URL_POLL_MAX seconds until the URL appears — Slack expires the status sooner than that,
                    // so re-set it each tick (same reason as compact).
                    // Without that the shimmer vanishes midway and it looks "stuck"
                    thinking.set(true);
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
                // Main frees the seat (and kills the session too). If this isn't sent back,
                // `login` would never be accepted again
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

    /// Feed the pasted code to the waiting CLI and wait for the screen's verdict.
    /// The Owner is bound **only on seeing an explicit success marker** — neither silence nor interruption counts as success.
    ///
    /// ponytail: no detection of the CLI returning to the shell — a 30-second
    /// timeout stands in (at worst the failure reply is up to 30 seconds late)
    pub(super) fn submit_code(&self, channel: String, code: String, reply_ts: String, user: String) {
        let key = ThreadKey::new(&channel, &reply_ts);
        LogCtx::default().info(
            "bridge",
            &format!("login: submitting pasted code for {user} (channel {channel})"),
        );
        let (api, cmd_tx) = (self.deps.slack.clone(), self.cmd_tx.clone());
        // If an Owner already exists, this is re-signing-in Claude Code on this machine. The Owner doesn't change
        let had_owner = !self.access.owner.is_empty();
        // The second half of sign-in (checking the pasted code, up to CODE_POLL_MAX seconds) is also waiting —
        // login_start's guard dropped once the URL was shown, so set it again here
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &channel, &reply_ts, true);
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
                    thinking.set(true); // don't let it expire (same as waiting for the URL)
                    // Read including scrollback: right after printing "Login successful." the CLI returns to the shell,
                    // and the marker scrolls off screen before the next poll
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
            // Cleaning up the session, removing the pending entry and writing the Owner are main's job
            let (text, bound) = if outcome == "success" {
                (
                    if had_owner {
                        crate::t!("Signed in ✅", "サインインしました ✅")
                    } else {
                        crate::t!(
                            "Signed in ✅ — you're now the owner of this bot.",
                            "サインインしました ✅ — あなたがこのボットの Owner になりました。"
                        )
                    },
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

    /// `logout`. Only the CLI sign-out is spawned;
    /// cleaning up agents and clearing the Owner is handed back to main.
    pub(super) fn user_logout(&mut self, channel: &str, root_ts: &str) {
        // Unlike the restart flag this one **really matters** — user_logout spawns and returns at once,
        // so a second logout can arrive during the few seconds `claude auth logout` runs
        if self.sign_in.signing_out {
            LogCtx {
                session_id: None,
                thread_key: Some(ThreadKey::new(channel, root_ts)),
            }
            .info("bridge", "logout ignored — already signing out");
            return;
        }
        self.sign_in.signing_out = true;
        // The shimmer lasts until `claude auth logout` returns. Tearing down agents afterwards is main's side
        // (CmdFx::LogoutFinished), so holding it here guarantees it **always** goes away
        let thinking = slack::Thinking::new(self.deps.slack.clone(), channel, root_ts, true);
        let (cmd_tx, channel, thread_ts) = (
            self.cmd_tx.clone(),
            channel.to_string(),
            root_ts.to_string(),
        );
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = clear
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
            // Run the cleanup whether or not the sign-out succeeded — agents half-surviving is worse
            let _ = cmd_tx
                .send(CmdFx::LogoutFinished {
                    ok,
                    channel,
                    thread_ts,
                })
                .await;
        });
    }

    /// Watch this machine's Claude Code sign-in. Checked at startup and every 12 hours (a user decision);
    /// notify the notification target **once, at the moment it is found expired**. Nothing is said when it comes back. If it
    /// couldn't be checked (`None`), the state is left alone. Not watched while there is no Owner yet (before first setup) —
    /// being signed out is expected then, and there is nobody to tell
    pub(in crate::bridge) async fn sign_in_tick(&mut self) {
        const EVERY_MS: u64 = 12 * 60 * 60 * 1000;
        let now = self.deps.clock.now_ms();
        let due = self.sign_in.checked_at_ms == 0 || now >= self.sign_in.checked_at_ms + EVERY_MS;
        if !due || self.access.owner.is_empty() {
            return;
        }
        self.sign_in.checked_at_ms = now;
        let Some(signed_in) = self.deps.agent.signed_in().await else {
            return;
        };
        let was = self.sign_in.last_known.replace(signed_in);
        if signed_in || was == Some(false) {
            return;
        }
        let ctx = LogCtx::default();
        ctx.info("bridge", "sign-in watch: this machine's agent is signed out — telling the owner");
        let machine = &self.machine_name;
        let text = crate::t!(
            "🔑 Claude Code on *{machine}* is signed out. Send `login` in a channel {machine} handles to sign in again.",
            "🔑 *{machine}* の Claude Code のサインインが切れています。{machine} が受け持つチャンネルで `login` と送ると、サインインし直せます。"
        );
        self.post_notice(&text, &ctx).await;
    }

    /// Apply, one by one on main's side, the state changes returned by spawned sign-ins and sign-outs.
    pub(in crate::bridge) async fn on_cmd_fx(&mut self, fx: CmdFx) {
        let ctx = LogCtx::default();
        match fx {
            CmdFx::LoginFinished { channel, bound } => {
                self.deps.agent.login_kill();
                self.sign_in.pending.remove(&channel);
                let Some(user) = bound else { return };
                if self.access.owner.is_empty() {
                    let mut access = self.access.clone();
                    access.owner.clone_from(&user);
                    self.adopt_access(access, &ctx);
                    ctx.info(
                        "bridge",
                        &format!("login: SUCCESS — {user} bound as Owner (channel {channel})"),
                    );
                } else {
                    // The Owner is already set — this is re-signing-in on this machine
                    ctx.info(
                        "bridge",
                        &format!(
                            "login: SUCCESS — signed in again by {user}; Owner unchanged (channel {channel})"
                        ),
                    );
                }
                // Sign-in is a fresh start — retry slots that gave up under the previous auth state
                // (without this, a temporary failure leaves the slot empty for the Bridge's whole lifetime)
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
                // pool_targets only becomes real once the Owner is set — this is the first pool fill
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
                self.sign_in.pending.clear();
                self.sign_in.signing_out = false;
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
            CmdFx::SpawnScreen { outcome, what, key, session_id } => {
                let ctx = LogCtx::default();
                match outcome {
                    // Only news while the agent never took its first message. One that started and later
                    // exited is someone else's story (exit / logout / the recovery paths), and a window the
                    // watch merely lost sight of (the process is still there) isn't an exit at all
                    SpawnOutcome::Exited => {
                        let window = self
                            .workers
                            .warm(&session_id)
                            .and_then(|h| h.window_id.clone());
                        let name = crate::agent::SessionId::from(session_id.clone()).window_name();
                        let gone = self.deps.agent.pid_of(window.as_deref(), &name).is_none();
                        if !self.workers.is_starting(&session_id) || !gone {
                            return;
                        }
                        match key {
                            Some(key) => {
                                let ctx = LogCtx {
                                    session_id: Some(session_id.clone()),
                                    thread_key: Some(key.clone()),
                                };
                                self.agent_never_started(&key, &session_id, &ctx);
                            }
                            // A warm pool agent: nobody is waiting on it. Refilling the pool is its own job
                            None => {
                                self.workers.clear_starting(&session_id);
                                ctx.error(
                                    "bridge",
                                    &format!("{what} exited before its first message (nobody waiting)"),
                                );
                            }
                        }
                    }
                    // The pane is only a **suspicion**. Confirming it is the existing /usage watch's job,
                    // so just move its deadline into the past so the next flush checks. **Never use 0** —
                    // `usage_tick` uses it as the "first run, wait a bit" signal, so writing 0 not only
                    // skips the check but pushes the next probe 60 seconds out
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
                    // No fleet gate (there is no `claude auth status` confirmation).
                    // Just say it. The Owner can send `login`
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
}

impl UsageRow {
    /// The window in minutes this row is projected over (None for rows not projected).
    pub fn window_minutes(label: &str, session_minutes: i64) -> Option<i64> {
        let l = label.to_ascii_lowercase();
        if l.contains("current session") {
            Some(session_minutes)
        } else if l.contains("current week") {
            Some(UsageProjection::WEEK_MINUTES)
        } else {
            None
        }
    }
}

/// Inputs for `resume`.
pub struct ResumeInfo {
    pub session_id: Option<String>,
    /// Where the session's history lives (= the agent's startup cwd)
    pub cwd: Option<String>,
    /// No history found on disk → resuming will most likely fail
    pub transcript_missing: bool,
    /// An agent is alive in this thread right now → end it after handing the line over
    pub worker_running: bool,
}

impl ResumeInfo {
    /// The `resume` answer as Slack mrkdwn. The command to continue with is built by `agent`.
    pub fn render(&self, agent: &dyn Agent) -> String {
        let info = self;
        let Some(sid) = info.session_id.as_deref().filter(|s| !s.is_empty()) else {
            return crate::t!(
                "This thread has no session to resume yet. Send a message to start one first.",
                "このスレッドには、まだ再開できるセッションがありません。先にメッセージを送ってセッションを始めてください。"
            );
        };
        let cmd = agent.resume_command(info.cwd.as_deref().filter(|c| !c.is_empty()), sid);
        let mut lines = vec![
            crate::t!(
                "Run this to continue the session in your own terminal:",
                "手元の端末でセッションを続けるには、次を実行してください。"
            ),
            String::new(),
            "```".to_string(),
            cmd,
            "```".to_string(),
        ];
        if info.transcript_missing {
            lines.push(String::new());
            lines.push(crate::t!(
                "⚠️ This session's history isn't on disk, so resuming may fail.",
                "⚠️ このセッションの履歴がディスクに見つからないので、再開できないかもしれません。"
            ));
        }
        if info.worker_running {
            lines.push(String::new());
            lines.push(crate::t!(
                "The agent for this thread has been stopped so the session can continue there.",
                "続きを手元で進められるよう、このスレッドのエージェントは止めました。"
            ));
        }
        lines.join("\n")
    }
}

// ── `context` / `ctx` — this thread's own context breakdown ─
// **Reading** the output is `Pane::context_report` in `agent/claude.rs` (reading the screen and output
// is the agent implementation's job). What remains here is only **how to draw it** for Slack.

/// Parsed `/context` as Slack mrkdwn: a human-readable model name + a monospace usage bar + an aligned breakdown table.
/// The table ends with a **`Used space` total row** instead of the raw `Free space` row (to show the same "used" side
/// as the bar). The usage ratio prefers `100 − Free space` — one digit finer than the header's integer `4%`.
impl ContextReport {
    /// Like JS `parseFloat` — reads only the leading number (`"95.6%"` → 95.6). NaN if unreadable.
    /// No exponent notation: /context prints only decimal percentages and token counts.
    fn leading_f64(s: &str) -> f64 {
        let t = s.trim_start();
        let mut end = 0;
        let mut seen_dot = false;
        for (i, c) in t.char_indices() {
            match c {
                '+' | '-' if i == 0 => {}
                '.' if !seen_dot => seen_dot = true,
                _ if c.is_ascii_digit() => {}
                _ => break,
            }
            end = i + c.len_utf8();
        }
        t[..end].parse().unwrap_or(f64::NAN)
    }

    pub fn render(&self) -> String {
        let r = self;
        let mut lines = vec![
            "📊 *Context Usage*".to_string(),
            r.model_label.clone(),
        ];

        let free_pct = r
            .categories
            .iter()
            .find(|(name, _, _)| name.eq_ignore_ascii_case("free space"))
            .map_or(f64::NAN, |(_, _, pct)| Self::leading_f64(pct));
        let used_pct = if free_pct.is_finite() {
            100.0 - free_pct
        } else {
            Self::leading_f64(&r.pct)
        };
        let used_int = if used_pct.is_finite() {
            used_pct.round() as i64
        } else {
            0
        };
        lines.push(format!(
            "`{}`  {} / {} ( {used_int}% used )",
            UsageReport::bar(used_pct, 24),
            r.used,
            r.total
        ));

        // Table: the consuming categories (raw Free space dropped) + a `Used space` total row
        let consumers: Vec<&ContextCategory> = r
            .categories
            .iter()
            .filter(|(name, _, _)| !name.eq_ignore_ascii_case("free space"))
            .collect();
        let used_row_pct = if used_pct.is_finite() {
            format!("{used_pct:.1}%")
        } else {
            r.pct.clone()
        };
        // Width is the max over "every consuming row + the total row + a minimum width"
        let width = |min: usize, f: fn(&ContextCategory) -> &String, own: &str| {
            consumers
                .iter()
                .map(|c| f(c).chars().count())
                .chain([min, own.chars().count()])
                .max()
                .unwrap_or(min)
        };
        let name_w = width(10, |c| &c.0, "Used space");
        let tok_w = width(6, |c| &c.1, &r.used);
        let pct_w = width(5, |c| &c.2, &used_row_pct);
        let row = |n: &str, t: &str, p: &str| format!("{n:<name_w$}  {t:>tok_w$}  {p:>pct_w$}");
        let divider = "─".repeat(name_w + tok_w + pct_w + 4);

        let mut body = vec![
            "```".to_string(),
            row("Category", "Tokens", "%"),
            divider.clone(),
        ];
        body.extend(consumers.iter().map(|(n, t, p)| row(n, t, p)));
        body.push(divider);
        body.push(row("Used space", &r.used, &used_row_pct));
        body.push("```".to_string());
        lines.push(body.join("\n"));
        lines.join("\n")
    }
}

// ── `usage` / `usg` — summary of the account's subscription limits ────
// Usage for the **bot account**, not per thread. No burn-rate projection
// in this implementation — label + bar + `Resets …`.

// **Reading** the limit rows is `Pane::usage_rows` in `agent/claude.rs`. This is only how they are drawn.

// ── `compact` — drawing the progress line ─────────────
// **Reading** the pane during compaction is `Pane::compact_progress` in `agent/claude.rs`.

/// Width of the drawn bar, and of the glowing window that sweeps across it.
const CELLS: usize = 24;
const WINDOW: usize = 5;

/// Compaction state as a Slack progress line. A 🗜️ label + elapsed time, and if the pane shows a real %, a
/// **determinate** bar filled to it (the % goes **after** the bar — matching the real TUI's bar line `▐▏███…░ 31%`).
/// Without a %, fall back to the token count and an indeterminate bar whose glowing window moves (and wraps) with elapsed seconds.
impl CompactProgress {
    pub fn render(&self) -> String {
        let p = self;
        let s = p.seconds.unwrap_or(0);
        let elapsed = match p.seconds {
            None => String::new(),
            Some(_) if s >= 60 => format!(" {}m{}s", s / 60, s % 60),
            Some(_) => format!(" {s}s"),
        };
        if let Some(pct) = p.percent {
            // Determinate: the same bar /usage draws, with the % after it
            let pct = pct.min(100);
            let bar = UsageReport::bar(f64::from(pct), CELLS);
            return crate::t!(
                "🗜️ Compacting the context…{elapsed}\n`{bar}` {pct}%",
                "🗜️ コンテキストを圧縮中…{elapsed}\n`{bar}` {pct}%"
            );
        }
        // Indeterminate: a glowing window sweeping over the bar with elapsed seconds
        let tok = match &p.tokens {
            // No arrow → an **empty string**. `Option<char>`'s default is `'\0'`, so it can't be used
            Some(t) => format!(
                " · {}{t} tokens",
                p.tokens_dir.map(String::from).unwrap_or_default()
            ),
            None => String::new(),
        };
        let pos = s as usize % CELLS;
        let bar: String = (0..CELLS)
            .map(|i| {
                if (i + CELLS - pos) % CELLS < WINDOW {
                    '█'
                } else {
                    '░'
                }
            })
            .collect();
        crate::t!(
            "🗜️ Compacting the context…{elapsed}{tok}\n`{bar}`",
            "🗜️ コンテキストを圧縮中…{elapsed}{tok}\n`{bar}`"
        )
    }
}

/// What's needed to draw the `/usage` answer.
///
/// If `projection` is set, a burn-rate projection line is added
/// (placed right after `Resets …`).
pub struct UsageReport<'a> {
    pub rows: &'a [UsageRow],
    /// `(now, window_minutes)` — the window used for the "Current session" row (normally 300).
    /// The "Current week" row uses `USAGE_WEEK_WINDOW_MINUTES`.
    pub projection: Option<(WallClock, i64)>,
}

impl UsageReport<'_> {
    /// Parsed `/usage` rows as Slack mrkdwn: a bold label + a monospace bar + `<n>% used`,
    /// with `Resets <when>` below. Labels and reset text are **verbatim**.
    pub fn render(&self) -> String {
        match self.projection {
            Some((now, w)) => Self::inner(self.rows, Some(now), w),
            None => Self::inner(self.rows, None, 0),
        }
    }

    fn inner(rows: &[UsageRow], now: Option<WallClock>, window_minutes: i64) -> String {
        let mut lines = vec!["📊 *Claude Code Usage*".to_string(), String::new()];
        for r in rows {
            let pct = r.pct.parse().unwrap_or(0.0);
            lines.push(format!("*{}*", r.label));
            lines.push(format!("`{}` {}% used", Self::bar(pct, 24), r.pct));
            if !r.reset.is_empty() {
                lines.push(format!("Resets {}", r.reset)); // a 0% row prints no reset
            }
            if let Some(hit) = now.and_then(|now| {
                UsageRow::window_minutes(&r.label, window_minutes)
                    .zip(WallClock::parse_reset(&r.reset, &now))
                    .map(|(window, reset)| UsageProjection::of(pct, &reset, &now, window))
                    .filter(|p| p.enough_data && p.at_risk)
                    .and_then(|p| p.projected_hit)
            }) {
                lines.push(format!(
                    "- Expected to reach limit at : {}",
                    hit.reset_like()
                ));
            }
            lines.push(String::new());
        }
        if lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// A 0–100 percentage as a fixed-width unicode bar (a copy of the /usage TUI screen).
    fn bar(pct: f64, width: usize) -> String {
        let p = if pct.is_finite() {
            pct.clamp(0.0, 100.0)
        } else {
            0.0
        };
        let filled = (p / 100.0 * width as f64).round() as usize;
        "█".repeat(filled) + &"░".repeat(width - filled)
    }
}

/// `help`'s sections for the commands in this file: (title, [(trigger, description)]).
/// The values `model` / `effort` / `mode` take come from the agent.
pub(super) fn help_sections(agent: &dyn Agent) -> Vec<(String, Vec<(String, String)>)> {
    let choice = |verb: &str, values: &[&str]| format!("{verb} [{}]", values.join("|"));
    vec![
        (
            crate::t!("In the threads", "スレッドで"),
            vec![
                ("stop".to_string(), crate::t!("stop the current turn (a 🛑 reaction works too)", "実行中のターンを止める(🛑 のリアクションでも可)")),
                ("exit / bye / done".to_string(), crate::t!("end this thread's agent; your next message resumes it", "このスレッドのエージェントを終える。次に書けば再開する")),
                ("compact".to_string(), crate::t!("compact the context, with a progress bar", "コンテキストを圧縮する(進捗バー付き)")),
                (choice("model", agent.models()), crate::t!("show or switch the model", "モデルを見る・切り替える")),
                (choice("effort", agent.effort_levels()), crate::t!("show or set the effort level", "effort を見る・決める")),
                (choice("mode", agent.modes()), crate::t!("show or switch the permission mode", "権限モードを見る・切り替える")),
                ("context / ctx".to_string(), crate::t!("show this agent's context usage", "このエージェントのコンテキストの使用量")),
                ("resume".to_string(), crate::t!("show the command to continue this session in your own terminal (stops the agent here)", "このセッションを手元の端末で続けるコマンドを出す(ここのエージェントは止める)")),
            ],
        ),
        (
            crate::t!("Account", "アカウント"),
            vec![
                ("login".to_string(), crate::t!("sign Claude Code in to your account (in a DM)", "Claude Code を自分のアカウントでサインインする(DM で)")),
                ("logout".to_string(), crate::t!("sign out and stop every agent", "サインアウトして、すべてのエージェントを止める")),
                ("usage / usg".to_string(), crate::t!("show your Claude subscription usage", "Claude のサブスクリプションの使用状況")),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    // The drawing test calls one reader (`renders_context_report`). The parser's own net
    // lives in `mod tests` of `agent/claude.rs`.
    use crate::agent::screen::Pane;

    fn sample_now() -> WallClock {
        WallClock::new(2026, 7, 29, 13, 0).expect("valid wall clock")
    }

    #[test]
    fn resume_report_variants() {
        let agent = crate::agent::fake::FakeAgent::default();
        let none = ResumeInfo {
            session_id: None,
            cwd: None,
            transcript_missing: false,
            worker_running: false,
        };
        assert!(
            none.render(&agent)
                .starts_with("This thread has no session to resume yet")
        );
        let full = ResumeInfo {
            session_id: Some("sid-1".into()),
            cwd: Some("/repo".into()),
            transcript_missing: true,
            worker_running: true,
        };
        let out = full.render(&agent);
        assert!(out.contains("cd /repo && claude --resume sid-1"));
        assert!(out.contains("⚠️ This session's history isn't on disk"));
        assert!(out.contains("The agent for this thread has been stopped"));
    }

    const CONTEXT_RAW: &str = "\
some preamble\n\n**Model:** claude-opus-4-8[1m]\n**Tokens:** 43.8k / 1m (4%)\n\n\
### Estimated usage by category\n\n| Category | Tokens | % |\n| --- | --- | --- |\n\
| System prompt | 3.2k | 0.3% |\n| Messages | 40.6k | 4.1% |\n| Free space | 956k | 95.6% |\n\n\
### Custom Agents\nignored\n";

    /// Net for the drawing side. The **reading side**'s (`Pane::context_report`) net lives in `agent/claude.rs`.
    #[test]
    fn renders_context_report() {
        let r = Pane::new(CONTEXT_RAW).context_report().unwrap();
        let out = r.render();
        assert!(out.starts_with("📊 *Context Usage*\nOpus 4.8（1M context）"));
        assert!(out.contains("43.8k / 1m ( 4% used )"));
        assert!(out.contains("Used space")); // total row
        assert!(!out.contains("Free space")); // the raw Free row is not shown
    }

    #[test]
    fn renders_usage_report() {
        let rows = vec![UsageRow {
            label: "Current week (all models)".into(),
            pct: "66".into(),
            reset: "Aug 1 at 9am".into(),
        }];
        let out = UsageReport {
            rows: &rows,
            projection: None,
        }
        .render();
        assert!(out.starts_with("📊 *Claude Code Usage*"));
        assert!(out.contains("*Current week (all models)*"));
        assert!(out.contains("66% used"));
        assert!(out.contains("Resets Aug 1 at 9am"));
        assert_eq!(
            UsageReport::bar(50.0, 24),
            format!("{}{}", "█".repeat(12), "░".repeat(12))
        );
        // Out of range is clamped; a 0% row prints no Resets line
        assert_eq!(UsageReport::bar(150.0, 10), "█".repeat(10));
        assert_eq!(UsageReport::bar(-5.0, 10), "░".repeat(10));
        let zero = vec![UsageRow {
            label: "Current session".into(),
            pct: "0".into(),
            reset: String::new(),
        }];
        assert!(
            !UsageReport {
                rows: &zero,
                projection: None
            }
            .render()
            .contains("Resets")
        );
    }

    #[test]
    fn usage_report_appends_projection_line_only_when_at_risk() {
        let rows = vec![UsageRow {
            label: "Current session".into(),
            pct: "40".into(),
            reset: "5:30pm".into(),
        }];
        let out = UsageReport {
            rows: &rows,
            projection: Some((sample_now(), 300)),
        }
        .render();
        assert!(out.contains("Expected to reach limit at"));
        // Verbatim `at :` (space + colon + space) + the fmtResetLike form
        assert!(out.contains("- Expected to reach limit at : Jul 29 at 1:45 pm"));

        // Nothing is added to rows that can't be projected or aren't at risk
        let calm = vec![UsageRow {
            label: "Current session".into(),
            pct: "6".into(),
            reset: "5:30pm".into(),
        }];
        assert!(
            !UsageReport {
                rows: &calm,
                projection: Some((sample_now(), 300))
            }
            .render()
            .contains("Expected to reach limit")
        );
        let unknown = vec![UsageRow {
            label: "Current month".into(),
            pct: "40".into(),
            reset: "5:30pm".into(),
        }];
        assert!(
            !UsageReport {
                rows: &unknown,
                projection: Some((sample_now(), 300))
            }
            .render()
            .contains("Expected to reach limit")
        );
        // The week row is judged over a 7-day window (using the session window misjudges it)
        assert_eq!(
            UsageRow::window_minutes("Current week (Fable)", 300),
            Some(UsageProjection::WEEK_MINUTES)
        );
        assert_eq!(UsageRow::window_minutes("Current session", 300), Some(300));
        assert_eq!(UsageRow::window_minutes("Current month", 300), None);
    }

    #[test]
    fn compact_progress_without_an_arrow_renders_no_nul() {
        // A driver can build a state holding only tokens (all fields are pub). The arrow must fall back to an empty string;
        // mixing in `Option<char>::unwrap_or_default()`'s `'\0'` would leak an invisible NUL to Slack
        let p = CompactProgress {
            active: true,
            seconds: Some(3),
            tokens: Some("876".into()),
            tokens_dir: None,
            percent: None,
        };
        let out = p.render();
        assert!(!out.contains('\0'));
        assert!(out.starts_with("🗜️ Compacting the context… 3s · 876 tokens\n"));
        // If there is an arrow it goes right before the number (no space in between)
        let with_dir = CompactProgress {
            tokens_dir: Some('↑'),
            ..p
        };
        assert!(
            with_dir
                .render()
                .starts_with("🗜️ Compacting the context… 3s · ↑876 tokens\n")
        );
    }
}
