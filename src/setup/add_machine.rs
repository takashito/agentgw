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
    /// The same release over plain HTTPS. **The repository is public**, so no gh and no credentials
    Url { url: String },
    /// A local artifact built by `cargo dist`
    Dist(PathBuf),
}

/// The public repository. **Only the gateway downloads** — the machine needs nothing.
pub const REPO: &str = "takashito/agentgw";

/// Where a release asset sits without gh. Public, so a plain fetch is enough.
pub fn release_url(tag: &str, triple: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/{tag}/agentgw-{triple}")
}

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
pub fn choose_source(
    has_gh: bool,
    dist: Option<PathBuf>,
    version: &str,
    triple: &str,
) -> Result<Source, String> {
    let tag = format!("v{version}");
    if has_gh {
        return Ok(Source::Release { tag });
    }
    // No gh is the normal case on a server. The repository is public, so fetch the same asset directly;
    // a local build is the fallback for a version that was never released
    if dist.is_none() {
        return Ok(Source::Url {
            url: release_url(&tag, triple),
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

/// Where this machine can be reached, **on the interface it uses to reach the gateway**: (host, IPv4).
///
/// Not "its best address": a machine on a tailnet that links over the LAN is reachable at its LAN
/// address, and saying its tailnet name there would name an interface the link doesn't use.
/// `local_ip` is the address the kernel would send from (see [`local_ip_toward`]); the name is the
/// tailnet's when that is the address it picked, and this machine's own name otherwise.
///
/// Pure so it can be tested without a tailnet.
pub fn reachable_at(
    tailscale_json: Option<&str>,
    hostname: &str,
    fqdn: Option<&str>,
    local_ip: Option<&str>,
) -> (String, String) {
    let own = fqdn
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .unwrap_or(hostname)
        .trim()
        .to_string();
    let (tailnet, tailnet_ip) = tailnet_identity(tailscale_json);
    match local_ip.map(str::trim).filter(|i| !i.is_empty()) {
        // Out over the tailnet: the tailnet's name is the one that resolves to this address
        Some(ip) if ip == tailnet_ip && !tailnet.is_empty() => (tailnet, ip.to_string()),
        Some(ip) => (own, ip.to_string()),
        // Nothing dialled yet (the gateway asking about itself). Its name, with no interface to name
        None => (own, String::new()),
    }
}

/// The address this machine would send from to reach `host:port`. **Asks the routing table, sends
/// nothing** — a connected UDP socket only fixes the peer, and then the local address is decided.
pub fn local_ip_toward(host: &str, port: u16) -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect((host, port)).ok()?;
    match sock.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) => Some(ip.to_string()),
        std::net::IpAddr::V6(_) => None,
    }
}

/// `host` and port from a dial URL: `ws://dock.lan:8787` → `("dock.lan", 8787)`, `wss://x` → `("x", 443)`.
pub fn host_port_of(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let rest = rest.split('/').next()?;
    let default = match scheme {
        "wss" | "https" => 443,
        _ => 80,
    };
    match rest.rsplit_once(':') {
        Some((host, port)) => Some((host.to_string(), port.parse().ok()?)),
        None => Some((rest.to_string(), default)),
    }
}

/// This machine on the tailnet: (DNS name, first IPv4). Empty strings where tailscale can't say.
pub fn tailnet_identity(tailscale_json: Option<&str>) -> (String, String) {
    let Some(json) = tailscale_json.and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
    else {
        return (String::new(), String::new());
    };
    let name = json["Self"]["DNSName"]
        .as_str()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_string();
    let ip = json["Self"]["TailscaleIPs"]
        .as_array()
        .and_then(|ips| {
            ips.iter()
                .filter_map(|ip| ip.as_str())
                .find(|ip| !ip.contains(':'))
        })
        .unwrap_or_default()
        .to_string();
    (name, ip)
}

/// What the machine can actually reach, decided **from both sides**:
///
/// 1. A URL set by hand wins.
/// 2. **Both on the tailnet** → the gateway's tailnet name. It works from anywhere, and TLS is
///    terminated by whatever sits in front (`tailscale serve`).
/// 3. Otherwise **the LAN, proved** — the caller reaches the gateway from the machine before this says yes.
/// 4. Nothing reachable → the caller falls back to an ssh tunnel.
pub fn route_url(
    env_url: Option<&str>,
    gateway_tailnet: Option<&str>,
    machine_has_tailscale: bool,
    lan_reachable: Option<&str>,
) -> Option<String> {
    if let Some(u) = env_url.map(str::trim).filter(|u| !u.is_empty()) {
        return Some(u.trim_end_matches('/').to_string());
    }
    if machine_has_tailscale
        && let Some(name) = gateway_tailnet.filter(|n| !n.is_empty())
    {
        return Some(format!("wss://{name}"));
    }
    lan_reachable.map(str::to_string)
}

/// The gateway's own name on the tailnet, if it is on one.
pub fn tailnet_name(tailscale_json: Option<&str>) -> Option<String> {
    let (name, _) = tailnet_identity(tailscale_json);
    (!name.is_empty()).then_some(name)
}

/// The port this gateway accepts links on. Taken from what it already listens on, and `8787` when it
/// has not been told — **the address is decided per machine**, the port is the same for all of them.
pub fn link_port(listen: Option<&str>) -> String {
    listen
        .unwrap_or_default()
        .split(',')
        .filter_map(|a| a.trim().rsplit_once(':'))
        .map(|(_, port)| port.to_string())
        .find(|p| !p.is_empty())
        .unwrap_or_else(|| crate::bridge::gateway::DEFAULT_PORT.to_string())
}

/// What a machine on the same network would dial, and the address the gateway must hold for it:
/// `(ws://dock.lan:8787, 192.168.10.11:8787)`. A name others can resolve reads better in `.env`, but
/// the address is the one we know is ours to open.
pub fn lan_route(host: Option<&str>, ip: Option<&str>, port: &str) -> Option<(String, String)> {
    let ip = ip.map(str::trim).filter(|i| !i.is_empty())?;
    let name = host
        .map(str::trim)
        .filter(|h| h.contains('.'))
        .unwrap_or(ip);
    Some((format!("ws://{name}:{port}"), format!("{ip}:{port}")))
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

/// The route this gateway settled on for a machine last time. **A tunnel is already written down**
/// (`AGENTGW_TUNNELS`, the gateway keeps it open); a direct one is remembered here so the next
/// `add-machine` starts where the last one ended instead of measuring everything again.
pub fn remembered_route(tunnels: &str, routes: &str, child: &str) -> Option<Transport> {
    if crate::bridge::machine::child_urls(tunnels)
        .iter()
        .any(|(id, _)| id == child)
    {
        return Some(Transport::Tunnel {
            remote_port: TUNNEL_PORT,
        });
    }
    crate::bridge::machine::child_urls(routes)
        .into_iter()
        .find(|(id, _)| id == child)
        .map(|(_, url)| Transport::Direct { url })
}

/// `AGENTGW_ROUTES` with this machine's direct URL written in (or taken out, when it moved to a tunnel).
pub fn routes_with(raw: &str, child: &str, url: Option<&str>) -> String {
    tunnels_with(raw, child, url)
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
    let mut access = ssh::Access::default();
    let mut fresh = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => name = it.next().cloned(),
            "--from" => from = it.next().cloned(),
            "-i" | "--identity" => access.identity = it.next().cloned(),
            // A flag, not a value: **the password stays between you and ssh** (in argv it would sit in
            // this machine's `ps` and shell history)
            "-p" | "--password" => access.ask_password = true,
            // Work the route out again instead of taking the one that worked last time
            "-n" | "--new" => fresh = true,
            s if s.starts_with("-i=") => access.identity = Some(s["-i=".len()..].to_string()),
            s if s.starts_with("--identity=") => access.identity = Some(s["--identity=".len()..].to_string()),
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
                "usage: agentgw add-machine <ssh destination> [-i <key>] [-p] [-n] [--name <name>] [--from <binary>]\n\
                 \n\
                 Example: agentgw add-machine user@host\n\
                 The destination can be a ~/.ssh/config alias; keys and jump hosts come from your ssh config.\n\
                 \n\
                   -i <key>  the private key to offer\n\
                   -p        let ssh ask for a password (asked once; every later step shares that connection)\n\
                   -n        work out the route again instead of the one that worked last time",
                "usage: agentgw add-machine <ssh先> [-i <鍵>] [-p] [-n] [--name <名前>] [--from <バイナリ>]\n\
                 \n\
                 例: agentgw add-machine user@host\n\
                 ssh 先は ~/.ssh/config の別名でも構いません(鍵も踏み台もそちらに任せます)。\n\
                 \n\
                   -i <鍵>   使う秘密鍵\n\
                   -p        ssh にパスワードを訊かせる(訊かれるのは最初の1回。以降の処理は同じ接続を使います)\n\
                   -n        前回の経路を使わず、もう一度調べ直す"
            )
        );
        return 2;
    };
    // Every ssh / scp from here on uses these (set once, before anything connects)
    ssh::use_access(access);
    match add_child(&target, name.as_deref(), from.as_deref(), fresh).await {
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
async fn add_child(
    target: &str,
    name: Option<&str>,
    from: Option<&str>,
    fresh: bool,
) -> Result<String, String> {
    let dir = StateDir::resolve();

    // 1. Look at the remote
    println!("{}", crate::t!("==> Checking {target}", "==> {target} を確認しています"));
    let uname = ssh::ssh_capture(target, "uname -sm")?;
    let triple = triple_for(&uname)
        .ok_or_else(|| crate::t!("There is no prebuilt binary for {uname}. Build and install agentgw there by hand.", "{uname} 用のバイナリは配布していません。そのマシンでビルドして入れてください。"))?;
    println!("  {uname} ({triple})");

    // With `-p` the way in is a password, and **a tunnel can't be asked for one** — it is reopened
    // unattended. Leave the gateway's own key behind while the password session is still open, and use
    // it from here on
    if ssh::asks_for_password() {
        println!(
            "{}",
            crate::t!(
                "==> Leaving this gateway's ssh key on {target} (so the tunnel can reopen without you)",
                "==> トンネルを人手なしで張り直せるように、このゲートウェイの ssh 鍵を {target} に置きます"
            )
        );
        let pubkey = ssh::ensure_gateway_key()?;
        ssh::authorize_key(target, &pubkey)?;
        ssh::use_key_from_now_on();
        ssh::ssh_capture(target, "true")
            .map_err(|e| crate::t!("The key didn't work on {target}: {e}", "{target} で鍵が使えませんでした: {e}"))?;
        println!("{}", crate::t!("  the key works", "  鍵で入れます"));
    }

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
            match choose_source(ssh::gh_ready(), dist, env!("CARGO_PKG_VERSION"), triple)? {
                Source::Release { tag } => {
                    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
                    println!("{}", crate::t!("==> Downloading release {tag} from GitHub", "==> GitHub から {tag} をダウンロードしています"));
                    ssh::gh_download(
                        &gh_download_args(&tag, triple, &staging),
                        &staging,
                        triple,
                    )?
                }
                Source::Url { url } => {
                    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
                    println!("{}", crate::t!("==> Downloading {url}", "==> {url} をダウンロードしています"));
                    let out = staging.join(format!("agentgw-{triple}"));
                    ssh::download(&url, &out)?;
                    out
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

    // 5. Work out **the order to try**, by measuring from the machine — which is the side that dials.
    //    Measuring says who answers; only a real link says it works, so what is measured is an order,
    //    not a decision. The ssh tunnel is last and is always there (we got here over ssh).
    let env = RelayCli::env_of(&dir);
    let by_hand = env.get("AGENTGW_LINK_PUBLIC_URL").cloned();
    let known = (!fresh)
        .then(|| {
            remembered_route(
                env.get("AGENTGW_TUNNELS").map(String::as_str).unwrap_or_default(),
                env.get("AGENTGW_ROUTES").map(String::as_str).unwrap_or_default(),
                &child,
            )
        })
        .flatten();
    let plan: Vec<RouteChoice> = match &known {
        // What worked last time, unless `-n` says to work it out again. Its address is already open,
        // and if it has stopped working the tunnel is behind it
        Some(Transport::Direct { url }) => {
            println!("{}", crate::t!("==> {child} came in over {url} last time", "==> 前回 {child} は {url} でつながりました"));
            vec![
                RouteChoice { label: url.clone(), url: Some(url.clone()), listen: None },
                tunnel_route(),
            ]
        }
        Some(Transport::Tunnel { .. }) => {
            println!("{}", crate::t!("==> {child} came in over the ssh tunnel last time", "==> 前回 {child} は ssh トンネルでつながりました"));
            vec![tunnel_route()]
        }
        None => {
            let listen = env.get("AGENTGW_LINK_LISTEN").map(String::as_str);
            let (tailnet, tailnet_ip) = tailnet_identity(ssh::tailscale_json().as_deref());
            let machine_on_tailnet =
                !ssh::ssh_capture(target, "tailscale status --json 2>/dev/null | head -c 1")
                    .unwrap_or_default()
                    .is_empty();
            let lan = lan_route(
                ssh::fqdn().as_deref(),
                ssh::lan_ip().as_deref(),
                &link_port(listen),
            );
            let mut options = routes_that_work(
                target,
                by_hand.as_deref(),
                Some((tailnet.as_str(), tailnet_ip.as_str())),
                machine_on_tailnet,
                lan,
                listen,
            );
            if let Some(i) = pick_route(&options, ask_which_route(&options)) {
                chosen_first(&mut options, i);
            }
            options
        }
    };

    let state_prefix = remote_state_prefix(
        std::env::var("AGENTGW_STATE_DIR").ok().as_deref(),
        &std::env::var("HOME").unwrap_or_default(),
    );

    // 6. Try them in order. **The machine is installed on the first pass** and only restarted after that
    let mut worked: Option<Transport> = None;
    let mut said: Vec<String> = Vec::new();
    for (i, route) in plan.iter().enumerate() {
        let last = i + 1 == plan.len();
        let transport = match &route.url {
            Some(url) => {
                println!("{}", crate::t!("==> Trying a direct connection to {url}", "==> {url} への直結を試しています"));
                // Open what this route needs — for good this time — and restart so it takes
                if let Some(addr) = &route.listen
                    && crate::setup::add_listen(&dir, addr)
                {
                    println!("{}", crate::t!("==> Opening {addr} for machines. Restarting to apply it.", "==> マシン用に {addr} を開きます。反映のため再起動します。"));
                    crate::service::Service::run("restart", &[]);
                }
                Transport::Direct { url: url.clone() }
            }
            None => {
                println!("{}", crate::t!("==> Connecting {child} over an ssh tunnel", "==> {child} を ssh トンネルでつなぎます"));
                set_tunnel(&dir, &child, Some(target))?;
                Transport::Tunnel { remote_port: TUNNEL_PORT }
            }
        };
        link_child(target, &child, &transport, &inlet, &state_prefix, remote_bin)?;
        if i == 0 {
            println!("{}", crate::t!("==> Installing and starting agentgw on {target}", "==> {target} に agentgw をインストールして起動しています"));
            let installed = ssh::ssh_interactive(
                target,
                &format!("{state_prefix}~/.local/bin/agentgw-install.sh --from ~/{remote_bin}"),
            );
            // The shipped install.sh is single-use. **Clean it up whether or not it worked** (the next add-machine sends it again)
            let _ = ssh::ssh_run(target, "rm -f ~/.local/bin/agentgw-install.sh");
            installed?;
        } else {
            ssh::ssh_run(target, &format!("{state_prefix}~/{remote_bin} restart"))?;
        }
        // The last one is given longer: there is nothing behind it to move on to
        match watch_for_link(&inlet, &child, target, if last { LAST_TRY } else { FIRST_TRY }).await {
            Ok(()) => {
                worked = Some(transport);
                break;
            }
            Err(why) => {
                let label = &route.label;
                println!("{}", crate::t!("==> {label} didn't connect. {why}", "==> {label} ではつながりませんでした。{why}"));
                said.push(format!("{label}: {why}"));
            }
        }
    }

    let Some(transport) = worked else {
        let tried = said.join("\n  ");
        return Err(crate::t!(
            "{child} can't reach the gateway. Every way was tried:\n  {tried}\n\
             Its log: ssh {target} 'tail ~/.local/state/agentgw/plugin-debug.log'",
            "{child} がゲートウェイにつながりません。どの経路もだめでした:\n  {tried}\n\
             {child} のログ: ssh {target} 'tail ~/.local/state/agentgw/plugin-debug.log'"
        ));
    };

    // 7. Write down what actually worked, so the next add-machine starts there
    match &transport {
        Transport::Direct { url } => {
            remember_route(&dir, &child, Some(url));
            // **If a tunnel was set up before, remove it** — otherwise the gateway keeps holding an ssh
            // that nobody uses
            set_tunnel(&dir, &child, None)?;
        }
        Transport::Tunnel { .. } => remember_route(&dir, &child, None),
    }

    let how = match &transport {
        Transport::Direct { url } => crate::t!("directly ({url})", "直結({url})"),
        Transport::Tunnel { .. } => crate::t!("over an ssh tunnel", "ssh トンネル経由"),
    };
    Ok(crate::t!(
        "\n{child} is connected {how}.\nTo hand a Slack channel to it, type `pwd {child}:<path>` in that channel.",
        "\n{child} がつながりました({how})。\nSlack のチャンネルを任せるには、そのチャンネルで `pwd {child}:<パス>` と打ってください。"
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
/// Write down the direct URL this machine came in on, so the next `add-machine` starts there.
fn remember_route(dir: &StateDir, child: &str, url: Option<&str>) {
    let env = RelayCli::env_of(dir);
    let before = env.get("AGENTGW_ROUTES").cloned().unwrap_or_default();
    let after = routes_with(&before, child, url);
    if after == before {
        return;
    }
    if let Err(e) = RelayCli::write_env(dir, &[("AGENTGW_ROUTES", after)]) {
        eprintln!("{}", crate::t!("Couldn't write .env: {e}", ".env が書けません: {e}"));
    }
}

fn set_tunnel(dir: &StateDir, child: &str, target: Option<&str>) -> Result<(), String> {
    let env = RelayCli::env_of(dir);
    let before = env.get("AGENTGW_TUNNELS").cloned().unwrap_or_default();
    let after = tunnels_with(&before, child, target);
    // **Setting one up always restarts**, even when the line is already there: the file saying so is not
    // the same as the running gateway holding it, and a gateway that missed the restart keeps no tunnel
    // while `machines` reads the empty list and draws the machine as direct (seen on a real machine)
    if after == before && target.is_none() {
        return Ok(());
    }
    if after != before {
        RelayCli::write_env(dir, &[("AGENTGW_TUNNELS", after)])
            .map_err(|e| crate::t!("Couldn't write .env: {e}", ".env が書けません: {e}"))?;
    }
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

/// How long to watch a route that has another one behind it. Short: when it doesn't work there is
/// something else to try, and the machine says so itself within a couple of tries.
const FIRST_TRY: std::time::Duration = std::time::Duration::from_secs(20);

/// How long to watch the **last** route before giving up — nothing is behind it. Long enough for the
/// machine and the gateway to restart and for the machine's own backoff (up to 30s) to come round.
const LAST_TRY: std::time::Duration = std::time::Duration::from_secs(90);

/// Watch for the link coming up. **Two ways to tell, both real**: the gateway lists the machine, or the
/// machine itself says it is ready. Checked every second and it stops the moment either says yes —
/// waiting a fixed time called it a failure while the link was coming up one second later.
///
/// When it doesn't come up, the machine's own last word about the link is the reason to show.
async fn watch_for_link(
    inlet: &Listener,
    child: &str,
    target: &str,
    limit: std::time::Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + limit;
    let mut last_word = String::new();
    loop {
        // **The machine is the one dialling**, so ask it first: it knows before the gateway's status
        // does, and it knows why when it doesn't
        if let Ok(line) = ssh::ssh_capture(
            target,
            "grep 'remote link:' ~/.local/state/agentgw/plugin-debug.log | tail -1",
        ) {
            if line.contains("remote link: ready") {
                return Ok(());
            }
            if !line.is_empty() {
                last_word = line;
            }
        }
        // A second opinion for the gateway-dials-the-machine setup, where the machine says nothing
        if let Some(names) = RelayCli::ask_connected(&inlet.listen, &inlet.token).await
            && names.iter().any(|n| n == child)
        {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            let said = match last_word.split_once("remote link: ") {
                Some((_, why)) => why.to_string(),
                None => crate::t!("nothing yet", "まだ何も言っていません"),
            };
            return Err(crate::t!("{child} says: {said}", "{child} はこう言っています: {said}"));
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// One way the machine could reach the gateway, and whether it just did.
pub struct RouteChoice {
    /// What to show a person.
    pub label: String,
    /// `None` = the ssh tunnel (always available: we are already on ssh).
    pub url: Option<String>,
    /// The address the gateway must hold for this route. `None` = it already holds what it needs.
    pub listen: Option<String>,
}

/// Put the chosen route first, leaving the rest in preference order behind it. **They are an order to
/// try, not a shortlist** — measuring says who answers, and only a real link says it works.
fn chosen_first<T>(routes: &mut [T], chosen: usize) {
    routes[..=chosen].rotate_right(1);
}

/// The ssh tunnel, which is always available: we are already on ssh.
fn tunnel_route() -> RouteChoice {
    RouteChoice {
        label: crate::t!(
            "ssh tunnel · the gateway keeps one open to this machine",
            "ssh トンネル · ゲートウェイがこのマシンへ張り続けます"
        ),
        url: None,
        listen: None,
    }
}

/// Pick from what actually worked. **One option is no question**; several is a question worth asking,
/// and with nobody to ask (a pipe, a script) the first one wins — they are in preference order.
pub fn pick_route(options: &[RouteChoice], answer: Option<usize>) -> Option<usize> {
    match options.len() {
        0 => None,
        1 => Some(0),
        n => answer.filter(|i| *i < n).or(Some(0)),
    }
}

/// One address to try: what to show, what the machine would dial, what to ask, and what the gateway
/// must hold for it.
struct Candidate {
    label: String,
    url: String,
    /// `None` = the gateway already holds what this needs (TLS terminated in front of it).
    listen: Option<String>,
}

/// Where `/status` lives for a machine that would dial this URL.
pub fn probe_base(url: &str) -> String {
    let url = url.trim_end_matches('/');
    match url.split_once("://") {
        Some(("wss", rest)) => format!("https://{rest}"),
        Some(("ws", rest)) => format!("http://{rest}"),
        _ => url.to_string(),
    }
}

/// Every route the machine can actually take, measured from the machine. In preference order:
/// the tailnet, then the LAN, then the ssh tunnel (which is always there — we got here over ssh).
///
/// **All of them are asked at once.** They are independent questions, each one an ssh round trip of up
/// to five seconds, and the answers don't depend on each other.
///
/// An address the gateway does not hold yet is opened for the length of the question (`Doorbell`) —
/// otherwise it could never answer and could never be chosen. What the chosen one needs is opened for
/// good afterwards.
fn routes_that_work(
    target: &str,
    by_hand: Option<&str>,
    tailnet: Option<(&str, &str)>,
    machine_on_tailnet: bool,
    lan: Option<(String, String)>,
    listen: Option<&str>,
) -> Vec<RouteChoice> {
    let held = |addr: &str| listen.unwrap_or_default().split(',').any(|a| a.trim() == addr);
    let port = link_port(listen);
    let mut cands: Vec<Candidate> = Vec::new();
    // A URL written by hand is asked first and asked the same way. It is someone's intent, not a fact:
    // one left over from an older setup sent every machine to an address nothing answered at
    if let Some(url) = by_hand.map(str::trim).filter(|u| !u.is_empty()) {
        cands.push(Candidate {
            label: crate::t!("as written · {url}", "手書きの指定 · {url}"),
            url: url.trim_end_matches('/').to_string(),
            listen: None,
        });
    }
    if machine_on_tailnet
        && let Some((name, ip)) = tailnet.filter(|(n, _)| !n.is_empty())
    {
        // Two ways over the tailnet: through whatever terminates TLS on 443 (`tailscale serve`, which
        // forwards to loopback — nothing to open here), or straight to the link port
        cands.push(Candidate {
            label: crate::t!("tailnet · wss://{name}", "tailnet · wss://{name}"),
            url: format!("wss://{name}"),
            listen: None,
        });
        if !ip.is_empty() {
            cands.push(Candidate {
                label: crate::t!("tailnet · ws://{name}:{port}", "tailnet · ws://{name}:{port}"),
                url: format!("ws://{name}:{port}"),
                listen: Some(format!("{ip}:{port}")),
            });
        }
    }
    if let Some((url, addr)) = lan {
        cands.push(Candidate {
            label: crate::t!("LAN · {url}", "LAN · {url}"),
            url,
            listen: Some(addr),
        });
    }
    // The same address twice (a hand-written URL that is also one of the measured ones) is one question
    cands.dedup_by(|a, b| a.url == b.url);

    // Open what needs opening first; a door that won't open takes its candidate with it
    let mut open: Vec<(Candidate, Option<Doorbell>)> = Vec::new();
    for c in cands {
        match &c.listen {
            Some(addr) => match Doorbell::at(addr, held(addr)) {
                Some(bell) => open.push((c, Some(bell))),
                None => println!("{}", crate::t!("==> Can't open {addr} here, so that route is out", "==> ここでは {addr} を開けないので、その経路は除きます")),
            },
            None => open.push((c, None)),
        }
    }
    for (c, _) in &open {
        println!("{}", crate::t!("==> Asking {target} whether the gateway answers at {}", "==> ゲートウェイが {} で答えるか {target} に訊いています", c.url));
    }
    let answered: Vec<bool> = std::thread::scope(|scope| {
        let asks: Vec<_> = open
            .iter()
            .map(|(c, _)| scope.spawn(|| gateway_answers(target, &probe_base(&c.url))))
            .collect();
        asks.into_iter().map(|a| a.join().unwrap_or(false)).collect()
    });

    let mut out: Vec<RouteChoice> = open
        .into_iter()
        .zip(answered)
        .filter(|(_, yes)| *yes)
        .map(|((c, _), _)| RouteChoice {
            label: c.label,
            url: Some(c.url),
            listen: c.listen,
        })
        .collect();
    out.push(tunnel_route());
    out
}

/// Show what worked and take the answer. **Only when there is a choice and someone to make it** —
/// a pipe or a script gets the first (preference order), with a line saying which.
fn ask_which_route(options: &[RouteChoice]) -> Option<usize> {
    use std::io::IsTerminal;
    if options.len() < 2 {
        return None;
    }
    println!("{}", crate::t!("==> Ways this machine can reach the gateway:", "==> このマシンからゲートウェイへ届く経路:"));
    for (i, o) in options.iter().enumerate() {
        let n = i + 1;
        println!("  {n}) {}", o.label);
    }
    if !std::io::stdin().is_terminal() {
        let first = &options[0].label;
        println!("{}", crate::t!("  Taking {first}", "  {first} を選びます"));
        return None;
    }
    let answer = crate::setup::prompt(&crate::t!("  Which one? [1]: ", "  どれにしますか? [1]: "));
    answer.trim().parse::<usize>().ok().map(|n| n.saturating_sub(1))
}

/// Hold an address open for as long as the guard lives, **answering exactly as the gateway would**,
/// just so a machine can try to reach it. The running gateway is not touched — this is an address it
/// does not hold yet.
///
/// Dropping it closes the socket *before* returning, so the gateway can bind the same address a moment
/// later when this turns out to be the route.
struct Doorbell {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Doorbell {
    /// `None` = can't open it here, so there is no point asking the machine.
    fn at(addr: &str, already_open: bool) -> Option<Doorbell> {
        if already_open {
            return Some(Doorbell { stop: Default::default(), thread: None });
        }
        let listener = std::net::TcpListener::bind(addr).ok()?;
        listener.set_nonblocking(true).ok()?;
        let stop: std::sync::Arc<std::sync::atomic::AtomicBool> = Default::default();
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            use std::io::{Read, Write};
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                let Ok((mut sock, _)) = listener.accept() else {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                };
                let _ = sock.set_nonblocking(false);
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_millis(500)));
                let _ = sock.read(&mut [0u8; 1024]);
                let _ = sock.write_all(UNAUTHORIZED.as_bytes());
            }
        });
        Some(Doorbell { stop, thread: Some(thread) })
    }
}

impl Drop for Doorbell {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What `/status` says to someone with no token — the gateway saying its own name, with no secret in it.
const UNAUTHORIZED: &str =
    "HTTP/1.1 401 Unauthorized\r\nContent-Length: 12\r\nConnection: close\r\n\r\nunauthorized";

/// Does **our gateway** answer at this address? Asked from the machine, because that is who will dial.
///
/// Not "is the port open": something else answering on 443 passed a plain TCP check and the link then
/// failed, after the address had already been written down (seen on a real machine). `/status` without a
/// token answers `unauthorized`, which nothing else does — and no secret goes over the wire to ask.
fn gateway_answers(target: &str, base: &str) -> bool {
    let probe = format!("curl -s -m 5 {base}/status");
    ssh::ssh_capture(target, &probe)
        .unwrap_or_default()
        .contains("unauthorized")
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

    /// `machines` shows where each machine can be reached **on the interface the link uses**. A machine
    /// with tailscale that links over the LAN is at its LAN address — naming its tailnet there would
    /// point at an interface the link doesn't use (seen on a real machine).
    #[test]
    fn reachable_at_names_the_interface_the_link_uses() {
        let json = r#"{"Self":{"DNSName":"pve.tail1234.ts.net.","TailscaleIPs":["100.89.207.102"]}}"#;
        assert_eq!(
            reachable_at(Some(json), "pve", Some("pve.lan"), Some("100.89.207.102")),
            ("pve.tail1234.ts.net".to_string(), "100.89.207.102".to_string()),
            "out over the tailnet"
        );
        assert_eq!(
            reachable_at(Some(json), "pve", Some("pve.lan"), Some("192.168.10.20")),
            ("pve.lan".to_string(), "192.168.10.20".to_string()),
            "tailscale is installed, but the link goes over the LAN"
        );
        // No tailscale at all is the normal case on a plain server, and `hostname -f` may say nothing
        assert_eq!(
            reachable_at(None, "pve", None, Some("192.168.10.20")),
            ("pve".to_string(), "192.168.10.20".to_string())
        );
        // Nothing dialled (the gateway asking about itself): a name, and no interface to name
        assert_eq!(reachable_at(None, " pve\n", None, None), ("pve".to_string(), String::new()));
    }

    #[test]
    fn a_dial_url_gives_the_host_and_port_to_ask_the_routing_table_about() {
        assert_eq!(host_port_of("ws://dock.lan:8787"), Some(("dock.lan".to_string(), 8787)));
        assert_eq!(host_port_of("wss://dock.example.ts.net"), Some(("dock.example.ts.net".to_string(), 443)));
        assert_eq!(host_port_of("wss://dock.example.ts.net/bridge/pve"), Some(("dock.example.ts.net".to_string(), 443)));
        assert_eq!(host_port_of("dock.lan"), None);
    }

    #[test]
    fn tailnet_identity_reads_the_name_and_the_v4_address() {
        let json = r#"{"Self":{"DNSName":"pve.tail1234.ts.net.","TailscaleIPs":["fd7a::1","100.89.207.102"]}}"#;
        assert_eq!(
            tailnet_identity(Some(json)),
            ("pve.tail1234.ts.net".to_string(), "100.89.207.102".to_string())
        );
        assert_eq!(tailnet_identity(None), (String::new(), String::new()));
        assert_eq!(tailnet_identity(Some("not json")), (String::new(), String::new()));
    }

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
        let s = choose_source(true, Some(PathBuf::from("/t/agentgw")), "0.18.5", "x86_64-linux").unwrap();
        assert_eq!(
            s,
            Source::Release {
                tag: "v0.18.5".to_string()
            }
        );
    }

    #[test]
    fn falls_back_to_the_local_build_without_gh() {
        let s = choose_source(false, Some(PathBuf::from("/t/agentgw")), "0.18.5", "x86_64-linux").unwrap();
        assert_eq!(s, Source::Dist(PathBuf::from("/t/agentgw")));
    }

    /// No gh is the normal case on a server. The repository is public, so the same release asset comes
    /// down over plain HTTPS instead of stopping with "run gh auth login".
    #[test]
    fn without_gh_or_a_local_build_it_fetches_the_public_release() {
        let s = choose_source(false, None, "0.18.5", "x86_64-unknown-linux-musl").unwrap();
        assert_eq!(
            s,
            Source::Url {
                url: "https://github.com/takashito/agentgw/releases/download/v0.18.5/agentgw-x86_64-unknown-linux-musl".to_string()
            }
        );
    }

    /// The ports the gateway accepts on, once each — a name reaches whatever address it resolves to.
    #[test]
    fn a_dial_url_points_at_where_status_lives() {
        assert_eq!(probe_base("wss://dock.example.ts.net"), "https://dock.example.ts.net");
        assert_eq!(probe_base("ws://dock.lan:8787"), "http://dock.lan:8787");
        assert_eq!(probe_base("wss://dock.example.ts.net/"), "https://dock.example.ts.net");
    }

    #[test]
    fn link_port_comes_from_what_is_open() {
        assert_eq!(link_port(Some("127.0.0.1:8788,192.168.10.11:8788")), "8788");
        // Nothing set yet → the default, which is what `install` writes
        assert_eq!(link_port(None), "8787");
        assert_eq!(link_port(Some("")), "8787");
    }

    /// Once a machine has come in, the way it came is written down: `add-machine` starts there next time
    /// instead of measuring everything again. `-n` is what asks for a fresh look.
    #[test]
    fn the_route_that_worked_is_remembered() {
        assert_eq!(
            remembered_route("", "pve=ws://dock.lan:8787", "pve"),
            Some(Transport::Direct { url: "ws://dock.lan:8787".into() })
        );
        assert_eq!(
            remembered_route("pve=root@pve.lan", "", "pve"),
            Some(Transport::Tunnel { remote_port: TUNNEL_PORT })
        );
        // A tunnel is the gateway's own doing, so it wins over a stale direct URL
        assert_eq!(
            remembered_route("pve=root@pve.lan", "pve=ws://dock.lan:8787", "pve"),
            Some(Transport::Tunnel { remote_port: TUNNEL_PORT })
        );
        assert_eq!(remembered_route("", "", "pve"), None);
        assert_eq!(remembered_route("", "mac=ws://x:1", "pve"), None);

        assert_eq!(routes_with("", "pve", Some("ws://dock.lan:8787")), "pve=ws://dock.lan:8787");
        assert_eq!(routes_with("pve=ws://a:1,mac=ws://b:2", "pve", None), "mac=ws://b:2");
    }

    fn choice(label: &str, url: Option<&str>) -> RouteChoice {
        RouteChoice { label: label.into(), url: url.map(str::to_string), listen: None }
    }

    /// The door is open while it is asked about, and **shut again before the gateway is told to bind
    /// it** — a socket left behind for a few seconds would meet the restart with "Address already in use".
    #[test]
    fn a_doorbell_opens_an_address_and_closes_it_on_drop() {
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let addr = format!("127.0.0.1:{port}");
        let bell = Doorbell::at(&addr, false).expect("can open it");
        // It answers the way the gateway does, so one probe fits an address it holds and one it doesn't
        use std::io::{Read, Write};
        let mut sock = std::net::TcpStream::connect(&addr).expect("a machine can reach it while asking");
        sock.write_all(b"GET /status HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut said = String::new();
        sock.read_to_string(&mut said).unwrap();
        assert!(said.contains("unauthorized"), "said: {said}");
        drop(bell);
        assert!(std::net::TcpListener::bind(&addr).is_ok(), "the gateway can take it over right after");

        // An address it already holds is not opened a second time
        let held = Doorbell::at(&addr, true).expect("nothing to do");
        assert!(held.thread.is_none());
    }

    #[test]
    fn the_chosen_route_goes_first_and_the_rest_keep_their_order() {
        let mut order = ["tailnet", "lan", "tunnel"];
        chosen_first(&mut order, 1);
        assert_eq!(order, ["lan", "tailnet", "tunnel"]);
        chosen_first(&mut order, 0);
        assert_eq!(order, ["lan", "tailnet", "tunnel"], "the first one chosen changes nothing");
    }

    /// One way that works needs no question. Several is a choice, and with nobody to ask (a pipe) the
    /// first wins — they come in preference order.
    #[test]
    fn one_route_is_taken_and_several_are_offered() {
        let tunnel = vec![choice("ssh tunnel", None)];
        assert_eq!(pick_route(&tunnel, None), Some(0));
        assert!(pick_route(&[], None).is_none());

        let both = vec![choice("tailnet", Some("wss://a")), choice("ssh tunnel", None)];
        assert_eq!(pick_route(&both, None), Some(0), "nobody to ask → the preferred one");
        assert_eq!(pick_route(&both, Some(1)), Some(1), "the answer is taken");
        assert_eq!(pick_route(&both, Some(9)), Some(0), "an answer out of range is not one");
    }

    /// The route is decided from **both** sides: the tailnet only when the machine is on it too, then a
    /// LAN address the machine actually reached, and a hand-set URL over everything.
    #[test]
    fn the_route_follows_what_both_sides_have() {
        assert_eq!(
            route_url(Some("wss://set.by.hand/"), Some("dock.tailnet.ts.net"), true, None).as_deref(),
            Some("wss://set.by.hand")
        );
        assert_eq!(
            route_url(None, Some("dock.tailnet.ts.net"), true, Some("ws://dock.lan:8787")).as_deref(),
            Some("wss://dock.tailnet.ts.net")
        );
        // The gateway is on the tailnet, the machine is not → the LAN it proved it can reach
        assert_eq!(
            route_url(None, Some("dock.tailnet.ts.net"), false, Some("ws://dock.lan:8787")).as_deref(),
            Some("ws://dock.lan:8787")
        );
        assert_eq!(route_url(None, None, false, None), None); // nothing left but a tunnel
    }

    /// What a machine could dial from the LAN: the gateway's own listeners, minus the ones that mean
    /// "this machine" on the other side.
    #[test]
    fn lan_route_pairs_a_name_with_the_address_to_open() {
        assert_eq!(
            lan_route(Some("dock.lan"), Some("192.168.10.11"), "8787"),
            Some(("ws://dock.lan:8787".to_string(), "192.168.10.11:8787".to_string()))
        );
        // No name others can resolve → dial the address itself
        assert_eq!(
            lan_route(Some("dock"), Some("192.168.10.11"), "8787"),
            Some(("ws://192.168.10.11:8787".to_string(), "192.168.10.11:8787".to_string()))
        );
        // No LAN address of our own → no LAN route to offer
        assert_eq!(lan_route(Some("dock.lan"), None, "8787"), None);
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
