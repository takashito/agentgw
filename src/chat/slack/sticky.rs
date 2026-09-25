//! The progress message: one Slack message per turn that lists the tools the agent runs,
//! updated in place and folded away when the turn ends. Includes the Edit diff rendering.

use crate::chat::ThreadKey;

// ─── Progress message (progress sticky) ────────────────────────────────────
//
// StickyBoard is pure state — it does no Slack I/O. The flush loop in main looks at
// what `take_dirty` / `settle` return and does the post/update/delete through Api.

/// State of a tool row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    Done,
    Error,
    Deny,
}

impl ToolStatus {
    /// hook event → tool state. Anything that is not the end of a call is still running (Pending).
    /// A call that **fails gets no PostToolUse** — Claude Code sends `PostToolUseFailure` instead, so
    /// without it a failed row would sit at ◌ forever and break the fold run around it.
    /// Among failures, permission denials go to 🚫 instead of × (judged by the result text).
    pub fn of(hook_event_name: &str, is_error: bool, result_text: &str) -> ToolStatus {
        let failed = match hook_event_name {
            "PostToolUseFailure" => true,
            "PostToolUse" => is_error,
            _ => return ToolStatus::Pending,
        };
        if !failed {
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
            // **Not `•`**: Slack rewrites U+2022 as `-` inside a markdown block, wherever it sits —
            // leading whitespace, a zero-width space before it and even a code span make no difference
            // (measured on screen 2026-09-24). U+00B7 comes through as itself, at the same width.
            ToolStatus::Done => "·",
            // A plain × , not an emoji: it lines up with ◌ / · / ● at the same width
            ToolStatus::Error => "×",
            ToolStatus::Deny => "🚫",
        }
    }
}

/// What gets folded. Rows are merged into one line **only when completed ones run consecutively**.
/// Running (◌) and failed (×/🚫) rows are never folded — what is happening now must always stay visible.
const FOLD_READ: [&str; 1] = ["Read"];
const FOLD_SEARCH: [&str; 2] = ["Grep", "Glob"];

/// Editing tools (they get a diff, so they are never folded).
const EDIT_TOOLS: [&str; 3] = ["Edit", "MultiEdit", "Write"];

/// Edit/MultiEdit/Write show **what changed** as a git-style unified diff under the row.
/// No new plumbing is needed: PostToolUse's `tool_input` already carries
/// `old_string`/`new_string` (Edit), `edits[]` (MultiEdit) and `content` (Write).
/// The fence is tagged ```diff, so Slack colors removed lines red and added lines green — that needs the
/// message to go out as a `markdown` block (the sticky does; see `flush_sticky`), since plain mrkdwn drops
/// the language hint. The one-character prefix (`-` removed / `+` added / ` ` context) is git's own. Shown **only on done rows** and **only on main-session rows**
/// (a folded subagent window stays one line per row).
const DIFF_CTX: usize = 3;
const DIFF_MAX_LINES: usize = 16;
const DIFF_MAX_BYTES: usize = 900;
const DIFF_MAX_LINE: usize = 120;
/// Upper bound for building the LCS DP table. Beyond it we fall back to a naive "delete all + add all" (the output is clipped above anyway).
const DIFF_MAX_INPUT_LINES: usize = 200;

/// Break runs of ``` with a zero-width space before clipping. A single bare ``` line
/// **closes our fence early**, so it must always be neutralized on the content side.
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

/// Line-level LCS diff — the shape git produces (matches are context, the rest is `-`/`+`). Edit works on
/// small fragments, so O(n·m) is enough.
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
    // dp[i][j] = length of the longest common subsequence of a[i..] and b[j..]
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

/// Fold the diff into git-style hunks: keep only `DIFF_CTX` lines of context around each change and
/// replace any other unchanged stretch with a single `…` line.
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

/// The diff appended to a tool row. It returns **the continuation of the row** in the form `" (+A -R)\n```…```"`,
/// or `None` when there is nothing to show (no text change / no input).
/// MultiEdit hunks are separated by `…`.
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
    // The truncation footnote is **two lines** (the count on one line, then `…`)
    if dropped > 0 {
        capped.push(format!("(+{dropped} more)"));
        capped.push("…".to_string());
    }
    Some(format!(
        " (+{added} -{removed})\n```diff\n{}\n```",
        capped.join("\n")
    ))
}

/// Links a progress-message row to a subagent.
///
/// **Which subagent this row belongs to**, or **which subagent it started**.
/// Comes from the hook payload; all None for tool calls in the main session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentRef {
    /// id of the subagent that ran this row (top-level `agent_id` in the payload).
    pub agent_id: Option<String>,
    /// Name of that subagent (`agent_type`: Explore / general-purpose etc.).
    pub agent_type: Option<String>,
    /// id of the subagent an `Agent` row started (`tool_response.agentId` / `.agent_id`).
    pub spawned_agent_id: Option<String>,
    /// Name an `Agent` row launched with (`tool_input.name`). Needed to link background/teammate agents **by name**.
    /// **`summary` cannot stand in for it** — `Self::summarize("Agent", …)` returns `description`,
    /// a different field from the launch name (which reads `input.name`).
    pub launched_name: Option<String>,
}

/// One row of a progress message.
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
        /// Diff of an editing tool that finished (`" (+A -R)\n```…```"`).
        /// Built **when the event arrives, not at render time** — the progress message does not keep
        /// `tool_input`, so this is the only moment the input is at hand.
        diff: Option<String>,
    },
    /// Closing line for an interruption. A raw line with no glyph (the notice is pushed as is).
    Interrupted,
}

/// Prefix of a narration line. Slack turns ⏺ (U+23FA) into an emoji, so we use ●.
const NARR_GLYPH: &str = "●";
/// Indent of a tool row. **NBSP, not a plain space** — Slack collapses leading whitespace.
const TOOL_INDENT: &str = "\u{A0}\u{A0}\u{A0}";
/// Slack strips the leading whitespace of a message's **first line** — NBSP included — so the top
/// tool row came out flush left while every row under it kept its indent. A zero-width space is not
/// whitespace and renders as nothing, so it holds the indent in place (added in `wrap_fences`).
const INDENT_GUARD: &str = "\u{200B}";
/// Line appended to the progress message when a permission prompt expires without anyone pressing it.
/// It carries TOOL_INDENT so it reads as a **note under**
/// the stalled tool row.
fn perm_timeout_line() -> String {
    crate::t!(
        "\u{A0}\u{A0}\u{A0}⚠️ No answer to the permission request — timed out",
        "\u{A0}\u{A0}\u{A0}⚠️ ツール許可の返事が無く、タイムアウトしました"
    )
}

/// Budget (bytes) for one progress message. Leaves headroom below Slack's message body limit.
const STICKY_BUDGET: usize = 3800;
/// Closing line for a round cut off by stop. It goes **under**
/// the progress so far — it shows the interruption while keeping "how far it got".
const INTERRUPTED_NOTICE: &str = "└ `Interrupted by user.`";

/// Number of recent rows shown in a folded subagent section (a rolling window).
/// Slack cannot scroll **inside** a message, so this "last N" stands in for scrolling.
const SUBAGENT_WINDOW: usize = 2;
/// Heading marker for a section.
const SUBAGENT_MARK: &str = "▾";
/// Indent of section rows (stacked on top of the tool row's TOOL_INDENT).
const SUBAGENT_INDENT: &str = "\u{A0}\u{A0}";

/// Result of settle — keep the progress message as a record, or delete it.
#[derive(Debug, PartialEq, Eq)]
pub enum StickyAction {
    Keep,
    Delete(String),
}

/// The progress message for one round.
#[derive(Default)]
struct Sticky {
    items: Vec<RenderItem>,
    posted_ts: Option<String>,
    dirty: bool,
    last_flush_ms: Option<u64>,
    /// A permission wait expired. The note stays until the next round.
    perm_timed_out: bool,
    /// **Number of lines already shown on sealed pages**. The page being grown now
    /// starts here. Slack rejects edits over about 4000 bytes, so once it no longer fits on one message
    /// we seal that message (never edit it again) and continue in the next message.
    sealed_lines: usize,
    /// Whether the current page starts **inside a code block** (carried over from the previous page).
    sealed_open_fence: bool,
    /// How many times a not-yet-posted page overflowed. Sealed pages are never edited again, so
    /// the ts that post returns is **discarded** (keeping it would make the next page edit it).
    sealed_awaiting_post: usize,
}

/// thread_key → the progress message in flight. One per thread (no pagination).
#[derive(Default)]
pub struct StickyBoard {
    stickies: std::collections::HashMap<ThreadKey, Sticky>,
    /// Settled thread → **whether follow-up work after the answer may open a new progress message**.
    /// `true` is a round answered with reply/edit; `false` is a round that went silent via no_reply/react or an interruption.
    settled: std::collections::HashMap<ThreadKey, bool>,
    /// Narration that arrived after settling. **Kept, not drawn** (`push_narration` /
    /// `open_after_answer` below put it in and take it out).
    held: std::collections::HashMap<ThreadKey, Vec<String>>,
    /// Threads whose turn is over (the agent stopped). The harness asks it to keep talking after
    /// that -- "your previous response had no visible output" -- so text and even tool calls trickle
    /// in for a while, aimed at its terminal, not at the thread. Nothing that arrives now may draw
    /// or open anything. Cleared at the next `on_turn_start`.
    closed: std::collections::HashSet<ThreadKey>,
}

impl StickyBoard {
    /// Build the lines and truncate to the budget. Truncation is always shown as `…(N more)` (never cut silently).
    /// One item per line.
    /// `lead_blank` says whether to put a blank line before `Interrupted` when it has preceding lines.
    /// `with_diff` says whether to attach the diff block of editing tools under the row. True only for
    /// main-session rows — a folded subagent window stays one line per row.
    fn render_item_line(it: &RenderItem, lead_blank: bool, with_diff: bool) -> String {
        match it {
            RenderItem::Narration { text } => format!("{NARR_GLYPH} {text}"),
            // With preceding lines, put a blank line in between so it is its own paragraph.
            // It is built as a single line including the blank, so the budget count still adds up
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

    /// Emit the accumulated run as one line (2 or more) or as a plain row (just 1).
    /// A run of one is not folded (it saves no lines and only hides the path or command).
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
                // The **two spaces** after the glyph line up with unfolded rows
                lines.push(format!("{TOOL_INDENT}·  {}", Self::tool_breakdown(&pairs)));
            }
        }
        run.clear();
    }

    /// One folded subagent section. If `header` is None it gets its own
    /// `▾ <type> · <breakdown>` heading; if Some, the Agent row is used as the heading.
    /// Only the latest SUBAGENT_WINDOW rows, indented one level deeper.
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

    /// The last line (exclusive) from `start` that fits the budget. **The budget is measured in bytes**
    /// because Slack's limit is in bytes; counting characters hits the limit early with Japanese text,
    /// the edit is rejected, and the progress message freezes. **Always advances at least one line** (a line that is over budget on its own gets its own page).
    fn pack_cut(lines: &[String], start: usize, budget: usize) -> usize {
        let mut len = 0usize;
        let mut i = start;
        while i < lines.len() {
            let add = usize::from(i > start) + lines[i].len(); // the joining \n is one byte
            if len + add > budget && i > start {
                break;
            }
            len += add;
            i += 1;
        }
        i
    }

    /// Whether the line opens/closes a ```. The diff **content** never matches
    /// (`clip_diff_line` splits runs of ``` with a zero-width space).
    fn is_fence_toggle(line: &str) -> bool {
        line.trim_start().starts_with("```")
    }

    /// Make one page a self-contained code block. `open_in` says whether a fence came in open
    /// from the previous page (if so, reopen it at the top). If the page ends with a fence still open,
    /// close it at the end. Returns (body, whether a fence is open at the end of the page).
    fn wrap_fences(page: &[String], open_in: bool) -> (String, bool) {
        let mut in_fence = open_in;
        for ln in page {
            if Self::is_fence_toggle(ln) {
                in_fence = !in_fence;
            }
        }
        let mut parts: Vec<&str> = Vec::new();
        if open_in {
            // Reopen tagged: the only fences the sticky writes are diffs, and an untagged
            // reopen would leave page 2 of a long diff uncolored
            parts.push("```diff");
        }
        parts.extend(page.iter().map(String::as_str));
        if in_fence {
            parts.push("```");
        }
        let text = parts.join("\n");
        let text = match text.starts_with('\u{A0}') {
            true => format!("{INDENT_GUARD}{text}"),
            false => text,
        };
        (text, in_fence)
    }

    /// Build one page starting at line `from`. Returns (body, first line of the next page, fence state at the end of the page).
    /// If the next page's first line is `lines.len()`, this page is the last.
    fn page(lines: &[String], from: usize, open_fence: bool) -> (String, usize, bool) {
        let cut = Self::pack_cut(lines, from, STICKY_BUDGET);
        // A page whose single line exceeds the budget is truncated to its head (otherwise Slack rejects the whole edit)
        if cut == from + 1 && lines[from].len() > STICKY_BUDGET {
            let head = Self::clip(&lines[from], STICKY_BUDGET);
            let (text, end) = Self::wrap_fences(&[head], open_fence);
            return (text, cut, end);
        }
        let (text, end) = Self::wrap_fences(&lines[from..cut], open_fence);
        (text, cut, end)
    }

    /// Drop hooks that arrive late after settling. Without this, an orphan progress message holding only
    /// "● …replied" appears (after no_reply, what should be silence looks like a message — reproduced 3 times in E2E).
    /// ponytail: silent after settling until the next turn. If resuming is needed, add the resume condition then
    fn settled(&self, key: &ThreadKey) -> bool {
        self.settled.contains_key(key)
    }

    /// The agent sometimes keeps working after answering (the "and also explain this" kind).
    /// Dropping that progress leaves nothing in Slack, so **open a new progress message under the answer**.
    ///
    /// Only one is opened per settle (later rows go into the same message). **Not opened for
    /// rounds that chose silence** — progress appearing after no_reply / react / an interruption
    /// makes what should be silence look like a message (the same accident as the caveat on `settled`).
    ///
    /// A new progress message may be opened **only when a real tool runs**
    /// (= proof that work continues after the answer). Only tool rows reach here; narration after
    /// settling is diverted to the held buffer by `push_narration` and never arrives.
    ///
    /// Returns "whether this row may be drawn".
    /// `by_tool` = whether this row is a tool. **A round settled in silence resumes only on a tool** —
    /// if real work runs after no_reply / react, it needs a record.
    /// Narration alone does not open one.
    fn open_after_answer(&mut self, key: &ThreadKey, by_tool: bool) -> bool {
        // Work that starts after the turn ended is not the round continuing -- it is the agent
        // answering the harness. It gets no progress message of its own.
        if self.closed.contains(key) {
            return false;
        }
        match self.settled.get(key) {
            None => true, // not settled yet — business as usual
            Some(false) if !by_tool => false,
            _ => {
                self.settled.remove(key);
                let mut s = Sticky::default();
                // Show the held narration **above** the follow-up work
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

    /// New round. The previous progress message stays in Slack as a record; we stop tracking it.
    pub fn on_turn_start(&mut self, key: &ThreadKey) {
        self.stickies.insert(key.clone(), Sticky::default());
        self.settled.remove(key);
        self.held.remove(key); // do not carry over to the next turn (a safety net for anything not dropped at turn end)
        self.closed.remove(key);
    }

    /// The turn ended. Held narration nobody showed by now **was a closing remark**,
    /// so drop it. Keeping it until the next message
    /// arrives would leave it behind forever for threads where no message ever comes again.
    ///
    /// The round also **closes here**: text and tool calls keep trickling in for a while afterwards
    /// (the harness asks the agent to keep talking once the answer went out as a tool call alone),
    /// and none of it is addressed to the thread. Only `on_turn_start` opens it again.
    pub fn on_turn_end(&mut self, key: &ThreadKey) {
        self.held.remove(key);
        self.closed.insert(key.clone());
    }

    /// A PostToolUse with the same tool_use_id replaces the PreToolUse ◌ row with •/×/🚫.
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
            return; // noise and our own tools never become rows — a board invariant, not a caller discipline
        }
        if !self.open_after_answer(key, true) {
            return;
        }
        // The diff **can only be built here** (the progress message does not keep the input). Only
        // for editing tools that are done. Showing it while running (◌) would present unapplied changes as "changed"
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
                // The PostToolUse diff lands later on the row created at PreToolUse
                if diff.is_some() {
                    *cur_diff = diff;
                }
                // At PreToolUse we do not yet know "which subagent it started". It first appears at
                // PostToolUse, so **only take values that arrive later** (never erase a value we already know)
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

    /// Add one line of narration that became final (accumulating deltas is the caller's job).
    pub fn push_narration(&mut self, key: &ThreadKey, text: &str) {
        // Straggling text after the turn ended is addressed to the terminal -- not kept either,
        // or it would sit in the buffer until the thread speaks again
        if self.closed.contains(key) {
            return;
        }
        // Narration after settling is **kept, not drawn**. If a real tool runs after the answer,
        // that proves "work continues", and `open_after_answer` shows it all together.
        // If the turn ends with nothing running, it was a closing remark and the next
        // `on_turn_start` drops it. Without this, a progress message holding only "● Replied in Slack."
        // appears under the answer and nobody deletes it
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

    /// A round cut off by stop. Add one closing line and go silent there — unlike settle,
    /// the progress message stays (the progress until the cut is the record). It resumes at the next `on_turn_start`.
    /// A permission prompt expired without being pressed.
    ///
    /// **The stalled tool row is left alone** (stays ◌). Dropping it to ⚠️ with a separate upsert
    /// **duplicates the row** — the perm frame's tool_use_id does not always match the key
    /// of the PreToolUse row.
    /// Only one indented note line is added.
    pub fn on_perm_timeout(&mut self, key: &ThreadKey) {
        if self.settled(key) {
            return;
        }
        let s = self.stickies.entry(key.clone()).or_default();
        s.perm_timed_out = true;
        s.dirty = true;
    }

    /// Deny was pressed. Mark **that tool's row** 🚫 (a separate path from expiry;
    /// no note line). Does nothing if the row cannot be found.
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
        // A round silenced by interruption — later progress does not open a new progress message
        self.settled.insert(key.clone(), false);
    }

    /// Remember the ts that was posted (updates from then on).
    pub fn set_posted(&mut self, key: &ThreadKey, ts: &str) {
        let s = self.stickies.entry(key.clone()).or_default();
        // This was the post of a sealed page — do not remember its ts (the next page would edit it)
        if s.sealed_awaiting_post > 0 {
            s.sealed_awaiting_post -= 1;
            return;
        }
        s.posted_ts = Some(ts.to_string());
    }

    /// ts of the **progress message currently shown** in this thread. Used to tell whether
    /// the message a stop emoji was added to is the progress message.
    pub fn sticky_ts(&self, key: &ThreadKey) -> Option<String> {
        self.stickies.get(key).and_then(|s| s.posted_ts.clone())
    }

    /// Return the progress messages that need redrawing as (key, posted ts, body).
    /// **Nothing is returned for a thread within 1 second of the last one** — protects Slack's edit rate.
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
                // It no longer fits. **Seal this page** and move on to the next message.
                // The sealed page's ts is no longer needed (never edited again), so let it go; on the next pass
                // the rest is posted as a new message. `dirty` stays set and the throttle
                // is not advanced — so the rest goes out on the next tick
                let sealed_ts = s.posted_ts.take();
                if sealed_ts.is_none() {
                    // Overflowed while the page was not posted yet — it will be posted now,
                    // but that ts belongs to the sealed page, so do not take it
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

    /// Final render at settle. Ignores the throttle and returns once (called right before settle).
    /// Without it, a progress message whose last PostToolUse settled within 1 second stays at `◌`.
    pub fn take_final(&mut self, key: &ThreadKey) -> Option<(Option<String>, String)> {
        let s = self.stickies.get_mut(key)?;
        if !s.dirty {
            return None;
        }
        s.dirty = false;
        // The final render is also only for **the page being grown now** (sealed pages are not edited). Even if
        // it overflows here, no next page is opened — settling ends tracking of this progress message
        let lines = Self::lines_of(&s.items, s.perm_timed_out);
        let (body, _, _) = Self::page(&lines, s.sealed_lines, s.sealed_open_fence);
        Some((s.posted_ts.clone(), body))
    }

    /// Settle the round. If it replied, the progress message stays as a record. If it went silent (no_reply)
    /// or only reacted, the progress looks like "the bot talking to itself", so delete it.
    /// Either way tracking ends — so the flush loop does not bring back a deleted message.
    pub fn settle(&mut self, key: &ThreadKey, kind: &str) -> StickyAction {
        // Only answered rounds allow a new progress message for follow-up progress
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

    /// Tools not shown in the progress message (noise and our own tools).
    pub fn is_denied(name: &str) -> bool {
        matches!(name, "TodoWrite" | "ToolSearch" | "advisor") || name.starts_with("mcp__agentgw__")
    }

    /// The most prominent argument of the tool input, on one line.
    /// It goes into Slack inline code, so newlines and backquotes are dropped, and it is cut with `…` at 70 chars.
    pub fn summarize(name: &str, input: &serde_json::Value) -> String {
        let get = |k: &str| {
            input
                .get(k)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        };
        let raw = match name {
            "Bash" => get("command"),
            // NotebookEdit has no file_path
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

    /// Whether the tool may be folded (Read / Grep / Glob / Bash).
    pub fn is_foldable(name: &str) -> bool {
        FOLD_READ.contains(&name) || FOLD_SEARCH.contains(&name) || name == "Bash"
    }

    /// Bash rows that ran a `grep`-like command count as "Searched for N patterns",
    /// not "Ran N commands". Agent sessions have no Grep/Glob tools, so
    /// code search actually runs as `grep`/`rg` through Bash.
    ///
    /// Judged by **the command launched** (the first token, skipping leading `VAR=val`, absolute paths reduced to the base name).
    /// grep used as a pipe **filter** (`ps ax | grep x`) is not counted because the main command is ps.
    pub fn bash_is_search(command: &str) -> bool {
        const SEARCH_CMDS: [&str; 7] = ["grep", "egrep", "fgrep", "rg", "ripgrep", "ag", "ack"];
        let mut s = command.trim();
        // Drop a leading `LC_ALL=C ` and the like
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

    /// Breakdown of the tools a subagent ran. Shown in the heading of the folded
    /// section so "what this agent did" is visible at a glance. **Every status is counted**, so
    /// the parts add up to the total. Anything outside the categories is grouped by tool name ("WebFetch 2").
    ///
    /// Takes `(tool name, summary)`. The Bash check uses only the first token, so
    /// a summary already clipped to 70 chars is enough.
    pub fn tool_breakdown(items: &[(&str, &str)]) -> String {
        let (mut reads, mut searches, mut cmds, mut edits) = (0usize, 0usize, 0usize, 0usize);
        // Keep order of appearance (with a HashMap the order of "WebFetch 2, Skill 1" is unstable)
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

    /// Longest prefix that fits in `room` bytes (**on a char boundary**) + `…`. Empty if not even one char fits.
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

    /// Build the lines to show (up to folding). Splitting into pages comes after this.
    fn lines_of(items: &[RenderItem], perm_timed_out: bool) -> Vec<String> {
        // Tools run inside a subagent are **not drawn in place**. Each agent gets one
        // section, shown once at the position of that agent's first tool.
        // This keeps a subagent running dozens of tools from filling the progress message.
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

        // ── Stage 1: fold and decide which lines to show ─────────────────────
        // Completed Read/search/Bash rows that are **consecutive** become one line. Running (◌) rows
        // are "what is happening now", so they are not folded. Failures (×/🚫) also stay visible.
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
            // An `Agent` row is folded into one block with the section of the subagent it started.
            // Two ways to link: for foreground the ids match. background/teammate live in a different id space, so
            // they are linked by **launch name** (tool_input.name) and agent_type (origin).
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
                        // Replace the leading glyph with ▾ so it looks like a standalone section heading
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
                // Nothing to link to yet (agent still starting / no tool has arrived yet) → draw as a normal row
            }
            // A subagent's own tool row: show that agent's section **only once**, at its first tool
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
        // An expired permission wait is added last as **a note under the tool row**
        // (the row order is not touched; before the interruption notice)
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
        /// For tests — a main agent row in the default thread `k`.
        fn tool(&mut self, tool_use_id: &str, name: &str, summary: &str, status: ToolStatus) {
            self.tool_at(&ThreadKey::parse("k"), tool_use_id, name, summary, status);
        }

        /// For tests — a main agent row in the given thread.
        fn tool_at(&mut self, key: &ThreadKey, tool_use_id: &str, name: &str, summary: &str, status: ToolStatus) {
            self.upsert_tool_t(key, tool_use_id, name, summary, status, &AgentRef::default());
        }

        /// For tests — a row that needs no diff (input is not read).
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
            // Plain search commands
            ("grep -rn foo src/", true),
            ("rg --hidden pattern", true),
            ("git grep TODO", true),
            // Leading environment variable assignments are skipped
            ("LC_ALL=C grep x file", true),
            ("A=1 B=2 rg x", true),
            // Absolute paths are judged by their base name
            ("/usr/bin/grep x file", true),
            // grep used as a pipe **filter** is not a search (the main command is ps)
            ("ps ax | grep x", false),
            ("cargo test", false),
            ("", false),
            // Other git subcommands are not searches
            ("git log --oneline", false),
        ] {
            assert_eq!(StickyBoard::bash_is_search(command), is_search, "{command:?}");
        }
    }

    /// Show what changed as a git-style diff: counts, context folding,
    /// and fence neutralizing.
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
        assert!(d.starts_with(" (+1 -1)\n```diff\n"), "{d}");
        assert!(d.contains("-line1\n+LINE1\n line2"), "{d}");
        assert!(d.contains("\n…"), "離れた文脈は … 1行に畳む: {d}");
        assert!(d.ends_with("\n```"), "{d}");

        // No change, nothing shown (never paste an empty ```)
        assert!(
            render_edit_diff(
                "Edit",
                &serde_json::json!({"old_string":"x","new_string":"x"})
            )
            .is_none()
        );
        // Write is all additions
        let w = render_edit_diff("Write", &serde_json::json!({"content":"a\nb"})).unwrap();
        assert!(w.starts_with(" (+2 -0)\n"), "{w}");
        // ``` in the content is split with a zero-width space — otherwise our fence closes early
        let f = render_edit_diff("Write", &serde_json::json!({"content":"```"})).unwrap();
        assert!(f.contains("`\u{200b}`\u{200b}`"), "{f}");
        // MultiEdit joins hunks with …
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

    /// The diff goes **only on done rows in the main session**. Not on running rows, nor in a folded
    /// subagent window (the window stays one line per row by design).
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
        assert!(body.contains("```diff\n-a\n+b\n```"), "{body}");

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
        // A main-session Agent row (the spawned id arrives with PostToolUse)
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
        // The heading is the Agent row, but its leading • is replaced by ▾
        assert!(
            body.contains(&format!(
                "{TOOL_INDENT}▾ Agent `コードを調べる` : Explore · Read 1 file"
            )),
            "{body}"
        );
        // A standalone ▾ Explore heading does **not** appear (never shown twice)
        assert_eq!(body.matches('▾').count(), 1, "{body}");
    }

    #[test]
    fn a_background_agent_joins_by_name_when_the_ids_never_match() {
        // For background/teammate, the id in the Agent result and the id on its tool rows live in different spaces
        // and **will never match**. Link by launch name (tool_input.name) and agent_type.
        // summary (= description) is different from the launch name, so it cannot be used
        let mut b = StickyBoard::default();
        b.upsert_tool_t(
            &ThreadKey::parse("k"),
            "t0",
            "Agent",
            "レビューを頼む", // description (goes into summary) — not the name
            ToolStatus::Done,
            &AgentRef {
                spawned_agent_id: Some("reviewer@session-9".into()),
                launched_name: Some("reviewer".into()), // ← this is what links them
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
        // Stays a normal row until the subagent's first tool arrives
        let mut b = StickyBoard::default();
        b.tool("t0", "Agent", "調査", ToolStatus::Pending);
        let body = b.take_dirty(10_000).pop().unwrap().2;
        assert_eq!(body, format!("{INDENT_GUARD}{TOOL_INDENT}◌ Agent `調査`"), "{body}");
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
        // Heading is ▾ + agent name + breakdown
        assert!(
            body.contains(&format!("{TOOL_INDENT}▾ Explore · Read 3 files")),
            "{body}"
        );
        // Only the latest 2, indented one level deeper
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
        // The subagent's rows are already folded in its own section. Do not fold them twice
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
            !body.contains("·  Read 2 files"),
            "run 畳みは適用しない: {body}"
        );
    }

    /// Turning a progress event with no tool name (an activity ping) **into a row** splits a run of
    /// consecutive Read/search rows, because a row with an empty name cannot be folded. The board-side gate alone is not enough;
    /// this pins that even one empty-name row breaks folding (a regression that actually happened).
    /// Expiry and Deny are **separate paths**.
    /// Expiry leaves the stalled tool row alone and only adds one note line (dropping it to ⚠️ with a separate upsert
    /// **duplicates the row**, because the perm frame's tool_use_id does not match the Pre row's key).
    /// Deny marks only that row 🚫 and shows no note.
    #[test]
    fn perm_timeout_annotates_without_touching_the_row_and_deny_marks_only_the_row() {
        let k = ThreadKey::parse("k");

        // Expiry: the row stays ◌, with a note below
        let mut timed = StickyBoard::default();
        timed.tool_at(&k, "t1", "Bash", "rm -rf /tmp/x", ToolStatus::Pending);
        timed.on_perm_timeout(&k);
        let (_, out) = timed.take_final(&k).expect("付箋が出ていない");
        assert!(out.contains("⚠️ No answer to the permission request — timed out"), "{out}");
        assert!(out.contains("◌ Bash"), "行は触らず ◌ のまま: {out}");
        assert!(!out.contains("🚫"), "満期で行を落としてはいけない: {out}");

        // On expiry the note appears even if the row cannot be found (the key-mismatch case)
        let mut lone = StickyBoard::default();
        lone.on_perm_timeout(&ThreadKey::parse("k2"));
        let (_, out) = lone
            .take_final(&ThreadKey::parse("k2"))
            .expect("付箋が出ていない");
        assert!(out.contains("⚠️ No answer to the permission request — timed out"), "{out}");

        // Deny: only that row becomes 🚫. No note
        let mut denied = StickyBoard::default();
        denied.tool_at(&k, "t1", "Bash", "rm -rf /tmp/x", ToolStatus::Pending);
        denied.on_perm_denied(&k, "t1");
        let (_, out) = denied.take_final(&k).expect("付箋が出ていない");
        assert!(out.contains("🚫 Bash"), "{out}");
        assert!(!out.contains("ツール許可待ちタイムアウト"), "{out}");
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
            format!("{INDENT_GUARD}{TOOL_INDENT}·  Read 2 files, Searched for 1 pattern"),
            "{body}"
        );
    }

    #[test]
    fn a_lone_finished_row_stays_expanded() {
        // Folding a single row saves no lines and only hides the path — folding starts at two
        let mut b = StickyBoard::default();
        b.tool("t1", "Read", "/a.rs", ToolStatus::Done);
        let body = b.take_dirty(10_000).pop().unwrap().2;
        assert_eq!(body, format!("{INDENT_GUARD}{TOOL_INDENT}· Read `/a.rs`"), "{body}");
    }

    #[test]
    fn a_running_or_failed_row_is_never_folded() {
        let mut b = StickyBoard::default();
        b.tool("t1", "Read", "/a.rs", ToolStatus::Done);
        b.tool("t2", "Read", "/b.rs", ToolStatus::Pending); // running
        b.tool("t3", "Read", "/c.rs", ToolStatus::Error); // failed
        let body = b.take_dirty(10_000).pop().unwrap().2;
        // A run with only one completed row is not folded; running and failed rows each keep their own line
        assert!(body.contains("`/a.rs`"), "{body}");
        assert!(body.contains("◌ Read `/b.rs`"), "{body}");
        assert!(body.contains("× Read `/c.rs`"), "{body}");
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
    fn the_breakdown_groups_by_category_then_falls_back_to_the_tool_name() {
        // Read / search / command / edit in that order, the rest by tool name
        let items = [
            ("Read", "/a.rs"),
            ("Grep", "foo"),
            ("Bash", "rg bar"), // grep-like Bash counts as a search
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
    fn summarize_picks_salient_arg() {
        for (name, input, want) in [
            ("Bash", serde_json::json!({"command": "cargo test"}), "cargo test"),
            ("Read", serde_json::json!({"file_path": "/a/b.rs"}), "/a/b.rs"),
            ("Grep", serde_json::json!({"pattern": "foo"}), "foo"),
            ("Bash", serde_json::json!({"command": "a`b`\nc"}), "ab c"),
        ] {
            assert_eq!(StickyBoard::summarize(name, &input), want);
        }
        // 70 chars + …
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
            // A failed call arrives as PostToolUseFailure — it ends the row even if is_error is missing
            ("PostToolUseFailure", false, "no such file", ToolStatus::Error),
            ("PostToolUseFailure", true, "permission denied", ToolStatus::Deny),
        ] {
            assert_eq!(ToolStatus::of(event, is_error, text), want);
        }
    }

    #[test]
    fn denied_tools_never_become_rows() {
        assert!(StickyBoard::is_denied("TodoWrite"));
        assert!(StickyBoard::is_denied("mcp__agentgw__reply"));
        assert!(!StickyBoard::is_denied("Bash"));
        // Even if the caller lets it through, the board does not make it a row
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
        assert!(dirty[0].2.contains("· Bash `cargo test`") && dirty[0].2.contains("● ビルド"));
        // A re-flush within 1 second is held back by the rate guard
        b.push_narration(&ThreadKey::parse("k"), "続き");
        assert!(b.take_dirty(10_500).is_empty());
        assert_eq!(b.take_dirty(11_100).len(), 1);
        // reply keeps it / no_reply deletes it
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

    /// When it no longer fits on one message, seal that message and continue
    /// in **the next message** (never cut and drop). Slack rejects edits over about 4000 bytes.
    #[test]
    fn an_overflowing_sticky_seals_the_page_and_continues_on_a_new_message() {
        let k = ThreadKey::parse("k");
        let mut b = StickyBoard::default();
        b.on_turn_start(&k);
        // Use Edit, which is **not** folded. With Bash/Read they would fold into one line and never overflow
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

        // The rest goes to **a new message**. The sealed page is never edited again, so it keeps no ts.
        // It goes out right away without waiting for the throttle (1 second)
        let second = b.take_dirty(10_100);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].1, None, "封じたページの続きは新規投稿");
        assert!(!second[0].2.is_empty());
        assert_ne!(first[0].2, second[0].2, "同じ内容を2度出さない");
    }

    /// Even when a code block is split at a page boundary, both pages are self-contained.
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
        assert!(p2.starts_with("```diff\n"), "次のページで開き直す: {p2}");
        assert_eq!(end, lines.len());
        assert!(!still_open, "最後の ``` で閉じている");
    }

    #[test]
    fn an_over_budget_single_line_is_clipped_so_the_page_still_sends() {
        // One 6000-byte narration. Dropping the whole line would hide everything after it, so it is truncated to its head
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

    /// The first row keeps its indent: Slack strips the leading whitespace of a message's first
    /// line, so a zero-width space goes in front of it.
    #[test]
    fn the_top_row_keeps_its_indent() {
        let items = vec![RenderItem::Tool {
            id: "t1".into(),
            name: "WebFetch".into(),
            summary: "https://x".into(),
            status: ToolStatus::Done,
            agent: AgentRef::default(),
            diff: None,
        }];
        let out = StickyBoard::page(&StickyBoard::lines_of(&items, false), 0, false).0;
        assert!(out.starts_with(&format!("{INDENT_GUARD}{TOOL_INDENT}")), "{out:?}");

        // A narration row starts flush left anyway — nothing to guard
        let out = StickyBoard::page(
            &StickyBoard::lines_of(&[RenderItem::Narration { text: "hi".into() }], false),
            0,
            false,
        )
        .0;
        assert!(out.starts_with(NARR_GLYPH), "{out:?}");
    }

    /// Work continuing after the answer goes to **a new progress message** (one under the answer).
    /// Rounds that chose silence (no_reply / react) stay silent after settling.
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

        // The next turn starts from a new progress message again
        b.on_turn_start(&k);
        b.push_narration(&k, "次のターン");
        let dirty = b.take_dirty(100_000);
        assert_eq!(dirty.len(), 1);
        assert!(dirty[0].2.contains("● 次のターン"), "{}", dirty[0].2);
        assert!(
            !dirty[0].2.contains("ついでに"),
            "前ターンの遅刻分が混ざった"
        );

        // If there is only a closing remark (**no tool runs** after it), no progress message appears.
        // (No progress message holding only "● Replied in Slack." is left under the answer)
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
        // Dropping happens **at turn end**. Do not leave it behind in threads where no message comes next
        // (waiting for `on_turn_start` leaves behind entries for threads that never speak again).
        // Past that point the round is closed -- see `nothing_after_the_turn_ended_reaches_the_thread`
        r.on_turn_end(&k);
        r.tool_at(&k, "t2", "Read", "/x", ToolStatus::Done);
        assert!(
            r.take_dirty(100_000).is_empty(),
            "ターンが終わった後は何も起こさない"
        );

        // A round that chose silence — rows after settling do not create a progress message (prevents silence looking like a message)
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
        // But **when real work runs**, the record is shown (the work is kept even after going silent)
        q.tool_at(&k, "t2", "Edit", "/x.rs", ToolStatus::Done);
        let after = q.take_dirty(99_000);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].1, None, "消した付箋を編集せず、新しく出す");
        assert!(after[0].2.contains("Edit"), "{}", after[0].2);
        assert!(q.take_final(&k).is_none());
    }

    /// The bug this guards, read off four live cases: the round replied, the turn ended, and then
    /// the harness asked the agent to keep talking ("no visible output"). What it wrote next was a
    /// summary for its terminal, and a tool it ran afterwards opened a progress message that
    /// replayed the summary -- clipped -- under the answer, reading as a broken duplicate reply.
    ///
    /// **Nothing that arrives after the turn ended reaches the thread**, text or tool.
    /// Continued work *within* the turn is untouched (the test above covers it).
    #[test]
    fn nothing_after_the_turn_ended_reaches_the_thread() {
        let k = ThreadKey::parse("k");
        let mut b = StickyBoard::default();
        b.on_turn_start(&k);
        b.tool_at(&k, "t1", "Bash", "cargo test", ToolStatus::Done);
        b.set_posted(&k, "999.1");
        assert!(matches!(b.settle(&k, "reply"), StickyAction::Keep));
        b.on_turn_end(&k); // the agent stopped -- the round is over

        // The harness nudges it; everything from here is aimed at the terminal
        b.push_narration(&k, "Slack に返信しました。要点は…");
        b.tool_at(&k, "t2", "Bash", "git status", ToolStatus::Done);
        b.push_narration(&k, "まとめると以上です");
        assert!(
            b.take_dirty(99_000).is_empty(),
            "ターンが終わった後の地の文もツールも出さない"
        );
        assert!(b.take_final(&k).is_none());

        // The next turn is business as usual again
        b.on_turn_start(&k);
        b.push_narration(&k, "次のターン");
        let dirty = b.take_dirty(100_000);
        assert_eq!(dirty.len(), 1);
        assert!(dirty[0].2.contains("● 次のターン"), "{}", dirty[0].2);
    }

    #[test]
    fn interrupted_appends_notice_and_settles() {
        let mut b = StickyBoard::default();
        b.on_turn_start(&ThreadKey::parse("k"));
        b.tool("t1", "Bash", "x", ToolStatus::Pending);
        b.on_interrupted(&ThreadKey::parse("k"));
        let dirty = b.take_dirty(10_000);
        assert_eq!(dirty.len(), 1);
        // Goes "under" the progress rows as its own paragraph after one blank line (not a replacement)
        assert_eq!(
            dirty[0].2,
            "\u{200B}\u{A0}\u{A0}\u{A0}◌ Bash `x`\n\n└ `Interrupted by user.`"
        );
        // After settling, new rows stay silent (until the next on_turn_start)
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
        // No blank line without preceding lines (the progress message never starts with a blank line)
        assert_eq!(dirty[0].2, "└ `Interrupted by user.`");
    }

    #[test]
    fn final_flush_ignores_the_throttle() {
        let mut b = StickyBoard::default();
        b.on_turn_start(&ThreadKey::parse("k"));
        b.tool("t1", "Bash", "cargo test", ToolStatus::Pending);
        assert_eq!(b.take_dirty(10_000).len(), 1);
        b.tool("t1", "Bash", "cargo test", ToolStatus::Done);
        // Even within the throttle, the final render right before settling goes out (it does not freeze at ◌)
        assert!(
            b.take_dirty(10_100).is_empty(),
            "通常 flush はスロットルで出ない"
        );
        let (ts, body) = b.take_final(&ThreadKey::parse("k")).expect("final draw");
        assert_eq!(ts, None);
        assert!(body.contains("· Bash `cargo test`"), "{body}");
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
