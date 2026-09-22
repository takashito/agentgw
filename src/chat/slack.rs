//! Traffic in and out of Slack. slack-morphism types never leave this file.

pub mod sticky;
pub use sticky::{StickyAction, StickyBoard, ToolStatus};

use crate::chat::{
    ChannelKind, Edited, FetchedMsg, InboundFile, InboundMsg, MessageAt, Reaction, deletion_notice,
};
use crate::log::LogCtx;
use crate::chat::ThreadKey;
use crate::chat::Chat;
use slack_morphism::prelude::*;
use std::sync::Arc;

// ── the Slack Web API ──────────────────────────────────────────────────────

/// Slack Web API. Callers never see slack-morphism types.
pub struct Api {
    client: Arc<SlackHyperClient>,
    token: SlackApiToken,
    bot_token: String,
}

impl Api {
    pub fn new(bot_token: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self {
            client: Arc::new(SlackClient::new(SlackClientHyperConnector::new()?)),
            token: SlackApiToken::new(bot_token.to_string().into()),
            bot_token: bot_token.to_string(),
        })
    }

    /// Returns the posted ts.
    pub async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String> {
        self.post(channel, text, thread_ts, true).await
    }

    /// Posting path for commands the Bridge answers itself. Suppresses link previews —
    /// the `status` body is full of thread permalinks, and unfurled each one becomes a big card
    /// and the post becomes unreadable (all command replies go out this way).
    pub async fn post_message_no_unfurl(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String> {
        self.post(channel, text, thread_ts, false).await
    }

    /// Post as standard Markdown (Slack's `markdown` block).
    ///
    /// Unlike mrkdwn (`*bold*`, no headings, **no tables**), `##` headings, tables, checkboxes and
    /// code blocks with a language render as is. Slack added it in 2025-02 and extended it to tables in 2026-03.
    /// **Keep `text`** — notifications (push, list previews) read that one, and
    /// a blocks-only post gives an empty notification.
    pub async fn post_markdown(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String> {
        self.post_with(channel, text, thread_ts, true, true).await
    }

    async fn post(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
        unfurl: bool,
    ) -> Result<String, String> {
        self.post_with(channel, text, thread_ts, unfurl, false)
            .await
    }

    async fn post_with(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
        unfurl: bool,
        markdown: bool,
    ) -> Result<String, String> {
        let mut content = SlackMessageContent::new().with_text(text.to_string());
        if markdown {
            content = content.with_blocks(vec![SlackBlock::Markdown(SlackMarkdownBlock {
                block_id: None,
                text: text.to_string(),
            })]);
        }
        let mut req = SlackApiChatPostMessageRequest::new(channel.into(), content);
        if let Some(ts) = thread_ts {
            req = req.with_thread_ts(ts.into());
        }
        // Only touched when suppressing — the default path (the agent's reply) must look the same as before
        if !unfurl {
            req = req.with_unfurl_links(false).with_unfurl_media(false);
        }
        let res = self
            .client
            .open_session(&self.token)
            .chat_post_message(&req)
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.ts.to_string())
    }

    /// Open the DM channel with the Owner and return its id (`conversations.open`).
    /// The notification target when there is no home channel — if it is already open the same id comes back,
    /// so calling it again never creates a new conversation.
    pub async fn open_dm(&self, user_id: &str) -> Result<String, String> {
        let req = SlackApiConversationsOpenRequest::new().with_users(vec![user_id.into()]);
        let res = self
            .client
            .open_session(&self.token)
            .conversations_open(&req)
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.channel.id.to_string())
    }

    /// Block Kit prompt asking a person for tool permission. Returns the post's ts, which
    /// is deleted on expiry (otherwise an Allow pressed later reaches an agent that has long since
    /// moved on, and "pressing it does nothing").
    ///
    /// In a DM `channel_grant_label` is "Allow for User" — a DM has one person, so the same
    /// allow-channel means "never ask this person again".
    pub async fn post_perm_prompt(
        &self,
        channel: &str,
        thread_ts: &str,
        req_id: &str,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> Result<String, String> {
        let preview: String = serde_json::to_string(tool_input)
            .unwrap_or_default()
            .chars()
            .take(600)
            .collect();
        let body = format!(":lock: *Permission requested* — `{tool_name}`\n```{preview}```");
        let channel_grant_label = if channel.starts_with('D') {
            "Allow for User"
        } else {
            "Allow for Channel"
        };
        let button = |label: &str, action: &str| {
            SlackBlockButtonElement::new(
                format!("perm:{action}:{req_id}").into(),
                SlackBlockPlainTextOnly::from(label),
            )
        };
        let blocks: Vec<SlackBlock> = vec![
            SlackSectionBlock::new()
                .with_text(SlackBlockText::MarkDown(body.clone().into()))
                .into(),
            SlackActionsBlock::new(vec![
                button("Allow", "allow")
                    .with_style(SlackBlockButtonStyle::Primary)
                    .into(),
                button("Allow for thread", "allow-thread").into(),
                button(channel_grant_label, "allow-channel").into(),
                button("Deny", "deny")
                    .with_style(SlackBlockButtonStyle::Danger)
                    .into(),
            ])
            .into(),
        ];
        let req = SlackApiChatPostMessageRequest::new(
            channel.into(),
            SlackMessageContent::new()
                .with_text(format!("Permission: {tool_name}"))
                .with_blocks(blocks),
        )
        .with_thread_ts(thread_ts.into())
        .with_unfurl_links(false);
        let res = self
            .client
            .open_session(&self.token)
            .chat_post_message(&req)
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.ts.to_string())
    }

    pub async fn add_reaction(&self, channel: &str, ts: &str, emoji: &str) -> Result<(), String> {
        let req = SlackApiReactionsAddRequest::new(channel.into(), emoji.into(), ts.into());
        self.client
            .open_session(&self.token)
            .reactions_add(&req)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub async fn remove_reaction(
        &self,
        channel: &str,
        ts: &str,
        emoji: &str,
    ) -> Result<(), String> {
        let req = SlackApiReactionsRemoveRequest::new(emoji.into())
            .with_channel(channel.into())
            .with_timestamp(ts.into());
        self.client
            .open_session(&self.token)
            .reactions_remove(&req)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub async fn delete_message(&self, channel: &str, ts: &str) -> Result<(), String> {
        let req = SlackApiChatDeleteRequest::new(channel.into(), ts.into());
        self.client
            .open_session(&self.token)
            .chat_delete(&req)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Rewrite as standard Markdown (`markdown` block).
    ///
    /// **Progress messages and permission prompts do not go through here** — they are hand-written mrkdwn
    /// (`*bold*`), and read as Markdown the bold turns into italics. Only the agent's
    /// `edit_message` (= replacing an answer) uses it.
    pub async fn update_markdown(&self, channel: &str, ts: &str, text: &str) -> Result<(), String> {
        self.update_with(channel, ts, text, true).await
    }

    pub async fn update_message(&self, channel: &str, ts: &str, text: &str) -> Result<(), String> {
        self.update_with(channel, ts, text, false).await
    }

    async fn update_with(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
        markdown: bool,
    ) -> Result<(), String> {
        let mut content = SlackMessageContent::new().with_text(text.to_string());
        if markdown {
            content = content.with_blocks(vec![SlackBlock::Markdown(SlackMarkdownBlock {
                block_id: None,
                text: text.to_string(),
            })]);
        }
        let req = SlackApiChatUpdateRequest::new(channel.into(), content, ts.into());
        self.client
            .open_session(&self.token)
            .chat_update(&req)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Channel history (the caller puts it oldest-first).
    pub async fn history(&self, channel: &str, limit: u16) -> Result<Vec<FetchedMsg>, String> {
        let req = SlackApiConversationsHistoryRequest::new()
            .with_channel(channel.into())
            .with_limit(limit);
        let res = self
            .client
            .open_session(&self.token)
            .conversations_history(&req)
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.messages.iter().map(Self::fetched).collect())
    }

    /// Root of the thread the message at this ts belongs to.
    ///
    /// Edit and delete events **do not tell us the thread** (the library's types
    /// drop the nested `thread_ts`), so we ask Slack for this one message.
    /// If it is not a reply, the thread root is the message itself — then `ts` is returned as is.
    pub async fn parent_thread_of(&self, channel: &str, ts: &str) -> Option<String> {
        self.message_at(channel, ts).await.map(|m| m.thread_ts)
    }

    /// **Author and thread** of the message at this ts.
    ///
    /// Reaction notifications carry **neither** (`item` has only the type, channel and ts).
    /// We look it up once for that reason. Without it we cannot tell
    /// "whose post the reaction is on", and the thread would have to be approximated by the ts.
    ///
    /// **`conversations.history` cannot do this.** history returns only top-level channel posts and
    /// **contains no thread replies at all** — looking up a reply's ts returns "some earlier post",
    /// which the ts check then rejects (measured 2026-08-02: an ✗ on a post inside a thread
    /// failed with `could not read`). `conversations.replies` takes the reply's ts as is and
    /// returns **that post itself** with its author and `thread_ts` (confirmed by measurement).
    pub async fn message_at(&self, channel: &str, ts: &str) -> Option<MessageAt> {
        let req = SlackApiConversationsRepliesRequest::new(channel.into(), ts.into())
            .with_limit(1)
            .with_inclusive(true);
        let res = self
            .client
            .open_session(&self.token)
            .conversations_replies(&req)
            .await
            .map_err(|e| {
                LogCtx::default().debug(
                    "slack",
                    &format!("parent lookup failed for {channel}:{ts}: {e}"),
                );
            })
            .ok()?;
        let m = res.messages.first()?;
        // Check that what came back really is that ts (the thread root can come back instead)
        if m.origin.ts.to_string() != ts {
            return None;
        }
        Some(MessageAt {
            user: m.sender.user.as_ref().map(|u| u.to_string()),
            is_bot: m.sender.bot_id.is_some(),
            thread_ts: m
                .origin
                .thread_ts
                .as_ref()
                .map_or_else(|| ts.to_string(), |t| t.to_string()),
        })
    }

    pub async fn replies(
        &self,
        channel: &str,
        thread_ts: &str,
        limit: u16,
    ) -> Result<Vec<FetchedMsg>, String> {
        // inclusive: include the message at the given ts itself
        let req = SlackApiConversationsRepliesRequest::new(channel.into(), thread_ts.into())
            .with_limit(limit)
            .with_inclusive(true);
        let res = self
            .client
            .open_session(&self.token)
            .conversations_replies(&req)
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.messages.iter().map(Self::fetched).collect())
    }

    /// Whether the thread root is gone. Posting with `thread_ts` to a deleted root makes Slack
    /// drop it **as a top-level channel message**, which clutters the channel.
    ///
    /// **Only claim it is gone when we know it "does not exist"**. Network and other API
    /// errors are not evidence of deletion, so return `false` (= post) — if the connection is really broken
    /// the post fails too, so nothing gets cluttered, and swallowing a legitimate post over a brief glitch is worse.
    ///
    /// **Not called for DMs** (roots do not vanish that way there; the caller filters out ids starting with `D`).
    pub async fn thread_root_gone(&self, channel: &str, thread_ts: &str) -> bool {
        match self.replies(channel, thread_ts, 1).await {
            Ok(msgs) => msgs.is_empty(),
            Err(e) => {
                let gone = e.contains("thread_not_found") || e.contains("message_not_found");
                if !gone {
                    LogCtx::default().debug(
                        "slack",
                        &format!("thread-root probe {channel}:{thread_ts} inconclusive: {e}"),
                    );
                }
                gone
            }
        }
    }

    /// Fetch url_private with a Bearer token and write it to a file.
    /// ponytail: shells out to curl. Every slack-morphism HTTP helper assumes JSON deserialization and
    /// has no way to return raw bytes, and adding dependencies is not allowed. Revisit when attachments get serious.
    pub async fn download_to(&self, url: &str, dest: &std::path::Path) -> Result<(), String> {
        // **The token goes in on stdin, never in argv** — a bot token is workspace-wide, and anyone running
        // `ps auxww` during a download would read it off the command line.
        let config = format!("header = \"Authorization: Bearer {}\"\n", self.bot_token);
        // `--max-time` is the real limit on the process side. If the caller drops the future, curl lives on
        // and keeps writing into the inbox after we said "timed out", so the limit must apply to the child too
        let max_time = DOWNLOAD_TIMEOUT.as_secs().to_string();
        let mut child = tokio::process::Command::new("/usr/bin/curl")
            .args(["-sSfL", "--max-time", &max_time, "--config", "-", "-o"])
            .arg(dest)
            .arg(url)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("curl: {e}"))?;
        {
            use tokio::io::AsyncWriteExt;
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| "curl: no stdin".to_string())?;
            stdin
                .write_all(config.as_bytes())
                .await
                .map_err(|e| format!("curl: {e}"))?;
        }
        let out = child
            .wait_with_output()
            .await
            .map_err(|e| format!("curl: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "download failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Upload one local file to Slack (getUploadURLExternal → raw byte PUT → completeUploadExternal).
    /// The content type is left to Slack's auto-detection (only the filename is passed).
    pub async fn upload_file(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        path: &std::path::Path,
    ) -> Result<(), String> {
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
            return Err(format!(
                "{} is {} bytes (max {MAX_ATTACHMENT_BYTES})",
                path.display(),
                bytes.len()
            ));
        }
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());
        let session = self.client.open_session(&self.token);
        let got = session
            .get_upload_url_external(&SlackApiFilesGetUploadUrlExternalRequest::new(
                filename,
                bytes.len(),
            ))
            .await
            .map_err(|e| e.to_string())?;
        session
            .files_upload_via_url(&SlackApiFilesUploadViaUrlRequest::new(
                got.upload_url,
                bytes,
                "application/octet-stream".to_string(),
            ))
            .await
            .map_err(|e| e.to_string())?;
        let mut complete =
            SlackApiFilesCompleteUploadExternalRequest::new(vec![SlackApiFilesComplete::new(
                got.file_id,
            )])
            .with_channel_id(channel.into());
        if let Some(ts) = thread_ts {
            complete = complete.with_thread_ts(ts.into());
        }
        session
            .files_complete_upload_external(&complete)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Permalink to a message (attached to each thread line of `status`).
    pub async fn get_permalink(&self, channel: &str, ts: &str) -> Result<String, String> {
        let req = SlackApiChatGetPermalinkRequest::new(channel.into(), ts.into());
        let res = self
            .client
            .open_session(&self.token)
            .chat_get_permalink(&req)
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.permalink.to_string())
    }

    /// Show Slack's native assistant status (a quiet shimmer) on a thread.
    /// **Sending an empty string clears it**. Unlike a post, it makes no notification and leaves nothing to read later.
    /// It is a DM-only API, so in channels it is a harmless no-op.
    pub async fn set_thinking_status(
        &self,
        channel: &str,
        thread_ts: &str,
        status: &str,
    ) -> Result<(), String> {
        let req = SlackApiAssistantThreadsSetStatusRequest::new(
            channel.into(),
            status.to_string(),
            thread_ts.into(),
        );
        self.client
            .open_session(&self.token)
            .assistant_threads_set_status(&req)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// GET read as raw JSON. For reading fields that slack-morphism's typed models **drop**
    /// (a DM's `channel.user`, `user.profile.bot_id` — neither is in the 2.24.0 models).
    /// `ok:false` is already turned into Err by the connector before it gets here.
    async fn get_json(
        &self,
        method: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, String> {
        let params: Vec<(&str, Option<&str>)> = params.iter().map(|&(k, v)| (k, Some(v))).collect();
        let session = self.client.open_session(&self.token);
        session
            .http_session_api
            .http_get(method, &params, None)
            .await
            .map_err(|e| e.to_string())
    }

    /// Human-readable channel name. For a DM (`is_im`) the other person's `@name`, for a channel `#name`.
    /// best-effort — None on failure.
    /// Channels the bot is in, as (id, name). One page of 200 is plenty for the fleets this serves;
    /// a workspace with more would need the cursor.
    pub async fn bot_channels(&self) -> Result<Vec<(String, String)>, String> {
        let page = self
            .get_json(
                "users.conversations",
                &[
                    ("types", "public_channel,private_channel"),
                    ("exclude_archived", "true"),
                    ("limit", "200"),
                ],
            )
            .await?;
        Ok(page["channels"]
            .as_array()
            .map(|cs| {
                cs.iter()
                    .filter_map(|c| {
                        let id = c["id"].as_str()?.to_string();
                        let name = c["name"].as_str().unwrap_or_default().to_string();
                        Some((id, name))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    pub async fn channel_display_name(&self, channel: &str) -> Option<String> {
        // An empty string counts as "missing" and falls through to the next candidate
        let s = |v: &serde_json::Value, k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .filter(|x| !x.is_empty())
                .map(str::to_string)
        };
        let resolved: Result<Option<String>, String> = async {
            let c = self
                .get_json("conversations.info", &[("channel", channel)])
                .await?;
            let c = c.get("channel").cloned().unwrap_or_default();
            if c.get("is_im").and_then(|v| v.as_bool()) != Some(true) {
                return Ok(s(&c, "name").map(|n| format!("#{n}")));
            }
            let Some(peer) = s(&c, "user") else {
                return Ok(None);
            };
            Ok(self.user_display_name(&peer).await)
        }
        .await;
        resolved.unwrap_or_else(|e| {
            let ctx = LogCtx {
                session_id: None,
                thread_key: Some(ThreadKey::parse(channel)),
            };
            let m = format!("status: channel name resolve failed for {channel}: {e}");
            ctx.debug("bridge", &m);
            None
        })
    }

    /// Human-readable `@name`. A raw `U…` tells nobody who it is, so the owner field of `status` and
    /// DM channel names go through this. best-effort — None on failure.
    pub async fn user_display_name(&self, user: &str) -> Option<String> {
        let s = |v: &serde_json::Value, k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .filter(|x| !x.is_empty())
                .map(str::to_string)
        };
        let resolved: Result<Option<String>, String> = async {
            let u = self.get_json("users.info", &[("user", user)]).await?;
            let u = u.get("user").cloned().unwrap_or_default();
            Ok(u.get("profile")
                .and_then(|p| s(p, "display_name"))
                .or_else(|| s(&u, "real_name"))
                .or_else(|| s(&u, "name"))
                .map(|n| format!("@{n}")))
        }
        .await;
        resolved.unwrap_or_else(|e| {
            let m = format!("status: user name resolve failed for {user}: {e}");
            LogCtx::default().debug("bridge", &m);
            None
        })
    }

    /// This bot's own user id (`U…`) and display name. The id is needed to strip self-mentions from text commands,
    /// and the name goes into the startup notice in home (a raw `U…` means nothing to a person;
    /// the earlier implementation showed `authResult.user`). Called once at startup.
    pub async fn auth_test(&self) -> Result<(String, Option<String>), String> {
        self.client
            .open_session(&self.token)
            .auth_test()
            .await
            .map(|r| (r.user_id.to_string(), r.user))
            .map_err(|e| e.to_string())
    }

    /// If a mention (`<@U…>`) is a bot, return its `bot_id` (B…), the key for the ledger. Ok(None) for humans.
    /// A bot without `profile.bot_id` is Err — so an id that cannot be saved is never dropped silently.
    pub async fn resolve_bot_id(&self, user_id: &str) -> Result<Option<String>, String> {
        let v = self.get_json("users.info", &[("user", user_id)]).await?;
        let u = v
            .get("user")
            .ok_or_else(|| format!("users.info returned no user for {user_id}"))?;
        if u.get("is_bot").and_then(|v| v.as_bool()) != Some(true) {
            return Ok(None);
        }
        u.get("profile")
            .and_then(|p| p.get("bot_id"))
            .and_then(|v| v.as_str())
            .map(|b| Some(b.to_string()))
            .ok_or_else(|| format!("{user_id} is a bot but users.info exposed no profile.bot_id"))
    }

    /// An attachment's (url_private, file name, byte count). The size is used for the limit check —
    /// slack-morphism's `SlackFile` has no size, so it is read from raw JSON
    /// (same approach as `resolve_bot_id`).
    pub async fn file_info(&self, file_id: &str) -> Result<(String, String, u64), String> {
        let v = self.get_json("files.info", &[("file", file_id)]).await?;
        let f = v
            .get("file")
            .ok_or_else(|| format!("files.info returned no file for {file_id}"))?;
        let url = f
            .get("url_private")
            .and_then(|v| v.as_str())
            .ok_or("file has no url_private")?;
        let name = f.get("name").and_then(|v| v.as_str()).unwrap_or(file_id);
        let size = f.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
        Ok((url.to_string(), name.to_string(), size))
    }

    // ── Shared conventions for each Slack call the Bridge makes ──

    /// Open Socket Mode, normalize message events and stream them.
    /// AppMention is dropped — the same message also arrives as a Message (a second line of defense next to dedup).
    /// When `fleet` is `Some` (= a gateway with machines), events are handed there **raw, without folding**.
    /// Once it decides who handles them, only our own share comes back to `tx` / `clicks` — so
    /// local delivery goes through the same conversion as a direct connection ([`inbound_of`]).
    pub async fn listen(
        app_token: &str,
        tx: tokio::sync::mpsc::Sender<InboundMsg>,
        clicks: tokio::sync::mpsc::Sender<PermClick>,
        fleet: Option<tokio::sync::mpsc::Sender<FleetEvent>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let token = SlackApiToken::new(app_token.to_string().into());
        let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new()?));
        let mut listener_env = SlackClientEventsListenerEnvironment::new(client.clone())
            .with_error_handler(on_listener_error)
            .with_user_state(tx)
            .with_user_state(clicks);
        if let Some(fleet) = fleet {
            listener_env = listener_env.with_user_state(fleet);
        }
        let env = Arc::new(listener_env);
        let callbacks = SlackSocketModeListenerCallbacks::new()
            .with_hello_events(|hello, _client, _state| async move {
                match connection_warning(hello.num_connections) {
                    Some(w) => LogCtx::default().error("slack", &w),
                    None => LogCtx::default()
                        .info("slack", "socket mode: 1 connection (as it should be)"),
                }
            })
            .with_push_events(on_push_event)
            .with_interaction_events(on_interaction_event);
        let listener =
            SlackClientSocketModeListener::new(&SlackClientSocketModeConfig::new(), env, callbacks);
        listener.listen_for(&token).await?;
        LogCtx::default().info("slack", "socket mode connected");
        listener.serve().await;
        Ok(())
    }

    /// File name used in the inbox. `name` is **external input from Slack** and may contain path separators,
    /// like `a/b.txt` or `../../etc/passwd` — joining it as is could write outside the inbox
    /// (which is why the name is never used raw as a path, as in `{ts}-{fileId}{ext}`).
    /// Take only the last component, drop any remaining separators, and **always end up with one component**. If nothing
    /// usable is left, just the file_id.
    pub fn attachment_file_name(file_id: &str, name: &str) -> String {
        // `Path::file_name()` gives "a/b.txt" → "b.txt", and ".." or "" → None.
        // On Unix `\` is not a separator, so we drop it ourselves
        let one = |s: &str| -> String {
            // trim **before** passing to `Path::new` (so " .. " is not kept as "..")
            let base = std::path::Path::new(s.trim())
                .file_name()
                .map(|b| b.to_string_lossy().into_owned())
                .unwrap_or_default();
            // `"` breaks the envelope attribute (`file_paths="…"`), so drop it
            let base = base.replace(['/', '\\', '"', '\0'], "");
            match base.trim() {
                // Stripping can turn it into "." / ".." (e.g. `".."` wrapped in double quotes)
                "." | ".." => String::new(),
                b => b.to_string(),
            }
        };
        let id = one(file_id);
        let id = if id.is_empty() {
            "attachment".to_string()
        } else {
            id
        };
        match one(name) {
            n if n.is_empty() => id,
            n => format!("{id}-{n}"),
        }
    }

    /// Delete inbox files past their TTL.
    /// **Everything is best-effort** — one unreadable or undeletable file does not stop the sweep.
    /// A file right at the TTL boundary is kept (strict `>`).
    pub fn sweep_inbox(inbox: &std::path::Path, now_ms: u64) {
        let Ok(entries) = std::fs::read_dir(inbox) else {
            return; // not created yet / unreadable — nothing to sweep
        };
        let mut swept = 0usize;
        for e in entries.flatten() {
            let age = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| now_ms.saturating_sub(d.as_millis() as u64));
            if age.is_some_and(|a| a > INBOX_TTL_MS) && std::fs::remove_file(e.path()).is_ok() {
                swept += 1;
            }
        }
        if swept > 0 {
            LogCtx::default().info(
                "download",
                &format!("swept {swept} stale inbox file(s) (ttl {INBOX_TTL_MS}ms)"),
            );
        }
    }

    /// Give up on a Slack call during restart after 5 seconds. Everything sent here is decoration that is "nice if it lands",
    /// and one hang must not block writing the marker and exit(0).
    /// Failures and timeouts are logged and we move on.
    pub async fn brief_call<T>(
        label: &str,
        call: impl std::future::Future<Output = Result<T, String>>,
        ctx: &LogCtx,
    ) -> Option<T> {
        const CAP: std::time::Duration = std::time::Duration::from_secs(5);
        match tokio::time::timeout(CAP, call).await {
            Ok(Ok(v)) => Some(v),
            Ok(Err(e)) => {
                ctx.error("bridge", &format!("{label}: {e}"));
                None
            }
            Err(_) => {
                ctx.error(
                    "bridge",
                    &format!("{label}: timed out after {}s — carrying on", CAP.as_secs()),
                );
                None
            }
        }
    }

    fn fetched(m: &SlackHistoryMessage) -> FetchedMsg {
        FetchedMsg {
            ts: m.origin.ts.to_string(),
            user: m
                .sender
                .user
                .as_ref()
                .map(|u| u.to_string())
                .unwrap_or_else(|| "bot".to_string()),
            text: m.content.text.clone().unwrap_or_default(),
            thread_ts: m.origin.thread_ts.as_ref().map(|t| t.to_string()),
        }
    }
}

/// Helpers every Slack port gets, real or fake.
impl dyn Chat {
    /// Send one assistant status call and log it. **best-effort, but never silent** —
    /// both success and failure are logged. Not blocking the caller is the caller's responsibility.
    pub async fn thinking(&self, channel: &str, thread_ts: &str, status: &str) {
        let ctx = LogCtx {
            session_id: None,
            thread_key: Some(ThreadKey::new(channel, thread_ts)),
        };
        let what = if status.is_empty() {
            "cleared".to_string()
        } else {
            format!("set \"{status}\"")
        };
        match self.set_thinking_status(channel, thread_ts, status).await {
            Ok(()) => ctx.debug("bridge", &format!("thinking status {what}")),
            Err(e) => ctx.debug("bridge", &format!("thinking status {what} failed: {e}")),
        }
    }

    /// The actual post for a Bridge direct answer. Called from an already spawned context (a probe's answer arrives 60 seconds later).
    /// Failures are only logged — a command's answer lives outside the unanswered ledger.
    pub async fn post_now(&self, channel: &str, thread_ts: &str, text: String, key: &ThreadKey) {
        if let Err(e) = self
            .post_message_no_unfurl(channel, &text, Some(thread_ts))
            .await
        {
            LogCtx {
                session_id: None,
                thread_key: Some(key.clone()),
            }
            .error(
                "bridge",
                &format!("command post failed for {channel}:{thread_ts}: {e}"),
            );
        }
    }
}

/// Time allowed for a download: 60 seconds, used both for curl's `--max-time`
/// and for the overall deadline of prefetching on receive.
pub const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

#[async_trait::async_trait]
impl crate::chat::Chat for Api {
    async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String> {
        Api::post_message(self, channel, text, thread_ts).await
    }
    async fn post_message_no_unfurl(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String> {
        Api::post_message_no_unfurl(self, channel, text, thread_ts).await
    }
    async fn post_markdown(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String> {
        Api::post_markdown(self, channel, text, thread_ts).await
    }
    async fn update_message(&self, channel: &str, ts: &str, text: &str) -> Result<(), String> {
        Api::update_message(self, channel, ts, text).await
    }
    async fn update_markdown(&self, channel: &str, ts: &str, text: &str) -> Result<(), String> {
        Api::update_markdown(self, channel, ts, text).await
    }
    async fn delete_message(&self, channel: &str, ts: &str) -> Result<(), String> {
        Api::delete_message(self, channel, ts).await
    }
    async fn add_reaction(&self, channel: &str, ts: &str, emoji: &str) -> Result<(), String> {
        Api::add_reaction(self, channel, ts, emoji).await
    }
    async fn remove_reaction(&self, channel: &str, ts: &str, emoji: &str) -> Result<(), String> {
        Api::remove_reaction(self, channel, ts, emoji).await
    }
    async fn flip_to_received(&self, channel: &str, message_ts: &str, ack: &str) {
        flip_to_received(self, channel, message_ts, ack).await
    }
    async fn post_perm_prompt(
        &self,
        channel: &str,
        thread_ts: &str,
        req_id: &str,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> Result<String, String> {
        Api::post_perm_prompt(self, channel, thread_ts, req_id, tool_name, tool_input).await
    }
    async fn open_dm(&self, user_id: &str) -> Result<String, String> {
        Api::open_dm(self, user_id).await
    }
    async fn history(&self, channel: &str, limit: u16) -> Result<Vec<FetchedMsg>, String> {
        Api::history(self, channel, limit).await
    }
    async fn replies(
        &self,
        channel: &str,
        thread_ts: &str,
        limit: u16,
    ) -> Result<Vec<FetchedMsg>, String> {
        Api::replies(self, channel, thread_ts, limit).await
    }
    async fn parent_thread_of(&self, channel: &str, ts: &str) -> Option<String> {
        Api::parent_thread_of(self, channel, ts).await
    }
    async fn message_at(&self, channel: &str, ts: &str) -> Option<MessageAt> {
        Api::message_at(self, channel, ts).await
    }
    async fn thread_root_gone(&self, channel: &str, thread_ts: &str) -> bool {
        Api::thread_root_gone(self, channel, thread_ts).await
    }
    async fn get_permalink(&self, channel: &str, ts: &str) -> Result<String, String> {
        Api::get_permalink(self, channel, ts).await
    }
    async fn channel_display_name(&self, channel: &str) -> Option<String> {
        Api::channel_display_name(self, channel).await
    }
    async fn bot_channels(&self) -> Result<Vec<(String, String)>, String> {
        Api::bot_channels(self).await
    }
    async fn user_display_name(&self, user: &str) -> Option<String> {
        Api::user_display_name(self, user).await
    }
    async fn auth_test(&self) -> Result<(String, Option<String>), String> {
        Api::auth_test(self).await
    }
    async fn resolve_bot_id(&self, user_id: &str) -> Result<Option<String>, String> {
        Api::resolve_bot_id(self, user_id).await
    }
    async fn file_info(&self, file_id: &str) -> Result<(String, String, u64), String> {
        Api::file_info(self, file_id).await
    }
    async fn download_to(&self, url: &str, dest: &std::path::Path) -> Result<(), String> {
        Api::download_to(self, url, dest).await
    }
    async fn download_attachment(
        &self,
        file_id: &str,
        state_dir: &std::path::Path,
    ) -> Result<String, String> {
        download_attachment(self, file_id, state_dir).await
    }
    async fn upload_file(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        path: &std::path::Path,
    ) -> Result<(), String> {
        Api::upload_file(self, channel, thread_ts, path).await
    }
    async fn set_thinking_status(
        &self,
        channel: &str,
        thread_ts: &str,
        status: &str,
    ) -> Result<(), String> {
        Api::set_thinking_status(self, channel, thread_ts, status).await
    }
}

// ── reading what Slack sends (Socket Mode events, button clicks) ────────────

/// A raw Slack event taken in by the gateway. Kept **in a form that can be forwarded to a machine as is**
/// (deciding which machine handles it is the job of `bridge::gateway`; this only carries it).
#[derive(Debug, Clone)]
pub enum FleetEvent {
    Event {
        name: String,
        event: serde_json::Value,
    },
    Action {
        action: serde_json::Value,
        body: serde_json::Value,
    },
}

/// Events the gateway forwards to a machine. **Only those the Bridge knows how to handle** — kinds not listed here
/// get no name and are dropped (this match is the subscription table itself).
fn fleet_event_of(ev: &SlackEventCallbackBody) -> Option<FleetEvent> {
    let (name, value) = match ev {
        SlackEventCallbackBody::Message(e) => ("message", serde_json::to_value(e)),
        SlackEventCallbackBody::ReactionAdded(e) => ("reaction_added", serde_json::to_value(e)),
        SlackEventCallbackBody::ReactionRemoved(e) => ("reaction_removed", serde_json::to_value(e)),
        SlackEventCallbackBody::MemberJoinedChannel(e) => {
            ("member_joined_channel", serde_json::to_value(e))
        }
        _ => return None,
    };
    Some(FleetEvent::Event {
        name: name.to_string(),
        event: value.ok()?,
    })
}

/// One pressed approval button. `action_id` is `perm:<action>:<reqId>`.
#[derive(Debug, Clone)]
pub struct PermClick {
    pub req_id: String,
    /// `allow` / `deny` / `allow-thread` / `allow-channel`
    pub action: String,
    /// Who pressed it (for audit and logs).
    pub by: String,
}

/// Block Kit buttons. Only **the fact that it was pressed** is passed to the main loop; no decision is made here
/// (Slack resends if event handling does not return within 3 seconds).
/// Slack sends `hello` on every connection, carrying **the number of connections the app currently holds**
/// (`num_connections`). The Web API has no way to return that count, so this is the only clue.
///
/// **Even a healthy state has 2.** slack-morphism opens two by default
/// (`SlackClientSocketModeConfig::DEFAULT_CONNECTIONS_COUNT = 2`: redundancy so one keeps receiving while Slack
/// periodically asks the other to reconnect; they are in the same process, so nothing is lost).
/// The startup log on a real machine also showed hello twice, counting `1` → `2` (measured 2026-08-01).
///
/// **From the third on it is an accident.** It means another process is connected with the same app token,
/// and Slack does not duplicate events — it **splits them between the two**. This design relies on exactly
/// one instance ever being connected to Slack. **We cannot tell who is connected** — only the count — but
/// that is far better than silently losing half the events, so we report it.
///
/// > `SlackSocketModeHelloEvent` **cannot be referenced by name** from slack-morphism 2.24.0
/// > (`models` is private and the glob re-export is shadowed by a module of the same name). So we do not write the type
/// > and take it with a closure whose argument type is inferred.
/// The number of connections this process opens itself. Anything above it belongs to **someone else**.
const OWN_CONNECTIONS: u32 = 2;

/// **Pure function** — returns what to say if the count exceeds our own.
pub fn connection_warning(num_connections: u32) -> Option<String> {
    (num_connections > OWN_CONNECTIONS).then(|| {
        format!(
            "socket mode: this Slack app has {num_connections} live connections but this Bridge \
         opens {OWN_CONNECTIONS} — someone else is consuming events. Slack SPLITS them at \
         random between consumers (it does not copy), so half of them land nowhere. \
         Another Bridge, or a dev/production token mix-up."
        )
    })
}

async fn on_interaction_event(
    event: SlackInteractionEvent,
    _client: Arc<SlackHyperClient>,
    state: SlackClientEventsUserState,
) -> UserCallbackResult<()> {
    // Record **the fact that it arrived**. With Interactivity disabled in the Slack app, not a single line
    // shows up here — this one line tells whether "pressing does nothing" is on the settings side or ours
    let SlackInteractionEvent::BlockActions(ev) = event else {
        LogCtx::default().debug("slack", "interaction (not block_actions) — ignored");
        return Ok(());
    };
    let by = ev
        .user
        .as_ref()
        .map(|u| u.id.to_string())
        .unwrap_or_default();
    let actions: Vec<_> = ev.actions.clone().into_iter().flatten().collect();
    {
        let guard = state.read().await;
        if let Some(fleet) = guard.get_user_state::<tokio::sync::mpsc::Sender<FleetEvent>>() {
            let body = serde_json::to_value(SlackInteractionBlockActionsEvent {
                actions: Some(actions.clone()),
                ..ev.clone()
            })
            .unwrap_or(serde_json::Value::Null);
            for a in &actions {
                let action = serde_json::to_value(a).unwrap_or(serde_json::Value::Null);
                if let Err(e) = fleet
                    .send(FleetEvent::Action {
                        action,
                        body: body.clone(),
                    })
                    .await
                {
                    LogCtx::default().error("slack", &format!("fleet queue closed: {e}"));
                }
            }
            return Ok(());
        }
    }
    LogCtx::default().debug(
        "slack",
        &format!(
            "interaction block_actions by={by} actions={}",
            actions.len()
        ),
    );
    for a in actions {
        let id = a.action_id.to_string();
        // `perm:<action>:<reqId>` — reqId itself contains no `:`, so splitting into 3 is enough
        let mut parts = id.splitn(3, ':');
        if parts.next() != Some("perm") {
            continue;
        }
        let (Some(action), Some(req_id)) = (parts.next(), parts.next()) else {
            continue;
        };
        let click = PermClick {
            req_id: req_id.to_string(),
            action: action.to_string(),
            by: by.clone(),
        };
        let guard = state.read().await;
        match guard.get_user_state::<tokio::sync::mpsc::Sender<PermClick>>() {
            Some(tx) => {
                if let Err(e) = tx.send(click).await {
                    LogCtx::default().error("slack", &format!("perm click dropped: {e}"));
                }
            }
            None => LogCtx::default().error("slack", "no perm-click channel in listener state"),
        }
    }
    Ok(())
}

/// Catch-all for when slack-morphism **could not read an event and dropped it**. The default handler
/// writes only to `tracing`, so nothing reaches our log — "Slack did not send it" and
/// "we could not read it" become indistinguishable (this dragged out the deletion investigation on 2026-07-31).
///
/// Unreadable cases really exist: slack-morphism keeps message kinds (`subtype`) as **a fixed list**,
/// and a kind not on it drops the whole `SlackMessageEvent`. The day Slack adds a new kind we would
/// silently miss events, so at least leave one line.
fn on_listener_error(
    err: Box<dyn std::error::Error + Send + Sync>,
    _client: Arc<SlackHyperClient>,
    _state: SlackClientEventsUserState,
) -> HttpStatusCode {
    LogCtx::default().error("slack", &format!("listener dropped an event: {err}"));
    HttpStatusCode::BAD_REQUEST
}

async fn on_push_event(
    event: SlackPushEventCallback,
    _client: Arc<SlackHyperClient>,
    state: SlackClientEventsUserState,
) -> UserCallbackResult<()> {
    let guard = state.read().await;
    // A gateway (a Bridge with machines) does not fold before deciding who handles it
    if let Some(fleet) = guard.get_user_state::<tokio::sync::mpsc::Sender<FleetEvent>>() {
        if let Some(item) = fleet_event_of(&event.event)
            && let Err(e) = fleet.send(item).await
        {
            LogCtx::default().error("slack", &format!("fleet queue closed: {e}"));
        }
        return Ok(());
    }
    let Some(msg) = inbound_of(event.event) else {
        return Ok(());
    };
    let Some(tx) = guard.get_user_state::<tokio::sync::mpsc::Sender<InboundMsg>>() else {
        LogCtx::default().error("slack", "no inbound channel in listener state");
        return Ok(());
    };
    if let Err(e) = tx.send(msg).await {
        LogCtx::default().error("slack", &format!("inbound queue closed: {e}"));
    }
    Ok(())
}

/// Slack message event → the Bridge's vocabulary. None for shapes we cannot reply to (no content and no attachment, no channel).
/// Slack push events into the Bridge's vocabulary. None for anything to drop.
fn message_of(ev: &SlackMessageEvent) -> Option<InboundMsg> {
    let content = ev.content.as_ref()?;
    // An image-only post arrives as `subtype:"file_share"` with no text. Even without text,
    // let it through if it has attachments
    let files: Vec<InboundFile> = content
        .files
        .iter()
        .flatten()
        .map(|f| InboundFile {
            id: f.id.to_string(),
            name: f.name.clone().unwrap_or_else(|| f.id.to_string()),
        })
        .collect();
    // Messages with a subtype are **system messages** ("X joined", "changed the topic").
    // Only `bot_message`, `thread_broadcast`, and those with attachments (file_share = an image-only
    // post) get through (the gate). Without this, even people joining and leaving the
    // channel would be delivered to the agent
    if let Some(sub) = &ev.subtype
        && !matches!(
            sub,
            SlackMessageEventType::BotMessage | SlackMessageEventType::ThreadBroadcast
        )
        && files.is_empty()
    {
        return None;
    }
    let text = content.text.clone();
    if text.is_none() && files.is_empty() {
        return None;
    }
    let text = text.unwrap_or_default();
    let channel = ev.origin.channel.as_ref()?.to_string();
    let kind = match ev.origin.channel_type.as_ref().map(|t| t.0.as_str()) {
        Some("im") => ChannelKind::Dm,
        _ => ChannelKind::Channel,
    };
    Some(InboundMsg {
        channel,
        channel_kind: kind,
        ts: ev.origin.ts.to_string(),
        thread_ts: ev.origin.thread_ts.as_ref().map(|t| t.to_string()),
        user: ev.sender.user.as_ref().map(|u| u.to_string()),
        // Check so we do not pick up our own replies and loop forever (proven in spike A)
        is_bot: ev.sender.bot_id.is_some() || ev.sender.user.is_none(),
        bot_id: ev.sender.bot_id.as_ref().map(|b| b.to_string()),
        text,
        files,
        file_paths: Vec::new(),
        file_errors: Vec::new(),
        reaction: None,
        deleted_ts: None,
        edited: None,
    })
}

/// Turn a `message_deleted` into one inbound message. What the agent gets is
/// an instruction to cancel, quoting the deleted text inside it.
///
/// **Deletions of the bot's own posts are ignored** (the user merely deleted a progress message or a reply) — there is no
/// "request" to cancel, so passing it on would only confuse the agent.
fn deletion_of(ev: &SlackMessageEvent) -> Option<InboundMsg> {
    let deleted_ts = ev.deleted_ts.as_ref()?.to_string();
    let prev = ev.previous_message.as_ref();
    let sender = prev.map(|p| &p.sender);
    // What the bot wrote is not a "request". Only what people wrote gets through
    if sender.is_some_and(|s| s.bot_id.is_some() || s.user.is_none()) {
        return None;
    }
    let content = prev.and_then(|p| p.content.as_ref());
    let text = content.and_then(|c| c.text.as_deref()).unwrap_or_default();
    let had_files = content.is_some_and(|c| c.files.iter().flatten().next().is_some());
    // A delete event sometimes gives no root (`previous_message` without thread_ts).
    // For now use "the deleted ts itself" as the root; the Bridge maps it to the real root from the ledger
    let thread_ts = ev
        .origin
        .thread_ts
        .as_ref()
        .map(|t| t.to_string())
        .unwrap_or_else(|| deleted_ts.clone());
    Some(InboundMsg {
        channel: ev.origin.channel.as_ref()?.to_string(),
        channel_kind: match ev.origin.channel_type.as_ref().map(|t| t.0.as_str()) {
            Some("im") => ChannelKind::Dm,
            _ => ChannelKind::Channel,
        },
        ts: deleted_ts.clone(),
        thread_ts: Some(thread_ts),
        user: sender.and_then(|s| s.user.as_ref().map(|u| u.to_string())),
        bot_id: None,
        is_bot: false,
        text: deletion_notice(&deleted_ts, text, had_files),
        files: Vec::new(),
        file_paths: Vec::new(),
        file_errors: Vec::new(),
        reaction: None,
        deleted_ts: Some(deleted_ts),
        edited: None,
    })
}

/// Turn a `message_changed` into one inbound message. `text` is **the new body
/// itself**; the instruction to the agent is built by the Bridge (the wording depends on "whether it is being processed now",
/// and only the Bridge, which holds the ledger, knows that).
///
/// Slack fires `message_changed` for things other than edits too — when a link preview is attached,
/// when the bot edits its own progress message. **Anything whose text did not change** and **bot posts** are dropped
fn edit_of(ev: &SlackMessageEvent) -> Option<InboundMsg> {
    let m = ev.message.as_ref()?;
    if m.sender.bot_id.is_some() || m.sender.user.is_none() {
        return None; // the bot's own edit (redrawing the progress message is this)
    }
    let new_text = m.content.as_ref()?.text.clone().unwrap_or_default();
    let old_text = ev
        .previous_message
        .as_ref()
        .and_then(|p| p.content.as_ref())
        .and_then(|c| c.text.clone())
        .unwrap_or_default();
    if new_text == old_text {
        return None; // meta change such as an unfurl — not an edit
    }
    let edited_ts = m.ts.to_string();
    let files: Vec<InboundFile> = m
        .content
        .iter()
        .flat_map(|c| c.files.iter().flatten())
        .map(|f| InboundFile {
            id: f.id.to_string(),
            name: f.name.clone().unwrap_or_else(|| f.id.to_string()),
        })
        .collect();
    Some(InboundMsg {
        channel: ev.origin.channel.as_ref()?.to_string(),
        channel_kind: match ev.origin.channel_type.as_ref().map(|t| t.0.as_str()) {
            Some("im") => ChannelKind::Dm,
            _ => ChannelKind::Channel,
        },
        ts: edited_ts.clone(),
        // Edit events sometimes give no root either (same as delete; the Bridge maps it from the ledger)
        thread_ts: ev.origin.thread_ts.as_ref().map(|t| t.to_string()),
        user: m.sender.user.as_ref().map(|u| u.to_string()),
        bot_id: None,
        is_bot: false,
        text: new_text,
        files,
        file_paths: Vec::new(),
        file_errors: Vec::new(),
        reaction: None,
        deleted_ts: None,
        edited: Some(Edited {
            // An id that changes with each revision. If missing, use the text length instead (the marker for redelivery)
            revision: m
                .edited
                .as_ref()
                .map(|e| e.ts.to_string())
                .unwrap_or_else(|| {
                    m.content
                        .as_ref()
                        .map_or(0, |c| c.text.as_ref().map_or(0, |t| t.len()))
                        .to_string()
                }),
            ts: edited_ts,
        }),
    })
}

/// Turn a reaction into one inbound message. **Only reactions on messages the bot
/// wrote** are handled (same check as `authored`)
/// Emoji on exchanges between people are not passed to the agent.
///
/// The thread root is the reacted-to message's `thread_ts`, or the message itself if absent. `ts` is
/// **the reacted-to message's ts** — that is where 👀 goes and what the dedup key is.
fn reaction_of(
    item: &SlackReactionsItem,
    reactor: &str,
    emoji: String,
    added: bool,
) -> Option<InboundMsg> {
    let SlackReactionsItem::Message(m) = item else {
        return None; // reactions on files are not handled
    };
    // **The author is unknown here.** A reaction's `item` has only the type, channel and ts,
    // and the sender field is always empty. Trying to decide "is this our post" here reads the empty field
    // as "our post" and becomes a check that **lets everything through** (measured 2026-08-02). The decision is on the Bridge side —
    // after looking up the original post once (`Api::message_at`).
    let channel = m.origin.channel.as_ref()?.to_string();
    let item_ts = m.origin.ts.to_string();
    let reaction = Reaction {
        emoji,
        item_ts: item_ts.clone(),
        added,
    };
    Some(InboundMsg {
        channel,
        channel_kind: match m.origin.channel_type.as_ref().map(|t| t.0.as_str()) {
            Some("im") => ChannelKind::Dm,
            _ => ChannelKind::Channel,
        },
        ts: item_ts,
        thread_ts: m.origin.thread_ts.as_ref().map(|t| t.to_string()),
        user: Some(reactor.to_string()),
        // The one who **added** the reaction is a person. That the reacted-to message is the bot's was checked above
        is_bot: false,
        bot_id: None,
        text: reaction
            .synthetic_text(reactor, m.content.text.as_deref().unwrap_or("(no text)")),
        files: Vec::new(),
        file_paths: Vec::new(),
        file_errors: Vec::new(),
        reaction: Some(reaction),
        deleted_ts: None,
        edited: None,
    })
}

/// Fold one Slack event into the shape the Bridge handles. **Direct and relayed events both go through here** —
/// a second copy would one day make the gate's decisions differ by path.
fn inbound_of(event: SlackEventCallbackBody) -> Option<InboundMsg> {
    let msg = match event {
        // A delete is a "cancel". It has no text, so from_event cannot pick it up; separate path
        SlackEventCallbackBody::Message(ev) if ev.deleted_ts.is_some() => {
            match deletion_of(&ev) {
                Some(msg) => msg,
                // Dropping silently makes "never arrived" and "dropped" indistinguishable. Deletes
                // travel a long path (Slack → ledger mapping → cancel delivery), so leave one line
                None => {
                    LogCtx::default().debug(
                        "slack",
                        &format!(
                            "message_deleted ignored ts={:?} sender={:?}",
                            ev.deleted_ts,
                            ev.previous_message
                                .as_ref()
                                .map(|p| (p.sender.user.clone(), p.sender.bot_id.clone()))
                        ),
                    );
                    return None;
                }
            }
        }
        // A rewrite. The new text is in `message`, not in the original event's
        // `content` (from_event cannot pick it up)
        SlackEventCallbackBody::Message(ev)
            if ev.subtype.as_ref() == Some(&SlackMessageEventType::MessageChanged) =>
        {
            match edit_of(&ev) {
                Some(msg) => msg,
                None => {
                    // Leave one line for the same reason as delete. But **the bot's own edits are not logged** —
                    // redrawing the progress message fires this every second, and logging it would bury the log
                    if !ev
                        .message
                        .as_ref()
                        .is_some_and(|m| m.sender.bot_id.is_some())
                    {
                        LogCtx::default().debug(
                            "slack",
                            &format!(
                                "message_changed ignored ts={:?} sender={:?}",
                                ev.message.as_ref().map(|m| m.ts.to_string()),
                                ev.message.as_ref().and_then(|m| m.sender.user.clone())
                            ),
                        );
                    }
                    return None;
                }
            }
        }
        SlackEventCallbackBody::Message(ev) => match message_of(&ev) {
            Some(msg) => msg,
            None => {
                LogCtx::default().debug(
                    "slack",
                    &format!("dropped unanswerable message subtype={:?}", ev.subtype),
                );
                return None;
            }
        },
        // Reactions are taken too. Judging the stop emoji and composing text for the agent
        // happens on the Bridge side (it knows the progress message ts and the bot id)
        SlackEventCallbackBody::ReactionAdded(ev) => {
            match reaction_of(&ev.item, &ev.user.to_string(), ev.reaction.0, true) {
                Some(msg) => msg,
                None => return None,
            }
        }
        SlackEventCallbackBody::ReactionRemoved(ev) => {
            match reaction_of(&ev.item, &ev.user.to_string(), ev.reaction.0, false) {
                Some(msg) => msg,
                None => return None,
            }
        }
        _ => return None, // other events, including AppMention, are not used
    };
    Some(msg)
}

/// Turn raw JSON forwarded by the Relay into the **same** [`InboundMsg`] as a direct connection.
///
/// The Relay side serializes slack-morphism types with `to_value`, so this just converts back with
/// the same serde implementation. If that fails, leave one line and drop it — dropping silently makes "the Relay did not send it" and
/// "we could not read it" indistinguishable.
pub fn inbound_from_relay(name: &str, event: &serde_json::Value) -> Option<InboundMsg> {
    let body = match name {
        "message" => serde_json::from_value(event.clone()).map(SlackEventCallbackBody::Message),
        "reaction_added" => {
            serde_json::from_value(event.clone()).map(SlackEventCallbackBody::ReactionAdded)
        }
        "reaction_removed" => {
            serde_json::from_value(event.clone()).map(SlackEventCallbackBody::ReactionRemoved)
        }
        "member_joined_channel" => {
            serde_json::from_value(event.clone()).map(SlackEventCallbackBody::MemberJoinedChannel)
        }
        other => {
            LogCtx::default().debug(
                "slack",
                &format!("relay sent a \"{other}\" we do not handle"),
            );
            return None;
        }
    };
    match body {
        Ok(body) => inbound_of(body),
        Err(e) => {
            LogCtx::default().error(
                "slack",
                &format!("could not read a \"{name}\" the relay forwarded: {e}"),
            );
            None
        }
    }
}

/// Turn a button press forwarded by the Relay into a [`PermClick`]. No decision here — only the fact it was pressed.
pub fn perm_click_from_relay(
    action: &serde_json::Value,
    body: &serde_json::Value,
) -> Option<PermClick> {
    let id = action.get("action_id")?.as_str()?;
    // `perm:<action>:<reqId>` — reqId itself contains no `:`, so splitting into 3 is enough
    let mut parts = id.splitn(3, ':');
    if parts.next() != Some("perm") {
        return None;
    }
    let (action_name, req_id) = (parts.next()?, parts.next()?);
    Some(PermClick {
        req_id: req_id.to_string(),
        action: action_name.to_string(),
        by: body
            .get("user")
            .and_then(|u| u.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

// ── Wording of the assistant status (pinned in `tests`).
// Any empty string clears it, so there is no "clear" constant here.

/// Right after delivery / on returning to a thread.
pub const TYPING_STATUS: &str = "is typing…";
/// When a turn has been silent past [`SILENCE_MS`] (`THINKING_STATUS`).
pub const THINKING_STATUS: &str = "is thinking…";
/// How long before we treat it as silent. The Bun version defaulted to 5s,
/// but **the Rust version uses 3s** (user decision 2026-07-31 — thinking took too long to come back after a tool ran).
pub const SILENCE_MS: u64 = 3_000;
/// Slack's "…ing" status line. Use as `thinking.set(&Status::Login.text())`.
///
/// There is **deliberately no** dedicated status for `compact` (a user decision).
/// The earlier implementation showed a "compacting context…" status during compact and re-set it with `… {n}s` on every
/// tick. The Rust version drops this — compact posts a progress checklist (sticky) and keeps
/// editing it, so a shimmer on top would be redundant (**not a porting omission**).
/// Delivery's `is typing…` and silence's `is thinking…` still show during compact as before.
#[derive(Clone, Copy, Debug)]
pub enum Status {
    /// `status`.
    Gathering,
    /// `context`.
    Context,
    /// `usage`.
    Usage,
    /// `model`.
    Model,
    /// `effort <level>`.
    Effort,
    /// Shimmer shown only while in `mode <name>`.
    Mode,
    /// `login` — newly worded to match the tone above.
    Login,
    /// `logout` — same (new).
    Logout,
    /// `resume` — same (new).
    Resume,
    /// `restart` — same (new).
    Restart,
}

impl Status {
    pub fn text(self) -> String {
        match self {
            Status::Gathering => crate::t!("Gathering…", "集計中…"),
            Status::Context => crate::t!("Checking the context…", "コンテキストを確認中…"),
            Status::Usage => crate::t!("Checking usage…", "使用状況を確認中…"),
            Status::Model => crate::t!("Switching the model…", "モデルを切り替え中…"),
            Status::Effort => crate::t!("Setting the effort level…", "effort を設定中…"),
            Status::Mode => crate::t!("Switching the permission mode…", "権限モードを切り替え中…"),
            Status::Login => crate::t!("Signing in…", "サインイン中…"),
            Status::Logout => crate::t!("Signing out…", "サインアウト中…"),
            Status::Resume => crate::t!("Resuming the thread…", "スレッドを再開中…"),
            Status::Restart => crate::t!("Restarting…", "再起動中…"),
        }
    }
}

// ── In-progress status (shimmer) — where the Bridge shows "thinking" ────────────

/// Assistant status kept up while processing (shimmers like `is thinking…`).
///
/// **Drop always sends an empty string** — pairing set with a clear in `finally`.
/// It clears on early return, on error paths and on panic, so forgetting to clear cannot happen structurally
/// (a forgotten status freezes the DM input box = blocks interrupting mid-turn).
///
/// Sends go through one dedicated serial task. If set and clear were separate spawns, they could reach Slack
/// out of order and "a status we cleared stays up" (compact re-sets every 800ms —
/// that is where it gets busiest). Dropping tx lets the task drain the rest and then finish.
///
/// ponytail: serial **only within one guard**. Firing a command while an agent is running in the same thread
/// lets the `Stall` path and the command's guard write the same Slack field in different orders
/// (not coordinated). Both always end with a clear, and the sweep at settle
/// catches the rest, so **it never gets stuck** — at worst "the agent's shimmer disappears
/// during the command". Fix it once a shared per-thread-key registry (Arc/Weak + cleanup)
/// is worth threading through the 10 places that create a guard
pub struct Thinking(tokio::sync::mpsc::UnboundedSender<String>);

impl Thinking {
    pub fn new(api: crate::chat::ChatRef, channel: &str, thread_ts: &str, status: &str) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (channel, thread_ts) = (channel.to_string(), thread_ts.to_string());
        tokio::spawn(async move {
            while let Some(s) = rx.recv().await {
                api.thinking(&channel, &thread_ts, &s).await;
            }
        });
        let t = Self(tx);
        // Created with an empty string, it **sends nothing** (for cases like the `Stall` send path, "set up the channel first,
        // the caller decides what to show"). Nothing has been shown yet, so no wasted clear
        // either. The clear on Drop always goes out regardless of status
        if !status.is_empty() {
            t.set(status);
        }
        t
    }

    /// Re-set (Slack expires the status after a short time — used by the compact tick).
    pub fn set(&self, status: &str) {
        let _ = self.0.send(status.to_string());
    }
}

impl Drop for Thinking {
    fn drop(&mut self) {
        self.set(""); // empty string = clear. Just send it — the serial task sends in order
    }
}

// ─── Reply limits: attachments, the inbox, long bodies ──────────────────────────

/// Per-file limit (the same number as "max 50MB each" in the reply tool schema).
pub const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;

/// How long attachments placed in the inbox are kept (7 days).
/// The sweep piggybacks on downloads — no dedicated timer in the Bridge.
pub const INBOX_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Body limit per post. Replies over this are sent in pieces.
/// Slack rejects bodies that are too long, so without splitting **the whole reply is lost**.
///
/// **11,500, measured against Slack on 2026-09-22.** A `markdown` block takes 11,900 characters and is
/// refused at 12,100 with `msg_too_long`, and the count is **characters, not bytes** — 11,900 Japanese
/// characters go through as a 70 KB request. The old value was 3,900, from the days of `section` blocks
/// (3,000 each); keeping it there cut tables and fenced code blocks that would now fit in one post.
/// The 500 left over is room for whatever Slack counts that we cannot see.
pub const MAX_CHUNK_LIMIT: usize = 11_500;

/// Split a reply into lengths that fit in Slack.
///
/// `newline` looks for a break at paragraph → line → word. But it **never breaks before half the limit** —
/// cutting hard at the limit beats chopping at a break that is too early. `length` cuts hard at the limit.
///
/// It counts **characters** (Rust's `len()` is bytes, which would split Japanese text mid-character).
pub fn chunk(text: &str, limit: usize, newline_mode: bool) -> Vec<String> {
    let limit = limit.max(1);
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return vec![text.to_string()];
    }
    let rfind = |from: usize, pat: &[char]| -> Option<usize> {
        chars[..from.min(chars.len())]
            .windows(pat.len())
            .rposition(|w| w == pat)
    };
    let mut out = Vec::new();
    let mut start = 0usize;
    while chars.len() - start > limit {
        let window_end = start + limit;
        let rel = |abs: Option<usize>| abs.map(|i| i - start);
        let mut cut = limit;
        if newline_mode {
            let para = rel(rfind(window_end, &['\n', '\n']).filter(|&i| i > start));
            let line = rel(rfind(window_end, &['\n']).filter(|&i| i > start));
            let space = rel(rfind(window_end, &[' ']).filter(|&i| i > start));
            cut = match (para, line, space) {
                (Some(p), _, _) if p > limit / 2 => p,
                (_, Some(l), _) if l > limit / 2 => l,
                (_, _, Some(s)) if s > 0 => s,
                _ => limit,
            };
        }
        out.push(chars[start..start + cut].iter().collect());
        start += cut;
        // The newline right after the break is not carried into the next piece
        while chars.get(start) == Some(&'\n') {
            start += 1;
        }
    }
    if start < chars.len() {
        out.push(chars[start..].iter().collect());
    }
    out
}

/// file_id → local path downloaded into the inbox. Shared by the MCP `download_attachment` tool and
/// the prefetch download on receive (keeping the storage conventions and the limit check in one place).
pub async fn download_attachment(
    slack: &dyn Chat,
    file_id: &str,
    state_dir: &std::path::Path,
) -> Result<String, String> {
    let (url, name, size) = slack.file_info(file_id).await?;
    if size > MAX_ATTACHMENT_BYTES {
        // Wording copied from the original
        return Err(format!(
            "file too large: {:.1}MB, max 50MB",
            size as f64 / 1024.0 / 1024.0
        ));
    }
    let dest = state_dir
        .join("inbox")
        .join(Api::attachment_file_name(file_id, &name));
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    slack.download_to(&url, &dest).await?;
    // The sweep piggybacks **after the write succeeds**. So there is no dedicated timer,
    // and so a sweep failure never affects this download (the file just written has mtime = now,
    // so it is not a target)
    Api::sweep_inbox(&state_dir.join("inbox"), crate::clock::now_ms());
    Ok(dest.to_string_lossy().to_string())
}

/// Switch to the delivered look: remove the ack and ⟳, add 🤖.
/// All best-effort — reactions are an observation signal, not the ledger, so
/// no_reaction / already_reacted never counts as a failed delivery.
pub async fn flip_to_received(slack: &dyn Chat, channel: &str, message_ts: &str, ack: &str) {
    let ctx = LogCtx {
        session_id: None,
        thread_key: Some(ThreadKey::new(channel, message_ts)),
    };
    for name in [ack, "arrows_counterclockwise"] {
        if let Err(e) = slack.remove_reaction(channel, message_ts, name).await {
            let m = format!("received-reaction remove '{name}' failed: {e}");
            ctx.debug("bridge", &m);
        }
    }
    if let Err(e) = slack.add_reaction(channel, message_ts, "robot_face").await {
        ctx.debug("bridge", &format!("received-reaction add failed: {e}"));
    }
}

// ─── Slack id shapes and mentions ──────────────────────────────────────────────
// Both users and bots render as `<@…>`. A bot is identified by its `B…` id.

/// A Slack id. The kind is known **from its shape alone** (no lookup).
pub struct SlackId;

impl SlackId {
    fn shaped(id: &str, first: &[char]) -> bool {
        let mut cs = id.chars();
        cs.next().is_some_and(|c| first.contains(&c))
            && id.len() > 1
            && cs.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    }

    pub fn is_user(id: &str) -> bool {
        Self::shaped(id, &['U'])
    }

    pub fn is_bot(id: &str) -> bool {
        Self::shaped(id, &['B'])
    }

    pub fn is_channel(id: &str) -> bool {
        Self::shaped(id, &['C', 'G'])
    }

    /// A DM channel (`D…`) — a room with only the bot and the other party.
    pub fn is_dm(id: &str) -> bool {
        Self::shaped(id, &['D'])
    }

    pub fn from_user_mention(token: &str) -> Option<String> {
        Self::parse_mention(token, '@', Self::is_user)
    }

    pub fn from_channel_mention(token: &str) -> Option<String> {
        Self::parse_mention(token, '#', Self::is_channel)
    }

    /// The id inside a mention token — `<@U…>` / `<@U…|label>` for a user, `<#C…>` / `<#C…|label>` for
    /// a channel — or a bare id written on its own. None if it is not such a reference.
    fn parse_mention(token: &str, sigil: char, shape: fn(&str) -> bool) -> Option<String> {
        let t = token.trim();
        let inner = t
            .strip_prefix('<')
            .and_then(|s| s.strip_prefix(sigil))
            .and_then(|s| s.strip_suffix('>'));
        let id = match inner {
            // A broken token whose label contains `>` is not treated as a mention (equivalent to `[^>]*`)
            Some(i) => match i.split_once('|') {
                Some((id, label)) if !label.contains('>') => id,
                Some(_) => t,
                None => i,
            },
            None => t,
        };
        shape(id).then(|| id.to_string())
    }
}

#[cfg(test)]
mod tests {
    /// The limit is what Slack measured at, **in characters**: 11,900 goes through, 12,100 comes back
    /// `msg_too_long`, and 11,900 Japanese characters (a 70 KB request) go through too. Raising this
    /// constant past what was measured loses whole replies, which is what the splitting exists to prevent.
    #[test]
    fn the_chunk_limit_stays_inside_what_slack_takes() {
        assert!(MAX_CHUNK_LIMIT <= 11_900, "measured ceiling");
        assert!(MAX_CHUNK_LIMIT > 3_900, "the old section-block value");
        // Counted in characters, so a body of multi-byte text is not split early
        let ja: String = "あ".repeat(MAX_CHUNK_LIMIT);
        assert_eq!(chunk(&ja, MAX_CHUNK_LIMIT, false).len(), 1, "one post");
        assert_eq!(chunk(&"あ".repeat(MAX_CHUNK_LIMIT + 1), MAX_CHUNK_LIMIT, false).len(), 2);
    }

    /// **Up to two of our own is normal** (slack-morphism's default; confirmed in the startup log on a real machine).
    /// From the third on, another process is consuming the same app = an accident.
    #[test]
    fn a_third_socket_connection_is_someone_else() {
        for ok in [1, 2] {
            assert!(connection_warning(ok).is_none(), "{ok}");
        }
        for n in [3, 5] {
            let w = connection_warning(n).unwrap_or_default();
            assert!(w.contains(&format!("{n} live connections")), "{w}");
            assert!(w.contains("SPLIT"), "{w}");
        }
    }

    use super::*;

    /// Only attachments past the TTL are deleted. One right at the TTL boundary is kept.
    #[test]
    fn the_inbox_sweep_removes_only_files_past_the_ttl() {
        let dir = std::env::temp_dir().join(format!("scinbox-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str| {
            let p = dir.join(name);
            std::fs::write(&p, b"x").unwrap();
            p
        };
        let (fresh, old, edge) = (write("fresh"), write("old"), write("edge"));
        // mtime is "now", so advance the sweep's now to create age
        let now = crate::clock::now_ms();
        Api::sweep_inbox(&dir, now); // nothing deleted yet
        assert!(fresh.exists() && old.exists());

        Api::sweep_inbox(&dir, now + INBOX_TTL_MS + 1_000);
        assert!(!old.exists(), "TTL を過ぎたものは消す");
        assert!(!fresh.exists(), "同じ時刻に書いたものは同じ扱い");
        assert!(!edge.exists());

        // Does not fail even on an unreadable directory
        Api::sweep_inbox(std::path::Path::new("/nonexistent-inbox-3f9"), now);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The stored name is always one component directly under the inbox — name is external input from Slack.
    #[test]
    fn attachment_name_never_escapes_the_inbox() {
        assert_eq!(Api::attachment_file_name("F1", "shot.png"), "F1-shot.png");
        assert_eq!(
            Api::attachment_file_name("F1", "../../etc/passwd"),
            "F1-passwd"
        );
        assert_eq!(Api::attachment_file_name("F1", "a/b.txt"), "F1-b.txt");
        assert_eq!(
            Api::attachment_file_name("F1", "..\\..\\win.txt"),
            "F1-....win.txt"
        );
        assert_eq!(
            Api::attachment_file_name("F1", ".."),
            "F1",
            "残らなければ id だけ"
        );
        assert_eq!(Api::attachment_file_name("F1", ""), "F1");
        assert_eq!(Api::attachment_file_name("F1", "   "), "F1");
        assert_eq!(
            Api::attachment_file_name("F1", "スクショ 1.png"),
            "F1-スクショ 1.png"
        );
        // file_id is external input too (an MCP tool argument) — treated the same
        assert_eq!(Api::attachment_file_name("../../F1", "a.png"), "F1-a.png");
        assert_eq!(Api::attachment_file_name("/", "a.png"), "attachment-a.png");
        // No input escapes the inbox directory — and none turns into "." / ".."
        let inbox = std::path::Path::new("/s/inbox");
        for (id, name) in [
            ("F1", "../../etc/passwd"),
            ("..", ".."),
            ("", "a/../../b"),
            (" .. ", " .. "),
            ("\"..\"", "\"..\""),
            (".", "."),
        ] {
            let s = Api::attachment_file_name(id, name);
            assert!(!matches!(s.as_str(), "." | ".."), "{id:?}/{name:?} → {s:?}");
            let p = inbox.join(&s);
            assert_eq!(p.parent(), Some(inbox), "{p:?} escaped the inbox");
        }
        // Drop `"`, which breaks the envelope attribute (file_paths="…")
        assert_eq!(Api::attachment_file_name("F1", "a\"b.png"), "F1-ab.png");
    }

    use crate::chat::fake::FakeChat;

    /// An attachment that is too large is refused **before** the download starts (FakeChat's download_to
    /// returns Err with different wording — if the check did not return first, the wording comparison would fail).
    #[tokio::test]
    async fn oversized_attachment_is_refused_before_downloading() {
        let mut api = FakeChat::default();
        api.file_size = MAX_ATTACHMENT_BYTES + 1;
        let err = download_attachment(&api, "F1", std::path::Path::new("/nonexistent"))
            .await
            .unwrap_err();
        assert_eq!(err, "file too large: 50.0MB, max 50MB", "文言は現行の原文");
    }

    #[tokio::test]
    async fn upload_file_round_trip_is_recorded() {
        let api = FakeChat::default();
        let path = std::env::temp_dir().join(format!("sc-upload-{}.txt", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();
        api.upload_file("C1", Some("171.002"), &path).await.unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            api.calls(),
            vec![format!("upload C1 171.002 {}", path.display())]
        );
    }

    #[tokio::test]
    async fn upload_file_missing_path_is_err() {
        let api = FakeChat::default();
        let missing = std::path::Path::new("/nonexistent/definitely-not-here.txt");
        assert!(api.upload_file("C1", None, missing).await.is_err());
    }

    #[tokio::test]
    async fn flip_removes_ack_and_adds_robot() {
        let api = FakeChat::default();
        flip_to_received(&api, "C1", "171.002", "eyes").await;
        assert_eq!(
            api.calls(),
            vec![
                "unreact C1 171.002 eyes",
                "unreact C1 171.002 arrows_counterclockwise",
                "react C1 171.002 robot_face",
            ]
        );
    }

    #[tokio::test]
    async fn flip_swallows_slack_errors() {
        let api = FakeChat::failing();
        flip_to_received(&api, "C1", "171.002", "eyes").await; // fine as long as it runs to completion without panicking
        assert_eq!(api.calls().len(), 3, "失敗しても3手とも試みる");
    }

    #[test]
    fn fetch_formatting_is_oldest_first() {
        let msgs = vec![
            FetchedMsg {
                ts: "2.0".into(),
                user: "U2".into(),
                text: "second".into(),
                thread_ts: None,
            },
            FetchedMsg {
                ts: "1.0".into(),
                user: "U1".into(),
                text: "first".into(),
                thread_ts: None,
            },
        ];
        assert_eq!(
            FetchedMsg::render_all(&msgs),
            "[1.0] U1: first\n[2.0] U2: second"
        );
    }

    /// Long replies are sent in pieces. Two split modes (the default cuts hard at the limit).
    #[test]
    fn a_long_reply_is_split_into_slack_sized_posts() {
        assert_eq!(chunk("short", 10, false), vec!["short"]);
        // Exactly at the limit is not split
        assert_eq!(chunk("0123456789", 10, false), vec!["0123456789"]);
        assert_eq!(chunk("0123456789a", 10, false), vec!["0123456789", "a"]);
        // Japanese is split by **characters** too (splitting by bytes would break it mid-character)
        let ja = "あ".repeat(25);
        let parts = chunk(&ja, 10, false);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].chars().count(), 10);
        assert_eq!(parts.concat(), ja);
        // newline: look for a break before the limit, and do not carry the break's newline over
        let text = format!("{}\n\n{}", "a".repeat(60), "b".repeat(60));
        let parts = chunk(&text, 100, true);
        assert_eq!(parts[0], "a".repeat(60));
        assert_eq!(parts[1], "b".repeat(60));
        // When the break is too early (before half the limit), cut hard at the limit
        let text = format!("{}\n{}", "a".repeat(10), "b".repeat(200));
        let parts = chunk(&text, 100, true);
        assert_eq!(parts[0].chars().count(), 100);
    }

    /// Exercise the three used by status / allow-bot through the trait (the real API's shape is checked in E2E).
    #[tokio::test]
    async fn lookup_apis_go_through_the_trait() {
        let api = FakeChat::default();
        assert_eq!(
            api.get_permalink("C1", "17.5").await.unwrap(),
            "https://slack/C1/17.5"
        );
        assert_eq!(api.channel_display_name("C1").await, Some("#C1".into()));
        assert_eq!(
            api.resolve_bot_id("UB42").await.unwrap(),
            Some("B42".into())
        );
        assert_eq!(api.resolve_bot_id("U9").await.unwrap(), None, "人間は None");
        assert_eq!(
            api.calls(),
            [
                "permalink C1 17.5",
                "channel_name C1",
                "bot_id UB42",
                "bot_id U9"
            ],
        );
        // Name resolution is best-effort — if Slack is down it returns None (not Err)
        assert_eq!(FakeChat::failing().channel_display_name("C1").await, None);
        assert!(FakeChat::failing().get_permalink("C1", "1").await.is_err());
    }

    /// The wording is pinned so what users see does not change.
    /// The source is each constant's doc comment.
    #[test]
    fn thinking_status_wording_is_pinned() {
        // The ellipsis is the single character U+2026. Turning into ASCII "..." would change how it looks
        assert_eq!(TYPING_STATUS, "is typing\u{2026}");
        assert_eq!(THINKING_STATUS, "is thinking\u{2026}");
        assert_eq!(
            SILENCE_MS, 3_000,
            "Bun の 5s から意図的に短縮(定数の doc 参照)"
        );
        let _ja = crate::i18n::pin(crate::i18n::Lang::Ja);
        assert_eq!(Status::Gathering.text(), "集計中\u{2026}");
        assert_eq!(Status::Login.text(), "サインイン中\u{2026}");
        drop(_ja);
        let _en = crate::i18n::pin(crate::i18n::Lang::En);
        assert_eq!(Status::Restart.text(), "Restarting\u{2026}");
    }

    /// An empty string clears (Slack's contract). FakeChat just records it as is.
    #[tokio::test]
    async fn set_thinking_status_records_set_and_clear() {
        let api = FakeChat::default();
        api.set_thinking_status("C1", "1.1", THINKING_STATUS)
            .await
            .expect("set");
        api.set_thinking_status("C1", "1.1", "")
            .await
            .expect("clear");
        assert_eq!(
            api.calls(),
            vec![
                "status C1 1.1 is thinking\u{2026}".to_string(),
                "status C1 1.1 ".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn set_thinking_status_failure_is_an_err_the_caller_can_log() {
        let api = FakeChat::failing();
        assert!(
            api.set_thinking_status("C1", "1.1", TYPING_STATUS)
                .await
                .is_err(),
            "呼び手が best-effort でログできるよう Err で返る"
        );
    }

    /// A real file_share event (image only = no text) gets through with its attachment instead of being dropped.
    #[test]
    fn normalize_keeps_a_text_less_file_share() {
        let ev: SlackMessageEvent = serde_json::from_str(
            r#"{"type":"message","subtype":"file_share","ts":"171.002","channel":"D1",
                "channel_type":"im","user":"U1",
                "files":[{"id":"F1","name":"shot.png"},{"id":"F2"}]}"#,
        )
        .expect("file_share event must deserialize");
        let m = message_of(&ev)
            .expect("a message with files is answerable even without text");
        assert_eq!(m.text, "");
        assert_eq!(
            m.files
                .iter()
                .map(|f| (f.id.as_str(), f.name.as_str()))
                .collect::<Vec<_>>(),
            [("F1", "shot.png"), ("F2", "F2")],
            "name が無ければ id を表示名に使う(劣化ノート用)"
        );
        // With neither attachment nor text, it is dropped as before
        let empty: SlackMessageEvent = serde_json::from_str(
            r#"{"type":"message","ts":"171.003","channel":"D1","channel_type":"im","user":"U1"}"#,
        )
        .unwrap();
        assert!(message_of(&empty).is_none());
    }

    fn ev(json: &str) -> SlackMessageEvent {
        serde_json::from_str(json).expect("event json")
    }

    #[test]
    fn normalizes_channel_message() {
        let m = message_of(&ev(
            r#"{"ts":"1.1","channel":"C1","channel_type":"channel","user":"U1","text":"hi"}"#,
        ))
        .expect("should normalize");
        assert_eq!(m.channel, "C1");
        assert_eq!(m.channel_kind, ChannelKind::Channel);
        assert_eq!(m.ts, "1.1");
        assert_eq!(m.thread_ts, None);
        assert_eq!(m.user.as_deref(), Some("U1"));
        assert!(!m.is_bot);
        assert_eq!(m.text, "hi");
    }

    #[test]
    fn normalizes_dm_with_thread() {
        let m = message_of(&ev(
            r#"{"ts":"2.2","thread_ts":"2.0","channel":"D1","channel_type":"im","user":"U1","text":"yo"}"#,
        ))
        .expect("should normalize");
        assert_eq!(m.channel_kind, ChannelKind::Dm);
        assert_eq!(m.thread_ts.as_deref(), Some("2.0"));
    }

    #[test]
    fn marks_bot_and_system_senders() {
        // with bot_id
        let m = message_of(&ev(
            r#"{"ts":"3.3","channel":"C1","user":"U1","bot_id":"B1","text":"echo"}"#,
        ))
        .expect("should normalize");
        assert!(m.is_bot, "bot_id present must mark is_bot");
        // unknown sender (system)
        let m = message_of(&ev(r#"{"ts":"3.4","channel":"C1","text":"joined"}"#))
            .expect("normalize");
        assert!(m.is_bot, "missing user must mark is_bot");
    }

    #[test]
    fn skips_messages_we_cannot_answer() {
        assert!(
            message_of(&ev(r#"{"ts":"4.4","channel":"C1","user":"U1"}"#)).is_none(),
            "no text"
        );
        assert!(
            message_of(&ev(r#"{"ts":"4.5","user":"U1","text":"x"}"#)).is_none(),
            "no channel"
        );
    }
}
