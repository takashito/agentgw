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
                repo_path: e.repo_path.clone(),
                permalink: None,
                topic: e.topic.clone(),
                channel_name: None,
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
        // `self` cannot be carried into a 'static spawn — take the pool cwds out here and move them in
        // Count only **usable** pool entries (starting ones "don't exist yet"; given-up slots are already gone)
        let pools: Vec<String> = self
            .workers
            .pool_summary()
            .into_iter()
            .filter(|(_, ready)| *ready)
            .map(|(cwd, _)| cwd)
            .collect();
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
            for t in &mut threads {
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
                if !names.contains_key(&t.channel_id) {
                    let n = slack.channel_display_name(&t.channel_id).await;
                    names.insert(t.channel_id.clone(), n);
                }
                t.channel_name = names[&t.channel_id].clone();
            }
            let report = StatusReport {
                bridge_version: env!("CARGO_PKG_VERSION").to_string(),
                now_ms: clock.now_ms(),
                home,
                // There is only one way to be connected (no Remote)
                mode: "local".to_string(),
                threads,
                pools,
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
        let entry = |access: &bridge::Access, ch: &str| {
            let (repo_path, is_fallback) = access.repo_path(ch, &home);
            PwdEntry {
                channel_id: ch.to_string(),
                repo_path,
                label: access.routes.get(ch).and_then(|r| r.label.clone()),
                is_fallback,
            }
        };
        match mode {
            PwdMode::Current => {
                entry(&self.access, &msg.channel).render(&crate::t!("Project directory for this channel", "このチャンネルの作業ディレクトリ"))
            }
            PwdMode::All => {
                let all: Vec<_> = self
                    .access
                    .routes
                    .keys()
                    .map(|ch| entry(&self.access, ch))
                    .collect();
                PwdEntry::render_all(&all, &home)
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
            PwdMode::Set(path) => {
                let op = bridge::AccessOp::SetRepo {
                    channel: msg.channel.clone(),
                    path: path.clone(),
                };
                match self.access.apply(op) {
                    Ok((access, message, warnings)) => {
                        self.adopt_access(access, ctx);
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
pub struct StatusThread {
    pub channel_id: String,
    pub thread_ts: String,
    /// Last activity in epoch ms. 0 = unknown
    pub last_activity_ms: u64,
    /// The repository the agent runs in (shown once per channel group)
    pub repo_path: Option<String>,
    pub permalink: Option<String>,
    /// The thread's topic (first line of the opening message) — used as the link text
    pub topic: Option<String>,
    /// The resolved channel name (`#general`, or `@alice` for a DM)
    pub channel_name: Option<String>,
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
    /// The snapshot carries the clock (so rendering stays pure and testable with a fixed now)
    pub now_ms: u64,
    /// $HOME. Used to fold long absolute paths into `~` for readability
    pub home: String,
    /// How this Bridge is connected (`local (label)` etc.)
    pub mode: String,
    /// Only threads whose agents are **actually running**
    pub threads: Vec<StatusThread>,
    /// Working directories that warm-pool agents are waiting in
    pub pools: Vec<String>,
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
        // Fold version and connection mode into one line. A lone `mode: local` line looks like
        // a stray log fragment under the heading (user feedback, 2026-07-31)
        let mode = if r.mode.is_empty() {
            String::new()
        } else {
            format!(" · {}", r.mode)
        };
        lines.push(format!("🟢 *agentgw* `{version}`{mode}"));
        // A bare blank line only gives a paragraph gap in Slack, which loses to the bullet spacing below,
        // so the heading looks glued to the body. A line with one full-width space survives as a tall line
        lines.push("　".to_string());

        if r.threads.is_empty() {
            lines.push(crate::t!("*Active threads* — none", "*動いているスレッド* — なし"));
        } else {
            // Group by channel, keeping **insertion order** (the sort below is stable)
            let mut groups: Vec<(&str, Vec<&StatusThread>)> = Vec::new();
            for t in &r.threads {
                match groups.iter_mut().find(|(ch, _)| *ch == t.channel_id) {
                    Some((_, g)) => g.push(t),
                    None => groups.push((&t.channel_id, vec![t])),
                }
            }
            let n = r.threads.len();
            lines.push(crate::t!("*Active threads* — {n}", "*動いているスレッド* — {n}"));
            // DMs first, then the busiest channels. Ties are broken by channel id.
            let dm_rank = |ch: &str| u8::from(!ch.starts_with('D'));
            groups.sort_by(|a, b| {
                dm_rank(a.0)
                    .cmp(&dm_rank(b.0))
                    .then(b.1.len().cmp(&a.1.len()))
                    .then(a.0.cmp(b.0))
            });
            for (i, (ch, ts)) in groups.iter_mut().enumerate() {
                // Blank lines only **between** channels. Right after the heading it makes the count look detached
                if i > 0 {
                    lines.push(String::new());
                }
                let dm = ch.starts_with('D');
                let name = ts
                    .iter()
                    .find_map(|t| t.channel_name.as_deref().filter(|n| !n.is_empty()));
                // Bake the resolved name into the label — readable even where a bare `<#id>` isn't resolved.
                // Slack strips the leading '#' itself, so pass the bare word.
                let head = match (name, dm) {
                    (Some(n), true) => n.to_string(),
                    (Some(n), false) => format!("<#{ch}|{}>", n.strip_prefix('#').unwrap_or(n)),
                    (None, true) => "DM".to_string(),
                    (None, false) => format!("<#{ch}>"),
                };
                let repo = ts
                    .iter()
                    .find_map(|t| t.repo_path.as_deref().filter(|p| !p.is_empty()));
                lines.push(match repo {
                    Some(_) => format!("{head} · `{}`", Self::short_path(repo, &r.home)),
                    None => head,
                });
                ts.sort_by_key(|t| t.last_activity_ms); // Longest idle first
                lines.extend(ts.iter().map(|t| t.line(r.now_ms)));
            }
        }
        lines.push(String::new());
        // A list of just the folders that are waiting
        if r.pools.is_empty() {
            lines.push(crate::t!("*Warm agents* — none", "*待機中のエージェント* — なし"));
        } else {
            let n = r.pools.len();
            lines.push(crate::t!("*Warm agents* — {n}", "*待機中のエージェント* — {n}"));
            // One per line only makes it tall — lay the paths out horizontally as monospace chips
            lines.push(
                r.pools
                    .iter()
                    .map(|cwd| format!("`{}`", Self::short_path(Some(cwd), &r.home)))
                    .collect::<Vec<_>>()
                    .join("　"),
            );
        }
        lines.join("\n")
    }
}

/// One resolved "channel → path" line for display.
///
/// `is_fallback` means it comes from the Home fallback (the channel has no explicit `repo_path`
/// route).
pub struct PwdEntry {
    pub channel_id: String,
    pub repo_path: String,
    pub label: Option<String>,
    pub is_fallback: bool,
}

/// One channel's path (the `pwd` form) as Slack mrkdwn. `heading` distinguishes "this channel"
/// from a named channel.
impl PwdEntry {
    /// The label in parentheses, or nothing if there is none.
    fn label_text(&self) -> String {
        self.label
            .as_deref()
            .filter(|l| !l.is_empty())
            .map(|l| format!("（{l}）"))
            .unwrap_or_default()
    }

    pub fn render(&self, heading: &str) -> String {
        let entry = self;
        let label = entry.label_text();
        let note = if entry.is_fallback {
            crate::t!(" — not set; using the default directory", " — 未設定のため既定のディレクトリ")
        } else {
            String::new()
        };
        format!(
            "● {heading}\n<#{}>{label}{note}\n  `{}`",
            entry.channel_id, entry.repo_path
        )
    }

    /// Every configured channel → path (the `pwd all` form). `home` is the Home fallback that channels /
    /// DMs without a route fall to.
    pub fn render_all(entries: &[PwdEntry], home: &str) -> String {
        let mut lines = vec![crate::t!("● Project directories by channel", "● チャンネルごとの作業ディレクトリ")];
        if entries.is_empty() {
            lines.push(crate::t!("  • None set.", "  • まだ設定していません。"));
        } else {
            for e in entries {
                lines.push(format!(
                    "  • <#{}>{} → `{}`",
                    e.channel_id,
                    e.label_text(),
                    e.repo_path
                ));
            }
        }
        lines.push(String::new());
        lines.push(crate::t!("_Channels without one, and DMs, use `{home}`_", "_設定していないチャンネルと DM は `{home}` を使います_"));
        lines.join("\n")
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
/// `fleet` decides whether `route` is listed. It is executed by **the side that accepts machines**
/// (`CommandCtx::route` in `relay.rs`), so the condition follows that side.
pub(super) fn help(fleet: bool, agent: &dyn crate::agent::Agent) -> String {
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
    if fleet {
        section(
            &mut lines,
            crate::t!("Machines", "マシン"),
            vec![
                ("route <machine>", crate::t!("hand this channel to a machine", "このチャンネルをマシンに任せる")),
                ("route", crate::t!("show which machine handles this channel and the others", "このチャンネルとほかのチャンネルを受け持つマシンを見る")),
            ],
        );
    }
    section(
        &mut lines,
        crate::t!("Channels", "チャンネル"),
        vec![
            ("pwd", crate::t!("show this channel's project directory", "このチャンネルの作業ディレクトリを見る")),
            ("pwd <absolute path>", crate::t!("set this channel's project directory", "このチャンネルの作業ディレクトリを決める")),
            ("pwd all", crate::t!("show every channel's project directory", "すべてのチャンネルの作業ディレクトリを見る")),
            ("warm on|off [<#channel>]", crate::t!("keep an agent started ahead of time for a channel (this one if none is given)", "チャンネルのエージェントを先に起動しておくか(省くとこのチャンネル)")),
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

    #[test]
    fn status_report_groups_and_orders() {
        let r = StatusReport {
            bridge_version: "0.1.0-rs".into(),
            now_ms: 1_000_000,
            home: "/Users/t".into(),
            mode: "local".into(),
            threads: vec![
                StatusThread {
                    channel_id: "C1".into(),
                    thread_ts: "1.0".into(),
                    last_activity_ms: 940_000,
                    repo_path: Some("/Users/t/dev/x".into()),
                    permalink: Some("https://s/p1".into()),
                    topic: Some("<@UBOT> READMEを要約して".into()),
                    channel_name: Some("#general".into()),
                },
                StatusThread {
                    channel_id: "D1".into(),
                    thread_ts: "2.0".into(),
                    last_activity_ms: 0,
                    repo_path: None,
                    permalink: None,
                    topic: None,
                    channel_name: Some("@alice".into()),
                },
            ],
            pools: vec![],
        };
        let out = r.render();
        // Version and mode on one line (a lone `mode: local` line looks like a log fragment)
        // The name is the binary name as-is. Under the heading is a line with one full-width space (a bare blank line leaves too little gap)
        assert!(out.starts_with("🟢 *agentgw* `0.1.0-rs` · local\n　\n"));
        assert!(out.contains("*Active threads* — 2"));
        // DMs first; the mention is stripped from the link text
        assert!(out.find("@alice").unwrap() < out.find("general").unwrap());
        assert!(!out.contains("UBOT"));
        // Elapsed time is a monospace chip at the start of the line, followed by the linked topic
        assert!(
            out.contains("\n<#C1|general> · `~/dev/x`\n`1m ago`　<https://s/p1|READMEを要約して>")
        );
        // A thread with an unknown time gets no chip, only the leading alignment
        assert!(out.contains("\n@alice\n　(untitled)\n"));
        assert!(out.contains("*Warm agents* — none"));
        // Empty thread
        let empty = StatusReport {
            threads: vec![],
            ..r
        };
        assert!(empty.render().contains("*Active threads* — none"));
    }

    /// Minimal warm-pool section. threads may be empty (the sections are independent).
    fn sample_report() -> StatusReport {
        StatusReport {
            bridge_version: "0.1.0-rs".into(),
            now_ms: 1_000_000,
            home: "/Users/t".into(),
            mode: "local".into(),
            threads: vec![],
            pools: vec![],
        }
    }

    /// A count + a flat list of cwds (no internal keys shown).
    #[test]
    fn status_report_lists_warm_pools() {
        let r = StatusReport {
            pools: vec!["/repo/a".into(), "/Users/t/dev/b".into()],
            ..sample_report()
        };
        let out = r.render();
        assert!(out.contains("*Warm agents* — 2"));
        assert!(out.contains("\n\n*Warm agents")); // Blank line between it and the section above
        assert!(out.contains("\n`/repo/a`　`~/dev/b`")); // One row / $HOME folded
    }

    #[test]
    fn status_report_warm_pool_empty_unchanged() {
        assert!(sample_report().render().contains("*Warm agents* — none"));
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

    #[test]
    fn pwd_renders() {
        let e = PwdEntry {
            channel_id: "C1".into(),
            repo_path: "/dev/x".into(),
            label: Some("dev".into()),
            is_fallback: true,
        };
        assert_eq!(
            e.render("This channel"),
            "● This channel\n<#C1>（dev） — not set; using the default directory\n  `/dev/x`"
        );
        assert!(PwdEntry::render_all(&[e], "/home").contains("  • <#C1>（dev） → `/dev/x`"));
        assert!(PwdEntry::render_all(&[], "/home").contains("None set."));
        assert!(
            PwdEntry::render_all(&[], "/home")
                .ends_with("_Channels without one, and DMs, use `/home`_")
        );
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
            "pwd <absolute path>",
            "warm on|off [<#channel>]",
            "set-home",
            "allow-bot <@bot>",
            "remove-bot <@bot>",
            "help / ?",
        ] {
            assert!(h.contains(word), "{word}");
        }
        assert!(!h.contains("route <machine>")); // A gateway with no machines has no routing table
        assert!(h.ends_with("just part of a normal message._"));
    }

    #[test]
    fn a_bridge_that_takes_children_lists_route() {
        // It was implemented in `CommandCtx::route` in relay.rs but missing from the list
        let h = help(true, &crate::agent::fake::FakeAgent::default());
        assert!(h.contains("route <machine>"));
        assert!(h.contains("hand this channel to a machine"));
        // Having machines doesn't change the other sections
        assert!(h.contains("status") && h.contains("help / ?"));
    }
}
