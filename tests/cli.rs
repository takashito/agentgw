//! 実際にバイナリを起こして、サブコマンドが**口として通じるか**だけを見る。
//!
//! `main.rs` の分岐はライブラリのテストからは見えない(bin クレートは lib の `cfg(test)`
//! 隔離が効かないので、main.rs にテストを書かない)。別プロセスとして
//! 起こせばその罠は無い。2026-09-18、`invite` を畳んだときに直後の `add-child` の分岐まで
//! 消していて、実機で打つまで誰も気づかなかった。

use std::process::Command;

fn agentgw(args: &[&str]) -> (i32, String) {
    // 本番の置き場を掴まない(起動時の guard が置き場を読むため)
    let dir = std::env::temp_dir().join(format!("agentgw-cli-test-{}", std::process::id()));
    let out = Command::new(env!("CARGO_BIN_EXE_agentgw"))
        .args(args)
        .env("AGENTGW_STATE_DIR", &dir)
        .output()
        .expect("agentgw を起こせない");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

#[test]
fn add_machine_answers_with_its_own_usage() {
    // 引数なしなら add-child 自身の usage。**全体の usage に落ちたら分岐が消えている**
    let (code, text) = agentgw(&["add-machine"]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("usage: agentgw add-machine"), "{text}");
}

#[test]
fn link_answers_with_its_own_usage() {
    let (_, text) = agentgw(&["link", "not-a-connection-string"]);
    assert!(text.contains("link:"), "link の口が通じていない: {text}");
}

#[test]
fn version_names_the_binary() {
    let (code, text) = agentgw(&["--version"]);
    assert_eq!(code, 0);
    assert!(text.starts_with("agentgw "), "{text}");
}

#[test]
fn unknown_commands_fall_to_the_overall_usage() {
    let (code, text) = agentgw(&["invite"]);
    assert_eq!(code, 2, "invite は畳んだ: {text}");
    assert!(text.contains("add-machine user@host"), "{text}");
}
