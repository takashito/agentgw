//! Asks between channels: an agent in one thread asks the agent in another channel for something.
//!
//! The gateway decides everything (whether a follow-up may go through, who to tell when a person is
//! needed); this module holds those decisions as pure functions so they test without Slack or links.

pub use crate::bridge::state::{AskState, AskedFrom};

pub use crate::chat::DeliveryKind;

/// One message from an agent for an agent, as the gateway hands it to the machine that runs the thread.
///
/// `channel` / `thread_ts` / `ts` are where it lands and the real Slack post it was shown as, so the
/// ledger and dedup work on it as on any message. `origin_*` is the other end of the exchange.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentDelivery {
    pub channel: String,
    pub thread_ts: String,
    pub ts: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_thread: Option<String>,
    pub kind: DeliveryKind,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_human: bool,
    /// The Owner's posts the request linked to, fetched and author-checked by the gateway ([`owner_links`]).
    #[serde(default, skip_serializing_if = "crate::chat::OwnerQuotes::is_empty")]
    pub quotes: crate::chat::OwnerQuotes,
}

impl From<AgentDelivery> for crate::chat::InboundMsg {
    fn from(d: AgentDelivery) -> Self {
        crate::chat::InboundMsg {
            channel: d.channel,
            channel_kind: crate::chat::ChannelKind::Channel,
            ts: d.ts,
            thread_ts: Some(d.thread_ts),
            user: None,
            is_bot: false,
            bot_id: None,
            text: d.text,
            files: Vec::new(),
            file_paths: Vec::new(),
            file_errors: Vec::new(),
            reaction: None,
            deleted_ts: None,
            edited: None,
            session_title: None,
            from_agent: Some(crate::chat::AgentOrigin {
                channel: d.origin_channel,
                thread_ts: d.origin_thread,
                kind: d.kind,
                needs_human: d.needs_human,
                quotes: d.quotes,
            }),
        }
    }
}

/// Whether a follow-up from `from_channel`/`from_thread` may go into the asked thread whose record is
/// `row`. The refusals are for the asking agent (a tool error), not shown to people.
pub fn check_follow_up(
    row: Option<&AskedFrom>,
    from_channel: &str,
    from_thread: &str,
    target_online: bool,
) -> Result<(), String> {
    let Some(row) = row.filter(|r| r.channel == from_channel && r.thread_ts == from_thread) else {
        return Err("no ask from this thread goes to that thread".into());
    };
    match row.state {
        AskState::Ended => Err("this exchange has ended; ask again in a new thread".into()),
        AskState::Approved if !target_online => Err("the machine for that channel is offline".into()),
        AskState::Approved => Ok(()),
    }
}

/// Whether `from_channel`/`from_thread` may end the asked thread whose record is `row` (`end_ask`):
/// only the thread that asked it can. `Ok(true)` = it has ended already, nothing more to do.
pub fn check_end(row: Option<&AskedFrom>, from_channel: &str, from_thread: &str) -> Result<bool, String> {
    let Some(row) = row.filter(|r| r.channel == from_channel && r.thread_ts == from_thread) else {
        return Err("that thread was not asked from this thread; you can only end threads you asked".into());
    };
    Ok(row.state == AskState::Ended)
}

/// Whether a new ask may go to `to_channel`. A new ask always opens a new thread, so it can never
/// land in the thread that asks (a follow-up into it is refused by [`check_follow_up`]: no record).
pub fn check_new(to_channel: &str) -> Result<(), String> {
    if to_channel.starts_with('D') {
        return Err("asks cannot go to a DM".into());
    }
    Ok(())
}

/// Where to post "a person is needed" for the asked thread `thread_ts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotifyTarget {
    pub channel: String,
    pub thread_ts: String,
    pub mention_owner: bool,
}

/// The thread that asked, and the top of the chain (where a person gave the first instruction).
/// One target when they are the same. Only the top mentions the Owner.
///
/// `rows` maps an asked thread's ts to (its channel, its record).
pub fn notify_targets(
    rows: &std::collections::HashMap<String, (String, AskedFrom)>,
    channel: &str,
    thread_ts: &str,
) -> Vec<NotifyTarget> {
    // A ts is only unique within a channel: a record kept for another channel is not this thread's
    let live = |ch: &str, ts: &str| {
        rows.get(ts)
            .filter(|(c, r)| c == ch && r.state != AskState::Ended && !r.origin_closed)
            .map(|(_, r)| r)
    };
    // An exchange that ended (Reject, `exit` on either side) has nobody waiting on it any more
    let Some(first) = live(channel, thread_ts) else {
        return Vec::new();
    };
    let asker = (first.channel.clone(), first.thread_ts.clone());
    // Each thread's record is written once, when it is asked, so the chain cannot loop; the visited
    // set only keeps a broken record from spinning. It stops below a link that ended: above it,
    // nobody is following this work any more
    let mut top = asker.clone();
    let mut seen = std::collections::HashSet::from([thread_ts.to_string()]);
    while let Some(up) = live(&top.0, &top.1) {
        if !seen.insert(top.1.clone()) {
            break;
        }
        top = (up.channel.clone(), up.thread_ts.clone());
    }
    let mut out = Vec::new();
    if asker != top {
        out.push(NotifyTarget {
            channel: asker.0,
            thread_ts: asker.1,
            mention_owner: false,
        });
    }
    out.push(NotifyTarget {
        channel: top.0,
        thread_ts: top.1,
        mention_owner: true,
    });
    out
}

/// What the asking agent is told when the asked machine drops mid-request.
pub const OFFLINE_LINE: &str = "The machine for that channel went offline while working on your request.";

/// What the asking agent is told when its request ended without a reply. English: read by the agent.
pub fn dead_end_line(outcome: &str, detail: &str) -> String {
    let detail = if detail.is_empty() { "no reason given" } else { detail };
    match outcome {
        "no_reply" => format!("The other agent chose not to reply ({detail})."),
        "failed" => format!("The other agent's turn failed ({detail})."),
        "stopped" => "A person stopped the other agent. You can still ask again in the same thread.".into(),
        "exited" => "A person ended this exchange. Ask in a new thread if you need more.".into(),
        "refused" => format!("The other agent could not take the request ({detail})."),
        "deleted" => "The other thread was deleted.".into(),
        "cancelled" => "A person deleted your request in the other thread, so it was cancelled. \
                        You can still ask again in the same thread."
            .into(),
        "closed" => "The other thread was closed before it answered.".into(),
        "offline" => OFFLINE_LINE.into(),
        other => format!("The other agent ended without a reply ({other})."),
    }
}

/// How much of a project's notes `list_channels` shows.
pub const PROJECT_NOTES_BYTES: usize = 4096;

/// The project notes of a folder: its `CLAUDE.md`, else its `README.md`, cut to
/// [`PROJECT_NOTES_BYTES`] on a character boundary. Empty when there are none.
pub fn read_project(dir: &std::path::Path) -> String {
    ["CLAUDE.md", "README.md"]
        .iter()
        .find_map(|f| read_notes(dir, f))
        .unwrap_or_default()
}

/// The first [`PROJECT_NOTES_BYTES`] of `dir/name`. A link is followed only while it stays inside
/// `dir` (a repo may keep its notes in a folder of its own); one leading out is not read.
fn read_notes(dir: &std::path::Path, name: &str) -> Option<String> {
    use std::io::Read;
    let path = dir.join(name);
    let real = path.canonicalize().ok()?;
    if !real.starts_with(dir.canonicalize().ok()?) || !real.is_file() {
        return None;
    }
    let mut bytes = Vec::with_capacity(PROJECT_NOTES_BYTES);
    std::fs::File::open(&real)
        .ok()?
        .take(PROJECT_NOTES_BYTES as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    // Cut on a character boundary: drop the tail of a character the limit split
    Some(match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => {
            let ok = e.utf8_error().valid_up_to();
            let mut bytes = e.into_bytes();
            bytes.truncate(ok);
            String::from_utf8(bytes).unwrap_or_default()
        }
    })
}

/// A few words naming a request: the title the agent gave, or else the first line of the request
/// without its Markdown marks, cut to 60 characters.
pub fn request_title(title: Option<&str>, text: &str) -> String {
    let raw = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
            first
                .trim_start_matches(['#', '>', '-', ' '])
                .replace(['*', '`', '_'], "")
                .trim()
                .to_string()
        });
    if raw.chars().count() <= 60 {
        return raw;
    }
    raw.chars().take(59).chain(['…']).collect()
}

/// How many linked posts one request may have checked (each costs a Slack call).
pub const OWNER_LINKS_MAX: usize = 3;

/// A Slack message link found in a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageLink {
    /// The link as written (up to the end of its query string).
    pub url: String,
    pub channel: String,
    pub ts: String,
    /// The thread it is in: `thread_ts` from the query, else the message itself.
    pub thread_ts: String,
}

/// The Slack message links in `text` (`https://<ws>.slack.com/archives/<C>/p<16 digits>[?…]`), first
/// [`OWNER_LINKS_MAX`] distinct ones. Also finds them inside `<…|…>` and Markdown links.
pub fn owner_links(text: &str) -> Vec<MessageLink> {
    let mut out: Vec<MessageLink> = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("https://") {
        let tail = &rest[at..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '>' | '|' | ')' | '<' | '"' | '\'' | ']'))
            .unwrap_or(tail.len());
        let url = &tail[..end];
        rest = &tail[end.max(1)..];
        let Some(link) = parse_message_link(url) else { continue };
        if !out.iter().any(|l| l.channel == link.channel && l.ts == link.ts) {
            out.push(link);
        }
        if out.len() == OWNER_LINKS_MAX {
            break;
        }
    }
    out
}

fn parse_message_link(url: &str) -> Option<MessageLink> {
    let after = url.strip_prefix("https://")?;
    let (host, path) = after.split_once('/')?;
    if !host.ends_with(".slack.com") {
        return None;
    }
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let mut parts = path.trim_end_matches('/').split('/');
    if parts.next()? != "archives" {
        return None;
    }
    let channel = parts.next()?;
    let p = parts.next()?.strip_prefix('p')?;
    if parts.next().is_some()
        || channel.is_empty()
        || !channel.chars().all(|c| c.is_ascii_alphanumeric())
        || p.len() != 16
        || !p.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let ts = format!("{}.{}", &p[..10], &p[10..]);
    // `&amp;` when the link was copied out of Slack's own escaped text
    let thread_ts = query
        .replace("&amp;", "&")
        .split('&')
        .find_map(|kv| kv.strip_prefix("thread_ts=").map(str::to_string))
        .filter(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .unwrap_or_else(|| ts.clone());
    Some(MessageLink { url: url.to_string(), channel: channel.to_string(), ts, thread_ts })
}

/// Longest stretch of a quoted post handed on (a long post would crowd out the request).
const QUOTE_MAX_CHARS: usize = 2000;

/// The Owner's post at `link` among `thread` (its thread, as Slack returned it), or why it is not one.
/// Only a post **written by `owner`** counts; the post before it comes along as context.
pub fn pick_owner_post(
    thread: &[crate::chat::FetchedMsg],
    link: &MessageLink,
    owner: &str,
) -> Result<crate::chat::OwnerQuote, &'static str> {
    if owner.is_empty() {
        return Err("no Owner is set");
    }
    let mut rows: Vec<&crate::chat::FetchedMsg> = thread.iter().collect();
    rows.sort_by(|a, b| a.ts.cmp(&b.ts));
    let at = rows.iter().position(|m| m.ts == link.ts).ok_or("the post was not found")?;
    let m = rows[at];
    if m.user != owner {
        return Err("the post is not the Owner's");
    }
    let cut = |t: &str| truncate_chars(t, QUOTE_MAX_CHARS);
    Ok(crate::chat::OwnerQuote {
        user: m.user.clone(),
        channel: link.channel.clone(),
        ts: m.ts.clone(),
        permalink: link.url.clone(),
        text: cut(&m.text),
        before: at.checked_sub(1).map(|i| crate::chat::QuotedPost {
            user: rows[i].user.clone(),
            ts: rows[i].ts.clone(),
            text: cut(&rows[i].text),
        }),
    })
}

fn truncate_chars(t: &str, max: usize) -> String {
    if t.chars().count() <= max {
        return t.to_string();
    }
    t.chars().take(max).chain(['…']).collect()
}

/// [`notify_targets`] for a thread that is about to ask: itself, and the top of its chain.
/// One target when it is the top. Only the top mentions the Owner.
pub fn notify_targets_from(
    rows: &std::collections::HashMap<String, (String, AskedFrom)>,
    channel: &str,
    thread_ts: &str,
) -> Vec<NotifyTarget> {
    let mut up = notify_targets(rows, channel, thread_ts);
    let me = NotifyTarget {
        channel: channel.to_string(),
        thread_ts: thread_ts.to_string(),
        mention_owner: up.is_empty(),
    };
    match up.pop() {
        None => vec![me],
        Some(top) => vec![me, top],
    }
}

#[cfg(test)]
mod frame_tests {
    use crate::bridge::gateway::link::{decode, encode, LinkFrame};

    #[test]
    fn ask_frames_roundtrip() {
        for f in [
            LinkFrame::AskChannel {
                req_id: "r1".into(),
                from_channel: "C1".into(),
                from_thread: "1.1".into(),
                to_channel: "C2".into(),
                to_thread: None,
                text: "exit".into(),
                title: Some("Check the sandbox".into()),
            },
            LinkFrame::AskAnswer { req_id: "r1".into(), result: Err("no".into()) },
            LinkFrame::AgentReply {
                channel: "C2".into(),
                thread_ts: "2.2".into(),
                ts: "2.3".into(),
                text: "done".into(),
                needs_human: true,
                outcome: "reply".into(),
                posted_in_asker: true,
            },
        ] {
            assert_eq!(decode(&encode(&f)), Some(f));
        }
    }

    #[test]
    fn a_machine_never_takes_a_frame_meant_for_the_gateway() {
        let f = LinkFrame::NeedsHuman { channel: "C".into(), thread_ts: "1".into(), ts: "2".into() };
        assert!(crate::bridge::machine::FromRelay::of(f).is_none());
    }

    #[test]
    fn an_agent_message_reaches_the_machine_as_a_delivery() {
        let d = super::AgentDelivery {
            channel: "C2".into(),
            thread_ts: "2.1".into(),
            ts: "2.1".into(),
            text: "hi".into(),
            origin_channel: Some("C1".into()),
            origin_thread: Some("1.1".into()),
            kind: super::DeliveryKind::Ask,
            needs_human: false,
            quotes: Default::default(),
        };
        let f = LinkFrame::AgentMessage { msg: serde_json::to_value(&d).unwrap() };
        match crate::bridge::machine::FromRelay::of(f) {
            Some(crate::bridge::machine::FromRelay::AgentMessage(got)) => assert_eq!(got, d),
            _ => panic!("not delivered as an agent message"),
        }
    }
}

#[cfg(test)]
mod decision_tests {
    use super::*;
    use std::collections::HashMap;

    fn row(ch: &str, th: &str, s: AskState) -> AskedFrom {
        AskedFrom {
            channel: ch.into(),
            thread_ts: th.into(),
            state: s,
            outstanding: false,
            missed: 0,
            origin_closed: false,
            line: None,
            title: None,
        }
    }

    #[test]
    fn follow_ups_need_an_approved_link_from_the_same_thread() {
        let ok = row("C1", "1.1", AskState::Approved);
        assert!(check_follow_up(Some(&ok), "C1", "1.1", true).is_ok());
        assert!(check_follow_up(None, "C1", "1.1", true).is_err());
        assert!(check_follow_up(Some(&ok), "C1", "9.9", true).is_err());
        let ended = row("C1", "1.1", AskState::Ended);
        assert!(check_follow_up(Some(&ended), "C1", "1.1", true).unwrap_err().contains("new thread"));
        assert!(check_follow_up(Some(&ok), "C1", "1.1", false).unwrap_err().contains("offline"));
    }

    #[test]
    fn new_asks_refuse_dms() {
        assert!(check_new("D1").is_err());
        assert!(check_new("C2").is_ok());
    }

    #[test]
    fn a_three_level_chain_tells_the_asker_and_the_top_and_mentions_only_the_top() {
        // CoS(C0/0.1) → Lead(C1/1.1) → project(C2/2.1)
        let mut rows = HashMap::new();
        rows.insert("1.1".to_string(), ("C1".to_string(), row("C0", "0.1", AskState::Approved)));
        rows.insert("2.1".to_string(), ("C2".to_string(), row("C1", "1.1", AskState::Approved)));
        assert_eq!(
            notify_targets(&rows, "C2", "2.1"),
            vec![
                NotifyTarget { channel: "C1".into(), thread_ts: "1.1".into(), mention_owner: false },
                NotifyTarget { channel: "C0".into(), thread_ts: "0.1".into(), mention_owner: true },
            ]
        );
    }

    #[test]
    fn one_level_sends_one_notice_with_the_mention() {
        let mut rows = HashMap::new();
        rows.insert("1.1".to_string(), ("C1".to_string(), row("C0", "0.1", AskState::Approved)));
        assert_eq!(
            notify_targets(&rows, "C1", "1.1"),
            vec![NotifyTarget { channel: "C0".into(), thread_ts: "0.1".into(), mention_owner: true }]
        );
    }

    #[test]
    fn a_thread_nobody_asked_has_nobody_to_tell() {
        assert!(notify_targets(&HashMap::new(), "C1", "1.1").is_empty());
    }

    #[test]
    fn a_broken_record_that_loops_still_ends() {
        let mut rows = HashMap::new();
        rows.insert("1.1".to_string(), ("C1".to_string(), row("C2", "2.1", AskState::Approved)));
        rows.insert("2.1".to_string(), ("C2".to_string(), row("C1", "1.1", AskState::Approved)));
        assert!(notify_targets(&rows, "C1", "1.1").len() <= 2);
    }

    #[test]
    fn an_ended_exchange_tells_nobody_that_a_person_is_needed() {
        let mut rows = HashMap::new();
        rows.insert("1.1".to_string(), ("C1".to_string(), row("C0", "0.1", AskState::Ended)));
        assert!(notify_targets(&rows, "C1", "1.1").is_empty());
        let mut closed = row("C0", "0.1", AskState::Approved);
        closed.origin_closed = true;
        rows.insert("1.1".to_string(), ("C1".to_string(), closed));
        assert!(notify_targets(&rows, "C1", "1.1").is_empty());
    }

    #[test]
    fn the_chain_stops_below_an_ended_link() {
        // CoS(C0/0.1) → Lead(C1/1.1) ended; Lead → project(C2/2.1) still on: the Lead is the top now
        let mut rows = HashMap::new();
        rows.insert("1.1".to_string(), ("C1".to_string(), row("C0", "0.1", AskState::Ended)));
        rows.insert("2.1".to_string(), ("C2".to_string(), row("C1", "1.1", AskState::Approved)));
        assert_eq!(
            notify_targets(&rows, "C2", "2.1"),
            vec![NotifyTarget { channel: "C1".into(), thread_ts: "1.1".into(), mention_owner: true }]
        );
    }

    #[test]
    fn a_record_in_another_channel_with_the_same_ts_is_not_the_one() {
        let mut rows = HashMap::new();
        rows.insert("1.1".to_string(), ("C9".to_string(), row("C0", "0.1", AskState::Approved)));
        assert!(notify_targets(&rows, "C1", "1.1").is_empty());
        // …nor a link up the chain
        rows.insert("2.1".to_string(), ("C2".to_string(), row("C1", "1.1", AskState::Approved)));
        assert_eq!(
            notify_targets(&rows, "C2", "2.1"),
            vec![NotifyTarget { channel: "C1".into(), thread_ts: "1.1".into(), mention_owner: true }]
        );
    }

    #[test]
    fn only_the_asking_thread_may_end_an_ask() {
        let ok = row("C1", "1.1", AskState::Approved);
        assert_eq!(check_end(Some(&ok), "C1", "1.1"), Ok(false));
        assert!(check_end(Some(&ok), "C1", "9.9").is_err(), "another thread");
        assert!(check_end(Some(&ok), "C9", "1.1").is_err(), "another channel");
        assert!(check_end(None, "C1", "1.1").is_err(), "a thread nobody asked");
        let ended = row("C1", "1.1", AskState::Ended);
        assert_eq!(check_end(Some(&ended), "C1", "1.1"), Ok(true), "ending twice is fine");
    }
}

#[cfg(test)]
mod project_tests {
    use super::read_project;

    fn dir_with(name: &str, file: &str, body: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("agentgw-project-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        if !file.is_empty() {
            std::fs::write(d.join(file), body).unwrap();
        }
        d
    }

    #[test]
    fn project_text_is_cut_at_4096_bytes_on_a_char_boundary() {
        let d = dir_with("cut", "CLAUDE.md", &"あ".repeat(3000)); // 9000 bytes
        let t = read_project(&d);
        assert!(t.len() <= 4096 && t.len() > 4000, "{}", t.len());
        assert!(t.chars().all(|c| c == 'あ'));
    }

    #[test]
    fn readme_is_read_when_there_is_no_claude_md() {
        let d = dir_with("readme", "README.md", "# hello");
        assert_eq!(read_project(&d), "# hello");
    }

    #[test]
    fn a_link_to_a_file_outside_the_folder_is_not_read() {
        let outside = dir_with("outside", "secret.txt", "do not show");
        let d = dir_with("linkout", "", "");
        std::os::unix::fs::symlink(outside.join("secret.txt"), d.join("CLAUDE.md")).unwrap();
        assert_eq!(read_project(&d), "");
    }

    #[test]
    fn a_link_that_stays_inside_the_folder_is_read() {
        // This repo's own CLAUDE.md is a link into a folder inside it
        let d = dir_with("linkin", "", "");
        std::fs::create_dir_all(d.join(".private")).unwrap();
        std::fs::write(d.join(".private/CLAUDE.md"), "# notes").unwrap();
        std::os::unix::fs::symlink(d.join(".private/CLAUDE.md"), d.join("CLAUDE.md")).unwrap();
        assert_eq!(read_project(&d), "# notes");
    }

    #[test]
    fn a_folder_without_notes_reads_empty() {
        assert_eq!(read_project(&dir_with("none", "", "")), "");
    }
}

#[cfg(test)]
mod title_tests {
    use super::request_title;

    #[test]
    fn a_given_title_is_used_trimmed() {
        assert_eq!(request_title(Some("  Fix the sandbox "), "body"), "Fix the sandbox");
    }

    #[test]
    fn without_a_title_the_first_line_names_the_request() {
        assert_eq!(request_title(None, "\n## **Fix** the `bwrap` sandbox\nmore detail"), "Fix the bwrap sandbox");
        assert_eq!(request_title(Some(""), "first\nsecond"), "first");
    }

    #[test]
    fn a_long_title_is_cut_to_60_characters() {
        let t = request_title(None, &"あ".repeat(100));
        assert_eq!(t.chars().count(), 60);
        assert!(t.ends_with('…'));
    }
}

#[cfg(test)]
mod owner_link_tests {
    use super::*;
    use crate::chat::FetchedMsg;

    fn post(ts: &str, user: &str, text: &str) -> FetchedMsg {
        FetchedMsg { ts: ts.into(), user: user.into(), text: text.into(), thread_ts: Some("1700000001.000100".into()) }
    }

    #[test]
    fn finds_message_links_in_any_wrapping() {
        let text = "OK: https://ws.slack.com/archives/C0AAAAAAAAA/p1700000003000300?thread_ts=1700000001.000100&cid=C0AAAAAAAAA\n\
                    and <https://ws.slack.com/archives/C0BBBBBBBBB/p1700000000000900|the thread> \
                    and [this](https://ws.slack.com/archives/C1/p1700000005000500?thread_ts=1700000001.000100&amp;cid=C1) \
                    again https://ws.slack.com/archives/C0AAAAAAAAA/p1700000003000300 \
                    not https://evil.example.com/archives/C1/p1700000005000500 nor https://ws.slack.com/archives/C1/p123 \
                    a fourth https://ws.slack.com/archives/C2/p1700000005000500";
        let got = owner_links(text);
        let ids: Vec<(&str, &str, &str)> =
            got.iter().map(|l| (l.channel.as_str(), l.ts.as_str(), l.thread_ts.as_str())).collect();
        assert_eq!(
            ids,
            vec![
                ("C0AAAAAAAAA", "1700000003.000300", "1700000001.000100"),
                ("C0BBBBBBBBB", "1700000000.000900", "1700000000.000900"),
                ("C1", "1700000005.000500", "1700000001.000100"),
            ],
            "duplicates, other hosts and short ids are skipped; at most {OWNER_LINKS_MAX}"
        );
        assert_eq!(got[1].url, "https://ws.slack.com/archives/C0BBBBBBBBB/p1700000000000900");
    }

    #[test]
    fn only_the_owners_own_post_is_taken() {
        let link = owner_links("https://ws.slack.com/archives/C9/p1700000005000500?thread_ts=1700000001.000100").remove(0);
        let thread = vec![
            post("1700000005.000500", "U_OWNER", "go ahead"),
            post("1700000001.000100", "U_OWNER", "sort this out"),
            post("1700000004.000400", "U_BOT", "may I start?"),
        ];
        let q = pick_owner_post(&thread, &link, "U_OWNER").unwrap();
        assert_eq!((q.user.as_str(), q.ts.as_str(), q.text.as_str()), ("U_OWNER", "1700000005.000500", "go ahead"));
        assert_eq!(q.channel, "C9");
        assert_eq!(q.permalink, link.url);
        // The post just before it (by time, whatever order Slack gave) comes along as context
        let b = q.before.unwrap();
        assert_eq!((b.user.as_str(), b.text.as_str()), ("U_BOT", "may I start?"));

        // Someone else's post, a missing post, or no Owner set: refused
        let bot = owner_links("https://ws.slack.com/archives/C9/p1700000004000400?thread_ts=1700000001.000100").remove(0);
        assert_eq!(pick_owner_post(&thread, &bot, "U_OWNER").unwrap_err(), "the post is not the Owner's");
        let gone = owner_links("https://ws.slack.com/archives/C9/p1700000009000000").remove(0);
        assert_eq!(pick_owner_post(&thread, &gone, "U_OWNER").unwrap_err(), "the post was not found");
        assert_eq!(pick_owner_post(&thread, &link, "").unwrap_err(), "no Owner is set");
        // A root post has nothing before it
        let root = owner_links("https://ws.slack.com/archives/C9/p1700000001000100").remove(0);
        assert!(pick_owner_post(&thread, &root, "U_OWNER").unwrap().before.is_none());
    }

    #[test]
    fn quotes_ride_the_wire_and_reach_the_envelope() {
        let d = AgentDelivery {
            channel: "C2".into(),
            thread_ts: "2.1".into(),
            ts: "2.1".into(),
            text: "deploy".into(),
            origin_channel: Some("C1".into()),
            origin_thread: Some("1.1".into()),
            kind: DeliveryKind::Ask,
            needs_human: false,
            quotes: crate::chat::OwnerQuotes { verified: Vec::new(), unverified: vec!["https://x".into()] },
        };
        let back: AgentDelivery = serde_json::from_value(serde_json::to_value(&d).unwrap()).unwrap();
        assert_eq!(back, d);
        // An older gateway's frame (no `quotes`) still reads
        let old = serde_json::json!({"channel":"C2","thread_ts":"2.1","ts":"2.1","text":"x","kind":"ask"});
        assert!(serde_json::from_value::<AgentDelivery>(old).unwrap().quotes.is_empty());
        let msg: crate::chat::InboundMsg = d.into();
        let env = crate::bridge::inbound::envelope(&msg, "2.1", None, 0);
        assert!(env.contains("could not verify this link as the Owner's own post, so it is not one: https://x"), "{env}");
    }
}
