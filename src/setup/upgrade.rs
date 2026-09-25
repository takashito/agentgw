//! `upgrade`: fetch a release of agentgw and put it where the running binary is.
//!
//! **The gateway and every machine use the same steps**, so one path is tested and one path runs:
//! download next to the running binary → compare with the release's `.sha256` → (macOS) sign →
//! ask the new file its `--version` → keep the old one as `agentgw.prev` → rename into place.
//! **Until the rename, the running binary is not touched** — any failure before it leaves the
//! machine exactly as it was.
//!
//! Replacing the file does not restart anything. The caller asks for the same graceful restart
//! `restart` uses (agents keep running), and the service manager starts the new file.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// How long a machine may take to come back on the new version before the rollout moves on.
pub const COME_BACK_MS: u64 = 5 * 60 * 1000;

/// `0.56.0` and `v0.56.0` both name the release `v0.56.0`.
pub fn tag_of(version: &str) -> String {
    let v = version.trim();
    match v.strip_prefix('v') {
        Some(_) => v.to_string(),
        None => format!("v{v}"),
    }
}

/// `v0.56.0` → `0.56.0` (what `--version` and the link report).
pub fn version_of(tag: &str) -> &str {
    let t = tag.trim();
    t.strip_prefix('v').unwrap_or(t)
}

/// Where `releases/latest` redirects to names the latest tag: `…/releases/tag/v0.56.0`.
pub fn tag_from_redirect(location: &str) -> Option<String> {
    let (_, tag) = location.trim().rsplit_once("/releases/tag/")?;
    let tag = tag.trim_end_matches('/');
    (!tag.is_empty() && !tag.contains('/')).then(|| tag.to_string())
}

fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = version_of(v).split('.').map(|p| p.parse::<u64>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

/// `a` is an older version than `b`, compared as numbers (`0.9.0` < `0.10.0`). Anything that isn't
/// three numbers is never older — an unreadable version is no reason to replace a binary.
pub fn older(a: &str, b: &str) -> bool {
    matches!((parse(a), parse(b)), (Some(x), Some(y)) if x < y)
}

/// The release asset this binary was built as. Same names as `add_machine::triple_for`, read off
/// what the compiler knew instead of `uname`.
pub fn this_triple() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        _ => return None,
    })
}

/// The binary and its checksum, as `release.sh` uploads them.
pub fn asset_urls(tag: &str, triple: &str) -> (String, String) {
    let bin = crate::setup::add_machine::release_url(tag, triple);
    let sum = format!("{bin}.sha256");
    (bin, sum)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// What a `.sha256` file says: the first word, if it is 64 hex digits (`sha256sum` adds a name after it).
pub fn expected_sum(text: &str) -> Option<String> {
    let word = text.split_whitespace().next()?.to_ascii_lowercase();
    (word.len() == 64 && word.bytes().all(|b| b.is_ascii_hexdigit())).then_some(word)
}

/// The file the service runs. **Not `~/.local/bin/agentgw` by assumption** — it is wherever this
/// process was started from. A cargo build is refused: replacing `target/debug/agentgw` with a
/// release would only confuse the next build.
pub fn running_binary() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot tell where agentgw runs from: {e}"))?;
    // Linux names a replaced file "… (deleted)"; the path we want is the one before it
    let exe = PathBuf::from(exe.to_string_lossy().trim_end_matches(" (deleted)").to_string());
    if exe.components().any(|c| c.as_os_str() == "target") {
        let path = exe.display();
        return Err(crate::t!(
            "this agentgw runs from a build folder ({path}); upgrade only replaces an installed one",
            "この agentgw はビルドの置き場から動いています({path})。upgrade が入れ替えるのは入れた物だけです"
        ));
    }
    Ok(exe)
}

/// Check `new` and put it at `target`. **Everything before the rename can fail without touching
/// `target`.** `want` is the version (`0.56.0`) the new file must say it is.
pub fn put_in_place(new: &Path, sum_text: &str, target: &Path, want: &str) -> Result<(), String> {
    let result = check_and_rename(new, sum_text, target, want);
    if result.is_err() {
        let _ = std::fs::remove_file(new);
    }
    result
}

fn check_and_rename(new: &Path, sum_text: &str, target: &Path, want: &str) -> Result<(), String> {
    let expected = expected_sum(sum_text)
        .ok_or_else(|| "the release's .sha256 does not hold a checksum".to_string())?;
    let bytes = std::fs::read(new).map_err(|e| format!("cannot read the download: {e}"))?;
    let got = sha256_hex(&bytes);
    if got != expected {
        return Err(format!("checksum mismatch (expected {expected}, got {got})"));
    }
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(new, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("cannot make the download executable: {e}"))?;
    }
    sign(new);
    let out = std::process::Command::new(new)
        .arg("--version")
        .output()
        .map_err(|e| format!("the download does not run: {e}"))?;
    let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if said != format!("agentgw {want}") {
        return Err(format!("the download says {said:?}, not agentgw {want}"));
    }
    // Keep the old one. A hard link, so `target` never stops existing: the service manager may
    // start it at any moment
    let prev = target.with_file_name("agentgw.prev");
    let _ = std::fs::remove_file(&prev);
    if target.exists() {
        std::fs::hard_link(target, &prev)
            .or_else(|_| std::fs::copy(target, &prev).map(|_| ()))
            .map_err(|e| format!("cannot keep the old binary as {}: {e}", prev.display()))?;
    }
    std::fs::rename(new, target).map_err(|e| format!("cannot put the new binary in place: {e}"))
}

/// macOS only, and the same rule as `install.sh`: the `agentgw dev` certificate if there is one
/// (macOS permissions survive a new build), ad-hoc otherwise. A failure leaves it unsigned, which
/// `--version` then reports.
fn sign(path: &Path) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let id = std::env::var("SIGN_IDENTITY").unwrap_or_else(|_| "agentgw dev".into());
    let has_id = std::process::Command::new("security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&id))
        .unwrap_or(false);
    let sign_as = if has_id { id.as_str() } else { "-" };
    let _ = std::process::Command::new("codesign")
        .args(["--force", "--sign", sign_as, "--identifier", "agentgw"])
        .arg(path)
        .output();
}

async fn curl(args: &[&str]) -> Result<Vec<u8>, String> {
    let out = tokio::process::Command::new("curl")
        .args(args)
        .output()
        .await
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!("download failed: {err}"));
    }
    Ok(out.stdout)
}

/// The latest release's tag. **Public repository: no gh, no sign-in.**
pub async fn latest_tag() -> Result<String, String> {
    let url = format!("https://github.com/{}/releases/latest", crate::setup::add_machine::REPO);
    let out = curl(&["-sI", "--max-time", "20", "-o", "/dev/null", "-w", "%{redirect_url}", &url]).await?;
    tag_from_redirect(&String::from_utf8_lossy(&out))
        .ok_or_else(|| "cannot find the latest release".to_string())
}

/// Fetch `tag` and put it in place of the running binary. Does **not** restart.
pub async fn self_replace(tag: &str) -> Result<(), String> {
    let target = running_binary()?;
    let triple = this_triple().ok_or("no release is built for this kind of machine")?;
    let (bin_url, sum_url) = asset_urls(tag, triple);
    let sum = curl(&["-fsSL", "--max-time", "60", &sum_url]).await?;
    // Next to the target, so the rename stays on one filesystem
    let new = target.with_file_name("agentgw.new");
    let new_s = new.to_string_lossy().to_string();
    curl(&["-fsSL", "--max-time", "600", "-o", &new_s, &bin_url]).await?;
    let want = version_of(tag).to_string();
    let sum = String::from_utf8_lossy(&sum).to_string();
    tokio::task::spawn_blocking(move || put_in_place(&new, &sum, &target, &want))
        .await
        .map_err(|e| format!("the upgrade stopped: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agentgw-upgrade-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A stand-in binary that answers `--version` like agentgw does.
    fn fake_binary(dir: &Path, name: &str, version: &str) -> (PathBuf, String) {
        let p = dir.join(name);
        let body = format!("#!/bin/sh\necho \"agentgw {version}\"\n");
        std::fs::write(&p, &body).unwrap();
        (p, sha256_hex(body.as_bytes()))
    }

    #[test]
    fn reads_the_tag_off_the_latest_redirect() {
        assert_eq!(
            tag_from_redirect("https://github.com/o/r/releases/tag/v0.56.0\n").as_deref(),
            Some("v0.56.0")
        );
        // No release at all: GitHub sends you to the releases page instead
        assert_eq!(tag_from_redirect("https://github.com/o/r/releases"), None);
        assert_eq!(tag_from_redirect(""), None);
    }

    #[test]
    fn compares_versions_as_numbers() {
        assert!(older("0.9.0", "0.10.0"));
        assert!(older("v0.55.0", "0.56.0"));
        assert!(!older("0.56.0", "0.56.0"));
        assert!(!older("1.0.0", "0.99.0"));
        // Unreadable is never older
        assert!(!older("", "0.56.0"));
        assert!(!older("0.56", "0.57.0"));
    }

    #[test]
    fn a_version_and_its_tag_name_the_same_release() {
        assert_eq!(tag_of("0.56.0"), "v0.56.0");
        assert_eq!(tag_of("v0.56.0"), "v0.56.0");
        assert_eq!(version_of("v0.56.0"), "0.56.0");
    }

    #[test]
    fn names_the_binary_and_its_checksum() {
        let (bin, sum) = asset_urls("v0.56.0", "x86_64-unknown-linux-musl");
        assert!(bin.ends_with("/releases/download/v0.56.0/agentgw-x86_64-unknown-linux-musl"), "{bin}");
        assert_eq!(sum, format!("{bin}.sha256"));
    }

    #[test]
    fn a_checksum_file_may_carry_a_name_after_the_sum() {
        let h = "a".repeat(64);
        assert_eq!(expected_sum(&format!("{h}  agentgw-x\n")), Some(h.clone()));
        assert_eq!(expected_sum(&h.to_uppercase()), Some(h));
        assert_eq!(expected_sum("not a sum"), None);
    }

    #[test]
    fn a_good_download_replaces_the_binary_and_keeps_the_old_one() {
        let d = scratch("good");
        let (target, _) = fake_binary(&d, "agentgw", "0.55.0");
        let (new, sum) = fake_binary(&d, "agentgw.new", "0.56.0");
        put_in_place(&new, &sum, &target, "0.56.0").unwrap();
        assert!(std::fs::read_to_string(&target).unwrap().contains("0.56.0"));
        assert!(std::fs::read_to_string(d.join("agentgw.prev")).unwrap().contains("0.55.0"));
        assert!(!new.exists());
    }

    #[test]
    fn a_checksum_mismatch_leaves_the_binary_alone() {
        let d = scratch("sum");
        let (target, _) = fake_binary(&d, "agentgw", "0.55.0");
        let (new, _) = fake_binary(&d, "agentgw.new", "0.56.0");
        let err = put_in_place(&new, &"0".repeat(64), &target, "0.56.0").unwrap_err();
        assert!(err.contains("checksum"), "{err}");
        assert!(std::fs::read_to_string(&target).unwrap().contains("0.55.0"));
        assert!(!d.join("agentgw.prev").exists());
        assert!(!new.exists(), "the bad download is not left lying around");
    }

    #[test]
    fn a_wrong_version_leaves_the_binary_alone() {
        let d = scratch("ver");
        let (target, _) = fake_binary(&d, "agentgw", "0.55.0");
        let (new, sum) = fake_binary(&d, "agentgw.new", "0.55.9");
        let err = put_in_place(&new, &sum, &target, "0.56.0").unwrap_err();
        assert!(err.contains("0.55.9"), "{err}");
        assert!(std::fs::read_to_string(&target).unwrap().contains("0.55.0"));
    }
}
