//! Which shells actually start on this Windows machine, **in the session agentgw runs in**.
//!
//! On Windows agentgw usually runs as a scheduled task while nobody is logged in (session 0), and
//! there a shell being installed is not the same as it starting: the Microsoft Store build of
//! PowerShell 7 is an app package, and packages do not start outside a desktop ("Permission
//! denied" / "The system cannot execute the specified program", measured 2026-11-14). Git for
//! Windows is optional. Windows PowerShell 5.1 is always there and starts anywhere.
//!
//! Three places use a shell, and only two are agentgw's to choose:
//! - the psmux window that starts the agent — always Windows PowerShell 5.1 (`agent::tmux`)
//! - the SessionStart hook — Claude Code offers `bash` or `powershell`; agentgw asks for PowerShell
//! - the agent's own tools — Claude Code picks: the Bash tool needs Git Bash, and the PowerShell
//!   tool takes `pwsh.exe` (7) when it finds one, else `powershell.exe` (5.1). There is no setting
//!   to choose the executable; there is one to turn the tool off, used when the `pwsh` it would
//!   find does not start.
//!
//! Each candidate is **started**, not looked for: whether the file exists says nothing in session 0.

use std::path::{Path, PathBuf};

/// What starting one kind of shell came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    /// Started (the path that did)
    Works(String),
    /// There, but would not start (the path, and what it said)
    Fails(String, String),
    /// Not installed
    Absent,
}

impl Probe {
    pub fn works(&self) -> bool {
        matches!(self, Probe::Works(_))
    }
}

/// The three shells, as found by [`probe`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shells {
    pub git_bash: Probe,
    pub pwsh: Probe,
    pub powershell: Probe,
}

impl Shells {
    /// Turn Claude Code's PowerShell tool off: there is a `pwsh` it may pick, and it does not start.
    /// With no `pwsh` at all the tool falls back to 5.1 and works, so it stays on.
    pub fn powershell_tool_off(&self) -> bool {
        matches!(self.pwsh, Probe::Fails(..))
    }

    /// When the agents are left without any working shell tool — no Git Bash, and the PowerShell
    /// tool is off — the words to tell the Owner, with what to install.
    pub fn problem(&self) -> Option<String> {
        if self.git_bash.works() || !self.powershell_tool_off() {
            return None;
        }
        Some(crate::t!(
            "Agents on this machine have no shell to run commands with: Git for Windows is not installed, \
             and the PowerShell 7 here (the Microsoft Store build) cannot start while no one is logged in. \
             Install Git for Windows, or PowerShell 7 from its MSI, then `restart`.",
            "このマシンのエージェントにはコマンドを走らせるシェルがありません: Git for Windows が無く、\
             ここの PowerShell 7(Microsoft Store 版)はログインしていない間は起動できません。\
             Git for Windows か、MSI 版の PowerShell 7 を入れてから `restart` してください。"
        ))
    }

    /// One line for logs and `status`: `Git Bash ✓ · pwsh ✗ (…) · Windows PowerShell 5.1 ✓`.
    pub fn summary(&self) -> String {
        let one = |name: &str, p: &Probe| match p {
            Probe::Works(_) => format!("{name} ✓"),
            Probe::Fails(path, why) => format!("{name} ✗ ({path}: {why})"),
            Probe::Absent => format!("{name} —"),
        };
        format!(
            "{} · {} · {}{}",
            one("Git Bash", &self.git_bash),
            one("pwsh", &self.pwsh),
            one("Windows PowerShell 5.1", &self.powershell),
            if self.powershell_tool_off() { " → PowerShell tool off" } else { "" }
        )
    }
}

/// The result of trying one candidate.
enum Tried {
    Started,
    Refused(String),
    Missing,
}

/// Starts `path` with `args` and waits for it to exit 0.
fn try_start(path: &Path, args: &[&str]) -> Tried {
    // `symlink_metadata`, not `is_file`: an app-execution alias is a reparse point that may not
    // read as a file, and it is exactly the one to try
    if std::fs::symlink_metadata(path).is_err() {
        return Tried::Missing;
    }
    match std::process::Command::new(path).args(args).stdin(std::process::Stdio::null()).output() {
        Ok(o) if o.status.success() => Tried::Started,
        Ok(o) => Tried::Refused(
            String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("exit code not 0").trim().to_string(),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Tried::Missing,
        Err(e) => Tried::Refused(e.to_string()),
    }
}

/// One kind of shell over its candidate paths. **Any candidate that is there but refuses makes
/// it `Fails`** — Claude Code's search order is not documented, so it may well pick that one.
fn probe_kind(candidates: &[PathBuf], args: &[&str]) -> Probe {
    let mut worked = None;
    for path in candidates {
        match try_start(path, args) {
            Tried::Started => worked = worked.or_else(|| Some(path.display().to_string())),
            Tried::Refused(why) => return Probe::Fails(path.display().to_string(), why),
            Tried::Missing => {}
        }
    }
    worked.map_or(Probe::Absent, Probe::Works)
}

fn env_path(var: &str, rest: &[&str]) -> Option<PathBuf> {
    let mut p = PathBuf::from(std::env::var_os(var)?);
    for r in rest {
        p.push(r);
    }
    Some(p)
}

/// Every place a `name` on PATH resolves to.
fn on_path(name: &str) -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join(name)).filter(|f| f.exists()).collect())
        .unwrap_or_default()
}

/// Tries the shells. Slow-ish (each PowerShell takes about a second to start), so it runs once,
/// at start-up.
pub fn probe() -> Shells {
    let mut bash: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("CLAUDE_CODE_GIT_BASH_PATH") {
        bash.push(PathBuf::from(p));
    }
    // Git for Windows: git.exe is on PATH from <root>\cmd; bash.exe is <root>\bin\bash.exe
    for git in on_path("git.exe") {
        if let Some(root) = git.parent().and_then(Path::parent) {
            bash.push(root.join("bin").join("bash.exe"));
        }
    }
    bash.extend(env_path("ProgramFiles", &["Git", "bin", "bash.exe"]));
    bash.dedup();

    let mut pwsh = on_path("pwsh.exe");
    pwsh.extend(env_path("ProgramFiles", &["PowerShell", "7", "pwsh.exe"]));
    // The Store build's alias. Claude Code finds it even when it is not on PATH (measured)
    pwsh.extend(env_path("LOCALAPPDATA", &["Microsoft", "WindowsApps", "pwsh.exe"]));
    pwsh.dedup();

    let powershell: Vec<PathBuf> = env_path("SystemRoot", &["System32", "WindowsPowerShell", "v1.0", "powershell.exe"])
        .into_iter()
        .collect();

    let ps_args = ["-NoProfile", "-NonInteractive", "-Command", "exit 0"];
    Shells {
        git_bash: probe_kind(&bash, &["-c", "exit 0"]),
        pwsh: probe_kind(&pwsh, &ps_args),
        powershell: probe_kind(&powershell, &ps_args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shells(git_bash: Probe, pwsh: Probe) -> Shells {
        Shells { git_bash, pwsh, powershell: Probe::Works("powershell.exe".into()) }
    }

    #[test]
    fn a_pwsh_that_will_not_start_turns_the_tool_off() {
        let store = Probe::Fails("WindowsApps\\pwsh.exe".into(), "Access is denied".into());
        assert!(shells(Probe::Works("bash".into()), store.clone()).powershell_tool_off());
        // No pwsh at all: the tool falls back to 5.1, which starts
        assert!(!shells(Probe::Works("bash".into()), Probe::Absent).powershell_tool_off());
        assert!(!shells(Probe::Absent, Probe::Works("pwsh".into())).powershell_tool_off());
    }

    #[test]
    fn only_no_bash_and_a_broken_pwsh_leaves_agents_without_a_shell() {
        let store = Probe::Fails("pwsh.exe".into(), "denied".into());
        assert!(shells(Probe::Absent, store.clone()).problem().is_some());
        assert!(shells(Probe::Works("bash".into()), store).problem().is_none());
        assert!(shells(Probe::Absent, Probe::Absent).problem().is_none(), "5.1 serves the PowerShell tool");
    }

    #[test]
    fn the_summary_says_what_started() {
        let s = shells(Probe::Absent, Probe::Fails("p".into(), "denied".into())).summary();
        assert!(s.contains("Git Bash —") && s.contains("pwsh ✗ (p: denied)") && s.contains("5.1 ✓"), "{s}");
        assert!(s.ends_with("PowerShell tool off"), "{s}");
    }
}
