//! Setup: everything that prepares a machine before `serve` runs — `install` / `uninstall`
//! (the role question, Slack tokens, the service definition), `link` (a connection string into
//! `.env`), and `add-machine` (install and connect another machine from the gateway).

pub mod add_machine;
pub mod ssh;

use crate::bridge::gateway::wire;
use crate::bridge::state::StateDir;
use crate::service::{Action, JobSpec, Service};
use std::io::Write;
use std::path::Path;

/// bot/app トークンの組。
pub struct Tokens {
    pub bot: String,
    pub app: String,
}

impl Tokens {
    /// 取り違えは実際に起きる(どちらも「Slack のトークン」に見える)。接頭辞で両方向を弾く。
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

/// agentgw が要る Slack アプリの設定(マニフェスト)と、それを入れた作成リンク。
///
/// マニフェストはリポジトリ直下の `slack-app-manifest.json` が唯一の正。README の
/// ワンクリックのリンクも同じものから作る(ずれたらテストで落ちる)。
pub struct SlackApp;

impl SlackApp {
    pub const MANIFEST: &'static str = include_str!("../slack-app-manifest.json");

    /// `https://api.slack.com/apps?new_app=1&manifest_json=…` — 開くと Slack の「マニフェストから
    /// 作る」画面が、この設定を入れた状態で出る。
    pub fn create_url() -> String {
        // 空白と改行を落としてから符号化する(URL を短くするため)
        let compact = serde_json::from_str::<serde_json::Value>(Self::MANIFEST)
            .map(|v| v.to_string())
            .unwrap_or_default();
        format!(
            "https://api.slack.com/apps?new_app=1&manifest_json={}",
            percent_encoding::utf8_percent_encode(&compact, percent_encoding::NON_ALPHANUMERIC)
        )
    }

    /// 開けるならブラウザで開く。開けなくても(ssh 越し、画面の無い Linux)何もしない —
    /// URL は既に印字してある。
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
/// 役割をここで訊くのが要だ。マシンは Slack のトークンを持たない(持てない — 直結と
/// ゲートウェイ経由は同時に有効にできない)ので、トークンだけを求めるとマシンは install を
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
    // マシンは2通り(自分から dial / ゲートウェイに迎えに来てもらう)。どちらも Slack トークンは持たない
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

/// マシンとして設定する — ゲートウェイが出した接続文字列1本と、このマシンの名前。
fn ask_child(state_dir: &StateDir) -> Result<(), String> {
    let raw = prompt(&crate::t!(
        "  Connection string from the gateway (SCLINK1-…): ",
        "  ゲートウェイから受け取った接続文字列 (SCLINK1-…): "
    ));
    let conn = crate::bridge::gateway::wire::decode_connection(&raw)?;
    // 名前は自動で決めない(衝突したマシンは互いの Slack メッセージを奪い合う)
    let name = prompt_bridge_id()
        .ok_or_else(|| {
            crate::t!(
                "This machine needs a name (it's what `route <name>` points at)",
                "このマシンの名前が要ります(`route <名前>` の指名先)"
            )
        })?;
    let env_file = state_dir.join(".env");
    let before = std::fs::read_to_string(&env_file).unwrap_or_default();
    let after = apply_connection(&before, &conn, &name);
    std::fs::create_dir_all(state_dir.path())
        .map_err(|e| format!("{}: {e}", state_dir.path().display()))?;
    crate::bridge::state::write_atomic_mode(&env_file, &after, Some(0o600))
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

/// ゲートウェイとして設定する — Slack の bot / app トークン2本。
fn ask_parent(state_dir: &StateDir) -> Result<(), String> {
    let env_file = state_dir.join(".env");
    let text = std::fs::read_to_string(&env_file).unwrap_or_default();
    // **アプリがまだ無い人のために、設定入りの作成画面を開く。** 権限・イベント・
    // Socket Mode・Interactivity を手で選ばせると、1つ抜けるだけで黙って動かない
    // (Interactivity を忘れるとボタンが届かない、DM タブを忘れると login できない)
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
    println!(
        "{}",
        crate::t!(
            "Saved the tokens: {} (chmod 600)",
            "トークンを保存しました: {} (chmod 600)",
            env_file.display()
        )
    );
    Ok(())
}

pub fn uninstall(mac: bool, job: &Path) -> i32 {
    // 止めてから定義を消す。止まっている相手への bootout は非ゼロを返すが、
    // 欲しいのは「消えていること」なので終了コードは見ない
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
    // サービスの標準出力・エラーも**状態の置き場の logs/ に**置く。サービス定義の隣
    // (~/Library/LaunchAgents など)に出すと、調べる人が探す場所が2つに割れる
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
    if let Err(e) = crate::bridge::state::write_atomic_at(job, &text) {
        eprintln!("install: {}", crate::t!("couldn't write {}: {e}", "{} が書けません: {e}", job.display()));
        return 1;
    }
    // 「.env は書き換えません」の直後に来る行。**何を書いたのかを言い分ける** —
    // どちらも「設定」と呼ぶと、書かないと言った直後に書いたことになって読めない
    println!("{}", crate::t!("Wrote the service definition: {}", "サービスの定義を書きました: {}", job.display()));
    let name = if mac { Service::label() } else { Service::unit() };
    println!("{}", crate::t!("  Service name: {name}", "  サービス名: {name}"));
    let state = &spec.state_dir;
    println!("{}", crate::t!("  State directory: {state}", "  状態を置くディレクトリ: {state}"));
    if mac {
        // **この定義が今すぐ効くのは、次に起動したときだけ。** launchd は起動時の定義を
        // 握ったままなので restart では古い方が起き直る。ただしそれを毎回説くのは
        // うるさい — 効かせ方は1行で足りる。
        // 呼び手が直後に起こし直すなら**言わない** — 読んだ人が同じことを手で打つ
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
        // --user のサービスはログアウトで死ぬ。headless で使うなら linger が要る
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

/// `.env` にゲートウェイへつなぐ設定を書き、**直結のトークンを畳む**。
///
/// 3つを手で書かせるのは、間違いを作る機会が3回あるということ。接続文字列1本から起こす。
/// `SLACK_APP_TOKEN` をコメントアウトするのが要 — 残っていると [`Mode::resolve`](crate::bridge::link::Mode::resolve) が
/// 起動を拒否する(そしてそれは正しい)。
pub fn apply_connection(env_text: &str, conn: &wire::Invite, bridge_id: &str) -> String {
    // 直結の口を閉じる。**消さずにコメントにする** — 戻したくなる日のために
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

/// `.env` のキーを**置換で**書き入れる(無ければ末尾に足す)。他の行は1文字も触らない。
///
/// 積み増しにすると `load_env` は後勝ちで読むので、消したはずの値が残り続ける。
pub fn set_env_keys(env_text: &str, pairs: &[(&str, String)]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut seen = vec![false; pairs.len()];
    for line in env_text.lines() {
        let key = line.split_once('=').map(|(k, _)| k.trim());
        match pairs.iter().position(|(k, _)| Some(*k) == key) {
            Some(i) => {
                seen[i] = true;
                out.push(format!("{}={}", pairs[i].0, pairs[i].1));
            }
            None => out.push(line.to_string()),
        }
    }
    for (i, (k, v)) in pairs.iter().enumerate() {
        if !seen[i] {
            out.push(format!("{k}={v}"));
        }
    }
    let mut text = out.join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// `agentgw link [--name <名前>] [<接続文字列>|-]` — 接続文字列1本を `.env` に落とす。
///
/// **引数が `-`、または引数が無く stdin が端末でないときは stdin から読む。** ssh 越しに
/// 渡すとき argv に置くと、相手の `ps` とシェル履歴に秘密が残る。
///
/// `--name` が要るのは、その stdin から読む経路だ — 接続文字列がパイプで来ていると、
/// 名前を訊く口([`prompt_bridge_id`])が塞がっている。名前は秘密ではないので argv でよい。
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
    let conn = match wire::decode_connection(&raw) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("link: {e}");
            return 1;
        }
    };
    // 名前は明示。**自動命名はしない**(衝突したマシンは互いの Slack メッセージを奪い合う)
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
    if let Err(e) = crate::bridge::state::write_atomic_mode(&env_path, &after, Some(0o600)) {
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
    // 空 Enter は「表示した候補でよい」の意思表示。何も候補が無ければ None
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
    use crate::bridge::link::Mode;

    #[test]
    fn the_slack_app_link_carries_the_whole_manifest() {
        let url = SlackApp::create_url();
        assert!(url.starts_with("https://api.slack.com/apps?new_app=1&manifest_json=%7B"), "{url}");
        // 符号化を戻すと、マニフェストと同じ JSON になる
        let encoded = url.split("manifest_json=").nth(1).unwrap();
        let decoded = percent_encoding::percent_decode_str(encoded).decode_utf8().unwrap();
        let back: serde_json::Value = serde_json::from_str(&decoded).unwrap();
        let want: serde_json::Value = serde_json::from_str(SlackApp::MANIFEST).unwrap();
        assert_eq!(back, want);
    }

    #[test]
    fn the_manifest_turns_on_what_silently_breaks_when_missing() {
        let m: serde_json::Value = serde_json::from_str(SlackApp::MANIFEST).unwrap();
        // 無いとボタンが届かない
        assert_eq!(m["settings"]["interactivity"]["is_enabled"], true);
        // 無いと Socket Mode でつながらない
        assert_eq!(m["settings"]["socket_mode_enabled"], true);
        // 無いと DM できない(login は DM で打つ)
        assert_eq!(m["features"]["app_home"]["messages_tab_enabled"], true);
        assert_eq!(m["features"]["app_home"]["messages_tab_read_only_enabled"], false);
    }

    #[test]
    fn the_readme_links_to_the_same_manifest() {
        // README のワンクリックのリンクは、マニフェストから作ったものと同じでなければならない
        let readme = include_str!("../README.md");
        assert!(
            readme.contains(&SlackApp::create_url()),
            "README のリンクが slack-app-manifest.json とずれています。\n\
             `cargo test -- --ignored print_slack_app_url --nocapture` で出る URL に差し替えてください"
        );
        let ja = include_str!("../README.ja.md");
        assert!(ja.contains(&SlackApp::create_url()), "README.ja.md も同じく");
    }

    /// README に貼る URL を印字する(`cargo test -- --ignored print_slack_app_url --nocapture`)。
    #[test]
    #[ignore = "README に貼る URL を出すだけ"]
    fn print_slack_app_url() {
        println!("{}", SlackApp::create_url());
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

    // ── .env の書き換え ─────────────────────────────────────────────────────

    fn conn() -> wire::Invite {
        wire::Invite {
            url: "wss://relay.example".into(),
            api_token: "s3cret".into(),
        }
    }

    /// 直結の口を**コメントにして**閉じ、ゲートウェイへつなぐ3つを書く。他の行は1文字も変えない。
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

    /// 貼り直しは**上書き**で、行を増やさない。
    #[test]
    fn re_applying_replaces_rather_than_appends() {
        let once = apply_connection("", &conn(), "desktop");
        let twice = apply_connection(
            &once,
            &wire::Invite {
                url: "wss://new".into(),
                api_token: "new".into(),
            },
            "laptop",
        );
        assert_eq!(twice.matches("AGENTGW_RELAY_URL=").count(), 1, "{twice}");
        assert!(twice.contains("AGENTGW_RELAY_URL=wss://new"), "{twice}");
        assert!(twice.contains("AGENTGW_BRIDGE_ID=laptop"), "{twice}");
    }

    /// 書いた .env が、そのままゲートウェイ経由のモード(`Mode::Relay`)として読めること(往復)。
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
