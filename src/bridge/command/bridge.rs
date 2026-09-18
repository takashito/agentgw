//! Commands aimed at agentgw itself: status / pwd and the Owner commands
//! (warm / set-home / allow-bot / remove-bot).

use super::PwdMode;
use crate::agent::SessionId;
use crate::bridge::inbound::InboundMsg;
use crate::bridge::state::{self as bridge, LogCtx, ThreadKey};
use crate::bridge::{Bridge, Host};
use crate::chat::slack;
use std::collections::HashMap;

impl Bridge {
    /// `status`。**本当に動いている**
    /// ワーカーだけを載せる — 生死は tmux の claude pid が唯一の答え(記憶ではなく実物)。
    /// Slack への問い合わせ(permalink / チャンネル名)は select ループの外でやる。
    pub(super) fn user_status(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let mut threads: Vec<crate::bridge::render::StatusThread> = Vec::new();
        // ponytail: スレッド1本につき tmux list-windows 1回(claude_pid_of が都度呼ぶ)。
        // dev のスレッド数では十分 — 数百本に育ったら現行と同じく
        // 窓の列挙を1回にまとめ、生きた窓名の集合で先に篩う
        for (tts, e) in &self.threads.entries {
            let (Some(channel_id), Some(sid)) = (e.channel_id.clone(), e.agent_id.as_deref())
            else {
                continue;
            };
            let h = self.workers.warm(sid);
            let window = SessionId::from(sid.to_string()).window_name();
            let alive = self
                .deps.agent
                .pid_of(h.and_then(|h| h.window_id.as_deref()), &window);
            if alive.is_none() {
                continue;
            }
            // idle の基準は transcript の mtime = 最後に**本当に働いた**時刻。
            // hook がまだ来ていない継承ワーカーは session id から探す
            let remembered = h.and_then(|h| h.transcript_path.clone());
            let last_activity_ms = self
                .deps.agent
                .last_activity_ms(remembered.as_deref(), sid)
                .unwrap_or(0);
            threads.push(crate::bridge::render::StatusThread {
                channel_id,
                thread_ts: tts.clone(),
                last_activity_ms,
                repo_path: e.repo_path.clone(),
                permalink: None,
                topic: e.topic.clone(),
                channel_name: None,
            });
        }
        ctx.debug(
            "bridge",
            &format!(
                "status: {} live thread(s) of {} recorded",
                threads.len(),
                self.threads.entries.len()
            ),
        );
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        // $HOME は `~` 畳みのため(`process.env.HOME`)。Host::home() は
        // 「ルート未設定のワーカーが立つ場所」で別物なので、ここで混ぜない
        let home = std::env::var("HOME").unwrap_or_default();
        // `self` は 'static な spawn へ持ち越せない — 在庫の cwd はここで取り出して move で渡す
        // 数えるのは**使える**在庫だけ(起動途中は「まだ無い」— 諦めた枠はそもそも消えている)
        let pools: Vec<String> = self
            .workers
            .pool_summary()
            .into_iter()
            .filter(|(_, ready)| *ready)
            .map(|(cwd, _)| cwd)
            .collect();
        // 集計は permalink とチャンネル名の解決でスレッド数ぶん Slack を叩く — 待つ間の shimmer
        let thinking =
            slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, &slack::Status::Gathering.text());
        let (slack, clock) = (self.deps.slack.clone(), self.deps.clock.clone());
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア。どの経路で抜けても消える
            let ctx = LogCtx {
                session_id: None,
                thread_key: Some(key.clone()),
            };
            // 名前はチャンネルごとに1回だけ解決する(同じ会話に何本もスレッドが立つ)
            let mut names: HashMap<String, Option<String>> = HashMap::new();
            for t in &mut threads {
                t.permalink = match slack.get_permalink(&t.channel_id, &t.thread_ts).await {
                    Ok(p) => Some(p),
                    Err(e) => {
                        ctx.debug(
                            "bridge",
                            &format!(
                                "status: getPermalink failed for {}/{}: {e}",
                                t.channel_id, t.thread_ts
                            ),
                        );
                        None
                    }
                };
                if !names.contains_key(&t.channel_id) {
                    let n = slack.channel_display_name(&t.channel_id).await;
                    names.insert(t.channel_id.clone(), n);
                }
                t.channel_name = names[&t.channel_id].clone();
            }
            let report = crate::bridge::render::StatusReport {
                bridge_version: env!("CARGO_PKG_VERSION").to_string(),
                now_ms: clock.now_ms(),
                home,
                // 繋がり方は1つしかない(Remote は作らない)
                mode: "local".to_string(),
                threads,
                pools,
            }
            .render();
            api.post_now(&channel, &root, report, &key).await;
        });
    }

    /// `pwd` の3形の答え。解決は spawn と同じ resolve_repo_path を
    /// 通すので、表示したパスは必ずワーカーが実際に立つ場所。
    pub(super) fn pwd_answer(
        &mut self,
        msg: &InboundMsg,
        mode: PwdMode,
        dm: bool,
        root_ts: &str,
        ctx: &LogCtx,
    ) -> String {
        let home = Host::home();
        let entry = |access: &bridge::Access, ch: &str| {
            let (repo_path, is_fallback) = access.repo_path(ch, &home);
            crate::bridge::render::PwdEntry {
                channel_id: ch.to_string(),
                repo_path,
                label: access.routes.get(ch).and_then(|r| r.label.clone()),
                is_fallback,
            }
        };
        match mode {
            PwdMode::Current => {
                entry(&self.access, &msg.channel).render(&crate::t!("Project directory for this channel", "このチャンネルの作業ディレクトリ"))
            }
            PwdMode::All => {
                let all: Vec<_> = self
                    .access
                    .routes
                    .keys()
                    .map(|ch| entry(&self.access, ch))
                    .collect();
                crate::bridge::render::PwdEntry::render_all(&all, &home)
            }
            // DM にはルートが無い(そのワーカーは常に Home で立つ)ので、黙って記録する
            // 代わりにそう言う
            PwdMode::Set(_) if dm || !crate::bridge::command::SlackId::is_channel(&msg.channel) => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: pwd set refused — not a channel (dm={dm}) msg={}",
                        msg.ts
                    ),
                );
                crate::bridge::render::Notice::PwdDmSetRefusal.render()
            }
            PwdMode::Set(path) => {
                let op = bridge::AccessOp::SetRepo {
                    channel: msg.channel.clone(),
                    path: path.clone(),
                };
                match self.access.apply(op) {
                    Ok((access, message, warnings)) => {
                        self.adopt_access(access, ctx);
                        ctx.info(
                            "bridge",
                            &format!(
                                "slack-events: pwd set channel={} path={path} msg={}",
                                msg.channel, msg.ts
                            ),
                        );
                        crate::bridge::render::Notice::with_warnings(&message, &warnings)
                    }
                    // 検証に落ちたパスは、そのエラー文そのものが Owner の読むもの
                    Err(e) => {
                        ctx.error(
                            "bridge",
                            &format!(
                                "slack-events: pwd set failed for {}:{root_ts}: {e}",
                                msg.channel
                            ),
                        );
                        e
                    }
                }
            }
        }
    }

    /// アクセス管理の verb。これは MCP ツールではない
    /// Owner の素のメッセージを配達**前に**Bridge が実行するので、prompt injection の面がゼロ。
    pub(super) async fn owner_command(
        &mut self,
        msg: &InboundMsg,
        oc: crate::bridge::command::OwnerCmd,
        dm: bool,
        root_ts: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) {
        let op = match oc.verb {
            // `warm on|off [<#channel>]`。引数なしは「今いるチャンネル」— Owner は大抵その
            // チャンネルに立っている。DM はルートが無く常に事前起動なので受けない
            "warm" => {
                // 第1引数が on/off であることは parse_owner_command が保証済み
                let on = oc
                    .args
                    .first()
                    .is_some_and(|a| a.eq_ignore_ascii_case("on"));
                let ch = match oc.args.get(1) {
                    Some(tok) => crate::bridge::command::SlackId::from_channel_mention(tok),
                    None if !dm => Some(msg.channel.clone()),
                    None => None,
                };
                let Some(channel) = ch else {
                    let usage = if dm && oc.args.len() < 2 {
                        crate::t!(
                            "The agent for DMs is always started ahead of time. Name a channel: `warm on|off <#channel>`",
                            "DM のエージェントは常に先に起動しています。チャンネルを指定してください: `warm on|off <#channel>`"
                        )
                    } else {
                        crate::t!("Usage: `warm on|off [<#channel>]`", "使い方: `warm on|off [<#channel>]`")
                    };
                    self.post(&msg.channel, root_ts, usage, key);
                    return;
                };
                bridge::AccessOp::SetWarm { channel, on }
            }
            // Home は実在のチャンネルでなければならない — 打たれたその場所が Home になる
            "set-home" => {
                if dm || !crate::bridge::command::SlackId::is_channel(&msg.channel) {
                    let refusal = crate::t!(
                        "Run `set-home` in the *channel* you want notices in. A DM can't be the notice channel.",
                        "`set-home` は、通知を出したい *チャンネル* で実行してください。DM は通知先にできません。"
                    );
                    self.post(&msg.channel, root_ts, refusal, key);
                    return;
                }
                bridge::AccessOp::SetHome(msg.channel.clone())
            }
            verb @ ("allow-bot" | "remove-bot") => {
                let raw = oc.args.first().map(String::as_str).unwrap_or("");
                let bot_id = if crate::bridge::command::SlackId::is_bot(raw) {
                    raw.to_string() // 素の B… id はそのまま受ける
                } else {
                    // bot は自分の USER id(`<@U…>`)で mention されるが、許可台帳の鍵は bot_id(B…)
                    let Some(uid) = crate::bridge::command::SlackId::from_user_mention(raw) else {
                        self.post(
                            &msg.channel,
                            root_ts,
                            crate::t!("Usage: `{verb} <@bot>`", "使い方: `{verb} <@bot>`"),
                            key,
                        );
                        return;
                    };
                    match self.deps.slack.resolve_bot_id(&uid).await {
                        Ok(Some(b)) => b,
                        Ok(None) => {
                            let human = crate::t!(
                                "`{verb}` needs a bot — that mention is a person.",
                                "`{verb}` にはボットを指定してください。今のメンションは人です。"
                            );
                            self.post(&msg.channel, root_ts, human, key);
                            return;
                        }
                        Err(e) => {
                            ctx.error("bridge", &format!(
                                    "slack-events: owner-command '{verb}' failed for {}:{root_ts}: {e}",
                                    msg.channel
                                ));
                            self.post(&msg.channel, root_ts, e, key);
                            return;
                        }
                    }
                };
                if verb == "allow-bot" {
                    bridge::AccessOp::BotAllow(bot_id)
                } else {
                    bridge::AccessOp::BotRemove(bot_id)
                }
            }
            // 今は parse_owner_command が知っている verb しか寄越さないので届かない枝。
            // それでも黙って消えない— 現行は throw を catch して**その文言をそのまま**
            // Owner に返すので、こちらもそう返す
            other => {
                let e = format!("unknown owner command: {other}");
                ctx.error(
                    "bridge",
                    &format!(
                        "slack-events: owner-command '{other}' failed for {}:{root_ts}: {e}",
                        msg.channel
                    ),
                );
                self.post(&msg.channel, root_ts, e, key);
                return;
            }
        };
        match self.access.apply(op) {
            Ok((access, message, warnings)) => {
                self.adopt_access(access, ctx);
                self.post(
                    &msg.channel,
                    root_ts,
                    crate::bridge::render::Notice::with_warnings(&message, &warnings),
                    key,
                );
            }
            // 検証エラーの文が Owner の読むもの
            Err(e) => {
                ctx.error(
                    "bridge",
                    &format!(
                        "slack-events: owner-command '{}' failed for {}:{root_ts}: {e}",
                        oc.verb, msg.channel
                    ),
                );
                self.post(&msg.channel, root_ts, e, key);
            }
        }
    }
}
