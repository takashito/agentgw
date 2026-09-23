//! Setup: everything that prepares a machine before `serve` runs — `install` / `uninstall`
//! (the role question, Slack tokens, the service definition), `link` (a connection string into
//! `.env`), and `add-machine` (install and connect another machine from the gateway).

pub mod add_machine;
pub mod ssh;

use crate::bridge::gateway::link;
use crate::state_dir::{StateDir, set_env_keys};
use crate::service::{Action, JobSpec, Service};
use std::io::Write;
use std::path::Path;

/// A bot/app token pair.
pub struct Tokens {
    pub bot: String,
    pub app: String,
}

impl Tokens {
    /// Mixing them up really happens (both look like "a Slack token"). The prefixes catch it both ways.
    pub fn validate(&self) -> Result<(), String> {
        if !self.bot.starts_with("xoxb-") {
            return Err(crate::t!(
                "The bot token should start with xoxb- (got {:?}). Did you swap it with the app token?",
                "bot token は xoxb- で始まるはず(受け取ったのは {:?})— app token と取り違えていませんか",
                self.bot.chars().take(8).collect::<String>()
            ));
        }
        if !self.app.starts_with("xapp-") {
            return Err(crate::t!(
                "The app token should start with xapp- (got {:?}). Did you swap it with the bot token?",
                "app token は xapp- で始まるはず(受け取ったのは {:?})— bot token と取り違えていませんか",
                self.app.chars().take(8).collect::<String>()
            ));
        }
        Ok(())
    }

    /// Writes the tokens into `.env` **by replacing**. If appended instead, `load_env` reads last-wins,
    /// so an old token you thought you removed keeps living. Values are written bare — the reader
    /// (bridge::state::parse_env) strips quotes, and the stripped value is what counts, so don't add them.
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
        out.push(String::new()); // one trailing newline
        out.join("\n")
    }
}

/// The Slack app settings (manifest) agentgw needs, and a creation link carrying them.
///
/// `slack-app-manifest.json` at the repo root is the single source of truth. The README's
/// one-click link is built from the same file (a test fails if they drift).
pub struct SlackApp;

impl SlackApp {
    pub const MANIFEST: &'static str = include_str!("../slack-app-manifest.json");

    /// `https://api.slack.com/apps?new_app=1&manifest_json=…` — opening it shows Slack's "create from
    /// a manifest" screen with these settings already filled in.
    pub fn create_url() -> String {
        // Strip whitespace and newlines before encoding (to keep the URL short)
        let compact = serde_json::from_str::<serde_json::Value>(Self::MANIFEST)
            .map(|v| v.to_string())
            .unwrap_or_default();
        format!(
            "https://api.slack.com/apps?new_app=1&manifest_json={}",
            percent_encoding::utf8_percent_encode(&compact, percent_encoding::NON_ALPHANUMERIC)
        )
    }

    /// Opens it in a browser if possible. If not (over ssh, Linux without a display), does nothing —
    /// the URL has already been printed.
    fn open_in_browser(url: &str) {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
            "xdg-open"
        } else {
            return;
        };
        let _ = std::process::Command::new(opener)
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// Reads one line from the terminal. Over a pipe (non-interactive) it returns "", which the caller rejects.
pub(crate) fn prompt(question: &str) -> String {
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    line.trim().to_string()
}

/// If `.env` doesn't say what this machine is, **ask the one role question** and write it.
/// If it already does, touch nothing (**no overwrite** — don't break a working setup as a
/// side effect of install).
///
/// Asking the role here is the point. A machine holds no Slack tokens (it can't — a direct
/// connection and going through the gateway can't both be on), so asking only for tokens would
/// leave a machine unable to get through install.
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
            "{}",
            crate::t!(
                "This machine is already set up as the gateway. Leaving .env as it is.\n  \
                 Settings: {}",
                "このマシンはゲートウェイとして設定済みです。.env は書き換えません。\n  \
                 設定ファイル: {}",
                env_file.display()
            )
        );
        return Ok(());
    }
    // A machine connects one of two ways (dials out itself / the gateway comes to fetch it). Neither holds Slack tokens
    if (has("AGENTGW_RELAY_URL") && has("AGENTGW_RELAY_TOKEN"))
        || (has("AGENTGW_LINK_LISTEN") && has("AGENTGW_LINK_TOKEN"))
    {
        println!(
            "{}",
            crate::t!(
                "This machine is already connected to a gateway. Leaving .env as it is.\n  \
                 Settings: {}",
                "このマシンはゲートウェイにつながる設定済みです。.env は書き換えません。\n  \
                 設定ファイル: {}",
                env_file.display()
            )
        );
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err(crate::t!(
            "There's no terminal to ask which role this machine plays. For the gateway, put \
             SLACK_BOT_TOKEN= and SLACK_APP_TOKEN= in {} and run install again; for any other \
             machine, run `agentgw add-machine` on the gateway instead.",
            "端末が無いので、このマシンの役割を訊けません。ゲートウェイにするなら {} に \
             SLACK_BOT_TOKEN= と SLACK_APP_TOKEN= を書いて install し直してください。\
             ほかのマシンは、ゲートウェイで `agentgw add-machine` を実行して加えます。",
            env_file.display()
        ));
    }
    println!(
        "{}",
        crate::t!(
            "\nWhat role does this machine play?\n  \
             1) Gateway — connects to Slack (one per Slack app)\n  \
             2) Machine — works for a gateway on another machine",
            "\nこのマシンの役割を選んでください。\n  \
             1) ゲートウェイ — Slack につなぐ(Slack アプリ1つにつき1台)\n  \
             2) マシン — 別のマシンのゲートウェイの下で動く"
        )
    );
    match prompt("> ").as_str() {
        "1" | "" => ask_parent(state_dir),
        "2" => ask_child(state_dir),
        other => Err(crate::t!("Choose 1 or 2 (got {other:?})", "1 か 2 を選んでください(受け取ったのは {other:?})")),
    }
}

/// Set up as a machine — one connection string from the gateway, and this machine's name.
fn ask_child(state_dir: &StateDir) -> Result<(), String> {
    let raw = prompt(&crate::t!(
        "  Connection string from the gateway (SCLINK1-…): ",
        "  ゲートウェイから受け取った接続文字列 (SCLINK1-…): "
    ));
    let conn = crate::bridge::gateway::link::decode_connection(&raw)?;
    // Never pick the name automatically (machines with the same name steal each other's Slack messages)
    let name = prompt_bridge_id()
        .ok_or_else(|| {
            crate::t!(
                "This machine needs a name (it's what `pwd <name>:<path>` points at)",
                "このマシンの名前が要ります(`pwd <名前>:<パス>` の指名先)"
            )
        })?;
    let env_file = state_dir.join(".env");
    let before = std::fs::read_to_string(&env_file).unwrap_or_default();
    let after = apply_connection(&before, &conn, &name);
    std::fs::create_dir_all(state_dir.path())
        .map_err(|e| format!("{}: {e}", state_dir.path().display()))?;
    crate::state_dir::write_atomic_mode(&env_file, &after, Some(0o600))
        .map_err(|e| format!("{}: {e}", env_file.display()))?;
    println!(
        "{}",
        crate::t!(
            "Saved as machine \"{name}\": {} (chmod 600)",
            "マシン「{name}」として保存しました: {} (chmod 600)",
            env_file.display()
        )
    );
    Ok(())
}

/// Set up as the gateway — the two Slack tokens, bot and app.
fn ask_parent(state_dir: &StateDir) -> Result<(), String> {
    let env_file = state_dir.join(".env");
    let text = std::fs::read_to_string(&env_file).unwrap_or_default();
    // **For people without an app yet, open a creation screen with the settings filled in.** Picking
    // the scopes, events, Socket Mode and Interactivity by hand means one miss and it silently doesn't work
    // (forget Interactivity and buttons never arrive; forget the DM tab and login is impossible)
    let url = SlackApp::create_url();
    println!(
        "{}",
        crate::t!(
            "No Slack app yet? This link creates one with everything pre-configured:\n  {url}\n\
             Then (1) install it to your workspace and copy the Bot User OAuth Token (xoxb-…), and\n\
             (2) under Basic Information → App-Level Tokens, create a token with connections:write (xapp-…).\n\
             Paste both below.\n",
            "Slack アプリがまだ無ければ、次の URL で設定済みのまま作れます:\n  {url}\n\
             作ったら (1) ワークスペースにインストールして Bot User OAuth Token(xoxb-…)を、\n\
             (2) Basic Information → App-Level Tokens で connections:write のトークン(xapp-…)を作って、\n\
             下に貼ってください。\n"
        )
    );
    SlackApp::open_in_browser(&url);
    println!("{}", crate::t!("Paste your Slack tokens.", "Slack のトークンを貼ってください。"));
    let bot = prompt("  bot token (xoxb-…): ");
    let app = prompt("  app token (xapp-…): ");
    if bot.is_empty() || app.is_empty() {
        return Err(crate::t!(
            "No tokens were entered. To install without prompts, put SLACK_BOT_TOKEN= and \
             SLACK_APP_TOKEN= in {} and run install again.",
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
    // **Owner-only from the moment it is created.** Writing it first and chmod-ing after leaves a window in
    // which both Slack tokens are world-readable
    crate::state_dir::write_atomic_mode(
        &env_file,
        &Tokens {
            bot: bot.clone(),
            app: app.clone(),
        }
        .apply_to_env(&text),
        Some(0o600),
    )
    .map_err(|e| format!("{}: {e}", env_file.display()))?;
    println!(
        "{}",
        crate::t!(
            "Saved the tokens: {} (owner only)",
            "トークンを保存しました: {}(自分だけが読めます)",
            env_file.display()
        )
    );
    default_listen(state_dir);
    Ok(())
}

/// Where the gateway accepts machines. Loopback to start with, and **loopback stays** whatever else
/// is added: it is where `tailscale serve` (or any front that terminates TLS) forwards to, and where
/// an ssh tunnel comes out. Nothing wider is opened until `add-machine` measures that a machine
/// needs it.
fn default_listen(state_dir: &StateDir) {
    let set = state_dir
        .load_env()
        .unwrap_or_default()
        .iter()
        .any(|(k, v)| k == "AGENTGW_LINK_LISTEN" && !v.trim().is_empty());
    if !set {
        write_listen(state_dir, &format!("127.0.0.1:{}", crate::bridge::gateway::DEFAULT_PORT));
    }
}

/// Add one address to what the gateway listens on, keeping what is there. `false` = it already had it.
///
/// **Called before each route is tried**, not at install time: a route can only be tried once the
/// address it needs is open. So this opens the address of **every route attempted**, not just the one
/// that wins, and nothing takes an address back out again.
pub(crate) fn add_listen(state_dir: &StateDir, addr: &str) -> bool {
    let current = state_dir
        .load_env()
        .unwrap_or_default()
        .into_iter()
        .find(|(k, _)| k == "AGENTGW_LINK_LISTEN")
        .map(|(_, v)| v)
        .unwrap_or_default();
    if current.split(',').any(|a| a.trim() == addr) {
        return false;
    }
    let mut addrs: Vec<&str> = current.split(',').map(str::trim).filter(|a| !a.is_empty()).collect();
    addrs.push(addr);
    write_listen(state_dir, &addrs.join(","));
    true
}

fn write_listen(state_dir: &StateDir, value: &str) {
    let path = state_dir.join(".env");
    let before = std::fs::read_to_string(&path).unwrap_or_default();
    let after = crate::state_dir::set_env_keys(&before, &[("AGENTGW_LINK_LISTEN", value.to_string())]);
    match crate::state_dir::write_atomic_mode(&path, &after, Some(0o600)) {
        Ok(()) => println!(
            "{}",
            crate::t!(
                "Machines connect to: {value}",
                "マシンからの接続を受ける場所: {value}"
            )
        ),
        Err(e) => eprintln!("{}: {e}", path.display()),
    }
}

pub fn uninstall(mac: bool, job: &Path) -> i32 {
    // Stop first, then remove the definition. bootout on a stopped service returns non-zero, but
    // what we want is "it is gone", so the exit code is ignored
    if mac {
        Service::run_ctl(
            "launchctl",
            &Action::Shutdown.launchctl_argv(Service::uid(), &Service::label(), ""),
        );
    } else {
        Service::run_ctl(
            "systemctl",
            &Action::systemctl_argv("disable", &Service::unit()),
        );
    }
    match std::fs::remove_file(job) {
        Ok(()) => println!("{}", crate::t!("Removed {}", "消しました: {}", job.display())),
        Err(e) => println!("{}", crate::t!("Nothing to remove ({}): {e}", "消すものがありません({}): {e}", job.display())),
    }
    if !mac {
        Service::run_ctl(
            "systemctl",
            &Action::systemctl_argv("daemon-reload", &Service::unit()),
        );
    }
    println!("{}", crate::t!("Your tokens and the state directory are left in place.", "トークンと状態ディレクトリはそのままです。"));
    0
}

pub fn install(mac: bool, job: &Path, rest: &[String]) -> i32 {
    let state_dir = StateDir::resolve();
    if let Err(e) = ensure_config(&state_dir) {
        eprintln!("install: {e}");
        return 1;
    }
    let program = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(e) => {
            eprintln!("install: {}", crate::t!("couldn't find my own path: {e}", "自分の実行パスが引けません: {e}"));
            return 1;
        }
    };
    // The service's stdout/stderr also go **under logs/ in the state directory**. Putting them next to
    // the service definition (~/Library/LaunchAgents etc.) splits the places to look in two
    let log_dir = state_dir.join("logs").to_string_lossy().to_string();
    let _ = std::fs::create_dir_all(&log_dir);
    let spec = JobSpec {
        label: Service::label(),
        program,
        state_dir: state_dir.path().to_string_lossy().to_string(),
        path: std::env::var("PATH").unwrap_or_default(),
        home: Service::home().to_string_lossy().to_string(),
        log_dir,
    };
    let text = if mac {
        spec.launchd_plist()
    } else {
        spec.systemd_unit()
    };
    if let Err(e) = crate::state_dir::write_atomic_at(job, &text) {
        eprintln!("install: {}", crate::t!("couldn't write {}: {e}", "{} が書けません: {e}", job.display()));
        return 1;
    }
    // The line right after ".env is not rewritten". **Say which thing was written** — if both are
    // called "settings", it reads as writing right after saying we wouldn't
    println!("{}", crate::t!("Wrote the service definition: {}", "サービスの定義を書きました: {}", job.display()));
    let name = if mac { Service::label() } else { Service::unit() };
    println!("{}", crate::t!("  Service name: {name}", "  サービス名: {name}"));
    let state = &spec.state_dir;
    println!("{}", crate::t!("  State directory: {state}", "  状態を置くディレクトリ: {state}"));
    if mac {
        // **This definition only takes effect the next time it starts.** launchd holds on to the definition
        // from start time, so restart brings the old one back up. Explaining that every time is noisy
        // though — one line on how to apply it is enough.
        // If the caller restarts it right after, **don't say it** — the reader would type the same thing by hand
        if !rest.iter().any(|a| a == "--no-restart-hint") {
            println!("{}", crate::t!("  To apply it: agentgw shutdown && agentgw start", "  反映するには: agentgw shutdown && agentgw start"));
        }
    } else {
        Service::run_ctl(
            "systemctl",
            &Action::systemctl_argv("daemon-reload", &Service::unit()),
        );
        Service::run_ctl(
            "systemctl",
            &Action::systemctl_argv("enable", &Service::unit()),
        );
        // A --user service dies at logout. Headless use needs linger
        let user = std::env::var("USER").unwrap_or_default();
        if Service::run_ctl("loginctl", &["enable-linger".to_string(), user.clone()]) != 0 {
            println!(
                "{}",
                crate::t!(
                    "NOTE: couldn't enable linger. To keep agentgw running after you log out, run once:\n  \
                     sudo loginctl enable-linger {user}",
                    "NOTE: linger を有効にできませんでした。ログアウト後も動かすなら1回だけ:\n  \
                     sudo loginctl enable-linger {user}"
                )
            );
        }
    }
    0
}

/// Writes the gateway connection settings into `.env` and **retires the direct-connection token**.
///
/// Having someone write three values by hand is three chances to get one wrong, so everything comes
/// from one connection string. Commenting out `SLACK_APP_TOKEN` is the key — if it stays,
/// [`Mode::resolve`](crate::bridge::machine::Mode::resolve) refuses to start (rightly).
pub fn apply_connection(env_text: &str, conn: &link::Invite, bridge_id: &str) -> String {
    // Close the direct connection. **Comment it out rather than delete it** — for the day you want it back
    let folded: Vec<String> = env_text
        .lines()
        .map(|line| match line.split_once('=').map(|(k, _)| k.trim()) {
            Some("SLACK_APP_TOKEN") => format!("# (relay mode) {line}"),
            _ => line.to_string(),
        })
        .collect();
    set_env_keys(
        &folded.join("\n"),
        &[
            ("AGENTGW_RELAY_URL", conn.url.clone()),
            ("AGENTGW_RELAY_TOKEN", conn.api_token.clone()),
            ("AGENTGW_BRIDGE_ID", bridge_id.to_string()),
        ],
    )
}

/// `agentgw install` / `agentgw uninstall` — returns the process exit code.
pub fn run(cmd: &str, rest: &[String]) -> i32 {
    if !Service::supported() {
        return 2;
    }
    let mac = cfg!(target_os = "macos");
    let job = Service::job_path();
    match cmd {
        "install" => install(mac, &job, rest),
        _ => uninstall(mac, &job),
    }
}

/// `agentgw link [--name <name>] [<connection-string>|-]` — puts one connection string into `.env`.
///
/// **Reads stdin when the argument is `-`, or when there is no argument and stdin is not a terminal.**
/// Passing it in argv over ssh would leave the secret in the other side's `ps` and shell history.
///
/// `--name` is needed for exactly that stdin path — with the connection string coming through a pipe,
/// the name prompt ([`prompt_bridge_id`]) has no input. The name is not a secret, so argv is fine.
pub fn cli(args: &[String], dir: &StateDir) -> i32 {
    use std::io::{IsTerminal, Read};
    let mut named: Option<String> = None;
    let mut rest: Vec<&str> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => named = it.next().map(|s| s.trim().to_string()),
            s if s.starts_with("--name=") => {
                named = Some(s.trim_start_matches("--name=").trim().to_string());
            }
            s => rest.push(s),
        }
    }
    let named = named.filter(|s| !s.is_empty());
    let arg = rest.first().copied();
    let raw = match arg {
        Some("-") | None if !std::io::stdin().is_terminal() => {
            let mut buf = String::new();
            if std::io::stdin().read_to_string(&mut buf).is_err() {
                eprintln!("link: {}", crate::t!("couldn't read standard input", "標準入力を読めません"));
                return 1;
            }
            buf
        }
        Some(s) if s != "-" => s.to_string(),
        _ => {
            eprintln!(
                "{}",
                crate::t!(
                    "usage: agentgw link <connection string>\n       \
                     agentgw link --name <name> -   (read from standard input)",
                    "usage: agentgw link <接続文字列>\n       \
                     agentgw link --name <名前> -   (標準入力から読む)"
                )
            );
            return 2;
        }
    };
    let conn = match link::decode_connection(&raw) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("link: {e}");
            return 1;
        }
    };
    // Name given explicitly. **No automatic naming** (machines with the same name steal each other's Slack messages)
    let bridge_id = match named
        .or_else(|| std::env::var("AGENTGW_BRIDGE_ID").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(id) => id,
        None => match prompt_bridge_id() {
            Some(id) => id,
            None => {
                eprintln!(
                    "link: {}",
                    crate::t!(
                        "this machine needs a name. Pass `--name <name>` or set AGENTGW_BRIDGE_ID.",
                        "このマシンに名前が必要です。`--name <名前>` を付けるか、AGENTGW_BRIDGE_ID を設定してください。"
                    )
                );
                return 1;
            }
        },
    };
    let env_path = dir.path().join(".env");
    let before = std::fs::read_to_string(&env_path).unwrap_or_default();
    let after = apply_connection(&before, &conn, &bridge_id);
    if let Err(e) = crate::state_dir::write_atomic_mode(&env_path, &after, Some(0o600)) {
        eprintln!("link: {}", crate::t!("couldn't write {}: {e}", "{} に書けません: {e}", env_path.display()));
        return 1;
    }
    println!("{}", crate::t!("Saved: {}", "保存しました: {}", env_path.display()));
    println!("  AGENTGW_RELAY_URL={}", conn.url);
    println!("  AGENTGW_BRIDGE_ID={bridge_id}");
    if before.lines().any(|l| l.starts_with("SLACK_APP_TOKEN=")) {
        println!(
            "{}",
            crate::t!(
                "  Commented out SLACK_APP_TOKEN: this machine now goes through the gateway instead of connecting to Slack itself.",
                "  SLACK_APP_TOKEN をコメントアウトしました。このマシンは自分で Slack につながず、ゲートウェイを通します。"
            )
        );
    }
    0
}

pub(crate) fn prompt_bridge_id() -> Option<String> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let host = std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let default = if host.is_empty() {
        String::new()
    } else {
        format!(" [{host}]")
    };
    print!("{}", crate::t!("Name for this machine{default}: ", "このマシンの名前{default}: "));
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok()?;
    let typed = line.trim();
    // An empty Enter means "the suggestion shown is fine". None if there is no suggestion
    let id = if typed.is_empty() {
        host
    } else {
        typed.to_string()
    };
    (!id.is_empty()).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::machine::Mode;

    #[test]
    fn the_slack_app_link_carries_the_whole_manifest() {
        let url = SlackApp::create_url();
        assert!(url.starts_with("https://api.slack.com/apps?new_app=1&manifest_json=%7B"), "{url}");
        // Decoding gives back the same JSON as the manifest
        let encoded = url.split("manifest_json=").nth(1).unwrap();
        let decoded = percent_encoding::percent_decode_str(encoded).decode_utf8().unwrap();
        let back: serde_json::Value = serde_json::from_str(&decoded).unwrap();
        let want: serde_json::Value = serde_json::from_str(SlackApp::MANIFEST).unwrap();
        assert_eq!(back, want);
    }

    #[test]
    fn the_manifest_turns_on_what_silently_breaks_when_missing() {
        let m: serde_json::Value = serde_json::from_str(SlackApp::MANIFEST).unwrap();
        // Without it buttons never arrive
        assert_eq!(m["settings"]["interactivity"]["is_enabled"], true);
        // Without it Socket Mode can't connect
        assert_eq!(m["settings"]["socket_mode_enabled"], true);
        // Without it DMs don't work (login is typed in a DM)
        assert_eq!(m["features"]["app_home"]["messages_tab_enabled"], true);
        assert_eq!(m["features"]["app_home"]["messages_tab_read_only_enabled"], false);
    }

    #[test]
    fn the_readme_links_to_the_same_manifest() {
        // The README's one-click link must match the one built from the manifest
        let readme = include_str!("../README.md");
        assert!(
            readme.contains(&SlackApp::create_url()),
            "README のリンクが slack-app-manifest.json とずれています。\n\
             `cargo test -- --ignored print_slack_app_url --nocapture` で出る URL に差し替えてください"
        );
        let ja = include_str!("../README.ja.md");
        assert!(ja.contains(&SlackApp::create_url()), "README.ja.md も同じく");
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
        // Mixing them up really happens (both look like "a Slack token"), so catch it both ways
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
        // Existing lines are **replaced**. Appending a second one makes load_env read last-wins, and the old value survives
        assert_eq!(after.matches("SLACK_BOT_TOKEN=").count(), 1, "{after}");
        assert!(after.contains("SLACK_BOT_TOKEN=xoxb-new"), "{after}");
        assert!(after.contains("SLACK_APP_TOKEN=xapp-new"), "{after}");
        assert!(!after.contains("xoxb-old"), "{after}");
        // Unrelated lines and comments are kept — people edit .env too
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
        // If writer and reader disagree on the format it breaks silently. Check with the reader itself
        let dir = crate::state_dir::StateDir::at(
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

    // ── rewriting .env ─────────────────────────────────────────────────────

    fn conn() -> link::Invite {
        link::Invite {
            url: "wss://relay.example".into(),
            api_token: "s3cret".into(),
        }
    }

    /// Closes the direct connection **by commenting it out** and writes the three gateway settings. No other line changes.
    #[test]
    fn applying_a_connection_folds_the_direct_token_and_keeps_everything_else() {
        let before = "SLACK_BOT_TOKEN=xoxb-1\nSLACK_APP_TOKEN=xapp-1\nOTHER=keep me\n";
        let after = apply_connection(before, &conn(), "desktop");
        assert!(
            after.contains("# (relay mode) SLACK_APP_TOKEN=xapp-1"),
            "{after}"
        );
        assert!(!after.contains("\nSLACK_APP_TOKEN="), "{after}");
        assert!(after.contains("SLACK_BOT_TOKEN=xoxb-1"), "{after}");
        assert!(after.contains("OTHER=keep me"), "{after}");
        assert!(
            after.contains("AGENTGW_RELAY_URL=wss://relay.example"),
            "{after}"
        );
        assert!(after.contains("AGENTGW_RELAY_TOKEN=s3cret"), "{after}");
        assert!(after.contains("AGENTGW_BRIDGE_ID=desktop"), "{after}");
    }

    /// Pasting again **overwrites**; it doesn't add lines.
    #[test]
    fn re_applying_replaces_rather_than_appends() {
        let once = apply_connection("", &conn(), "desktop");
        let twice = apply_connection(
            &once,
            &link::Invite {
                url: "wss://new".into(),
                api_token: "new".into(),
            },
            "laptop",
        );
        assert_eq!(twice.matches("AGENTGW_RELAY_URL=").count(), 1, "{twice}");
        assert!(twice.contains("AGENTGW_RELAY_URL=wss://new"), "{twice}");
        assert!(twice.contains("AGENTGW_BRIDGE_ID=laptop"), "{twice}");
    }

    /// The written .env reads back as the via-gateway mode (`Mode::Relay`) (round trip).
    #[test]
    fn what_it_writes_is_what_resolve_reads() {
        let text = apply_connection("SLACK_APP_TOKEN=xapp-1\n", &conn(), "desktop");
        let pairs: Vec<(String, String)> = text
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| {
                l.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect();
        let got = Mode::resolve(move |k: &str| {
            pairs.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone())
        })
        .unwrap();
        assert_eq!(
            got,
            Mode::Relay {
                url: "wss://relay.example".into(),
                api_token: "s3cret".into(),
                bridge_id: "desktop".into()
            }
        );
    }

}
