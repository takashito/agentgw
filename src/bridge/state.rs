//! What the Bridge remembers and decides on its own. Its only I/O is the state files.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::chat::{ChannelKind, InboundMsg, ThreadKey};
use crate::log::LogCtx;
use crate::state_dir::{StateDir, json_obj, write_atomic_at};


/// A stable, tmux-safe key per repo. djb2 → base36. Built by [`PoolKey::of_cwd`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PoolKey(String);

impl PoolKey {
    /// A stable, tmux-safe pool key per repo. djb2 → base36 (hashed as i32, then
    /// base36 as u32). Keeps window names short whatever the path length.
    pub fn of_cwd(cwd: &str) -> PoolKey {
        PoolKey(Self::key_str(cwd))
    }

    fn key_str(cwd: &str) -> String {
        let mut h: i32 = 5381;
        // Same as JS charCodeAt = UTF-16 code units
        for u in cwd.encode_utf16() {
            h = h.wrapping_shl(5).wrapping_add(h).wrapping_add(u as i32);
        }
        let mut n = h as u32;
        if n == 0 {
            return "repo-0".to_string();
        }
        let mut buf = Vec::new();
        while n > 0 {
            buf.push(char::from_digit(n % 36, 36).unwrap() as u8);
            n /= 36;
        }
        buf.reverse();
        format!("repo-{}", String::from_utf8(buf).unwrap())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PoolKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<&str> for PoolKey {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// One lifecycle milestone (which event number / ms since the previous one / ms since spawn).
///
/// Arrival times are stamped on **one clock**, so the gaps between events are authoritative
/// (no clock skew between processes mixed in).
pub struct Milestone {
    pub seq: u32,
    pub since_prev_ms: u64,
    pub since_spawn_ms: u64,
}

impl Milestone {
    /// The lifecycle log text; must not change by a single character.
    pub fn message(&self, key: &ThreadKey, event: &str) -> String {
        format!(
            "thread={key} #{} {event} +{}ms (since spawn +{}ms)",
            self.seq, self.since_prev_ms, self.since_spawn_ms
        )
    }
}

/// The per-thread timeline itself. The thing that stamps [`Milestone`]s.
///
/// Its job is to own **one clock** (so no clock skew between processes leaks in; gaps always come from here).
#[derive(Default)]
pub struct Lifecycle {
    /// key → (spawn_at, prev_at, seq)
    state: HashMap<ThreadKey, (u64, u64, u32)>,
}

impl Lifecycle {
    pub fn new() -> Self {
        Self::default()
    }

    /// `spawn` (or the first event for an unknown key) resets that key's timeline.
    pub fn record(&mut self, key: &ThreadKey, event: &str, now_ms: u64) -> Milestone {
        let s = self.state.entry(key.clone()).or_insert((now_ms, now_ms, 0));
        if event == "spawn" {
            *s = (now_ms, now_ms, 0);
        }
        s.2 += 1;
        let m = Milestone {
            seq: s.2,
            // The wall clock can go backwards — clamp negative gaps to 0
            since_prev_ms: now_ms.saturating_sub(s.1),
            since_spawn_ms: now_ms.saturating_sub(s.0),
        };
        s.1 = now_ms;
        m
    }
}

// ─── ledgers: threads.json / access.json ───────────────────────────────────────
// Only threads' agent_id/channel_id/repo_path and access's owner/routes are used.
// The rest round-trips through the flatten catch-all (so production JSON carries over unconverted on switchover day).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One thread in threads.json = **what the Bridge remembers about that thread**
/// (which session handles it / where it runs / what was allowed).
///
/// Unknown fields round-trip through `extra` — so production JSON carries over unconverted on switchover day.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct ThreadEntry {
    /// **The on-disk name is `session_id`, as in the Bun version.**
    /// The alias also accepts dev state written back when it was the custom `agent_id` —
    /// if the names disagreed, dropping production's threads.json in on switchover day would make every
    /// thread look like "no session" and start over in a new session (key to unconverted carry-over).
    #[serde(
        rename = "session_id",
        alias = "agent_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub agent_id: Option<String>,
    /// Tool names **a person clicked** "don't ask again" for in this thread.
    /// There is no path for a session to write this itself (a structural guard against prompt injection).
    #[serde(rename = "allowedTools", skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_path: Option<String>,
    /// The thread's topic (first 60 characters of the opening message). Becomes the link text in `status`.
    /// Written in the same shape as before — so production values are read as-is on switchover day
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// Requests handed over that **have no reply yet** (moved from pending.json on 2026-08-02).
    ///
    /// **Not the same as** the entry's `pending` (inside `extra`) — that one is the delivery queue of
    /// "not handed over yet"; this one marks "handed over but no reply". The names are confusing, so
    /// this uses the Bun version's name (inflight). Written in one place only: [`Ledger::flush`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inflight: Vec<Inflight>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl ThreadEntry {
    pub fn new(channel_id: &str, agent_id: &str) -> Self {
        Self {
            agent_id: Some(agent_id.to_string()),
            channel_id: Some(channel_id.to_string()),
            ..Default::default()
        }
    }

    /// The threads.json entry shell used when binding a worker claimed from the pool to a thread.
    /// The caller puts claimed.session_id into `agent_id`.
    pub fn for_pool_assignment(channel_id: &str, cwd: &str, topic: Option<String>) -> ThreadEntry {
        ThreadEntry {
            channel_id: Some(channel_id.to_string()),
            repo_path: Some(cwd.to_string()),
            topic,
            ..Default::default()
        }
    }
}

/// threads.json — top level is thread_ts → entry. BTreeMap for a stable key order.
#[derive(Default)]
pub struct Threads {
    pub entries: BTreeMap<String, ThreadEntry>,
    path: Option<PathBuf>,
}

impl Threads {
    pub fn from_str(src: &str) -> serde_json::Result<Self> {
        Ok(Self {
            entries: serde_json::from_str(src)?,
            path: None,
        })
    }

    /// **Don't stay quiet about failing to read.** "No file" (first start = normal) and "there but
    /// unreadable / corrupt" (an accident) mean opposite things, yet both were squashed into "empty" by
    /// `.ok()`. To `Bridge::reap_stray_windows` an empty table means "every window's owner is
    /// unknown", so **running agents got closed one after another** (3 of them on a real machine, 2026-08-03).
    /// And not a single log line came out, so there was no way to trace why afterwards. **Missing is
    /// normal, unreadable is an accident** — keep them apart and log only the accident as error.
    pub fn load(dir: &StateDir) -> Self {
        let path = dir.join("threads.json");
        let entries = match std::fs::read_to_string(&path) {
            Ok(src) => serde_json::from_str(&src).unwrap_or_else(|e| {
                LogCtx::default().error(
                    "bridge",
                    &format!(
                        "threads.json is corrupt ({e}) — every thread lost its worker; \
                         live windows are NOT closed, but delivery starts from scratch"
                    ),
                );
                Default::default()
            }),
            // First start. There are simply no threads yet, so start empty without a word
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(e) => {
                LogCtx::default().error(
                    "bridge",
                    &format!("could not read threads.json ({e}) — every thread lost its worker"),
                );
                Default::default()
            }
        };
        Self {
            entries,
            path: Some(path),
        }
    }

    pub fn to_string_pretty(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(&self.entries)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        write_atomic_at(path, &self.to_string_pretty()?)
    }

    pub fn get(&self, thread_ts: &str) -> Option<&ThreadEntry> {
        self.entries.get(thread_ts)
    }

    /// Whether this thread is **already running** (`isThreadActive`). If so, a channel
    /// follow-up is accepted without a mention. `pending` / `paused` count as not running.
    pub fn is_active(&self, thread_ts: &str) -> bool {
        !matches!(self.status_of(thread_ts), "pending" | "paused" | "none")
    }

    /// `getThreadStatus` — an unknown thread is `none`, an entry without `status` is `active`.
    pub fn status_of(&self, thread_ts: &str) -> &str {
        match self.get(thread_ts) {
            None => "none",
            Some(e) => match e.extra.get("status").and_then(|v| v.as_str()) {
                Some(s @ ("pending" | "paused")) => s,
                _ => "active",
            },
        }
    }

    /// Parks a not-yet-delivered message in threads.json (`enqueuePending`).
    /// **Idempotent on message_id** — the same message is never queued twice. Creates a minimal entry if missing
    /// (never drop it just because there is nowhere to park it).
    pub fn enqueue_pending(&mut self, thread_ts: &str, channel: &str, msg: &InboundMsg) {
        let e = self.entries.entry(thread_ts.to_string()).or_default();
        if e.channel_id.is_none() {
            e.channel_id = Some(channel.to_string());
        }
        let mut queue = match e.extra.get("pending") {
            Some(serde_json::Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        if queue
            .iter()
            .any(|m| m["meta"]["message_id"].as_str() == Some(msg.ts.as_str()))
        {
            return;
        }
        queue.push(serde_json::json!({
            "meta": {
                "channel_id": msg.channel,
                "message_id": msg.ts,
                "thread_ts": thread_ts,
                "user": msg.user,
            },
            // Keep the body and attachments **as they are**, so the same envelope can be built on restore
            "text": msg.text,
            "file_paths": msg.file_paths,
            "file_errors": msg.file_errors,
        }));
        e.extra
            .insert("pending".into(), serde_json::Value::Array(queue));
    }

    /// Takes out the parked messages and **deletes** them (`drainPending`).
    pub fn drain_pending(&mut self, thread_ts: &str) -> Vec<InboundMsg> {
        let Some(e) = self.entries.get_mut(thread_ts) else {
            return Vec::new();
        };
        let Some(serde_json::Value::Array(queue)) = e.extra.remove("pending") else {
            return Vec::new();
        };
        let channel_kind = |c: &str| {
            if c.starts_with('D') {
                ChannelKind::Dm
            } else {
                ChannelKind::Channel
            }
        };
        queue
            .into_iter()
            .filter_map(|m| {
                let channel = m["meta"]["channel_id"].as_str()?.to_string();
                Some(InboundMsg {
                    channel_kind: channel_kind(&channel),
                    channel,
                    ts: m["meta"]["message_id"].as_str()?.to_string(),
                    thread_ts: Some(thread_ts.to_string()),
                    user: m["meta"]["user"].as_str().map(str::to_string),
                    is_bot: false,
                    bot_id: None,
                    text: m["text"].as_str().unwrap_or_default().to_string(),
                    files: Vec::new(),
                    file_paths: m["file_paths"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                    file_errors: m["file_errors"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                    reaction: None,
                    deleted_ts: None,
                    edited: None,
                })
            })
            .collect()
    }

    /// Roots of threads holding parked messages (where to pick up again at startup).
    pub fn threads_with_pending(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, e)| {
                matches!(e.extra.get("pending"), Some(serde_json::Value::Array(a)) if !a.is_empty())
            })
            .map(|(ts, _)| ts.clone())
            .collect()
    }

    /// Counts one more consecutive bot post and returns the streak (`bumpBotStreak`). 0 if there is no entry —
    /// **nothing to count** (a thread nobody has spoken in yet).
    pub fn bump_bot_streak(&mut self, thread_ts: &str) -> u64 {
        let Some(e) = self.entries.get_mut(thread_ts) else {
            return 0;
        };
        let next = e
            .extra
            .get("bot_streak")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            + 1;
        e.extra.insert("bot_streak".into(), next.into());
        next
    }

    /// Loop breaker — pauses this thread (`pauseThread`). Bots can't get in until a person speaks.
    pub fn pause(&mut self, thread_ts: &str) {
        if let Some(e) = self.entries.get_mut(thread_ts) {
            e.extra
                .insert("status".into(), serde_json::Value::String("paused".into()));
        }
    }

    /// A person spoke — reset the streak and resume a paused thread (`resetThreadStreak`).
    /// `pending` (not started yet) is left alone. Returns whether it was paused.
    pub fn reset_bot_streak(&mut self, thread_ts: &str) -> bool {
        let was_paused = self.status_of(thread_ts) == "paused";
        let Some(e) = self.entries.get_mut(thread_ts) else {
            return false;
        };
        if e.extra.get("status").and_then(|v| v.as_str()) == Some("pending") {
            return false;
        }
        e.extra.insert("bot_streak".into(), 0u64.into());
        if was_paused {
            e.extra
                .insert("status".into(), serde_json::Value::String("active".into()));
        }
        was_paused
    }

    pub fn upsert(&mut self, thread_ts: &str, entry: ThreadEntry) {
        self.entries.insert(thread_ts.to_string(), entry);
    }

    /// Keeps only the keys that may be put back on the ledger at startup (`Bridge::restore_pending`).
    ///
    /// **Only threads whose agent is alive.** Nobody will answer a dead thread's pending replies,
    /// so loading them would leave the silence watcher waiting forever. Keys whose thread can't be found or
    /// that have no session yet are dropped for the same reason. `alive` is session_id → alive (measured from tmux pids).
    pub fn surviving(&self, keys: &[ThreadKey], alive: impl Fn(&str) -> bool) -> Vec<ThreadKey> {
        keys.iter()
            .filter(|key| {
                key.split()
                    .1
                    .and_then(|ts| self.get(&ts))
                    .and_then(|e| e.agent_id.as_deref())
                    .is_some_and(&alive)
            })
            .cloned()
            .collect()
    }

    pub fn find_by_session(&self, session_id: &str) -> Option<(&String, &ThreadEntry)> {
        self.entries
            .iter()
            .find(|(_, e)| e.agent_id.as_deref() == Some(session_id))
    }

    /// Whether "don't ask again" was clicked for this tool in this thread.
    pub fn thread_tool_allowed(&self, thread_ts: &str, tool: &str) -> bool {
        self.get(thread_ts)
            .and_then(|e| e.allowed_tools.as_ref())
            .is_some_and(|v| v.iter().any(|t| t == tool))
    }

    /// Remembers "don't ask again in this thread". **Called only when a person clicks the button.**
    /// Creates a minimal shell if the thread has no record yet.
    pub fn grant_thread_tool(&mut self, thread_ts: &str, tool: &str) {
        if thread_ts.is_empty() || tool.is_empty() {
            return;
        }
        let mut e = self.get(thread_ts).cloned().unwrap_or_default();
        let allowed = e.allowed_tools.get_or_insert_with(Vec::new);
        if !allowed.iter().any(|t| t == tool) {
            allowed.push(tool.to_string());
        }
        self.upsert(thread_ts, e);
    }

    // ── warm pool ──

    /// Whether the pool may be claimed. **Only threads we don't know yet** qualify.
    /// An existing thread loses the conversation unless it resumes/delivers in its own session.
    pub fn should_claim_pool(entry: Option<&ThreadEntry>) -> bool {
        entry.is_none()
    }
}

/// `"pools"` in access.json — cwd → the session ID **designated** for the pool. Keyed by cwd
/// (human-readable); [`PoolKey`] can be derived from the cwd, so it isn't stored. BTreeMap for a stable key order.
///
/// What it holds is not the pool itself but the **designation**, which outlives a Bridge process. When starting
/// a pool agent, `--resume` if designated, otherwise cut a new ID and designate it here. Without this, every
/// restart would cut a throwaway session and claude's session history would fill up with pool sessions.
///
/// Only four things drop a designation: claiming it for a thread (graduation) / a failed resume / the session ending /
/// the cwd no longer being a pool target.
///
/// **Moved from pools.json into access.json on 2026-08-02.** Writes go per key through
/// [`StateDir::patch_json`], so they don't clobber the path that writes the settings.
#[derive(Default)]
pub struct Pools {
    entries: BTreeMap<String, String>,
    dir: Option<StateDir>,
}

impl Pools {
    pub fn load(dir: &StateDir) -> Self {
        Self {
            entries: serde_json::from_value(
                dir.read_json_or("access.json", json_obj())["pools"].clone(),
            )
            .unwrap_or_default(),
            dir: Some(dir.clone()),
        }
    }

    pub fn from_str(src: &str) -> serde_json::Result<Self> {
        Ok(Self {
            entries: serde_json::from_str(src)?,
            dir: None,
        })
    }

    pub fn to_string_pretty(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(&self.entries)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(dir) = &self.dir else {
            return Ok(());
        };
        dir.patch_json("access.json", "pools", serde_json::to_value(&self.entries)?)
    }

    /// Every designated session_id (any cwd). Window cleanup uses this to check "does it have an owner" —
    /// **the in-memory pool is not enough**. Right after the Bridge restarts, the pool isn't up
    /// yet, and live pool agents would look ownerless.
    pub fn sessions(&self) -> impl Iterator<Item = &str> {
        self.entries.values().map(String::as_str)
    }

    /// The session designated for this cwd's pool. If any, it is started with `--resume`.
    pub fn session_of(&self, cwd: &str) -> Option<&str> {
        self.entries.get(cwd).map(String::as_str)
    }

    pub fn nominate(&mut self, cwd: &str, session_id: &str) {
        self.entries.insert(cwd.to_string(), session_id.to_string());
    }

    pub fn release(&mut self, cwd: &str) -> Option<String> {
        self.entries.remove(cwd)
    }

    /// Drops this session_id's designation (called where the cwd isn't at hand). Returns the cwd if dropped.
    pub fn release_session(&mut self, session_id: &str) -> Option<String> {
        // ponytail: a handful of pools at most — no reverse index needed
        let cwd = self
            .entries
            .iter()
            .find(|(_, sid)| sid.as_str() == session_id)
            .map(|(cwd, _)| cwd.clone())?;
        self.entries.remove(&cwd);
        Some(cwd)
    }

    /// Designated `(cwd, session_id)` pairs. Walked by the pickup at startup.
    pub fn rows(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .map(|(cwd, sid)| (cwd.clone(), sid.clone()))
            .collect()
    }
}

/// Settings for one channel in access.json = **where things run when someone speaks in this channel**.
///
/// repo_path is the working directory, warm is whether to pre-start, allowed_tools are standing permissions.
/// Like [`ThreadEntry`], unknown fields round-trip through `extra`.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Route {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Whether to pre-start this channel's repo. Unset = follow the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warm: Option<bool>,
    /// Tool names **a person clicked** "don't ask again" for in this channel.
    #[serde(rename = "allowedTools", skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    /// The machine that handles this channel (a machine's name / the gateway's own name).
    /// **Unset = this machine handles it itself** (the default for a standalone Bridge).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// access.json. Empty owner = nobody gets in (fail-closed).
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Access {
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub routes: BTreeMap<String, Route>,
    /// Bot ids allowed in. Always written out (the key stays even when empty).
    #[serde(rename = "allowedBots", default)]
    pub allowed_bots: Vec<String>,
    #[serde(rename = "homeChannel", skip_serializing_if = "Option::is_none")]
    pub home_channel: Option<String>,
    /// Reaction name for the receive ack. If unset, [`Access::ack_emoji`] returns "eyes".
    #[serde(rename = "ackReaction", skip_serializing_if = "Option::is_none")]
    pub ack_reaction: Option<String>,
    /// Body length limit per post. Both default and max are `slack::MAX_CHUNK_LIMIT`.
    #[serde(rename = "textChunkLimit", skip_serializing_if = "Option::is_none")]
    pub text_chunk_limit: Option<usize>,
    /// How to split. `"newline"` looks for a break at paragraph → line → word. By default it cuts hard at the limit.
    #[serde(rename = "chunkMode", skip_serializing_if = "Option::is_none")]
    pub chunk_mode: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Access {
    /// Emoji for the receive ack ("eyes" when unset).
    pub fn ack_emoji(&self) -> &str {
        self.ack_reaction.as_deref().unwrap_or("eyes")
    }

    pub fn from_str(src: &str) -> serde_json::Result<Self> {
        serde_json::from_str(src)
    }

    /// If missing, owner is empty = fail-closed.
    pub fn load(dir: &StateDir) -> Self {
        std::fs::read_to_string(dir.join("access.json"))
            .ok()
            .and_then(|s| Self::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn to_string_pretty(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// **Never overwrite the whole file** — lay only the keys we own over what is on disk.
    /// The same file also holds keys only the Bridge touches (`endpoints` / `pools`), so
    /// the side writing the settings must not wipe them.
    pub fn save(&self, dir: &StateDir) -> std::io::Result<()> {
        let mut root = dir.read_json_or("access.json", serde_json::json!({}));
        let mine = serde_json::to_value(self)?;
        match (root.as_object_mut(), mine.as_object()) {
            (Some(root), Some(mine)) => {
                for (k, v) in mine {
                    root.insert(k.clone(), v.clone());
                }
            }
            // Disk is corrupt / empty — write our own view as is
            _ => root = mine,
        }
        dir.write_atomic("access.json", &serde_json::to_string_pretty(&root)?)
    }

    /// Channel → handling machine. Collects **only rows that name a machine**
    /// (`routes` also has rows with just a work path; passing them as-is would make every row "assigned").
    pub fn bridges(&self) -> BTreeMap<String, String> {
        self.routes
            .iter()
            .filter_map(|(ch, r)| Some((ch.clone(), r.bridge.clone()?)))
            .collect()
    }

    /// Sets the machine handling this channel. Leaves the row's other settings, like the work path, alone.
    pub fn set_bridge(&mut self, channel: &str, bridge_id: &str) {
        self.routes.entry(channel.to_string()).or_default().bridge = Some(bridge_id.to_string());
    }

    // ── mutations (AccessOp) ──
    // Applies a mutation as a pure function — prev is untouched; returns a new Access plus human-facing message and warnings.
    // Validation failure is Err (the caller reports it). **No authorization here.**
    // Message, warning and error wording is kept verbatim.
    /// Applies a mutation as a pure function — self is untouched; returns a new Access plus human-facing
    /// message and warnings. Validation failure is Err (the caller reports it). **No authorization here.**
    pub fn apply(&self, op: AccessOp) -> Result<(Access, String, Vec<String>), String> {
        let mut access = self.clone();
        let mut warnings = Vec::new();
        let message = match op {
            AccessOp::BotAllow(id) => {
                if !crate::chat::slack::SlackId::is_bot(&id) {
                    return Err(format!("bot_allow expects a bot id (B…), got \"{id}\""));
                }
                if !access.allowed_bots.contains(&id) {
                    access.allowed_bots.push(id.clone());
                }
                crate::t!("Added {id} to the allowed bots.", "{id} を許可 bot に追加しました。")
            }
            AccessOp::BotRemove(id) => {
                if !crate::chat::slack::SlackId::is_bot(&id) {
                    return Err(format!("bot_remove expects a bot id (B…), got \"{id}\""));
                }
                let had = access.allowed_bots.contains(&id);
                access.allowed_bots.retain(|x| *x != id);
                if had {
                    crate::t!("Removed {id} from the allowed bots.", "{id} を許可 bot から外しました。")
                } else {
                    crate::t!("{id} is not an allowed bot.", "{id} は許可 bot ではありません。")
                }
            }
            AccessOp::SetRepo { channel, path } => {
                if !crate::chat::slack::SlackId::is_channel(&channel) {
                    return Err(format!(
                        "set_repo expects a channel id (C…/G…), got \"{channel}\""
                    ));
                }
                if !path.starts_with('/') {
                    return Err(crate::t!(
                        "The project path must be absolute: \"{path}\"",
                        "プロジェクトのパスは絶対パスで指定してください: \"{path}\""
                    ));
                }
                access.routes.entry(channel.clone()).or_default().repo_path = Some(path.clone());
                // `<#C…>` renders as a clickable `#channel-name` — the Owner sees the name they typed
                // (not the raw internal id).
                crate::t!("<#{channel}> now uses the project directory `{path}`.", "<#{channel}> をプロジェクト `{path}` に紐付けました。")
            }
            AccessOp::SetWarm { channel, on } => {
                // Warm pool keys are cwds, so this only means something for channels with a repo
                // (channels without one are served by the resident Home pool). Keep the flag, but say so.
                if !crate::chat::slack::SlackId::is_channel(&channel) {
                    return Err(format!(
                        "set_warm expects a channel id (C…/G…), got \"{channel}\""
                    ));
                }
                let route = access.routes.entry(channel.clone()).or_default();
                let has_repo = !route.repo_path.as_deref().unwrap_or("").is_empty();
                route.warm = Some(on);
                if !has_repo {
                    warnings.push(crate::t!("<#{channel}> has no project yet. This takes effect once you set one with `pwd <absolute path>` in that channel.", "<#{channel}> はまだプロジェクトに紐付いていません。そのチャンネルで `pwd ＜絶対パス＞` を設定すると効きます。"));
                }
                if on {
                    crate::t!(
                        "An agent for <#{channel}> will be started ahead of time, so the first message gets a faster reply.",
                        "<#{channel}> のエージェントを前もって起動しておきます。最初のメッセージへの返事が速くなります。"
                    )
                } else {
                    crate::t!(
                        "Agents for <#{channel}> will no longer be started ahead of time. The first message waits for one to start.",
                        "<#{channel}> のエージェントを前もって起動するのをやめました。最初のメッセージは起動を待つぶん遅くなります。"
                    )
                }
            }
            AccessOp::SetHome(channel) => {
                let ch = channel.trim();
                if ch.is_empty() {
                    access.home_channel = None;
                    crate::t!("The home channel is no longer set.", "Home チャンネルの設定を解除しました。")
                } else {
                    if !crate::chat::slack::SlackId::is_channel(ch) {
                        return Err(format!(
                            "set_home_channel expects a channel id (C…/G…), got \"{ch}\""
                        ));
                    }
                    access.home_channel = Some(ch.to_string());
                    crate::t!("<#{ch}> is now the home channel.", "<#{ch}> を Home チャンネルに設定しました。")
                }
            }
        };
        Ok((access, message, warnings))
    }

    /// Where this channel's agent starts. Falls back to home with no explicit route (the second return value
    /// says so — pwd's "no route (Home fallback)" display). **pwd and spawn both read this one.**
    pub fn repo_path(&self, channel: &str, home: &str) -> (String, bool) {
        match self
            .routes
            .get(channel)
            .and_then(|r| r.repo_path.as_deref())
            .filter(|p| !p.is_empty())
        {
            Some(p) => (p.to_string(), false),
            None => (home.to_string(), true),
        }
    }

    // ── warm pool: pool granularity (pure logic) ──

    /// The set of pools to keep stocked = **the cwds that should have a pool agent waiting** (fixed when agents start).
    /// A pure function of access alone.
    /// - Empty if owner is unset (serving nobody, pre-starting is pure waste)
    /// - Always one HOME pool (the next new DM / channel without a repo. Can't opt out)
    /// - Then one per **distinct repo_path** in routes. The flag is per channel
    ///   but pools are per repo, so if even one channel pointing at the repo opts in (`warm != Some(false)`,
    ///   unset counts as opt-in) it gets stocked (OR). Opted-out channels still claim that pool if it exists.
    /// `me` is this machine's name: **a folder on another machine is not ours to warm**. The gateway keeps
    /// a copy of every channel's folder (for `channels`), and without this check it started warming
    /// agents in folders that only exist on the machines it hands those channels to.
    pub fn pool_targets(&self, home_dir: &str, me: &str) -> Vec<String> {
        if self.owner.is_empty() {
            return Vec::new();
        }
        let mut targets = vec![home_dir.to_string()];
        for cfg in self.routes.values() {
            if cfg.bridge.as_deref().is_some_and(|b| b != me) {
                continue;
            }
            let repo = match cfg.repo_path.as_deref().filter(|p| !p.is_empty()) {
                // ponytail: linear search — pool count is repo count (a few at most). Switch to a HashSet if it grows.
                Some(r) if !targets.iter().any(|cwd| cwd == r) => r,
                _ => continue,
            };
            // Opting out only means "no reason to start that pool". It isn't taken here, so
            // another route to the same repo that opts in adopts it later.
            if cfg.warm == Some(false) {
                continue;
            }
            targets.push(repo.to_string());
        }
        targets
    }
}

/// One mutation of the access ledger. An empty string for `SetHome` clears Home.
#[derive(Clone, Debug)]
pub enum AccessOp {
    BotAllow(String),
    BotRemove(String),
    SetRepo { channel: String, path: String },
    SetWarm { channel: String, on: bool },
    SetHome(String),
}


// ─── warm pool: pool granularity (pure logic) ──────────────────────

/// Where to post the start/stop home notice (only the decision, as a pure function).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoticeTarget {
    Home(String),
    /// ponytail: the Rust version has no way to open a DM yet. The caller falls back to one log line.
    OwnerDm(String),
    None_,
}

impl NoticeTarget {
    /// home_channel > Owner DM > silence (the caller still logs).
    pub fn of(home_channel: Option<&str>, owner: &str) -> NoticeTarget {
        match home_channel {
            Some(ch) => NoticeTarget::Home(ch.to_string()),
            None if !owner.is_empty() => NoticeTarget::OwnerDm(owner.to_string()),
            None => NoticeTarget::None_,
        }
    }
}

/// One pool's state as seen from the registry. Only these two bits matter for the start decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolStatus {
    pub present: bool,
    pub gave_up: bool,
}

impl PoolStatus {
    /// Whether this pool should be started now. **A given-up worker is not retried**
    /// (so spawn doesn't loop forever where MCP never comes up).
    pub fn needs_launch(this: Option<PoolStatus>) -> bool {
        match this {
            None => true,
            Some(s) => !s.present && !s.gave_up,
        }
    }

    /// Whether it is time to give up on a pool that is starting (MCP init timeout).
    pub fn should_give_up(spawned_at_ms: u64, now_ms: u64, timeout_ms: u64) -> bool {
        now_ms.saturating_sub(spawned_at_ms) > timeout_ms
    }
}

/// At startup, what to do with a pool agent still designated in [`Pools`] (pure logic).
#[derive(Debug, PartialEq, Eq)]
pub enum PoolRestore {
    /// It is still alive — adopt it as this process's pool agent (same as inheriting a thread agent).
    Adopt,
    /// It is dead — keep the designation; refill starts the same session_id with `--resume`.
    Respawn,
    /// The cwd is no longer a pool target — shut it down and drop the designation.
    Discard,
}

impl PoolRestore {
    pub fn decide(configured: bool, alive: bool) -> Self {
        match (configured, alive) {
            (false, _) => Self::Discard,
            (true, true) => Self::Adopt,
            (true, false) => Self::Respawn,
        }
    }
}

// ─── machine names and project paths ────────────────────────────────────────────

/// A machine's name: `[A-Za-z0-9][A-Za-z0-9_.-]*`. Narrow on purpose — names show up in logs, in
/// the `channels` table and in dial paths, and nothing is gained by accepting `..`.
pub fn is_machine_name(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

/// A project path as typed → the absolute path to store. No shell ever sees it (tmux gets it as is),
/// so `~` and `./` are resolved against `home` — the home directory of the machine running the agents.
/// A trailing `/` is dropped.
pub fn absolute_project_path(typed: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    let abs = match typed.strip_prefix('~') {
        Some("") => home.to_string(),
        Some(rest) if rest.starts_with('/') => format!("{home}{rest}"),
        _ if typed.starts_with('/') => typed.to_string(),
        _ => format!("{home}/{}", typed.strip_prefix("./").unwrap_or(typed)),
    };
    match abs.trim_end_matches('/') {
        "" => "/".to_string(),
        p => p.to_string(),
    }
}

/// The answer when a project folder isn't there. Said the same way wherever `pwd` is answered.
pub fn no_such_folder(path: &str, machine: &str) -> String {
    crate::t!(
        "There's no folder `{path}` on *{machine}*. Nothing was changed.",
        "*{machine}* に `{path}` というフォルダがありません。何も変えていません。"
    )
}

// ─── decisions: dedup → gate → decide ────────────────────────────────────────────

/// Loop-breaker threshold — once this many bot posts in a row land in one thread,
/// stop until a person speaks.
pub const LOOP_LIMIT: u64 = 5;

// ─── disposition: the ledger and the Stop contract ──────────────────────────────────────────

/// Ledger of messages delivered but **not yet answered**. thread_key → unanswered list.
///
/// Three states: track / received / disposed.
///
/// ponytail: linear scan over a Vec of a few items per thread. Insertion order matters (pending order is
/// the resend and log order), so no HashMap.
#[derive(Default)]
pub struct Ledger {
    /// Keys that go empty are removed (a key exists = at least one unanswered item).
    by_key: HashMap<ThreadKey, Vec<Inflight>>,
    /// Changed. Only [`Ledger::flush`] on the 500ms tick writes it out — mutating methods just
    /// set this, so there is no path where someone touching the ledger forgets to save and loses it.
    dirty: bool,
}

/// One unanswered item. Listed as `inflight` inside a threads.json entry.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Inflight {
    id: String,
    received: bool,
    /// The envelope that was delivered. **Resending is impossible without it** — for a long time this
    /// held only the id. None before delivery.
    envelope: Option<String>,
    /// Resends used on turn failure. **Kept in the item itself**, so when the item is answered and dropped
    /// the budget goes with it (no separate map to clean up).
    retries: u32,
}

impl Ledger {
    /// Picks up from pending.json (once at startup). **If the ledger were volatile, threads spanning a restart
    /// would drop out of the silence watcher** — knowing no unanswered items, "thinking" would never show again.
    /// The caller decides which threads actually get reloaded with [`Ledger::retain_keys`] (only
    /// those of live agents).
    pub fn load(threads: &Threads) -> Self {
        let by_key = threads
            .entries
            .iter()
            .filter(|(_, e)| !e.inflight.is_empty())
            .filter_map(|(ts, e)| {
                Some((
                    ThreadKey::new(e.channel_id.as_deref()?, ts),
                    e.inflight.clone(),
                ))
            })
            .collect();
        Self {
            by_key,
            dirty: false,
        }
    }

    /// Writes to threads.json if changed. Called on the 500ms tick and once right before exiting.
    ///
    /// **Mirrors the ledger into the entries as is** (removed keys become empty). This included, only
    /// `Threads::save` writes threads.json, so no second write path appears.
    ///
    /// ponytail: granularity is one tick. If the process dies instantly the last 500ms are lost, but
    /// all that is lost is the "waiting for an answer" mark — not the message itself or the delivery record.
    pub fn flush(&mut self, threads: &mut Threads) -> Option<std::io::Result<()>> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        let mut want: HashMap<String, Vec<Inflight>> = self
            .by_key
            .iter()
            .filter_map(|(k, v)| k.split().1.map(|ts| (ts, v.clone())))
            .collect();
        for (ts, e) in threads.entries.iter_mut() {
            let next = want.remove(ts).unwrap_or_default();
            if e.inflight != next {
                e.inflight = next;
            }
        }
        // Keys without an entry are dropped. The entry exists before delivery, so this normally doesn't happen
        for ts in want.keys() {
            LogCtx::default().debug(
                "bridge",
                &format!("ledger: dropping undisposed for an unknown thread tts={ts}"),
            );
        }
        Some(threads.save())
    }

    /// Restore at startup — keep only keys in `keep`. **Pass only threads of live agents.**
    /// Loading a dead thread's unanswered items leaves the watcher waiting forever for a reply that never comes.
    pub fn retain_keys(&mut self, keep: &[ThreadKey]) {
        let before = self.by_key.len();
        self.by_key.retain(|k, _| keep.contains(k));
        self.dirty = self.dirty || self.by_key.len() != before;
    }

    /// Records a delivery. Redelivery resets received for the same id.
    pub fn track(&mut self, key: &ThreadKey, id: &str) {
        let entries = self.by_key.entry(key.clone()).or_default();
        match entries.iter_mut().find(|e| e.id == id) {
            Some(e) => e.received = false,
            None => entries.push(Inflight {
                id: id.to_string(),
                received: false,
                envelope: None,
                retries: 0,
            }),
        }
        self.dirty = true;
    }

    /// Remembers the delivered envelope. `track` runs right after the ack (before the envelope exists),
    /// so the side that built the envelope hands it in.
    pub fn remember_envelope(&mut self, key: &ThreadKey, id: &str, envelope: &str) {
        if let Some(e) = self
            .by_key
            .get_mut(key)
            .and_then(|v| v.iter_mut().find(|e| e.id == id))
        {
            e.envelope = Some(envelope.to_string());
            self.dirty = true;
        }
    }

    /// Unanswered items that can be resent, as `(id, envelope)`. Items without a remembered envelope are skipped (nothing to resend).
    pub fn undisposed(&self, key: &ThreadKey) -> Vec<(String, String)> {
        self.by_key
            .get(key)
            .map(|v| {
                v.iter()
                    .filter_map(|e| e.envelope.clone().map(|env| (e.id.clone(), env)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Spends one resend from the budget. Spends and returns true **only when everyone has some left**. If even
    /// one is out, spends from nobody and returns false (an earlier version returned as soon as it
    /// found one out, leaving partial spends behind).
    pub fn spend_retry(&mut self, key: &ThreadKey, ids: &[String], cap: u32) -> bool {
        let Some(entries) = self.by_key.get_mut(key) else {
            return false;
        };
        let mine = |e: &&mut Inflight| ids.contains(&e.id);
        if entries.iter_mut().filter(mine).count() != ids.len()
            || entries.iter_mut().filter(mine).any(|e| e.retries >= cap)
        {
            return false;
        }
        for e in entries.iter_mut().filter(|e| ids.contains(&e.id)) {
            e.retries += 1;
        }
        self.dirty = true;
        true
    }

    /// Marks unreceived items as received and returns **only the ids newly received** (idempotent — empty the second time).
    /// Drives the 🤖 flip and the received milestone. Received is not answered, so they stay in the ledger.
    pub fn mark_received(&mut self, key: &ThreadKey) -> Vec<String> {
        let Some(entries) = self.by_key.get_mut(key) else {
            return Vec::new();
        };
        let flipped: Vec<String> = entries
            .iter_mut()
            .filter(|e| !e.received)
            .map(|e| {
                e.received = true;
                e.id.clone()
            })
            .collect();
        self.dirty = self.dirty || !flipped.is_empty();
        flipped
    }

    /// Drops the ids a disposition covered from the ledger.
    pub fn disposed(&mut self, key: &ThreadKey, ids: &[String]) {
        let Some(entries) = self.by_key.get_mut(key) else {
            return;
        };
        entries.retain(|e| !ids.contains(&e.id));
        if entries.is_empty() {
            self.by_key.remove(key);
        }
        self.dirty = true;
    }

    /// A disposition without ids clears the whole thread. Returns the ids dropped.
    pub fn dispose_all(&mut self, key: &ThreadKey) -> Vec<String> {
        let dropped: Vec<String> = self
            .by_key
            .remove(key)
            .map(|entries| entries.into_iter().map(|e| e.id).collect())
            .unwrap_or_default();
        self.dirty = self.dirty || !dropped.is_empty();
        dropped
    }

    /// Ids not received yet (non-destructive — so the transcript scan knows what to look for).
    pub fn unreceived(&self, key: &ThreadKey) -> Vec<String> {
        self.by_key
            .get(key)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|e| !e.received)
                    .map(|e| e.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns **only the newly received** ids among those given (idempotent). For receipt via the transcript —
    /// unlike the whole-thread `mark_received`, it marks only what the agent actually read.
    pub fn mark_received_ids(&mut self, key: &ThreadKey, ids: &[String]) -> Vec<String> {
        let Some(entries) = self.by_key.get_mut(key) else {
            return Vec::new();
        };
        let flipped: Vec<String> = entries
            .iter_mut()
            .filter(|e| !e.received && ids.contains(&e.id))
            .map(|e| {
                e.received = true;
                e.id.clone()
            })
            .collect();
        self.dirty = self.dirty || !flipped.is_empty();
        flipped
    }

    /// Thread keys holding unanswered items (`pendingKeys`). Where the restart notice goes.
    pub fn pending_keys(&self) -> Vec<ThreadKey> {
        self.by_key.keys().cloned().collect()
    }

    /// The thread holding this id as unanswered. A delete event sometimes comes without the root,
    /// so this is used to **find the real thread from the deleted ts**.
    pub fn key_of_id(&self, id: &str) -> Option<ThreadKey> {
        self.by_key
            .iter()
            .find(|(_, entries)| entries.iter().any(|e| e.id == id))
            .map(|(key, _)| key.clone())
    }

    /// Unanswered ids (non-destructive — read by the Stop contract).
    pub fn pending(&self, key: &ThreadKey) -> Vec<String> {
        self.by_key
            .get(key)
            .map(|entries| entries.iter().map(|e| e.id.clone()).collect())
            .unwrap_or_default()
    }

    // ── receipt from the transcript ──

    /// The envelope's `message_id` appearing in the transcript = the agent read it. What was sent with
    /// send-keys mid-turn doesn't fire UserPromptSubmit (steering consumption), so this is the only
    /// evidence of receipt.
    ///
    /// The transcript is JSONL — the envelope sits inside a JSON string, so **quotes are escaped**
    /// (measured: `message_id=\"1783500885.490429\"`). So skip the run of separator characters after
    /// `message_id` and match the id (the set `message_id[\\"':=\s]*`).
    /// The plural `message_ids` (= a claim of coverage, not receipt) is rejected by its trailing `s`.
    pub fn find_received_ids(new_bytes: &str, ids: &[String]) -> Vec<String> {
        let sep = |c: char| matches!(c, '\\' | '"' | '\'' | ':' | '=' | ' ' | '\t' | '\n' | '\r');
        ids.iter()
            .filter(|id| {
                new_bytes.match_indices(id.as_str()).any(|(at, _)| {
                    // A digit right after means it is part of another (longer) id
                    !new_bytes[at + id.len()..].starts_with(|c: char| c.is_ascii_digit())
                        && new_bytes[..at]
                            .trim_end_matches(sep)
                            .ends_with("message_id")
                })
            })
            .cloned()
            .collect()
    }
}

/// What the silence watcher's 500ms tick decides for one thread.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum StallAction {
    /// Leave it.
    Nothing,
    /// Show `is thinking…`.
    Fire,
    /// Tear down the watcher (= clear the status). No unanswered items = the conversation is settled.
    Settle,
}

/// The tick's decision. `has_pending` is whether the ledger still has unanswered items.
///
/// **Settled wins** — with no unanswered items, tear down whether silent or already shown. The ledger
/// empties not only through disposition but also through `terminate` (exit / logout / resume), which
/// doesn't clear the status itself. This is the only catch, so missing it leaves the shimmer stuck.
impl StallAction {
    pub fn of(
        has_pending: bool,
        last_activity_ms: u64,
        now_ms: u64,
        shown: bool,
        silence_ms: u64,
    ) -> StallAction {
        if !has_pending {
            return StallAction::Settle;
        }
        if Self::due(last_activity_ms, now_ms, shown, true, silence_ms) {
            return StallAction::Fire;
        }
        StallAction::Nothing
    }

    /// Only the firing condition (when the watcher's timer expires).
    /// The whole tick decision is [`StallAction::of`].
    ///
    /// - `shown` = already shown → never fire twice (idempotent)
    /// - `has_pending` = unanswered items are held. Empty = the conversation is settled, so the watcher
    ///   is not re-armed
    pub fn due(
        last_activity_ms: u64,
        now_ms: u64,
        shown: bool,
        has_pending: bool,
        silence_ms: u64,
    ) -> bool {
        !shown && has_pending && now_ms.saturating_sub(last_activity_ms) >= silence_ms
    }
}

/// Notice that a disposition happened (slack.rs → main). kind is "reply"/"react"/"no_reply"/"edit".
#[derive(Clone, Debug)]
pub struct Disposition {
    pub kind: &'static str,
    pub channel_id: String,
    pub thread_ts: Option<String>,
    pub message_ids: Vec<String>,
    pub session_id: String,
}

/// Whether the agent may end its turn. true means block Stop and push for a disposition.
///
/// - Thread can't be found → nobody to push → let it through
/// - `stop_hook_active` = this stop already follows a re-prompt → re-prompt only once
/// - MCP not ready → **fail-open** (saying "reply" before the disposition tools exist just
///   misses. Anything dropped is left to warm-push / recovery)
impl Disposition {
    pub fn should_block_stop(
        thread_resolved: bool,
        stop_hook_active: bool,
        mcp_ready: bool,
        pending: usize,
    ) -> bool {
        thread_resolved && mcp_ready && !stop_hook_active && pending > 0
    }

    /// The Stop hook's block response. A flat shape, unlike permission's `{decision:{behavior}}`.
    /// `pending` = message_ids not disposed yet. **Always put them in the text** — without them the
    /// agent guesses which ones are left, fires `no_reply` at an id it already answered, and the ledger
    /// doesn't shrink. Since it doesn't shrink, the next Stop blocks again, and all the while
    /// the watcher keeps re-showing `is thinking…` (the shimmer stays even after the answer).
    pub fn block_output(pending: &[String]) -> serde_json::Value {
        let reason = format!(
            "{STOP_BLOCK_REASON} Outstanding message_ids: [{}]",
            pending.join(", ")
        );
        serde_json::json!({ "decision": "block", "reason": reason })
    }
}

/// The text handed to a blocked agent. Explicitly overrides Claude Code's default framing (prose = delivery,
/// outward actions need confirmation). **Keep the wording verbatim.**
pub const STOP_BLOCK_REASON: &str = "You ended your turn without delivering. Your prose streams to Slack live, but it is not the delivered answer — dispose the received message with `reply` (answer), `react` (emoji ack), or `no_reply` (nothing). A reply is your answer, not an outward action: do NOT ask whether to send it. Call reply/react/no_reply (with the message_id) now.";

#[cfg(test)]
mod tests {

    use super::*;
    use crate::chat::deletion_notice;

    #[test]
    fn notice_target_prefers_home_channel() {
        assert!(matches!(NoticeTarget::of(Some("C1"), "U1"), NoticeTarget::Home(c) if c == "C1"));
    }

    #[test]
    fn notice_target_falls_back_to_owner_dm() {
        assert!(matches!(NoticeTarget::of(None, "U1"), NoticeTarget::OwnerDm(u) if u == "U1"));
    }

    #[test]
    fn notice_target_silent_without_either() {
        assert!(matches!(NoticeTarget::of(None, ""), NoticeTarget::None_));
    }

    /// The deletion text doesn't let the agent just reply "it was deleted". The body is quoted,
    /// and cut if too long. Only the deleted id can be looked up from the ledger.
    #[test]
    fn a_deletion_tells_the_worker_to_discard_and_the_ledger_finds_its_thread() {
        let n = deletion_notice("1.2", "  やっぱりやめて  ", false);
        assert!(n.starts_with("[message_deleted] The user DELETED their own message (id 1.2)."));
        assert!(n.contains("Its content was:\n\"\"\"\nやっぱりやめて\n\"\"\""));
        assert!(n.contains("call no_reply and output nothing"));
        assert!(n.ends_with("Never output any text merely stating that the message was deleted."));
        // No body: the wording depends on whether it was an attachment
        assert!(deletion_notice("1.2", "", true).contains("it was a file/attachment upload"));
        assert!(deletion_notice("1.2", "", false).contains("Its text is unavailable."));
        // Cut at 1000 characters
        assert!(deletion_notice("1.2", &"あ".repeat(1500), false).contains("… (truncated)"));

        // A delete event sometimes comes without the root — find the thread from the deleted id
        let mut l = Ledger::default();
        let key = ThreadKey::parse("C1:1.0");
        l.track(&key, "1.2");
        assert_eq!(l.key_of_id("1.2"), Some(key));
        assert_eq!(l.key_of_id("9.9"), None, "答え済み・無関係な id は引けない");
    }

    /// Requests that couldn't be handed over are parked in threads.json and taken out at the next start.
    /// The same message is never queued twice (idempotent on message_id).
    #[test]
    fn undelivered_messages_survive_a_restart_through_threads_json() {
        let mut t = Threads::from_str(r#"{}"#).unwrap();
        let msg = |ts: &str, text: &str| InboundMsg {
            channel: "C1".into(),
            channel_kind: ChannelKind::Channel,
            ts: ts.into(),
            thread_ts: Some("1.0".into()),
            user: Some("U1".into()),
            is_bot: false,
            bot_id: None,
            text: text.into(),
            files: Vec::new(),
            file_paths: vec!["/tmp/a.png".into()],
            file_errors: Vec::new(),
            reaction: None,
            deleted_ts: None,
            edited: None,
        };
        assert!(t.threads_with_pending().is_empty());
        t.enqueue_pending("1.0", "C1", &msg("1.1", "ひとつ目"));
        t.enqueue_pending("1.0", "C1", &msg("1.2", "ふたつ目"));
        t.enqueue_pending("1.0", "C1", &msg("1.1", "ひとつ目(再)"));
        assert_eq!(t.threads_with_pending(), vec!["1.0"], "1スレッドに溜まる");

        // Survives writing out and reading back (= spans a restart)
        let round_tripped = Threads::from_str(&t.to_string_pretty().unwrap()).unwrap();
        let mut t = round_tripped;
        let out = t.drain_pending("1.0");
        assert_eq!(out.len(), 2, "同じ id は二度積まない");
        assert_eq!(out[0].ts, "1.1");
        assert_eq!(out[0].text, "ひとつ目");
        assert_eq!(out[0].thread_ts.as_deref(), Some("1.0"));
        assert_eq!(out[0].file_paths, vec!["/tmp/a.png"], "添付も持ち越す");
        assert!(t.threads_with_pending().is_empty(), "取り出したら消える");
        assert!(t.drain_pending("9.9").is_empty(), "知らないスレッドは空");
    }

    /// Loop breaker — counts consecutive bot posts, stops at the limit, resumes when a person speaks.
    #[test]
    fn the_loop_guard_counts_bot_messages_and_a_human_resumes_the_thread() {
        let mut t = Threads::from_str(r#"{}"#).unwrap();
        assert_eq!(t.bump_bot_streak("1.0"), 0, "知らないスレッドは数えない");
        t.upsert(
            "1.0",
            ThreadEntry {
                channel_id: Some("C1".into()),
                ..Default::default()
            },
        );
        for n in 1..LOOP_LIMIT {
            assert_eq!(t.bump_bot_streak("1.0"), n);
            assert_eq!(t.status_of("1.0"), "active", "上限までは止めない");
        }
        assert_eq!(t.bump_bot_streak("1.0"), LOOP_LIMIT);
        t.pause("1.0");
        assert_eq!(t.status_of("1.0"), "paused");
        assert!(!t.is_active("1.0"), "止まったスレッドは動いていない");

        // A person spoke → reset the count and resume
        assert!(t.reset_bot_streak("1.0"), "止まっていたことを返す");
        assert_eq!(t.status_of("1.0"), "active");
        assert_eq!(t.bump_bot_streak("1.0"), 1, "連続数は 0 に戻っている");
        assert!(!t.reset_bot_streak("1.0"), "止まっていなければ false");
    }

    /// A thread that has spoken even once counts as running **even with no session yet** (user's choice).
    /// A thread that began with a command that doesn't start an agent, like status / usage, still gets
    /// its follow-ups without a mention.
    #[test]
    fn a_thread_is_active_once_it_exists_even_without_a_session() {
        let mut t = Threads::from_str(r#"{}"#).unwrap();
        assert!(!t.is_active("1.0"), "知らないスレッドは動いていない");
        t.upsert(
            "1.0",
            ThreadEntry {
                channel_id: Some("C1".into()),
                ..Default::default()
            },
        );
        assert!(t.is_active("1.0"), "agent_id が無くても動いている扱い");
        // Only pending / paused are exceptions
        for status in ["pending", "paused"] {
            let mut e = ThreadEntry {
                channel_id: Some("C1".into()),
                ..Default::default()
            };
            e.extra
                .insert("status".into(), serde_json::Value::String(status.into()));
            t.upsert("2.0", e);
            assert!(!t.is_active("2.0"), "{status} は動いていない");
        }
    }

    /// access.json has several owners. **The side writing settings must not erase keys only the
    /// Bridge touches** — if that breaks, pool designations and ports silently vanish.
    #[test]
    fn writing_the_settings_keeps_the_keys_only_the_bridge_touches() {
        let dir = StateDir::at(std::env::temp_dir().join("scrs-access-merge-test"));
        let _ = std::fs::remove_file(dir.join("access.json"));

        // Bridge side — put the endpoints and pool designations
        let port = dir.remembered_port("hook", || 8791);
        let token = dir.remembered_token("hook");
        let mut pools = Pools::load(&dir);
        pools.nominate("/repo", "sid-1");
        pools.save().unwrap();

        // Fleet side — reads knowing nothing, adds owner, writes the whole thing back
        let mut access = Access::load(&dir);
        access.owner = "U1".into();
        access.save(&dir).unwrap();

        // Both the settings and the Bridge-only keys survive
        let after = Access::load(&dir);
        assert_eq!(after.owner, "U1");
        assert_eq!(
            dir.remembered_port("hook", || panic!("再割り当てされた")),
            port
        );
        assert_eq!(dir.remembered_token("hook"), token);
        assert_eq!(Pools::load(&dir).session_of("/repo"), Some("sid-1"));

        // The other direction — owner survives the Bridge changing a designation
        let mut pools = Pools::load(&dir);
        pools.nominate("/repo", "sid-2");
        pools.save().unwrap();
        assert_eq!(Access::load(&dir).owner, "U1");
        assert_eq!(Pools::load(&dir).session_of("/repo"), Some("sid-2"));

        let _ = std::fs::remove_file(dir.join("access.json"));
    }

    #[test]
    fn lifecycle_resets_on_spawn_and_counts() {
        let mut lc = Lifecycle::new();
        let m = lc.record(&ThreadKey::parse("C1:1.0"), "spawn", 1000);
        assert_eq!((m.seq, m.since_prev_ms, m.since_spawn_ms), (1, 0, 0));
        let m = lc.record(&ThreadKey::parse("C1:1.0"), "session_start", 1450);
        assert_eq!((m.seq, m.since_prev_ms, m.since_spawn_ms), (2, 450, 450));
        let m = lc.record(&ThreadKey::parse("C1:1.0"), "user_prompt", 2000);
        assert_eq!((m.seq, m.since_prev_ms, m.since_spawn_ms), (3, 550, 1000));
        // spawn resets the timeline for the same key
        let m = lc.record(&ThreadKey::parse("C1:1.0"), "spawn", 5000);
        assert_eq!((m.seq, m.since_prev_ms, m.since_spawn_ms), (1, 0, 0));
        // For an unknown key the first event is the origin
        let m = lc.record(&ThreadKey::parse("C2:2.0"), "session_start", 100);
        assert_eq!((m.seq, m.since_spawn_ms), (1, 0));
    }

    #[test]
    fn milestone_message_matches_current_format() {
        let m = Milestone {
            seq: 3,
            since_prev_ms: 550,
            since_spawn_ms: 1000,
        };
        assert_eq!(
            m.message(&ThreadKey::parse("C1:1.0"), "user_prompt"),
            "thread=C1:1.0 #3 user_prompt +550ms (since spawn +1000ms)"
        );
    }

    #[test]
    fn threads_unknown_fields_survive_roundtrip() {
        let src = r#"{"171.001":{"agent_id":"sid-1","channel_id":"C1","topic":"直近の話題","future_field":{"x":1}}}"#;
        let mut reg = Threads::from_str(src).unwrap();
        reg.upsert("172.002", ThreadEntry::new("C2", "sid-2"));
        let out = reg.to_string_pretty().unwrap();
        assert!(out.contains("future_field"), "unknown field lost: {out}");
        assert!(out.contains("sid-2"));
        // topic reads back typed and survives a write-back (it is the link text in status)
        assert_eq!(
            reg.get("171.001").unwrap().topic.as_deref(),
            Some("直近の話題")
        );
        assert!(out.contains("直近の話題"), "topic lost: {out}");
    }

    #[test]
    fn the_session_id_is_read_and_written_under_the_name_the_current_bot_uses() {
        // This is all switchover day needs — drop production's threads.json in as is, and each thread
        // picks up its own session with `--resume`
        let reg =
            Threads::from_str(r#"{"171.001":{"session_id":"S-bun","channel_id":"C1"}}"#).unwrap();
        assert_eq!(
            reg.get("171.001").unwrap().agent_id.as_deref(),
            Some("S-bun")
        );
        assert_eq!(reg.find_by_session("S-bun").unwrap().0, "171.001");
        let out = reg.to_string_pretty().unwrap();
        assert!(
            out.contains(r#""session_id": "S-bun""#),
            "書き戻しも現行の名前: {out}"
        );
        assert!(!out.contains("agent_id"), "独自の名前は残さない: {out}");
        // dev state written under the custom name also reads (alias)
        let old =
            Threads::from_str(r#"{"171.001":{"agent_id":"S-rs","channel_id":"C1"}}"#).unwrap();
        assert_eq!(
            old.get("171.001").unwrap().agent_id.as_deref(),
            Some("S-rs")
        );
    }

    #[test]
    fn threads_find_by_session() {
        let reg =
            Threads::from_str(r#"{"171.001":{"agent_id":"sid-1","channel_id":"C1"}}"#).unwrap();
        let (ts, e) = reg.find_by_session("sid-1").unwrap();
        assert_eq!(ts, "171.001");
        assert_eq!(e.channel_id.as_deref(), Some("C1"));
    }

    /// Choosing the keys to reload into the unanswered ledger at startup. **Only live agents** —
    /// loading a dead thread leaves the silence watcher waiting forever with nobody to answer.
    #[test]
    fn surviving_keeps_only_threads_whose_worker_is_alive() {
        let reg = Threads::from_str(
            r#"{
              "171.001":{"agent_id":"sid-live","channel_id":"C1"},
              "171.002":{"agent_id":"sid-dead","channel_id":"C1"},
              "171.003":{"channel_id":"C1"}
            }"#,
        )
        .unwrap();
        let keys: Vec<ThreadKey> = ["C1:171.001", "C1:171.002", "C1:171.003", "C1:171.404", "C1"]
            .iter()
            .map(|k| ThreadKey::parse(k))
            .collect();

        let kept = reg.surviving(&keys, |sid| sid == "sid-live");

        assert_eq!(
            kept.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
            vec!["C1:171.001"],
            "生きている1本だけが残る(死んだ / セッション未割当 / 台帳に無い / スレッド無しは落ちる)"
        );
    }

    #[test]
    fn access_loads_real_shape_and_roundtrips() {
        let src = r#"{"owner":"U1","allowedBots":["B1"],"routes":{"C1":{"repo_path":"/x","zzz":1}},"homeChannel":"C9"}"#;
        let a = Access::from_str(src).unwrap();
        assert_eq!(a.owner, "U1");
        assert_eq!(a.routes["C1"].repo_path.as_deref(), Some("/x"));
        assert_eq!(a.home_channel.as_deref(), Some("C9"));
        let out = a.to_string_pretty().unwrap();
        assert!(out.contains("zzz"), "unknown route field lost: {out}"); // unknown fields preserved
        assert!(
            out.contains("allowedBots"),
            "unknown top-level field lost: {out}"
        );
    }

    #[test]
    fn access_ack_reaction_roundtrips() {
        let src = r#"{"owner":"U1","ackReaction":"spiral_note_pad","zzz":1}"#;
        let a = Access::from_str(src).unwrap();
        assert_eq!(a.ack_reaction.as_deref(), Some("spiral_note_pad"));
        let out = a.to_string_pretty().unwrap();
        assert!(out.contains("ackReaction") && out.contains("zzz"), "{out}");
    }

    #[test]
    fn access_mutations_are_pure_and_verbatim() {
        let mut prev = Access::default();
        prev.owner = "U1".into();
        let (a, msg, warns) = prev
            .apply(AccessOp::SetRepo {
                channel: "C1".into(),
                path: "/repo".into(),
            })
            .unwrap();
        assert_eq!(a.routes["C1"].repo_path.as_deref(), Some("/repo"));
        assert_eq!(msg, "<#C1> now uses the project directory `/repo`.");
        assert!(warns.is_empty());
        assert_eq!(prev.routes.len(), 0); // prev is unchanged
        // Relative paths are rejected
        assert!(
            prev.apply(AccessOp::SetRepo {
                channel: "C1".into(),
                path: "rel".into()
            })
            .is_err()
        );
        // warm: a channel without a repo gets a warning
        let (a2, msg2, warns2) = prev
            .apply(AccessOp::SetWarm {
                channel: "C2".into(),
                on: true,
            })
            .unwrap();
        assert_eq!(a2.routes["C2"].warm, Some(true));
        assert!(msg2.contains("started ahead of time"));
        assert_eq!(warns2.len(), 1);
        // bot allow doesn't duplicate
        let (a3, _, _) = prev.apply(AccessOp::BotAllow("B9".into())).unwrap();
        let (a4, _, _) = a3.apply(AccessOp::BotAllow("B9".into())).unwrap();
        assert_eq!(a4.allowed_bots, vec!["B9"]);
        let (_, msg5, _) = a4.apply(AccessOp::BotRemove("B0".into())).unwrap();
        assert_eq!(msg5, "B0 is not an allowed bot.");
    }

    #[test]
    fn access_typed_fields_roundtrip_with_unknowns() {
        let src = r#"{"owner":"U1","allowedBots":["B1"],"routes":{"C1":{"repo_path":"/r","warm":false,"zzz":1}},"yyy":2}"#;
        let a = Access::from_str(src).unwrap();
        assert_eq!(a.allowed_bots, vec!["B1"]);
        assert_eq!(a.routes["C1"].warm, Some(false));
        let out = a.to_string_pretty().unwrap();
        for k in ["allowedBots", "zzz", "yyy", "warm"] {
            assert!(out.contains(k), "{k}");
        }
    }

    #[test]
    fn repo_path_resolves_like_spawn() {
        let mut a = Access::default();
        a.routes.insert(
            "C1".into(),
            Route {
                repo_path: Some("/r".into()),
                ..Default::default()
            },
        );
        assert_eq!(a.repo_path("C1", "/home"), ("/r".into(), false));
        assert_eq!(a.repo_path("C2", "/home"), ("/home".into(), true));
    }

    #[test]
    fn pool_targets_empty_without_owner() {
        let mut a = Access::default();
        a.owner = String::new();
        assert!(a.pool_targets("/home", "me").is_empty());
    }

    #[test]
    fn pool_targets_always_includes_home_plus_distinct_repos() {
        let mut a = Access::default();
        a.owner = "U1".to_string();
        a.routes.insert(
            "C1".into(),
            Route {
                repo_path: Some("/repo/a".into()),
                warm: None,
                ..Default::default()
            },
        );
        a.routes.insert(
            "C2".into(),
            Route {
                repo_path: Some("/repo/a".into()),
                warm: Some(false),
                ..Default::default()
            },
        );
        a.routes.insert(
            "C3".into(),
            Route {
                repo_path: Some("/repo/b".into()),
                warm: Some(true),
                ..Default::default()
            },
        );
        let pools = a.pool_targets("/home", "me");
        let cwds: Vec<&str> = pools.iter().map(|p| p.as_str()).collect();
        assert!(cwds.contains(&"/home"));
        assert!(
            cwds.contains(&"/repo/a"),
            "opt-in on C1 stocks the pool even though C2 opted out"
        );
        assert!(cwds.contains(&"/repo/b"));
        assert_eq!(
            cwds.len(),
            3,
            "distinct repo_path collapses C1/C2 into one pool"
        );
    }

    #[test]
    fn pool_targets_excludes_repo_when_every_route_opts_out() {
        let mut a = Access::default();
        a.owner = "U1".to_string();
        a.routes.insert(
            "C1".into(),
            Route {
                repo_path: Some("/repo/c".into()),
                warm: Some(false),
                ..Default::default()
            },
        );
        let pools = a.pool_targets("/home", "me");
        assert!(!pools.iter().any(|p| p == "/repo/c"));
    }

    /// The gateway keeps a copy of every channel's folder so `channels` can show it. Those folders live
    /// on the machines that handle them, and warming an agent in one here would start it in the wrong place
    /// (seen on a real gateway: it tried to warm the Mac's repos).
    #[test]
    fn pool_targets_skip_folders_that_belong_to_another_machine() {
        let mut a = Access::default();
        a.owner = "U1".to_string();
        a.routes.insert(
            "C_MINE".into(),
            Route { repo_path: Some("/repo/mine".into()), ..Default::default() },
        );
        a.routes.insert(
            "C_THEIRS".into(),
            Route {
                repo_path: Some("/repo/theirs".into()),
                bridge: Some("other".into()),
                ..Default::default()
            },
        );
        let pools = a.pool_targets("/home", "me");
        assert!(pools.iter().any(|p| p == "/repo/mine"), "{pools:?}");
        assert!(!pools.iter().any(|p| p == "/repo/theirs"), "{pools:?}");
    }

    #[test]
    fn pool_key_is_stable_and_repo_scoped() {
        assert_eq!(PoolKey::of_cwd("/repo/a"), PoolKey::of_cwd("/repo/a"));
        assert_ne!(PoolKey::of_cwd("/repo/a"), PoolKey::of_cwd("/repo/b"));
        assert!(PoolKey::of_cwd("/repo/a").as_str().starts_with("repo-"));
    }

    /// A grant is written **only when a person clicks the button**. Pins that it round-trips under
    /// the `allowedTools` key without dragging unknown fields along.
    #[test]
    fn tool_grants_round_trip_under_the_current_key() {
        // Thread side: lands in the threads.json entry
        let mut t = Threads::default();
        t.grant_thread_tool("1.0", "Bash");
        t.grant_thread_tool("1.0", "Bash"); // clicking twice doesn't add another
        t.grant_thread_tool("1.0", "Edit");
        assert!(t.thread_tool_allowed("1.0", "Bash"));
        assert!(t.thread_tool_allowed("1.0", "Edit"));
        assert!(!t.thread_tool_allowed("1.0", "Write"));
        assert!(
            !t.thread_tool_allowed("2.0", "Bash"),
            "別スレッドには効かない"
        );
        let json = t.to_string_pretty().unwrap();
        assert!(json.contains("\"allowedTools\""), "{json}");
        assert_eq!(
            json.matches("\"Bash\"").count(),
            1,
            "重複して積まれている: {json}"
        );

        // Channel side: lands in the access.json route. Unknown fields are left alone
        let mut a = Access::from_str(
            r#"{"owner":"U1","routes":{"C1":{"repo_path":"/r","futureThing":{"k":1}}}}"#,
        )
        .unwrap();
        a.grant_channel_tool("C1", "Bash");
        assert!(a.channel_tool_allowed("C1", "Bash"));
        assert!(!a.channel_tool_allowed("C2", "Bash"));
        let json = a.to_string_pretty().unwrap();
        assert!(json.contains("\"allowedTools\""), "{json}");
        assert!(
            json.contains("futureThing"),
            "未知フィールドが消えた: {json}"
        );
    }

    #[test]
    fn should_claim_pool_only_for_brand_new_threads() {
        assert!(Threads::should_claim_pool(None));
        let e = ThreadEntry {
            agent_id: Some("s".into()),
            ..Default::default()
        };
        assert!(!Threads::should_claim_pool(Some(&e)));
    }

    #[test]
    fn pool_assignment_entry_carries_repo_and_channel() {
        let e = ThreadEntry::for_pool_assignment("C1", "/repo/a", Some("t".into()));
        assert_eq!(e.channel_id.as_deref(), Some("C1"));
        assert_eq!(e.repo_path.as_deref(), Some("/repo/a"));
        assert_eq!(e.topic.as_deref(), Some("t"));
        // The caller puts claimed.session_id into agent_id — this is just the shell
        assert_eq!(e.agent_id, None);
    }

    #[test]
    fn pool_needs_launch_rules() {
        assert!(PoolStatus::needs_launch(None));
        assert!(!PoolStatus::needs_launch(Some(PoolStatus {
            present: true,
            gave_up: false
        })));
        assert!(!PoolStatus::needs_launch(Some(PoolStatus {
            present: false,
            gave_up: true
        })));
        // A given-up pool agent is **shut down and removed**, so present is always false from then on. Looking only at
        // "not there" would respawn it; the line above is the stopper / refill after a claim goes through below
        assert!(PoolStatus::needs_launch(Some(PoolStatus {
            present: false,
            gave_up: false
        })));
    }

    #[test]
    fn pool_give_up_after_timeout() {
        assert!(!PoolStatus::should_give_up(1_000, 1_000 + 49_999, 50_000));
        assert!(PoolStatus::should_give_up(1_000, 1_000 + 50_001, 50_000));
    }

    #[test]
    fn access_missing_file_is_fail_closed() {
        let a = Access::load(&StateDir::at("/nonexistent-dir-3f9"));
        assert_eq!(a.owner, "", "missing access.json must serve nobody");
    }

    /// The designation stays in the file — this is what stops "cut a new pool session on every restart".
    #[test]
    fn pool_nominations_survive_a_reload() {
        let dir = StateDir::at(std::env::temp_dir().join(format!("scpool-{}", std::process::id())));
        std::fs::create_dir_all(dir.path()).unwrap();
        let _ = std::fs::remove_file(dir.join("pools.json"));

        let mut p = Pools::load(&dir);
        assert_eq!(p.session_of("/repo/a"), None, "空のうちは指名なし");
        p.nominate("/repo/a", "sid-a");
        p.nominate("/home/u", "sid-h");
        p.save().unwrap();

        // Same as another process starting — the designation reads back as is
        let again = Pools::load(&dir);
        assert_eq!(again.session_of("/repo/a"), Some("sid-a"));
        assert_eq!(again.session_of("/home/u"), Some("sid-h"));
        assert_eq!(
            again.rows(),
            vec![
                ("/home/u".to_string(), "sid-h".to_string()),
                ("/repo/a".to_string(), "sid-a".to_string()),
            ],
            "キー順は安定(BTreeMap)"
        );
    }

    /// Graduation (a claim) drops by cwd; a session ending drops by session_id.
    #[test]
    fn pool_nominations_are_released_both_ways() {
        let mut p = Pools::from_str(r#"{"/repo/a":"sid-a","/repo/b":"sid-b"}"#).unwrap();
        assert_eq!(p.release("/repo/a").as_deref(), Some("sid-a"));
        assert_eq!(p.session_of("/repo/a"), None);
        assert_eq!(p.release_session("sid-b").as_deref(), Some("/repo/b"));
        assert!(p.rows().is_empty());
        // Dropping an unknown one doesn't break anything
        assert_eq!(p.release("/repo/zzz"), None);
        assert_eq!(p.release_session("sid-zzz"), None);
    }

    /// A **corrupt** (not just unknown-field) pools.json starts empty (doesn't block startup).
    #[test]
    fn a_broken_pools_file_starts_empty() {
        assert!(Pools::from_str("{ not json").is_err());
        let p = Pools::load(&StateDir::at("/nonexistent-dir-3f9"));
        assert!(p.rows().is_empty());
        // There is a path, but that doesn't mean save can't fail (no parent). The caller logs a save failure
        assert!(p.save().is_err());
    }

    #[test]
    fn pool_restore_decides_by_configuration_and_liveness() {
        // A live pool agent is adopted (one the previous Bridge left running on exit)
        assert_eq!(PoolRestore::decide(true, true), PoolRestore::Adopt);
        // A dead one keeps its designation and is restarted with `--resume`
        assert_eq!(PoolRestore::decide(true, false), PoolRestore::Respawn);
        // A cwd no longer in the pool targets is dropped, alive or not
        assert_eq!(PoolRestore::decide(false, true), PoolRestore::Discard);
        assert_eq!(PoolRestore::decide(false, false), PoolRestore::Discard);
    }

    #[test]
    fn ledger_tracks_receives_and_disposes() {
        let mut l = Ledger::default();
        l.track(&ThreadKey::parse("C1:1.0"), "1.1");
        l.track(&ThreadKey::parse("C1:1.0"), "1.2");
        assert_eq!(l.pending(&ThreadKey::parse("C1:1.0")), vec!["1.1", "1.2"]);
        // Keys holding unanswered items = where the restart notice goes (keys that go empty are removed)
        assert_eq!(l.pending_keys(), vec!["C1:1.0"]);
        // mark_received returns only the newly received (empty the second time = idempotent)
        assert_eq!(
            l.mark_received(&ThreadKey::parse("C1:1.0")),
            vec!["1.1", "1.2"]
        );
        assert!(l.mark_received(&ThreadKey::parse("C1:1.0")).is_empty());
        // Received but still unanswered — stays in pending (received and answered are different states)
        assert_eq!(l.pending(&ThreadKey::parse("C1:1.0")).len(), 2);
        l.disposed(&ThreadKey::parse("C1:1.0"), &["1.1".to_string()]);
        assert_eq!(l.pending(&ThreadKey::parse("C1:1.0")), vec!["1.2"]);
        // A disposition without ids clears the whole thread
        assert_eq!(l.dispose_all(&ThreadKey::parse("C1:1.0")), vec!["1.2"]);
        assert!(l.pending(&ThreadKey::parse("C1:1.0")).is_empty());
        assert!(
            l.pending_keys().is_empty(),
            "全消化した鍵は予告先に残らない"
        );
        // Redelivery resets received
        l.track(&ThreadKey::parse("C1:1.0"), "1.3");
        l.mark_received(&ThreadKey::parse("C1:1.0"));
        l.track(&ThreadKey::parse("C1:1.0"), "1.3");
        assert_eq!(l.mark_received(&ThreadKey::parse("C1:1.0")), vec!["1.3"]);
    }

    /// The two preconditions for resending. Items without a remembered envelope are not resend candidates (nothing to send).
    /// The budget is **per message** and is spent only when all have some — if even one is out, nobody is charged.
    #[test]
    fn ledger_remembers_envelopes_and_spends_retry_budget_atomically() {
        let key = ThreadKey::parse("C1:1.0");
        let mut l = Ledger::default();
        l.track(&key, "m1");
        l.track(&key, "m2");
        assert!(
            l.undisposed(&key).is_empty(),
            "封筒が無ければ再送候補に出ない"
        );

        l.remember_envelope(&key, "m1", "envelope-1");
        assert_eq!(
            l.undisposed(&key),
            vec![("m1".to_string(), "envelope-1".to_string())]
        );

        // m2 has no remembered envelope, so spending the budget for both together fails
        let both = ["m1".to_string(), "m2".to_string()];
        l.remember_envelope(&key, "m2", "envelope-2");
        assert!(l.spend_retry(&key, &both, 1));
        assert!(!l.spend_retry(&key, &both, 1), "2回目は予算切れ");

        // The budget goes away with the item — another message starts from full
        l.disposed(&key, &both);
        l.track(&key, "m3");
        l.remember_envelope(&key, "m3", "envelope-3");
        assert!(l.spend_retry(&key, &["m3".to_string()], 1));

        // Don't spend if an id not in the ledger is mixed in (counts don't match = a broken precondition)
        assert!(!l.spend_retry(&key, &["m3".to_string(), "nope".to_string()], 9));
    }

    /// The ledger spans Bridge processes (inside threads.json entries). It is written only when something
    /// changed, and the caller narrows the keys to reload (only threads of live agents).
    #[test]
    fn ledger_round_trips_through_threads_json_and_keeps_only_the_kept_keys() {
        let dir = StateDir::at(std::env::temp_dir().join(format!("scled-{}", std::process::id())));
        let (live, dead) = (ThreadKey::parse("C1:1.0"), ThreadKey::parse("C1:2.0"));
        // Where the ledger lives. An entry without channel_id can't form a key, so always give it one
        let mut threads = Threads::load(&dir);
        for ts in ["1.0", "2.0"] {
            threads.upsert(ts, ThreadEntry::new("C1", "sid-1"));
        }
        let mut l = Ledger::load(&threads);
        assert!(l.flush(&mut threads).is_none(), "変更が無ければ書かない");

        l.track(&live, "m1");
        l.remember_envelope(&live, "m1", "envelope-1");
        l.track(&live, "m2");
        l.mark_received(&live);
        l.track(&dead, "m9");
        assert!(l.spend_retry(&live, &["m1".to_string()], 2));
        l.flush(&mut threads).unwrap().unwrap();
        assert!(
            l.flush(&mut threads).is_none(),
            "2回目は dirty が下りている"
        );

        // Successor process: load only the live one, drop dead
        let mut next = Ledger::load(&Threads::load(&dir));
        assert_eq!(next.pending_keys().len(), 2);
        next.retain_keys(std::slice::from_ref(&live));
        assert_eq!(next.pending(&live), vec!["m1", "m2"]);
        assert!(next.pending(&dead).is_empty());
        // Envelope, receipt and used budget all carry over (resend preconditions are the same after restore)
        assert_eq!(
            next.undisposed(&live),
            vec![("m1".to_string(), "envelope-1".to_string())]
        );
        assert!(next.unreceived(&live).is_empty(), "受領済みは受領のまま");
        assert!(
            !next.spend_retry(&live, &["m1".to_string()], 1),
            "予算は使用済み"
        );

        std::fs::remove_dir_all(dir.path()).ok();
    }

    #[test]
    fn ledger_marks_only_the_ids_the_worker_actually_read() {
        let mut l = Ledger::default();
        l.track(&ThreadKey::parse("C1:1.0"), "1.1");
        l.track(&ThreadKey::parse("C1:1.0"), "1.2");
        assert_eq!(
            l.unreceived(&ThreadKey::parse("C1:1.0")),
            vec!["1.1", "1.2"]
        );
        // Only 1.2 appeared in the transcript → 1.1 is still unreceived
        assert_eq!(
            l.mark_received_ids(&ThreadKey::parse("C1:1.0"), &["1.2".to_string()]),
            vec!["1.2"]
        );
        assert_eq!(l.unreceived(&ThreadKey::parse("C1:1.0")), vec!["1.1"]);
        // Idempotent — empty the second time
        assert!(
            l.mark_received_ids(&ThreadKey::parse("C1:1.0"), &["1.2".to_string()])
                .is_empty()
        );
        // Received but still unanswered — stays in pending
        assert_eq!(l.pending(&ThreadKey::parse("C1:1.0")).len(), 2);
        assert!(
            l.mark_received_ids(&ThreadKey::parse("C9:9.9"), &["1.1".to_string()])
                .is_empty()
        );
    }

    #[test]
    fn transcript_scan_finds_the_envelope_id_escaped_in_jsonl() {
        let ids = vec!["1783500885.490429".to_string(), "1.2".to_string()];
        // A real transcript (JSONL) puts the envelope in a JSON string = quotes are escaped
        let real = r#"{"type":"user","message":{"content":"<channel source=\"plugin:agentgw:agentgw\" channel_id=\"C1\" message_id=\"1783500885.490429\" user=\"U1\">\nhi\n</channel>"}}"#;
        assert_eq!(
            Ledger::find_received_ids(real, &ids),
            vec!["1783500885.490429"]
        );
        // Also catches the raw unescaped form (logs and other paths)
        assert_eq!(
            Ledger::find_received_ids(r#"message_id="1.2""#, &ids),
            vec!["1.2"]
        );
        // message_ids (plural) is a claim of coverage, not receipt
        assert!(Ledger::find_received_ids(r#"message_ids=[\"1.2\"]"#, &ids).is_empty());
        // A mere mention isn't receipt either
        assert!(Ledger::find_received_ids("the message 1.2 arrived", &ids).is_empty());
        // Don't grab part of another id (1.2 is inside 1.25 but is a different id)
        assert!(Ledger::find_received_ids(r#"message_id=\"1.25\""#, &ids).is_empty());
        assert!(Ledger::find_received_ids("", &ids).is_empty());
    }

    #[test]
    fn stop_block_truth_table() {
        assert!(Disposition::should_block_stop(true, false, true, 1)); // unanswered → block
        assert!(!Disposition::should_block_stop(false, false, true, 1)); // unknown thread → let through
        assert!(!Disposition::should_block_stop(true, true, true, 1)); // already re-prompted → only once
        assert!(!Disposition::should_block_stop(true, false, false, 1)); // MCP not ready → fail-open, let through
        assert!(!Disposition::should_block_stop(true, false, true, 0)); // all answered → let through
    }

    #[test]
    fn stall_watchdog_truth_table() {
        const S: u64 = 5_000; // default silence window
        // Fires exactly at expiry (the same boundary as a setTimeout of the silence window)
        assert!(StallAction::due(0, S, false, true, S));
        assert!(
            !StallAction::due(0, S - 1, false, true, S),
            "1ms 手前ではまだ黙る"
        );
        assert!(
            !StallAction::due(0, S, true, true, S),
            "出している間は二度撃たない"
        );
        assert!(
            !StallAction::due(0, S, false, false, S),
            "未応答が無い = 決着済み。応答待ちを蒸し返さない"
        );
        // Activity resets the clock to 0 (the caller sets last_activity to now)
        assert!(!StallAction::due(S, S, false, true, S));
        // A clock going backwards doesn't panic, it stays quiet (saturating_sub)
        assert!(!StallAction::due(S * 2, S, false, true, S));
    }

    #[test]
    fn stall_tick_action_table() {
        use StallAction::{Fire, Nothing, Settle};
        const S: u64 = 5_000;
        // Unanswered + silence expired → show
        assert_eq!(StallAction::of(true, 0, S, false, S), Fire);
        // Unanswered + still talking / already shown → leave it
        assert_eq!(StallAction::of(true, S, S, false, S), Nothing);
        assert_eq!(StallAction::of(true, 0, S, true, S), Nothing);
        // No unanswered items = settled → tear down. **Even when silent / already shown / with recent activity.**
        // This is the only catch for the ledger emptying through terminate (exit / logout / resume),
        // so returning anything but Settle here leaves the shimmer stuck
        assert_eq!(StallAction::of(false, 0, S, true, S), Settle);
        assert_eq!(StallAction::of(false, 0, S, false, S), Settle);
        assert_eq!(
            StallAction::of(false, S, S, false, S),
            Settle,
            "直前の活動より決着が優先"
        );
    }

    #[test]
    fn stop_block_output_shape() {
        let v = Disposition::block_output(&["1.1".to_string(), "2.2".to_string()]);
        assert_eq!(v["decision"], "block");
        let reason = v["reason"].as_str().unwrap();
        assert!(reason.starts_with("You ended your turn without delivering."));
        // Name the ids still left — without it the agent misses and the ledger doesn't shrink
        assert!(
            reason.ends_with("Outstanding message_ids: [1.1, 2.2]"),
            "{reason}"
        );
    }

    #[test]
    fn ack_emoji_defaults_to_eyes() {
        let mut access = Access::default();
        assert_eq!(access.ack_emoji(), "eyes");
        access.ack_reaction = Some("spiral_note_pad".into());
        assert_eq!(access.ack_emoji(), "spiral_note_pad");
    }

    #[test]
    fn project_paths_become_absolute_under_home() {
        for (typed, want) in [
            ("/srv/app", "/srv/app"),
            ("/srv/app/", "/srv/app"),
            ("~", "/home/me"),
            ("~/dev/x", "/home/me/dev/x"),
            ("./dev/x", "/home/me/dev/x"),
            ("/", "/"),
        ] {
            assert_eq!(absolute_project_path(typed, "/home/me"), want, "{typed}");
        }
    }

    #[test]
    fn machine_names_are_narrow() {
        for ok in ["dock", "tyo-mpv5l", "a.b_c", "9"] {
            assert!(is_machine_name(ok), "{ok}");
        }
        for bad in ["", "-x", ".x", "a b", "の使い方", "a/b", "<@U1>"] {
            assert!(!is_machine_name(bad), "{bad}");
        }
    }
}
