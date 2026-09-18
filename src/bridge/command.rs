//! Bridge が自分だけで答える「本文コマンド」— その解釈と実行。
//!
//! 前半は解釈(すべて純関数・I/O なし)。
//! 後半(`// ── running commands ──`)は実行: 解釈したコマンドを `impl Bridge` で走らせる
//! (`handle_command` が入口。サインイン・サインアウトの結末は `CmdFx` で main に戻す)。
//!
//! コマンドは3層(検出 / パース / レンダ)のうちの**検出**。ここの唯一の掟は
//! 「メッセージ**全体**がそのコマンドのときだけ発動する」。
//! 語句を含むだけの文は素通しし、普通のメッセージとしてワーカーに届く。
//!
//! ここから `agent::` を見るのは依存の向きどおり(Bridge → エージェント)。逆は無い。
//! 答えの文面は、そのコマンドを走らせる側(`command/agent.rs` / `command/bridge.rs`)に居る。
//!
//! 並びは役割の順:
//!
//! 1. 本文の読み方   `Message`(mention と不可視文字の除去はここだけ)
//! 2. Slack の id    `SlackId`
//! 3. コマンドの検出 `Cmd` / `PwdMode` / `OwnerCmd` — 素のワードの語彙は `Cmd::WORDS` 1枚
//! 4. 上限の見張り   `UsageWatch`(時刻の読み書きは `state::WallClock`)
//! 5. ツール許可     `ToolPermission`
//! 6. 実行           `SignIn` と入口の `handle_command`。中身はスレッドのエージェントに対するもの
//!    (`command/agent.rs`)と Bridge 自身のもの(`command/bridge.rs`)に分かれる

mod agent;
mod bridge;

use crate::bridge::state::WallClock;

use crate::agent::UsageRow;
use crate::agent::Agent;
use std::path::Path;

use super::Bridge;
use crate::chat::InboundMsg;
use super::state::{LogCtx, ThreadKey};
use crate::agent::screen::SpawnOutcome;
use std::collections::HashMap;

// ── 節1: 本文の読み方 ────────────────────────────────────────────────────────

/// Bridge に届いた1メッセージの本文。コマンド判定はここを通る。
///
/// 唯一の掟は「メッセージ**全体**がそのコマンドのときだけ発動する」。
/// 語句を含むだけの文は素通しし、普通のメッセージとしてワーカーに届く。
#[derive(Clone, Copy)]
pub struct Message<'a> {
    text: &'a str,
    bot_user_id: Option<&'a str>,
}

impl<'a> Message<'a> {
    pub fn new(text: &'a str, bot_user_id: Option<&'a str>) -> Self {
        Self { text, bot_user_id }
    }

    /// **自分の** mention だけを落とした本文。他人のは残す — `<@other> stop` は他人宛なので
    /// 我々にとっては素のコマンドでなくなる。残りの大小文字と空白は保存(パス引数が要る)。
    fn without_mention(&self) -> String {
        let Some(bot) = self.bot_user_id.filter(|b| !b.is_empty()) else {
            return self.text.to_string();
        };
        let needle = format!("<@{bot}");
        let mut out = String::with_capacity(self.text.len());
        let mut rest = self.text;
        while let Some(i) = rest.find(&needle) {
            let after = &rest[i + needle.len()..];
            // 剥がすのは `<@ID>` と `<@ID|label>` だけ。先頭が一致するだけの別 id は温存する
            let tail = match after.strip_prefix('>') {
                Some(t) => Some(t),
                None => after
                    .strip_prefix('|')
                    .and_then(|t| t.find('>').map(|j| &t[j + 1..])),
            };
            match tail {
                Some(t) => {
                    out.push_str(&rest[..i]);
                    out.push(' ');
                    rest = t;
                }
                None => {
                    let cut = i + needle.len();
                    out.push_str(&rest[..cut]);
                    rest = &rest[cut..];
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// 自 mention を落とし、異体字セレクタ / ZWJ(不可視。Slack の `text` は絵文字にこれを
    /// 付けて寄越す)を除いた本文。大小文字と空白はそのまま。
    fn cleaned(&self) -> String {
        self.without_mention()
            .chars()
            .filter(|c| !matches!(c, '\u{FE0F}' | '\u{200D}'))
            .collect()
    }

    /// 比較可能な形に均したメッセージ: [`Self::cleaned`] + trim + 小文字化。
    pub fn normalized(&self) -> String {
        self.cleaned().trim().to_lowercase()
    }

    /// 本文が**全体として** `name` コマンドであるときだけ true。語を含むだけの長い文は
    /// 決して素のコマンドではない。未知の `name` は false。
    pub fn is(&self, name: &str) -> bool {
        let normalized = self.normalized();
        Cmd::WORDS
            .iter()
            .find(|(n, ..)| *n == name)
            .is_some_and(|(_, words, _)| words.contains(&normalized.as_str()))
    }

    /// 本文が `verb` で始まるときの、その**後ろ**の語。そうでなければ None。
    /// 大小文字は保存する(パス引数が要る) — 比べるときに小文字化すること。
    pub fn verb_args(&self, verb: &str) -> Option<Vec<String>> {
        let cleaned = self.cleaned();
        let mut words = cleaned.split_whitespace();
        let first = words.next().unwrap_or("");
        (first.to_lowercase() == verb).then(|| words.map(str::to_string).collect())
    }

    /// 本文が**自分を**メンションしているか(`containsSelfMention`)。
    /// チャンネルでは、これかアクティブスレッドでないと配達しない。
    pub fn mentions_bot(&self) -> bool {
        self.bot_user_id
            .filter(|b| !b.is_empty())
            .is_some_and(|_| self.without_mention() != self.text)
    }

    /// 本文が**自分以外の誰か**を名指ししているか。自分の名指しを落とした残りに `<@` が
    /// 残っていればそれは他人宛(`<@誰か> おーい` — 動いているスレッドに流れてはくるが、
    /// こちらへの用件ではない)。
    pub fn mentions_someone_else(&self) -> bool {
        self.without_mention().contains("<@")
    }
}

// ── 節2: Slack の id 形と mention ────────────────────────────────────────────
// user も bot も `<@…>` で描画される。bot は `B…` id で識別する。

/// Slack の id。**形だけ**で判る種別(問い合わせに行かない)。
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

    /// DM チャンネル(`D…`)— bot と相手しか居ない部屋。
    pub fn is_dm(id: &str) -> bool {
        Self::shaped(id, &['D'])
    }

    pub fn from_user_mention(token: &str) -> Option<String> {
        Self::parse_mention(token, '@', Self::is_user)
    }

    pub fn from_channel_mention(token: &str) -> Option<String> {
        Self::parse_mention(token, '#', Self::is_channel)
    }

    /// mention トークンの中身の id — user なら `<@U…>` / `<@U…|label>`、channel なら
    /// `<#C…>` / `<#C…|label>` — か、単体で書かれた素の id。その種の参照でなければ None。
    fn parse_mention(token: &str, sigil: char, shape: fn(&str) -> bool) -> Option<String> {
        let t = token.trim();
        let inner = t
            .strip_prefix('<')
            .and_then(|s| s.strip_prefix(sigil))
            .and_then(|s| s.strip_suffix('>'));
        let id = match inner {
            // label に `>` を含む壊れたトークンは mention 扱いしない(現物の `[^>]*` 相当)
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

// ── 節3: コマンドの検出 ─────────────────────────────────────────────────────
// 引数を取る5つ(`model` / `effort` / `mode` / `pwd` / access verb)の検出は**形**で行う:
// 既知のモデル名だけが引数なので、「model の説明をして」は文でありそのままワーカーに届く。

/// Bridge が自分で答えるコマンド16種。
#[derive(Debug, PartialEq, Eq)]
pub enum Cmd {
    Stop,
    Exit,
    Resume,
    Help,
    Status,
    Context,
    Usage,
    Compact,
    Restart,
    /// Owner が既に居るのに来た `login`(未設定のときは gate の手前で捌く)
    Login,
    Logout,
    /// `None` = 素の `model`(現在値の表示) / `Some(name)` = 切替
    Model(Option<String>),
    /// `None` = 素の `effort` / `Some(level)` = 設定
    Effort(Option<String>),
    /// `None` = 素の `mode`(現在値の表示) / `Some(name)` = 切替
    Mode(Option<String>),
    Pwd(PwdMode),
    Owner(OwnerCmd),
}

impl Cmd {
    /// 素のワードで発動する全コマンドの語彙を**1つの表**に。
    /// 名前・言い換え・変種を1行に持つので、`parse` にも `label` にも別の表が要らない。
    /// 引数を取る5つ(`model` / `effort` / `mode` / `pwd` / access verb)は引数を読まないと
    /// 言われたかどうかが判らないので、下で個別にパースする。
    const WORDS: [(&str, &[&str], Cmd); 11] = [
        // stop: ワード・`:shortcode:`・生絵文字 — Slack の `text` はどれでも寄越しうる
        (
            "stop",
            &[
                "stop",
                ":red_circle:",
                ":octagonal_sign:",
                ":black_square_for_stop:",
                ":hand:",
                ":raised_hand:",
                ":x:",
                "🔴",
                "🛑",
                "⏹",
                "✋",
                "❌",
            ],
            Cmd::Stop,
        ),
        ("exit", &["exit", "bye", "done"], Cmd::Exit),
        ("resume", &["resume"], Cmd::Resume),
        ("status", &["status", "ステータス"], Cmd::Status),
        ("context", &["context", "ctx"], Cmd::Context),
        ("usage", &["usage", "usg"], Cmd::Usage),
        // 素の疑問符は自然な「何ができる?」
        ("help", &["help", "ヘルプ", "?", "？"], Cmd::Help),
        ("compact", &["compact"], Cmd::Compact),
        ("restart", &["restart"], Cmd::Restart),
        ("login", &["login"], Cmd::Login),
        ("logout", &["logout"], Cmd::Logout),
    ];

    /// 本文**全体**がコマンドならそれを返す。引数を読まないと言われたか判らない5つ
    /// (model / effort / mode / pwd / owner verb)は最後に形で見る。`model` / `effort` / `mode`
    /// が受ける値は `agent` の語彙(Bridge が `deps.agent` を渡す)。
    pub fn parse(msg: &Message<'_>, agent: &dyn Agent) -> Option<Cmd> {
        // 素のワードで発動するもの — 表の順に見る(均すのは1回でいい)
        let normalized = msg.normalized();
        if let Some((_, _, cmd)) = Self::WORDS
            .into_iter()
            .find(|(_, words, _)| words.contains(&normalized.as_str()))
        {
            return Some(cmd);
        }
        if let Some(m) = Self::value_of(msg, "model", |v| agent.canonical_model(v)) {
            return Some(Cmd::Model(m));
        }
        if let Some(l) = Self::value_of(msg, "effort", Self::listed(agent.effort_levels())) {
            return Some(Cmd::Effort(l));
        }
        if let Some(m) = Self::value_of(msg, "mode", Self::listed(agent.modes())) {
            return Some(Cmd::Mode(m));
        }
        if let Some(mode) = Self::pwd(msg) {
            return Some(Cmd::Pwd(mode));
        }
        Self::owner(msg).map(Cmd::Owner)
    }

    /// ログと断り文に出る呼び名(現行の文面と1文字同じ)。
    pub fn label(&self) -> String {
        let name = match self {
            Cmd::Stop => "stop",
            Cmd::Exit => "exit",
            Cmd::Resume => "resume",
            Cmd::Help => "help",
            Cmd::Status => "status",
            Cmd::Context => "context",
            Cmd::Usage => "usage",
            Cmd::Compact => "compact",
            Cmd::Restart => "restart",
            Cmd::Login => "login",
            Cmd::Logout => "logout",
            Cmd::Model(_) => "model",
            Cmd::Effort(_) => "effort",
            Cmd::Mode(_) => "mode",
            Cmd::Pwd(_) => "pwd",
            Cmd::Owner(oc) => return format!("owner-command '{}'", oc.verb),
        };
        format!("'{name}'")
    }

    /// 「素の verb = 現在値の表示 / verb + **既知の値** = 設定」の形をした3つ
    /// (`model` / `effort` / `mode`)の共通パース。返りは
    /// None = コマンドでない / `Some(None)` = 素の verb / `Some(Some(値))` = 設定。
    /// 大小文字は不問、知らない値(`accept` が None を返す値)はコマンドでない(文としてワーカーに届く)。
    fn value_of(
        msg: &Message<'_>,
        verb: &str,
        accept: impl Fn(&str) -> Option<String>,
    ) -> Option<Option<String>> {
        let args = msg.verb_args(verb)?;
        if args.len() > 1 {
            return None;
        }
        let Some(raw) = args.first() else {
            return Some(None);
        };
        accept(&raw.to_lowercase()).map(Some)
    }

    /// 一覧にある値だけを受ける `accept`(`effort` / `mode` 用)。
    fn listed(known: &'static [&'static str]) -> impl Fn(&str) -> Option<String> {
        move |v| known.contains(&v).then(|| v.to_string())
    }

    /// `pwd` コマンドとしてのパース。コマンドでなければ None を返し、「pwd の使い方を変える」は
    /// 文としてワーカーに届く。パスは空白を含みうるので残り**全部**をパスとして繋ぐ
    /// (`pwd` にはラベル引数が無いので取り違えようがない)。`~…` も形としてはパスなので発動し、
    /// 「絶対パスでどうぞ」の答えを得る(ワーカーのターンに消えるより良い)。
    fn pwd(msg: &Message<'_>) -> Option<PwdMode> {
        let args = msg.verb_args("pwd")?;
        let Some(first) = args.first() else {
            return Some(PwdMode::Current);
        };
        if args.len() == 1 && first.to_lowercase() == "all" {
            return Some(PwdMode::All);
        }
        (first.starts_with('/') || first.starts_with('~')).then(|| PwdMode::Set(args.join(" ")))
    }

    /// その verb を**試みている**のか、単にその語で始まっただけなのか。判断するのは第1引数の
    /// **形**だけ: switch のところに `on`/`off`、bot のところに bot、何も取らない verb には何も。
    /// これで「warm の話をしよう」は文だと判る。試みが**完全**か・値が妥当かは見ない —
    /// チャンネルを忘れた `warm on` も試みであり、dispatch から usage 行を貰う。
    fn looks_like_owner(verb: &str, args: &[String]) -> bool {
        let first = args.first().map(String::as_str).unwrap_or("");
        match verb {
            "warm" => matches!(first.to_lowercase().as_str(), "on" | "off"),
            "allow-bot" | "remove-bot" => {
                SlackId::is_bot(first) || SlackId::from_user_mention(first).is_some()
            }
            // set-home に引数の形は無いので、メッセージ全体が素のワードでなければならない
            "set-home" => args.is_empty(),
            _ => false,
        }
    }

    /// Owner 管理コマンドとしてのパース。第1語が verb で、**かつ**続きがその verb の引数の形を
    /// しているときだけ Some。bot / channel の mention は空白を含まないので単純分割で壊れない。
    fn owner(msg: &Message<'_>) -> Option<OwnerCmd> {
        OWNER_COMMAND_VERBS.iter().find_map(|verb| {
            let args = msg.verb_args(verb)?;
            Self::looks_like_owner(verb, &args).then_some(OwnerCmd { verb, args })
        })
    }

    // ── コマンドは**ボタン** — 押された時に発火するか、さもなくば発火しない
    // コマンドは送り主が「今」期待する副作用を持つのに、
    // メッセージは送られてずっと後に届きうる(Slack は receipt を得られなかったイベントを
    // 再配達し、停止中の投稿をまとめて寄越す)。通常のリクエストは無傷 — 再起動を待たされた
    // リクエストはちゃんと配達される。

    /// このコマンドを走らせては**ならない**理由、走らせて良いなら None。読めない `ts` は決して
    /// 妨げない(fail-open: Owner が今押したコマンドは必ず効かねばならない)。
    pub fn stale_reason(
        &self,
        message_ts: &str,
        bridge_started_at_ms: u64,
        retry_num: u32,
    ) -> Option<String> {
        if retry_num > 0 {
            return Some(format!("Slack redelivered it (retry {retry_num})"));
        }
        let posted_at_ms = message_ts.parse::<f64>().ok()? * 1000.0;
        if !posted_at_ms.is_finite() || posted_at_ms <= 0.0 {
            return None;
        }
        let started = bridge_started_at_ms as f64;
        (posted_at_ms < started).then(|| {
            let secs = ((started - posted_at_ms) / 1000.0).round() as i64;
            format!("posted {secs}s before this Bridge started listening")
        })
    }
}

/// パースされた `pwd` が取りうる3つの形。
///
/// 「引数が不正」という形は無い — 読めない引数はそもそもコマンドでなかった、と解釈する。
#[derive(Debug, PartialEq, Eq)]
pub enum PwdMode {
    Current,
    Set(String),
    All,
}

// access 管理は MCP ツールではない: Owner が素のメッセージを送り、Bridge がここでパースして
// **配達前に**実行する(ワーカーには決して渡らない = prompt injection の面がゼロ)。

const OWNER_COMMAND_VERBS: [&str; 4] = ["allow-bot", "remove-bot", "set-home", "warm"];

/// パース済みの Owner コマンド1本(`allow-bot U123` など)。
///
/// `verb` は [`OWNER_COMMAND_VERBS`] のどれか — 未知の語はコマンドとして成立しないので、
/// 「不正な verb」という状態は存在しない。
#[derive(Debug, PartialEq, Eq)]
pub struct OwnerCmd {
    pub verb: &'static str,
    pub args: Vec<String>,
}

// ── 節4: 上限の見張り ──────────────────────────────────────────────────────

/// `/usage` を定期的に読んで、上限が近い/当たったことに気づく側の判断。
///
/// 「いつ読むか(間隔)」「どこで警告するか(節目)」「いつ塞がっているか(reset)」の3つは
/// 一緒に効くので1つに置く。
pub struct UsageWatch;

impl UsageWatch {
    /// 警告を出す使用率の節目。
    const WARN_THRESHOLDS: [u32; 2] = [80, 90];
    /// 平常時の間隔と、上限到達が見込まれるときの間隔。
    pub const POLL_MS: u64 = 60 * 60 * 1000;
    pub const POLL_AT_RISK_MS: u64 = 15 * 60 * 1000;

    /// 前回警告した節目より上で、**今回新しく跨いだ**節目(`newlyCrossedThresholds`)。
    /// 使用率は窓の中では上がる一方なので、下がったら窓が変わったということ — 呼び手が
    /// `last_warned` を 0 に戻して警告を張り直す。
    pub fn newly_crossed(usage_pct: u32, last_warned: u32) -> Vec<u32> {
        Self::WARN_THRESHOLDS
            .into_iter()
            .filter(|t| usage_pct >= *t && *t > last_warned)
            .collect()
    }

    /// 上限に達している窓のうち、**最も遅い**リセット時刻(`bindingLimitResetEpoch`)。
    ///
    /// 見るのは全部の行 — 週の壁はセッションの使用率が低くても効く。早く解ける方に合わせると
    /// まだ塞がっている壁へ突っ込むので、**遅い方**を採る。達している窓が無ければ None。
    pub fn binding_limit_reset(rows: &[UsageRow], now_ms: u64, limit_pct: f64) -> Option<u64> {
        rows.iter()
            .filter(|r| r.pct.parse::<f64>().unwrap_or(0.0) >= limit_pct)
            .filter_map(|r| WallClock::parse_reset_epoch(&r.reset, now_ms))
            .filter(|reset| *reset > now_ms)
            .max()
    }
}


// ── 節5: ツール許可 — 人に訊く前に効く常設の規則 ─────────
// 人に訊くべきツールだけを人に訊くための門番。**これが無いと**ワーカーは自分の返信ツール
// (`reply`)の許可を人に訊きにいく = 「Slack で答えてよいか」を Slack で訊くことになり、
// 誰も押さないまま止まる。

/// このワーカー自身の MCP サーバのツール接頭辞。**この実装のサーバ名**に対応する
/// (`--mcp-config` の `agentgw` — 現行はプラグイン経由なので別綴りだが、
/// 見ている対象は同じ「自前のツール」)。
const OWN_MCP_PREFIX: &str = "mcp__agentgw__";

/// 常設規則の答え。`Ask` = 規則では決まらない(人に訊く)。
#[derive(Debug, PartialEq, Eq)]
pub enum ToolPermission {
    /// 訊かずに通す。文字列は理由(ワーカーに返す message に載る)。
    Allow(&'static str),
    /// 訊かずに拒む。人に「いいですか」と出してはいけないもの。
    Deny(&'static str),
    /// 規則では決まらない — 人に訊く。
    Ask,
}

impl ToolPermission {
    /// 常設規則を当てる。**純関数** — 状態ディレクトリは自前のファイルを見分けるためだけに使う。
    /// 移植元。
    pub fn decide(tool_name: &str, tool_input: &serde_json::Value, state_dir: &Path) -> Self {
        let field = |k: &str| tool_input.get(k).and_then(|v| v.as_str()).unwrap_or("");
        let (skill, file_path) = (field("skill"), field("file_path"));
        let threads_file = state_dir.join("threads.json");
        let access_file = state_dir.join("access.json");
        let is =
            |p: &std::path::Path| !file_path.is_empty() && std::path::Path::new(file_path) == p;

        // 自己信頼: ワーカーが答える唯一の手段は自前のツールなので、`reply` の許可を人に
        // 訊くのは「Slack で答えてよいか」を Slack で訊くこと。実行権限は Bridge にあり、
        // 何をしてよいかは Bridge が実行時に決める
        if tool_name.starts_with(OWN_MCP_PREFIX) {
            return Self::Allow("own MCP tool");
        }
        if tool_name == "Skill" && skill.starts_with("agentgw:") {
            return Self::Allow("own skill");
        }
        if tool_name == "WebSearch" || tool_name == "WebFetch" {
            return Self::Allow("read-only web tool");
        }
        if tool_name == "Read" && (is(&threads_file) || is(&access_file)) {
            return Self::Allow("own state read");
        }

        // 構造的な拒否: access は Owner のもので、変えるのは Owner の DM コマンドだけ
        // (Bridge が自分でパースする)。ワーカーが直に書けるのは権限昇格なので、
        // 「通しますか」と人に出さずその場で拒む
        if (tool_name == "Edit" || tool_name == "Write")
            && (is(&access_file) || file_path.ends_with("/.agentgw/access.json"))
        {
            return Self::Deny(
                "a worker cannot edit access.json directly — access changes are the Owner\u{2019}s DM commands",
            );
        }

        Self::Ask
    }

    /// permission hook の応答。Claude Code v2 は `hookSpecificOutput` の中に
    /// `{decision:{behavior}}` を入れ子で欲しがる — stop の平たい形とは**別形**
    pub fn decision_output(behavior: &str, message: &str) -> serde_json::Value {
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": { "behavior": behavior, "message": message },
            }
        })
    }
}

/// サインイン・サインアウトの進行状態。触るのは `command/agent.rs` だけ。
#[derive(Default)]
pub(super) struct SignIn {
    /// コード待ちのサインイン: channel → その sign-in を始めた人。**同時に1本だけ**
    /// (login セッションは1つ — 2本目を通すと後から来たコードで先の人が Owner になる)
    pending: HashMap<String, String>,
    /// サインアウトが走っているか。`logout` の2連打で `claude auth logout` が2回走り、
    /// 2本目の teardown が1本目の後始末と噛み合わなくなるのを防ぐ
    signing_out: bool,
}

// ── running commands ─────────────────────────────────────────────────────────

/// spawn したコマンドが main ループへ返す状態変更の便り。
///
/// サインイン・サインアウトは spawn したタスクの中で何十秒も走る(ブラウザの往復を待つ)ので、
/// 状態の書換えは main ループに**戻して**やる — tmux とポーリングはタスク、access.json と
/// サインインの状態(`SignIn`)は main、と持ち場を割る(dispo_rx と同じ形)。
pub(super) enum CmdFx {
    /// サインインの結末。成否どちらでも login セッションを畳んで pending の席を空ける
    /// (**始まり**は main が同期で登録する — 席取りを spawn に任せると2本目に奪われる)。
    /// `bound` = Owner にする人
    LoginFinished {
        channel: String,
        bound: Option<String>,
    },
    LogoutFinished {
        ok: bool,
        channel: String,
        thread_ts: String,
    },
    /// 起動直後の窓に**誰も答えられない画面**が出ていた。tmux のポーリングはタスク、
    /// ゲートと通知は main、と持ち場を割る(LoginFinished と同じ形)。
    SpawnScreen {
        outcome: SpawnOutcome,
        /// ログに出す相手(`thread=…` / `pool session=…`)
        what: String,
    },
}

impl Bridge {
    /// Bridge が自分で答える本文コマンド。**true = 消費した** — 呼び手はそこで打ち切る。
    ///
    /// 認可は1本の規則だけ: 送信者が Owner か。チャンネルでも DM でも、
    /// mention の有無にも依らない。Owner でない誰かのコマンドは記録して捨てる — ワーカーに
    /// 落ちると「永遠に再spawn される未応答メッセージ」になってスレッドを詰まらせる。
    pub(super) async fn handle_command(&mut self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str) -> bool {
        // 検出は Cmd::parse が全部やる — どれでもなければコマンドではない
        let msg_body = crate::bridge::command::Message::new(&msg.text, self.bot_user_id.as_deref());
        let Some(cmd) = crate::bridge::command::Cmd::parse(&msg_body, self.deps.agent.as_ref()) else {
            return false;
        };
        let label = cmd.label();

        let ctx = LogCtx {
            session_id: None,
            thread_key: Some(key.clone()),
        };
        let sender = msg.user.as_deref().unwrap_or("");
        // 今は gate が非 Owner を先に落とすのでここは通らない。それでも残す — 現行が
        // 非 Owner の発言をワーカーに「読ませる」context 配達を持っており、
        // それを移植した日に gate は非 Owner を通し始める。コマンドの認可はその時も**ここ**にある
        if self.access.owner.is_empty() || msg.user.as_deref() != Some(self.access.owner.as_str()) {
            // `login` だけは文脈が1つ増える — Owner が**既に居る**のに来た login だから
            // ここに落ちている(居なければ gate の手前の抜け道が捌く)
            let note = match cmd {
                Cmd::Login => " while an Owner is bound",
                _ => "",
            };
            ctx.info(
                "bridge",
                &format!(
                    "slack-events: {label} from non-owner {sender}{note} — \
                     ignoring (not delivered to worker) msg={}",
                    msg.ts
                ),
            );
            return true;
        }
        let dm = msg.channel_kind == crate::chat::ChannelKind::Dm;

        match cmd {
            // stop。ESC は tmux を通るので
            // MCP が死んでいても効く — 止めたくなるのは大抵まさにその状況
            Cmd::Stop => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: stop command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_stop(msg, key, root_ts, &ctx);
            }
            // exit。終わらせるのは**ワーカーだけ** — スレッドは残る
            Cmd::Exit => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: exit command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_exit(msg, key, root_ts, &ctx).await;
            }
            // resume。手元の端末に線を渡す
            Cmd::Resume => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: resume command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_resume(msg, key, root_ts, &ctx);
            }
            Cmd::Help => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: help command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.post(
                    &msg.channel,
                    root_ts,
                    bridge::help(self.fleet, self.deps.agent.as_ref()),
                    key,
                );
            }
            // status。ワーカーを一切通さないので、全員が
            // 固まっていても答えが返る
            Cmd::Status => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: status command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_status(msg, key, root_ts, &ctx);
            }
            Cmd::Context => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: context command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_context(msg, key, root_ts, &ctx);
            }
            Cmd::Usage => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: usage command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_usage(msg, key, root_ts, &ctx);
            }
            Cmd::Compact => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: compact command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_compact(msg, key, root_ts, &ctx);
            }
            // restart。この Bridge 自身を
            // 入れ替える唯一のコマンド — 進捗チェックリストだけが2つのプロセスをまたぐ
            Cmd::Restart => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: restart command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                let channel = msg.channel.clone();
                self.maintenance_restart("slack restart command", Some((&channel, root_ts)), &ctx)
                    .await;
            }
            // Owner が居るときの `login`。Owner が決まっていても、**このマシンの** Claude Code の
            // サインインは別に切れる(2026-09-18、あるマシンだけ切れていて Slack から戻す手段が無かった)。
            // だから実際の状態を訊き、切れていればここで(このマシンで)サインインを始める。
            // サインイン済みなら打ち止める — ワーカーに落とすと「何にログインしますか?」と訊き返す
            Cmd::Login => {
                let signed_in = self.deps.agent.signed_in().await;
                if signed_in == Some(true) {
                    ctx.info(
                        "bridge",
                        &format!(
                            "slack-events: login command from Owner while already signed in — \
                             replying already-signed-in msg={} channel={} dm={dm}",
                            msg.ts, msg.channel
                        ),
                    );
                    self.post(
                        &msg.channel,
                        root_ts,
                        {
                            let owner = &self.access.owner;
                            crate::t!(
                                "Already signed in (owner: <@{owner}>). To switch accounts, send `logout`, then `login`.",
                                "既にサインインしています(Owner: <@{owner}>)。アカウントを切り替えるには、`logout` のあとに `login` を送ってください。"
                            )
                        },
                        key,
                    );
                } else {
                    ctx.info(
                        "bridge",
                        &format!(
                            "slack-events: login command from Owner — this machine's agent is {} — \
                             starting sign-in here msg={} channel={} dm={dm}",
                            if signed_in == Some(false) { "signed out" } else { "of unknown sign-in state" },
                            msg.ts,
                            msg.channel
                        ),
                    );
                    self.start_login(msg.channel.clone(), sender.to_string(), root_ts.to_string());
                }
            }
            Cmd::Logout => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: logout command msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_logout(&msg.channel, root_ts);
            }
            Cmd::Model(name) => {
                let what = match &name {
                    Some(n) => format!("model command ({n})"),
                    None => "model show command".to_string(),
                };
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: {what} msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_model(msg, name, key, root_ts, &ctx);
            }
            Cmd::Effort(level) => {
                let what = match &level {
                    Some(l) => format!("effort command ({l})"),
                    None => "effort show command".to_string(),
                };
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: {what} msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_effort(msg, level, key, root_ts, &ctx);
            }
            Cmd::Mode(name) => {
                let what = match &name {
                    Some(n) => format!("mode command ({n})"),
                    None => "mode show command".to_string(),
                };
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: {what} msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                self.user_mode(msg, name, key, root_ts, &ctx);
            }
            Cmd::Pwd(mode) => {
                let kind = match mode {
                    PwdMode::Current => "current",
                    PwdMode::Set(_) => "set",
                    PwdMode::All => "all",
                };
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: pwd command mode={kind} msg={} channel={} dm={dm}",
                        msg.ts, msg.channel
                    ),
                );
                let out = self.pwd_answer(msg, mode, dm, root_ts, &ctx);
                self.post(&msg.channel, root_ts, out, key);
            }
            Cmd::Owner(oc) => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: owner-command '{}' args=[{}] msg={} channel={} dm={dm}",
                        oc.verb,
                        oc.args.join(" "),
                        msg.ts,
                        msg.channel
                    ),
                );
                self.owner_command(msg, oc, dm, root_ts, key, &ctx).await;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 節目の跨ぎと、ゲートを解く時刻の選び方。
    #[test]
    fn the_usage_monitor_warns_once_per_threshold_and_gates_until_the_latest_wall() {
        // 跨いだ節目だけ返す(同じ節目で二度警告しない)
        assert_eq!(UsageWatch::newly_crossed(75, 0), Vec::<u32>::new());
        assert_eq!(UsageWatch::newly_crossed(80, 0), vec![80]);
        assert_eq!(
            UsageWatch::newly_crossed(95, 0),
            vec![80, 90],
            "一気に跨いだら両方"
        );
        assert_eq!(
            UsageWatch::newly_crossed(95, 80),
            vec![90],
            "済んだ節目は出さない"
        );
        assert_eq!(UsageWatch::newly_crossed(95, 90), Vec::<u32>::new());

        let now = 1_785_387_600_000u64; // 2026-07-30T05:00Z = 30日 14:00 JST
        let row = |label: &str, pct: &str, reset: &str| UsageRow {
            label: label.into(),
            pct: pct.into(),
            reset: reset.into(),
        };
        // 達している窓が無ければゲートしない
        assert_eq!(
            UsageWatch::binding_limit_reset(&[row("Current session", "99", "at 11pm")], now, 100.0),
            None
        );
        // 週の壁はセッションが低くても効く。**遅い方**まで塞ぐ
        let rows = [
            row("Current session", "100", "at 4pm"),
            row("Current week (all models)", "100", "at 11pm"),
        ];
        let reset = UsageWatch::binding_limit_reset(&rows, now, 100.0).unwrap();
        assert_eq!(reset, WallClock::parse_reset_epoch("at 11pm", now).unwrap());
        // 既に過ぎた reset は採らない(0% 行の空 reset も同じく落ちる)
        assert_eq!(
            UsageWatch::binding_limit_reset(&[row("Current session", "100", "")], now, 100.0),
            None
        );
    }

    /// 名指しの見分け。動いているスレッドには他人宛の発言も流れてくるので、
    /// 「自分が名指しされたか」と「他人が名指しされたか」を別々に答える。
    #[test]
    fn tells_our_own_mention_apart_from_someone_elses() {
        fn m<'a>(t: &'a str) -> Message<'a> {
            Message::new(t, Some("UBOT"))
        }
        assert!(m("<@UBOT> やって").mentions_bot());
        assert!(!m("<@UBOT> やって").mentions_someone_else());
        // 他人宛 — 自分は名指しされていない
        assert!(!m("<@UOTHER> おーい").mentions_bot());
        assert!(m("<@UOTHER> おーい").mentions_someone_else());
        // 両方 — 自分も呼ばれている
        assert!(m("<@UBOT> <@UOTHER> と相談して").mentions_bot());
        assert!(m("<@UBOT> <@UOTHER> と相談して").mentions_someone_else());
        // 誰も名指ししていない素の続き
        assert!(!m("ありがとう").mentions_bot());
        assert!(!m("ありがとう").mentions_someone_else());
        // 表示名つきの形(`<@ID|label>`)でも同じ
        assert!(m("<@UBOT|claude> やって").mentions_bot());
    }

    /// 常設規則。**自前の MCP ツールを通すのが肝** — これが無いとワーカーは
    /// 「Slack で答えてよいか」を Slack で訊きにいき、誰も押さないまま止まる。
    #[test]
    fn standing_tool_policy_lets_the_worker_answer_and_refuses_access_writes() {
        use ToolPermission as P;
        let dir = std::path::Path::new("/st");
        let none = serde_json::json!({});
        let file = |p: &str| serde_json::json!({ "file_path": p });

        // 自前のもの: 訊かない
        assert_eq!(
            P::decide("mcp__agentgw__reply", &none, dir),
            P::Allow("own MCP tool")
        );
        assert_eq!(
            P::decide(
                "Skill",
                &serde_json::json!({"skill": "agentgw:status"}),
                dir
            ),
            P::Allow("own skill")
        );
        assert_eq!(
            P::decide("WebFetch", &none, dir),
            P::Allow("read-only web tool")
        );
        assert_eq!(
            P::decide("Read", &file("/st/threads.json"), dir),
            P::Allow("own state read")
        );

        // 権限昇格は人に訊かず断る
        assert!(matches!(
            P::decide("Write", &file("/st/access.json"), dir),
            P::Deny(_)
        ));
        assert!(matches!(
            P::decide("Edit", &file("/home/u/.agentgw/access.json"), dir),
            P::Deny(_)
        ));

        // それ以外は人の判断
        assert_eq!(
            P::decide("Bash", &serde_json::json!({"command": "rm -rf /"}), dir),
            P::Ask
        );
        assert_eq!(P::decide("Read", &file("/etc/passwd"), dir), P::Ask);
        // 他人の MCP は通さない
        assert_eq!(P::decide("mcp__other__do_thing", &none, dir), P::Ask);
    }

    /// 応答の形は stop の平たい形と**別**。入れ子を間違えると Claude Code が読まない。
    #[test]
    fn perm_decision_output_is_the_nested_shape() {
        let v = ToolPermission::decision_output("allow", "Slack bridge (own MCP tool)");
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert_eq!(v["hookSpecificOutput"]["decision"]["behavior"], "allow");
        assert_eq!(
            v["hookSpecificOutput"]["decision"]["message"],
            "Slack bridge (own MCP tool)"
        );
    }



    #[test]
    fn whole_message_only_is_a_command() {
        assert!(Message::new("stop", None).is("stop"));
        assert!(Message::new("  STOP ", None).is("stop"));
        assert!(Message::new("🛑", None).is("stop")); // 絵文字 alias
        assert!(Message::new(":red_circle:", None).is("stop"));
        assert!(!Message::new("stop the deploy", None).is("stop")); // 文中は発動しない
        assert!(Message::new("？", None).is("help")); // 全角
        assert!(Message::new("ステータス", None).is("status"));
        assert!(Message::new("bye", None).is("exit"));
        // 自ボット mention は剥がす。他人の mention は剥がさない
        assert!(Message::new("<@UBOT> stop", Some("UBOT")).is("stop"));
        assert!(!Message::new("<@UOTHER> stop", Some("UBOT")).is("stop"));
    }

    #[test]
    fn arg_commands_parse_by_shape() {
        let agent = crate::agent::fake::FakeAgent::default();
        let model = |v: &str| agent.canonical_model(v);
        let modes = Cmd::listed(agent.modes());
        assert_eq!(
            Cmd::value_of(&Message::new("model opus", None), "model", model),
            Some(Some("opus".into()))
        );
        assert_eq!(
            Cmd::value_of(&Message::new("model sonet", None), "model", model),
            Some(Some("sonnet".into()))
        );
        assert_eq!(
            Cmd::value_of(&Message::new("model", None), "model", model),
            Some(None)
        );
        assert_eq!(
            Cmd::value_of(
                &Message::new("model の説明をして", None),
                "model",
                model
            ),
            None
        ); // 文は素通し
        assert_eq!(
            Cmd::value_of(
                &Message::new("effort xhigh", None),
                "effort",
                Cmd::listed(agent.effort_levels())
            ),
            Some(Some("xhigh".into()))
        );
        assert_eq!(
            Cmd::value_of(&Message::new("mode plan", None), "mode", &modes),
            Some(Some("plan".into()))
        );
        assert_eq!(
            Cmd::value_of(&Message::new("MODE", None), "mode", &modes),
            Some(None)
        );
        assert_eq!(
            Cmd::value_of(&Message::new("mode bypass", None), "mode", &modes),
            None
        ); // 引数に無い = 文
        assert_eq!(
            Cmd::value_of(&Message::new("mode を実装して", None), "mode", &modes),
            None
        );
        assert!(
            matches!(Cmd::pwd(&Message::new("pwd /a b/c", None)), Some(PwdMode::Set(p)) if p == "/a b/c")
        );
        assert!(matches!(
            Cmd::pwd(&Message::new("pwd all", None)),
            Some(PwdMode::All)
        ));
        assert_eq!(Cmd::pwd(&Message::new("pwd の使い方", None)), None);
        let oc = Cmd::owner(&Message::new("warm on <#C1|general>", None)).unwrap();
        assert_eq!((oc.verb, oc.args.len()), ("warm", 2));
        assert!(Cmd::owner(&Message::new("warm の話をしよう", None)).is_none()); // on/off が無い = 文
        assert!(Cmd::owner(&Message::new("set-home", None)).is_some());
        assert!(Cmd::owner(&Message::new("set-home here", None)).is_none()); // 引数付きは文
    }

    /// 手書き走査(id 形・mention・自 mention 剥がし)の最小の網。
    #[test]
    fn id_shapes_and_mentions() {
        assert!(SlackId::is_user("U012AB") && !SlackId::is_user("U") && !SlackId::is_user("BU12"));
        assert!(SlackId::is_bot("B01") && SlackId::is_channel("C01") && SlackId::is_channel("G01"));
        assert_eq!(
            SlackId::from_user_mention("<@U01|taito>").as_deref(),
            Some("U01")
        );
        assert_eq!(SlackId::from_user_mention("U01").as_deref(), Some("U01")); // 素の id も可
        assert_eq!(
            SlackId::from_channel_mention("<#C01|general>").as_deref(),
            Some("C01")
        );
        assert_eq!(SlackId::from_channel_mention("<@U01>"), None);
        // 自 mention は剥がれ、引数の大小文字は保存される
        assert_eq!(
            Message::new("<@UBOT> pwd /Users/Me", Some("UBOT"))
                .verb_args("pwd")
                .unwrap(),
            ["/Users/Me"]
        );
        let agent = crate::agent::fake::FakeAgent::default();
        let is_command = |text: &str| Cmd::parse(&Message::new(text, None), &agent).is_some();
        assert!(is_command("restart") && is_command("pwd all"));
        assert!(!is_command("restart してください"));
    }

    #[test]
    fn stale_command_detection() {
        // 起動(=100_000ms)より前に投稿された ts はコマンドとして死んでいる
        assert!(Cmd::Stop.stale_reason("99.000000", 100_000, 0).is_some());
        assert!(Cmd::Stop.stale_reason("101.000000", 100_000, 0).is_none());
        assert!(Cmd::Stop.stale_reason("101.000000", 100_000, 1).is_some()); // 再配達
        assert!(Cmd::Stop.stale_reason("garbage", 100_000, 0).is_none()); // fail-open
    }

}
