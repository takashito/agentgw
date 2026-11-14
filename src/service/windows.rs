//! The Windows stand-in for launchd / systemd: a scheduled task that starts at boot and runs as the
//! user **whether or not anyone is logged in**, and `agentgw supervise` inside it, which keeps one
//! `agentgw serve` running.
//!
//! Why this shape (measured on a Windows 11 ARM64 machine, 2026-11-14):
//! - A task set to "run whether the user is logged on or not" runs in session 0 as the user. The
//!   user's profile, `~/.claude/.credentials.json` and psmux all work there — **as long as psmux
//!   does not reach the Store build of `pwsh`**, which cannot start outside a desktop. agentgw's
//!   psmux uses Windows PowerShell 5.1 and does not see the Store aliases (see `agent::tmux`).
//!   Registering the task needs no administrator shell (measured from a normal one).
//! - Task Scheduler restarts a failed task after a minute at the soonest, too slow for a
//!   maintenance restart. So the task runs `supervise`, which starts `serve` again within a second.
//! - Windows has no SIGUSR1 / SIGHUP / SIGTERM to send to a background process. `restart`,
//!   `shutdown` and rereading access.json are asked for with **request files** in the state
//!   directory (`control/<what>`), which `serve` looks for every second.
//!
//! The task's password is asked for and handed over by PowerShell's own `Get-Credential`; agentgw
//! never sees it.

use crate::state_dir::StateDir;
use std::path::{Path, PathBuf};

/// What can be asked of a running `serve` / `supervise`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// Maintenance restart (agents keep running) — SIGUSR1 elsewhere
    Restart,
    /// Reread access.json / threads.json — SIGHUP elsewhere
    Reload,
    /// Graceful shutdown — SIGTERM elsewhere
    Shutdown,
    /// `supervise` stops instead of starting `serve` again
    StopSupervising,
}

impl Request {
    fn file(self) -> &'static str {
        match self {
            Request::Restart => "restart",
            Request::Reload => "reload",
            Request::Shutdown => "shutdown",
            Request::StopSupervising => "stop-supervising",
        }
    }
}

/// Where the request files live.
fn control_dir(dir: &StateDir) -> PathBuf {
    dir.join("control")
}

/// Leaves a request for the running process.
pub fn ask(dir: &StateDir, what: Request) -> std::io::Result<()> {
    let control = control_dir(dir);
    std::fs::create_dir_all(&control)?;
    std::fs::write(control.join(what.file()), b"")
}

/// Takes a request if one is waiting (removing it, so it is acted on once).
pub fn take(dir: &StateDir, what: Request) -> bool {
    std::fs::remove_file(control_dir(dir).join(what.file())).is_ok()
}

/// Clears a request that is no longer wanted (a restart asked for before the process that would
/// have acted on it went away).
pub fn clear(dir: &StateDir, what: Request) {
    let _ = std::fs::remove_file(control_dir(dir).join(what.file()));
}

/// The file `supervise` keeps the running `serve`'s pid in.
fn pid_file(dir: &StateDir) -> PathBuf {
    dir.join("serve.pid")
}

/// The pid of the `serve` that `supervise` started last, if it is still there.
pub fn serve_pid(dir: &StateDir) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(pid_file(dir)).ok()?.trim().parse().ok()?;
    alive(pid).then_some(pid)
}

/// Asks `tasklist` whether a pid is there.
pub fn alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
}

/// How long `supervise` waits before starting `serve` again: a second normally, longer while it
/// keeps dying young (a broken `.env` would otherwise spin through a start every second).
pub fn backoff(quick_exits_in_a_row: u32) -> std::time::Duration {
    std::time::Duration::from_secs(match quick_exits_in_a_row {
        0 | 1 => 1,
        2 => 5,
        3 => 15,
        _ => 60,
    })
}

/// `agentgw supervise` — what the scheduled task runs. Starts `serve` and starts it again
/// whenever it exits, until `shutdown` asks it to stop.
///
/// **The binary it starts is the one it was started as, by path** — `update` renames the running
/// file aside and puts the new one at that path, so the next `serve` is the new build while this
/// small loop goes on running from the old file.
pub fn supervise(dir: &StateDir) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("supervise: cannot find my own path: {e}");
            return 1;
        }
    };
    let logs = dir.join("logs");
    let _ = std::fs::create_dir_all(&logs);
    let note = |line: &str| {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(logs.join("supervise.log")) {
            let _ = writeln!(f, "{} pid={} {line}", chrono::Local::now().to_rfc3339(), std::process::id());
        }
    };
    // A stop asked for while nothing was supervising is stale: drop it, or this would quit at once
    clear(dir, Request::StopSupervising);
    note(&format!("supervising {}", exe.display()));
    let mut quick = 0u32;
    loop {
        let out = std::fs::OpenOptions::new().create(true).append(true).open(logs.join("agentgw.out.log"));
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("serve").env("AGENTGW_STATE_DIR", dir.path());
        if let Ok(f) = out {
            if let Ok(f2) = f.try_clone() {
                cmd.stdout(f).stderr(f2);
            }
        }
        let started = std::time::Instant::now();
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                note(&format!("could not start serve: {e}"));
                quick += 1;
                std::thread::sleep(backoff(quick));
                continue;
            }
        };
        let _ = std::fs::write(pid_file(dir), child.id().to_string());
        note(&format!("serve started (pid {})", child.id()));
        let status = child.wait();
        let _ = std::fs::remove_file(pid_file(dir));
        note(&format!("serve exited ({status:?}) after {}s", started.elapsed().as_secs()));
        if take(dir, Request::StopSupervising) {
            note("asked to stop — not starting serve again");
            return 0;
        }
        quick = if started.elapsed() < std::time::Duration::from_secs(30) { quick + 1 } else { 0 };
        std::thread::sleep(backoff(quick));
    }
}

/// The PowerShell that registers the task. `Get-Credential` asks for the password in the console
/// and hands it straight to Task Scheduler.
pub fn register_script(task: &str, exe: &Path, state_dir: &Path, home: &Path) -> String {
    let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let args = format!("supervise --state-dir \"{}\"", state_dir.display());
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         $cred = Get-Credential -UserName \"$env:USERDOMAIN\\$env:USERNAME\" -Message {msg}\n\
         if (-not $cred) {{ exit 1 }}\n\
         $a = New-ScheduledTaskAction -Execute {exe} -Argument {args} -WorkingDirectory {home}\n\
         $t = New-ScheduledTaskTrigger -AtStartup\n\
         $s = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 \
         -RestartInterval (New-TimeSpan -Minutes 1) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries \
         -StartWhenAvailable -MultipleInstances IgnoreNew\n\
         Register-ScheduledTask -TaskName {task} -Action $a -Trigger $t -Settings $s -User $cred.UserName \
         -Password $cred.GetNetworkCredential().Password -RunLevel Limited -Force | Out-Null\n",
        msg = q("Your Windows password, so agentgw can run while you are not logged in"),
        exe = q(&exe.to_string_lossy()),
        args = q(&args),
        home = q(&home.to_string_lossy()),
        task = q(task),
    )
}

/// Runs a PowerShell script in this console (so `Get-Credential` can ask). Windows PowerShell 5.1
/// is used: it is always there, and it is not the Store build.
fn powershell(script: &str) -> i32 {
    std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", script])
        .status()
        .map(|s| s.code().unwrap_or(1))
        .unwrap_or(1)
}

fn schtasks(args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("schtasks").args(args).output()
}

/// `agentgw install` on Windows: registers the task (asking for the password) and starts it.
pub fn install(task: &str, dir: &StateDir) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("install: cannot find my own path: {e}");
            return 1;
        }
    };
    let home = crate::service::Service::home();
    println!(
        "{}",
        crate::t!(
            "Registering the scheduled task {task} (starts at boot, runs as you, logged in or not).",
            "タスク {task} を登録します(PC の起動時に、あなたとして、ログインしていなくても動きます)。"
        )
    );
    if powershell(&register_script(task, &exe, dir.path(), &home)) != 0 {
        eprintln!("{}", crate::t!("install: the task was not registered", "install: タスクを登録できませんでした"));
        return 1;
    }
    println!("{}", crate::t!("Registered. Starting it.", "登録しました。起動します。"));
    start(task)
}

pub fn start(task: &str) -> i32 {
    match schtasks(&["/Run", "/TN", task]) {
        Ok(o) if o.status.success() => 0,
        Ok(o) => {
            eprintln!("{}", String::from_utf8_lossy(&o.stderr).trim());
            1
        }
        Err(e) => {
            eprintln!("schtasks: {e}");
            1
        }
    }
}

/// Waits up to `ms` for `cond`.
fn wait_for(ms: u64, cond: impl Fn() -> bool) -> bool {
    let mut waited = 0;
    while waited < ms {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
        waited += 250;
    }
    cond()
}

/// Stops `serve` gracefully and `supervise` with it. If `serve` does not go in time it is ended.
pub fn shutdown(task: &str, dir: &StateDir) -> i32 {
    let _ = ask(dir, Request::StopSupervising);
    match serve_pid(dir) {
        Some(pid) => {
            let _ = ask(dir, Request::Shutdown);
            if !wait_for(60_000, || !alive(pid)) {
                let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).output();
            }
        }
        None => clear(dir, Request::StopSupervising),
    }
    let _ = schtasks(&["/End", "/TN", task]);
    println!("{}", crate::t!("Stopped.", "止めました。"));
    0
}

/// Maintenance restart: `serve` exits on request and `supervise` starts it again. Agents keep running.
pub fn restart(task: &str, dir: &StateDir) -> i32 {
    let Some(pid) = serve_pid(dir) else {
        println!("{}", crate::t!("agentgw isn't running — starting it.", "agentgw が動いていないので起動します。"));
        return start(task);
    };
    println!("{}", crate::t!("  Restarting agentgw. Running agents keep going.", "  agentgw を再起動します。動いているエージェントはそのまま続きます。"));
    let _ = ask(dir, Request::Restart);
    if !wait_for(60_000, || !alive(pid)) {
        clear(dir, Request::Restart);
        let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).output();
    }
    if wait_for(30_000, || serve_pid(dir).is_some_and(|p| p != pid)) {
        println!("{}", crate::t!("  Restarted.", "  再起動しました。"));
        0
    } else {
        eprintln!("{}", crate::t!("  agentgw did not come back — see logs\\supervise.log", "  agentgw が戻ってきません — logs\\supervise.log を見てください"));
        1
    }
}

pub fn status(task: &str, dir: &StateDir) -> i32 {
    let registered = schtasks(&["/Query", "/TN", task]).is_ok_and(|o| o.status.success());
    match (registered, serve_pid(dir)) {
        (false, _) => println!("{}", crate::t!("✗ not installed (no task {task})", "✗ 入っていません(タスク {task} がありません)")),
        (true, Some(pid)) => println!("{}", crate::t!("● running (pid {pid})", "● 動いています(pid {pid})")),
        (true, None) => println!("{}", crate::t!("○ installed, not running", "○ 入っていますが動いていません")),
    }
    0
}

pub fn uninstall(task: &str, dir: &StateDir) -> i32 {
    shutdown(task, dir);
    match schtasks(&["/Delete", "/TN", task, "/F"]) {
        Ok(o) if o.status.success() => println!("{}", crate::t!("Removed the task {task}", "タスク {task} を消しました")),
        _ => println!("{}", crate::t!("No task {task} to remove", "消すタスク {task} がありません")),
    }
    println!("{}", crate::t!("Your tokens and the state directory are left in place.", "トークンと状態ディレクトリはそのままです。"));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_taken_once() {
        let dir = StateDir::at(std::env::temp_dir().join(format!("agentgw-control-{}", std::process::id())));
        assert!(!take(&dir, Request::Restart));
        ask(&dir, Request::Restart).unwrap();
        assert!(!take(&dir, Request::Reload), "requests do not cross");
        assert!(take(&dir, Request::Restart));
        assert!(!take(&dir, Request::Restart), "taken once");
        let _ = std::fs::remove_dir_all(dir.path());
    }

    #[test]
    fn a_serve_that_keeps_dying_young_is_restarted_more_slowly() {
        assert_eq!(backoff(0).as_secs(), 1);
        assert_eq!(backoff(1).as_secs(), 1);
        assert!(backoff(3) > backoff(2));
        assert_eq!(backoff(50).as_secs(), 60);
    }

    #[test]
    fn the_task_runs_supervise_at_boot_with_the_state_dir() {
        let s = register_script(
            "com.agentgw.bridge",
            Path::new(r"C:\Users\o'neil\.local\bin\agentgw.exe"),
            Path::new(r"C:\Users\o'neil\AppData\Local\agentgw"),
            Path::new(r"C:\Users\o'neil"),
        );
        assert!(s.contains(r"-Execute 'C:\Users\o''neil\.local\bin\agentgw.exe'"), "{s}");
        assert!(s.contains(r#"-Argument 'supervise --state-dir "C:\Users\o''neil\AppData\Local\agentgw"'"#), "{s}");
        assert!(s.contains("New-ScheduledTaskTrigger -AtStartup"));
        assert!(s.contains("-ExecutionTimeLimit ([TimeSpan]::Zero)"), "no time limit: it runs for good");
        assert!(s.contains("Get-Credential"));
        assert!(s.contains("-TaskName 'com.agentgw.bridge'"));
    }
}
