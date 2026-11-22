//! Changing which machine handles a channel, and forgetting a machine, **from the gateway's terminal**:
//! `agentgw route <channel> <machine>[:<folder>]`, `agentgw unroute <channel>` and
//! `agentgw remove-machine <name>`.
//!
//! The Slack `route` only works when the Owner types it in the channel itself, so an agent asked to
//! move a channel had nothing to type it with, and a machine that was returned for good had no command
//! at all. Both ended with access.json edited by hand. These run **inside the gateway** (the CLI only
//! asks over the local link port, like `update`): the gateway is one of two writers of access.json,
//! and a second process writing it would undo the other's changes.
//!
//! **Each one first says what it would change, and changes nothing** unless asked with `apply`.

use super::*;
use crate::bridge::state::Route;
use serde::{Deserialize, Serialize};

/// One request from the CLI (`POST /admin`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum AdminOp {
    /// Hand a channel to a machine, in a folder there (`None` = that machine's home folder).
    Route { channel: String, machine: String, path: Option<String> },
    /// Take the channel's assignment away: it is handled by the gateway again.
    Unroute { channel: String },
    /// Forget a machine: its record, and the assignment of every channel it handles.
    RemoveMachine { machine: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminRequest {
    #[serde(flatten)]
    pub op: AdminOp,
    /// `false` = only say what would change.
    #[serde(default)]
    pub apply: bool,
}

/// How long `route` waits for the machine to check the folder.
const FOLDER_WAIT: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_millis(300)
} else {
    std::time::Duration::from_secs(20)
};

/// What a change would do, worked out from the records alone (pure, so it is tested without a fleet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Lines for the person, saying what changes.
    pub lines: Vec<String>,
    /// The channels whose assignment goes, with the machine that had them.
    pub drop_routes: Vec<(String, String)>,
}

/// How a channel is shown: `#name (C…)`, or just the ID when the name is unknown.
fn shown(channel: &str, names: &HashMap<String, String>) -> String {
    match names.get(channel) {
        Some(n) => format!("#{n} ({channel})"),
        None => channel.to_string(),
    }
}

/// `route`: what moves. Refused when the machine is not connected right now — the same rule as the
/// Slack `route`, since only a connected machine can check the folder.
pub fn plan_route(
    access: &Access,
    connected: &[String],
    self_id: &str,
    channel: &str,
    machine: &str,
    path: Option<&str>,
    names: &HashMap<String, String>,
) -> Result<Plan, String> {
    if !connected.iter().any(|c| c == machine) {
        return Err(unknown_machine(machine, connected));
    }
    let row = access.routes.get(channel);
    let was_machine = row.and_then(|r| r.bridge.clone()).unwrap_or_else(|| self_id.to_string());
    let was_folder = row.and_then(|r| r.repo_path.clone()).unwrap_or_else(|| "~".into());
    let folder = path.unwrap_or("~");
    let assigned = row.is_some_and(|r| r.bridge.is_some());
    let mut lines = vec![shown(channel, names)];
    lines.push(match assigned {
        true => crate::t!("  machine: {was_machine} → {machine}", "  マシン: {was_machine} → {machine}"),
        false => crate::t!(
            "  machine: none (the gateway, {was_machine}) → {machine}",
            "  マシン: なし(ゲートウェイ {was_machine})→ {machine}"
        ),
    });
    lines.push(crate::t!(
        "  folder:  {was_folder} → {folder} (checked on {machine} first; nothing changes if it is not there)",
        "  フォルダ: {was_folder} → {folder}(先に {machine} で有るか確かめます。無ければ何も変えません)"
    ));
    if assigned && was_machine != machine {
        lines.push(crate::t!(
            "  Threads already running in it stay on {was_machine}; new threads start on {machine}.",
            "  動いているスレッドは {was_machine} に残ります。新しいスレッドから {machine} で始まります。"
        ));
    } else {
        lines.push(crate::t!(
            "  Threads already running in it stay where they are; new threads start there.",
            "  動いているスレッドはそのままです。新しいスレッドからそこで始まります。"
        ));
    }
    Ok(Plan { lines, drop_routes: vec![] })
}

/// `unroute`: what goes. Refused for a channel with no assignment — there is nothing to take away.
pub fn plan_unroute(
    access: &Access,
    self_id: &str,
    channel: &str,
    names: &HashMap<String, String>,
) -> Result<Plan, String> {
    let Some(row) = access.routes.get(channel) else {
        return Err(crate::t!(
            "{} has no assignment; it is already handled by the gateway ({self_id}).",
            "{} には割り当てがありません。すでにゲートウェイ({self_id})が受け持っています。",
            shown(channel, names),
        ));
    };
    let machine = row.bridge.clone().unwrap_or_else(|| self_id.to_string());
    let mut lines = vec![shown(channel, names)];
    lines.extend(row_lines(&machine, row));
    lines.push(crate::t!(
        "  After this, a mention there is handled by the gateway ({self_id}) in its home folder. \
         Threads already running in it stay on {machine}.",
        "  この後、そこでのメンションはゲートウェイ({self_id})が家のディレクトリで受けます。\
         動いているスレッドは {machine} に残ります。"
    ));
    Ok(Plan { lines, drop_routes: vec![(channel.to_string(), machine)] })
}

/// `remove-machine`: the record and every channel it handles. Refused for the gateway itself, for a
/// machine connected right now (it would be recorded again at once — stop agentgw on it first), and
/// for a name nothing knows.
pub fn plan_remove_machine(
    access: &Access,
    connected: &[String],
    self_id: &str,
    machine: &str,
    names: &HashMap<String, String>,
) -> Result<Plan, String> {
    if machine == self_id {
        return Err(crate::t!(
            "{machine} is this gateway; it cannot remove itself.",
            "{machine} はこのゲートウェイです。自分は外せません。"
        ));
    }
    let record = access.machines.get(machine);
    let channels: Vec<(&String, &Route)> =
        access.routes.iter().filter(|(_, r)| r.bridge.as_deref() == Some(machine)).collect();
    if record.is_none() && channels.is_empty() {
        let known: Vec<String> = access.machines.keys().map(|m| format!("`{m}`")).collect();
        return Err(crate::t!(
            "No machine named `{machine}` is recorded. Recorded: {}",
            "`{machine}` という名前のマシンは記録にありません。記録にあるのは: {}",
            if known.is_empty() { "-".to_string() } else { known.join(", ") },
        ));
    }
    if connected.iter().any(|c| c == machine) {
        return Err(crate::t!(
            "{machine} is connected right now, so it would be recorded again at once. \
             Stop agentgw on it first (`agentgw uninstall` there), then run this again.",
            "{machine} はいまつながっているので、外してもすぐ記録し直されます。\
             先にそのマシンの agentgw を止めて(そこで `agentgw uninstall`)から、もう一度打ってください。"
        ));
    }
    let mut lines = vec![crate::t!("Machine {machine}", "マシン {machine}")];
    match record {
        Some(l) => {
            let seen = if l.last_seen.is_empty() { "-" } else { l.last_seen.as_str() };
            let version = if l.version.is_empty() { "-" } else { l.version.as_str() };
            lines.push(crate::t!(
                "  record: removed (last seen {seen}, version {version})",
                "  記録: 消します(最後に見えたのは {seen}、版 {version})"
            ));
            if let Some(ssh) = &l.ssh_target {
                lines.push(crate::t!(
                    "  ssh tunnel to {ssh}: no longer kept from the next gateway restart",
                    "  {ssh} への ssh トンネル: 次にゲートウェイを起こし直したときから張りません"
                ));
            }
        }
        None => lines.push(crate::t!(
            "  record: none (only channels name it)",
            "  記録: なし(チャンネルが名前を持っているだけ)"
        )),
    }
    let mut drop_routes = vec![];
    if channels.is_empty() {
        lines.push(crate::t!("  channels: none assigned", "  チャンネル: 割り当てなし"));
    } else {
        lines.push(crate::t!(
            "  channels whose assignment goes (handled by the gateway, {self_id}, afterwards):",
            "  割り当てを外すチャンネル(この後はゲートウェイ {self_id} が受け持ちます):"
        ));
        for (ch, row) in channels {
            lines.push(format!("  - {}", shown(ch, names)));
            lines.extend(row_lines(machine, row).into_iter().map(|l| format!("  {l}")));
            drop_routes.push((ch.clone(), machine.to_string()));
        }
    }
    lines.push(crate::t!(
        "  If {machine} connects again, it is recorded again but does not get these channels back.",
        "  {machine} がまたつながると記録はされ直しますが、これらのチャンネルは戻りません。"
    ));
    Ok(Plan { lines, drop_routes })
}

/// What a channel's row holds, as lines of what is lost with it.
fn row_lines(machine: &str, row: &Route) -> Vec<String> {
    let folder = row.repo_path.as_deref().unwrap_or("~");
    let mut lines = vec![crate::t!(
        "  assignment: {machine}, folder {folder} — removed",
        "  割り当て: {machine}、フォルダ {folder} — 消します"
    )];
    if let Some(tools) = row.allowed_tools.as_ref().filter(|t| !t.is_empty()) {
        lines.push(crate::t!(
            "  tools allowed with \"don't ask again\" here ({}): {} — removed",
            "  ここで「次から聞かない」にしたツール({}件): {} — 消します",
            tools.len(),
            tools.join(", "),
        ));
    }
    lines
}

/// The CLI's arguments → a request. `Err` is the usage to print.
pub fn parse_cli(cmd: &str, args: &[String]) -> Result<AdminRequest, String> {
    let apply = args.iter().any(|a| a == "--yes" || a == "-y");
    let words: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let op = match (cmd, words.as_slice()) {
        ("route", [channel, target]) => {
            let (machine, path) = match target.split_once(':') {
                Some((m, p)) => (m.to_string(), Some(p.to_string()).filter(|p| !p.is_empty())),
                None => (target.to_string(), None),
            };
            if !crate::bridge::state::is_machine_name(&machine) {
                return Err(usage(cmd));
            }
            AdminOp::Route { channel: channel.to_string(), machine, path }
        }
        ("unroute", [channel]) => AdminOp::Unroute { channel: channel.to_string() },
        ("remove-machine", [machine]) => AdminOp::RemoveMachine { machine: machine.to_string() },
        _ => return Err(usage(cmd)),
    };
    Ok(AdminRequest { op, apply })
}

pub fn usage(cmd: &str) -> String {
    match cmd {
        "route" => crate::t!(
            "Usage: agentgw route <channel> <machine>[:<folder>] [--yes]\n  \
             Hand a channel to a machine. <channel> is its ID (C…) or #name.\n  \
             Without --yes it only shows what would change.",
            "使い方: agentgw route <チャンネル> <マシン>[:<フォルダ>] [--yes]\n  \
             チャンネルをマシンに任せます。<チャンネル> は ID(C…)か #名前。\n  \
             --yes が無ければ、何が変わるかを出すだけです。"
        ),
        "unroute" => crate::t!(
            "Usage: agentgw unroute <channel> [--yes]\n  \
             Take a channel's assignment away (the gateway handles it afterwards).\n  \
             Without --yes it only shows what would change.",
            "使い方: agentgw unroute <チャンネル> [--yes]\n  \
             チャンネルの割り当てを外します(この後はゲートウェイが受け持ちます)。\n  \
             --yes が無ければ、何が変わるかを出すだけです。"
        ),
        _ => crate::t!(
            "Usage: agentgw remove-machine <name> [--yes]\n  \
             Forget a machine that is gone, and the assignment of every channel it handles.\n  \
             Without --yes it only shows what would change.",
            "使い方: agentgw remove-machine <名前> [--yes]\n  \
             もう無いマシンの記録と、そのマシンが受け持つチャンネルの割り当てを消します。\n  \
             --yes が無ければ、何が変わるかを出すだけです。"
        ),
    }
}

impl Fleet {
    /// A channel as typed (`C…`, `<#C…|name>`, `#name`, `name`) → its ID, and the bot's channel names
    /// for showing them. A name must match a channel the bot is in.
    async fn resolve_channel(&self, typed: &str) -> Result<(String, HashMap<String, String>), String> {
        let names: HashMap<String, String> = self.api.bot_channels().await.unwrap_or_default().into_iter().collect();
        let bare = typed.trim().trim_start_matches("<#").trim_end_matches('>');
        let bare = bare.split('|').next().unwrap_or(bare);
        if crate::chat::slack::SlackId::is_channel(bare) {
            let mut names = names;
            if !names.contains_key(bare)
                && let Some(n) = self.api.channel_display_name(bare).await
            {
                names.insert(bare.to_string(), n.trim_start_matches('#').to_string());
            }
            return Ok((bare.to_string(), names));
        }
        let want = bare.trim_start_matches('#');
        let found: Vec<&String> = names.iter().filter(|(_, n)| n.as_str() == want).map(|(id, _)| id).collect();
        match found.as_slice() {
            [id] => Ok(((*id).clone(), names.clone())),
            _ => Err(crate::t!(
                "No channel named #{want} that the bot is in. Give its ID (C…) instead.",
                "ボットが入っている #{want} というチャンネルがありません。ID(C…)で指定してください。"
            )),
        }
    }

    /// Run one request: the plan, and with `apply` the change. `Ok` is what to print.
    pub(crate) async fn admin(self: &Arc<Self>, req: AdminRequest) -> Result<String, String> {
        let access = self.access();
        let connected = self.machines();
        let (plan, names) = match &req.op {
            AdminOp::Route { channel, machine, path } => {
                let (channel, names) = self.resolve_channel(channel).await?;
                (plan_route(&access, &connected, &self.self_id, &channel, machine, path.as_deref(), &names)?, names)
            }
            AdminOp::Unroute { channel } => {
                let (channel, names) = self.resolve_channel(channel).await?;
                (plan_unroute(&access, &self.self_id, &channel, &names)?, names)
            }
            AdminOp::RemoveMachine { machine } => {
                let names = self.api.bot_channels().await.unwrap_or_default().into_iter().collect();
                (plan_remove_machine(&access, &connected, &self.self_id, machine, &names)?, names)
            }
        };
        let mut out = plan.lines.join("\n");
        if !req.apply {
            out.push_str(&crate::t!(
                "\n\nNothing was changed. Add --yes to do it.",
                "\n\nまだ何も変えていません。実行するには --yes を付けてください。"
            ));
            return Ok(out);
        }
        let done = match &req.op {
            AdminOp::Route { channel, machine, path } => {
                let (channel, _) = self.resolve_channel(channel).await?;
                self.admin_route(&channel, machine, path.as_deref().unwrap_or("~"), &names).await?
            }
            AdminOp::Unroute { .. } => self.admin_drop(&plan.drop_routes, None).await,
            AdminOp::RemoveMachine { machine } => self.admin_drop(&plan.drop_routes, Some(machine)).await,
        };
        out.push_str("\n\n");
        out.push_str(&done);
        Ok(out)
    }

    /// Hand `channel` to `machine` once the folder is confirmed there. Nothing is written before that.
    async fn admin_route(
        self: &Arc<Self>,
        channel: &str,
        machine: &str,
        path: &str,
        names: &HashMap<String, String>,
    ) -> Result<String, String> {
        let abs = if machine == self.self_id {
            let abs = crate::bridge::state::absolute_project_path(path, &StateDir::home());
            crate::bridge::state::ensure_folder(&abs, machine, false)?;
            abs
        } else {
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.admin_waits
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(channel.to_string(), (machine.to_string(), tx));
            let frame = link::LinkFrame::SetProject {
                channel: channel.to_string(),
                thread_ts: String::new(),
                path: path.to_string(),
                thread_only: false,
                create: false,
            };
            let answer = match self.links.send_to(machine, &frame) {
                true => tokio::time::timeout(FOLDER_WAIT, rx).await.ok().and_then(Result::ok),
                false => None,
            };
            self.admin_waits.lock().unwrap_or_else(|e| e.into_inner()).remove(channel);
            match answer {
                Some(r) => r?,
                None => {
                    return Err(crate::t!(
                        "{machine} did not answer about the folder, so nothing was changed. Is it still connected?",
                        "{machine} がフォルダについて返事をしなかったので、何も変えていません。まだつながっていますか?"
                    ));
                }
            }
        };
        let (ch, id, folder) = (channel.to_string(), machine.to_string(), abs.clone());
        self.edit_access_whole(move |a| {
            let r = a.routes.entry(ch.clone()).or_default();
            r.bridge = Some(id);
            r.repo_path = Some(folder);
            a.released.remove(&ch);
        })
        .await;
        rlog("info", &format!("route from the terminal: {channel} → {machine}:{abs}"));
        let line = crate::t!(
            "This channel is now handled by **{machine}** in `{abs}` (changed from the gateway's terminal). \
             Threads already running stay where they are.",
            "このチャンネルは **{machine}** の `{abs}` が受け持つようになりました(ゲートウェイの端末から変えました)。\
             動いているスレッドはそのままです。"
        );
        if let Err(e) = self.api.post_markdown(channel, &line, None).await {
            rlog("info", &format!("route from the terminal: could not post in {channel}: {e}"));
        }
        Ok(crate::t!(
            "Done: {} is handled by {machine} in {abs}.",
            "済み: {} は {machine} の {abs} が受け持ちます。",
            shown(channel, names),
        ))
    }

    /// Drop these channels' assignments (and, with `machine`, its record). Each machine that had a
    /// channel keeps its own row, so the channel is noted as released from it — its next handshake
    /// would hand the channel straight back otherwise. Nothing is sent to the machines: their agents
    /// in those channels keep running.
    async fn admin_drop(self: &Arc<Self>, drop: &[(String, String)], machine: Option<&String>) -> String {
        let self_id = self.self_id.clone();
        let (gone, forget) = (drop.to_vec(), machine.cloned());
        self.edit_access_whole(move |a| {
            for (ch, m) in &gone {
                a.routes.remove(ch);
                if *m != self_id {
                    a.released.insert(ch.clone(), m.clone());
                }
            }
            if let Some(m) = &forget {
                a.machines.remove(m);
            }
        })
        .await;
        for (ch, m) in drop {
            rlog("info", &format!("unrouted from the terminal: {ch} (was {m})"));
        }
        match machine {
            Some(m) => {
                rlog("info", &format!("machine {m} removed from the terminal"));
                crate::t!(
                    "Done: {m} is removed, with {} channel assignment(s).",
                    "済み: {m} を外しました(チャンネルの割り当て {} 件も)。",
                    drop.len(),
                )
            }
            None => crate::t!("Done: the assignment is removed.", "済み: 割り当てを外しました。"),
        }
    }

    /// [`Fleet::edit_access`] for edits that may empty `released` or `machines`: a save leaves an
    /// empty map's old copy on disk, so the key is dropped in the same locked step.
    async fn edit_access_whole(&self, f: impl FnOnce(&mut Access)) {
        let saved = {
            let _one = EDITING.lock().unwrap_or_else(|e| e.into_inner());
            let mut access = self.access();
            f(&mut access);
            access.save(&self.dir).and_then(|()| {
                for (key, empty) in [("released", access.released.is_empty()), ("machines", access.machines.is_empty())] {
                    if empty {
                        self.dir.remove_json_key("access.json", key)?;
                    }
                }
                Ok(())
            })
        };
        if let Err(e) = saved {
            rlog("error", &format!("could not save access.json: {e}"));
            return;
        }
        let _ = self.reload.send(()).await;
    }

    /// A channel was assigned again (by the Slack `route` or a join): it is nobody's released channel.
    pub(super) async fn unrelease(&self, channel: &str) {
        if !self.access().released.contains_key(channel) {
            return;
        }
        let ch = channel.to_string();
        self.edit_access_whole(move |a| {
            a.released.remove(&ch);
        })
        .await;
    }

    /// A machine's answer to a `SetProject` sent by `agentgw route`. `true` = it was, and it went to
    /// the waiting request.
    pub(super) fn admin_answer(&self, machine: &str, channel: &str, result: &Result<String, String>) -> bool {
        let mut waits = self.admin_waits.lock().unwrap_or_else(|e| e.into_inner());
        if !waits.get(channel).is_some_and(|(m, _)| m == machine) {
            return false;
        }
        let (_, tx) = waits.remove(channel).expect("checked above");
        let _ = tx.send(result.clone());
        true
    }
}

/// `POST /admin`: one [`AdminRequest`] from the CLI. 200 with the text to print, 400 with why not.
pub(super) async fn on_admin(
    State(fleet): State<Arc<Fleet>>,
    headers: HeaderMap,
    body: String,
) -> axum::response::Response {
    if !secret_eq(&header(&headers, "x-api-token"), &fleet.token) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let Ok(req) = serde_json::from_str::<AdminRequest>(&body) else {
        return (StatusCode::BAD_REQUEST, "unreadable request").into_response();
    };
    match fleet.admin(req).await {
        Ok(text) => (StatusCode::OK, text).into_response(),
        Err(why) => (StatusCode::BAD_REQUEST, why).into_response(),
    }
}

/// The CLI side: ask the gateway on this host. Typed anywhere but the gateway, it says where to type it.
pub async fn cli(dir: &StateDir, cmd: &str, args: &[String]) -> i32 {
    let req = match parse_cli(cmd, args) {
        Ok(r) => r,
        Err(u) => {
            eprintln!("{u}");
            return 2;
        }
    };
    let env = Cli::env_of(dir);
    let role = env.get("AGENTGW_BRIDGE_ROLE").map(|r| r.trim().to_lowercase());
    let token = env.get("AGENTGW_LINK_TOKEN").filter(|t| !t.is_empty());
    let (false, Some(token)) = (role.as_deref() == Some("machine"), token) else {
        eprintln!(
            "{}",
            crate::t!(
                "Run this on the gateway: only it keeps which machine handles each channel.",
                "ゲートウェイで打ってください。どのマシンがどのチャンネルを受け持つかは、ゲートウェイだけが持っています。"
            )
        );
        return 2;
    };
    let port = crate::bridge::machine::link_port(|k| env.get(k).cloned());
    let url = format!("http://127.0.0.1:{port}/admin");
    let body = serde_json::to_string(&req).unwrap_or_default();
    let out = tokio::process::Command::new("curl")
        .args(["-s", "--max-time", "40", "-w", "\n%{http_code}", "-H"])
        .arg(format!("x-api-token: {token}"))
        .args(["--data-binary", &body, &url])
        .output()
        .await;
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    let (said, code) = text.rsplit_once('\n').unwrap_or(("", ""));
    match code {
        "200" => {
            println!("{said}");
            0
        }
        "" | "000" => {
            eprintln!(
                "{}",
                crate::t!(
                    "The gateway did not answer at {url}. Is it running?",
                    "ゲートウェイが {url} で応答しません。動いていますか?"
                )
            );
            1
        }
        _ => {
            eprintln!("{said}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::state::Link;

    fn names() -> HashMap<String, String> {
        [("C1".to_string(), "audit".to_string())].into()
    }

    fn access() -> Access {
        let mut a = Access::default();
        a.routes.insert(
            "C1".into(),
            Route {
                bridge: Some("mac".into()),
                repo_path: Some("/Users/x/audit".into()),
                allowed_tools: Some(vec!["Bash(ls)".into()]),
                ..Default::default()
            },
        );
        a.routes.insert("C2".into(), Route { bridge: Some("pve".into()), ..Default::default() });
        a.machines.insert("mac".into(), Link { version: "0.73.0".into(), last_seen: "2026-11-20".into(), ..Default::default() });
        a.machines.insert("pve".into(), Link::default());
        a
    }

    fn conn(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_cli_reads_each_form_and_applies_only_with_yes() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse_cli("route", &a(&["C1", "dock:~/dev/x"])).unwrap().op,
            AdminOp::Route { channel: "C1".into(), machine: "dock".into(), path: Some("~/dev/x".into()) }
        );
        assert!(!parse_cli("route", &a(&["C1", "dock"])).unwrap().apply);
        assert!(parse_cli("unroute", &a(&["#audit", "--yes"])).unwrap().apply);
        assert_eq!(
            parse_cli("remove-machine", &a(&["mac", "-y"])).unwrap(),
            AdminRequest { op: AdminOp::RemoveMachine { machine: "mac".into() }, apply: true }
        );
        assert!(parse_cli("route", &a(&["C1"])).is_err(), "no machine");
        assert!(parse_cli("route", &a(&["C1", "~/dev"])).is_err(), "a folder alone names no machine");
        assert!(parse_cli("unroute", &a(&[])).is_err());
    }

    #[test]
    fn route_says_where_the_channel_moves_and_only_to_a_connected_machine() {
        let plan = plan_route(&access(), &conn(&["dock", "pve"]), "dock", "C1", "dock", Some("~/dev/audit"), &names()).unwrap();
        let text = plan.lines.join("\n");
        assert!(text.contains("#audit (C1)"), "{text}");
        assert!(text.contains("mac → dock"), "{text}");
        assert!(text.contains("/Users/x/audit → ~/dev/audit"), "{text}");
        assert!(text.contains("stay on mac"), "{text}");
        let err = plan_route(&access(), &conn(&["dock"]), "dock", "C1", "mac", None, &names()).unwrap_err();
        assert!(err.contains("mac"), "{err}");
    }

    #[test]
    fn unroute_lists_what_goes_with_the_row() {
        let plan = plan_unroute(&access(), "dock", "C1", &names()).unwrap();
        let text = plan.lines.join("\n");
        assert!(text.contains("mac") && text.contains("/Users/x/audit"), "{text}");
        assert!(text.contains("Bash(ls)"), "the allowed tools go too: {text}");
        assert_eq!(plan.drop_routes, vec![("C1".to_string(), "mac".to_string())]);
        assert!(plan_unroute(&access(), "dock", "C9", &names()).is_err(), "nothing to take away");
    }

    #[test]
    fn remove_machine_lists_its_channels_and_refuses_what_it_must_not_touch() {
        let plan = plan_remove_machine(&access(), &conn(&["dock"]), "dock", "mac", &names()).unwrap();
        let text = plan.lines.join("\n");
        assert!(text.contains("0.73.0") && text.contains("#audit (C1)"), "{text}");
        assert!(!text.contains("C2"), "another machine's channel stays: {text}");
        assert_eq!(plan.drop_routes, vec![("C1".to_string(), "mac".to_string())]);
        assert!(plan_remove_machine(&access(), &conn(&["dock"]), "dock", "dock", &names()).is_err(), "itself");
        assert!(plan_remove_machine(&access(), &conn(&["dock", "mac"]), "dock", "mac", &names()).is_err(), "connected");
        assert!(plan_remove_machine(&access(), &conn(&["dock"]), "dock", "nope", &names()).is_err(), "unknown");
    }
}
