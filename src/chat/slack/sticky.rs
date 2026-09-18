//! The progress message: one Slack message per turn that lists the tools the agent runs,
//! updated in place and folded away when the turn ends. Includes the Edit diff rendering.

use crate::bridge::state::ThreadKey;

// ─── 付箋(進捗スティッキー) ─────────────────────────────────────────────
//
// StickyBoard は純粋な状態 — Slack I/O は持たない。post/update/delete は main の
// flush ループが `take_dirty` / `settle` の結果を見て Api で行う。

/// ツール行の状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    Done,
    Error,
    Deny,
}

impl ToolStatus {
    /// hook イベント → ツールの状態。PostToolUse 以外はまだ走っている(Pending)。
    /// 失敗のうち権限拒否は 💥 でなく 🚫 に振り分ける(結果テキストで判定)。
    pub fn of(hook_event_name: &str, is_error: bool, result_text: &str) -> ToolStatus {
        if hook_event_name != "PostToolUse" {
            return ToolStatus::Pending;
        }
        if !is_error {
            return ToolStatus::Done;
        }
        let t = result_text.to_lowercase();
        if ["deni", "permission", "not allowed", "blocked"]
            .iter()
            .any(|p| t.contains(p))
        {
            ToolStatus::Deny
        } else {
            ToolStatus::Error
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            ToolStatus::Pending => "◌",
            ToolStatus::Done => "•",
            ToolStatus::Error => "💥",
            ToolStatus::Deny => "🚫",
        }
    }
}

/// 畳みの対象。**完了済みが連続したときだけ**1行にまとめる。
/// 走行中(◌)と失敗(💥/🚫)は畳まない — 今なにが起きているかは常に見えていないと困る。
const FOLD_READ: [&str; 1] = ["Read"];
const FOLD_SEARCH: [&str; 2] = ["Grep", "Glob"];

/// 編集系(diff を出す対象。畳みには載せない)。
const EDIT_TOOLS: [&str; 3] = ["Edit", "MultiEdit", "Write"];

/// Edit/MultiEdit/Write は**何が変わったか**を git 風の unified diff で行の下に出す
/// 新しい配管は要らない: PostToolUse の `tool_input` に
/// `old_string`/`new_string`(Edit)・`edits[]`(MultiEdit)・`content`(Write)が既に来ている。
/// Slack のコードブロックは色を持てないので、git と同じ1文字の前置(`-` 削除 / `+` 追加 /
/// ` ` 文脈)を ``` フェンスに入れる。**done の行にだけ**、**main セッションの行にだけ**出す
/// (畳んだ subagent の窓は1行のまま)。
const DIFF_CTX: usize = 3;
const DIFF_MAX_LINES: usize = 16;
const DIFF_MAX_BYTES: usize = 900;
const DIFF_MAX_LINE: usize = 120;
/// LCS の DP を張る上限。超えたら素朴な「全削除 + 全追加」に落とす(出力はどのみち上で切る)。
const DIFF_MAX_INPUT_LINES: usize = 200;

/// ``` の run を zero-width space で分断してからクリップする。裸の ``` 行が1本あるだけで
/// **こちらのフェンスが先に閉じてしまう**ので、中身側で必ず殺す。
fn clip_diff_line(s: &str) -> String {
    fn flush(out: &mut String, run: usize) {
        for k in 0..run {
            if run >= 3 && k > 0 {
                out.push('\u{200b}');
            }
            out.push('`');
        }
    }
    let mut out = String::new();
    let mut run = 0usize;
    for c in s.chars() {
        if c == '`' {
            run += 1;
            continue;
        }
        flush(&mut out, run);
        run = 0;
        out.push(c);
    }
    flush(&mut out, run);
    if out.chars().count() > DIFF_MAX_LINE {
        return out.chars().take(DIFF_MAX_LINE).chain(['…']).collect();
    }
    out
}

/// 行単位の LCS 差分 — git が作る形(一致は文脈、残りが `-`/`+`)。Edit が扱う断片は小さいので
/// O(n·m) で足りる。
pub fn diff_lines(old_text: &str, new_text: &str) -> Vec<(char, String)> {
    let split = |t: &str| -> Vec<String> {
        if t.is_empty() {
            Vec::new()
        } else {
            t.split('\n').map(str::to_string).collect()
        }
    };
    let (a, b) = (split(old_text), split(new_text));
    if a.len() > DIFF_MAX_INPUT_LINES || b.len() > DIFF_MAX_INPUT_LINES {
        return a
            .into_iter()
            .map(|s| ('-', s))
            .chain(b.into_iter().map(|s| ('+', s)))
            .collect();
    }
    let (n, m) = (a.len(), b.len());
    // dp[i][j] = a[i..] と b[j..] の最長共通部分列の長さ
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((' ', a[i].clone()));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push(('-', a[i].clone()));
            i += 1;
        } else {
            out.push(('+', b[j].clone()));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|s| ('-', s.clone())));
    out.extend(b[j..].iter().map(|s| ('+', s.clone())));
    out
}

/// 差分を git 風の hunk に畳む: 変化の前後 `DIFF_CTX` 行だけ文脈を残し、それ以外の
/// 変化していない連なりは `…` 1行に置き換える。
fn collapse_hunk(diff: &[(char, String)]) -> Vec<String> {
    let mut keep = vec![false; diff.len()];
    for (idx, (t, _)) in diff.iter().enumerate() {
        if *t == ' ' {
            continue;
        }
        let lo = idx.saturating_sub(DIFF_CTX);
        let hi = (idx + DIFF_CTX).min(diff.len().saturating_sub(1));
        keep[lo..=hi].fill(true);
    }
    let mut out = Vec::new();
    let mut gap = false;
    for (idx, (t, s)) in diff.iter().enumerate() {
        if keep[idx] {
            out.push(format!("{t}{}", clip_diff_line(s)));
            gap = false;
        } else if !gap {
            out.push("…".to_string());
            gap = true;
        }
    }
    out
}

/// ツール行に足す差分。返すのは `" (+A -R)\n```…```"` の形の**行の続き**で、
/// 出すものが無ければ `None`(テキストの変化なし / input が無い)。
/// MultiEdit の hunk と hunk の間は `…` で区切る。
pub fn render_edit_diff(name: &str, input: &serde_json::Value) -> Option<String> {
    let text = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
    let hunks: Vec<(String, String)> = match name {
        "MultiEdit" => input["edits"]
            .as_array()
            .map(|es| {
                es.iter()
                    .map(|e| (text(&e["old_string"]), text(&e["new_string"])))
                    .collect()
            })
            .unwrap_or_default(),
        "Write" => vec![(String::new(), text(&input["content"]))],
        _ => vec![(text(&input["old_string"]), text(&input["new_string"]))],
    };
    let (mut added, mut removed) = (0usize, 0usize);
    let mut rendered: Vec<String> = Vec::new();
    for (idx, (old, new)) in hunks.iter().enumerate() {
        let diff = diff_lines(old, new);
        added += diff.iter().filter(|(t, _)| *t == '+').count();
        removed += diff.iter().filter(|(t, _)| *t == '-').count();
        if idx > 0 {
            rendered.push("…".to_string());
        }
        rendered.extend(collapse_hunk(&diff));
    }
    if added == 0 && removed == 0 {
        return None;
    }
    let mut capped: Vec<String> = Vec::new();
    let (mut bytes, mut dropped) = (0usize, 0usize);
    for (k, ln) in rendered.iter().enumerate() {
        if capped.len() >= DIFF_MAX_LINES || bytes + ln.len() + 1 > DIFF_MAX_BYTES {
            dropped = rendered.len() - k;
            break;
        }
        bytes += ln.len() + 1;
        capped.push(ln.clone());
    }
    // 打ち切りの脚注は**2行**(件数を1行、そのあとに `…`)
    if dropped > 0 {
        capped.push(format!("(+{dropped} more)"));
        capped.push("…".to_string());
    }
    Some(format!(
        " (+{added} -{removed})\n```\n{}\n```",
        capped.join("\n")
    ))
}

/// 付箋の1行と subagent の結び付き。
///
/// この行が **どの subagent のものか**、あるいは **どの subagent を起こしたか**。
/// hook payload 由来で、main セッションのツール呼び出しでは全部 None。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentRef {
    /// この行を走らせた subagent の id(payload の top-level `agent_id`)。
    pub agent_id: Option<String>,
    /// その subagent の名前(`agent_type`。Explore / general-purpose など)。
    pub agent_type: Option<String>,
    /// `Agent` 行が起こした subagent の id(`tool_response.agentId` / `.agent_id`)。
    pub spawned_agent_id: Option<String>,
    /// `Agent` 行が起動した名前(`tool_input.name`)。background/teammate を**名前で**結ぶのに要る。
    /// **`summary` では代用できない** — `Self::summarize("Agent", …)` が返すのは `description` で、
    /// 起動名とは別のフィールド(`input.name` を見ている)。
    pub launched_name: Option<String>,
}

/// 付箋の1行。
#[derive(Debug, Clone)]
pub enum RenderItem {
    Narration {
        text: String,
    },
    Tool {
        id: String,
        name: String,
        summary: String,
        status: ToolStatus,
        agent: AgentRef,
        /// done になった編集系ツールの差分(`" (+A -R)\n```…```"`)。
        /// **描画時ではなく受け取った時に**組む — 付箋は `tool_input` を持ち続けないので、
        /// 手元に input があるこの瞬間しか作れない。
        diff: Option<String>,
    },
    /// 中断の締め行。グリフ無しの生の1行(notice をそのまま push)。
    Interrupted,
}

/// ナレーション行の頭。⏺(U+23FA)は Slack が絵文字化するので ● を使う。
const NARR_GLYPH: &str = "●";
/// ツール行のインデント。**普通の空白ではなく NBSP** — Slack は行頭の空白を潰し、
/// `• ` で始まる行を箇条書きに整形してしまう。
const TOOL_INDENT: &str = "\u{A0}\u{A0}\u{A0}";
/// 許可プロンプトが誰にも押されないまま満期になったときに付箋へ足す1行
/// TOOL_INDENT を付けて、止まったツール行の
/// **下の注記**として読ませる。
fn perm_timeout_line() -> String {
    crate::t!(
        "\u{A0}\u{A0}\u{A0}⚠️ No answer to the permission request — timed out",
        "\u{A0}\u{A0}\u{A0}⚠️ ツール許可の返事が無く、タイムアウトしました"
    )
}

/// 1枚の付箋の予算(バイト)。Slack の本文上限に対して余裕をとった値。
const STICKY_BUDGET: usize = 3800;
/// stop で切られたラウンドの締め行。ここまでの進捗の
/// **下**に付く — 「どこまで行ったか」を残したまま中断だと分かる形。
const INTERRUPTED_NOTICE: &str = "└ `Interrupted by user.`";

/// 畳んだ subagent セクションで見せる直近件数(ローリング窓)。
/// Slack はメッセージの**中を**スクロールできないので、この「最後の N 件」がスクロールの代わり。
const SUBAGENT_WINDOW: usize = 2;
/// セクションの見出し記号。
const SUBAGENT_MARK: &str = "▾";
/// セクション行のインデント(ツール行の TOOL_INDENT にさらに重ねる)。
const SUBAGENT_INDENT: &str = "\u{A0}\u{A0}";

/// settle の結果 — 付箋を記録として残すか、消すか。
#[derive(Debug, PartialEq, Eq)]
pub enum StickyAction {
    Keep,
    Delete(String),
}

/// 1ラウンド分の付箋。
#[derive(Default)]
struct Sticky {
    items: Vec<RenderItem>,
    posted_ts: Option<String>,
    dirty: bool,
    last_flush_ms: Option<u64>,
    /// 許可待ちが満期になった。次のラウンドまで注記を出し続ける。
    perm_timed_out: bool,
    /// **封じたページに出し終えた行数**。いま育てているページは
    /// ここから始まる。Slack は約4000バイトを超える編集を拒むので、1枚に収まらなくなったら
    /// そのメッセージを封じて(以後編集しない)続きを次のメッセージに出す。
    sealed_lines: usize,
    /// いまのページが**コードブロックの中から**始まるか(前のページから持ち越した)。
    sealed_open_fence: bool,
    /// まだ投稿していないページのまま溢れた回数。封じたページは二度と編集しないので、
    /// その投稿が返してくる ts は**捨てる**(拾うと次のページがそれを編集してしまう)。
    sealed_awaiting_post: usize,
}

/// thread_key → 進行中の付箋。1スレッド1枚(ページ繰りは持たない)。
#[derive(Default)]
pub struct StickyBoard {
    stickies: std::collections::HashMap<ThreadKey, Sticky>,
    /// 決着したスレッド → **返事の後の続きを新しい付箋に出してよいか**。
    /// `true` は reply/edit で答えたラウンド、`false` は no_reply/react や中断で黙ったラウンド。
    settled: std::collections::HashMap<ThreadKey, bool>,
    /// 決着後に来たナレーションの控え。**描かずにとっておく**(下の
    /// `push_narration` / `open_after_answer` が入れ手と出し手)。
    held: std::collections::HashMap<ThreadKey, Vec<String>>,
}

impl StickyBoard {
    /// 行を組んで予算で打ち切る。切ったことは `…(N more)` で必ず見せる(黙って切らない)。
    /// item 1つを1行に。
    /// `lead_blank` は `Interrupted` が先行行を持つときに前へ空行を入れるかどうか。
    /// `with_diff` は編集系の差分ブロックを行の下に付けるかどうか。main セッションの行だけ
    /// true — 畳んだ subagent の窓は1行のままにする。
    fn render_item_line(it: &RenderItem, lead_blank: bool, with_diff: bool) -> String {
        match it {
            RenderItem::Narration { text } => format!("{NARR_GLYPH} {text}"),
            // 先行行があれば空行を挟んで独立した段落にする。
            // 空行込みで1本の行として組むので、予算の勘定もそのまま合う
            RenderItem::Interrupted if lead_blank => format!("\n{INTERRUPTED_NOTICE}"),
            RenderItem::Interrupted => INTERRUPTED_NOTICE.to_string(),
            RenderItem::Tool {
                name,
                summary,
                status,
                diff,
                ..
            } => {
                let head = if summary.is_empty() {
                    format!("{TOOL_INDENT}{} {name}", status.glyph())
                } else {
                    format!("{TOOL_INDENT}{} {name} `{summary}`", status.glyph())
                };
                match diff {
                    Some(d) if with_diff => format!("{head}{d}"),
                    _ => head,
                }
            }
        }
    }

    /// 溜めた run を1行(2本以上)か素の行(1本)にして吐く。
    /// 1本だけの run は畳まない(行数が減らず、パスやコマンドが消えるだけなので)。
    fn flush_fold_run(run: &mut Vec<&RenderItem>, lines: &mut Vec<String>) {
        match run.len() {
            0 => {}
            1 => lines.push(Self::render_item_line(run[0], !lines.is_empty(), true)),
            _ => {
                let pairs: Vec<(&str, &str)> = run
                    .iter()
                    .filter_map(|it| match it {
                        RenderItem::Tool { name, summary, .. } => Some((name.as_str(), summary.as_str())),
                        _ => None,
                    })
                    .collect();
                // `•` の後の**空白2つ**は、畳まれていない `•` 行と桁を揃えるため
                lines.push(format!("{TOOL_INDENT}•  {}", Self::tool_breakdown(&pairs)));
            }
        }
        run.clear();
    }

    /// 畳んだ subagent セクション1つ分。`header` が None なら独立した
    /// `▾ <type> · <内訳>` 見出し、Some なら Agent 行を見出しに使う。
    /// 行は直近 SUBAGENT_WINDOW 件だけ、1段深いインデントで。
    fn push_agent_section(
        lines: &mut Vec<String>,
        header: Option<String>,
        agent_type: &str,
        rows: &[&RenderItem],
    ) {
        let pairs: Vec<(&str, &str)> = rows
            .iter()
            .filter_map(|it| match it {
                RenderItem::Tool { name, summary, .. } => Some((name.as_str(), summary.as_str())),
                _ => None,
            })
            .collect();
        let breakdown = Self::tool_breakdown(&pairs);
        lines.push(match header {
            Some(h) => format!("{h} : {agent_type} · {breakdown}"),
            None => format!("{TOOL_INDENT}{SUBAGENT_MARK} {agent_type} · {breakdown}"),
        });
        let start = rows.len().saturating_sub(SUBAGENT_WINDOW);
        for it in &rows[start..] {
            lines.push(format!(
                "{SUBAGENT_INDENT}{}",
                Self::render_item_line(it, false, false)
            ));
        }
    }

    /// `start` から予算に収まる最後の行(排他)。**予算はバイトで測る**
    /// Slack の上限がバイト基準なので、日本語だと文字数勘定では上限に先に当たって
    /// 編集が拒まれ、付箋が固まる。**必ず1行は進む**(1行で超える行はそれ単独のページ)。
    fn pack_cut(lines: &[String], start: usize, budget: usize) -> usize {
        let mut len = 0usize;
        let mut i = start;
        while i < lines.len() {
            let add = usize::from(i > start) + lines[i].len(); // 継ぎ目の \n は1バイト
            if len + add > budget && i > start {
                break;
            }
            len += add;
            i += 1;
        }
        i
    }

    /// ``` を開く/閉じる行か。diff の**中身**は当たらない
    /// (`clip_diff_line` が ``` の連なりを zero-width space で割ってある)。
    fn is_fence_toggle(line: &str) -> bool {
        line.trim_start().starts_with("```")
    }

    /// 1ページをコードブロックとして自己完結させる。`open_in` は前のページから
    /// フェンスが開いたまま来たか(なら頭で開き直す)。ページの途中でフェンスが開いたまま
    /// 終わるなら末尾で閉じる。返すのは (本文, ページ末でフェンスが開いているか)。
    fn wrap_fences(page: &[String], open_in: bool) -> (String, bool) {
        let mut in_fence = open_in;
        for ln in page {
            if Self::is_fence_toggle(ln) {
                in_fence = !in_fence;
            }
        }
        let mut parts: Vec<&str> = Vec::new();
        if open_in {
            parts.push("```");
        }
        parts.extend(page.iter().map(String::as_str));
        if in_fence {
            parts.push("```");
        }
        (parts.join("\n"), in_fence)
    }

    /// `from` 行目から1ページ分を組む。返すのは (本文, 次ページの開始行, ページ末のフェンス状態)。
    /// 次ページの開始行が `lines.len()` なら、そのページで終わり。
    fn page(lines: &[String], from: usize, open_fence: bool) -> (String, usize, bool) {
        let cut = Self::pack_cut(lines, from, STICKY_BUDGET);
        // 1行だけで予算を超えるページは頭出しする(Slack はそのままだと編集ごと拒む)
        if cut == from + 1 && lines[from].len() > STICKY_BUDGET {
            let head = Self::clip(&lines[from], STICKY_BUDGET);
            let (text, end) = Self::wrap_fences(&[head], open_fence);
            return (text, cut, end);
        }
        let (text, end) = Self::wrap_fences(&lines[from..cut], open_fence);
        (text, cut, end)
    }

    /// 決着後に遅れて来た hook を落とす。これが無いと「● …返信しました」だけの
    /// 孤児付箋が生える(no_reply の後だと沈黙のはずが発言に見える — E2E で3回再現)。
    /// ponytail: 決着後は次ターンまで沈黙。再開が要るなら、そのときに再開の条件を足す
    fn settled(&self, key: &ThreadKey) -> bool {
        self.settled.contains_key(key)
    }

    /// 返事を出した後もワーカーが働き続けることがある(「ついでに説明して」の類)。
    /// その進捗を捨てると Slack には何も残らないので、**返事の下に新しい付箋を1枚起こす**。
    ///
    /// 起こすのは決着1回につき1枚だけ(2枚目以降の行は同じ付箋に足す)。**黙ると決めた
    /// ラウンドでは起こさない** — no_reply / react / 中断の後に進捗だけ生えると、沈黙の
    /// はずが発言に見える(`settled` の但し書きと同じ事故)。
    ///
    /// 新しい付箋を起こしてよいのは**本物のツールが動いたとき**だけ
    /// (= 返事の後も仕事が続いている証拠)。ここに来るのはツールの行だけで、決着後の
    /// ナレーションは `push_narration` が控えに回すので届かない。
    ///
    /// 返り値は「この行を描いてよいか」。
    /// `by_tool` = この行がツールか。**沈黙で決着したラウンドはツールでだけ再開する** —
    /// no_reply / react の後に本物の仕事が動いたなら記録は要る。
    /// ナレーションだけでは起こさない。
    fn open_after_answer(&mut self, key: &ThreadKey, by_tool: bool) -> bool {
        match self.settled.get(key) {
            None => true, // まだ決着していない — 普段どおり
            Some(false) if !by_tool => false,
            _ => {
                self.settled.remove(key);
                let mut s = Sticky::default();
                // とっておいたナレーションを、続きの仕事の**上**に出す
                s.items.extend(
                    self.held
                        .remove(key)
                        .into_iter()
                        .flatten()
                        .map(|text| RenderItem::Narration { text }),
                );
                self.stickies.insert(key.clone(), s);
                true
            }
        }
    }

    /// 新ラウンド。前の付箋は Slack 上に記録として残り、こちらは追跡をやめる。
    pub fn on_turn_start(&mut self, key: &ThreadKey) {
        self.stickies.insert(key.clone(), Sticky::default());
        self.settled.remove(key);
        self.held.remove(key); // 次ターンへ持ち越さない(ターン終わりで消し損ねた分の保険)
    }

    /// ターンが終わった。ここまで誰も出さなかった控えは**締めの一言だった**
    /// ということなので捨てる。次の発言が
    /// 来るまで持ち続けると、二度と発言の来ないスレッドのぶんが残りっぱなしになる。
    pub fn on_turn_end(&mut self, key: &ThreadKey) {
        self.held.remove(key);
    }

    /// PreToolUse の ◌ 行を、同じ tool_use_id の PostToolUse が •/💥/🚫 に差し替える。
    pub fn upsert_tool(
        &mut self,
        key: &ThreadKey,
        tool_use_id: &str,
        name: &str,
        summary: &str,
        status: ToolStatus,
        agent: &AgentRef,
        input: &serde_json::Value,
    ) {
        if Self::is_denied(name) {
            return; // ノイズと自前ツールは行にしない — 呼び手の規律でなく board の不変条件
        }
        if !self.open_after_answer(key, true) {
            return;
        }
        // 差分は**ここでしか作れない**(付箋は input を持ち続けない)。done の
        // 編集系だけ。走行中(◌)に出すと、まだ適用されていない変更を「変わった」と見せてしまう
        let diff = (status == ToolStatus::Done && EDIT_TOOLS.contains(&name))
            .then(|| render_edit_diff(name, input))
            .flatten();
        let s = self.stickies.entry(key.clone()).or_default();
        let found = s
            .items
            .iter_mut()
            .find(|i| matches!(i, RenderItem::Tool { id, .. } if id == tool_use_id));
        match found {
            Some(RenderItem::Tool {
                status: cur,
                agent: cur_agent,
                diff: cur_diff,
                ..
            }) => {
                *cur = status;
                // PreToolUse で作った行に、PostToolUse の差分が後から乗る
                if diff.is_some() {
                    *cur_diff = diff;
                }
                // PreToolUse の時点では「どの subagent を起こしたか」は分からない。PostToolUse で
                // 初めて載るので、**後から来た値だけ**採る(既に知っている値は消さない)
                if agent.spawned_agent_id.is_some() {
                    cur_agent.spawned_agent_id = agent.spawned_agent_id.clone();
                }
                if agent.launched_name.is_some() {
                    cur_agent.launched_name = agent.launched_name.clone();
                }
            }
            _ => s.items.push(RenderItem::Tool {
                id: tool_use_id.to_string(),
                name: name.to_string(),
                summary: summary.to_string(),
                status,
                agent: agent.clone(),
                diff,
            }),
        }
        s.dirty = true;
    }

    /// final になったナレーションを1行足す(delta の蓄積は呼び出し側)。
    pub fn push_narration(&mut self, key: &ThreadKey, text: &str) {
        // 決着後のナレーションは**とっておくだけで描かない**。返事の後に本物の
        // ツールが動けば「仕事が続いている」証拠なので `open_after_answer` がまとめて出す。
        // 何も動かないままターンが終われば締めの一言だったということで、次の
        // `on_turn_start` が捨てる。これが無いと「● Slack に返信しました。」だけの
        // 付箋が返事の下に生えて誰も消さない
        if self.settled.get(key) == Some(&true) {
            self.held
                .entry(key.clone())
                .or_default()
                .push(text.to_string());
            return;
        }
        if !self.open_after_answer(key, false) {
            return;
        }
        let s = self.stickies.entry(key.clone()).or_default();
        s.items.push(RenderItem::Narration {
            text: text.to_string(),
        });
        s.dirty = true;
    }

    /// stop で切られたラウンド。締め行を1本足してそこで沈黙する — settle と違い
    /// 付箋は残す(切られるまでの進捗が記録)。復帰は次の `on_turn_start`。
    /// 許可プロンプトが押されないまま満期になった。
    ///
    /// **止まったツール行は触らない**(◌ のまま)。別 upsert で ⚠️ に落とすと
    /// **行が二重になる** — perm フレームの tool_use_id は PreToolUse の行の鍵と
    /// 一致するとは限らないため。
    /// 足すのはインデント付きの注記1行だけ。
    pub fn on_perm_timeout(&mut self, key: &ThreadKey) {
        if self.settled(key) {
            return;
        }
        let s = self.stickies.entry(key.clone()).or_default();
        s.perm_timed_out = true;
        s.dirty = true;
    }

    /// Deny が押された。**そのツールの行**を 🚫 にする(満期とは別の道で、
    /// 注記行は出さない)。行が引けなければ何もしない。
    pub fn on_perm_denied(&mut self, key: &ThreadKey, tool_use_id: &str) {
        if self.settled(key) || tool_use_id.is_empty() {
            return;
        }
        let Some(s) = self.stickies.get_mut(key) else {
            return;
        };
        for it in s.items.iter_mut() {
            if let RenderItem::Tool { id, status, .. } = it
                && id == tool_use_id
            {
                *status = ToolStatus::Deny;
                s.dirty = true;
                return;
            }
        }
    }

    pub fn on_interrupted(&mut self, key: &ThreadKey) {
        if self.settled(key) {
            return;
        }
        let s = self.stickies.entry(key.clone()).or_default();
        s.items.push(RenderItem::Interrupted);
        s.dirty = true;
        // 中断で黙ったラウンド — 後から来る進捗で新しい付箋を起こさない
        self.settled.insert(key.clone(), false);
    }

    /// 投稿できた ts を覚える(以降は update)。
    pub fn set_posted(&mut self, key: &ThreadKey, ts: &str) {
        let s = self.stickies.entry(key.clone()).or_default();
        // 封じたページの投稿だった — その ts は覚えない(覚えると次のページがそれを編集する)
        if s.sealed_awaiting_post > 0 {
            s.sealed_awaiting_post -= 1;
            return;
        }
        s.posted_ts = Some(ts.to_string());
    }

    /// このスレッドで**いま出ている進捗付箋**の ts。stop 絵文字が付いたのが
    /// 付箋かどうかを見分けるのに使う。
    pub fn sticky_ts(&self, key: &ThreadKey) -> Option<String> {
        self.stickies.get(key).and_then(|s| s.posted_ts.clone())
    }

    /// 描き直しが要る付箋を (key, 投稿済み ts, 本文) で返す。
    /// **スレッドごとに前回から1秒未満は返さない** — Slack の編集レート保護。
    pub fn take_dirty(&mut self, now_ms: u64) -> Vec<(ThreadKey, Option<String>, String)> {
        let mut out = Vec::new();
        for (key, s) in self.stickies.iter_mut() {
            if !s.dirty
                || s.last_flush_ms
                    .is_some_and(|t| now_ms.saturating_sub(t) < 1000)
            {
                continue;
            }
            let lines = Self::lines_of(&s.items, s.perm_timed_out);
            let (body, next, end_fence) = Self::page(&lines, s.sealed_lines, s.sealed_open_fence);
            if next < lines.len() {
                // 入りきらなくなった。**このページを封じて**次のメッセージへ移る。
                // 封じたページの ts はもう要らない(二度と編集しない)ので手放し、次の周回で
                // 続きが新しいメッセージとして投稿される。`dirty` は立てたまま・スロットルも
                // 進めない — 残りを次の tick ですぐ出すため
                let sealed_ts = s.posted_ts.take();
                if sealed_ts.is_none() {
                    // まだ投稿されていないページのまま溢れた — これから post されるが、
                    // その ts は封じたページのものなので拾わない
                    s.sealed_awaiting_post += 1;
                }
                out.push((key.clone(), sealed_ts, body));
                s.sealed_lines = next;
                s.sealed_open_fence = end_fence;
                continue;
            }
            s.dirty = false;
            s.last_flush_ms = Some(now_ms);
            out.push((key.clone(), s.posted_ts.clone(), body));
        }
        out
    }

    /// 決着時の最終描画。スロットルを無視して1回だけ返す(呼ぶのは settle の直前)。
    /// これが無いと、最後の PostToolUse が1秒以内に決着した付箋が `◌` のまま残る。
    pub fn take_final(&mut self, key: &ThreadKey) -> Option<(Option<String>, String)> {
        let s = self.stickies.get_mut(key)?;
        if !s.dirty {
            return None;
        }
        s.dirty = false;
        // 最終描画も**いま育てているページ**だけ(封じたページは編集しない)。ここで
        // 溢れていても次ページは起こさない — 決着でこの付箋の追跡は終わる
        let lines = Self::lines_of(&s.items, s.perm_timed_out);
        let (body, _, _) = Self::page(&lines, s.sealed_lines, s.sealed_open_fence);
        Some((s.posted_ts.clone(), body))
    }

    /// ラウンドの決着。返信したなら付箋は記録として残す。沈黙(no_reply)や
    /// リアクションだけなら進捗は「ボットの独り言」に見えるので消す。
    /// どちらでも追跡は終える — flush ループが消した付箋を復活させないため。
    pub fn settle(&mut self, key: &ThreadKey, kind: &str) -> StickyAction {
        // 答えたラウンドだけ、後続の進捗に新しい付箋を許す
        let answered_with_text = !matches!(kind, "no_reply" | "react");
        self.settled.insert(key.clone(), answered_with_text);
        match self.stickies.remove(key) {
            Some(Sticky {
                posted_ts: Some(ts),
                ..
            }) if matches!(kind, "no_reply" | "react") => StickyAction::Delete(ts),
            _ => StickyAction::Keep,
        }
    }

    /// 付箋に出さないツール(ノイズと自前ツール)。
    pub fn is_denied(name: &str) -> bool {
        matches!(name, "TodoWrite" | "ToolSearch" | "advisor") || name.starts_with("mcp__agentgw__")
    }

    /// ツール入力から一番目立つ引数を1行に。
    /// Slack のインラインコードに入れるので改行とバッククォートを落とし、70字で `…`。
    pub fn summarize(name: &str, input: &serde_json::Value) -> String {
        let get = |k: &str| {
            input
                .get(k)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        };
        let raw = match name {
            "Bash" => get("command"),
            // NotebookEdit は file_path を持たない
            "Edit" | "Write" | "Read" | "NotebookEdit" => {
                get("file_path").or_else(|| get("notebook_path"))
            }
            "WebFetch" | "WebSearch" => get("url").or_else(|| get("query")),
            "Skill" => get("skill"),
            "Agent" => get("description"),
            _ => [
                "command",
                "file_path",
                "url",
                "query",
                "description",
                "pattern",
                "path",
            ]
            .iter()
            .find_map(|k| get(k)),
        };
        let s = raw.unwrap_or_default().replace('\n', " ").replace('`', "");
        if s.chars().count() > 70 {
            s.chars().take(70).chain(['…']).collect()
        } else {
            s
        }
    }

    /// 畳んでよいツールか(Read / Grep / Glob / Bash)。
    pub fn is_foldable(name: &str) -> bool {
        FOLD_READ.contains(&name) || FOLD_SEARCH.contains(&name) || name == "Bash"
    }

    /// `grep` 系を走らせた Bash 行は「Ran N commands」ではなく
    /// 「Searched for N patterns」に数える。ワーカーのセッションには Grep/Glob ツールが無く、
    /// コード検索は実際には Bash 越しの `grep`/`rg` で走るため。
    ///
    /// 判定は**起動したコマンド**(先頭トークン。先頭の `VAR=val` を捨て、絶対パスは基底名に)。
    /// パイプの**フィルタ**として使う grep(`ps ax | grep x`)は主コマンドが ps なので数えない。
    pub fn bash_is_search(command: &str) -> bool {
        const SEARCH_CMDS: [&str; 7] = ["grep", "egrep", "fgrep", "rg", "ripgrep", "ag", "ack"];
        let mut s = command.trim();
        // 先頭の `LC_ALL=C ` 等を落とす
        while let Some((head, rest)) = s.split_once(char::is_whitespace) {
            let is_assign = head.split_once('=').is_some_and(|(k, _)| {
                !k.is_empty() && k.chars().all(|c| c.is_alphanumeric() || c == '_')
            });
            if !is_assign {
                break;
            }
            s = rest.trim_start();
        }
        let mut tokens = s.split_whitespace();
        let Some(first) = tokens.next() else {
            return false;
        };
        let base = first.rsplit('/').next().unwrap_or(first);
        SEARCH_CMDS.contains(&base) || (base == "git" && tokens.next() == Some("grep"))
    }

    /// subagent が走らせたツールの内訳。畳んだセクションの
    /// 見出しに出して「何をした agent か」を一目で分かるようにする。**全ステータスを数える**ので、
    /// 各節の合計は総数に一致する。分類に載らないものはツール名ごとに束ねる("WebFetch 2")。
    ///
    /// 受けるのは `(ツール名, summary)`。Bash の判定は先頭トークンしか使わないので、
    /// 70 字にクリップ済みの summary で足りる。
    pub fn tool_breakdown(items: &[(&str, &str)]) -> String {
        let (mut reads, mut searches, mut cmds, mut edits) = (0usize, 0usize, 0usize, 0usize);
        // 出現順を保つ(HashMap だと "WebFetch 2, Skill 1" の順が不定になる)
        let mut other: Vec<(String, usize)> = Vec::new();
        for (name, summary) in items {
            if *name == "Read" {
                reads += 1;
            } else if FOLD_SEARCH.contains(name) {
                searches += 1;
            } else if *name == "Bash" {
                if Self::bash_is_search(summary) {
                    searches += 1;
                } else {
                    cmds += 1;
                }
            } else if EDIT_TOOLS.contains(name) {
                edits += 1;
            } else {
                match other.iter_mut().find(|(n, _)| n == name) {
                    Some((_, c)) => *c += 1,
                    None => other.push(((*name).to_string(), 1)),
                }
            }
        }
        let mut parts: Vec<String> = Vec::new();
        for (n, verb, one, many) in [
            (reads, "Read", "file", "files"),
            (searches, "Searched for", "pattern", "patterns"),
            (cmds, "Ran", "command", "commands"),
            (edits, "Edited", "file", "files"),
        ] {
            if n > 0 {
                parts.push(format!("{verb} {n} {}", if n == 1 { one } else { many }));
            }
        }
        for (name, n) in other {
            parts.push(format!("{name} {n}"));
        }
        parts.join(", ")
    }

    /// `room` バイトに収まる最長の prefix(**char 境界**)+ `…`。1文字も入らなければ空。
    pub fn clip(line: &str, room: usize) -> String {
        let budget = room.saturating_sub("…".len());
        match line
            .char_indices()
            .map(|(i, c)| i + c.len_utf8())
            .take_while(|&end| end <= budget)
            .last()
        {
            Some(end) => format!("{}…", &line[..end]),
            None => String::new(),
        }
    }

    /// 出す行を組む(畳み込みまで)。ページ分割はこの後の仕事。
    fn lines_of(items: &[RenderItem], perm_timed_out: bool) -> Vec<String> {
        // subagent の中で走ったツールは**その場では描かない**。agent ごとに1つの
        // セクションにまとめ、その agent の最初のツールがあった位置に1回だけ出す。
        // 何十本もツールを走らせる subagent が付箋を埋め尽くすのを防ぐ。
        let mut groups: Vec<(String, String, Vec<&RenderItem>)> = Vec::new(); // (agent_id, type, items)
        for it in items {
            let RenderItem::Tool { agent, .. } = it else {
                continue;
            };
            let Some(id) = agent.agent_id.as_deref() else {
                continue;
            };
            match groups.iter_mut().find(|(gid, _, _)| gid == id) {
                Some((_, _, v)) => v.push(it),
                None => groups.push((
                    id.to_string(),
                    agent
                        .agent_type
                        .clone()
                        .unwrap_or_else(|| "subagent".to_string()),
                    vec![it],
                )),
            }
        }
        let mut rendered_agents: Vec<String> = Vec::new();

        // ── 1段目: 畳んで「出す行」を決める ─────────────────────────────
        // 完了した Read/検索/Bash が**連続**したら1行にまとめる。走行中(◌)は
        // 「いま何をしているか」なので畳まない。失敗(💥/🚫)も見えたまま残す。
        let mut lines: Vec<String> = Vec::new();
        let mut run: Vec<&RenderItem> = Vec::new();
        for it in items {
            if matches!(
                it,
                RenderItem::Tool { name, status: ToolStatus::Done, agent, .. }
                    if agent.agent_id.is_none() && Self::is_foldable(name)
            ) {
                run.push(it);
                continue;
            }
            Self::flush_fold_run(&mut run, &mut lines);
            // `Agent` 行は、それが起こした subagent のセクションと1ブロックに畳む。
            // 結び方は2通り: foreground は id が一致する。background/teammate は id 空間が違うので
            // **起動名**(tool_input.name)と agent_type で結ぶ(由来)。
            if let RenderItem::Tool { name, agent, .. } = it
                && (name == "Agent" || name == "Task")
            {
                let key = agent
                    .spawned_agent_id
                    .as_deref()
                    .filter(|id| {
                        groups.iter().any(|(gid, _, _)| gid == id)
                            && !rendered_agents.iter().any(|r| r == id)
                    })
                    .map(str::to_string)
                    .or_else(|| {
                        let nm = agent.launched_name.as_deref()?;
                        groups
                            .iter()
                            .find(|(gid, ty, _)| {
                                ty == nm && !rendered_agents.iter().any(|r| r == gid)
                            })
                            .map(|(gid, _, _)| gid.clone())
                    });
                if let Some(key) = key {
                    rendered_agents.push(key.clone());
                    if let Some((_, ty, rows)) = groups.iter().find(|(gid, _, _)| *gid == key) {
                        // 行頭の • を ▾ に差し替え、独立セクションの見出しと同じ形にする
                        let head = Self::render_item_line(it, false, false);
                        let head = match head.strip_prefix(TOOL_INDENT) {
                            Some(rest) => {
                                let body = rest.split_once(' ').map(|(_, b)| b).unwrap_or(rest);
                                format!("{TOOL_INDENT}{SUBAGENT_MARK} {body}")
                            }
                            None => head,
                        };
                        Self::push_agent_section(&mut lines, Some(head), ty, rows);
                    }
                    continue;
                }
                // 結ぶ相手がまだ居ない(agent が起動中 / ツールが1つも来ていない)→ 普通の行として描く
            }
            // subagent 自身のツール行: その agent のセクションを**1回だけ**、最初のツールの位置で出す
            if let RenderItem::Tool { agent, .. } = it
                && let Some(id) = agent.agent_id.as_deref()
            {
                if rendered_agents.iter().any(|r| r == id) {
                    continue;
                }
                rendered_agents.push(id.to_string());
                if let Some((_, ty, rows)) = groups.iter().find(|(gid, _, _)| gid == id) {
                    Self::push_agent_section(&mut lines, None, ty, rows);
                }
                continue;
            }
            lines.push(Self::render_item_line(it, !lines.is_empty(), true));
        }
        Self::flush_fold_run(&mut run, &mut lines);
        // 許可待ちの満期は**ツール行の下の注記**として最後に足す
        // (行の並びは触らない。中断通知より前)
        if perm_timed_out {
            lines.push(perm_timeout_line());
        }

        lines
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    impl StickyBoard {
        /// テスト用 — 本体のエージェントの行を、既定のスレッド `k` に。
        fn tool(&mut self, tool_use_id: &str, name: &str, summary: &str, status: ToolStatus) {
            self.tool_at(&ThreadKey::parse("k"), tool_use_id, name, summary, status);
        }

        /// テスト用 — 本体のエージェントの行を、指定したスレッドに。
        fn tool_at(&mut self, key: &ThreadKey, tool_use_id: &str, name: &str, summary: &str, status: ToolStatus) {
            self.upsert_tool_t(key, tool_use_id, name, summary, status, &AgentRef::default());
        }

        /// テスト用 — 差分の要らない行(input を見ない)。
        fn upsert_tool_t(
            &mut self,
            key: &ThreadKey,
            tool_use_id: &str,
            name: &str,
            summary: &str,
            status: ToolStatus,
            agent: &AgentRef,
        ) {
            self.upsert_tool(
                key,
                tool_use_id,
                name,
                summary,
                status,
                agent,
                &serde_json::Value::Null,
            );
        }
    }

    #[test]
    fn a_bash_row_that_is_really_a_search_counts_as_one() {
        for (command, is_search) in [
            // 素の検索コマンド
            ("grep -rn foo src/", true),
            ("rg --hidden pattern", true),
            ("git grep TODO", true),
            // 先頭の環境変数代入は読み飛ばす
            ("LC_ALL=C grep x file", true),
            ("A=1 B=2 rg x", true),
            // 絶対パスでも基底名で判定
            ("/usr/bin/grep x file", true),
            // パイプの**フィルタ**として使う grep は検索ではない(主コマンドは ps)
            ("ps ax | grep x", false),
            ("cargo test", false),
            ("", false),
            // git の別サブコマンドは検索ではない
            ("git log --oneline", false),
        ] {
            assert_eq!(StickyBoard::bash_is_search(command), is_search, "{command:?}");
        }
    }

    /// 何が変わったかを git 風の diff で出す。カウント・文脈の畳み・
    /// フェンスの無害化まで。
    #[test]
    fn edit_diff_renders_git_style_hunks_with_counts() {
        let old = (1..=12)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = old.replace("line1\n", "LINE1\n");
        let d = render_edit_diff(
            "Edit",
            &serde_json::json!({"old_string": old, "new_string": new}),
        )
        .unwrap();
        assert!(d.starts_with(" (+1 -1)\n```\n"), "{d}");
        assert!(d.contains("-line1\n+LINE1\n line2"), "{d}");
        assert!(d.contains("\n…"), "離れた文脈は … 1行に畳む: {d}");
        assert!(d.ends_with("\n```"), "{d}");

        // 変化が無ければ何も出さない(空の ``` を貼らない)
        assert!(
            render_edit_diff(
                "Edit",
                &serde_json::json!({"old_string":"x","new_string":"x"})
            )
            .is_none()
        );
        // Write は全行が追加
        let w = render_edit_diff("Write", &serde_json::json!({"content":"a\nb"})).unwrap();
        assert!(w.starts_with(" (+2 -0)\n"), "{w}");
        // 中身の ``` は zero-width space で分断する — でないとこちらのフェンスが先に閉じる
        let f = render_edit_diff("Write", &serde_json::json!({"content":"```"})).unwrap();
        assert!(f.contains("`\u{200b}`\u{200b}`"), "{f}");
        // MultiEdit は hunk を … で継ぐ
        let m = render_edit_diff(
            "MultiEdit",
            &serde_json::json!({"edits":[
                {"old_string":"a","new_string":"b"},
                {"old_string":"c","new_string":"d"},
            ]}),
        )
        .unwrap();
        assert!(m.starts_with(" (+2 -2)\n"), "{m}");
        assert!(m.contains("-a\n+b\n…\n-c\n+d"), "{m}");
    }

    /// 差分が乗るのは **main セッションの done の行だけ**。走行中の行と、畳んだ subagent の
    /// 窓には出さない(窓は1行ずつのままにする約束)。
    #[test]
    fn edit_diff_rides_the_main_row_only() {
        let k = ThreadKey::parse("k");
        let input = serde_json::json!({"old_string": "a", "new_string": "b"});

        let mut running = StickyBoard::default();
        running.upsert_tool(
            &k,
            "t1",
            "Edit",
            "/x.rs",
            ToolStatus::Pending,
            &AgentRef::default(),
            &input,
        );
        let body = running.take_dirty(10_000).pop().unwrap().2;
        assert!(!body.contains("```"), "走行中は出さない: {body}");

        let mut done = StickyBoard::default();
        done.upsert_tool(
            &k,
            "t1",
            "Edit",
            "/x.rs",
            ToolStatus::Done,
            &AgentRef::default(),
            &input,
        );
        let body = done.take_dirty(10_000).pop().unwrap().2;
        assert!(body.contains("Edit `/x.rs` (+1 -1)"), "{body}");
        assert!(body.contains("```\n-a\n+b\n```"), "{body}");

        let mut folded = StickyBoard::default();
        folded.upsert_tool(
            &k,
            "t1",
            "Edit",
            "/x.rs",
            ToolStatus::Done,
            &AgentRef {
                agent_id: Some("A1".into()),
                agent_type: Some("Explore".into()),
                ..Default::default()
            },
            &input,
        );
        let body = folded.take_dirty(10_000).pop().unwrap().2;
        assert!(!body.contains("```"), "畳んだ窓には持ち込まない: {body}");
    }

    #[test]
    fn an_agent_row_folds_together_with_the_subagent_it_spawned() {
        let mut b = StickyBoard::default();
        // main セッションの Agent 行(PostToolUse で spawned id が載る)
        b.upsert_tool_t(
            &ThreadKey::parse("k"),
            "t0",
            "Agent",
            "コードを調べる",
            ToolStatus::Done,
            &AgentRef {
                spawned_agent_id: Some("A1".into()),
                ..Default::default()
            },
        );
        let sub = AgentRef {
            agent_id: Some("A1".into()),
            agent_type: Some("Explore".into()),
            ..Default::default()
        };
        b.upsert_tool_t(
            &ThreadKey::parse("k"),
            "t1",
            "Read",
            "/a.rs",
            ToolStatus::Done,
            &sub,
        );
        let body = b.take_dirty(10_000).pop().unwrap().2;
        // 見出しは Agent 行だが、頭の • は ▾ に差し替わる
        assert!(
            body.contains(&format!(
                "{TOOL_INDENT}▾ Agent `コードを調べる` : Explore · Read 1 file"
            )),
            "{body}"
        );
        // 独立した ▾ Explore 見出しは**出ない**(二重に出さない)
        assert_eq!(body.matches('▾').count(), 1, "{body}");
    }

    #[test]
    fn a_background_agent_joins_by_name_when_the_ids_never_match() {
        // background/teammate は Agent 結果の id と、そのツール行の id が別空間で、
        // **id では永久に一致しない**。起動名(tool_input.name)と agent_type で結ぶ。
        // summary(= description)は起動名とは別物なので使えない
        let mut b = StickyBoard::default();
        b.upsert_tool_t(
            &ThreadKey::parse("k"),
            "t0",
            "Agent",
            "レビューを頼む", // description(summary に入る)— 名前とは別物
            ToolStatus::Done,
            &AgentRef {
                spawned_agent_id: Some("reviewer@session-9".into()),
                launched_name: Some("reviewer".into()), // ← これで結ぶ
                ..Default::default()
            },
        );
        let sub = AgentRef {
            agent_id: Some("B7".into()),
            agent_type: Some("reviewer".into()),
            ..Default::default()
        };
        b.upsert_tool_t(
            &ThreadKey::parse("k"),
            "t1",
            "Bash",
            "cargo test",
            ToolStatus::Done,
            &sub,
        );
        let body = b.take_dirty(10_000).pop().unwrap().2;
        assert!(
            body.contains("▾ Agent `レビューを頼む` : reviewer · Ran 1 command"),
            "{body}"
        );
        assert_eq!(body.matches('▾').count(), 1, "{body}");
    }

    #[test]
    fn an_agent_row_without_a_linked_group_renders_as_a_plain_row() {
        // まだ subagent のツールが1つも届いていない間は普通の行のまま
        let mut b = StickyBoard::default();
        b.tool("t0", "Agent", "調査", ToolStatus::Pending);
        let body = b.take_dirty(10_000).pop().unwrap().2;
        assert_eq!(body, format!("{TOOL_INDENT}◌ Agent `調査`"), "{body}");
    }

    #[test]
    fn a_subagents_tools_collapse_into_one_section_with_a_rolling_window() {
        let mut b = StickyBoard::default();
        let sub = AgentRef {
            agent_id: Some("A1".into()),
            agent_type: Some("Explore".into()),
            ..Default::default()
        };
        for (i, f) in ["/a.rs", "/b.rs", "/c.rs"].iter().enumerate() {
            b.upsert_tool_t(
                &ThreadKey::parse("k"),
                &format!("t{i}"),
                "Read",
                f,
                ToolStatus::Done,
                &sub,
            );
        }
        let body = b.take_dirty(10_000).pop().unwrap().2;
        // 見出しは ▾ + agent 名 + 内訳
        assert!(
            body.contains(&format!("{TOOL_INDENT}▾ Explore · Read 3 files")),
            "{body}"
        );
        // 直近2件だけ、さらに1段深いインデントで
        assert!(
            !body.contains("/a.rs"),
            "古い行はスクロールアウトする: {body}"
        );
        assert!(body.contains("/b.rs"), "{body}");
        assert!(body.contains("/c.rs"), "{body}");
    }

    #[test]
    fn parallel_subagents_stay_separate() {
        let mut b = StickyBoard::default();
        let a1 = AgentRef {
            agent_id: Some("A1".into()),
            agent_type: Some("Explore".into()),
            ..Default::default()
        };
        let a2 = AgentRef {
            agent_id: Some("A2".into()),
            agent_type: Some("general-purpose".into()),
            ..Default::default()
        };
        b.upsert_tool_t(
            &ThreadKey::parse("k"),
            "t1",
            "Read",
            "/a.rs",
            ToolStatus::Done,
            &a1,
        );
        b.upsert_tool_t(
            &ThreadKey::parse("k"),
            "t2",
            "Bash",
            "cargo test",
            ToolStatus::Done,
            &a2,
        );
        let body = b.take_dirty(10_000).pop().unwrap().2;
        assert!(body.contains("▾ Explore · Read 1 file"), "{body}");
        assert!(body.contains("▾ general-purpose · Ran 1 command"), "{body}");
    }

    #[test]
    fn a_subagent_section_sits_at_its_first_tools_position() {
        let mut b = StickyBoard::default();
        let sub = AgentRef {
            agent_id: Some("A1".into()),
            agent_type: Some("Explore".into()),
            ..Default::default()
        };
        b.push_narration(&ThreadKey::parse("k"), "先に言うこと");
        b.upsert_tool_t(
            &ThreadKey::parse("k"),
            "t1",
            "Read",
            "/a.rs",
            ToolStatus::Done,
            &sub,
        );
        b.push_narration(&ThreadKey::parse("k"), "後で言うこと");
        let body = b.take_dirty(10_000).pop().unwrap().2;
        let first = body.find("先に言うこと").unwrap();
        let sect = body.find("▾ Explore").unwrap();
        let last = body.find("後で言うこと").unwrap();
        assert!(
            first < sect && sect < last,
            "到着順のままであるべき: {body}"
        );
    }

    #[test]
    fn a_subagents_rows_are_never_folded_by_the_read_run_rule() {
        // subagent 側は自分のセクションで既に畳まれている。二重に畳まない
        let mut b = StickyBoard::default();
        let sub = AgentRef {
            agent_id: Some("A1".into()),
            agent_type: Some("Explore".into()),
            ..Default::default()
        };
        b.upsert_tool_t(
            &ThreadKey::parse("k"),
            "t1",
            "Read",
            "/a.rs",
            ToolStatus::Done,
            &sub,
        );
        b.upsert_tool_t(
            &ThreadKey::parse("k"),
            "t2",
            "Read",
            "/b.rs",
            ToolStatus::Done,
            &sub,
        );
        let body = b.take_dirty(10_000).pop().unwrap().2;
        assert!(body.contains("▾ Explore · Read 2 files"), "{body}");
        assert!(
            body.contains("/a.rs") && body.contains("/b.rs"),
            "窓は2件: {body}"
        );
        assert!(
            !body.contains("•  Read 2 files"),
            "run 畳みは適用しない: {body}"
        );
    }

    /// ツール名の無い progress(活動 ping)を**行にしてしまう**と、名前が空の行が
    /// 畳めないので連続した Read/検索の run を分断する。board 側の門番だけでは足りず、
    /// 空名の行が1つでも混ざると畳み込みが壊れることを固定する(実際に壊れていた退行)。
    /// 満期と Deny は**別の道**。
    /// 満期は止まったツール行を触らず注記1行だけ足す(別 upsert で ⚠️ に落とすと、
    /// perm フレームの tool_use_id が Pre の行の鍵と一致せず**行が二重になる**)。
    /// Deny はその行だけ 🚫 にして注記は出さない。
    #[test]
    fn perm_timeout_annotates_without_touching_the_row_and_deny_marks_only_the_row() {
        let k = ThreadKey::parse("k");

        // 満期: 行は ◌ のまま、下に注記
        let mut timed = StickyBoard::default();
        timed.tool_at(&k, "t1", "Bash", "rm -rf /tmp/x", ToolStatus::Pending);
        timed.on_perm_timeout(&k);
        let (_, out) = timed.take_final(&k).expect("付箋が出ていない");
        assert!(out.contains("⚠️ No answer to the permission request — timed out"), "{out}");
        assert!(out.contains("◌ Bash"), "行は触らず ◌ のまま: {out}");
        assert!(!out.contains("🚫"), "満期で行を落としてはいけない: {out}");

        // 満期は行が引けなくても注記だけ出る(鍵が一致しないケース)
        let mut lone = StickyBoard::default();
        lone.on_perm_timeout(&ThreadKey::parse("k2"));
        let (_, out) = lone
            .take_final(&ThreadKey::parse("k2"))
            .expect("付箋が出ていない");
        assert!(out.contains("⚠️ No answer to the permission request — timed out"), "{out}");

        // Deny: その行だけ 🚫。注記は出さない
        let mut denied = StickyBoard::default();
        denied.tool_at(&k, "t1", "Bash", "rm -rf /tmp/x", ToolStatus::Pending);
        denied.on_perm_denied(&k, "t1");
        let (_, out) = denied.take_final(&k).expect("付箋が出ていない");
        assert!(out.contains("🚫 Bash"), "{out}");
        assert!(!out.contains("ツール許可待ちタイムアウト"), "{out}");
    }

    #[test]
    fn an_empty_named_row_would_split_a_fold_run() {
        let k = ThreadKey::parse("k");
        let read = |b: &mut StickyBoard, id: &str, path: &str| {
            b.tool_at(&k, id, "Read", path, ToolStatus::Done);
        };

        // 素直に3連続 → 1行に畳まれる
        let mut good = StickyBoard::default();
        read(&mut good, "t1", "/a.rs");
        read(&mut good, "t2", "/b.rs");
        read(&mut good, "t3", "/c.rs");
        let (_, out) = good.take_final(&k).expect("付箋が出ていない");
        assert!(out.contains("Read 3 files"), "{out}");

        // 真ん中に空名の行が入ると run が割れて畳めない = 行にしてはいけない証拠
        let mut split = StickyBoard::default();
        read(&mut split, "t1", "/a.rs");
        split.tool_at(&k, "ping", "", "", ToolStatus::Done);
        read(&mut split, "t3", "/c.rs");
        let (_, out) = split.take_final(&k).expect("付箋が出ていない");
        assert!(
            !out.contains("Read 2 files"),
            "空名の行が run を分断していない = この検査が意味を失っている: {out}"
        );
    }

    #[test]
    fn a_run_of_finished_reads_folds_into_one_line() {
        let mut b = StickyBoard::default();
        b.tool("t1", "Read", "/a.rs", ToolStatus::Done);
        b.tool("t2", "Read", "/b.rs", ToolStatus::Done);
        b.tool("t3", "Grep", "foo", ToolStatus::Done);
        let body = b.take_dirty(10_000).pop().unwrap().2;
        assert_eq!(
            body,
            format!("{TOOL_INDENT}•  Read 2 files, Searched for 1 pattern"),
            "{body}"
        );
    }

    #[test]
    fn a_lone_finished_row_stays_expanded() {
        // 1本を畳んでも行は減らず、パスだけ見えなくなる — 畳むのは2本以上から
        let mut b = StickyBoard::default();
        b.tool("t1", "Read", "/a.rs", ToolStatus::Done);
        let body = b.take_dirty(10_000).pop().unwrap().2;
        assert_eq!(body, format!("{TOOL_INDENT}• Read `/a.rs`"), "{body}");
    }

    #[test]
    fn a_running_or_failed_row_is_never_folded() {
        let mut b = StickyBoard::default();
        b.tool("t1", "Read", "/a.rs", ToolStatus::Done);
        b.tool("t2", "Read", "/b.rs", ToolStatus::Pending); // 走行中
        b.tool("t3", "Read", "/c.rs", ToolStatus::Error); // 失敗
        let body = b.take_dirty(10_000).pop().unwrap().2;
        // 完了1本だけの run は畳まれず、走行中と失敗はそれぞれ自分の行を保つ
        assert!(body.contains("`/a.rs`"), "{body}");
        assert!(body.contains("◌ Read `/b.rs`"), "{body}");
        assert!(body.contains("💥 Read `/c.rs`"), "{body}");
        assert!(!body.contains("Read 2 files"), "{body}");
    }

    #[test]
    fn a_narration_breaks_the_run_in_two() {
        let mut b = StickyBoard::default();
        b.tool("t1", "Read", "/a.rs", ToolStatus::Done);
        b.tool("t2", "Read", "/b.rs", ToolStatus::Done);
        b.push_narration(&ThreadKey::parse("k"), "次を調べます");
        b.tool("t3", "Bash", "cargo test", ToolStatus::Done);
        b.tool("t4", "Bash", "cargo fmt", ToolStatus::Done);
        let body = b.take_dirty(10_000).pop().unwrap().2;
        assert!(body.contains("Read 2 files"), "{body}");
        assert!(body.contains("● 次を調べます"), "{body}");
        assert!(body.contains("Ran 2 commands"), "{body}");
    }

    #[test]
    fn a_grep_through_bash_counts_as_a_search_not_a_command() {
        let mut b = StickyBoard::default();
        b.tool("t1", "Bash", "rg foo src/", ToolStatus::Done);
        b.tool("t2", "Bash", "cargo test", ToolStatus::Done);
        let body = b.take_dirty(10_000).pop().unwrap().2;
        assert_eq!(
            body,
            format!("{TOOL_INDENT}•  Searched for 1 pattern, Ran 1 command"),
            "{body}"
        );
    }

    #[test]
    fn the_breakdown_groups_by_category_then_falls_back_to_the_tool_name() {
        // Read / 検索 / コマンド / 編集 の順、残りはツール名ごと
        let items = [
            ("Read", "/a.rs"),
            ("Grep", "foo"),
            ("Bash", "rg bar"), // grep 系 Bash は検索に数える
            ("Bash", "cargo test"),
            ("Edit", "/b.rs"),
            ("Write", "/c.rs"),
            ("WebFetch", "https://x"),
            ("WebFetch", "https://y"),
        ];
        assert_eq!(
            StickyBoard::tool_breakdown(&items),
            "Read 1 file, Searched for 2 patterns, Ran 1 command, Edited 2 files, WebFetch 2"
        );
    }

    #[test]
    fn the_breakdown_is_empty_without_tools() {
        assert_eq!(StickyBoard::tool_breakdown(&[]), "");
    }

    #[test]
    fn the_fold_summary_drops_zero_clauses() {
        // 0 件の節は落ち、単複を言い分け、順序は Read → Searched → Ran で固定
        let read = ("Read", "a.rs");
        let search = ("Grep", "fn main");
        let cmd = ("Bash", "cargo test");
        for (items, want) in [
            (vec![read; 3], "Read 3 files"),
            (vec![read], "Read 1 file"),
            (vec![search; 2], "Searched for 2 patterns"),
            (vec![search], "Searched for 1 pattern"),
            (vec![cmd], "Ran 1 command"),
            (
                vec![read, search, search, cmd, cmd, cmd],
                "Read 1 file, Searched for 2 patterns, Ran 3 commands",
            ),
        ] {
            assert_eq!(StickyBoard::tool_breakdown(&items), want);
        }
    }

    #[test]
    fn summarize_picks_salient_arg() {
        for (name, input, want) in [
            ("Bash", serde_json::json!({"command": "cargo test"}), "cargo test"),
            ("Read", serde_json::json!({"file_path": "/a/b.rs"}), "/a/b.rs"),
            ("Grep", serde_json::json!({"pattern": "foo"}), "foo"),
            ("Bash", serde_json::json!({"command": "a`b`\nc"}), "ab c"),
        ] {
            assert_eq!(StickyBoard::summarize(name, &input), want);
        }
        // 70 字 + …
        let long = serde_json::json!({"command": "x".repeat(80)});
        assert_eq!(StickyBoard::summarize("Bash", &long).chars().count(), 71);
    }

    #[test]
    fn tool_status_classification() {
        for (event, is_error, text, want) in [
            ("PreToolUse", false, "", ToolStatus::Pending),
            ("PostToolUse", false, "", ToolStatus::Done),
            ("PostToolUse", true, "permission denied", ToolStatus::Deny),
            ("PostToolUse", true, "boom", ToolStatus::Error),
        ] {
            assert_eq!(ToolStatus::of(event, is_error, text), want);
        }
    }

    #[test]
    fn denied_tools_never_become_rows() {
        assert!(StickyBoard::is_denied("TodoWrite"));
        assert!(StickyBoard::is_denied("mcp__agentgw__reply"));
        assert!(!StickyBoard::is_denied("Bash"));
        // 呼び手が漏らしても board が行にしない
        let mut b = StickyBoard::default();
        b.on_turn_start(&ThreadKey::parse("k"));
        b.tool("t", "TodoWrite", "x", ToolStatus::Done);
        b.tool("t2", "mcp__agentgw__reply", "hi", ToolStatus::Done);
        assert!(
            b.take_dirty(1_000).is_empty(),
            "denied tool must not even dirty the board"
        );
    }

    #[test]
    fn board_upserts_and_settles() {
        let mut b = StickyBoard::default();
        b.on_turn_start(&ThreadKey::parse("k"));
        b.tool("t1", "Bash", "cargo test", ToolStatus::Pending);
        b.tool("t1", "Bash", "cargo test", ToolStatus::Done);
        b.push_narration(&ThreadKey::parse("k"), "ビルドを確認します");
        let dirty = b.take_dirty(10_000);
        assert_eq!(dirty.len(), 1);
        assert!(dirty[0].2.contains("• Bash `cargo test`") && dirty[0].2.contains("● ビルド"));
        // 1秒以内の再 flush はレート保護で出てこない
        b.push_narration(&ThreadKey::parse("k"), "続き");
        assert!(b.take_dirty(10_500).is_empty());
        assert_eq!(b.take_dirty(11_100).len(), 1);
        // reply は残す / no_reply は消す
        b.set_posted(&ThreadKey::parse("k"), "999.1");
        assert!(matches!(
            b.settle(&ThreadKey::parse("k"), "reply"),
            StickyAction::Keep
        ));
        b.on_turn_start(&ThreadKey::parse("k"));
        b.tool("t2", "Read", "/x", ToolStatus::Done);
        b.set_posted(&ThreadKey::parse("k"), "999.2");
        assert!(
            matches!(b.settle(&ThreadKey::parse("k"), "no_reply"), StickyAction::Delete(ts) if ts == "999.2")
        );
    }

    /// 1枚に収まらなくなったら、そのメッセージを封じて続きを
    /// **次のメッセージ**に出す(切って捨てない)。Slack は約4000バイトを超える編集を拒む。
    #[test]
    fn an_overflowing_sticky_seals_the_page_and_continues_on_a_new_message() {
        let k = ThreadKey::parse("k");
        let mut b = StickyBoard::default();
        b.on_turn_start(&k);
        // 畳み対象**外**の Edit を使う。Bash/Read だ で1行に畳まれて溢れない
        for i in 0..500 {
            b.upsert_tool_t(
                &k,
                &format!("t{i}"),
                "Edit",
                &format!("{i}-{}", "x".repeat(60)),
                ToolStatus::Done,
                &AgentRef::default(),
            );
        }
        b.set_posted(&k, "999.1");

        let first = b.take_dirty(10_000);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].1.as_deref(), Some("999.1"), "1枚目は編集で締める");
        assert!(
            first[0].2.len() <= STICKY_BUDGET + 32,
            "len={}",
            first[0].2.len()
        );

        // 続きは**新しいメッセージ**。封じたページは二度と編集しないので ts を持たない。
        // スロットル(1秒)を待たずに続けて出る
        let second = b.take_dirty(10_100);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].1, None, "封じたページの続きは新規投稿");
        assert!(!second[0].2.is_empty());
        assert_ne!(first[0].2, second[0].2, "同じ内容を2度出さない");
    }

    /// ページの境目でコードブロックが割れても、両ページが自己完結する。
    #[test]
    fn a_code_fence_split_by_a_page_boundary_is_closed_and_reopened() {
        let lines: Vec<String> = vec![
            "```".into(),
            "a".repeat(3000),
            "b".repeat(3000),
            "```".into(),
        ];
        let (p1, next, open) = StickyBoard::page(&lines, 0, false);
        assert!(open, "1ページ目はフェンスが開いたまま終わる");
        assert!(
            p1.ends_with("\n```"),
            "封じる前に閉じる: {}",
            &p1[p1.len() - 8..]
        );
        assert!(next < lines.len());

        let (p2, end, still_open) = StickyBoard::page(&lines, next, open);
        assert!(p2.starts_with("```\n"), "次のページで開き直す: {p2}");
        assert_eq!(end, lines.len());
        assert!(!still_open, "最後の ``` で閉じている");
    }

    #[test]
    fn an_over_budget_single_line_is_clipped_so_the_page_still_sends() {
        // 6000 バイトのナレーション1本。行ごと落とすと後続が何も見えなくなるので頭出しする
        let mut items = vec![RenderItem::Narration {
            text: "あ".repeat(2000),
        }];
        items.extend((0..3).map(|i| RenderItem::Tool {
            id: format!("t{i}"),
            name: "Edit".into(),
            summary: "x".into(),
            status: ToolStatus::Done,
            agent: AgentRef::default(),
            diff: None,
        }));
        let out = StickyBoard::page(&StickyBoard::lines_of(&items, false), 0, false).0;
        assert!(out.len() <= STICKY_BUDGET + 32, "len={}", out.len());
        assert!(
            out.starts_with("● あああ"),
            "頭出しされていない: {:?}",
            &out[..20.min(out.len())]
        );
        assert!(
            out.ends_with('…'),
            "切ったことを示す: {:?}",
            &out[out.len() - 8..]
        );
    }

    /// 返事の後も働き続けたぶんは**新しい付箋**に出す(返事の下に1枚)。
    /// 黙ると決めたラウンド(no_reply / react)は決着後も沈黙のまま。
    #[test]
    fn work_after_a_reply_splits_into_a_new_sticky_but_silence_stays_silent() {
        let k = ThreadKey::parse("k");
        let mut b = StickyBoard::default();
        b.on_turn_start(&k);
        b.tool_at(&k, "t1", "Bash", "cargo test", ToolStatus::Done);
        b.set_posted(&k, "999.1");
        assert!(matches!(b.settle(&k, "reply"), StickyAction::Keep));

        b.push_narration(&k, "ついでに調べました");
        b.tool_at(&k, "t2", "Read", "/x", ToolStatus::Done);
        let dirty = b.take_dirty(99_000);
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].1, None, "返事の下に**新しく**出す(編集ではない)");
        assert!(
            dirty[0].2.contains("● ついでに調べました"),
            "{}",
            dirty[0].2
        );
        assert!(dirty[0].2.contains("Read"), "{}", dirty[0].2);
        assert!(
            !dirty[0].2.contains("cargo test"),
            "前の付箋の行を持ち越さない: {}",
            dirty[0].2
        );

        // 次ターンはまた新しい付箋から
        b.on_turn_start(&k);
        b.push_narration(&k, "次のターン");
        let dirty = b.take_dirty(100_000);
        assert_eq!(dirty.len(), 1);
        assert!(dirty[0].2.contains("● 次のターン"), "{}", dirty[0].2);
        assert!(
            !dirty[0].2.contains("ついでに"),
            "前ターンの遅刻分が混ざった"
        );

        // 締めの一言だけ(後に**ツールが動かない**)なら付箋は生えない。
        // (「● Slack に返信しました。」だけの付箋が返事の下に残らないこと)
        let mut r = StickyBoard::default();
        r.on_turn_start(&k);
        r.tool_at(&k, "t1", "Bash", "ls", ToolStatus::Done);
        r.set_posted(&k, "999.3");
        assert!(matches!(r.settle(&k, "reply"), StickyAction::Keep));
        r.push_narration(&k, "Slack に返信しました。");
        assert!(
            r.take_dirty(99_000).is_empty(),
            "締めのナレーションだけでは新しい付箋を起こさない"
        );
        assert!(r.take_final(&k).is_none());
        // 捨てるのは**ターンの終わり**。次の発言が来ないスレッドで残りっぱなしにしない
        // (`on_turn_start` を待つと、二度と喋られないスレッドのぶんが残る)
        r.on_turn_end(&k);
        r.tool_at(&k, "t2", "Read", "/x", ToolStatus::Done);
        let dirty = r.take_dirty(100_000);
        assert_eq!(dirty.len(), 1);
        assert!(
            !dirty[0].2.contains("返信しました"),
            "捨てたはずの控えが混ざった: {}",
            dirty[0].2
        );

        // 黙ると決めたラウンド — 決着後の行は付箋を生まない(沈黙が発言に見える事故を防ぐ)
        let mut q = StickyBoard::default();
        q.on_turn_start(&k);
        q.tool_at(&k, "t1", "Bash", "ls", ToolStatus::Done);
        q.set_posted(&k, "999.2");
        assert!(matches!(
            q.settle(&k, "no_reply"),
            StickyAction::Delete(ts) if ts == "999.2"
        ));
        q.push_narration(&k, "黙りました");
        assert!(
            q.take_dirty(99_000).is_empty(),
            "沈黙のあとのナレーションだけでは何も出さない"
        );
        // ただし**本物の仕事が動いたら**記録は出す(沈黙しても作業は残す)
        q.tool_at(&k, "t2", "Edit", "/x.rs", ToolStatus::Done);
        let after = q.take_dirty(99_000);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].1, None, "消した付箋を編集せず、新しく出す");
        assert!(after[0].2.contains("Edit"), "{}", after[0].2);
        assert!(q.take_final(&k).is_none());
    }

    #[test]
    fn interrupted_appends_notice_and_settles() {
        let mut b = StickyBoard::default();
        b.on_turn_start(&ThreadKey::parse("k"));
        b.tool("t1", "Bash", "x", ToolStatus::Pending);
        b.on_interrupted(&ThreadKey::parse("k"));
        let dirty = b.take_dirty(10_000);
        assert_eq!(dirty.len(), 1);
        // 進捗行の「下」に、空行を1つ挟んだ独立した段落として付く(置き換えではない)
        assert_eq!(
            dirty[0].2,
            "\u{A0}\u{A0}\u{A0}◌ Bash `x`\n\n└ `Interrupted by user.`"
        );
        // settled 後は新しい行が来ても沈黙(次の on_turn_start まで)
        b.push_narration(&ThreadKey::parse("k"), "続き");
        assert!(b.take_dirty(20_000).is_empty());
    }

    #[test]
    fn interrupted_with_no_progress_stands_alone() {
        let mut b = StickyBoard::default();
        b.on_turn_start(&ThreadKey::parse("k"));
        b.on_interrupted(&ThreadKey::parse("k"));
        let dirty = b.take_dirty(10_000);
        assert_eq!(dirty.len(), 1);
        // 先行行が無ければ空行は挟まない(頭が空行の付箋にしない)
        assert_eq!(dirty[0].2, "└ `Interrupted by user.`");
    }

    #[test]
    fn final_flush_ignores_the_throttle() {
        let mut b = StickyBoard::default();
        b.on_turn_start(&ThreadKey::parse("k"));
        b.tool("t1", "Bash", "cargo test", ToolStatus::Pending);
        assert_eq!(b.take_dirty(10_000).len(), 1);
        b.tool("t1", "Bash", "cargo test", ToolStatus::Done);
        // スロットル内でも決着直前の最終描画は出る(◌ のまま固まらない)
        assert!(
            b.take_dirty(10_100).is_empty(),
            "通常 flush はスロットルで出ない"
        );
        let (ts, body) = b.take_final(&ThreadKey::parse("k")).expect("final draw");
        assert_eq!(ts, None);
        assert!(body.contains("• Bash `cargo test`"), "{body}");
        assert!(
            b.take_final(&ThreadKey::parse("k")).is_none(),
            "2回目は返さない"
        );
        assert!(matches!(
            b.settle(&ThreadKey::parse("k"), "reply"),
            StickyAction::Keep
        ));
        assert!(
            b.take_final(&ThreadKey::parse("k")).is_none(),
            "settle 後は entry ごと消えている"
        );
    }
}
