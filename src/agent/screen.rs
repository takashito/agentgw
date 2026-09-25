//! Reading Claude Code's terminal screen: the prompts shown while it starts, the status
//! lines, `/model`, `/context` and `/usage` output. Pure text in, facts out — no tmux here.

use super::{CompactProgress, ContextReport, LoginOutcome, UsageRow};

/// Model names in the form `/model` takes as is. This is **claude's vocabulary**, so this table
/// is the source of truth; `Claude::models` returns it too.
pub const MODEL_NAMES: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];

/// Levels shown on the slider's status line. `auto` is **not** among them: under auto the line
/// names the level it resolved to.
const STATUS_LEVELS: [&str; 6] = ["low", "medium", "high", "xhigh", "max", "ultracode"];

/// Permission-mode footer line → our name for it. Below the input box the TUI states the
/// current mode as `⏵⏵ auto mode on (shift+tab to cycle)`
/// (table in cli 2.1.220: default→`manual mode` / plan→`plan mode` / acceptEdits→`accept edits` /
/// auto→`auto mode` / bypassPermissions→`bypass permissions` / dontAsk→`don't ask`).
///
/// **Checked on a real agent 2026-07-31** (`send-keys BTab` four times on a dev agent): the cycle
/// goes `auto → manual → accept edits → plan → auto`, and manual also shows a
/// `⏸ manual mode on` line. The `(shift+tab to cycle)` note comes and goes, so it is not used
/// as a marker.
/// bypass / dontask are not arguments of the `mode` command, but reading their lines keeps
/// them from being mistaken for manual, so they are in the table.
const MODE_LABELS: [(&str, &str); 6] = [
    ("plan mode on", "plan"),
    ("accept edits on", "edit"),
    ("auto mode on", "auto"),
    ("bypass permissions on", "bypass"),
    ("don't ask on", "dontask"),
    ("manual mode on", "manual"),
];

/// Markers of a failed sign-in.
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

/// Decoration that can lead a line **the screen printed itself** (frame, selector, option number).
const MODAL_DECORATION: &[char] = &[
    ' ', '\t', '│', '┃', '|', '┆', '╎', '▏', '▕', '>', '❯', '➤', '·', '•', '-', '–', '—',
];

/// Usage-limit modal prompts.
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

/// Sign-in screen prompts.
const LOGIN_PROMPTS: Prompts = &[(&[""], &["select login method:"])];

/// workspace-trust. **Anchored at line start** (not a plain substring match). The wording was
/// captured from a real screen on 2026-08-02: ` ❯ 1. Yes, I trust this folder` → decoration
/// stripped to `yes, i trust…`.
const TRUST_PROMPTS: Prompts = &[(&[""], &["yes, i trust this folder"])];

/// Screens that can appear in a freshly started window. **Until someone answers, claude reads
/// not a single character of the prompt.**
///
/// Of the four, the first two can be answered with Enter and the last two **cannot** (they are
/// about the account, so restarting hits the same wall every time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnScreen {
    /// claude's own workspace-trust. Shown when the cwd is not in `~/.claude.json`'s
    /// `projects[cwd].hasTrustDialogAccepted` (2026-08-02, real machine)
    Trust,
    /// dev channels confirmation. The default option is Yes, so Enter gets through
    Confirm,
    /// Sign-in screen. Enter does not dismiss it
    LoginRequired,
    /// Usage-limit modal. Enter does not dismiss it
    UsageLimited,
    None_,
}


/// "Optional prefix" × "stem": a way to express the patterns without `regex`
/// (`(?:…)?` also allows empty, hence the `""` prefix).
type Prompts = &'static [(&'static [&'static str], &'static [&'static str])];

/// claude's screen (`capture-pane` output) and headless run output. Read only.
///
/// The TUI can drift from one day to the next (two cases confirmed on real machines).
/// **These methods are the only place that absorbs that drift**; the `mod tests` below guards it.
pub struct Pane<'a>(&'a str);

impl<'a> Pane<'a> {
    pub fn new(text: &'a str) -> Self {
        Pane(text)
    }

    /// What a freshly started window is showing.
    ///
    /// **Account refusals are checked first**: the sign-in screen and the usage-limit modal can
    /// share the screen with other text, and pressing Enter after mistaking them for a slow
    /// confirmation prompt does not dismiss them.
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
        // Plain substring match here only. There is no real capture of this screen, and anchoring
        // on a guessed decoration would hurt more by **missing the real one**
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

    /// Parse the Markdown of `claude -p "/context"`. A leading warning (such as "workspace not
    /// trusted") is passed over; None without the Model/Tokens headers, so the caller can say
    /// "couldn't read it" instead of posting garbage.
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
                break; // the next heading ends the table
            }
            if !t.starts_with('|') {
                if !categories.is_empty() {
                    break;
                }
                continue;
            }
            // Drop both ends of `split('|')` (outside the table's vertical rules)
            let parts: Vec<&str> = t.split('|').map(str::trim).collect();
            let cells = parts.get(1..parts.len() - 1).unwrap_or(&[]);
            let [name, tokens, pct, ..] = cells else {
                continue;
            };
            if name.eq_ignore_ascii_case("category") {
                continue; // header row
            }
            if !name.is_empty() && name.chars().all(|c| c == '-') {
                continue; // markdown separator row
            }
            categories.push((name.to_string(), tokens.to_string(), pct.to_string()));
        }
        Some(ContextReport {
            model_label: ModelId::new(&model).friendly(&total),
            model,
            used,
            total,
            pct,
            categories,
        })
    }

    /// Parse the plain text of `claude -p "/usage"`. Only the limit rows
    /// `<label>: <n>% used [· resets <when>]` are taken; the "What's contributing…" breakdown after
    /// them is ignored. `· resets <when>` is **optional**: it is not printed at 0% (e.g. Current
    /// session right after a reset), but the row is still kept (dropping it was a bug). None if
    /// there are no rows.
    pub fn usage_rows(&self) -> Option<Vec<UsageRow>> {
        let rows: Vec<UsageRow> = self.0.split('\n').filter_map(parse_usage_line).collect();
        (!rows.is_empty()).then_some(rows)
    }

    /// Sign that a modal **holds the keyboard**. Every Claude Code dialog shows a cancel hint at
    /// its foot (real screens 2026-08-18: the auto mode onboarding's
    /// `Enter to confirm · Esc to cancel` / `/model`'s
    /// `Enter to set as default · s to use this session only · Esc to cancel`).
    ///
    /// A running turn says `esc to interrupt`, a **different phrase**, so a working agent is not
    /// mistaken for a stuck one. The check ignores the dialog type: modals we don't know by name
    /// are exactly what causes silence, so no table of wordings.
    pub fn modal_footer(&self) -> Option<&'a str> {
        let lines: Vec<&str> = self.0.split('\n').collect();
        let from = lines.len().saturating_sub(MODAL_FOOTER_TAIL);
        lines[from..]
            .iter()
            .find(|l| {
                let t = l.trim().to_ascii_lowercase();
                t.ends_with(CANCEL_HINT)
            })
            .copied()
    }

    /// The rows a modal is offering and which one the cursor sits on. `None` when no modal
    /// holds the screen, or when the `❯` above the hint is the input box rather than a
    /// choice list (nothing to pick from).
    ///
    /// Shape taken from real screens: the rows are one unbroken block just above the cancel
    /// hint, exactly one of them carrying the `❯` cursor. They may be numbered
    /// (`❯ 1. Set it up`) or not (`❯ Try again`), and the rows that are not selected
    /// are indented with NBSP or spaces.
    ///
    /// **The dialog is not looked up by name.** Modals we do not know are exactly the ones
    /// that get stuck, so everything shown comes off the screen itself.
    pub fn dialog(&self) -> Option<crate::agent::Dialog> {
        let footer = self.modal_footer()?;
        let lines: Vec<&str> = self.0.split('\n').collect();
        let at = lines.iter().position(|l| *l == footer)?;
        let from = at.saturating_sub(DIALOG_LOOK_BACK);
        // The cursor row, and only one near the hint: a `❯` further up is the echo of a
        // line that was already sent, which would drag scrollback in as options
        let cursor = from
            + lines[from..at]
                .iter()
                .rposition(|l| l.trim_start().starts_with('❯'))?;
        // Leading whitespace in bytes - enough to tell a row of the block from the paragraph
        // above it, which is written further left
        let indent = |l: &str| l.len() - l.trim_start().len();
        let cursor_indent = indent(lines[cursor]);
        let is_row = |l: &str| !l.trim().is_empty() && indent(l) >= cursor_indent;
        let top = lines[from..cursor]
            .iter()
            .rposition(|l| !is_row(l))
            .map_or(from, |i| from + i + 1);
        let bottom = cursor
            + lines[cursor..at]
                .iter()
                .position(|l| !is_row(l))
                .unwrap_or(at - cursor);
        // A numbered list writes each row's description on the line below it, indented further
        // (seen on the question dialog 2026-09-25). Those lines are the row's continuation, not
        // rows of their own: where numbers are used, they are what marks a row.
        let block = &lines[top..bottom];
        let numbered = block.iter().any(|l| row_number(l).is_some());
        let text = |l: &str| strip_modal_decoration(l.trim()).trim_end().to_string();
        // A numbered list writes each row's description on the line below it, indented further
        // (the question dialog, seen 2026-09-25). Those lines belong to the row above: where
        // numbers are used, a number is what marks a row.
        let mut rows: Vec<(usize, String, String)> = Vec::new();
        for (i, l) in block.iter().enumerate() {
            if !numbered || row_number(l).is_some() {
                rows.push((top + i, text(l), String::new()));
            } else if let Some((_, _, detail)) = rows.last_mut() {
                if !detail.is_empty() {
                    detail.push(' ');
                }
                detail.push_str(&text(l));
            }
        }
        // An empty row means that `\u{276f}` was the input box under the modal, not a choice
        if rows.is_empty() || rows.iter().any(|(_, label, _)| label.is_empty()) {
            return None;
        }
        let selected = rows.iter().position(|(i, _, _)| *i == cursor)?;
        Some(crate::agent::Dialog {
            title: dialog_title(&lines[from..top], footer),
            footer: footer.trim().to_string(),
            selected,
            asked: false,
            options: rows.iter().map(|(_, label, _)| label.clone()).collect(),
            details: rows.into_iter().map(|(_, _, detail)| detail).collect(),
        })
    }

    /// The TUI's live input box = the **last** `❯` line of the pane. A command **already sent**
    /// stays on screen as the echoed `❯ /effort high`, shaped exactly like a line being typed;
    /// only its position tells them apart.
    pub fn input_line(&self) -> &str {
        self.0
            .split('\n')
            .rfind(|l| l.trim_start().starts_with('❯'))
            .unwrap_or("")
    }

    /// True only while `cmd` (e.g. `/effort`) sits **unsent** in the input box. Asking the whole
    /// pane would say yes forever because of the echo above.
    pub fn still_has_command(&self, cmd: &str) -> bool {
        self.input_line().contains(cmd)
    }

    /// Input box empty = **it was sent**. The `❯` line itself stays even when empty (followed by
    /// U+00A0, not a plain space; `trim` uses White_Space, so both go). A long message wraps
    /// inside the box and the `❯` line holds **the first line currently visible**. Whatever the
    /// content, "not empty = not sent yet" holds.
    ///
    /// Also returns empty when there is no `❯` line at all (a permission prompt covers the box /
    /// the screen can't be read), so we never press Enter on something we can't see.
    pub fn input_box_empty(&self) -> bool {
        self.input_line()
            .trim_start()
            .trim_start_matches('❯')
            .trim()
            .is_empty()
    }

    /// Lines printed when `/effort <level>` takes effect. Each names the level:
    /// `Set effort level to high (saved as your default …)`, auto's `Effort level set to auto`,
    /// and, **when the same level is picked again**, `Kept effort level as xhigh`.
    ///
    /// The third, `Kept effort level as`, was confirmed on a real machine 2026-07-29 (it pairs with
    /// `model`'s `Kept model as`). Code written against the 2026-07-17 TUI missed it and timed out
    /// every time.
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
                // `\s+` needs at least one char / `\b` after the level (a following word char makes another word: `highest`)
                t.len() < rest.len()
                    && starts_with_ci(t, level)
                    && !t[level.len()..].starts_with(is_word)
            })
        })
    }

    /// Lines printed when `/model <name>` takes effect. The model is named by its **friendly
    /// name**, which contains the requested alias (`opus` → "Set model to Opus 4.8 …"). If already
    /// on that model it says "Kept model as Opus 4.8"; both mean "on the requested model".
    pub fn model_confirmed(&self, name: &str) -> bool {
        let pane = self.0;
        let name = name.to_ascii_lowercase();
        ["set model to", "kept model as"].iter().any(|marker| {
            match_indices_ci(pane, marker).into_iter().any(|i| {
                let rest = &pane[i + marker.len()..];
                // `\b` after the marker, and `[^\n]*`: look within the same line only
                !rest.starts_with(is_word) && {
                    let line = rest.split('\n').next().unwrap_or("");
                    // `\b` before the name (`myopus` is not opus)
                    match_indices_ci(line, &name)
                        .into_iter()
                        .any(|j| !line[..j].ends_with(is_word))
                }
            })
        })
    }

    /// Whether the `/effort` slider is on the pane (the footer hint line is a stable marker).
    pub fn effort_slider_open(&self) -> bool {
        self.0.contains("←/→ to adjust")
    }

    /// In a session **with history**, `/effort <level>` makes the TUI show a confirmation dialog
    /// before applying it (2026-07-29, real machine; a warning that the cache stops helping):
    /// ```text
    /// Change effort level?
    /// This conversation is cached for the current effort level. Switching to xhigh …
    /// ❯ 1. Yes, switch to xhigh
    ///   2. No, go back
    /// ```
    /// Only the heading is used as the marker; the option rows vary with the level. **The
    /// 2026-07-17 TUI had no such dialog.**
    pub fn effort_confirm_dialog_open(&self) -> bool {
        self.0.contains("Change effort level?")
    }

    /// Read the current effort level from the status line the TUI prints after the `/effort`
    /// slider is closed with Escape (right-aligned above the input box; both wordings measured):
    ///     ● high · /effort            (sometimes ○; the dot changes)
    ///     ✦ ultracode · xhigh effort + dynamic workflows for maximum thoroughness
    /// None if not on screen (not shown yet / replaced by another notice).
    ///
    /// `◉` (U+25C9) was added: the real machine on 2026-07-29 shows `◉ xhigh · /effort`.
    /// `●○✦` came from the 2026-07-17 TUI and the dot may change again (add new ones only here).
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

    /// The permission mode named by the footer **below** the input box. `manual` if there is no
    /// such line (the default is not announced).
    ///
    /// Only below the input box, so the same words in the conversation (a transcript discussing
    /// this feature) are not picked up; same anchor as `input_line`.
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

    /// Parse a captured pane. The anchor is the live compaction spinner, the line containing
    /// `Compacting conversation`, and from it we read:
    ///   • the elapsed timer in that line's parentheses, `(33s)` / `(1m 4s · …)` (if shown yet);
    ///   • the completion percent on the bar line just below (**limited** to the spinner line + the
    ///     next two, so a distant status line's `ctx:NN%` is never taken). Starting at the spinner
    ///     line also catches versions that put the % on the same line.
    /// A live spinner always shows one of them. A static mention (code, a comment, a transcript
    /// quoting this feature) has neither, so it is not mistaken for progress (the first version
    /// hung for minutes on this).
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
        // No live progress signal at all → just static text that happens to contain the words
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

    /// Extract `https://claude.com/cai/oauth/authorize?…` from the `claude auth login` pane.
    /// The URL is printed after `visit: `. A **wide** pane (the driver uses `-x 400`) fits it on
    /// one line; if a narrow pane wraps it, the continuation lines contain no whitespace, so they
    /// are joined back (stopping at a blank line, a line with spaces like "Paste code", or the
    /// end). None if absent.
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
                break; // blank line = end of URL / a line with spaces is body text ("Paste code"), not a wrap
            }
            joined.push_str(seg); // soft-wrap continuation: join it back
        }
        let i = joined.find(URL)?;
        let url: String = joined[i..]
            .chars()
            .take_while(|c| !c.is_whitespace())
            .collect();
        (url.len() > URL.len()).then_some(url) // `\S+` needs at least one char
    }

    /// Classify the pane by the real markers of `claude auth login` (checked against real success
    /// and failure output):
    ///   success → "Login successful." (printed inline after "Paste code here …")
    ///   invalid → "Invalid code. Please make sure the full code was copied."
    /// Success **requires the explicit success marker**, so a bad code (or a crash / abort) never
    /// looks like success, and the Owner is bound only on a real sign-in.
    /// pending = still waiting.
    pub fn login_outcome(&self) -> LoginOutcome {
        let low = self.0.to_ascii_lowercase();
        if low.contains("login successful") {
            return LoginOutcome::Success;
        }
        // Only `\berror\b` uses word boundaries (not `errorless` or inside an identifier)
        let bare_error = low
            .match_indices("error")
            .any(|(i, m)| !low[..i].ends_with(is_word) && !low[i + m.len()..].starts_with(is_word));
        if bare_error || LOGIN_ERROR_MARKERS.iter().any(|m| low.contains(m)) {
            LoginOutcome::Error
        } else {
            LoginOutcome::Pending
        }
    }

    /// From the tail of a transcript, take the model id named by the **last** assistant record
    /// (`/"model"\s*:\s*"(claude-[^"]+)"/g` as a string scan).
    /// Only ids starting with `claude-` count, so an error record's `"model":"<synthetic>"` is
    /// skipped. The tail may be cut mid-line (a partial line doesn't match and is ignored).
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
            // `[^"]+`: nothing after `claude-` means it is not an id
            let Some(end) = rest.find('"').filter(|e| *e > 0) else {
                continue;
            };
            last = Some(format!("claude-{}", &rest[..end]));
        }
        last
    }
}

/// A claude model identifier (a **full id** such as `claude-opus-4-8[1m]`).
pub struct ModelId<'a>(&'a str);

impl<'a> ModelId<'a> {
    pub fn new(id: &'a str) -> Self {
        ModelId(id)
    }

    /// The friendly name inside a full model id (`claude-fable-5` → `fable`). None if it contains
    /// none of the known names.
    pub fn alias(&self) -> Option<&'static str> {
        let lower = self.0.to_lowercase();
        MODEL_NAMES.iter().find(|n| lower.contains(**n)).copied()
    }

    /// Human-readable model name for headings: `("claude-opus-4-8", "1m")` → `Opus 4.8（1M context）`.
    /// The window note comes from the **Tokens total** (not the id's `[1m]` suffix): some versions
    /// omit the suffix even at 1M, while the total is always there. Falls back to the suffix only
    /// when the total can't be read. Non-claude ids are returned as is.
    pub fn friendly(&self, total: &str) -> String {
        let id = self.0;
        let t = total.trim();
        // `/^[\d.]+[mk]$/i`: ends in m/k with only digits and dots before it
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
// Hand-written scanning helpers (no `regex` dependency)

/// Prefix match ignoring ASCII case (in place of regex `/i`).
fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
}

/// regex `\w` (word character). `\b` checks both sides with this.
fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// All match positions ignoring ASCII case (like regex `/i` plus the engine advancing).
/// `to_ascii_lowercase` changes only ASCII, so byte positions match the original. Pass `needle` in lowercase.
fn match_indices_ci(hay: &str, needle: &str) -> Vec<usize> {
    hay.to_ascii_lowercase()
        .match_indices(needle)
        .map(|(i, _)| i)
        .collect()
}

/// The **value** of `**Key:**`: the line after skipping following whitespace (newlines included,
/// as `\s*` does). None if the value is empty.
fn header_value(raw: &str, key: &str) -> Option<String> {
    let i = raw.find(key)?;
    let rest = raw[i + key.len()..].trim_start();
    let line = rest.split('\n').next().unwrap_or("").trim_end();
    (!line.is_empty()).then(|| line.to_string())
}

/// `**Tokens:** <used> / <total> (<pct>)`, like a regex with three captures.
/// A malformed header is skipped and the next `**Tokens:**` is tried (as a regex would advance).
fn tokens_header(raw: &str) -> Option<(String, String, String)> {
    const KEY: &str = "**Tokens:**";
    for (i, _) in raw.match_indices(KEY) {
        let s = raw[i + KEY.len()..].trim_start();
        // `[^/\s]+`: a run with no slash and no whitespace
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

/// The rest after `used`, like `(?:.*?\bresets\s+(.+?))?\s*$`.
/// `Some("")` = the line ends with no reset clause / `Some(when)` = has a clause /
/// **None = not a limit row** (extra text that isn't `resets` remains).
fn usage_reset_clause(tail: &str) -> Option<String> {
    let lower = tail.to_ascii_lowercase(); // ASCII-only, so byte positions match tail
    let mut from = 0;
    while let Some(rel) = lower[from..].find("resets") {
        let i = from + rel;
        // `\b`: no boundary if the previous char is a word char (`presets` is not resets)
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

/// Read one line as a limit row. Hand-written `^\s*(.+?):\s*(\d+)%\s+used\b…$`:
/// the label is a lazy match, so split at **the first colon that parses**.
fn parse_usage_line(line: &str) -> Option<UsageRow> {
    for (ci, _) in line.match_indices(':') {
        let label = line[..ci].trim();
        if label.is_empty() {
            continue; // `(.+?)` needs at least one char
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
            continue; // `\s+` needs at least one char
        }
        if !starts_with_ci(trimmed, "used") {
            continue;
        }
        let rest = &trimmed["used".len()..];
        // `\b` right after `used`: a following word char makes another word (`usedxx`)
        if rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        // continue here too: if the rest is not a reset clause, only this colon was the wrong
        // split, and a lazy match tries the next colon (`A: 12% used, X: 66% used` matches at the second)
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

/// Elapsed seconds from `(33s)` / `(1m 4s · ↑ 876 tokens)`. Hand-written
/// `\((?:(\d+)m\s*)?(\d+)s\b[^)]*\)`: digits start **right after** `(` (`(elapsed 33s)` does not match).
fn parse_elapsed(spinner: &str) -> Option<u32> {
    for (i, _) in spinner.match_indices('(') {
        let s = &spinner[i + 1..];
        let d1: String = s.chars().take_while(char::is_ascii_digit).collect();
        if d1.is_empty() {
            continue;
        }
        let after = &s[d1.len()..];
        // `Nm` means minutes. Otherwise the first number is the seconds (the regex optional group backtracking)
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
        // After `s\b` comes `[^)]*\)`; `[^)]*` can't cross `)`, so this just means "a `)` follows"
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

/// Token count in the spinner line's parentheses, like `([↑↓])\s*([\d.]+[km]?)\s*tokens`.
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
        // `[km]?` is inside the capture (`1.6k` is printed as one token)
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

/// The **first** match of `(\d{1,3})\s*%` in a line. Skip whitespace before `%` and take up to 3 digits backwards.
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
            return digits.parse().ok(); // only the first match in the line
        }
    }
    None
}

/// Strip leading decoration, in order: decoration → option number → whitespace.
/// How far above the cancel hint a modal's own text can be. Past that is scrollback.
/// The words every cancel hint ends with.
const CANCEL_HINT: &str = "esc to cancel";

/// How close to the foot of the pane a real hint line sits. The TUI draws it just above the
/// input box: the tallest real case (the auto mode onboarding) leaves 5 lines below it.
///
/// **Both anchors earn their keep.** The check used to be "these words appear anywhere on the
/// screen", and a thread that merely *talked* about dialogs jammed itself: an agent's own
/// sentence with the words in the middle was read as a modal, every delivery was refused, and
/// the person was told to answer a dialog that was not there (57 refusals over an hour on a
/// real machine 2026-09-25). Ending with the words drops the sentence that runs on; the tail
/// drops the same words quoted higher up the conversation.
const MODAL_FOOTER_TAIL: usize = 10;

const DIALOG_LOOK_BACK: usize = 30;

/// Characters of the modal's question kept for the thread. A paragraph, not an essay.
const DIALOG_TITLE_CAP: usize = 300;

/// The line shown above the rows: the modal's question, or, when it does not ask one, the
/// paragraph sitting just above them. The hint line stands in when there is neither, which at
/// least says "this is a dialog".
///
/// **Whole paragraphs, not single lines.** The screen wraps a question over several lines, so
/// the `?` is rarely at the end of one — looking line by line missed the workspace-trust
/// question entirely and fell back to the hint. Only the lines above the rows are searched:
/// a `?` taken from the whole pane would pick up something a person said that is still in the
/// scrollback.
fn dialog_title(head: &[&str], footer: &str) -> String {
    let paragraphs: Vec<String> = head
        .split(|l| l.trim().is_empty())
        .filter(|p| !p.is_empty())
        .map(|p| {
            p.iter()
                .map(|l| strip_modal_decoration(l.trim()).trim_end())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|p| !p.is_empty())
        .collect();
    let title = paragraphs
        .iter()
        .rev()
        .find(|p| p.contains('?'))
        .or_else(|| paragraphs.last())
        .map_or_else(|| footer.trim().to_string(), |p| p.clone());
    if title.chars().count() <= DIALOG_TITLE_CAP {
        return title;
    }
    title.chars().take(DIALOG_TITLE_CAP - 1).collect::<String>() + "\u{2026}"
}

/// The number the TUI writes in front of a row (`\u{276f} 1. Set it up` -> `Some(1)`).
/// A row's description, written on the line below it, carries none.
fn row_number(line: &str) -> Option<u32> {
    let t = line.trim().trim_start_matches(MODAL_DECORATION);
    let digits = t.len() - t.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 || !(t[digits..].starts_with('.') || t[digits..].starts_with(')')) {
        return None;
    }
    t[..digits].parse().ok()
}

fn strip_modal_decoration(line: &str) -> &str {
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

/// "Is the screen **itself** printing this prompt?" **The line-start anchor is everything**: the
/// same words in the middle of a line (this repo's source, grep output, a quoted Slack message)
/// are not the screen.
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

/// Drop every `[...]` and trim (like `id.replace(/\[[^\]]*\]/g, '').trim()`).
/// An unclosed `[` is not a bracket; leave it.
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

/// Window size (`"1M"`) from the id's `[<n>m]` suffix. None if absent. `/\[(\d+)\s*m\]/i`
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

    // ── Reading the screen and output ─────────────────────────────────────────
    // Everything below **guards against TUI drift** (two cases confirmed on real machines).
    // Do not drop a single one; without them the next drift goes unnoticed.

    /// Same sample as the const of this name in the old `command.rs` (shared with the rendering tests).
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

    /// The 1M note comes from the **Tokens total** (some versions have the id's `[1m]`, some don't).
    #[test]
    fn context_window_note_comes_from_total() {
        assert_eq!(
            ModelId::new("claude-opus-4-8").friendly("1m"),
            "Opus 4.8（1M context）"
        );
        // Falls back to the id's `[<n>m]` suffix only when the total can't be read
        assert_eq!(
            ModelId::new("claude-opus-4-8[1m]").friendly("-"),
            "Opus 4.8（1M context）"
        );
        assert_eq!(
            ModelId::new("claude-haiku").friendly("200k"),
            "Haiku（200K context）"
        );
        assert_eq!(ModelId::new("gpt-4").friendly(""), "gpt-4"); // non-claude ids stay raw
    }

    /// Coverage for the old `command::model_alias` / `command::last_model_id`
    /// (split from the old `id_shapes_and_mentions`; the rest of the id-shape coverage is in `command.rs`).
    #[test]
    fn model_id_alias_and_transcript_tail() {
        assert_eq!(ModelId::new("claude-fable-5").alias().unwrap(), "fable");
        assert_eq!(ModelId::new("gpt-4").alias(), None);
        // The **last** claude- id wins. `<synthetic>` is not a model
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
        assert_eq!(rows[2].reset, ""); // a 0% row is kept even without resets
        assert!(Pane::new("garbage").usage_rows().is_none());
        // The lazy label match **does not drop the row**; it moves to the next colon
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
        // Real output from 2026-07 (`claude -p /usage` pasted as is): resets with a time zone, and
        // a "What's contributing" breakdown containing `%` and colons. Take **only** the limit rows
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
        assert_eq!(rows[0].reset, "Jul 29 at 12:29am (Asia/Tokyo)"); // don't split at the `12:29` in the line
    }

    #[test]
    fn input_line_is_the_last_prompt() {
        let pane = "❯ /effort high\nSet effort level to high (saved)\n❯ ";
        assert_eq!(Pane::new(pane).input_line().trim(), "❯"); // a sent echo is not the input line
        assert!(!Pane::new(pane).still_has_command("/effort"));
        assert!(Pane::new("❯ /effort").still_has_command("/effort"));
    }

    #[test]
    fn confirmations_match_both_wordings() {
        assert!(Pane::new("Set effort level to high (saved as default)").effort_confirmed("high"));
        assert!(Pane::new("Effort level set to auto").effort_confirmed("auto"));
        assert!(!Pane::new("Set effort level to highest").effort_confirmed("high")); // word boundary
        // The third wording, for re-picking the same level (the exact line from a real machine, 2026-07-29)
        assert!(Pane::new("Kept effort level as xhigh").effort_confirmed("xhigh"));
        assert!(!Pane::new("Kept effort level as xhigh").effort_confirmed("high")); // word boundary
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
        // A static mention (no timer, no %) is inactive
        assert!(
            !Pane::new("the words Compacting conversation appear in prose")
                .compact_progress()
                .active
        );
        // ctx:NN% is not taken as the percent
        let ctx = "✳ Compacting conversation… (3s)\n  ctx:42%\n";
        assert_eq!(Pane::new(ctx).compact_progress().percent, None);
    }

    /// Read the current permission mode from the footer line (real lines from 2026-07-31).
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
        // No such line → manual (if it ever disappears again, this falls back to the default)
        assert_eq!(Pane::new(&pane("")).mode_status(), "manual");
        // The same words in the conversation are above the input box, so they are not picked up
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
        // 2026-07-29 real machine (the exact line that failed E2E twice in a row): the dot drifted to ◉
        assert_eq!(
            Pane::new("                  ◉ xhigh · /effort").effort_status(),
            Some("xhigh")
        );
        assert_eq!(Pane::new("nothing here").effort_status(), None);
    }

    #[test]
    fn effort_change_dialog_is_detected_and_not_mistaken_for_a_confirmation() {
        // Full dialog from a real machine, 2026-07-29 (`/effort xhigh` in a session with history)
        let dialog = "\
Change effort level?
Your next response will be slower and use more tokens
This conversation is cached for the current effort level. Switching to xhigh means \
the full history gets re-read on your next message.
❯ 1. Yes, switch to xhigh
  2. No, go back";
        assert!(Pane::new(dialog).effort_confirm_dialog_open());
        // The dialog is **not yet** applied. Don't read it as done even though a line names the level
        assert!(!Pane::new(dialog).effort_confirmed("xhigh"));
        assert_eq!(Pane::new(dialog).effort_status(), None);
        // The option's `❯` row looks like the input box, but no command is left (don't invite an Enter retry)
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
        ); // soft-wrap joined back
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

    // ── Classifying and watching startup screens ─────────────────────────────
    // The real-screen wording was captured on 2026-08-02.

    /// Verbatim `tmux capture-pane` output from a real machine, 2026-08-02.
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

    /// Real screen from Claude Code 2.1.276 (2026-09-18). **No comes first and is selected by
    /// default**: pressing just Enter picks exit
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

    /// The screen above after pressing Down once.
    pub(crate) const TRUST_PANE_YES_SECOND: &str = "\
 Accessing workspace:

 /tmp

 Security guide

   No, exit
 \u{276f} Yes, I trust this folder

 Enter to confirm \u{b7} Esc to cancel";

    /// A real modal that stopped a thread on 2026-09-24, read back from a screenshot of the
    /// pane. Two things the numbered dialogs do not have: **the rows carry no numbers**, and
    /// the modal's heading is not the paragraph nearest them.
    ///
    /// Transcribed from an image, so the exact decoration (space or NBSP) cannot be sworn to.
    /// What the reading leans on — one block of rows above the hint, exactly one of them
    /// carrying the cursor — is plain in the picture.
    const PERMISSION_MODAL: &str = "\
\u{25cf} Calling a tool, running 2 shell commands \u{b7} 5m 47s\u{2026}\n\
 \u{2514} $ grep -rn 'something' docs/notes.md | head -20\n\
\n\
 Computer Use needs macOS permissions\n\
\n\
 Accessibility: \u{2718} not granted\n\
 Screen Recording: \u{2718} not granted\n\
\n\
 Grant the missing permissions in System Settings, then select \"Try again\".\n\
\n\
 \u{276f} Open System Settings \u{2192} Accessibility\n\
   Open System Settings \u{2192} Screen Recording\n\
   Try again\n\
\n\
 Enter to confirm \u{b7} Esc to cancel";

    /// A **real** pane, captured 2026-09-25 while a thread jammed. Nothing was on screen but
    /// the conversation — and the agent's own sentence about dialogs was read as a modal, so
    /// every delivery was refused for an hour (57 retries) and the person was told to go and
    /// answer a dialog that did not exist.
    ///
    /// Local details taken out; the load-bearing parts are kept as captured: the sentence runs
    /// on past the words, it sits 14 lines above the foot, and the status line at the very
    /// bottom says `esc to interrupt`, which a working agent always shows.
    const JAMMED_PANE: &str = "\
\u{25cf} The test never reached the new code path: the bridge only looks at the screen when a\n\
  \u{2014} then Slack's stop button sent ESC and cancelled it. The dialog's footer does contain Esc to cancel, so detection should fire.\n\
\n\
  I've asked whether to add the deferred screen watch.\n\
\n\
\u{273b} Cooked for 1m 16s \u{b7} done 1:47 AM\n\
\n\
\u{276f} [Image #1] I am seeing issue.\n\
  \u{23bf}  [Image #1]\n\
\n\
\u{25cf} Capturing the pane and finding the false footer\n\
\n\
\u{2500}\u{2500}\u{2500}\u{2500}\n\
\u{276f} \u{a0}\n\
\u{2500}\u{2500}\u{2500}\u{2500}\n\
  \u{23f5}\u{23f5} auto mode on (shift+tab to cycle) \u{b7} esc to interrupt \u{2190} for agents\n";

    #[test]
    fn a_sentence_about_dialogs_is_not_a_dialog() {
        let p = Pane::new(JAMMED_PANE);
        assert_eq!(
            p.modal_footer(),
            None,
            "the agent's own words were read as a modal"
        );
        assert_eq!(p.dialog(), None);
        // The same words at the foot of the pane, written as the TUI writes them, still count
        let real = format!("{JAMMED_PANE}\n \u{276f} 1. Go ahead\n   2. Stop\n\n Enter to confirm \u{b7} Esc to cancel");
        assert!(Pane::new(&real).modal_footer().is_some());
    }

    #[test]
    fn dialog_reads_rows_that_carry_no_numbers() {
        let d = Pane::new(PERMISSION_MODAL).dialog().expect("a modal is up");
        assert_eq!(
            d.options,
            [
                "Open System Settings \u{2192} Accessibility",
                "Open System Settings \u{2192} Screen Recording",
                "Try again",
            ]
        );
        assert_eq!(d.selected, 0);
        assert_eq!(d.footer, "Enter to confirm \u{b7} Esc to cancel");
        // No question on this screen, so the paragraph just above the rows stands in. The
        // heading two paragraphs up would read better, but nothing on the screen marks it as
        // the heading, and guessing is what put the hint line in the notice before this
        assert_eq!(
            d.title,
            "Grant the missing permissions in System Settings, then select \"Try again\"."
        );
    }

    /// The question dialog (`AskUserQuestion`), read back from a screenshot of a real pane
    /// 2026-09-25. **Every row carries a description on the line below it**, and Claude Code
    /// adds rows of its own past the ones that were asked for.
    ///
    /// From an image, so the decoration cannot be sworn to; the shape it is read for — a number
    /// in front of every row, descriptions indented under them — is plain in the picture.
    const QUESTION_MODAL: &str = "\
 Which one next?\n\
\n\
 \u{276f} 1. Watch the screen\n\
      Look at a quiet worker now and then, so nobody has to speak first\n\
   2. Update the laptop\n\
      It is the only one left behind\n\
   3. Nothing for now\n\
      Stop once this is confirmed\n\
   4. Type something.\n\
\n\
 Enter to select \u{b7} \u{2191}/\u{2193} to navigate \u{b7} Esc to cancel";

    /// The descriptions under each row used to be counted as rows of their own, which made a
    /// 4-row list look like 7 and put the buttons out of step with the screen.
    #[test]
    fn a_rows_description_is_not_a_row_of_its_own() {
        let d = Pane::new(QUESTION_MODAL).dialog().expect("a modal is up");
        assert_eq!(
            d.options,
            [
                "Watch the screen",
                "Update the laptop",
                "Nothing for now",
                "Type something.",
            ]
        );
        assert_eq!(d.selected, 0);
        assert_eq!(d.title, "Which one next?");
    }

    #[test]
    fn dialog_reads_numbered_rows_and_where_the_cursor_is() {
        let d = Pane::new(TRUST_PANE).dialog().expect("a modal is up");
        assert_eq!(d.options, ["Yes, I trust this folder", "No, exit"]);
        assert_eq!(d.selected, 0);
        // The question wins over the paragraph nearest the rows ("Security guide")
        assert!(d.title.starts_with("Quick safety check"), "{}", d.title);

        let moved = Pane::new(TRUST_PANE_YES_SECOND)
            .dialog()
            .expect("a modal is up");
        assert_eq!(moved.options, ["No, exit", "Yes, I trust this folder"]);
        assert_eq!(moved.selected, 1);
    }

    /// Why the reading refuses an empty row: under a modal the input box keeps its own
    /// `\u{276f}`, and taking that for a choice would send Enter into the box.
    #[test]
    fn dialog_does_not_take_a_bare_input_box_for_a_choice() {
        let pane = " \u{276f} \u{a0}\n\n Enter to confirm \u{b7} Esc to cancel";
        assert_eq!(Pane::new(pane).dialog(), None);
        assert_eq!(Pane::new("nothing is up here").dialog(), None);
    }

    #[test]
    fn the_trust_dialog_says_which_answer_is_selected() {
        assert_eq!(Pane::new(TRUST_PANE).trust_selected_is_yes(), Some(true));
        assert_eq!(Pane::new(TRUST_PANE_NO_FIRST).trust_selected_is_yes(), Some(false));
        assert_eq!(Pane::new(TRUST_PANE_YES_SECOND).trust_selected_is_yes(), Some(true));
        assert_eq!(Pane::new("no dialog here").trust_selected_is_yes(), None);
    }

    /// Sign-in screen (noted as real output from Claude Code v2.1.207).
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
        // How the real screen was drawn, as recorded by the earlier implementation (one line in a frame)
        assert_eq!(
            Pane::new("\u{2502} You've hit your session limit").spawn_screen(),
            SpawnScreen::UsageLimited
        );
        // Numbered option
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
    fn an_account_refusal_wins_over_the_ordinary_prompts() {
        // Order matters: sign-in / limit are not dismissed by answering like trust/confirm.
        // Enter won't clear them, so give up before pressing it
        let mixed = format!("{LOGIN_PANE}\n \u{276f} 1. Yes, I trust this folder");
        assert_eq!(Pane::new(&mixed).spawn_screen(), SpawnScreen::LoginRequired);
        assert_eq!(
            Pane::new("\u{2502} You've hit your session limit\nlocal development").spawn_screen(),
            SpawnScreen::UsageLimited
        );
    }

    #[test]
    fn the_words_inside_a_line_of_content_are_not_a_screen() {
        // An agent's screen showing its own source or grep output. Pressing Enter here would send
        // whatever is in the input box. The line-start anchor is the only safeguard
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
        // A false positive can stall the fleet. When in doubt, None_
        assert_eq!(Pane::new("").spawn_screen(), SpawnScreen::None_);
        assert_eq!(
            Pane::new("still booting\u{2026}").spawn_screen(),
            SpawnScreen::None_
        );
        assert_eq!(Pane::new("\u{276f} ").spawn_screen(), SpawnScreen::None_);
    }
}
