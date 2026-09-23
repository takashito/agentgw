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
    remember_gateway(state_dir, &conn);
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
    write_role(state_dir, "gateway");
    Ok(())
}

/// Write down what this Bridge is. **The role is written, not guessed**: which keys happen to be
/// present used to decide it, so a half-finished `.env` read as a different role instead of as an
/// error. Where machines are accepted isn't written at all — that follows from their records.
fn write_role(state_dir: &StateDir, role: &str) {
    let path = state_dir.join(".env");
    let before = std::fs::read_to_string(&path).unwrap_or_default();
    let after = crate::state_dir::set_env_keys(&before, &[("AGENTGW_BRIDGE_ROLE", role.to_string())]);
    if let Err(e) = crate::state_dir::write_atomic_mode(&path, &after, Some(0o600)) {
        eprintln!("{}: {e}", path.display());
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

/// Writes what `.env` keeps of a connection string: **the role, the name and the key**. Where the
/// gateway is goes to access.json instead — `.env` keeps what has to be there before anything is
/// read, and the secrets, because it is the file with the tight mode.
///
/// Having someone write three values by hand is three chances to get one wrong, so everything comes
/// from one connection string. Commenting out `SLACK_APP_TOKEN` is part of it — this machine goes
/// through the gateway now, and two consumers of one Slack app split the messages between them.
pub fn apply_connection(env_text: &str, conn: &link::Invite, bridge_id: &str) -> String {
    // Close the direct connection. **Comment it out rather than delete it** — for the day you want it back
    let folded: Vec<String> = env_text
        .lines()
        .map(|line| match line.split_once('=').map(|(k, _)| k.trim()) {
            Some("SLACK_APP_TOKEN") => format!("# (machine mode) {line}"),
            _ => line.to_string(),
        })
        .collect();
    set_env_keys(
        &folded.join("\n"),
        &[
            ("AGENTGW_BRIDGE_ROLE", "machine".to_string()),
            ("AGENTGW_LINK_TOKEN", conn.api_token.clone()),
            ("AGENTGW_BRIDGE_ID", bridge_id.to_string()),
        ],
    )
}

/// The other half of a connection string: **where the gateway is**, written to access.json.
pub fn remember_gateway(dir: &StateDir, conn: &link::Invite) {
    let mut access = crate::bridge::state::Access::load(dir);
    let was = access.gateway.take().unwrap_or_default();
    access.gateway = Some(crate::bridge::state::Link {
        kind: link_kind(&conn.url).to_string(),
        link_url: conn.url.clone(),
        ..was
    });
    if let Err(e) = access.save(dir) {
        eprintln!("{}", crate::t!("Couldn't write access.json: {e}", "access.json が書けません: {e}"));
    }
}

/// Which kind of route a dial URL describes. **Loopback means the ssh tunnel** — nothing else asks a
/// machine to dial itself.
pub fn link_kind(url: &str) -> &'static str {
    use crate::bridge::state::Link;
    let host = url
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
        .map(|hp| hp.rsplit_once(':').map(|(h, _)| h).unwrap_or(hp))
        .unwrap_or_default();
    if host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()) || host == "localhost" {
        return Link::TUNNEL;
    }
    const TAILNET: &str = ".ts.net"; // Tailscale's own domain, as in hub.example.ts.net
    match host.ends_with(TAILNET) {
        true => Link::TAILSCALE,
        false => Link::LAN,
    }
}

/// Fold an older `.env` into the shape this version reads. **Runs once, before anything is read**,
/// and does nothing when it has already run.
///
/// How a machine connects used to live in three keys — `AGENTGW_TUNNELS`, `AGENTGW_ROUTES` and the
/// addresses in `AGENTGW_LINK_LISTEN` — and one fact in three places is a fact that can disagree with
/// itself. It is one record per machine in access.json now, and `.env` keeps only what has to be read
/// before that file can be: the role, the name, the port and the secrets.
pub fn migrate_env(dir: &StateDir) {
    use crate::bridge::state::{Access, Link};
    let env: std::collections::HashMap<String, String> =
        dir.load_env().unwrap_or_default().into_iter().collect();
    let v = |k: &str| env.get(k).map(|s| s.trim()).filter(|s| !s.is_empty());
    // Already in the new shape
    if v("AGENTGW_BRIDGE_ROLE").is_some() {
        return;
    }
    let (listen, relay_url) = (v("AGENTGW_LINK_LISTEN"), v("AGENTGW_RELAY_URL"));
    let role = match (listen, relay_url) {
        (Some(_), _) => "gateway",
        (None, Some(_)) => "machine",
        // Nothing to fold: a standalone Bridge, which is a gateway with no machines
        (None, None) => "gateway",
    };
    let port = listen
        .and_then(|l| crate::bridge::gateway::first_addr(l).rsplit(':').next())
        .unwrap_or(crate::bridge::gateway::DEFAULT_PORT)
        .to_string();

    let mut access = Access::load(dir);
    if role == "machine" {
        if let Some(url) = relay_url {
            access.gateway = Some(Link {
                kind: link_kind(url).to_string(),
                link_url: url.to_string(),
                ..Default::default()
            });
        }
    } else {
        // A tunnel is named by its ssh target; the machine at its end dials its own loopback
        for (id, target) in crate::bridge::machine::child_urls(v("AGENTGW_TUNNELS").unwrap_or("")) {
            access.machines.insert(
                id,
                Link {
                    kind: Link::TUNNEL.to_string(),
                    link_url: format!("ws://127.0.0.1:{}", crate::bridge::gateway::TUNNEL_PORT),
                    ssh_target: Some(target),
                    ..Default::default()
                },
            );
        }
        for (id, url) in crate::bridge::machine::child_urls(v("AGENTGW_ROUTES").unwrap_or("")) {
            let kind = match Some(url.as_str()) == v("AGENTGW_LINK_PUBLIC_URL") {
                true => Link::PUBLIC,
                false => link_kind(&url),
            };
            access.machines.insert(
                id,
                Link {
                    kind: kind.to_string(),
                    link_url: url,
                    ..Default::default()
                },
            );
        }
    }
    if let Err(e) = access.save(dir) {
        eprintln!("{}", crate::t!("Couldn't write access.json: {e}", "access.json が書けません: {e}"));
        return;
    }

    // The machine's key had its own name; there is one key now, whichever side you are
    let mut set: Vec<(&str, String)> = vec![("AGENTGW_BRIDGE_ROLE", role.to_string())];
    if port != crate::bridge::gateway::DEFAULT_PORT {
        set.push(("AGENTGW_BRIDGE_PORT", port));
    }
    if v("AGENTGW_LINK_TOKEN").is_none()
        && let Some(tok) = v("AGENTGW_RELAY_TOKEN")
    {
        set.push(("AGENTGW_LINK_TOKEN", tok.to_string()));
    }
    let path = dir.join(".env");
    let before = std::fs::read_to_string(&path).unwrap_or_default();
    let kept: String = before
        .lines()
        .filter(|l| {
            !matches!(
                l.split_once('=').map(|(k, _)| k.trim()),
                Some("AGENTGW_LINK_LISTEN")
                    | Some("AGENTGW_TUNNELS")
                    | Some("AGENTGW_ROUTES")
                    | Some("AGENTGW_RELAY_URL")
                    | Some("AGENTGW_RELAY_TOKEN")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let after = crate::state_dir::set_env_keys(&kept, &set);
    if let Err(e) = crate::state_dir::write_atomic_mode(&path, &after, Some(0o600)) {
        eprintln!("{}: {e}", path.display());
    }
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
    remember_gateway(dir, &conn);
    println!("{}", crate::t!("Saved: {}", "保存しました: {}", env_path.display()));
    println!("  gateway={}", conn.url);
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
            after.contains("# (machine mode) SLACK_APP_TOKEN=xapp-1"),
            "{after}"
        );
        assert!(!after.contains("\nSLACK_APP_TOKEN="), "{after}");
        assert!(after.contains("SLACK_BOT_TOKEN=xoxb-1"), "{after}");
        assert!(after.contains("OTHER=keep me"), "{after}");
        assert!(after.contains("AGENTGW_BRIDGE_ROLE=machine"), "{after}");
        assert!(after.contains("AGENTGW_LINK_TOKEN=s3cret"), "{after}");
        assert!(after.contains("AGENTGW_BRIDGE_ID=desktop"), "{after}");
        // **Where the gateway is doesn't go in .env** — that is access.json's
        assert!(!after.contains("relay.example"), "{after}");
    }

    /// The fold from the three old keys. **The one thing that must not be wrong** — get it wrong and
    /// the gateway opens different addresses than the machines dial.
    #[test]
    fn an_older_env_folds_into_one_record_per_machine() {
        use crate::bridge::state::{Access, Link};
        let dir = StateDir::at(std::env::temp_dir().join(format!(
            "agentgw-migrate-{}-{}",
            std::process::id(),
            line!()
        )));
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            dir.join(".env"),
            "SLACK_APP_TOKEN=xapp-1\n             AGENTGW_BRIDGE_ID=hub\n             AGENTGW_LINK_TOKEN=k\n             AGENTGW_LINK_LISTEN=127.0.0.1:8787,192.0.2.10:8787\n             AGENTGW_TUNNELS=mac=me@mac\n             AGENTGW_ROUTES=pve=ws://hub.lan:8787,vps=wss://hub.example.ts.net\n             OTHER=keep me\n",
        )
        .unwrap();

        migrate_env(&dir);
        let access = Access::load(&dir);
        assert_eq!(access.machines["pve"].kind, Link::LAN);
        assert_eq!(access.machines["pve"].link_url, "ws://hub.lan:8787");
        assert_eq!(access.machines["vps"].kind, Link::TAILSCALE);
        assert_eq!(access.machines["mac"].kind, Link::TUNNEL);
        assert_eq!(access.machines["mac"].ssh_target.as_deref(), Some("me@mac"));
        // The machine at the far end of a tunnel dials its own loopback
        assert_eq!(access.machines["mac"].link_url, "ws://127.0.0.1:8799");

        let env: std::collections::HashMap<String, String> =
            dir.load_env().unwrap().into_iter().collect();
        assert_eq!(env.get("AGENTGW_BRIDGE_ROLE").map(String::as_str), Some("gateway"));
        // 8787 is the default, so it isn't written out
        assert!(!env.contains_key("AGENTGW_BRIDGE_PORT"));
        assert!(!env.contains_key("AGENTGW_LINK_LISTEN"));
        assert!(!env.contains_key("AGENTGW_TUNNELS"));
        assert!(!env.contains_key("AGENTGW_ROUTES"));
        // Secrets and anything else stay exactly where they were
        assert_eq!(env.get("AGENTGW_LINK_TOKEN").map(String::as_str), Some("k"));
        assert_eq!(env.get("SLACK_APP_TOKEN").map(String::as_str), Some("xapp-1"));
        assert_eq!(env.get("OTHER").map(String::as_str), Some("keep me"));

        // **Running again changes nothing** — it is the role being written that says it is done
        migrate_env(&dir);
        assert_eq!(Access::load(&dir).machines.len(), 3);

        // The addresses this opens are the ones the machines dial
        let addrs = crate::bridge::machine::listen_addrs("8787", &access, |n| {
            (n == "hub.lan").then(|| "192.0.2.10".to_string())
        });
        assert_eq!(addrs, vec!["127.0.0.1:8787", "192.0.2.10:8787"]);
        std::fs::remove_dir_all(dir.path()).ok();
    }

    /// A machine's side: where its gateway is moves out of `.env`, and its key loses the separate name.
    #[test]
    fn an_older_machine_env_folds_the_same_way() {
        use crate::bridge::state::{Access, Link};
        let dir = StateDir::at(std::env::temp_dir().join(format!(
            "agentgw-migrate-{}-{}",
            std::process::id(),
            line!()
        )));
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            dir.join(".env"),
            "AGENTGW_BRIDGE_ID=pve\n             AGENTGW_RELAY_URL=wss://hub.example.ts.net\n             AGENTGW_RELAY_TOKEN=k\n",
        )
        .unwrap();

        migrate_env(&dir);
        let access = Access::load(&dir);
        let gw = access.gateway.unwrap();
        assert_eq!(gw.link_url, "wss://hub.example.ts.net");
        assert_eq!(gw.kind, Link::TAILSCALE);
        let env: std::collections::HashMap<String, String> =
            dir.load_env().unwrap().into_iter().collect();
        assert_eq!(env.get("AGENTGW_BRIDGE_ROLE").map(String::as_str), Some("machine"));
        assert_eq!(env.get("AGENTGW_LINK_TOKEN").map(String::as_str), Some("k"));
        assert!(!env.contains_key("AGENTGW_RELAY_URL"));
        assert!(!env.contains_key("AGENTGW_RELAY_TOKEN"));
        std::fs::remove_dir_all(dir.path()).ok();
    }

    /// The kind of route is read off the dial URL. **Loopback means the tunnel** — nothing else asks a
    /// machine to dial itself.
    #[test]
    fn the_kind_of_route_is_read_off_the_url() {
        use crate::bridge::state::Link;
        assert_eq!(link_kind("ws://127.0.0.1:8799"), Link::TUNNEL);
        assert_eq!(link_kind("ws://localhost:8799"), Link::TUNNEL);
        assert_eq!(link_kind("wss://hub.example.ts.net"), Link::TAILSCALE);
        assert_eq!(link_kind("ws://hub.example.ts.net:8787"), Link::TAILSCALE);
        assert_eq!(link_kind("ws://hub.lan:8787"), Link::LAN);
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
        assert_eq!(twice.matches("AGENTGW_LINK_TOKEN=").count(), 1, "{twice}");
        assert!(twice.contains("AGENTGW_LINK_TOKEN=new"), "{twice}");
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
        let access = crate::bridge::state::Access {
            gateway: Some(crate::bridge::state::Link {
                link_url: "wss://relay.example".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let got = Mode::resolve(
            move |k: &str| pairs.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone()),
            &access,
        )
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
