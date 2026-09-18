//! A thin layer that only calls ssh / scp / gh. **No decisions here** (the pure functions in [`add_machine`](super::add_machine) own them).
//!
//! No ssh crate — we want `~/.ssh/config` aliases, keys, jump hosts and ProxyJump to work
//! as they are. Keys, ports and jump hosts are left to ssh itself.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Run one command on the remote and return its stdout. On failure, Err(its stderr).
pub fn ssh_capture(target: &str, command: &str) -> Result<String, String> {
    let out = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            target,
            command,
        ])
        .output()
        .map_err(|e| crate::t!("Couldn't run ssh: {e}", "ssh が実行できません: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run on the remote and check only success (output goes straight to the remote's terminal).
pub fn ssh_run(target: &str, command: &str) -> Result<(), String> {
    let st = Command::new("ssh")
        .args(["-o", "BatchMode=yes", target, command])
        .status()
        .map_err(|e| crate::t!("Couldn't run ssh: {e}", "ssh が実行できません: {e}"))?;
    st.success()
        .then_some(())
        .ok_or_else(|| crate::t!("Failed on {target}: {command}", "{target} で失敗しました: {command}"))
}

/// Run on the remote interactively.
///
/// **`-t` allocates a terminal.** `install` asks for the token and the role interactively, so
/// without a terminal it stops with "not a terminal, can't ask for the role". **Don't pipe the
/// output** — taking over the allocated terminal makes input impossible.
pub fn ssh_interactive(target: &str, command: &str) -> Result<(), String> {
    let st = Command::new("ssh")
        .args(["-t", target, command])
        .status()
        .map_err(|e| crate::t!("Couldn't run ssh: {e}", "ssh が実行できません: {e}"))?;
    st.success()
        .then_some(())
        .ok_or_else(|| crate::t!("Failed on {target}: {command}", "{target} で失敗しました: {command}"))
}

/// Feed it through stdin.
///
/// **Never put the connection string in argv** — it would stay in the remote's `ps` and shell history.
pub fn ssh_stdin(target: &str, command: &str, stdin: &str) -> Result<String, String> {
    let mut child = Command::new("ssh")
        .args(["-o", "BatchMode=yes", target, command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| crate::t!("Couldn't run ssh: {e}", "ssh が実行できません: {e}"))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| crate::t!("Couldn't open ssh's stdin", "ssh の標準入力が開けません"))?
        .write_all(stdin.as_bytes())
        .map_err(|e| crate::t!("Couldn't write to ssh: {e}", "ssh に渡せません: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| crate::t!("Couldn't wait for ssh: {e}", "ssh の終了を待てません: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Put a local file on the remote.
pub fn scp(local: &Path, target: &str, remote_path: &str) -> Result<(), String> {
    let st = Command::new("scp")
        .args([
            "-q",
            &local.to_string_lossy(),
            &format!("{target}:{remote_path}"),
        ])
        .status()
        .map_err(|e| crate::t!("Couldn't run scp: {e}", "scp が実行できません: {e}"))?;
    st.success()
        .then_some(())
        .ok_or_else(|| crate::t!("Couldn't copy to {remote_path}", "{remote_path} に置けませんでした"))
}

/// Write a local string to a file on the remote (used to send install.sh).
pub fn put_text(target: &str, remote_path: &str, text: &str) -> Result<(), String> {
    // Stream it into `cat > path` over stdin, so scp doesn't need a temp file
    ssh_stdin(target, &format!("cat > '{remote_path}'"), text).map(|_| ())
}

/// Whether `gh` is usable (installed and authenticated).
pub fn gh_ready() -> bool {
    Command::new("gh")
        .args(["auth", "status"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Download one asset from a GitHub release. Returns the path it landed at.
pub fn gh_download(
    args: &[String],
    out_dir: &Path,
    triple: &str,
) -> Result<std::path::PathBuf, String> {
    let out = Command::new("gh")
        .args(args)
        .output()
        .map_err(|e| crate::t!("Couldn't run gh: {e}", "gh が実行できません: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let path = out_dir.join(format!("agentgw-{triple}"));
    path.exists()
        .then_some(path)
        .ok_or_else(|| crate::t!("The release has no agentgw-{triple}", "release に agentgw-{triple} がありません"))
}

/// `tailscale status --json`. `None` if it's not installed or not running.
pub fn tailscale_json() -> Option<String> {
    for bin in [
        "tailscale",
        "/usr/local/bin/tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ] {
        if let Ok(out) = Command::new(bin).args(["status", "--json"]).output()
            && out.status.success()
        {
            return Some(String::from_utf8_lossy(&out.stdout).to_string());
        }
    }
    None
}
