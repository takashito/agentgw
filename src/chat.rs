//! Talking to the chat platform. [`Chat`] is what the core asks of it; `chat::slack` implements
//! it. Another platform would sit next to `slack` as `chat::<name>`.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

// ── what reads return ──

/// 既にある1件の投稿について、**イベントからは分からないこと**だけ。
/// [`Chat::message_at`] が返す。
pub struct MessageAt {
    /// 書き手(bot が投げたものには入らないことがある)。
    pub user: Option<String>,
    /// bot が投げたもの。
    pub is_bot: bool,
    /// 属するスレッドの根。返信でなければ自分自身の ts。
    pub thread_ts: String,
}

/// history/replies の1行。
#[derive(Clone)]
pub struct FetchedMsg {
    pub ts: String,
    pub user: String,
    pub text: String,
    /// スレッドの根。返信なら親の ts、根自身なら自分の ts、スレッド外なら None。
    pub thread_ts: Option<String>,
}

impl FetchedMsg {
    /// oldest-first の `[ts] user: text`。
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
    async fn set_thinking_status(
        &self,
        channel: &str,
        thread_ts: &str,
        status: &str,
    ) -> Result<(), String>;
}

pub type ChatRef = Arc<dyn Chat>;

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
    }

    impl FakeChat {
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
        async fn set_thinking_status(&self, c: &str, th: &str, s: &str) -> Result<(), String> {
            self.record(format!("status {c} {th} {s}"))
        }
    }

}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    #[tokio::test]
    async fn fake_slack_records_calls_in_order() {
        let s = FakeChat::default();
        let ts = s.post_message("C1", "hello", Some("1.0")).await.unwrap();
        s.add_reaction("C1", &ts, "eyes").await.unwrap();
        assert_eq!(
            s.calls(),
            vec!["post C1 1.0 hello".to_string(), format!("react C1 {ts} eyes")]
        );
    }
}
