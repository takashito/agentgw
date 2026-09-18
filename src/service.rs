//! サービス管理 — macOS は launchd の LaunchAgent、Linux は systemd の --user unit。
//!
//! 現行は `shared/servicectl.ts`(手順)と `bridge/service.ts`(この job の素性)の2枚だが、
//! 割った理由は Remote の service と手順を共有するため。こちらは
//! 利用者がまだ1人なので1枚のまま — Relay Server を作るときに割る。
//!
//! 現行との意図的な差分: **plist / unit に `AGENTGW_STATE_DIR` を書き込む**。Bun は渡しておらず、
//! それが「1マシン1ボット」の制約になっている(skills/setup/SKILL.md:244-251)。dev の Rust 版を
//! 本番 Bun 版と同じマシンで並走させるには、ここを渡さないと隔離できない。

use std::path::{Path, PathBuf};

/// この実装の既定。`AGENTGW_SERVICE_LABEL` で上書きできる(dev を並走させるとき)。
pub const DEFAULT_LABEL: &str = "com.agentgw.bridge";
/// systemd 側の同じもの。
pub const DEFAULT_UNIT: &str = "agentgw-bridge.service";

/// job 定義テキストを起こすのに要る素性。全部**絶対パス** — launchd は PATH を引かない。
pub struct JobSpec {
    pub label: String,
    /// `agentgw` バイナリの絶対パス。
    pub program: String,
    /// この job が使う状態ディレクトリ。これを渡すのが dev/本番並走の要。
    pub state_dir: String,
    /// claude / tmux を解決できる PATH。
    pub path: String,
    pub home: String,
    /// サービスマネージャの stdout/stderr を落とす先。
    pub log_dir: String,
}

/// サービスの生殺しに使う4つ。`Restart` は**強制**の側(行儀よい方は SIGUSR1)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Start,
    Restart,
    Shutdown,
    Status,
}

/// 行儀よい step-down を待つ上限。現行の 180 秒は
/// `claude plugin update` の 120 秒を含む値で、Rust にその工程は無い。
pub const GRACEFUL_RESTART_TIMEOUT_MS: u64 = 30_000;

/// 待ちループが1周ごとに回す判断。時計もプロセスも触らないので、そのままテストできる。
#[derive(Debug)]
pub enum RestartStep {
    /// 降りた。サービスマネージャが新しいのを起こす
    Gone,
    /// まだ生きている。もう1周待つ
    KeepWaiting,
    /// 行儀よい方は当てにできない。強制再起動に落とす(理由つき)
    Force(String),
}

// ── 実行部 ───────────────────────────────────────────────────────────────────
// ここは OS を叩くだけの層。判断は全部上の純関数に出してある(そちらがテスト済み)。

/// `serve` 以外に受け付けるサブコマンド。現行と同じ語彙から
/// `link`(Relay Server 用)を抜いたもの。
pub const COMMANDS: [&str; 6] = [
    "install",
    "start",
    "restart",
    "shutdown",
    "status",
    "uninstall",
];

impl JobSpec {
    /// LaunchAgent の job 定義。RunAtLoad + KeepAlive = ログイン時に起き、**どんな終わり方でも**
    /// 起き直る(crash も、restart の行儀よい step-down も等しく)。
    /// shim は挟まない(現行は plugin cache から最新版を exec する必要があったが、Rust は
    /// バイナリ1個)。launchd が本物の Bridge の pid を握るので、bootout / kickstart -k / SIGUSR1 が
    /// そのまま本体に届く。
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

    /// systemd の --user unit。plist と同じ意図: Restart=always + RestartSec=1 が
    /// KeepAlive + ThrottleInterval にあたる。Type=simple かつ shim 無しなので MainPID は
    /// Bridge 本体 — `systemctl --user stop` も MainPID への SIGUSR1 も本体に届く。
    ///
    /// **起動レート制限は切る(`StartLimitIntervalSec=0`)。** 既定は「10秒に5回落ちたら
    /// もう起こさない」で、unit が `failed` のまま**永久に**放置される。2026-08-03 に子が
    /// これで5時間15分死んだ(握手を5連続で断られ、1秒間隔の再起動が制限を踏んだ)。
    /// launchd 側は KeepAlive + ThrottleInterval=1 で諦めない — 両者の挙動をここで揃える。
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

    /// plist は XML — パスに `&` や `<` が入ると定義ごと読めなくなり、launchd は黙って失敗する。
    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
}

impl Action {
    /// 今どきの `gui/<uid>` ドメイン記法。`load`/`unload` は使わない(古い記法は
    /// bootstrap 済みの定義に効かないことがある)。
    pub fn launchctl_argv(&self, uid: u32, label: &str, plist_path: &str) -> Vec<String> {
        let target = format!("gui/{uid}/{label}");
        match *self {
            Action::Start => vec!["bootstrap".into(), format!("gui/{uid}"), plist_path.into()],
            Action::Restart => vec!["kickstart".into(), "-k".into(), target],
            Action::Shutdown => vec!["bootout".into(), target],
            Action::Status => vec!["print".into(), target],
        }
    }

    /// 常に `--user` スコープ。system の daemon は作らない。
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
    /// `pid` = サービスマネージャに聞いた本体 pid(None = 走っていない)。
    /// `alive` = その pid がまだ居るか。`waited_ms` = SIGUSR1 を送ってからの経過。
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

/// サービスの操作(launchd / systemd)。CLI からはここだけを叩く。
pub struct Service;

impl Service {
    /// CLI が受け付ける操作。`main` の分岐がこの表を引く。
    pub const COMMANDS: [&'static str; 6] = COMMANDS;

    /// この機で使う launchd ラベル。
    pub fn label() -> String {
        Self::label_of()
    }

    /// この機で使う systemd ユニット名。
    pub fn unit() -> String {
        Self::unit_of()
    }

    /// Relay は別の環境変数で上書きする — 1つの変数を共有すると、片方を dev に振ったときに
    /// もう片方まで巻き込まれる。
    fn label_of() -> String {
        std::env::var("AGENTGW_SERVICE_LABEL").unwrap_or_else(|_| DEFAULT_LABEL.to_string())
    }

    fn unit_of() -> String {
        std::env::var("AGENTGW_SERVICE_UNIT").unwrap_or_else(|_| DEFAULT_UNIT.to_string())
    }

    /// `launchctl print` の `pid = N` 行。走っている間しか出ないので、None は
    /// 「止まっている / そんな job は無い」= 行儀よい再起動は諦めて force、の合図。
    pub fn parse_launchctl_pid(print_output: &str) -> Option<u32> {
        print_output.lines().find_map(|line| {
            let (k, v) = line.split_once('=')?;
            if k.trim() != "pid" {
                return None;
            }
            v.trim().parse().ok()
        })
    }

    /// `systemctl --user show <unit> -p MainPID` の出力。`MainPID=1234` でも `--value` の
    /// 裸の数字でも読む。`0` は systemd の「本体プロセス無し」なので None に畳む。
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

    /// この OS の job 定義ファイルの置き場。
    fn job_path() -> PathBuf {
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

    /// サービスマネージャを1回叩き、終了コードを返す。出力はそのまま人に見せる。
    /// サービスの状態。**生ダンプは出さない** — `launchctl print` は jetsam やら 40 行、
    /// `systemctl status` は直近のログまで返すが、人が要るのは動いているかの1行。
    /// 呼び手が整形し直さなくて済むよう、ここで畳む。生が要るときは `--raw`
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
            // 定義ファイルがあるかは見れば分かる。推測で並べない
            None if job.exists() => println!("{}", crate::t!("Service: stopped", "サービス: 止まっています")),
            None => println!("{}", crate::t!("Service: not installed", "サービス: install していません")),
        }
        o.status.code().unwrap_or(-1)
    }

    /// 生ダンプから人が要る1行を取る。見つからなければ None。
    /// launchctl は `state = running`、systemctl は `Active: active (running) since …`
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

    /// `systemctl --user` は「どのユーザーの systemd に話すか」を `XDG_RUNTIME_DIR` で決める。
    /// **ssh の非対話セッションでは、これが空のことがある。**
    ///
    /// 2026-08-02 に実測: `/run/user/0` はあり linger も有効なのに、ssh 越しの
    /// 環境には変数だけが載っておらず、`daemon-reload` と `enable` が
    /// `Failed to connect to bus: No such file or directory` で落ちた。unit は書けているので
    /// **「入れたのに動かない」**という一番分かりにくい形で終わる(deploy.sh は失敗を
    /// 握り潰していたので、その2行だけが手がかりだった)。
    ///
    /// 既にあれば触らない — 人が別の値を指しているときに奪わない。
    fn runtime_dir_fallback(existing: Option<&std::ffi::OsStr>, uid: u32) -> Option<String> {
        match existing {
            Some(v) if !v.is_empty() => None,
            _ => Some(format!("/run/user/{uid}")),
        }
    }

    /// systemctl / loginctl / launchctl を起こす。Linux では上の穴を埋めてから渡す。
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

    /// getuid は libc 無しでは引けないので `id -u` に聞く。
    pub(crate) fn uid() -> u32 {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
            .unwrap_or(0)
    }

    /// サービスマネージャに本体 pid を聞く。走っていなければ None。
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

    /// pid がまだ居るか。`kill -0` と同じ意味を kill コマンドで訊く。
    /// `output()` なのは "No such process" を人の画面に漏らさないため(降りた瞬間に必ず出る)。
    fn alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// サブコマンド1つを実行し、プロセスの終了コードを返す。
    pub fn run(cmd: &str, rest: &[String]) -> i32 {
        if !cfg!(target_os = "macos") && !cfg!(target_os = "linux") {
            eprintln!(
                "{}",
                crate::t!(
                    "There's no service support for this platform (macOS uses launchd, Linux uses \
                     systemd). Run `agentgw serve` yourself.",
                    "このプラットフォームにはサービスの実装がありません(macOS=launchd / Linux=systemd)。\
                     `agentgw serve` を手で起こしてください"
                )
            );
            return 2;
        }
        let mac = cfg!(target_os = "macos");
        let job = Self::job_path();
        match cmd {
            "install" => crate::setup::install(mac, &job, rest),
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
            "uninstall" => crate::setup::uninstall(mac, &job),
            "restart" => Self::graceful_restart(mac, &job),
            other => {
                eprintln!("{}", crate::t!("Unknown command: {other}", "知らないサブコマンドです: {other}"));
                2
            }
        }
    }

    /// SIGUSR1 を送って降りるのを待つ。待ちきれなければサービスマネージャの強制再起動に落とす
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
        // **ワーカーは畳まれない。** 畳むのは shutdown と logout だけで、maintenance restart は
        // 在庫をそのまま残す(後継が pools.json の指名で拾い直す)。ここで「片付けてから降ります」と
        // 言っていたせいで、デプロイを「会話が切れるから」と遠慮する読み方が生まれた
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
        // Bun は渡していない(1マシン1ボットの原因)。ここが隔離の要
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
        // step-down(exit 0)も crash も等しく起こし直すのが常駐の条件
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
        // サービスの標準出力・エラーは状態の置き場の logs/ に(探す場所を1つにする)
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
        // Type=simple + 直接 exec = MainPID が Bridge 本体。stop / SIGUSR1 が本体に届く
        assert!(u.contains("Type=simple"), "{u}");
        assert!(u.contains("Restart=always"), "{u}");
        assert!(u.contains("RestartSec=1"), "{u}");
        // **諦めさせない。** 既定のレート制限(10秒に5回)は unit を `failed` のまま
        // 永久に放置する — launchd 側は諦めないので、ここで挙動を揃える
        assert!(u.contains("StartLimitIntervalSec=0"), "{u}");
    }

    #[test]
    fn status_line_takes_the_one_line_people_need() {
        // launchctl print はタブ字下げの `state = running` を 40 行の中に埋めてくる
        let mac =
            "gui/503/com.x.bridge = {\n\tactive count = 1\n\tstate = running\n\n\tprogram = /x\n";
        assert_eq!(
            Service::status_line(true, mac).as_deref(),
            Some("launchd: running")
        );
        // systemctl は since 以降も出す — いつから動いているかは人が見たい情報なので残す
        let linux = "● bridge.service - Bridge\n     Loaded: loaded\n     \
                     Active: active (running) since Sun 2026-08-02 04:00:00 UTC; 3s ago\n";
        assert_eq!(
            Service::status_line(false, linux).as_deref(),
            Some("systemd: active (running) since Sun 2026-08-02 04:00:00 UTC; 3s ago")
        );
        // 入っていない相手は何も返さない
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
        // restart は**強制**の側。行儀よい方は SIGUSR1 で、これはその取りこぼしの受け皿
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
        // ssh の非対話セッション(実測)。無いので補う
        assert_eq!(
            Service::runtime_dir_fallback(None, 0).as_deref(),
            Some("/run/user/0")
        );
        assert_eq!(
            Service::runtime_dir_fallback(Some(OsStr::new("")), 1000).as_deref(),
            Some("/run/user/1000")
        );
        // 人が指している値は奪わない
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
        // disable は --now(止めてから無効化)
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
        // 止まっている出力には pid 行が無い → force に落とす合図
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
        // systemd は「本体プロセス無し」を 0 で言う。生きた pid と混ぜてはいけない
        assert_eq!(Service::parse_systemd_main_pid("MainPID=0\n"), None);
        assert_eq!(Service::parse_systemd_main_pid(""), None);
    }

    #[test]
    fn restart_forces_when_there_is_nothing_to_signal() {
        // pid が引けない = 止まっている / そんな job は無い。待っても無駄
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
        // 現行の 180 秒は claude plugin update 120 秒込み。Rust にその工程は無い
        assert_eq!(GRACEFUL_RESTART_TIMEOUT_MS, 30_000);
    }

    #[test]
    fn label_and_unit_never_default_to_the_bun_names() {
        // 別実装のボット(Bun 版)の com.slack-channel.bridge と衝突しないことが不変条件
        assert_eq!(DEFAULT_LABEL, "com.agentgw.bridge");
        assert_eq!(DEFAULT_UNIT, "agentgw-bridge.service");
        assert_ne!(DEFAULT_LABEL, "com.slack-channel.bridge");
    }
}
