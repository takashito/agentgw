//! Talking to the chat platform. [`Chat`] is what the core asks of it; `chat::slack` implements
//! it. Another platform would sit next to `slack` as `chat::<name>`.
//! The shape of what arrives ([`InboundMsg`] and friends) lives here; `chat::slack` parses into it.

use std::path::Path;
use std::sync::Arc;

pub mod slack;

use async_trait::async_trait;

// ── what arrives from the chat ──

/// 受信メッセージの出どころ。
///
/// DM とチャンネルで**言い方も既定も変わる**(DM の `pwd` は拒む /
/// チャンネルは mention の要否が違う)ので、素性を型で持つ。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChannelKind {
    Dm,
    Channel,
}

/// 受信メッセージに付いていた添付1つ。先読みダウンロードの入力。
///
/// `name` は劣化ノートに出す表示名 — Slack が name を寄越さなければ id そのもの。
#[derive(Clone, Debug)]
pub struct InboundFile {
    pub id: String,
    pub name: String,
}

/// Bridge の語彙。slack.rs が生成し、worker/endpoints も参照する。
#[derive(Clone, Debug)]
pub struct InboundMsg {
    pub channel: String,
    pub channel_kind: ChannelKind,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub user: Option<String>,
    pub is_bot: bool,
    /// Slack が付ける bot の id。`allow-bot` で許した相手かを照合するのに要る。
    pub bot_id: Option<String>,
    pub text: String,
    /// Slack の `files[]`(slack.rs が埋める)。
    pub files: Vec<InboundFile>,
    /// 先読みダウンロードの結果 — 成功したローカルパスと、失敗の劣化ノート。
    /// 受信経路(main.rs)が配達の直前に埋める。queue された分もこの値ごと持ち越す。
    pub file_paths: Vec<String>,
    pub file_errors: Vec<String>,
    /// このメッセージがリアクション由来ならその素性。ワーカーには合成テキストで
    /// 届く(`text` に入っている)が、**進捗付箋に付いた stop 絵文字だけ**は配達せず停止に使う。
    pub reaction: Option<Reaction>,
    /// ユーザーが**消した**メッセージの ts。埋まっていれば、これは削除の合図で
    /// `text` は取り消しの指示文(`deletion_notice`)。
    pub deleted_ts: Option<String>,
    /// ユーザーが**書き換えた**メッセージの ts と、その改訂 id(dedup 用)。
    /// 埋まっていれば `text` は**新しい本文そのもの** — 指示文は Bridge が組む
    /// (「いま処理中のものか」で言い方が変わり、それを知っているのは Bridge だけ)。
    pub edited: Option<Edited>,
}

/// 書き換え1件。`revision` は Slack の `edited.ts`(同じ編集が再配達されたときの目印)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edited {
    pub ts: String,
    pub revision: String,
}

/// リアクション1つ。
///
/// `item_ts` は**付けられた側のメッセージ**の ts — 付箋かどうかをこれで見分ける
/// (付箋以外への stop 絵文字はただのリアクション)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reaction {
    pub emoji: String,
    pub item_ts: String,
    pub added: bool,
}

/// stop として扱うリアクション名(6つ)。`hand` と `raised_hand` は
/// 同じ ✋ の別名なので、どちらを選んでも通す。**コロンは付かない** — Slack のリアクション名は
/// `red_circle` の形で来る(本文の `:red_circle:` とは別物)。
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

    /// ワーカーに渡す合成テキスト。リアクションは
    /// 軽い合図なので、**黙らないように**と念を押す一文が付く(added のときだけ)。
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

/// 書き換えをワーカーに伝える文。
/// `is_current` = いま処理中の依頼が書き換わった(捨てて新しい方をやる)/ そうでなければ
/// 過去の依頼の改訂(いまの仕事は続け、手が空いてから)。
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

/// 取り消しをワーカーに伝える文。
/// **「消されました」とだけ言い返させない** — まだ外に出していない作業は捨てる、
/// もう外に出した副作用があるときだけ説明する、という判断をさせる。
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
    async fn fake_chat_records_calls_in_order() {
        let s = FakeChat::default();
        let ts = s.post_message("C1", "hello", Some("1.0")).await.unwrap();
        s.add_reaction("C1", &ts, "eyes").await.unwrap();
        assert_eq!(
            s.calls(),
            vec!["post C1 1.0 hello".to_string(), format!("react C1 {ts} eyes")]
        );
    }

    /// stop になるのは**付けられた**stop 絵文字だけ。合成テキストは Bun の原文。
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
        // 長い本文は 280 文字で切る
        let long = r("eyes", true).synthetic_text("U1", &"あ".repeat(400));
        assert!(long.contains(&format!("{}…", "あ".repeat(280))));
    }

    /// 書き換えの文は「いま処理中か」で言い方が変わる。処理中なら古い作業を捨てさせ、
    /// 過去の依頼なら今の仕事を続けさせる。
    #[test]
    fn an_edit_tells_the_worker_to_redo_only_when_it_is_the_current_work() {
        let now = edit_notice("1.2", "こっちでお願い", false, true);
        assert!(now.contains("the message you are CURRENTLY working on (id 1.2)"));
        assert!(now.contains("Throw away the work you were doing for the OLD wording"));
        assert!(now.contains("The new content is:\n\"\"\"\nこっちでお願い\n\"\"\""));

        let past = edit_notice("1.2", "こっちでお願い", false, false);
        assert!(past.contains("EDITED an earlier message (id 1.2)"));
        assert!(past.contains("Do NOT abandon your current work"));
        // どちらも締めは同じ — 何も変わらないなら黙る
        for n in [&now, &past] {
            assert!(n.ends_with("if the edit changes nothing you need to do, call no_reply."));
        }
        // 本文が消えた編集は、添付だけになったのか読めないのかで言い分ける
        assert!(edit_notice("1.2", "", true, true).contains("a file/attachment only"));
        assert!(edit_notice("1.2", "", false, true).contains("Its new text is unavailable."));
        assert!(edit_notice("1.2", &"あ".repeat(1500), false, true).contains("… (truncated)"));
    }
}
