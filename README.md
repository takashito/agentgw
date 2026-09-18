<div align="center">

<h1>agentgw</h1>

<p><b>Your own Claude Code agents, on every machine in your homelab — driven from one Slack bot.</b></p>

[![Release](https://img.shields.io/github/v/release/takashito/agentgw?style=flat-square&labelColor=black)](https://github.com/takashito/agentgw/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-green?style=flat-square&labelColor=black)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey?style=flat-square&labelColor=black)](#requirements)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-dea584?style=flat-square&labelColor=black&logo=rust)](https://www.rust-lang.org/)

[Quick start](#quick-start) · [Slack app](#slack-app-setup) · [Adding machines](#adding-machines) · [Commands](#slack-commands) · [Security](#security) · [日本語](README.ja.md)

</div>

agentgw is a small service that connects one Slack bot to Claude Code on every machine you own. Post in a channel and a Claude Code agent starts **on the machine that channel belongs to**, in that project's directory, and answers in the thread — showing its progress as it goes and asking for your approval in Slack when it needs it.

<p align="center">
  <img src="assets/thread.svg" width="780" alt="A Slack thread: you ask agentgw to fix flaky tests; a live progress message lists the tools Claude Code runs; a permission request with Allow and Deny buttons; the agent replies with the fix.">
</p>

## Why agentgw?

### 🖧 Every machine, one bot

The agents on your laptop, your NAS, your hypervisor and your build box all answer through **one Slack bot**. Each channel belongs to a machine and a project directory; `route` moves a channel, `status` shows the whole fleet and how each machine is linked, and `add-machine` brings in a new machine with a single command. It's built for a homelab, not a single desk.

| Channel | Machine | Works in |
|---|---|---|
| `#nas` | a storage box in the closet (gateway) | `/srv/compose` |
| `#laptop` | the Mac you carry | `~/code/app` |
| `#proxmox` | the hypervisor | `/root/infra` |
| `#build` | a Linux build box | `~/src/firmware` |

### What remote control and multiplexers leave to you

They keep a session you **already started** within reach. The rest is agentgw's job:

| | |
|---|---|
| 🏠 **Machines you don't sit at** | No terminal is open on the server in the closet, so nothing is waiting there. agentgw runs as a service on each machine and starts an agent — in the right directory — when a thread arrives. It starts again on its own after a reboot (on macOS, once you log in). |
| 🧭 **Not remembering where things run** | With several machines and several repos on each, the friction is *which box, which directory, which session*. Map a channel once; after that, where you post is all that decides. |
| 🔔 **Work that already arrives in Slack** | Alerts from monitoring, failures from CI. Let a bot's messages through with `allow-bot`, and its alert can start an investigation — with a guard against two bots answering each other forever. |
| 📝 **A record where people look** | The request, every tool the agent ran, what you approved and what it concluded stay in the thread, searchable in Slack. |

It drives the **official Claude Code CLI**, on your hardware. Nothing is proxied through a third-party service.

## Features

<table>
<tr>
<td width="50%" valign="top">

**🧵 One thread, one session**<br>
Each thread gets its own agent — a real Claude Code session in `tmux`. Reply to continue; sessions survive restarts and upgrades of agentgw.

</td>
<td width="50%" valign="top">

**🖧 A fleet, one Slack app**<br>
One **gateway** holds the Slack connection. Your other **machines** dial out to it, so they need no open ports. `route <machine>` assigns a channel.

</td>
</tr>
<tr>
<td valign="top">

**🚀 Add a machine in one command**<br>
`agentgw add-machine user@host` ships the binary over ssh, installs the service, connects it, and waits until the gateway sees it.

</td>
<td valign="top">

**🔌 Links that pick themselves**<br>
Tries a direct connection (e.g. over Tailscale) first; if that doesn't come up, the gateway keeps an ssh tunnel open instead.

</td>
</tr>
<tr>
<td valign="top">

**👀 Live progress**<br>
One message per turn, updated as tools run — files read, commands run, edits made — and folded away when the turn ends.

</td>
<td valign="top">

**🔐 Approvals as buttons**<br>
In `manual` mode each tool call asks first: **Allow**, allow for the thread or the channel, or **Deny**.

</td>
</tr>
<tr>
<td valign="top">

**🎛️ Control from chat**<br>
Model, effort and permission mode; compact; usage; stop with a 🛑 reaction; `resume` hands the session to your local terminal.

</td>
<td valign="top">

**📦 One static binary**<br>
A single Rust binary per machine, running as a `launchd` agent or a `systemd` user unit. Nothing else to install or babysit.

</td>
</tr>
</table>

## How it works

```mermaid
flowchart LR
    S(["Slack"]) <-->|Socket Mode| P["agentgw<br/>gateway"]
    P --> WP["Claude Code agents<br/>in tmux"]
    C1["agentgw<br/>machine: build-box"] -->|outbound WebSocket| P
    C2["agentgw<br/>machine: homelab"] -->|ssh tunnel| P
    C1 --> W1["Claude Code agents<br/>in tmux"]
    C2 --> W2["Claude Code agents<br/>in tmux"]
```

1. **You post** in a channel or thread.
2. **The gateway** receives it over Slack Socket Mode and passes it to the machine that owns the channel — or keeps it, if the channel has no owner.
3. **That machine** hands it to the thread's agent, starting one in the channel's project directory if needed.
4. **The agent** replies through agentgw's MCP tools, with progress and permission prompts along the way.

Only the gateway reads Slack's event stream. Other machines get the bot token over the link and keep it in memory only; the app token never leaves the gateway.

## Requirements

| | |
|---|---|
| **OS** | macOS or Linux. Prebuilt binaries: Apple Silicon Mac, x86_64 Linux. Other targets build from source. |
| **Claude Code** | [Installed](https://docs.claude.com/en/docs/claude-code) and signed in on every machine |
| **tmux** | Installed by the installer if missing |
| **Slack** | A workspace where you can create an app — [one click](#slack-app-setup) |

## Quick start

**1. Create the Slack app**

[![Create the Slack app](https://img.shields.io/badge/Create_the_Slack_app-4A154B?style=for-the-badge&logo=slack&logoColor=white)](https://api.slack.com/apps?new_app=1&manifest_json=%7B%22%5Fmetadata%22%3A%7B%22major%5Fversion%22%3A1%2C%22minor%5Fversion%22%3A1%7D%2C%22display%5Finformation%22%3A%7B%22background%5Fcolor%22%3A%22%23262626%22%2C%22description%22%3A%22Run%20Claude%20Code%20on%20your%20own%20machines%20and%20drive%20it%20from%20Slack%22%2C%22name%22%3A%22agentgw%22%7D%2C%22features%22%3A%7B%22app%5Fhome%22%3A%7B%22home%5Ftab%5Fenabled%22%3Afalse%2C%22messages%5Ftab%5Fenabled%22%3Atrue%2C%22messages%5Ftab%5Fread%5Fonly%5Fenabled%22%3Afalse%7D%2C%22bot%5Fuser%22%3A%7B%22always%5Fonline%22%3Atrue%2C%22display%5Fname%22%3A%22agentgw%22%7D%7D%2C%22oauth%5Fconfig%22%3A%7B%22scopes%22%3A%7B%22bot%22%3A%5B%22app%5Fmentions%3Aread%22%2C%22assistant%3Awrite%22%2C%22channels%3Ahistory%22%2C%22channels%3Aread%22%2C%22chat%3Awrite%22%2C%22files%3Aread%22%2C%22files%3Awrite%22%2C%22groups%3Ahistory%22%2C%22groups%3Aread%22%2C%22im%3Ahistory%22%2C%22im%3Aread%22%2C%22im%3Awrite%22%2C%22mpim%3Ahistory%22%2C%22mpim%3Aread%22%2C%22reactions%3Aread%22%2C%22reactions%3Awrite%22%2C%22users%3Aread%22%5D%7D%7D%2C%22settings%22%3A%7B%22event%5Fsubscriptions%22%3A%7B%22bot%5Fevents%22%3A%5B%22app%5Fmention%22%2C%22member%5Fjoined%5Fchannel%22%2C%22message%2Echannels%22%2C%22message%2Egroups%22%2C%22message%2Eim%22%2C%22message%2Empim%22%2C%22reaction%5Fadded%22%2C%22reaction%5Fremoved%22%5D%7D%2C%22interactivity%22%3A%7B%22is%5Fenabled%22%3Atrue%7D%2C%22socket%5Fmode%5Fenabled%22%3Atrue%2C%22token%5Frotation%5Fenabled%22%3Afalse%7D%7D)

Pick a workspace and click **Create**, then collect two tokens: **Install to Workspace** → *Bot User OAuth Token* (`xoxb-…`), and **Basic Information → App-Level Tokens** → a token with `connections:write` (`xapp-…`).

**2. Install on the gateway machine**

Pick a machine that stays on — a home server, a NAS or a small VM. Everything goes through the gateway, so when it sleeps every machine goes quiet. A laptop you carry around works better as one of the machines: it drops off when it sleeps and comes back when it wakes, without affecting the others.

```bash
bash -c "$(curl -fsSL https://raw.githubusercontent.com/takashito/agentgw/main/scripts/install.sh)"
```

It downloads the binary, installs the service, and asks for the two tokens.

> [!NOTE]
> Use `bash -c "$(curl …)"`, not `curl … | bash` — piping takes the terminal away, and the installer can't ask for your tokens.

**3. Sign in**

DM the bot **`login`**. It signs Claude Code on that machine in to **your** Claude account — you get a sign-in link, and paste the code back into the DM. The person who signs in becomes the **owner**, the only person the bot works for.

> Already use an API key? If Claude Code on the machine is set up with one, agents use it. agentgw itself never sees model credentials.

**4. Use it**

Mention the bot in a channel (or DM it). Every thread is its own session.

<details>
<summary><b>Build from source</b></summary>

```bash
git clone https://github.com/takashito/agentgw.git && cd agentgw
cargo build --release
./scripts/install.sh --from target/release/agentgw
```

</details>

## Slack app setup

The button above opens Slack's *Create an app from a manifest* page with [`slack-app-manifest.json`](slack-app-manifest.json) filled in: scopes, events, Socket Mode, Interactivity and the DM tab. The installer shows the same link (and opens it) when it asks for your tokens.

<details>
<summary><b>Setting the app up by hand</b></summary>

Create an app **From a manifest** at [api.slack.com/apps](https://api.slack.com/apps) and paste [`slack-app-manifest.json`](slack-app-manifest.json). These fail silently when they're missing:

- **Socket Mode** on — agentgw never opens a public endpoint.
- **Interactivity** on, even with Socket Mode — otherwise the Allow / Deny buttons never arrive.
- **App Home → Messages Tab** on and not read-only — otherwise nobody can DM the bot, and `login` is a DM.

</details>

## Adding machines

From the gateway:

```bash
agentgw add-machine user@host                   # any ssh destination, ~/.ssh/config aliases included
agentgw add-machine user@host --name build-box  # name the machine (default: its hostname)
```

Then assign channels to it — in the channel: `@agentgw route build-box`.

`add-machine` checks the remote OS, sends the matching binary and the installer, links the machine, starts the service and **waits until the gateway sees it**. Run it again later to bring that machine to the gateway's version.

The link is chosen by trying, not guessing:

| Link | How | Used when |
|---|---|---|
| **Direct** | The machine dials the gateway's public name (`AGENTGW_LINK_PUBLIC_URL`, or its Tailscale name) | The gateway is reachable — e.g. `tailscale serve --bg 8787` on the gateway |
| **SSH tunnel** | The gateway keeps `ssh -N -R` open to the machine, which dials its own loopback | Direct didn't connect within 20 seconds |

`agentgw status` on the gateway lists each machine and how it is connected.

## Slack commands

Mention the bot (`@agentgw <command>`), or type in a thread it's working in.

| Command | What it does |
|---|---|
| `stop` | Stop the current turn (a 🛑 reaction works too) |
| `exit` · `bye` · `done` | End this thread's agent; your next message resumes it |
| `compact` | Compact the context, with a progress bar |
| `model [fable\|opus\|sonnet\|haiku]` | Show or switch the model |
| `effort [low\|medium\|high\|xhigh\|max\|…]` | Show or set the effort level |
| `mode [manual\|plan\|edit\|auto]` | Show or switch the permission mode |
| `context` | This agent's context usage |
| `resume` | The command to continue this session in your own terminal |
| `usage` | Your Claude subscription usage |
| `status` | Version, active threads and warm agents |
| `route [machine]` | Show routing, or assign this channel to a machine |
| `pwd [absolute path]` | Show or set this channel's project directory |
| `warm on\|off` | Keep an agent started ahead of time for this channel |
| `set-home` | Send notices (online, offline, errors) to this channel |
| `allow-bot @bot` · `remove-bot @bot` | Let another bot's messages start work |
| `login` · `logout` | Sign Claude Code in to your account (by DM) · sign out and stop all agents |
| `help` | All commands |

## CLI

```bash
agentgw status               # role, machines and how they connect, channel routing
agentgw add-machine <ssh>    # add or upgrade a machine
agentgw restart              # swap in a new binary; running agents keep going
agentgw shutdown             # stop the service and its agents
agentgw start                # start the service
agentgw uninstall            # remove the service definition (tokens and state are kept)
agentgw --version
```

agentgw speaks English. For Japanese — in Slack, the CLI and the installer — add `AGENTGW_LANG=ja` to `~/.local/state/agentgw/.env` and run `agentgw restart`.

<details>
<summary><b>Files on disk</b></summary>

Every machine uses the same layout.

| Path | What |
|---|---|
| `~/.local/bin/agentgw` | The binary |
| `~/.local/state/agentgw/` | State — override with `AGENTGW_STATE_DIR` |
| ├ `.env` | Settings and tokens (mode 600) |
| ├ `access.json` | Owner, notice channel, routing, project directories, warm agents |
| ├ `threads.json` | Which Slack thread belongs to which session |
| ├ `plugin-debug.log` | Main log (rotated to `.1`) |
| ├ `logs/` | Per-thread and per-session logs, plus `service.out.log` / `service.err.log` |
| └ `inbox/` | Downloaded Slack attachments |
| `~/Library/LaunchAgents/com.agentgw.bridge.plist` | Service definition (macOS) |
| `~/.config/systemd/user/agentgw-bridge.service` | Service definition (Linux) |
| `$TMPDIR/agentgw-agentgw/` | Hook and MCP config handed to agents (regenerated on start) |
| tmux session `agentgw-workers` | One window per agent |

</details>

## Security

- **Only the owner can start work or run commands** — the person who first signed Claude Code in with `login`. In a thread that's already running, other people's messages reach the agent as context only. agentgw is not a shared bot running on somebody else's account.
- **Agents run on your own credentials** — your Claude subscription, or an API key set up for Claude Code on that machine. agentgw never handles model credentials: signing in goes through Claude Code's own sign-in flow. Each machine keeps its own sign-in; if one lapses, send `login` in a channel that machine handles.
- **Agents run as the user that installed agentgw** and can do anything that user can. Install under a dedicated account (e.g. `agentgw add-machine agent@host`) rather than `root`.
- **Tokens stay local**, in `.env` with mode 600. Other machines never get the app token; they hold the bot token in memory only.
- **The link secret is a password.** Whoever has it can join as a machine and receive that machine's messages. `add-machine` sends it over ssh stdin, never on a command line.
- **Machines open no ports.** The gateway's listener binds `0.0.0.0:8787` by default without TLS — keep it behind Tailscale or an ssh tunnel, or set `AGENTGW_LINK_LISTEN=127.0.0.1:8787`.
- **macOS signing.** Run from a terminal, the installer creates a self-signed code-signing identity once, so macOS permission grants survive upgrades.

## Limitations

- **Slack and Claude Code only.** No other chat platforms or agent CLIs.
- **One owner per install.** It isn't a multi-user service: other people can follow a thread and add context, but can't start work or run commands.
- **macOS and Linux only.** No Windows. Prebuilt binaries cover Apple Silicon and x86_64 Linux.
- **One process per Slack app.** Two agentgw gateways on the same app token split Slack's events between them — give each its own app.

## Troubleshooting

| Symptom | Where to look |
|---|---|
| Nothing happens in Slack | `agentgw status`, then `~/.local/state/agentgw/plugin-debug.log` |
| The service won't start or keeps restarting | `~/.local/state/agentgw/logs/service.err.log` |
| Allow / Deny buttons do nothing | Turn on **Interactivity** in the Slack app |
| You can't DM the bot | Turn on **App Home → Messages Tab** in the Slack app |
| Only some messages get answered | Another process is using the same app token |
| A machine shows as offline | `agentgw status` on the gateway; `service.err.log` on that machine |
| An agent can't use new tool options after an upgrade | Run `/mcp` in that agent — it keeps the tool list it started with |

## Development

```bash
cargo build && cargo test
AGENTGW_STATE_DIR=$HOME/.local/state/agentgw-dev ./target/debug/agentgw serve

cargo dist              # release binaries: macOS (arm64) and Linux (x86_64, musl)
./scripts/release.sh    # upload them as a GitHub release
```

`cargo dist` needs [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild), zig, and `rustup target add` for each target. For a development instance next to a real one, use a separate `AGENTGW_STATE_DIR` **and a separate Slack app**.

Issues and pull requests are welcome — [open an issue](https://github.com/takashito/agentgw/issues) for bugs and questions.

## License

[MIT](LICENSE)
