//! Talking to the chat platform. [`Chat`] is what the core asks of it; `chat::slack` implements
//! it. Another platform would sit next to `slack` as `chat::<name>`.
//! The shape of what arrives ([`InboundMsg`] and friends) lives here; `chat::slack` parses into it.

use std::path::Path;
use std::sync::Arc;

pub mod slack;

use async_trait::async_trait;

// ── what arrives from the chat ──

/// Where an incoming message came from.
///
/// DMs and channels **differ in wording and in defaults** (`pwd` is refused in a DM /
/// channels differ in whether a mention is required), so the origin is carried as a type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChannelKind {
    Dm,
    Channel,
}

/// One attachment on an incoming message. The input to the prefetch download.
///
/// `name` is the display name shown in the degradation note — the id itself when Slack gives no name.
#[derive(Clone, Debug)]
pub struct InboundFile {
    pub id: String,
    pub name: String,
}

/// The Bridge's vocabulary. Built by slack.rs, also read by worker/endpoints.
#[derive(Clone, Debug)]
pub struct InboundMsg {
    pub channel: String,
    pub channel_kind: ChannelKind,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub user: Option<String>,
    pub is_bot: bool,
    /// The bot id Slack attaches. Needed to check whether the sender was allowed with `allow-bot`.
    pub bot_id: Option<String>,
    pub text: String,
    /// Slack's `files[]` (filled in by slack.rs).
    pub files: Vec<InboundFile>,
    /// Result of the prefetch download — local paths that succeeded, and degradation notes for failures.
    /// Filled in by the receive path (main.rs) right before delivery. Queued messages carry it along.
    pub file_paths: Vec<String>,
    pub file_errors: Vec<String>,
    /// Set when this message came from a reaction. The agent gets it as synthesized text (in `text`),
    /// but **a stop emoji on a progress message** is not delivered — it is used to stop the turn.
    pub reaction: Option<Reaction>,
    /// The ts of a message the user **deleted**. When set, this is a deletion signal and
    /// `text` is the instruction to take it back (`deletion_notice`).
    pub deleted_ts: Option<String>,
    /// The ts of a message the user **edited**, and its revision id (for dedup).
    /// When set, `text` is **the new body itself** — the Bridge builds the instruction
    /// (the wording depends on "is this the one being worked on now", which only the Bridge knows).
    pub edited: Option<Edited>,
}

/// One edit. `revision` is Slack's `edited.ts` (marks the same edit being redelivered).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edited {
    pub ts: String,
    pub revision: String,
}

/// One reaction.
///
/// `item_ts` is the ts of **the message it was put on** — that is how we tell a progress message
/// (a stop emoji on anything else is just a reaction).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reaction {
    pub emoji: String,
    pub item_ts: String,
    pub added: bool,
}

/// Reaction names treated as stop (six). `hand` and `raised_hand` are aliases of the same ✋,
/// so either one works. **No colons** — Slack sends reaction names in the form
/// `red_circle` (not the `:red_circle:` of message text).
const STOP_REACTION_NAMES: [&str; 6] = [
    "red_circle",
    "octagonal_sign",
    "black_square_for_stop",
    "hand",
    "raised_hand",
    "x",
];

impl Reaction {
    pub fn is_stop(&self) -> bool {
        self.added && STOP_REACTION_NAMES.contains(&self.emoji.as_str())
    }

    /// Synthesized text handed to the agent. A reaction is a light signal, so a sentence is added
    /// reminding the agent **not to stay silent** (only when added).
    pub fn synthetic_text(&self, reactor: &str, item_text: &str) -> String {
        let truncated: String = if item_text.chars().count() > 280 {
            item_text.chars().take(280).chain(['…']).collect()
        } else {
            item_text.to_string()
        };
        let kind = if self.added { "added" } else { "removed" };
        let verb = if self.added {
            "reacted with"
        } else {
            "removed reaction"
        };
        let hint = if self.added {
            " Do not stay silent: at minimum call the `react` tool to acknowledge (e.g. mirror \
             the emoji, or use 👍/🙏/🤔 as appropriate). Reply with text only if the reaction \
             calls for a substantive response."
        } else {
            ""
        };
        format!(
            "[reaction {kind}] <@{reactor}> {verb} :{}: on your message: {truncated}{hint}",
            self.emoji
        )
    }
}

/// Text telling the agent about an edit.
/// `is_current` = the request being worked on now was rewritten (drop it and do the new one) / otherwise
/// a past request was revised (keep the current work going, handle it once free).
pub fn edit_notice(edited_ts: &str, new_text: &str, had_files: bool, is_current: bool) -> String {
    const SNIPPET_MAX: usize = 1000;
    let trimmed = new_text.trim();
    let content_desc = if trimmed.is_empty() {
        if had_files {
            "It now has no text (a file/attachment only).".to_string()
        } else {
            "Its new text is unavailable.".to_string()
        }
    } else if trimmed.chars().count() > SNIPPET_MAX {
        let head: String = trimmed.chars().take(SNIPPET_MAX).collect();
        format!("The new content is:\n\"\"\"\n{head}… (truncated)\n\"\"\"")
    } else {
        format!("The new content is:\n\"\"\"\n{trimmed}\n\"\"\"")
    };
    let lead = if is_current {
        format!(
            "[message_edited] The user EDITED the message you are CURRENTLY working on \
             (id {edited_ts}) — they changed the request, so your in-progress turn was \
             interrupted. Throw away the work you were doing for the OLD wording and handle the \
             NEW content instead. {content_desc}"
        )
    } else {
        format!(
            "[message_edited] The user EDITED an earlier message (id {edited_ts}) that you had \
             already moved past — they revised that request. Do NOT abandon your current work; \
             once you are free, handle the revised request. {content_desc}"
        )
    };
    format!(
        "{lead}\n\nRespond to the now-edited request as you normally would; if the edit changes \
         nothing you need to do, call no_reply."
    )
}

/// Text telling the agent about a deletion.
/// **Don't let it just reply "it was deleted"** — make it decide: throw away work not yet
/// visible outside, and explain only when side effects have already gone out.
pub fn deletion_notice(deleted_ts: &str, text: &str, had_files: bool) -> String {
    const SNIPPET_MAX: usize = 1000;
    let trimmed = text.trim();
    let content_desc = if trimmed.is_empty() {
        if had_files {
            "It had no text (it was a file/attachment upload).".to_string()
        } else {
            "Its text is unavailable.".to_string()
        }
    } else if trimmed.chars().count() > SNIPPET_MAX {
        let head: String = trimmed.chars().take(SNIPPET_MAX).collect();
        format!("Its content was:\n\"\"\"\n{head}… (truncated)\n\"\"\"")
    } else {
        format!("Its content was:\n\"\"\"\n{trimmed}\n\"\"\"")
    };
    format!(
        "[message_deleted] The user DELETED their own message (id {deleted_ts}). They removed \
         that request from the conversation — handle it as if the message had never been sent. \
         {content_desc}\n\nDecide based on what you have ACTUALLY done for it so far:\n\
         • If you have NOT yet performed any external/irreversible action for it — including the \
         case where you only prepared, computed, or were about to send an answer — then DISCARD \
         that work: call no_reply and output nothing. A not-yet-sent answer is NOT a side-effect; \
         throw it away.\n\
         • Only reply if you ALREADY performed a real external side-effect that the user must know \
         about or that needs reverting (e.g. created/edited a file, ran a command with lasting \
         effects, or already posted a message), in which case briefly explain and/or undo it.\n\
         Never output any text merely stating that the message was deleted."
    )
}

// ── what reads return ──

/// Only what **the event doesn't tell us** about an existing message.
/// Returned by [`Chat::message_at`].
pub struct MessageAt {
    /// The author (may be missing on bot posts).
    pub user: Option<String>,
    /// Posted by a bot.
    pub is_bot: bool,
    /// Root of the thread it belongs to. Its own ts if it is not a reply.
    pub thread_ts: String,
}

/// One line of history/replies.
#[derive(Clone)]
pub struct FetchedMsg {
    pub ts: String,
    pub user: String,
    pub text: String,
    /// Thread root. The parent's ts for a reply, its own ts for a root, None outside a thread.
    pub thread_ts: Option<String>,
}

impl FetchedMsg {
    /// `[ts] user: text`, oldest first.
    pub fn render_all(msgs: &[FetchedMsg]) -> String {
        let mut rows: Vec<&FetchedMsg> = msgs.iter().collect();
        rows.sort_by(|a, b| a.ts.cmp(&b.ts));
        rows.iter()
            .map(|m| format!("[{}] {}: {}", m.ts, m.user, m.text))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// What the core asks of the chat platform: post, edit, react, read. Implemented by
/// `chat::slack::Api`; the methods still speak Slack's terms (`ts`, `thread_ts`).
#[async_trait]
pub trait Chat: Send + Sync + 'static {
    async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String>;
    async fn post_message_no_unfurl(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String>;
    async fn post_markdown(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String>;
    async fn update_message(&self, channel: &str, ts: &str, text: &str) -> Result<(), String>;
    async fn update_markdown(&self, channel: &str, ts: &str, text: &str) -> Result<(), String>;
    async fn delete_message(&self, channel: &str, ts: &str) -> Result<(), String>;
    async fn add_reaction(&self, channel: &str, ts: &str, emoji: &str) -> Result<(), String>;
    async fn remove_reaction(&self, channel: &str, ts: &str, emoji: &str) -> Result<(), String>;
    /// Marks a delivered message as received: drops the ack and ⟳, adds 🤖. Best-effort.
    async fn flip_to_received(&self, channel: &str, message_ts: &str, ack: &str);
    async fn post_perm_prompt(
        &self,
        channel: &str,
        thread_ts: &str,
        req_id: &str,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> Result<String, String>;
    async fn open_dm(&self, user_id: &str) -> Result<String, String>;
    async fn history(&self, channel: &str, limit: u16) -> Result<Vec<FetchedMsg>, String>;
    async fn replies(
        &self,
        channel: &str,
        thread_ts: &str,
        limit: u16,
    ) -> Result<Vec<FetchedMsg>, String>;
    async fn parent_thread_of(&self, channel: &str, ts: &str) -> Option<String>;
    async fn message_at(&self, channel: &str, ts: &str) -> Option<MessageAt>;
    async fn thread_root_gone(&self, channel: &str, thread_ts: &str) -> bool;
    async fn get_permalink(&self, channel: &str, ts: &str) -> Result<String, String>;
    async fn channel_display_name(&self, channel: &str) -> Option<String>;
    /// Every channel the bot is a member of, as (id, name). The gateway needs it for `status`: a channel
    /// nobody was assigned is **its own**, and until someone types in it there is nothing else to learn it from.
    async fn bot_channels(&self) -> Result<Vec<(String, String)>, String>;
    async fn user_display_name(&self, user: &str) -> Option<String>;
    async fn auth_test(&self) -> Result<(String, Option<String>), String>;
    async fn resolve_bot_id(&self, user_id: &str) -> Result<Option<String>, String>;
    async fn file_info(&self, file_id: &str) -> Result<(String, String, u64), String>;
    async fn download_to(&self, url: &str, dest: &Path) -> Result<(), String>;
    /// file_id → the local path it was saved to under `state_dir/inbox`.
    async fn download_attachment(&self, file_id: &str, state_dir: &Path)
    -> Result<String, String>;
    async fn upload_file(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        path: &Path,
    ) -> Result<(), String>;
    /// Busy while a turn is in flight, idle when it is over. Slack draws it — a working line and,
    /// with `agent_session_stopped` subscribed, a stop button.
    async fn set_busy(&self, channel: &str, thread_ts: &str, busy: bool) -> Result<(), String>;
    /// Register a thread as an agent session named `title` — what the `Agents & tools` sidebar lists.
    async fn start_session(
        &self,
        channel: &str,
        thread_ts: &str,
        title: &str,
    ) -> Result<(), String>;
}

pub type ChatRef = Arc<dyn Chat>;

/// `{channel}:{thread_ts}`.
///
/// **Not used as a log directory name as-is** — dropping `:` and `.` is
/// [`LogCtx::sanitized_key`](crate::log::LogCtx::sanitized_key)'s job.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct ThreadKey(String);

impl ThreadKey {
    pub fn new(channel: &str, thread_ts: &str) -> Self {
        ThreadKey(format!("{channel}:{thread_ts}"))
    }

    /// From an existing string (from threads.json / logs / hook payloads).
    pub fn parse(raw: &str) -> Self {
        ThreadKey(raw.to_string())
    }

    /// Splits at the first `:`. With no `:`, it is a channel-only key.
    pub fn split(&self) -> (String, Option<String>) {
        match self.0.split_once(':') {
            Some((ch, ts)) => (ch.to_string(), Some(ts.to_string())),
            None => (self.0.clone(), None),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ThreadKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Compare against a literal (reads better in log lines and tests).
impl PartialEq<&str> for ThreadKey {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Every Slack call, in order: `"post C1 1.0 hello"` / `"react C1 1.1 eyes"` / ...
    ///
    /// Markdown goes on record as `post_md` / `update_md` so a test can tell it from mrkdwn.
    #[derive(Default)]
    pub struct FakeChat {
        pub calls: Mutex<Vec<String>>,
        next_ts: AtomicU64,
        /// What `history` and `replies` answer.
        pub msgs: Vec<FetchedMsg>,
        /// Every recorded call fails (after being recorded).
        pub fail: bool,
        /// Only `replies` fails.
        pub replies_fail: bool,
        /// The size `file_info` reports.
        pub file_size: u64,
        /// What `bot_channels` answers: (id, name).
        pub channels: Vec<(String, String)>,
    }

    impl FakeChat {
        /// A fake that answers `bot_channels` with these (id, name) pairs.
        pub fn in_channels(pairs: &[(&str, &str)]) -> FakeChat {
            FakeChat {
                channels: pairs
                    .iter()
                    .map(|(id, name)| (id.to_string(), name.to_string()))
                    .collect(),
                ..Default::default()
            }
        }


        /// A fake whose thread root is **alive** (the normal state, where replies are not suppressed).
        pub fn with_live_root() -> FakeChat {
            let mut api = FakeChat::default();
            api.msgs = vec![FetchedMsg {
                ts: "1.0".into(),
                user: "U1".into(),
                text: "root".into(),
                thread_ts: None,
            }];
            api
        }

        /// A fake where every call returns Err.
        pub fn failing() -> FakeChat {
            let mut api = FakeChat::default();
            api.fail = true;
            api
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn record(&self, s: String) -> Result<(), String> {
            self.calls.lock().unwrap().push(s);
            if self.fail {
                Err("slack said no".into())
            } else {
                Ok(())
            }
        }
        fn ts(&self) -> String {
            format!("9.{}", self.next_ts.fetch_add(1, Ordering::SeqCst))
        }
    }

    #[async_trait]
    impl Chat for FakeChat {
        async fn post_message(&self, c: &str, t: &str, th: Option<&str>) -> Result<String, String> {
            self.record(format!("post {c} {} {t}", th.unwrap_or("-")))?;
            Ok(self.ts())
        }
        async fn post_message_no_unfurl(
            &self,
            c: &str,
            t: &str,
            th: Option<&str>,
        ) -> Result<String, String> {
            self.post_message(c, t, th).await
        }
        async fn post_markdown(&self, c: &str, t: &str, th: Option<&str>) -> Result<String, String> {
            self.record(format!("post_md {c} {} {t}", th.unwrap_or("-")))?;
            Ok(self.ts())
        }
        async fn update_message(&self, c: &str, ts: &str, t: &str) -> Result<(), String> {
            self.record(format!("update {c} {ts} {t}"))
        }
        async fn update_markdown(&self, c: &str, ts: &str, t: &str) -> Result<(), String> {
            self.record(format!("update_md {c} {ts} {t}"))
        }
        async fn delete_message(&self, c: &str, ts: &str) -> Result<(), String> {
            self.record(format!("delete {c} {ts}"))
        }
        async fn add_reaction(&self, c: &str, ts: &str, e: &str) -> Result<(), String> {
            self.record(format!("react {c} {ts} {e}"))
        }
        async fn remove_reaction(&self, c: &str, ts: &str, e: &str) -> Result<(), String> {
            self.record(format!("unreact {c} {ts} {e}"))
        }
        async fn flip_to_received(&self, c: &str, ts: &str, _ack: &str) {
            let _ = self.record(format!("received {c} {ts}"));
        }
        async fn post_perm_prompt(
            &self,
            c: &str,
            th: &str,
            id: &str,
            tool: &str,
            _i: &serde_json::Value,
        ) -> Result<String, String> {
            self.record(format!("perm {c} {th} {id} {tool}"))?;
            Ok(self.ts())
        }
        async fn open_dm(&self, u: &str) -> Result<String, String> {
            Ok(format!("D-{u}"))
        }
        async fn history(&self, _c: &str, _l: u16) -> Result<Vec<FetchedMsg>, String> {
            if self.fail {
                return Err("slack said no".into());
            }
            Ok(self.msgs.clone())
        }
        async fn replies(&self, _c: &str, _t: &str, _l: u16) -> Result<Vec<FetchedMsg>, String> {
            if self.fail || self.replies_fail {
                return Err("channel_not_found".into());
            }
            Ok(self.msgs.clone())
        }
        async fn parent_thread_of(&self, _c: &str, _t: &str) -> Option<String> {
            None
        }
        async fn message_at(&self, _c: &str, _t: &str) -> Option<MessageAt> {
            None
        }
        async fn thread_root_gone(&self, _c: &str, _t: &str) -> bool {
            false
        }
        async fn bot_channels(&self) -> Result<Vec<(String, String)>, String> {
            self.record("bot_channels".to_string())?;
            Ok(self.channels.clone())
        }
        async fn get_permalink(&self, c: &str, ts: &str) -> Result<String, String> {
            self.record(format!("permalink {c} {ts}"))?;
            Ok(format!("https://slack/{c}/{ts}"))
        }
        async fn channel_display_name(&self, c: &str) -> Option<String> {
            self.record(format!("channel_name {c}")).ok()?;
            Some(format!("#{c}"))
        }
        async fn user_display_name(&self, u: &str) -> Option<String> {
            Some(u.to_string())
        }
        async fn auth_test(&self) -> Result<(String, Option<String>), String> {
            Ok(("U_BOT".into(), Some("agentgw".into())))
        }
        /// `UB…` is a bot (→ `B…`); anyone else is a person.
        async fn resolve_bot_id(&self, u: &str) -> Result<Option<String>, String> {
            self.record(format!("bot_id {u}"))?;
            Ok(u.strip_prefix("UB").map(|rest| format!("B{rest}")))
        }
        async fn file_info(&self, _f: &str) -> Result<(String, String, u64), String> {
            Ok(("https://slack/f".into(), "shot.png".into(), self.file_size))
        }
        async fn download_to(&self, _u: &str, _d: &Path) -> Result<(), String> {
            Err("no files in fake".into())
        }
        async fn download_attachment(&self, _f: &str, _d: &Path) -> Result<String, String> {
            Err("no files in fake".into())
        }
        /// Fails like the real one when the file can't be read.
        async fn upload_file(&self, c: &str, th: Option<&str>, p: &Path) -> Result<(), String> {
            std::fs::metadata(p).map_err(|e| format!("read {}: {e}", p.display()))?;
            self.record(format!("upload {c} {} {}", th.unwrap_or("-"), p.display()))
        }
        async fn set_busy(&self, c: &str, th: &str, busy: bool) -> Result<(), String> {
            self.record(format!("status {c} {th} {}", if busy { "busy" } else { "idle" }))
        }
        async fn start_session(&self, c: &str, th: &str, title: &str) -> Result<(), String> {
            self.record(format!("session {c} {th} {title}"))
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only an **added** stop emoji means stop. The synthesized text keeps its original wording.
    #[test]
    fn a_stop_reaction_is_only_a_stop_when_added() {
        let r = |emoji: &str, added: bool| Reaction {
            emoji: emoji.into(),
            item_ts: "1.1".into(),
            added,
        };
        assert!(r("red_circle", true).is_stop());
        assert!(r("raised_hand", true).is_stop(), "hand の別名も通す");
        assert!(!r("red_circle", false).is_stop(), "外したのは停止ではない");
        assert!(!r("eyes", true).is_stop());

        let added = r("tada", true).synthetic_text("U1", "done");
        assert!(
            added.starts_with("[reaction added] <@U1> reacted with :tada: on your message: done"),
            "{added}"
        );
        assert!(added.contains("Do not stay silent"));
        let removed = r("tada", false).synthetic_text("U1", "done");
        assert!(
            removed.starts_with(
                "[reaction removed] <@U1> removed reaction :tada: on your message: done"
            ),
            "{removed}"
        );
        assert!(
            !removed.contains("Do not stay silent"),
            "外した側は促さない"
        );
        // Long bodies are cut at 280 characters
        let long = r("eyes", true).synthetic_text("U1", &"あ".repeat(400));
        assert!(long.contains(&format!("{}…", "あ".repeat(280))));
    }

    /// The edit text depends on "is it being worked on now". If so, drop the old work;
    /// if it is a past request, keep doing the current work.
    #[test]
    fn an_edit_tells_the_worker_to_redo_only_when_it_is_the_current_work() {
        let now = edit_notice("1.2", "こっちでお願い", false, true);
        assert!(now.contains("the message you are CURRENTLY working on (id 1.2)"));
        assert!(now.contains("Throw away the work you were doing for the OLD wording"));
        assert!(now.contains("The new content is:\n\"\"\"\nこっちでお願い\n\"\"\""));

        let past = edit_notice("1.2", "こっちでお願い", false, false);
        assert!(past.contains("EDITED an earlier message (id 1.2)"));
        assert!(past.contains("Do NOT abandon your current work"));
        // Both end the same way — stay silent if nothing changes
        for n in [&now, &past] {
            assert!(n.ends_with("if the edit changes nothing you need to do, call no_reply."));
        }
        // An edit that empties the body is worded differently for attachments-only vs unreadable
        assert!(edit_notice("1.2", "", true, true).contains("a file/attachment only"));
        assert!(edit_notice("1.2", "", false, true).contains("Its new text is unavailable."));
        assert!(edit_notice("1.2", &"あ".repeat(1500), false, true).contains("… (truncated)"));
    }

    #[test]
    fn thread_key_roundtrip() {
        assert_eq!(ThreadKey::new("C0AAA", "123.456").as_str(), "C0AAA:123.456");
        assert_eq!(
            ThreadKey::parse("C0AAA:123.456").split(),
            ("C0AAA".into(), Some("123.456".into()))
        );
        assert_eq!(ThreadKey::parse("C0AAA").split(), ("C0AAA".into(), None));
        assert_eq!(
            ThreadKey::parse("C0AAA:1:2").split(),
            ("C0AAA".into(), Some("1:2".into()))
        ); // split at the first ':'
    }
}
