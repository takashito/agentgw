//! "Body commands" the Bridge answers by itself — parsing them and running them.
//!
//! The first half is parsing (all pure functions, no I/O).
//! The second half (`// ── running commands ──`) runs them: a parsed command is executed in `impl Bridge`
//! (`handle_command` is the entry point; sign-in / sign-out outcomes go back to main as `CmdFx`).
//!
//! Commands are the **detection** layer of three (detect / parse / render). The single rule here:
//! "fire only when the **whole** message is that command".
//! A sentence that merely contains the word passes through and reaches the agent as a normal message.
//!
//! Looking at `agent::` from here follows the dependency direction (Bridge → agent). Never the reverse.
//! The reply text lives with whoever runs the command (`command/agent.rs` / `command/bridge.rs`).
//!
//! Sections, in order of role:
//!
//! 1. Reading the body   `Message` (the only place mentions and invisible characters are removed)
//! 2. Command detection  `Cmd` / `PwdMode` / `OwnerCmd` — the bare-word vocabulary is the single table `Cmd::WORDS`
//! 3. Usage-limit watch  `UsageWatch` (reading and writing times is `clock::WallClock`)
//! 4. Tool permission    `ToolPermission`
//! 5. Execution          `SignIn` and the entry point `handle_command`. The bodies split into those acting on
//!    the thread's agent (`command/agent.rs`) and those about the Bridge itself (`command/bridge.rs`)

mod agent;
mod bridge;

use crate::clock::WallClock;

use crate::agent::UsageRow;
use crate::agent::Agent;
use std::path::Path;

use super::Bridge;
use crate::chat::InboundMsg;
use crate::log::LogCtx;
use crate::chat::ThreadKey;
use crate::chat::slack::SlackId;
use crate::agent::SpawnOutcome;
use std::collections::HashMap;

// ── Section 1: reading the body ──────────────────────────────────────────────

/// The body of one message delivered to the Bridge. Command detection goes through here.
///
/// The single rule: "fire only when the **whole** message is that command".
/// A sentence that merely contains the word passes through and reaches the agent as a normal message.
#[derive(Clone, Copy)]
pub struct Message<'a> {
    text: &'a str,
    bot_user_id: Option<&'a str>,
}

impl<'a> Message<'a> {
    pub fn new(text: &'a str, bot_user_id: Option<&'a str>) -> Self {
        Self { text, bot_user_id }
    }

    /// The body with only **our own** mention removed. Others' mentions stay — `<@other> stop` is addressed to
    /// someone else, so for us it is no longer a bare command. Case and whitespace are preserved (path arguments need them).
    fn without_mention(&self) -> String {
        let Some(bot) = self.bot_user_id.filter(|b| !b.is_empty()) else {
            return self.text.to_string();
        };
        let needle = format!("<@{bot}");
        let mut out = String::with_capacity(self.text.len());
        let mut rest = self.text;
        while let Some(i) = rest.find(&needle) {
            let after = &rest[i + needle.len()..];
            // Strip only `<@ID>` and `<@ID|label>`. A different id that merely shares the prefix is kept
            let tail = match after.strip_prefix('>') {
                Some(t) => Some(t),
                None => after
                    .strip_prefix('|')
                    .and_then(|t| t.find('>').map(|j| &t[j + 1..])),
            };
            match tail {
                Some(t) => {
                    out.push_str(&rest[..i]);
                    out.push(' ');
                    rest = t;
                }
                None => {
                    let cut = i + needle.len();
                    out.push_str(&rest[..cut]);
                    rest = &rest[cut..];
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// The body with our mention removed and variation selectors / ZWJ removed (invisible; Slack's `text`
    /// attaches them to emoji). Case and whitespace are kept.
    fn cleaned(&self) -> String {
        self.without_mention()
            .chars()
            .filter(|c| !matches!(c, '\u{FE0F}' | '\u{200D}'))
            .collect()
    }

    /// The message normalized for comparison: [`Self::cleaned`] + trim + lowercase.
    pub fn normalized(&self) -> String {
        self.cleaned().trim().to_lowercase()
    }

    /// True only when the body is, **as a whole**, the `name` command. A long sentence that contains the word
    /// is never a bare command. An unknown `name` is false.
    pub fn is(&self, name: &str) -> bool {
        let normalized = self.normalized();
        Cmd::WORDS
            .iter()
            .find(|(n, ..)| *n == name)
            .is_some_and(|(_, words, _)| words.contains(&normalized.as_str()))
    }

    /// When the body starts with `verb`, the words **after** it; otherwise None.
    /// Case is preserved (path arguments need it) — lowercase it when comparing.
    pub fn verb_args(&self, verb: &str) -> Option<Vec<String>> {
        let cleaned = self.cleaned();
        let mut words = cleaned.split_whitespace();
        let first = words.next().unwrap_or("");
        (first.to_lowercase() == verb).then(|| words.map(str::to_string).collect())
    }

    /// Whether the body mentions **us** (`containsSelfMention`).
    /// In a channel, a message is delivered only if this holds or the thread is active.
    pub fn mentions_bot(&self) -> bool {
        self.bot_user_id
            .filter(|b| !b.is_empty())
            .is_some_and(|_| self.without_mention() != self.text)
    }

    /// Whether the body names **someone other than us**. If `<@` remains after removing our own mention,
    /// it is addressed to someone else (`<@someone> hey` — it does flow into a running thread,
    /// but it is not meant for us).
    pub fn mentions_someone_else(&self) -> bool {
        self.without_mention().contains("<@")
    }
}

// ── Section 2: command detection ─────────────────────────────────────────────
// The five that take arguments (`model` / `effort` / `mode` / `pwd` / access verbs) are detected by **shape**:
// only known model names count as arguments, so "explain model to me" is a sentence and goes to the agent as-is.

/// The 16 commands the Bridge answers by itself.
#[derive(Debug, PartialEq, Eq)]
pub enum Cmd {
    Stop,
    Exit,
    Resume,
    Help,
    Status,
    Context,
    Usage,
    Compact,
    Restart,
    /// `login` that arrives when an Owner already exists (when unset, it is handled before the gate)
    Login,
    Logout,
    /// `None` = bare `model` (show the current value) / `Some(name)` = switch
    Model(Option<String>),
    /// `None` = bare `effort` / `Some(level)` = set
    Effort(Option<String>),
    /// `None` = bare `mode` (show the current value) / `Some(name)` = switch
    Mode(Option<String>),
    Pwd(PwdMode),
    /// `channels` / `channel` — which machine handles this channel and the others. **The gateway answers it**;
    /// a machine passes it up (the gateway never sees a follow-up in a running thread — it carries no mention).
    Channels,
    /// `machines` / `machine` — the gateway and every machine it knows, with where each can be reached.
    Machines,
    Owner(OwnerCmd),
}

impl Cmd {
    /// The vocabulary of every bare-word command in **one table**.
    /// Name, synonyms and variants sit on one row, so neither `parse` nor `label` needs another table.
    /// The five that take arguments (`model` / `effort` / `mode` / `pwd` / access verbs) can't be recognised
    /// without reading the argument, so they are parsed separately below.
    const WORDS: [(&str, &[&str], Cmd); 13] = [
        // stop: word, `:shortcode:`, raw emoji — Slack's `text` may send any of them
        (
            "stop",
            &[
                "stop",
                ":red_circle:",
                ":octagonal_sign:",
                ":black_square_for_stop:",
                ":hand:",
                ":raised_hand:",
                ":x:",
                "🔴",
                "🛑",
                "⏹",
                "✋",
                "❌",
            ],
            Cmd::Stop,
        ),
        ("exit", &["exit", "bye", "done"], Cmd::Exit),
        ("resume", &["resume"], Cmd::Resume),
        ("status", &["status", "ステータス"], Cmd::Status),
        ("context", &["context", "ctx"], Cmd::Context),
        ("usage", &["usage", "usg"], Cmd::Usage),
        // A bare question mark is the natural "what can you do?"
        ("help", &["help", "ヘルプ", "?", "？"], Cmd::Help),
        ("compact", &["compact"], Cmd::Compact),
        ("restart", &["restart"], Cmd::Restart),
        ("login", &["login"], Cmd::Login),
        ("logout", &["logout"], Cmd::Logout),
        ("channels", &["channels", "channel"], Cmd::Channels),
        ("machines", &["machines", "machine"], Cmd::Machines),
    ];

    /// If the **whole** body is a command, return it. The five that need their argument read
    /// (model / effort / mode / pwd / owner verbs) are checked by shape last. The values `model` / `effort` / `mode`
    /// accept come from `agent`'s vocabulary (the Bridge passes `deps.agent`).
    pub fn parse(msg: &Message<'_>, agent: &dyn Agent) -> Option<Cmd> {
        // Bare-word commands — checked in table order (normalize only once)
        let normalized = msg.normalized();
        if let Some((_, _, cmd)) = Self::WORDS
            .into_iter()
            .find(|(_, words, _)| words.contains(&normalized.as_str()))
        {
            return Some(cmd);
        }
        if let Some(m) = Self::value_of(msg, "model", |v| agent.canonical_model(v)) {
            return Some(Cmd::Model(m));
        }
        if let Some(l) = Self::value_of(msg, "effort", Self::listed(agent.effort_levels())) {
            return Some(Cmd::Effort(l));
        }
        if let Some(m) = Self::value_of(msg, "mode", Self::listed(agent.modes())) {
            return Some(Cmd::Mode(m));
        }
        if let Some(mode) = Self::pwd(msg) {
            return Some(Cmd::Pwd(mode));
        }
        Self::owner(msg).map(Cmd::Owner)
    }

    /// The name shown in logs and refusals.
    pub fn label(&self) -> String {
        let name = match self {
            Cmd::Stop => "stop",
            Cmd::Exit => "exit",
            Cmd::Resume => "resume",
            Cmd::Help => "help",
            Cmd::Status => "status",
            Cmd::Context => "context",
            Cmd::Usage => "usage",
            Cmd::Compact => "compact",
            Cmd::Restart => "restart",
            Cmd::Login => "login",
            Cmd::Logout => "logout",
            Cmd::Model(_) => "model",
            Cmd::Effort(_) => "effort",
            Cmd::Mode(_) => "mode",
            Cmd::Pwd(_) => "pwd",
            Cmd::Channels => "channels",
            Cmd::Machines => "machines",
            Cmd::Owner(oc) => return format!("owner-command '{}'", oc.verb),
        };
        format!("'{name}'")
    }

    /// Shared parsing for the three shaped as "bare verb = show current value / verb + **known value** = set"
    /// (`model` / `effort` / `mode`). Returns
    /// None = not a command / `Some(None)` = bare verb / `Some(Some(value))` = set.
    /// Case-insensitive; an unknown value (one for which `accept` returns None) is not a command (it reaches the agent as a sentence).
    fn value_of(
        msg: &Message<'_>,
        verb: &str,
        accept: impl Fn(&str) -> Option<String>,
    ) -> Option<Option<String>> {
        let args = msg.verb_args(verb)?;
        if args.len() > 1 {
            return None;
        }
        let Some(raw) = args.first() else {
            return Some(None);
        };
        accept(&raw.to_lowercase()).map(Some)
    }

    /// An `accept` that takes only values in the list (for `effort` / `mode`).
    fn listed(known: &'static [&'static str]) -> impl Fn(&str) -> Option<String> {
        move |v| known.contains(&v).then(|| v.to_string())
    }

    /// Parse as a `pwd` command. Returns None if it isn't one, so "change how pwd works" reaches the agent
    /// as a sentence. A path may contain spaces, so **all** the rest is joined as the path
    /// (`pwd` has no label argument, so nothing can be confused). `~…` is shaped like a path too, so it fires
    /// and gets the "use an absolute path" answer (better than vanishing into the agent's turn).
    /// A path starts with `/`, `~` or `.`. Any other first word names a machine — `pwd dev` means the
    /// machine *dev*, never a folder called dev (write `./dev` or `~/dev` for that). A machine form is
    /// one word (`pwd dev`) or a word with a colon (`pwd dev:~/a b`); anything else is a sentence.
    pub(crate) fn pwd(msg: &Message<'_>) -> Option<PwdMode> {
        let args = msg.verb_args("pwd")?;
        let Some(first) = args.first() else {
            return Some(PwdMode::Current);
        };
        // `pwd all` is gone — `channels` shows every channel with its machine and folder
        if args.len() == 1 && first.to_lowercase() == "all" {
            return Some(PwdMode::Usage);
        }
        if first.starts_with(['/', '~', '.']) {
            return Some(PwdMode::Set(args.join(" ")));
        }
        let joined = args.join(" ");
        let (machine, path) = match joined.split_once(':') {
            Some((m, p)) => (m.to_string(), Some(p.trim().to_string()).filter(|p| !p.is_empty())),
            None if args.len() == 1 => (joined, None),
            None => return Some(PwdMode::Usage),
        };
        Some(match crate::bridge::state::is_machine_name(&machine) {
            true => PwdMode::On { machine, path },
            false => PwdMode::Usage,
        })
    }

    /// Whether this is an **attempt** at the verb or just a sentence that starts with that word. Decided only by
    /// the **shape** of the first argument: `on`/`off` where a switch goes, a bot where a bot goes, nothing for verbs that take nothing.
    /// That tells "let's talk about warm" is a sentence. Whether the attempt is **complete** or the value valid is not checked —
    /// `warm on` without the channel is still an attempt and gets the usage line from dispatch.
    fn looks_like_owner(verb: &str, args: &[String]) -> bool {
        let first = args.first().map(String::as_str).unwrap_or("");
        match verb {
            "warm" => matches!(first.to_lowercase().as_str(), "on" | "off"),
            "allow-bot" | "remove-bot" => {
                SlackId::is_bot(first) || SlackId::from_user_mention(first).is_some()
            }
            // set-home takes no argument shape, so the whole message must be the bare word
            "set-home" => args.is_empty(),
            _ => false,
        }
    }

    /// Parse as an Owner management command. Some only when the first word is a verb **and** the rest has
    /// that verb's argument shape. Bot / channel mentions contain no spaces, so a plain split doesn't break them.
    fn owner(msg: &Message<'_>) -> Option<OwnerCmd> {
        OWNER_COMMAND_VERBS.iter().find_map(|verb| {
            let args = msg.verb_args(verb)?;
            Self::looks_like_owner(verb, &args).then_some(OwnerCmd { verb, args })
        })
    }

    // ── A command is a **button** — it fires when pressed, or not at all
    // A command has side effects the sender expects "now",
    // but a message can arrive long after it was sent (Slack redelivers events it got no receipt for,
    // and hands over everything posted while we were down). Normal requests are unaffected — a request
    // that had to wait for a restart is still delivered.

    /// Why this command must **not** run, or None if it may. An unreadable `ts` never
    /// blocks (fail-open: a command the Owner just pressed must always work).
    pub fn stale_reason(
        &self,
        message_ts: &str,
        bridge_started_at_ms: u64,
        retry_num: u32,
    ) -> Option<String> {
        if retry_num > 0 {
            return Some(format!("Slack redelivered it (retry {retry_num})"));
        }
        let posted_at_ms = message_ts.parse::<f64>().ok()? * 1000.0;
        if !posted_at_ms.is_finite() || posted_at_ms <= 0.0 {
            return None;
        }
        let started = bridge_started_at_ms as f64;
        (posted_at_ms < started).then(|| {
            let secs = ((started - posted_at_ms) / 1000.0).round() as i64;
            format!("posted {secs}s before this Bridge started listening")
        })
    }
}

/// The three forms a parsed `pwd` can take.
///
/// There is no "invalid argument" form — an unreadable argument means it was never a command.
#[derive(Debug, PartialEq, Eq)]
pub enum PwdMode {
    Current,
    Set(String),
    /// `pwd <machine>` / `pwd <machine>:` / `pwd <machine>:<path>` — hand this channel to a machine, and
    /// with a path, set its project folder there. **The gateway answers these** (it's the one that knows
    /// the machines); a Bridge that sees one has no machine by that name.
    On { machine: String, path: Option<String> },
    /// The message starts with `pwd` but what follows is none of the forms. **Answer with the usage**
    /// rather than handing it to the agent: a mistyped path used to vanish into the conversation.
    Usage,
}

// Access management is not an MCP tool: the Owner sends a plain message and the Bridge parses it here and
// runs it **before delivery** (it never reaches the agent = zero prompt-injection surface).

const OWNER_COMMAND_VERBS: [&str; 4] = ["allow-bot", "remove-bot", "set-home", "warm"];

/// One parsed Owner command (`allow-bot U123` etc.).
///
/// `verb` is one of [`OWNER_COMMAND_VERBS`] — an unknown word never forms a command, so
/// an "invalid verb" state does not exist.
#[derive(Debug, PartialEq, Eq)]
pub struct OwnerCmd {
    pub verb: &'static str,
    pub args: Vec<String>,
}

// ── Section 3: usage-limit watch ─────────────────────────────────────────────

/// The decisions for reading `/usage` periodically and noticing that a limit is near or hit.
///
/// "When to read (interval)", "where to warn (thresholds)" and "when it is blocked (reset)" act together,
/// so they live in one place.
pub struct UsageWatch;

impl UsageWatch {
    /// Usage thresholds at which to warn.
    const WARN_THRESHOLDS: [u32; 2] = [80, 90];
    /// The normal interval, and the interval when hitting the limit is expected.
    pub const POLL_MS: u64 = 60 * 60 * 1000;
    pub const POLL_AT_RISK_MS: u64 = 15 * 60 * 1000;

    /// Thresholds above the last warned one that were **newly crossed this time** (`newlyCrossedThresholds`).
    /// Usage only rises within a window, so a drop means the window changed — the caller resets
    /// `last_warned` to 0 to re-arm the warnings.
    pub fn newly_crossed(usage_pct: u32, last_warned: u32) -> Vec<u32> {
        Self::WARN_THRESHOLDS
            .into_iter()
            .filter(|t| usage_pct >= *t && *t > last_warned)
            .collect()
    }

    /// The **latest** reset time among the windows that have hit their limit (`bindingLimitResetEpoch`).
    ///
    /// Every row is checked — the weekly wall applies even when session usage is low. Following the earlier reset
    /// would run into a wall that is still up, so take the **later** one. None if no window has hit its limit.
    pub fn binding_limit_reset(rows: &[UsageRow], now_ms: u64, limit_pct: f64) -> Option<u64> {
        rows.iter()
            .filter(|r| r.pct.parse::<f64>().unwrap_or(0.0) >= limit_pct)
            .filter_map(|r| WallClock::parse_reset_epoch(&r.reset, now_ms))
            .filter(|reset| *reset > now_ms)
            .max()
    }
}


// ── Section 4: tool permission — standing rules applied before asking a human ─
// The gatekeeper that makes sure only tools worth asking about are asked about. **Without it** the agent asks a
// human for permission to use its own reply tool (`reply`) = asking on Slack "may I answer on Slack?",
// and it stalls with nobody pressing the button.

/// The tool prefix of this agent's own MCP server. It matches **this implementation's server name**
/// (`agentgw` in `--mcp-config`).
const OWN_MCP_PREFIX: &str = "mcp__agentgw__";

/// The standing rule's answer. `Ask` = the rules don't decide (ask a human).
#[derive(Debug, PartialEq, Eq)]
pub enum ToolPermission {
    /// Allow without asking. The string is the reason (carried in the message returned to the agent).
    Allow(&'static str),
    /// Deny without asking. Things that must never be put to a human as "is this OK?".
    Deny(&'static str),
    /// The rules don't decide — ask a human.
    Ask,
}

impl ToolPermission {
    /// Apply the standing rules. **Pure function** — the state directory is used only to recognise our own files.
    pub fn decide(tool_name: &str, tool_input: &serde_json::Value, state_dir: &Path) -> Self {
        let field = |k: &str| tool_input.get(k).and_then(|v| v.as_str()).unwrap_or("");
        let (skill, file_path) = (field("skill"), field("file_path"));
        let threads_file = state_dir.join("threads.json");
        let access_file = state_dir.join("access.json");
        let is =
            |p: &std::path::Path| !file_path.is_empty() && std::path::Path::new(file_path) == p;

        // Self-trust: the agent's only way to answer is its own tool, so asking a human for `reply`
        // permission is asking on Slack "may I answer on Slack?". Execution rights are the Bridge's,
        // and the Bridge decides at run time what is allowed
        if tool_name.starts_with(OWN_MCP_PREFIX) {
            return Self::Allow("own MCP tool");
        }
        if tool_name == "Skill" && skill.starts_with("agentgw:") {
            return Self::Allow("own skill");
        }
        if tool_name == "WebSearch" || tool_name == "WebFetch" {
            return Self::Allow("read-only web tool");
        }
        if tool_name == "Read" && (is(&threads_file) || is(&access_file)) {
            return Self::Allow("own state read");
        }

        // Structural denial: access belongs to the Owner and only the Owner's DM commands change it
        // (the Bridge parses them itself). An agent writing it directly is privilege escalation,
        // so deny on the spot without asking a human "allow this?"
        if (tool_name == "Edit" || tool_name == "Write")
            && (is(&access_file) || file_path.ends_with("/.agentgw/access.json"))
        {
            return Self::Deny(
                "a worker cannot edit access.json directly — access changes are the Owner\u{2019}s DM commands",
            );
        }

        Self::Ask
    }

    /// The permission hook's response. Claude Code v2 wants `{decision:{behavior}}` nested inside
    /// `hookSpecificOutput` — a **different shape** from stop's flat one
    pub fn decision_output(behavior: &str, message: &str) -> serde_json::Value {
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": { "behavior": behavior, "message": message },
            }
        })
    }
}

/// Sign-in / sign-out progress. Only `command/agent.rs` touches it.
#[derive(Default)]
pub(super) struct SignIn {
    /// Sign-in waiting for a code: channel → the person who started it. **Only one at a time**
    /// (there is one login session — letting a second through would make the first person Owner via the later code)
    pending: HashMap<String, String>,
    /// Whether a sign-out is running. Prevents a double `logout` from running `claude auth logout` twice,
    /// with the second teardown tripping over the first one's cleanup
    signing_out: bool,
    /// Sign-in expiry watch: when it was last checked (0 = not yet) and the last known state
    /// (`None` = not known yet. Notify once, at the moment it is found expired)
    checked_at_ms: u64,
    last_known: Option<bool>,
}

impl SignIn {
    /// A sign-in started in `channel` is waiting for its pasted code.
    pub(super) fn awaiting_code(&self, channel: &str) -> bool {
        self.pending.contains_key(channel)
    }
}

// ── running commands ─────────────────────────────────────────────────────────

/// A state-change note that a spawned command sends back to the main loop.
///
/// Sign-in and sign-out run for tens of seconds inside a spawned task (waiting on the browser round trip),
/// so state updates are **handed back** to the main loop — tmux and polling belong to the task, access.json and
/// the sign-in state (`SignIn`) to main (the same shape as dispo_rx).
pub(super) enum CmdFx {
    /// The outcome of a sign-in. Success or failure, the login session is torn down and the pending seat freed
    /// (the **start** is registered synchronously by main — leaving the seat-taking to the spawn lets a second one steal it).
    /// `bound` = the person to make Owner
    LoginFinished {
        channel: String,
        bound: Option<String>,
    },
    LogoutFinished {
        ok: bool,
        channel: String,
        thread_ts: String,
    },
    /// A window right after startup showed **a screen nobody can answer**. tmux polling belongs to the task,
    /// the gate and notification to main (the same shape as LoginFinished).
    SpawnScreen {
        outcome: SpawnOutcome,
        /// Who to name in the log (`thread=…` / `pool session=…`)
        what: String,
        /// The thread waiting on this agent. None for a warm pool agent (nobody is waiting yet)
        key: Option<ThreadKey>,
        session_id: String,
    },
}

impl Bridge {
    /// Hand a command to the gateway, which answers in the thread itself. `false` = there is no gateway
    /// (a Bridge on its own), so the caller says what it can.
    fn ask_the_gateway(
        &self,
        frame: crate::bridge::gateway::link::LinkFrame,
        ctx: &LogCtx,
    ) -> bool {
        let Some(up) = &self.ask_gateway else {
            return false;
        };
        match up.send(frame) {
            Ok(()) => true,
            Err(e) => {
                ctx.error("bridge", &format!("could not ask the gateway: {e}"));
                false
            }
        }
    }

    /// Body commands the Bridge answers itself. **true = consumed** — the caller stops there.
    ///
    /// Authorization is a single rule: is the sender the Owner. Regardless of channel or DM,
    /// or whether we were mentioned. A command from anyone else is logged and dropped — if it fell through to the agent
    /// it would become "an unanswered message respawned forever" and clog the thread.
    pub(super) async fn handle_command(&mut self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str) -> bool {
        // Cmd::parse does all the detection — if it matches nothing, it is not a command
        let msg_body = crate::bridge::command::Message::new(&msg.text, self.bot_user_id.as_deref());
        let Some(cmd) = crate::bridge::command::Cmd::parse(&msg_body, self.deps.agent.as_ref()) else {
            return false;
        };
        let label = cmd.label();

        let ctx = LogCtx {
            session_id: None,
            thread_key: Some(key.clone()),
        };
        let sender = msg.user.as_deref().unwrap_or("");
        // Today the gate drops non-Owners first, so this is never reached. Kept anyway — the original had
        // a context delivery that lets the agent "read" non-Owner messages, and the day that is
        // ported the gate starts letting non-Owners through. Command authorization will still be **here** then
        if self.access.owner.is_empty() || msg.user.as_deref() != Some(self.access.owner.as_str()) {
            // Only `login` carries one more piece of context — it lands here because it arrived while an Owner
            // **already exists** (without one, the shortcut before the gate handles it)
            let note = match cmd {
                Cmd::Login => " while an Owner is bound",
                _ => "",
            };
            ctx.info(
                "bridge",
                &format!(
                    "slack-events: {label} from non-owner {sender}{note} — \
                     ignoring (not delivered to worker) msg={}",
                    msg.ts
                ),
            );
            return true;
        }
        let dm = msg.channel_kind == crate::chat::ChannelKind::Dm;

        match cmd {
            // stop. ESC goes through tmux, so it
            // works even when MCP is dead — which is usually exactly when you want to stop
            Cmd::Stop => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: stop command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_stop(msg, key, root_ts, &ctx);
            }
            // exit. Ends **only the agent** — the thread stays
            Cmd::Exit => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: exit command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_exit(msg, key, root_ts, &ctx).await;
            }
            // resume. Hand the line over to the local terminal
            Cmd::Resume => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: resume command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_resume(msg, key, root_ts, &ctx);
            }
            Cmd::Help => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: help command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.post(
                    &msg.channel,
                    root_ts,
                    // Any machine with a gateway can hand its channel over — it passes the command up
                    bridge::help(
                        self.fleet || self.ask_gateway.is_some(),
                        self.deps.agent.as_ref(),
                    ),
                    key,
                );
            }
            // status. Never goes through an agent, so it answers
            // even when every agent is stuck
            Cmd::Status => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: status command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_status(msg, key, root_ts, &ctx);
            }
            Cmd::Context => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: context command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_context(msg, key, root_ts, &ctx);
            }
            Cmd::Usage => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: usage command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_usage(msg, key, root_ts, &ctx);
            }
            Cmd::Compact => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: compact command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_compact(msg, key, root_ts, &ctx);
            }
            // restart. The only command that replaces
            // this Bridge itself — only the progress checklist spans the two processes
            Cmd::Restart => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: restart command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                let channel = msg.channel.clone();
                self.maintenance_restart("slack restart command", Some((&channel, root_ts)), &ctx)
                    .await;
            }
            // `login` with an Owner present. Even with an Owner set, **this machine's** Claude Code
            // sign-in can expire separately (2026-09-18: one machine had expired and there was no way to restore it from Slack).
            // So ask for the actual state, and if expired start signing in here (on this machine).
            // If already signed in, stop — falling through to the agent makes it ask back "log in to what?"
            Cmd::Login => {
                let signed_in = self.deps.agent.signed_in().await;
                if signed_in == Some(true) {
                    ctx.info(
                        "bridge",
                        &format!(
                            "slack-events: login command from Owner while already signed in — \
                             replying already-signed-in msg={} channel={} dm={dm}",
                            msg.ts, msg.channel
                        ),
                    );
                    self.post(
                        &msg.channel,
                        root_ts,
                        {
                            let owner = &self.access.owner;
                            crate::t!(
                                "Already signed in (owner: <@{owner}>). To switch accounts, send `logout`, then `login`.",
                                "既にサインインしています(Owner: <@{owner}>)。アカウントを切り替えるには、`logout` のあとに `login` を送ってください。"
                            )
                        },
                        key,
                    );
                } else {
                    ctx.info(
                        "bridge",
                        &format!(
                            "slack-events: login command from Owner — this machine's agent is {} — \
                             starting sign-in here msg={} channel={} dm={dm}",
                            if signed_in == Some(false) { "signed out" } else { "of unknown sign-in state" },
                            msg.ts,
                            msg.channel
                        ),
                    );
                    self.start_login(msg.channel.clone(), sender.to_string(), root_ts.to_string());
                }
            }
            Cmd::Logout => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: logout command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_logout(&msg.channel, root_ts);
            }
            Cmd::Model(name) => {
                let what = match &name {
                    Some(n) => format!("model command ({n})"),
                    None => "model show command".to_string(),
                };
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: {what} msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_model(msg, name, key, root_ts, &ctx);
            }
            Cmd::Effort(level) => {
                let what = match &level {
                    Some(l) => format!("effort command ({l})"),
                    None => "effort show command".to_string(),
                };
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: {what} msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_effort(msg, level, key, root_ts, &ctx);
            }
            Cmd::Mode(name) => {
                let what = match &name {
                    Some(n) => format!("mode command ({n})"),
                    None => "mode show command".to_string(),
                };
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: {what} msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_mode(msg, name, key, root_ts, &ctx);
            }
            Cmd::Pwd(mode) => {
                let kind = match mode {
                    PwdMode::Current => "current",
                    PwdMode::Set(_) => "set",
                    PwdMode::On { .. } => "on",
                    PwdMode::Usage => "usage",
                };
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: pwd command mode={kind} msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                // Which machines exist is the gateway's knowledge — ask it, and it answers in this thread
                if let PwdMode::On { machine, path } = &mode
                    && self.ask_the_gateway(
                        crate::bridge::gateway::link::LinkFrame::PwdOn {
                            channel: msg.channel.clone(),
                            thread_ts: root_ts.to_string(),
                            machine: machine.clone(),
                            path: path.clone(),
                        },
                        &ctx,
                    )
                {
                    return true;
                }
                let out = self.pwd_answer(msg, mode, dm, root_ts, &ctx);
                self.post(&msg.channel, root_ts, out, key);
            }
            // Same: only the gateway knows the machines
            Cmd::Machines => {
                ctx.info("bridge", &format!("slack-events: machines command msg={}", msg.ts));
                if !self.ask_the_gateway(
                    crate::bridge::gateway::link::LinkFrame::Machines {
                        channel: msg.channel.clone(),
                        thread_ts: root_ts.to_string(),
                    },
                    &ctx,
                ) {
                    let text = crate::t!(
                        "This machine works on its own — there are no other machines.",
                        "このマシンは単独で動いていて、ほかのマシンはありません。"
                    );
                    self.post(&msg.channel, root_ts, text, key);
                }
            }
            // Same: only the gateway knows every channel and machine
            Cmd::Channels => {
                ctx.info(
                    "bridge",
                    &format!("slack-events: channels command msg={}", msg.ts),
                );
                if !self.ask_the_gateway(
                    crate::bridge::gateway::link::LinkFrame::Channels {
                        channel: msg.channel.clone(),
                        thread_ts: root_ts.to_string(),
                    },
                    &ctx,
                ) {
                    let text = crate::t!(
                        "This machine works on its own — there are no other machines.",
                        "このマシンは単独で動いていて、ほかのマシンはありません。"
                    );
                    self.post(&msg.channel, root_ts, text, key);
                }
            }
            Cmd::Owner(oc) => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: owner-command '{}' args=[{}] msg={} channel={} dm={dm}",
                        oc.verb,
                        oc.args.join(" "),
                        msg.ts,
                        msg.channel
                    ),
                );
                self.owner_command(msg, oc, dm, root_ts, key, &ctx).await;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Threshold crossing, and choosing when to lift the gate.
    #[test]
    fn the_usage_monitor_warns_once_per_threshold_and_gates_until_the_latest_wall() {
        // Return only crossed thresholds (don't warn twice at the same one)
        assert_eq!(UsageWatch::newly_crossed(75, 0), Vec::<u32>::new());
        assert_eq!(UsageWatch::newly_crossed(80, 0), vec![80]);
        assert_eq!(
            UsageWatch::newly_crossed(95, 0),
            vec![80, 90],
            "一気に跨いだら両方"
        );
        assert_eq!(
            UsageWatch::newly_crossed(95, 80),
            vec![90],
            "済んだ節目は出さない"
        );
        assert_eq!(UsageWatch::newly_crossed(95, 90), Vec::<u32>::new());

        let now = 1_785_387_600_000u64; // 2026-07-30T05:00Z = 14:00 JST on the 30th
        let row = |label: &str, pct: &str, reset: &str| UsageRow {
            label: label.into(),
            pct: pct.into(),
            reset: reset.into(),
        };
        // No window at its limit means no gate
        assert_eq!(
            UsageWatch::binding_limit_reset(&[row("Current session", "99", "at 11pm")], now, 100.0),
            None
        );
        // The weekly wall applies even when the session is low. Block until the **later** one
        let rows = [
            row("Current session", "100", "at 4pm"),
            row("Current week (all models)", "100", "at 11pm"),
        ];
        let reset = UsageWatch::binding_limit_reset(&rows, now, 100.0).unwrap();
        assert_eq!(reset, WallClock::parse_reset_epoch("at 11pm", now).unwrap());
        // A reset already in the past is not taken (the empty reset of a 0% row drops out the same way)
        assert_eq!(
            UsageWatch::binding_limit_reset(&[row("Current session", "100", "")], now, 100.0),
            None
        );
    }

    /// Telling who is named. Messages addressed to others also flow into a running thread,
    /// so "were we named" and "was someone else named" are answered separately.
    #[test]
    fn tells_our_own_mention_apart_from_someone_elses() {
        fn m<'a>(t: &'a str) -> Message<'a> {
            Message::new(t, Some("UBOT"))
        }
        assert!(m("<@UBOT> やって").mentions_bot());
        assert!(!m("<@UBOT> やって").mentions_someone_else());
        // Addressed to someone else — we are not named
        assert!(!m("<@UOTHER> おーい").mentions_bot());
        assert!(m("<@UOTHER> おーい").mentions_someone_else());
        // Both — we are called too
        assert!(m("<@UBOT> <@UOTHER> と相談して").mentions_bot());
        assert!(m("<@UBOT> <@UOTHER> と相談して").mentions_someone_else());
        // A plain follow-up naming nobody
        assert!(!m("ありがとう").mentions_bot());
        assert!(!m("ありがとう").mentions_someone_else());
        // Same with the display-name form (`<@ID|label>`)
        assert!(m("<@UBOT|claude> やって").mentions_bot());
    }

    /// Standing rules. **Letting our own MCP tools through is the key** — without it the agent
    /// asks on Slack "may I answer on Slack?" and stalls with nobody pressing the button.
    #[test]
    fn standing_tool_policy_lets_the_worker_answer_and_refuses_access_writes() {
        use ToolPermission as P;
        let dir = std::path::Path::new("/st");
        let none = serde_json::json!({});
        let file = |p: &str| serde_json::json!({ "file_path": p });

        // Our own: don't ask
        assert_eq!(
            P::decide("mcp__agentgw__reply", &none, dir),
            P::Allow("own MCP tool")
        );
        assert_eq!(
            P::decide(
                "Skill",
                &serde_json::json!({"skill": "agentgw:status"}),
                dir
            ),
            P::Allow("own skill")
        );
        assert_eq!(
            P::decide("WebFetch", &none, dir),
            P::Allow("read-only web tool")
        );
        assert_eq!(
            P::decide("Read", &file("/st/threads.json"), dir),
            P::Allow("own state read")
        );

        // Privilege escalation is refused without asking
        assert!(matches!(
            P::decide("Write", &file("/st/access.json"), dir),
            P::Deny(_)
        ));
        assert!(matches!(
            P::decide("Edit", &file("/home/u/.agentgw/access.json"), dir),
            P::Deny(_)
        ));

        // Everything else is a human's call
        assert_eq!(
            P::decide("Bash", &serde_json::json!({"command": "rm -rf /"}), dir),
            P::Ask
        );
        assert_eq!(P::decide("Read", &file("/etc/passwd"), dir), P::Ask);
        // Other MCP servers are not let through
        assert_eq!(P::decide("mcp__other__do_thing", &none, dir), P::Ask);
    }

    /// The response shape is **different** from stop's flat one. Get the nesting wrong and Claude Code ignores it.
    #[test]
    fn perm_decision_output_is_the_nested_shape() {
        let v = ToolPermission::decision_output("allow", "Slack bridge (own MCP tool)");
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert_eq!(v["hookSpecificOutput"]["decision"]["behavior"], "allow");
        assert_eq!(
            v["hookSpecificOutput"]["decision"]["message"],
            "Slack bridge (own MCP tool)"
        );
    }



    #[test]
    fn whole_message_only_is_a_command() {
        assert!(Message::new("stop", None).is("stop"));
        assert!(Message::new("  STOP ", None).is("stop"));
        assert!(Message::new("🛑", None).is("stop")); // emoji alias
        assert!(Message::new(":red_circle:", None).is("stop"));
        assert!(!Message::new("stop the deploy", None).is("stop")); // doesn't fire mid-sentence
        assert!(Message::new("？", None).is("help")); // full-width
        assert!(Message::new("ステータス", None).is("status"));
        assert!(Message::new("bye", None).is("exit"));
        // Our own bot mention is stripped. Others' mentions are not
        assert!(Message::new("<@UBOT> stop", Some("UBOT")).is("stop"));
        assert!(!Message::new("<@UOTHER> stop", Some("UBOT")).is("stop"));
    }

    #[test]
    fn arg_commands_parse_by_shape() {
        let agent = crate::agent::fake::FakeAgent::default();
        let model = |v: &str| agent.canonical_model(v);
        let modes = Cmd::listed(agent.modes());
        assert_eq!(
            Cmd::value_of(&Message::new("model opus", None), "model", model),
            Some(Some("opus".into()))
        );
        assert_eq!(
            Cmd::value_of(&Message::new("model sonet", None), "model", model),
            Some(Some("sonnet".into()))
        );
        assert_eq!(
            Cmd::value_of(&Message::new("model", None), "model", model),
            Some(None)
        );
        assert_eq!(
            Cmd::value_of(
                &Message::new("model の説明をして", None),
                "model",
                model
            ),
            None
        ); // a sentence passes through
        assert_eq!(
            Cmd::value_of(
                &Message::new("effort xhigh", None),
                "effort",
                Cmd::listed(agent.effort_levels())
            ),
            Some(Some("xhigh".into()))
        );
        assert_eq!(
            Cmd::value_of(&Message::new("mode plan", None), "mode", &modes),
            Some(Some("plan".into()))
        );
        assert_eq!(
            Cmd::value_of(&Message::new("MODE", None), "mode", &modes),
            Some(None)
        );
        assert_eq!(
            Cmd::value_of(&Message::new("mode bypass", None), "mode", &modes),
            None
        ); // not among the arguments = a sentence
        assert_eq!(
            Cmd::value_of(&Message::new("mode を実装して", None), "mode", &modes),
            None
        );
        assert!(
            matches!(Cmd::pwd(&Message::new("pwd /a b/c", None)), Some(PwdMode::Set(p)) if p == "/a b/c")
        );
        assert_eq!(Cmd::pwd(&Message::new("pwd all", None)), Some(PwdMode::Usage));
        // Starts with pwd but isn't one of the forms → the usage, not a sentence for the agent
        assert_eq!(Cmd::pwd(&Message::new("pwd の使い方", None)), Some(PwdMode::Usage));
        // A path starts with / ~ or . — any other first word is a machine
        let on = |m: &str, p: Option<&str>| {
            Some(PwdMode::On { machine: m.into(), path: p.map(str::to_string) })
        };
        assert_eq!(Cmd::pwd(&Message::new("pwd laptop", None)), on("laptop", None));
        assert_eq!(Cmd::pwd(&Message::new("pwd laptop:", None)), on("laptop", None));
        assert_eq!(Cmd::pwd(&Message::new("pwd hub:~/a b", None)), on("hub", Some("~/a b")));
        assert_eq!(Cmd::pwd(&Message::new("pwd ./dev", None)), Some(PwdMode::Set("./dev".into())));
        assert_eq!(Cmd::pwd(&Message::new("pwd /a:b", None)), Some(PwdMode::Set("/a:b".into())));
        assert_eq!(Cmd::pwd(&Message::new("pwd is handy", None)), Some(PwdMode::Usage));
        assert_eq!(Cmd::pwd(&Message::new("pwd 何か変な値", None)), Some(PwdMode::Usage));
        assert_eq!(Cmd::pwd(&Message::new("please pwd /x", None)), None); // not the first word = a sentence
        let oc = Cmd::owner(&Message::new("warm on <#C1|general>", None)).unwrap();
        assert_eq!((oc.verb, oc.args.len()), ("warm", 2));
        assert!(Cmd::owner(&Message::new("warm の話をしよう", None)).is_none()); // no on/off = a sentence
        assert!(Cmd::owner(&Message::new("set-home", None)).is_some());
        assert!(Cmd::owner(&Message::new("set-home here", None)).is_none()); // with an argument, it's a sentence
    }

    /// Minimal net for the hand-written scans (id shape, mention, stripping our own mention).
    #[test]
    fn id_shapes_and_mentions() {
        assert!(SlackId::is_user("U012AB") && !SlackId::is_user("U") && !SlackId::is_user("BU12"));
        assert!(SlackId::is_bot("B01") && SlackId::is_channel("C01") && SlackId::is_channel("G01"));
        assert_eq!(
            SlackId::from_user_mention("<@U01|alice>").as_deref(),
            Some("U01")
        );
        assert_eq!(SlackId::from_user_mention("U01").as_deref(), Some("U01")); // a bare id works too
        assert_eq!(
            SlackId::from_channel_mention("<#C01|general>").as_deref(),
            Some("C01")
        );
        assert_eq!(SlackId::from_channel_mention("<@U01>"), None);
        // Our mention is stripped and the argument's case is preserved
        assert_eq!(
            Message::new("<@UBOT> pwd /Users/Me", Some("UBOT"))
                .verb_args("pwd")
                .unwrap(),
            ["/Users/Me"]
        );
        let agent = crate::agent::fake::FakeAgent::default();
        let is_command = |text: &str| Cmd::parse(&Message::new(text, None), &agent).is_some();
        assert!(is_command("restart") && is_command("pwd all"));
        assert!(!is_command("restart してください"));
    }

    #[test]
    fn stale_command_detection() {
        // A ts posted before startup (=100_000ms) is dead as a command
        assert!(Cmd::Stop.stale_reason("99.000000", 100_000, 0).is_some());
        assert!(Cmd::Stop.stale_reason("101.000000", 100_000, 0).is_none());
        assert!(Cmd::Stop.stale_reason("101.000000", 100_000, 1).is_some()); // redelivery
        assert!(Cmd::Stop.stale_reason("garbage", 100_000, 0).is_none()); // fail-open
    }

}
