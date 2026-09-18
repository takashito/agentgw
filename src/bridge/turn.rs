//! One turn of an agent's work as seen from Slack: hooks from the agent, the start and
//! failure of a turn, the usage-limit watch, the silence watch, tool permission prompts,
//! and writing the progress message.

use super::{Bridge, Host};
use crate::agent::tmux::Window;
use crate::agent::{HookEvent, ProbeErr, SessionId};
use crate::bridge::state as bridge;
use crate::bridge::state::{Disposition, LogCtx, ThreadKey, WallClock};
use crate::chat::slack;

const NARRATION_CAP: usize = 600;

/// How many times one message may be re-sent after turn failures (`TURN_FAILURE_RETRY_CAP`).
/// **Once is deliberate** — failures a re-send fixes (congestion, an API blip, a one-off without a type) clear on the next try.
/// The same failure twice is real, so tell a person rather than show a loop.
pub(super) const TURN_FAILURE_RETRY_CAP: u32 = 1;

/// Grace period before the usage-limit watch starts (don't hit start-up with a probe).
const USAGE_MONITOR_STARTUP_DELAY_MS: u64 = 60_000;

/// How long to wait for a person. Wrapped up inside the hook's declared timeout (125s) and the Bridge's wait (120s).
const PERM_WAIT_MS: u64 = 115_000;

/// One tool permission waiting for a person's click.
///
/// **The agent's hook stays open holding this `respond`** — it doesn't return until clicked or expired.
pub(super) struct PermPending {
    /// The way back to the hook. The answer goes here when clicked / given up.
    respond: tokio::sync::oneshot::Sender<serde_json::Value>,
    pub(super) channel: String,
    pub(super) thread_ts: String,
    tool_name: String,
    /// Key for turning **that tool's line** into 🚫 on Deny.
    tool_use_id: String,
    /// ts of the posted prompt. Kept only to **delete** it on expiry.
    pub(super) prompt_ts: String,
    /// Expiry (epoch ms). Cut a little inside the hook's own wait.
    deadline_ms: u64,
}

/// The silence watch for one thread.
///
/// The key is that it **holds the thread's whole status sender**, so
/// delivery / hooks / watch firing / settle all go through the one serial task of `thinking` = none
/// overtakes another. Back when each was its own `tokio::spawn`, "delivery's `is typing…`" and
/// "the watch's clear" went out in any order and cancelled each other.
///
/// Dropping the entry makes `slack::Thinking`'s Drop send a final clear — **it queues at the tail**,
/// so it never overtakes the set sent just before it.
pub(super) struct Stall {
    /// When the last activity happened (epoch ms).
    pub(super) last_activity_ms: u64,
    /// Whether the watch is showing `is thinking…` (`Entry.stalled`).
    pub(super) shown: bool,
    /// A permission prompt is up and the agent is silent **on purpose**.
    /// Stops the watch (the "Permission requested" prompt itself shows that it's waiting).
    pub(super) awaiting_perm: bool,
    pub(super) thinking: slack::Thinking,
}

impl Bridge {
    /// No milestone until the thread can be looked up.
    pub(super) fn milestone(&mut self, key: Option<&ThreadKey>, event: &str, ctx: &LogCtx) {
        let Some(key) = key else {
            ctx.debug("bridge", &format!("{event} before its thread is known"));
            return;
        };
        let m = self.lifecycle.record(key, event, self.deps.clock.now_ms());
        ctx.info("lifecycle", &m.message(key, event));
    }

    pub(super) async fn on_hook(&mut self, mut ev: HookEvent) {
        // Hooks only carry the session ID — look the thread back up from threads.json
        let owning = self
            .threads
            .find_by_session(&ev.session_id)
            .map(|(ts, _)| ts.clone());
        let key = self.key_of_session(&ev.session_id);
        let ctx = LogCtx {
            session_id: Some(ev.session_id.clone()),
            thread_key: key.clone(),
        };
        // A hook arrived = the agent is alive and working. Re-arm the silence watch (turn start
        // and progress re-arm it — all hooks are a superset of those, and each is "evidence of activity")
        if let Some(k) = key.clone() {
            self.touch_thread(&k, ""); // activity = lift the stall display. Unlike delivery, there's nothing new to show
        }
        // Every hook carries the transcript location — the first one sets up the receipt-check tail
        if let Some(path) = ev.payload["transcript_path"].as_str() {
            let h = self.workers.warm_mut(&ev.session_id);
            if h.transcript_path.as_deref() != Some(path) {
                h.transcript_path = Some(path.to_string());
                h.transcript_offset = 0; // a different file (resume etc.) means read from the start
            }
        }
        match ev.kind.as_str() {
            "session_start" => {
                // **Don't touch the latch.** This signal fires not only on start-up but also on compact / clear,
                // so going back to "starting" here jams deliveries to a running agent in the queue
                // (2026-08-01 on a real machine. Details in `Workers::starting`)
                self.workers.warm_mut(&ev.session_id).ended = false;
                self.milestone(key.as_ref(), "session_start", &ctx);
            }
            "session_end" => {
                self.workers.warm_mut(&ev.session_id).ended = true;
                // A pooled agent died = it's no longer in the pool (clearForSession).
                // Left in, it stays ready:true, gets taken by the next new thread, and the delivery is lost
                self.drop_pool_worker(&ev.session_id, "ended", &ctx);
                self.milestone(key.as_ref(), "session_end", &ctx);
            }
            // A tool call worked = it holds MCP (an inherited agent never gets another chance to
            // show initialize — mcp.rs's call_tool sends the only evidence). No milestone
            "mcp_ready" => self.workers.warm_mut(&ev.session_id).mcp_ready = true,
            // Holding MCP = the disposition tools can be called (the only evidence that forcing Stop is fine)
            "mcp_initialized" => {
                // It's also the only moment a pooled agent becomes "usable" (`Workers::pool_ready` reads this)
                self.workers.warm_mut(&ev.session_id).mcp_ready = true;
                self.milestone(key.as_ref(), "mcp_initialized", &ctx);
            }
            "user_prompt" => {
                // The first turn went through = the TUI takes keys. Release the latch and flush the queue
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
                // If blocked, the turn **continues** (re-prompt), so it's not over yet
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

    /// A turn failed (`StopFailure`). Classify the reason and tell the person waiting in the thread
    /// in plain words. Failing silently looks to a person like
    /// "the bot ignored me".
    ///
    /// **Log unconditionally, before the thread is looked up** — a turn failure is never a silent path.
    /// Keep the raw payload too: `error_type` has arrived **empty** on a real machine, and without
    /// the record we can't tell "Claude sent no type" from "we dropped it".
    ///
    /// Keep the order: log → thread gate → **limit gate** → **re-delivery** → wording.
    fn on_turn_failure(&mut self, ev: &HookEvent, key: Option<&ThreadKey>, ctx: &LogCtx) {
        let sent = ev.payload["error_type"].as_str().unwrap_or("");
        // An empty type means asking the agent's own record. On a real machine (2026-09-18) an expired sign-in
        // arrived empty, was read as retry, and told people to "send it again" — re-sending doesn't help
        let recorded = if sent.is_empty() && !ev.session_id.is_empty() {
            let remembered = self
                .workers
                .warm(&ev.session_id)
                .and_then(|h| h.transcript_path.clone());
            self.deps.agent.failure_type(remembered.as_deref(), &ev.session_id)
        } else {
            None
        };
        let reason = recorded.unwrap_or(sent);
        let (klass, mut text) = TurnFailureClass::of(reason);
        // An expired sign-in is about **this machine**. Say where to send it — this thread's messages reach
        // this machine, so `login` typed here signs this machine back in
        if reason.to_ascii_lowercase().contains("authentication_failed") {
            let machine = &self.machine_name;
            text = crate::t!(
                "Claude Code on *{machine}* is signed out, so no reply was written. Send `login` here to sign in again.",
                "*{machine}* の Claude Code のサインインが切れていて、返信を書けませんでした。ここで `login` と送ると、サインインし直せます。"
            );
        }
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
                match (sent.is_empty(), recorded) {
                    (false, _) => sent.to_string(),
                    (true, Some(r)) => format!("{r} (none sent; read from the agent's record)"),
                    (true, None) => "(none sent)".to_string(),
                },
            ),
        );
        // Stop here if the thread can't be looked up. No copy goes to the home channel:
        // the failure belongs to one thread, and the person waiting there is told below
        let Some((channel, Some(thread_ts))) = key.map(ThreadKey::split) else {
            return;
        };
        let key = key.expect("thread split above implies a key");
        // Before anything else: "is this the limit?". `error_type` can't answer (it arrived empty on a real machine,
        // and `rate_limit` is also used for congestion), so ask the agent's own history. Re-sending into the limit
        // is **the one answer that never works**, and it replaces the one sentence the person needs with silence
        if !ev.session_id.is_empty() && self.turn_hit_usage_limit(&ev.session_id, key, ctx) {
            self.settle_told(key);
            return;
        }
        // Retry-class failures clear with "deliver once more". If that delivery works, post nothing —
        // what the person should see is how the re-sent turn ends
        if klass == TurnFailureClass::Retry
            && self.retry_turn_failure(key, reason, ctx)
        {
            return;
        }
        // A retry-class message the worker couldn't take yet still belongs to the recovery paths
        if klass != TurnFailureClass::Retry {
            self.settle_told(key);
        }
        self.post_error_frame(channel, thread_ts, text);
    }

    /// The person was told why this turn produced no reply — that is the thread's answer.
    /// Settle it: otherwise the unanswered messages keep the silence watchdog armed and
    /// "is thinking…" stays up on a turn that is already over.
    fn settle_told(&mut self, key: &ThreadKey) {
        // Dropping the entry clears the status (slack::Thinking's Drop)
        self.stall.remove(key);
        self.ledger.dispose_all(key);
    }

    /// Did this turn die from "hit the usage limit" (`turnHitUsageLimit`)? Asked **before** re-sending —
    /// re-sending is the one thing that never works (the wall doesn't move until the reset).
    ///
    /// The measurement that led to this (2026-07-17): StopFailure arrived one second after hitting the limit.
    /// `error_type` was **empty**, so it was read as `retry` and quietly re-sent to an agent stuck
    /// on a modal. Claude Code had **already written** the answer (with the reset time) into that
    /// agent's history. Nobody read it, and the silence lasted 90 minutes.
    ///
    /// true = raised the gate and told the thread (the caller must not re-send). The gate matters as
    /// much as the wording: while it's up, the **next** message gets the reset time at the door.
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
                // An unreadable or missing history is no proof of "not the limit", but there's nowhere
                // else to ask. Fall back to handling it as an ordinary turn failure
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
            let text = limited_notice(hit.reset_ms);
            self.post(&channel, &ts, text, key);
        }
        true
    }

    /// A turn failed and the message it couldn't answer is still unanswered — before telling the person
    /// "gave up", **deliver it to the same agent once more** (`retryTurnFailure`).
    ///
    /// Why here and not a timer: the failure is **already known** (Claude Code reports the moment the
    /// turn dies). Until now the only path that re-delivered unanswered messages was the agent's **death**,
    /// so a turn that failed under a live agent stayed in the ledger with nobody answering it.
    ///
    /// Re-send only when the agent can **really hear** (Ready). Otherwise the receipt timer and the
    /// recovery path already own that message, and pushing it in would just be ignored a second time.
    /// Returns true = delivered (the caller posts nothing).
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
        if state != crate::agent::WorkerState::Ready {
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
            self.settle_told(key);
            return false;
        }
        // Prefer window_id for the target (window names can be renamed) — same lookup as Dispatch::Deliver
        let target = sid
            .as_deref()
            .and_then(|s| self.workers.warm(s))
            .and_then(|h| h.window_id.clone())
            .unwrap_or(window);
        for (id, envelope) in &undisposed {
            if let Err(e) = self.deps.agent.deliver(&Window::of(&target), envelope) {
                ctx.error(
                    "bridge",
                    &format!("turn-failure re-send failed for {id}: {e} — telling the user"),
                );
                return false;
            }
            // Same message_id → the ledger has one entry per message, so idempotent. received resets and the
            // receipt check re-arms = this is a **new delivery**, so that's right
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

    /// Posts one ⚠️ for the `error` frame. Goes through the same **vanished-thread-root gate** as the perm prompt
    /// (posting with `thread_ts` to a vanished root makes Slack drop it at the channel top level).
    /// The caller doesn't wait — a probe can take seconds, and the main loop must not hang.
    pub(super) fn post_error_frame(&self, channel: String, thread_ts: String, text: String) {
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

    /// The usage-limit watch. **Without it the limit gate never fires on its own**
    /// (the only way to notice would be the turn-failure wording).
    ///
    /// Interval is 60 minutes normally, 15 minutes when the limit is expected. It only runs `/usage` once on Home,
    /// needing no thread or session (same path as `user_usage`).
    ///
    /// It does two things:
    /// - **raise the gate if any window has hit its limit**. All windows are checked, and the latest
    ///   reset time wins (the weekly wall applies even when the session is low)
    /// - on newly crossing 80% / 90%, warn each live thread once
    pub(super) async fn usage_tick(&mut self) {
        let now = self.deps.clock.now_ms();
        let due = self.usage_polled_at_ms
            + if self.usage_at_risk {
                crate::bridge::command::UsageWatch::POLL_AT_RISK_MS
            } else {
                crate::bridge::command::UsageWatch::POLL_MS
            };
        // Wait a bit right after start-up (don't hit start-up with a probe)
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

        // Usage went down = the window rolled over. Reset the warning latch
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
        // Projection (how soon the limit will be hit). It also sets the interval
        let w = crate::bridge::state::WallClock::now();
        let projection = crate::bridge::state::WallClock::parse_reset(&reset_text, &w)
            .map(|reset| UsageProjection::of(pct, &reset, &w, 300));
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

        // The limit gate — if any window has hit it, block until the latest reset
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
        let text = usage_warning_notice(
            pct as u32,
            &reset_text,
            projection
                .filter(|p| p.enough_data && p.at_risk)
                .and_then(|p| p.projected_hit),
        );
        // Once each, only to threads holding a live agent
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

    /// Keys of threads holding a live agent. Where limit warnings go (`liveThreads`).
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

    /// One pass of the silence watch (from the 500ms tick; one setTimeout per thread before).
    /// The decision is [`bridge::stall_action`] — raise / fold / nothing.
    ///
    /// Folded are keys whose unanswered list is empty. `on_disposition` dropping them at once is the main path, but the ledger
    /// also empties on `terminate` (exit / logout / resume) — **every path that emptied the ledger** is caught here in one place,
    /// so the shimmer doesn't linger without each caller adding its own cleanup.
    ///
    /// ponytail: once raised it isn't re-sent (idempotent like `showStall`). If Slack expires it,
    /// the shimmer just quietly goes away, which is less harmful than one that lingers. To refresh it, fire `set`
    /// every tick like compact does
    pub(super) fn stall_tick(&mut self) {
        self.save_ledger();
        let now = self.deps.clock.now_ms();
        let acts: Vec<(ThreadKey, bridge::StallAction)> = self
            .stall
            .iter()
            .map(|(key, s)| {
                // Waiting for permission isn't silence — don't fire. But let the settle (Settle) through
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
                // Just drop it — slack::Thinking's Drop sends the clear from the tail of the queue
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

    /// Writes the ledger to pending.json if it changed. Called from the tick and right before shutting down, nowhere else —
    /// saves aren't sprinkled over the code that touches the ledger, so new paths can't forget to write.
    pub(super) fn save_ledger(&mut self) {
        if let Some(Err(e)) = self.ledger.flush(&mut self.threads) {
            LogCtx::default().error("bridge", &format!("pending.json save failed: {e}"));
        }
    }

    /// The moment the agent took in the input. This is **proof of receipt** — switch delivered ones from 👀 to 🤖,
    /// fold the previous round's progress message and start a new one.
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

    /// Backstop for receipt checks. **Messages pushed in mid-turn don't fire UserPromptSubmit**
    /// (they're consumed as steering), so the envelope's message_id appearing in the agent's
    /// transcript counts as proof of receipt.
    ///
    /// ponytail: a naive 500ms polling tail. Move to fs notifications or JSON line parsing
    /// if it ever needs them
    pub(super) fn scan_transcripts(&mut self) {
        let tails = self.workers.transcripts();
        for (sid, path, offset) in tails {
            let Some(key) = self.key_of_session(&sid) else {
                continue;
            };
            let unreceived = self.ledger.unreceived(&key);
            if unreceived.is_empty() {
                // While there's nothing to look for, skip the output unread (the envelope is written after delivery =
                // after track, so nothing is missed). Leaving it would make the first mid-turn delivery
                // read a whole turn synchronously in one go and stall the main loop
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
            // Receipt starts a new round. Handle it like the UserPromptSubmit path —
            // without folding here, the old progress message **above** the follow-up keeps getting
            // narration and tool lines appended (it looks like "the reply grows back into the past")
            if taken_in {
                self.sticky.on_turn_start(&key);
            }
        }
    }

    /// The answer to the PermissionRequest hook. The Bridge is **not the sole authority** on this:
    /// it answers right away only what standing rules decide, and declines the rest, leaving it to Claude Code's own
    /// permission path (`{}` = stay out of it).
    ///
    /// **Standing rules going first** is the key: without them the agent asks a person for permission to use
    /// its own reply tool, and stalls with nobody clicking (a known pitfall).
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
                // Rules don't decide it — ask a person. The answer comes later, so nothing is returned here
                crate::bridge::command::ToolPermission::Ask => return None,
            },
        )
    }

    /// Answer on the spot if standing rules decide it; otherwise ask on Slack and hold on to `respond`.
    /// If the thread can't be looked up or posting fails, decline (`{}`) and fall back to Claude Code's own path —
    /// better to leave options than to deny silently.
    async fn on_perm(&mut self, ev: &mut HookEvent, key: Option<&ThreadKey>, ctx: &LogCtx) {
        let Some(respond) = ev.respond.take() else {
            return; // not waiting for an answer = do nothing
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
        // Don't ask for a scope the person already approved. Check the narrower one (thread) first
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
        // Posting with thread_ts to a vanished thread root makes Slack drop it as a **top-level
        // channel message** = channel clutter. Check the root is alive before posting, and if it's gone,
        // deny without showing a prompt (DMs aren't checked: their roots don't vanish that way).
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
        // reqId is a tag that only means something inside the Bridge. Time + pid + counter is unique enough
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
        // From here the agent is silent waiting for a person. Stop the watch
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

    /// A permission prompt went up = the agent goes silent **on purpose**. Stop the watch and
    /// clear any `is thinking…` already shown (the prompt itself shows that it's waiting).
    fn suspend_stall_for_perm(&mut self, key: &ThreadKey) {
        let Some(e) = self.stall.get_mut(key) else {
            return; // no watch for this round yet (first turn) — nothing to stop
        };
        e.awaiting_perm = true;
        if std::mem::replace(&mut e.shown, false) {
            e.thinking.set("");
        }
    }

    /// A permission was settled. `rearm` = a person clicked (restart measuring silence).
    /// **false** on expiry — having just said "timed out", don't re-arm the watch
    /// (real activity will re-arm it via touch_thread).
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

    /// An approval button was clicked. **Answer the agent the moment it's clicked** — then redraw the prompt
    /// to show the result (the agent doesn't wait even if rewriting the post is slow).
    pub(super) async fn on_perm_click(&mut self, click: slack::PermClick) {
        let Some(p) = self.perm_pending.remove(&click.req_id) else {
            // Already clicked / folded on expiry. A second click does nothing
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
            // Turn the denied tool's line into 🚫. A **separate path** from the expiry note line
            self.sticky.on_perm_denied(&key, &p.tool_use_id);
        }
        // Remember "don't ask again in this scope". **Only written when a person clicks**
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

    /// Expired with nobody clicking. The agent already gave up and moved on, so **delete the leftover prompt** —
    /// left in place, a later Allow click would "work but do nothing".
    pub(super) async fn expire_perm_prompts(&mut self) {
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
            // Don't re-arm — having just said "timed out", stay quiet until real activity comes
            self.resume_stall_after_perm(&key, false);
            self.sticky.on_perm_timeout(&key);
            if let Err(e) = self.deps.slack.delete_message(&p.channel, &p.prompt_ts).await {
                ctx.debug("bridge", &format!("perm prompt delete failed: {e}"));
            }
        }
    }

    /// Writes "don't ask again in this thread / channel" to access.json.
    /// **Only called when a person clicks a button** — a session has no way to write it itself.
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

    /// PreToolUse / PostToolUse → a tool line in the progress message. Which tools **don't** get a line is an invariant of the board.
    fn on_progress(&mut self, key: Option<&ThreadKey>, p: &serde_json::Value) {
        let Some(key) = key else { return };
        let name = p["tool_name"].as_str().unwrap_or_default();
        // A progress with no tool name is an activity ping "before the tool is known".
        // **No line for it** (the caller already re-armed the watch regardless of the hook kind).
        // A line would have an empty name and can't be folded, so it **splits a run of consecutive Read/search lines**
        // (the position gate).
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
        // Without an id every tool overwrites the same line — use name + arguments instead
        let id = match p["tool_use_id"].as_str().unwrap_or_default() {
            "" => format!("{name}:{summary}"),
            id => id.to_string(),
        };
        // Where the subagent came from. The top-level agent_id/agent_type are only on
        // **tools run inside a subagent** (not in the main session).
        // The spawner's id is in tool_response: camelCase for foreground,
        // snake_case for background/teammate. Accept both.
        let agent = slack::sticky::AgentRef {
            agent_id: p["agent_id"].as_str().map(str::to_string),
            agent_type: p["agent_type"].as_str().map(str::to_string),
            // Picked up **only from Agent/Task PostToolUse**. A same-named field in another
            // tool's result is ignored. There are three shapes, and
            // background/teammate has content null, so the last-resort scan doesn't work for them
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
            // The launch name (`input.name`) for tying background/teammate by name
            launched_name: p["tool_input"]["name"].as_str().map(str::to_string),
        };
        self.sticky
            .upsert_tool(key, &id, name, &summary, status, &agent, &p["tool_input"]);
    }

    /// `agentId: <id>` written plainly in the result text (last resort. `/agentId:\s*(\w+)/`).
    fn agent_id_in_text(result_text: &str) -> Option<String> {
        let rest = result_text.split_once("agentId:")?.1.trim_start();
        let id: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        (!id.is_empty()).then_some(id)
    }

    /// Joins MessageDisplay deltas per message_id and settles them into one line on `final`.
    fn on_narration(&mut self, key: Option<&ThreadKey>, p: &serde_json::Value, ctx: &LogCtx) {
        let Some(key) = key else { return };
        let msg_id = p["message_id"].as_str().unwrap_or_default();
        // message_id can arrive empty — without a per-thread namespace every thread shares one
        // buffer and they cross (NUL never appears in Slack ids, so it works as a separator)
        let slot = format!("{key}\u{0}{msg_id}");
        let buf = self.narration.entry(slot.clone()).or_default();
        // Left alone the buffer grows forever — stop appending once it exceeds one line's budget
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

    /// May the turn end? Unanswered, the agent hangs up to 5 seconds, so always return a value.
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

    /// A tool "answered" — drop it from the ledger and settle the progress message.
    pub(super) async fn on_disposition(&mut self, d: Disposition) {
        // reply / no_reply / edit may not carry thread_ts — look the root back up from the session
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
        // An answer came = the shimmer's job is done (any of reply / no_reply / edit_message).
        // Just drop it — slack::Thinking's Drop sends the clear from the tail of the queue, so
        // it always clears after, never overtaking, the `is thinking…` the watch just sent
        self.stall.remove(&key);
        let before = self.ledger.pending(&key).len();
        if d.message_ids.is_empty() {
            self.ledger.dispose_all(&key); // no ids = the whole thread is done
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
        // Reply and edit show the final picture and stay as a record. Silence and reactions remove the whole progress message
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

    pub(super) async fn flush_stickies(&mut self) {
        for (key, posted, body) in self.sticky.take_dirty(self.deps.clock.now_ms()) {
            self.flush_sticky(&key, posted, body).await;
        }
    }

    /// Pushes one progress message to Slack. Only post waits, since it remembers the ts (update is fire-and-forget).
    async fn flush_sticky(&mut self, key: &ThreadKey, posted: Option<String>, body: String) {
        if body.is_empty() {
            return; // Slack rejects an update with empty text
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

/// A burn-rate projection built from one `/usage` reading.
///
/// When `enough_data` is false, neither warn nor lock.
/// `at_risk` = expected to reach 100% **before** the window resets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageProjection {
    pub enough_data: bool,
    pub at_risk: bool,
    pub projected_hit: Option<WallClock>,
}

impl UsageProjection {
    /// The weekly window (7 days).
    pub const WEEK_MINUTES: i64 = 7 * 24 * 60;

    /// Projection noise guard: a window with less time or use than this is "not enough data".
    const MIN_ELAPSED_MINUTES: i64 = 30;
    const MIN_PCT: f64 = 5.0;

    /// The answer when there's too little to go on. **Neither warns nor locks.**
    const NONE: UsageProjection = UsageProjection {
        enough_data: false,
        at_risk: false,
        projected_hit: None,
    };

    /// Window start = reset − window, burn = pct / elapsed minutes, limit hit = now + remaining% / burn.
    /// A reset in the past (= `minutes_to` is None) means a broken reading, so err on the safe side.
    pub fn of(
        pct: f64,
        reset: &WallClock,
        now: &WallClock,
        window_minutes: i64,
    ) -> UsageProjection {
        let Some(to_reset) = WallClock::minutes_to(now, reset) else {
            return Self::NONE;
        };
        let elapsed = window_minutes - to_reset;
        if elapsed < Self::MIN_ELAPSED_MINUTES || pct < Self::MIN_PCT {
            return Self::NONE;
        }
        let burn = pct / elapsed as f64;
        if !burn.is_finite() || burn <= 0.0 {
            return Self::NONE;
        }
        let to_limit = (100.0 - pct).max(0.0) / burn;
        let hit = now.plus_minutes(to_limit.round() as i64);
        UsageProjection {
            enough_data: true,
            at_risk: WallClock::minutes_to(&hit, reset).is_some_and(|m| m > 0),
            projected_hit: Some(hit),
        }
    }
}

// ── Turn failure classification — class and wording are **one table** ─────
// The class (retry / tell-user) and the wording are two sides of one decision. Split into two tables, only one gets
// fixed and they drift apart, so they're merged into one here. **The wording is copied verbatim** (compatibility promise).

/// How a turn failure is handled. `Retry` is the "deliver the unanswered message again" class.
///
/// **Re-delivery itself doesn't exist yet** (separate commit), so for now both show wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnFailureClass {
    Retry,
    TellUser,
}

impl TurnFailureClass {
    /// Table that looks up the `error` frame's `error_type` by **substring, case-insensitive**.
    /// (word contained in `error_type`, handling, English, Japanese)
    const TABLE: &'static [(&'static str, TurnFailureClass, &'static str, &'static str)] = &[
        (
            "rate_limit",
            TurnFailureClass::Retry,
            "Claude is busy right now, so no reply was written. Wait a moment and send it again.",
            "Claude が混み合っていて、返信を書けませんでした。少し待ってからもう一度送ってください。",
        ),
        (
            "overloaded",
            TurnFailureClass::Retry,
            "Claude had a temporary problem, so no reply was written. Wait a moment and send it again.",
            "Claude 側の一時的な不具合で、返信を書けませんでした。少し待ってからもう一度送ってください。",
        ),
        (
            "server_error",
            TurnFailureClass::Retry,
            "Claude had a temporary problem, so no reply was written. Wait a moment and send it again.",
            "Claude 側の一時的な不具合で、返信を書けませんでした。少し待ってからもう一度送ってください。",
        ),
        (
            "authentication_failed",
            TurnFailureClass::TellUser,
            "Claude Code is signed out, so no reply was written. Send `login` here to sign in again.",
            "Claude Code のサインインが切れていて、返信を書けませんでした。ここで `login` と送ると、サインインし直せます。",
        ),
        (
            "oauth_org_not_allowed",
            TurnFailureClass::TellUser,
            "Your organization doesn't allow this account to use Claude Code, so no reply was written. Ask your admin.",
            "組織の設定で、このアカウントは Claude Code を使えません。返信を書けなかったので、管理者に確認してください。",
        ),
        (
            "billing_error",
            TurnFailureClass::TellUser,
            "There's a billing problem with your Claude account, so no reply was written. Check your billing settings.",
            "Claude のアカウントの支払いに問題があり、返信を書けませんでした。支払いの設定を確認してください。",
        ),
        (
            "max_output_tokens",
            TurnFailureClass::TellUser,
            "The answer hit the output limit before it was finished. Ask for a narrower part.",
            "答えが長すぎて、出力の上限を超えました。範囲を絞って聞き直してください。",
        ),
        (
            "invalid_request",
            TurnFailureClass::TellUser,
            "The request was too large (or invalid). Run `compact` to shrink the conversation, or send a shorter message.",
            "依頼が大きすぎるか、正しくありません。`compact` で会話を圧縮するか、短くして送り直してください。",
        ),
        (
            "model_not_found",
            TurnFailureClass::TellUser,
            "The selected model doesn't exist, so no reply was written. Check the model with `model`.",
            "選んだモデルが見つからず、返信を書けませんでした。`model` でモデルを確認してください。",
        ),
    ];

    /// Classifies a turn failure and **at the same time** puts it in words a person can act on (`turnFailure`).
    ///
    /// The 10th, `unknown`, and any unknown or **empty type** all fall to `Retry` — empty really
    /// happens (seen on a real machine on 2026-07-14; the next turn succeeded with the same credentials). Nothing is swallowed:
    /// the raw keyword is the only clue, so it stays in the wording.
    pub fn of(reason: &str) -> (TurnFailureClass, String) {
        let r = reason.to_lowercase();
        if let Some((_, klass, en, ja)) = Self::TABLE.iter().find(|(needle, ..)| r.contains(needle)) {
            let text = match crate::i18n::lang() {
                crate::i18n::Lang::En => en,
                crate::i18n::Lang::Ja => ja,
            };
            return (*klass, (*text).to_string());
        }
        (
            TurnFailureClass::Retry,
            if reason.is_empty() {
                crate::t!(
                    "No reply was written. Send it again.",
                    "返信を書けませんでした。もう一度送ってください。"
                )
            } else {
                crate::t!(
                    "No reply was written (`{reason}`). Send it again.",
                    "返信を書けませんでした(`{reason}`)。もう一度送ってください。"
                )
            },
        )
    }
}

impl std::fmt::Display for TurnFailureClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Retry => "retry",
            Self::TellUser => "tell-user",
        })
    }
}

/// The one reply to a request that arrives while the usage limit is hit. Host = Asia/Tokyo is an assumption of
/// this repo's whole usage feature — it just adds a fixed +9:00.
pub fn limited_notice(limited_until_ms: u64) -> String {
    let at = WallClock::tokyo(limited_until_ms).format("%-m/%-d %H:%M");
    crate::t!(
        "⏸️ You've reached your Claude Code usage limit. New requests are paused until it resets around {at} (Asia/Tokyo) — send yours again after that.",
        "⏸️ Claude Code の利用上限に達しました。{at}(Asia/Tokyo)ごろのリセットまで新しい依頼は受け付けません。リセット後にもう一度送ってください。"
    )
}

/// The limit is near (crossed 80% / 90%). Sent once to each live thread.
/// `reset` is the reset clause `/usage` printed (shown as is); `projected_hit` is when the limit
/// is expected to be hit at this pace.
fn usage_warning_notice(pct: u32, reset: &str, projected_hit: Option<WallClock>) -> String {
    let mut lines = vec![crate::t!(
        "⚠️ You're close to your usage limit — current session *{pct}% used*",
        "⚠️ 利用上限が近づいています — 今のセッションで *{pct}%* 使用"
    )];
    if !reset.is_empty() {
        lines.push(crate::t!("Resets {reset}", "リセット: {reset}"));
    }
    if let Some(hit) = projected_hit {
        let at = hit.reset_like();
        lines.push(crate::t!(
            "At this pace you'll hit the limit around {at}.",
            "このペースだと {at} ごろに上限に達します。"
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One wall-clock time. The date isn't what the tests are about, so make it writable in one line.
    fn wc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> WallClock {
        WallClock::new(year, month, day, hour, minute).expect("valid wall clock")
    }

    /// Classification is **substring, case-insensitive**. Anything not in the table and empty both fall to the retry class,
    /// and the raw keyword stays in the wording (it's the only clue, so don't hide it).
    #[test]
    fn turn_failure_classifies_and_speaks() {
        use TurnFailureClass::*;
        let (k, t) = TurnFailureClass::of("API Error: rate_limit_error");
        assert_eq!(k, Retry);
        assert!(t.contains("Claude is busy"), "{t}");

        let (k, t) = TurnFailureClass::of("BILLING_ERROR");
        assert_eq!(k, TellUser);
        assert!(t.contains("billing problem"), "{t}");

        let (k, t) = TurnFailureClass::of("unknown");
        assert_eq!(k, Retry);
        assert_eq!(t, "No reply was written (`unknown`). Send it again.");

        // "Type arrived empty", as happened on a real machine — don't swallow it; say it with keyword-less wording
        let (k, t) = TurnFailureClass::of("");
        assert_eq!(k, Retry);
        assert_eq!(t, "No reply was written. Send it again.");
    }

    #[test]
    fn format_limit_reply_names_the_reset_time() {
        // Measured values (checked with Python zoneinfo): epoch ms → Asia/Tokyo wall clock
        let out = limited_notice(1_782_635_400_000); // 2026-06-28 17:30 JST
        assert!(out.contains("6/28 17:30"), "{out}");
        assert_eq!(
            out,
            "⏸️ You've reached your Claude Code usage limit. New requests are paused until it resets around 6/28 17:30 (Asia/Tokyo) — send yours again after that."
        );
        // An example where minutes are zero-padded
        let out = limited_notice(1_785_283_500_000); // 2026-07-29 09:05 JST
        assert!(out.contains("7/29 09:05"), "{out}");
    }

    fn sample_now() -> WallClock {
        wc(2026, 7, 29, 13, 0)
    }

    #[test]
    fn project_usage_not_enough_data_below_threshold() {
        let now = wc(2026, 7, 29, 10, 0);
        let reset = wc(2026, 7, 29, 15, 0);
        let p = UsageProjection::of(3.0, &reset, &now, 300); // pct<5% → enough_data=false
        assert!(!p.enough_data);
        // Also not taken when the whole window hasn't elapsed (reset exactly one window ahead)
        assert!(!UsageProjection::of(40.0, &reset, &now, 300).enough_data);
    }

    #[test]
    fn project_usage_at_risk_when_burn_rate_outpaces_reset() {
        let now = sample_now();
        let reset = wc(2026, 7, 29, 17, 30);
        // Elapsed 300-270=30 min with 40% used → burn=1.333%/min → remaining 60% ÷ 1.333 = hit in 45 min
        // Runs out sooner than the 270 min until reset → at_risk
        let p = UsageProjection::of(40.0, &reset, &now, 300);
        assert!(p.enough_data);
        assert!(p.at_risk);
        assert_eq!(p.projected_hit, Some(wc(2026, 7, 29, 13, 45)));
        // The weekly window (10080 min) has a long elapsed time and a gentle burn, so the same % may not run out
        let week_reset = wc(2026, 8, 3, 9, 0);
        let w = UsageProjection::of(20.0, &week_reset, &now, UsageProjection::WEEK_MINUTES);
        assert!(w.enough_data);
        assert!(!w.at_risk);
    }
}
