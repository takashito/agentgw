//! 子を足すときの判断。**OS を叩かない** — ここは全部テストできる純関数。
//!
//! ssh / scp / gh / tailscale を呼ぶ層は [`remote`](crate::fleet::remote)。

use std::path::{Path, PathBuf};

/// 相手の `uname -sm` → rust の triple。知らない組み合わせは `None`
/// (= 相手のマシンでビルドする枝)。
pub fn triple_for(uname_sm: &str) -> Option<&'static str> {
    Some(match uname_sm.trim() {
        "Linux x86_64" => "x86_64-unknown-linux-musl",
        "Linux aarch64" | "Linux arm64" => "aarch64-unknown-linux-musl",
        "Darwin arm64" => "aarch64-apple-darwin",
        "Darwin x86_64" => "x86_64-apple-darwin",
        _ => return None,
    })
}

/// 手元で選んだ状態ディレクトリを、相手のシェルに渡す形にする。
///
/// **`$HOME` は相手で違う**ので、ホームの下なら `~` 相対に直して向こうのシェルに
/// 展開させる。付けないと相手は既定の `~/.local/state/agentgw` に落ちる。
pub fn remote_state_prefix(state_dir: Option<&str>, home: &str) -> String {
    match state_dir {
        None => String::new(),
        Some(dir) => match dir.strip_prefix(&format!("{home}/")) {
            Some(rel) => format!("AGENTGW_STATE_DIR=\"$HOME/{rel}\" "),
            None => format!("AGENTGW_STATE_DIR='{dir}' "),
        },
    }
}

/// 送る物の出どころ。
#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    /// GitHub release から落とす(親と子の版が自動で揃う)
    Release { tag: String },
    /// `cargo dist` が焼いた手元の成果物
    Dist(PathBuf),
}

/// 非公開リポジトリ。**落とすのは親だけ** — 子に gh も認証も要らない。
pub const REPO: &str = "takashito/agentgw";

/// `gh release download` の引数。**組み立てだけ**(ここでは叩かない)。
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

/// **release が先、手元の成果物が後。** release から落とすと親と子が必ず同じ版になる。
///
/// `dist` は「その triple の成果物が実在するなら、そのパス」。呼び手が確かめて渡す。
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

// ── 通り道 ───────────────────────────────────────────────────────────────────

/// 子が親につなぐ通り道。**検出ではなく実測で決める** — 子を起こして、親の
/// `ask_connected` に名前が出たかで判定する(`relay.rs:2488`)。
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Transport {
    /// 子が親の公開名に直接 dial する(tailnet など)
    Direct { url: String },
    /// 親が張る ssh トンネル越しに、子が自分の loopback へ dial する
    Tunnel { remote_port: u16 },
}

/// トンネルを使うときの、**子の loopback 側**のポート。
pub const TUNNEL_PORT: u16 = 8799;

/// 直結を試す候補。`.env` に覚えている URL が最優先、無ければ tailscale の名前。
///
/// `tailscale status --json` の `Self.DNSName` は**末尾にドットが付く**(実測:
/// `mac.tail1234.ts.net.`)。落としてから組む。
pub fn candidate_url(env_url: Option<&str>, tailscale_json: Option<&str>) -> Option<String> {
    if let Some(u) = env_url.map(str::trim).filter(|u| !u.is_empty()) {
        return Some(u.trim_end_matches('/').to_string());
    }
    let json: serde_json::Value = serde_json::from_str(tailscale_json?).ok()?;
    let name = json["Self"]["DNSName"].as_str()?.trim_end_matches('.');
    (!name.is_empty()).then(|| format!("wss://{name}"))
}

/// 子の `.env` に書く URL。
pub fn dial_url(t: &Transport) -> String {
    match t {
        Transport::Direct { url } => url.clone(),
        Transport::Tunnel { remote_port } => format!("ws://127.0.0.1:{remote_port}"),
    }
}

/// 親が張り続ける ssh の引数。**-N でコマンドは流さない。**
///
/// `ExitOnForwardFailure=yes` が要る — 無いと転送に失敗しても ssh だけ生き残り、
/// 「繋がっているのに届かない」状態になる。
pub fn tunnel_ssh_args(target: &str, remote_port: u16, parent_addr: &str) -> Vec<String> {
    [
        "-N",
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=30",
        "-o",
        "ServerAliveCountMax=3",
        "-R",
        &format!("127.0.0.1:{remote_port}:{parent_addr}"),
        target,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// `.env` の `AGENTGW_TUNNELS`(`laptop=me@laptop,desktop=me@desktop`)に、子1台分を足す / 外す。
///
/// **トンネルは親の agentgw が自分で張る**(別サービスにしない — agentgw が動いている間だけ
/// 繋がっていればよい)。起動時にこの一覧を読んで、子ごとに ssh を1本ずつ見張る。
pub fn tunnels_with(raw: &str, child: &str, target: Option<&str>) -> String {
    let mut list: Vec<(String, String)> = crate::bridge::link::child_urls(raw)
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

// ── 実行部 ───────────────────────────────────────────────────────────────────
// ここは OS と相手のマシンを叩く層。判断は上の純関数が持つ(そちらがテスト済み)。

pub mod remote;

use crate::bridge::relay::Cli as RelayCli;
use crate::bridge::relay::link;
use crate::bridge::state::StateDir;

/// 子に送るブートストラップ。**バイナリに焼き込む** — release から入れた親には
/// repo が無いので、ファイルとして探すと見つからない。
const INSTALL_SH: &str = include_str!("../scripts/install.sh");

/// 親の口と鍵。無ければ作って `.env` に書き、鍵を新しく作ったときだけ restart する。
pub struct Inlet {
    pub listen: String,
    pub token: String,
    pub name: String,
}

/// `agentgw add-child <ssh先> [--name <名前>] [--from <パス>]`
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

/// 子を1台足す。**順番に意味がある** — 詳細は各ステップのコメント。
async fn add_child(target: &str, name: Option<&str>, from: Option<&str>) -> Result<String, String> {
    let dir = StateDir::resolve();

    // 1. 相手を見る
    println!("{}", crate::t!("==> Checking {target}", "==> {target} を確認しています"));
    let uname = remote::ssh_capture(target, "uname -sm")?;
    let triple = triple_for(&uname)
        .ok_or_else(|| crate::t!("There is no prebuilt binary for {uname}. Build and install agentgw there by hand.", "{uname} 用のバイナリは配布していません。そのマシンでビルドして入れてください。"))?;
    println!("  {uname} ({triple})");

    // 名前は明示が最優先。無ければ相手のホスト名を使う(**自動命名の推測はここだけ**)
    let child = match name {
        Some(n) => n.trim().to_string(),
        None => remote::ssh_capture(target, "hostname -s 2>/dev/null || hostname")
            .unwrap_or_default()
            .trim()
            .to_string(),
    };
    if child.is_empty() {
        return Err(crate::t!("Couldn't work out this machine's name. Pass --name <name>.", "このマシンの名前が決められません。--name <名前> を付けてください"));
    }
    println!("{}", crate::t!("  Name: {child}", "  名前: {child}"));

    // 2. 送る物を選ぶ(release が先、cargo dist の成果物が後)
    let staging = std::env::temp_dir().join(format!("agentgw-add-{}", std::process::id()));
    let binary = match from {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let dist = dist_artifact(triple);
            match choose_source(remote::gh_ready(), dist, env!("CARGO_PKG_VERSION"))? {
                Source::Release { tag } => {
                    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
                    println!("{}", crate::t!("==> Downloading release {tag} from GitHub", "==> GitHub から {tag} をダウンロードしています"));
                    remote::gh_download(
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

    // 3. 届ける(バイナリと install.sh)
    println!("{}", crate::t!("==> Copying agentgw to {target}", "==> {target} に agentgw をコピーしています"));
    let remote_bin = ".local/bin/agentgw";
    remote::ssh_run(target, "mkdir -p ~/.local/bin")?;
    // 走っているバイナリは上書きできない。先に退けてから置く
    remote::ssh_run(
        target,
        &format!("[ -e {remote_bin} ] && mv -f {remote_bin} {remote_bin}.old || true"),
    )?;
    remote::scp(&binary, target, remote_bin)?;
    remote::ssh_run(
        target,
        &format!("chmod 755 {remote_bin} && rm -f {remote_bin}.old"),
    )?;
    remote::put_text(target, ".local/bin/agentgw-install.sh", INSTALL_SH)?;
    remote::ssh_run(target, "chmod 755 .local/bin/agentgw-install.sh")?;
    let _ = std::fs::remove_dir_all(&staging);

    // 4. 親の口と鍵を用意する
    let inlet = ensure_inlet(&dir)?;

    // 5. 直結を試す。候補が無ければ最初からトンネル
    let candidate = {
        let env = RelayCli::env_of(&dir);
        candidate_url(
            env.get("AGENTGW_LINK_PUBLIC_URL").map(String::as_str),
            remote::tailscale_json().as_deref(),
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

    // 6. 子を起こして、親から見えるかを確かめる
    println!("{}", crate::t!("==> Installing and starting agentgw on {target}", "==> {target} に agentgw をインストールして起動しています"));
    let installed = remote::ssh_interactive(
        target,
        &format!("{state_prefix}~/.local/bin/agentgw-install.sh --from ~/{remote_bin}"),
    );
    // 送り込んだ install.sh は使い捨て。**成否に関わらず片付ける**(次の add-child がまた送る)
    let _ = remote::ssh_run(target, "rm -f ~/.local/bin/agentgw-install.sh");
    installed?;

    if !wait_connected(&inlet, &child).await {
        // 直結が駄目だったなら、トンネルに落ちてもう一度
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
            remote::ssh_run(target, &format!("{state_prefix}~/{remote_bin} restart"))?;
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
        // 直結で繋がった。**前にトンネルを張っていたなら外す** — 残すと、使われない
        // ssh を親が張り続ける
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

/// 親の口と鍵。無ければ作って `.env` に書き、**鍵を新しく作ったときだけ**起こし直す
/// (走っている Bridge は古い鍵を握ったままなので、子が来ても 401 になる)。
fn ensure_inlet(dir: &StateDir) -> Result<Inlet, String> {
    let env = RelayCli::env_of(dir);
    let listen = env
        .get("AGENTGW_LINK_LISTEN")
        .cloned()
        .unwrap_or_else(|| crate::bridge::relay::DEFAULT_LISTEN.to_string());
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
    Ok(Inlet {
        listen,
        token,
        name,
    })
}

/// 接続文字列を作って子に食わせる。**argv に置かない**(相手の ps と履歴に残る)。
fn link_child(
    target: &str,
    child: &str,
    transport: &Transport,
    inlet: &Inlet,
    state_prefix: &str,
    remote_bin: &str,
) -> Result<(), String> {
    let conn = link::encode_connection(&link::Invite {
        url: dial_url(transport),
        api_token: inlet.token.clone(),
    });
    remote::ssh_stdin(
        target,
        &format!("{state_prefix}~/{remote_bin} link --name '{child}' -"),
        &format!("{conn}\n"),
    )?;
    Ok(())
}

/// 親の `.env` の `AGENTGW_TUNNELS` を書き換え、**変わったときだけ**親を起こし直す
/// (トンネルは親の agentgw が起動時に読んで張る。restart はワーカーを畳まない)。
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

/// 親の agentgw の中で、子1台分の ssh トンネルを張り続ける。
///
/// **別サービスにしない。** agentgw が動いている間だけ見張ればよいので、子プロセスとして持つ。
///
/// **ssh の多重化(`ControlMaster auto` + `ControlPersist`)はそのまま使う。** そのときの ssh は
/// 既にある親玉に転送を預けて、すぐ**終了 0** で抜ける(2026-09-18 実機)。これは失敗ではない —
/// 転送は親玉の中で生きている。なので 0 で抜けたら間を空けて頼み直すだけにする(親玉が
/// 居なくなっていれば、次の ssh が新しい親玉になって転送を持つ)。多重化を使っていない
/// 設定なら ssh は前に居続け、`kill_on_drop` で agentgw と一緒に消える。
pub async fn keep_tunnel(
    fleet: std::sync::Arc<crate::bridge::relay::Fleet>,
    child: String,
    target: String,
    parent_addr: String,
) {
    use crate::bridge::state::LogCtx;
    let args = tunnel_ssh_args(&target, TUNNEL_PORT, &parent_addr);
    // ログは状態が変わったときだけ(1分ごとの頼み直しで plugin-debug.log を埋めない)
    let mut was_ok: Option<bool> = None;
    loop {
        let out = tokio::process::Command::new("ssh")
            .args(&args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await;
        let (ok, why) = match out {
            Ok(o) if o.status.success() => (true, String::new()),
            // 抜けた理由を残す。黙って張り直し続けると、鍵が無いのか相手が居ないのか分からない
            Ok(o) => (
                false,
                format!(
                    "exited ({}) {}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
            ),
            Err(e) => (false, format!("could not start ssh: {e}")),
        };
        if was_ok != Some(ok) {
            // `status` に経路を出すため、親の手元に今の様子を置く
            fleet.tunnels.lock().unwrap().insert(
                child.clone(),
                crate::bridge::relay::Tunnel {
                    target: target.clone(),
                    error: (!ok).then(|| why.clone()),
                },
            );
            if ok {
                LogCtx::default().info(
                    "relay",
                    &format!(
                        "tunnel {child}: up via ssh {target} (child 127.0.0.1:{TUNNEL_PORT} -> {parent_addr})"
                    ),
                );
            } else {
                LogCtx::default().error("relay", &format!("tunnel {child}: {why} — retrying"));
            }
            was_ok = Some(ok);
        }
        let wait = if ok { 60 } else { 5 };
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
    }
}

/// 親の `status` 口に「今つながっている子」を訊いて、名前が出るまで待つ。
///
/// **これが通り道の判定**(probe ではなく本物の link が張れたか)。
async fn wait_connected(inlet: &Inlet, child: &str) -> bool {
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

/// `cargo dist` の成果物。**repo があるときだけ**見つかる。
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
        // `ssh 相手 'uname -sm'` は改行つきで返る
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
        // Self.DNSName は末尾にドットが付く(実測)
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
    fn the_tunnel_forwards_the_childs_loopback_to_the_parents_listener() {
        let args = tunnel_ssh_args("me@laptop", 8799, "127.0.0.1:8787");
        assert!(
            args.contains(&"-N".to_string()),
            "コマンドは流さない: {args:?}"
        );
        assert!(
            args.contains(&"127.0.0.1:8799:127.0.0.1:8787".to_string()),
            "子の 8799 を親の listener へ: {args:?}"
        );
        assert!(
            args.contains(&"ExitOnForwardFailure=yes".to_string()),
            "転送に失敗したら黙って生き残らない: {args:?}"
        );
        assert_eq!(args.last().unwrap(), "me@laptop", "ssh 先は最後: {args:?}");
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
