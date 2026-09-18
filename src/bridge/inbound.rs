//! What arrives from Slack, and the gate it passes through: the envelope handed to the agent,
//! edits / deletions / reactions, duplicate suppression, and the owner gate.
//! The message types themselves ([`InboundMsg`] and friends) live in `chat.rs`.
//!
//! `impl Access` here holds only the gate (`gate`, per-channel tool grants); `Access`
//! itself is persisted state and lives in `state.rs`.

use super::{Bridge, Host};
use crate::bridge::turn::Stall;
use crate::agent::tmux::Window;
use crate::agent::{Envelope, SessionId, SpawnReq, WorkerState};
use crate::bridge::state as bridge;
use crate::bridge::state::{Access, LogCtx, ThreadEntry, ThreadKey};
use crate::bridge::{inbound, worker};
use crate::chat::{ChannelKind, InboundMsg, Reaction, slack};

/// ドレインを諦めてでも殺す上限。現行は受信確認のタイムアウトを流用する
/// (RECEIPT_TIMEOUT_MS = 30s)— 新しいつまみを増やさないため。
const DRAIN_TIMEOUT_MS: u64 = 30_000;

/// Slack が再配達に刻む試行回数。slack-morphism 2.24.0 の Socket Mode envelope は
/// `envelope_id` と `accepts_response_payload` しか持たず、Slack が載せる `retry_attempt` は
/// push イベントのコールバックに渡る前に捨てられる(models/socket_mode/mod.rs:59-64)ので
/// 0 固定。同じ (channel, ts) の再到着は dedup が落とすため、stale の判定に残るのは
/// 「この Bridge が聞き始める前に投稿されたか」だけになる。
const RETRY_NUM: u32 = 0;

/// この1通 → エージェントに渡す封筒テキスト。`ts` は**配達時点**の now(`now_ms`)、
/// `thread_ts` は解決済みの根(現行`threadTs || msg.ts` を渡す)。
pub fn envelope(msg: &InboundMsg, root_ts: &str, now_ms: u64) -> String {
    envelope_guarded(msg, root_ts, None, now_ms)
}

/// ループ遮断が立った配達だけ `loop_guard` を載せる(空文字 = 呼ぶ相手が居ない)。
pub fn envelope_guarded(
    msg: &InboundMsg,
    root_ts: &str,
    loop_guard: Option<String>,
    now_ms: u64,
) -> String {
    Envelope {
        loop_guard,
        channel_id: msg.channel.clone(),
        message_id: msg.ts.clone(),
        user: msg.user.clone().unwrap_or_else(|| "unknown".to_string()),
        ts: crate::bridge::state::iso8601(now_ms),
        thread_ts: Some(root_ts.to_string()),
        text: msg.text.clone(),
        // 先読みダウンロードの結果はメッセージが持っている(queue 経由でも持ち越す)
        file_paths: msg.file_paths.clone(),
        file_errors: msg.file_errors.clone(),
    }
    .render()
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

/// 門を通った1通をどう捌くか — 「起こす / 再開する / 渡す / 溜める」の4通りしかない。
///
/// スレッドの記録とワーカーの生死だけで決まる([`Dispatch::decide`])。
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

/// 「同じ出来事を2度処理しない」ための鍵。
///
/// 素の `ts` で足りるのは**本文が届いたとき**だけ。リアクション・書き換え・削除は `ts` が
/// **相手側のメッセージ**を指すので、そのままだと元の配達で覚えた鍵とぶつかって必ず
/// 「重複」で落ちる。種別ごとに名前空間を分ける。
pub(super) fn dedup_key(msg: &InboundMsg) -> String {
    match (&msg.reaction, &msg.edited, &msg.deleted_ts) {
        // 同じ投稿への2つ目の絵文字を「重複」にしない — 絵文字と付/外を混ぜて数える
        (Some(r), _, _) => format!("{}#{}{}", msg.ts, r.emoji, if r.added { "+" } else { "-" }),
        // 書き換えは改訂ごとに1回だけ通す(同じ改訂の再配達は落とす)
        (_, Some(e), _) => format!("{}#edit#{}", msg.ts, e.revision),
        (_, _, Some(_)) => format!("{}#deleted", msg.ts),
        _ => msg.ts.clone(),
    }
}

/// 自分が付けた印(ack の 👀 / 🤖 / 受信確認の 🔄)は、Slack から自分に返ってくる。
///
/// **ワーカーには渡さない。** リアクションの合成本文は「黙っていないで返事を」と促すので、
/// 素直に従うワーカーは ack のたびに返事を増やす。実測(2026-08-02)では**スレッドの1通目**の
/// たびに3通入っていた — 1通目に限るのは、リアクションの通知がスレッドを教えてくれず、
/// 代用した ID がそのときだけ本物のスレッドと一致するため。
pub(super) fn is_own_reaction(msg: &InboundMsg, bot_user_id: Option<&str>) -> bool {
    msg.reaction.is_some() && bot_user_id.is_some_and(|b| msg.user.as_deref() == Some(b))
}

/// **自分が書いたのではない**投稿に付いたリアクションの行き先。
#[derive(PartialEq, Debug)]
pub(super) enum ForeignReaction {
    /// Owner が**自分の依頼**に stop を付けた — 止める(2026-08-02 のユーザー指定)。
    Stop,
    /// それ以外。現行はここを黙って捨てる — 他人のやりとりへの
    /// リアクションが、合成テキストとしてワーカーに流れ込まないように
    Drop,
}

pub(super) fn foreign_reaction(
    r: &Reaction,
    reactor: Option<&str>,
    author: Option<&str>,
    owner: &str,
) -> ForeignReaction {
    let by_owner = !owner.is_empty() && reactor == Some(owner);
    // `author == reactor` = 自分の投稿に自分で付けた。**他人の投稿への stop は効かない** —
    // 誰の仕事を止めるのかが決まらない
    if r.is_stop() && by_owner && author == reactor {
        ForeignReaction::Stop
    } else {
        ForeignReaction::Drop
    }
}

// ── handling inbound messages ──

impl Bridge {
    /// 受信メッセージの添付を先読みダウンロードし、結果を msg に書き戻す。
    /// **決して失敗を投げない** — 1ファイルごとに握りつぶして劣化ノートにする。壊れた/大きすぎる
    /// 1つが、ユーザーの本文や他の添付を巻き添えにしないため(現行)。
    async fn with_attachments(&self, mut msg: InboundMsg, ctx: &LogCtx) -> InboundMsg {
        let (mut paths, mut errors) = (Vec::new(), Vec::new());
        // ここは select ループの腕の中 — 止まった分だけ**全スレッド**の hook / disposition /
        // コマンド / sticky flush が待たされる。だから締切は1ファイルごとではなく
        // **メッセージ全体で1つ**。何枚貼られても最悪 DOWNLOAD_TIMEOUT で必ず抜ける。
        // ponytail: 逐次。並列化(tokio::spawn)は多数添付の待ち時間が実際に痛くなってから
        let deadline = tokio::time::Instant::now() + slack::DOWNLOAD_TIMEOUT;
        for f in &msg.files {
            let dl = self.deps.slack.download_attachment(&f.id, self.deps.dir.path());
            let reason = match tokio::time::timeout_at(deadline, dl).await {
                Ok(Ok(path)) => {
                    paths.push(path);
                    continue;
                }
                Ok(Err(e)) => e,
                // 締切超過。残りのファイルも同じ枝で即座にノートになる
                Err(_) => format!(
                    "timed out ({}s budget for this message's attachments)",
                    slack::DOWNLOAD_TIMEOUT.as_secs()
                ),
            };
            ctx.debug(
                "bridge",
                &format!("attachment download failed file={}: {reason}", f.id),
            );
            // 文言は原文コピー— ワーカーが人に伝える一次資料
            errors.push(format!("[attachment {} not downloaded: {reason}]", f.name));
        }
        ctx.debug("bridge", &format!(
                "slack-events: message accepted message_id={} channel={} type={} files={} dl={} err={}",
                msg.ts,
                msg.channel,
                match msg.channel_kind {
                    inbound::ChannelKind::Dm => "im",
                    inbound::ChannelKind::Channel => "channel",
                },
                msg.files.len(),
                paths.len(),
                errors.len()
            ));
        msg.file_paths = paths;
        msg.file_errors = errors;
        msg
    }

    pub(super) async fn on_inbound(&mut self, msg: &InboundMsg) {
        // 自分の印は自分に返る。**dedup より前**で捨てる — 覚え書きを自分の絵文字で埋めない
        if is_own_reaction(msg, self.bot_user_id.as_deref()) {
            LogCtx::default().debug(
                "bridge",
                &format!(
                    "own reaction on {}:{} — dropped",
                    msg.channel,
                    msg.reaction
                        .as_ref()
                        .map(|r| r.emoji.as_str())
                        .unwrap_or("")
                ),
            );
            return;
        }
        // dedup は gate の前 — 1メッセージが複数イベントで届く
        let dedup_key = dedup_key(msg);
        if self.dedup.seen(&msg.channel, &dedup_key) {
            LogCtx::default().debug(
                "bridge",
                &format!("duplicate event {}:{} — dropped", msg.channel, msg.ts),
            );
            return;
        }
        // コマンドは**ボタン**: 押された時に効くか、さもなくば効かない。
        // Bridge が落ちていた間の投稿は Slack が再接続時にまとめて寄越すので、送り主がとうに
        // 諦めた後の `exit` が届いて二度目の別れを告げる。全コマンド経路の手前で落とす。
        // 普通のリクエストは無傷 — 遅れて配達される方が、本物の仕事を失うよりましだから
        let body = crate::bridge::command::Message::new(&msg.text, self.bot_user_id.as_deref());
        if let Some(cmd) = crate::bridge::command::Cmd::parse(&body, self.deps.agent.as_ref())
            && let Some(stale) = cmd.stale_reason(&msg.ts, self.started_at_ms, RETRY_NUM)
        {
            LogCtx {
                session_id: None,
                thread_key: msg
                    .thread_ts
                    .as_deref()
                    .map(|t| ThreadKey::new(&msg.channel, t)),
            }
            .info(
                "bridge",
                &format!(
                    "slack-events: dropped: command arrived too late to honor ({stale}) — \
                     msg={} channel={} sender={} text={}",
                    msg.ts,
                    msg.channel,
                    msg.user.as_deref().unwrap_or(""),
                    serde_json::to_string(&msg.text.chars().take(40).collect::<String>())
                        .unwrap_or_default()
                ),
            );
            return;
        }
        // Owner がまだ居ないときのサインイン専用の抜け道。gate は
        // owner 空を全部落とすので、**その手前**でなければサインインは永久に始められない。
        // Owner が縛られた後はこの枝ごと素通りする(以後は普通の gate の世界)
        if self.access.owner.is_empty() && !msg.is_bot && self.login_carve_out(msg) {
            return;
        }
        // 編集・削除のイベントは**スレッドを教えてくれない**(ライブラリの型が
        // 入れ子の `thread_ts` を落とす)。まず未応答の台帳で引き、載っていなければ Slack に
        // 1件だけ問い合わせる。**返事し終わった過去のメッセージを直した場合はこちらしか無い** —
        // 台帳から消えているので、聞かないと別スレッド扱いで捨ててしまう
        // リアクションも同じで、**書き手もスレッドも教えてくれない**。元の投稿を1回だけ引く。
        // 省いて ts をスレッドの代用にしていたのが、
        // 「スレッドの1通目にだけ湧く誤配達」の正体だった(2026-08-02)。
        // ponytail: リアクション1つにつき1回問い合わせる。多すぎるなら `item_user` を
        // InboundMsg まで持ち上げて、stop でない他人宛のものを引く前に落とす
        let reacted = match &msg.reaction {
            Some(r) => match self.deps.slack.message_at(&msg.channel, &r.item_ts).await {
                Some(m) => Some(m),
                None => {
                    LogCtx::default().debug(
                        "bridge",
                        &format!(
                            "reaction dropped: could not read {}:{}",
                            msg.channel, r.item_ts
                        ),
                    );
                    return;
                }
            },
            None => None,
        };
        let touched = msg
            .deleted_ts
            .as_deref()
            .or(msg.edited.as_ref().map(|e| e.ts.as_str()));
        let root_ts = match (&reacted, touched) {
            (Some(m), _) => m.thread_ts.clone(),
            (None, None) => msg.thread_ts.clone().unwrap_or_else(|| msg.ts.clone()),
            (None, Some(id)) => match self.ledger.key_of_id(id).and_then(|k| k.split().1) {
                Some(root) => root,
                None => self
                    .deps.slack
                    .parent_thread_of(&msg.channel, id)
                    .await
                    .or_else(|| msg.thread_ts.clone())
                    .unwrap_or_else(|| id.to_string()),
            },
        };
        let key = ThreadKey::new(&msg.channel, &root_ts);
        let ctx = |sid: Option<&str>| LogCtx {
            session_id: sid.map(str::to_string),
            thread_key: Some(key.clone()),
        };

        // チャンネルでは**名指しか、もう動いているスレッド**でないと入れない
        // (DM は名指し扱い)。登録済みかどうかは見ない — 判断材料はこの2つだけ
        let dm = msg.channel_kind == inbound::ChannelKind::Dm;
        let is_mention = dm
            || crate::bridge::command::Message::new(&msg.text, self.bot_user_id.as_deref())
                .mentions_bot();
        let is_active_thread = !dm && self.threads.is_active(&root_ts);
        let context_only = match self.access.gate(msg, is_mention, is_active_thread) {
            GateVerdict::Drop(reason) => {
                ctx(None).info(
                    "bridge",
                    &format!("dropped {}:{} — {reason}", msg.channel, msg.ts),
                );
                return;
            }
            // Owner 以外が動いているスレッドで喋った — 流すが返事は期待しない
            GateVerdict::Context => {
                ctx(None).info(
                    "bridge",
                    &format!(
                        "slack-events: channel deliver: context (non-owner, silent-expected) — \
                         msg={} channel={} sender={}",
                        msg.ts,
                        msg.channel,
                        msg.user.as_deref().unwrap_or("")
                    ),
                );
                true
            }
            GateVerdict::Serve => false,
        };
        let _ = context_only;

        // ユーザーが自分の依頼を消した。**まだ処理中のものだけ**取り消しを伝える:
        // 既に答えた過去のメッセージを消しても、取り下げる仕事はもう無い
        if let Some(deleted) = msg.deleted_ts.clone() {
            self.on_message_deleted(msg, &key, &root_ts, &deleted, &ctx(None));
            return;
        }

        // ユーザーが依頼を書き換えた。**いま処理中のもの**なら中断して新しい方を
        // やり直させ、過去のものなら今の仕事は続けさせて「手が空いたら」と伝える
        if let Some(edited) = msg.edited.clone() {
            self.on_message_edited(msg, &key, &root_ts, &edited.ts, &ctx(None));
            return;
        }

        // **Owner が自分の依頼に付けた stop は止める合図**(2026-08-02 のユーザー指定。現行との
        // 意図的な差 — 現行は付箋に付いたものだけを見る)。付箋を探して押さなくても、
        // 止めたい依頼そのものに ✋ を付ければ止まる。
        //
        // その手前に、現行にあって移植されていなかった検査を置く: **自分が書いた投稿への
        // リアクションだけ拾う**。他人の投稿に付いたものまで拾うと、
        // 無関係なやりとりが合成テキストとしてワーカーに流れ込む
        if let (Some(r), Some(m)) = (&msg.reaction, &reacted)
            && !(m.is_bot || m.user.as_deref() == self.bot_user_id.as_deref())
        {
            if foreign_reaction(
                r,
                msg.user.as_deref(),
                m.user.as_deref(),
                &self.access.owner,
            ) == ForeignReaction::Stop
            {
                ctx(None).info(
                    "bridge",
                    &format!(
                        "slack-events: stop reaction :{}: on the requester's own message {}:{} \
                         (thread {root_ts})",
                        r.emoji, msg.channel, r.item_ts
                    ),
                );
                self.user_stop(msg, &key, &root_ts, &ctx(None));
                return;
            }
            ctx(None).debug(
                "bridge",
                &format!(
                    "reaction dropped: {}:{} is not our message",
                    msg.channel, r.item_ts
                ),
            );
            return;
        }

        // **進捗付箋に付いた** stop 絵文字は stop コマンドと同じ。他のメッセージへの
        // 同じ絵文字はただのリアクションとして下へ流す(付箋を狙って押した時だけ止める)
        if let Some(r) = &msg.reaction
            && r.is_stop()
            && self.sticky.sticky_ts(&key).as_deref() == Some(r.item_ts.as_str())
        {
            ctx(None).info(
                "bridge",
                &format!(
                    "slack-events: stop reaction :{}: on progress sticky {}:{} (thread {root_ts})",
                    r.emoji, msg.channel, r.item_ts
                ),
            );
            self.user_stop(msg, &key, &root_ts, &ctx(None));
            return;
        }

        // ループ遮断(チャンネルのみ)。許可した bot が居る以上、
        // bot 同士が延々と返し合う道が開いている。**人が入るまで**止めるのがここ
        let mut loop_guard: Option<String> = None;
        if !dm {
            if msg.is_bot {
                if self.threads.status_of(&root_ts) == "paused" {
                    ctx(None).info(
                        "bridge",
                        &format!(
                            "slack-events: dropped: thread paused (loop guard) — msg={} \
                             (awaiting human re-engagement)",
                            msg.ts
                        ),
                    );
                    return;
                }
                let streak = self.threads.bump_bot_streak(&root_ts);
                if streak >= bridge::LOOP_LIMIT {
                    self.threads.pause(&root_ts);
                    ctx(None).info(
                        "bridge",
                        &format!(
                            "slack-events: loop-guard: thread paused after {streak} consecutive \
                             bot msgs (≥ LOOP_LIMIT) — msg={}",
                            msg.ts
                        ),
                    );
                    // 呼ぶ相手が居なければ空(印だけ立てる)
                    loop_guard = Some(match self.access.owner.as_str() {
                        "" => String::new(),
                        o => format!("<@{o}>"),
                    });
                }
                if let Err(e) = self.threads.save() {
                    ctx(None).error("bridge", &format!("threads.json save failed: {e}"));
                }
            } else if self.threads.reset_bot_streak(&root_ts) {
                ctx(None).info(
                    "bridge",
                    "slack-events: thread resumed — human re-engaged (was paused)",
                );
                if let Err(e) = self.threads.save() {
                    ctx(None).error("bridge", &format!("threads.json save failed: {e}"));
                }
            }
        }

        // コマンドは Bridge が自分で答えて**ここで終わる** — ワーカーには決して渡さない。
        // ack より前に返すのは、コマンドに 👀 を付けないため
        if self.handle_command(msg, &key, &root_ts).await {
            // Bridge が**このスレッドで喋った**。status / usage のようにワーカーを起こさない
            // コマンドでも、以後そのスレッドは「動いているスレッド」— 続きはメンション無しで
            // 受ける(ユーザー指定。セッションの有無とは切り離す)
            self.mark_thread_seen(&msg.channel, &root_ts, &ctx(None));
            return;
        }

        // usage 上限中は新規の依頼を受けない。キューイングもしない。
        // コマンドの**後ろ**なのは現行どおり — 上限中でも stop/restart/logout は効く
        if self.deps.clock.now_ms() < self.limited_until_ms {
            let text = super::turn::limited_notice(self.limited_until_ms);
            self.post(&msg.channel, &root_ts, text, &key);
            return;
        }

        // 「見た」の合図をすぐ返し、未応答として台帳に載せる(🤖 への切替は user_prompt hook)
        self.react(
            &msg.channel,
            &msg.ts,
            slack::Api::ack_emoji(&self.access),
            &key,
        )
        .await;
        self.ledger.track(&key, &msg.ts);

        // 添付は**受信した時に**落とし、封筒には
        // ローカルパスを載せる。ワーカーは Read するだけでよく、引き金のメッセージについて
        // download_attachment を呼ぶ必要が無い(呼ぶための file_id も知らされない)
        let msg = &self.with_attachments(msg.clone(), &ctx(None)).await;

        let entry = self.threads.get(&root_ts).cloned();
        let is_new = entry.is_none();
        let sid = entry.as_ref().and_then(|e| e.agent_id.as_deref());
        // 窓名は session_id だけで決まる。まだセッションの無いスレッドは
        // 窓も無い — worker_state は sid 無しで Absent を返すので空文字で害が無い
        let window = sid
            .map(|s| SessionId::from(s).window_name())
            .unwrap_or_default();
        let state = self.workers.state_of(entry.as_ref(), &window, self.deps.agent.as_ref());
        // 配達先は window_id を優先(窓名は改名されうる)
        let target = entry
            .as_ref()
            .and_then(|e| e.agent_id.as_deref())
            .and_then(|sid| self.workers.warm(sid))
            .and_then(|h| h.window_id.clone())
            .unwrap_or_else(|| window.clone());
        let envelope = envelope_guarded(msg, &root_ts, loop_guard, self.deps.clock.now_ms());
        // ターン失敗の再送に要る。`track` は ack の直後(封筒がまだ無い時点)なので、
        // 封筒ができた**ここ**で台帳に預ける — これより下の配達経路は全部この後ろ
        self.ledger.remember_envelope(&key, &msg.ts, &envelope);
        // pwd と同じ解決を通す(ルート無し → Home フォールバック)
        let (cwd, _) = self.access.repo_path(&msg.channel, &Host::home());

        // 新規スレッドは cold spawn の前に在庫を覗く。無ければ下の decide にそのまま落ちる
        if bridge::Threads::should_claim_pool(entry.as_ref()) {
            let key_pool = bridge::PoolKey::of_cwd(&cwd);
            if let Some(claimed) = self.claim_pool_worker(&key_pool) {
                // SpawnNew と同じ話題の取り方(trim して頭 60 文字 — 文字単位)
                let topic: String = msg.text.trim().chars().take(60).collect();
                let assigned = self.assign_pool_worker(
                    claimed,
                    &root_ts,
                    &msg.channel,
                    &cwd,
                    &envelope,
                    &topic,
                    &msg.ts,
                    &key,
                    &ctx(None),
                );
                // Dispatch::Deliver の失敗と同じ扱い: 取っておいて、待っている人に言う
                if let Err(e) = assigned {
                    self.pending
                        .entry(root_ts.clone())
                        .or_default()
                        .push(msg.clone());
                    self.post_error_frame(
                        msg.channel.clone(),
                        root_ts.clone(),
                        crate::t!("Couldn't hand this to the agent: {e}", "エージェントに渡せませんでした: {e}"),
                    );
                }
                return;
            }
        }

        // 配達できたら沈黙見張りを起こす。`entry` の借用が生きているうちは `&mut self` を
        // 取れないので、match の外まで持ち出す
        let mut delivered = false;
        match Dispatch::decide(entry.as_ref(), state) {
            Dispatch::SpawnNew => {
                let sid = SessionId::new().as_str().to_string();
                let mut e = entry.unwrap_or_default();
                e.agent_id = Some(sid.clone());
                e.channel_id = Some(msg.channel.clone());
                e.repo_path = Some(cwd.clone());
                // status のリンク文字列になる話題。現行と同じく trim して頭 60 文字
                // (字数は文字単位で数える — 日本語を割らないため)
                let topic: String = msg.text.trim().chars().take(60).collect();
                e.topic = (!topic.is_empty()).then_some(topic);
                self.threads.upsert(&root_ts, e);
                if let Err(err) = self.threads.save() {
                    ctx(Some(&sid)).error("bridge", &format!("threads.json save failed: {err}"));
                }
                let Some(mcp) = self.write_mcp(&sid, &ctx(Some(&sid))) else {
                    return;
                };
                let req = SpawnReq {
                    session_id: sid.clone().into(),
                    cwd: cwd.clone(),
                    prompt: Some(envelope.clone()),
                    resume_from: None,
                    // 窓名は**今決まった** session_id から作る(スレッドの窓名は存在しない)
                    window: SessionId::from(sid.clone()).window_name(),
                    state,
                    hooks_file: self.hooks_file.clone(),
                    mcp_config: mcp,
                };
                self.spawn_worker(&req, &key, &ctx(Some(&sid)), &sid);
            }
            Dispatch::SpawnResume(sid) => {
                let Some(mcp) = self.write_mcp(&sid, &ctx(Some(&sid))) else {
                    return;
                };
                let req = SpawnReq {
                    session_id: sid.clone().into(),
                    cwd: cwd.clone(),
                    prompt: Some(envelope.clone()),
                    // 継続は同じ session_id を `--resume` で開き直す
                    resume_from: Some(sid.clone().into()),
                    window: SessionId::from(sid.clone()).window_name(),
                    state,
                    hooks_file: self.hooks_file.clone(),
                    mcp_config: mcp,
                };
                self.spawn_worker(&req, &key, &ctx(Some(&sid)), &sid);
            }
            Dispatch::Deliver => {
                let sid = entry.as_ref().and_then(|e| e.agent_id.clone());
                // 暖まっているワーカーには prefix 抜きの素の封筒(push と同じ形)
                match self.deps.agent.deliver(&Window::of(&target), &envelope) {
                    // 1メッセージ1行の配達記録(現行)
                    Ok(()) => {
                        ctx(sid.as_deref()).info(
                            "bridge",
                            &format!(
                                "slack-events: deliver message_id={} chat={} thread={root_ts} \
                                 new={is_new} -> worker {}",
                                msg.ts, msg.channel, msg.channel
                            ),
                        );
                        delivered = true;
                    }
                    // 配達できなかった = このメッセージは**誰にも届いていない**。ログだけだと
                    // 👀 が付いたまま無言で終わる(2026-08-18: モーダルで詰まった2スレッドが
                    // それで18分沈黙した)ので、待っている人に1本言う。
                    // ponytail: 詰まっている間は1通ごとに1本出る。うるさければスレッド単位で
                    // 抑制する(`cannot_deliver` の cooldown と同じ形)
                    Err(e) => {
                        ctx(sid.as_deref()).error(
                            "bridge",
                            &format!("delivery failed: {e} — queued for retry"),
                        );
                        // 取っておかないと、覆いが晴れても**このメッセージは二度と届かない**
                        // (再配達の経路が無い理由は [`Bridge::retry_pending`])
                        self.pending
                            .entry(root_ts.clone())
                            .or_default()
                            .push(msg.clone());
                        self.post_error_frame(
                            msg.channel.clone(),
                            root_ts.clone(),
                            crate::t!("Couldn't hand this to the agent: {e}", "エージェントに渡せませんでした: {e}"),
                        );
                    }
                }
            }
            Dispatch::Queue => {
                self.pending
                    .entry(root_ts.clone())
                    .or_default()
                    .push(msg.clone());
                ctx(None).info("bridge", "worker still starting — queued");
            }
        }
        // 渡した = ここから先は返事待ち。shimmer で「受け取って動いている」を出す
        // 無音が続けば見張りが「思考中」に差し替える。
        // 見張りの張り直しと同じ1回の送信で出すので、既に出ている shimmer と喧嘩しない
        // リアクションは軽い合図 — 👀 だけが活動の印で、shimmer は立てない
        // (ユーザー判断)。
        // **他人宛の発言でも立てない** — 動いているスレッドには `<@誰か> おーい` も流れてくるが、
        // それはこちらへの用件ではない。出すと「返事を書いている」という嘘になる
        let for_someone_else = !is_mention
            && crate::bridge::command::Message::new(&msg.text, self.bot_user_id.as_deref())
                .mentions_someone_else();
        if delivered && msg.reaction.is_none() && !for_someone_else {
            self.touch_thread(&key, slack::TYPING_STATUS);
        }
    }

    /// 走っているターンを ESC で断ち切る。生きているのは
    /// 「ワーカーのプロセスが在る」かつ「未応答が1件以上ある」ときだけ。
    /// 書き換えられた依頼をワーカーに渡す。分かれ道は1つだけ:
    /// **いま処理中の依頼が書き換わったのか**(= 未応答に載っていて、かつスレッドで最新)。
    ///
    /// - 処理中 → 付箋に「Interrupted by user.」を足し(既に出ているときだけ)、新しい本文を
    ///   押し込んでから ESC。古い言い回しのためにやっていた作業は捨てさせる
    /// - 過去の依頼 → **中断しない**。「手が空いてから直した依頼をやって」と伝えるだけ
    ///   (過去のメッセージを消したときに今の仕事へ手を出さないのと同じ扱い)
    ///
    /// ワーカーの居ないスレッドは best-effort で捨てる(中断する相手も渡す先も無い)。
    fn on_message_edited(
        &mut self,
        msg: &InboundMsg,
        key: &ThreadKey,
        root_ts: &str,
        edited_ts: &str,
        ctx: &LogCtx,
    ) {
        let Some(sid) = self.threads.get(root_ts).and_then(|e| e.agent_id.clone()) else {
            ctx.info(
                "bridge",
                &format!(
                    "message_changed chan={} ts={edited_ts} -> no active worker thread, dropped \
                     (best-effort)",
                    msg.channel
                ),
            );
            return;
        };
        // 「いま処理中」= 未応答に載っていて、かつスレッドの未応答の中で最新
        // (Slack の id は秒.マイクロ秒なので数値比較が新しさの比較になる)
        let pending = self.ledger.pending(key);
        let num = |id: &str| id.parse::<f64>().unwrap_or(0.0);
        let is_current = pending.iter().any(|id| id == edited_ts)
            && pending.iter().all(|id| num(id) <= num(edited_ts));

        let mut notice = msg.clone();
        notice.text = crate::chat::edit_notice(
            edited_ts,
            &msg.text,
            !msg.files.is_empty(),
            is_current,
        );
        let target = self
            .workers
            .warm(&sid)
            .and_then(|h| h.window_id.clone())
            .unwrap_or_else(|| SessionId::from(sid.clone()).window_name());
        // 付箋が出ているときだけ中断の締め行を足す(まだ何も出ていないなら足さない)
        if is_current && self.sticky.sticky_ts(key).is_some() {
            self.sticky.on_interrupted(key);
        }
        ctx.info(
            "bridge",
            &format!(
                "message_changed chan={} ts={edited_ts} thread={root_ts} -> {}",
                msg.channel,
                if is_current {
                    "CURRENT work edited: interrupt + redo with new text"
                } else {
                    "past message revised: notify only (current work untouched)"
                }
            ),
        );
        let envelope = envelope(&notice, root_ts, self.deps.clock.now_ms());
        if let Err(e) = self.deps.agent.deliver(&Window::of(&target), &envelope) {
            ctx.error("bridge", &format!("message_changed delivery failed: {e}"));
            return;
        }
        // 押し込んでから切る(削除と同じ順序)— 逆にすると、切られたワーカーが
        // 書き換えを知らないまま次の入力を待つ
        if is_current {
            self.user_stop(msg, key, root_ts, ctx);
        }
        // 書き換えられた依頼は**もう一度未応答**。載せ直しは user_stop の後(先に載せると
        // dispose が巻き込む)。これが無いと沈黙見張りが「決着済み」と見て即畳まれ、
        // ワーカーが作り直している間ずっと shimmer も「考え中」も出ない。
        // 封筒も差し替える — 覚えているのは書き換え**前**の本文で、ターン失敗の再送が
        // 古い言い回しを送り直してしまう
        self.ledger.track(key, edited_ts);
        self.ledger.remember_envelope(key, edited_ts, &envelope);
        self.touch_thread(key, slack::TYPING_STATUS);
    }

    /// 降りる前に、**まだ渡していないメッセージを threads.json へ逃がす**。
    /// これが無いと、起動待ちで queue に積まれた依頼は落ちた瞬間に消える。
    ///
    /// 逃がすのは queue の中身だけ(台帳の未応答は**渡し済み**で、ワーカーが抱えている)。
    pub(super) fn flush_pending_to_disk(&mut self, ctx: &LogCtx) {
        let queued: Vec<(String, Vec<InboundMsg>)> = self.pending.drain().collect();
        let mut n = 0usize;
        for (root_ts, msgs) in queued {
            for msg in &msgs {
                self.threads.enqueue_pending(&root_ts, &msg.channel, msg);
                n += 1;
            }
            if !msgs.is_empty() {
                ctx.info(
                    "bridge",
                    &format!(
                        "re-enqueued {} undelivered message(s) to the durable queue thread={root_ts}",
                        msgs.len()
                    ),
                );
            }
        }
        if n > 0
            && let Err(e) = self.threads.save()
        {
            ctx.error("bridge", &format!("threads.json save failed: {e}"));
        }
    }

    /// 逃がしてあった分を配り直す(起動時に1回)。**普通の受信と同じ道**に流すので、
    /// ワーカーが生きていれば押し込み、居なければ起こして渡す(= 中断したスレッドの再開)。
    pub(super) async fn resume_pending_from_disk(&mut self) {
        for root_ts in self.threads.threads_with_pending() {
            let msgs = self.threads.drain_pending(&root_ts);
            if msgs.is_empty() {
                continue;
            }
            LogCtx {
                session_id: None,
                thread_key: None,
            }
            .info(
                "bridge",
                &format!(
                    "resuming {} message(s) left undelivered by the previous bridge process \
                     thread={root_ts}",
                    msgs.len()
                ),
            );
            for msg in msgs {
                self.on_inbound(&msg).await;
            }
        }
        if let Err(e) = self.threads.save() {
            LogCtx::default().error("bridge", &format!("threads.json save failed: {e}"));
        }
    }

    /// 消された依頼をワーカーに取り下げさせる。
    ///
    /// 順序は Bun と同じ: **先に取り消しの通知を押し込み**、
    /// そのあと ESC で走っているターンを切る。逆にすると、切られたワーカーが取り消しを
    /// 知らないまま次の入力を待つ。
    ///
    /// 最後に台帳から落とす — 消した本人はもう返事を待っていないので、ここを残すと
    /// 「応答待ち」の見張りが取り消しの処理中ずっと居座る(塞いだ穴)。
    fn on_message_deleted(
        &mut self,
        msg: &InboundMsg,
        key: &ThreadKey,
        root_ts: &str,
        deleted: &str,
        ctx: &LogCtx,
    ) {
        if !self.ledger.pending(key).iter().any(|id| id == deleted) {
            ctx.info(
                "bridge",
                &format!(
                    "message_deleted chan={} ts={deleted} -> already answered / not in-flight, \
                     no action",
                    msg.channel
                ),
            );
            return;
        }
        let sid = self.threads.get(root_ts).and_then(|e| e.agent_id.clone());
        let target = sid.as_deref().map(|s| {
            self.workers
                .warm(s)
                .and_then(|h| h.window_id.clone())
                .unwrap_or_else(|| SessionId::from(s.to_string()).window_name())
        });
        match target {
            Some(w) => {
                let envelope = envelope(msg, root_ts, self.deps.clock.now_ms());
                match self.deps.agent.deliver(&Window::of(&w), &envelope) {
                    Ok(()) => ctx.info(
                        "bridge",
                        &format!(
                            "message_deleted chan={} ts={deleted} thread={root_ts} \
                             -> interrupt notify",
                            msg.channel
                        ),
                    ),
                    Err(e) => ctx.error("bridge", &format!("message_deleted delivery failed: {e}")),
                }
                self.user_stop(msg, key, root_ts, ctx);
            }
            None => ctx.info(
                "bridge",
                &format!(
                    "message_deleted chan={} ts={deleted} -> no active worker thread, dropped \
                     (best-effort)",
                    msg.channel
                ),
            ),
        }
        // 取り消された依頼は未応答ではない(見張りの対象から外す)
        self.ledger.disposed(key, &[deleted.to_string()]);
    }

    /// 終了予約を積む。同じスレッドの2本目は積まない — 積むと同じ窓を二度殺しに行き、
    /// 二度目の別れを告げる(現行の in-flight guard 相当)。
    pub(super) fn push_drain(
        &mut self,
        key: &ThreadKey,
        session_id: String,
        farewell: Option<(String, String)>,
        thinking: Option<slack::Thinking>,
        ctx: &LogCtx,
    ) {
        if self.workers.is_draining(key) {
            ctx.debug(
                "bridge",
                &format!("exit: already draining key={key} — ignoring the second request"),
            );
            return;
        }
        let pending = self.ledger.pending(key);
        if !pending.is_empty() {
            ctx.info(
                "bridge",
                &format!(
                    "exit: waiting for in-flight reply before terminating key={key} pending=[{}]",
                    pending.join(",")
                ),
            );
        }
        self.workers.push_drain(worker::DrainJob {
            key: key.clone(),
            session_id,
            deadline_ms: self.deps.clock.now_ms() + DRAIN_TIMEOUT_MS,
            farewell,
            thinking,
        });
    }

    /// 満期の来た終了予約を実行する。待ちの終わりは「未応答が空になった」か 30 秒のどちらか早い方
    /// (見るのは**生死ではなく未応答**なので、
    /// ドレイン中の一瞬の respawn 隙間で早まって殺すことがない)。
    pub(super) async fn run_drains(&mut self) {
        let now = self.deps.clock.now_ms();
        // ponytail: select 直列・1 tick 1本・最悪 ~3秒(SIGTERM 猶予 + SIGKILL 待ち)。この間
        // main ループは止まるので、Stop hook の 5 秒枠(endpoints.rs STOP_DECISION_CAP)を割らない
        // ように**同一 tick で2本殺さない** — 残りは次の tick(500ms 後)。並行 kill が要るなら
        // kill を spawn に逃がす(hooked 除去の順序に注意)
        let ledger = &self.ledger;
        let Some(i) = self
            .workers
            .due_drain(now, |key| ledger.pending(key).is_empty())
        else {
            return;
        };
        let job = self.workers.take_drain(i);
        let ctx = LogCtx {
            session_id: Some(job.session_id.clone()),
            thread_key: Some(job.key.clone()),
        };
        let remaining = self.ledger.pending(&job.key);
        if remaining.is_empty() {
            ctx.info(
                "bridge",
                &format!(
                    "exit: in-flight reply drained — terminating key={}",
                    job.key
                ),
            );
        } else {
            ctx.info(
                "bridge",
                &format!(
                    "exit: receipt timeout reached with undisposed=[{}] — terminating anyway \
                     key={}",
                    remaining.join(","),
                    job.key
                ),
            );
        }
        self.terminate(&job.key, Some(&job.session_id), job.farewell)
            .await;
        // 待っていたものが終わった**後**に消す(Drop = クリア)。scope 終端の暗黙 Drop でも
        // 同じ順序になるが、ここが消えるタイミングだと読めるように明示しておく
        drop(job.thinking);
    }

    /// このスレッドで一度でも喋ったことを threads.json に残す。**セッションは紐付けない** —
    /// 印だけ(`agent_id` 無し = ワーカーはまだ居ない)。次にここへ来たメッセージは
    /// 「動いているスレッドの続き」として、名指し無しでも通る。既にエントリがあれば触らない。
    fn mark_thread_seen(&mut self, channel: &str, root_ts: &str, ctx: &LogCtx) {
        if self.threads.get(root_ts).is_some() {
            return;
        }
        self.threads.upsert(
            root_ts,
            bridge::ThreadEntry {
                channel_id: Some(channel.to_string()),
                ..Default::default()
            },
        );
        if let Err(e) = self.threads.save() {
            ctx.error("bridge", &format!("threads.json save failed: {e}"));
        }
    }

    /// 前の Bridge が答えを待っていたスレッドを拾い直す(起動時に1回)。
    ///
    /// 台帳は pending.json に落ちているが、**そのまま全部載せない**。ワーカーが死んでいる
    /// スレッドの未応答は誰も応えないので、載せると沈黙の見張りが永久に居座る。在庫の
    /// [`Self::restore_pools`] と同じ論法で、tmux に実体が残っている分だけを残す。
    ///
    /// **生き残りを数え上げに行かない** — 掛け金は元から空だし(= 継承ワーカーはそのまま
    /// 渡してよい)、MCP の印は最初のツール呼び出しが事実として立てる(`"mcp_ready"` hook)。
    /// 起動時に threads.json を全件 tmux に問い合わせても、その2つは何も早くならない。
    ///
    /// 載せ直したら見張りも張り直す(`touch_thread` の空文字 = 何も出さずに時計だけ始める)。
    /// これが無いと台帳だけ復活して、無音になっても「考え中」が出ない。
    pub(super) fn restore_pending(&mut self, ctx: &LogCtx) {
        let all = self.ledger.pending_keys();
        let alive = self.threads.surviving(&all, |sid| {
            let name = SessionId::from(sid.to_string()).window_name();
            self.deps.agent.pid_of(None, &name).is_some()
        });
        self.ledger.retain_keys(&alive);
        if all.is_empty() {
            return;
        }
        ctx.info(
            "bridge",
            &format!(
                "restored {}/{} thread(s) with undisposed messages from the previous bridge process",
                alive.len(),
                all.len()
            ),
        );
        for key in alive {
            self.touch_thread(&key, "");
        }
    }

    /// ack の付与は**待つ** — 投げっぱなしだと受領時の flip(👀 remove → 🤖 add)が
    /// 飛行中の add を追い越し、👀 が後から付き直して残る。失敗は握る(best-effort)。
    async fn react(&self, channel: &str, ts: &str, emoji: &str, key: &ThreadKey) {
        if let Err(e) = self.deps.slack.add_reaction(channel, ts, emoji).await {
            LogCtx {
                session_id: None,
                thread_key: Some(key.clone()),
            }
            .debug("bridge", &format!("ack reaction '{emoji}' failed: {e}"));
        }
    }

    /// 活動があった — 沈黙タイマーを張り直し、ステータスを `status` に差し替える
    /// (現行の `armWatchdog` + `clearStall`)。冪等。
    ///
    /// `status` は「活動の結果いま出したいもの」: 配達は `is typing…`、hook は `""`(解除)。
    /// **差し替えは1回の送信にまとめる** — 「出す」と「消す」を別々に投げると順序が無いので
    /// 互いを打ち消す(既に `is thinking…` が出ているスレッドへの追撃配達で実際に起きる)。
    pub(super) fn touch_thread(&mut self, key: &ThreadKey, status: &str) {
        // 未応答が1件も無いスレッドに見張りは要らない — 立てても撃たないうえ、次の tick が
        // 「決着済み」として畳むだけ(答えた後も流れてくる hook で毎回それをやるのは無駄)
        if !self.stall.contains_key(key) && self.ledger.pending(key).is_empty() {
            return;
        }
        let e = match self.stall.get_mut(key) {
            Some(e) => e,
            None => {
                let (channel, thread) = key.split();
                // API は thread_ts 必須 — 根の引けない鍵は見張らない
                let Some(ts) = thread else { return };
                // **黙って**作る(空文字 = 初回送信なし)。`status` を渡すとここで1回送り、
                // 下の共通処理でもう1回同じものを送ってしまう。送信口は下の1箇所に統一する
                self.stall.entry(key.clone()).or_insert(Stall {
                    last_activity_ms: 0,
                    shown: false,
                    awaiting_perm: false,
                    thinking: slack::Thinking::new(self.deps.slack.clone(), &channel, &ts, ""),
                })
            }
        };
        e.last_activity_ms = self.deps.clock.now_ms();
        // 送るのは**見た目が変わるときだけ**。出ていた見張りを解除する(shown)か、
        // 新しく何かを出す(status 非空)か。turn 中の hook 連打は既に何も出ていなければ無送信
        let was_shown = std::mem::replace(&mut e.shown, false);
        if was_shown || !status.is_empty() {
            e.thinking.set(status);
        }
    }

    /// 受領した id を milestone に出し、👀 を 🤖 に替える(投げっぱなし)。
    pub(super) fn received(&mut self, key: &ThreadKey, ids: Vec<String>, ctx: &LogCtx) {
        if ids.is_empty() {
            return;
        }
        self.milestone(Some(key), "received", ctx);
        let (channel, _) = key.split();
        let ack = slack::Api::ack_emoji(&self.access).to_string();
        for id in ids {
            let (api, channel, ack) = (self.deps.slack.clone(), channel.clone(), ack.clone());
            tokio::spawn(async move {
                api.flip_to_received(&channel, &id, &ack).await;
            });
        }
    }

    /// queue に残っている分を tick ごとに押し直す。**Rust 版には受領タイムアウトの再配達が
    /// 無い**ので、これが無いと詰まりから自力で戻れない — 既存の再送経路は2つとも hook 起点で、
    /// どちらもワーカーが止まっている間は永久に飛ばない:
    /// [`Bridge::retry_turn_failure`] は StopFailure(ターンが始まらないので出ない)、
    /// [`Bridge::flush_queued`] は user_prompt(何も送信できていないので出ない)。
    ///
    /// 押し直しが安全なのは [`crate::agent::claude::Claude::deliver`] が**打つ前に**入力欄を
    /// 見るから — 覆われていれば1文字も送らずに Err を返す。だから「人がダイアログに答えた
    /// 次の tick で流れる」が、余計な打鍵なしで成立する。2026-08-18 の18分沈黙への答え。
    pub(super) fn retry_pending(&mut self, ctx: &LogCtx) {
        let roots: Vec<String> = self.pending.keys().cloned().collect();
        for root_ts in roots {
            let entry = self.threads.get(&root_ts).cloned();
            let Some(sid) = entry.as_ref().and_then(|e| e.agent_id.clone()) else {
                continue;
            };
            let window = SessionId::from(sid.clone()).window_name();
            // 聞こえる相手にだけ押す。Starting の分は user_prompt が流す道が生きている
            if self.workers.state_of(entry.as_ref(), &window, self.deps.agent.as_ref())
                != crate::agent::WorkerState::Ready
            {
                continue;
            }
            let key = entry
                .as_ref()
                .and_then(|e| e.channel_id.as_ref())
                .map(|ch| ThreadKey::new(ch, &root_ts));
            self.flush_queued(&sid, Some(root_ts), key.as_ref(), ctx);
        }
    }

    /// Ready になった瞬間に、待たせていた分を押し込む。
    pub(super) fn flush_queued(
        &mut self,
        session_id: &str,
        owning: Option<String>,
        key: Option<&ThreadKey>,
        ctx: &LogCtx,
    ) {
        let Some(root_ts) = owning else { return };
        let Some(mut queued) = self.pending.remove(&root_ts) else {
            return;
        };
        let window = self
            .workers
            .window_of(session_id)
            .unwrap_or_else(|| SessionId::from(session_id.to_string()).window_name());
        let mut delivered = false;
        let mut i = 0;
        while i < queued.len() {
            let text = envelope(&queued[i], &root_ts, self.deps.clock.now_ms());
            match self.deps.agent.deliver(&Window::of(&window), &text) {
                Ok(()) => {
                    ctx.info("bridge", "flushed queued message");
                    delivered = true;
                    i += 1;
                }
                // 渡せなかった分は queue に**戻す**(捨てると二度と届かない)。同じ窓宛なので
                // 1通目が詰まれば残りも詰まる — そこで止めて、順序のまま先頭に返す
                Err(e) => {
                    let back: Vec<InboundMsg> = queued.split_off(i);
                    ctx.error(
                        "bridge",
                        &format!("flush failed: {e} — {} message(s) re-queued", back.len()),
                    );
                    self.pending
                        .entry(root_ts.clone())
                        .or_default()
                        .splice(0..0, back);
                    break;
                }
            }
        }
        // 押し込みも配達 — 渡した瞬間に shimmer を出す(直接配達の 446-448 と同じ)。
        // ここを落とすと、起動待ちで queue に積まれたメッセージは最後まで無印のままになる
        if delivered {
            if let Some(key) = key {
                self.touch_thread(key, slack::TYPING_STATUS);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

}
