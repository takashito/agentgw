//! Bridge が自分だけで答える「本文コマンド」の検出層。
//! 移植元 604-794, 840-890, 1010-1049`(すべて純関数・I/O なし)。
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
//! 4. 壁時計         `WallClock`(暦の計算・時刻のパースは全部ここ)
//! 6. 上限の見張り   `UsageWatch` / `LimitHit`
//! 7. ツール許可     `ToolPermission`

use chrono::{DateTime, Datelike, Local, Month, NaiveDate, NaiveDateTime, TimeDelta, Timelike};

use crate::agent::UsageRow;
use crate::agent::screen::MODEL_NAMES;
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
    pub(super) fn plus_minutes(&self, minutes: i64) -> Self {
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
    pub(super) fn reset_like(&self) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 壁時計1つ。テストの主役は年月日ではないので、1行で書けるようにする。
    fn wc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> WallClock {
        WallClock::new(year, month, day, hour, minute).expect("valid wall clock")
    }

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
}
