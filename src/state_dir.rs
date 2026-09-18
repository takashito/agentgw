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
    /// the other side's read and its write. **Always writing per key** removes that window by construction.
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
    pub fn write_runtime_json(
        &self,
        name: &str,
        value: &serde_json::Value,
    ) -> std::io::Result<PathBuf> {
        let path = self.runtime_dir().join(name);
        write_json_at(&path, value)?;
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
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let token = format!("{:x}{nanos:x}", std::process::id());
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
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    match mode {
        None => std::fs::write(&tmp, text)?,
        Some(m) => {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(m);
            }
            #[cfg(not(unix))]
            let _ = m;
            use std::io::Write;
            opts.open(&tmp)?.write_all(text.as_bytes())?;
        }
    }
    std::fs::rename(&tmp, path)
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
        let _ = std::fs::remove_file(&path);
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
