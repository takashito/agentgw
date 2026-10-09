//! Which build this is: the version and the commit it was cut from (`0.65.0+e1a48c9`, with `.dirty`
//! when the tree had uncommitted changes).
//!
//! **`--version` does not show this** — every machine running an older agentgw checks a download's
//! `--version` for exactly `agentgw <version>`, and would refuse the next release. `--build` does.

/// The label, with a marker in front so it can be found in the file of a build that can't be run
/// here (a Mac build on Linux). One literal: [`LABEL`] is a slice of it.
static MARKED: &str = concat!(
    "agentgw-build:",
    env!("CARGO_PKG_VERSION"),
    "+",
    env!("AGENTGW_COMMIT"),
    "\0"
);

/// Length of the `agentgw-build:` marker in front of the label.
const MARKER_LEN: usize = 14;

/// This build's label (`0.65.0+e1a48c9`).
pub static LABEL: &str = {
    let (_, rest) = MARKED.as_bytes().split_at(MARKER_LEN);
    let (label, _) = rest.split_at(rest.len() - 1);
    match std::str::from_utf8(label) {
        Ok(s) => s,
        Err(_) => panic!("the build label is not UTF-8"),
    }
};

/// `0.65.0+e1a48c9` → `0.65.0`.
pub fn version_of_label(label: &str) -> &str {
    label.split_once('+').map_or(label, |(v, _)| v)
}

/// Whether `s` looks like a label: `<n>.<n>.<n>+<commit>[.dirty]`.
fn is_label(s: &str) -> bool {
    let Some((v, commit)) = s.split_once('+') else {
        return false;
    };
    let nums: Vec<&str> = v.split('.').collect();
    nums.len() == 3
        && nums
            .iter()
            .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        && !commit.is_empty()
        && commit.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
}

/// The label baked into the agentgw at `path`, read from its bytes (it need not run here).
pub fn read_label(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    // Built at run time so this needle is not itself a marker in the binary
    let needle = ["agentgw", "-build:"].concat();
    let needle = needle.as_bytes();
    let mut at = 0;
    while let Some(i) = bytes[at..].windows(needle.len()).position(|w| w == needle) {
        let start = at + i + needle.len();
        let end = start + bytes[start..].iter().position(|&b| b == 0)?;
        if let Ok(s) = std::str::from_utf8(&bytes[start..end])
            && is_label(s)
        {
            return Some(s.to_string());
        }
        at = start;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("agentgw-label-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_label_is_the_version_and_a_commit() {
        let (v, commit) = LABEL.split_once('+').expect("a label has a +");
        assert_eq!(v, env!("CARGO_PKG_VERSION"));
        assert!(!commit.is_empty());
        assert!(is_label(LABEL), "{LABEL}");
    }

    #[test]
    fn a_label_is_read_out_of_any_bytes() {
        let dir = scratch("read");
        let f = dir.join("bin");
        let mut bytes = b"\x7fELF junk ".to_vec();
        bytes.extend_from_slice(b"agentgw-build:0.65.0+e1a48c9.dirty\0more junk");
        std::fs::write(&f, &bytes).unwrap();
        assert_eq!(read_label(&f).as_deref(), Some("0.65.0+e1a48c9.dirty"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The reader's own needle sits in every binary too. It must not be taken for a label
    #[test]
    fn the_needle_itself_is_not_a_label() {
        let dir = scratch("needle");
        let f = dir.join("bin");
        std::fs::write(&f, b"agentgw-build:\0agentgw-build:not a label\0").unwrap();
        assert_eq!(read_label(&f), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_version_is_what_comes_before_the_plus() {
        assert_eq!(version_of_label("0.65.0+e1a48c9"), "0.65.0");
        assert_eq!(version_of_label("0.65.0"), "0.65.0");
    }
}
