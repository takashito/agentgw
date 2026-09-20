//! Commands aimed at agentgw itself: status / pwd and the Owner commands
//! (warm / set-home / allow-bot / remove-bot).

use super::PwdMode;
use crate::agent::SessionId;
use crate::chat::InboundMsg;
use crate::bridge::state::{self as bridge};
use crate::log::LogCtx;
use crate::chat::ThreadKey;
use crate::bridge::{Bridge, Host};
use crate::chat::slack;
use std::collections::HashMap;

impl Bridge {
    /// `status`. Lists only agents that are **actually running** —
    /// whether one is alive is answered solely by the claude pid in tmux (the real thing, not memory).
    /// Slack lookups (permalink / channel name) happen outside the select loop.
    pub(super) fn user_status(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let mut threads: Vec<StatusThread> = Vec::new();
        // ponytail: one tmux list-windows per thread (claude_pid_of calls it each time).
        // Fine at dev thread counts — if it grows to hundreds, enumerate the windows
        // once and filter first against the set of live window names
        for (tts, e) in &self.threads.entries {
            let (Some(channel_id), Some(sid)) = (e.channel_id.clone(), e.agent_id.as_deref())
            else {
                continue;
            };
            let h = self.workers.warm(sid);
            let window = SessionId::from(sid.to_string()).window_name();
            let alive = self
                .deps.agent
                .pid_of(h.and_then(|h| h.window_id.as_deref()), &window);
            if alive.is_none() {
                continue;
            }
            // Idle is measured from the transcript mtime = the last time it **actually worked**.
            // An inherited agent that has not sent a hook yet is looked up by session id
            let remembered = h.and_then(|h| h.transcript_path.clone());
            let last_activity_ms = self
                .deps.agent
                .last_activity_ms(remembered.as_deref(), sid)
                .unwrap_or(0);
            threads.push(StatusThread {
                channel_id,
                thread_ts: tts.clone(),
                last_activity_ms,
                permalink: None,
                topic: e.topic.clone(),
            });
        }
        ctx.debug(
            "bridge",
            &format!(
                "status: {} live thread(s) of {} recorded",
                threads.len(),
                self.threads.entries.len()
            ),
        );
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        // $HOME is for folding paths into `~`. Host::home() is
        // "where an agent without a route starts", a different thing, so don't mix them here
        let home = std::env::var("HOME").unwrap_or_default();
        // `self` cannot be carried into a 'static spawn — build the whole picture here and move it in.
        // Count only **usable** pool entries (starting ones "don't exist yet"; given-up slots are already gone)
        let waiting: Vec<(String, u64)> = self
            .workers
            .pool_entries()
            .into_iter()
            .filter(|(_, _, ready)| *ready)
            .map(|(cwd, sid, _)| {
                let idle = self.deps.agent.last_activity_ms(None, &sid).unwrap_or(0);
                (cwd, idle)
            })
            .collect();
        // Every channel handed to this machine, plus any that has something running (DMs have no route)
        let mut ids: Vec<String> = self
            .access
            .routes
            .iter()
            .filter(|(_, r)| r.bridge.as_deref().is_none_or(|b| b == self.machine_name))
            .map(|(ch, _)| ch.clone())
            .collect();
        for t in &threads {
            if !ids.contains(&t.channel_id) {
                ids.push(t.channel_id.clone());
            }
        }
        let agent_home = Host::home();
        let mut channels: Vec<StatusChannel> = ids
            .into_iter()
            .map(|channel_id| {
                let folder = self.access.repo_path(&channel_id, &agent_home).0;
                let mine: Vec<StatusThread> = threads
                    .iter()
                    .filter(|t| t.channel_id == channel_id)
                    .cloned()
                    .collect();
                StatusChannel {
                    warm_on: self
                        .access
                        .routes
                        .get(&channel_id)
                        .and_then(|r| r.warm)
                        .unwrap_or(false),
                    warm: waiting
                        .iter()
                        .filter(|(cwd, _)| *cwd == folder)
                        .map(|(_, idle)| *idle)
                        .collect(),
                    threads: mine,
                    folder,
                    name: None,
                    channel_id,
                }
            })
            .collect();
        // Busy channels first, then by id — the same shape every time it is asked
        channels.sort_by(|a, b| {
            b.threads
                .len()
                .cmp(&a.threads.len())
                .then(a.channel_id.cmp(&b.channel_id))
        });
        // Where we sit in the fleet. Read here (the spawn below can't hold `self`): the gateway asks its
        // own link server, a machine reads the link it keeps
        let machine = self.machine_name.clone();
        let role = match (&self.machines_now, &self.link) {
            (Some(connected), _) => {
                let live = connected();
                // Machines we know of from the assignments too, so one that is away still shows (in red)
                let mut names: Vec<String> = live.clone();
                for id in self.access.routes.values().filter_map(|r| r.bridge.clone()) {
                    if id != machine && !names.contains(&id) {
                        names.push(id);
                    }
                }
                names.sort();
                StatusRole::Gateway {
                    machines: names
                        .into_iter()
                        .map(|name| {
                            let online = live.contains(&name);
                            (name, online)
                        })
                        .collect(),
                }
            }
            (None, Some(link)) => StatusRole::Machine {
                gateway: link.gateway.clone(),
                address: link.address.clone(),
                online: link.up.load(std::sync::atomic::Ordering::SeqCst),
            },
            (None, None) => StatusRole::Alone,
        };
        // The report hits Slack once per thread to resolve permalinks and channel names — show a shimmer while waiting
        let thinking =
            slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, &slack::Status::Gathering.text());
        let (slack, clock) = (self.deps.slack.clone(), self.deps.clock.clone());
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = clear. It goes away whichever path exits
            let ctx = LogCtx {
                session_id: None,
                thread_key: Some(key.clone()),
            };
            // Resolve each channel name only once (one conversation can have many threads)
            let mut names: HashMap<String, Option<String>> = HashMap::new();
            for c in &mut channels {
                for t in &mut c.threads {
                t.permalink = match slack.get_permalink(&t.channel_id, &t.thread_ts).await {
                    Ok(p) => Some(p),
                    Err(e) => {
                        ctx.debug(
                            "bridge",
                            &format!(
                                "status: getPermalink failed for {}/{}: {e}",
                                t.channel_id, t.thread_ts
                            ),
                        );
                        None
                    }
                };
                }
                if !names.contains_key(&c.channel_id) {
                    let n = slack.channel_display_name(&c.channel_id).await;
                    names.insert(c.channel_id.clone(), n);
                }
                c.name = names[&c.channel_id].clone();
            }
            let report = StatusReport {
                bridge_version: env!("CARGO_PKG_VERSION").to_string(),
                machine,
                role,
                now_ms: clock.now_ms(),
                home,
                channels,
            }
            .render();
            api.post_now(&channel, &root, report, &key).await;
        });
    }

    /// The three forms of `pwd`. Resolution goes through the same resolve_repo_path
    /// as spawn, so the displayed path is always where the agent actually starts.
    pub(super) fn pwd_answer(
        &mut self,
        msg: &InboundMsg,
        mode: PwdMode,
        dm: bool,
        root_ts: &str,
        ctx: &LogCtx,
    ) -> String {
        let home = Host::home();
        let me = self.machine_name.clone();
        let entry = |access: &bridge::Access, ch: &str| {
            let (repo_path, _) = access.repo_path(ch, &home);
            PwdEntry {
                // Unset = this machine handles the channel itself
                machine: access
                    .routes
                    .get(ch)
                    .and_then(|r| r.bridge.clone())
                    .unwrap_or_else(|| me.clone()),
                repo_path,
            }
        };
        match mode {
            PwdMode::Usage => pwd_usage(),
            // Only the gateway knows the machines and answers these. Reaching a Bridge means there is
            // no gateway, or no such machine behind it
            PwdMode::On { machine, .. } => crate::t!(
                "No machine named `{machine}` is connected. This machine works on its own.",
                "`{machine}` という名前のマシンはつながっていません。このマシンは単独で動いています。"
            ),
            PwdMode::Current => {
                entry(&self.access, &msg.channel).render()
            }
            // A DM has no route (its agent always starts at Home), so say so
            // instead of silently recording it
            PwdMode::Set(_) if dm || !crate::chat::slack::SlackId::is_channel(&msg.channel) => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: pwd set refused — not a channel (dm={dm}) msg={}",
                        msg.ts
                    ),
                );
                pwd_dm_set_refusal()
            }
            PwdMode::Set(typed) => {
                // No shell ever sees this path (tmux gets it as is), so `~` and relative paths are
                // resolved here, on the machine that runs this channel's agents
                let path = &bridge::absolute_project_path(&typed, &Host::home());
                if !self.deps.agent.workdir_exists(path) {
                    ctx.info(
                        "bridge",
                        &format!("slack-events: pwd set refused — no folder {path} msg={}", msg.ts),
                    );
                    return bridge::no_such_folder(path, &self.machine_name);
                }
                let op = bridge::AccessOp::SetRepo {
                    channel: msg.channel.clone(),
                    path: path.clone(),
                };
                match self.access.apply(op) {
                    Ok((access, message, warnings)) => {
                        self.adopt_access(access, ctx);
                        // No thread = nothing to say, just keep the gateway's copy current (`channels`)
                        self.ask_the_gateway(
                            crate::bridge::gateway::link::LinkFrame::ProjectSet {
                                channel: msg.channel.clone(),
                                thread_ts: String::new(),
                                result: Ok(path.clone()),
                            },
                            ctx,
                        );
                        ctx.info(
                            "bridge",
                            &format!(
                                "slack-events: pwd set channel={} path={path} msg={}",
                                msg.channel, msg.ts
                            ),
                        );
                        with_warnings(&message, &warnings)
                    }
                    // For a path that fails validation, the error text itself is what the Owner reads
                    Err(e) => {
                        ctx.error(
                            "bridge",
                            &format!(
                                "slack-events: pwd set failed for {}:{root_ts}: {e}",
                                msg.channel
                            ),
                        );
                        e
                    }
                }
            }
        }
    }

    /// Access-management verbs. These are not MCP tools:
    /// the Bridge runs them on the Owner's raw message **before** delivery, so there is zero prompt-injection surface.
    pub(super) async fn owner_command(
        &mut self,
        msg: &InboundMsg,
        oc: crate::bridge::command::OwnerCmd,
        dm: bool,
        root_ts: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) {
        let op = match oc.verb {
            // `warm on|off [<#channel>]`. No argument means "the current channel" — the Owner is usually
            // standing in it. A DM has no route and is always pre-started, so it is refused
            "warm" => {
                // parse_owner_command already guarantees the first argument is on/off
                let on = oc
                    .args
                    .first()
                    .is_some_and(|a| a.eq_ignore_ascii_case("on"));
                let ch = match oc.args.get(1) {
                    Some(tok) => crate::chat::slack::SlackId::from_channel_mention(tok),
                    None if !dm => Some(msg.channel.clone()),
                    None => None,
                };
                let Some(channel) = ch else {
                    let usage = if dm && oc.args.len() < 2 {
                        crate::t!(
                            "The agent for DMs is always started ahead of time. Name a channel: `warm on|off <#channel>`",
                            "DM のエージェントは常に先に起動しています。チャンネルを指定してください: `warm on|off <#channel>`"
                        )
                    } else {
                        crate::t!("Usage: `warm on|off [<#channel>]`", "使い方: `warm on|off [<#channel>]`")
                    };
                    self.post(&msg.channel, root_ts, usage, key);
                    return;
                };
                bridge::AccessOp::SetWarm { channel, on }
            }
            // Home must be a real channel — the place where it was typed becomes Home
            "set-home" => {
                if dm || !crate::chat::slack::SlackId::is_channel(&msg.channel) {
                    let refusal = crate::t!(
                        "Run `set-home` in the *channel* you want notices in. A DM can't be the notice channel.",
                        "`set-home` は、通知を出したい *チャンネル* で実行してください。DM は通知先にできません。"
                    );
                    self.post(&msg.channel, root_ts, refusal, key);
                    return;
                }
                // Every machine writes its notices to the same place, so the gateway decides and tells
                // them all; deciding it here alone would leave the others pointing somewhere else
                if self.ask_the_gateway(
                    crate::bridge::gateway::link::LinkFrame::SetHome {
                        channel: msg.channel.clone(),
                        thread_ts: root_ts.to_string(),
                    },
                    ctx,
                ) {
                    return;
                }
                bridge::AccessOp::SetHome(msg.channel.clone())
            }
            verb @ ("allow-bot" | "remove-bot") => {
                let raw = oc.args.first().map(String::as_str).unwrap_or("");
                let bot_id = if crate::chat::slack::SlackId::is_bot(raw) {
                    raw.to_string() // A bare B… id is taken as-is
                } else {
                    // A bot is mentioned by its USER id (`<@U…>`), but the allowlist is keyed by bot_id (B…)
                    let Some(uid) = crate::chat::slack::SlackId::from_user_mention(raw) else {
                        self.post(
                            &msg.channel,
                            root_ts,
                            crate::t!("Usage: `{verb} <@bot>`", "使い方: `{verb} <@bot>`"),
                            key,
                        );
                        return;
                    };
                    match self.deps.slack.resolve_bot_id(&uid).await {
                        Ok(Some(b)) => b,
                        Ok(None) => {
                            let human = crate::t!(
                                "`{verb}` needs a bot — that mention is a person.",
                                "`{verb}` にはボットを指定してください。今のメンションは人です。"
                            );
                            self.post(&msg.channel, root_ts, human, key);
                            return;
                        }
                        Err(e) => {
                            ctx.error("bridge", &format!(
                                    "slack-events: owner-command '{verb}' failed for {}:{root_ts}: {e}",
                                    msg.channel
                                ));
                            self.post(&msg.channel, root_ts, e, key);
                            return;
                        }
                    }
                };
                if verb == "allow-bot" {
                    bridge::AccessOp::BotAllow(bot_id)
                } else {
                    bridge::AccessOp::BotRemove(bot_id)
                }
            }
            // Unreachable today, since parse_owner_command only passes verbs it knows.
            // Still, don't vanish silently — reply to the Owner with the error
            // text **as-is**
            other => {
                let e = format!("unknown owner command: {other}");
                ctx.error(
                    "bridge",
                    &format!(
                        "slack-events: owner-command '{other}' failed for {}:{root_ts}: {e}",
                        msg.channel
                    ),
                );
                self.post(&msg.channel, root_ts, e, key);
                return;
            }
        };
        match self.access.apply(op) {
            Ok((access, message, warnings)) => {
                self.adopt_access(access, ctx);
                self.post(
                    &msg.channel,
                    root_ts,
                    with_warnings(&message, &warnings),
                    key,
                );
            }
            // The validation error text is what the Owner reads
            Err(e) => {
                ctx.error(
                    "bridge",
                    &format!(
                        "slack-events: owner-command '{}' failed for {}:{root_ts}: {e}",
                        oc.verb, msg.channel
                    ),
                );
                self.post(&msg.channel, root_ts, e, key);
            }
        }
    }
}

// ── `status` — rendering the raw diagnostic snapshot the Bridge collected ─────

/// One "running thread" that `status` draws on a single line.
///
/// **Carries no version** — there is no mechanism for agents to report their version,
/// so no version-skew display.
#[derive(Clone)]
pub struct StatusThread {
    pub channel_id: String,
    pub thread_ts: String,
    /// Last activity in epoch ms. 0 = unknown
    pub last_activity_ms: u64,
    pub permalink: Option<String>,
    /// The thread's topic (first line of the opening message) — used as the link text
    pub topic: Option<String>,
}

impl StatusThread {
    /// One thread's line: a monospace chip with the elapsed time + a link to the topic. Readers look for
    /// "which one is old", and a column of short fixed-width chips on the left is easy to scan (instead of
    /// aligning columns). An unknown time is not written — an `unknown` takes up a column while saying nothing.
    fn line(&self, now_ms: u64) -> String {
        let label = Self::link_text(self.topic.as_deref());
        let text = match self.permalink.as_deref().filter(|p| !p.is_empty()) {
            Some(p) => format!("<{p}|{label}>"),
            None => label,
        };
        match Self::idle_label(now_ms, self.last_activity_ms) {
            None => format!("　{text}"),
            Some(idle) => format!("`{idle}`　{text}"),
        }
    }

    /// Human-readable idle: `just now` (<1 min) / `45m ago` (<60 min) / `9.4h ago` (≥60 min).
    /// A raw `565m` is hard to judge.
    ///
    /// **A deliberate change from the original**: append "ago". A bare `4m` doesn't say what the 4 minutes are
    /// (user feedback, 2026-07-31). Not a porting omission.
    /// `None` = the last activity time is unknown.
    fn idle_label(now_ms: u64, last_activity_ms: u64) -> Option<String> {
        if last_activity_ms == 0 {
            return None;
        }
        let mins = now_ms.saturating_sub(last_activity_ms) / 60_000;
        if mins < 1 {
            return Some(crate::t!("just now", "たった今"));
        }
        if mins < 60 {
            return Some(crate::t!("{mins}m ago", "{mins}分前"));
        }
        let hours = format!("{:.1}", mins as f64 / 60.0);
        Some(crate::t!("{hours}h ago", "{hours}時間前"))
    }

    /// mrkdwn link text cannot contain `<` `>` `|` (the link delimiters). A thread's topic is the
    /// opening message itself, so it usually starts with `<@…>` — a mention inside link text is never
    /// resolved and leaks the raw id. Drop all Slack tokens first, then strip leftover delimiters and
    /// fold newlines.
    fn link_text(topic: Option<&str>) -> String {
        // Hand-written scan equivalent to `/<[@#!][^>]*>/g` (no regex, to avoid adding a dependency)
        let raw = topic.unwrap_or("");
        let mut stripped = String::with_capacity(raw.len());
        let mut rest = raw;
        while let Some(i) = rest.find('<') {
            let after = &rest[i + 1..];
            let end = after
                .starts_with(['@', '#', '!'])
                .then(|| after.find('>'))
                .flatten();
            match end {
                Some(j) => {
                    stripped.push_str(&rest[..i]);
                    rest = &after[j + 1..];
                }
                // A `<` that is not a token is just a character — don't delete it, move on
                None => {
                    stripped.push_str(&rest[..=i]);
                    rest = after;
                }
            }
        }
        stripped.push_str(rest);
        let cleaned: String = stripped
            .chars()
            .filter(|c| !matches!(c, '<' | '>' | '|'))
            .collect();
        // `\s+` → ' ' plus trim, in one pass
        let t = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.is_empty() {
            return crate::t!("(untitled)", "(無題)");
        }
        // Counts characters, not UTF-16 units (safer not to split emoji)
        if t.chars().count() > 50 {
            format!("{}…", t.chars().take(50).collect::<String>())
        } else {
            t
        }
    }
}

/// A snapshot with everything `status` needs to render. It includes the clock, so rendering stays a pure function.
///
/// `pool` holds only the cwd (an opaque internal pool key is not shown, and
/// there is no `version` since this implementation has no version skew).
pub struct StatusReport {
    pub bridge_version: String,
    /// This machine's name.
    pub machine: String,
    /// Where this Bridge sits in the fleet.
    pub role: StatusRole,
    /// The snapshot carries the clock (so rendering stays pure and testable with a fixed now)
    pub now_ms: u64,
    /// $HOME. Used to fold long absolute paths into `~` for readability
    pub home: String,
    /// The channels this machine handles, each with its folder and what is running in it.
    pub channels: Vec<StatusChannel>,
}

/// One channel this machine handles.
pub struct StatusChannel {
    pub channel_id: String,
    /// The resolved name (`#general`, or `@alice` for a DM).
    pub name: Option<String>,
    /// Where its agents work.
    pub folder: String,
    /// Whether an agent is kept started ahead of time for it.
    pub warm_on: bool,
    /// Only threads whose agents are **actually running**.
    pub threads: Vec<StatusThread>,
    /// One per agent waiting in this channel's folder: when it last did anything (0 = unknown).
    pub warm: Vec<u64>,
}

/// Where a Bridge sits: the gateway, a machine linked to one, or on its own.
pub enum StatusRole {
    /// The gateway. `machines` is every machine it knows, and whether each is connected right now.
    Gateway { machines: Vec<(String, bool)> },
    /// A machine. The gateway's name (as it said in the handshake), where the link runs, and whether it is up.
    Machine {
        gateway: Option<String>,
        address: String,
        online: bool,
    },
    /// No gateway anywhere.
    Alone,
}

/// A machine and whether it is connected, as one word: `` `pve` 🟢 ``.
fn machine_mark(name: &str, online: bool) -> String {
    let dot = if online { "🟢" } else { "🔴" };
    format!("`{name}` {dot}")
}

/// StatusReport as Slack mrkdwn. A pure function (the clock is the report's now_ms).
///
/// Design: show names, not ids, and stay quiet when nothing is wrong. Each channel group gets a heading
/// with its resolved name (`<#id|name>` for a channel, the other party's `@name` for a DM) and its routed
/// repository; threads are bullets of the opening message (bot mention removed) + idle time.
impl StatusReport {
    /// Absolute paths under $HOME become `~`-relative. Paths outside $HOME stay as-is (not basename —
    /// neighbouring repos share a prefix, so the distinction would be lost).
    fn short_path(p: Option<&str>, home: &str) -> String {
        let Some(p) = p.filter(|s| !s.is_empty()) else {
            return crate::t!("(unknown)", "(不明)");
        };
        match p.strip_prefix(home).filter(|_| !home.is_empty()) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
            _ => p.to_string(),
        }
    }

    pub fn render(&self) -> String {
        let r = self;
        let mut lines: Vec<String> = Vec::new();
        let version = if r.bridge_version.is_empty() {
            "(unversioned)"
        } else {
            r.bridge_version.as_str()
        };
        lines.push(crate::t!("*version*: `{version}`", "*版*: `{version}`"));
        let me = &r.machine;
        match &r.role {
            StatusRole::Gateway { machines } => {
                lines.push(crate::t!(
                    "*machine id*: `{me}` (gateway)",
                    "*マシン*: `{me}`(ゲートウェイ)"
                ));
                let list = machines
                    .iter()
                    .map(|(name, online)| machine_mark(name, *online))
                    .collect::<Vec<_>>()
                    .join(", ");
                let list = if list.is_empty() {
                    crate::t!("none", "なし")
                } else {
                    list
                };
                lines.push(crate::t!(
                    "*connected machines*: {list}",
                    "*つながっているマシン*: {list}"
                ));
            }
            StatusRole::Machine {
                gateway,
                address,
                online,
            } => {
                lines.push(crate::t!("*machine id*: `{me}`", "*マシン*: `{me}`"));
                let name = gateway.as_deref().unwrap_or("?");
                let dot = if *online { "🟢" } else { "🔴" };
                // A loopback address is the ssh tunnel `add-machine` sets up when there is no direct route
                let via = if address.contains("127.0.0.1")
                    || address.contains("localhost")
                    || address.contains("[::1]")
                {
                    crate::t!(" · via ssh", " · ssh トンネル")
                } else {
                    String::new()
                };
                lines.push(crate::t!(
                    "*gateway*: `{name}` (`{address}`{via}) {dot}",
                    "*ゲートウェイ*: `{name}`(`{address}`{via}) {dot}"
                ));
            }
            StatusRole::Alone => {
                lines.push(crate::t!("*machine id*: `{me}`", "*マシン*: `{me}`"));
            }
        }
        // A bare blank line only gives a paragraph gap in Slack, which loses to the bullet spacing below,
        // so the heading looks glued to the body. A line with one full-width space survives as a tall line
        lines.push("　".to_string());

        if r.channels.is_empty() {
            lines.push(crate::t!(
                "*My channels* — none yet",
                "*このマシンのチャンネル* — まだありません"
            ));
        } else {
            lines.push(crate::t!("*My channels*", "*このマシンのチャンネル*"));
        }
        for (i, c) in r.channels.iter().enumerate() {
            // Blank lines only **between** channels; right after the heading it looks detached
            if i > 0 {
                lines.push(String::new());
            }
            let dm = c.channel_id.starts_with('D');
            // Bake the resolved name into the label — readable even where a bare `<#id>` isn't resolved.
            // Slack strips the leading '#' itself, so pass the bare word
            let head = match (c.name.as_deref().filter(|n| !n.is_empty()), dm) {
                (Some(n), true) => n.to_string(),
                (Some(n), false) => format!("<#{}|{}>", c.channel_id, n.strip_prefix('#').unwrap_or(n)),
                (None, true) => "DM".to_string(),
                (None, false) => format!("<#{}>", c.channel_id),
            };
            let folder = Self::short_path(Some(&c.folder), &r.home);
            let warm = if c.warm_on {
                crate::t!(" (warm on)", "(warm on)")
            } else {
                String::new()
            };
            lines.push(format!("{head} · `{folder}`{warm}"));
            let mut rows: Vec<(u64, String)> = c
                .threads
                .iter()
                .map(|t| (t.last_activity_ms, t.line(r.now_ms)))
                .collect();
            rows.extend(c.warm.iter().map(|idle| {
                let label = crate::t!("waiting", "待機中");
                let line = match StatusThread::idle_label(r.now_ms, *idle) {
                    None => format!("　{label}"),
                    Some(ago) => format!("`{ago}`　{label}"),
                };
                (*idle, line)
            }));
            if rows.is_empty() {
                lines.push(crate::t!("　no active threads", "　動いているスレッドなし"));
                continue;
            }
            rows.sort_by_key(|(idle, _)| *idle); // Longest idle first
            lines.extend(rows.into_iter().map(|(_, line)| line));
        }
        lines.join("\n")
    }
}

/// One resolved "channel → path" line for display.
///
/// `is_fallback` means it comes from the Home fallback (the channel has no explicit `repo_path`
/// route).
pub struct PwdEntry {
    /// The machine that works in it. Written in front of the path (`dock:/srv/app`), because the same
    /// path means different folders on different machines.
    pub machine: String,
    pub repo_path: String,
}

/// One channel's project directory (the `pwd` answer) as Slack mrkdwn.
impl PwdEntry {
    /// One line: where this channel's agents work, machine first (`pve:/root`). A channel with no folder
    /// of its own shows the machine's home — the folder it actually starts in, so there is nothing to note.
    pub fn render(&self) -> String {
        let (machine, path) = (&self.machine, &self.repo_path);
        crate::t!(
            "Current project directory: `{machine}:{path}`",
            "いまの作業ディレクトリ: `{machine}:{path}`"
        )
    }
}

/// Append warnings to a refusal or notice. **Only when there are warnings**, one per line, prefixed
/// with `⚠️ `.
fn with_warnings(message: &str, warnings: &[String]) -> String {
    if warnings.is_empty() {
        return message.to_string();
    }
    let w: Vec<String> = warnings.iter().map(|w| format!("⚠️ {w}")).collect();
    format!("{message}\n{}", w.join("\n"))
}

/// help as Slack mrkdwn. Grouped by purpose; each line is a monospace trigger (with aliases) + a one-line description.
/// Kept in sync **by hand** with `COMMAND_WORDS` and the argument parser — this is their human-facing index.
///
/// `machines` decides whether the machine commands (`pwd <machine>`, `channels`) are listed: true on the
/// gateway and on any machine linked to one (it passes those commands up). A Bridge on its own has
/// nobody to hand a channel to, so it doesn't offer.
pub(super) fn help(machines: bool, agent: &dyn crate::agent::Agent) -> String {
    fn section<C: std::fmt::Display>(lines: &mut Vec<String>, title: String, rows: Vec<(C, String)>) {
        lines.push(format!("*{title}*"));
        for (cmd, desc) in rows {
            lines.push(format!("  • `{cmd}` — {desc}"));
        }
        lines.push(String::new());
    }

    let mut lines: Vec<String> = vec![crate::t!("*agentgw commands*", "*agentgw のコマンド*"), String::new()];

    for (title, rows) in super::agent::help_sections(agent) {
        section(&mut lines, title, rows);
    }
    if machines {
        section(
            &mut lines,
            crate::t!("Machines", "マシン"),
            vec![
                ("pwd <machine>[:<path>]", crate::t!("hand this channel to a machine (its home folder, or the one you name)", "このチャンネルをマシンに任せる(パス無しならそのマシンの家、付ければそのフォルダ)")),
                ("channels", crate::t!("show which machine handles this channel and the others", "このチャンネルとほかのチャンネルを受け持つマシンを見る")),
            ],
        );
    }
    section(
        &mut lines,
        crate::t!("Channels", "チャンネル"),
        vec![
            ("pwd", crate::t!("show this channel's project directory", "このチャンネルの作業ディレクトリを見る")),
            ("pwd <path>", crate::t!("set this channel's project directory (`/…`, `~/…` or `./…`)", "このチャンネルの作業ディレクトリを決める(`/…`・`~/…`・`./…`)")),
            ("warm on|off [<#channel>]", crate::t!("keep an agent started ahead of time for a channel", "チャンネルのエージェントを先に起動しておくか")),
            ("set-home", crate::t!("send notices to this channel", "通知をこのチャンネルに出す")),
        ],
    );
    section(
        &mut lines,
        "agentgw".to_string(),
        vec![
            ("status", crate::t!("version, active threads and warm agents", "版・動いているスレッド・待機中のエージェント")),
            ("restart", crate::t!("restart agentgw (picks up a new version)", "agentgw を再起動する(新しい版を読み込む)")),
        ],
    );
    section(
        &mut lines,
        crate::t!("Other bots", "ほかのボット"),
        vec![
            ("allow-bot <@bot>", crate::t!("let a bot's messages start work", "そのボットの投稿で作業を始められるようにする")),
            ("remove-bot <@bot>", crate::t!("stop letting that bot's messages through", "そのボットの投稿を通さないようにする")),
        ],
    );
    section(
        &mut lines,
        crate::t!("Help", "ヘルプ"),
        vec![("help / ?", crate::t!("show this list", "この一覧を出す"))],
    );

    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.push(String::new());
    lines.push(crate::t!(
        "_A command only runs when it's the whole message (with its arguments). Inside a sentence it's just part of a normal message._",
        "_コマンドは、メッセージ全体がそのコマンド(引数を含む)のときだけ動きます。文の中に書いたときは、普通のメッセージとして届きます。_"
    ));
    lines.join("\n")
}

/// What `pwd` accepts. Answered when a message starts with `pwd` but the rest is none of the forms —
/// a mistyped path is a mistake to point out, not a sentence to hand to the agent.
fn pwd_usage() -> String {
    crate::t!(
        "`pwd` takes one of these:\n\
         • `pwd` — this channel's project folder\n\
         • `pwd /srv/app` · `pwd ~/dev/app` · `pwd ./dev/app` — work in that folder\n\
         • `pwd <machine>` — hand this channel to that machine (its home folder)\n\
         • `pwd <machine>:~/dev/app` — hand it over and work in that folder there",
        "`pwd` の書き方:\n\
         • `pwd` — このチャンネルの作業ディレクトリを見る\n\
         • `pwd /srv/app`・`pwd ~/dev/app`・`pwd ./dev/app` — そのフォルダで作業する\n\
         • `pwd <マシン>` — このチャンネルをそのマシンに任せる(そのマシンの家のディレクトリ)\n\
         • `pwd <マシン>:~/dev/app` — そのマシンに任せて、そのフォルダで作業する"
    )
}

/// The answer when `pwd <path>` is typed in a DM. A DM's agent always runs in the default directory —
/// there is nothing to set, so say so instead of silently doing nothing.
fn pwd_dm_set_refusal() -> String {
    crate::t!(
        "Agents in DMs always work in the default directory; only channels can have their own.",
        "DM のエージェントは常に既定のディレクトリで動きます。作業ディレクトリを決められるのはチャンネルだけです。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_and_paths() {
        let _en = crate::i18n::pin(crate::i18n::Lang::En);
        let idle = |now, last| StatusThread::idle_label(now, last);
        assert_eq!(idle(10_000, 0), None);
        assert_eq!(idle(60_000, 30_000).as_deref(), Some("just now"));
        assert_eq!(idle(46 * 60_000, 60_000).as_deref(), Some("45m ago"));
        assert_eq!(idle(10 * 60 * 60_000, 36 * 60_000).as_deref(), Some("9.4h ago"));
        assert_eq!(
            StatusReport::short_path(Some("/Users/t/dev/x"), "/Users/t"),
            "~/dev/x"
        );
        assert_eq!(
            StatusReport::short_path(Some("/opt/x"), "/Users/t"),
            "/opt/x"
        );
        assert_eq!(StatusReport::short_path(None, "/Users/t"), "(unknown)");
    }

    fn thread(channel: &str, ts: &str, idle: u64, topic: Option<&str>, link: Option<&str>) -> StatusThread {
        StatusThread {
            channel_id: channel.into(),
            thread_ts: ts.into(),
            last_activity_ms: idle,
            permalink: link.map(str::to_string),
            topic: topic.map(str::to_string),
        }
    }

    fn sample_report() -> StatusReport {
        StatusReport {
            bridge_version: "0.1.0-rs".into(),
            now_ms: 1_000_000,
            home: "/Users/t".into(),
            machine: "dock".into(),
            role: StatusRole::Gateway {
                machines: vec![("pve".into(), true), ("mac".into(), false)],
            },
            channels: vec![],
        }
    }

    /// Every channel this machine handles, each with its folder and what is running in it: threads and
    /// agents waiting. A channel with nothing running still shows — "where can I work" is the question.
    #[test]
    fn status_lists_each_channel_with_its_threads() {
        let r = StatusReport {
            channels: vec![
                StatusChannel {
                    channel_id: "C1".into(),
                    name: Some("#general".into()),
                    folder: "/Users/t/dev/x".into(),
                    warm_on: true,
                    threads: vec![thread(
                        "C1",
                        "1.0",
                        940_000,
                        Some("<@UBOT> READMEを要約して"),
                        Some("https://s/p1"),
                    )],
                    warm: vec![700_000],
                },
                StatusChannel {
                    channel_id: "C2".into(),
                    name: Some("#agentgw".into()),
                    folder: "/Users/t/dev/agentgw".into(),
                    warm_on: false,
                    threads: vec![],
                    warm: vec![],
                },
            ],
            ..sample_report()
        };
        let out = r.render();
        assert!(out.contains("*My channels*\n"), "{out}");
        // Folder next to the channel, warm flag only where it is on
        assert!(out.contains("<#C1|general> · `~/dev/x` (warm on)\n"), "{out}");
        assert!(out.contains("<#C2|agentgw> · `~/dev/agentgw`\n"), "{out}");
        // Longest idle first: the waiting agent (5m) above the thread (1m)
        assert!(
            out.contains("`5m ago`　waiting\n`1m ago`　<https://s/p1|READMEを要約して>"),
            "{out}"
        );
        assert!(!out.contains("UBOT"), "{out}");
        assert!(out.contains("\n　no active threads"), "{out}");
    }

    /// A DM has no channel name of its own and no route; it still shows what is running.
    #[test]
    fn status_shows_a_dm_and_says_when_there_is_nothing() {
        let r = StatusReport {
            channels: vec![StatusChannel {
                channel_id: "D1".into(),
                name: Some("@alice".into()),
                folder: "/Users/t".into(),
                warm_on: false,
                threads: vec![thread("D1", "2.0", 0, None, None)],
                warm: vec![],
            }],
            ..sample_report()
        };
        let out = r.render();
        assert!(out.contains("@alice · `~`\n　(untitled)"), "{out}");

        let empty = StatusReport {
            channels: vec![],
            ..sample_report()
        };
        assert!(empty.render().contains("*My channels* — none yet"));
    }

    /// Net for the hand-written token scan (the three replaces) that avoids regex.
    #[test]
    fn link_text_strips_tokens_and_truncates() {
        assert_eq!(StatusThread::link_text(None), "(untitled)");
        assert_eq!(
            StatusThread::link_text(Some("<@U1> <#C1|general>")),
            "(untitled)"
        ); // All tokens → empty
        assert_eq!(StatusThread::link_text(Some("a<b\nc  d")), "ab c d"); // An unclosed `<` just disappears (no gap left)
        assert_eq!(
            StatusThread::link_text(Some("<https://x|見出し>")),
            "https://x見出し"
        ); // A non-token `<`
        let long = "あ".repeat(60);
        let cut = StatusThread::link_text(Some(&long));
        assert!(cut.ends_with('…') && cut.chars().count() == 51);
    }

    /// A machine's header names its gateway and where the link runs. A loopback address is the ssh
    /// tunnel `add-machine` sets up when there's no direct route, so say so.
    #[test]
    fn status_header_of_a_machine_names_its_gateway() {
        let head = |role| StatusReport {
            bridge_version: "1.2.3".into(),
            machine: "pve".into(),
            role,
            now_ms: 0,
            home: "/root".into(),
            channels: vec![],
        }
        .render();

        let direct = head(StatusRole::Machine {
            gateway: Some("dock".into()),
            address: "wss://dock.example".into(),
            online: true,
        });
        assert!(direct.contains("*machine id*: `pve`\n"), "{direct}");
        assert!(direct.contains("*gateway*: `dock` (`wss://dock.example`) 🟢"), "{direct}");

        let tunnel = head(StatusRole::Machine {
            gateway: Some("dock".into()),
            address: "ws://127.0.0.1:8799".into(),
            online: false,
        });
        assert!(tunnel.contains("(`ws://127.0.0.1:8799` · via ssh) 🔴"), "{tunnel}");

        let alone = head(StatusRole::Alone);
        assert!(alone.contains("*machine id*: `pve`"), "{alone}");
        assert!(!alone.contains("gateway"), "{alone}");
    }

    #[test]
    fn pwd_renders() {
        let e = PwdEntry {
            machine: "dock".into(),
            repo_path: "/dev/x".into(),
        };
        assert_eq!(e.render(), "Current project directory: `dock:/dev/x`");
    }

    #[test]
    fn help_lists_every_command() {
        let h = help(false, &crate::agent::fake::FakeAgent::default());
        for word in [
            "stop",
            "exit / bye / done",
            "compact",
            "model [fable|opus|sonnet|haiku]",
            "effort [low|medium|high|xhigh|max|ultracode|auto]",
            "mode [manual|plan|edit|auto]",
            "context / ctx",
            "resume",
            "login",
            "logout",
            "usage / usg",
            "status",
            "restart",
            "pwd <path>",
            "warm on|off [<#channel>]",
            "set-home",
            "allow-bot <@bot>",
            "remove-bot <@bot>",
            "help / ?",
        ] {
            assert!(h.contains(word), "{word}");
        }
        assert!(!h.contains("pwd <machine>")); // A gateway with no machines has nobody to hand a channel to
        assert!(h.ends_with("just part of a normal message._"));
    }

    #[test]
    fn a_bridge_that_takes_children_lists_the_machine_commands() {
        let h = help(true, &crate::agent::fake::FakeAgent::default());
        assert!(h.contains("pwd <machine>[:<path>]") && h.contains("channels"));
        assert!(h.contains("hand this channel to a machine"));
        // Having machines doesn't change the other sections
        assert!(h.contains("status") && h.contains("help / ?"));
    }

}
