//! The state directory and the files in it (`.env`, JSON written atomically).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};


/// An empty JSON object. Needed again and again as the default for `read_json_or`.
pub(crate) fn json_obj() -> serde_json::Value {
    serde_json::json!({})
}

/// The state directory. **Never touch `~/.local/state` without going through this.**
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateDir(PathBuf);

impl StateDir {
    /// `$AGENTGW_STATE_DIR` → `$XDG_STATE_HOME/agentgw` → `~/.local/state/agentgw`
    ///
    /// Last line of defense for isolation — **in test builds the fallback never points at production**.
    /// set_var/remove_var act on the whole process (and tests run in parallel), so a single
    /// test that clears the env would make other tests write into the production state dir.
    pub fn resolve() -> Self {
        if let Ok(dir) = std::env::var("AGENTGW_STATE_DIR") {
            return StateDir(PathBuf::from(dir));
        }
        #[cfg(test)]
        return StateDir(std::env::temp_dir().join(format!("agentgw-test-{}", std::process::id())));
        #[cfg(not(test))]
        StateDir(Self::default_base().join("agentgw"))
    }

    /// Parent of the state directory (`$XDG_STATE_HOME` → `~/.local/state`). The rename migration looks here too.
    pub fn default_base() -> PathBuf {
        std::env::var("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_default())
                    .join(".local")
                    .join("state")
            })
    }

    /// From an explicit path (tests, and when the CLI points somewhere else).
    pub fn at(path: impl Into<PathBuf>) -> Self {
        StateDir(path.into())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// The home directory. Default place where agents for channels with no route start.
    pub fn home() -> String {
        std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
    }

    pub fn read_json_or(&self, name: &str, default: serde_json::Value) -> serde_json::Value {
        read_json_at(&self.join(name), default)
    }

    pub fn write_json_atomic(&self, name: &str, value: &serde_json::Value) -> std::io::Result<()> {
        write_json_at(&self.join(name), value)
    }

    pub fn write_atomic(&self, name: &str, text: &str) -> std::io::Result<()> {
        write_atomic_at(&self.join(name), text)
    }

    /// Replaces **just one key** of the file. Other keys stay as they are on disk.
    ///
    /// access.json has several owners (settings come from the fleet side and Slack commands; `endpoints`
    /// and `pools` belong to the Bridge). Overwriting the whole file drops whatever we changed between
    /// the other side's read and its write. **Writing per key narrows that window to one key** — two
    /// writers touching the *same* key can still lose one, since this is read-modify-write with no lock.
    pub fn patch_json(
        &self,
        name: &str,
        key: &str,
        value: serde_json::Value,
    ) -> std::io::Result<()> {
        let mut root = self.read_json_or(name, serde_json::json!({}));
        if !root.is_object() {
            root = serde_json::json!({});
        }
        root[key] = value;
        self.write_json_atomic(name, &root)
    }

    /// Where the **generated files** passed to claude live (`--settings` / `--mcp-config`).
    ///
    /// **They are not state, so they don't go in the state directory.** They are rewritten on every start,
    /// and recreated on the next start if gone. claude **only reads them at startup** (measured: they don't
    /// show up in lsof of a running claude), so they go in the OS temp area, which also handles cleanup.
    /// While they lived in state, per-session MCP configs piled up undeleted (101 of them, measured 2026-08-02).
    ///
    /// The state directory's name is appended to keep dev and production apart.
    pub fn runtime_dir(&self) -> PathBuf {
        let tag = self.0.file_name().unwrap_or_default().to_string_lossy();
        std::env::temp_dir().join(format!("agentgw-{tag}"))
    }

    /// Writes a generated file and returns the path to pass to claude. Creates the parent directory.
    ///
    /// **Owner-only, directory included.** These files carry the hook and MCP tokens, and they live in the
    /// shared OS temp area — at the default mode every local user could read them and then post to Slack or
    /// forge a permission answer (measured on a real machine: `644` on `worker-hooks.json` and `mcp/<sid>.json`).
    pub fn write_runtime_json(
        &self,
        name: &str,
        value: &serde_json::Value,
    ) -> std::io::Result<PathBuf> {
        let path = self.runtime_dir().join(name);
        private_dir(&self.runtime_dir())?;
        if let Some(parent) = path.parent() {
            private_dir(parent)?;
        }
        write_atomic_mode(&path, &serde_json::to_string_pretty(value)?, Some(0o600))?;
        Ok(path)
    }

    /// Where the restart marker lives. A restart spans two Bridge processes, so the requesting thread
    /// and the progress message's ts are left here for the successor.
    /// **The successor deletes it after reading** — left behind, the next start posts a bogus "✅ restart complete".
    pub fn restart_marker(&self) -> PathBuf {
        self.join("restart-marker.json")
    }

    pub fn load_env(&self) -> std::io::Result<Vec<(String, String)>> {
        Ok(parse_env(&std::fs::read_to_string(self.join(".env"))?))
    }

    /// The port is remembered and reused — agents bake the URL in and outlive a Bridge restart.
    pub fn remembered_port(&self, which: &str, allocate: impl FnOnce() -> u16) -> u16 {
        if let Some(port) = self.endpoint(which)["port"].as_u64() {
            return port as u16;
        }
        let port = allocate();
        let _ = self.put_endpoint(which, "port", serde_json::json!(port));
        port
    }

    /// One of `hook` / `mcp` under `"endpoints"` in access.json.
    ///
    /// **Moved here from hook-endpoint.json / mcp-endpoint.json on 2026-08-02.**
    /// Only the Bridge reads it (agents get the baked-in value).
    fn endpoint(&self, which: &str) -> serde_json::Value {
        self.read_json_or("access.json", json_obj())["endpoints"][which].clone()
    }

    /// Sets a single field of one endpoint. Leaves everything outside `endpoints` (the settings) alone.
    fn put_endpoint(
        &self,
        which: &str,
        field: &str,
        value: serde_json::Value,
    ) -> std::io::Result<()> {
        let mut endpoints = self.read_json_or("access.json", json_obj())["endpoints"].clone();
        if !endpoints.is_object() {
            endpoints = json_obj();
        }
        endpoints[which][field] = value;
        self.patch_json("access.json", "endpoints", endpoints)
    }

    /// The secret shared with agents. Remembered and reused (agents bake it in and outlive a Bridge restart).
    /// Built the same way for hook and MCP — only which entry in `endpoints` differs.
    pub fn remembered_token(&self, which: &str) -> String {
        if let Some(t) = self.endpoint(which)["token"].as_str() {
            return t.to_string();
        }
        let token = mint_secret();
        let _ = self.put_endpoint(which, "token", serde_json::json!(token));
        token
    }

    /// Lets the OS pick a free port.
    pub fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map(|a| a.port())
            .unwrap_or(0)
    }
}

// ─── reading and writing disk ─────────────────────────────────────────────────────
// `StateDir`'s methods are the only entry point. What follows are helpers called only through
// `impl StateDir`, used directly only where files outside the state directory are touched
// (transcripts / plists given by absolute path).

fn read_json_at(path: &Path, default: serde_json::Value) -> serde_json::Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(default)
}

/// Mint one secret: 32 bytes of `/dev/urandom` as hex.
///
/// **Read exactly 32 bytes.** `/dev/urandom` never returns EOF, so `fs::read` reads forever (confirmed on
/// a real machine on 2026-08-01, where it hung the connection string).
///
/// This is the one generator for every secret the Bridge makes. The hook and MCP tokens used to be
/// `pid` + nanoseconds, which is guessable from a rough start time — and they are the only thing between a
/// local process and posting to Slack or forging a permission answer.
pub(crate) fn mint_secret() -> String {
    let mut bytes = [0u8; 32];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut bytes);
    }
    // Even if it somehow can't be read, never use all zeros as the secret
    let pid = std::process::id().to_be_bytes();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
        .to_be_bytes();
    for (i, b) in pid.iter().chain(now.iter()).enumerate() {
        bytes[i] ^= b;
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Create a directory only its owner may enter. Applied to what is already there too: the runtime
/// directory outlives one start, so a directory made before this rule existed stays open otherwise.
fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_json_at(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    write_atomic_at(path, &serde_json::to_string_pretty(value)?)
}

/// Write to tmp → rename. Never let anyone read half-written JSON.
pub(crate) fn write_atomic_at(path: &Path, text: &str) -> std::io::Result<()> {
    write_atomic_mode(path, text, None)
}

/// The same tmp→rename, but with **the mode set from the moment of creation**.
/// Files holding tokens (Relay's state.json) are created 0600 — chmod afterwards would leave
/// a moment in which others can read it.
pub(crate) fn write_atomic_mode(path: &Path, text: &str, mode: Option<u32>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // **Unique per write, not per process.** The gateway and its own Bridge both write access.json, and
    // sharing one tmp name made the second rename fail with "No such file or directory"
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{}-{seq}", std::process::id()));
    // **Flush the contents before the rename.** The rename is atomic, but a power loss can land it while
    // the bytes are still in the page cache, leaving a file that is there and empty — the worst of both
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if let Some(m) = mode {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(m);
    }
    #[cfg(not(unix))]
    let _ = mode;
    {
        use std::io::Write;
        let mut f = opts.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // And the directory entry itself, so the rename survives too
    if let Some(parent) = path.parent() {
        let _ = std::fs::File::open(parent).and_then(|d| d.sync_all());
    }
    Ok(())
}

/// Sweep the temp files a crash left behind. **Only ours, only old ones**: the name carries the pid and a
/// counter, and a write in flight right now must not be touched.
pub(crate) fn sweep_write_leftovers(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let ours = name.rsplit_once(".tmp").is_some_and(|(_, tail)| {
            tail.split_once('-')
                .is_some_and(|(pid, seq)| !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()) && seq.chars().all(|c| c.is_ascii_digit()))
        });
        if !ours {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < hour_ago);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn parse_env(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            let v = v.trim().trim_matches('"').trim_matches('\'');
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

/// Writes a `.env` key **by replacing** (appends it if missing). No other line is touched.
///
/// If appended instead, `load_env` reads last-wins, so a value you thought you removed keeps living.
pub fn set_env_keys(env_text: &str, pairs: &[(&str, String)]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut seen = vec![false; pairs.len()];
    for line in env_text.lines() {
        let key = line.split_once('=').map(|(k, _)| k.trim());
        match pairs.iter().position(|(k, _)| Some(*k) == key) {
            Some(i) => {
                seen[i] = true;
                out.push(format!("{}={}", pairs[i].0, pairs[i].1));
            }
            None => out.push(line.to_string()),
        }
    }
    for (i, (k, v)) in pairs.iter().enumerate() {
        if !seen[i] {
            out.push(format!("{k}={v}"));
        }
    }
    let mut text = out.join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_is_remembered_across_calls() {
        let dir = std::env::temp_dir().join(format!("sc-bridge-port-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = StateDir::at(dir);
        let first = dir.remembered_port("hook", || 8791);
        assert_eq!(first, 8791);
        // The second call doesn't allocate a number (panics if called)
        let second = dir.remembered_port("hook", || panic!("must not re-allocate"));
        assert_eq!(
            second, 8791,
            "worker bakes the URL in — the port must not move"
        );
    }

    #[test]
    fn parses_kv_ignores_comments_blanks_and_strips_quotes() {
        let text = "\
# comment
SLACK_APP_TOKEN=xapp-1-abc

SLACK_BOT_TOKEN=\"xoxb-def\"
EMPTY=
NOEQ_LINE
SPACES = padded ";
        let kv = parse_env(text);
        assert_eq!(
            kv,
            vec![
                ("SLACK_APP_TOKEN".to_string(), "xapp-1-abc".to_string()),
                ("SLACK_BOT_TOKEN".to_string(), "xoxb-def".to_string()),
                ("EMPTY".to_string(), "".to_string()),
                ("SPACES".to_string(), "padded".to_string()),
            ]
        );
    }

    #[test]
    fn state_dir_honors_override() {
        unsafe { std::env::set_var("AGENTGW_STATE_DIR", "/tmp/scdev-test") };
        assert_eq!(StateDir::resolve().path(), PathBuf::from("/tmp/scdev-test"));
        unsafe { std::env::remove_var("AGENTGW_STATE_DIR") };
    }

    /// Generated files for claude land **outside the state directory**. If they move back into state,
    /// per-session MCP configs pile up with nobody deleting them (101 measured).
    #[test]
    fn generated_files_land_outside_the_state_dir() {
        let dir = StateDir::at(std::env::temp_dir().join("scrs-runtime-test"));
        let path = dir
            .write_runtime_json("mcp/sid-1.json", &serde_json::json!({"k": 1}))
            .unwrap();

        assert!(
            !path.starts_with(dir.path()),
            "生成物が state に落ちている: {}",
            path.display()
        );
        assert!(path.ends_with("mcp/sid-1.json"));
        assert!(
            path.to_string_lossy().contains("scrs-runtime-test"),
            "dev と本番を分ける印が付いていない: {}",
            path.display()
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            "{\n  \"k\": 1\n}"
        );
        // These carry the hook and MCP tokens and sit in the shared temp area: owner only, directory included
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &std::path::Path| {
                std::fs::metadata(p).unwrap().permissions().mode() & 0o777
            };
            assert_eq!(mode(&path), 0o600, "{}", path.display());
            assert_eq!(mode(path.parent().unwrap()), 0o700);
            assert_eq!(mode(&dir.runtime_dir()), 0o700);
        }
        let _ = std::fs::remove_file(&path);
    }

    /// The hook and MCP tokens guard posting to Slack and answering permission prompts. They used to be
    /// `pid` + nanoseconds — two starts a moment apart produced neighbouring values.
    /// A crash leaves the temp file of a write that never finished. Nothing swept them before.
    /// **Only ours and only old ones** — a write happening right now must survive the sweep.
    #[test]
    fn leftover_temp_files_are_swept_but_only_the_old_ones() {
        let dir = std::env::temp_dir().join(format!("agentgw-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("access.tmp1234-7");
        let fresh = dir.join("threads.tmp999-1");
        let theirs = dir.join("something.tmp");
        for f in [&old, &fresh, &theirs] {
            std::fs::write(f, "x").unwrap();
        }
        // Age only the first one
        let two_hours_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
        filetime_set(&old, two_hours_ago);

        sweep_write_leftovers(&dir);

        assert!(!old.exists(), "an hour-old leftover goes");
        assert!(fresh.exists(), "a write in flight stays");
        assert!(theirs.exists(), "a name that is not ours is not ours to delete");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `filetime` is not a dependency, so set it the way the OS lets us.
    fn filetime_set(path: &Path, when: std::time::SystemTime) {
        let secs = when
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let stamp = chrono::DateTime::from_timestamp(secs as i64, 0)
            .unwrap()
            .format("%Y%m%d%H%M.%S")
            .to_string();
        let _ = std::process::Command::new("touch")
            .args(["-t", &stamp])
            .arg(path)
            .status();
    }

    #[test]
    fn a_minted_secret_is_random_and_full_length() {
        let a = mint_secret();
        let b = mint_secret();
        assert_eq!(a.len(), 64, "{a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, b, "two secrets in a row must not match");
        assert_ne!(a, "0".repeat(64), "never all zeros");
    }

    #[test]
    fn json_atomic_roundtrip() {
        let dir = StateDir::at(std::env::temp_dir().join(format!("scjson-{}", std::process::id())));
        std::fs::create_dir_all(dir.path()).unwrap();
        dir.write_json_atomic("x.json", &serde_json::json!({"a": 1}))
            .unwrap();
        let v = dir.read_json_or("x.json", serde_json::json!({}));
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn read_json_or_returns_default_when_missing() {
        let dir = StateDir::at(std::env::temp_dir());
        let _ = std::fs::remove_file(dir.join("scjson-does-not-exist-9e3.json"));
        let v = dir.read_json_or(
            "scjson-does-not-exist-9e3.json",
            serde_json::json!({"d": true}),
        );
        assert_eq!(v["d"], true);
    }
}
