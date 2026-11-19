//! What each thread's agent is doing right now, for `thread_activity`: the agent in the thread that
//! asked reads it instead of asking (which would interrupt) or guessing from "Working…".
//!
//! Kept apart from the progress message: that one is folded and cleared at the end of each turn, and
//! the question "what did it last do" is asked exactly when nothing is running.
//!
//! **Only read by other agents**, so everything that can carry a secret (a command line, a tool's
//! output) is cut short and passed through [`redact`] before it leaves.

use crate::chat::ThreadKey;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Shared between the main loop (the only writer) and the link handler that answers the gateway.
pub type ActivityRef = Arc<Mutex<ActivityBook>>;

/// How much of a command line goes out.
const DETAIL_MAX: usize = 200;
/// How much of a tool's output goes out (its last lines: a test run ends with its tally).
const RESULT_MAX: usize = 300;
/// How much output is looked at before cutting — a secret cut in half is not caught by [`redact`].
const SCAN_MAX: usize = 4096;

#[derive(Clone, Debug)]
struct ToolRun {
    id: String,
    name: String,
    detail: String,
    started_ms: u64,
}

#[derive(Clone, Debug)]
struct ToolDone {
    name: String,
    detail: String,
    outcome: &'static str,
    result: Option<String>,
    ended_ms: u64,
}

#[derive(Clone, Debug, Default)]
struct ThreadActivity {
    running: Vec<ToolRun>,
    last: Option<ToolDone>,
    in_turn: bool,
    turn_started_ms: Option<u64>,
    turn_ended_ms: Option<u64>,
    turn_failed: Option<String>,
    waiting_for_person: bool,
    last_report_ms: Option<u64>,
    replied_in_turn: bool,
    last_activity_ms: u64,
    session_ended: bool,
    /// The agent's window went away in the middle of its work (it crashed, or its tmux server did).
    /// Cleared when the next turn starts.
    lost: bool,
}

/// One row per thread this Bridge has seen work in since it started (memory only).
#[derive(Debug, Default)]
pub struct ActivityBook {
    threads: HashMap<ThreadKey, ThreadActivity>,
}

impl ActivityBook {
    fn row(&mut self, key: &ThreadKey, now: u64) -> &mut ThreadActivity {
        let row = self.threads.entry(key.clone()).or_default();
        row.last_activity_ms = now;
        row
    }

    /// The agent took in a message: a turn is under way.
    pub fn turn_start(&mut self, key: &ThreadKey, now: u64) {
        let row = self.row(key, now);
        if !row.in_turn {
            row.turn_started_ms = Some(now);
            row.replied_in_turn = false;
        }
        row.in_turn = true;
        row.turn_ended_ms = None;
        row.turn_failed = None;
        row.session_ended = false;
        row.lost = false;
    }

    /// The turn is over. A tool still marked running was cut off (no PostToolUse comes for it).
    pub fn turn_end(&mut self, key: &ThreadKey, now: u64) {
        let row = self.row(key, now);
        row.in_turn = false;
        row.turn_ended_ms = Some(now);
        row.running.clear();
        row.waiting_for_person = false;
    }

    /// The turn failed (`StopFailure`); `why` is its error type.
    pub fn turn_failed(&mut self, key: &ThreadKey, why: &str, now: u64) {
        self.turn_end(key, now);
        if let Some(row) = self.threads.get_mut(key) {
            row.turn_failed = Some(if why.is_empty() { "unknown".into() } else { why.to_string() });
        }
    }

    pub fn session_end(&mut self, key: &ThreadKey, now: u64) {
        self.turn_end(key, now);
        if let Some(row) = self.threads.get_mut(key) {
            row.session_ended = true;
        }
    }

    /// The agent's window went away while it was at work: nothing will end its turn now.
    pub fn agent_lost(&mut self, key: &ThreadKey, now: u64) {
        self.turn_end(key, now);
        if let Some(row) = self.threads.get_mut(key) {
            row.lost = true;
        }
    }

    /// Whether the book says this thread's agent is at work: in a turn, a tool running, or waiting
    /// on a person. Only then can a missing window mean it died mid-work.
    pub fn busy(&self, key: &ThreadKey) -> bool {
        self.settled(key) == Some(false)
    }

    /// [`Self::snapshot`], but **checking that a busy agent's window is still there** first.
    /// The book only hears hooks, and a dead agent sends none: without this a crashed agent stays
    /// `running_tool` forever. `alive` is asked only when the book says the agent is at work.
    ///
    /// **Reads only.** The book is left as it is, so the cleanup round still sees the agent at work
    /// and tells the thread (`notice_lost_agents`): marking it lost here made a look at the thread
    /// swallow the notice.
    pub fn snapshot_checked(
        &self,
        key: &ThreadKey,
        has_agent: bool,
        alive: impl FnOnce() -> bool,
        now: u64,
    ) -> serde_json::Value {
        if has_agent && self.busy(key) && !alive() {
            let mut seen = ActivityBook::default();
            if let Some(row) = self.threads.get(key) {
                seen.threads.insert(key.clone(), row.clone());
            }
            seen.agent_lost(key, now);
            return seen.snapshot(key, has_agent, now);
        }
        self.snapshot(key, has_agent, now)
    }

    /// A permission prompt or a question is up (`true`), or answered (`false`).
    pub fn waiting_for_person(&mut self, key: &ThreadKey, on: bool, now: u64) {
        self.row(key, now).waiting_for_person = on;
    }

    /// The agent posted to the thread (`reply`, `edit_message`).
    pub fn report(&mut self, key: &ThreadKey, now: u64) {
        let row = self.row(key, now);
        row.last_report_ms = Some(now);
        row.replied_in_turn = true;
    }

    /// Whether this thread's agent is known to be done for now: a turn was seen ending since this
    /// Bridge started, and nothing is running or waiting on a person. `None` = nothing seen since
    /// start (an agent inherited across a restart), so its state is unknown.
    pub fn settled(&self, key: &ThreadKey) -> Option<bool> {
        let r = self.threads.get(key)?;
        if r.in_turn || !r.running.is_empty() || r.waiting_for_person {
            return Some(false);
        }
        r.turn_ended_ms.is_some().then_some(true)
    }

    /// PreToolUse.
    pub fn tool_start(&mut self, key: &ThreadKey, id: &str, name: &str, detail: &str, now: u64) {
        let row = self.row(key, now);
        // A tool call means a turn is running, even if its start was missed (a restart in between)
        row.in_turn = true;
        row.turn_ended_ms = None;
        row.running.retain(|r| r.id != id);
        row.running.push(ToolRun {
            id: id.to_string(),
            name: name.to_string(),
            detail: detail.to_string(),
            started_ms: now,
        });
    }

    /// PostToolUse / PostToolUseFailure. `outcome` is `ok`, `error` or `denied`.
    pub fn tool_done(
        &mut self,
        key: &ThreadKey,
        id: &str,
        name: &str,
        detail: &str,
        outcome: &'static str,
        result: Option<String>,
        now: u64,
    ) {
        let row = self.row(key, now);
        row.running.retain(|r| r.id != id);
        row.last = Some(ToolDone {
            name: name.to_string(),
            detail: detail.to_string(),
            outcome,
            result,
            ended_ms: now,
        });
    }

    /// What the thread's agent is doing, for another agent to read. `has_agent` is whether the
    /// thread still has an agent (threads.json); without one the state is `no_agent` whatever the
    /// book remembers. **Always has a `state` and a `summary`** — "nothing" is an answer too.
    pub fn snapshot(&self, key: &ThreadKey, has_agent: bool, now: u64) -> serde_json::Value {
        let secs = |ms: u64| now.saturating_sub(ms) / 1000;
        let Some(row) = self.threads.get(key) else {
            let (state, summary) = if has_agent {
                ("idle", "No activity seen since this machine's agentgw started; the agent is waiting for a message.".to_string())
            } else {
                ("no_agent", "This thread has no agent running (it ended, or was never started).".to_string())
            };
            return serde_json::json!({"state": state, "summary": summary});
        };
        let running: Vec<serde_json::Value> = row
            .running
            .iter()
            .map(|r| serde_json::json!({"tool": r.name, "detail": r.detail, "seconds": secs(r.started_ms)}))
            .collect();
        let last = row.last.as_ref().map(|d| {
            let mut v = serde_json::json!({
                "tool": d.name,
                "detail": d.detail,
                "outcome": d.outcome,
                "ended_seconds_ago": secs(d.ended_ms),
            });
            if let Some(r) = &d.result {
                v["result"] = r.clone().into();
            }
            v
        });
        let last_words = |prefix: &str| match &row.last {
            Some(d) => format!("{prefix} {}s since {} finished", secs(d.ended_ms), d.name),
            None => match row.turn_started_ms {
                Some(t) => format!("{prefix} {}s into the turn, no tool yet", secs(t)),
                None => prefix.to_string(),
            },
        };
        let (state, summary) = if !has_agent || row.session_ended {
            ("no_agent", "This thread's agent has ended.".to_string())
        } else if row.lost {
            (
                "agent_gone",
                "This thread's agent stopped unexpectedly in the middle of its work (its window is gone). \
                 It will not answer by itself; a new message to the thread starts it again from the conversation."
                    .to_string(),
            )
        } else if row.waiting_for_person {
            ("waiting_for_person", "Waiting for a person to answer a permission prompt or a question in the thread.".to_string())
        } else if let Some(r) = row.running.iter().min_by_key(|r| r.started_ms) {
            let more = match row.running.len() {
                1 => String::new(),
                n => format!(" (+{} more running)", n - 1),
            };
            let what = if r.detail.is_empty() { String::new() } else { format!(": {}", first_line(&r.detail)) };
            ("running_tool", format!("Running {} for {}s{what}{more}", r.name, secs(r.started_ms)))
        } else if row.in_turn {
            ("thinking", last_words("Thinking:"))
        } else if let Some(why) = &row.turn_failed {
            ("turn_failed", format!("The last turn failed ({why}); waiting for a message."))
        } else {
            let ended = row.turn_ended_ms.map(|t| format!(" {}s ago", secs(t))).unwrap_or_default();
            let replied = if row.replied_in_turn { "replied" } else { "without replying" };
            ("turn_ended", format!("Turn ended{ended}, {replied}; waiting for a message."))
        };
        let mut out = serde_json::json!({
            "state": state,
            "summary": summary,
            "running": running,
            "last_tool": last,
            "last_report": row.last_report_ms.map(|t| serde_json::json!({"at": crate::clock::iso8601(t), "seconds_ago": secs(t)})),
            "last_activity_seconds_ago": secs(row.last_activity_ms),
        });
        if row.in_turn {
            out["turn_started_seconds_ago"] = row.turn_started_ms.map(secs).into();
        } else if let Some(t) = row.turn_ended_ms {
            out["turn_ended_seconds_ago"] = secs(t).into();
            out["replied_in_last_turn"] = row.replied_in_turn.into();
        }
        out
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or_default()
}

/// A command line or a path as it goes out: secrets masked, then cut to [`DETAIL_MAX`].
pub fn detail(s: &str) -> String {
    clip_head(&redact(&clip_head(s, SCAN_MAX)), DETAIL_MAX)
}

/// The end of a tool's output as it goes out: secrets masked, then its last lines up to [`RESULT_MAX`].
pub fn result_tail(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() || s == "null" {
        return None;
    }
    let start = s.len().saturating_sub(SCAN_MAX);
    let start = (start..=s.len()).find(|&i| s.is_char_boundary(i)).unwrap_or(s.len());
    let clean = redact(&s[start..]);
    let chars: Vec<char> = clean.chars().collect();
    if chars.len() <= RESULT_MAX {
        return Some(clean);
    }
    Some(format!("…{}", chars[chars.len() - RESULT_MAX..].iter().collect::<String>()))
}

fn clip_head(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if out.len() < s.len() {
        out.push('…');
    }
    out
}

/// Prefixes of tokens issued by well-known services.
const TOKEN_PREFIXES: [&str; 18] = [
    "xoxb-", "xoxp-", "xoxa-", "xoxe-", "xoxr-", "xapp-", "ghp_", "gho_", "ghs_", "ghu_", "ghr_",
    "github_pat_", "glpat-", "sk-", "lin_api_", "AKIA", "AIza", "hf_",
];

/// Words in a variable or field name that say its value is a secret.
const SECRET_NAMES: [&str; 10] = [
    "TOKEN", "SECRET", "PASSWORD", "PASSWD", "API_KEY", "APIKEY", "ACCESS_KEY", "PRIVATE_KEY", "CREDENTIAL",
    "AUTHORIZATION",
];

/// Flags whose next word is a secret.
const SECRET_FLAGS: [&str; 7] = ["--token", "--password", "--passwd", "--api-key", "--secret", "--auth", "--key"];

/// Commands that take a password glued to `-p` (`mysql -pS3cret`). Only these: elsewhere `-p…` is an
/// ordinary flag (`cp -pr`, `mkdir -p`, `cargo test -p name`).
const GLUED_PASSWORD_COMMANDS: [&str; 9] = [
    "mysql", "mysqldump", "mysqladmin", "mysqlimport", "mysqlshow", "mysqlcheck", "mariadb", "mariadb-dump",
    "sshpass",
];

const MASK: &str = "***";

/// The word ends one command in a shell line, so the next word starts another.
fn ends_command(w: &str) -> bool {
    matches!(w, "|" | "||" | "&&" | ";" | "&") || w.ends_with(';')
}

fn is_glued_password_command(w: &str) -> bool {
    let name = w.trim_matches(|c: char| c == '"' || c == '\'' || c == '(');
    let name = name.rsplit('/').next().unwrap_or(name);
    GLUED_PASSWORD_COMMANDS.contains(&name)
}

fn is_secret_name(name: &str) -> bool {
    let n = name
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '-' || c == '$' || c == '{' || c == '}')
        .to_ascii_uppercase()
        .replace('-', "_");
    !n.is_empty() && (SECRET_NAMES.iter().any(|w| n.contains(w)) || n.ends_with("_KEY") || n == "KEY")
}

/// Masks what looks like a secret: tokens with a known prefix, `NAME=value` and `name: value` where
/// the name says secret, the word after `--token`-style flags and after `Bearer`/`Basic`, the password
/// in a URL, and long random-looking strings. Whitespace is kept as it was.
pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut st = RedactState::default();
    let mut word = String::new();
    for c in s.chars() {
        if c.is_whitespace() {
            st.flush(&mut word, &mut out);
            out.push(c);
        } else {
            word.push(c);
        }
    }
    st.flush(&mut word, &mut out);
    out
}

#[derive(Default)]
struct RedactState {
    /// The previous word said the next one is a secret (`--token`, `Bearer`, `password:`).
    mask_next: bool,
    /// Inside a quoted secret that spans words: masked up to this closing quote.
    until_quote: Option<char>,
    /// The current command is one that takes `-p<password>` (`mysql`, `sshpass`).
    password_command: bool,
    /// The current command is `sshpass`, whose `-p` may also take the password as the next word.
    sshpass: bool,
}

impl RedactState {
    fn flush(&mut self, word: &mut String, out: &mut String) {
        if word.is_empty() {
            return;
        }
        let w = std::mem::take(word);
        if let Some(q) = self.until_quote {
            match w.find(q) {
                Some(i) => {
                    self.until_quote = None;
                    out.push_str(MASK);
                    out.push_str(&w[i..]);
                }
                None => out.push_str(MASK),
            }
            return;
        }
        if self.password_command
            && let Some(pw) = w.strip_prefix("-p")
            && !pw.is_empty()
        {
            // mysql -pS3cret / sshpass -pS3cret
            let (open, close) = quote_ends(pw);
            out.push_str("-p");
            out.push_str(open);
            out.push_str(MASK);
            out.push_str(close);
            if let Some(q) = open.chars().next().filter(|_| close.is_empty()) {
                self.until_quote = Some(q);
            }
            if ends_command(&w) {
                self.password_command = false;
                self.sshpass = false;
            }
            return;
        } else if ends_command(&w) {
            self.password_command = false;
            self.sshpass = false;
        } else if is_glued_password_command(&w) {
            self.password_command = true;
            self.sshpass = w.ends_with("sshpass");
        } else if self.sshpass && w == "-p" {
            // sshpass -p S3cret
            self.mask_next = true;
            out.push_str(&w);
            return;
        }
        let bare = w.trim_matches(|c: char| c == '"' || c == '\'');
        let lower = bare.to_ascii_lowercase();
        let scheme = matches!(lower.as_str(), "bearer" | "basic" | "token");
        if self.mask_next && !scheme && w.chars().any(|c| c.is_alphanumeric()) {
            self.mask_next = false;
            let (open, close) = quote_ends(&w);
            out.push_str(open);
            out.push_str(MASK);
            out.push_str(close);
            if let Some(q) = open.chars().next().filter(|_| close.is_empty()) {
                self.until_quote = Some(q);
            }
            return;
        }
        self.mask_next = (self.mask_next && scheme)
            || lower == "bearer"
            || lower == "basic"
            || SECRET_FLAGS.contains(&lower.as_str())
            || (bare.ends_with(':') && is_secret_name(bare.trim_end_matches(':')))
            || (bare.ends_with('=') && is_secret_name(bare.trim_end_matches('=')));
        // NAME="value with spaces": the rest of the quote is the secret too
        if let Some((name, value)) = w.split_once('=')
            && is_secret_name(name)
            && let Some(q) = value.chars().next().filter(|c| *c == '"' || *c == '\'')
            && !value[1..].contains(q)
        {
            self.until_quote = Some(q);
        }
        out.push_str(&redact_word(&w));
    }
}

/// The quotes around a word, kept when the word is masked.
fn quote_ends(w: &str) -> (&str, &str) {
    let open = if w.starts_with('"') || w.starts_with('\'') { &w[..1] } else { "" };
    let close = match w.trim_end_matches([',', ';']) {
        t if t.len() > open.len() && (t.ends_with('"') || t.ends_with('\'')) => &w[t.len() - 1..],
        _ => &w[w.len() - (w.len() - w.trim_end_matches([',', ';']).len())..],
    };
    (open, close)
}

/// One whitespace-free word.
fn redact_word(w: &str) -> String {
    // NAME=value (also --name=value, "name=value")
    if let Some((name, value)) = w.split_once('=')
        && !value.is_empty()
        && is_secret_name(name.rsplit(['&', '?']).next().unwrap_or(name))
    {
        let (_, close) = quote_ends(value);
        return format!("{name}={MASK}{close}");
    }
    let mut w = w.to_string();
    // scheme://user:password@host
    if let Some(at_scheme) = w.find("://") {
        let rest_start = at_scheme + 3;
        let rest = &w[rest_start..];
        let host_end = rest.find('/').unwrap_or(rest.len());
        if let Some(at) = rest[..host_end].rfind('@')
            && let Some(colon) = rest[..at].find(':')
        {
            w.replace_range(rest_start + colon + 1..rest_start + at, MASK);
        }
    }
    let w = mask_jwts(&w);
    let w = mask_prefixed(&w);
    mask_blobs(&w)
}

/// A JWT (`eyJ….eyJ….sig`) is masked whole: its middle part is readable JSON that can name a person.
fn mask_jwts(w: &str) -> String {
    let is_jwt_char = |c: char| is_token_char(c) || c == '.';
    let mut out = String::new();
    let mut i = 0;
    while i < w.len() {
        let at_start = i == 0 || !w[..i].chars().next_back().is_some_and(is_jwt_char);
        if at_start && w[i..].starts_with("eyJ") {
            let len = w[i..].find(|c: char| !is_jwt_char(c)).unwrap_or(w.len() - i);
            let parts: Vec<&str> = w[i..i + len].split('.').collect();
            if parts.len() >= 3 && parts[0].len() >= 10 && !parts[1].is_empty() {
                out.push_str(MASK);
                i += len;
                continue;
            }
        }
        let c = w[i..].chars().next().unwrap_or_default();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// `xoxb-…`, `ghp_…` and the like: the prefix stays (it says what kind of secret it was).
fn mask_prefixed(w: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    'scan: while i < w.len() {
        let at_start = i == 0 || !w[..i].chars().next_back().is_some_and(is_token_char);
        if at_start {
            for p in TOKEN_PREFIXES {
                if w[i..].starts_with(p) {
                    let tail = w[i + p.len()..].find(|c: char| !is_token_char(c)).unwrap_or(w.len() - i - p.len());
                    if tail >= 8 {
                        out.push_str(p);
                        out.push_str(MASK);
                        i += p.len() + tail;
                        continue 'scan;
                    }
                }
            }
        }
        let c = w[i..].chars().next().unwrap_or_default();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// A run of 32+ letters and digits mixing upper case, lower case and digits looks like a key, not a
/// word. Plain hex (commit ids, hashes) is left alone: it is what a progress check wants to see.
fn mask_blobs(w: &str) -> String {
    let is_blob_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '_' | '-' | '=');
    let mut out = String::new();
    let mut run = String::new();
    let take = |run: &mut String, out: &mut String| {
        let r = std::mem::take(run);
        let looks_random = r.len() >= 32
            && r.chars().any(|c| c.is_ascii_uppercase())
            && r.chars().any(|c| c.is_ascii_lowercase())
            && r.chars().any(|c| c.is_ascii_digit())
            && !r.contains('/');
        out.push_str(if looks_random { MASK } else { &r });
    };
    for c in w.chars() {
        if is_blob_char(c) {
            run.push(c);
        } else {
            take(&mut run, &mut out);
            out.push(c);
        }
    }
    take(&mut run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> ThreadKey {
        ThreadKey::new("C1", "1.1")
    }

    #[test]
    fn known_token_prefixes_are_masked_but_the_prefix_stays() {
        assert_eq!(redact("curl -H x xoxb-1234-5678-abcdefgh"), "curl -H x xoxb-***");
        assert_eq!(redact("push https://ghp_abcdefghijklmnop@github.com"), "push https://ghp_***@github.com");
        assert_eq!(redact("sk-short"), "sk-short", "too short to be a key");
        assert_eq!(redact("task-runner"), "task-runner", "a word that only contains a prefix");
    }

    #[test]
    fn assignments_flags_and_headers_are_masked() {
        assert_eq!(redact("SLACK_BOT_TOKEN=abc123 cargo run"), "SLACK_BOT_TOKEN=*** cargo run");
        assert_eq!(redact("export OPENAI_API_KEY=\"abc def\" ls"), "export OPENAI_API_KEY=*** ***\" ls");
        assert_eq!(redact("gh auth login --token abc123"), "gh auth login --token ***");
        assert_eq!(redact("curl -H 'Authorization: Bearer abc.def'"), "curl -H 'Authorization: Bearer ***'");
        assert_eq!(redact(r#"{"password": "hunter2", "user": "a"}"#), r#"{"password": "***", "user": "a"}"#);
        assert_eq!(redact("PWD=/tmp ls"), "PWD=/tmp ls", "PWD is the folder, not a password");
    }

    #[test]
    fn url_passwords_and_random_blobs_are_masked() {
        assert_eq!(redact("git clone https://me:s3cret@host/repo"), "git clone https://me:***@host/repo");
        assert_eq!(redact("key aB3dE5fG7hJ9kL1mN3pQ5rS7tU9vW1xY3z"), "key ***");
        let sha = "2a002c9f0e1d2c3b4a5968778695a4b3c2d1e0f9";
        assert_eq!(redact(&format!("git show {sha}")), format!("git show {sha}"), "commit ids stay");
        assert_eq!(redact("cargo test --lib bridge::activity"), "cargo test --lib bridge::activity");
    }

    #[test]
    fn passwords_glued_to_dash_p_are_masked() {
        assert_eq!(redact("mysql -uroot -pS3cret db"), "mysql -uroot -p*** db");
        assert_eq!(redact("/usr/bin/mysqldump -p'S3 cret' db"), "/usr/bin/mysqldump -p'*** ***' db");
        assert_eq!(redact("sshpass -pS3cret ssh user@host"), "sshpass -p*** ssh user@host");
        assert_eq!(redact("sshpass -p S3cret ssh user@host"), "sshpass -p *** ssh user@host");
        assert_eq!(redact("cd x && mysql -pS3cret; cp -pr a b"), "cd x && mysql -p***; cp -pr a b");
        assert_eq!(redact("mysql -p db"), "mysql -p db", "a bare -p asks for the password");
        assert_eq!(redact("cp -pr a b && mkdir -p c"), "cp -pr a b && mkdir -p c");
        assert_eq!(redact("cargo test -p agentgw"), "cargo test -p agentgw");
    }

    #[test]
    fn jwts_are_masked_whole() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhbGljZSIsImVtYWlsIjoiYUBiLmMifQ.SflKxwRJSMeKKF2QT4fwpM";
        assert_eq!(redact(&format!("curl -d token={jwt}")), "curl -d token=***");
        assert_eq!(redact(&format!("echo {jwt} | jq")), "echo *** | jq");
        assert_eq!(redact(&format!("\"id_token\":\"{jwt}\",")), "\"id_token\":\"***\",");
        assert_eq!(redact("eyJhbGciOiJub25lIn0.eyJzdWIiOiJhIn0."), "***", "unsigned JWT");
        assert_eq!(redact("eyJust.a.word"), "eyJust.a.word", "too short to be a JWT header");
    }

    #[test]
    fn details_and_results_are_cut() {
        assert_eq!(detail(&"a".repeat(500)).chars().count(), DETAIL_MAX + 1);
        let long = format!("{}\ntest result: ok. 3 passed", "x ".repeat(1000));
        let tail = result_tail(&long).unwrap();
        assert!(tail.starts_with('…') && tail.ends_with("3 passed"), "{tail}");
        assert_eq!(result_tail("null"), None);
    }

    #[test]
    fn every_answer_has_a_state() {
        let b = ActivityBook::default();
        assert_eq!(b.snapshot(&key(), true, 0)["state"], "idle");
        assert_eq!(b.snapshot(&key(), false, 0)["state"], "no_agent");
    }

    #[test]
    fn a_running_tool_says_what_and_for_how_long() {
        let mut b = ActivityBook::default();
        b.turn_start(&key(), 1_000);
        b.tool_start(&key(), "t1", "Bash", "cargo test", 2_000);
        let s = b.snapshot(&key(), true, 44_000);
        assert_eq!(s["state"], "running_tool");
        assert_eq!(s["summary"], "Running Bash for 42s: cargo test");
        assert_eq!(s["running"][0]["seconds"], 42);
        assert_eq!(s["turn_started_seconds_ago"], 43);
    }

    #[test]
    fn between_tools_it_is_thinking_and_says_since_when() {
        let mut b = ActivityBook::default();
        b.turn_start(&key(), 0);
        b.tool_start(&key(), "t1", "Read", "/a", 1_000);
        b.tool_done(&key(), "t1", "Read", "/a", "ok", None, 2_000);
        let s = b.snapshot(&key(), true, 12_000);
        assert_eq!(s["state"], "thinking");
        assert_eq!(s["summary"], "Thinking: 10s since Read finished");
        assert_eq!(s["last_tool"]["tool"], "Read");
        assert_eq!(s["last_tool"]["outcome"], "ok");
    }

    #[test]
    fn a_finished_turn_says_whether_it_replied() {
        let mut b = ActivityBook::default();
        b.turn_start(&key(), 0);
        b.tool_start(&key(), "t1", "Bash", "sleep 99", 1_000);
        b.report(&key(), 3_000);
        b.turn_end(&key(), 4_000);
        let s = b.snapshot(&key(), true, 9_000);
        assert_eq!(s["state"], "turn_ended");
        assert_eq!(s["replied_in_last_turn"], true);
        assert_eq!(s["running"].as_array().unwrap().len(), 0, "a tool cut off by the end is not running");
        assert_eq!(s["last_report"]["seconds_ago"], 6);
        // The next turn starts unreplied
        b.turn_start(&key(), 10_000);
        b.turn_end(&key(), 11_000);
        assert_eq!(b.snapshot(&key(), true, 12_000)["replied_in_last_turn"], false);
    }

    #[test]
    fn waiting_for_a_person_and_an_ended_agent_win() {
        let mut b = ActivityBook::default();
        b.turn_start(&key(), 0);
        b.tool_start(&key(), "t1", "Bash", "rm -rf x", 0);
        b.waiting_for_person(&key(), true, 0);
        assert_eq!(b.snapshot(&key(), true, 1_000)["state"], "waiting_for_person");
        b.session_end(&key(), 2_000);
        assert_eq!(b.snapshot(&key(), true, 3_000)["state"], "no_agent");
        assert_eq!(b.snapshot(&key(), false, 3_000)["state"], "no_agent");
    }

    /// A crashed agent sends no more hooks: the book alone would say `running_tool` forever.
    #[test]
    fn a_busy_agent_whose_window_is_gone_is_reported_gone() {
        let mut b = ActivityBook::default();
        b.turn_start(&key(), 0);
        b.tool_start(&key(), "t1", "Bash", "sleep 5", 1_000);
        assert_eq!(b.snapshot_checked(&key(), true, || true, 2_000)["state"], "running_tool");
        let s = b.snapshot_checked(&key(), true, || false, 3_000);
        assert_eq!(s["state"], "agent_gone");
        assert_eq!(s["running"].as_array().unwrap().len(), 0);
        assert!(b.busy(&key()), "a look does not change the book: the cleanup round still tells the thread");
        b.agent_lost(&key(), 3_500);
        assert!(!b.busy(&key()), "once told, no longer counted as at work");
        // The next turn (the agent started again) clears it
        b.turn_start(&key(), 4_000);
        assert_eq!(b.snapshot(&key(), true, 5_000)["state"], "thinking");
    }

    /// An idle agent's window is not looked at: a torn-down cold agent is normal, not a crash.
    #[test]
    fn an_idle_agent_is_not_checked() {
        let mut b = ActivityBook::default();
        b.turn_start(&key(), 0);
        b.turn_end(&key(), 1_000);
        let s = b.snapshot_checked(&key(), true, || panic!("must not look at an idle agent's window"), 2_000);
        assert_eq!(s["state"], "turn_ended");
    }

    #[test]
    fn a_failed_turn_says_so() {
        let mut b = ActivityBook::default();
        b.turn_start(&key(), 0);
        b.turn_failed(&key(), "rate_limit", 1_000);
        let s = b.snapshot(&key(), true, 2_000);
        assert_eq!(s["state"], "turn_failed");
        assert!(s["summary"].as_str().unwrap().contains("rate_limit"));
    }
}
