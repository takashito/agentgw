//! Slack との出入り。slack-morphism の型はこのファイルの外に出さない。

pub mod sticky;
pub use sticky::{StickyAction, StickyBoard, ToolStatus};

use crate::bridge::inbound::InboundMsg;
use crate::bridge::state::{self as bridge_state, LogCtx, ThreadKey};
use slack_morphism::prelude::*;
use std::sync::Arc;

/// 親が引き取る Slack の生イベント。**子へそのまま転送できる形**で持つ
/// (どのマシンの担当かを決めるのは `bridge::gateway` の仕事で、ここは運ぶだけ)。
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

/// 親が子へ転送するイベント。**Bridge が扱いを知っているものだけ** — ここに無い種類は
/// 名前が付かず、そのまま落ちる(この match が購読表そのもの)。
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

/// 押された承認ボタン1つ。`action_id` は `perm:<動作>:<reqId>`。
#[derive(Debug, Clone)]
pub struct PermClick {
    pub req_id: String,
    /// `allow` / `deny` / `allow-thread` / `allow-channel`
    pub action: String,
    /// 押した人(監査とログ用)。
    pub by: String,
}

/// Block Kit のボタン。**押した事実だけ**を main ループへ渡し、判断はしない
/// (Slack のイベント処理は3秒で返さないと Slack が再送する)。
/// Slack は接続のたびに `hello` を投げ、そこに**その app がいま張っている接続の本数**が入る
/// (`num_connections`)。Web API には本数を返す口が無いので、これが唯一の手がかり。
///
/// **健全な状態でも 2 本ある。** slack-morphism は既定で2本張る
/// (`SlackClientSocketModeConfig::DEFAULT_CONNECTIONS_COUNT = 2`。Slack が定期的に投げる
/// 張り直しの間、もう1本が受け続けるための冗長で、同じプロセスの中なので取りこぼさない)。
/// 実機の起動ログでも hello が2回来て `1` → `2` と数えた(2026-08-01 実測)。
///
/// **3本目からが事故。** それは別のプロセスが同じ app トークンで繋いでいるということで、
/// Slack はイベントを複製せず**半分ずつ振り分ける**。この設計は Slack に繋がるのが
/// 常に1台であることに乗っている。**誰が繋いでいるかは分からない** — 分かるのは本数だけだが、
/// 黙って半分消えるよりはるかにましだから出す。
///
/// > `SlackSocketModeHelloEvent` は slack-morphism 2.24.0 から**名前で参照できない**
/// > (`models` が private で、glob 再輸出が同名の module に隠される)。だから型を書かず、
/// > 引数の型が推論されるクロージャで受ける。
/// このプロセスが自分で張る本数。これを超えた分は**他人**。
const OWN_CONNECTIONS: u32 = 2;

/// **純関数** — 自分の本数を超えていたら言うことを返す。
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
    // **届いた事実そのもの**を残す。Slack アプリで Interactivity が無効だとここに1行も
    // 出ない — 「押しても無反応」が設定側かこちら側かを、この1行で切り分ける
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
        // `perm:<動作>:<reqId>` — reqId 自体に `:` は入らないので3分割で足りる
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

/// slack-morphism が**イベントを読めずに落とした**ときの受け皿。既定のハンドラは
/// `tracing` にしか書かないので、こちらのログには何も残らない — 「Slack が送っていない」と
/// 「こちらが読めなかった」が区別できなくなる(2026-07-31 の削除の調査がこれで長引いた)。
///
/// 読めない筋は実在する: メッセージの種類(`subtype`)を slack-morphism が**固定の一覧**で
/// 持っていて、そこに無い種類が来ると `SlackMessageEvent` ごと落ちる。Slack が新しい種類を
/// 足した日に静かに取りこぼすので、せめて1行残す。
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
    // 親(子を持つ Bridge)は、誰の担当かを決める前に畳まない
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

/// Slack のイベント1つを、Bridge が扱う形に畳む。**直結でも Relay 経由でもここを通る** —
/// 2本目を書くと、門番の判断が経路によっていつか食い違う。
fn inbound_of(event: SlackEventCallbackBody) -> Option<InboundMsg> {
    let msg = match event {
        // 削除は「取り消し」。本文が無いので from_event では拾えない別の道
        SlackEventCallbackBody::Message(ev) if ev.deleted_ts.is_some() => {
            match InboundMsg::from_deletion(&ev) {
                Some(msg) => msg,
                // 黙って落とすと「届いていない」と「落とした」が区別できない。削除は
                // 経路が長い(Slack → 台帳の読み替え → 取り消し配達)ので1行だけ残す
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
        // 書き換え。新しい本文は `message` に入っていて、元のイベントの
        // `content` には無い(from_event では拾えない)
        SlackEventCallbackBody::Message(ev)
            if ev.subtype.as_ref() == Some(&SlackMessageEventType::MessageChanged) =>
        {
            match InboundMsg::from_edit(&ev) {
                Some(msg) => msg,
                None => {
                    // 削除と同じ理由で1行残す。ただし **bot 自身の編集は書かない** —
                    // 付箋の描き直しが毎秒これを撃つので、書くとログが埋まって使えなくなる
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
        SlackEventCallbackBody::Message(ev) => match InboundMsg::from_event(&ev) {
            Some(msg) => msg,
            None => {
                LogCtx::default().debug(
                    "slack",
                    &format!("dropped unanswerable message subtype={:?}", ev.subtype),
                );
                return None;
            }
        },
        // リアクションも受ける。stop 絵文字の判定と、ワーカーへの合成テキストは
        // Bridge 側(付箋の ts と bot の id を知っているのはあちら)
        SlackEventCallbackBody::ReactionAdded(ev) => {
            match InboundMsg::from_reaction(&ev.item, &ev.user.to_string(), ev.reaction.0, true) {
                Some(msg) => msg,
                None => return None,
            }
        }
        SlackEventCallbackBody::ReactionRemoved(ev) => {
            match InboundMsg::from_reaction(&ev.item, &ev.user.to_string(), ev.reaction.0, false) {
                Some(msg) => msg,
                None => return None,
            }
        }
        _ => return None, // AppMention 含め、他のイベントは使わない
    };
    Some(msg)
}

/// Relay が転送してきた生の JSON を、直結と**同じ** [`InboundMsg`] にする。
///
/// Relay 側は slack-morphism の型を `to_value` して載せているので、ここは同じ serde 実装で
/// 戻すだけ。戻せなかったら1行残して捨てる — 黙って落とすと「Relay が送っていない」と
/// 「こちらが読めなかった」が区別できなくなる。
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

/// Relay が転送してきたボタン押しを [`PermClick`] にする。判断はしない — 押された事実だけ。
pub fn perm_click_from_relay(
    action: &serde_json::Value,
    body: &serde_json::Value,
) -> Option<PermClick> {
    let id = action.get("action_id")?.as_str()?;
    // `perm:<動作>:<reqId>` — reqId 自体に `:` は入らないので3分割で足りる
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

/// 既にある1件の投稿について、**イベントからは分からないこと**だけ。
/// [`Api::message_at`] が返す。
pub struct MessageAt {
    /// 書き手(bot が投げたものには入らないことがある)。
    pub user: Option<String>,
    /// bot が投げたもの。
    pub is_bot: bool,
    /// 属するスレッドの根。返信でなければ自分自身の ts。
    pub thread_ts: String,
}

/// Slack Web API。呼び出し側に slack-morphism の型を見せない。
pub struct Api {
    client: Arc<SlackHyperClient>,
    token: SlackApiToken,
    bot_token: String,
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

impl Api {
    pub fn new(bot_token: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self {
            client: Arc::new(SlackClient::new(SlackClientHyperConnector::new()?)),
            token: SlackApiToken::new(bot_token.to_string().into()),
            bot_token: bot_token.to_string(),
        })
    }

    /// 投稿した ts を返す。
    pub async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String> {
        self.post(channel, text, thread_ts, true).await
    }

    /// Bridge が自分で答えるコマンドの投稿口。プレビュー展開を止める —
    /// `status` の本文はスレッド permalink だらけで、展開されると1本ずつ大きなカードになって
    /// 読めなくなる(現行はコマンド応答を全部この形で出す)。
    pub async fn post_message_no_unfurl(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String> {
        self.post(channel, text, thread_ts, false).await
    }

    /// 標準 Markdown で投稿する(Slack の `markdown` ブロック)。
    ///
    /// mrkdwn(`*bold*`・見出し無し・**表無し**)と違い、`##` 見出し・表・チェックボックス・
    /// 言語つき code block がそのまま出る。Slack が 2025-02 に足し、2026-03 に表まで広げた口。
    /// **`text` は残す** — 通知(プッシュ・一覧のプレビュー)が読むのはそちらで、
    /// blocks だけの投稿は通知が空になる。
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
        // 抑止するときだけ触る — 既定のままの経路(ワーカーの reply)の見え方を変えない
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

    /// Owner との DM チャンネルを開いて id を返す(`conversations.open`)。
    /// home チャンネルが無いときの通知先 — 既に開いていれば同じ id が返るので、
    /// 呼ぶたびに新しい会話ができることはない。
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

    /// ツール許可を人に訊く Block Kit プロンプト。返すのは投稿の ts で、
    /// 期限切れのときにこれを消す(消さないと後から押された Allow が、とうに動き出した
    /// ワーカーに届いて「押しても何も起きない」になる)。
    ///
    /// `channel_grant_label` は DM だと「Allow for User」— DM は1人なので同じ
    /// allow-channel が「この人には以後訊かない」になる。
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

    /// 標準 Markdown で書き換える(`markdown` ブロック)。
    ///
    /// **付箋(進捗)や許可プロンプトはこちらを通さない** — あれは手書きの mrkdwn
    /// (`*太字*`)で、Markdown として読ませると太字が斜体に化ける。使うのはワーカーの
    /// `edit_message`(= 答えの差し替え)だけ。
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

    /// チャンネル履歴(oldest-first に整えるのは呼び出し側)。
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

    /// この ts のメッセージが属するスレッドの根。
    ///
    /// 編集・削除のイベントは**スレッドを教えてくれない**(使っているライブラリの型が
    /// 入れ子の `thread_ts` を落とす)ので、Slack に1件だけ問い合わせて引く。
    /// 返信でなければスレッドの根はそのメッセージ自身 — その場合は `ts` をそのまま返す。
    pub async fn parent_thread_of(&self, channel: &str, ts: &str) -> Option<String> {
        self.message_at(channel, ts).await.map(|m| m.thread_ts)
    }

    /// この ts のメッセージの**書き手とスレッド**。
    ///
    /// リアクションの通知には**どちらも入っていない**(`item` は種別・チャンネル・ts だけ)。
    /// 現行 も同じ理由でここを1回引いている。省くと
    /// 「誰の投稿へのリアクションか」が分からず、スレッドも ts で代用するしかない。
    ///
    /// **`conversations.history` では引けない。** history が返すのはチャンネル直下の投稿だけで、
    /// **スレッドの中の返信は1件も入っていない** — 返信の ts で引くと「それ以前の別の投稿」が
    /// 来て、ts の照合で弾かれる(2026-08-02 実測: スレッド内の投稿への ✗ が
    /// `could not read` で落ちた)。`conversations.replies` は返信の ts をそのまま解釈し、
    /// **その投稿自身**を書き手と `thread_ts` 付きで返す(実測で確認)。
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
        // 取れたのが本当にその ts か確かめる(スレッドの根が返ることがある)
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
        // inclusive: 指定した ts のメッセージ自身を含める(現行)
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

    /// スレッドの根がもう無いか。消えた根に `thread_ts` 付きで投げると、Slack は
    /// それを**チャンネル直下の発言**として落とすのでチャンネルが荒れる(現行
    /// の `probeThreadRoot`)。
    ///
    /// **消えたと言い切るのは「無い」と分かったときだけ**。ネットワークやその他の API
    /// エラーは削除の証拠ではないので `false`(= 投げる)を返す — 本当に通信が壊れていれば
    /// 投稿自体も失敗するので荒れようがなく、一時的な瞬断で正当な投稿を握り潰す方が悪い。
    ///
    /// **DM では呼ばない**(根がそうやって消えない。呼び手が `D` 始まりを弾く)。
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

    /// url_private を Bearer 付きで取ってファイルに落とす。
    /// ponytail: curl 呼び出し。slack-morphism の HTTP ヘルパーは全て JSON デシリアライズ前提で
    /// 生バイトを返す口が無く、依存追加は禁止のため。添付を本格化するとき見直す。
    pub async fn download_to(&self, url: &str, dest: &std::path::Path) -> Result<(), String> {
        let auth = format!("Authorization: Bearer {}", self.bot_token);
        // `--max-time` はプロセス側の実上限。呼び手が future を drop しても curl は生き残り、
        // 「timed out」と言った後も inbox に書き続けてしまうため、子プロセスにも効かせる
        let max_time = DOWNLOAD_TIMEOUT.as_secs().to_string();
        let out = tokio::process::Command::new("/usr/bin/curl")
            .args(["-sSfL", "--max-time", &max_time, "-H", &auth, "-o"])
            .arg(dest)
            .arg(url)
            .output()
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

    /// ローカルファイルを1つ Slack に上げる(getUploadURLExternal → 生バイト PUT → completeUploadExternal)。
    /// content-type は Slack の自動判定に委ねる(現行 Bun 版も filename しか渡さない)。
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

    /// メッセージへの permalink(`status` の各スレッド行に付ける)。
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

    /// スレッドに Slack ネイティブの assistant ステータス(静かな shimmer)を出す。
    /// **空文字を送るとクリア**。投稿と違って通知を鳴らさず、後に読むものを残さない
    /// DM 専用の API なのでチャンネルでは無害な no-op。
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

    /// 生 JSON で読む GET。slack-morphism の型付きモデルが**落としてしまう**フィールド
    /// (DM の `channel.user`、`user.profile.bot_id` — どちらも 2.24.0 のモデルに無い)を
    /// 読むための口。`ok:false` は connector が Err にしてからここへ来る。
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

    /// 人が読めるチャンネル名。DM(`is_im`)は相手の `@name`、チャンネルは `#name`。
    /// best-effort — 失敗は None。
    pub async fn channel_display_name(&self, channel: &str) -> Option<String> {
        // 空文字は「無い」扱いで次の候補に落とす(現行 TS の `||` と同じ)
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

    /// 人が読める `@名前`。生の `U…` は誰のことだか分からないので、`status` の owner 欄と
    /// DM のチャンネル名がこれを通る。best-effort — 失敗は None。
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

    /// この bot 自身の user id(`U…`)と表示名。id は本文コマンドから自 mention を剥がすのに要り、
    /// 名前は home への起動通知に出す(生の `U…` は人が見て誰だか分からない。以前の実装は
    /// `authResult.user` を出している)。`SlackApi` trait には載せない — 起動時に1回呼ぶだけ。
    pub async fn auth_test(&self) -> Result<(String, Option<String>), String> {
        self.client
            .open_session(&self.token)
            .auth_test()
            .await
            .map(|r| (r.user_id.to_string(), r.user))
            .map_err(|e| e.to_string())
    }

    /// メンション(`<@U…>`)が bot なら台帳の鍵になる `bot_id`(B…)を返す。人間は Ok(None)。
    /// bot なのに `profile.bot_id` が無いのは Err — 保存できない id を黙って捨てないため
    /// (resolveSenderForException と同じ判断)。
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

    /// 添付の (url_private, ファイル名, バイト数)。size は上限判定に使う —
    /// slack-morphism の `SlackFile` は size を持たないので生 JSON で取る
    /// (`resolve_bot_id` と同じ作法)。
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

    // ── Bridge が Slack を1本叩くときの共通の作法 ──

    /// Socket Mode を張り、message イベントを正規化して流す。
    /// AppMention は捨てる — 同じ発言が Message としても届く(dedup と二段構え)。
    /// `fleet` が `Some` のとき(= 子を持つ親)は、**畳まずに生のまま**そちらへ渡す。
    /// 誰の担当かを決めてから、自分の分だけ `tx` / `clicks` に戻ってくる — つまり
    /// ローカル配達も直結と同じ変換([`inbound_of`])を通る。
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

    /// inbox に置く保存名。`name` は **Slack から来る外部入力**で、`a/b.txt` や `../../etc/passwd`
    /// のようにパス区切りを含みうる — そのまま join すると inbox の外に書けてしまう
    /// (現行が `{ts}-{fileId}{ext}` にして生の name をパスに使わないのはこれを避けるため)。
    /// 最終成分だけを取り、残った区切り文字を落として**必ず1要素**にする。使えるものが
    /// 残らなければ file_id だけ。
    pub fn attachment_file_name(file_id: &str, name: &str) -> String {
        // `Path::file_name()` は "a/b.txt" → "b.txt"、".." や "" → None。
        // Unix では `\` が区切りでないので、その分は自前で落とす
        let one = |s: &str| -> String {
            // trim は `Path::new` に渡す**前**(" .. " を ".." として残さないため)
            let base = std::path::Path::new(s.trim())
                .file_name()
                .map(|b| b.to_string_lossy().into_owned())
                .unwrap_or_default();
            // `"` は封筒の属性(`file_paths="…"`)を壊すので落とす
            let base = base.replace(['/', '\\', '"', '\0'], "");
            match base.trim() {
                // 除去の結果 "." / ".." に化けることがある(ダブルクォートで包んだ `".."` など)
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

    /// 受信 ack の絵文字(未設定は "eyes")。
    pub fn ack_emoji(access: &bridge_state::Access) -> &str {
        access.ack_reaction.as_deref().unwrap_or("eyes")
    }

    /// assistant ステータスを1本投げてログに残す。**best-effort, but never silent** —
    /// 成功も失敗も残す。呼び手を待たせないのは呼び手側の責任。
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

    /// Bridge 直答の実投稿。既に spawn 済みの文脈から呼ぶ(probe の答えは60秒後に届く)。
    /// 失敗はログだけ — コマンドの答えは未応答台帳の外の出来事。
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

    /// 再起動の道中の Slack 1本を5秒で見切る。ここで投げるものはどれも「出れば嬉しい」飾りで、
    /// 1本の hang が marker 書きと exit(0) を止めてはならない(現行が allSettled で束ねているのと
    /// 同じ趣旨)。失敗も時間切れもログして続ける。
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

/// ダウンロードに許す時間。現行の `DOWNLOAD_TIMEOUT_MS` と同じ 60 秒で、
/// curl の `--max-time` と、受信時の先読み全体の締切の両方に使う。
pub const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

// ─── ワーカーの MCP ツール実行 ───────────────────────────────────────────────

/// execute_tool が使う Slack 操作。実体は `Api`、テストは fake。
pub trait SlackApi: Send + Sync {
    fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> impl Future<Output = Result<String, String>> + Send;
    /// 標準 Markdown で投稿する(`markdown` ブロック)。ワーカーの返信の既定。
    fn post_markdown(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> impl Future<Output = Result<String, String>> + Send;
    /// 標準 Markdown で書き換える。付箋は通さない(手書き mrkdwn なので)。
    fn update_markdown(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;
    fn add_reaction(
        &self,
        channel: &str,
        ts: &str,
        emoji: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;
    fn remove_reaction(
        &self,
        channel: &str,
        ts: &str,
        emoji: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;
    fn delete_message(
        &self,
        channel: &str,
        ts: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;
    fn update_message(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;
    fn history(
        &self,
        channel: &str,
        limit: u16,
    ) -> impl Future<Output = Result<Vec<FetchedMsg>, String>> + Send;
    fn replies(
        &self,
        channel: &str,
        thread_ts: &str,
        limit: u16,
    ) -> impl Future<Output = Result<Vec<FetchedMsg>, String>> + Send;
    fn file_info(
        &self,
        file_id: &str,
    ) -> impl Future<Output = Result<(String, String, u64), String>> + Send;
    fn get_permalink(
        &self,
        channel: &str,
        ts: &str,
    ) -> impl Future<Output = Result<String, String>> + Send;
    /// best-effort — 解決できなければ None。
    fn channel_display_name(&self, channel: &str) -> impl Future<Output = Option<String>> + Send;
    /// bot なら Some(B…)、人間なら None。
    fn resolve_bot_id(
        &self,
        user_id: &str,
    ) -> impl Future<Output = Result<Option<String>, String>> + Send;
    fn download_to(
        &self,
        url: &str,
        dest: &std::path::Path,
    ) -> impl Future<Output = Result<(), String>> + Send;
    fn upload_file(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        path: &std::path::Path,
    ) -> impl Future<Output = Result<(), String>> + Send;
    /// assistant ステータスを張る / 空文字で消す。best-effort — 呼び手は失敗で止まらない。
    fn set_thinking_status(
        &self,
        channel: &str,
        thread_ts: &str,
        status: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;
}

/// 1ファイルあたりの上限(reply ツールスキーマの "max 50MB each" と同じ数値 — `endpoints.rs:130`)。
pub const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;

/// inbox に置いた添付をどれだけ残すか(7日)。
/// 掃除はダウンロードの後ろに相乗りする — Bridge に専用のタイマーを増やさない。
pub const INBOX_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// 1投稿あたりの本文の上限。これを超える返信は分けて投げる
/// Slack は長すぎる本文を弾くので、分けないと**返信ごと落ちる**。
pub const MAX_CHUNK_LIMIT: usize = 3900;

/// 返信を Slack に収まる長さに割る。
///
/// `newline` は段落 → 行 → 単語の順に切れ目を探す。ただし**上限の半分より手前では切らない** —
/// 早すぎる切れ目でぶつ切りにするより、上限で断ち切る方がまし。`length` は上限で断ち切る。
///
/// 数えるのは**文字**(Rust の `len()` はバイトなので、日本語だと途中で割れる)。
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
        // 切れ目の直後の改行は次の断片の頭に持ち越さない
        while chars.get(start) == Some(&'\n') {
            start += 1;
        }
    }
    if start < chars.len() {
        out.push(chars[start..].iter().collect());
    }
    out
}

// ── assistant ステータスの文言。現行 Bun の原文をそのまま持ってくる(`tests` で固定)。
// 空文字はどれでもクリアなので、ここに「クリア用の定数」は置かない。

/// 配達直後 / スレッド復帰。
pub const TYPING_STATUS: &str = "is typing…";
/// ターンが無音のまま [`SILENCE_MS`] 過ぎたとき(`THINKING_STATUS`)。
pub const THINKING_STATUS: &str = "is thinking…";
/// 無音と見なすまでの間。現行 Bun の `deps.silenceMs` 既定値は 5s だが、
/// **Rust 版は 3s**(2026-07-31 ユーザー判断 — ツール実行後に思考中が戻るまでが遅い)。
pub const SILENCE_MS: u64 = 3_000;
/// Slack's "…ing" status line. Use as `thinking.set(&Status::Login.text())`.
///
/// `compact` の専用ステータスは**意図的に持たない**(Bun からの逸脱 — ユーザー判断)。
/// 以前の実装は compact の間 `コンテキストを圧縮中…` を張り、tick ごとに `… {n}s` 付きへ
/// 張り直していた。Rust 版はこれを持たない — compact は進捗チェックリスト(sticky)を
/// 投稿して編集し続けるので、shimmer と二重で冗長だという判断(**移植漏れではない**)。
/// 配達の `is typing…` と無音の `is thinking…` は compact 実行中も従来どおり出る。
#[derive(Clone, Copy, Debug)]
pub enum Status {
    /// `status`。
    Gathering,
    /// `context`。
    Context,
    /// `usage`。
    Usage,
    /// `model`。
    Model,
    /// `effort <level>`。
    Effort,
    /// `mode <名前>` の間だけ出す shimmer。
    Mode,
    /// `login` — 現行 Bun に原文が無い。上の語調に合わせて新規に決めたもの。
    Login,
    /// `logout` — 同上(新規)。
    Logout,
    /// `resume` — 同上(新規)。
    Resume,
    /// `restart` — 同上(新規)。
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

/// disposition を main の台帳へ流す。満杯なら待つ(落とすと台帳が消えないまま残る)。
async fn notify(
    dispo: &tokio::sync::mpsc::Sender<bridge_state::Disposition>,
    d: bridge_state::Disposition,
    ctx: &LogCtx,
) {
    if let Err(e) = dispo.send(d).await {
        ctx.error("bridge", &format!("disposition channel closed: {e}"));
    }
}

/// MCP 受け口に差す実体。
pub struct ToolExec {
    pub api: Arc<Api>,
    pub state_dir: std::path::PathBuf,
    /// disposition の通知先(受けて台帳を消すのは main)。
    pub dispo: tokio::sync::mpsc::Sender<bridge_state::Disposition>,
}

impl crate::mcp::ToolExecutor for ToolExec {
    fn execute(
        &self,
        session_id: String,
        tool: String,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
        let (api, dir, dispo) = (self.api.clone(), self.state_dir.clone(), self.dispo.clone());
        Box::pin(async move {
            api.execute_tool(&dir, &session_id, &tool, &args, &dispo)
                .await
        })
    }
}

impl SlackApi for Api {
    async fn post_markdown(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String> {
        Api::post_markdown(self, channel, text, thread_ts).await
    }

    async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String> {
        Api::post_message(self, channel, text, thread_ts).await
    }
    async fn add_reaction(&self, channel: &str, ts: &str, emoji: &str) -> Result<(), String> {
        Api::add_reaction(self, channel, ts, emoji).await
    }
    async fn remove_reaction(&self, channel: &str, ts: &str, emoji: &str) -> Result<(), String> {
        Api::remove_reaction(self, channel, ts, emoji).await
    }
    async fn delete_message(&self, channel: &str, ts: &str) -> Result<(), String> {
        Api::delete_message(self, channel, ts).await
    }
    async fn update_message(&self, channel: &str, ts: &str, text: &str) -> Result<(), String> {
        Api::update_message(self, channel, ts, text).await
    }
    async fn update_markdown(&self, channel: &str, ts: &str, text: &str) -> Result<(), String> {
        Api::update_markdown(self, channel, ts, text).await
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
    async fn file_info(&self, file_id: &str) -> Result<(String, String, u64), String> {
        Api::file_info(self, file_id).await
    }
    async fn get_permalink(&self, channel: &str, ts: &str) -> Result<String, String> {
        Api::get_permalink(self, channel, ts).await
    }
    async fn channel_display_name(&self, channel: &str) -> Option<String> {
        Api::channel_display_name(self, channel).await
    }
    async fn resolve_bot_id(&self, user_id: &str) -> Result<Option<String>, String> {
        Api::resolve_bot_id(self, user_id).await
    }
    async fn download_to(&self, url: &str, dest: &std::path::Path) -> Result<(), String> {
        Api::download_to(self, url, dest).await
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

// ── 処理中ステータス(shimmer)— Bridge が「考え中」を張る口 ─────────────────

/// 処理中のあいだ張っておく assistant ステータス(`is thinking…` などの shimmer)。
///
/// **Drop で必ず空文字を送る** — 現行 Bun が set → finally で `''` と対にしているものの
/// Rust 版。早期 return でもエラー経路でも panic でも消えるので、消し忘れが構造的に起きない
/// (消し忘れは DM の入力欄を固まらせる = ターン中の割り込みを塞ぐ)。
///
/// 送信は専属の直列タスク1本に流す。set と clear が別々の spawn だと Slack への到着順が
/// 逆転して「消したはずのステータスが残る」ことがあるため(compact は 800ms ごとに張り直す —
/// ここが一番詰まる)。tx を落とすとタスクは残りを吐いてから終わる。
///
/// ponytail: 直列なのは **guard 1本の中だけ**。同じスレッドでワーカーが走っている最中に
/// コマンドを撃つと、`Stall` の口とコマンドの guard が同じ Slack フィールドを別々の順で
/// 書きうる(現行 Bun も同様に無調整)。どちらも最後は必ずクリアで終わり、決着時の sweep が
/// 受け皿になるので**張り付きにはならない** — 見えるのは「ワーカーの shimmer が
/// コマンドの間だけ消える」程度。スレッド鍵ごとの共有レジストリ(Arc/Weak + 後始末)を
/// 10 箇所の guard 生成に通す価値が出たらそこで直す
pub struct Thinking(tokio::sync::mpsc::UnboundedSender<String>);

impl Thinking {
    pub fn new(api: &Arc<Api>, channel: &str, thread_ts: &str, status: &str) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (api, channel, thread_ts) = (api.clone(), channel.to_string(), thread_ts.to_string());
        tokio::spawn(async move {
            while let Some(s) = rx.recv().await {
                api.thinking(&channel, &thread_ts, &s).await;
            }
        });
        let t = Self(tx);
        // 空文字で作ると**何も送らない**(`Stall` の送信口のように「口だけ先に用意して、
        // 何を出すかは呼び手が決める」場合)。まだ何も出していないのだからクリアの無駄打ちも
        // しない。Drop のクリアは status に関わらず必ず流れる
        if !status.is_empty() {
            t.set(status);
        }
        t
    }

    /// 張り直し(Slack はステータスを短時間で失効させる — compact の tick で使う)。
    pub fn set(&self, status: &str) {
        let _ = self.0.send(status.to_string());
    }
}

impl Drop for Thinking {
    fn drop(&mut self) {
        self.set(""); // 空文字 = クリア。送るだけ — 直列タスクが順番どおりに投げる
    }
}

/// `SlackApi` を実装したものが**そのまま**持つ振る舞い。fake api でも同じ道を通る。
///
/// `async fn` を trait に置くと呼び手が `Send` を要求できない、という lint が出るが、
/// ここの実装は具体型(`Api` / テストの fake)しか無く、`tokio::spawn` へ渡す経路
/// (`ToolExec::execute` の Box<dyn Future + Send>)は実際に Send を満たしてコンパイルが通る。
/// dyn で使い回す予定も無いので、`Pin<Box<dyn Future>>` の手書きは足さない。
#[allow(async_fn_in_trait)]
pub trait SlackOps: SlackApi + Sync {
    /// 添付を**投稿前に**まとめて検証する。1つでも読めない・
    /// 大きすぎるものがあれば、テキストも含めて何も投稿しないための門番。
    fn check_attachment_sizes(paths: &[String], max_bytes: u64) -> Result<(), String> {
        for p in paths {
            let meta = std::fs::metadata(p).map_err(|e| format!("file not found: {p} ({e})"))?;
            if meta.len() > max_bytes {
                return Err(format!(
                    "file too large: {p} ({:.1}MB, max {}MB)",
                    meta.len() as f64 / 1024.0 / 1024.0,
                    max_bytes / 1024 / 1024
                ));
            }
        }
        Ok(())
    }
    /// 根が消えたスレッドか(`probeThreadRoot`)。判断できないときは「生きている」に倒す —
    /// 分からないことを理由に返信を握りつぶさない。
    async fn root_gone(&self, channel: &str, thread_ts: &str) -> bool {
        match self.replies(channel, thread_ts, 1).await {
            Ok(msgs) => msgs.is_empty(),
            Err(e) => e.contains("thread_not_found") || e.contains("message_not_found"),
        }
    }

    /// 長い本文を分けて投げる。返すのは**最初の**投稿の ts(以後の断片は続きとして並ぶ)。
    /// 上限と切り方は access.json で変えられる(`textChunkLimit` / `chunkMode`)。
    async fn post_chunked(
        &self,
        state_dir: &std::path::Path,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
        markdown: bool,
    ) -> Result<String, String> {
        let a = bridge_state::Access::load(&bridge_state::StateDir::at(state_dir));
        let limit = a
            .text_chunk_limit
            .unwrap_or(MAX_CHUNK_LIMIT)
            .clamp(1, MAX_CHUNK_LIMIT);
        let mut first: Option<String> = None;
        // 分割は Markdown でも同じ。長い表が途中で切れると2通目の頭が表に見えなくなるが、
        // 分けずに投げると Slack が本文ごと弾く — 弾かれるよりは切れる方がまし
        for part in chunk(text, limit, a.chunk_mode.as_deref() == Some("newline")) {
            let ts = if markdown {
                self.post_markdown(channel, &part, thread_ts).await?
            } else {
                self.post_message(channel, &part, thread_ts).await?
            };
            first.get_or_insert(ts);
        }
        first.ok_or_else(|| "nothing to post".to_string())
    }

    /// file_id → inbox に落としたローカルパス。MCP の `download_attachment` ツールと、
    /// 受信時の先読みダウンロードが共有する(保存先の作法と上限判定を1箇所に置くため)。
    async fn download_attachment(
        &self,
        file_id: &str,
        state_dir: &std::path::Path,
    ) -> Result<String, String> {
        let (url, name, size) = self.file_info(file_id).await?;
        if size > MAX_ATTACHMENT_BYTES {
            // 文言は原文コピー
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
        self.download_to(&url, &dest).await?;
        // 掃除は**書けた後**に相乗りさせる。専用のタイマーを増やさないためと、
        // 掃除で落ちてもこのダウンロードには影響させないため(いま書いた物は mtime が今なので
        // 対象にならない)
        Api::sweep_inbox(&state_dir.join("inbox"), crate::bridge::Host::now_ms());
        Ok(dest.to_string_lossy().to_string())
    }

    /// TTL を過ぎた inbox のファイルを消す。
    /// **全部 best-effort** — 読めない・消せない1件で掃除ごと止めない。
    /// 境界(ちょうど TTL)は残す(現行と同じ厳密な `>`)。
    fn sweep_inbox(inbox: &std::path::Path, now_ms: u64) {
        let Ok(entries) = std::fs::read_dir(inbox) else {
            return; // まだ作られていない / 読めない — 掃除するものが無い
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

    /// 配達済みの見た目に直す: ack と ⟳ を外して 🤖 を付ける。
    /// 全て best-effort — リアクションは台帳でなく観測シグナルなので、
    /// no_reaction / already_reacted で配達を失敗扱いにはしない。
    async fn flip_to_received(&self, channel: &str, message_ts: &str, ack: &str) {
        let ctx = LogCtx {
            session_id: None,
            thread_key: Some(ThreadKey::new(channel, message_ts)),
        };
        for name in [ack, "arrows_counterclockwise"] {
            if let Err(e) = self.remove_reaction(channel, message_ts, name).await {
                let m = format!("received-reaction remove '{name}' failed: {e}");
                ctx.debug("bridge", &m);
            }
        }
        if let Err(e) = self.add_reaction(channel, message_ts, "robot_face").await {
            ctx.debug("bridge", &format!("received-reaction add failed: {e}"));
        }
    }

    /// ワーカーが呼んだツールを実行する。失敗は文字列で返し、必ずログにも出す。
    ///
    /// disposition(reply / react / no_reply / message_ids 付きの edit)は**ログに出して
    /// `dispo` に流すだけ** — 台帳を消すのは Threads を持つ main 側(現行の
    /// 「slack-action は通知、tracker は bridge」 同じ分担)。
    async fn execute_tool(
        &self,
        state_dir: &std::path::Path,
        session_id: &str,
        tool: &str,
        args: &serde_json::Value,
        dispo: &tokio::sync::mpsc::Sender<bridge_state::Disposition>,
    ) -> Result<String, String> {
        let ctx = LogCtx {
            session_id: Some(session_id.to_string()),
            thread_key: None,
        };
        let s = |k: &str| args[k].as_str().unwrap_or_default().to_string();
        let opt = |k: &str| args[k].as_str().map(str::to_string);
        // 覆う受信 id。空は異常ではない(reply は UNSPECIFIED として通す)
        let ids: Vec<String> = args["message_ids"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        // disposition ログの宛先。根が引けないものは session ログに落ちる(現行と同じ振り分け)
        let dctx = |root: Option<&str>| LogCtx {
            session_id: Some(session_id.to_string()),
            thread_key: root.map(|t| ThreadKey::new(&s("channel_id"), t)),
        };
        let out = match tool {
            "reply" => {
                let files: Vec<String> = args["files"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let thread = opt("thread_ts");
                // 根が消えたスレッドには投稿しない。**黙って捨てず**
                // 台帳は覆う — 落ちたのではなく「出さないと決めた」記録を残す
                if let Some(t) = thread.as_deref()
                    && !s("channel_id").starts_with('D')
                    && self.root_gone(&s("channel_id"), t).await
                {
                    let c = dctx(Some(t));
                    c.info(
                        "bridge",
                        &format!(
                            "disposition=reply-suppressed thread={t} covers=[{}] \
                             (thread root deleted)",
                            ids.join(",")
                        ),
                    );
                    notify(
                        dispo,
                        bridge_state::Disposition {
                            kind: "reply",
                            channel_id: s("channel_id"),
                            thread_ts: thread.clone(),
                            message_ids: ids,
                            session_id: session_id.to_string(),
                        },
                        &c,
                    )
                    .await;
                    return Ok("reply suppressed: the thread root was deleted".to_string());
                }
                // 添付の検証はテキスト投稿より**前**— 1つでも
                // 弾かれたらテキストも投稿しない
                let posted = match Self::check_attachment_sizes(&files, MAX_ATTACHMENT_BYTES) {
                    Err(e) => Err(e),
                    // 長い本文は分けて投げる。分けないと Slack が本文ごと弾く
                    Ok(()) => {
                        self.post_chunked(
                            state_dir,
                            &s("channel_id"),
                            &s("text"),
                            thread.as_deref(),
                            // **既定が Markdown**。mrkdwn に戻すのは `markdown: false` のときだけ
                            args["markdown"].as_bool().unwrap_or(true),
                        )
                        .await
                    }
                };
                async {
                    let ts = posted?;
                    // 1回に1ファイル・順番に(現行 Bun 版もループ)。
                    // ここで落ちたときテキストだけ残るのは既知 — 現行にもフォールバックは無い
                    for f in &files {
                        self.upload_file(
                            &s("channel_id"),
                            thread.as_deref(),
                            std::path::Path::new(f),
                        )
                        .await
                        .map_err(|e| format!("text posted but attachment upload failed: {e}"))?;
                    }
                    // 投稿できたときだけ覆う — Slack が受けていない返信で台帳を消さない
                    let c = dctx(thread.as_deref());
                    c.info(
                        "bridge",
                        &format!(
                            "disposition=reply thread={} covers=[{}]{}",
                            thread.as_deref().unwrap_or("-"),
                            ids.join(","),
                            if ids.is_empty() { " (UNSPECIFIED)" } else { "" }
                        ),
                    );
                    let d = bridge_state::Disposition {
                        kind: "reply",
                        channel_id: s("channel_id"),
                        thread_ts: thread.clone(),
                        message_ids: ids,
                        session_id: session_id.to_string(),
                    };
                    notify(dispo, d, &c).await;
                    Ok(format!("posted ts={ts}"))
                }
                .await
            }
            "react" => {
                let (channel, message_ts, emoji) = (s("channel_id"), s("message_ts"), s("emoji"));
                match self.add_reaction(&channel, &message_ts, &emoji).await {
                    Err(e) => Err(e),
                    Ok(()) => {
                        // react の args は thread を運ばない。台帳の鍵は**配達時の根**なので
                        // replies で引く。返信の ts で引くと先頭は
                        // 親とは限らないので、**その ts でなく thread_ts** が根。
                        // スレッド外(thread_ts 無し)なら message_ts 自身が根
                        let root = match opt("thread_ts") {
                            Some(t) => Some(t),
                            None => match self.replies(&channel, &message_ts, 1).await {
                                Ok(m) => Some(
                                    m.first()
                                        .and_then(|f| f.thread_ts.clone())
                                        .unwrap_or_else(|| message_ts.clone()),
                                ),
                                // 引けなかったときだけ根が不明 — 撃たない
                                Err(e) => {
                                    let m = format!("react: thread-root lookup failed: {e}");
                                    ctx.debug("bridge", &m);
                                    None
                                }
                            },
                        };
                        let c = dctx(root.as_deref().or(Some(&message_ts)));
                        let m = format!("disposition=react message_ts={message_ts} emoji={emoji}");
                        c.info("bridge", &m);
                        match root {
                            Some(t) => {
                                let d = bridge_state::Disposition {
                                    kind: "react",
                                    channel_id: channel,
                                    thread_ts: Some(t),
                                    message_ids: vec![message_ts],
                                    session_id: session_id.to_string(),
                                };
                                notify(dispo, d, &c).await;
                            }
                            // 根が違えば別スレッドの台帳を誤射する — 撃たずに残す(best-effort)
                            None => c.debug(
                                "bridge",
                                &format!(
                                    "react: thread root unknown for message_ts={message_ts} \
                                     — skipping disposition disarm (best-effort)"
                                ),
                            ),
                        }
                        Ok("reacted".to_string())
                    }
                }
            }
            "edit_message" => {
                // reply と同じ既定にする。答えを「投稿」しても「編集で差し替え」ても
                // 同じ見え方でなければ、書く側は使い分けを覚えなければならなくなる
                let md = args["markdown"].as_bool().unwrap_or(true);
                let edited = if md {
                    self.update_markdown(&s("channel_id"), &s("message_ts"), &s("text"))
                        .await
                } else {
                    self.update_message(&s("channel_id"), &s("message_ts"), &s("text"))
                        .await
                };
                match edited {
                    Err(e) => Err(e),
                    Ok(()) => {
                        // message_ids 付きの編集だけが「答え」。無ければ進捗編集で、
                        // 台帳には触らない
                        if !ids.is_empty() {
                            let thread = opt("thread_ts");
                            let c = dctx(thread.as_deref().or(ids.first().map(String::as_str)));
                            c.info(
                                "bridge",
                                &format!(
                                    "disposition=edit thread={} covers=[{}]",
                                    thread.as_deref().unwrap_or("-"),
                                    ids.join(",")
                                ),
                            );
                            let d = bridge_state::Disposition {
                                kind: "edit",
                                channel_id: s("channel_id"),
                                thread_ts: thread,
                                message_ids: ids,
                                session_id: session_id.to_string(),
                            };
                            notify(dispo, d, &c).await;
                        }
                        Ok("edited".to_string())
                    }
                }
            }
            "fetch_messages" => {
                let limit = args["limit"].as_u64().unwrap_or(20).min(100) as u16;
                let channel = s("channel");
                match opt("thread_ts") {
                    Some(ts) => self.replies(&channel, &ts, limit).await,
                    None => self.history(&channel, limit).await,
                }
                .map(|m| FetchedMsg::render_all(&m))
            }
            "download_attachment" => self.download_attachment(&s("file_id"), state_dir).await,
            "no_reply" => {
                // Slack には何も出さない。だが沈黙は disposition — 記録して台帳を消す。
                let thread = opt("thread_ts");
                let c = dctx(thread.as_deref().or(ids.first().map(String::as_str)));
                c.info(
                    "bridge",
                    &format!(
                        "disposition=no_reply thread={} covers=[{}]{}",
                        thread.as_deref().unwrap_or("-"),
                        ids.join(","),
                        opt("reason")
                            .filter(|r| !r.is_empty())
                            .map(|r| format!(" reason={r}"))
                            .unwrap_or_default()
                    ),
                );
                let d = bridge_state::Disposition {
                    kind: "no_reply",
                    channel_id: s("channel_id"),
                    thread_ts: thread,
                    message_ids: ids,
                    session_id: session_id.to_string(),
                };
                notify(dispo, d, &c).await;
                Ok("recorded — nothing posted".to_string())
            }
            other => Err(format!("unknown tool: {other}")),
        };
        match out {
            Ok(v) => {
                ctx.info("tools", &format!("{tool} ok"));
                Ok(v)
            }
            Err(e) => {
                ctx.error("tools", &format!("{tool} failed: {e}"));
                Err(e)
            }
        }
    }
}

impl<A: SlackApi + Sync> SlackOps for A {}

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

#[cfg(test)]
mod tests {
    /// **自分の2本までは正常**(slack-morphism の既定。実機の起動ログで確定)。
    /// 3本目からは別のプロセスが同じ app を消費している = 事故。
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

    /// TTL を過ぎた添付だけ消す。境界(ちょうど TTL)は残す。
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
        // mtime は「今」なので、掃除の now を進めて年齢を作る
        let now = crate::bridge::Host::now_ms();
        Api::sweep_inbox(&dir, now); // まだ何も消えない
        assert!(fresh.exists() && old.exists());

        Api::sweep_inbox(&dir, now + INBOX_TTL_MS + 1_000);
        assert!(!old.exists(), "TTL を過ぎたものは消す");
        assert!(!fresh.exists(), "同じ時刻に書いたものは同じ扱い");
        assert!(!edge.exists());

        // 読めないディレクトリでも落ちない
        Api::sweep_inbox(std::path::Path::new("/nonexistent-inbox-3f9"), now);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 保存名は必ず inbox 直下の1要素になる — name は Slack から来る外部入力。
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
        // file_id 側も外部入力(MCP ツールの引数)— 同じ扱い
        assert_eq!(Api::attachment_file_name("../../F1", "a.png"), "F1-a.png");
        assert_eq!(Api::attachment_file_name("/", "a.png"), "attachment-a.png");
        // どの入力でも inbox の直下から出ない — かつ "." / ".." に化けない
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
        // 封筒の属性を壊す `"` は落とす(file_paths="…")
        assert_eq!(Api::attachment_file_name("F1", "a\"b.png"), "F1-ab.png");
    }

    /// 大きすぎる添付はダウンロードに**入る前に**断る(FakeApi の download_to は
    /// unimplemented! — 判定が先に返らなければこのテストは panic する)。
    #[tokio::test]
    async fn oversized_attachment_is_refused_before_downloading() {
        let api = FakeApi {
            file_size: MAX_ATTACHMENT_BYTES + 1,
            ..Default::default()
        };
        let err = api
            .download_attachment("F1", std::path::Path::new("/nonexistent"))
            .await
            .unwrap_err();
        assert_eq!(err, "file too large: 50.0MB, max 50MB", "文言は現行の原文");
    }

    /// 呼び出しを 1 行の文字列で記録する fake。 7 でも使う。
    #[derive(Default)]
    struct FakeApi {
        posted: std::sync::Mutex<Vec<(String, String, Option<String>)>>,
        calls: std::sync::Mutex<Vec<String>>,
        msgs: Vec<FetchedMsg>,
        /// file_info が返すバイト数(上限判定のテスト用)。
        file_size: u64,
        fail: bool,
        /// replies だけ落とす(react の根引き失敗を作るため — add_reaction は成功させたい)。
        replies_fail: bool,
    }

    impl FakeApi {
        /// スレッドの根が**生きている** fake(返信の抑止に掛からない普通の状態)。
        fn with_live_root() -> Self {
            Self {
                msgs: vec![FetchedMsg {
                    ts: "1.0".into(),
                    user: "U1".into(),
                    text: "root".into(),
                    thread_ts: None,
                }],
                ..Default::default()
            }
        }

        /// 全ての呼び出しが Err を返す fake。
        fn failing() -> Self {
            Self {
                fail: true,
                ..Default::default()
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn record(&self, call: String) -> Result<(), String> {
            self.calls.lock().unwrap().push(call);
            if self.fail {
                Err("slack said no".into())
            } else {
                Ok(())
            }
        }
    }

    impl SlackApi for FakeApi {
        async fn post_message(
            &self,
            channel: &str,
            text: &str,
            thread_ts: Option<&str>,
        ) -> Result<String, String> {
            self.posted.lock().unwrap().push((
                channel.to_string(),
                text.to_string(),
                thread_ts.map(str::to_string),
            ));
            self.record(format!("post_message {channel} {text}"))?;
            Ok("9.9".to_string())
        }
        /// **記録の語を変える** — テストが「mrkdwn で出たか Markdown で出たか」を見分けられるように。
        async fn post_markdown(
            &self,
            channel: &str,
            text: &str,
            thread_ts: Option<&str>,
        ) -> Result<String, String> {
            self.posted.lock().unwrap().push((
                channel.to_string(),
                text.to_string(),
                thread_ts.map(str::to_string),
            ));
            self.record(format!("post_markdown {channel} {text}"))?;
            Ok("9.9".to_string())
        }
        async fn add_reaction(&self, c: &str, t: &str, e: &str) -> Result<(), String> {
            self.record(format!("add_reaction {c} {t} {e}"))
        }
        async fn remove_reaction(&self, c: &str, t: &str, e: &str) -> Result<(), String> {
            self.record(format!("remove_reaction {c} {t} {e}"))
        }
        async fn delete_message(&self, c: &str, t: &str) -> Result<(), String> {
            self.record(format!("delete_message {c} {t}"))
        }
        async fn update_markdown(&self, c: &str, t: &str, x: &str) -> Result<(), String> {
            self.record(format!("update_markdown {c} {t} {x}"))
        }
        async fn update_message(&self, _c: &str, _t: &str, _x: &str) -> Result<(), String> {
            Ok(())
        }
        async fn history(&self, _c: &str, _l: u16) -> Result<Vec<FetchedMsg>, String> {
            Ok(self.msgs.clone())
        }
        async fn replies(&self, c: &str, t: &str, _l: u16) -> Result<Vec<FetchedMsg>, String> {
            self.record(format!("replies {c} {t}"))?;
            if self.replies_fail {
                return Err("channel_not_found".into());
            }
            Ok(self.msgs.clone())
        }
        async fn file_info(&self, _f: &str) -> Result<(String, String, u64), String> {
            Ok((
                "https://slack.test/f".into(),
                "shot.png".into(),
                self.file_size,
            ))
        }
        async fn get_permalink(&self, c: &str, t: &str) -> Result<String, String> {
            self.record(format!("get_permalink {c} {t}"))?;
            Ok(format!(
                "https://slack.test/archives/{c}/p{}",
                t.replace('.', "")
            ))
        }
        async fn channel_display_name(&self, c: &str) -> Option<String> {
            self.record(format!("channel_display_name {c}")).ok()?;
            Some(format!("#{c}"))
        }
        /// `UB…` を bot(→ `B…`)、それ以外を人間として扱う固定ルール。
        async fn resolve_bot_id(&self, u: &str) -> Result<Option<String>, String> {
            self.record(format!("resolve_bot_id {u}"))?;
            Ok(u.strip_prefix("UB").map(|rest| format!("B{rest}")))
        }
        async fn download_to(&self, _u: &str, _d: &std::path::Path) -> Result<(), String> {
            unimplemented!()
        }
        async fn upload_file(
            &self,
            channel: &str,
            thread_ts: Option<&str>,
            path: &std::path::Path,
        ) -> Result<(), String> {
            let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            self.record(format!("get_upload_url_external {name} {}", bytes.len()))?;
            self.record(format!("files_upload_via_url {name}"))?;
            self.record(format!(
                "files_complete_upload_external {channel} {}",
                thread_ts.unwrap_or("-")
            ))
        }
        async fn set_thinking_status(
            &self,
            channel: &str,
            thread_ts: &str,
            status: &str,
        ) -> Result<(), String> {
            self.record(format!(
                "set_thinking_status {channel} {thread_ts} {status}"
            ))
        }
    }

    /// execute_tool を呼ぶのに要るもの一式(disposition の受信口つき)。
    fn harness() -> (
        FakeApi,
        std::path::PathBuf,
        tokio::sync::mpsc::Sender<bridge_state::Disposition>,
        tokio::sync::mpsc::Receiver<bridge_state::Disposition>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        // 根は生きている前提(消えたスレッドの抑止は専用のテストで見る)
        (
            FakeApi::with_live_root(),
            std::path::PathBuf::from("/tmp"),
            tx,
            rx,
        )
    }

    #[tokio::test]
    async fn upload_file_round_trip_records_all_three_calls() {
        let api = FakeApi::default();
        let path = std::env::temp_dir().join(format!("sc-upload-{}.txt", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();
        api.upload_file("C1", Some("171.002"), &path).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let calls = api.calls();
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("get_upload_url_external "))
        );
        assert!(calls.iter().any(|c| c.starts_with("files_upload_via_url ")));
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("files_complete_upload_external "))
        );
    }

    #[test]
    fn oversized_or_missing_attachment_is_rejected() {
        let path = std::env::temp_dir().join(format!("sc-size-{}.bin", std::process::id()));
        std::fs::write(&path, vec![0u8; 10]).unwrap();
        let paths = vec![path.to_string_lossy().to_string()];
        let err = FakeApi::check_attachment_sizes(&paths, 5).unwrap_err();
        let ok = FakeApi::check_attachment_sizes(&paths, MAX_ATTACHMENT_BYTES);
        let _ = std::fs::remove_file(&path);
        assert!(err.contains("too large"), "{err}");
        assert!(ok.is_ok());
        assert!(
            FakeApi::check_attachment_sizes(&["/nonexistent/nope.bin".to_string()], 5).is_err()
        );
    }

    /// `markdown: true` のときだけ Markdown ブロックで出す。既定は今までどおり mrkdwn —
    /// 既存のワーカーの見え方を1バイトも変えない。
    #[tokio::test]
    async fn reply_uses_a_markdown_block_only_when_asked() {
        let (api, dir, tx, _rx) = harness();
        api.execute_tool(
            &dir,
            "sid-1",
            "reply",
            &serde_json::json!({"channel_id": "C1", "text": "# 見出し", "markdown": true}),
            &tx,
        )
        .await
        .unwrap();
        assert!(
            api.calls()
                .iter()
                .any(|c| c.starts_with("post_markdown C1")),
            "{:?}",
            api.calls()
        );
    }

    /// **既定が標準 Markdown**。LLM が普通に書く `**bold**` や表がそのまま出る。
    #[tokio::test]
    async fn reply_defaults_to_standard_markdown() {
        let (api, dir, tx, _rx) = harness();
        api.execute_tool(
            &dir,
            "sid-1",
            "reply",
            &serde_json::json!({"channel_id": "C1", "text": "# 見出し"}),
            &tx,
        )
        .await
        .unwrap();
        assert!(
            api.calls()
                .iter()
                .any(|c| c.starts_with("post_markdown C1")),
            "{:?}",
            api.calls()
        );
    }

    /// 旧来の mrkdwn に戻す口は残す(`*太字*` を意図して書いた文面のため)。
    #[tokio::test]
    async fn reply_can_fall_back_to_mrkdwn() {
        let (api, dir, tx, _rx) = harness();
        api.execute_tool(
            &dir,
            "sid-1",
            "reply",
            &serde_json::json!({"channel_id": "C1", "text": "*太字*", "markdown": false}),
            &tx,
        )
        .await
        .unwrap();
        assert!(
            api.calls().iter().any(|c| c.starts_with("post_message C1")),
            "{:?}",
            api.calls()
        );
        assert!(!api.calls().iter().any(|c| c.starts_with("post_markdown")));
    }

    /// `edit_message` も同じ既定 — 投稿と編集で見え方が違うと使い分けを覚える羽目になる。
    #[tokio::test]
    async fn edit_message_defaults_to_standard_markdown_too() {
        let (api, dir, tx, _rx) = harness();
        api.execute_tool(
            &dir,
            "sid-1",
            "edit_message",
            &serde_json::json!({"channel_id": "C1", "message_ts": "1.0", "text": "## 答え"}),
            &tx,
        )
        .await
        .unwrap();
        assert!(
            api.calls()
                .iter()
                .any(|c| c.starts_with("update_markdown C1")),
            "{:?}",
            api.calls()
        );
    }

    #[tokio::test]
    async fn reply_with_files_uploads_after_posting_text() {
        let (api, dir, tx, _rx) = harness();
        let path = std::env::temp_dir().join(format!("sc-reply-{}.txt", std::process::id()));
        std::fs::write(&path, b"x").unwrap();
        let args = serde_json::json!({
            "channel_id": "C1", "text": "hi", "thread_ts": "1.0",
            "files": [path.to_string_lossy()],
        });
        let out = api
            .execute_tool(&dir, "sid", "reply", &args, &tx)
            .await
            .unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(out.starts_with("posted ts="), "{out}");
        let calls = api.calls();
        let post = calls
            .iter()
            .position(|c| c.starts_with("post_markdown") || c.starts_with("post_message"))
            .unwrap();
        let upload = calls
            .iter()
            .position(|c| c.starts_with("get_upload_url_external"))
            .unwrap();
        assert!(
            post < upload,
            "text must post before file upload: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c == "files_complete_upload_external C1 1.0"),
            "{calls:?}"
        );
    }

    /// 検証はテキスト投稿より前 — 1つでも読めなければ**何も**投稿しない。
    #[tokio::test]
    async fn reply_with_unreadable_file_posts_nothing() {
        let (api, dir, tx, mut rx) = harness();
        let args = serde_json::json!({
            "channel_id": "C1", "text": "hi", "files": ["/nonexistent/nope.bin"],
        });
        assert!(
            api.execute_tool(&dir, "sid", "reply", &args, &tx)
                .await
                .is_err()
        );
        assert!(api.calls().is_empty(), "{:?}", api.calls());
        assert!(
            rx.try_recv().is_err(),
            "a refused reply disposes of nothing"
        );
    }

    #[tokio::test]
    async fn upload_file_missing_path_is_err() {
        let api = FakeApi::default();
        let missing = std::path::Path::new("/nonexistent/definitely-not-here.txt");
        assert!(api.upload_file("C1", None, missing).await.is_err());
    }

    #[tokio::test]
    async fn flip_removes_ack_and_adds_robot() {
        let api = FakeApi::default();
        api.flip_to_received("C1", "171.002", "eyes").await;
        assert_eq!(
            api.calls(),
            vec![
                "remove_reaction C1 171.002 eyes",
                "remove_reaction C1 171.002 arrows_counterclockwise",
                "add_reaction C1 171.002 robot_face",
            ]
        );
    }

    #[tokio::test]
    async fn flip_swallows_slack_errors() {
        let api = FakeApi::failing();
        api.flip_to_received("C1", "171.002", "eyes").await; // panic せず完走すれば良い
        assert_eq!(api.calls().len(), 3, "失敗しても3手とも試みる");
    }

    #[test]
    fn ack_emoji_defaults_to_eyes() {
        let mut access = bridge_state::Access::default();
        assert_eq!(Api::ack_emoji(&access), "eyes");
        access.ack_reaction = Some("spiral_note_pad".into());
        assert_eq!(Api::ack_emoji(&access), "spiral_note_pad");
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

    /// 長い返信は分けて投げる。切り方は2通り(既定は上限で断ち切る)。
    #[test]
    fn a_long_reply_is_split_into_slack_sized_posts() {
        assert_eq!(chunk("short", 10, false), vec!["short"]);
        // 上限ちょうどは割らない
        assert_eq!(chunk("0123456789", 10, false), vec!["0123456789"]);
        assert_eq!(chunk("0123456789a", 10, false), vec!["0123456789", "a"]);
        // 日本語でも**文字**で割る(バイトで割ると途中で壊れる)
        let ja = "あ".repeat(25);
        let parts = chunk(&ja, 10, false);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].chars().count(), 10);
        assert_eq!(parts.concat(), ja);
        // newline: 上限の手前の切れ目を探し、切れ目の改行は持ち越さない
        let text = format!("{}\n\n{}", "a".repeat(60), "b".repeat(60));
        let parts = chunk(&text, 100, true);
        assert_eq!(parts[0], "a".repeat(60));
        assert_eq!(parts[1], "b".repeat(60));
        // 切れ目が早すぎる(上限の半分より手前)ときは上限で断ち切る
        let text = format!("{}\n{}", "a".repeat(10), "b".repeat(200));
        let parts = chunk(&text, 100, true);
        assert_eq!(parts[0].chars().count(), 100);
    }

    /// 根が消えたスレッドには投稿しない。**黙って捨てず**台帳は覆う。
    #[tokio::test]
    async fn a_reply_into_a_deleted_thread_is_suppressed_but_still_disposes() {
        let (api, dir, tx, mut rx) = harness();
        let api = FakeApi {
            msgs: Vec::new(), // 根が引けない = 消えている
            ..api
        };
        let out = api
            .execute_tool(
                &dir,
                "sid-1",
                "reply",
                &serde_json::json!({
                    "channel_id": "C1", "text": "hi", "thread_ts": "1.0",
                    "message_ids": ["1.1"],
                }),
                &tx,
            )
            .await
            .unwrap();
        assert!(out.contains("suppressed"), "{out}");
        assert!(api.posted.lock().unwrap().is_empty(), "投稿はしない");
        let d = rx.try_recv().unwrap();
        assert_eq!(d.kind, "reply", "台帳は覆う(未応答のまま残さない)");
        assert_eq!(d.message_ids, vec!["1.1"]);
    }

    #[tokio::test]
    async fn reply_posts_to_the_thread() {
        let (api, dir, tx, mut rx) = harness();
        let out = api
            .execute_tool(
                &dir,
                "sid-1",
                "reply",
                &serde_json::json!({"channel_id": "C1", "text": "hi", "thread_ts": "1.0"}),
                &tx,
            )
            .await
            .unwrap();
        assert!(out.contains("9.9"), "{out}");
        let posted = api.posted.lock().unwrap();
        assert_eq!(posted[0].0, "C1");
        assert_eq!(posted[0].2.as_deref(), Some("1.0"));
        // ids 空でも**送る** — 「空 = スレッド全消化」の振り分けは台帳を持つ main の仕事
        assert!(rx.try_recv().unwrap().message_ids.is_empty());
    }

    #[tokio::test]
    async fn no_reply_posts_nothing() {
        let (api, dir, tx, _rx) = harness();
        let out = api
            .execute_tool(
                &dir,
                "sid-1",
                "no_reply",
                &serde_json::json!({"channel_id": "C1", "message_ids": ["C1:1.0"]}),
                &tx,
            )
            .await
            .unwrap();
        assert!(
            api.posted.lock().unwrap().is_empty(),
            "no_reply must post nothing"
        );
        assert!(!out.is_empty(), "but it must answer the worker: {out}");
    }

    #[tokio::test]
    async fn unknown_tool_is_an_error() {
        let (api, dir, tx, _rx) = harness();
        let out = api
            .execute_tool(&dir, "sid-1", "nope", &serde_json::json!({}), &tx)
            .await;
        assert!(out.is_err());
    }

    #[tokio::test]
    async fn reply_emits_disposition_with_covers() {
        let (api, dir, tx, mut rx) = harness();
        let args = serde_json::json!({"channel_id": "C1", "text": "hi", "thread_ts": "1.0",
            "message_ids": ["1.1", "1.2"]});
        api.execute_tool(&dir, "sid", "reply", &args, &tx)
            .await
            .unwrap();
        let d = rx.try_recv().unwrap();
        assert_eq!((d.kind, d.message_ids.len()), ("reply", 2));
        assert_eq!(d.thread_ts.as_deref(), Some("1.0"));
    }

    #[tokio::test]
    async fn a_failed_post_disposes_of_nothing() {
        let (_, dir, tx, mut rx) = harness();
        let api = FakeApi::failing();
        let args = serde_json::json!({"channel_id": "C1", "text": "hi", "message_ids": ["1.1"]});
        assert!(
            api.execute_tool(&dir, "sid", "reply", &args, &tx)
                .await
                .is_err()
        );
        assert!(
            rx.try_recv().is_err(),
            "Slack が受けていない返信で台帳を消してはいけない"
        );
    }

    #[tokio::test]
    async fn no_reply_emits_disposition_and_posts_nothing() {
        let (api, dir, tx, mut rx) = harness();
        let args =
            serde_json::json!({"channel_id": "C1", "message_ids": ["1.1"], "thread_ts": "1.0"});
        api.execute_tool(&dir, "sid", "no_reply", &args, &tx)
            .await
            .unwrap();
        assert!(api.calls().is_empty()); // Slack には何も出ていない
        assert_eq!(rx.try_recv().unwrap().kind, "no_reply");
    }

    #[tokio::test]
    async fn edit_without_ids_is_not_a_disposition() {
        let (api, dir, tx, mut rx) = harness();
        let args = serde_json::json!({"channel_id": "C1", "message_ts": "9.9", "text": "v2"});
        api.execute_tool(&dir, "sid", "edit_message", &args, &tx)
            .await
            .unwrap();
        assert!(rx.try_recv().is_err()); // 進捗編集は台帳に触らない
    }

    #[tokio::test]
    async fn edit_with_ids_covers_them() {
        let (api, dir, tx, mut rx) = harness();
        let args = serde_json::json!({"channel_id": "C1", "message_ts": "9.9", "text": "v2",
            "thread_ts": "1.0", "message_ids": ["1.1"]});
        api.execute_tool(&dir, "sid", "edit_message", &args, &tx)
            .await
            .unwrap();
        let d = rx.try_recv().unwrap();
        assert_eq!((d.kind, d.thread_ts.as_deref()), ("edit", Some("1.0")));
    }

    #[tokio::test]
    async fn react_covers_the_reacted_ts_under_the_thread_root() {
        let (mut api, dir, tx, mut rx) = harness();
        // スレッド外のメッセージ(thread_ts 無し)— 根は message_ts 自身
        api.msgs = vec![FetchedMsg {
            ts: "1.5".into(),
            user: "U1".into(),
            text: "standalone".into(),
            thread_ts: None,
        }];
        let args = serde_json::json!({"channel_id": "C1", "message_ts": "1.5", "emoji": "eyes"});
        api.execute_tool(&dir, "sid", "react", &args, &tx)
            .await
            .unwrap();
        let d = rx.try_recv().unwrap();
        assert_eq!((d.kind, d.thread_ts.as_deref()), ("react", Some("1.5")));
        assert_eq!(d.message_ids, vec!["1.5".to_string()]);
    }

    #[tokio::test]
    async fn react_to_a_reply_keys_off_the_thread_root_not_the_replied_ts() {
        let (mut api, dir, tx, mut rx) = harness();
        // スレッド内の**返信**にリアクション。replies が返す先頭は親とは限らないので、
        // その ts(1.5)ではなく thread_ts(1.0)が台帳の鍵でなければならない
        api.msgs = vec![FetchedMsg {
            ts: "1.5".into(),
            user: "U1".into(),
            text: "a reply".into(),
            thread_ts: Some("1.0".into()),
        }];
        let args = serde_json::json!({"channel_id": "C1", "message_ts": "1.5", "emoji": "eyes"});
        api.execute_tool(&dir, "sid", "react", &args, &tx)
            .await
            .unwrap();
        let d = rx.try_recv().unwrap();
        assert_eq!(
            d.thread_ts.as_deref(),
            Some("1.0"),
            "返信の ts を根にしてはいけない"
        );
        assert_eq!(
            d.message_ids,
            vec!["1.5".to_string()],
            "覆うのはリアクトした ts"
        );
    }

    #[tokio::test]
    async fn react_without_a_resolvable_root_skips_the_disposition() {
        // 根引きが**失敗**したときだけ根が不明(現行も catch のときだけ undefined)
        let (mut api, dir, tx, mut rx) = harness();
        api.replies_fail = true;
        let args = serde_json::json!({"channel_id": "C1", "message_ts": "1.5", "emoji": "eyes"});
        api.execute_tool(&dir, "sid", "react", &args, &tx)
            .await
            .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "根が不明なまま台帳を消すと誤射する(best-effort)"
        );
    }

    /// status / allow-bot が使う3種を trait 経由で踏む(実 API の形は E2E で確かめる)。
    #[tokio::test]
    async fn lookup_apis_go_through_the_trait() {
        let api = FakeApi::default();
        assert_eq!(
            api.get_permalink("C1", "17.5").await.unwrap(),
            "https://slack.test/archives/C1/p175"
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
                "get_permalink C1 17.5",
                "channel_display_name C1",
                "resolve_bot_id UB42",
                "resolve_bot_id U9"
            ],
        );
        // 名前解決は best-effort — Slack が落ちても None で返る(Err にしない)
        assert_eq!(FakeApi::failing().channel_display_name("C1").await, None);
        assert!(FakeApi::failing().get_permalink("C1", "1").await.is_err());
    }

    /// 文言は現行 Bun の原文をコピーしたもの — 切替日の見た目を変えないための固定。
    /// 出典は各定数の doc コメント(…`)。
    #[test]
    fn thinking_status_wording_is_pinned() {
        // 三点リーダは U+2026 の1文字。ASCII の "..." に化けると見た目が変わる
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

    /// 空文字がクリア(Slack の約束)。FakeApi は素通しで記録するだけ。
    #[tokio::test]
    async fn set_thinking_status_records_set_and_clear() {
        let api = FakeApi::default();
        api.set_thinking_status("C1", "1.1", THINKING_STATUS)
            .await
            .expect("set");
        api.set_thinking_status("C1", "1.1", "")
            .await
            .expect("clear");
        assert_eq!(
            api.calls(),
            vec![
                "set_thinking_status C1 1.1 is thinking\u{2026}".to_string(),
                "set_thinking_status C1 1.1 ".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn set_thinking_status_failure_is_an_err_the_caller_can_log() {
        let api = FakeApi::failing();
        assert!(
            api.set_thinking_status("C1", "1.1", TYPING_STATUS)
                .await
                .is_err(),
            "呼び手が best-effort でログできるよう Err で返る"
        );
    }
}
