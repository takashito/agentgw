//! サービス管理 — macOS は launchd の LaunchAgent、Linux は systemd の --user unit。
//!
//! 現行は `shared/servicectl.ts`(手順)と `bridge/service.ts`(この job の素性)の2枚だが、
//! 割った理由は Remote の service と手順を共有するため。こちらは
//! 利用者がまだ1人なので1枚のまま — Relay Server を作るときに割る。
//!
//! 現行との意図的な差分: **plist / unit に `AGENTGW_STATE_DIR` を書き込む**。Bun は渡しておらず、
//! それが「1マシン1ボット」の制約になっている(skills/setup/SKILL.md:244-251)。dev の Rust 版を
//! 本番 Bun 版と同じマシンで並走させるには、ここを渡さないと隔離できない。

use crate::bridge::state::StateDir;
use std::io::Write;
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
  <string>{log_dir}/launchd-{base}.out.log</string>
  <key>StandardErrorPath</key>
  <string>{log_dir}/launchd-{base}.err.log</string>
</dict>
</plist>
"#,
            label = Self::xml_escape(&self.label),
            program = Self::xml_escape(&self.program),
            log_dir = Self::xml_escape(&self.log_dir),
            base = "bridge",
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
             StandardOutput=append:{log_dir}/systemd-{base}.out.log\n\
             StandardError=append:{log_dir}/systemd-{base}.err.log\n\
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
            base = "bridge",
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

/// bot/app トークンの組。
pub struct Tokens {
    pub bot: String,
    pub app: String,
}

impl Tokens {
    /// 取り違えは実際に起きる(どちらも「Slack のトークン」に見える)。接頭辞で両方向を弾く。
    pub fn validate(&self) -> Result<(), String> {
        if !self.bot.starts_with("xoxb-") {
            return Err(format!(
                "bot token は xoxb- で始まるはず(受け取ったのは {:?})— app token と取り違えていませんか",
                self.bot.chars().take(8).collect::<String>()
            ));
        }
        if !self.app.starts_with("xapp-") {
            return Err(format!(
                "app token は xapp- で始まるはず(受け取ったのは {:?})— bot token と取り違えていませんか",
                self.app.chars().take(8).collect::<String>()
            ));
        }
        Ok(())
    }

    /// `.env` にトークンを**置換で**書き入れる。積み増しにすると `load_env` は後勝ちで読むので、
    /// 消したはずの旧トークンが残り続ける。値は素で書く — 読み手(bridge::state::parse_env)は
    /// 引用符を剥ぐが、剥がれた後の値が正になるので最初から付けない。
    pub fn apply_to_env(&self, env_text: &str) -> String {
        let mut out: Vec<String> = env_text
            .lines()
            .filter(|line| {
                let k = line.trim_start();
                !k.starts_with("SLACK_BOT_TOKEN=") && !k.starts_with("SLACK_APP_TOKEN=")
            })
            .map(str::to_string)
            .collect();
        while out.last().is_some_and(|l| l.trim().is_empty()) {
            out.pop();
        }
        out.push(format!("SLACK_BOT_TOKEN={}", self.bot));
        out.push(format!("SLACK_APP_TOKEN={}", self.app));
        out.push(String::new()); // 末尾改行1つ
        out.join("\n")
    }
}

impl RestartStep {
    /// `pid` = サービスマネージャに聞いた本体 pid(None = 走っていない)。
    /// `alive` = その pid がまだ居るか。`waited_ms` = SIGUSR1 を送ってからの経過。
    pub fn next(pid: Option<u32>, alive: bool, waited_ms: u64, timeout_ms: u64) -> RestartStep {
        let Some(pid) = pid else {
            return RestartStep::Force("走っている Bridge の pid が引けない".to_string());
        };
        if !alive {
            return RestartStep::Gone;
        }
        if waited_ms >= timeout_ms {
            return RestartStep::Force(format!("pid {pid} が {timeout_ms}ms 待っても降りない"));
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

    fn home() -> PathBuf {
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
            eprintln!("{prog} が実行できません");
            return -1;
        };
        match Self::status_line(mac, &String::from_utf8_lossy(&o.stdout)) {
            Some(line) => println!("{line}"),
            // 定義ファイルがあるかは見れば分かる。推測で並べない
            None if job.exists() => println!("サービス: 止まっています"),
            None => println!("サービス: install していません"),
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

    fn run_ctl(cmd: &str, args: &[String]) -> i32 {
        match Self::ctl_command(cmd).args(args).status() {
            Ok(s) => s.code().unwrap_or(-1),
            Err(e) => {
                eprintln!("{cmd} が実行できません: {e}");
                -1
            }
        }
    }

    /// getuid は libc 無しでは引けないので `id -u` に聞く。
    fn uid() -> u32 {
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

    /// 端末から1行受け取る。パイプ越し(非対話)なら空文字が返るので、呼び手が弾く。
    fn prompt(question: &str) -> String {
        print!("{question}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line.trim().to_string()
    }

    /// `.env` に「このマシンが何者か」が書かれていなければ、**役割を1問だけ訊いて**書く。
    /// 既に書かれているなら何も触らない(**上書きしない** — 動いている設定を install の
    /// 副作用で壊さない)。
    ///
    /// 役割をここで訊くのが要だ。子は Slack のトークンを持たない(持てない — 直結と
    /// Relay 経由は同時に有効にできない)ので、トークンだけを求めると子は install を
    /// 通り抜けられない。
    fn ensure_config(state_dir: &StateDir) -> Result<(), String> {
        use std::io::IsTerminal;
        let env_file = state_dir.join(".env");
        let parsed = state_dir.load_env().unwrap_or_default();
        let has = |k: &str| {
            parsed
                .iter()
                .any(|(kk, v): &(String, String)| kk == k && !v.is_empty())
        };
        if has("SLACK_BOT_TOKEN") && has("SLACK_APP_TOKEN") {
            println!(
                "このマシンは親として設定済みです。.env は書き換えません。\n  \
                 設定ファイル: {}",
                env_file.display()
            );
            return Ok(());
        }
        // 子は2通り(自分から dial / 親に迎えに来てもらう)。どちらも Slack トークンは持たない
        if (has("AGENTGW_RELAY_URL") && has("AGENTGW_RELAY_TOKEN"))
            || (has("AGENTGW_LINK_LISTEN") && has("AGENTGW_LINK_TOKEN"))
        {
            println!(
                "このマシンは子として設定済みです。.env は書き換えません。\n  \
                 設定ファイル: {}",
                env_file.display()
            );
            return Ok(());
        }
        if !std::io::stdin().is_terminal() {
            return Err(format!(
                "端末ではないので役割を訊けません。{} に、親なら SLACK_BOT_TOKEN= と \
                 SLACK_APP_TOKEN= を、子なら接続文字列を `agentgw link` で\
                 書いてから install し直してください",
                env_file.display()
            ));
        }
        println!(
            "\nこのマシンの役割は?\n  \
             1) 親 — Slack に直接つなぐ(Slack app 1つにつき1台だけ)\n  \
             2) 子 — 別のマシンの親にぶら下がる"
        );
        match Self::prompt("> ").as_str() {
            "1" | "" => Self::ask_parent(state_dir),
            "2" => Self::ask_child(state_dir),
            other => Err(format!("1 か 2 を選んでください(受け取ったのは {other:?})")),
        }
    }

    /// 子として設定する — 親が出した接続文字列1本と、このマシンの名前。
    fn ask_child(state_dir: &StateDir) -> Result<(), String> {
        let raw = Self::prompt("  親から渡された接続文字列 (SCLINK1-…): ");
        let conn = crate::bridge::relay::link::decode_connection(&raw)?;
        // 名前は自動で決めない(衝突したマシンは互いの Slack メッセージを奪い合う)
        let name = crate::bridge::link::prompt_bridge_id()
            .ok_or_else(|| "このマシンの名前が要ります(`route <名前>` の指名先)".to_string())?;
        let env_file = state_dir.join(".env");
        let before = std::fs::read_to_string(&env_file).unwrap_or_default();
        let after = crate::bridge::link::apply_connection(&before, &conn, &name);
        std::fs::create_dir_all(state_dir.path())
            .map_err(|e| format!("{}: {e}", state_dir.path().display()))?;
        crate::bridge::state::write_atomic_mode(&env_file, &after, Some(0o600))
            .map_err(|e| format!("{}: {e}", env_file.display()))?;
        println!(
            "子「{name}」として書きました: {} (chmod 600)",
            env_file.display()
        );
        Ok(())
    }

    /// 親として設定する — Slack の bot / app トークン2本。
    fn ask_parent(state_dir: &StateDir) -> Result<(), String> {
        let env_file = state_dir.join(".env");
        let text = std::fs::read_to_string(&env_file).unwrap_or_default();
        println!("Slack のトークンを貼ってください。");
        let bot = Self::prompt("  bot token (xoxb-…): ");
        let app = Self::prompt("  app token (xapp-…): ");
        if bot.is_empty() || app.is_empty() {
            return Err(format!(
                "トークンが入力されませんでした。非対話で入れるなら {} に \
                 SLACK_BOT_TOKEN= と SLACK_APP_TOKEN= を書いてから install し直してください",
                env_file.display()
            ));
        }
        Tokens {
            bot: bot.clone(),
            app: app.clone(),
        }
        .validate()?;
        std::fs::create_dir_all(state_dir.path())
            .map_err(|e| format!("{}: {e}", state_dir.path().display()))?;
        state_dir
            .write_atomic(
                ".env",
                &Tokens {
                    bot: bot.clone(),
                    app: app.clone(),
                }
                .apply_to_env(&text),
            )
            .map_err(|e| format!("{}: {e}", env_file.display()))?;
        // トークンが入ったファイルを他人に読ませない
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o600));
        }
        println!("トークンを書きました: {} (chmod 600)", env_file.display());
        Ok(())
    }

    /// サブコマンド1つを実行し、プロセスの終了コードを返す。
    pub fn run(cmd: &str, rest: &[String]) -> i32 {
        if !cfg!(target_os = "macos") && !cfg!(target_os = "linux") {
            eprintln!(
                "このプラットフォームにはサービスの実装がありません(macOS=launchd / Linux=systemd)。\
                 `agentgw serve` を手で起こしてください"
            );
            return 2;
        }
        let mac = cfg!(target_os = "macos");
        let job = Self::job_path();
        match cmd {
            "install" => Self::install(mac, &job, rest),
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
            "uninstall" => Self::uninstall(mac, &job),
            "restart" => Self::graceful_restart(mac, &job),
            other => {
                eprintln!("知らないサブコマンドです: {other}");
                2
            }
        }
    }

    fn uninstall(mac: bool, job: &Path) -> i32 {
        // 止めてから定義を消す。止まっている相手への bootout は非ゼロを返すが、
        // 欲しいのは「消えていること」なので終了コードは見ない
        if mac {
            Self::run_ctl(
                "launchctl",
                &Action::Shutdown.launchctl_argv(Self::uid(), &Self::label(), ""),
            );
        } else {
            Self::run_ctl(
                "systemctl",
                &Action::systemctl_argv("disable", &Self::unit()),
            );
        }
        match std::fs::remove_file(job) {
            Ok(()) => println!("消しました: {}", job.display()),
            Err(e) => println!("消すものがありません({}): {e}", job.display()),
        }
        if !mac {
            Self::run_ctl(
                "systemctl",
                &Action::systemctl_argv("daemon-reload", &Self::unit()),
            );
        }
        println!("トークンと状態ディレクトリはそのままです。");
        0
    }

    fn install(mac: bool, job: &Path, rest: &[String]) -> i32 {
        let state_dir = StateDir::resolve();
        if let Err(e) = Self::ensure_config(&state_dir) {
            eprintln!("install: {e}");
            return 1;
        }
        let program = match std::env::current_exe() {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(e) => {
                eprintln!("install: 自分の実行パスが引けません: {e}");
                return 1;
            }
        };
        let log_dir = job
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let spec = JobSpec {
            label: Self::label(),
            program,
            state_dir: state_dir.path().to_string_lossy().to_string(),
            path: std::env::var("PATH").unwrap_or_default(),
            home: Self::home().to_string_lossy().to_string(),
            log_dir,
        };
        let text = if mac {
            spec.launchd_plist()
        } else {
            spec.systemd_unit()
        };
        if let Err(e) = crate::bridge::state::write_atomic_at(job, &text) {
            eprintln!("install: {} が書けません: {e}", job.display());
            return 1;
        }
        // 「.env は書き換えません」の直後に来る行。**何を書いたのかを言い分ける** —
        // どちらも「設定」と呼ぶと、書かないと言った直後に書いたことになって読めない
        println!("サービスの定義は書き直しました: {}", job.display());
        println!(
            "  サービス名: {}",
            if mac { Self::label() } else { Self::unit() }
        );
        println!("  状態の置き場: {}", spec.state_dir);
        if mac {
            // **この定義が今すぐ効くのは、次に起動したときだけ。** launchd は起動時の定義を
            // 握ったままなので restart では古い方が起き直る。ただしそれを毎回説くのは
            // うるさい — 効かせ方は1行で足りる。
            // 呼び手が直後に起こし直すなら**言わない** — 読んだ人が同じことを手で打つ
            if !rest.iter().any(|a| a == "--no-restart-hint") {
                println!("  反映するには: agentgw shutdown && agentgw start");
            }
        } else {
            Self::run_ctl(
                "systemctl",
                &Action::systemctl_argv("daemon-reload", &Self::unit()),
            );
            Self::run_ctl(
                "systemctl",
                &Action::systemctl_argv("enable", &Self::unit()),
            );
            // --user のサービスはログアウトで死ぬ。headless で使うなら linger が要る
            let user = std::env::var("USER").unwrap_or_default();
            if Self::run_ctl("loginctl", &["enable-linger".to_string(), user.clone()]) != 0 {
                println!(
                    "NOTE: linger を有効にできませんでした。ログアウト後も動かすなら1回だけ:\n  \
                     sudo loginctl enable-linger {user}"
                );
            }
        }
        0
    }

    /// SIGUSR1 を送って降りるのを待つ。待ちきれなければサービスマネージャの強制再起動に落とす
    fn graceful_restart(mac: bool, job: &Path) -> i32 {
        let force = |why: &str| -> i32 {
            println!(
                "  行儀よく降りられませんでした({why})— {} に強制的に再起動させます",
                if mac { "launchd" } else { "systemd" }
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
            return force("走っている Bridge の pid が引けない");
        };
        // **ワーカーは畳まれない。** 畳むのは shutdown と logout だけで、maintenance restart は
        // 在庫をそのまま残す(後継が pools.json の指名で拾い直す)。ここで「片付けてから降ります」と
        // 言っていたせいで、デプロイを「会話が切れるから」と遠慮する読み方が生まれた
        println!("  再起動します(走っているワーカーはそのまま、Bridge だけ入れ替わります)");
        if !std::process::Command::new("kill")
            .args(["-USR1", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return force(&format!("pid {pid} への SIGUSR1 が失敗"));
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
                    println!("  入れ替わりました");
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
            log_dir: "/Users/x/Library/LaunchAgents".to_string(),
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
    fn tokens_must_look_like_slack_tokens() {
        assert!(
            Tokens {
                bot: "xoxb-1-2".into(),
                app: "xapp-1-2".into()
            }
            .validate()
            .is_ok()
        );
        // 取り違えは実際に起きる(どちらも「Slack のトークン」に見える)ので、両方向を弾く
        assert!(
            Tokens {
                bot: "xapp-1-2".into(),
                app: "xoxb-1-2".into()
            }
            .validate()
            .is_err()
        );
        assert!(
            Tokens {
                bot: "".into(),
                app: "xapp-1".into()
            }
            .validate()
            .is_err()
        );
        assert!(
            Tokens {
                bot: "xoxb-1".into(),
                app: "".into()
            }
            .validate()
            .is_err()
        );
        assert!(
            Tokens {
                bot: "bot".into(),
                app: "app".into()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn applying_tokens_replaces_in_place_and_keeps_everything_else() {
        let before = "# 手書きのメモ\nSLACK_BOT_TOKEN=xoxb-old\nSOMETHING=else\n";
        let after = Tokens {
            bot: "xoxb-new".into(),
            app: "xapp-new".into(),
        }
        .apply_to_env(before);
        // 既存行は**置換**。2本目を積み増すと load_env は後勝ちになり、消したはずの旧値が残る
        assert_eq!(after.matches("SLACK_BOT_TOKEN=").count(), 1, "{after}");
        assert!(after.contains("SLACK_BOT_TOKEN=xoxb-new"), "{after}");
        assert!(after.contains("SLACK_APP_TOKEN=xapp-new"), "{after}");
        assert!(!after.contains("xoxb-old"), "{after}");
        // 無関係な行とコメントは保つ — .env は人も編集する
        assert!(after.contains("# 手書きのメモ"), "{after}");
        assert!(after.contains("SOMETHING=else"), "{after}");
    }

    #[test]
    fn applied_env_round_trips_through_the_reader() {
        let after = Tokens {
            bot: "xoxb-new".into(),
            app: "xapp-new".into(),
        }
        .apply_to_env("");
        // 書き手と読み手の形式が食い違うと静かに壊れる。読み手そのもので確かめる
        let dir = crate::bridge::state::StateDir::at(
            std::env::temp_dir().join(format!("sc-env-{}", std::process::id())),
        );
        dir.write_atomic(".env", &after).unwrap();
        let parsed = dir.load_env().unwrap();
        let get = |k: &str| {
            parsed
                .iter()
                .find(|(kk, _)| kk == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("SLACK_BOT_TOKEN"), Some("xoxb-new".to_string()));
        assert_eq!(get("SLACK_APP_TOKEN"), Some("xapp-new".to_string()));
    }

    #[test]
    fn applying_tokens_to_an_empty_file_ends_with_exactly_one_newline() {
        let after = Tokens {
            bot: "xoxb-1".into(),
            app: "xapp-1".into(),
        }
        .apply_to_env("");
        assert!(after.ends_with('\n'), "{after:?}");
        assert!(!after.ends_with("\n\n"), "{after:?}");
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
