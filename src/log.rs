//! Log lines and where they go.

use crate::chat::ThreadKey;
use crate::clock::{iso8601, now_ms};
use crate::state_dir::StateDir;

/// Where one log line goes.
///
/// This alone decides **which file gets written** (session log / per-thread log /
/// plugin-debug.log). If both are None, only the global log.
#[derive(Default, Clone)]
pub struct LogCtx {
    pub session_id: Option<String>,
    pub thread_key: Option<ThreadKey>,
}

/// 16KB cap per record.
const MAX_RECORD: usize = 16 * 1024;

/// A log file is rolled once it passes this. One generation is kept (`<name>.1`), so a file's worth of
/// history stays reachable while the pair stays bounded.
const MAX_LOG_BYTES: u64 = 32 * 1024 * 1024;

/// Move the file aside, dropping whatever was kept before it. **Best effort**: logging must never break
/// the caller, so a failure here just means the file keeps growing until the next line.
fn roll(path: &std::path::Path) {
    let kept = path.with_extension(match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.1"),
        None => "1".to_string(),
    });
    let _ = std::fs::remove_file(&kept);
    let _ = std::fs::rename(path, &kept);
}

impl LogCtx {
    /// A log line: `<ISO8601> <level> <component> pid=<pid> session=<sid|-> <message>`
    /// Must not change by a single character (the e2e measurement skill parses it).
    ///
    /// It is `pub(crate)` because Relay writes **the same line to another destination (stdout)**. With the
    /// format in two places, the day only one gets fixed is the day the e2e skill can't read the other.
    pub(crate) fn line(&self, level: &str, component: &str, message: &str) -> String {
        let mut msg = message.replace("\r\n", "\\n").replace('\n', "\\n");
        if msg.len() > MAX_RECORD {
            let extra = msg.len() - MAX_RECORD;
            msg.truncate(MAX_RECORD);
            msg.push_str(&format!(" …[+{extra} chars truncated]"));
        }
        let sid = self.session_id.as_deref().unwrap_or("-");
        format!(
            "{} {level} {component} pid={} session={sid} {msg}\n",
            iso8601(now_ms()),
            std::process::id()
        )
    }

    fn write(&self, level: &str, component: &str, message: &str) {
        // **A test must not write into the live logs.** The state directory is resolved from the
        // environment, so a test that logs anything lands in the production `plugin-debug.log` and
        // `logs/by-thread/` unless it was told where to go (lines from the suite were found there:
        // `C1-1-0`, and a delivery watchdog firing on a fake clock)
        #[cfg(test)]
        if std::env::var_os("AGENTGW_STATE_DIR").is_none() {
            return;
        }
        // Logging must never break the hot path — failures are swallowed
        let path = self.path(&StateDir::resolve());
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = f.write_all(self.line(level, component, message).as_bytes());
            // **Nothing else ever trims these.** They hold message bodies verbatim and only grow;
            // measured at 9.9 MB for one file and 31 MB of by-thread logs on a machine in daily use
            if f.metadata().is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
                roll(&path);
            }
        }
    }

    /// Where logs go. thread_key set → by-thread, only session_id → sessions,
    /// neither → plugin-debug.log
    fn path(&self, dir: &StateDir) -> std::path::PathBuf {
        match (&self.thread_key, &self.session_id) {
            (Some(key), _) => dir
                .join("logs")
                .join("by-thread")
                .join(self.sanitized_key().unwrap_or_else(|| key.to_string()))
                .join("bridge.log"),
            (None, Some(sid)) => dir
                .join("logs")
                .join("sessions")
                .join(format!("{sid}.log")),
            (None, None) => dir.join("plugin-debug.log"),
        }
    }

    pub fn info(&self, component: &str, message: &str) {
        self.write("info", component, message)
    }

    pub fn debug(&self, component: &str, message: &str) {
        self.write("debug", component, message)
    }

    pub fn error(&self, component: &str, message: &str) {
        self.write("error", component, message)
    }

    /// Makes a threadKey (`channel:thread_ts`) safe for a file name.
    /// **Only for log directory names** (`logs/by-thread/<key>/`) — never used for tmux window names.
    pub(crate) fn sanitized_key(&self) -> Option<String> {
        self.thread_key
            .as_ref()
            .map(|k| k.as_str().replace([':', '.'], "-"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_line_format_is_scp44_compatible() {
        let line = LogCtx {
            session_id: Some("abc".into()),
            thread_key: None,
        }
        .line("info", "bridge", "hello world");
        // e.g. 2026-07-27T12:00:00.000Z info bridge pid=123 session=abc hello world
        let parts: Vec<&str> = line.trim_end().splitn(6, ' ').collect();
        assert_eq!(parts[1], "info");
        assert_eq!(parts[2], "bridge");
        assert!(parts[3].starts_with("pid="));
        assert_eq!(parts[4], "session=abc");
        assert_eq!(parts[5], "hello world");
    }

    #[test]
    fn log_line_timestamp_is_iso8601_millis_z() {
        let line = LogCtx {
            session_id: None,
            thread_key: None,
        }
        .line("info", "bridge", "x");
        let ts = line.split(' ').next().unwrap();
        // 2026-07-27T12:00:00.000Z
        assert_eq!(ts.len(), 24, "{ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[19..20], ".");
        assert!(ts.ends_with('Z'), "{ts}");
        assert!(line.contains("session=-"), "{line}");
    }

    #[test]
    fn log_message_newlines_are_escaped() {
        let line = LogCtx {
            session_id: None,
            thread_key: None,
        }
        .line("info", "bridge", "a\nb");
        assert!(line.contains("a\\nb"), "{line}");
        assert_eq!(
            line.matches('\n').count(),
            1,
            "record must be one line: {line}"
        );
    }

    #[test]
    fn sanitize_thread_key_replaces_colon_and_dot() {
        let ctx = LogCtx {
            session_id: None,
            thread_key: Some(ThreadKey::parse("C0AAA:123.456")),
        };
        assert_eq!(ctx.sanitized_key().unwrap(), "C0AAA-123-456");
    }

}
