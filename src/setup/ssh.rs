//! ssh / scp / gh を叩くだけの層。**判断はここに書かない**([`add_machine`](super::add_machine) の純関数が持つ)。
//!
//! ssh のクレートは足さない — `~/.ssh/config` の別名・鍵・踏み台・ProxyJump を
//! そのまま使いたい。鍵とポートと踏み台の設定は ssh 自身に任せる。

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// 相手で1つコマンドを回して標準出力を返す。失敗は Err(標準エラーの中身)。
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

/// 相手で回して、成否だけ見る(出力は相手の端末にそのまま流す)。
pub fn ssh_run(target: &str, command: &str) -> Result<(), String> {
    let st = Command::new("ssh")
        .args(["-o", "BatchMode=yes", target, command])
        .status()
        .map_err(|e| crate::t!("Couldn't run ssh: {e}", "ssh が実行できません: {e}"))?;
    st.success()
        .then_some(())
        .ok_or_else(|| crate::t!("Failed on {target}: {command}", "{target} で失敗しました: {command}"))
}

/// 相手で対話つきに回す。
///
/// **`-t` で端末を割り当てる。** `install` はトークンと役割を対話で訊くので、端末が
/// 無いと「端末ではないので役割を訊けません」で止まる。**出力をパイプに落とさない** —
/// 割り当てた端末を潰すと入力できなくなる。
pub fn ssh_interactive(target: &str, command: &str) -> Result<(), String> {
    let st = Command::new("ssh")
        .args(["-t", target, command])
        .status()
        .map_err(|e| crate::t!("Couldn't run ssh: {e}", "ssh が実行できません: {e}"))?;
    st.success()
        .then_some(())
        .ok_or_else(|| crate::t!("Failed on {target}: {command}", "{target} で失敗しました: {command}"))
}

/// 標準入力から食わせる。
///
/// **接続文字列を argv に置かない** — 相手の `ps` とシェル履歴に残る。
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

/// 手元のファイルを相手に置く。
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

/// 手元の文字列を相手のファイルに書く(install.sh を送るのに使う)。
pub fn put_text(target: &str, remote_path: &str, text: &str) -> Result<(), String> {
    // `cat > path` に標準入力で流す。scp のために一時ファイルを作らない
    ssh_stdin(target, &format!("cat > '{remote_path}'"), text).map(|_| ())
}

/// `gh` が使えるか(入っていて、認証が通っているか)。
pub fn gh_ready() -> bool {
    Command::new("gh")
        .args(["auth", "status"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// GitHub release から1つ落とす。落ちた先のパスを返す。
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

/// `tailscale status --json`。入っていない / 落ちていれば `None`。
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
