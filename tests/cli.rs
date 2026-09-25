//! Start the real binary and check only that each subcommand **is still wired up**.
//!
//! Library tests can't see the branches in `main.rs` (a bin crate doesn't get the lib's
//! `cfg(test)` isolation, so we don't write tests in main.rs). Running it as a separate
//! process avoids that trap. On 2026-09-18, removing `invite` also removed the `add-child`
//! branch right after it, and nobody noticed until it was typed on a real machine.

use std::process::Command;

fn agentgw(args: &[&str]) -> (i32, String) {
    // Don't touch the production state directory (the startup guard reads it)
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
    // With no arguments, add-child prints its own usage. **Falling to the overall usage means the branch is gone**
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

#[test]
fn install_reaches_setup() {
    // With no terminal and no .env, install stops at the role question **before writing anything**.
    // HOME and the service label point away from the real service anyway.
    let dir = std::env::temp_dir().join(format!("agentgw-cli-install-{}", std::process::id()));
    let out = Command::new(env!("CARGO_BIN_EXE_agentgw"))
        .arg("install")
        .env("AGENTGW_STATE_DIR", dir.join("state"))
        .env("HOME", &dir)
        .env("AGENTGW_SERVICE_LABEL", "agentgw-cli-test")
        .env("AGENTGW_SERVICE_UNIT", "agentgw-cli-test.service")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("agentgw failed to start");
    let text = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.starts_with("install: "), "{text}");
}

#[test]
fn update_on_a_bridge_with_no_gateway_says_so() {
    // No .env: no gateway to ask, and nothing is asked of anyone
    let (code, text) = agentgw(&["update"]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("works on its own"), "{text}");
}
