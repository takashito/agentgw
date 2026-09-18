//! What arrives from Slack, and the gate it passes through: the inbound message model,
//! edits / deletions / reactions, duplicate suppression, and the owner gate.
//!
//! `impl Access` here holds only the gate (`gate`, per-channel tool grants); `Access`
//! itself is persisted state and lives in `state.rs`.

use crate::bridge::state::{Access, ThreadEntry};
use slack_morphism::prelude::*;

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

impl InboundMsg {
    /// Slack の message イベント → Bridge の語彙。返信できない形(中身も添付も無い・channel 無し)は None。
    /// Slack の push イベントを Bridge の語彙へ。落とすべきものは None。
    pub(crate) fn from_event(ev: &SlackMessageEvent) -> Option<InboundMsg> {
        let content = ev.content.as_ref()?;
        // 画像だけの投稿は `subtype:"file_share"` で text 無しに届く。テキストが無くても
        // 添付があれば通す(現行は `msg.text ?? ''` で素通り)
        let files: Vec<InboundFile> = content
            .files
            .iter()
            .flatten()
            .map(|f| InboundFile {
                id: f.id.to_string(),
                name: f.name.clone().unwrap_or_else(|| f.id.to_string()),
            })
            .collect();
        // subtype 付きは**システムメッセージ**(「〜が参加しました」「トピックを変えました」)。
        // 通すのは `bot_message` と `thread_broadcast`、そして添付付き(file_share = 画像だけの
        // 投稿。)だけ(関門)。これが無いと、チャンネルへの
        // 入退室までワーカーに配達される
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
            // 自分の返信を拾って無限ループしないための判定(スパイクA 実証)
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

    /// `message_deleted` を1件の受信メッセージに仕立てる。ワーカーに渡すのは
    /// 取り消しの指示文で、消された本文はその中に引用する。
    ///
    /// **bot 自身の投稿の削除は無視する**(付箋や返信をユーザーが消しただけ) — 取り消す
    /// 「依頼」が無いので、伝えてもワーカーを惑わせるだけ。
    pub(crate) fn from_deletion(ev: &SlackMessageEvent) -> Option<InboundMsg> {
        let deleted_ts = ev.deleted_ts.as_ref()?.to_string();
        let prev = ev.previous_message.as_ref();
        let sender = prev.map(|p| &p.sender);
        // bot が書いたものは「依頼」ではない。人が書いたものだけ通す
        if sender.is_some_and(|s| s.bot_id.is_some() || s.user.is_none()) {
            return None;
        }
        let content = prev.and_then(|p| p.content.as_ref());
        let text = content.and_then(|c| c.text.as_deref()).unwrap_or_default();
        let had_files = content.is_some_and(|c| c.files.iter().flatten().next().is_some());
        // 削除イベントは根を寄越さないことがある(`previous_message` に thread_ts が無い形)。
        // 暫定で「消された ts 自身」を根に置き、Bridge 側が台帳から本当の根に読み替える
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

    /// `message_changed` を1件の受信メッセージに仕立てる。`text` は**新しい本文
    /// そのもの**で、ワーカーに渡す指示文は Bridge が組む(言い方が「いま処理中かどうか」で
    /// 変わり、それを知っているのは台帳を持つ Bridge だけ)。
    ///
    /// Slack は編集以外でも `message_changed` を撃つ — リンクのプレビューが付いたとき、
    /// bot が自分の付箋を編集したとき。**本文が変わっていないもの**と **bot の投稿**は捨てる
    pub(crate) fn from_edit(ev: &SlackMessageEvent) -> Option<InboundMsg> {
        let m = ev.message.as_ref()?;
        if m.sender.bot_id.is_some() || m.sender.user.is_none() {
            return None; // bot 自身の編集(付箋の描き直しがこれ)
        }
        let new_text = m.content.as_ref()?.text.clone().unwrap_or_default();
        let old_text = ev
            .previous_message
            .as_ref()
            .and_then(|p| p.content.as_ref())
            .and_then(|c| c.text.clone())
            .unwrap_or_default();
        if new_text == old_text {
            return None; // unfurl などのメタ変更 — 編集ではない
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
            // 編集イベントも根を寄越さないことがある(削除と同じ。Bridge が台帳で読み替える)
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
                // 改訂ごとに変わる id。取れなければ本文長で代用する(再配達の目印)
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

    /// リアクションを1件の受信メッセージに仕立てる。**bot が書いたメッセージへの
    /// リアクションだけ**扱う(`authored` と同じ判定)
    /// 人同士のやり取りに付いた絵文字までワーカーに流さない。
    ///
    /// スレッドの根は付けられた側の `thread_ts`、無ければそのメッセージ自身。`ts` は
    /// **付けられた側の ts** にする — 👀 を付ける先も、dedup の鍵もそこになる。
    pub(crate) fn from_reaction(
        item: &SlackReactionsItem,
        reactor: &str,
        emoji: String,
        added: bool,
    ) -> Option<InboundMsg> {
        let SlackReactionsItem::Message(m) = item else {
            return None; // ファイルへのリアクションは扱わない
        };
        // **書き手はここでは分からない。** リアクションの `item` に入るのは種別・チャンネル・ts
        // だけで、送り主の欄は常に空。ここで「自分の投稿か」を判断しようとすると、空欄を
        // 「自分の投稿」と読んで**全部通す**検査になる(2026-08-02 に実測)。判断は Bridge 側 —
        // 元の投稿を1回引いた後(`Api::message_at`)。
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
            // リアクションを**付けた**のは人。付けられた側が bot なのは上で確かめた
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
}

/// 書き換え1件。`revision` は Slack の `edited.ts`(同じ編集が再配達されたときの目印)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edited {
    pub ts: String,
    pub revision: String,
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

/// 門番の答え — この1通をワーカーに渡すか。
///
/// `Drop` の `&'static str` は落とした理由で、そのままログに出る(黙って捨てない)。
/// 判断そのものは [`Access::gate`]。
#[derive(PartialEq, Eq, Debug)]
pub enum GateVerdict {
    Serve,
    /// Owner 以外の人が、動いているスレッドで喋った。ワーカーには**文脈として**
    /// 渡す(返事は期待しない)。落とすとスレッドの会話が歯抜けになる。
    Context,
    Drop(&'static str),
}

/// ワーカーの生存。`bridge/worker.rs` の `Workers` が facts から導く。
///
/// `agent::SpawnReq` もこの型を借りている — 依存の向きの唯一の例外(座席チェックの
/// エラー文言が `{state:?}` を含むため)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WorkerState {
    Absent,
    Starting,
    Ready,
}

/// 門を通った1通をどう捌くか — 「起こす / 再開する / 渡す / 溜める」の4通りしかない。
///
/// スレッドの記録とワーカーの生死だけで決まる([`Action::decide`])。
#[derive(PartialEq, Eq, Debug)]
pub enum Dispatch {
    SpawnNew,
    SpawnResume(String),
    Deliver,
    Queue,
}

/// entry 無し → SpawnNew / Ready → Deliver / Starting → Queue / Absent → SpawnResume。
impl Dispatch {
    pub fn decide(entry: Option<&ThreadEntry>, worker: WorkerState) -> Dispatch {
        let Some(entry) = entry else {
            return Dispatch::SpawnNew;
        };
        match worker {
            WorkerState::Ready => Dispatch::Deliver,
            WorkerState::Starting => Dispatch::Queue,
            // 過去のセッションを知らない entry は再開できない → 新規で建てる
            WorkerState::Absent => match entry.agent_id.as_deref() {
                Some(sid) => Dispatch::SpawnResume(sid.to_string()),
                None => Dispatch::SpawnNew,
            },
        }
    }
}

const DEDUP_CAP: usize = 512;

/// 同一 (channel, ts) の再配達を落とす覚え書き。1メッセージが複数イベントで届くため必須
///
/// ponytail: 上限512の線形スキャン。イベント率が上がるなら HashSet + VecDeque に。
#[derive(Default)]
pub struct RecentDeliveries {
    seen: std::collections::VecDeque<(String, String)>,
}

impl RecentDeliveries {
    pub fn new() -> Self {
        Self::default()
    }

    /// 既出なら true。初出なら覚えて false。
    pub fn seen(&mut self, channel: &str, ts: &str) -> bool {
        if self.seen.iter().any(|(c, t)| c == channel && t == ts) {
            return true;
        }
        if self.seen.len() >= DEDUP_CAP {
            self.seen.pop_front();
        }
        self.seen.push_back((channel.to_string(), ts.to_string()));
        false
    }
}

impl Access {
    // ── 門 ──
    // 疑わしきは Drop(fail-closed)。owner 空の access は誰も通さない。
    /// このチャンネルで「以後訊かない」と押されたツールか。
    pub fn channel_tool_allowed(&self, channel: &str, tool: &str) -> bool {
        self.routes
            .get(channel)
            .and_then(|r| r.allowed_tools.as_ref())
            .is_some_and(|v| v.iter().any(|t| t == tool))
    }

    /// 「以後このチャンネルでは訊かない」を覚える。**人がボタンを押したときだけ**呼ばれる。
    pub fn grant_channel_tool(&mut self, channel: &str, tool: &str) {
        if channel.is_empty() || tool.is_empty() {
            return;
        }
        let allowed = self
            .routes
            .entry(channel.to_string())
            .or_default()
            .allowed_tools
            .get_or_insert_with(Vec::new);
        if !allowed.iter().any(|t| t == tool) {
            allowed.push(tool.to_string());
        }
    }

    /// 入れる / 文脈として入れる / 落とす の3値(`decideChannelAccess` と
    /// `decideDmAccess`)。
    ///
    /// **チャンネルが access.json に載っているかは見ない。** 登録(routes)が持っているのは
    /// 「そのチャンネルでどのフォルダを触るか」で、入れる判断には使わない — 未登録の
    /// チャンネルでも Owner のメンションには応える(現行の判定と同じ)。
    ///
    /// チャンネルでは**メンション**か**既に動いているスレッド**が要る。これが無いと、
    /// 登録済みチャンネルの雑談まで全部ワーカーに流れる。
    pub fn gate(&self, msg: &InboundMsg, is_mention: bool, is_active_thread: bool) -> GateVerdict {
        let dm = msg.channel_kind == ChannelKind::Dm;
        // bot を DM に入れる道は無い(`isBotDMBlocked`)
        if msg.is_bot && dm {
            return GateVerdict::Drop("bot-dm-blocked");
        }
        if self.owner.is_empty() {
            return GateVerdict::Drop(if dm { "dm-no-owner" } else { "no-owner" });
        }
        // Slack Web API 経由の投稿には**人が書いたものでも** bot_id が付く。
        // それでも Slack は本当の `user` を刻む(トークン由来なので本文からは詐称できない)ので、
        // その人が Owner なら人として扱う。Owner 以外・user 無しは bot(閉じる方に倒す)
        let is_owner = msg.user.as_deref() == Some(self.owner.as_str());
        if dm {
            return if is_owner {
                GateVerdict::Serve
            } else {
                GateVerdict::Drop("dm-not-owner")
            };
        }
        let reachable = is_mention || is_active_thread;
        if msg.is_bot && !is_owner {
            // Owner が allow-bot で許した bot だけ。それ以外は名指しでも通さない
            let allowed = msg
                .bot_id
                .as_deref()
                .is_some_and(|id| self.allowed_bots.iter().any(|b| b == id));
            if !allowed {
                return GateVerdict::Drop("drop-bot-not-allowed");
            }
            return if reachable {
                GateVerdict::Serve
            } else {
                GateVerdict::Drop("require-mention-unmet")
            };
        }
        if is_owner {
            return if reachable {
                GateVerdict::Serve
            } else {
                GateVerdict::Drop("require-mention-unmet")
            };
        }
        // Owner 以外の人 — 動いているスレッドの中でだけ**文脈として**渡す(返事はさせない)
        if is_active_thread {
            GateVerdict::Context
        } else {
            GateVerdict::Drop("drop-not-owner")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn msg(kind: ChannelKind, user: Option<&str>, is_bot: bool) -> InboundMsg {
        InboundMsg {
            channel: match kind {
                ChannelKind::Dm => "D1".into(),
                ChannelKind::Channel => "C1".into(),
            },
            channel_kind: kind,
            ts: "1.1".into(),
            thread_ts: None,
            user: user.map(str::to_string),
            is_bot,
            bot_id: is_bot.then(|| "B1".to_string()),
            text: "hi".into(),
            files: Vec::new(),
            file_paths: Vec::new(),
            file_errors: Vec::new(),
            reaction: None,
            deleted_ts: None,
            edited: None,
        }
    }

    fn access_with_route() -> Access {
        Access::from_str(r#"{"owner":"U1","routes":{"C1":{"repo_path":"/x"}}}"#).unwrap()
    }

    /// Web API 経由の投稿は**人が書いたものでも** bot_id が付く。Owner 本人の
    /// 投稿は人として通し、それ以外の bot は `allow-bot` されていなければ落とす。
    #[test]
    fn gate_drops_bots_but_honors_the_owners_web_api_post() {
        let a = access_with_route();
        assert_eq!(
            a.gate(&msg(ChannelKind::Channel, Some("U1"), true), true, false),
            GateVerdict::Serve
        );
        assert_eq!(
            a.gate(&msg(ChannelKind::Channel, Some("U2"), true), true, false),
            GateVerdict::Drop("drop-bot-not-allowed")
        );
        assert_eq!(
            a.gate(&msg(ChannelKind::Channel, None, true), true, false),
            GateVerdict::Drop("drop-bot-not-allowed")
        );
        // allow-bot された bot は、名指し(かアクティブスレッド)でだけ通る
        let allowed =
            Access::from_str(r#"{"owner":"U1","allowedBots":["B7"],"routes":{}}"#).unwrap();
        let mut b = msg(ChannelKind::Channel, Some("U9"), true);
        b.bot_id = Some("B7".into());
        assert_eq!(allowed.gate(&b, true, false), GateVerdict::Serve);
        assert_eq!(
            allowed.gate(&b, false, false),
            GateVerdict::Drop("require-mention-unmet")
        );
        // bot の DM は無条件で落とす
        let mut dm_bot = msg(ChannelKind::Dm, Some("U9"), true);
        dm_bot.bot_id = Some("B7".into());
        assert_eq!(
            allowed.gate(&dm_bot, true, false),
            GateVerdict::Drop("bot-dm-blocked")
        );
    }

    #[test]
    fn gate_serves_owner_dm() {
        let v = access_with_route().gate(&msg(ChannelKind::Dm, Some("U1"), false), true, false);
        assert_eq!(v, GateVerdict::Serve);
    }

    #[test]
    fn gate_drops_other_user_dm() {
        let v = access_with_route().gate(&msg(ChannelKind::Dm, Some("U2"), false), true, false);
        assert_eq!(v, GateVerdict::Drop("dm-not-owner"));
    }

    /// チャンネルで入れるかは **名指しか、動いているスレッドか** だけで決まる。
    /// **登録(routes)は見ない** — 未登録のチャンネルでも Owner の名指しには応える。
    #[test]
    fn gate_needs_a_mention_or_an_active_thread_not_a_route() {
        let a = access_with_route();
        let owner = msg(ChannelKind::Channel, Some("U1"), false);
        assert_eq!(a.gate(&owner, true, false), GateVerdict::Serve, "名指し");
        assert_eq!(
            a.gate(&owner, false, true),
            GateVerdict::Serve,
            "動いているスレッドの続き"
        );
        assert_eq!(
            a.gate(&owner, false, false),
            GateVerdict::Drop("require-mention-unmet"),
            "名指しでもスレッドの続きでもない雑談は流さない"
        );
        // 未登録チャンネル(C9)でも Owner の名指しは通る
        let mut elsewhere = owner.clone();
        elsewhere.channel = "C9".into();
        assert_eq!(a.gate(&elsewhere, true, false), GateVerdict::Serve);
    }

    /// Owner 以外の人は、動いているスレッドの中でだけ**文脈として**渡す。
    #[test]
    fn gate_passes_a_non_owner_as_context_only_inside_an_active_thread() {
        let a = access_with_route();
        let other = msg(ChannelKind::Channel, Some("U2"), false);
        assert_eq!(a.gate(&other, false, true), GateVerdict::Context);
        assert_eq!(
            a.gate(&other, true, false),
            GateVerdict::Drop("drop-not-owner"),
            "名指しされても Owner でなければ動かさない"
        );
    }

    #[test]
    fn gate_is_fail_closed_without_owner() {
        let empty = Access::default();
        assert!(matches!(
            empty.gate(&msg(ChannelKind::Dm, Some("U1"), false), true, false),
            GateVerdict::Drop(_)
        ));
        assert!(matches!(
            empty.gate(&msg(ChannelKind::Dm, None, false), true, false),
            GateVerdict::Drop(_)
        ));
    }

    #[test]
    fn dedup_drops_second_delivery_of_same_event() {
        let mut d = RecentDeliveries::new();
        assert!(!d.seen("C1", "1.1"), "first sighting is new");
        assert!(
            d.seen("C1", "1.1"),
            "same (channel, ts) must be a duplicate"
        );
        assert!(!d.seen("C1", "1.2"));
        assert!(!d.seen("C2", "1.1"));
    }

    #[test]
    fn dedup_evicts_oldest_past_cap() {
        let mut d = RecentDeliveries::new();
        for i in 0..512 {
            assert!(!d.seen("C1", &format!("{i}")));
        }
        assert!(d.seen("C1", "511"), "newest must still be remembered");
        d.seen("C1", "512"); // 513件目 → 最古(0)が押し出される
        assert!(!d.seen("C1", "0"), "oldest must have been evicted");
    }

    #[test]
    fn decide_table() {
        let entry = ThreadEntry::new("C1", "sid-1");
        assert_eq!(Dispatch::decide(None, WorkerState::Absent), Dispatch::SpawnNew);
        assert_eq!(
            Dispatch::decide(Some(&entry), WorkerState::Ready),
            Dispatch::Deliver
        );
        assert_eq!(
            Dispatch::decide(Some(&entry), WorkerState::Starting),
            Dispatch::Queue
        );
        assert_eq!(
            Dispatch::decide(Some(&entry), WorkerState::Absent),
            Dispatch::SpawnResume("sid-1".into())
        );
    }

    #[test]
    fn decide_spawns_new_when_entry_has_no_session() {
        let entry = ThreadEntry::default();
        assert_eq!(
            Dispatch::decide(Some(&entry), WorkerState::Absent),
            Dispatch::SpawnNew
        );
    }

    /// 実物の file_share イベント(画像だけ = text 無し)が落ちずに添付ごと通ること。
    #[test]
    fn normalize_keeps_a_text_less_file_share() {
        let ev: SlackMessageEvent = serde_json::from_str(
            r#"{"type":"message","subtype":"file_share","ts":"171.002","channel":"D1",
                "channel_type":"im","user":"U1",
                "files":[{"id":"F1","name":"shot.png"},{"id":"F2"}]}"#,
        )
        .expect("file_share event must deserialize");
        let m = InboundMsg::from_event(&ev)
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
        // 添付も本文も無ければ従来どおり落とす
        let empty: SlackMessageEvent = serde_json::from_str(
            r#"{"type":"message","ts":"171.003","channel":"D1","channel_type":"im","user":"U1"}"#,
        )
        .unwrap();
        assert!(InboundMsg::from_event(&empty).is_none());
    }

    fn ev(json: &str) -> SlackMessageEvent {
        serde_json::from_str(json).expect("event json")
    }

    #[test]
    fn normalizes_channel_message() {
        let m = InboundMsg::from_event(&ev(
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
        let m = InboundMsg::from_event(&ev(
            r#"{"ts":"2.2","thread_ts":"2.0","channel":"D1","channel_type":"im","user":"U1","text":"yo"}"#,
        ))
        .expect("should normalize");
        assert_eq!(m.channel_kind, ChannelKind::Dm);
        assert_eq!(m.thread_ts.as_deref(), Some("2.0"));
    }

    #[test]
    fn marks_bot_and_system_senders() {
        // bot_id 付き
        let m = InboundMsg::from_event(&ev(
            r#"{"ts":"3.3","channel":"C1","user":"U1","bot_id":"B1","text":"echo"}"#,
        ))
        .expect("should normalize");
        assert!(m.is_bot, "bot_id present must mark is_bot");
        // 送信者不明(システム)
        let m = InboundMsg::from_event(&ev(r#"{"ts":"3.4","channel":"C1","text":"joined"}"#))
            .expect("normalize");
        assert!(m.is_bot, "missing user must mark is_bot");
    }

    #[test]
    fn skips_messages_we_cannot_answer() {
        assert!(
            InboundMsg::from_event(&ev(r#"{"ts":"4.4","channel":"C1","user":"U1"}"#)).is_none(),
            "no text"
        );
        assert!(
            InboundMsg::from_event(&ev(r#"{"ts":"4.5","user":"U1","text":"x"}"#)).is_none(),
            "no channel"
        );
    }
}
