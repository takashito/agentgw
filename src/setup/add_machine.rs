//! Decisions for adding a machine. **Touches no OS** — everything here is a testable pure function.
//!
//! The layer that calls ssh / scp / gh / tailscale is [`ssh`](crate::setup::ssh).

use std::path::{Path, PathBuf};

/// The remote `uname -sm` → a Rust target triple. An unknown combination is `None`
/// (= the branch that builds on the remote machine).
pub fn triple_for(uname_sm: &str) -> Option<&'static str> {
    Some(match uname_sm.trim() {
        "Linux x86_64" => "x86_64-unknown-linux-musl",
        "Linux aarch64" | "Linux arm64" => "aarch64-unknown-linux-musl",
        "Darwin arm64" => "aarch64-apple-darwin",
        "Darwin x86_64" => "x86_64-apple-darwin",
        _ => return None,
    })
}

/// Turn the state directory chosen locally into a form to hand to the remote shell.
///
/// **`$HOME` differs on the remote**, so a path under home is rewritten relative to `~` and
/// the remote shell expands it. Without this the remote falls back to the default `~/.local/state/agentgw`.
pub fn remote_state_prefix(state_dir: Option<&str>, home: &str) -> String {
    match state_dir {
        None => String::new(),
        Some(dir) => match dir.strip_prefix(&format!("{home}/")) {
            Some(rel) => format!("AGENTGW_STATE_DIR=\"$HOME/{rel}\" "),
            None => format!("AGENTGW_STATE_DIR='{dir}' "),
        },
    }
}

/// Where the shipped binary comes from.
#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    /// Download from a GitHub release (the gateway and machine versions match automatically)
    Release { tag: String },
    /// A local artifact built by `cargo dist`
    Dist(PathBuf),
}

/// A private repository. **Only the gateway downloads** — the machine needs neither gh nor credentials.
pub const REPO: &str = "takashito/agentgw";

/// Arguments for `gh release download`. **Only builds them** (nothing is run here).
pub fn gh_download_args(tag: &str, triple: &str, out_dir: &Path) -> Vec<String> {
    [
        "release",
        "download",
        tag,
        "--repo",
        REPO,
        "--pattern",
        &format!("agentgw-{triple}"),
        "--dir",
        &out_dir.to_string_lossy(),
        "--clobber",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// **Release first, local artifact second.** Downloading from a release guarantees the gateway and machine run the same version.
///
/// `dist` is "the path of the artifact for that triple, if it exists". The caller checks and passes it.
pub fn choose_source(has_gh: bool, dist: Option<PathBuf>, version: &str) -> Result<Source, String> {
    if has_gh {
        return Ok(Source::Release {
            tag: format!("v{version}"),
        });
    }
    dist.map(Source::Dist).ok_or_else(|| {
        crate::t!(
            "No agentgw binary to send. Either run `gh auth login` so it can be downloaded from a release,\n\
             or build one with `cargo dist`, then try again.",
            "送るバイナリがありません。`gh auth login` で release から落とせるようにするか、\n\
             `cargo dist` で手元に焼いてから、もう一度。"
        )
    })
}

// ── Route ────────────────────────────────────────────────────────────────────

/// The route a machine uses to reach the gateway. **Decided by measurement, not detection** — start the
/// machine and check whether its name shows up in the gateway's `ask_connected` (`bridge::gateway::Cli`).
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Transport {
    /// The machine dials the gateway's public name directly (a tailnet, etc.)
    Direct { url: String },
    /// The machine dials its own loopback, through an ssh tunnel the gateway opens
    Tunnel { remote_port: u16 },
}

/// Candidate for a direct connection. The URL remembered in `.env` wins; otherwise the tailscale name.
///
/// `Self.DNSName` from `tailscale status --json` **has a trailing dot** (measured:
/// `mac.tail1234.ts.net.`). Strip it before building the URL.
pub fn candidate_url(env_url: Option<&str>, tailscale_json: Option<&str>) -> Option<String> {
    if let Some(u) = env_url.map(str::trim).filter(|u| !u.is_empty()) {
        return Some(u.trim_end_matches('/').to_string());
    }
    let json: serde_json::Value = serde_json::from_str(tailscale_json?).ok()?;
    let name = json["Self"]["DNSName"].as_str()?.trim_end_matches('.');
    (!name.is_empty()).then(|| format!("wss://{name}"))
}

/// The URL written to the machine's `.env`.
pub fn dial_url(t: &Transport) -> String {
    match t {
        Transport::Direct { url } => url.clone(),
        Transport::Tunnel { remote_port } => format!("ws://127.0.0.1:{remote_port}"),
    }
}

/// Add or remove one machine in `.env`'s `AGENTGW_TUNNELS` (`laptop=me@laptop,desktop=me@desktop`).
///
/// **The gateway's agentgw opens the tunnels itself** (not a separate service — they only need to be
/// up while agentgw runs). It reads this list at startup and watches one ssh per machine.
pub fn tunnels_with(raw: &str, child: &str, target: Option<&str>) -> String {
    let mut list: Vec<(String, String)> = crate::bridge::machine::child_urls(raw)
        .into_iter()
        .filter(|(id, _)| id != child)
        .collect();
    if let Some(t) = target {
        list.push((child.to_string(), t.to_string()));
    }
    list.iter()
        .map(|(id, t)| format!("{id}={t}"))
        .collect::<Vec<_>>()
        .join(",")
}

// ── Execution ────────────────────────────────────────────────────────────────
// This layer touches the OS and the remote machine. The decisions live in the pure functions above (which are tested).

use super::ssh;
use crate::bridge::gateway::TUNNEL_PORT;

use crate::bridge::gateway::Cli as RelayCli;
use crate::bridge::gateway::link;
use crate::state_dir::StateDir;

/// The bootstrap sent to the machine. **Baked into the binary** — a gateway installed from a release
/// has no repo, so looking for it as a file would fail.
const INSTALL_SH: &str = include_str!("../../scripts/install.sh");

/// The gateway's listener and key. Create them in `.env` if missing; restart only when a new key was made.
pub struct Listener {
    pub listen: String,
    pub token: String,
    pub name: String,
}

/// `agentgw add-child <ssh-target> [--name <name>] [--from <path>]`
pub async fn cli(args: &[String]) -> i32 {
    let mut target = None;
    let mut name = None;
    let mut from = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => name = it.next().cloned(),
            "--from" => from = it.next().cloned(),
            s if s.starts_with("--name=") => name = Some(s["--name=".len()..].to_string()),
            s if s.starts_with("--from=") => from = Some(s["--from=".len()..].to_string()),
            s if s.starts_with('-') => {
                eprintln!("{}", crate::t!("Unknown option: {s}", "知らない引数です: {s}"));
                return 2;
            }
            s => target = Some(s.to_string()),
        }
    }
    let Some(target) = target else {
        eprintln!(
            "{}",
            crate::t!(
                "usage: agentgw add-machine <ssh destination> [--name <name>] [--from <binary>]\n\
                 \n\
                 Example: agentgw add-machine user@host\n\
                 The destination can be a ~/.ssh/config alias; keys and jump hosts come from your ssh config.",
                "usage: agentgw add-machine <ssh先> [--name <名前>] [--from <バイナリ>]\n\
                 \n\
                 例: agentgw add-machine user@host\n\
                 ssh 先は ~/.ssh/config の別名でも構いません(鍵も踏み台もそちらに任せます)。"
            )
        );
        return 2;
    };
    match add_child(&target, name.as_deref(), from.as_deref()).await {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(why) => {
            eprintln!("add-machine: {why}");
            1
        }
    }
}

/// Add one machine. **The order matters** — see each step's comment.
async fn add_child(target: &str, name: Option<&str>, from: Option<&str>) -> Result<String, String> {
    let dir = StateDir::resolve();

    // 1. Look at the remote
    println!("{}", crate::t!("==> Checking {target}", "==> {target} を確認しています"));
    let uname = ssh::ssh_capture(target, "uname -sm")?;
    let triple = triple_for(&uname)
        .ok_or_else(|| crate::t!("There is no prebuilt binary for {uname}. Build and install agentgw there by hand.", "{uname} 用のバイナリは配布していません。そのマシンでビルドして入れてください。"))?;
    println!("  {uname} ({triple})");

    // An explicit name wins. Otherwise use the remote's hostname (**the only place a name is guessed**)
    let child = match name {
        Some(n) => n.trim().to_string(),
        None => ssh::ssh_capture(target, "hostname -s 2>/dev/null || hostname")
            .unwrap_or_default()
            .trim()
            .to_string(),
    };
    if child.is_empty() {
        return Err(crate::t!("Couldn't work out this machine's name. Pass --name <name>.", "このマシンの名前が決められません。--name <名前> を付けてください"));
    }
    println!("{}", crate::t!("  Name: {child}", "  名前: {child}"));

    // 2. Pick what to ship (release first, then the cargo dist artifact)
    let staging = std::env::temp_dir().join(format!("agentgw-add-{}", std::process::id()));
    let binary = match from {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let dist = dist_artifact(triple);
            match choose_source(ssh::gh_ready(), dist, env!("CARGO_PKG_VERSION"))? {
                Source::Release { tag } => {
                    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
                    println!("{}", crate::t!("==> Downloading release {tag} from GitHub", "==> GitHub から {tag} をダウンロードしています"));
                    ssh::gh_download(
                        &gh_download_args(&tag, triple, &staging),
                        &staging,
                        triple,
                    )?
                }
                Source::Dist(p) => {
                    println!("{}", crate::t!("==> Using the binary built on this machine", "==> このマシンでビルドしたバイナリを使います"));
                    p
                }
            }
        }
    };

    // 3. Deliver (the binary and install.sh)
    println!("{}", crate::t!("==> Copying agentgw to {target}", "==> {target} に agentgw をコピーしています"));
    let remote_bin = ".local/bin/agentgw";
    ssh::ssh_run(target, "mkdir -p ~/.local/bin")?;
    // A running binary cannot be overwritten. Move it aside first, then place the new one
    ssh::ssh_run(
        target,
        &format!("[ -e {remote_bin} ] && mv -f {remote_bin} {remote_bin}.old || true"),
    )?;
    ssh::scp(&binary, target, remote_bin)?;
    ssh::ssh_run(
        target,
        &format!("chmod 755 {remote_bin} && rm -f {remote_bin}.old"),
    )?;
    ssh::put_text(target, ".local/bin/agentgw-install.sh", INSTALL_SH)?;
    ssh::ssh_run(target, "chmod 755 .local/bin/agentgw-install.sh")?;
    let _ = std::fs::remove_dir_all(&staging);

    // 4. Prepare the gateway's listener and key
    let inlet = ensure_inlet(&dir)?;

    // 5. Try a direct connection. With no candidate, start with the tunnel
    let candidate = {
        let env = RelayCli::env_of(&dir);
        candidate_url(
            env.get("AGENTGW_LINK_PUBLIC_URL").map(String::as_str),
            ssh::tailscale_json().as_deref(),
        )
    };
    let state_prefix = remote_state_prefix(
        std::env::var("AGENTGW_STATE_DIR").ok().as_deref(),
        &std::env::var("HOME").unwrap_or_default(),
    );

    let mut transport = match candidate {
        Some(url) => {
            println!("{}", crate::t!("==> Trying a direct connection to {url}", "==> {url} への直結を試しています"));
            Transport::Direct { url }
        }
        None => {
            println!("{}", crate::t!("==> This gateway has no public address, so {child} will connect over an ssh tunnel", "==> このゲートウェイには公開アドレスが無いので、{child} を ssh トンネルでつなぎます"));
            set_tunnel(&dir, &child, Some(target))?;
            Transport::Tunnel {
                remote_port: TUNNEL_PORT,
            }
        }
    };

    link_child(
        target,
        &child,
        &transport,
        &inlet,
        &state_prefix,
        remote_bin,
    )?;

    // 6. Start the machine and check the gateway can see it
    println!("{}", crate::t!("==> Installing and starting agentgw on {target}", "==> {target} に agentgw をインストールして起動しています"));
    let installed = ssh::ssh_interactive(
        target,
        &format!("{state_prefix}~/.local/bin/agentgw-install.sh --from ~/{remote_bin}"),
    );
    // The shipped install.sh is single-use. **Clean it up whether or not it worked** (the next add-child sends it again)
    let _ = ssh::ssh_run(target, "rm -f ~/.local/bin/agentgw-install.sh");
    installed?;

    if !wait_connected(&inlet, &child).await {
        // The direct connection failed, so fall back to the tunnel and try again
        if matches!(transport, Transport::Direct { .. }) {
            println!("{}", crate::t!("==> The direct connection didn't work. Switching to an ssh tunnel.", "==> 直結ではつながりませんでした。ssh トンネルに切り替えます。"));
            set_tunnel(&dir, &child, Some(target))?;
            transport = Transport::Tunnel {
                remote_port: TUNNEL_PORT,
            };
            link_child(
                target,
                &child,
                &transport,
                &inlet,
                &state_prefix,
                remote_bin,
            )?;
            ssh::ssh_run(target, &format!("{state_prefix}~/{remote_bin} restart"))?;
            if !wait_connected(&inlet, &child).await {
                return Err(crate::t!(
                    "{child} can't reach the gateway, either directly or over an ssh tunnel.\n\
                     Its log: ssh {target} 'tail ~/.local/state/agentgw/plugin-debug.log'",
                    "{child} がゲートウェイにつながりません。直結でも ssh トンネルでもだめでした。\n\
                     {child} のログ: ssh {target} 'tail ~/.local/state/agentgw/plugin-debug.log'"
                ));
            }
        } else {
            return Err(crate::t!(
                "{child} can't reach the gateway over the ssh tunnel.\n\
                 This machine's log: grep tunnel ~/.local/state/agentgw/plugin-debug.log\n\
                 {child}'s log: ssh {target} 'tail ~/.local/state/agentgw/plugin-debug.log'",
                "{child} が ssh トンネル経由でゲートウェイにつながりません。\n\
                 このマシンのログ: grep tunnel ~/.local/state/agentgw/plugin-debug.log\n\
                 {child} のログ: ssh {target} 'tail ~/.local/state/agentgw/plugin-debug.log'"
            ));
        }
    } else if matches!(transport, Transport::Direct { .. }) {
        // Connected directly. **If a tunnel was set up before, remove it** — otherwise the gateway
        // keeps holding an ssh that nobody uses
        set_tunnel(&dir, &child, None)?;
    }

    let how = match &transport {
        Transport::Direct { url } => crate::t!("directly ({url})", "直結({url})"),
        Transport::Tunnel { .. } => crate::t!("over an ssh tunnel", "ssh トンネル経由"),
    };
    Ok(crate::t!(
        "\n{child} is connected {how}.\nTo hand a Slack channel to it, type `route {child}` in that channel.",
        "\n{child} がつながりました({how})。\nSlack のチャンネルを任せるには、そのチャンネルで `route {child}` と打ってください。"
    ))
}

/// The gateway's listener and key. Create them in `.env` if missing, and restart **only when a new key was made**
/// (the running Bridge still holds the old key, so the machine would get a 401).
fn ensure_inlet(dir: &StateDir) -> Result<Listener, String> {
    let env = RelayCli::env_of(dir);
    let listen = env
        .get("AGENTGW_LINK_LISTEN")
        .cloned()
        .unwrap_or_else(|| crate::bridge::gateway::DEFAULT_LISTEN.to_string());
    let (token, minted) =
        RelayCli::key_for_invite(env.get("AGENTGW_LINK_TOKEN").map(String::as_str));
    let name = env
        .get("AGENTGW_BRIDGE_ID")
        .cloned()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            crate::t!(
                "This machine has no name yet (AGENTGW_BRIDGE_ID in .env). Run `agentgw install` first.",
                "このマシンにはまだ名前がありません(.env の AGENTGW_BRIDGE_ID)。先に `agentgw install` を実行してください。"
            )
        })?;
    if minted {
        RelayCli::write_env(
            dir,
            &[
                ("AGENTGW_LINK_LISTEN", listen.clone()),
                ("AGENTGW_LINK_TOKEN", token.clone()),
            ],
        )
        .map_err(|e| crate::t!("Couldn't write .env: {e}", ".env が書けません: {e}"))?;
        println!(
            "{}",
            crate::t!(
                "==> This gateway now accepts machines on {listen}, with a new secret key. Restarting to apply it.",
                "==> このゲートウェイがマシンを受け入れるようにしました({listen}、新しい秘密鍵)。反映のため再起動します。"
            )
        );
        crate::service::Service::run("restart", &[]);
    }
    Ok(Listener {
        listen,
        token,
        name,
    })
}

/// Build the connection string and feed it to the machine. **Never on argv** (it would stay in the remote's ps and history).
fn link_child(
    target: &str,
    child: &str,
    transport: &Transport,
    inlet: &Listener,
    state_prefix: &str,
    remote_bin: &str,
) -> Result<(), String> {
    let conn = link::encode_connection(&link::Invite {
        url: dial_url(transport),
        api_token: inlet.token.clone(),
    });
    ssh::ssh_stdin(
        target,
        &format!("{state_prefix}~/{remote_bin} link --name '{child}' -"),
        &format!("{conn}\n"),
    )?;
    Ok(())
}

/// Rewrite `AGENTGW_TUNNELS` in the gateway's `.env` and restart the gateway **only if it changed**
/// (the gateway's agentgw reads it at startup to open tunnels; restart does not tear down agents).
fn set_tunnel(dir: &StateDir, child: &str, target: Option<&str>) -> Result<(), String> {
    let env = RelayCli::env_of(dir);
    let before = env.get("AGENTGW_TUNNELS").cloned().unwrap_or_default();
    let after = tunnels_with(&before, child, target);
    if after == before {
        return Ok(());
    }
    RelayCli::write_env(dir, &[("AGENTGW_TUNNELS", after)])
        .map_err(|e| crate::t!("Couldn't write .env: {e}", ".env が書けません: {e}"))?;
    let line = match target {
        Some(t) => crate::t!(
            "  This gateway will keep an ssh tunnel open to {child} (ssh {t}). Restarting to apply it.",
            "  このゲートウェイが {child} への ssh トンネルを張り続けます(ssh {t})。反映のため再起動します。"
        ),
        None => crate::t!(
            "  {child} no longer needs an ssh tunnel. Restarting to apply it.",
            "  {child} への ssh トンネルは不要になりました。反映のため再起動します。"
        ),
    };
    println!("{line}");
    crate::service::Service::run("restart", &[]);
    Ok(())
}

/// Ask the gateway's `status` endpoint for the currently connected machines and wait until the name appears.
///
/// **This is what decides the route** (whether a real link came up, not a probe).
async fn wait_connected(inlet: &Listener, child: &str) -> bool {
    for i in 0..10 {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        if let Some(names) = RelayCli::ask_connected(&inlet.listen, &inlet.token).await
            && names.iter().any(|n| n == child)
        {
            return true;
        }
    }
    false
}

/// The `cargo dist` artifact. Found **only when the repo is present**.
fn dist_artifact(triple: &str) -> Option<std::path::PathBuf> {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let p = base.join(triple).join("release").join("agentgw");
    p.exists().then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_uname_to_a_rust_triple() {
        assert_eq!(
            triple_for("Linux x86_64"),
            Some("x86_64-unknown-linux-musl")
        );
        assert_eq!(
            triple_for("Linux aarch64"),
            Some("aarch64-unknown-linux-musl")
        );
        assert_eq!(
            triple_for("Linux arm64"),
            Some("aarch64-unknown-linux-musl")
        );
        assert_eq!(triple_for("Darwin arm64"), Some("aarch64-apple-darwin"));
        assert_eq!(triple_for("Darwin x86_64"), Some("x86_64-apple-darwin"));
        assert_eq!(triple_for("Plan9 386"), None, "知らない形は空で返す");
    }

    #[test]
    fn trims_what_ssh_gives_back() {
        // `ssh <remote> 'uname -sm'` returns with a trailing newline
        assert_eq!(
            triple_for("Linux x86_64\n"),
            Some("x86_64-unknown-linux-musl")
        );
    }

    #[test]
    fn carries_the_state_dir_as_a_home_relative_path() {
        assert_eq!(
            remote_state_prefix(Some("/home/me/.local/state/agentgw-dev"), "/home/me"),
            "AGENTGW_STATE_DIR=\"$HOME/.local/state/agentgw-dev\" "
        );
    }

    #[test]
    fn keeps_an_absolute_path_outside_home_as_is() {
        assert_eq!(
            remote_state_prefix(Some("/srv/agentgw"), "/home/me"),
            "AGENTGW_STATE_DIR='/srv/agentgw' "
        );
    }

    #[test]
    fn passes_nothing_when_the_parent_uses_the_default() {
        assert_eq!(remote_state_prefix(None, "/home/me"), "");
    }

    #[test]
    fn builds_the_gh_download_argv() {
        let args = gh_download_args("v0.18.5", "x86_64-unknown-linux-musl", Path::new("/tmp/x"));
        assert_eq!(
            args,
            vec![
                "release",
                "download",
                "v0.18.5",
                "--repo",
                REPO,
                "--pattern",
                "agentgw-x86_64-unknown-linux-musl",
                "--dir",
                "/tmp/x",
                "--clobber",
            ]
        );
    }

    #[test]
    fn prefers_the_release_over_the_local_build() {
        let s = choose_source(true, Some(PathBuf::from("/t/agentgw")), "0.18.5").unwrap();
        assert_eq!(
            s,
            Source::Release {
                tag: "v0.18.5".to_string()
            }
        );
    }

    #[test]
    fn falls_back_to_the_local_build_without_gh() {
        let s = choose_source(false, Some(PathBuf::from("/t/agentgw")), "0.18.5").unwrap();
        assert_eq!(s, Source::Dist(PathBuf::from("/t/agentgw")));
    }

    #[test]
    fn says_what_to_do_when_there_is_nothing_to_send() {
        let why = choose_source(false, None, "0.18.5").unwrap_err();
        assert!(why.contains("cargo dist"), "打つべきコマンドを出す: {why}");
    }

    #[test]
    fn uses_the_remembered_public_url_first() {
        assert_eq!(
            candidate_url(Some("wss://mac.tailnet.ts.net/"), None).as_deref(),
            Some("wss://mac.tailnet.ts.net")
        );
    }

    #[test]
    fn falls_back_to_the_tailscale_name() {
        // Self.DNSName has a trailing dot (measured)
        let json = r#"{"Self":{"DNSName":"mac.tail1234.ts.net."}}"#;
        assert_eq!(
            candidate_url(None, Some(json)).as_deref(),
            Some("wss://mac.tail1234.ts.net")
        );
    }

    #[test]
    fn has_no_candidate_without_either() {
        assert_eq!(candidate_url(None, None), None);
        assert_eq!(
            candidate_url(Some("  "), None),
            None,
            "空白だけは候補でない"
        );
        assert_eq!(candidate_url(None, Some("not json")), None);
    }

    #[test]
    fn the_child_dials_its_own_loopback_through_the_tunnel() {
        assert_eq!(
            dial_url(&Transport::Tunnel { remote_port: 8799 }),
            "ws://127.0.0.1:8799"
        );
        assert_eq!(
            dial_url(&Transport::Direct {
                url: "wss://p".into()
            }),
            "wss://p"
        );
    }

    #[test]
    fn a_tunnel_is_added_replaced_and_removed_by_child_name() {
        assert_eq!(
            tunnels_with("", "laptop", Some("me@laptop")),
            "laptop=me@laptop"
        );
        assert_eq!(
            tunnels_with("desktop=me@desktop", "laptop", Some("me@laptop")),
            "desktop=me@desktop,laptop=me@laptop"
        );
        assert_eq!(
            tunnels_with(
                "laptop=old@laptop,desktop=me@desktop",
                "laptop",
                Some("me@laptop")
            ),
            "desktop=me@desktop,laptop=me@laptop",
            "同じ子は1本だけ(差し替え)"
        );
        assert_eq!(
            tunnels_with("laptop=me@laptop,desktop=me@desktop", "laptop", None),
            "desktop=me@desktop",
            "直結で繋がったら外す"
        );
    }
}
