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
        let mut threads: Vec<StatusThread> = Vec::new();
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
            threads.push(StatusThread {
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
            let report = StatusReport {
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
            PwdEntry {
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
                PwdEntry::render_all(&all, &home)
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
                pwd_dm_set_refusal()
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
                        with_warnings(&message, &warnings)
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
                    with_warnings(&message, &warnings),
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

// ── `status` — Bridge が集めた生の診断スナップショットの描画 ──────────────────

/// `status` が1行に描く「動いているスレッド1本」。
///
/// **version は持たない** — ワーカーの版報告機構が部品ごと無いので、
/// 現行の版skew 表示(verSuffix)は落とす。
pub struct StatusThread {
    pub channel_id: String,
    pub thread_ts: String,
    /// 最終活動の epoch ms。0 = 不明
    pub last_activity_ms: u64,
    /// ワーカーが動いているリポジトリ(チャンネルの組ごとに1回だけ出す)
    pub repo_path: Option<String>,
    pub permalink: Option<String>,
    /// スレッドの話題(冒頭メッセージの1行目) — リンクの文字列に使う
    pub topic: Option<String>,
    /// 解決済みのチャンネル名(`#general` / DM なら `@taito`)
    pub channel_name: Option<String>,
}

impl StatusThread {
    /// 1本ぶんの行: 経過時間の等幅チップ + 話題のリンク。読む人が探すのは「どれが古いか」で、
    /// 左端に短い固定幅の塊が並ぶと目で追える(桁揃えの代わり)。判らない時刻は書かない —
    /// `不明` は情報が無いのに1列ぶん場所を取る。
    fn line(&self, now_ms: u64) -> String {
        let label = Self::link_text(self.topic.as_deref());
        let text = match self.permalink.as_deref().filter(|p| !p.is_empty()) {
            Some(p) => format!("<{p}|{label}>"),
            None => label,
        };
        match Self::idle_label(now_ms, self.last_activity_ms) {
            None => format!("　{text}"),
            Some(idle) => format!("`{idle}`　{text}"),
        }
    }

    /// 人が読める idle: `たった今`(<1分)/ `45分前`(<60分)/ `9.4時間前`(≥60分)。
    /// 生の `565分` は判断しづらい。
    ///
    /// **原文からの意図的差分**: 「前」を付ける。裸の `4分` は何の4分か判らない
    /// (2026-07-31 ユーザー指摘)。移植漏れではない。
    /// `None` = 最後に動いた時刻が分からない。
    fn idle_label(now_ms: u64, last_activity_ms: u64) -> Option<String> {
        if last_activity_ms == 0 {
            return None;
        }
        let mins = now_ms.saturating_sub(last_activity_ms) / 60_000;
        if mins < 1 {
            return Some(crate::t!("just now", "たった今"));
        }
        if mins < 60 {
            return Some(crate::t!("{mins}m ago", "{mins}分前"));
        }
        let hours = format!("{:.1}", mins as f64 / 60.0);
        Some(crate::t!("{hours}h ago", "{hours}時間前"))
    }

    /// mrkdwn のリンク文字列には `<` `>` `|` を置けない(リンクの区切り文字)。スレッドの話題は
    /// 冒頭メッセージそのものなので大抵 `<@…>` で始まる — リンク文字列の中の mention は決して
    /// 解決されず生の id が漏れる。先に Slack トークンを全部落とし、残った区切り文字を除いて
    /// 改行を畳む。
    fn link_text(topic: Option<&str>) -> String {
        // `/<[@#!][^>]*>/g` 相当の手書き走査(依存を増やさないため regex は使わない)
        let raw = topic.unwrap_or("");
        let mut stripped = String::with_capacity(raw.len());
        let mut rest = raw;
        while let Some(i) = rest.find('<') {
            let after = &rest[i + 1..];
            let end = after
                .starts_with(['@', '#', '!'])
                .then(|| after.find('>'))
                .flatten();
            match end {
                Some(j) => {
                    stripped.push_str(&rest[..i]);
                    rest = &after[j + 1..];
                }
                // トークンでない `<` はただの文字 — 消さずに次へ進める
                None => {
                    stripped.push_str(&rest[..=i]);
                    rest = after;
                }
            }
        }
        stripped.push_str(rest);
        let cleaned: String = stripped
            .chars()
            .filter(|c| !matches!(c, '<' | '>' | '|'))
            .collect();
        // `\s+` → ' ' + trim を一手で
        let t = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.is_empty() {
            return crate::t!("(untitled)", "(無題)");
        }
        // 現物は UTF-16 単位で 50 を数えるが、ここは文字数(絵文字を割らない方が安全)
        if t.chars().count() > 50 {
            format!("{}…", t.chars().take(50).collect::<String>())
        } else {
            t
        }
    }
}

/// `status` の描画に要るものを全部載せたスナップショット。時計も含むので、描画は純関数のまま。
///
/// `pool` は cwd だけ持つ(現行の `poolKey` は不透明な内部キーで表示しない、
/// `version` は版 skew を作らないこの実装には無い)。
pub struct StatusReport {
    pub bridge_version: String,
    /// 時計はスナップショットが運ぶ(描画を純関数のまま固定 now でテストできる)
    pub now_ms: u64,
    /// $HOME。長い絶対パスを `~` に畳んで読みやすくするため
    pub home: String,
    /// この Bridge の繋がり方(`local (label)` など)
    pub mode: String,
    /// **実際に動いている**ワーカーのスレッドだけ
    pub threads: Vec<StatusThread>,
    /// 在庫中の Warm Pool worker が待っている作業ディレクトリ
    pub pools: Vec<String>,
}

/// StatusReport を Slack mrkdwn で。純関数(時計は report の now_ms)。
///
/// 設計: id でなく名前を読ませ、問題が無いときは黙る。チャンネルの組ごとに、解決済みの名前
/// (チャンネルは `<#id|name>`、DM は相手の `@name`)とルート先リポジトリを見出しにし、
/// スレッドは冒頭メッセージ(bot mention 除去済み)+ idle 時間の箇条書きにする。
impl StatusReport {
    /// $HOME 配下の絶対パスを `~` 相対に。$HOME の外はそのまま(basename にはしない —
    /// 隣り合うリポは接頭辞を共有するので区別が消える)。
    fn short_path(p: Option<&str>, home: &str) -> String {
        let Some(p) = p.filter(|s| !s.is_empty()) else {
            return crate::t!("(unknown)", "(不明)");
        };
        match p.strip_prefix(home).filter(|_| !home.is_empty()) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
            _ => p.to_string(),
        }
    }

    pub fn render(&self) -> String {
        let r = self;
        let mut lines: Vec<String> = Vec::new();
        let version = if r.bridge_version.is_empty() {
            "(unversioned)"
        } else {
            r.bridge_version.as_str()
        };
        // 版と繋がり方は1行に畳む。`mode: local` を単独行に置くと、見出しの下に
        // ログの断片が紛れ込んだように見える(2026-07-31 ユーザー指摘)
        let mode = if r.mode.is_empty() {
            String::new()
        } else {
            format!(" · {}", r.mode)
        };
        lines.push(format!("🟢 *agentgw* `{version}`{mode}"));
        // 素の空行だと Slack は段落の隙間しか空けず、下の箇条書きの余白に負けて
        // 見出しが本文にくっついて見える。全角スペース1つの行なら高さのある行として残る
        lines.push("　".to_string());

        if r.threads.is_empty() {
            lines.push(crate::t!("*Active threads* — none", "*動いているスレッド* — なし"));
        } else {
            // チャンネルごとに束ねる。現物の Map と同じく**挿入順**を保つ(下の整列は安定ソート)
            let mut groups: Vec<(&str, Vec<&StatusThread>)> = Vec::new();
            for t in &r.threads {
                match groups.iter_mut().find(|(ch, _)| *ch == t.channel_id) {
                    Some((_, g)) => g.push(t),
                    None => groups.push((&t.channel_id, vec![t])),
                }
            }
            let n = r.threads.len();
            lines.push(crate::t!("*Active threads* — {n}", "*動いているスレッド* — {n}"));
            // DM が先、次に賑やかなチャンネル順。同点は channel id で決める。
            let dm_rank = |ch: &str| u8::from(!ch.starts_with('D'));
            groups.sort_by(|a, b| {
                dm_rank(a.0)
                    .cmp(&dm_rank(b.0))
                    .then(b.1.len().cmp(&a.1.len()))
                    .then(a.0.cmp(b.0))
            });
            for (i, (ch, ts)) in groups.iter_mut().enumerate() {
                // 空行はチャンネルの**間**だけ。見出しの直後に置くと本数だけが浮いて見える
                if i > 0 {
                    lines.push(String::new());
                }
                let dm = ch.starts_with('D');
                let name = ts
                    .iter()
                    .find_map(|t| t.channel_name.as_deref().filter(|n| !n.is_empty()));
                // 解決済みの名前をラベルに焼き込む — 素の `<#id>` が解決されない面でも名前で読める。
                // Slack は先頭の '#' を自分で外すので裸の語を渡す。
                let head = match (name, dm) {
                    (Some(n), true) => n.to_string(),
                    (Some(n), false) => format!("<#{ch}|{}>", n.strip_prefix('#').unwrap_or(n)),
                    (None, true) => "DM".to_string(),
                    (None, false) => format!("<#{ch}>"),
                };
                let repo = ts
                    .iter()
                    .find_map(|t| t.repo_path.as_deref().filter(|p| !p.is_empty()));
                lines.push(match repo {
                    Some(_) => format!("{head} · `{}`", Self::short_path(repo, &r.home)),
                    None => head,
                });
                ts.sort_by_key(|t| t.last_activity_ms); // 放置が長い順
                lines.extend(ts.iter().map(|t| t.line(r.now_ms)));
            }
        }
        lines.push(String::new());
        // 現行と同じ、待っているフォルダだけの一覧
        if r.pools.is_empty() {
            lines.push(crate::t!("*Warm agents* — none", "*待機中のエージェント* — なし"));
        } else {
            let n = r.pools.len();
            lines.push(crate::t!("*Warm agents* — {n}", "*待機中のエージェント* — {n}"));
            // 1行1件だと縦に伸びるだけ — パスは等幅チップにして横に並べる
            lines.push(
                r.pools
                    .iter()
                    .map(|cwd| format!("`{}`", Self::short_path(Some(cwd), &r.home)))
                    .collect::<Vec<_>>()
                    .join("　"),
            );
        }
        lines.join("\n")
    }
}

/// 表示用に解決済みの「チャンネル → パス」1行。
///
/// `is_fallback` は Home フォールバック由来(そのチャンネルに明示の `repo_path` ルートが
/// 無い)ことを示す。
pub struct PwdEntry {
    pub channel_id: String,
    pub repo_path: String,
    pub label: Option<String>,
    pub is_fallback: bool,
}

/// 1チャンネルのパス(`pwd` の形)を Slack mrkdwn で。`heading` が「このチャンネル」と
/// 名指しのチャンネルを区別する。
impl PwdEntry {
    /// ラベルは括弧つきで、無ければ何も出さない。
    fn label_text(&self) -> String {
        self.label
            .as_deref()
            .filter(|l| !l.is_empty())
            .map(|l| format!("（{l}）"))
            .unwrap_or_default()
    }

    pub fn render(&self, heading: &str) -> String {
        let entry = self;
        let label = entry.label_text();
        let note = if entry.is_fallback {
            crate::t!(" — not set; using the default directory", " — 未設定のため既定のディレクトリ")
        } else {
            String::new()
        };
        format!(
            "● {heading}\n<#{}>{label}{note}\n  `{}`",
            entry.channel_id, entry.repo_path
        )
    }

    /// 設定済みの全チャンネル → パス(`pwd all` の形)。`home` はルート未設定のチャンネル / DM が
    /// 落ちる Home フォールバック。
    pub fn render_all(entries: &[PwdEntry], home: &str) -> String {
        let mut lines = vec![crate::t!("● Project directories by channel", "● チャンネルごとの作業ディレクトリ")];
        if entries.is_empty() {
            lines.push(crate::t!("  • None set.", "  • まだ設定していません。"));
        } else {
            for e in entries {
                lines.push(format!(
                    "  • <#{}>{} → `{}`",
                    e.channel_id,
                    e.label_text(),
                    e.repo_path
                ));
            }
        }
        lines.push(String::new());
        lines.push(crate::t!("_Channels without one, and DMs, use `{home}`_", "_設定していないチャンネルと DM は `{home}` を使います_"));
        lines.join("\n")
    }
}

/// 断りや通知に警告を添える。**警告があるときだけ**、1行1件で `⚠️ ` を付けて足す
/// (現行の文面と1文字同じ)。
fn with_warnings(message: &str, warnings: &[String]) -> String {
    if warnings.is_empty() {
        return message.to_string();
    }
    let w: Vec<String> = warnings.iter().map(|w| format!("⚠️ {w}")).collect();
    format!("{message}\n{}", w.join("\n"))
}

/// help を Slack mrkdwn で。目的ごとに束ね、各行は等幅のトリガ(別名込み)+ 一行説明。
/// `COMMAND_WORDS` と引数パーサとは**手で**同期させる — ここはそれらの人間向け索引。
///
/// 現行の `remoteMode` に当たるのが `fleet`。現行は「Remote の下にぶら下がる Local」で
/// 出していたが、この実装で `route` を実行するのは**子を迎える側**(`relay.rs` の
/// `CommandCtx::route`)なので、条件もそちらに合わせる。`fleet=false` の出力は
/// 現行の `remoteMode=false` と一致する。
pub(super) fn help(fleet: bool, agent: &dyn crate::agent::Agent) -> String {
    fn section<C: std::fmt::Display>(lines: &mut Vec<String>, title: String, rows: Vec<(C, String)>) {
        lines.push(format!("*{title}*"));
        for (cmd, desc) in rows {
            lines.push(format!("  • `{cmd}` — {desc}"));
        }
        lines.push(String::new());
    }

    let mut lines: Vec<String> = vec![crate::t!("*agentgw commands*", "*agentgw のコマンド*"), String::new()];

    for (title, rows) in super::agent::help_sections(agent) {
        section(&mut lines, title, rows);
    }
    if fleet {
        section(
            &mut lines,
            crate::t!("Machines", "マシン"),
            vec![
                ("route <machine>", crate::t!("hand this channel to a machine", "このチャンネルをマシンに任せる")),
                ("route", crate::t!("show which machine handles this channel and the others", "このチャンネルとほかのチャンネルを受け持つマシンを見る")),
            ],
        );
    }
    section(
        &mut lines,
        crate::t!("Channels", "チャンネル"),
        vec![
            ("pwd", crate::t!("show this channel's project directory", "このチャンネルの作業ディレクトリを見る")),
            ("pwd <absolute path>", crate::t!("set this channel's project directory", "このチャンネルの作業ディレクトリを決める")),
            ("pwd all", crate::t!("show every channel's project directory", "すべてのチャンネルの作業ディレクトリを見る")),
            ("warm on|off [<#channel>]", crate::t!("keep an agent started ahead of time for a channel (this one if none is given)", "チャンネルのエージェントを先に起動しておくか(省くとこのチャンネル)")),
            ("set-home", crate::t!("send notices to this channel", "通知をこのチャンネルに出す")),
        ],
    );
    section(
        &mut lines,
        "agentgw".to_string(),
        vec![
            ("status", crate::t!("version, active threads and warm agents", "版・動いているスレッド・待機中のエージェント")),
            ("restart", crate::t!("restart agentgw (picks up a new version)", "agentgw を再起動する(新しい版を読み込む)")),
        ],
    );
    section(
        &mut lines,
        crate::t!("Other bots", "ほかのボット"),
        vec![
            ("allow-bot <@bot>", crate::t!("let a bot's messages start work", "そのボットの投稿で作業を始められるようにする")),
            ("remove-bot <@bot>", crate::t!("stop letting that bot's messages through", "そのボットの投稿を通さないようにする")),
        ],
    );
    section(
        &mut lines,
        crate::t!("Help", "ヘルプ"),
        vec![("help / ?", crate::t!("show this list", "この一覧を出す"))],
    );

    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.push(String::new());
    lines.push(crate::t!(
        "_A command only runs when it's the whole message (with its arguments). Inside a sentence it's just part of a normal message._",
        "_コマンドは、メッセージ全体がそのコマンド(引数を含む)のときだけ動きます。文の中に書いたときは、普通のメッセージとして届きます。_"
    ));
    lines.join("\n")
}

/// DM で `pwd <path>` と打たれたときの答え。DM のエージェントは常に既定のディレクトリで
/// 動く — 設定するものが無いので、黙って何もしない代わりにそう言う。
fn pwd_dm_set_refusal() -> String {
    crate::t!(
        "Agents in DMs always work in the default directory; only channels can have their own.",
        "DM のエージェントは常に既定のディレクトリで動きます。作業ディレクトリを決められるのはチャンネルだけです。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_and_paths() {
        let _en = crate::i18n::pin(crate::i18n::Lang::En);
        let idle = |now, last| StatusThread::idle_label(now, last);
        assert_eq!(idle(10_000, 0), None);
        assert_eq!(idle(60_000, 30_000).as_deref(), Some("just now"));
        assert_eq!(idle(46 * 60_000, 60_000).as_deref(), Some("45m ago"));
        assert_eq!(idle(10 * 60 * 60_000, 36 * 60_000).as_deref(), Some("9.4h ago"));
        assert_eq!(
            StatusReport::short_path(Some("/Users/t/dev/x"), "/Users/t"),
            "~/dev/x"
        );
        assert_eq!(
            StatusReport::short_path(Some("/opt/x"), "/Users/t"),
            "/opt/x"
        );
        assert_eq!(StatusReport::short_path(None, "/Users/t"), "(unknown)");
    }

    #[test]
    fn status_report_groups_and_orders() {
        let r = StatusReport {
            bridge_version: "0.1.0-rs".into(),
            now_ms: 1_000_000,
            home: "/Users/t".into(),
            mode: "local".into(),
            threads: vec![
                StatusThread {
                    channel_id: "C1".into(),
                    thread_ts: "1.0".into(),
                    last_activity_ms: 940_000,
                    repo_path: Some("/Users/t/dev/x".into()),
                    permalink: Some("https://s/p1".into()),
                    topic: Some("<@UBOT> READMEを要約して".into()),
                    channel_name: Some("#general".into()),
                },
                StatusThread {
                    channel_id: "D1".into(),
                    thread_ts: "2.0".into(),
                    last_activity_ms: 0,
                    repo_path: None,
                    permalink: None,
                    topic: None,
                    channel_name: Some("@taito".into()),
                },
            ],
            pools: vec![],
        };
        let out = r.render();
        // 版と mode は1行(単独の `mode: local` 行はログの断片に見える)
        // 名乗りはバイナリ名そのまま。見出しの下は全角スペース1つの行(素の空行では隙間が足りない)
        assert!(out.starts_with("🟢 *agentgw* `0.1.0-rs` · local\n　\n"));
        assert!(out.contains("*Active threads* — 2"));
        // DM が先、mention は link text から剥がれる
        assert!(out.find("@taito").unwrap() < out.find("general").unwrap());
        assert!(!out.contains("UBOT"));
        // 経過時間は行頭の等幅チップ、その後ろが飛び先つきの話題
        assert!(
            out.contains("\n<#C1|general> · `~/dev/x`\n`1m ago`　<https://s/p1|READMEを要約して>")
        );
        // 時刻が判らないスレッドはチップを置かず、頭を揃えるだけ
        assert!(out.contains("\n@taito\n　(untitled)\n"));
        assert!(out.contains("*Warm agents* — none"));
        // 空スレッド
        let empty = StatusReport {
            threads: vec![],
            ..r
        };
        assert!(empty.render().contains("*Active threads* — none"));
    }

    /// Warm Pool 節の最小構成。threads は空でよい(節どうしは独立)。
    fn sample_report() -> StatusReport {
        StatusReport {
            bridge_version: "0.1.0-rs".into(),
            now_ms: 1_000_000,
            home: "/Users/t".into(),
            mode: "local".into(),
            threads: vec![],
            pools: vec![],
        }
    }

    /// 現行と同じ「本」+ cwd の平箇条書き(内部キーは出さない)。
    #[test]
    fn status_report_lists_warm_pools() {
        let r = StatusReport {
            pools: vec!["/repo/a".into(), "/Users/t/dev/b".into()],
            ..sample_report()
        };
        let out = r.render();
        assert!(out.contains("*Warm agents* — 2"));
        assert!(out.contains("\n\n*Warm agents")); // 上の節との間に空行
        assert!(out.contains("\n`/repo/a`　`~/dev/b`")); // 横一列 / $HOME は畳む
    }

    #[test]
    fn status_report_warm_pool_empty_unchanged() {
        assert!(sample_report().render().contains("*Warm agents* — none"));
    }

    /// regex を使わない手書きのトークン走査(3つの replace)の網。
    #[test]
    fn link_text_strips_tokens_and_truncates() {
        assert_eq!(StatusThread::link_text(None), "(untitled)");
        assert_eq!(
            StatusThread::link_text(Some("<@U1> <#C1|general>")),
            "(untitled)"
        ); // 全部トークン → 空
        assert_eq!(StatusThread::link_text(Some("a<b\nc  d")), "ab c d"); // 閉じない `<` は消えるだけ(隙間は空かない)
        assert_eq!(
            StatusThread::link_text(Some("<https://x|見出し>")),
            "https://x見出し"
        ); // 非トークンの `<`
        let long = "あ".repeat(60);
        let cut = StatusThread::link_text(Some(&long));
        assert!(cut.ends_with('…') && cut.chars().count() == 51);
    }

    #[test]
    fn pwd_renders() {
        let e = PwdEntry {
            channel_id: "C1".into(),
            repo_path: "/dev/x".into(),
            label: Some("dev".into()),
            is_fallback: true,
        };
        assert_eq!(
            e.render("This channel"),
            "● This channel\n<#C1>（dev） — not set; using the default directory\n  `/dev/x`"
        );
        assert!(PwdEntry::render_all(&[e], "/home").contains("  • <#C1>（dev） → `/dev/x`"));
        assert!(PwdEntry::render_all(&[], "/home").contains("None set."));
        assert!(
            PwdEntry::render_all(&[], "/home")
                .ends_with("_Channels without one, and DMs, use `/home`_")
        );
    }

    #[test]
    fn help_lists_every_command() {
        let h = help(false, &crate::agent::fake::FakeAgent::default());
        for word in [
            "stop",
            "exit / bye / done",
            "compact",
            "model [fable|opus|sonnet|haiku]",
            "effort [low|medium|high|xhigh|max|ultracode|auto]",
            "mode [manual|plan|edit|auto]",
            "context / ctx",
            "resume",
            "login",
            "logout",
            "usage / usg",
            "status",
            "restart",
            "pwd <absolute path>",
            "warm on|off [<#channel>]",
            "set-home",
            "allow-bot <@bot>",
            "remove-bot <@bot>",
            "help / ?",
        ] {
            assert!(h.contains(word), "{word}");
        }
        assert!(!h.contains("route <machine>")); // マシンが居ないゲートウェイに担当表は無い
        assert!(h.ends_with("just part of a normal message._"));
    }

    #[test]
    fn a_bridge_that_takes_children_lists_route() {
        // 実装は relay.rs の `CommandCtx::route` にあるのに一覧に出ていなかった
        let h = help(true, &crate::agent::fake::FakeAgent::default());
        assert!(h.contains("route <machine>"));
        assert!(h.contains("hand this channel to a machine"));
        // マシンが居ても他の節は変わらない
        assert!(h.contains("status") && h.contains("help / ?"));
    }
}
