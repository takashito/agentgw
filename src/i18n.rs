//! Language for what agentgw says to people: the CLI, the installer and Slack.
//!
//! English by default; `AGENTGW_LANG=ja` switches to Japanese. The setting is read from
//! the process environment first, then from the state directory's `.env`, once per process.
//!
//! Logs are **not** translated — they stay English so they can be searched and compared.
//!
//! Every translated message is written with [`t!`](crate::t), which keeps the two languages
//! side by side at the call site:
//!
//! ```ignore
//! let msg = t!("Linked {name}.", "{name} をつなぎました。");
//! ```

use std::cell::Cell;
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    Ja,
}

impl Lang {
    /// `ja` / `ja_JP.UTF-8` / `japanese` → Japanese; anything else (or nothing) → English.
    pub fn parse(value: Option<&str>) -> Lang {
        match value.map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if v == "ja" || v.starts_with("ja_") || v.starts_with("ja-") || v == "japanese" => {
                Lang::Ja
            }
            _ => Lang::En,
        }
    }
}

static LANG: OnceLock<Lang> = OnceLock::new();

thread_local! {
    /// Tests pin the language per thread (they run in parallel in one process).
    static OVERRIDE: Cell<Option<Lang>> = const { Cell::new(None) };
}

/// The language to speak in.
pub fn lang() -> Lang {
    if let Some(l) = OVERRIDE.with(Cell::get) {
        return l;
    }
    // Tests never read the machine's settings: English unless a test pins a language.
    if cfg!(test) {
        return Lang::En;
    }
    *LANG.get_or_init(|| {
        let from_env = std::env::var("AGENTGW_LANG").ok();
        let value = from_env.or_else(|| {
            crate::bridge::state::StateDir::resolve()
                .load_env()
                .ok()?
                .into_iter()
                .find(|(k, _)| k == "AGENTGW_LANG")
                .map(|(_, v)| v)
        });
        Lang::parse(value.as_deref())
    })
}

/// Speak `l` on this thread until the guard is dropped (tests only).
pub fn pin(l: Lang) -> Pin {
    let before = OVERRIDE.with(|o| o.replace(Some(l)));
    Pin(before)
}

pub struct Pin(Option<Lang>);

impl Drop for Pin {
    fn drop(&mut self) {
        OVERRIDE.with(|o| o.set(self.0));
    }
}

/// `t!("English {x}", "日本語 {x}")` → a `String` in the current language.
///
/// Both strings are format strings; variables can be captured inline or passed after them,
/// the same arguments for both: `t!("{} files", "{} ファイル", n)`.
#[macro_export]
macro_rules! t {
    ($en:literal, $ja:literal $(, $arg:expr)* $(,)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::En => format!($en $(, $arg)*),
            $crate::i18n::Lang::Ja => format!($ja $(, $arg)*),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_unless_japanese_is_asked_for() {
        assert_eq!(Lang::parse(None), Lang::En);
        assert_eq!(Lang::parse(Some("")), Lang::En);
        assert_eq!(Lang::parse(Some("en")), Lang::En);
        assert_eq!(Lang::parse(Some("fr")), Lang::En);
        assert_eq!(Lang::parse(Some("ja")), Lang::Ja);
        assert_eq!(Lang::parse(Some(" JA ")), Lang::Ja);
        assert_eq!(Lang::parse(Some("ja_JP.UTF-8")), Lang::Ja);
    }

    #[test]
    fn t_speaks_the_pinned_language_with_inline_captures() {
        let name = "laptop";
        let n = 3;
        {
            let _p = pin(Lang::En);
            assert_eq!(t!("Linked {name} ({n})", "{name} をつなぎました({n})"), "Linked laptop (3)");
        }
        {
            let _p = pin(Lang::Ja);
            assert_eq!(t!("Linked {name} ({n})", "{name} をつなぎました({n})"), "laptop をつなぎました(3)");
            assert_eq!(t!("{} files", "{} ファイル", n), "3 ファイル");
        }
    }
}
