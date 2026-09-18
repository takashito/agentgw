//! Service management: a launchd LaunchAgent on macOS, a systemd --user unit on Linux.
//!
//! Kept as one module because it has a single user; split it if another service needs
//! the same steps.
//!
//! **The plist / unit pins `AGENTGW_STATE_DIR`.** Without it a machine can run only one bot:
//! a dev instance cannot be isolated from production on the same machine.

use std::path::{Path, PathBuf};

/// Default label. Override with `AGENTGW_SERVICE_LABEL` (to run a dev instance alongside).
pub const DEFAULT_LABEL: &str = "com.agentgw.bridge";
/// The same thing for systemd.
pub const DEFAULT_UNIT: &str = "agentgw-bridge.service";

/// What it takes to write the job definition. All **absolute paths**: launchd does not look up PATH.
pub struct JobSpec {
    pub label: String,
    /// Absolute path of the `agentgw` binary.
    pub program: String,
    /// State directory for this job. Passing it is what lets dev and production run side by side.
    pub state_dir: String,
    /// A PATH that can find claude and tmux.
    pub path: String,
    pub home: String,
    /// Where the service manager writes stdout/stderr.
    pub log_dir: String,
}

/// The four service operations. `Restart` is the **forced** one (the graceful one is SIGUSR1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Start,
    Restart,
    Shutdown,
    Status,
}

/// How long to wait for a graceful step-down. The old 180 s budget included 120 s for
/// `claude plugin update`, a step this binary does not have.
pub const GRACEFUL_RESTART_TIMEOUT_MS: u64 = 30_000;

/// The decision made on each turn of the wait loop. Touches no clock or process, so it is testable as is.
#[derive(Debug)]
pub enum RestartStep {
    /// It exited. The service manager starts a new one
    Gone,
    /// Still running. Wait another round
    KeepWaiting,
    /// The graceful path cannot be relied on. Fall back to a forced restart (with the reason)
    Force(String),
}

// ── Execution ───────────────────────────────────────────────────────────────
// This layer only calls the OS. Every decision lives in the pure functions above (which are tested).

/// Subcommands accepted besides `serve`.
/// (`install` / `uninstall` are setup's — see `setup::run`.)
pub const COMMANDS: [&str; 4] = ["start", "restart", "shutdown", "status"];

impl JobSpec {
    /// LaunchAgent job definition. RunAtLoad + KeepAlive = start at login and come back up
    /// **however it exits** (a crash and a graceful restart step-down alike).
    /// No shim in between (it is a single binary), so launchd holds the real Bridge pid and
    /// bootout / kickstart -k / SIGUSR1 reach the process itself.
    pub fn launchd_plist(&self) -> String {
        let env = [
            ("PATH", self.path.as_str()),
            ("HOME", self.home.as_str()),
            ("AGENTGW_STATE_DIR", self.state_dir.as_str()),
            ("AGENTGW_MANAGED", "1"),
        ]
        .iter()
        .map(|(k, v)| {
            format!(
                "    <key>{k}</key>\n    <string>{}</string>",
                Self::xml_escape(v)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>

  <key>ProgramArguments</key>
  <array>
    <string>{program}</string>
{args}
  </array>

  <key>EnvironmentVariables</key>
  <dict>
{env}
  </dict>

  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>1</integer>

  <key>StandardOutPath</key>
  <string>{log_dir}/service.out.log</string>
  <key>StandardErrorPath</key>
  <string>{log_dir}/service.err.log</string>
</dict>
</plist>
"#,
            label = Self::xml_escape(&self.label),
            program = Self::xml_escape(&self.program),
            log_dir = Self::xml_escape(&self.log_dir),
            args = "    <string>serve</string>",
        )
    }

    /// systemd --user unit, with the same intent as the plist: Restart=always + RestartSec=1
    /// match KeepAlive + ThrottleInterval. Type=simple with no shim makes MainPID the Bridge
    /// itself, so both `systemctl --user stop` and SIGUSR1 to MainPID reach it.
    ///
    /// **The start rate limit is off (`StartLimitIntervalSec=0`).** The default ("5 failures in
    /// 10 s and it stops restarting") leaves the unit `failed` **forever**. On 2026-08-03 a machine
    /// was down for 5 h 15 min this way (the handshake was refused 5 times in a row and the
    /// 1-second restarts hit the limit). launchd never gives up with KeepAlive +
    /// ThrottleInterval=1, so this makes the two behave the same.
    pub fn systemd_unit(&self) -> String {
        format!(
            "[Unit]\n\
             Description={description}\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             StartLimitIntervalSec=0\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={program} {args}\n\
             Restart=always\n\
             RestartSec=1\n\
             Environment=PATH={path}\n\
             Environment=HOME={home}\n\
             Environment={state_env}={state_dir}\n\
             Environment=AGENTGW_MANAGED=1\n\
             StandardOutput=append:{log_dir}/service.out.log\n\
             StandardError=append:{log_dir}/service.err.log\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
            description = "agentgw Bridge",
            program = self.program,
            args = "serve",
            path = self.path,
            home = self.home,
            state_env = "AGENTGW_STATE_DIR",
            state_dir = self.state_dir,
            log_dir = self.log_dir,
        )
    }

    /// The plist is XML: a `&` or `<` in a path makes the whole definition unreadable and launchd fails silently.
    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
}

impl Action {
    /// Modern `gui/<uid>` domain syntax. No `load`/`unload` (the old syntax sometimes has no
    /// effect on a definition that is already bootstrapped).
    pub fn launchctl_argv(&self, uid: u32, label: &str, plist_path: &str) -> Vec<String> {
        let target = format!("gui/{uid}/{label}");
        match *self {
            Action::Start => vec!["bootstrap".into(), format!("gui/{uid}"), plist_path.into()],
            Action::Restart => vec!["kickstart".into(), "-k".into(), target],
            Action::Shutdown => vec!["bootout".into(), target],
            Action::Status => vec!["print".into(), target],
        }
    }

    /// Always `--user` scope. Never creates a system daemon.
    pub fn systemctl_argv(action: &str, unit: &str) -> Vec<String> {
        match action {
            "daemon-reload" => vec!["--user".into(), "daemon-reload".into()],
            "disable" => vec![
                "--user".into(),
                "disable".into(),
                "--now".into(),
                unit.into(),
            ],
            other => vec!["--user".into(), other.into(), unit.into()],
        }
    }
}

impl RestartStep {
    /// `pid` = the main pid reported by the service manager (None = not running).
    /// `alive` = whether that pid still exists. `waited_ms` = time since SIGUSR1 was sent.
    pub fn next(pid: Option<u32>, alive: bool, waited_ms: u64, timeout_ms: u64) -> RestartStep {
        if pid.is_none() {
            return RestartStep::Force(crate::t!("agentgw isn't running", "agentgw が動いていません"));
        }
        if !alive {
            return RestartStep::Gone;
        }
        if waited_ms >= timeout_ms {
            return RestartStep::Force(crate::t!("it was still running after {timeout_ms}ms", "{timeout_ms}ms 待っても止まりませんでした"));
        }
        RestartStep::KeepWaiting
    }
}

/// Service operations (launchd / systemd). The CLI only goes through here.
pub struct Service;

impl Service {
    /// Operations the CLI accepts. The dispatch in `main` looks up this table.
    pub const COMMANDS: [&'static str; 4] = COMMANDS;

    /// The launchd label used on this machine.
    pub fn label() -> String {
        Self::label_of()
    }

    /// The systemd unit name used on this machine.
    pub fn unit() -> String {
        Self::unit_of()
    }

    /// Each service gets its own override variable: sharing one would drag the other along
    /// when one of them is pointed at dev.
    fn label_of() -> String {
        std::env::var("AGENTGW_SERVICE_LABEL").unwrap_or_else(|_| DEFAULT_LABEL.to_string())
    }

    fn unit_of() -> String {
        std::env::var("AGENTGW_SERVICE_UNIT").unwrap_or_else(|_| DEFAULT_UNIT.to_string())
    }

    /// The `pid = N` line of `launchctl print`. It only appears while running, so None means
    /// "stopped / no such job", the signal to give up on a graceful restart and force it.
    pub fn parse_launchctl_pid(print_output: &str) -> Option<u32> {
        print_output.lines().find_map(|line| {
            let (k, v) = line.split_once('=')?;
            if k.trim() != "pid" {
                return None;
            }
            v.trim().parse().ok()
        })
    }

    /// Output of `systemctl --user show <unit> -p MainPID`. Reads both `MainPID=1234` and the
    /// bare number from `--value`. `0` is systemd for "no main process", so it becomes None.
    pub fn parse_systemd_main_pid(show_output: &str) -> Option<u32> {
        show_output.lines().find_map(|line| {
            let raw = line.trim().strip_prefix("MainPID=").unwrap_or(line.trim());
            match raw.parse::<u32>() {
                Ok(0) | Err(_) => None,
                Ok(pid) => Some(pid),
            }
        })
    }

    pub(crate) fn home() -> PathBuf {
        std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
    }

    /// Where this OS keeps the job definition file.
    pub(crate) fn job_path() -> PathBuf {
        Self::job_path_of()
    }

    fn job_path_of() -> PathBuf {
        if cfg!(target_os = "macos") {
            Self::home()
                .join("Library/LaunchAgents")
                .join(format!("{}.plist", Self::label_of()))
        } else {
            Self::home()
                .join(".config/systemd/user")
                .join(Self::unit_of())
        }
    }

    /// Service status. **No raw dump**: `launchctl print` returns 40 lines of jetsam and more,
    /// `systemctl status` adds recent logs, but people only need one line saying whether it runs.
    /// Condensed here so callers need not reformat it. Use `--raw` for the full output
    fn status(mac: bool, rest: &[String], job: &Path) -> i32 {
        let (prog, argv) = if mac {
            (
                "launchctl",
                Action::Status.launchctl_argv(Self::uid(), &Self::label(), ""),
            )
        } else {
            ("systemctl", Action::systemctl_argv("status", &Self::unit()))
        };
        if rest.iter().any(|a| a == "--raw") {
            return Self::run_ctl(prog, &argv);
        }
        let Ok(o) = Self::ctl_command(prog).args(&argv).output() else {
            eprintln!("{}", crate::t!("Couldn't run {prog}", "{prog} が実行できません"));
            return -1;
        };
        match Self::status_line(mac, &String::from_utf8_lossy(&o.stdout)) {
            Some(line) => println!("{line}"),
            // Whether the definition file exists can be checked, so check instead of guessing
            None if job.exists() => println!("{}", crate::t!("Service: stopped", "サービス: 止まっています")),
            None => println!("{}", crate::t!("Service: not installed", "サービス: install していません")),
        }
        o.status.code().unwrap_or(-1)
    }

    /// Pick the one line people need from the raw dump. None if not found.
    /// launchctl: `state = running`, systemctl: `Active: active (running) since …`
    fn status_line(mac: bool, out: &str) -> Option<String> {
        let (key, label) = if mac {
            ("state = ", "launchd")
        } else {
            ("Active: ", "systemd")
        };
        out.lines()
            .find_map(|l| l.trim().strip_prefix(key))
            .map(|v| format!("{label}: {}", v.trim()))
    }

    /// `systemctl --user` uses `XDG_RUNTIME_DIR` to decide which user's systemd to talk to.
    /// **In a non-interactive ssh session it can be unset.**
    ///
    /// Measured 2026-08-02: `/run/user/0` existed and linger was on, but the variable was missing
    /// from the ssh environment, and `daemon-reload` and `enable` failed with
    /// `Failed to connect to bus: No such file or directory`. The unit file was written, so it
    /// ended in the most confusing way: **"installed but not running"** (the old deploy script
    /// swallowed the failure, so those two lines were the only clue).
    ///
    /// Left alone if already set: never override a value someone chose.
    fn runtime_dir_fallback(existing: Option<&std::ffi::OsStr>, uid: u32) -> Option<String> {
        match existing {
            Some(v) if !v.is_empty() => None,
            _ => Some(format!("/run/user/{uid}")),
        }
    }

    /// Build a systemctl / loginctl / launchctl command. On Linux, fill the gap above first.
    fn ctl_command(cmd: &str) -> std::process::Command {
        let mut c = std::process::Command::new(cmd);
        if !cfg!(target_os = "macos")
            && let Some(dir) = Self::runtime_dir_fallback(
                std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
                Self::uid(),
            )
        {
            c.env("XDG_RUNTIME_DIR", dir);
        }
        c
    }

    pub(crate) fn run_ctl(cmd: &str, args: &[String]) -> i32 {
        match Self::ctl_command(cmd).args(args).status() {
            Ok(s) => s.code().unwrap_or(-1),
            Err(e) => {
                eprintln!("{}", crate::t!("Couldn't run {cmd}: {e}", "{cmd} が実行できません: {e}"));
                -1
            }
        }
    }

    /// getuid needs libc, so ask `id -u` instead.
    pub(crate) fn uid() -> u32 {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
            .unwrap_or(0)
    }

    /// Ask the service manager for the main pid. None if not running.
    fn service_pid() -> Option<u32> {
        let (cmd, args) = if cfg!(target_os = "macos") {
            (
                "launchctl",
                Action::Status.launchctl_argv(Self::uid(), &Self::label(), ""),
            )
        } else {
            (
                "systemctl",
                vec![
                    "--user".to_string(),
                    "show".to_string(),
                    Self::unit(),
                    "-p".to_string(),
                    "MainPID".to_string(),
                    "--value".to_string(),
                ],
            )
        };
        let out = Self::ctl_command(cmd).args(&args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        if cfg!(target_os = "macos") {
            Self::parse_launchctl_pid(&text)
        } else {
            Self::parse_systemd_main_pid(&text)
        }
    }

    /// Whether the pid still exists, asked like `kill -0` through the kill command.
    /// `output()` keeps "No such process" off the user's screen (it always shows once the process exits).
    fn alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Whether this platform has a service manager we drive (launchd / systemd). Says why not when it hasn't.
    pub(crate) fn supported() -> bool {
        if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
            return true;
        }
        eprintln!(
            "{}",
            crate::t!(
                "There's no service support for this platform (macOS uses launchd, Linux uses \
                 systemd). Run `agentgw serve` yourself.",
                "このプラットフォームにはサービスの実装がありません(macOS=launchd / Linux=systemd)。\
                 `agentgw serve` を手で起こしてください"
            )
        );
        false
    }

    /// Run one subcommand and return the process exit code.
    pub fn run(cmd: &str, rest: &[String]) -> i32 {
        if !Self::supported() {
            return 2;
        }
        let mac = cfg!(target_os = "macos");
        let job = Self::job_path();
        match cmd {
            "start" => {
                if mac {
                    Self::run_ctl(
                        "launchctl",
                        &Action::Start.launchctl_argv(
                            Self::uid(),
                            &Self::label(),
                            &job.to_string_lossy(),
                        ),
                    )
                } else {
                    Self::run_ctl("systemctl", &Action::systemctl_argv("start", &Self::unit()))
                }
            }
            "shutdown" => {
                if mac {
                    Self::run_ctl(
                        "launchctl",
                        &Action::Shutdown.launchctl_argv(Self::uid(), &Self::label(), ""),
                    )
                } else {
                    Self::run_ctl("systemctl", &Action::systemctl_argv("stop", &Self::unit()))
                }
            }
            "status" => Self::status(mac, rest, &job),
            "restart" => Self::graceful_restart(mac, &job),
            other => {
                eprintln!("{}", crate::t!("Unknown command: {other}", "知らないサブコマンドです: {other}"));
                2
            }
        }
    }

    /// Send SIGUSR1 and wait for it to exit. If it takes too long, fall back to the service manager's forced restart
    fn graceful_restart(mac: bool, job: &Path) -> i32 {
        let force = |why: &str| -> i32 {
            let manager = if mac { "launchd" } else { "systemd" };
            println!(
                "{}",
                crate::t!(
                    "  agentgw didn't stop on its own ({why}), so {manager} will restart it",
                    "  agentgw が自分で止まれなかったので({why})、{manager} に再起動させます"
                )
            );
            if mac {
                Self::run_ctl(
                    "launchctl",
                    &Action::Restart.launchctl_argv(
                        Self::uid(),
                        &Self::label(),
                        &job.to_string_lossy(),
                    ),
                )
            } else {
                Self::run_ctl(
                    "systemctl",
                    &Action::systemctl_argv("restart", &Self::unit()),
                )
            }
        };
        let Some(pid) = Self::service_pid() else {
            return force(&crate::t!("agentgw isn't running", "agentgw が動いていません"));
        };
        // **Agents are not shut down.** Only shutdown and logout do that; a maintenance restart
        // keeps the pool as is (the successor picks it back up by name from pools.json). This
        // message used to say "cleaning up before exiting", which made people hold off deploying
        // for fear of cutting conversations
        println!("{}", crate::t!("  Restarting agentgw. Running agents keep going.", "  agentgw を再起動します。動いているエージェントはそのまま続きます。"));
        if !std::process::Command::new("kill")
            .args(["-USR1", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return force(&crate::t!("couldn't signal it to stop", "止める合図を送れませんでした"));
        }
        let mut waited = 0u64;
        loop {
            match RestartStep::next(
                Some(pid),
                Self::alive(pid),
                waited,
                GRACEFUL_RESTART_TIMEOUT_MS,
            ) {
                RestartStep::Gone => {
                    println!("{}", crate::t!("  Restarted.", "  再起動しました。"));
                    return 0;
                }
                RestartStep::Force(why) => return force(&why),
                RestartStep::KeepWaiting => {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    waited += 250;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> JobSpec {
        JobSpec {
            label: "com.agentgw.bridge".to_string(),
            program: "/Users/x/.local/bin/agentgw".to_string(),
            state_dir: "/Users/x/.local/state/agentgw-dev".to_string(),
            path: "/usr/local/bin:/usr/bin:/bin".to_string(),
            home: "/Users/x".to_string(),
            log_dir: "/Users/x/.local/state/agentgw-dev/logs".to_string(),
        }
    }

    #[test]
    fn plist_pins_the_state_dir_so_dev_and_prod_can_coexist() {
        let p = spec().launchd_plist();
        // Without this a machine can run only one bot. This is what isolates dev from production
        assert!(
            p.contains(
                "<key>AGENTGW_STATE_DIR</key>\n    <string>/Users/x/.local/state/agentgw-dev</string>"
            ),
            "{p}"
        );
        assert!(
            p.contains("<string>/Users/x/.local/bin/agentgw</string>"),
            "{p}"
        );
        assert!(p.contains("<string>serve</string>"), "{p}");
        assert!(
            p.contains("<key>Label</key>\n  <string>com.agentgw.bridge</string>"),
            "{p}"
        );
    }

    #[test]
    fn plist_relaunches_on_any_exit() {
        let p = spec().launchd_plist();
        // Staying resident means restarting after a step-down (exit 0) and a crash alike
        assert!(p.contains("<key>KeepAlive</key>\n  <true/>"), "{p}");
        assert!(p.contains("<key>RunAtLoad</key>\n  <true/>"), "{p}");
        assert!(
            p.contains("<key>ThrottleInterval</key>\n  <integer>1</integer>"),
            "{p}"
        );
    }

    #[test]
    fn plist_escapes_xml_so_a_path_cannot_break_the_definition() {
        let mut s = spec();
        s.path = "/opt/a&b:/usr/bin".to_string();
        let p = s.launchd_plist();
        assert!(p.contains("/opt/a&amp;b:/usr/bin"), "{p}");
        assert!(
            !p.contains("/opt/a&b:"),
            "生の & が残ると launchd は plist ごと読めない: {p}"
        );
    }

    #[test]
    fn service_logs_live_with_the_rest_of_the_state() {
        // Service stdout/stderr go to logs/ in the state directory (one place to look)
        let p = spec().launchd_plist();
        assert!(
            p.contains("<string>/Users/x/.local/state/agentgw-dev/logs/service.err.log</string>"),
            "{p}"
        );
        let u = spec().systemd_unit();
        assert!(
            u.contains(
                "StandardError=append:/Users/x/.local/state/agentgw-dev/logs/service.err.log"
            ),
            "{u}"
        );
    }

    #[test]
    fn systemd_unit_mirrors_the_plist_intent() {
        let u = spec().systemd_unit();
        assert!(
            u.contains("Environment=AGENTGW_STATE_DIR=/Users/x/.local/state/agentgw-dev"),
            "{u}"
        );
        assert!(
            u.contains("ExecStart=/Users/x/.local/bin/agentgw serve"),
            "{u}"
        );
        // Type=simple + direct exec = MainPID is the Bridge itself. stop / SIGUSR1 reach it
        assert!(u.contains("Type=simple"), "{u}");
        assert!(u.contains("Restart=always"), "{u}");
        assert!(u.contains("RestartSec=1"), "{u}");
        // **Never give up.** The default rate limit (5 in 10 s) leaves the unit `failed`
        // forever; launchd never gives up, so match it here
        assert!(u.contains("StartLimitIntervalSec=0"), "{u}");
    }

    #[test]
    fn status_line_takes_the_one_line_people_need() {
        // launchctl print buries a tab-indented `state = running` among 40 lines
        let mac =
            "gui/503/com.x.bridge = {\n\tactive count = 1\n\tstate = running\n\n\tprogram = /x\n";
        assert_eq!(
            Service::status_line(true, mac).as_deref(),
            Some("launchd: running")
        );
        // systemctl adds "since …"; keep it, people want to know how long it has been running
        let linux = "● bridge.service - Bridge\n     Loaded: loaded\n     \
                     Active: active (running) since Sun 2026-08-02 04:00:00 UTC; 3s ago\n";
        assert_eq!(
            Service::status_line(false, linux).as_deref(),
            Some("systemd: active (running) since Sun 2026-08-02 04:00:00 UTC; 3s ago")
        );
        // Nothing installed returns nothing
        assert_eq!(Service::status_line(true, ""), None);
        assert_eq!(
            Service::status_line(false, "Unit bridge.service could not be found."),
            None
        );
    }

    #[test]
    fn launchctl_argv_targets_the_gui_domain_of_this_user() {
        let a = Action::Start.launchctl_argv(501, "com.x.bridge", "/Users/x/L/com.x.bridge.plist");
        assert_eq!(
            a,
            vec!["bootstrap", "gui/501", "/Users/x/L/com.x.bridge.plist"]
        );
        assert_eq!(
            Action::Shutdown.launchctl_argv(501, "com.x.bridge", "/p"),
            vec!["bootout", "gui/501/com.x.bridge"]
        );
        // restart is the **forced** one. The graceful path is SIGUSR1; this catches what it misses
        assert_eq!(
            Action::Restart.launchctl_argv(501, "com.x.bridge", "/p"),
            vec!["kickstart", "-k", "gui/501/com.x.bridge"]
        );
        assert_eq!(
            Action::Status.launchctl_argv(501, "com.x.bridge", "/p"),
            vec!["print", "gui/501/com.x.bridge"]
        );
    }

    #[test]
    fn runtime_dir_is_filled_in_only_when_missing() {
        use std::ffi::OsStr;
        // Non-interactive ssh session (measured). Unset, so fill it in
        assert_eq!(
            Service::runtime_dir_fallback(None, 0).as_deref(),
            Some("/run/user/0")
        );
        assert_eq!(
            Service::runtime_dir_fallback(Some(OsStr::new("")), 1000).as_deref(),
            Some("/run/user/1000")
        );
        // Never override a value someone chose
        assert_eq!(
            Service::runtime_dir_fallback(Some(OsStr::new("/run/user/1000")), 1000),
            None
        );
    }

    #[test]
    fn systemctl_argv_is_always_user_scope() {
        for action in [
            "start",
            "restart",
            "stop",
            "status",
            "enable",
            "disable",
            "daemon-reload",
        ] {
            let a = Action::systemctl_argv(action, "u.service");
            assert_eq!(a[0], "--user", "system 全体を触ってはいけない: {a:?}");
        }
        assert_eq!(
            Action::systemctl_argv("stop", "u.service"),
            vec!["--user", "stop", "u.service"]
        );
        // disable uses --now (stop, then disable)
        assert_eq!(
            Action::systemctl_argv("disable", "u.service"),
            vec!["--user", "disable", "--now", "u.service"]
        );
        assert_eq!(
            Action::systemctl_argv("daemon-reload", "u.service"),
            vec!["--user", "daemon-reload"]
        );
    }

    #[test]
    fn launchctl_pid_is_read_only_while_it_runs() {
        let running = "com.x.bridge = {\n\tactive count = 1\n\tpid = 4242\n\tstate = running\n}";
        assert_eq!(Service::parse_launchctl_pid(running), Some(4242));
        // Stopped output has no pid line → the signal to force
        assert_eq!(
            Service::parse_launchctl_pid("com.x.bridge = {\n\tstate = not running\n}"),
            None
        );
        assert_eq!(Service::parse_launchctl_pid(""), None);
    }

    #[test]
    fn systemd_main_pid_treats_zero_as_absent() {
        assert_eq!(
            Service::parse_systemd_main_pid("MainPID=4242\n"),
            Some(4242)
        );
        assert_eq!(Service::parse_systemd_main_pid("4242\n"), Some(4242));
        // systemd says "no main process" with 0. Never mix it up with a live pid
        assert_eq!(Service::parse_systemd_main_pid("MainPID=0\n"), None);
        assert_eq!(Service::parse_systemd_main_pid(""), None);
    }

    #[test]
    fn restart_forces_when_there_is_nothing_to_signal() {
        // No pid = stopped / no such job. Waiting is pointless
        assert!(matches!(
            RestartStep::next(None, false, 0, GRACEFUL_RESTART_TIMEOUT_MS),
            RestartStep::Force(_)
        ));
    }

    #[test]
    fn restart_reports_gone_the_moment_the_pid_disappears() {
        assert!(matches!(
            RestartStep::next(Some(42), false, 250, GRACEFUL_RESTART_TIMEOUT_MS),
            RestartStep::Gone
        ));
    }

    #[test]
    fn restart_waits_until_the_deadline_then_forces() {
        assert!(matches!(
            RestartStep::next(Some(42), true, 29_000, GRACEFUL_RESTART_TIMEOUT_MS),
            RestartStep::KeepWaiting
        ));
        assert!(matches!(
            RestartStep::next(Some(42), true, 30_000, GRACEFUL_RESTART_TIMEOUT_MS),
            RestartStep::Force(_)
        ));
    }

    #[test]
    fn the_graceful_budget_is_not_the_bun_one() {
        // The old 180 s included 120 s for claude plugin update, a step this binary does not have
        assert_eq!(GRACEFUL_RESTART_TIMEOUT_MS, 30_000);
    }

    #[test]
    fn label_and_unit_never_default_to_the_bun_names() {
        // Invariant: never collide with com.slack-channel.bridge, the older implementation's bot
        assert_eq!(DEFAULT_LABEL, "com.agentgw.bridge");
        assert_eq!(DEFAULT_UNIT, "agentgw-bridge.service");
        assert_ne!(DEFAULT_LABEL, "com.slack-channel.bridge");
    }
}
