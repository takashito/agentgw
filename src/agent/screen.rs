//! Reading Claude Code's terminal screen: the prompts shown while it starts, the status
//! lines, `/model`, `/context` and `/usage` output. Pure text in, facts out — no tmux here.

use super::{CompactProgress, ContextReport, LoginOutcome, UsageRow};

/// `/model` にそのまま渡す形のモデル名。**claude の語彙**なので
/// ここが正 — `Claude::models` もこの表を返す。
pub const MODEL_NAMES: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];

/// スライダの状態行に出る表示レベル。`auto` は**入らない** — auto の時は解決先の
/// レベルが名乗られる。 の選択肢そのまま。
const STATUS_LEVELS: [&str; 6] = ["low", "medium", "high", "xhigh", "max", "ultracode"];

/// 権限モードのフッタ行 → こちらの呼び名。TUI は入力欄の下に
/// `⏵⏵ auto mode on (shift+tab to cycle)` の形で今のモードを名乗る
/// (cli 2.1.220 の表: default→`manual mode` / plan→`plan mode` / acceptEdits→`accept edits` /
/// auto→`auto mode` / bypassPermissions→`bypass permissions` / dontAsk→`don't ask`)。
///
/// **2026-07-31 実機**(dev ワーカーに `send-keys BTab` を4回)で巡回が
/// `auto → manual → accept edits → plan → auto` と1周することと、
/// manual でも `⏸ manual mode on` の行が出ることを確認。`(shift+tab to cycle)` の但し書きは
/// 出たり出なかったりするので目印にしない。
/// bypass / dontask は `mode` コマンドの引数には無いが、行が読めれば manual と
/// 取り違えずに済むので表に入れておく。
const MODE_LABELS: [(&str, &str); 6] = [
    ("plan mode on", "plan"),
    ("accept edits on", "edit"),
    ("auto mode on", "auto"),
    ("bypass permissions on", "bypass"),
    ("don't ask on", "dontask"),
    ("manual mode on", "manual"),
];

/// エラー側の目印(alternation 原文どおり)。
const LOGIN_ERROR_MARKERS: [&str; 9] = [
    "invalid code",
    "please make sure the full code",
    "invalid",
    "expired",
    "failed",
    "incorrect",
    "denied",
    "not valid",
    "try again",
];

/// 画面が**自分で印字した行**の頭に出うる飾り(枠線・選択子・選択肢の番号)。
/// 現行 `MODAL_LINE_DECORATION` と同じ集合。
const MODAL_DECORATION: &[char] = &[
    ' ', '\t', '│', '┃', '|', '┆', '╎', '▏', '▕', '>', '❯', '➤', '·', '•', '-', '–', '—',
];

/// 現行 `LIMIT_MODAL_PROMPTS`。
const LIMIT_PROMPTS: Prompts = &[
    (
        &["", "you've ", "you have "],
        &[
            "hit your session limit",
            "hit your usage limit",
            "hit your weekly limit",
        ],
    ),
    (
        &["", "claude ", "claude code "],
        &[
            "session limit reached",
            "weekly limit reached",
            "usage limit reached",
        ],
    ),
    (
        &["you've ", "youve "],
        &[
            "reached your session limit",
            "reached your usage limit",
            "reached your weekly limit",
        ],
    ),
    (
        &["", "stop and "],
        &["wait for limit to reset", "wait for the limit to reset"],
    ),
];

/// 現行 `LOGIN_SCREEN_PROMPTS`。
const LOGIN_PROMPTS: Prompts = &[(&[""], &["select login method:"])];

/// workspace-trust。**行頭アンカー付き**(現行は素の部分一致)。文面は 2026-08-02 に
/// 実機で撮った実物: ` ❯ 1. Yes, I trust this folder` → 飾りを落として `yes, i trust…`。
const TRUST_PROMPTS: Prompts = &[(&[""], &["yes, i trust this folder"])];

/// 起動直後の窓に出うる画面。**誰かが答えるまで claude はプロンプトを1文字も読まない。**
///
/// 現行 `classifySpawnPane` の移植。4種のうち上2つは Enter で答えられ、
/// 下2つは**答えられない**(アカウントの事情なので、何度起こし直しても同じ壁に当たる)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnScreen {
    /// claude 自身の workspace-trust。cwd が `~/.claude.json` の
    /// `projects[cwd].hasTrustDialogAccepted` に無いと出る(2026-08-02、実機)
    Trust,
    /// dev channels の確認。既定の選択肢が Yes なので Enter で通る
    Confirm,
    /// サインイン画面。Enter では消えない
    LoginRequired,
    /// 上限モーダル。Enter では消えない
    UsageLimited,
    None_,
}

/// 見張りの結末。現行 `SpawnOutcome` の移植だが、**成功の意味が違う**:
/// Rust の「起動できた」は `starting` の掛け金を最初の user_prompt が外すことで表しているので、
/// ここは「答えるべき画面に答えたか」しか言わない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnOutcome {
    /// 答えられる画面に答えた(あるいは答えた上で期限まで見ていた)
    Answered,
    /// サインイン画面。**誰も答えられない** — Owner に言うしかない
    LoginRequired,
    /// 上限モーダル。同上(裏取りは呼び手の仕事)
    UsageLimited,
    /// 期限まで何も出なかった。**失敗ではない** — 普通に立ち上がった窓がこれ
    NoScreen,
}

/// 「省略可の前置き」×「語幹」。現行の正規表現を `regex` 無しで言うための形
/// (`(?:…)?` は空も許すので前置きに `""` が入っている)。
type Prompts = &'static [(&'static [&'static str], &'static [&'static str])];

/// claude の画面(`capture-pane` の出力)と、headless 実行の出力。読むだけ。
///
/// TUI は日付が変わるとドリフトすることがある(実機で確定した2件)。
/// **ここのメソッドがそのドリフトを吸収する唯一の場所** — 見張っているのは下の `mod tests`。
pub struct Pane<'a>(&'a str);

impl<'a> Pane<'a> {
    pub fn new(text: &'a str) -> Self {
        Pane(text)
    }

    /// `claude -p "/context"` の Markdown をパース。前置きの警告文(「workspace not trusted」など)は
    /// 素通しし、Model/Tokens ヘッダが無ければ None — 呼び手は文字化けを投稿する代わりに
    /// 「読めなかった」と言える。
    /// 起動直後の窓に何が出ているか。現行 `classifySpawnPane`。
    ///
    /// **アカウントの拒絶を先に見る** — サインイン画面や上限モーダルは他の文字と同居できるし、
    /// 「遅い確認プロンプト」と取り違えて Enter を撃っても消えない(現行)。
    pub fn spawn_screen(&self) -> SpawnScreen {
        if self.0.is_empty() {
            return SpawnScreen::None_;
        }
        if shows_prompt(self.0, LOGIN_PROMPTS) {
            return SpawnScreen::LoginRequired;
        }
        if shows_prompt(self.0, LIMIT_PROMPTS) {
            return SpawnScreen::UsageLimited;
        }
        if shows_prompt(self.0, TRUST_PROMPTS) {
            return SpawnScreen::Trust;
        }
        // ここだけ素の部分一致(現行)。実物のキャプチャが無いので、
        // 飾りの形を推測してアンカーを掛けると**本物を取り逃がす**方が痛い
        if self.0.to_ascii_lowercase().contains("local development") {
            return SpawnScreen::Confirm;
        }
        SpawnScreen::None_
    }

    /// On the workspace-trust dialog: is the selected answer (the `❯` row) Yes? `None` if no
    /// row is selected or it is neither. Claude Code 2.1.276 lists "No, exit" first and
    /// selects it; older versions selected Yes.
    pub fn trust_selected_is_yes(&self) -> Option<bool> {
        let row = self.0.lines().find(|l| l.trim_start().starts_with('\u{276f}'))?.to_ascii_lowercase();
        if row.contains("yes") {
            Some(true)
        } else if row.contains("no, exit") {
            Some(false)
        } else {
            None
        }
    }

    pub fn context_report(&self) -> Option<ContextReport> {
        let raw = self.0;
        let model = header_value(raw, "**Model:**")?;
        let (used, total, pct) = tokens_header(raw)?;
        let mut categories = Vec::new();
        let mut in_table = false;
        for line in raw.split('\n') {
            let t = line.trim();
            // `/^###\s+Estimated usage by category/i`
            if t.strip_prefix("###").is_some_and(|r| {
                r.starts_with(char::is_whitespace)
                    && starts_with_ci(r.trim_start(), "Estimated usage by category")
            }) {
                in_table = true;
                continue;
            }
            if !in_table {
                continue;
            }
            if t.starts_with('#') {
                break; // 次の見出しが表の終わり
            }
            if !t.starts_with('|') {
                if !categories.is_empty() {
                    break;
                }
                continue;
            }
            // `split('|')` の両端(表の縦罫の外側)を落とす
            let parts: Vec<&str> = t.split('|').map(str::trim).collect();
            let cells = parts.get(1..parts.len() - 1).unwrap_or(&[]);
            let [name, tokens, pct, ..] = cells else {
                continue;
            };
            if name.eq_ignore_ascii_case("category") {
                continue; // ヘッダ行
            }
            if !name.is_empty() && name.chars().all(|c| c == '-') {
                continue; // markdown の区切り行
            }
            categories.push((name.to_string(), tokens.to_string(), pct.to_string()));
        }
        Some(ContextReport {
            model,
            used,
            total,
            pct,
            categories,
        })
    }

    /// `claude -p "/usage"` の平文をパース。拾うのは `<label>: <n>% used [· resets <when>]` の
    /// 上限行だけで、後ろに続く「What's contributing…」の内訳は無視する。`· resets <when>` は
    /// **任意** — 0%(リセット直後の Current session など)では印字されないが、その行も残す
    /// (落としていたのが修正)。1本も無ければ None。
    pub fn usage_rows(&self) -> Option<Vec<UsageRow>> {
        let rows: Vec<UsageRow> = self.0.split('\n').filter_map(parse_usage_line).collect();
        (!rows.is_empty()).then_some(rows)
    }

    /// モーダルが**キーボードを持っている**印。Claude Code のダイアログは種類を問わず
    /// 取り消しの案内を足元に出す(2026-08-18 実物: auto mode オンボーディングの
    /// `Enter to confirm · Esc to cancel` / `/model` の
    /// `Enter to set as default · s to use this session only · Esc to cancel`)。
    ///
    /// 走行中のターンは `esc to interrupt` で**別の語** — 動いているワーカーを詰まりと
    /// 読み違えない。判定は種類を問わない: 名前を知らないモーダルこそが沈黙の正体なので、
    /// 文言表で当てにいかない。
    pub fn modal_footer(&self) -> Option<&'a str> {
        self.0
            .split('\n')
            .find(|l| l.to_ascii_lowercase().contains("esc to cancel"))
    }

    /// TUI の生きた入力ボックス = pane の**最後の** `❯` 行。**送信済み**のコマンドも
    /// echo された `❯ /effort high` として画面に残り、打鍵途中の行と形が同じ —
    /// 位置だけが両者を分ける。
    pub fn input_line(&self) -> &str {
        self.0
            .split('\n')
            .rfind(|l| l.trim_start().starts_with('❯'))
            .unwrap_or("")
    }

    /// `cmd`(例 `/effort`)が**未送信**のまま入力ボックスに居る間だけ true。pane 全体に訊くと
    /// 上記の echo のせいで永遠に yes になる。
    pub fn still_has_command(&self, cmd: &str) -> bool {
        self.input_line().contains(cmd)
    }

    /// 入力ボックスが空 = **送信された**。空でも `❯` の行そのものは残る(後ろは半角空白では
    /// なく U+00A0 — `trim` は White_Space なのでどちらも落ちる)。長い本文は箱の中で
    /// 折り返され、`❯` の行には**その時見えている先頭行**が乗る。中身が何であれ
    /// 「空でない = まだ送られていない」は同じ。
    ///
    /// `❯` の行が1本も無いとき(許可プロンプトが箱を覆っている / 画面が読めない)も空を返す —
    /// 見えていないものに Enter を撃たないため。
    pub fn input_box_empty(&self) -> bool {
        self.input_line()
            .trim_start()
            .trim_start_matches('❯')
            .trim()
            .is_empty()
    }

    /// `/effort <level>` が効いた時に出る行。どれも level を名指しする:
    /// `Set effort level to high (saved as your default …)`、auto の `Effort level set to auto`
    /// (ここまでregex を文字列走査に)、そして**同じ level を選び直した時**の
    /// `Kept effort level as xhigh`。
    ///
    /// **原文からの意図的差分**: 3つ目の `Kept effort level as` は 2026-07-29 実機で確認した文言
    /// (`model` の `Kept model as` と対)。Bun の移植元は 2026-07-17 時点の TUI で、これを取り
    /// こぼして毎回タイムアウトしていた。
    pub fn effort_confirmed(&self, level: &str) -> bool {
        let pane = self.0;
        [
            "set effort level to",
            "effort level set to",
            "kept effort level as",
        ]
        .iter()
        .any(|marker| {
            match_indices_ci(pane, marker).into_iter().any(|i| {
                let rest = &pane[i + marker.len()..];
                let t = rest.trim_start();
                // `\s+` は1文字以上 / level の後ろは `\b`(語が続くなら別の語 — `highest`)
                t.len() < rest.len()
                    && starts_with_ci(t, level)
                    && !t[level.len()..].starts_with(is_word)
            })
        })
    }

    /// `/model <名前>` が効いた時に出る行。モデルは**フレンドリ名**で名乗り、頼んだ alias を
    /// 含む(`opus` → "Set model to Opus 4.8 …")。既にそのモデルなら "Kept model as Opus 4.8"
    /// になるが、どちらも「頼んだモデルに居る」という答え。
    pub fn model_confirmed(&self, name: &str) -> bool {
        let pane = self.0;
        let name = name.to_ascii_lowercase();
        ["set model to", "kept model as"].iter().any(|marker| {
            match_indices_ci(pane, marker).into_iter().any(|i| {
                let rest = &pane[i + marker.len()..];
                // marker の後ろの `\b` と、`[^\n]*` — 同一行の中だけを見る
                !rest.starts_with(is_word) && {
                    let line = rest.split('\n').next().unwrap_or("");
                    // name の手前は `\b`(`myopus` は opus ではない)
                    match_indices_ci(line, &name)
                        .into_iter()
                        .any(|j| !line[..j].ends_with(is_word))
                }
            })
        })
    }

    /// pane に `/effort` のスライダが出ているか(フッタのヒント行が安定した目印)。
    pub fn effort_slider_open(&self) -> bool {
        self.0.contains("←/→ to adjust")
    }

    /// **履歴のある**セッションで `/effort <level>` を出すと、現行 TUI は確定の前に確認ダイアログを
    /// 挟む(2026-07-29 実機。キャッシュが効かなくなることの断り):
    /// ```text
    /// Change effort level?
    /// This conversation is cached for the current effort level. Switching to xhigh …
    /// ❯ 1. Yes, switch to xhigh
    ///   2. No, go back
    /// ```
    /// 見出しだけを目印にする — 選択肢の行は level を含んで揺れる。**移植元(2026-07-17 時点の
    /// TUI)にはダイアログが無く**、Bun 版はこの経路を知らない。
    pub fn effort_confirm_dialog_open(&self) -> bool {
        self.0.contains("Change effort level?")
    }

    /// `/effort` のスライダを Escape で閉じた後に TUI が出す状態行から、今の effort level を読む
    /// (入力ボックスの上の右寄せ行。両方の文言を実測):
    ///     ● high · /effort            (○ のこともある — 点は変わる)
    ///     ✦ ultracode · xhigh effort + dynamic workflows for maximum thoroughness
    /// 画面に無ければ None(まだ出ていない / 別の通知に置き換わった)。
    ///
    /// **原文からの意図的差分**: `◉`(U+25C9)を追加 — 2026-07-29 の実機は `◉ xhigh · /effort`。
    /// Bun の `●○✦` は 2026-07-17 時点の TUI で、点は今後も変わり得る(足すのはここだけ)。
    pub fn effort_status(&self) -> Option<&'static str> {
        let pane = self.0;
        for (i, glyph) in pane
            .char_indices()
            .filter(|(_, c)| matches!(c, '●' | '○' | '✦' | '◉'))
        {
            let rest = pane[i + glyph.len_utf8()..].trim_start();
            for lv in STATUS_LEVELS {
                if rest
                    .strip_prefix(lv)
                    .is_some_and(|r| r.trim_start().starts_with('·'))
                {
                    return Some(lv);
                }
            }
        }
        None
    }

    /// 入力欄の**下**のフッタが名乗る権限モード。行が無ければ `manual`(既定は名乗らない)。
    ///
    /// 見るのを入力欄より下に限るのは、会話の本文に同じ語(この機能の話をした transcript)が
    /// 出ても拾わないため — `input_line` と同じ錨。
    pub fn mode_status(&self) -> &'static str {
        let foot = match self.0.rfind('❯') {
            Some(i) => &self.0[i..],
            None => self.0,
        };
        MODE_LABELS
            .iter()
            .find(|(label, _)| foot.contains(label))
            .map(|(_, name)| *name)
            .unwrap_or("manual")
    }

    /// 捕った pane をパースする。錨は生きた圧縮スピナー — `Compacting conversation` を含む行 — で、
    /// そこから読むのは:
    ///   • その行の括弧の経過タイマー `(33s)` / `(1m 4s · …)`(まだ出ていれば);
    ///   • 直下のバー行の完了率(スピナー行 + 次の2行に**限る**ので、遠くの status line の
    ///     `ctx:NN%` を拾うことは無い)。スピナー行から見るので、% が同じ行にある版も拾える。
    /// 生きたスピナーは必ずどちらかを出す。単なる静的言及(コード・コメント・この機能を引用した
    /// transcript)はどちらも持たないので、進行中とは取り違えない(初版が数分ハングしたバグ)。
    pub fn compact_progress(&self) -> CompactProgress {
        let lines: Vec<&str> = self.0.split('\n').collect();
        let Some(idx) = lines
            .iter()
            .position(|l| !match_indices_ci(l, "compacting conversation").is_empty())
        else {
            return CompactProgress::default();
        };
        let spinner = lines[idx];
        let seconds = parse_elapsed(spinner);
        let tok = parse_tokens(spinner);
        let percent = lines[idx..(idx + 3).min(lines.len())]
            .iter()
            .filter(|l| !l.is_empty() && match_indices_ci(l, "ctx:").is_empty())
            .find_map(|l| parse_percent_line(l).filter(|v| *v <= 100));
        // 生きた進捗の合図が1つも無い → ただ言葉が出てくる静的なテキスト
        if seconds.is_none() && percent.is_none() {
            return CompactProgress::default();
        }
        CompactProgress {
            active: true,
            seconds,
            tokens: tok.as_ref().map(|(_, n)| n.clone()),
            tokens_dir: tok.map(|(d, _)| d),
            percent: percent.map(|v| v as u8),
        }
    }

    /// `claude auth login` の pane から `https://claude.com/cai/oauth/authorize?…` を取り出す。
    /// URL は `visit: ` の後に印字される。**広い** pane(driver は `-x 400`)なら1行に収まるが、
    /// 狭い pane が折り返した場合、折り返しの続き行は空白を含まないので再結合する(空行・
    /// 「Paste code」のような空白入りの行・末尾で止める)。無ければ None。
    pub fn auth_login_url(&self) -> Option<String> {
        const MARKER: &str = "visit: ";
        const URL: &str = "https://claude.com/cai/oauth/authorize?";
        let pane = self.0;
        let idx = pane.find(MARKER)?;
        let mut lines = pane[idx + MARKER.len()..].split('\n');
        let mut joined = lines.next().unwrap_or("").trim().to_string();
        for seg in lines {
            let seg = seg.trim();
            if seg.is_empty() || seg.contains(char::is_whitespace) {
                break; // 空行 = URL の終わり / 空白入りは本文の行(「Paste code」)で折り返しではない
            }
            joined.push_str(seg); // soft-wrap の続き: 再結合
        }
        let i = joined.find(URL)?;
        let url: String = joined[i..]
            .chars()
            .take_while(|c| !c.is_whitespace())
            .collect();
        (url.len() > URL.len()).then_some(url) // `\S+` は1文字以上
    }

    /// `claude auth login` の実際の目印から pane を分類する(成功・失敗の実出力で確認):
    ///   success → "Login successful."(「Paste code here …」の後にインラインで出る)
    ///   invalid → "Invalid code. Please make sure the full code was copied."
    /// success を名乗るには**明示の成功マーカーが必須** — 悪いコード(やクラッシュ・中断)が
    /// 成功に見えることは無い — ので、Owner が縛られるのは本物のサインインの時だけ。
    /// pending = まだ待っている。
    pub fn login_outcome(&self) -> LoginOutcome {
        let low = self.0.to_ascii_lowercase();
        if low.contains("login successful") {
            return LoginOutcome::Success;
        }
        // `\berror\b` だけは語境界つき(`errorless` や識別子の中は拾わない)
        let bare_error = low
            .match_indices("error")
            .any(|(i, m)| !low[..i].ends_with(is_word) && !low[i + m.len()..].starts_with(is_word));
        if bare_error || LOGIN_ERROR_MARKERS.iter().any(|m| low.contains(m)) {
            LoginOutcome::Error
        } else {
            LoginOutcome::Pending
        }
    }

    /// transcript の末尾から、**最後の** assistant レコードが名乗ったモデル id を拾う
    /// (`/"model"\s*:\s*"(claude-[^"]+)"/g` を文字列走査に)。
    /// 見るのは `claude-` 始まりだけなので、エラーレコードの `"model":"<synthetic>"` は
    /// 素通りする。末尾を途中から切って渡してよい(半端な行は形が合わず無視される)。
    pub fn last_model_id(&self) -> Option<String> {
        let tail = self.0;
        let mut last = None;
        for (i, m) in tail.match_indices("\"model\"") {
            let rest = tail[i + m.len()..].trim_start();
            let Some(rest) = rest.strip_prefix(':').map(str::trim_start) else {
                continue;
            };
            let Some(rest) = rest.strip_prefix("\"claude-") else {
                continue;
            };
            // `[^"]+` — `claude-` の後ろが空なら id ではない
            let Some(end) = rest.find('"').filter(|e| *e > 0) else {
                continue;
            };
            last = Some(format!("claude-{}", &rest[..end]));
        }
        last
    }
}

/// claude のモデル識別子(`claude-opus-4-8[1m]` のような**フルの id**)。
pub struct ModelId<'a>(&'a str);

impl<'a> ModelId<'a> {
    pub fn new(id: &'a str) -> Self {
        ModelId(id)
    }

    /// フルのモデル id に含まれる親しみ名(`claude-fable-5` → `fable`)。既知の名前を
    /// どれも含まなければ None。
    pub fn alias(&self) -> Option<&'static str> {
        let lower = self.0.to_lowercase();
        MODEL_NAMES.iter().find(|n| lower.contains(**n)).copied()
    }

    /// 見出し用に人が読めるモデル名: `("claude-opus-4-8", "1m")` → `Opus 4.8（1M context）`。
    /// 窓の注記は **Tokens の total** から採る(id の `[1m]` サフィックスではなく) — このサフィックスは
    /// 1M でも出る版と出ない版があるのに対し total は必ずある。total が読めない形のときだけ
    /// サフィックスに落ちる。claude 以外の id は生のまま。
    pub fn friendly(&self, total: &str) -> String {
        let id = self.0;
        let t = total.trim();
        // `/^[\d.]+[mk]$/i` — 末尾が m/k で、その手前は数字とドットだけ
        let numeric_total = matches!(t.chars().next_back(), Some('m' | 'M' | 'k' | 'K'))
            && t.len() > 1
            && t[..t.len() - 1]
                .chars()
                .all(|c| c.is_ascii_digit() || c == '.');
        let ctx = if numeric_total {
            t.to_ascii_uppercase()
        } else {
            id_context_suffix(id).unwrap_or_default()
        };
        let note = if ctx.is_empty() {
            String::new()
        } else {
            format!("（{ctx} context）")
        };
        let base = strip_brackets(id);
        let parts: Vec<&str> = base.split('-').collect();
        let label = match parts.split_first() {
            Some((&"claude", tail)) if !tail.is_empty() => {
                let family = capitalize(tail[0]);
                let ver = tail[1..].join(".");
                if ver.is_empty() {
                    family
                } else {
                    format!("{family} {ver}")
                }
            }
            _ => base,
        };
        format!("{label}{note}")
    }
}

// ── parsing helpers ──
// 手書きの走査ヘルパ(regex を足さない)

/// ASCII 大小文字を無視した接頭辞一致(regex の `/i` の代わり)。
fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
}

/// regex の `\w`(語構成文字)。`\b` の判定は両側をこれで見る。
fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// ASCII 大小を無視した全出現位置(regex の `/i` + エンジンの位置送り相当)。
/// `to_ascii_lowercase` は ASCII しか変えないのでバイト位置は原文と同じ。`needle` は小文字で渡す。
fn match_indices_ci(hay: &str, needle: &str) -> Vec<usize> {
    hay.to_ascii_lowercase()
        .match_indices(needle)
        .map(|(i, _)| i)
        .collect()
}

/// `**Key:**` の**値**: 続く空白(改行も含む — 現物の `\s*` がそう)を飛ばした先の行。
/// 値が空なら None。
fn header_value(raw: &str, key: &str) -> Option<String> {
    let i = raw.find(key)?;
    let rest = raw[i + key.len()..].trim_start();
    let line = rest.split('\n').next().unwrap_or("").trim_end();
    (!line.is_empty()).then(|| line.to_string())
}

/// `**Tokens:** <used> / <total> (<pct>)`。 の3捕獲 regex 相当
/// 形が崩れているヘッダは飛ばして次の `**Tokens:**` を試す(regex が位置を進めるのと同じ)。
fn tokens_header(raw: &str) -> Option<(String, String, String)> {
    const KEY: &str = "**Tokens:**";
    for (i, _) in raw.match_indices(KEY) {
        let s = raw[i + KEY.len()..].trim_start();
        // `[^/\s]+` — スラッシュも空白も含まない一続き
        let used: String = s
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '/')
            .collect();
        if used.is_empty() {
            continue;
        }
        let Some(s) = s[used.len()..].trim_start().strip_prefix('/') else {
            continue;
        };
        let s = s.trim_start();
        let total: String = s
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '(')
            .collect();
        if total.is_empty() {
            continue;
        }
        let Some(s) = s[total.len()..].trim_start().strip_prefix('(') else {
            continue;
        };
        let Some(end) = s.find(')') else { continue };
        return Some((used, total, s[..end].trim().to_string()));
    }
    None
}

/// `used` の後ろの残り: `(?:.*?\bresets\s+(.+?))?\s*$` 相当。
/// `Some("")` = reset 節なしで行が終わっている / `Some(when)` = 節あり /
/// **None = 上限行ではない**(`resets` でない余計な文字が残っている)。
fn usage_reset_clause(tail: &str) -> Option<String> {
    let lower = tail.to_ascii_lowercase(); // ASCII 変換なのでバイト位置は tail と同じ
    let mut from = 0;
    while let Some(rel) = lower[from..].find("resets") {
        let i = from + rel;
        // `\b`: 直前が語構成文字なら境界でない(`presets` は resets ではない)
        let word_before = tail[..i]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let after = &tail[i + "resets".len()..];
        if !word_before && after.starts_with(char::is_whitespace) {
            return Some(after.trim().to_string());
        }
        from = i + "resets".len();
    }
    tail.trim().is_empty().then(String::new)
}

/// 1行を上限行として読む。`^\s*(.+?):\s*(\d+)%\s+used\b…$` の手書き版 —
/// ラベルは最短一致なので、**うまくパースできる最初のコロン**で切る。
fn parse_usage_line(line: &str) -> Option<UsageRow> {
    for (ci, _) in line.match_indices(':') {
        let label = line[..ci].trim();
        if label.is_empty() {
            continue; // `(.+?)` は1文字以上
        }
        let after = line[ci + 1..].trim_start();
        let pct: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if pct.is_empty() {
            continue;
        }
        let Some(rest) = after[pct.len()..].strip_prefix('%') else {
            continue;
        };
        let trimmed = rest.trim_start();
        if trimmed.len() == rest.len() {
            continue; // `\s+` は1文字以上
        }
        if !starts_with_ci(trimmed, "used") {
            continue;
        }
        let rest = &trimmed["used".len()..];
        // `used` の直後の `\b` — 語が続くなら別の語(`usedxx`)
        if rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        // ここも continue — 残りが reset 節でなければ「このコロンでの切り方が違った」だけで、
        // 現物の遅延一致は次のコロンを試す(`A: 12% used, X: 66% used` は2つ目で一致する)
        let Some(reset) = usage_reset_clause(rest) else {
            continue;
        };
        return Some(UsageRow {
            label: label.to_string(),
            pct,
            reset,
        });
    }
    None
}

/// `(33s)` / `(1m 4s · ↑ 876 tokens)` の経過秒。`\((?:(\d+)m\s*)?(\d+)s\b[^)]*\)` の手書き版 —
/// 数字は `(` の**直後**から始まる(`(elapsed 33s)` は一致しない)。
fn parse_elapsed(spinner: &str) -> Option<u32> {
    for (i, _) in spinner.match_indices('(') {
        let s = &spinner[i + 1..];
        let d1: String = s.chars().take_while(char::is_ascii_digit).collect();
        if d1.is_empty() {
            continue;
        }
        let after = &s[d1.len()..];
        // `Nm` があれば分。無ければ最初の数字がそのまま秒(regex の optional group の backtrack)
        let (mins, secs, rest) = match after.strip_prefix(['m', 'M']) {
            Some(r) => {
                let r = r.trim_start();
                let d2: String = r.chars().take_while(char::is_ascii_digit).collect();
                if d2.is_empty() {
                    continue;
                }
                (d1.parse().unwrap_or(0), d2.clone(), &r[d2.len()..])
            }
            None => (0u32, d1.clone(), after),
        };
        // `s\b` の後は `[^)]*\)` — `[^)]*` は `)` を跨げないので「後ろに `)` がある」と同義
        let Some(tail) = rest.strip_prefix(['s', 'S']) else {
            continue;
        };
        if tail.starts_with(is_word) || !tail.contains(')') {
            continue;
        }
        return Some(mins * 60 + secs.parse().unwrap_or(0));
    }
    None
}

/// スピナー行の括弧内のトークン数: `([↑↓])\s*([\d.]+[km]?)\s*tokens` 相当。
fn parse_tokens(spinner: &str) -> Option<(char, String)> {
    for (i, dir) in spinner
        .char_indices()
        .filter(|(_, c)| matches!(c, '↑' | '↓'))
    {
        let s = spinner[i + dir.len_utf8()..].trim_start();
        let mut num: String = s
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if num.is_empty() {
            continue;
        }
        let after = &s[num.len()..];
        // `[km]?` は捕獲の中(`1.6k` で1つの印字)
        let after = match after.strip_prefix(['k', 'm', 'K', 'M']) {
            Some(r) => {
                num.push_str(&after[..after.len() - r.len()]);
                r
            }
            None => after,
        };
        if starts_with_ci(after.trim_start(), "tokens") {
            return Some((dir, num));
        }
    }
    None
}

/// 1行から `(\d{1,3})\s*%` の**最初の**一致。`%` の手前の空白を飛ばして最大3桁を後ろから取る。
fn parse_percent_line(line: &str) -> Option<u32> {
    for (i, _) in line.match_indices('%') {
        let head = line[..i].trim_end();
        let digits: String = head
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if !digits.is_empty() {
            return digits.parse().ok(); // 行内の最初の一致だけ(現物も break せず次の行へ)
        }
    }
    None
}

/// 行頭の飾りを落とす。現行 `MODAL_LINE_DECORATION` と同じ順(飾り → 選択肢番号 → 空白)。
pub(super) fn strip_modal_decoration(line: &str) -> &str {
    let t = line.trim_start_matches(MODAL_DECORATION);
    let digits = t.len() - t.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return t;
    }
    match t[digits..]
        .strip_prefix('.')
        .or_else(|| t[digits..].strip_prefix(')'))
    {
        Some(rest) => rest.trim_start(),
        None => t,
    }
}

/// 「画面が**自分で**このプロンプトを印字しているか」。現行
/// `paneShowsPrompt` の移植 — **行頭アンカーが全部**。同じ語が文の途中にあるもの
/// (このリポのソース・grep の結果・引用された Slack の発言)は画面ではない。
fn shows_prompt(pane: &str, prompts: Prompts) -> bool {
    pane.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        let body = strip_modal_decoration(&lower);
        prompts.iter().any(|(prefixes, stems)| {
            prefixes.iter().any(|p| {
                body.strip_prefix(p)
                    .is_some_and(|rest| stems.iter().any(|s| rest.starts_with(s)))
            })
        })
    })
}

/// `[...]` を全部落として trim(`id.replace(/\[[^\]]*\]/g, '').trim()` 相当)。
/// 閉じない `[` は括弧でない — そのまま残す。
fn strip_brackets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('[') {
        let Some(j) = rest[i + 1..].find(']') else {
            break;
        };
        out.push_str(&rest[..i]);
        rest = &rest[i + 1 + j + 1..];
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// id の `[<n>m]` サフィックスから窓の大きさ(`"1M"`)。無ければ None。`/\[(\d+)\s*m\]/i`
fn id_context_suffix(id: &str) -> Option<String> {
    for (i, _) in id.match_indices('[') {
        let s = &id[i + 1..];
        let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            continue;
        }
        let r = s[digits.len()..].trim_start();
        if matches!(r.chars().next(), Some('m' | 'M')) && r[1..].starts_with(']') {
            return Some(format!("{digits}M"));
        }
    }
    None
}

fn capitalize(s: &str) -> String {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) => c.to_uppercase().collect::<String>() + cs.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // ── 画面と出力の読み取り ────────────────────────────────────────────────
    // ここから下は **TUI ドリフトの見張り**(実機で確定した2件)。
    // 1件も落とさないこと — 落とすと次のドリフトに気づけない。

    /// 旧 `command.rs` の同名 const(描く側のテストと同じ現物)。
    const CONTEXT_RAW: &str = "\
some preamble\n\n**Model:** claude-opus-4-8[1m]\n**Tokens:** 43.8k / 1m (4%)\n\n\
### Estimated usage by category\n\n| Category | Tokens | % |\n| --- | --- | --- |\n\
| System prompt | 3.2k | 0.3% |\n| Messages | 40.6k | 4.1% |\n| Free space | 956k | 95.6% |\n\n\
### Custom Agents\nignored\n";

    #[test]
    fn parses_context_output() {
        let r = Pane::new(CONTEXT_RAW).context_report().unwrap();
        assert_eq!(
            (
                r.model.as_str(),
                r.used.as_str(),
                r.total.as_str(),
                r.pct.as_str()
            ),
            ("claude-opus-4-8[1m]", "43.8k", "1m", "4%")
        );
        assert_eq!(r.categories.len(), 3);
        assert_eq!(r.categories[0].0, "System prompt");
        assert!(Pane::new("no headers here").context_report().is_none());
    }

    /// 1M 注記の出どころは **Tokens の total**(id の `[1m]` はある版と無い版がある)。
    #[test]
    fn context_window_note_comes_from_total() {
        assert_eq!(
            ModelId::new("claude-opus-4-8").friendly("1m"),
            "Opus 4.8（1M context）"
        );
        // total が読めないときだけ id の `[<n>m]` サフィックスに落ちる
        assert_eq!(
            ModelId::new("claude-opus-4-8[1m]").friendly("-"),
            "Opus 4.8（1M context）"
        );
        assert_eq!(
            ModelId::new("claude-haiku").friendly("200k"),
            "Haiku（200K context）"
        );
        assert_eq!(ModelId::new("gpt-4").friendly(""), "gpt-4"); // claude でなければ生の id
    }

    /// 旧 `command::model_alias` / `command::last_model_id` の網
    /// (旧 `id_shapes_and_mentions` から分離 — 残りの id 形の網は `command.rs` に居る)。
    #[test]
    fn model_id_alias_and_transcript_tail() {
        assert_eq!(ModelId::new("claude-fable-5").alias().unwrap(), "fable");
        assert_eq!(ModelId::new("gpt-4").alias(), None);
        // 勝つのは**最後の** claude- id。`<synthetic>` はモデルではない
        let tail = "{\"model\":\"claude-opus-4-5\"}\n{\"model\" : \"claude-fable-5\"}\n\
                    {\"model\":\"<synthetic>\"}\n";
        assert_eq!(
            Pane::new(tail).last_model_id().as_deref(),
            Some("claude-fable-5")
        );
        assert_eq!(
            Pane::new("{\"model\":\"<synthetic>\"}").last_model_id(),
            None
        );
        assert_eq!(Pane::new("model claude-fable-5 の話").last_model_id(), None);
    }

    #[test]
    fn parses_usage_rows() {
        let raw = "\
Opening usage…\nCurrent session (all models): 12% used · resets Jul 29 at 5pm\n\
Current week (all models): 66% used · resets Aug 1 at 9am\nCurrent session: 0% used\n\
What's contributing to your limits\nignored: 55% something\n";
        let rows = Pane::new(raw).usage_rows().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].pct, "12");
        assert_eq!(rows[2].reset, ""); // 0% 行は resets 無しでも拾う
        assert!(Pane::new("garbage").usage_rows().is_none());
        // ラベルの遅延一致は**行を捨てず**次のコロンへ進む(bun で現物の出力を確認)
        let two = Pane::new("A: 12% used, X: 66% used").usage_rows().unwrap();
        assert_eq!(two.len(), 1);
        assert_eq!(
            (
                two[0].label.as_str(),
                two[0].pct.as_str(),
                two[0].reset.as_str()
            ),
            ("A: 12% used, X", "66", "")
        );
        // 2026-07 の実物(`claude -p /usage` をそのまま貼った): タイムゾーン付きの reset と、
        // `%` とコロンを含む「What's contributing」の内訳。上限行**だけ**拾うこと
        let live = "\
You are currently using your subscription to power your Claude Code usage\n\n\
Current session: 21% used · resets Jul 29 at 12:29am (Asia/Tokyo)\n\
Current week (all models): 42% used · resets Jul 29 at 4:59pm (Asia/Tokyo)\n\n\
What's contributing to your limits usage?\n\
Last 24h · 2120 requests · 21 sessions\n\
  98% of your usage came from subagent-heavy sessions\n\
  Top skills: /superpowers:writing-plans 2%, /claude-api 1%\n";
        let rows = Pane::new(live).usage_rows().unwrap();
        assert_eq!(
            rows.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(),
            ["Current session", "Current week (all models)"]
        );
        assert_eq!(rows[0].reset, "Jul 29 at 12:29am (Asia/Tokyo)"); // 行内の `12:29` で切らない
    }

    #[test]
    fn input_line_is_the_last_prompt() {
        let pane = "❯ /effort high\nSet effort level to high (saved)\n❯ ";
        assert_eq!(Pane::new(pane).input_line().trim(), "❯"); // 送信済み echo は入力行ではない
        assert!(!Pane::new(pane).still_has_command("/effort"));
        assert!(Pane::new("❯ /effort").still_has_command("/effort"));
    }

    #[test]
    fn confirmations_match_both_wordings() {
        assert!(Pane::new("Set effort level to high (saved as default)").effort_confirmed("high"));
        assert!(Pane::new("Effort level set to auto").effort_confirmed("auto"));
        assert!(!Pane::new("Set effort level to highest").effort_confirmed("high")); // 語境界
        // 同じ level を選び直した時の3つ目の文言(2026-07-29 実機の行そのまま)
        assert!(Pane::new("Kept effort level as xhigh").effort_confirmed("xhigh"));
        assert!(!Pane::new("Kept effort level as xhigh").effort_confirmed("high")); // 語境界
        assert!(Pane::new("⏺ Set model to Opus 4.8 (claude-opus-4-8)").model_confirmed("opus"));
        assert!(Pane::new("Kept model as Opus 4.8").model_confirmed("opus"));
        assert!(!Pane::new("model: opus is nice").model_confirmed("opus"));
    }

    #[test]
    fn compact_pane_reads_live_signals_only() {
        let live = "✳ Compacting conversation… (1m 4s · ↑ 1.6k tokens)\n▐▏████░░░░ 31%\n";
        let p = Pane::new(live).compact_progress();
        assert!(p.active);
        assert_eq!((p.seconds, p.percent), (Some(64), Some(31)));
        assert_eq!(p.tokens.as_deref(), Some("1.6k"));
        // 静的な言及(タイマーも % も無い)は inactive
        assert!(
            !Pane::new("the words Compacting conversation appear in prose")
                .compact_progress()
                .active
        );
        // ctx:NN% は percent として拾わない
        let ctx = "✳ Compacting conversation… (3s)\n  ctx:42%\n";
        assert_eq!(Pane::new(ctx).compact_progress().percent, None);
    }

    /// フッタ行(2026-07-31 実機の現物)から今の権限モードを読む。
    #[test]
    fn mode_status_reads_the_footer_below_the_input_box() {
        let pane = |foot: &str| {
            format!(
                "user: mode を実装した\n\
                 ─────\n\
                 ❯ \n\
                 ─────\n\
                   ~  ctx:4%  14:38  Opus 5\n{foot}"
            )
        };
        assert_eq!(
            Pane::new(&pane("  ⏵⏵ auto mode on (shift+tab to cycle) · ← 1 agent")).mode_status(),
            "auto"
        );
        assert_eq!(
            Pane::new(&pane("  ⏸ plan mode on (shift+tab to cycle)")).mode_status(),
            "plan"
        );
        assert_eq!(
            Pane::new(&pane("  ⏵⏵ accept edits on (shift+tab to cycle)")).mode_status(),
            "edit"
        );
        assert_eq!(
            Pane::new(&pane("  ⏸ manual mode on · ← 1 agent")).mode_status(),
            "manual"
        );
        // 名乗る行が無ければ manual(将来また出なくなっても既定に落ちる)
        assert_eq!(Pane::new(&pane("")).mode_status(), "manual");
        // 会話の本文に同じ語が出ても入力欄より上なので拾わない
        assert_eq!(
            Pane::new(&pane("").replace("mode を実装した", "plan mode on の話")).mode_status(),
            "manual"
        );
    }

    #[test]
    fn effort_status_and_slider() {
        assert!(Pane::new("… ←/→ to adjust …").effort_slider_open());
        assert_eq!(
            Pane::new("  ● high · /effort").effort_status(),
            Some("high")
        );
        assert_eq!(
            Pane::new("✦ ultracode · xhigh effort + …").effort_status(),
            Some("ultracode")
        );
        // 2026-07-29 実機(E2E で 2 連続失敗した現物の行そのまま)— 点は ◉ にドリフトした
        assert_eq!(
            Pane::new("                  ◉ xhigh · /effort").effort_status(),
            Some("xhigh")
        );
        assert_eq!(Pane::new("nothing here").effort_status(), None);
    }

    #[test]
    fn effort_change_dialog_is_detected_and_not_mistaken_for_a_confirmation() {
        // 2026-07-29 実機のダイアログ全文(履歴のあるセッションの `/effort xhigh`)
        let dialog = "\
Change effort level?
Your next response will be slower and use more tokens
This conversation is cached for the current effort level. Switching to xhigh means \
the full history gets re-read on your next message.
❯ 1. Yes, switch to xhigh
  2. No, go back";
        assert!(Pane::new(dialog).effort_confirm_dialog_open());
        // ダイアログは**まだ**確定ではない。level を名指す行があっても done と読まない
        assert!(!Pane::new(dialog).effort_confirmed("xhigh"));
        assert_eq!(Pane::new(dialog).effort_status(), None);
        // 選択肢の `❯` 行は入力ボックスに見えるが、コマンドは残っていない(Enter リトライを誘わない)
        assert!(!Pane::new(dialog).still_has_command("/effort"));
        assert!(!Pane::new("… ←/→ to adjust …").effort_confirm_dialog_open());
    }

    #[test]
    fn login_pane_parsers() {
        let pane =
            "… visit: https://claude.com/cai/oauth/authorize?code=abc\ndef\n\nPaste code here";
        assert_eq!(
            Pane::new(pane).auth_login_url().unwrap(),
            "https://claude.com/cai/oauth/authorize?code=abcdef"
        ); // soft-wrap 再結合
        assert!(Pane::new("no marker").auth_login_url().is_none());
        assert!(matches!(
            Pane::new("… Login successful.").login_outcome(),
            LoginOutcome::Success
        ));
        assert!(matches!(
            Pane::new("Invalid code. …").login_outcome(),
            LoginOutcome::Error
        ));
        assert!(matches!(
            Pane::new("waiting").login_outcome(),
            LoginOutcome::Pending
        ));
    }

    // ── 起動画面の分類と見張り ──────────────────────────────────────────────
    // 現行 `classifySpawnPane` `answerSpawnScreens` の移植。
    // 実機の文面は 2026-08-02 に撮ったもの。

    /// 2026-08-02、実機の `tmux capture-pane` の原文。
    pub(crate) const TRUST_PANE: &str = "\
 Accessing workspace:

 /root

 Quick safety check: Is this a project you created or one you trust? (Like your
 own code, a well-known open source project, or work from your team). If not,
 take a moment to review what's in this folder first.

 Claude Code'll be able to read, edit, and execute files here.

 Security guide

 \u{276f} 1. Yes, I trust this folder
   2. No, exit

 Enter to confirm \u{b7} Esc to cancel";

    /// Claude Code 2.1.276 の実物(2026-09-18、pve)。**No が先頭で、既定で選ばれている** —
    /// Enter だけ押すと終了を選ぶ
    pub(crate) const TRUST_PANE_NO_FIRST: &str = "\
 Accessing workspace:

 /tmp

 Quick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source project, or work from your team). If not, take a moment to review what's in this
 folder first.

 Claude Code'll be able to read, edit, and execute files here.

 Security guide

 \u{276f} No, exit
   Yes, I trust this folder

 Enter to confirm \u{b7} Esc to cancel";

    /// 上の画面で下キーを1回押した後。
    pub(crate) const TRUST_PANE_YES_SECOND: &str = "\
 Accessing workspace:

 /tmp

 Security guide

   No, exit
 \u{276f} Yes, I trust this folder

 Enter to confirm \u{b7} Esc to cancel";

    #[test]
    fn the_trust_dialog_says_which_answer_is_selected() {
        assert_eq!(Pane::new(TRUST_PANE).trust_selected_is_yes(), Some(true));
        assert_eq!(Pane::new(TRUST_PANE_NO_FIRST).trust_selected_is_yes(), Some(false));
        assert_eq!(Pane::new(TRUST_PANE_YES_SECOND).trust_selected_is_yes(), Some(true));
        assert_eq!(Pane::new("no dialog here").trust_selected_is_yes(), None);
    }

    /// 現行のLOGIN(Claude Code v2.1.207 の実物と注記されている)。
    pub(crate) const LOGIN_PANE: &str = " Claude Code can be used with your Claude subscription\u{2026}\n \
Select login method:\n \u{276f} 1. Claude account with subscription \u{b7} Pro, Max, Team, or Enterprise\n \
  2. Anthropic Console account \u{b7} API usage billing";

    #[test]
    fn spawn_screen_reads_the_real_trust_dialog() {
        assert_eq!(Pane::new(TRUST_PANE).spawn_screen(), SpawnScreen::Trust);
    }

    #[test]
    fn spawn_screen_reads_the_real_login_screen() {
        assert_eq!(
            Pane::new(LOGIN_PANE).spawn_screen(),
            SpawnScreen::LoginRequired
        );
    }

    #[test]
    fn spawn_screen_reads_the_limit_modal_with_its_frame() {
        // 以前の実装が挙げていた実物の描かれ方(枠の中に1行)
        assert_eq!(
            Pane::new("\u{2502} You've hit your session limit").spawn_screen(),
            SpawnScreen::UsageLimited
        );
        // 番号つきの選択肢(現行)
        assert_eq!(
            Pane::new("\u{276f} 1. Stop and wait for the limit to reset").spawn_screen(),
            SpawnScreen::UsageLimited
        );
        assert_eq!(
            Pane::new("  Claude Code weekly limit reached").spawn_screen(),
            SpawnScreen::UsageLimited
        );
    }

    #[test]
    fn spawn_screen_reads_the_dev_channels_confirm() {
        // 現行 / 2865 の文面。**素の部分一致**(実物のキャプチャが無い)
        assert_eq!(
            Pane::new("Yes, proceed with local development").spawn_screen(),
            SpawnScreen::Confirm
        );
        assert_eq!(
            Pane::new("\u{2026}load local development channels?").spawn_screen(),
            SpawnScreen::Confirm
        );
    }

    #[test]
    fn an_account_refusal_wins_over_the_ordinary_prompts() {
        // 現行 と同じ順序の約束 — サインイン/上限は trust/confirm に
        // 「答えて消す」ものではない。Enter を撃っても消えないので、撃つ前に諦める
        let mixed = format!("{LOGIN_PANE}\n \u{276f} 1. Yes, I trust this folder");
        assert_eq!(Pane::new(&mixed).spawn_screen(), SpawnScreen::LoginRequired);
        assert_eq!(
            Pane::new("\u{2502} You've hit your session limit\nlocal development").spawn_screen(),
            SpawnScreen::UsageLimited
        );
    }

    #[test]
    fn the_words_inside_a_line_of_content_are_not_a_screen() {
        // 自分のソースや grep の結果を映しているワーカーの画面。ここへ Enter を撃つと
        // 入力欄の中身が送信される。行頭アンカーが唯一の防波堤(現行)
        assert_eq!(
            Pane::new("  if paneText.includes('I trust this folder') return 'trust'")
                .spawn_screen(),
            SpawnScreen::None_
        );
        assert_eq!(
            Pane::new("grep -n \"you've hit your session limit\" bridge/command.ts").spawn_screen(),
            SpawnScreen::None_
        );
        assert_eq!(
            Pane::new("  * the screen prints Select login method: as its own line").spawn_screen(),
            SpawnScreen::None_
        );
    }

    #[test]
    fn an_ordinary_booting_pane_is_not_a_screen() {
        // 誤検知はフリートを止めうる(現行)。疑わしきは None_
        assert_eq!(Pane::new("").spawn_screen(), SpawnScreen::None_);
        assert_eq!(
            Pane::new("still booting\u{2026}").spawn_screen(),
            SpawnScreen::None_
        );
        assert_eq!(Pane::new("\u{276f} ").spawn_screen(), SpawnScreen::None_);
    }
}
