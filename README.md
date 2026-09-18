<div align="center">

# agentgw

### Run Claude Code on your own machines — and drive it from Slack.

**Every Slack thread is a Claude Code session. Every channel can live on a different machine.**

[![Release](https://img.shields.io/github/v/release/takashito/agentgw?style=flat-square&labelColor=black)](https://github.com/takashito/agentgw/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-green?style=flat-square&labelColor=black)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey?style=flat-square&labelColor=black)](#requirements)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-dea584?style=flat-square&labelColor=black&logo=rust)](https://www.rust-lang.org/)

**English** · [日本語](README.ja.md)

</div>

---

## Why agentgw?

Claude Code is great at real work — but it lives in a terminal on one machine.

- 🖥️ **Tied to the desk.** Walk away from the keyboard and you can't see or steer what it's doing.
- 🧵 **One conversation at a time.** Juggling several tasks means juggling several terminals.
- 🗄️ **Your code is spread out.** The repo you need is on the home server, the build box, the laptop.

**agentgw turns Slack into the front door.** Post in a thread and a dedicated Claude Code worker picks it up on the right machine, works in your real checkout, and answers in the thread — with live progress, and Slack buttons whenever it needs your permission.

It drives the **official Claude Code CLI** on your own hardware, with your own subscription. Nothing is proxied through a third-party service.

## Features

### 🧵 One thread, one session
Each Slack thread gets its own worker — a real Claude Code session running inside `tmux`. Reply in the thread to keep going; sessions survive restarts and upgrades of agentgw itself.

### 🖧 A fleet, not a single box
Connect several machines. One **parent** holds the Slack connection; **children** dial out to it, so they need no open ports. Assign each channel to a machine with `route <machine>`.

### 🚀 Add a machine with one command
`agentgw add-child user@host` ships the binary over ssh, installs it as a service, links it to the parent and waits until it shows up. It picks the path for you: a direct connection (for example over Tailscale), or an ssh tunnel the parent keeps alive.

### 👀 See the work as it happens
A progress message in the thread updates live with each tool call — files read, commands run, edits made — and folds away when the turn ends.

### 🔐 Approve tools from Slack
In `manual` mode, every tool call the worker wants to make shows up with **Allow** / **Deny** buttons.

### 🎛️ Full control from chat
Switch model, effort and permission mode, compact the context, check your subscription usage, stop a turn with a 🛑 reaction, or get the command to resume the session in your local terminal.

### ⚡ Warm workers
Keep a worker pre-started for a busy channel (`warm on`), so the first reply comes without the start-up wait.

### 📦 One static binary
A single Rust binary per machine, installed as a `launchd` agent or a `systemd` user unit. No runtime to install, nothing else to babysit.

## How it works

```
                 Socket Mode
   Slack  ◀──────────────────────▶  agentgw (parent)  ──▶  tmux ─ Claude Code …
                                      ▲         ▲
                  outbound WebSocket  │         │  outbound WebSocket
                  (direct or tunnel)  │         │
                               agentgw (child)  agentgw (child)
                                      │         │
                        tmux ─ Claude Code …   tmux ─ Claude Code …
```

1. **You write** in a Slack channel or thread.
2. **The parent** receives it over Slack Socket Mode and forwards it to the machine that owns the channel — or handles it itself if the channel has no owner.
3. **That machine** hands it to the thread's worker, starting one if needed, in the channel's project directory.
4. **The worker** replies in the thread through agentgw's MCP tools, with progress and permission prompts along the way.

Only the parent talks to Slack's event stream. Children receive the bot token over the link and keep it in memory only; the app token never leaves the parent.

## Requirements

- **macOS or Linux** — prebuilt binaries for Apple Silicon Macs and x86_64 Linux
- **[Claude Code](https://docs.claude.com/en/docs/claude-code)**, installed and signed in on every machine
- **`tmux`** — the installer adds it if it's missing
- **A Slack app** with Socket Mode — see [Slack app setup](#slack-app-setup)

## Quick start

**1. Install on the parent machine**

```bash
bash -c "$(curl -fsSL https://raw.githubusercontent.com/takashito/agentgw/main/scripts/install.sh)"
```

The installer downloads the binary for your machine, installs it as a service, and asks for your Slack tokens (`xoxb-…` and `xapp-…`).

> [!NOTE]
> Use `bash -c "$(curl …)"` rather than `curl … | bash`. Piping takes the terminal away, and the installer can no longer ask for your tokens.

**2. Sign in from Slack**

Send the bot a DM: **`login`**. The first person to do so becomes the **owner** — the only person the bot takes commands from.

**3. Talk to it**

Mention the bot in a channel, or DM it. Each thread becomes its own session.

**4. Add more machines (optional)**

From the parent:

```bash
agentgw add-child user@host
```

Then, in the channel that machine should handle: `@agentgw route <machine>`.

<details>
<summary><b>Build from source</b></summary>

```bash
cargo build --release
./scripts/install.sh --from target/release/agentgw
```

</details>

## Slack app setup

**[➜ Create the Slack app with everything pre-configured](https://api.slack.com/apps?new_app=1&manifest_json=%7B%22%5Fmetadata%22%3A%7B%22major%5Fversion%22%3A1%2C%22minor%5Fversion%22%3A1%7D%2C%22display%5Finformation%22%3A%7B%22background%5Fcolor%22%3A%22%23262626%22%2C%22description%22%3A%22Run%20Claude%20Code%20on%20your%20own%20machines%20and%20drive%20it%20from%20Slack%22%2C%22name%22%3A%22agentgw%22%7D%2C%22features%22%3A%7B%22app%5Fhome%22%3A%7B%22home%5Ftab%5Fenabled%22%3Afalse%2C%22messages%5Ftab%5Fenabled%22%3Atrue%2C%22messages%5Ftab%5Fread%5Fonly%5Fenabled%22%3Afalse%7D%2C%22bot%5Fuser%22%3A%7B%22always%5Fonline%22%3Atrue%2C%22display%5Fname%22%3A%22agentgw%22%7D%7D%2C%22oauth%5Fconfig%22%3A%7B%22scopes%22%3A%7B%22bot%22%3A%5B%22app%5Fmentions%3Aread%22%2C%22assistant%3Awrite%22%2C%22channels%3Ahistory%22%2C%22channels%3Aread%22%2C%22chat%3Awrite%22%2C%22files%3Aread%22%2C%22files%3Awrite%22%2C%22groups%3Ahistory%22%2C%22groups%3Aread%22%2C%22im%3Ahistory%22%2C%22im%3Aread%22%2C%22im%3Awrite%22%2C%22mpim%3Ahistory%22%2C%22mpim%3Aread%22%2C%22reactions%3Aread%22%2C%22reactions%3Awrite%22%2C%22users%3Aread%22%5D%7D%7D%2C%22settings%22%3A%7B%22event%5Fsubscriptions%22%3A%7B%22bot%5Fevents%22%3A%5B%22app%5Fmention%22%2C%22member%5Fjoined%5Fchannel%22%2C%22message%2Echannels%22%2C%22message%2Egroups%22%2C%22message%2Eim%22%2C%22message%2Empim%22%2C%22reaction%5Fadded%22%2C%22reaction%5Fremoved%22%5D%7D%2C%22interactivity%22%3A%7B%22is%5Fenabled%22%3Atrue%7D%2C%22socket%5Fmode%5Fenabled%22%3Atrue%2C%22token%5Frotation%5Fenabled%22%3Afalse%7D%7D)**

The link opens Slack's *Create an app from a manifest* page with [`slack-app-manifest.json`](slack-app-manifest.json) already filled in — scopes, events, Socket Mode, Interactivity and the DM tab. Pick your workspace and click **Create**. The installer shows (and opens) the same link when it asks for your tokens.

Then collect the two tokens:

1. **Install to Workspace**, and copy the **Bot User OAuth Token** (`xoxb-…`).
2. Under **Basic Information → App-Level Tokens**, generate a token with the `connections:write` scope (`xapp-…`).

<details>
<summary><b>Setting the app up by hand</b></summary>

Create an app **From a manifest** at [api.slack.com/apps](https://api.slack.com/apps) and paste the contents of [`slack-app-manifest.json`](slack-app-manifest.json). The parts that break silently when they're missing:

- **Socket Mode** on — agentgw never opens a public endpoint.
- **Interactivity** on — even with Socket Mode. Without it, the Allow / Deny buttons never arrive.
- **App Home → Messages Tab** on, and not read-only — otherwise you can't DM the bot, and `login` is sent by DM.

</details>

## Adding machines

```bash
agentgw add-child user@host                   # any ssh destination, including ~/.ssh/config aliases
agentgw add-child user@host --name build-box  # choose the machine's name (default: its hostname)
```

`add-child` looks at the remote machine, sends it the matching binary and the installer, links it to the parent, starts the service, and **waits until the parent sees it**. Run it again later to upgrade that machine to the parent's version.

The link between parent and child is chosen by trying, not guessing:

| | How | When |
|---|---|---|
| **Direct** | The child dials the parent's public name (`AGENTGW_LINK_PUBLIC_URL`, or the parent's Tailscale name) | The parent is reachable — e.g. `tailscale serve --bg 8787` on the parent |
| **SSH tunnel** | The parent keeps `ssh -N -R` open to the child; the child dials its own loopback | Direct didn't connect within 20 seconds |

`agentgw status` on the parent shows which path each child is using.

## Slack commands

Mention the bot (`@agentgw <command>`), or type the command in a thread it's working in.

| Command | What it does |
|---|---|
| `stop` | Stop the current turn (a 🛑 reaction works too) |
| `exit` / `bye` / `done` | End this thread's worker — the thread resumes on your next message |
| `compact` | Compact the context, with a progress bar |
| `model [fable\|opus\|sonnet\|haiku]` | Show or switch the model |
| `effort [low\|medium\|high\|xhigh\|max\|…]` | Show or set the effort level |
| `mode [manual\|plan\|edit\|auto]` | Show or switch the permission mode |
| `context` | Show this worker's context usage |
| `resume` | Print the command to continue this session in your own terminal |
| `usage` | Show your Claude subscription usage |
| `status` | Version, active threads and warm workers |
| `route [machine]` | Show routing, or assign this channel to a machine |
| `pwd [absolute path]` | Show or set this channel's project directory |
| `warm on\|off` | Keep a worker pre-started for this channel |
| `set-home` | Send notices (online, offline, errors) to this channel |
| `allow-bot @bot` / `remove-bot @bot` | Let another bot's messages through |
| `login` / `logout` | Sign in (from a DM) / sign out and stop all workers |
| `help` | List all commands |

## CLI

```bash
agentgw status             # role, children and their links, channel routing
agentgw restart            # swap in a new binary; running workers keep going
agentgw shutdown           # stop the service and its workers
agentgw start              # start the service
agentgw uninstall          # remove the service definition (tokens and state are kept)
agentgw add-child <ssh>    # add or upgrade a machine
agentgw --version
```

## Files on disk

Every machine uses the same layout.

| Path | What |
|---|---|
| `~/.local/bin/agentgw` | The binary |
| `~/.local/state/agentgw/` | State — override with `AGENTGW_STATE_DIR` |
| ├ `.env` | Settings and tokens (mode 600) |
| ├ `access.json` | Owner, notice channel, routing, project directories, warm workers |
| ├ `threads.json` | Which Slack thread belongs to which session |
| ├ `plugin-debug.log` | Main log (rotated to `.1`) |
| ├ `logs/` | Per-thread and per-session logs, and the service's own `service.out.log` / `service.err.log` |
| └ `inbox/` | Downloaded Slack attachments |
| `~/Library/LaunchAgents/com.agentgw.bridge.plist` | Service definition (macOS) |
| `~/.config/systemd/user/agentgw-bridge.service` | Service definition (Linux) |
| `$TMPDIR/agentgw-agentgw/` | Hook and MCP config handed to workers (regenerated on start) |
| tmux session `agentgw-workers` | One window per worker |

## Security

- **Only the owner is obeyed.** The first person to `login` becomes the owner; commands from anyone else are refused.
- **Workers run as the user that installed agentgw**, and can do whatever that user can. For isolation, install under a dedicated account (for example `agentgw add-child agent@host`) rather than `root`.
- **Tokens stay local.** They live in `.env` with mode 600. Children never receive the app token; they get the bot token over the link and hold it in memory only.
- **The link secret is a password.** Anyone holding it can join as a child and receive that machine's messages. `add-child` passes it over ssh stdin, never on a command line.
- **Children need no open ports** — they dial out. The parent's listener binds `0.0.0.0:8787` by default and does not terminate TLS itself: keep it behind Tailscale or an ssh tunnel, or set `AGENTGW_LINK_LISTEN=127.0.0.1:8787`.
- **macOS code signing.** When run from a terminal, the installer creates a self-signed code-signing identity once, so macOS permission grants survive upgrades. Without it, macOS asks again after every upgrade.

## Troubleshooting

| Symptom | Where to look |
|---|---|
| Nothing happens in Slack | `agentgw status`, then `~/.local/state/agentgw/plugin-debug.log` |
| The service won't start, or keeps restarting | `~/.local/state/agentgw/logs/service.err.log` |
| Allow / Deny buttons do nothing | Turn on **Interactivity** in the Slack app settings |
| Only some messages get answered | Another process is connected with the same app token — Slack splits events between them |
| A child shows as offline | `agentgw status` on the parent shows its link; on the child, check `service.err.log` |
| A worker can't use new tool options after an upgrade | Run `/mcp` in that worker — it keeps the tool list from when it started |

## Development

```bash
cargo build && cargo test
AGENTGW_STATE_DIR=$HOME/.local/state/agentgw-dev ./target/debug/agentgw serve

cargo dist              # release binaries for macOS (arm64) and Linux (x86_64, musl)
./scripts/release.sh    # upload them as a GitHub release
```

`cargo dist` needs [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild), zig, and `rustup target add` for each target.

A second, separate instance on the same machine is fine: give it its own `AGENTGW_STATE_DIR` and **its own Slack app**. Two processes on one app token split Slack's events between them.

## License

[MIT](LICENSE)
