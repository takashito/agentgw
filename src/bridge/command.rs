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
//! 文面(描画)は `render.rs` に居る。
//!
//! 並びは役割の順:
//!
//! 1. 本文の読み方   `Message`(mention と不可視文字の除去はここだけ)
//! 2. Slack の id    `SlackId`
//! 3. コマンドの検出 `Cmd` / `PwdMode` / `OwnerCmd` — 素のワードの語彙は `Cmd::WORDS` 1枚
//! 4. 上限の見張り   `UsageWatch`(時刻の読み書きは `state::WallClock`)
//! 5. ツール許可     `ToolPermission`
//! 6. 実行           `SignIn` と `impl Bridge`(`handle_command` / `user_*` / `owner_command` / サインイン)

use crate::bridge::state::WallClock;

use crate::agent::UsageRow;
use crate::agent::screen::MODEL_NAMES;
use std::path::Path;

use super::inbound::InboundMsg;
use super::state::{self as bridge, LogCtx, ThreadKey};
use super::{Bridge, Host};
use crate::agent::screen::SpawnOutcome;
use crate::agent::tmux::Window;
use crate::agent::{CompactOutcome, CompactProgress, LoginOutcome, ProbeErr, SessionId};
use crate::bridge::inbound;
use crate::{ports, slack};
use std::collections::HashMap;
use tokio::sync::mpsc;

/// サインインのポーリング(定数どおり)。URL は普通 1〜3 秒で出る。
const LOGIN_POLL: std::time::Duration = std::time::Duration::from_secs(1);

const URL_POLL_MAX: u32 = 20;

const CODE_POLL_MAX: u32 = 30;

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

    /// 本文**全体**が Bridge の本文コマンドのいずれかか。`Cmd::parse` と同じ判定 —
    /// 「コマンドだったか」だけを知りたい所(stale ガード)のための入口。
    pub fn is_body_command(&self) -> bool {
        Cmd::parse(self).is_some()
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
    /// (model / effort / mode / pwd / owner verb)は最後に形で見る。
    pub fn parse(msg: &Message<'_>) -> Option<Cmd> {
        // 素のワードで発動するもの — 表の順に見る(均すのは1回でいい)
        let normalized = msg.normalized();
        if let Some((_, _, cmd)) = Self::WORDS
            .into_iter()
            .find(|(_, words, _)| words.contains(&normalized.as_str()))
        {
            return Some(cmd);
        }
        if let Some(m) = Self::value_of(msg, "model", &MODEL_NAMES) {
            return Some(Cmd::Model(m));
        }
        if let Some(l) = Self::value_of(msg, "effort", &EFFORT_LEVELS) {
            return Some(Cmd::Effort(l));
        }
        if let Some(m) = Self::value_of(msg, "mode", &MODE_NAMES) {
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
    /// 大小文字は不問、知らない値はコマンドでない(文としてワーカーに届く)。
    /// 672-680
    fn value_of(msg: &Message<'_>, verb: &str, known: &[&str]) -> Option<Option<String>> {
        let args = msg.verb_args(verb)?;
        if args.len() > 1 {
            return None;
        }
        let Some(raw) = args.first() else {
            return Some(None);
        };
        // `sonet` は `sonnet` の綴り間違いとして受ける(model だけの救済だが、他の表に
        // その語は無いので当てても結果は変わらない)
        let value = match raw.to_lowercase().as_str() {
            "sonet" => "sonnet".to_string(),
            v => v.to_string(),
        };
        known.contains(&value.as_str()).then_some(Some(value))
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

/// `/effort` が受け取る level(2026-07-17 実機確認: スライダの5段 + `ultracode` / `auto`)。
const EFFORT_LEVELS: [&str; 7] = ["low", "medium", "high", "xhigh", "max", "ultracode", "auto"];

/// `mode` が受け取る権限モード。shift+tab の巡回で行ける4つだけを引数にする —
/// `bypass` / `don't ask` は設定でしか入らず、Slack から踏ませたいものでもない。
const MODE_NAMES: [&str; 4] = ["manual", "plan", "edit", "auto"];

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

/// サインイン・サインアウトの進行状態。触るのはこのファイルだけ。
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

/// セッションの無いスレッドに返す1行(全コマンド共通)。
fn no_session() -> String {
    crate::t!(
        "This thread has no session running yet. Send a message to start one first.",
        "このスレッドには、まだ動いているセッションがありません。先にメッセージを送ってセッションを始めてください。"
    )
}

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
        let Some(cmd) = crate::bridge::command::Cmd::parse(&msg_body) else {
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
        let dm = msg.channel_kind == inbound::ChannelKind::Dm;

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
                    crate::bridge::render::Notice::Help { fleet: self.fleet }.render(),
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
            // 既にサインイン済みでの `login`。ワーカーに落とすと
            // 「何にログインしますか?」と訊き返してくるので、ここで打ち止める
            Cmd::Login => {
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

    pub(super) fn user_stop(&mut self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let sid = self.threads.get(root_ts).and_then(|e| e.agent_id.clone());
        let pending = self.ledger.pending(key);
        let name = sid
            .as_deref()
            .map(|s| SessionId::from(s).window_name())
            .unwrap_or_default();
        let window_id = sid
            .as_deref()
            .and_then(|s| self.workers.warm(s))
            .and_then(|h| h.window_id.clone());
        // 順に短絡する — 未応答が無い stop で tmux を叩きに行かない
        let running = sid.is_some()
            && !pending.is_empty()
            && self.deps.agent.pid_of(window_id.as_deref(), &name).is_some();
        let Some(sid) = sid.as_deref().filter(|_| running) else {
            ctx.info(
                "bridge",
                &format!(
                    "user stop: key={key} nothing running (session={} pending={}) — no ESC",
                    sid.as_deref().unwrap_or("none"),
                    pending.len()
                ),
            );
            ctx.info(
                "bridge",
                &format!(
                    "slack-events: user stop → nothing running for {}:{root_ts}",
                    msg.channel
                ),
            );
            self.post(
                &msg.channel,
                root_ts,
                crate::t!("Nothing is running right now.", "いま止めるものはありません。"),
                key,
            );
            return;
        };
        // 台帳を**先に**落とす — そうしないと ESC で切られたターンが未応答を抱えたまま終わり、
        // stop hook が「応答待ち」の再プロンプトを撃つ
        self.ledger.disposed(key, &pending);
        let target = Window::of(window_id.as_deref().unwrap_or(&name));
        match self.deps.agent.interrupt(&target) {
            Ok(()) => ctx.info(
                "bridge",
                &format!(
                    "user stop: sent ESC to worker window {target} (session {sid}) key={key}, \
                     disposed pending=[{}]",
                    pending.join(",")
                ),
            ),
            Err(e) => ctx.error("bridge", &format!("user stop: ESC to {target} failed: {e}")),
        }
        self.sticky.on_interrupted(key);
        ctx.info(
            "bridge",
            &format!(
                "slack-events: user stop → interrupted running turn for {}:{root_ts}",
                msg.channel
            ),
        );
    }

    /// `exit` / `bye` / `done`。終わらせるのは
    /// **ワーカーであってスレッドではない** — threads.json の entry は残すので、次のメッセージが
    /// `--resume` で同じセッションを継ぐ。走っているターンは切らずに待つ(stop との違い):
    /// 別れの挨拶がワーカーの最後の返信より**下**に着くように。
    async fn user_exit(&mut self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let farewell = Some((msg.channel.clone(), root_ts.to_string()));
        let sid = self.threads.get(root_ts).and_then(|e| e.agent_id.clone());
        let name = sid
            .as_deref()
            .map(|s| SessionId::from(s).window_name())
            .unwrap_or_default();
        let window_id = sid
            .as_deref()
            .and_then(|s| self.workers.warm(s))
            .and_then(|h| h.window_id.clone());
        // 短絡する — セッションの無いスレッドの exit で tmux を叩きに行かない
        let live = sid.is_some() && self.deps.agent.pid_of(window_id.as_deref(), &name).is_some();
        // ワーカーが居ない exit も別れは告げる(現行 performUserExit は session 無しでも
        // farewell まで行く)。待つものが無いので予約せずその場で片付ける
        let Some(sid) = sid.filter(|_| live) else {
            self.terminate(key, None, farewell).await;
            return;
        };
        self.push_drain(key, sid, farewell, None, ctx); // exit は別れの挨拶を出すので shimmer 不要
    }

    /// `resume`。手元の端末で続きを開く1行を
    /// 出し、**線を本当に渡す** — ワーカーが生きていれば exit と同じドレイン後に終わらせる
    /// (別れの挨拶は無し。報告文が終了を告げ終えている)。
    fn user_resume(&mut self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let entry = self.threads.get(root_ts).cloned().unwrap_or_default();
        // セッション ID は**発行しない** — 走った覚えの無いスレッドに渡す id は端末で失敗するだけ
        let Some(sid) = entry.agent_id.filter(|s| !s.is_empty()) else {
            ctx.info(
                "bridge",
                &format!("resume: no bound session for thread tts={root_ts} — nothing to resume"),
            );
            let none = crate::bridge::render::ResumeInfo {
                session_id: None,
                cwd: None,
                transcript_missing: false,
                worker_running: false,
            };
            self.post(&msg.channel, root_ts, none.render(), key);
            return;
        };
        // 履歴の在処は hook が運んできた道が第一(worktree に入ったワーカーは transcript ごと
        // 別プロジェクトに移る — 記録した repo_path は当てにならない。)
        let remembered = self
            .workers
            .warm(&sid)
            .and_then(|h| h.transcript_path.clone());
        let history_cwd = self.deps.agent.session_cwd(remembered.as_deref(), &sid);
        let history_exists = self
            .deps.agent
            .session_history_exists(remembered.as_deref(), &sid);
        let name = SessionId::from(sid.clone()).window_name();
        let window_id = self.workers.window_of(&sid);
        let worker_running = self.deps.agent.pid_of(window_id.as_deref(), &name).is_some();
        let info = crate::bridge::render::ResumeInfo {
            // cwd は id と同じくらい大事 — claude は cwd ごとに履歴を仕舞うので、
            // 違う場所で --resume すると見つからない
            cwd: Some(history_cwd.or(entry.repo_path).unwrap_or_else(Host::home)),
            transcript_missing: !history_exists,
            worker_running,
            session_id: Some(sid.clone()),
        };
        // 他の6箇所と同じ文字単位の頭8字(バイト添字は非 ASCII の id で panic の芽 —
        // ここは select ループの中)。transcript は現行と同じく在処ではなく2値
        let short: String = sid.chars().take(8).collect();
        ctx.info(
            "bridge",
            &format!(
                "resume: session={short} cwd={} transcript={} worker={}",
                info.cwd.as_deref().unwrap_or(""),
                if history_exists { "found" } else { "MISSING" },
                if worker_running { "RUNNING" } else { "ABSENT" }
            ),
        );
        self.post(&msg.channel, root_ts, info.render(), key);
        if worker_running {
            ctx.info(
                "bridge",
                &format!(
                    "slack-events: resume command → terminating live worker for {key} \
                     (session handed to the Owner's terminal)"
                ),
            );
            // shimmer は**ドレインに預ける**。resume 本体は tmux を1回覗くだけで終わるので、
            // ここで guard を持っても set と clear が連続して飛ぶだけで一度も描画されない。
            // 実際に待つのは「返事が捌けるか30秒」を待つドレイン側(現行 Bun に原文なし)
            let thinking =
                slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, &slack::Status::Resume.text());
            self.push_drain(key, sid, None, Some(thinking), ctx);
        }
    }

    /// `context` / `ctx`。このスレッド自身の
    /// セッションを `--fork-session` で複製して `/context` を訊く — 走っているワーカーには
    /// 触らないので、ターンの最中でも答えが出る。probe は10〜20秒かかるので投げっぱなし。
    fn user_context(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let entry = self.threads.get(root_ts);
        // セッションが無いスレッドには訊く先が無い
        let Some(sid) = entry.and_then(|e| e.agent_id.clone()) else {
            ctx.info(
                "bridge",
                &format!("context: no bound session for thread tts={root_ts} — nothing to probe"),
            );
            self.post(&msg.channel, root_ts, no_session(), key);
            return;
        };
        let cwd = entry
            .and_then(|e| e.repo_path.clone())
            .unwrap_or_else(Host::home);
        let short: String = sid.chars().take(8).collect();
        ctx.info(
            "bridge",
            &format!("context: probing /context tts={root_ts} session={short} cwd={cwd}"),
        );
        let argv = self.deps.agent.context_argv(&sid);
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        let thinking =
            slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, &slack::Status::Context.text());
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア(probe が失敗しても消える)
            let ctx = LogCtx {
                session_id: Some(sid),
                thread_key: Some(key.clone()),
            };
            let out = match agent.probe(argv, cwd).await {
                Ok(raw) => {
                    ctx.info(
                        "bridge",
                        &format!(
                            "context: probe ok tts={root} session={short} bytes={}",
                            raw.len()
                        ),
                    );
                    match agent.context_report(&raw) {
                        Some(r) => r.render(),
                        None => {
                            ctx.error(
                                "bridge",
                                &format!(
                                    "slack-events: context command — could not parse /context \
                                     output for {channel}:{root}"
                                ),
                            );
                            crate::t!("Couldn't read the context usage.", "コンテキストの使用量を読み取れませんでした。")
                        }
                    }
                }
                // 失敗の理由は英語の内部文字列 — ログに置き、スレッドには流さない
                Err(ProbeErr::Failed(e)) => {
                    ctx.error(
                        "bridge",
                        &format!("context: probe failed tts={root} session={short}: {e}"),
                    );
                    ctx.error(
                        "bridge",
                        &format!(
                            "slack-events: context command probe failed for {channel}:{root}: {e}"
                        ),
                    );
                    crate::t!("Couldn't get the context usage. Try again.", "コンテキストの使用量を取得できませんでした。もう一度試してください。")
                }
                Err(ProbeErr::Errored(e)) => {
                    ctx.error(
                        "bridge",
                        &format!("slack-events: context command errored for {channel}:{root}: {e}"),
                    );
                    crate::t!("Something went wrong while getting the context usage.", "コンテキストの使用量を取得する途中でエラーが起きました。")
                }
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// `usage` / `usg`。アカウント全体の話
    /// なのでスレッドもセッションも要らない — Home で `claude -p /usage` を回すだけ。
    fn user_usage(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let cwd = Host::home();
        ctx.info("bridge", &format!("usage: probing /usage cwd={cwd}"));
        let argv = self.deps.agent.usage_argv();
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, &slack::Status::Usage.text());
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア
            let ctx = LogCtx {
                session_id: None,
                thread_key: Some(key.clone()),
            };
            let out = match agent.probe(argv, cwd).await {
                Ok(raw) => {
                    ctx.info("bridge", &format!("usage: probe ok bytes={}", raw.len()));
                    match agent.usage_rows(&raw) {
                        // 300 = "Current session" の窓(5h)。"Current week" 行の窓は
                        // format_usage_report_with_projection が内側で差し替える
                        Some(rows) => crate::bridge::render::UsageReport {
                            rows: &rows,
                            projection: Some((crate::bridge::state::WallClock::now(), 300)),
                        }
                        .render(),
                        None => {
                            ctx.error(
                                "bridge",
                                &format!(
                                    "slack-events: usage command — could not parse /usage \
                                     output for {channel}:{root}"
                                ),
                            );
                            crate::t!("Couldn't read your usage.", "使用状況を読み取れませんでした。")
                        }
                    }
                }
                Err(ProbeErr::Failed(e)) => {
                    ctx.error("bridge", &format!("usage: probe failed: {e}"));
                    ctx.error(
                        "bridge",
                        &format!(
                            "slack-events: usage command probe failed for {channel}:{root}: {e}"
                        ),
                    );
                    crate::t!("Couldn't get your usage. Try again.", "使用状況を取得できませんでした。もう一度試してください。")
                }
                Err(ProbeErr::Errored(e)) => {
                    ctx.error(
                        "bridge",
                        &format!("slack-events: usage command errored for {channel}:{root}: {e}"),
                    );
                    crate::t!("Something went wrong while getting your usage.", "使用状況を取得する途中でエラーが起きました。")
                }
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// TUI を叩くコマンド(compact / model / effort)の共通の入口。
    /// セッションと窓を解決し、ターンが走っている間は断る — 走行中の TUI には打ち込めない。
    /// `Ok((target, session_id))` の時だけ tmux を叩いてよい。`Err` はそのまま返す1行。
    fn tui_guard(
        &self,
        label: &str,
        no_session_tail: &str,
        busy_tail: &str,
        key: &ThreadKey,
        root_ts: &str,
        ctx: &LogCtx,
    ) -> Result<(Window, String), String> {
        let Some(sid) = self.threads.get(root_ts).and_then(|e| e.agent_id.clone()) else {
            ctx.info(
                "bridge",
                &format!("{label}: no bound session for thread tts={root_ts} — {no_session_tail}"),
            );
            return Err(no_session());
        };
        let name = SessionId::from(sid.clone()).window_name();
        let window_id = self.workers.window_of(&sid);
        // 未応答を先に見る — 空なら tmux を叩きに行かない(user_stop と同じ短絡)
        let pending = self.ledger.pending(key);
        if !pending.is_empty() && self.deps.agent.pid_of(window_id.as_deref(), &name).is_some() {
            ctx.info(
                "bridge",
                &format!(
                    "{label}: turn in-flight for key={key} (pending={}) — {busy_tail}",
                    pending.len()
                ),
            );
            return Err(crate::t!(
                "The agent is busy. Send `stop` first, then `{label}`.",
                "エージェントが作業中です。`stop` で止めてから `{label}` を送ってください。"
            ));
        }
        Ok((Window::of(window_id.as_deref().unwrap_or(&name)), sid))
    }

    /// `compact`。**生きているセッション**の
    /// TUI に `/compact` を打ち込み、進捗スピナーを Slack の1本の付箋に流し込む
    /// (context/usage と違って使い捨ての probe ではない — 圧縮するのは走っている会話そのもの)。
    /// 最長6分かかるので select ループの外(spawn)で回す。
    fn user_compact(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let (target, sid) = match self.tui_guard(
            "compact",
            "nothing to compact",
            "refusing (busy)",
            key,
            root_ts,
            ctx,
        ) {
            Ok(v) => v,
            Err(text) => {
                self.post(&msg.channel, root_ts, text, key);
                return;
            }
        };
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        // compact に専用の thinking status は**付けない**(Bun からの逸脱 — ユーザー判断)。
        // 以前の実装はここで「考え中」を張り、tick ごとに秒数付きへ張り直していた。
        // こちらは進捗チェックリスト(run_compact が投稿して編集し続ける sticky)が同じことを
        // 見せているので、shimmer と二重で冗長という判断(**移植漏れではない**)。
        tokio::spawn(Self::run_compact(api, self.deps.agent.clone(), channel, root, key, target, sid));
    }

    /// `model`。名前付きは TUI に
    /// `/model <名前>` を打ち込む。素の `model` は transcript を読むだけ — ワーカーに触らない。
    fn user_model(
        &self,
        msg: &InboundMsg,
        name: Option<String>,
        key: &ThreadKey,
        root_ts: &str,
        ctx: &LogCtx,
    ) {
        let Some(name) = name else {
            self.model_show(msg, key, root_ts, ctx);
            return;
        };
        let (target, sid) = match self.tui_guard(
            "model",
            "nothing to switch",
            "refusing (busy)",
            key,
            root_ts,
            ctx,
        ) {
            Ok(v) => v,
            Err(text) => {
                self.post(&msg.channel, root_ts, text, key);
                return;
            }
        };
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, &slack::Status::Model.text());
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア(TUI が確定しなくても消える)
            let ctx = LogCtx {
                session_id: Some(sid),
                thread_key: Some(key.clone()),
            };
            // None = このエージェントが model 切替に非対応。実体が1つの今は起きない
            let done = agent.set_model(&target, &name, &key, &ctx).await == Some(true);
            let out = if done {
                crate::t!("✅ Switched this thread's model to *{name}*.", "✅ このスレッドのモデルを *{name}* に切り替えました。")
            } else {
                crate::t!("Couldn't switch the model. Try again.", "モデルを切り替えられませんでした。もう一度試してください。")
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// 素の `model`。今のモデルを名乗るのは transcript の**最後の**
    /// assistant レコード。TUI も probe も要らないので select ループの中で答える
    /// (読むのは末尾 256KB だけ — 長いセッションの .jsonl は数十 MB ある)。
    fn model_show(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let Some(sid) = self.threads.get(root_ts).and_then(|e| e.agent_id.clone()) else {
            ctx.info(
                "bridge",
                &format!("model: no bound session for thread tts={root_ts} — no current model"),
            );
            self.post(&msg.channel, root_ts, no_session(), key);
            return;
        };
        let short: String = sid.chars().take(8).collect();
        // 履歴の在処は hook が運んできた道が第一(worktree に入ったワーカーは transcript ごと
        // 移る)。忘れていれば総当たりで探す — resume と同じ解決順
        let remembered = self
            .workers
            .warm(&sid)
            .and_then(|h| h.transcript_path.clone());
        let found = self.deps.agent.current_model(remembered.as_deref(), &sid);
        let failed = || crate::t!("Couldn't read the current model.", "今のモデルを読み取れませんでした。");
        let out = match found {
            None => {
                ctx.info(
                    "bridge",
                    &format!(
                        "model: no transcript for session={short} tts={root_ts} — \
                         cannot read current model"
                    ),
                );
                failed()
            }
            Some(read) => match read {
                Err(e) => {
                    ctx.error(
                        "bridge",
                        &format!("model: transcript read failed for session={short}: {e}"),
                    );
                    failed()
                }
                Ok(model) => match model {
                    None => {
                        ctx.info(
                            "bridge",
                            &format!("model: no model id in transcript tail of session={short}"),
                        );
                        failed()
                    }
                    Some(model) => {
                        ctx.info(
                            "bridge",
                            &format!("model: current={model} session={short} key={key}"),
                        );
                        match self.deps.agent.model_alias(&model) {
                            Some(alias) => crate::t!("Model: *{alias}* (`{model}`)", "モデル: *{alias}*(`{model}`)"),
                            None => crate::t!("Model: `{model}`", "モデル: `{model}`"),
                        }
                    }
                },
            },
        };
        self.post(&msg.channel, root_ts, out, key);
    }

    /// `effort`。level 付きは model と同じ
    /// TUI 駆動。素の `effort` は現在値を**どこにも記録が無い**ので TUI に訊く —
    /// スライダを開いて Escape で閉じ、TUI が出す状態行を読む。
    ///
    /// **現行 TUI との差分**(2026-07-29 実機・実弾で3経路とも確認): 履歴なしは即
    /// `Set effort level to …`、履歴ありは確認ダイアログを挟んで**状態行しか出さず**、
    /// 同じ level の選び直しは `Kept effort level as …`。移植元の Bun が知るのは1つ目だけ。
    fn user_effort(
        &self,
        msg: &InboundMsg,
        level: Option<String>,
        key: &ThreadKey,
        root_ts: &str,
        ctx: &LogCtx,
    ) {
        let (tail, busy) = match &level {
            Some(_) => ("nothing to set", "refusing (busy)"),
            None => ("no current level", "refusing read (busy)"),
        };
        let (target, sid) = match self.tui_guard("effort", tail, busy, key, root_ts, ctx) {
            Ok(v) => v,
            Err(text) => {
                self.post(&msg.channel, root_ts, text, key);
                return;
            }
        };
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        // 素の `effort` は現在値を読むだけ — 現行も status を出さない(出典は set 側の 3182)
        let Some(level) = level else {
            tokio::spawn(Self::run_effort_show(
                api,
                self.deps.agent.clone(),
                channel,
                root,
                key,
                target,
                sid,
            ));
            return;
        };
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, &slack::Status::Effort.text());
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア
            let ctx = LogCtx {
                session_id: Some(sid),
                thread_key: Some(key.clone()),
            };
            // None = このエージェントが effort に非対応。実体が1つの今は起きない
            let done = agent.set_effort(&target, &level, &key, &ctx).await == Some(true);
            let out = if done {
                crate::t!("✅ Set this thread's effort level to *{level}*.", "✅ このスレッドの effort を *{level}* にしました。")
            } else {
                crate::t!("Couldn't set the effort level. Try again.", "effort を設定できませんでした。もう一度試してください。")
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// `mode`(移植元に無い — Rust 版の新機能)。Claude Code の権限モードは TUI の
    /// shift+tab でしか変えられないので、`/effort` のようなスラッシュコマンドは使わず
    /// **キーを押して**目当てのモードに着くまで回す。素の `mode` はフッタを1回読むだけ。
    fn user_mode(
        &self,
        msg: &InboundMsg,
        name: Option<String>,
        key: &ThreadKey,
        root_ts: &str,
        ctx: &LogCtx,
    ) {
        let (tail, busy) = match &name {
            Some(_) => ("nothing to switch", "refusing (busy)"),
            None => ("no current mode", "refusing read (busy)"),
        };
        let (target, sid) = match self.tui_guard("mode", tail, busy, key, root_ts, ctx) {
            Ok(v) => v,
            Err(text) => {
                self.post(&msg.channel, root_ts, text, key);
                return;
            }
        };
        let ctx = LogCtx {
            session_id: Some(sid),
            thread_key: Some(key.clone()),
        };
        // 読むだけなら tmux を1回叩くだけ — spawn も shimmer も要らない
        let Some(name) = name else {
            let out = match self.deps.agent.mode(&target, &ctx) {
                Some(m) => crate::t!("Permission mode: *{m}*", "権限モード: *{m}*"),
                None => crate::t!("Couldn't read the permission mode.", "権限モードを読み取れませんでした。"),
            };
            self.post(&msg.channel, root_ts, out, key);
            return;
        };
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, &slack::Status::Mode.text());
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア
            let done = agent.set_mode(&target, &name, &key, &ctx).await == Some(true);
            let out = if done {
                crate::t!("✅ Switched this thread's permission mode to *{name}*.", "✅ このスレッドの権限モードを *{name}* にしました。")
            } else {
                crate::t!("Couldn't switch the permission mode. Try again.", "権限モードを切り替えられませんでした。もう一度試してください。")
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// `status`。**本当に動いている**
    /// ワーカーだけを載せる — 生死は tmux の claude pid が唯一の答え(記憶ではなく実物)。
    /// Slack への問い合わせ(permalink / チャンネル名)は select ループの外でやる。
    fn user_status(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
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
    fn pwd_answer(
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
    async fn owner_command(
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

    /// Owner がまだ居ないときだけ通る、サインインの抜け道。
    /// **true = 消費した** — 呼び手はそこで打ち切る。届く場所は2つ:
    ///   • 人間の DM — 進行中のサインインがあれば次の1通を貼り付けコードと読む。素の `login` は
    ///     新しいサインイン。それ以外には短い案内を返す
    ///   • Owner が route したチャンネル — 受けるのは**貼り付けコードだけ**、しかも
    ///     そのサインインを始めた本人からのものだけ(相席の第三者にコードプロンプトを触らせない)
    pub(super) fn login_carve_out(&mut self, msg: &InboundMsg) -> bool {
        let dm = msg.channel_kind == inbound::ChannelKind::Dm;
        let sender = msg.user.as_deref().unwrap_or("");
        let pending = self.sign_in.pending.get(&msg.channel).cloned();
        if !dm && pending.as_deref() != Some(sender) {
            return false; // 通りすがりはこの枝の外 — 普通に gate へ落とす
        }
        // 返信は「その人が喋ったメッセージ」の下に吊る。DM/チャンネルの根に出すと、
        // 本人が見ているスレッドと違う場所に着く
        let reply_ts = msg.thread_ts.clone().unwrap_or_else(|| msg.ts.clone());
        let key = ThreadKey::new(&msg.channel, &reply_ts);
        let venue = if dm { "dm" } else { "channel" };
        let ch = &msg.channel;
        let ctx = LogCtx::default();
        match pending {
            Some(user) => {
                ctx.info(
                    "bridge",
                    &format!("slack-events: login code from {sender} ({venue} {ch})"),
                );
                self.submit_code(ch.clone(), msg.text.trim().to_string(), reply_ts, user);
            }
            None if crate::bridge::command::Message::new(
                &msg.text,
                self.bot_user_id.as_deref(),
            )
            .is("login") =>
            {
                ctx.info(
                    "bridge",
                    &format!("slack-events: login command from {sender} ({venue} {ch})"),
                );
                self.start_login(ch.clone(), sender.to_string(), reply_ts);
            }
            None => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: owner-less {venue} from {sender} — login guidance \
                         ({venue} {ch})"
                    ),
                );
                self.post(
                    ch,
                    &reply_ts,
                    crate::t!(
                        "Claude Code isn't signed in yet. Send `login` to sign in.",
                        "Claude Code はまだサインインしていません。`login` と送るとサインインを始めます。"
                    ),
                    &key,
                );
            }
        }
        true
    }

    /// サインインの開始。専用の tmux セッションで `claude auth login` を
    /// 回し、印字される認証 URL を掬って返す。ここから先はコード待ち。
    fn start_login(&mut self, channel: String, user: String, reply_ts: String) {
        let key = ThreadKey::new(&channel, &reply_ts);
        let ctx = LogCtx::default();
        // SECURITY: サインインは**全員で1つの** tmux セッションを使う。
        // 別チャンネルの2本目にセッションを作り直させると、1人目のポーリングが2人目の pane を
        // 読み、2人目の成功で**1人目**が Owner になる(他人の認証への相乗り)。同じチャンネルの
        // 撃ち直しは自分の流れをやり直すだけなので通す
        if let Some(other) = self.sign_in.pending.keys().find(|k| **k != channel) {
            ctx.info(
                "bridge",
                &format!(
                    "login: refusing concurrent sign-in for {user} (channel {channel}) — \
                     a sign-in is already in progress (dm {other})"
                ),
            );
            self.post(
                &channel,
                &reply_ts,
                crate::t!(
                    "Another sign-in is in progress. Wait a moment, then send `login` again.",
                    "別のサインインが進行中です。少し待ってから、もう一度 `login` と送ってください。"
                ),
                &key,
            );
            return;
        }
        ctx.info(
            "bridge",
            &format!("login: starting sign-in for {user} (channel {channel})"),
        );
        // 席は判定の**直後**に取る。取るのを spawn の中(URL が出た後)にすると、URL を待つ
        // 最大20秒の間に別チャンネルの2本目が同じ判定をすり抜け、セッションを作り直して
        // 1本目の pane を奪う。席は成否どちらでも LoginFinished が外す。
        // 代償: URL が届くまでの間にこのチャンネルへ来た1通はコード扱いになり失敗の返事になる
        // (Owner は `login` を撃ち直せばよい)
        self.sign_in.pending.insert(channel.clone(), user.clone());
        let (api, cmd_tx, home) = (self.deps.slack.clone(), self.cmd_tx.clone(), Host::home());
        // サインインは URL を出してからコードを待つ数十秒 — その間ずっと shimmer を出す
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &channel, &reply_ts, &slack::Status::Login.text());
        // `thinking` は本文で触るので async move が丸ごと持っていく(どの経路で抜けても Drop = クリア)
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let ctx = LogCtx::default();
            let started = agent.login_begin(&home);
            let mut url = None;
            if let Err(e) = started {
                ctx.error(
                    "bridge",
                    &format!(
                        "login: could not start the sign-in session for {user} \
                         (channel {channel}): {e}"
                    ),
                );
            } else {
                // 待つのは URL **だけ**。ここで広いエラー判定を回すと、URL より前の飾りに
                // 紛れた "error/failed" で誤って諦める。本物の早期失敗は
                // 「URL が出ないまま時間切れ」という形で下に現れる
                for _ in 0..URL_POLL_MAX {
                    tokio::time::sleep(LOGIN_POLL).await;
                    // URL が出るまで最大 URL_POLL_MAX 秒 — Slack はそれより早く status を
                    // 失効させるので tick ごとに張り直す(compact と同じ理由)。
                    // 張り直さないと途中で shimmer が消えて「止まった」に見える
                    thinking.set(&slack::Status::Login.text());
                    url = agent.login_url();
                    if url.is_some() {
                        break;
                    }
                }
            }
            let Some(url) = url else {
                ctx.error(
                    "bridge",
                    &format!(
                        "login: no sign-in URL captured within {URL_POLL_MAX}s for {user} \
                         (channel {channel}) — aborting"
                    ),
                );
                // 席を空けるのは main(セッションの kill も向こうがやる)。ここで返さないと
                // 二度と `login` を受け付けなくなる
                let _ = cmd_tx
                    .send(CmdFx::LoginFinished {
                        channel: channel.clone(),
                        bound: None,
                    })
                    .await;
                api.post_now(&channel, &reply_ts, crate::t!(
                        "Couldn't start the sign-in. Wait a moment, then send `login` again.",
                        "サインインを始められませんでした。少し待ってから、もう一度 `login` と送ってください。"
                    ), &key)
                .await;
                return;
            };
            api.post_now(&channel, &reply_ts, crate::t!(
                    "🔐 Open this link in your browser and sign in to Claude, then paste the code it shows as your *next message* in this thread:\n{url}",
                    "🔐 このリンクをブラウザで開いて Claude にサインインし、表示されたコードを、このスレッドの *次のメッセージ* として貼ってください:\n{url}"
                ), &key)
            .await;
            ctx.info(
                "bridge",
                &format!(
                    "login: sign-in URL sent to {user} (channel {channel}) — awaiting pasted code"
                ),
            );
        });
    }

    /// 貼られたコードを待っている CLI に流し込み、画面の判定を待つ。
    /// Owner が縛られるのは**明示の成功マーカーを見たときだけ** — 沈黙も中断も成功ではない。
    ///
    /// ponytail: CLI がシェルに戻ったことの検知(現行の paneCommand)は持たない — 30 秒の
    /// 時間切れで代替する(最悪、失敗の返事が最大 30 秒遅れるだけ)
    fn submit_code(&self, channel: String, code: String, reply_ts: String, user: String) {
        let key = ThreadKey::new(&channel, &reply_ts);
        LogCtx::default().info(
            "bridge",
            &format!("login: submitting pasted code for {user} (channel {channel})"),
        );
        let (api, cmd_tx) = (self.deps.slack.clone(), self.cmd_tx.clone());
        // サインインの後半(貼られたコードの判定、最大 CODE_POLL_MAX 秒)も待ち時間 —
        // login_start の guard は URL を出した時点で落ちているので、ここで張り直す
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &channel, &reply_ts, &slack::Status::Login.text());
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let ctx = LogCtx::default();
            let mut outcome = "timeout";
            if let Err(e) = agent.login_submit_code(&code) {
                ctx.error(
                    "bridge",
                    &format!(
                        "login: could not submit the pasted code for {user} \
                         (channel {channel}): {e}"
                    ),
                );
                outcome = "error";
            } else {
                for _ in 0..CODE_POLL_MAX {
                    tokio::time::sleep(LOGIN_POLL).await;
                    thinking.set(&slack::Status::Login.text()); // 失効させない(URL 待ちと同じ)
                    // scrollback ごと読む: "Login successful." を出した直後に CLI はシェルへ
                    // 戻り、次のポーリングまでに印が画面外へ流れる
                    match agent.login_outcome() {
                        LoginOutcome::Success => {
                            outcome = "success";
                            break;
                        }
                        LoginOutcome::Error => {
                            outcome = "error";
                            break;
                        }
                        LoginOutcome::Pending => {}
                    }
                }
            }
            // セッションの片付けと pending の削除、Owner の書き込みは main の仕事
            let (text, bound) = if outcome == "success" {
                (
                    crate::t!(
                        "Signed in ✅ — you're now the owner of this bot.",
                        "サインインしました ✅ — あなたがこのボットの Owner になりました。"
                    ),
                    Some(user),
                )
            } else {
                ctx.error(
                    "bridge",
                    &format!(
                        "login: sign-in {outcome} for {user} (channel {channel}) — no Owner bound"
                    ),
                );
                (
                    crate::t!(
                        "Sign-in failed — the code may be wrong or expired. Send `login` to try again.",
                        "サインインできませんでした。コードが違うか、期限が切れた可能性があります。`login` と送ってやり直してください。"
                    ),
                    None,
                )
            };
            let _ = cmd_tx
                .send(CmdFx::LoginFinished {
                    channel: channel.clone(),
                    bound,
                })
                .await;
            api.post_now(&channel, &reply_ts, text.to_string(), &key)
                .await;
        });
    }

    /// `logout`。CLI のサインアウトだけを
    /// spawn で回し、ワーカーの後片付けと Owner の解除は main に戻してやる。
    fn user_logout(&mut self, channel: &str, root_ts: &str) {
        // restart の札と違ってこれは**本当に効く** — user_logout は spawn を撒いてすぐ返るので、
        // `claude auth logout` が走っている数秒の間に2通目の logout が届きうる
        if self.sign_in.signing_out {
            LogCtx {
                session_id: None,
                thread_key: Some(ThreadKey::new(channel, root_ts)),
            }
            .info("bridge", "logout ignored — already signing out");
            return;
        }
        self.sign_in.signing_out = true;
        // shimmer は `claude auth logout` が返るまで。この後のワーカー畳みは main 側
        // (CmdFx::LogoutFinished)なので、ここで持たせておけば **必ず** 消える
        let thinking = slack::Thinking::new(self.deps.slack.clone(), channel, root_ts, &slack::Status::Logout.text());
        let (cmd_tx, channel, thread_ts) = (
            self.cmd_tx.clone(),
            channel.to_string(),
            root_ts.to_string(),
        );
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア
            let ctx = LogCtx::default();
            ctx.info("bridge", "logout: signing out (claude auth logout)");
            let ok = match agent.logout().await {
                Ok(()) => {
                    ctx.info("bridge", "logout: claude auth logout OK");
                    true
                }
                Err(ProbeErr::Failed(status)) => {
                    ctx.error(
                        "bridge",
                        &format!("logout: 'claude auth logout' exited {status}"),
                    );
                    false
                }
                Err(ProbeErr::Errored(e)) => {
                    ctx.error(
                        "bridge",
                        &format!("logout: 'claude auth logout' threw: {e}"),
                    );
                    false
                }
            };
            // サインアウトの成否に関わらず片付けは走らせる — 半端にワーカーだけ生き残る方が悪い
            let _ = cmd_tx
                .send(CmdFx::LogoutFinished {
                    ok,
                    channel,
                    thread_ts,
                })
                .await;
        });
    }

    /// spawn したサインイン・サインアウトが戻してきた状態変更を、main の側で1つずつ適用する。
    pub(super) async fn on_cmd_fx(&mut self, fx: CmdFx) {
        let ctx = LogCtx::default();
        match fx {
            CmdFx::LoginFinished { channel, bound } => {
                self.deps.agent.login_kill();
                self.sign_in.pending.remove(&channel);
                let Some(user) = bound else { return };
                let mut access = self.access.clone();
                access.owner.clone_from(&user);
                self.adopt_access(access, &ctx);
                ctx.info(
                    "bridge",
                    &format!("login: SUCCESS — {user} bound as Owner (channel {channel})"),
                );
                // サインインは仕切り直し — 前の認証状態で諦めた枠をもう一度試す
                // (これが無いと一時的な失敗で枠が Bridge の寿命いっぱい空く)
                if !self.workers.gave_up_count() == 0 {
                    ctx.info(
                        "bridge",
                        &format!(
                            "login: retrying {} pool(s) given up on earlier",
                            self.workers.gave_up_count()
                        ),
                    );
                }
                self.workers.clear_gave_up();
                // Owner が決まって初めて pool_targets が実体を持つ — ここが初回の在庫作り
                self.start_missing_pool_workers(&ctx);
            }
            CmdFx::LogoutFinished {
                ok,
                channel,
                thread_ts,
            } => {
                self.teardown_all_workers(
                    "logout",
                    &format!(" (claude auth logout ok={ok})"),
                    &ctx,
                )
                .await;
                let mut access = self.access.clone();
                access.owner.clear();
                self.adopt_access(access, &ctx);
                self.sign_in.pending.clear();
                self.sign_in.signing_out = false;
                ctx.info(
                    "bridge",
                    "logout: Owner cleared — bot is now Owner-less (login required)",
                );
                let key = ThreadKey::new(&channel, &thread_ts);
                self.post(
                    &channel,
                    &thread_ts,
                    crate::t!(
                        "Signed out. Send `login` to sign in again.",
                        "サインアウトしました。また使うときは `login` と送ってサインインしてください。"
                    ),
                    &key,
                );
            }
            CmdFx::SpawnScreen { outcome, what } => {
                let ctx = LogCtx::default();
                match outcome {
                    // pane は**疑い**でしかない。裏を取るのは既存の /usage 見張りの仕事なので、
                    // 次の flush で見に行くよう期限を過去に倒すだけ。**0 は使わない** —
                    // `usage_tick` が「初回なので少し待つ」の合図に使っているので、0 を書くと
                    // 確認が走らないどころか次の probe が 60 秒先送りになる
                    SpawnOutcome::UsageLimited => {
                        ctx.error(
                            "bridge",
                            &format!(
                                "spawn screen of {what} reads like the USAGE-LIMIT modal — \
                                 asking the usage monitor to confirm against /usage now \
                                 (the pane alone does NOT gate the fleet)"
                            ),
                        );
                        self.usage_polled_at_ms = 1;
                    }
                    // フリートゲートは張らない(`claude auth status` の裏取りが無い)。
                    // 言うだけ。Owner は `login` を送れる
                    SpawnOutcome::LoginRequired => {
                        ctx.error(
                            "bridge",
                            &format!(
                                "spawn screen of {what} reads like the LOGIN screen — the worker \
                                 is stuck at it and no key clears it. NOT gating the fleet \
                                 (no auth-status confirmation here yet); telling the Owner"
                            ),
                        );
                        self.post_notice(
                            &crate::t!(
                                "⚠️ An agent stopped at Claude's sign-in screen. Send `login` to sign in again.",
                                "⚠️ エージェントが Claude のサインイン画面で止まりました。`login` と送ってサインインし直してください。"
                            ),
                            &ctx,
                        )
                        .await;
                    }
                    SpawnOutcome::Answered | SpawnOutcome::NoScreen => {}
                }
            }
        }
    }

    /// `/compact` を打ち込み、pane のスピナーを Slack の付箋に流す。付箋は**最初の進捗が出てから**作る — busy / no-session の
    /// 断りが1本で済むのはそのため。最後の1行は付箋があれば書き換え、無ければ新規投稿。
    ///
    /// TUI を回すのはエージェント側(`AgentPort::compact`)。ここは**進捗を Slack に描く側**だけ —
    /// 最長6分かかるので select ループの外(spawn)で回す。
    async fn run_compact(
        api: ports::Slack,
        agent: ports::AgentRef,
        channel: String,
        root: String,
        key: ThreadKey,
        target: Window,
        sid: String,
    ) {

        let ctx = LogCtx {
            session_id: Some(sid.clone()),
            thread_key: Some(key.clone()),
        };
        // The agent sends each reading; this task draws them one at a time, in order.
        // Capacity 1 keeps the agent at most one reading ahead of the drawing.
        let (tx, mut rx) = mpsc::channel::<CompactProgress>(1);
        let draw = async {
            let mut progress_ts: Option<String> = None;
            let mut last_rendered = String::new();
            while let Some(st) = rx.recv().await {
                let rendered = st.render();
                // 同じ絵を描き直さない(Slack の編集回数はタダではない)
                if rendered == last_rendered {
                    continue;
                }
                last_rendered.clone_from(&rendered);
                let posted = match &progress_ts {
                    Some(ts) => api
                        .update_message(&channel, ts, &rendered)
                        .await
                        .map(|()| None),
                    None => api
                        .post_message_no_unfurl(&channel, &rendered, Some(&root))
                        .await
                        .map(Some),
                };
                match posted {
                    Ok(Some(ts)) => progress_ts = Some(ts),
                    Ok(None) => {}
                    // 描き損ねても圧縮は続く
                    Err(e) => ctx.error("bridge", &format!(
                        "slack-events: compact progress render failed for {channel}:{root}: {e}"
                    )),
                }
            }
            progress_ts
        };
        let (outcome, progress_ts) = tokio::join!(agent.compact(&target, &key, &sid, tx), draw);
        // None = このエージェントが compact に非対応。実体が1つの今は起きない
        let final_text = match outcome {
            Some(CompactOutcome::Done) => crate::t!("✅ Compacted the context.", "✅ コンテキストを圧縮しました。"),
            Some(CompactOutcome::Nothing) => crate::t!(
                "There isn't enough history to compact yet.",
                "圧縮するほどの履歴がまだありません。"
            ),
            Some(CompactOutcome::Failed) | None => crate::t!(
                "Couldn't compact the context. Try again.",
                "コンテキストを圧縮できませんでした。もう一度試してください。"
            ),
        };
        let posted = match &progress_ts {
            Some(ts) => api.update_message(&channel, ts, &final_text).await,
            None => api
                .post_message_no_unfurl(&channel, &final_text, Some(&root))
                .await
                .map(|_| ()),
        };
        if let Err(e) = posted {
            ctx.error(
                "bridge",
                &format!("slack-events: compact final post failed for {channel}:{root}: {e}"),
            );
        }
    }

    /// 素の `effort`。今の level を TUI に訊くのはエージェント側
    /// (`AgentPort::effort`)。ここは答えを1行にして投げるだけ。
    async fn run_effort_show(
        api: ports::Slack,
        agent: ports::AgentRef,
        channel: String,
        root: String,
        key: ThreadKey,
        target: Window,
        sid: String,
    ) {
        let failed = || crate::t!("Couldn't read the current effort level.", "今の effort を読み取れませんでした。");
        // None = 非対応か、状態行が読めなかったか — どちらも同じ断りを返す
        let out = match agent.effort(&target, &key, &sid).await {
            Some(level) => crate::t!("Effort level: *{level}*", "effort: *{level}*"),
            None => failed(),
        };
        api.post_now(&channel, &root, out, &key).await;
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
        assert_eq!(
            Cmd::value_of(&Message::new("model opus", None), "model", &MODEL_NAMES),
            Some(Some("opus".into()))
        );
        assert_eq!(
            Cmd::value_of(&Message::new("model sonet", None), "model", &MODEL_NAMES),
            Some(Some("sonnet".into()))
        );
        assert_eq!(
            Cmd::value_of(&Message::new("model", None), "model", &MODEL_NAMES),
            Some(None)
        );
        assert_eq!(
            Cmd::value_of(
                &Message::new("model の説明をして", None),
                "model",
                &MODEL_NAMES
            ),
            None
        ); // 文は素通し
        assert_eq!(
            Cmd::value_of(
                &Message::new("effort xhigh", None),
                "effort",
                &EFFORT_LEVELS
            ),
            Some(Some("xhigh".into()))
        );
        assert_eq!(
            Cmd::value_of(&Message::new("mode plan", None), "mode", &MODE_NAMES),
            Some(Some("plan".into()))
        );
        assert_eq!(
            Cmd::value_of(&Message::new("MODE", None), "mode", &MODE_NAMES),
            Some(None)
        );
        assert_eq!(
            Cmd::value_of(&Message::new("mode bypass", None), "mode", &MODE_NAMES),
            None
        ); // 引数に無い = 文
        assert_eq!(
            Cmd::value_of(&Message::new("mode を実装して", None), "mode", &MODE_NAMES),
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
        assert!(
            Message::new("restart", None).is_body_command()
                && Message::new("pwd all", None).is_body_command()
        );
        assert!(!Message::new("restart してください", None).is_body_command());
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
