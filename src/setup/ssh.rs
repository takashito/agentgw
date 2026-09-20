//! A thin layer that only calls ssh / scp / gh. **No decisions here** (the pure functions in [`add_machine`](super::add_machine) own them).
//!
//! No ssh crate — we want `~/.ssh/config` aliases, keys, jump hosts and ProxyJump to work
//! as they are. Keys, ports and jump hosts are left to ssh itself.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// How to reach the machine: the key to offer, and whether ssh may ask for a password.
///
/// **A password is never ours to hold.** `-p` only takes `BatchMode` off so ssh asks on the terminal
/// itself; with the connection multiplexed, it asks once and every later ssh / scp rides that one.
/// Passing a password in argv would leave it in this machine's `ps` and shell history.
#[derive(Default, Clone)]
pub struct Access {
    pub identity: Option<String>,
    pub ask_password: bool,
}

static ACCESS: OnceLock<Access> = OnceLock::new();

/// Set once, before anything connects (`add-machine` does it from its arguments).
pub fn use_access(access: Access) {
    let _ = ACCESS.set(access);
}

/// Whether we were told to let ssh ask for a password.
pub fn asks_for_password() -> bool {
    ACCESS.get().is_some_and(|a| a.ask_password)
}

/// From now on, reach the machine with the gateway's key — the password session did its one job.
pub fn use_key_from_now_on() {
    let key = gateway_key().to_string_lossy().to_string();
    let _ = KEY_NOW.set(key);
}

static KEY_NOW: OnceLock<String> = OnceLock::new();

/// The options every ssh / scp call starts with.
fn opts() -> Vec<String> {
    opts_of(&ACCESS.get().cloned().unwrap_or_default())
}

fn opts_of(access: &Access) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let installed = KEY_NOW.get().cloned();
    if let Some(key) = installed.as_ref().or(access.identity.as_ref()) {
        out.push("-i".into());
        out.push(key.clone());
        // An explicit key is the one to use — don't let the agent's keys go first
        out.push("-o".into());
        out.push("IdentitiesOnly=yes".into());
    }
    if access.ask_password {
        // Ask once: the first connection prompts, the rest share it
        out.push("-o".into());
        out.push("ControlMaster=auto".into());
        out.push("-o".into());
        out.push(format!(
            "ControlPath={}/agentgw-%r@%h-%p",
            std::env::temp_dir().display()
        ));
        out.push("-o".into());
        out.push("ControlPersist=120".into());
    }
    out
}

/// `BatchMode=yes` means "never ask a human". With `-p` the human is right here, so let ssh ask.
fn batch_mode() -> [String; 2] {
    // Once the key is in place there is nothing left to ask
    batch_mode_of(KEY_NOW.get().is_none() && asks_for_password())
}

fn batch_mode_of(ask: bool) -> [String; 2] {
    ["-o".to_string(), format!("BatchMode={}", if ask { "no" } else { "yes" })]
}

/// Run one command on the remote and return its stdout. On failure, Err(its stderr).
pub fn ssh_capture(target: &str, command: &str) -> Result<String, String> {
    let out = Command::new("ssh")
        .args(opts())
        .args(batch_mode())
        .args(["-o", "ConnectTimeout=10", target, command])
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
        .args(opts())
        .args(batch_mode())
        .args([target, command])
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
        .args(opts())
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
        .args(opts())
        .args(batch_mode())
        .args([target, command])
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
        .args(opts())
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

/// The gateway's own key for reaching machines. Kept next to the user's keys, named so it is obvious
/// where it came from; the tunnel and every later `add-machine` use it.
pub fn gateway_key() -> std::path::PathBuf {
    std::path::PathBuf::from(crate::state_dir::StateDir::home()).join(".ssh/agentgw_ed25519")
}

/// Make the gateway's key if it isn't there yet, and return its **public** half.
/// No passphrase: the tunnel is reopened unattended, and a passphrase would need a human every time.
pub fn ensure_gateway_key() -> Result<String, String> {
    let key = gateway_key();
    let pubkey = key.with_extension("pub");
    if !key.exists() {
        if let Some(dir) = key.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let st = Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                "agentgw",
                "-f",
                &key.to_string_lossy(),
            ])
            .stdout(Stdio::null())
            .status()
            .map_err(|e| crate::t!("Couldn't run ssh-keygen: {e}", "ssh-keygen が実行できません: {e}"))?;
        if !st.success() {
            return Err(crate::t!("Couldn't make an ssh key", "ssh の鍵を作れませんでした"));
        }
    }
    let at = pubkey.display().to_string();
    std::fs::read_to_string(&pubkey)
        .map(|s| s.trim().to_string())
        .map_err(|e| crate::t!("Couldn't read {at}: {e}", "{at} を読めません: {e}"))
}

/// Put the gateway's public key in the machine's `authorized_keys` (once), over the connection that is
/// already open. **This is what lets the tunnel live on** — it is reopened unattended, so it can't ask
/// anyone for a password.
pub fn authorize_key(target: &str, pubkey: &str) -> Result<(), String> {
    let script = "mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && \
                  chmod 600 ~/.ssh/authorized_keys && key=$(cat) && \
                  if ! grep -qF \"$key\" ~/.ssh/authorized_keys; then echo \"$key\" >> ~/.ssh/authorized_keys; fi";
    ssh_stdin(target, script, pubkey).map(|_| ())
}

/// Fetch a URL to a local path with curl. **No credentials** — the release is public.
pub fn download(url: &str, out: &Path) -> Result<(), String> {
    let st = Command::new("curl")
        .args([
            "-fL",
            "--retry",
            "2",
            "-o",
            &out.to_string_lossy(),
            url,
        ])
        .status()
        .map_err(|e| crate::t!("Couldn't run curl: {e}", "curl が実行できません: {e}"))?;
    if !st.success() {
        let _ = std::fs::remove_file(out);
        return Err(crate::t!(
            "Couldn't download {url}. Check the release exists, or pass --from <binary>.",
            "{url} を取得できませんでした。release があるか確かめるか、--from <バイナリ> を指定してください。"
        ));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `-i` picks the key and says "this one" (the agent's keys would otherwise go first); `-p` lets ssh
    /// ask, and multiplexes so it asks once. **No password ever reaches argv.**
    #[test]
    fn access_shapes_the_ssh_options() {
        let args = |a: Access| {
            let joined = opts_of(&a).join(" ");
            joined
        };
        assert_eq!(args(Access::default()), "");
        let with_key = args(Access { identity: Some("/k/id".into()), ask_password: false });
        assert!(with_key.contains("-i /k/id") && with_key.contains("IdentitiesOnly=yes"), "{with_key}");
        let with_pw = args(Access { identity: None, ask_password: true });
        assert!(with_pw.contains("ControlMaster=auto") && with_pw.contains("ControlPersist=120"), "{with_pw}");
        assert!(!with_pw.contains("password"), "{with_pw}");
        assert_eq!(batch_mode_of(false)[1], "BatchMode=yes");
        assert_eq!(batch_mode_of(true)[1], "BatchMode=no");
    }
}
