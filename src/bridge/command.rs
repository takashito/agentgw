//! Bridge が自分だけで答える「本文コマンド」の検出層。
//! 移植元 604-794, 840-890, 1010-1049`(すべて純関数・I/O なし)。
//!
//! コマンドは3層(検出 / パース / レンダ)のうちの**検出**。ここの唯一の掟は
//! 「メッセージ**全体**がそのコマンドのときだけ発動する」。
//! 語句を含むだけの文は素通しし、普通のメッセージとしてワーカーに届く。
//!
//! パース結果の型(`ContextReport` / `UsageRow` / `CompactProgress`)はエージェント共通の
//! 語彙なので `crate::agent` に置いてある — ここはその**描き方**だけ。読み方(claude の画面と
//! 出力のパース)は `agent/claude.rs` の `Pane` / `ModelId` に居る。
//!
//! ここから `agent::` を見るのは依存の向きどおり(Bridge → エージェント)。逆は無い。
//!
//! 並びは役割の順。**節1〜4 は判断、節5〜8 は文面**:
//!
//! 1. 本文の読み方   `Message`(mention と不可視文字の除去はここだけ)
//! 2. Slack の id    `SlackId`
//! 3. コマンドの検出 `Cmd` / `PwdMode` / `OwnerCmd` — 素のワードの語彙は `Cmd::WORDS` 1枚
//! 4. 壁時計         `WallClock`(暦の計算・時刻のパースは全部ここ)/ `UsageProjection`
//! 5. 描画           `ResumeInfo` / `StatusReport` / `PwdEntry` / `ContextReport` /
//!    `UsageReport` / `CompactProgress` / `RestartPhase` / `Notice`
//! 6. 上限の見張り   `UsageWatch` / `LimitHit`
//! 7. ツール許可     `ToolPermission`
//! 8. ターン失敗     `TurnFailureClass`

use chrono::{DateTime, Datelike, Local, Month, NaiveDate, NaiveDateTime, TimeDelta, Timelike};

use crate::agent::claude::{MODEL_NAMES, ModelId};
use crate::agent::{CompactProgress, ContextCategory, ContextReport, UsageRow};
use std::path::Path;

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

// ── 節4: 壁時計 ─────────────────────────────────────────────────────────────
// Bridge も TUI も同じホストの同じゾーンで動くので、ゾーン変換は要らず「同じ壁時計どうしの
// 引き算」で足りる(現行 Bun 版と同じ前提)。暦そのものは `chrono` に任せ、ここに置くのは
// 「`/usage` の書き方をどう読むか」だけ。

/// タイムゾーンを持たない壁時計(分まで — 秒は持たない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct WallClock(NaiveDateTime);

impl WallClock {
    /// タイムゾーンは **Asia/Tokyo 固定**。表示側([`Notice::Limited`])が既にそうなっており
    /// (文面に「（Asia/Tokyo）」と書いてある)、Bridge も TUI も同じホストの同じゾーンで動く。
    /// 他ゾーンへ移すならこの2箇所を一緒に直す。
    const TOKYO_OFFSET_MS: i64 = 9 * 3_600_000;

    /// 分までの壁時計を1つ。存在しない日付(2月30日など)は None。
    pub fn new(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> Option<Self> {
        NaiveDate::from_ymd_opt(year, month, day)?
            .and_hms_opt(hour, minute, 0)
            .map(Self)
    }

    /// このホストの今。**分まで**に丸める(以下すべて分の粒度で比べる)。
    pub fn now() -> Self {
        Self::of(Local::now().naive_local())
    }

    /// epoch ミリ秒 → 東京の壁時計。ずらしてから素の壁時計として読む(= +9:00 の現地時刻)。
    /// 表現できない値は epoch に落ちるが、実際の epoch ミリ秒では起きない。
    pub fn tokyo(ms: u64) -> Self {
        let shifted = ms as i64 + Self::TOKYO_OFFSET_MS;
        Self::of(
            DateTime::from_timestamp_millis(shifted)
                .unwrap_or_default()
                .naive_utc(),
        )
    }

    /// 秒以下を落として包む。
    fn of(t: NaiveDateTime) -> Self {
        Self(
            t.with_second(0)
                .and_then(|t| t.with_nanosecond(0))
                .unwrap_or(t),
        )
    }

    /// この東京の壁時計の epoch ミリ秒。
    fn epoch_ms(&self) -> Option<u64> {
        u64::try_from(self.0.and_utc().timestamp_millis() - Self::TOKYO_OFFSET_MS).ok()
    }

    /// N 分後。
    fn plus_minutes(&self, minutes: i64) -> Self {
        Self(
            self.0
                .checked_add_signed(TimeDelta::minutes(minutes))
                .unwrap_or(self.0),
        )
    }

    /// `to` が `from` 以降なら経過分、`to` が前なら None。
    pub fn minutes_to(from: &WallClock, to: &WallClock) -> Option<i64> {
        let diff = (to.0 - from.0).num_minutes();
        (diff >= 0).then_some(diff)
    }

    /// `/usage` の `resets …` 節を「次に来るその壁時計」に。TUI が出す2形
    /// (`Jun 28 at 5:30pm (Asia/Tokyo)` と裸の `5pm` / `3:59am`)を扱い、読めなければ None
    /// (呼び出し側は None を「データ不足」として安全側に倒す)。
    pub fn parse_reset(reset: &str, now: &WallClock) -> Option<WallClock> {
        let (hour, minute) = Self::clock_time(reset)?;
        if let Some((month, day)) = Self::month_day(reset) {
            // 月日あり: 今年に当て、それが過去なら来年(古い年から見た 12月→1月 の窓)
            let cand = Self::new(now.year(), month, day, hour, minute)?;
            return Some(if WallClock::minutes_to(now, &cand).is_some() {
                cand
            } else {
                Self::new(now.year() + 1, month, day, hour, minute)?
            });
        }
        // 時刻のみ: 今日のその時刻、既に過ぎていれば(現行同様ちょうど今も含めて)明日
        let cand = Self::new(now.year(), now.month(), now.day(), hour, minute)?;
        Some(
            if WallClock::minutes_to(now, &cand).is_some_and(|m| m > 0) {
                cand
            } else {
                cand.plus_minutes(24 * 60)
            },
        )
    }

    /// Claude Code が名乗ったリセット時刻を epoch(ms)にする。解釈そのものは `/usage` と
    /// 同じ [`Self::parse_reset`] に任せる — 同じ TUI の同じ書式なので、2つ持つと必ず
    /// 片方だけ直されて食い違う。
    pub fn parse_reset_epoch(text: &str, now_ms: u64) -> Option<u64> {
        Self::parse_reset(text, &Self::tokyo(now_ms))?.epoch_ms()
    }

    /// Claude Code が履歴に書く RFC3339(`2026-07-30T14:00:00.000Z`)を epoch ms に。
    fn parse_iso8601_ms(s: &str) -> Option<u64> {
        u64::try_from(DateTime::parse_from_rfc3339(s).ok()?.timestamp_millis()).ok()
    }

    /// `/usage` の `resets …` と同じ体裁(`Jul 1 at 5:00 pm`)。
    fn reset_like(&self) -> String {
        self.format("%b %-d at %-I:%M %P").to_string()
    }

    /// `\b(\d{1,2})(?::(\d{2}))?\s*(am|pm)\b` の手書き版 → 24時間制の `(hour, minute)`。
    fn clock_time(s: &str) -> Option<(u32, u32)> {
        let b = s.as_bytes();
        for i in 0..b.len() {
            // `\b` — 数字の直前が語構成文字なら、そこは数の途中(regex も開始しない)
            if !b[i].is_ascii_digit() || (i > 0 && Self::is_word(b[i - 1] as char)) {
                continue;
            }
            let digits = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
            if digits > 2 {
                continue; // `\d{1,2}` の後ろに数字は続けられない
            }
            let mut j = i + digits;
            let mut minute = 0;
            if b.get(j) == Some(&b':')
                && b[j + 1..]
                    .iter()
                    .take(2)
                    .filter(|c| c.is_ascii_digit())
                    .count()
                    == 2
            {
                minute = s[j + 1..j + 3].parse().unwrap_or(0);
                j += 3;
            }
            while b.get(j).is_some_and(u8::is_ascii_whitespace) {
                j += 1;
            }
            let Some(tag) = s.get(j..j + 2) else { continue };
            let pm = tag.eq_ignore_ascii_case("pm");
            if !pm && !tag.eq_ignore_ascii_case("am") {
                continue;
            }
            if b.get(j + 2).is_some_and(|c| Self::is_word(*c as char)) {
                continue; // `spam` の `am` は am ではない
            }
            let hour12: u32 = s[i..i + digits].parse().unwrap_or(0);
            if !(1..=12).contains(&hour12) {
                return None; // 現行と同じく「壊れた時刻」は諦める(次の候補を探さない)
            }
            return Some((
                match (hour12, pm) {
                    (12, false) => 0,
                    (12, true) => 12,
                    (h, true) => h + 12,
                    (h, false) => h,
                },
                minute,
            ));
        }
        None
    }

    /// `\b([A-Za-z]{3,})\s+(\d{1,2})\b` の**最初の**一致を月名として読む。現行同様、最初の
    /// 一致が月名でなければ(`tomorrow 8am`)そこで諦めて「時刻のみ」に落とす。
    fn month_day(s: &str) -> Option<(u32, u32)> {
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if !b[i].is_ascii_alphabetic() || (i > 0 && Self::is_word(b[i - 1] as char)) {
                i += 1;
                continue;
            }
            let word = b[i..]
                .iter()
                .take_while(|c| c.is_ascii_alphabetic())
                .count();
            let mut j = i + word;
            if word < 3 || !b.get(j).is_some_and(u8::is_ascii_whitespace) {
                i += word;
                continue;
            }
            while b.get(j).is_some_and(u8::is_ascii_whitespace) {
                j += 1;
            }
            let digits = b[j..].iter().take_while(|c| c.is_ascii_digit()).count();
            // `\d{1,2}\b` — 3桁以上、または数字の直後が語構成文字なら一致しない
            if digits == 0
                || digits > 2
                || b.get(j + digits).is_some_and(|c| Self::is_word(*c as char))
            {
                i = j;
                continue;
            }
            let month = s[i..i + 3].parse::<Month>().ok()?.number_from_month();
            return Some((month, s[j..j + digits].parse().ok()?));
        }
        None
    }

    /// regex の `\w`(語構成文字)。`\b` の判定は両側をこれで見る。
    fn is_word(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_'
    }
}

/// `year()` / `hour()` / `format()` — 暦の読み書きは `chrono` のものをそのまま使う。
impl std::ops::Deref for WallClock {
    type Target = NaiveDateTime;
    fn deref(&self) -> &NaiveDateTime {
        &self.0
    }
}

/// 1回の `/usage` 読み取りから立てたバーンレート予測。
///
/// `enough_data` が false のときは(現行同様)警告もロックもしてはいけない。
/// `at_risk` = 窓が reset する**前**に 100% に達する見込み。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageProjection {
    pub enough_data: bool,
    pub at_risk: bool,
    pub projected_hit: Option<WallClock>,
}

impl UsageProjection {
    /// 週の窓(7日)。
    pub const WEEK_MINUTES: i64 = 7 * 24 * 60;

    /// 予測ノイズガード: これだけ経っていない/使われていない窓は「データ不足」。
    const MIN_ELAPSED_MINUTES: i64 = 30;
    const MIN_PCT: f64 = 5.0;

    /// 判断材料が足りないときの答え。**警告もロックもしない。**
    const NONE: UsageProjection = UsageProjection {
        enough_data: false,
        at_risk: false,
        projected_hit: None,
    };

    /// 窓の開始 = reset − window、burn = pct / 経過分、上限到達 = now + 残り% / burn。
    /// reset が過去(= `minutes_to` が None)なら読みが壊れているので安全側に倒す。
    pub fn of(
        pct: f64,
        reset: &WallClock,
        now: &WallClock,
        window_minutes: i64,
    ) -> UsageProjection {
        let Some(to_reset) = WallClock::minutes_to(now, reset) else {
            return Self::NONE;
        };
        let elapsed = window_minutes - to_reset;
        if elapsed < Self::MIN_ELAPSED_MINUTES || pct < Self::MIN_PCT {
            return Self::NONE;
        }
        let burn = pct / elapsed as f64;
        if !burn.is_finite() || burn <= 0.0 {
            return Self::NONE;
        }
        let to_limit = (100.0 - pct).max(0.0) / burn;
        let hit = now.plus_minutes(to_limit.round() as i64);
        UsageProjection {
            enough_data: true,
            at_risk: WallClock::minutes_to(&hit, reset).is_some_and(|m| m > 0),
            projected_hit: Some(hit),
        }
    }
}

impl UsageRow {
    /// この行を何分の窓で予測するか(予測しない行は None)。
    pub fn window_minutes(label: &str, session_minutes: i64) -> Option<i64> {
        let l = label.to_ascii_lowercase();
        if l.contains("current session") {
            Some(session_minutes)
        } else if l.contains("current week") {
            Some(UsageProjection::WEEK_MINUTES)
        } else {
            None
        }
    }
}

// ── 節5: 描画(すべて純関数・I/O なし) ──────────────────────────────────────
// ここから下はワーカーを介さず Bridge が自分で返す文面。**ユーザーに見える文字列は
// 現行 Bun 版からの原文コピー**(切替日に文面が変わらないこと自体が互換の約束)。

/// `resume` の材料。
pub struct ResumeInfo {
    pub session_id: Option<String>,
    /// セッションの履歴が置かれた場所(= ワーカーの起動 cwd)
    pub cwd: Option<String>,
    /// 履歴がディスク上に見つからなかった → 再開はまず失敗する
    pub transcript_missing: bool,
    /// 今このスレッドでワーカーが生きている → 手元に線を渡したら終了する
    pub worker_running: bool,
}

impl ResumeInfo {
    /// `resume` の答えを Slack mrkdwn で。
    pub fn render(&self) -> String {
        let info = self;
        let Some(sid) = info.session_id.as_deref().filter(|s| !s.is_empty()) else {
            return crate::t!(
                "This thread has no session to resume yet. Send a message to start one first.",
                "このスレッドには、まだ再開できるセッションがありません。先にメッセージを送ってセッションを始めてください。"
            );
        };
        let cmd = match info.cwd.as_deref().filter(|c| !c.is_empty()) {
            Some(cwd) => format!("cd {cwd} && claude --resume {sid}"),
            None => format!("claude --resume {sid}"),
        };
        let mut lines = vec![
            crate::t!(
                "Run this to continue the session in your own terminal:",
                "手元の端末でセッションを続けるには、次を実行してください。"
            ),
            String::new(),
            "```".to_string(),
            cmd,
            "```".to_string(),
        ];
        if info.transcript_missing {
            lines.push(String::new());
            lines.push(crate::t!(
                "⚠️ This session's history isn't on disk, so resuming may fail.",
                "⚠️ このセッションの履歴がディスクに見つからないので、再開できないかもしれません。"
            ));
        }
        if info.worker_running {
            lines.push(String::new());
            lines.push(crate::t!(
                "The agent for this thread has been stopped so the session can continue there.",
                "続きを手元で進められるよう、このスレッドのエージェントは止めました。"
            ));
        }
        lines.join("\n")
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

// ── `help` — 全本文コマンドの静的な索引 ──────────────────

// ── `pwd` の描画 ────────────────────────────────────────

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

// ── `context` / `ctx` — このスレッド自身のコンテキスト内訳 ─
// 出力の**読み取り**は `agent/claude.rs` の `Pane::context_report`(画面と出力を読むのは
// エージェント実体の仕事)。ここに残るのは Slack へ出す**描き方**だけ。

/// パース済み `/context` を Slack mrkdwn で: 人が読めるモデル名 + 等幅の使用バー + 桁揃えの内訳表。
/// 表は生の `Free space` 行の代わりに **`Used space` の合計行**で閉じる(バーと同じ「使った側」を
/// 見せるため)。使用率は `100 − Free space` を優先する — ヘッダの整数 `4%` より1桁細かい。
impl ContextReport {
    /// JS の `parseFloat` 相当 — 先頭の数値部分だけ読む(`"95.6%"` → 95.6)。読めなければ NaN。
    /// 指数表記は扱わない: /context が印字するのは十進のパーセントとトークン数だけ。
    fn leading_f64(s: &str) -> f64 {
        let t = s.trim_start();
        let mut end = 0;
        let mut seen_dot = false;
        for (i, c) in t.char_indices() {
            match c {
                '+' | '-' if i == 0 => {}
                '.' if !seen_dot => seen_dot = true,
                _ if c.is_ascii_digit() => {}
                _ => break,
            }
            end = i + c.len_utf8();
        }
        t[..end].parse().unwrap_or(f64::NAN)
    }

    pub fn render(&self) -> String {
        let r = self;
        let mut lines = vec![
            "📊 *Context Usage*".to_string(),
            ModelId::new(&r.model).friendly(&r.total),
        ];

        let free_pct = r
            .categories
            .iter()
            .find(|(name, _, _)| name.eq_ignore_ascii_case("free space"))
            .map_or(f64::NAN, |(_, _, pct)| Self::leading_f64(pct));
        let used_pct = if free_pct.is_finite() {
            100.0 - free_pct
        } else {
            Self::leading_f64(&r.pct)
        };
        let used_int = if used_pct.is_finite() {
            used_pct.round() as i64
        } else {
            0
        };
        lines.push(format!(
            "`{}`  {} / {} ( {used_int}% used )",
            UsageReport::bar(used_pct, 24),
            r.used,
            r.total
        ));

        // 表: 消費側のカテゴリ(生の Free space は落とす)+ `Used space` の合計行
        let consumers: Vec<&ContextCategory> = r
            .categories
            .iter()
            .filter(|(name, _, _)| !name.eq_ignore_ascii_case("free space"))
            .collect();
        let used_row_pct = if used_pct.is_finite() {
            format!("{used_pct:.1}%")
        } else {
            r.pct.clone()
        };
        // 桁は「消費側の全行 + 合計行 + 最低幅」の最大
        let width = |min: usize, f: fn(&ContextCategory) -> &String, own: &str| {
            consumers
                .iter()
                .map(|c| f(c).chars().count())
                .chain([min, own.chars().count()])
                .max()
                .unwrap_or(min)
        };
        let name_w = width(10, |c| &c.0, "Used space");
        let tok_w = width(6, |c| &c.1, &r.used);
        let pct_w = width(5, |c| &c.2, &used_row_pct);
        let row = |n: &str, t: &str, p: &str| format!("{n:<name_w$}  {t:>tok_w$}  {p:>pct_w$}");
        let divider = "─".repeat(name_w + tok_w + pct_w + 4);

        let mut body = vec![
            "```".to_string(),
            row("Category", "Tokens", "%"),
            divider.clone(),
        ];
        body.extend(consumers.iter().map(|(n, t, p)| row(n, t, p)));
        body.push(divider);
        body.push(row("Used space", &r.used, &used_row_pct));
        body.push("```".to_string());
        lines.push(body.join("\n"));
        lines.join("\n")
    }
}

// ── `usage` / `usg` — アカウントのサブスク上限の要約 ────
// スレッド単位ではなく **bot アカウント**の使用状況。バーンレート予測(projection)は
// この実装では出さない — ラベル + バー + `Resets …` まで。

// 上限行の**読み取り**は `agent/claude.rs` の `Pane::usage_rows`。ここは描き方だけ。

// ── `compact` — 進捗行を描く ──────────────────────────
// 圧縮中の pane を**読む**のは `agent/claude.rs` の `Pane::compact_progress`。

/// 描くバーの幅と、その上を流れる光る窓の幅。
const CELLS: usize = 24;
const WINDOW: usize = 5;

/// 圧縮の状態を Slack の進捗行に。🗜️ のラベル + 経過時間、pane が本物の % を出していれば
/// そこまで満たした**確定**バー(% はバーの**後ろ** — 実 TUI のバー行 `▐▏███…░ 31%` に合わせる)。
/// % が無い時はトークン数と、経過秒とともに光る窓が進む(そして巻き戻る)不定バーに落ちる。
impl CompactProgress {
    pub fn render(&self) -> String {
        let p = self;
        let s = p.seconds.unwrap_or(0);
        let elapsed = match p.seconds {
            None => String::new(),
            Some(_) if s >= 60 => format!(" {}m{}s", s / 60, s % 60),
            Some(_) => format!(" {s}s"),
        };
        if let Some(pct) = p.percent {
            // 確定: /usage が描くのと同じバー。% はその後ろ
            let pct = pct.min(100);
            let bar = UsageReport::bar(f64::from(pct), CELLS);
            return crate::t!(
                "🗜️ Compacting the context…{elapsed}\n`{bar}` {pct}%",
                "🗜️ コンテキストを圧縮中…{elapsed}\n`{bar}` {pct}%"
            );
        }
        // 不定: 経過秒とともにバーの上を流れる光る窓
        let tok = match &p.tokens {
            // 矢印が無ければ**空文字**(現物の `?? ''`)。`Option<char>` の既定は `'\0'` なので使えない
            Some(t) => format!(
                " · {}{t} tokens",
                p.tokens_dir.map(String::from).unwrap_or_default()
            ),
            None => String::new(),
        };
        let pos = s as usize % CELLS;
        let bar: String = (0..CELLS)
            .map(|i| {
                if (i + CELLS - pos) % CELLS < WINDOW {
                    '█'
                } else {
                    '░'
                }
            })
            .collect();
        crate::t!(
            "🗜️ Compacting the context…{elapsed}{tok}\n`{bar}`",
            "🗜️ コンテキストを圧縮中…{elapsed}{tok}\n`{bar}`"
        )
    }
}

// ── `restart` の進捗チェックリスト ────────────────────
// Owner のスレッドに**1本**投稿し、再起動が進むごとにその場で編集する。
// 形を決めている制約: 再起動は2つの Bridge プロセスをまたぐ。古い方が最初の数段を進めて
// 降り(Slack に何も繋がっていない数秒間があり、launchd が最新コードで起こし直す)、
// **後継**が残りを終える(どのメッセージを編集するかは restart マーカーから知る)。だから
// 両者が「どこまで進んだか」という1つの値から同じ固定チェックリストを描く — 表示は跳ねない。
//
// 現行との**意図的な差**: 版注記(`Bridge を停止 (vX)` / `…復帰(vX)`)は落とす —
// この実装には版報告の機構が部品ごと無い。

/// 再起動の段。`Done`/`Failed` は終端で、行ではない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPhase {
    /// Owner の restart を受理した(最初の投稿)
    Received,
    /// **古い** Bridge が止まる(launchd が起こし直す)
    Switching,
    /// **後継** Bridge が Slack に繋ぎ直した
    Online,
    /// 再起動完了(全行 done)
    Done,
    /// 再起動が中断(進行中の行を failed に)
    Failed,
}

impl RestartPhase {
    /// チェックリストの1行の文言。
    fn step_label(self) -> String {
        match self {
            RestartPhase::Received => crate::t!("Restart requested", "再起動を受け付けました"),
            RestartPhase::Switching => crate::t!("Stopping agentgw", "agentgw を止めています"),
            RestartPhase::Online => crate::t!("agentgw is back online", "agentgw がオンラインに戻りました"),
            RestartPhase::Done | RestartPhase::Failed => String::new(),
        }
    }

    /// 与えられた進捗点でチェックリスト全体を描く:
    ///   - `completed_through` までの行 → `•`(done)
    ///   - その次の1行 → `◌ …`(進行中)、`failed_reason` があれば `💥`
    ///   - それ以降 → `◌`(未着手)
    ///   - done なら末尾に `✅ 再起動が完了しました`、失敗なら `💥 再起動に失敗しました — <理由>`
    ///
    /// `RestartPhase::Done` は全行 done、`Failed` は**最初の行**を failed に。
    pub fn render(&self, failed_reason: Option<&str>) -> String {
        let completed_through = *self;
        let failed = completed_through == RestartPhase::Failed || failed_reason.is_some();
        // done な行の**本数**。途中での失敗は「最後に done だった段 + failed_reason」で表され
        // (その次の行が 💥 になる)、`Failed` そのものは何もしないうちに落ちた退化ケース。
        let done_count = match completed_through {
            RestartPhase::Done => RESTART_STEPS.len(),
            RestartPhase::Failed => 0,
            p => RESTART_STEPS
                .iter()
                .position(|k| *k == p)
                .map_or(0, |i| i + 1),
        };
        let active = (completed_through != RestartPhase::Done).then_some(done_count);

        // 1行は `<glyph> <label>` — 字下げはしない
        let mut lines: Vec<String> = RESTART_STEPS
            .iter()
            .enumerate()
            .map(|(i, step)| {
                let label = step.step_label();
                if i < done_count {
                    format!("• {label}")
                } else if active == Some(i) {
                    if failed {
                        format!("💥 {label}")
                    } else {
                        format!("◌ {label}…")
                    }
                } else {
                    format!("◌ {label}")
                }
            })
            .collect();
        if completed_through == RestartPhase::Done {
            lines.push(crate::t!("✅ Restart complete", "✅ 再起動が完了しました"));
        } else if failed {
            let why = failed_reason.map(|r| format!(" — {r}")).unwrap_or_default();
            lines.push(crate::t!("💥 Restart failed{why}", "💥 再起動に失敗しました{why}"));
        }
        lines.join("\n")
    }
}

/// 固定の行の集合、順番どおり。`completed_through` は**完全に done な最後の行**を指し、
/// その次の行が進行中。両 Bridge がこの表を共有するのでメッセージは跳ねない。
/// Bun の表にあった「Bridge を更新」と「中断していたスレッドの処理を再開」は**持たない**:
/// Rust はバイナリ1個で更新機能が無く(差し替えは install script の仕事)、自動再開も無いので、
/// どちらも毎回「何もせず done」になる飾りだった。
const RESTART_STEPS: [RestartPhase; 3] = [
    RestartPhase::Received,
    RestartPhase::Switching,
    RestartPhase::Online,
];

// ── 節6: 上限の見張り───────────

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

/// Claude Code 自身が「上限に当たった」と書いた記録。
#[derive(Debug, PartialEq, Eq)]
pub struct LimitHit {
    /// 記録の本文(ログにそのまま出す — 何を読んで判断したかが残る)。
    pub detail: String,
    /// 壁が解ける時刻(epoch ms)。
    pub reset_ms: u64,
}

impl LimitHit {
    /// 履歴の末尾から**上限のエラーだけ**を拾う(`limitErrorInTranscriptTail`)。
    ///
    /// 後から来た無関係な api-error(混雑の一時障害)が、まだ効いている上限を隠してはいけないので
    /// 上限の記録だけを残す。読めないリセット時刻はエラー時刻 +1時間として**保守的に**縛り、
    /// それも過ぎていれば窓は既に開いた = ただの履歴なので `None`。
    pub fn in_transcript_tail(tail: &str, now_ms: u64) -> Option<LimitHit> {
        let mut latest: Option<(String, u64)> = None;
        for line in tail.lines() {
            // 安い前段の網 — 256KB を JSON にするのが高い。この旗は滅多に立たない
            if !line.contains("\"isApiErrorMessage\"") {
                continue;
            }
            // 末尾スライスは行の途中から始まるし、いま書かれかけの行は半端 — どちらも飛ばす
            let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if rec["isApiErrorMessage"] != serde_json::Value::Bool(true) {
                continue;
            }
            let text = match &rec["message"]["content"] {
                serde_json::Value::Array(a) => a
                    .iter()
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                serde_json::Value::String(s) => s.clone(),
                _ => String::new(),
            };
            if !Self::says_limit_reached(&text) {
                continue;
            }
            let at = rec["timestamp"]
                .as_str()
                .and_then(WallClock::parse_iso8601_ms);
            latest = Some((text, at.unwrap_or(now_ms)));
        }
        let (detail, at) = latest?;
        let reset_ms = WallClock::parse_reset_epoch(&detail, at).unwrap_or(at + 3_600_000);
        (reset_ms > now_ms).then_some(LimitHit { detail, reset_ms })
    }

    /// 現行 `LIMIT_MODAL_PROMPTS` の**アンカー無しの部分一致**と等価な literal 群。
    /// 先頭の `(?:…)?` は「空でもよい」ので部分一致テストの結果を変えない — だから落とせる。
    ///
    /// ⚠️ **pane 検出に流用しないこと。** 現行は同じ表を**行頭アンカー付き**でも使う
    /// (`paneShowsPrompt` — 画面は自分のプロンプトを行として印字するので、文の途中に同じ語が
    /// 出てくる「内容」と区別できる)。その検出を移植するときは還元をやり直す必要がある。
    fn says_limit_reached(text: &str) -> bool {
        let t = text.to_ascii_lowercase();
        if t.contains("wait for limit to reset") || t.contains("wait for the limit to reset") {
            return true;
        }
        ["session", "usage", "weekly"].iter().any(|w| {
            t.contains(&format!("hit your {w} limit"))
                || t.contains(&format!("{w} limit reached"))
                || t.contains(&format!("you've reached your {w} limit"))
                || t.contains(&format!("youve reached your {w} limit"))
        })
    }
}

// ── 節7: ツール許可 — 人に訊く前に効く常設の規則 ─────────
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

// ── 節8: ターン失敗の分類 — 級と文面は**1つの表** ─────
// 級(retry / tell-user)と文面は1つの判断の2つの面。2枚の表に分けると片方だけが直されて
// 食い違うので、現行はここを1枚に統合してある。**文面は原文コピー**(互換の約束)。

/// ターン失敗の扱い。`Retry` は「未応答をもう一度配達する」級。
///
/// **再配達自体はまだ無い**(別コミット)ので、いまはどちらも文面を出す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnFailureClass {
    Retry,
    TellUser,
}

impl TurnFailureClass {
    /// `error` フレームの `error_type` を**部分一致・大小無視**で引く表。
    /// (`error_type` に含まれる語, 扱い, 英語, 日本語)
    const TABLE: &'static [(&'static str, TurnFailureClass, &'static str, &'static str)] = &[
        (
            "rate_limit",
            TurnFailureClass::Retry,
            "Claude is busy right now, so no reply was written. Wait a moment and send it again.",
            "Claude が混み合っていて、返信を書けませんでした。少し待ってからもう一度送ってください。",
        ),
        (
            "overloaded",
            TurnFailureClass::Retry,
            "Claude had a temporary problem, so no reply was written. Wait a moment and send it again.",
            "Claude 側の一時的な不具合で、返信を書けませんでした。少し待ってからもう一度送ってください。",
        ),
        (
            "server_error",
            TurnFailureClass::Retry,
            "Claude had a temporary problem, so no reply was written. Wait a moment and send it again.",
            "Claude 側の一時的な不具合で、返信を書けませんでした。少し待ってからもう一度送ってください。",
        ),
        (
            "authentication_failed",
            TurnFailureClass::TellUser,
            "Claude Code is signed out, so no reply was written. DM the bot `login` to sign in again.",
            "Claude Code のサインインが切れていて、返信を書けませんでした。ボットに `login` と DM してサインインし直してください。",
        ),
        (
            "oauth_org_not_allowed",
            TurnFailureClass::TellUser,
            "Your organization doesn't allow this account to use Claude Code, so no reply was written. Ask your admin.",
            "組織の設定で、このアカウントは Claude Code を使えません。返信を書けなかったので、管理者に確認してください。",
        ),
        (
            "billing_error",
            TurnFailureClass::TellUser,
            "There's a billing problem with your Claude account, so no reply was written. Check your billing settings.",
            "Claude のアカウントの支払いに問題があり、返信を書けませんでした。支払いの設定を確認してください。",
        ),
        (
            "max_output_tokens",
            TurnFailureClass::TellUser,
            "The answer hit the output limit before it was finished. Ask for a narrower part.",
            "答えが長すぎて、出力の上限を超えました。範囲を絞って聞き直してください。",
        ),
        (
            "invalid_request",
            TurnFailureClass::TellUser,
            "The request was too large (or invalid). Run `compact` to shrink the conversation, or send a shorter message.",
            "依頼が大きすぎるか、正しくありません。`compact` で会話を圧縮するか、短くして送り直してください。",
        ),
        (
            "model_not_found",
            TurnFailureClass::TellUser,
            "The selected model doesn't exist, so no reply was written. Check the model with `model`.",
            "選んだモデルが見つからず、返信を書けませんでした。`model` でモデルを確認してください。",
        ),
    ];

    /// ターン失敗を分類し、**同時に**人が動ける言葉にする(`turnFailure`)。
    ///
    /// 10番目の `unknown`、そして知らない型・**空の型**はすべて `Retry` に落ちる — 空は実際に
    /// 起きる(2026-07-14 に実機で観測、次のターンは同じ資格で成功した)。何も飲み込まない:
    /// 生のキーワードは唯一の手掛かりなので文面に残す。
    pub fn of(reason: &str) -> (TurnFailureClass, String) {
        let r = reason.to_lowercase();
        if let Some((_, klass, en, ja)) = Self::TABLE.iter().find(|(needle, ..)| r.contains(needle)) {
            let text = match crate::i18n::lang() {
                crate::i18n::Lang::En => en,
                crate::i18n::Lang::Ja => ja,
            };
            return (*klass, (*text).to_string());
        }
        (
            TurnFailureClass::Retry,
            if reason.is_empty() {
                crate::t!(
                    "No reply was written. Send it again.",
                    "返信を書けませんでした。もう一度送ってください。"
                )
            } else {
                crate::t!(
                    "No reply was written (`{reason}`). Send it again.",
                    "返信を書けませんでした(`{reason}`)。もう一度送ってください。"
                )
            },
        )
    }
}

impl std::fmt::Display for TurnFailureClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Retry => "retry",
            Self::TellUser => "tell-user",
        })
    }
}

/// Bridge が自発的に出す通知。文言は現行と1文字同じ。
pub enum Notice {
    /// `fleet` = このマシンが子を迎える口を持っているか。持っているときだけ `route` を載せる
    /// (現行 `formatHelpReport(remoteMode)` と同じ条件 — 振り分ける相手が居ないなら
    /// 一覧に出しても押せるものが無い)。
    Help {
        fleet: bool,
    },
    PwdDmSetRefusal,
    Limited {
        until_ms: u64,
    },
    /// 上限が近い(80% / 90% を跨いだ)。生きているスレッドに1回ずつ出す。
    UsageWarning {
        pct: u32,
        /// `/usage` が印字した reset 節(そのまま出す)。
        reset: String,
        /// このペースで使い続けたときに上限へ当たる見込みの時刻。
        projected_hit: Option<WallClock>,
    },
    Startup {
        pools: Vec<String>,
        pending: u32,
    },
    Online {
        label: String,
        connected_as: String,
        version: String,
        pid: u32,
        pools: Vec<String>,
        pending: u32,
    },
    Offline {
        label: String,
        version: String,
        pid: u32,
        reason: String,
    },
}

impl Notice {
    pub fn render(&self) -> String {
        match self {
            Notice::Help { fleet } => Self::help(*fleet),
            Notice::PwdDmSetRefusal => Self::pwd_dm_set_refusal(),
            Notice::Limited { until_ms } => Self::limited(*until_ms),
            Notice::UsageWarning {
                pct,
                reset,
                projected_hit,
            } => {
                let mut lines = vec![crate::t!(
                    "⚠️ You're close to your usage limit — current session *{pct}% used*",
                    "⚠️ 利用上限が近づいています — 今のセッションで *{pct}%* 使用"
                )];
                if !reset.is_empty() {
                    lines.push(crate::t!("Resets {reset}", "リセット: {reset}"));
                }
                if let Some(hit) = projected_hit {
                    let at = hit.reset_like();
                    lines.push(crate::t!(
                        "At this pace you'll hit the limit around {at}.",
                        "このペースだと {at} ごろに上限に達します。"
                    ));
                }
                lines.join("\n")
            }
            Notice::Startup { pools, pending } => Self::startup(pools, *pending),
            Notice::Online {
                label,
                connected_as,
                version,
                pid,
                pools,
                pending,
            } => Self::online(label, connected_as, version, *pid, pools, *pending),
            Notice::Offline {
                label,
                version,
                pid,
                reason,
            } => Self::offline(label, version, *pid, reason),
        }
    }

    /// 断りや通知に警告を添える。**警告があるときだけ**、1行1件で `⚠️ ` を付けて足す
    /// (現行の文面と1文字同じ)。
    pub fn with_warnings(message: &str, warnings: &[String]) -> String {
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
    fn help(fleet: bool) -> String {
        fn section(lines: &mut Vec<String>, title: String, rows: Vec<(&str, String)>) {
            lines.push(format!("*{title}*"));
            for (cmd, desc) in rows {
                lines.push(format!("  • `{cmd}` — {desc}"));
            }
            lines.push(String::new());
        }

        let mut lines: Vec<String> = vec![crate::t!("*agentgw commands*", "*agentgw のコマンド*"), String::new()];

        section(
            &mut lines,
            crate::t!("In this thread", "このスレッドで"),
            vec![
                ("stop", crate::t!("stop the current turn (a 🛑 reaction works too)", "実行中のターンを止める(🛑 のリアクションでも可)")),
                ("exit / bye / done", crate::t!("end this thread's agent; your next message resumes it", "このスレッドのエージェントを終える。次に書けば再開する")),
                ("compact", crate::t!("compact the context, with a progress bar", "コンテキストを圧縮する(進捗バー付き)")),
                ("model [fable|opus|sonnet|haiku]", crate::t!("show or switch the model", "モデルを見る・切り替える")),
                ("effort [low|medium|high|xhigh|max|ultracode|auto]", crate::t!("show or set the effort level", "effort を見る・決める")),
                ("mode [manual|plan|edit|auto]", crate::t!("show or switch the permission mode", "権限モードを見る・切り替える")),
                ("context / ctx", crate::t!("show this agent's context usage", "このエージェントのコンテキストの使用量")),
                ("resume", crate::t!("show the command to continue this session in your own terminal (stops the agent here)", "このセッションを手元の端末で続けるコマンドを出す(ここのエージェントは止める)")),
            ],
        );
        section(
            &mut lines,
            crate::t!("Account", "アカウント"),
            vec![
                ("login", crate::t!("sign Claude Code in to your account (in a DM)", "Claude Code を自分のアカウントでサインインする(DM で)")),
                ("logout", crate::t!("sign out and stop every agent", "サインアウトして、すべてのエージェントを止める")),
                ("usage / usg", crate::t!("show your Claude subscription usage", "Claude のサブスクリプションの使用状況")),
            ],
        );
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

    /// usage 上限中に来た依頼へ返す1本。ホスト = Asia/Tokyo はこのリポの usage 機能全体の
    /// 前提 — 固定 +9:00 で足すだけ。
    fn limited(limited_until_ms: u64) -> String {
        let at = WallClock::tokyo(limited_until_ms).format("%-m/%-d %H:%M");
        crate::t!(
            "⏸️ You've reached your Claude Code usage limit. New requests are paused until it resets around {at} (Asia/Tokyo) — send yours again after that.",
            "⏸️ Claude Code の利用上限に達しました。{at}(Asia/Tokyo)ごろのリセットまで新しい依頼は受け付けません。リセット後にもう一度送ってください。"
        )
    }

    /// home への起動通知に付く要約。
    fn startup(pools: &[String], pending_count: u32) -> String {
        let n = pools.len();
        let mut lines = vec![crate::t!("• Started {n} warm agent(s)", "• 待機用のエージェントを {n} 個起動")];
        lines.extend(pools.iter().map(|cwd| format!("  • {cwd}")));
        if pending_count > 0 {
            lines.push(crate::t!(
                "• Resumed {pending_count} unfinished thread(s)",
                "• 途中だったスレッドを {pending_count} 件再開"
            ));
        }
        lines.join("\n")
    }

    /// home への起動通知。
    fn online(
        label: &str,
        connected_as: &str,
        version: &str,
        _pid: u32,
        pools: &[String],
        pending_count: u32,
    ) -> String {
        let summary = Self::startup(pools, pending_count);
        crate::t!(
            "🟢 *{label}* is online — as {connected_as}, agentgw v{version}\n{summary}",
            "🟢 *{label}* がオンラインになりました — {connected_as} として、agentgw v{version}\n{summary}"
        )
    }

    /// home への終了通知。`reason` は止まった理由の内部の名前なので、人の言葉にしてから出す。
    fn offline(label: &str, version: &str, _pid: u32, reason: &str) -> String {
        let why = if reason.starts_with("signal:") {
            crate::t!("stopped by the system", "システムに止められたため")
        } else {
            match reason {
                "restart" => crate::t!("restarting", "再起動のため"),
                "logout" => crate::t!("signed out", "サインアウトしたため"),
                other => other.to_string(),
            }
        };
        crate::t!(
            "🔴 *{label}* is going offline ({why}) — agentgw v{version}",
            "🔴 *{label}* がオフラインになります({why})— agentgw v{version}"
        )
    }
}

/// `/usage` の答えを描くのに要るもの。
///
/// `projection` があればバーンレート予測行を足す
/// (位置 — `Resets …` の直後)。
pub struct UsageReport<'a> {
    pub rows: &'a [UsageRow],
    /// `(now, window_minutes)` — "Current session" 行に使う窓(通常 300)。
    /// "Current week" 行は `USAGE_WEEK_WINDOW_MINUTES` を使う。
    pub projection: Option<(WallClock, i64)>,
}

impl UsageReport<'_> {
    /// パース済みの `/usage` 行を Slack mrkdwn で: 太字のラベル + 等幅のバー + `<n>% used`、
    /// その下に `Resets <when>`。ラベルと reset の文言は**原文のまま**。
    pub fn render(&self) -> String {
        match self.projection {
            Some((now, w)) => Self::inner(self.rows, Some(now), w),
            None => Self::inner(self.rows, None, 0),
        }
    }

    fn inner(rows: &[UsageRow], now: Option<WallClock>, window_minutes: i64) -> String {
        let mut lines = vec!["📊 *Claude Code Usage*".to_string(), String::new()];
        for r in rows {
            let pct = r.pct.parse().unwrap_or(0.0);
            lines.push(format!("*{}*", r.label));
            lines.push(format!("`{}` {}% used", Self::bar(pct, 24), r.pct));
            if !r.reset.is_empty() {
                lines.push(format!("Resets {}", r.reset)); // 0% 行は reset を印字しない
            }
            if let Some(hit) = now.and_then(|now| {
                UsageRow::window_minutes(&r.label, window_minutes)
                    .zip(WallClock::parse_reset(&r.reset, &now))
                    .map(|(window, reset)| UsageProjection::of(pct, &reset, &now, window))
                    .filter(|p| p.enough_data && p.at_risk)
                    .and_then(|p| p.projected_hit)
            }) {
                lines.push(format!(
                    "- Expected to reach limit at : {}",
                    hit.reset_like()
                ));
            }
            lines.push(String::new());
        }
        if lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// 0–100 の百分率を固定幅の unicode バーに(/usage の TUI 画面を写したもの)。
    fn bar(pct: f64, width: usize) -> String {
        let p = if pct.is_finite() {
            pct.clamp(0.0, 100.0)
        } else {
            0.0
        };
        let filled = (p / 100.0 * width as f64).round() as usize;
        "█".repeat(filled) + &"░".repeat(width - filled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 壁時計1つ。テストの主役は年月日ではないので、1行で書けるようにする。
    fn wc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> WallClock {
        WallClock::new(year, month, day, hour, minute).expect("valid wall clock")
    }
    // 描き方のテストが読み手を1つ呼ぶ(`renders_context_report`)。パーサ本体の網は
    // `agent/claude.rs` の `mod tests` に居る。
    use crate::agent::claude::Pane;

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

    /// 現行 LIMIT_MODAL_PROMPTS をアンカー無しの literal に還元したもの。
    /// 「上限」以外の api-error は拾わない(混雑の一時エラーで壁を立てない)。
    #[test]
    fn limit_error_is_read_from_the_transcript_tail() {
        let now = 1_785_000_000_000u64;
        let line = |ts: &str, text: &str| {
            format!(
                "{}\n",
                serde_json::json!({
                    "isApiErrorMessage": true,
                    "timestamp": ts,
                    "message": { "content": [{ "text": text }] },
                })
            )
        };
        let tail = line(
            "2026-07-30T14:00:00.000Z",
            "Claude usage limit reached. Your limit will reset at 11pm",
        );
        let hit = LimitHit::in_transcript_tail(&tail, now).expect("limit hit");
        assert!(hit.detail.contains("usage limit reached"), "{hit:?}");
        assert!(hit.reset_ms > now);

        // 上限でない api-error は拾わない
        let other = line("2026-07-30T14:00:00.000Z", "API Error: overloaded_error");
        assert!(LimitHit::in_transcript_tail(&other, now).is_none());

        // 末尾スライスは行の途中から始まる — 半端な行で落ちも止まりもしないこと
        let sliced = format!("Message\",\"isApiErrorMessage\":true}}\n{tail}");
        assert!(LimitHit::in_transcript_tail(&sliced, now).is_some());

        // リセット時刻が読めなければエラー時刻 +1h。それも過ぎていれば履歴なので None
        let at = 1_785_420_000_000u64; // 2026-07-30T14:00:00Z
        let no_time = line("2026-07-30T14:00:00.000Z", "Claude usage limit reached.");
        assert_eq!(
            LimitHit::in_transcript_tail(&no_time, at + 1_800_000).map(|h| h.reset_ms),
            Some(at + 3_600_000)
        );
        assert!(LimitHit::in_transcript_tail(&no_time, at + 2 * 3_600_000).is_none());
    }

    /// 現行 parseResetToEpoch。時刻だけなら「今日のその時刻、過ぎていれば明日」。
    /// 月日が付いていればその日。タイムゾーンは Asia/Tokyo 固定(Notice::Limited と同じ前提)。
    #[test]
    fn reset_time_is_read_in_tokyo_time() {
        let now = 1_785_387_600_000u64; // 2026-07-30T05:00:00Z = 30日 14:00 JST
        // 30日 23:00 JST = 30日 14:00Z
        assert_eq!(
            WallClock::parse_reset_epoch("resets at 11pm", now),
            Some(1_785_420_000_000)
        );
        // 既に過ぎた時刻は翌日に回る(13:00 JST < 14:00 JST)
        assert_eq!(
            WallClock::parse_reset_epoch("resets at 1pm", now),
            Some(1_785_470_400_000)
        );
        // 月日つき: 2026-08-01 09:30 JST = 2026-08-01T00:30:00Z
        assert_eq!(
            WallClock::parse_reset_epoch("resets Aug 1 at 9:30am", now),
            Some(1_785_544_200_000)
        );
        // 12 時制の端。12am = 00:00 — 今日の 0 時は過ぎているので翌日 0 時 JST
        assert_eq!(
            WallClock::parse_reset_epoch("resets at 12am", now),
            Some(1_785_423_600_000)
        );
        // `\b` — 語の途中の数字は時刻ではない
        assert_eq!(WallClock::parse_reset_epoch("at11pm", now), None);
        assert_eq!(WallClock::parse_reset_epoch("resets soon", now), None);
    }

    /// 分類は**部分一致・大小無視**。表に無いものと空はどちらも retry 級に落ち、
    /// 生のキーワードは文面に残る(唯一の手掛かりなので隠さない)。
    #[test]
    fn turn_failure_classifies_and_speaks() {
        use TurnFailureClass::*;
        let (k, t) = TurnFailureClass::of("API Error: rate_limit_error");
        assert_eq!(k, Retry);
        assert!(t.contains("Claude is busy"), "{t}");

        let (k, t) = TurnFailureClass::of("BILLING_ERROR");
        assert_eq!(k, TellUser);
        assert!(t.contains("billing problem"), "{t}");

        let (k, t) = TurnFailureClass::of("unknown");
        assert_eq!(k, Retry);
        assert_eq!(t, "No reply was written (`unknown`). Send it again.");

        // 実機で起きた「型が空のまま」— 飲み込まず、キーワード無しの文面で言う
        let (k, t) = TurnFailureClass::of("");
        assert_eq!(k, Retry);
        assert_eq!(t, "No reply was written. Send it again.");
    }

    #[test]
    fn startup_summary_lists_pools_and_omits_zero_pending() {
        let out = Notice::Startup {
            pools: ["/home/orchestrator".into(), "/repo/one".into()].to_vec(),
            pending: 0,
        }
        .render();
        assert!(out.contains("Started 2 warm agent(s)"));
        assert!(out.contains("/home/orchestrator"));
        assert!(!out.contains("unfinished"));
    }

    #[test]
    fn startup_summary_includes_pending_when_nonzero() {
        let out = Notice::Startup {
            pools: [].to_vec(),
            pending: 1,
        }
        .render();
        assert!(out.contains("Resumed 1 unfinished thread(s)"));
    }

    #[test]
    fn online_notice_matches_bun_shape() {
        let out = Notice::Online {
            label: "myhost".to_string(),
            connected_as: "botname".to_string(),
            version: "1.2.3".to_string(),
            pid: 12345,
            pools: ["/repo/one".into()].to_vec(),
            pending: 0,
        }
        .render();
        assert!(out.starts_with("🟢 *myhost* is online — as botname, agentgw v1.2.3\n"), "{out}");
        assert!(out.contains("Started 1 warm agent(s)"));
    }

    #[test]
    fn offline_notice_names_the_reason() {
        let out = Notice::Offline {
            label: "myhost".to_string(),
            version: "1.2.3".to_string(),
            pid: 12345,
            reason: "restart".to_string(),
        }
        .render();
        assert_eq!(out, "🔴 *myhost* is going offline (restarting) — agentgw v1.2.3");
    }

    #[test]
    fn format_limit_reply_names_the_reset_time() {
        // 実測値(Python zoneinfo で確認): epoch ms → Asia/Tokyo の壁時計
        let out = Notice::Limited {
            until_ms: 1_782_635_400_000,
        }
        .render(); // 2026-06-28 17:30 JST
        assert!(out.contains("6/28 17:30"), "{out}");
        assert_eq!(
            out,
            "⏸️ You've reached your Claude Code usage limit. New requests are paused until it resets around 6/28 17:30 (Asia/Tokyo) — send yours again after that."
        );
        // 分が0埋めされる例
        let out = Notice::Limited {
            until_ms: 1_785_283_500_000,
        }
        .render(); // 2026-07-29 09:05 JST
        assert!(out.contains("7/29 09:05"), "{out}");
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
    fn resume_report_variants() {
        let none = ResumeInfo {
            session_id: None,
            cwd: None,
            transcript_missing: false,
            worker_running: false,
        };
        assert!(
            none.render()
                .starts_with("This thread has no session to resume yet")
        );
        let full = ResumeInfo {
            session_id: Some("sid-1".into()),
            cwd: Some("/repo".into()),
            transcript_missing: true,
            worker_running: true,
        };
        let out = full.render();
        assert!(out.contains("cd /repo && claude --resume sid-1"));
        assert!(out.contains("⚠️ This session's history isn't on disk"));
        assert!(out.contains("The agent for this thread has been stopped"));
    }

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
        let h = Notice::Help { fleet: false }.render();
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
        let h = Notice::Help { fleet: true }.render();
        assert!(h.contains("route <machine>"));
        assert!(h.contains("hand this channel to a machine"));
        // マシンが居ても他の節は変わらない
        assert!(h.contains("status") && h.contains("help / ?"));
    }

    const CONTEXT_RAW: &str = "\
some preamble\n\n**Model:** claude-opus-4-8[1m]\n**Tokens:** 43.8k / 1m (4%)\n\n\
### Estimated usage by category\n\n| Category | Tokens | % |\n| --- | --- | --- |\n\
| System prompt | 3.2k | 0.3% |\n| Messages | 40.6k | 4.1% |\n| Free space | 956k | 95.6% |\n\n\
### Custom Agents\nignored\n";

    /// 描く側の網。**読む側**(`Pane::context_report`)の網は `agent/claude.rs` に居る。
    #[test]
    fn renders_context_report() {
        let r = Pane::new(CONTEXT_RAW).context_report().unwrap();
        let out = r.render();
        assert!(out.starts_with("📊 *Context Usage*\nOpus 4.8（1M context）"));
        assert!(out.contains("43.8k / 1m ( 4% used )"));
        assert!(out.contains("Used space")); // 合計行
        assert!(!out.contains("Free space")); // 生の Free 行は出さない
    }

    #[test]
    fn renders_usage_report() {
        let rows = vec![UsageRow {
            label: "Current week (all models)".into(),
            pct: "66".into(),
            reset: "Aug 1 at 9am".into(),
        }];
        let out = UsageReport {
            rows: &rows,
            projection: None,
        }
        .render();
        assert!(out.starts_with("📊 *Claude Code Usage*"));
        assert!(out.contains("*Current week (all models)*"));
        assert!(out.contains("66% used"));
        assert!(out.contains("Resets Aug 1 at 9am"));
        assert_eq!(
            UsageReport::bar(50.0, 24),
            format!("{}{}", "█".repeat(12), "░".repeat(12))
        );
        // 範囲外は clamp、0% 行は Resets 行を出さない
        assert_eq!(UsageReport::bar(150.0, 10), "█".repeat(10));
        assert_eq!(UsageReport::bar(-5.0, 10), "░".repeat(10));
        let zero = vec![UsageRow {
            label: "Current session".into(),
            pct: "0".into(),
            reset: String::new(),
        }];
        assert!(
            !UsageReport {
                rows: &zero,
                projection: None
            }
            .render()
            .contains("Resets")
        );
    }

    fn sample_now() -> WallClock {
        wc(2026, 7, 29, 13, 0)
    }

    #[test]
    fn project_usage_not_enough_data_below_threshold() {
        let now = wc(2026, 7, 29, 10, 0);
        let reset = wc(2026, 7, 29, 15, 0);
        let p = UsageProjection::of(3.0, &reset, &now, 300); // pct<5% → enough_data=false
        assert!(!p.enough_data);
        // 窓が丸ごと未経過(reset がちょうど window 先)でも同じく取らない
        assert!(!UsageProjection::of(40.0, &reset, &now, 300).enough_data);
    }

    #[test]
    fn project_usage_at_risk_when_burn_rate_outpaces_reset() {
        let now = sample_now();
        let reset = wc(2026, 7, 29, 17, 30);
        // 経過 300-270=30分で40%消費 → burn=1.333%/min → 残り60% ÷ 1.333 = 45分後に到達
        // reset までの270分より早く枯渇する → at_risk
        let p = UsageProjection::of(40.0, &reset, &now, 300);
        assert!(p.enough_data);
        assert!(p.at_risk);
        assert_eq!(p.projected_hit, Some(wc(2026, 7, 29, 13, 45)));
        // 週窓(10080分)は経過が長く burn が緩いので、同じ %でも枯渇しないことがある
        let week_reset = wc(2026, 8, 3, 9, 0);
        let w = UsageProjection::of(20.0, &week_reset, &now, UsageProjection::WEEK_MINUTES);
        assert!(w.enough_data);
        assert!(!w.at_risk);
    }

    #[test]
    fn usage_report_appends_projection_line_only_when_at_risk() {
        let rows = vec![UsageRow {
            label: "Current session".into(),
            pct: "40".into(),
            reset: "5:30pm".into(),
        }];
        let out = UsageReport {
            rows: &rows,
            projection: Some((sample_now(), 300)),
        }
        .render();
        assert!(out.contains("Expected to reach limit at"));
        // 原文どおり `at :`(スペース+コロン+スペース)+ fmtResetLike 形
        assert!(out.contains("- Expected to reach limit at : Jul 29 at 1:45 pm"));

        // 予測できない/危なくない行には何も足さない
        let calm = vec![UsageRow {
            label: "Current session".into(),
            pct: "6".into(),
            reset: "5:30pm".into(),
        }];
        assert!(
            !UsageReport {
                rows: &calm,
                projection: Some((sample_now(), 300))
            }
            .render()
            .contains("Expected to reach limit")
        );
        let unknown = vec![UsageRow {
            label: "Current month".into(),
            pct: "40".into(),
            reset: "5:30pm".into(),
        }];
        assert!(
            !UsageReport {
                rows: &unknown,
                projection: Some((sample_now(), 300))
            }
            .render()
            .contains("Expected to reach limit")
        );
        // 週の行は 7 日窓で判定(session 窓を当てると誤判定する)
        assert_eq!(
            UsageRow::window_minutes("Current week (Fable)", 300),
            Some(UsageProjection::WEEK_MINUTES)
        );
        assert_eq!(UsageRow::window_minutes("Current session", 300), Some(300));
        assert_eq!(UsageRow::window_minutes("Current month", 300), None);
    }

    #[test]
    fn stale_command_detection() {
        // 起動(=100_000ms)より前に投稿された ts はコマンドとして死んでいる
        assert!(Cmd::Stop.stale_reason("99.000000", 100_000, 0).is_some());
        assert!(Cmd::Stop.stale_reason("101.000000", 100_000, 0).is_none());
        assert!(Cmd::Stop.stale_reason("101.000000", 100_000, 1).is_some()); // 再配達
        assert!(Cmd::Stop.stale_reason("garbage", 100_000, 0).is_none()); // fail-open
    }

    #[test]
    fn compact_progress_without_an_arrow_renders_no_nul() {
        // driver が tokens だけ持つ状態を組み得る(全フィールド pub)。現物の `?? ''` は空文字で、
        // `Option<char>::unwrap_or_default()` の `'\0'` を混ぜると不可視の NUL が Slack へ流れる
        let p = CompactProgress {
            active: true,
            seconds: Some(3),
            tokens: Some("876".into()),
            tokens_dir: None,
            percent: None,
        };
        let out = p.render();
        assert!(!out.contains('\0'));
        assert!(out.starts_with("🗜️ Compacting the context… 3s · 876 tokens\n"));
        // 矢印があれば数字の直前に付く(空白は挟まない)
        let with_dir = CompactProgress {
            tokens_dir: Some('↑'),
            ..p
        };
        assert!(
            with_dir
                .render()
                .starts_with("🗜️ Compacting the context… 3s · ↑876 tokens\n")
        );
    }

    #[test]
    fn minutes_between_same_day() {
        let from = wc(2026, 7, 29, 10, 0);
        let to = wc(2026, 7, 29, 12, 30);
        assert_eq!(WallClock::minutes_to(&from, &to), Some(150));
        assert_eq!(WallClock::minutes_to(&to, &from), None); // 過去は None
    }

    #[test]
    fn minutes_between_crosses_month_and_year() {
        let from = wc(2026, 12, 31, 23, 0);
        let to = wc(2027, 1, 1, 1, 0);
        assert_eq!(WallClock::minutes_to(&from, &to), Some(120));
    }

    #[test]
    fn minutes_between_counts_the_leap_day() {
        // 2028 はうるう年 — 2/28 → 3/1 は 2 日ぶん(2027 なら 1 日ぶん)
        let day = 24 * 60;
        let span = |year| WallClock::minutes_to(&wc(year, 2, 28, 0, 0), &wc(year, 3, 1, 0, 0));
        assert_eq!(span(2028), Some(2 * day));
        assert_eq!(span(2027), Some(day));
        // 100 で割れて 400 で割れない年はうるう年ではない(2100/2 は 28 日)
        assert_eq!(span(2100), Some(day));
    }

    #[test]
    fn parse_reset_clock_bare_time_rolls_to_tomorrow_if_past() {
        let now = wc(2026, 7, 29, 18, 0);
        let future_today = WallClock::parse_reset("11:30pm", &now).unwrap();
        assert_eq!((future_today.day(), future_today.hour()), (29, 23));
        let past_today = WallClock::parse_reset("5:30pm", &now).unwrap(); // 17:30 は既に過ぎている(now=18:00)
        assert_eq!(
            past_today.day(),
            30,
            "rolls to tomorrow when the bare time already passed"
        );
    }

    #[test]
    fn parse_reset_clock_bare_time_rolls_over_month_and_year_ends() {
        let eom = wc(2026, 6, 30, 18, 0);
        let next = WallClock::parse_reset("5pm", &eom).unwrap();
        assert_eq!(
            (next.year(), next.month(), next.day(), next.hour()),
            (2026, 7, 1, 17)
        );
        let eoy = wc(2026, 12, 31, 23, 30);
        let next = WallClock::parse_reset("11:00pm", &eoy).unwrap();
        assert_eq!((next.year(), next.month(), next.day()), (2027, 1, 1));
        // うるう年の 2/28 の翌日は 2/29
        let leap = wc(2028, 2, 28, 23, 0);
        let next = WallClock::parse_reset("10pm", &leap).unwrap();
        assert_eq!((next.month(), next.day()), (2, 29));
    }

    #[test]
    fn parse_reset_clock_dated_rolls_to_next_year_if_past() {
        let now = wc(2026, 7, 29, 0, 0);
        let past = WallClock::parse_reset("Jun 28 at 5:30pm", &now).unwrap();
        assert_eq!(
            past.year(),
            2027,
            "Jun 28 already passed this year, so it means next year's Jun 28"
        );
        let future = WallClock::parse_reset("Dec 1 at 5:30pm", &now).unwrap();
        assert_eq!(future.year(), 2026);
    }

    #[test]
    fn parse_reset_clock_shapes_and_junk() {
        let now = wc(2026, 7, 29, 9, 0);
        // 現物の全文(タイムゾーン注記つき)。12am/12pm の折り返しも現行と同じ
        let full = WallClock::parse_reset("Jul 1 at 5pm (Asia/Tokyo)", &now).unwrap();
        assert_eq!(
            (
                full.year(),
                full.month(),
                full.day(),
                full.hour(),
                full.minute()
            ),
            (2027, 7, 1, 17, 0)
        );
        assert_eq!(WallClock::parse_reset("12am", &now).unwrap().hour(), 0);
        assert_eq!(WallClock::parse_reset("12:15pm", &now).unwrap().hour(), 12);
        assert_eq!(WallClock::parse_reset("3:59AM", &now).unwrap().minute(), 59);
        // 月名でない語は「時刻のみ」に落ちる(現行 monthIdx=-1 と同じ)
        let tomorrow = WallClock::parse_reset("tomorrow 8am", &now).unwrap();
        assert_eq!((tomorrow.month(), tomorrow.day()), (7, 30));
        assert_eq!(WallClock::parse_reset("", &now), None);
        assert_eq!(WallClock::parse_reset("in 5 hours", &now), None);
        assert_eq!(WallClock::parse_reset("13pm", &now), None); // 1–12 の外
        assert_eq!(WallClock::parse_reset("5:30 spam", &now), None); // am/pm の語境界
    }

    #[test]
    fn restart_checklist_renders() {
        let first = RestartPhase::Received.render(None);
        assert!(first.starts_with("• Restart requested\n◌ Stopping agentgw…"), "{first}");
        let done = RestartPhase::Done.render(None);
        assert!(done.ends_with("• agentgw is back online\n✅ Restart complete"), "{done}");
        let failed = RestartPhase::Received.render(Some("could not restart"));
        assert!(failed.contains("💥"));
    }
}
