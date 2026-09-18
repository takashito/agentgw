use agentgw::bridge::Bridge;
use agentgw::bridge::state::LogCtx;
use agentgw::service::Service;

/// **別の実装のボットの置き場を掴んだら、何もせずに止まる。**
///
/// `AGENTGW_STATE_DIR` を付け忘れると既定の `~/.local/state/agentgw` に落ちる。
/// そこが別実装のボットの置き場になっているマシンでは、`install` が plist を、`link` が
/// `.env` を書いてしまう(2026-08-02 に実際に両方起きた)。この実装は UDS を作らないので、
/// `bridge.sock` があれば相手は Bun 版だと分かる。**読む前・書く前に断つ。**
fn refuse_other_bots_state_dir() {
    let dir = agentgw::bridge::state::StateDir::resolve();
    if !dir.path().join("bridge.sock").exists() {
        return;
    }
    eprintln!(
        "{} は別の実装のボットの置き場です(bridge.sock があります)。\n\
         同じ Slack app に2本目が繋がると、Slack はイベントを複製せず半分ずつ振り分けるので、\n\
         どちらのボットも動いたり動かなかったりします。何もしていません。\n\n\
         置き場を指定してやり直してください:\n  \
         AGENTGW_STATE_DIR=$HOME/.local/state/slack-channel-rs-dev agentgw …",
        dir.path().display()
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
        // 接続文字列1本を .env に落とす(親にぶら下がる側)
        "link" => {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            let dir = agentgw::bridge::state::StateDir::resolve();
            std::process::exit(agentgw::bridge::link::cli(&rest, &dir));
        }
        // 子を1台足す(届ける → 繋ぐ → 入れて起こす)
        "add-child" => {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            std::process::exit(agentgw::fleet::cli(&rest).await);
        }
        c if Service::COMMANDS.contains(&c) => {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            let rc = Service::run(c, &rest);
            // 子を迎える設定があるときだけ、フリートの様子を続けて出す
            if c == "status" {
                let dir = agentgw::bridge::state::StateDir::resolve();
                agentgw::bridge::relay::Cli::print_fleet(&dir).await;
            }
            std::process::exit(rc);
        }
        // 版は**単独で名乗る**。usage に紛れ込ませると、実機で版を確かめる1行が
        // 使い方の壁になる(そして終了コードが 2 になる)
        "--version" | "-V" | "version" => {
            println!("agentgw {}", env!("CARGO_PKG_VERSION"));
        }
        _ => {
            // ユーザーが読む行。**内部の事情(実装言語・課題番号)は出さない** —
            // 打ち間違えた人が要るのは正しい使い方だけ。名前は実際のコマンド名と揃える
            eprintln!(
                "agentgw {}\n\
                 \n\
                 まず:   install            このマシンを設定して起動する(親か子かを訊きます)\n\
                 ふだん: status | restart | shutdown | start | uninstall\n\
                 \n\
                 子を足すとき:\n\
                   親で: add-child user@host          届けて・繋いで・起こすまで1本で\n\
                 \n\
                 serve はサービスが呼ぶ本体です(手で打つ必要はありません)。",
                env!("CARGO_PKG_VERSION")
            );
            std::process::exit(2);
        }
    }
}
