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
    if !matches!(cmd.as_str(), "--version" | "-V" | "version" | "") {
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
                agentgw::bridge::gateway::Cli::print_fleet(&dir).await;
            }
            std::process::exit(rc);
        }
        // The version **stands on its own**. Mixed into the usage, the one line you need
        // to check the version on a machine becomes a wall of usage (and exits with code 2)
        "--version" | "-V" | "version" => {
            println!("agentgw {}", env!("CARGO_PKG_VERSION"));
        }
        _ => {
            // A line users read. **Don't show internals (implementation language, issue
            // numbers)** — someone who mistyped needs only the right usage. Names match the real commands
            let v = env!("CARGO_PKG_VERSION");
            eprintln!(
                "{}",
                agentgw::t!(
                    "agentgw {v}\n\
                     \n\
                     First:      install  set up this machine (as the gateway or as a machine) and start it\n\
                     Day to day: status | restart | shutdown | start | uninstall\n\
                     \n\
                     Add a machine (run on the gateway):\n  \
                     add-machine user@host   install agentgw on another machine and connect it here\n\
                     \n\
                     `serve` is what the service runs; you don't need to type it.",
                    "agentgw {v}\n\
                     \n\
                     まず:   install            このマシンを設定して起動する(ゲートウェイかマシンかを訊きます)\n\
                     ふだん: status | restart | shutdown | start | uninstall\n\
                     \n\
                     マシンを足すとき(ゲートウェイで):\n  \
                     add-machine user@host   ほかのマシンに agentgw を入れて、ここにつなぐ\n\
                     \n\
                     serve はサービスが呼ぶ本体です(手で打つ必要はありません)。"
                )
            );
            std::process::exit(2);
        }
    }
}
