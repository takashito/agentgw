//! The setup code: how the gateway's Owner is decided.
//!
//! `agentgw install` (the gateway) makes one and shows it. While there is no Owner, Slack asks for it
//! when the bot is invited to a channel or sent a DM; whoever sends it then signs Claude Code in on the
//! gateway, and **only when that sign-in succeeds** becomes the Owner. That ties the Owner's Slack
//! account to the Claude account. The code is removed once the Owner is set.
//!
//! It lives in `.env` as `AGENTGW_SETUP_CODE` (written 600, kept out of the agents' environment like
//! the other secrets). Only someone at the gateway's terminal can read it (`agentgw status` shows it).

use crate::state_dir::StateDir;

pub const ENV_KEY: &str = "AGENTGW_SETUP_CODE";

/// No look-alikes (0/O, 1/I/L), so it can be typed from the screen.
const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const LEN: usize = 8;

/// A new code, `XXXX-XXXX`.
pub fn generate() -> String {
    let seed = crate::state_dir::mint_secret(); // 64 hex chars from /dev/urandom
    let bytes: Vec<u8> = (0..LEN)
        .map(|i| u8::from_str_radix(&seed[i * 2..i * 2 + 2], 16).unwrap_or(0))
        .collect();
    let chars: String = bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect();
    format!("{}-{}", &chars[..4], &chars[4..])
}

/// The code in `.env`, if there is one.
pub fn load(dir: &StateDir) -> Option<String> {
    dir.load_env()
        .ok()?
        .into_iter()
        .find(|(k, v)| k == ENV_KEY && !v.is_empty())
        .map(|(_, v)| v)
}

/// The code in `.env`, made and saved first if there is none.
pub fn ensure(dir: &StateDir) -> std::io::Result<String> {
    if let Some(code) = load(dir) {
        return Ok(code);
    }
    let code = generate();
    std::fs::create_dir_all(dir.path())?;
    let path = dir.join(".env");
    let before = std::fs::read_to_string(&path).unwrap_or_default();
    let after = crate::state_dir::set_env_keys(&before, &[(ENV_KEY, code.clone())]);
    crate::state_dir::write_atomic_mode(&path, &after, Some(0o600))?;
    Ok(code)
}

/// Remove the code (the Owner is set: it has been used).
pub fn clear(dir: &StateDir) -> std::io::Result<()> {
    let path = dir.join(".env");
    let Ok(before) = std::fs::read_to_string(&path) else { return Ok(()) };
    let kept: Vec<&str> = before
        .lines()
        .filter(|l| l.split_once('=').map(|(k, _)| k.trim()) != Some(ENV_KEY))
        .collect();
    if kept.len() == before.lines().count() {
        return Ok(());
    }
    let mut text = kept.join("\n");
    text.push('\n');
    crate::state_dir::write_atomic_mode(&path, &text, Some(0o600))
}

/// The `status` line on a gateway with no Owner yet: the code to send in Slack. `None` on a machine
/// (it never decides an Owner) or once there is an Owner.
pub fn status_line(
    dir: &StateDir,
    env: &std::collections::HashMap<String, String>,
    access: &crate::bridge::state::Access,
) -> Option<String> {
    let machine = env.get("AGENTGW_BRIDGE_ROLE").is_some_and(|r| r.trim().eq_ignore_ascii_case("machine"));
    if machine || !access.owner.is_empty() {
        return None;
    }
    let code = load(dir)?;
    Some(crate::t!(
        "Owner: not set yet — send the setup code {code} to the bot in Slack (invite it to a channel or DM it)",
        "Owner: まだ居ません — Slack で bot にセットアップコード {code} を送ってください(チャンネルに招待するか DM で)"
    ))
}

/// What Slack says while there is no Owner (the bot was invited, sent a DM, mentioned or sent `login`).
pub fn ask_text() -> String {
    crate::t!(
        "This bot has no owner yet. Send the setup code `agentgw install` showed on the gateway (`agentgw status` there shows it again). You then sign Claude Code in, and become the owner.",
        "このボットにはまだ Owner がいません。ゲートウェイの `agentgw install` が表示したセットアップコードを送ってください(ゲートウェイで `agentgw status` を実行するともう一度見られます)。続けて Claude Code にサインインすると、あなたが Owner になります。"
    )
}

/// What a message is, held against the code.
#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    Match,
    /// Shaped like a code (8 letters and digits, a hyphen in the middle or not), but not this one.
    Wrong,
    NotACode,
}

/// Case, spaces and the hyphen don't matter.
pub fn check(text: &str, code: &str) -> Check {
    let typed: String = text
        .trim()
        .chars()
        .filter(|c| *c != '-' && !c.is_whitespace())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let shaped = typed.len() == LEN && typed.chars().all(|c| c.is_ascii_alphanumeric()) && text.trim().len() <= LEN + 2;
    if !shaped {
        return Check::NotACode;
    }
    let want: String = code.chars().filter(|c| *c != '-').collect();
    if crate::bridge::gateway::secret_eq(&typed, &want) {
        Check::Match
    } else {
        Check::Wrong
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_is_two_groups_of_four_without_look_alikes() {
        for _ in 0..50 {
            let c = generate();
            assert_eq!(c.len(), 9, "{c}");
            assert_eq!(&c[4..5], "-");
            assert!(c.chars().filter(|c| *c != '-').all(|c| ALPHABET.contains(&(c as u8))), "{c}");
        }
        assert_ne!(generate(), generate());
    }

    #[test]
    fn checking_ignores_case_spaces_and_the_hyphen() {
        assert_eq!(check("7kqm-h3xd", "7KQM-H3XD"), Check::Match);
        assert_eq!(check(" 7KQMH3XD ", "7KQM-H3XD"), Check::Match);
        assert_eq!(check("7KQM-H3XE", "7KQM-H3XD"), Check::Wrong);
        assert_eq!(check("hello", "7KQM-H3XD"), Check::NotACode);
        assert_eq!(check("please fix the tests", "7KQM-H3XD"), Check::NotACode);
    }

    #[test]
    fn the_code_is_kept_in_env_privately_until_cleared() {
        let tmp = std::env::temp_dir().join(format!("agentgw-owner-code-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let dir = StateDir::at(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join(".env"), "SLACK_BOT_TOKEN=xoxb-1\n").unwrap();
        let code = ensure(&dir).unwrap();
        assert_eq!(ensure(&dir).unwrap(), code, "made once");
        assert_eq!(load(&dir).as_deref(), Some(code.as_str()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(tmp.join(".env")).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        clear(&dir).unwrap();
        assert_eq!(load(&dir), None);
        assert!(std::fs::read_to_string(tmp.join(".env")).unwrap().contains("SLACK_BOT_TOKEN=xoxb-1"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
