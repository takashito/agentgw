use agentgw::bridge::Bridge;
use agentgw::log::LogCtx;
use agentgw::service::Service;

/// **If we got hold of another implementation's bot state directory, stop without doing anything.**
///
/// Forgetting `AGENTGW_STATE_DIR` falls back to the default `~/.local/state/agentgw`.
/// On a machine where that directory belongs to a different bot implementation, `install`
/// would write its plist and `link` its `.env` (both actually happened on 2026-08-02). This
/// implementation never creates a UDS, so a `bridge.sock` means the other side is the Bun
/// version. **Bail out before reading or writing anything.**
fn refuse_other_bots_state_dir() {
    let dir = agentgw::state_dir::StateDir::resolve();
    if !dir.path().join("bridge.sock").exists() {
        return;
    }
    let dir = dir.path().display();
    eprintln!(
        "{}",
        agentgw::t!(
            "{dir} belongs to a different bot (it has a bridge.sock).\n\
             A second process on the same Slack app makes Slack split events between the two,\n\
             so both bots would half-work. Nothing was done.\n\n\
             Point agentgw at its own state directory and try again:\n  \
             AGENTGW_STATE_DIR=$HOME/.local/state/agentgw-dev agentgw …",
            "{dir} は別の実装のボットの置き場です(bridge.sock があります)。\n\
             同じ Slack app に2本目が繋がると、Slack はイベントを複製せず半分ずつ振り分けるので、\n\
             どちらのボットも動いたり動かなかったりします。何もしていません。\n\n\
             置き場を指定してやり直してください:\n  \
             AGENTGW_STATE_DIR=$HOME/.local/state/agentgw-dev agentgw …"
        )
    );
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    // What `serve` says goes to Slack, where every machine speaks the same language. Decided
    // before anything is said: the language is fixed once per process
    if cmd == "serve" {
        agentgw::i18n::english_only();
    }
    // `--help` and unknown options stop here, before the subcommand runs (or reads the state directory)
    let args: Vec<String> = std::env::args().skip(2).collect();
    match agentgw::cli::check(&cmd, &args) {
        agentgw::cli::Check::Run => {}
        agentgw::cli::Check::Help(text) => {
            println!("{text}");
            return;
        }
        agentgw::cli::Check::Refuse(text) => {
            eprintln!("{text}");
            std::process::exit(2);
        }
    }
    let asked_for_help = matches!(cmd.as_str(), "--help" | "-h" | "help");
    if !matches!(cmd.as_str(), "--version" | "-V" | "version" | "--build" | "") && !asked_for_help {
        refuse_other_bots_state_dir();
    }
    match cmd.as_str() {
        "serve" => {
            if let Err(e) = Bridge::run().await {
                LogCtx::default().error("bridge", &format!("serve failed: {e}"));
                eprintln!("serve failed: {e}");
                std::process::exit(1);
            }
        }
        // What the Windows scheduled task runs: keeps one `serve` going (launchd / systemd do this elsewhere)
        "supervise" => {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            // The task passes the state directory as an argument (a task action cannot set
            // variables); supervise hands it to serve as AGENTGW_STATE_DIR
            let dir = match rest.iter().position(|a| a == "--state-dir").and_then(|i| rest.get(i + 1)) {
                Some(d) => agentgw::state_dir::StateDir::at(d),
                None => agentgw::state_dir::StateDir::resolve(),
            };
            std::process::exit(agentgw::service::windows::supervise(&dir));
        }
        // Write one connection string into .env (on the machine that connects to the gateway)
        "link" => {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            let dir = agentgw::state_dir::StateDir::resolve();
            std::process::exit(agentgw::setup::cli(&rest, &dir));
        }
        // Add one machine (deliver -> connect -> install and start)
        "add-machine" => {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            std::process::exit(agentgw::setup::add_machine::cli(&rest).await);
        }
        // Ask the running gateway to update itself and then every machine
        "update" => {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            let dir = agentgw::state_dir::StateDir::resolve();
            std::process::exit(agentgw::bridge::gateway::Cli::update(&dir, &rest).await);
        }
        // Change which machine handles a channel, or forget a machine (run on the gateway)
        "route" | "unroute" | "remove-machine" => {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            let dir = agentgw::state_dir::StateDir::resolve();
            std::process::exit(agentgw::bridge::gateway::admin::cli(&dir, &cmd, &rest).await);
        }
        "install" | "uninstall" => {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            std::process::exit(agentgw::setup::run(&cmd, &rest));
        }
        c if Service::COMMANDS.contains(&c) => {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            let rc = Service::run(c, &rest);
            // Only when this host is set up to accept machines, follow with the fleet status
            if c == "status" {
                let dir = agentgw::state_dir::StateDir::resolve();
                agentgw::setup::migrate_env(&dir);
                let env = dir.load_env().unwrap_or_default().into_iter().collect();
                let access = agentgw::bridge::state::Access::load(&dir);
                println!("\n{}", agentgw::bridge::machine::role_line(&env, &access));
                if let Some(line) = agentgw::owner_code::status_line(&dir, &env, &access) {
                    println!("{line}");
                }
                agentgw::bridge::gateway::Cli::print_fleet(&dir).await;
            }
            std::process::exit(rc);
        }
        // The version **stands on its own**. Mixed into the usage, the one line you need
        // to check the version on a machine becomes a wall of usage (and exits with code 2)
        "--version" | "-V" | "version" => {
            println!("agentgw {}", env!("CARGO_PKG_VERSION"));
        }
        // Which build: the version and the commit. Kept apart from --version, which older machines
        // compare word for word
        "--build" => {
            println!("agentgw {}", agentgw::build_label::LABEL);
        }
        _ => {
            // A line users read. **Don't show internals (implementation language, issue
            // numbers)** — someone who mistyped needs only the right usage. Names match the real commands
            let v = env!("CARGO_PKG_VERSION");
            let text = agentgw::t!(
                    "agentgw {v}\n\
                     \n\
                     First:      install  set up this machine (as the gateway or as a machine) and start it\n\
                     Day to day: status | restart | shutdown | start | uninstall\n\
                     Update:    update [version]   the gateway, then every machine one at a time\n\
                     \n\
                     Machines and channels (run on the gateway):\n  \
                     add-machine user@host   install agentgw on another machine and connect it here\n  \
                     route <channel> <machine>[:<folder>]   hand a channel to a machine\n  \
                     unroute <channel>       take a channel's assignment away\n  \
                     remove-machine <name>   forget a machine that is gone\n  \
                     (these three show what would change; add --yes to do it)\n\
                     \n\
                     `serve` is what the service runs; you don't need to type it.\n\
                     \n\
                     agentgw <command> --help shows how to use each one.",
                    "agentgw {v}\n\
                     \n\
                     まず:   install            このマシンを設定して起動する(ゲートウェイかマシンかを訊きます)\n\
                     ふだん: status | restart | shutdown | start | uninstall\n\
                     上げる: update [版]         ゲートウェイ、次に各マシンを1台ずつ\n\
                     \n\
                     マシンとチャンネル(ゲートウェイで):\n  \
                     add-machine user@host   ほかのマシンに agentgw を入れて、ここにつなぐ\n  \
                     route <チャンネル> <マシン>[:<フォルダ>]   チャンネルをマシンに任せる\n  \
                     unroute <チャンネル>    チャンネルの割り当てを外す\n  \
                     remove-machine <名前>   もう無いマシンを外す\n  \
                     (この3つは何が変わるかを出すだけ。--yes で実行)\n\
                     \n\
                     serve はサービスが呼ぶ本体です(手で打つ必要はありません)。\n\
                     \n\
                     agentgw <command> --help で、それぞれの使い方が出ます。"
            );
            // Asked for: the usage is the answer. Mistyped: it's an error
            if asked_for_help {
                println!("{text}");
            } else {
                eprintln!("{text}");
                std::process::exit(2);
            }
        }
    }
}
