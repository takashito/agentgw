//! What arrives from Slack, and the gate it passes through: the envelope handed to the agent,
//! edits / deletions / reactions, duplicate suppression, and the owner gate.
//! The message types themselves ([`InboundMsg`] and friends) live in `chat.rs`.
//!
//! `impl Access` here holds only the gate (`gate`, per-channel tool grants); `Access`
//! itself is persisted state and lives in `state.rs`.

use super::{Bridge, Host};
use crate::bridge::turn::Stall;
use crate::agent::Window;
use crate::agent::{Envelope, SessionId, SpawnReq, WorkerState};
use crate::bridge::state as bridge;
use crate::bridge::state::{Access, ThreadEntry};
use crate::log::LogCtx;
use crate::chat::ThreadKey;
use crate::bridge::{inbound, worker};
use crate::chat::{ChannelKind, InboundMsg, Reaction, slack};

/// Deadline after which the agent is killed even if draining hasn't finished. Reuses the
/// receipt timeout (30 s) rather than adding another knob.
const DRAIN_TIMEOUT_MS: u64 = 30_000;

/// Retry attempt count Slack stamps on a redelivery. Fixed at 0: slack-morphism 2.24.0's
/// Socket Mode envelope only keeps `envelope_id` and `accepts_response_payload`, and the
/// `retry_attempt` Slack sends is dropped before the push-event callback sees it. Re-arrivals
/// of the same (channel, ts) are dropped by dedup, so all the stale check has left is
/// "was it posted before this Bridge started listening".
const RETRY_NUM: u32 = 0;

/// This message → the envelope text handed to the agent. `ts` is now **at delivery** (`now_ms`),
/// `thread_ts` is the resolved thread root (the thread's ts, or the message's own ts).
/// How long a delivery may run before the watchdog gives up on it. The real one takes at most 1.2 s;
/// this is twenty-five times that, so it only ever fires on something genuinely wedged.
const DELIVERY_STUCK_MS: u64 = 30_000;

/// How long a window that refused a delivery is left alone. Long enough that a person reads one notice
/// instead of a column of them, short enough that a passing modal costs one wait.
const DELIVERY_RETRY_WAIT_MS: u64 = 30_000;

/// How long a delivered body may go unproven before the person is told. **Generous on purpose**: a body
/// taken as mid-turn steering is only recorded when the turn reaches it, which is as long as the tool
/// call it lands in. Short enough and a long build would produce a warning about a message that arrives
/// fine.
const RECEIPT_GRACE_MS: u64 = 180_000;

pub fn envelope(msg: &InboundMsg, root_ts: &str, now_ms: u64) -> String {
    envelope_guarded(msg, root_ts, None, now_ms)
}

/// Only deliveries that tripped the loop guard carry `loop_guard` (empty = nobody to call).
pub fn envelope_guarded(
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
        ts: crate::clock::iso8601(now_ms),
        thread_ts: Some(root_ts.to_string()),
        text: msg.text.clone(),
        // The message carries the result of the eager download (it survives the queue too)
        file_paths: msg.file_paths.clone(),
        file_errors: msg.file_errors.clone(),
    }
    .render()
}

/// The gate's answer: whether this message goes to the agent.
///
/// The `&'static str` in `Drop` is the reason, logged as is (nothing is dropped silently).
/// The decision itself is [`Access::gate`].
#[derive(PartialEq, Eq, Debug)]
pub enum GateVerdict {
    Serve,
    /// Someone other than the Owner spoke in an active thread. Hand it to the agent **as
    /// context** (no reply expected). Dropping it would leave gaps in the thread's conversation.
    Context,
    Drop(&'static str),
}

/// What to do with a message that passed the gate. Only four options: start / resume / deliver / queue.
///
/// Decided only by the thread record and whether the agent is alive ([`Dispatch::decide`]).
#[derive(PartialEq, Eq, Debug)]
pub enum Dispatch {
    SpawnNew,
    SpawnResume(String),
    Deliver,
    Queue,
}

/// No entry → SpawnNew / Ready → Deliver / Starting → Queue / Absent → SpawnResume.
impl Dispatch {
    pub fn decide(entry: Option<&ThreadEntry>, worker: WorkerState) -> Dispatch {
        let Some(entry) = entry else {
            return Dispatch::SpawnNew;
        };
        match worker {
            WorkerState::Ready => Dispatch::Deliver,
            WorkerState::Starting => Dispatch::Queue,
            // An entry with no past session can't be resumed → start a new one
            WorkerState::Absent => match entry.agent_id.as_deref() {
                Some(sid) => Dispatch::SpawnResume(sid.to_string()),
                None => Dispatch::SpawnNew,
            },
        }
    }
}

const DEDUP_CAP: usize = 512;

/// Memory of seen (channel, ts) to drop redeliveries. Required: one message arrives as several events
///
/// ponytail: linear scan capped at 512. Switch to HashSet + VecDeque if the event rate grows.
#[derive(Default)]
pub struct RecentDeliveries {
    seen: std::collections::VecDeque<(String, String)>,
}

impl RecentDeliveries {
    pub fn new() -> Self {
        Self::default()
    }

    /// True if already seen. Otherwise remember it and return false.
    pub fn seen(&mut self, channel: &str, ts: &str) -> bool {
        if self.seen.iter().any(|(c, t)| c == channel && t == ts) {
            return true;
        }
        if self.seen.len() >= DEDUP_CAP {
            self.seen.pop_front();
        }
        self.seen.push_back((channel.to_string(), ts.to_string()));
        false
    }
}

impl Access {
    // ── Gate ──
    // When in doubt, Drop (fail-closed). An access with no owner lets nobody through.
    /// Whether this tool was granted "don't ask again" in this channel.
    pub fn channel_tool_allowed(&self, channel: &str, tool: &str) -> bool {
        self.routes
            .get(channel)
            .and_then(|r| r.allowed_tools.as_ref())
            .is_some_and(|v| v.iter().any(|t| t == tool))
    }

    /// Remember "don't ask again in this channel". Called **only when a person presses the button**.
    pub fn grant_channel_tool(&mut self, channel: &str, tool: &str) {
        if channel.is_empty() || tool.is_empty() {
            return;
        }
        let allowed = self
            .routes
            .entry(channel.to_string())
            .or_default()
            .allowed_tools
            .get_or_insert_with(Vec::new);
        if !allowed.iter().any(|t| t == tool) {
            allowed.push(tool.to_string());
        }
    }

    /// Three outcomes: serve / serve as context / drop.
    ///
    /// **Whether the channel is in access.json is not checked.** A route only says which folder
    /// to work in for that channel and plays no part in letting a message in: the Owner's
    /// mention is answered even in an unregistered channel.
    ///
    /// In a channel it takes a **mention** or an **already active thread**. Without that, every
    /// bit of chatter in a registered channel would flow to the agent.
    pub fn gate(&self, msg: &InboundMsg, is_mention: bool, is_active_thread: bool) -> GateVerdict {
        let dm = msg.channel_kind == ChannelKind::Dm;
        // There is no way for a bot to get in through a DM
        if msg.is_bot && dm {
            return GateVerdict::Drop("bot-dm-blocked");
        }
        if self.owner.is_empty() {
            return GateVerdict::Drop(if dm { "dm-no-owner" } else { "no-owner" });
        }
        // Posts made through the Slack Web API carry a bot_id **even when a person wrote them**.
        // Slack still stamps the real `user` (it comes from the token, so the text can't spoof it),
        // so if that person is the Owner, treat it as a person. Anyone else, or no user, is a bot (fail closed)
        let is_owner = msg.user.as_deref() == Some(self.owner.as_str());
        if dm {
            return if is_owner {
                GateVerdict::Serve
            } else {
                GateVerdict::Drop("dm-not-owner")
            };
        }
        let reachable = is_mention || is_active_thread;
        if msg.is_bot && !is_owner {
            // Only bots the Owner allowed with allow-bot. Others don't get in even when mentioned
            let allowed = msg
                .bot_id
                .as_deref()
                .is_some_and(|id| self.allowed_bots.iter().any(|b| b == id));
            if !allowed {
                return GateVerdict::Drop("drop-bot-not-allowed");
            }
            return if reachable {
                GateVerdict::Serve
            } else {
                GateVerdict::Drop("require-mention-unmet")
            };
        }
        if is_owner {
            return if reachable {
                GateVerdict::Serve
            } else {
                GateVerdict::Drop("require-mention-unmet")
            };
        }
        // Someone other than the Owner: pass **as context** only inside an active thread (no reply)
        if is_active_thread {
            GateVerdict::Context
        } else {
            GateVerdict::Drop("drop-not-owner")
        }
    }
}

/// Key that keeps the same event from being handled twice.
///
/// A bare `ts` is enough only **when a message body arrives**. For reactions, edits and
/// deletions `ts` points at **the target message**, so as is it collides with the key stored
/// for the original delivery and is always dropped as a duplicate. Each kind gets its own namespace.
pub(super) fn dedup_key(msg: &InboundMsg) -> String {
    match (&msg.reaction, &msg.edited, &msg.deleted_ts) {
        // A second emoji on the same post is not a duplicate: the key includes the emoji and add/remove
        (Some(r), _, _) => format!("{}#{}{}", msg.ts, r.emoji, if r.added { "+" } else { "-" }),
        // An edit passes once per revision (redeliveries of the same revision are dropped)
        (_, Some(e), _) => format!("{}#edit#{}", msg.ts, e.revision),
        (_, _, Some(_)) => format!("{}#deleted", msg.ts),
        _ => msg.ts.clone(),
    }
}

/// Our own marks (the ack 👀 / 🤖 / the receipt 🔄) come back to us from Slack.
///
/// **Never pass them to the agent.** The synthesized reaction text urges "reply rather than
/// stay silent", so an obedient agent adds a reply for every ack. Measured (2026-08-02): three
/// extra messages on **the first message of every thread**. Only the first, because the
/// reaction event doesn't say which thread it's in, and the ID used as a stand-in matched the
/// real thread only then.
pub(super) fn is_own_reaction(msg: &InboundMsg, bot_user_id: Option<&str>) -> bool {
    msg.reaction.is_some() && bot_user_id.is_some_and(|b| msg.user.as_deref() == Some(b))
}

/// Where a reaction on a post **we did not write** goes.
#[derive(PartialEq, Debug)]
pub(super) enum ForeignReaction {
    /// The Owner put stop on **their own request**: stop (the user's call, 2026-08-02).
    Stop,
    /// Anything else. Dropped quietly, so reactions to other people's exchanges don't flow
    /// into the agent as synthesized text
    Drop,
}

pub(super) fn foreign_reaction(
    r: &Reaction,
    reactor: Option<&str>,
    author: Option<&str>,
    owner: &str,
) -> ForeignReaction {
    let by_owner = !owner.is_empty() && reactor == Some(owner);
    // `author == reactor` = put on their own post. **stop on someone else's post does nothing**:
    // there's no telling whose work to stop
    if r.is_stop() && by_owner && author == reactor {
        ForeignReaction::Stop
    } else {
        ForeignReaction::Drop
    }
}

// ── handling inbound messages ──

impl Bridge {
    /// Download the incoming message's attachments up front and write the result back to msg.
    /// **Never fails**: each file's failure is swallowed into a degraded note, so one broken or
    /// oversized file doesn't take the user's text or other attachments down with it.
    async fn with_attachments(&self, mut msg: InboundMsg, ctx: &LogCtx) -> InboundMsg {
        let (mut paths, mut errors) = (Vec::new(), Vec::new());
        // This runs inside an arm of the select loop: while it waits, hooks / dispositions /
        // commands / progress-message flushes for **every thread** wait too. So the deadline is
        // **one for the whole message**, not per file. However many files, it always returns
        // within DOWNLOAD_TIMEOUT.
        // ponytail: sequential. Parallelize (tokio::spawn) once many-attachment waits actually hurt
        let deadline = tokio::time::Instant::now() + slack::DOWNLOAD_TIMEOUT;
        for f in &msg.files {
            let dl = self.deps.slack.download_attachment(&f.id, self.deps.dir.path());
            let reason = match tokio::time::timeout_at(deadline, dl).await {
                Ok(Ok(path)) => {
                    paths.push(path);
                    continue;
                }
                Ok(Err(e)) => e,
                // Deadline passed. The remaining files hit this branch at once and become notes
                Err(_) => format!(
                    "timed out ({}s budget for this message's attachments)",
                    slack::DOWNLOAD_TIMEOUT.as_secs()
                ),
            };
            ctx.debug(
                "bridge",
                &format!("attachment download failed file={}: {reason}", f.id),
            );
            // Keep this wording: it is what the agent relays to the person
            errors.push(format!("[attachment {} not downloaded: {reason}]", f.name));
        }
        ctx.debug("bridge", &format!(
                "slack-events: message accepted message_id={} channel={} type={} files={} dl={} err={}",
                msg.ts,
                msg.channel,
                match msg.channel_kind {
                    inbound::ChannelKind::Dm => "im",
                    inbound::ChannelKind::Channel => "channel",
                },
                msg.files.len(),
                paths.len(),
                errors.len()
            ));
        msg.file_paths = paths;
        msg.file_errors = errors;
        msg
    }

    pub(super) async fn on_inbound(&mut self, msg: &InboundMsg) {
        // **A person renamed the thread.** Their name wins from here on: note it and stop, the
        // agent is told nothing (it is not a message, and there is nothing for it to do)
        if let Some(title) = &msg.session_title {
            self.lock_thread_title(msg, title);
            return;
        }
        // Our own marks come back to us. Drop them **before dedup** so our emoji don't fill the memory
        if is_own_reaction(msg, self.bot_user_id.as_deref()) {
            LogCtx::default().debug(
                "bridge",
                &format!(
                    "own reaction on {}:{} — dropped",
                    msg.channel,
                    msg.reaction
                        .as_ref()
                        .map(|r| r.emoji.as_str())
                        .unwrap_or("")
                ),
            );
            return;
        }
        // dedup before the gate: one message arrives as several events
        let dedup_key = dedup_key(msg);
        if self.dedup.seen(&msg.channel, &dedup_key) {
            LogCtx::default().debug(
                "bridge",
                &format!("duplicate event {}:{} — dropped", msg.channel, msg.ts),
            );
            return;
        }
        // A command is a **button**: it works when pressed or not at all.
        // Slack delivers posts made while the Bridge was down in a batch on reconnect, so an `exit`
        // the sender gave up on long ago arrives and says goodbye a second time. Drop it before
        // any command path. Ordinary requests are untouched: late delivery beats losing real work
        let body = crate::bridge::command::Message::new(&msg.text, self.bot_user_id.as_deref());
        if let Some(cmd) = crate::bridge::command::Cmd::parse(&body, self.deps.agent.as_ref())
            && let Some(stale) = cmd.stale_reason(&msg.ts, self.started_at_ms, RETRY_NUM)
        {
            LogCtx {
                session_id: None,
                thread_key: msg
                    .thread_ts
                    .as_deref()
                    .map(|t| ThreadKey::new(&msg.channel, t)),
            }
            .info(
                "bridge",
                &format!(
                    "slack-events: dropped: command arrived too late to honor ({stale}) — \
                     msg={} channel={} sender={} text={}",
                    msg.ts,
                    msg.channel,
                    msg.user.as_deref().unwrap_or(""),
                    serde_json::to_string(&msg.text.chars().take(40).collect::<String>())
                        .unwrap_or_default()
                ),
            );
            return;
        }
        // Sign-in carve-out. The gate drops everything while there is no owner, so unless this
        // comes **before** it, the first sign-in could never start. Even with an Owner, let it
        // through while a sign-in in this channel is waiting for a code; otherwise the code for a
        // machine's re-sign-in (which starts with an Owner set) flows to the agent (found on a
        // real machine 2026-09-18)
        if (self.access.owner.is_empty() || self.sign_in.awaiting_code(&msg.channel))
            && !msg.is_bot
            && self.login_carve_out(msg)
        {
            return;
        }
        // Edit and delete events **don't say which thread** (the library's type drops the nested
        // `thread_ts`). Look it up in the pending ledger first, and if it's not there ask Slack
        // once. **For an edit to an old, already answered message this is the only way**: it's
        // gone from the ledger, so without asking it would be dropped as a different thread.
        // Reactions are the same and **tell us neither the author nor the thread**, so fetch the
        // original post once. Skipping that and using ts as the thread was the cause of the
        // "misdelivery only on a thread's first message" (2026-08-02).
        // ponytail: one lookup per reaction. If that's too many, lift `item_user` into
        // InboundMsg and drop non-stop reactions on other people's posts before the lookup
        let reacted = match &msg.reaction {
            Some(r) => match self.deps.slack.message_at(&msg.channel, &r.item_ts).await {
                Some(m) => Some(m),
                None => {
                    LogCtx::default().debug(
                        "bridge",
                        &format!(
                            "reaction dropped: could not read {}:{}",
                            msg.channel, r.item_ts
                        ),
                    );
                    return;
                }
            },
            None => None,
        };
        let touched = msg
            .deleted_ts
            .as_deref()
            .or(msg.edited.as_ref().map(|e| e.ts.as_str()));
        let root_ts = match (&reacted, touched) {
            (Some(m), _) => m.thread_ts.clone(),
            (None, None) => msg.thread_ts.clone().unwrap_or_else(|| msg.ts.clone()),
            (None, Some(id)) => match self.ledger.key_of_id(id).and_then(|k| k.split().1) {
                Some(root) => root,
                None => self
                    .deps.slack
                    .parent_thread_of(&msg.channel, id)
                    .await
                    .or_else(|| msg.thread_ts.clone())
                    .unwrap_or_else(|| id.to_string()),
            },
        };
        let key = ThreadKey::new(&msg.channel, &root_ts);
        let ctx = |sid: Option<&str>| LogCtx {
            session_id: sid.map(str::to_string),
            thread_key: Some(key.clone()),
        };

        // In a channel it takes **a mention or an already active thread** to get in
        // (a DM counts as a mention). Registration is not checked; these two are all that matter
        let dm = msg.channel_kind == inbound::ChannelKind::Dm;
        let is_mention = dm
            || crate::bridge::command::Message::new(&msg.text, self.bot_user_id.as_deref())
                .mentions_bot();
        let is_active_thread = !dm && self.threads.is_active(&root_ts);
        let context_only = match self.access.gate(msg, is_mention, is_active_thread) {
            GateVerdict::Drop(reason) => {
                ctx(None).info(
                    "bridge",
                    &format!("dropped {}:{} — {reason}", msg.channel, msg.ts),
                );
                return;
            }
            // Someone other than the Owner spoke in an active thread: pass it on, no reply expected
            GateVerdict::Context => {
                ctx(None).info(
                    "bridge",
                    &format!(
                        "slack-events: channel deliver: context (non-owner, silent-expected) — \
                         msg={} channel={} sender={}",
                        msg.ts,
                        msg.channel,
                        msg.user.as_deref().unwrap_or("")
                    ),
                );
                true
            }
            GateVerdict::Serve => false,
        };
        let _ = context_only;

        // The user deleted their own request. Report the cancellation **only if it's still in
        // progress**: deleting an old, already answered message leaves no work to withdraw
        if let Some(deleted) = msg.deleted_ts.clone() {
            self.on_message_deleted(msg, &key, &root_ts, &deleted, &ctx(None)).await;
            return;
        }

        // The user edited a request. If it's **the one in progress**, interrupt and redo it with
        // the new text; if it's an old one, keep the current work going and say "when you're free"
        if let Some(edited) = msg.edited.clone() {
            self.on_message_edited(msg, &key, &root_ts, &edited.ts, &ctx(None));
            return;
        }

        // **A stop the Owner puts on their own request means stop** (the user's call, 2026-08-02;
        // not only on the progress message). No need to find the progress message: putting ✋
        // on the request itself stops it.
        //
        // Before that, a check: **only take reactions on posts we wrote**. Taking reactions on
        // other people's posts would pour unrelated exchanges into the agent as synthesized text
        if let (Some(r), Some(m)) = (&msg.reaction, &reacted)
            && !(m.is_bot || m.user.as_deref() == self.bot_user_id.as_deref())
        {
            if foreign_reaction(
                r,
                msg.user.as_deref(),
                m.user.as_deref(),
                &self.access.owner,
            ) == ForeignReaction::Stop
            {
                ctx(None).info(
                    "bridge",
                    &format!(
                        "slack-events: stop reaction :{}: on the requester's own message {}:{} \
                         (thread {root_ts})",
                        r.emoji, msg.channel, r.item_ts
                    ),
                );
                self.user_stop(msg, &key, &root_ts, &ctx(None));
                return;
            }
            ctx(None).debug(
                "bridge",
                &format!(
                    "reaction dropped: {}:{} is not our message",
                    msg.channel, r.item_ts
                ),
            );
            return;
        }

        // A stop emoji **on the progress message** is the same as the stop command. The same emoji
        // on another message falls through as a plain reaction (stop only when aimed at the progress message)
        if let Some(r) = &msg.reaction
            && r.is_stop()
            && self.sticky.sticky_ts(&key).as_deref() == Some(r.item_ts.as_str())
        {
            ctx(None).info(
                "bridge",
                &format!(
                    "slack-events: stop reaction :{}: on progress sticky {}:{} (thread {root_ts})",
                    r.emoji, msg.channel, r.item_ts
                ),
            );
            self.user_stop(msg, &key, &root_ts, &ctx(None));
            return;
        }

        // Loop guard (channels only). Once bots are allowed, bots can reply to each other forever.
        // This stops the thread **until a person steps in**
        let mut loop_guard: Option<String> = None;
        if !dm {
            if msg.is_bot {
                if self.threads.status_of(&root_ts) == "paused" {
                    ctx(None).info(
                        "bridge",
                        &format!(
                            "slack-events: dropped: thread paused (loop guard) — msg={} \
                             (awaiting human re-engagement)",
                            msg.ts
                        ),
                    );
                    return;
                }
                let streak = self.threads.bump_bot_streak(&root_ts);
                if streak >= bridge::LOOP_LIMIT {
                    self.threads.pause(&root_ts);
                    ctx(None).info(
                        "bridge",
                        &format!(
                            "slack-events: loop-guard: thread paused after {streak} consecutive \
                             bot msgs (≥ LOOP_LIMIT) — msg={}",
                            msg.ts
                        ),
                    );
                    // Empty if there is nobody to call (just set the flag)
                    loop_guard = Some(match self.access.owner.as_str() {
                        "" => String::new(),
                        o => format!("<@{o}>"),
                    });
                }
                if let Err(e) = self.threads.save() {
                    ctx(None).error("bridge", &format!("threads.json save failed: {e}"));
                }
            } else if self.threads.reset_bot_streak(&root_ts) {
                ctx(None).info(
                    "bridge",
                    "slack-events: thread resumed — human re-engaged (was paused)",
                );
                if let Err(e) = self.threads.save() {
                    ctx(None).error("bridge", &format!("threads.json save failed: {e}"));
                }
            }
        }

        // The Bridge answers commands itself and **stops here**; they never reach the agent.
        // Returning before the ack keeps 👀 off commands
        if self.handle_command(msg, &key, &root_ts).await {
            // The Bridge **spoke in this thread**. Even for commands that don't start an agent,
            // like status / usage, the thread is active from now on and follow-ups are taken
            // without a mention (the user's call; independent of whether a session exists)
            self.mark_thread_seen(&msg.channel, &root_ts, &ctx(None));
            return;
        }

        // While at the usage limit, take no new requests and don't queue them either.
        // This comes **after** commands so stop/restart/logout still work at the limit
        if self.deps.clock.now_ms() < self.limited_until_ms {
            let text = super::turn::limited_notice(self.limited_until_ms);
            self.post(&msg.channel, &root_ts, text, &key);
            return;
        }

        // Signal "seen" right away and record it as pending in the ledger (the switch to 🤖 is the user_prompt hook)
        self.react(
            &msg.channel,
            &msg.ts,
            self.access.ack_emoji(),
            &key,
        )
        .await;
        self.ledger.track(&key, &msg.ts);
        // Where `status` should link to from now on: the newest message, not the thread's first one
        self.remember_last_message(&root_ts, msg);

        // Attachments are downloaded **on receipt** and the envelope carries local paths. The
        // agent just Reads them and never needs download_attachment for the triggering message
        // (it isn't even told the file_id)
        let msg = &self.with_attachments(msg.clone(), &ctx(None)).await;

        let entry = self.threads.get(&root_ts).cloned();
        let is_new = entry.is_none();
        if is_new {
            // Name the session on the thread's first message, **whichever way it gets an agent**.
            // A warm pool worker takes most threads (`pool: assigned` → `Dispatch::Deliver`), so
            // naming from the cold-spawn branch missed nearly all of them
            let (api, channel, ts) =
                (self.deps.slack.clone(), msg.channel.clone(), root_ts.clone());
            let topic: String = msg.text.trim().chars().take(60).collect();
            tokio::spawn(async move { api.name_session(&channel, &ts, &topic).await });
        }
        let sid = entry.as_ref().and_then(|e| e.agent_id.as_deref());
        // The window name depends only on session_id. A thread with no session yet has no
        // window; worker_state returns Absent without a sid, so an empty string is harmless
        let window = sid
            .map(|s| SessionId::from(s).window_name())
            .unwrap_or_default();
        let state = self.workers.state_of(entry.as_ref(), &window, self.deps.agent.as_ref());
        // Prefer window_id as the target (window names can be renamed)
        let target = entry
            .as_ref()
            .and_then(|e| e.agent_id.as_deref())
            .and_then(|sid| self.workers.warm(sid))
            .and_then(|h| h.window_id.clone())
            .unwrap_or_else(|| window.clone());
        let envelope = envelope_guarded(msg, &root_ts, loop_guard, self.deps.clock.now_ms());
        // Hold on to it: the hand-over reads it back, so the guard is not lost on the way
        self.pending_envelopes.insert(msg.ts.clone(), envelope.clone());
        // Needed to resend after a failed turn. `track` runs right after the ack (before the
        // envelope exists), so store it in the ledger **here**, once built; every delivery path below comes after this
        self.ledger.remember_envelope(&key, &msg.ts, &envelope);
        // Resolve the same way as pwd (no route → fall back to Home)
        let (cwd, home_fallback) = self.access.repo_path(&msg.channel, &Host::home());
        // About to start an agent in a registered folder that has gone away: tmux would quietly start it
        // in the home directory instead, working somewhere nobody asked for. Say so and stop
        if !home_fallback
            && state == WorkerState::Absent
            && !self.deps.agent.workdir_exists(&cwd)
        {
            ctx(None).error("bridge", &format!("project folder {cwd} is missing — not starting an agent"));
            self.settle_told(&key);
            let machine = &self.machine_name;
            self.post_error_frame(
                msg.channel.clone(),
                root_ts.clone(),
                crate::t!(
                    "This channel's project folder `{cwd}` doesn't exist on *{machine}*, so no agent was started. Set it again with `pwd <path>`.",
                    "このチャンネルのプロジェクトのフォルダ `{cwd}` が *{machine}* にありません。エージェントは起動していません。`pwd <パス>` で設定し直してください。"
                ),
            );
            return;
        }

        // A new thread checks the pool before a cold spawn. If it's empty, fall through to decide below
        if bridge::Threads::should_claim_pool(entry.as_ref()) {
            let key_pool = bridge::PoolKey::of_cwd(&cwd);
            if let Some(claimed) = self.claim_pool_worker(&key_pool) {
                // Same topic as SpawnNew (trimmed, first 60 characters, counted in chars)
                let topic: String = msg.text.trim().chars().take(60).collect();
                // **Queued before it is handed over**, like every other delivery: the queue is what says
                // "not confirmed yet", and the report clears it
                self.pending
                    .entry(root_ts.clone())
                    .or_default()
                    .push(msg.clone());
                let assigned = self.assign_pool_worker(
                    claimed,
                    &root_ts,
                    &msg.channel,
                    &cwd,
                    &topic,
                    &msg.ts,
                    &key,
                    &ctx(None),
                );
                // Assignment itself can fail before any delivery starts (threads.json, the pool). The
                // message is already queued, so say so and leave it for the next tick
                if let Err(e) = assigned {
                    self.post_error_frame(
                        msg.channel.clone(),
                        root_ts.clone(),
                        crate::t!("Couldn't hand this to the agent: {e}", "エージェントに渡せませんでした: {e}"),
                    );
                }
                return;
            }
        }

        // Once delivered, start the silence watch. `&mut self` can't be taken while `entry` is
        // borrowed, so carry the flag out of the match
        // **Handed off, not yet confirmed.** The shimmer means "received and working", and that is true the
        // moment the delivery task takes it; a failure a second later posts its own error frame
        let mut handed_off = false;
        match Dispatch::decide(entry.as_ref(), state) {
            Dispatch::SpawnNew => {
                let sid = SessionId::new().as_str().to_string();
                let mut e = entry.unwrap_or_default();
                e.agent_id = Some(sid.clone());
                e.channel_id = Some(msg.channel.clone());
                e.repo_path = Some(cwd.clone());
                // The topic used as the link text in status: trimmed, first 60 characters
                // (counted in chars so multi-byte text isn't split)
                let topic: String = msg.text.trim().chars().take(60).collect();
                e.topic = (!topic.is_empty()).then_some(topic.clone());
                // The record is created here, after `remember_last_message` ran and found nothing
                e.last_ts = Some(msg.ts.clone());
                e.last_text = (!topic.is_empty()).then_some(topic.clone());
                self.threads.upsert(&root_ts, e);
                if let Err(err) = self.threads.save() {
                    ctx(Some(&sid)).error("bridge", &format!("threads.json save failed: {err}"));
                }
                let Some(mcp) = self.write_mcp(&sid, &ctx(Some(&sid))) else {
                    return;
                };
                let req = SpawnReq {
                    session_id: sid.clone().into(),
                    cwd: cwd.clone(),
                    prompt: Some(envelope.clone()),
                    resume_from: None,
                    // The window name comes from the session_id **just chosen** (the thread has no window name yet)
                    window: SessionId::from(sid.clone()).window_name(),
                    state,
                    hooks_file: self.hooks_file.clone(),
                    mcp_config: mcp,
                };
                self.spawn_worker(&req, &key, &ctx(Some(&sid)), &sid);
            }
            Dispatch::SpawnResume(sid) => {
                let Some(mcp) = self.write_mcp(&sid, &ctx(Some(&sid))) else {
                    return;
                };
                let req = SpawnReq {
                    session_id: sid.clone().into(),
                    cwd: cwd.clone(),
                    prompt: Some(envelope.clone()),
                    // To continue, reopen the same session_id with `--resume`
                    resume_from: Some(sid.clone().into()),
                    window: SessionId::from(sid.clone()).window_name(),
                    state,
                    hooks_file: self.hooks_file.clone(),
                    mcp_config: mcp,
                };
                self.spawn_worker(&req, &key, &ctx(Some(&sid)), &sid);
            }
            Dispatch::Deliver => {
                // **Queue first, hand over second.** The queue is the record of what has not been
                // confirmed; a delivery that never reports leaves the message there for the next tick
                self.pending
                    .entry(root_ts.clone())
                    .or_default()
                    .push(msg.clone());
                self.start_delivery(&target, &key, &root_ts, &ctx(None));
                ctx(entry.as_ref().and_then(|e| e.agent_id.as_deref())).info(
                    "bridge",
                    &format!(
                        "slack-events: hand over message_id={} chat={} thread={root_ts} new={is_new}",
                        msg.ts, msg.channel
                    ),
                );
                handed_off = true;
            }
            Dispatch::Queue => {
                self.pending
                    .entry(root_ts.clone())
                    .or_default()
                    .push(msg.clone());
                ctx(None).info("bridge", "worker still starting — queued");
            }
        }
        // Handed over = now waiting for a reply. Show the shimmer for "received and working";
        // if silence continues, the watch replaces it with "thinking".
        // It goes out in the same single send as re-arming the watch, so it doesn't fight a shimmer already shown.
        // A reaction is a light signal: 👀 alone marks activity, no shimmer
        // (the user's call).
        // **Nor for messages addressed to someone else**: an active thread also carries things like
        // `<@someone> hey`, which aren't for us. Showing it would falsely claim a reply is being written
        let for_someone_else = !is_mention
            && crate::bridge::command::Message::new(&msg.text, self.bot_user_id.as_deref())
                .mentions_someone_else();
        if handed_off && msg.reaction.is_none() && !for_someone_else {
            self.touch_thread(&key, true);
        }
    }

    /// Hand an edited request to the agent. There is one fork:
    /// **was the request in progress edited?** (= it's pending and the newest in the thread).
    ///
    /// - In progress → add "Interrupted by user." to the progress message (only if one is shown),
    ///   push the new text, then ESC. Work done for the old wording is thrown away
    /// - An old request → **don't interrupt**. Just say "do the edited request when you're free"
    ///   (the same as deleting an old message doesn't touch the current work)
    ///
    /// A thread with no agent is dropped best-effort (nothing to interrupt or deliver to).
    fn on_message_edited(
        &mut self,
        msg: &InboundMsg,
        key: &ThreadKey,
        root_ts: &str,
        edited_ts: &str,
        ctx: &LogCtx,
    ) {
        let Some(sid) = self.threads.get(root_ts).and_then(|e| e.agent_id.clone()) else {
            ctx.info(
                "bridge",
                &format!(
                    "message_changed chan={} ts={edited_ts} -> no active worker thread, dropped \
                     (best-effort)",
                    msg.channel
                ),
            );
            return;
        };
        // "In progress" = pending and the newest pending in the thread
        // (Slack ids are seconds.microseconds, so comparing numbers compares recency)
        let pending = self.ledger.pending(key);
        let num = |id: &str| id.parse::<f64>().unwrap_or(0.0);
        let is_current = pending.iter().any(|id| id == edited_ts)
            && pending.iter().all(|id| num(id) <= num(edited_ts));

        let mut notice = msg.clone();
        notice.text = crate::chat::edit_notice(
            edited_ts,
            &msg.text,
            !msg.files.is_empty(),
            is_current,
        );
        let target = self
            .workers
            .warm(&sid)
            .and_then(|h| h.window_id.clone())
            .unwrap_or_else(|| SessionId::from(sid.clone()).window_name());
        // Add the interrupted closing line only when a progress message is shown (not if nothing is out yet)
        if is_current && self.sticky.sticky_ts(key).is_some() {
            self.sticky.on_interrupted(key);
        }
        ctx.info(
            "bridge",
            &format!(
                "message_changed chan={} ts={edited_ts} thread={root_ts} -> {}",
                msg.channel,
                if is_current {
                    "CURRENT work edited: interrupt + redo with new text"
                } else {
                    "past message revised: notify only (current work untouched)"
                }
            ),
        );
        let envelope = envelope(&notice, root_ts, self.deps.clock.now_ms());
        // **Off the loop like every other delivery.** The edit stands whether or not the notice lands, so
        // the interrupt below no longer waits for it; a failure reports itself
        self.hand_notice(&target, key, &envelope);
        // Push first, then cut (same order as deletion). The other way round, the interrupted
        // agent waits for the next input without knowing about the edit
        if is_current {
            self.user_stop(msg, key, root_ts, ctx);
        }
        // The edited request is **pending again**. Re-add it after user_stop (added before, the
        // dispose would sweep it up). Without this the silence watch sees it as settled and folds
        // at once, and no shimmer or "thinking" shows while the agent redoes the work.
        // Replace the envelope too: the stored one has the text from **before** the edit, and a
        // resend after a failed turn would send the old wording again
        self.ledger.track(key, edited_ts);
        self.ledger.remember_envelope(key, edited_ts, &envelope);
        self.touch_thread(key, true);
    }

    /// Before exiting, **save messages not yet handed over to threads.json**.
    /// Without this, requests queued while an agent was starting vanish the moment we go down.
    ///
    /// Only the queue is saved (pending entries in the ledger were **already handed over** and the agent holds them).
    pub(super) fn flush_pending_to_disk(&mut self, ctx: &LogCtx) {
        let queued: Vec<(String, Vec<InboundMsg>)> = self.pending.drain().collect();
        let mut n = 0usize;
        for (root_ts, msgs) in queued {
            for msg in &msgs {
                self.threads.enqueue_pending(&root_ts, &msg.channel, msg);
                n += 1;
            }
            if !msgs.is_empty() {
                ctx.info(
                    "bridge",
                    &format!(
                        "re-enqueued {} undelivered message(s) to the durable queue thread={root_ts}",
                        msgs.len()
                    ),
                );
            }
        }
        if n > 0
            && let Err(e) = self.threads.save()
        {
            ctx.error("bridge", &format!("threads.json save failed: {e}"));
        }
    }

    /// Redeliver what was saved (once at startup). It goes **the same way as a normal incoming
    /// message**: pushed if the agent is alive, otherwise the agent is started (= the interrupted thread resumes).
    pub(super) async fn resume_pending_from_disk(&mut self) {
        for root_ts in self.threads.threads_with_pending() {
            let msgs = self.threads.drain_pending(&root_ts);
            if msgs.is_empty() {
                continue;
            }
            LogCtx {
                session_id: None,
                thread_key: None,
            }
            .info(
                "bridge",
                &format!(
                    "resuming {} message(s) left undelivered by the previous bridge process \
                     thread={root_ts}",
                    msgs.len()
                ),
            );
            for msg in msgs {
                self.on_inbound(&msg).await;
            }
        }
        if let Err(e) = self.threads.save() {
            LogCtx::default().error("bridge", &format!("threads.json save failed: {e}"));
        }
    }

    /// Make the agent withdraw a deleted request.
    ///
    /// Order: **push the cancellation notice first**, then cut the running turn with ESC. The
    /// other way round, the interrupted agent waits for the next input without knowing about
    /// the cancellation.
    ///
    /// Finally drop it from the ledger: the person who deleted it isn't waiting for a reply,
    /// and leaving it would keep the "awaiting reply" watch around for the whole cancellation (a fixed hole).
    async fn on_message_deleted(
        &mut self,
        msg: &InboundMsg,
        key: &ThreadKey,
        root_ts: &str,
        deleted: &str,
        ctx: &LogCtx,
    ) {
        // **The thread's own root: the conversation is gone, not just one request.** Slack takes the
        // replies with it, so there is nowhere left to post and nobody left waiting. Left alone, the
        // agent holds one of the seats until the idle rules reach it up to an hour later.
        if deleted == root_ts {
            let sid = self.threads.get(root_ts).and_then(|e| e.agent_id.clone());
            ctx.info(
                "bridge",
                &format!(
                    "message_deleted chan={} ts={deleted} -> thread root gone, ending session={}",
                    msg.channel,
                    sid.as_deref().unwrap_or("none")
                ),
            );
            // **The thread itself goes first**, unlike every other teardown: `terminate` keeps the
            // entry so the next message can `--resume`, and here there will never be a next message.
            // Dropping it before the teardown also lets that log line say which ending it was
            self.threads.entries.remove(root_ts);
            if let Err(e) = self.threads.save() {
                ctx.error("bridge", &format!("threads.json save failed: {e}"));
            }
            self.terminate(key, sid.as_deref(), None).await;
            // Nothing may be posted into it later either
            self.awaiting_receipt.retain(|_, (k, _)| k != key);
            return;
        }
        if !self.ledger.pending(key).iter().any(|id| id == deleted) {
            ctx.info(
                "bridge",
                &format!(
                    "message_deleted chan={} ts={deleted} -> already answered / not in-flight, \
                     no action",
                    msg.channel
                ),
            );
            return;
        }
        let sid = self.threads.get(root_ts).and_then(|e| e.agent_id.clone());
        let target = sid.as_deref().map(|s| {
            self.workers
                .warm(s)
                .and_then(|h| h.window_id.clone())
                .unwrap_or_else(|| SessionId::from(s.to_string()).window_name())
        });
        match target {
            Some(w) => {
                let envelope = envelope(msg, root_ts, self.deps.clock.now_ms());
                ctx.info(
                    "bridge",
                    &format!(
                        "message_deleted chan={} ts={deleted} thread={root_ts} -> interrupt notify",
                        msg.channel
                    ),
                );
                self.hand_notice(&w, key, &envelope);
                self.user_stop(msg, key, root_ts, ctx);
            }
            None => ctx.info(
                "bridge",
                &format!(
                    "message_deleted chan={} ts={deleted} -> no active worker thread, dropped \
                     (best-effort)",
                    msg.channel
                ),
            ),
        }
        // A cancelled request is not pending (take it off the watch)
        self.ledger.disposed(key, &[deleted.to_string()]);
    }

    /// Queue a termination. No second one for the same thread: it would try to kill the same
    /// window twice and say goodbye twice (an in-flight guard).
    pub(super) fn push_drain(
        &mut self,
        key: &ThreadKey,
        session_id: String,
        farewell: Option<(String, String)>,
        thinking: Option<slack::Thinking>,
        ctx: &LogCtx,
    ) {
        if self.workers.is_draining(key) {
            ctx.debug(
                "bridge",
                &format!("exit: already draining key={key} — ignoring the second request"),
            );
            return;
        }
        let pending = self.ledger.pending(key);
        if !pending.is_empty() {
            ctx.info(
                "bridge",
                &format!(
                    "exit: waiting for in-flight reply before terminating key={key} pending=[{}]",
                    pending.join(",")
                ),
            );
        }
        self.workers.push_drain(worker::DrainJob {
            key: key.clone(),
            session_id,
            deadline_ms: self.deps.clock.now_ms() + DRAIN_TIMEOUT_MS,
            farewell,
            thinking,
        });
    }

    /// Run terminations that are due. The wait ends when nothing is pending or after 30 s,
    /// whichever comes first (it watches **pending, not liveness**, so a brief respawn gap
    /// during draining never triggers an early kill).
    pub(super) async fn run_drains(&mut self) {
        let now = self.deps.clock.now_ms();
        // ponytail: serial in select, one per tick, worst case ~3 s (SIGTERM grace + SIGKILL wait).
        // The main loop is blocked meanwhile, so to stay inside the Stop hook's 5 s budget
        // (endpoints.rs STOP_DECISION_CAP) **never kill two in one tick**; the rest wait for the
        // next tick (500 ms later). If concurrent kills are needed, move kill into a spawn (mind the order of hooked removal)
        let ledger = &self.ledger;
        let Some(i) = self
            .workers
            .due_drain(now, |key| ledger.pending(key).is_empty())
        else {
            return;
        };
        let job = self.workers.take_drain(i);
        let ctx = LogCtx {
            session_id: Some(job.session_id.clone()),
            thread_key: Some(job.key.clone()),
        };
        let remaining = self.ledger.pending(&job.key);
        if remaining.is_empty() {
            ctx.info(
                "bridge",
                &format!(
                    "exit: in-flight reply drained — terminating key={}",
                    job.key
                ),
            );
        } else {
            ctx.info(
                "bridge",
                &format!(
                    "exit: receipt timeout reached with undisposed=[{}] — terminating anyway \
                     key={}",
                    remaining.join(","),
                    job.key
                ),
            );
        }
        self.terminate(&job.key, Some(&job.session_id), job.farewell)
            .await;
        // Clear it **after** what was waiting has finished (Drop = clear). The implicit Drop at the
        // end of scope gives the same order, but this makes the moment explicit
        drop(job.thinking);
    }

    /// Record in threads.json that we have spoken in this thread. **No session is attached**, just
    /// a mark (no `agent_id` = no agent yet). The next message here passes as a follow-up in an
    /// active thread, without a mention. Leaves an existing entry alone.
    fn mark_thread_seen(&mut self, channel: &str, root_ts: &str, ctx: &LogCtx) {
        if self.threads.get(root_ts).is_some() {
            return;
        }
        self.threads.upsert(
            root_ts,
            bridge::ThreadEntry {
                channel_id: Some(channel.to_string()),
                ..Default::default()
            },
        );
        if let Err(e) = self.threads.save() {
            ctx.error("bridge", &format!("threads.json save failed: {e}"));
        }
    }

    /// Pick up the threads the previous Bridge was waiting on (once at startup).
    ///
    /// The ledger is saved in pending.json, but **not all of it is restored**. Nobody answers
    /// pending messages in threads whose agent is dead, so restoring them would keep the silence
    /// watch around forever. Same reasoning as the pool's [`Self::restore_pools`]: keep only
    /// what still exists in tmux.
    ///
    /// **Don't go counting survivors**: the latch starts empty (= inherited agents can be
    /// delivered to as is), and the MCP mark is set by the first tool call as a fact (the
    /// `"mcp_ready"` hook). Asking tmux about every entry in threads.json at startup speeds neither up.
    ///
    /// Re-arm the watch after restoring (`touch_thread` with "" = start the clock without
    /// showing anything). Without it only the ledger comes back and "thinking" never shows during silence.
    pub(super) fn restore_pending(&mut self, ctx: &LogCtx) {
        let all = self.ledger.pending_keys();
        let alive = self.threads.surviving(&all, |sid| {
            let name = SessionId::from(sid.to_string()).window_name();
            self.deps.agent.pid_of(None, &name).is_some()
        });
        self.ledger.retain_keys(&alive);
        if all.is_empty() {
            return;
        }
        ctx.info(
            "bridge",
            &format!(
                "restored {}/{} thread(s) with undisposed messages from the previous bridge process",
                alive.len(),
                all.len()
            ),
        );
        for key in alive {
            self.touch_thread(&key, false);
        }
    }

    /// **Await** adding the ack. Fire-and-forget lets the flip on receipt (👀 remove → 🤖 add)
    /// overtake the add in flight, and 👀 lands afterwards and stays. Failures are swallowed (best-effort).
    async fn react(&self, channel: &str, ts: &str, emoji: &str, key: &ThreadKey) {
        if let Err(e) = self.deps.slack.add_reaction(channel, ts, emoji).await {
            LogCtx {
                session_id: None,
                thread_key: Some(key.clone()),
            }
            .debug("bridge", &format!("ack reaction '{emoji}' failed: {e}"));
        }
    }

    /// There was activity: re-arm the silence timer and replace the status with `status`.
    /// Idempotent.
    ///
    /// `busy` is what the activity means for the session: a delivery hands over a turn (busy), a
    /// hook is only evidence the agent is alive (not busy by itself).
    pub(super) fn touch_thread(&mut self, key: &ThreadKey, busy: bool) {
        // A thread with nothing pending needs no watch: it would never fire, and the next tick
        // would just fold it as settled (wasteful for every hook that keeps coming after the answer)
        if !self.stall.contains_key(key) && self.ledger.pending(key).is_empty() {
            return;
        }
        let e = match self.stall.get_mut(key) {
            Some(e) => e,
            None => {
                let (channel, thread) = key.split();
                // The API requires thread_ts, so a key without a root isn't watched
                let Some(ts) = thread else { return };
                // Create it **silently** (empty = no initial send). Passing `status` would send once
                // here and again in the shared code below. The single send point is below
                self.stall.entry(key.clone()).or_insert(Stall {
                    last_activity_ms: 0,
                    shown: false,
                    awaiting_perm: false,
                    thinking: slack::Thinking::new(self.deps.slack.clone(), &channel, &ts, false),
                })
            }
        };
        e.last_activity_ms = self.deps.clock.now_ms();
        // **The idle one never goes out from here.** Slack draws the busy state itself, with a
        // stop button beside it, so "activity, but nothing new to show" used to lift a line of text
        // and now takes the button away and puts it back a moment later (seen on a live thread: it
        // blinked through the whole turn). A turn ends where it always ended — the entry is
        // dropped, and its guard sends the clear from the tail of the queue
        e.shown = false;
        if busy {
            e.thinking.set(true);
        }
    }

    /// Remember that the thread's name is a person's doing, so nothing overwrites it later.
    /// A thread nobody has an entry for is left alone — there is nothing yet to protect.
    fn lock_thread_title(&mut self, msg: &InboundMsg, title: &str) {
        let Some(root) = msg.thread_ts.clone() else {
            return;
        };
        let ctx = LogCtx {
            session_id: None,
            thread_key: Some(ThreadKey::new(&msg.channel, &root)),
        };
        let Some(mut e) = self.threads.get(&root).cloned() else {
            ctx.debug("bridge", &format!("session renamed by hand to \"{title}\" — no thread of ours"));
            return;
        };
        if e.title_locked == Some(true) {
            return; // already theirs; renaming again changes nothing here
        }
        e.title_locked = Some(true);
        self.threads.upsert(&root, e);
        if let Err(err) = self.threads.save() {
            ctx.error("bridge", &format!("threads.json save failed: {err}"));
        }
        ctx.info("bridge", &format!("session renamed by hand to \"{title}\" — ours to rename no more"));
    }

    /// Log the received ids as a milestone and swap 👀 for 🤖 (fire-and-forget).
    pub(super) fn received(&mut self, key: &ThreadKey, ids: Vec<String>, ctx: &LogCtx) {
        if ids.is_empty() {
            return;
        }
        self.milestone(Some(key), "received", ctx);
        let (channel, _) = key.split();
        let ack = self.access.ack_emoji().to_string();
        for id in ids {
            let (api, channel, ack) = (self.deps.slack.clone(), channel.clone(), ack.clone());
            tokio::spawn(async move {
                api.flip_to_received(&channel, &id, &ack).await;
            });
        }
    }

    /// Push whatever is left in the queue again on each tick. **There is no receipt-timeout
    /// redelivery**, so without this a stuck thread can't recover on its own: both existing
    /// resend paths start from hooks, and neither fires while the agent is stuck:
    /// [`Bridge::retry_turn_failure`] needs StopFailure (never sent, the turn never starts),
    /// [`Bridge::flush_queued`] needs user_prompt (never sent, nothing was submitted).
    ///
    /// Re-pushing is safe because [`crate::agent::claude::Claude::deliver`] looks at the input box
    /// **before typing**: if it's covered it returns Err without sending a character. So "it flows
    /// on the tick after a person answers the dialog" works without stray keystrokes. This is the
    /// answer to the 18-minute silence of 2026-08-18.
    pub(super) fn retry_pending(&mut self, ctx: &LogCtx) {
        let roots: Vec<String> = self.pending.keys().cloned().collect();
        for root_ts in roots {
            let entry = self.threads.get(&root_ts).cloned();
            let Some(sid) = entry.as_ref().and_then(|e| e.agent_id.clone()) else {
                continue;
            };
            let window = SessionId::from(sid.clone()).window_name();
            // Push only to agents that can hear. For Starting ones, the user_prompt path still flushes
            if self.workers.state_of(entry.as_ref(), &window, self.deps.agent.as_ref())
                != crate::agent::WorkerState::Ready
            {
                continue;
            }
            let key = entry
                .as_ref()
                .and_then(|e| e.channel_id.as_ref())
                .map(|ch| ThreadKey::new(ch, &root_ts));
            self.flush_queued(&sid, Some(root_ts), key.as_ref(), ctx);
        }
    }

    /// The moment it becomes Ready, push what was held back. **One at a time**: the report of a finished
    /// delivery asks for the next one, so the queue drains in order without two texts ever racing into the
    /// same input box.
    pub(super) fn flush_queued(
        &mut self,
        session_id: &str,
        owning: Option<String>,
        key: Option<&ThreadKey>,
        ctx: &LogCtx,
    ) {
        let Some(root_ts) = owning else { return };
        if !self.pending.contains_key(&root_ts) {
            return;
        }
        let window = self
            .workers
            .window_of(session_id)
            .unwrap_or_else(|| SessionId::from(session_id.to_string()).window_name());
        let key = key
            .cloned()
            .or_else(|| {
                self.threads
                    .get(&root_ts)
                    .and_then(|e| e.channel_id.clone())
                    .map(|ch| ThreadKey::new(&ch, &root_ts))
            })
            .unwrap_or_else(|| ThreadKey::new("", &root_ts));
        self.start_delivery(&window, &key, &root_ts, ctx);
    }

    /// Hand the first queued message of a thread to the agent, **off the loop**.
    ///
    /// Delivery sleeps (up to 1.2 s, pressing Enter until the input box empties), and the loop answers
    /// hooks — the `PreToolUse` budget is 3 s — so it cannot be done here. One per window: a second text
    /// arriving mid-delivery would interleave in the agent's input box.
    pub(super) fn start_delivery(
        &mut self,
        window: &str,
        key: &ThreadKey,
        root_ts: &str,
        ctx: &LogCtx,
    ) {
        if self.delivering.contains_key(window) {
            return; // busy: the report will come back and ask for the next one
        }
        // A window that just refused is left alone for a while. Typing the same message in again every
        // tick stacks it in the input box, and the person hears about each attempt
        if self
            .delivery_trouble
            .get(window)
            .is_some_and(|t| self.deps.clock.now_ms() < t.retry_after_ms)
        {
            return;
        }
        let Some(msg) = self.pending.get(root_ts).and_then(|q| q.first()).cloned() else {
            return;
        };
        let text = self
            .pending_envelopes
            .get(&msg.ts)
            .cloned()
            .unwrap_or_else(|| envelope(&msg, root_ts, self.deps.clock.now_ms()));
        self.delivering.insert(
            window.to_string(),
            crate::bridge::DeliveryInFlight {
                key: key.clone(),
                msg_ts: msg.ts.clone(),
                since_ms: self.deps.clock.now_ms(),
            },
        );
        ctx.debug(
            "bridge",
            &format!("handing message_id={} to {window}", msg.ts),
        );
        let agent = self.deps.agent.clone();
        let tx = self.deliv_tx.clone();
        let report = crate::bridge::DeliveryDone {
            window: window.to_string(),
            key: key.clone(),
            msg_ts: msg.ts.clone(),
            result: Ok(()),
        };
        let w = Window::of(window);
        tokio::spawn(async move {
            // **One report, whatever happens** — a panic in the blocking task included. Without it the
            // latch below never clears and that thread goes quiet for good
            let result = tokio::task::spawn_blocking(move || agent.deliver(&w, &text))
                .await
                .unwrap_or_else(|e| Err(format!("delivery task died: {e}")));
            let _ = tx.send(crate::bridge::DeliveryDone { result, ..report }).await;
        });
    }

    /// Hand a notice to an agent off the loop — an edit or a deletion, not a request of its own.
    ///
    /// **Not queued**: a notice that misses its moment is worth nothing, and there is no one waiting on a
    /// reply to it. It also does not take the per-window latch, so it can go out while a request is being
    /// delivered; the text is short and the TUI takes it in one go.
    pub(super) fn hand_notice(&self, window: &str, key: &ThreadKey, text: &str) {
        let agent = self.deps.agent.clone();
        let (w, text) = (Window::of(window), text.to_string());
        let key = key.clone();
        tokio::spawn(async move {
            let why = match tokio::task::spawn_blocking(move || agent.deliver(&w, &text)).await {
                Ok(Ok(())) => return,
                Ok(Err(e)) => e,
                Err(e) => format!("delivery task died: {e}"),
            };
            LogCtx {
                session_id: None,
                thread_key: Some(key),
            }
            .error("bridge", &format!("notice delivery failed: {why}"));
        });
    }

    /// A delivery finished. **Everything that used to follow the call happens here.**
    pub(super) async fn on_delivery_done(&mut self, done: crate::bridge::DeliveryDone) {
        let (channel, thread_ts) = done.key.split();
        let root_ts = thread_ts.unwrap_or_default();
        let ctx = LogCtx {
            session_id: None,
            thread_key: Some(done.key.clone()),
        };
        self.delivering.remove(&done.window);
        match &done.result {
            Ok(()) => {
                // Confirmed, so it leaves the queue. Until now it stayed there on purpose
                // It went through: this window is out of trouble, and the next failure is news again
                self.delivery_trouble.remove(&done.window);
                self.pending_envelopes.remove(&done.msg_ts);
                // Typed in, not yet proven to be in. [`Bridge::warn_unreceived`] comes back to it
                self.awaiting_receipt.insert(
                    done.msg_ts.clone(),
                    (done.key.clone(), self.deps.clock.now_ms() + RECEIPT_GRACE_MS),
                );
                if let Some(q) = self.pending.get_mut(&root_ts) {
                    q.retain(|m| m.ts != done.msg_ts);
                    if q.is_empty() {
                        self.pending.remove(&root_ts);
                    }
                }
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: deliver message_id={} thread={root_ts} -> worker {}",
                        done.msg_ts, channel
                    ),
                );
                self.touch_thread(&done.key, true);
                // Next in line for this window
                self.start_delivery(&done.window.clone(), &done.key.clone(), &root_ts, &ctx);
            }
            // Not delivered = this message **reached nobody**, and it is still in the queue. A log line
            // alone leaves 👀 on it in silence (2026-08-18: two threads behind a modal were silent for
            // 18 minutes that way), so tell the person waiting
            Err(e) => {
                ctx.error(
                    "bridge",
                    &format!("delivery failed: {e} — left queued for retry"),
                );
                // **Told once per spell of trouble.** The message stays queued and the tick keeps trying,
                // so without this the thread fills with the same warning every couple of seconds
                let told = self
                    .delivery_trouble
                    .get(&done.window)
                    .is_some_and(|t| t.told);
                self.delivery_trouble.insert(
                    done.window.clone(),
                    crate::bridge::DeliveryTrouble {
                        retry_after_ms: self.deps.clock.now_ms() + DELIVERY_RETRY_WAIT_MS,
                        told: true,
                    },
                );
                if !told {
                    self.post_error_frame(
                        channel,
                        root_ts,
                        crate::t!("Couldn't hand this to the agent: {e}", "エージェントに渡せませんでした: {e}"),
                    );
                }
            }
        }
    }

    /// Remembers the newest message of a thread on its record, for `status` to link to.
    ///
    /// **Only what a person sent.** The agent's own replies would need the posted ts carried back
    /// through the disposition channel; a person's last message sits a line or two above it, which
    /// lands on the same screen, and reads better as the thread's title than its opening line.
    fn remember_last_message(&mut self, root_ts: &str, msg: &InboundMsg) {
        let Some(e) = self.threads.entries.get_mut(root_ts) else {
            return; // a thread whose record is written a moment later, by the spawn path
        };
        e.last_ts = Some(msg.ts.clone());
        // Same shape as `topic`: trimmed, 60 characters, counted in chars so multi-byte text is whole
        let text: String = msg.text.trim().chars().take(60).collect();
        e.last_text = (!text.is_empty()).then_some(text);
        if let Err(err) = self.threads.save() {
            LogCtx::default().error("bridge", &format!("threads.json save failed: {err}"));
        }
    }

    /// A body that was typed in and never turned up anywhere. **Told once, and never retyped** — retyping
    /// is what stacked four copies of the same request in one input box. The ledger is the judge: an id
    /// still unreceived after [`RECEIPT_GRACE_MS`] reached no one, whatever the screen looked like.
    ///
    /// The thread is settled with it, as a told failure is elsewhere: left unanswered, the silence watcher
    /// puts up `is thinking…` for a turn that will never start.
    pub(super) fn warn_unreceived(&mut self, ctx: &LogCtx) {
        let now = self.deps.clock.now_ms();
        let due: Vec<String> = self
            .awaiting_receipt
            .iter()
            .filter(|(_, (_, at))| now >= *at)
            .map(|(ts, _)| ts.clone())
            .collect();
        for ts in due {
            let Some((key, _)) = self.awaiting_receipt.remove(&ts) else {
                continue;
            };
            if !self.ledger.unreceived(&key).contains(&ts) {
                continue; // it went in — the hook or the transcript said so
            }
            let ctx = LogCtx {
                thread_key: Some(key.clone()),
                ..ctx.clone()
            };
            ctx.error(
                "bridge",
                &format!("message_id={ts} was typed in but never reached the agent"),
            );
            self.settle_told(&key);
            let (channel, Some(thread_ts)) = key.split() else {
                continue;
            };
            self.post_error_frame(
                channel,
                thread_ts,
                crate::t!(
                    "This never reached the agent — it was typed in, but nothing came back. Send it again if it still matters.",
                    "これはエージェントに届きませんでした。入力は通りましたが、取り込まれた形跡がありません。必要なら送り直してください。"
                ),
            );
        }
    }

    /// A delivery that never reported. **Give up on it** — the message is still queued, so the next tick
    /// tries again; leaving the latch set would silence that window for good.
    pub(super) fn give_up_stuck_deliveries(&mut self, ctx: &LogCtx) {
        let now = self.deps.clock.now_ms();
        let stuck: Vec<String> = self
            .delivering
            .iter()
            .filter(|(_, d)| now.saturating_sub(d.since_ms) > DELIVERY_STUCK_MS)
            .map(|(w, _)| w.clone())
            .collect();
        for window in stuck {
            let Some(d) = self.delivering.remove(&window) else {
                continue;
            };
            ctx.error(
                "bridge",
                &format!(
                    "delivery to {window} never reported (message_id={}) — giving up; it stays queued",
                    d.msg_ts
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(kind: ChannelKind, user: Option<&str>, is_bot: bool) -> InboundMsg {
        InboundMsg {
            channel: match kind {
                ChannelKind::Dm => "D1".into(),
                ChannelKind::Channel => "C1".into(),
            },
            channel_kind: kind,
            ts: "1.1".into(),
            thread_ts: None,
            user: user.map(str::to_string),
            is_bot,
            bot_id: is_bot.then(|| "B1".to_string()),
            text: "hi".into(),
            files: Vec::new(),
            file_paths: Vec::new(),
            file_errors: Vec::new(),
            reaction: None,
            deleted_ts: None,
            edited: None,
            session_title: None,
        }
    }

    fn access_with_route() -> Access {
        Access::from_str(r#"{"owner":"U1","routes":{"C1":{"repo_path":"/x"}}}"#).unwrap()
    }

    /// Posts through the Web API carry a bot_id **even when a person wrote them**. The Owner's own
    /// post passes as a person; other bots are dropped unless `allow-bot` lets them in.
    #[test]
    fn gate_drops_bots_but_honors_the_owners_web_api_post() {
        let a = access_with_route();
        assert_eq!(
            a.gate(&msg(ChannelKind::Channel, Some("U1"), true), true, false),
            GateVerdict::Serve
        );
        assert_eq!(
            a.gate(&msg(ChannelKind::Channel, Some("U2"), true), true, false),
            GateVerdict::Drop("drop-bot-not-allowed")
        );
        assert_eq!(
            a.gate(&msg(ChannelKind::Channel, None, true), true, false),
            GateVerdict::Drop("drop-bot-not-allowed")
        );
        // An allow-bot bot passes only when mentioned (or in an active thread)
        let allowed =
            Access::from_str(r#"{"owner":"U1","allowedBots":["B7"],"routes":{}}"#).unwrap();
        let mut b = msg(ChannelKind::Channel, Some("U9"), true);
        b.bot_id = Some("B7".into());
        assert_eq!(allowed.gate(&b, true, false), GateVerdict::Serve);
        assert_eq!(
            allowed.gate(&b, false, false),
            GateVerdict::Drop("require-mention-unmet")
        );
        // A bot's DM is always dropped
        let mut dm_bot = msg(ChannelKind::Dm, Some("U9"), true);
        dm_bot.bot_id = Some("B7".into());
        assert_eq!(
            allowed.gate(&dm_bot, true, false),
            GateVerdict::Drop("bot-dm-blocked")
        );
    }

    #[test]
    fn gate_serves_owner_dm() {
        let v = access_with_route().gate(&msg(ChannelKind::Dm, Some("U1"), false), true, false);
        assert_eq!(v, GateVerdict::Serve);
    }

    #[test]
    fn gate_drops_other_user_dm() {
        let v = access_with_route().gate(&msg(ChannelKind::Dm, Some("U2"), false), true, false);
        assert_eq!(v, GateVerdict::Drop("dm-not-owner"));
    }

    /// Getting in on a channel depends only on **a mention or an active thread**.
    /// **Routes are not checked**: the Owner's mention is answered even in an unregistered channel.
    #[test]
    fn gate_needs_a_mention_or_an_active_thread_not_a_route() {
        let a = access_with_route();
        let owner = msg(ChannelKind::Channel, Some("U1"), false);
        assert_eq!(a.gate(&owner, true, false), GateVerdict::Serve, "名指し");
        assert_eq!(
            a.gate(&owner, false, true),
            GateVerdict::Serve,
            "動いているスレッドの続き"
        );
        assert_eq!(
            a.gate(&owner, false, false),
            GateVerdict::Drop("require-mention-unmet"),
            "名指しでもスレッドの続きでもない雑談は流さない"
        );
        // The Owner's mention passes even in an unregistered channel (C9)
        let mut elsewhere = owner.clone();
        elsewhere.channel = "C9".into();
        assert_eq!(a.gate(&elsewhere, true, false), GateVerdict::Serve);
    }

    /// Someone other than the Owner is passed **as context** only inside an active thread.
    #[test]
    fn gate_passes_a_non_owner_as_context_only_inside_an_active_thread() {
        let a = access_with_route();
        let other = msg(ChannelKind::Channel, Some("U2"), false);
        assert_eq!(a.gate(&other, false, true), GateVerdict::Context);
        assert_eq!(
            a.gate(&other, true, false),
            GateVerdict::Drop("drop-not-owner"),
            "名指しされても Owner でなければ動かさない"
        );
    }

    #[test]
    fn gate_is_fail_closed_without_owner() {
        let empty = Access::default();
        assert!(matches!(
            empty.gate(&msg(ChannelKind::Dm, Some("U1"), false), true, false),
            GateVerdict::Drop(_)
        ));
        assert!(matches!(
            empty.gate(&msg(ChannelKind::Dm, None, false), true, false),
            GateVerdict::Drop(_)
        ));
    }

    #[test]
    fn dedup_drops_second_delivery_of_same_event() {
        let mut d = RecentDeliveries::new();
        assert!(!d.seen("C1", "1.1"), "first sighting is new");
        assert!(
            d.seen("C1", "1.1"),
            "same (channel, ts) must be a duplicate"
        );
        assert!(!d.seen("C1", "1.2"));
        assert!(!d.seen("C2", "1.1"));
    }

    #[test]
    fn dedup_evicts_oldest_past_cap() {
        let mut d = RecentDeliveries::new();
        for i in 0..512 {
            assert!(!d.seen("C1", &format!("{i}")));
        }
        assert!(d.seen("C1", "511"), "newest must still be remembered");
        d.seen("C1", "512"); // the 513th → the oldest (0) is pushed out
        assert!(!d.seen("C1", "0"), "oldest must have been evicted");
    }

    #[test]
    fn decide_table() {
        let entry = ThreadEntry::new("C1", "sid-1");
        assert_eq!(Dispatch::decide(None, WorkerState::Absent), Dispatch::SpawnNew);
        assert_eq!(
            Dispatch::decide(Some(&entry), WorkerState::Ready),
            Dispatch::Deliver
        );
        assert_eq!(
            Dispatch::decide(Some(&entry), WorkerState::Starting),
            Dispatch::Queue
        );
        assert_eq!(
            Dispatch::decide(Some(&entry), WorkerState::Absent),
            Dispatch::SpawnResume("sid-1".into())
        );
    }

    #[test]
    fn decide_spawns_new_when_entry_has_no_session() {
        let entry = ThreadEntry::default();
        assert_eq!(
            Dispatch::decide(Some(&entry), WorkerState::Absent),
            Dispatch::SpawnNew
        );
    }

}
