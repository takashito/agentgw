//! Bakes the commit this binary was built from into `AGENTGW_COMMIT`, so a build can be told apart
//! from another of the same version (`agentgw --build`).
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    println!(
        "cargo:rustc-env=AGENTGW_COMMIT={commit}{}",
        if dirty { ".dirty" } else { "" }
    );
    // Rebuild the label when the sources change (dirty) or a commit moves HEAD. In a worktree
    // `--git-dir` is that worktree's own folder; the branch refs live in the common one
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/index");
    }
    if let (Some(common), Some(head)) = (
        git(&["rev-parse", "--git-common-dir"]),
        git(&["symbolic-ref", "-q", "HEAD"]),
    ) {
        println!("cargo:rerun-if-changed={common}/{head}");
        println!("cargo:rerun-if-changed={common}/packed-refs");
    }
}
