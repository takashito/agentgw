//! Bridge — the core that sits between Slack and the agent. This file holds only **the starting point of the assembly**:
//! the `Bridge` struct, [`Deps`] for receiving the outside world (Slack, the agent, the clock — [`crate::chat::Chat`], [`crate::agent::Agent`], [`crate::clock::Clock`]),
//! `run()` (wiring and the select loop), and start-up, shutdown and restart.
//!
//! Each feature's `impl Bridge` lives in its own child module:
//! [`inbound`](crate::bridge::inbound) (receiving and gates) / [`turn`](crate::bridge::turn) (hooks, turns, permissions, the silence watch, progress) /
//! [`worker`](crate::bridge::worker) (starting agents, the pool, recovery) / [`command`](crate::bridge::command) (parsing and running commands).
//! State and logs kept on disk are in [`state`],
//! the gateway side in [`gateway`], and the machine side of the connection in [`machine`].

pub mod command;
pub mod machine;
pub mod gateway;
pub mod inbound;
pub mod turn;
pub mod state;
pub mod worker;

use crate::agent::claude::HookIntake;
use crate::agent::claude::Claude;
use crate::bridge::command::CmdFx;
use crate::bridge::state as bridge;
use crate::chat::InboundMsg;
use crate::log::LogCtx;
use crate::chat::ThreadKey;
use crate::bridge::turn::{PermPending, Stall};
use crate::chat::slack;
use crate::mcp;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

/// How often progress messages are pushed to Slack. The board also throttles to once a second per thread on its own.
const FLUSH_INTERVAL: Duration = Duration::from_millis(500);

/// The note left in threads with unanswered messages.
///
/// **Deliberately differs from the original wording**, which said "the remaining work resumes automatically
/// after the restart". A restart only takes down the Bridge; agents and the pool are not torn down
/// (the successor inherits them). Saying "resume" for something never stopped is doubly false — and there's
/// no auto-resume mechanism yet either (see the ponytail note on `maintenance_restart`). State only the facts.
fn restart_notice() -> String {
    crate::t!(
        "🙏 agentgw is restarting for a few seconds. Running agents aren't stopped, so their work continues.",
        "🙏 agentgw を数秒だけ再起動します。動いているエージェントは止めないので、作業はそのまま続きます。"
    )
}

/// All the mutable state the main loop holds. Each select arm just calls a method here.
pub struct Bridge {
    /// The outside world: Slack, the agent, the clock and the state directory.
    deps: Deps,
    access: bridge::Access,
    threads: bridge::Threads,
    /// cwd → the session nominated for the pool (pools.json). **A nomination, not the process itself**, so it
    /// survives across Bridges. When starting a pool agent, this decides `--resume` or new.
    pools: bridge::Pools,
    dedup: inbound::RecentDeliveries,
    /// Ledger of live agents and the pool.
    workers: worker::Workers,
    /// Waiting for delivery (the queue of threads whose agent isn't warm yet). Not a ledger, so it lives in the Bridge.
    /// **root_ts** → the texts waiting for delivery. Not a thread key (it's the queue that fills while the agent
    /// isn't warm yet, and lookups always happen within the same channel).
    pending: HashMap<String, Vec<InboundMsg>>,
    lifecycle: bridge::Lifecycle,
    ledger: bridge::Ledger,
    sticky: slack::StickyBoard,
    /// Tool permissions waiting for a person's click. reqId → the waiting agent and the prompt to delete.
    perm_pending: HashMap<String, PermPending>,
    /// `thread_key\0message_id` → narration fragments whose final hasn't arrived yet.
    narration: HashMap<String, String>,
    hooks_file: String,
    mcp_port: u16,
    mcp_token: String,
    /// Our own Slack user id. Text commands are checked after stripping our own mention.
    /// None on a start where auth.test failed (commands with a mention just pass through).
    bot_user_id: Option<String>,
    /// When this Bridge started listening. Commands posted before then are too late.
    started_at_ms: u64,
    /// Where spawned sign-in / sign-out tasks send back their state changes.
    cmd_tx: mpsc::Sender<CmdFx>,
    /// Where to ask the gateway the things only it knows (`channels`, `pwd <machine>`). `None` = this
    /// Bridge works on its own, with no gateway anywhere.
    ask_gateway: Option<mpsc::UnboundedSender<crate::bridge::gateway::link::LinkFrame>>,
    /// The sign-in / sign-out in progress ([`command::SignIn`]).
    sign_in: command::SignIn,
    /// Whether a restart has begun. A flag so the few hundred ms until exit(0) don't run twice
    restarting: bool,
    /// Reset time of the usage limit (epoch ms). While now is below it, new deliveries are blocked.
    /// 0 = gate open. The periodic polling is what actually fills it.
    limited_until_ms: u64,
    /// The usage-limit watch: when `/usage` was last read, whether the limit is expected,
    /// and how far warnings have gone.
    usage_polled_at_ms: u64,
    usage_at_risk: bool,
    usage_warned_pct: u32,
    /// The silence watch (`armWatchdog` / `showStall`). `thread_key` → [`Stall`].
    stall: HashMap<ThreadKey, Stall>,
    /// Whether this has a port for machines to connect to. Only used to decide whether `help` shows the machine commands
    /// (running it is `relay::CommandCtx::route`. Listing it on a machine without one leaves
    /// nobody to route to).
    fleet: bool,
    /// This machine's name as the gateway knows it (`pwd <name>`); the hostname if unset.
    machine_name: String,
}

/// The outside world. Only `Bridge::run()` wires the real ones; tests fill it with fakes
/// and hand it to `Bridge::new`.
#[derive(Clone)]
pub struct Deps {
    pub slack: crate::chat::ChatRef,
    pub agent: crate::agent::AgentRef,
    pub clock: crate::clock::ClockRef,
    pub dir: crate::state_dir::StateDir,
}

/// Values fixed at start-up (the result of the wiring).
struct Config {
    hooks_file: String,
    mcp_port: u16,
    mcp_token: String,
    bot_user_id: Option<String>,
    /// When this Bridge started listening. Commands posted before it are stale.
    started_at_ms: u64,
    fleet: bool,
    machine_name: String,
    cmd_tx: mpsc::Sender<CmdFx>,
    ask_gateway: Option<mpsc::UnboundedSender<crate::bridge::gateway::link::LinkFrame>>,
}

impl Bridge {
    /// Loads the state files from `deps.dir`; everything else starts empty.
    fn new(deps: Deps, config: Config) -> Bridge {
        // The ledger lives inside threads.json (the entry's `inflight`), so build it after reading
        let threads = bridge::Threads::load(&deps.dir);
        Bridge {
            ledger: bridge::Ledger::load(&threads),
            threads,
            pools: bridge::Pools::load(&deps.dir),
            access: bridge::Access::load(&deps.dir),
            deps,
            dedup: inbound::RecentDeliveries::new(),
            workers: worker::Workers::default(),
            pending: HashMap::new(),
            lifecycle: bridge::Lifecycle::new(),
            sticky: slack::StickyBoard::default(),
            perm_pending: HashMap::new(),
            narration: HashMap::new(),
            hooks_file: config.hooks_file,
            mcp_port: config.mcp_port,
            mcp_token: config.mcp_token,
            bot_user_id: config.bot_user_id,
            started_at_ms: config.started_at_ms,
            cmd_tx: config.cmd_tx,
            ask_gateway: config.ask_gateway,
            sign_in: command::SignIn::default(),
            restarting: false,
            limited_until_ms: 0,
            usage_polled_at_ms: 0,
            usage_at_risk: false,
            usage_warned_pct: 0,
            stall: HashMap::new(),
            fleet: config.fleet,
            machine_name: config.machine_name,
        }
    }

    #[cfg(test)]
    fn for_test(deps: Deps) -> (Bridge, mpsc::Receiver<CmdFx>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let config = Config {
            hooks_file: String::new(),
            mcp_port: 0,
            mcp_token: String::new(),
            bot_user_id: Some("U_BOT".into()),
            started_at_ms: deps.clock.now_ms(),
            fleet: false,
            machine_name: "test-machine".into(),
            cmd_tx,
            ask_gateway: None,
        };
        (Bridge::new(deps, config), cmd_rx)
    }

    /// Posts one start / stop notice to home. **Failures are only logged**;
    /// they stop neither start-up nor restart. No DM fallback yet (but no silence either).
    /// Posts "online" to home. Used both **at start-up and when the link to the gateway is re-established**
    /// (for a machine this one line is the only way to tell a person "connected" — the gateway's presence only reports 🔴).
    async fn announce_online(&mut self, connected_as: &str) {
        let pools: Vec<String> = self.access.pool_targets(&Host::home());
        let text = online_notice(
            &Host::name().await,
            // A notice for people to read, so show the **name**
            connected_as,
            env!("CARGO_PKG_VERSION"),
            &pools,
            0,
        );
        self.post_notice(&text, &LogCtx::default()).await;
    }

    async fn post_notice(&self, text: &str, ctx: &LogCtx) {
        match bridge::NoticeTarget::of(self.access.home_channel.as_deref(), &self.access.owner) {
            bridge::NoticeTarget::Home(ch) => {
                slack::Api::brief_call(
                    &format!("home notice post failed for {ch}"),
                    self.deps.slack.post_message_no_unfurl(&ch, text, None),
                    ctx,
                )
                .await;
            }
            // Without a home channel, fall back to the Owner's DM.
            // Reopening a DM returns the same id, so open it on the spot and post
            bridge::NoticeTarget::OwnerDm(owner) => match self.deps.slack.open_dm(&owner).await {
                Ok(ch) => {
                    slack::Api::brief_call(
                        &format!("owner DM notice post failed for {owner}"),
                        self.deps.slack.post_message_no_unfurl(&ch, text, None),
                        ctx,
                    )
                    .await;
                }
                Err(e) => ctx.error(
                    "bridge",
                    &format!("home notice skipped: could not open a DM with {owner}: {e}"),
                ),
            },
            bridge::NoticeTarget::None_ => ctx.info(
                "bridge",
                "home notice skipped: no home_channel and no owner — nothing to notify",
            ),
        }
    }

    /// `restart` (simplified). The job is to **take down** this
    /// Bridge — the supervisor starts it again, and the successor closes the checklist.
    ///
    /// `req` = the requester's thread `(channel, root_ts)`. Some for Slack's `restart`, None for an operator's
    /// SIGUSR1 (there's nobody to answer, so no checklist, no thinking status, no marker
    /// — the same branch as `req ? … : undefined`).
    ///
    /// ponytail: no plugin updating (Rust is a single binary — replacing it is the install script's job).
    /// Unanswered messages are "announced and left behind" — there's no auto-resume yet, so the Owner re-sends to continue
    async fn maintenance_restart(&mut self, source: &str, req: Option<(&str, &str)>, ctx: &LogCtx) {
        // A select arm runs to completion, so a second one can't get in with the current code, but keep this as an ordering promise
        if self.restarting {
            ctx.info(
                "bridge",
                &format!("maintenance restart ignored — already in flight ({source})"),
            );
            return;
        }
        self.restarting = true;
        // exit(0) doesn't run Drop — restart alone skips the slack::Thinking guard and
        // **awaits** both set and clear on the spot (fire-and-forget would be killed along with its task by exit)
        if let Some((channel, root_ts)) = req {
            slack::Api::brief_call(
                "restart: thinking status set failed",
                self.deps.slack
                    .set_thinking_status(channel, root_ts, &slack::Status::Restart.text()),
                ctx,
            )
            .await;
        }
        // (a) Post one progress checklist and note its ts (it's edited from then on). **A failed post
        // doesn't stop the restart** — the list is decoration
        let progress_ts = match req {
            Some((channel, root_ts)) => {
                let first = RestartPhase::Received.render(None);
                slack::Api::brief_call(
                    &format!("slack-events: restart progress post failed for {channel}:{root_ts}"),
                    self.deps.slack
                        .post_message_no_unfurl(channel, &first, Some(root_ts)),
                    ctx,
                )
                .await
            }
            None => None,
        };
        ctx.info(
            "bridge",
            &format!("maintenance restart triggered ({source})"),
        );
        // (b) Tell threads still owed a reply
        for key in self.ledger.pending_keys() {
            let (channel, thread) = key.split();
            slack::Api::brief_call(
                &format!("restart notice failed key={key}"),
                self.deps.slack
                    .post_message_no_unfurl(&channel, &restart_notice(), thread.as_deref()),
                ctx,
            )
            .await;
        }
        // (c) The handoff to the successor. Without it the checklist freezes at "◌ …".
        //     Not written when there's no requester — leaving a marker with nowhere to send ✅
        // makes a later, unrelated start pick it up and post a bogus "done" (invariant)
        if let Some((channel, root_ts)) = req {
            let marker = serde_json::json!({
                "channel": channel,
                "thread_ts": root_ts,
                "progress_ts": progress_ts,
            });
            match self.deps.dir.write_json_atomic("restart-marker.json", &marker) {
                Ok(()) => ctx.info("bridge", &format!(
                        "wrote restart marker (requester carrier for the ✅ reply) \
                         (requester {channel}:{root_ts})"
                    )),
                Err(e) => ctx.error("bridge", &format!(
                        "could not write restart marker (the ✅ back-online reply may be skipped): {e}"
                    )),
            }
        }
        // (d) Tell home once that we're going down (the successor posts the online notice)
        let offline = offline_notice(&Host::name().await, env!("CARGO_PKG_VERSION"), "restart");
        self.post_notice(&offline, ctx).await;
        // (e) Mark up to "stop the Bridge" as done before going down. The list freezes for the few seconds until the successor connects
        if let (Some((channel, _)), Some(ts)) = (req, &progress_ts) {
            let switching = RestartPhase::Switching.render(None);
            slack::Api::brief_call(
                &format!("restart: progress checklist update failed for {channel}:{ts}"),
                self.deps.slack.update_message(channel, ts, &switching),
                ctx,
            )
            .await;
        }
        // (f) The pool is **not torn down**. Tearing it down makes the successor start fresh sessions,
        //     piling a throwaway pool session into claude's history on every restart.
        //     The successor relies on the pools.json nominations to pick up survivors with `restore_pools`
        //     (only dead slots are started with the same session_id via `--resume`)
        // Always clear before going down. Left in place, this thread's shimmer lingers with nobody to clear it
        // (the successor doesn't know about a status it didn't set)
        if let Some((channel, root_ts)) = req {
            slack::Api::brief_call(
                "restart: thinking status clear failed",
                self.deps.slack.set_thinking_status(channel, root_ts, ""),
                ctx,
            )
            .await;
        }
        ctx.info(
            "bridge",
            "maintenance restart: stepping down now (the supervisor brings the successor up)",
        );
        // Save changes since the last tick before going down — agents survive, so if the successor
        // can't pick up the unanswered messages, those threads fall outside the watch
        self.save_ledger();
        self.flush_pending_to_disk(ctx);
        std::process::exit(0);
    }

    /// Adopts the mutated access and saves it. From then on gate / resolve_repo_path read this
    /// (adopted even if saving fails — disagreeing with the current answer would be more confusing).
    fn adopt_access(&mut self, access: bridge::Access, ctx: &LogCtx) {
        if let Err(e) = access.save(&self.deps.dir) {
            ctx.error("bridge", &format!("access.json save failed: {e}"));
        }
        self.access = access;
        // When settings change, adjust the pool **right away**. Waiting for the next death or restart
        // leaves `warm off` sitting there without releasing anything
        self.start_missing_pool_workers(ctx);
    }

    /// One direct reply from the Bridge. The caller doesn't wait — Slack's answer happens outside the ledger.
    fn post(&self, channel: &str, thread_ts: &str, text: String, key: &ThreadKey) {
        let (api, channel, thread_ts, key) = (
            self.deps.slack.clone(),
            channel.to_string(),
            thread_ts.to_string(),
            key.clone(),
        );
        tokio::spawn(async move {
            api.post_now(&channel, &thread_ts, text, &key).await;
        });
    }

    /// SIGTERM / SIGINT — **really stop**. No successor comes.
    /// Unlike restart (SIGUSR1 / Slack's `restart`), no agent is left running:
    /// a claude + tmux window with no Bridge becomes an orphan nobody cleans up.
    ///
    /// Note that slack-morphism grabs TERM_SIGNALS itself once Socket Mode is up
    /// (tokio_clients_manager.rs:151-161 — it just logs at debug and doesn't end the process).
    /// signal-hook's registry allows several receivers per signal, so it coexists with this arm.
    async fn shutdown(&mut self, reason: &str) -> ! {
        let ctx = LogCtx::default();
        ctx.info(
            "bridge",
            &format!("shutting down ({reason}) pid={}", std::process::id()),
        );
        eprintln!("slack bridge: shutting down ({reason})");
        // Always go down (5 seconds), even if something below gets stuck.
        // Teardown waits out tmux's SIGTERM grace, so this is the only upper bound
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            std::process::exit(0);
        });
        // Post offline **first**. Teardown takes seconds, so leaving it for later
        // gets the notice eaten by the hard exit above (the same accident is noted there)
        let offline = offline_notice(&Host::name().await, env!("CARGO_PKG_VERSION"), reason);
        self.post_notice(&offline, &ctx).await;
        // Save them before tearing down — agents go down with it, so requests left in the queue can only be rescued here
        self.flush_pending_to_disk(&ctx);
        self.save_ledger();
        self.teardown_all_workers("shutdown", "", &ctx).await;
        ctx.info("bridge", &format!("shutdown complete ({reason})"));
        std::process::exit(0);
    }

    /// SIGHUP. access.json / threads.json are **authoritative in memory**, so
    /// this is the only way to take in hand edits.
    fn reload_from_disk(&mut self) {
        let ctx = LogCtx::default();
        let access = bridge::Access::load(&self.deps.dir);
        self.adopt_access(access, &ctx);
        self.threads = bridge::Threads::load(&self.deps.dir);
        ctx.info(
            "bridge",
            "access.json + threads.json reloaded from disk (SIGHUP)",
        );
    }

    /// Wires things up and runs the select loop. The signal contract is
    /// SIGTERM/SIGINT=graceful shutdown / SIGUSR1=maintenance restart / SIGHUP=reload.
    pub async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let dir = crate::state_dir::StateDir::resolve();
        for (k, v) in dir.load_env()? {
            // Safe: single-threaded section at start-up, before any spawn
            unsafe { std::env::set_var(k, v) };
        }
        // Upstream (direct or via the gateway) and downstream (accepting machines). **A config with both doesn't start**
        // (silently connecting both makes Slack start load-balancing, bringing split-brain straight back)
        let wiring = machine::Wiring::resolve(|k| std::env::var(k).ok())?;
        let mode = wiring.upstream.clone();
        LogCtx::default().info(
            "bridge",
            &format!("starting — state dir {}", dir.path().display()),
        );

        let (msg_tx, mut msg_rx) = mpsc::channel(64);
        let (click_tx, mut click_rx) = mpsc::channel(16);
        // Take it **before** starting to listen. Taken after, live commands that arrived during the few hundred ms of
        // connecting and auth.test look like "posted before start-up" and are silently dropped
        let started_at_ms = Host::now_ms();

        let machine_name = match wiring.self_id.clone() {
            Some(id) if !id.is_empty() => id,
            _ => Host::name().await,
        };
        // What this Bridge sends up to the gateway: answers (`ProjectSet`) and the commands only the
        // gateway can run (`channels`, `pwd <machine>`). On the gateway itself it loops back into its own
        // fleet, so both sides go through the same handler
        let (up_tx, up_rx) = mpsc::unbounded_channel();
        let up_is_linked = !matches!(wiring.upstream, machine::Mode::Direct { .. });
        let uplink: machine::Uplink = Arc::new(tokio::sync::Mutex::new(up_rx));
        // Via Relay the bot token **comes in the handshake**, so the Api can only be built after it.
        // This is the only ordering difference from a direct connection.
        let (bot_token, link_home, relay_rx) = match &mode {
            machine::Mode::Direct { bot_token, .. } => (bot_token.clone(), None, None),
            machine::Mode::Relay {
                url,
                api_token,
                bridge_id,
            } => {
                let (tx, mut rx) = mpsc::channel(64);
                let l = Arc::new(machine::RelayLink::new(url, api_token, bridge_id, uplink.clone()));
                tokio::spawn({
                    let l = l.clone();
                    async move { l.run(tx).await }
                });
                // Nothing can be written to Slack until the first Ready. **Wait**
                let (token, home) = loop {
                    match rx.recv().await {
                        Some(machine::FromRelay::Ready { bot_token, home }) => {
                            break (bot_token, home);
                        }
                        // **Don't exit when the handshake is refused.** Exiting makes the supervisor restart at once,
                        // and that hammering trips systemd's start rate limit (default 5 in 10 seconds),
                        // leaving the unit `failed` — a momentary 401 during a deploy
                        // keeps a machine down for good (2026-08-03, a machine stopped for 5 hours 15 minutes).
                        // `RelayLink::run` keeps reconnecting with pauses in between, so just wait here
                        Some(machine::FromRelay::Fatal(f)) => {
                            LogCtx::default().error(
                                "bridge",
                                &format!("remote link: {} — retrying until it is fixed", f.message()),
                            );
                            continue;
                        }
                        Some(_) => continue, // anything arriving before acceptance is dropped
                        None => return Err("relay link ended before the handshake".into()),
                    }
                };
                (token, home, Some(rx))
            }
            // The gateway comes to us. **Waiting is the same** — nothing can be written to Slack until the first Ready
            machine::Mode::AwaitParent => {
                let Some(listen) = wiring.inlet.clone() else {
                    return Err("AGENTGW_LINK_LISTEN is not set, so the gateway has nowhere to connect".into());
                };
                let (tx, mut rx) = mpsc::channel(64);
                let inlet = Arc::new(machine::GatewayInlet {
                    token: listen.token,
                    tx,
                    up: uplink.clone(),
                });
                tokio::spawn(inlet.serve(listen.addr));
                let (token, home) = loop {
                    match rx.recv().await {
                        Some(machine::FromRelay::Ready { bot_token, home }) => {
                            break (bot_token, home);
                        }
                        Some(_) => continue,
                        None => return Err("the inlet closed before the parent arrived".into()),
                    }
                };
                (token, home, Some(rx))
            }
        };
        // Apply the home carried in the first handshake here. `Access::load` rereads it after this,
        // so the start-up notice (online) goes to the same channel as the gateway's from the start
        adopt_home(&dir, link_home);
        if !matches!(mode, machine::Mode::Direct { .. }) {
            drop_gateway_records(&dir);
        }

        let api: crate::chat::ChatRef = Arc::new(slack::Api::new(&bot_token)?);
        let (hook_tx, mut hook_rx) = mpsc::channel(64);
        let (dispo_tx, mut dispo_rx) = mpsc::channel(64);
        let (hook_port, hook_token) = HookIntake::serve(&dir, hook_tx.clone()).await?;
        let (mcp_port, mcp_token) = mcp::Mcp::serve(
            &dir,
            Arc::new(turn::ToolExec {
                slack: api.clone(),
                state_dir: dir.path().to_path_buf(),
                dispo: dispo_tx,
            }),
            hook_tx,
        )
        .await?;
        let hooks_file = HookIntake::write_settings(&dir, hook_port, &hook_token)?;
        let hooks_file = hooks_file.to_string_lossy().to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (reload_tx, mut reload_rx) = mpsc::channel(4);
        // The signal that the link to the gateway was re-established (only machines use it)
        let (relink_tx, mut relink_rx) = mpsc::channel(4);
        consume_restart_marker(&dir, api.as_ref()).await;

        // When accepting machines, Slack events go here **before being folded**. Once it's decided whose they are,
        // only our own share goes back to msg_tx / click_tx (= local delivery goes through the same conversion as a direct connection)
        let fleet = wiring.children.map(|listen| {
            // If exposed beyond loopback, leave one line. **Not refused** (decided 2026-08-02), but
            // a Bridge running without the TLS in front can't be noticed if the log shows no trace either
            if listen.is_exposed() {
                LogCtx::default().info(
                    "bridge",
                    &format!(
                        "children port {} is outside the loopback — \
                         put TLS in front (tailscale serve / reverse proxy) or the bot token \
                         crosses the network in the clear",
                        listen.addr
                    ),
                );
            }
            let fleet = Arc::new(crate::bridge::gateway::Fleet {
                links: crate::bridge::gateway::LinkServer::new(),
                token: listen.token,
                self_id: wiring.self_id.clone().unwrap_or_default(),
                bot_token: bot_token.clone(),
                api: api.clone(),
                dir: dir.clone(),
                cooldown: Default::default(),
                presence: Default::default(),
                pending_selection: Default::default(),
                bot_user_id: Default::default(),
                msg_tx: msg_tx.clone(),
                click_tx: click_tx.clone(),
                reload: reload_tx.clone(),
                tunnels: Default::default(),
            });
            tokio::spawn(crate::bridge::gateway::serve_children(
                fleet.clone(),
                listen.addr,
            ));
            tokio::spawn(fleet.clone().watch_presence());
            {
                // The gateway's own channels: its Bridge asks through the same channel a machine uses
                let (fleet, up) = (fleet.clone(), uplink.clone());
                let me = wiring.self_id.clone().unwrap_or_default();
                tokio::spawn(async move {
                    while let Some(frame) = up.lock().await.recv().await {
                        fleet.on_machine_frame(&me, frame).await;
                    }
                });
            }
            fleet
        });
        // Only when the gateway is behind NAT do we go out to fetch the machine
        if let Some(fleet) = &fleet
            && let Ok(raw) = std::env::var("AGENTGW_CHILD_URLS")
        {
            let targets = machine::child_urls(&raw);
            if !targets.is_empty() {
                LogCtx::default().info(
                    "relay",
                    &format!(
                        "dialling {} child(ren) from AGENTGW_CHILD_URLS: {}",
                        targets.len(),
                        targets
                            .iter()
                            .map(|(id, _)| id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
                fleet.dial_children(targets);
            }
        }
        // For machines a direct connection can't reach, the gateway opens an ssh tunnel (the list `add-child` writes).
        // **Held as a child process of agentgw** — it only needs to be connected while running
        if let Some(fleet) = &fleet
            && let Ok(raw) = std::env::var("AGENTGW_TUNNELS")
        {
            // The exit is the gateway's port. Even listening on 0.0.0.0, loopback is enough for the tunnel's exit
            let port = std::env::var("AGENTGW_LINK_LISTEN")
                .ok()
                .and_then(|l| l.rsplit(':').next().map(str::to_string))
                .unwrap_or_else(|| "8787".to_string());
            for (child, target) in machine::child_urls(&raw) {
                tokio::spawn(gateway::keep_tunnel(
                    fleet.clone(),
                    child,
                    target,
                    format!("127.0.0.1:{port}"),
                ));
            }
        }
        let fleet_tx = fleet.as_ref().map(|fleet| {
            let (tx, mut rx) = mpsc::channel::<slack::FleetEvent>(64);
            let fleet = fleet.clone();
            tokio::spawn(async move {
                while let Some(item) = rx.recv().await {
                    fleet.on_fleet_event(item).await;
                }
            });
            tx
        });

        match (mode, relay_rx) {
            (machine::Mode::Direct { app_token, .. }, _) => {
                tokio::spawn(async move {
                    if let Err(e) = slack::Api::listen(&app_token, msg_tx, click_tx, fleet_tx).await
                    {
                        LogCtx::default().error("slack", &format!("socket mode stopped: {e}"));
                    }
                });
            }
            // Via the gateway (whether we dial or it comes to fetch us). **Fed into the same two
            // channels**, so not one line downstream changes
            (machine::Mode::Relay { .. } | machine::Mode::AwaitParent, Some(mut rx)) => {
                let sinks = RelaySinks {
                    msg_tx,
                    click_tx,
                    dir: dir.clone(),
                    reload: reload_tx.clone(),
                    relink: relink_tx.clone(),
                    up: up_tx.clone(),
                    machine: machine_name.clone(),
                };
                tokio::spawn(async move {
                    while let Some(item) = rx.recv().await {
                        pump_relay(item, &sinks).await;
                    }
                });
            }
            (machine::Mode::Relay { .. } | machine::Mode::AwaitParent, None) => {
                unreachable!("a machine behind a gateway always has a receiver")
            }
        }

        if bridge::Access::load(&dir).owner.is_empty() {
            LogCtx::default().info("bridge", "no owner in access.json — serving nobody");
        }
        // Asked once at start-up. A failure doesn't stop start-up — command checks just only match the form without a mention
        let (bot_user_id, bot_name) = match api.auth_test().await {
            Ok((id, name)) => {
                LogCtx::default().info(
                    "bridge",
                    &format!("bot user id {id} ({})", name.as_deref().unwrap_or("?")),
                );
                (Some(id), name)
            }
            Err(e) => {
                LogCtx::default().error("bridge", &format!(
                        "auth.test failed: {e} — commands written with an @mention won't be recognized"
                    ));
                (None, None)
            }
        };
        // Fleet command checks need the same id (`@bot route …`)
        if let Some(fleet) = &fleet {
            *fleet.bot_user_id.lock().await = bot_user_id.clone();
        }
        let mut b = Bridge::new(
            Deps {
                slack: api.clone(),
                agent: Arc::new(Claude::real()),
                clock: Arc::new(crate::clock::SystemClock),
                dir,
            },
            Config {
                hooks_file,
                mcp_port,
                mcp_token,
                bot_user_id,
                started_at_ms,
                fleet: fleet.is_some(),
                machine_name: machine_name.clone(),
                cmd_tx,
                ask_gateway: (fleet.is_some() || up_is_linked).then(|| up_tx.clone()),
            },
        );
        if let Some(up) = &b.ask_gateway {
            report_folders(&b.deps.dir, up);
        }
        // Sweep login sessions the previous Bridge left before going down
        b.deps.agent.login_kill();
        // The Owner stays in access.json across restarts. restart goes down without tearing down the pool,
        // so first pick up survivors (restore_pools) and start only the missing slots. Slots with a nominated
        // session come up with `--resume`, not a new ID
        if !b.access.owner.is_empty() {
            b.restore_pools(&LogCtx::default()).await;
            b.start_missing_pool_workers(&LogCtx::default());
        }
        // Pick up threads the previous process was waiting to answer (only those with live agents)
        b.restore_pending(&LogCtx::default());
        // Re-deliver requests that never got handed over (start an agent to hand them to if there is none)
        b.resume_pending_from_disk().await;
        // Tell home once about the start. pending is always 0 — the Rust version has no auto-resume of unfinished threads
        let online_as = bot_name
            .or_else(|| b.bot_user_id.clone())
            .unwrap_or_else(|| "?".to_string());
        b.announce_online(&online_as).await;
        // tokio's signal is in features = ["full"] (no new dependency).
        // slack-morphism grabs TERM_SIGNALS first via signal-hook, but the registry
        // allows several receivers per signal, so both get it
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sighup = signal(SignalKind::hangup())?;
        let mut sigusr1 = signal(SignalKind::user_defined1())?;
        let mut flush = tokio::time::interval(FLUSH_INTERVAL);

        loop {
            tokio::select! {
                Some(msg) = msg_rx.recv() => b.on_inbound(&msg).await,
                Some(ev) = hook_rx.recv() => b.on_hook(ev).await,
                // If this clogs, agents freeze on every MCP tool call — always take it
                Some(d) = dispo_rx.recv() => b.on_disposition(d).await,
                Some(fx) = cmd_rx.recv() => b.on_cmd_fx(fx).await,
                Some(c) = click_rx.recv() => b.on_perm_click(c).await,
                _ = flush.tick() => {
                    b.scan_transcripts();
                    b.stall_tick();
                    b.run_drains().await;
                    b.flush_stickies().await;
                    b.expire_perm_prompts().await;
                    b.retry_pending(&LogCtx::default());
                    b.give_up_stale_pools(&LogCtx::default()).await;
                    b.sweep_pools(&LogCtx::default());
                    b.cleanup_workers().await;
                    b.usage_tick().await;
                    b.sign_in_tick().await;
                }
                _ = sigterm.recv() => b.shutdown("signal:SIGTERM").await,
                _ = sigint.recv() => b.shutdown("signal:SIGINT").await,
                _ = sighup.recv() => b.reload_from_disk(),
                // The fleet side wrote access.json (route / set-home / owner). Same as SIGHUP
                Some(()) = reload_rx.recv() => b.reload_from_disk(),
                // The link to the gateway was re-established. **Post online again** — for a machine
                // this one line is the only way to tell a person "connected" (the gateway's presence
                // only reports 🔴. Don't say the same thing in two places)
                Some(()) = relink_rx.recv() => b.announce_online(&online_as).await,
                // A restart request from the operator. Joins the same path as Slack's `restart`,
                // skipping the checklist, status and marker since there's no requester thread
                _ = sigusr1.recv() => b.maintenance_restart("SIGUSR1", None, &LogCtx::default()).await,
                else => break,
            }
        }
        Ok(())
    }
}

/// **Consumes** the marker the previous process left when going down with `restart` (read, then delete).
/// Left in place, the next start would post an unexplained "✅ restart complete".
/// This implementation has no auto-resume of interrupted threads, so the resume line closes with 0 entries.
async fn consume_restart_marker(dir: &crate::state_dir::StateDir, api: &dyn crate::chat::Chat) {
    let ctx = LogCtx::default();
    let path = dir.restart_marker();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    if let Err(e) = std::fs::remove_file(&path) {
        ctx.error(
            "bridge",
            &format!(
                "could not remove the restart marker (a later start may post a false ✅): {e}"
            ),
        );
    }
    let m: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    let (Some(channel), Some(ts)) = (m["channel"].as_str(), m["progress_ts"].as_str()) else {
        ctx.info(
            "bridge",
            "restart marker consumed — no progress checklist to finish",
        );
        return;
    };
    let done = RestartPhase::Done.render(None);
    match api.update_message(channel, ts, &done).await {
        Ok(()) => ctx.info(
            "bridge",
            &format!("restart: checklist completed for {channel}:{ts} (marker consumed)"),
        ),
        Err(e) => ctx.error(
            "bridge",
            &format!("restart: could not finish the progress checklist for {channel}:{ts}: {e}"),
        ),
    }
}

/// What to ask outside commands about this machine.
pub struct Host;

impl Host {
    /// Gets the host name for home notices with a single `hostname` (same approach as `now_wallclock`).
    /// It's decoration, so failures don't bring anything down — `unknown` is used.
    pub async fn name() -> String {
        let out = tokio::process::Command::new("hostname").output().await;
        match out {
            Ok(o) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if s.is_empty() {
                    "unknown".to_string()
                } else {
                    s
                }
            }
            _ => "unknown".to_string(),
        }
    }

    /// Where agents for channels / DMs with no route stand. **pwd and spawn read this same value**
    /// (usage probes, the cwd fallback for context/resume, and the login session's `-c` all come here too).
    /// `workerHomeOf` = `access.workerHome ?? homedir()` — it must not change with where the Bridge
    /// was started from. Falls back to the current directory only when HOME can't be read.
    ///
    /// ponytail: `access.workerHome` isn't typed yet (it's round-tripped as an unknown field).
    /// Once setup starts writing it, read it here first
    pub fn home() -> String {
        match std::env::var("HOME") {
            Ok(h) if !h.is_empty() => h,
            _ => std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".to_string()),
        }
    }

    /// Wall-clock epoch ms for the wiring in `run()`. Bridge methods read `deps.clock` instead.
    pub fn now_ms() -> u64 {
        crate::clock::now_ms()
    }
}

/// Feeds what comes from Relay into **the same two channels** as direct mode.
///
/// This is where "via Relay" and "direct" meet. Events are turned back into slack-morphism types and then
/// into `InboundMsg` — the point is **not duplicating the conversion logic**: a second copy would sooner or later
/// make the gates decide differently for direct and Relay.
///
/// `reload` is the "access.json was written, reread it" signal. **Writing without it leaves a running
/// Bridge looking at the old value (no Owner), dropping everything that arrives as `no-owner`** — since it's dropped
/// at the door, even the command that fixes the Owner can't get in, and only a restart gets out (happened on a real machine 2026-08-02).
///
/// ponytail: the signal and the payload are separate channels, so for one round where both arrive at the same moment
/// the order can flip (only when the handshake and the payload arrive together). If it matters, wait for an ack over a oneshot
/// Applies the home carried in the handshake to our current value. **true if it changed** (the caller sends the
/// reread signal).
///
/// **The start-up "wait for the first `Ready`" goes through here too.** The wait loop took only the bot token and
/// dropped home, so the home the gateway sends once in `attach()` was read by nobody, and a
/// freshly started machine posted its notices to its own old home (or, unset → the Owner's DM)
/// (2026-08-02 on a real machine: a machine's online notice went to a different channel than the gateway's home).
fn adopt_home(dir: &crate::state_dir::StateDir, home: Option<String>) -> bool {
    let Some(home) = home else { return false };
    let mut access = bridge::Access::load(dir);
    if access.home_channel.as_deref() == Some(home.as_str()) {
        return false;
    }
    access.home_channel = Some(home.clone());
    if let Err(e) = access.save(dir) {
        LogCtx::default().error("bridge", &format!("could not save the home channel: {e}"));
        return false;
    }
    LogCtx::default().info(
        "bridge",
        &format!("remote link: home channel is now {home}"),
    );
    true
}

/// Where [`pump_relay`] hands things on, and who this machine is (for the words it answers with).
struct RelaySinks {
    msg_tx: mpsc::Sender<InboundMsg>,
    click_tx: mpsc::Sender<slack::PermClick>,
    dir: crate::state_dir::StateDir,
    reload: mpsc::Sender<()>,
    relink: mpsc::Sender<()>,
    up: mpsc::UnboundedSender<crate::bridge::gateway::link::LinkFrame>,
    machine: String,
}

/// Tell the gateway where this machine works for each of its channels. The folders are **this machine's
/// record** (it reads them when it starts an agent); the gateway keeps a copy only so `channels` can show
/// them. Sent at start-up and on every reconnect, so the copy is right even for folders set before it
/// started keeping one.
fn report_folders(
    dir: &crate::state_dir::StateDir,
    up: &mpsc::UnboundedSender<crate::bridge::gateway::link::LinkFrame>,
) {
    for (channel, route) in bridge::Access::load(dir).routes {
        let Some(path) = route.repo_path.filter(|p| !p.is_empty()) else {
            continue;
        };
        let _ = up.send(crate::bridge::gateway::link::LinkFrame::ProjectSet {
            channel,
            thread_ts: String::new(),
            result: Ok(path),
        });
    }
}

/// **Who handles which channel is the gateway's record.** A machine that was the gateway once keeps its
/// old assignments in access.json, and then names the wrong machine for a channel it now handles itself.
/// Dropped at start-up; a route left with nothing in it goes too.
fn drop_gateway_records(dir: &crate::state_dir::StateDir) {
    let mut access = bridge::Access::load(dir);
    let before = access.routes.len();
    let mut changed = false;
    for route in access.routes.values_mut() {
        changed |= route.bridge.take().is_some();
    }
    access.routes.retain(|_, r| {
        r.repo_path.is_some() || r.label.is_some() || r.warm.is_some() || r.allowed_tools.is_some() || !r.extra.is_empty()
    });
    changed |= access.routes.len() != before;
    if !changed {
        return;
    }
    match access.save(dir) {
        Ok(()) => LogCtx::default().info(
            "bridge",
            &format!(
                "dropped the gateway's own records from access.json ({before} → {} channel(s) kept)",
                access.routes.len()
            ),
        ),
        Err(e) => LogCtx::default().error("bridge", &format!("could not save access.json: {e}")),
    }
}

async fn pump_relay(item: machine::FromRelay, sinks: &RelaySinks) {
    let RelaySinks {
        msg_tx,
        click_tx,
        dir,
        reload,
        relink,
        up,
        machine,
    } = sinks;
    match item {
        machine::FromRelay::Event { name, event } => {
            if let Some(msg) = slack::inbound_from_relay(&name, &event)
                && msg_tx.send(msg).await.is_err()
            {
                return;
            }
        }
        machine::FromRelay::Action { action, body } => {
            if let Some(click) = slack::perm_click_from_relay(&action, &body)
                && click_tx.send(click).await.is_err()
            {
                return;
            }
        }
        // Comes with every handshake. Apply the home Relay holds to our current value
        // (a machine that missed a `set-home` catches up on reconnect)
        machine::FromRelay::Ready { home, .. } => {
            if adopt_home(dir, home) {
                let _ = reload.send(()).await;
            }
            // The gateway may have restarted without our folders; say where we work
            report_folders(dir, up);
            // **The first one at start-up doesn't come through here** (the wait loop before construction eats it). Only
            // reconnects get here, so post online each time
            let _ = relink.send(()).await;
        }
        // The Owner assigned this machine. **Record the Owner** — a Bridge via Relay
        // knows nothing of its own identity on Slack until this arrives
        machine::FromRelay::Linked {
            owner_user_id,
            channel,
            ..
        } => {
            let mut access = bridge::Access::load(dir);
            if access.owner != owner_user_id {
                access.owner = owner_user_id.clone();
                if let Err(e) = access.save(dir) {
                    LogCtx::default().error("bridge", &format!("could not save the owner: {e}"));
                } else {
                    LogCtx::default().info(
                        "bridge",
                        &format!("remote link: owner is {owner_user_id} (in charge of {channel})"),
                    );
                    let _ = reload.send(()).await;
                }
            }
        }
        // The gateway decided where notices go. Every machine writes to the same place
        machine::FromRelay::Home { channel } => {
            if adopt_home(dir, Some(channel)) {
                let _ = reload.send(()).await;
            }
        }
        // `pwd <this machine>:<path>`. Only this machine can see its folders, so the gateway waits for this
        // answer before handing the channel over — a folder that isn't there changes nothing
        machine::FromRelay::SetProject {
            channel,
            thread_ts,
            path,
        } => {
            let abs = bridge::absolute_project_path(&path, &Host::home());
            let result = if !std::path::Path::new(&abs).is_dir() {
                Err(bridge::no_such_folder(&abs, machine))
            } else {
                let op = bridge::AccessOp::SetRepo {
                    channel: channel.clone(),
                    path: abs.clone(),
                };
                match bridge::Access::load(dir).apply(op) {
                    Ok((next, ..)) => match next.save(dir) {
                        Ok(()) => {
                            let _ = reload.send(()).await;
                            Ok(abs)
                        }
                        Err(e) => Err(format!("could not save access.json: {e}")),
                    },
                    Err(e) => Err(e),
                }
            };
            LogCtx::default().info(
                "bridge",
                &format!("remote link: project for {channel} — {result:?}"),
            );
            let _ = up.send(crate::bridge::gateway::link::LinkFrame::ProjectSet {
                channel,
                thread_ts,
                result,
            });
        }
        // Getting here means the link has been given up. **Don't touch the agents** — running ones
        // keep running. New Slack messages just stop coming until a person fixes the config and restarts
        machine::FromRelay::Fatal(f) => {
            LogCtx::default().error("bridge", &format!("remote link: {} — no new messages will arrive (running workers keep going)", f.message()));
        }
    }
}


// ── The `restart` progress checklist ────────────────────
// Posted **once** in the Owner's thread and edited in place as the restart advances.
// The constraint shaping it: a restart spans two Bridge processes. The old one advances the first few steps and
// goes down (there are a few seconds with nothing connected to Slack, and launchd restarts it with the latest code),
// then the **successor** finishes the rest (it learns which message to edit from the restart marker). So
// both draw the same fixed checklist from one value, "how far it got" — the display doesn't jump.
//
// **Deliberate difference**: the version notes (`stop Bridge (vX)` / `…back (vX)`) are dropped —
// this implementation has no version-reporting machinery at all.

/// A restart phase. `Done`/`Failed` are terminal, not lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPhase {
    /// Accepted the Owner's restart (the first post)
    Received,
    /// The **old** Bridge stops (launchd starts it again)
    Switching,
    /// The **successor** Bridge reconnected to Slack
    Online,
    /// Restart complete (all lines done)
    Done,
    /// Restart aborted (the line in progress becomes failed)
    Failed,
}

impl RestartPhase {
    /// The wording of one checklist line.
    fn step_label(self) -> String {
        match self {
            RestartPhase::Received => crate::t!("Restart requested", "再起動を受け付けました"),
            RestartPhase::Switching => crate::t!("Stopping agentgw", "agentgw を止めています"),
            RestartPhase::Online => crate::t!("agentgw is back online", "agentgw がオンラインに戻りました"),
            RestartPhase::Done | RestartPhase::Failed => String::new(),
        }
    }

    /// Draws the whole checklist at a given progress point:
    ///   - lines up to `completed_through` → `•` (done)
    ///   - the next line → `◌ …` (in progress), or `💥` if there's a `failed_reason`
    ///   - after that → `◌` (not started)
    ///   - when done, `✅ restart complete` at the end; on failure `💥 restart failed — <reason>`
    ///
    /// `RestartPhase::Done` makes every line done; `Failed` makes **the first line** failed.
    pub fn render(&self, failed_reason: Option<&str>) -> String {
        let completed_through = *self;
        let failed = completed_through == RestartPhase::Failed || failed_reason.is_some();
        // The **number** of done lines. A mid-way failure is expressed as "the last done phase + failed_reason"
        // (the line after it becomes 💥); `Failed` itself is the degenerate case of failing before doing anything.
        let done_count = match completed_through {
            RestartPhase::Done => RESTART_STEPS.len(),
            RestartPhase::Failed => 0,
            p => RESTART_STEPS
                .iter()
                .position(|k| *k == p)
                .map_or(0, |i| i + 1),
        };
        let active = (completed_through != RestartPhase::Done).then_some(done_count);

        // One line is `<glyph> <label>` — no indent
        let mut lines: Vec<String> = RESTART_STEPS
            .iter()
            .enumerate()
            .map(|(i, step)| {
                let label = step.step_label();
                if i < done_count {
                    format!("• {label}")
                } else if active == Some(i) {
                    if failed {
                        format!("💥 {label}")
                    } else {
                        format!("◌ {label}…")
                    }
                } else {
                    format!("◌ {label}")
                }
            })
            .collect();
        if completed_through == RestartPhase::Done {
            lines.push(crate::t!("✅ Restart complete", "✅ 再起動が完了しました"));
        } else if failed {
            let why = failed_reason.map(|r| format!(" — {r}")).unwrap_or_default();
            lines.push(crate::t!("💥 Restart failed{why}", "💥 再起動に失敗しました{why}"));
        }
        lines.join("\n")
    }
}

/// The fixed set of lines, in order. `completed_through` points to **the last fully done line**,
/// and the one after it is in progress. Both Bridges share this table, so the message doesn't jump.
/// "Update the Bridge" and "Resume interrupted threads" from the original table are **left out**:
/// Rust is a single binary with no update feature (replacing it is the install script's job) and no auto-resume,
/// so both were decoration that went "done without doing anything" every time.
const RESTART_STEPS: [RestartPhase; 3] = [
    RestartPhase::Received,
    RestartPhase::Switching,
    RestartPhase::Online,
];

/// The summary attached to the start-up notice in home.
fn startup_notice(pools: &[String], pending_count: u32) -> String {
    let n = pools.len();
    let mut lines = vec![crate::t!("• Started {n} warm agent(s)", "• 待機用のエージェントを {n} 個起動")];
    lines.extend(pools.iter().map(|cwd| format!("  • {cwd}")));
    if pending_count > 0 {
        lines.push(crate::t!(
            "• Resumed {pending_count} unfinished thread(s)",
            "• 途中だったスレッドを {pending_count} 件再開"
        ));
    }
    lines.join("\n")
}

/// The start-up notice in home.
fn online_notice(
    label: &str,
    connected_as: &str,
    version: &str,
    pools: &[String],
    pending_count: u32,
) -> String {
    let summary = startup_notice(pools, pending_count);
    crate::t!(
        "🟢 *{label}* is online — as {connected_as}, agentgw v{version}\n{summary}",
        "🟢 *{label}* がオンラインになりました — {connected_as} として、agentgw v{version}\n{summary}"
    )
}

/// The stop notice in home. `reason` is the internal name for why it stopped, so turn it into human words before showing.
fn offline_notice(label: &str, version: &str, reason: &str) -> String {
    let why = if reason.starts_with("signal:") {
        crate::t!("stopped by the system", "システムに止められたため")
    } else {
        match reason {
            "restart" => crate::t!("restarting", "再起動のため"),
            "logout" => crate::t!("signed out", "サインアウトしたため"),
            other => other.to_string(),
        }
    };
    crate::t!(
        "🔴 *{label}* is going offline ({why}) — agentgw v{version}",
        "🔴 *{label}* がオフラインになります({why})— agentgw v{version}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::HookEvent;
    use crate::agent::{Pid, WindowRow};
    use std::collections::HashSet;
    use crate::bridge::inbound::{ForeignReaction, dedup_key, foreign_reaction, is_own_reaction};

    /// If the deletion key collides with the delivery of the original message, the cancel **never** goes through
    /// (it failed 100% of the time on a real machine). Just check that each kind gets its own key.
    #[test]
    fn dedup_key_separates_a_deletion_from_the_message_it_removes() {
        let base = InboundMsg {
            channel: "C1".into(),
            channel_kind: crate::chat::ChannelKind::Channel,
            ts: "1.1".into(),
            thread_ts: None,
            user: Some("U1".into()),
            is_bot: false,
            bot_id: None,
            text: String::new(),
            files: Vec::new(),
            file_paths: Vec::new(),
            file_errors: Vec::new(),
            reaction: None,
            deleted_ts: None,
            edited: None,
        };
        let deleted = InboundMsg {
            deleted_ts: Some("1.1".into()),
            ..base.clone()
        };
        assert_eq!(dedup_key(&base), "1.1");
        assert_ne!(dedup_key(&deleted), dedup_key(&base));

        // Drop only marks we added. Ones people add pass as before (otherwise stop's ✋ dies)
        let react = |by: &str| InboundMsg {
            user: Some(by.into()),
            reaction: Some(crate::chat::Reaction {
                emoji: "eyes".into(),
                item_ts: "1.1".into(),
                added: true,
            }),
            ..base.clone()
        };
        assert!(is_own_reaction(&react("UBOT"), Some("UBOT")));
        assert!(!is_own_reaction(&react("U1"), Some("UBOT")));
        // Right after start-up, before we know our own id, don't catch people's reactions
        assert!(!is_own_reaction(&react("UBOT"), None));
        assert!(!is_own_reaction(&base, Some("UBOT")), "本文は素通し");
    }

    /// A stop on a post we didn't write passes **only when the Owner put it on their own request**.
    #[test]
    fn only_the_owner_stopping_their_own_request_counts() {
        let r = |emoji: &str| crate::chat::Reaction {
            emoji: emoji.into(),
            item_ts: "1.1".into(),
            added: true,
        };
        let v = |emoji, reactor, author| foreign_reaction(&r(emoji), reactor, author, "UOWNER");
        assert_eq!(
            v("raised_hand", Some("UOWNER"), Some("UOWNER")),
            ForeignReaction::Stop,
            "Owner が自分の依頼に付けた"
        );
        assert_eq!(
            v("raised_hand", Some("UOWNER"), Some("U2")),
            ForeignReaction::Drop,
            "他人の投稿 — 誰の仕事を止めるか決まらない"
        );
        assert_eq!(
            v("raised_hand", Some("U2"), Some("U2")),
            ForeignReaction::Drop,
            "Owner ではない"
        );
        assert_eq!(
            v("thumbsup", Some("UOWNER"), Some("UOWNER")),
            ForeignReaction::Drop,
            "stop の絵文字ではない"
        );
        assert_eq!(
            foreign_reaction(&r("raised_hand"), Some("UOWNER"), Some("UOWNER"), ""),
            ForeignReaction::Drop,
            "Owner がまだ居ない"
        );
        // Removing it doesn't stop anything (`is_stop` is true only when added)
        let removed = crate::chat::Reaction {
            added: false,
            ..r("raised_hand")
        };
        assert_eq!(
            foreign_reaction(&removed, Some("UOWNER"), Some("UOWNER"), "UOWNER"),
            ForeignReaction::Drop,
            "外した"
        );
    }

    /// A machine told "you're in charge" by the gateway not only writes access.json but **also sends the reread signal**.
    /// Without it the running gate keeps an empty Owner and drops every later delivery as `no-owner`
    /// (2026-08-02 on a real machine. Dropped at the door, so even the fixing command couldn't get in; only a restart got out).
    /// A machine that used to be the gateway keeps its old assignments; they name the wrong machine for
    /// channels it now handles itself, so they go at start-up.
    #[test]
    fn a_machine_drops_the_gateways_records() {
        let dir = crate::state_dir::StateDir::at(
            std::env::temp_dir().join(format!("sc-dropgw-{}", std::process::id())),
        );
        let _ = std::fs::remove_dir_all(dir.path());
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            dir.join("access.json"),
            r#"{"owner":"U1","routes":{"C_MINE":{"bridge":"pve","repo_path":"/srv/app"},"C_THEIRS":{"bridge":"pve"}}}"#,
        )
        .unwrap();

        drop_gateway_records(&dir);

        let access = bridge::Access::load(&dir);
        assert_eq!(access.routes.len(), 1, "a route with nothing left goes");
        let mine = &access.routes["C_MINE"];
        assert_eq!(mine.repo_path.as_deref(), Some("/srv/app"), "the folder stays");
        assert_eq!(mine.bridge, None, "who handles it is the gateway's record");
        let _ = std::fs::remove_dir_all(dir.path());
    }

    /// `pwd <this machine>:<path>`: a folder that's there is stored (absolute) and the answer goes up;
    /// one that isn't changes nothing and says why.
    #[tokio::test]
    async fn a_machine_checks_and_stores_the_project_it_is_given() {
        let base = std::env::temp_dir().join(format!("sc-setproject-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = crate::state_dir::StateDir::at(base.join("state"));
        std::fs::create_dir_all(base.join("proj")).unwrap();
        let (msg_tx, _msg_rx) = mpsc::channel(4);
        let (click_tx, _click_rx) = mpsc::channel(4);
        let (reload, mut reload_rx) = mpsc::channel(4);
        let (relink, _relink_rx) = mpsc::channel(4);
        let (up, mut up_rx) = mpsc::unbounded_channel();
        let sinks = RelaySinks { msg_tx, click_tx, dir: dir.clone(), reload, relink, up, machine: "desk".into() };
        let ask = |path: &str| machine::FromRelay::SetProject {
            channel: "C1".into(),
            thread_ts: "1.1".into(),
            path: path.into(),
        };
        let proj = base.join("proj").to_string_lossy().to_string();

        pump_relay(ask(&format!("{proj}/")), &sinks).await;
        let crate::bridge::gateway::link::LinkFrame::ProjectSet { result, .. } = up_rx.try_recv().unwrap() else {
            panic!("an answer goes up")
        };
        assert_eq!(result, Ok(proj.clone()));
        assert_eq!(bridge::Access::load(&dir).repo_path("C1", "/h").0, proj, "stored");
        assert_eq!(reload_rx.try_recv(), Ok(()), "the running Bridge rereads it");

        let missing = base.join("nope").to_string_lossy().to_string();
        pump_relay(ask(&missing), &sinks).await;
        let crate::bridge::gateway::link::LinkFrame::ProjectSet { result, .. } = up_rx.try_recv().unwrap() else {
            panic!("an answer goes up")
        };
        assert!(result.as_ref().is_err_and(|e| e.contains(&missing) && e.contains("*desk*")), "{result:?}");
        assert_eq!(bridge::Access::load(&dir).repo_path("C1", "/h").0, proj, "unchanged");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn being_put_in_charge_asks_the_running_bridge_to_reread_access() {
        let dir = crate::state_dir::StateDir::at(
            std::env::temp_dir().join(format!("sc-linked-{}", std::process::id())),
        );
        let _ = std::fs::remove_dir_all(dir.path());
        let (msg_tx, _msg_rx) = mpsc::channel(4);
        let (click_tx, _click_rx) = mpsc::channel(4);
        let (reload_tx, mut reload_rx) = mpsc::channel(4);
        let (relink_tx, _relink_rx) = mpsc::channel(4);

        let (up, _up_rx) = mpsc::unbounded_channel();
        let sinks = RelaySinks {
            msg_tx,
            click_tx,
            dir: dir.clone(),
            reload: reload_tx,
            relink: relink_tx,
            up,
            machine: "test-machine".into(),
        };
        pump_relay(
            machine::FromRelay::Linked {
                owner_user_id: "U0OWNER".into(),
                channel: "C1".into(),
                thread_ts: "1.1".into(),
            },
            &sinks,
        )
        .await;

        assert_eq!(
            bridge::Access::load(&dir).owner,
            "U0OWNER",
            "ディスクに残る"
        );
        assert_eq!(reload_rx.try_recv(), Ok(()), "読み直しの合図が出る");
        let _ = std::fs::remove_dir_all(dir.path());
    }

    /// The home from the handshake stays on disk. **false for the same value** — sending the reread
    /// signal when nothing changed would make the gate reread access.json on every reconnect.
    #[test]
    fn a_home_from_the_handshake_is_kept_on_disk() {
        let dir = crate::state_dir::StateDir::at(
            std::env::temp_dir().join(format!("sc-adopt-home-{}", std::process::id())),
        );
        let _ = std::fs::remove_dir_all(dir.path());

        assert!(!adopt_home(&dir, None), "home が無い握手は何もしない");
        assert!(adopt_home(&dir, Some("C_HOME".into())), "初回は変わる");
        assert_eq!(
            bridge::Access::load(&dir).home_channel.as_deref(),
            Some("C_HOME"),
            "ディスクに残る"
        );
        assert!(
            !adopt_home(&dir, Some("C_HOME".into())),
            "同じ値なら変わらない"
        );
        let _ = std::fs::remove_dir_all(dir.path());
    }

    /// The 2026-08-03 accident itself: when the cleaner runs with an empty assignment table, **every running agent's
    /// window looks "ownerless" and gets closed**. A round with zero owners + agent windows is left untouched.
    #[test]
    fn the_sweeper_does_not_run_when_it_knows_no_owner_but_worker_windows_exist() {
        let row = |name: &str| WindowRow {
            id: "@1".into(),
            pid: Pid(1),
            command: "claude".into(),
            name: name.into(),
        };
        let none: HashSet<String> = HashSet::new();
        let some: HashSet<String> = ["S-1".to_string()].into_iter().collect();
        let worker = [row("w-S-1")];
        let human = [row("scratch")];

        assert!(
            !Bridge::safe_to_reap(&none, &worker),
            "持ち主ゼロ + 生きたワーカーの窓 = 記憶を失っている。閉じてはいけない"
        );
        assert!(
            Bridge::safe_to_reap(&some, &worker),
            "持ち主が1人でも居れば、いつもどおり掃除する"
        );
        assert!(
            Bridge::safe_to_reap(&none, &human),
            "ワーカーの窓が無いなら掃除しても何も起きない"
        );
    }

    // ── flows through Bridge, with fakes for Slack, the agent and the clock ──

    use crate::agent::fake::FakeAgent;
    use crate::clock::fake::FakeClock;
    use crate::chat::fake::FakeChat;

    /// A fresh state dir with only the owner in access.json.
    fn flow_deps(name: &str) -> (Deps, Arc<FakeChat>, Arc<FakeAgent>, Arc<FakeClock>) {
        let path = std::env::temp_dir().join(format!("agentgw-flow-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("access.json"), r#"{"owner":"U_OWNER"}"#).unwrap();
        let slack = Arc::new(FakeChat::default());
        let agent = Arc::new(FakeAgent::default());
        let clock = FakeClock::at(1_782_000_000_000);
        let deps = Deps {
            slack: slack.clone(),
            agent: agent.clone(),
            clock: clock.clone(),
            dir: crate::state_dir::StateDir::at(path),
        };
        (deps, slack, agent, clock)
    }

    fn channel_msg(ts: &str, user: &str, text: &str) -> InboundMsg {
        InboundMsg {
            channel: "C1".into(),
            channel_kind: crate::chat::ChannelKind::Channel,
            ts: ts.into(),
            thread_ts: None,
            user: Some(user.into()),
            is_bot: false,
            bot_id: None,
            text: text.into(),
            files: vec![],
            file_paths: vec![],
            file_errors: vec![],
            reaction: None,
            deleted_ts: None,
            edited: None,
        }
    }

    #[tokio::test]
    async fn an_owner_message_in_a_new_thread_starts_an_agent_and_acks() {
        let (d, slack, agent, _clock) = flow_deps("owner");
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_inbound(&channel_msg("1782000000.000100", "U_OWNER", "<@U_BOT> fix the tests"))
            .await;
        assert_eq!(agent.spawned.lock().unwrap().len(), 1, "one agent for the new thread");
        assert!(
            slack.calls().contains(&"react C1 1782000000.000100 eyes".to_string()),
            "{:?}",
            slack.calls()
        );
    }

    #[tokio::test]
    async fn a_stranger_cannot_start_work() {
        let (d, slack, agent, _clock) = flow_deps("stranger");
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_inbound(&channel_msg("1782000000.000100", "U_STRANGER", "<@U_BOT> rm -rf /"))
            .await;
        assert!(agent.spawned.lock().unwrap().is_empty());
        assert!(slack.calls().is_empty(), "{:?}", slack.calls());
    }

    #[tokio::test]
    async fn the_second_event_for_the_same_message_is_dropped() {
        let (d, _slack, agent, _clock) = flow_deps("dedup");
        let (mut b, _fx) = Bridge::for_test(d);
        let m = channel_msg("1782000000.000100", "U_OWNER", "<@U_BOT> hi");
        b.on_inbound(&m).await;
        b.on_inbound(&m).await;
        assert_eq!(agent.spawned.lock().unwrap().len(), 1);
    }

    const ROOT: &str = "1782000000.000100";

    /// The first `user_prompt` hook: what lifts the "still starting" latch.
    async fn on_hook_user_prompt_for_test(b: &mut Bridge, sid: &str) {
        b.on_hook(HookEvent {
            kind: "user_prompt".into(),
            session_id: sid.into(),
            payload: serde_json::json!({}),
            respond: None,
        })
        .await;
    }

    /// Lets the tasks Bridge spawned (posts, the status line, reaction flips) run.
    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    fn in_thread(ts: &str, text: &str) -> InboundMsg {
        let mut m = channel_msg(ts, "U_OWNER", text);
        m.thread_ts = Some(ROOT.into());
        m
    }

    /// Starts a thread at ROOT and lets its agent take the first turn. Returns the session id.
    async fn running_thread(b: &mut Bridge, agent: &FakeAgent) -> String {
        b.on_inbound(&channel_msg(ROOT, "U_OWNER", "<@U_BOT> fix the tests"))
            .await;
        let sid = agent.spawned.lock().unwrap()[0].session_id.as_str().to_string();
        on_hook_user_prompt_for_test(b, &sid).await;
        sid
    }

    #[tokio::test]
    async fn a_reply_in_a_running_thread_reaches_the_same_agent() {
        // Observed: once user_prompt clears the latch, the entry's session is Ready and
        // Dispatch::Deliver types the envelope into the window the spawn returned (@0).
        let (d, _slack, agent, _clock) = flow_deps("reply");
        let (mut b, _fx) = Bridge::for_test(d);
        running_thread(&mut b, &agent).await;
        b.on_inbound(&in_thread("1782000000.000200", "and also this"))
            .await;
        assert_eq!(agent.spawned.lock().unwrap().len(), 1, "no second agent");
        let delivered = agent.delivered.lock().unwrap().clone();
        assert_eq!(delivered.len(), 1, "{delivered:?}");
        assert_eq!(delivered[0].0, "@0");
        assert!(delivered[0].1.contains("and also this"), "{delivered:?}");
    }

    #[tokio::test]
    async fn stop_interrupts_the_running_agent() {
        // Observed: `stop` from the Owner with a request still unanswered sends Escape to the
        // worker's window id and posts nothing.
        let (d, slack, agent, _clock) = flow_deps("stop");
        let (mut b, _fx) = Bridge::for_test(d);
        running_thread(&mut b, &agent).await;
        settle().await;
        let before = slack.calls().len();
        b.on_inbound(&in_thread("1782000000.000200", "stop")).await;
        settle().await;
        assert_eq!(*agent.interrupted.lock().unwrap(), vec!["@0".to_string()]);
        assert!(
            !slack.calls()[before..].iter().any(|c| c.starts_with("post")),
            "{:?}",
            slack.calls()
        );
    }

    #[tokio::test]
    async fn a_tool_prompt_is_posted_and_answered_on_allow() {
        // Observed: a tool no standing rule covers (Bash) gets one prompt in the thread; Allow
        // answers the worker with "allow" and rewrites the prompt to ✅ (it is not deleted).
        let (d, slack, agent, _clock) = flow_deps("perm");
        let (mut b, _fx) = Bridge::for_test(d);
        let sid = running_thread(&mut b, &agent).await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        b.on_hook(HookEvent {
            kind: "perm".into(),
            session_id: sid,
            payload: serde_json::json!({
                "tool_name": "Bash",
                "tool_input": {"command": "cargo test"},
                "tool_use_id": "toolu_1",
            }),
            respond: Some(tx),
        })
        .await;
        let perms: Vec<String> = slack
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("perm "))
            .collect();
        assert_eq!(perms.len(), 1, "{:?}", slack.calls());
        let req_id = perms[0].split(' ').nth(3).unwrap().to_string();
        assert_eq!(perms[0], format!("perm C1 {ROOT} {req_id} Bash"));
        let prompt_ts = b.perm_pending[&req_id].prompt_ts.clone();

        b.on_perm_click(slack::PermClick {
            req_id: req_id.clone(),
            action: "allow".into(),
            by: "U_OWNER".into(),
        })
        .await;
        let answer = rx.await.unwrap();
        assert!(answer.to_string().contains("\"allow\""), "{answer}");
        assert!(b.perm_pending.is_empty());
        assert!(
            slack
                .calls()
                .contains(&format!("update C1 {prompt_ts} ✅ `Bash` — allow by <@U_OWNER>")),
            "{:?}",
            slack.calls()
        );
    }

    #[tokio::test]
    async fn silence_shows_the_thinking_status_once() {
        // Observed: the user_prompt hook arms the watchdog quietly; after SILENCE_MS with the
        // request unanswered, stall_tick sets "is thinking…" once and a second tick is a no-op.
        let (d, slack, agent, clock) = flow_deps("stall");
        let (mut b, _fx) = Bridge::for_test(d);
        running_thread(&mut b, &agent).await;
        clock.advance(slack::SILENCE_MS + 1);
        b.stall_tick();
        b.stall_tick();
        settle().await;
        let thinking = format!("status C1 {ROOT} {}", slack::THINKING_STATUS);
        let shown = slack.calls().iter().filter(|c| **c == thinking).count();
        assert_eq!(shown, 1, "{:?}", slack.calls());
    }

    #[tokio::test]
    async fn the_usage_limit_gate_refuses_a_new_thread() {
        // Observed: while limited_until_ms is ahead of the clock, a new request gets the
        // Limited notice in its thread — no ack reaction, no agent.
        let (d, slack, agent, clock) = flow_deps("limit");
        let (mut b, _fx) = Bridge::for_test(d);
        let until_ms = crate::clock::Clock::now_ms(clock.as_ref()) + 3_600_000;
        b.limited_until_ms = until_ms;
        b.on_inbound(&channel_msg(ROOT, "U_OWNER", "<@U_BOT> fix the tests"))
            .await;
        settle().await;
        assert!(agent.spawned.lock().unwrap().is_empty());
        let notice = crate::bridge::turn::limited_notice(until_ms);
        assert_eq!(slack.calls(), vec![format!("post C1 {ROOT} {notice}")]);
    }

    /// A new thread handed to a warm pool agent whose delivery fails is not lost: it waits in
    /// the queue for the retry, and the person is told (same as a failed delivery to a running
    /// thread).
    #[tokio::test]
    async fn a_failed_delivery_to_a_pool_agent_is_kept_and_reported() {
        use crate::agent::{SessionId, SpawnReq};
        use crate::agent::Agent;
        let (d, slack, agent, _clock) = flow_deps("pool-fail");
        let (mut b, _fx) = Bridge::for_test(d);
        let sid = "pool-sid".to_string();
        let home = Host::home();
        let req = SpawnReq {
            session_id: SessionId::from(sid.clone()),
            cwd: home.clone(),
            prompt: None,
            resume_from: None,
            window: SessionId::from(sid.clone()).window_name(),
            state: crate::agent::WorkerState::Absent,
            hooks_file: String::new(),
            mcp_config: String::new(),
        };
        let w = agent.spawn(&req).unwrap();
        b.workers.insert_pool(
            bridge::PoolKey::of_cwd(&home),
            worker::PoolWorker { session_id: sid.clone(), spawned_at_ms: 0, cwd: home, resumed: false },
        );
        let warm = b.workers.warm_mut(&sid);
        warm.mcp_ready = true;
        warm.window_id = Some(w.as_str().to_string());
        agent.fail_deliver.store(true, std::sync::atomic::Ordering::SeqCst);

        b.on_inbound(&channel_msg("1.0", "U_OWNER", "<@U_BOT> start")).await;

        assert_eq!(
            b.threads.get("1.0").and_then(|e| e.agent_id.clone()).as_deref(),
            Some("pool-sid"),
            "the thread went to the warm pool agent"
        );
        assert_eq!(b.pending.get("1.0").map(Vec::len), Some(1), "the message waits for the retry");
        settle().await;
        assert!(
            slack.calls().iter().any(|c| c.contains("Couldn't hand this to the agent")),
            "{:?}",
            slack.calls()
        );
    }

    #[test]
    fn startup_summary_lists_pools_and_omits_zero_pending() {
        let out = startup_notice(&["/home/orchestrator".into(), "/repo/one".into()], 0);
        assert!(out.contains("Started 2 warm agent(s)"));
        assert!(out.contains("/home/orchestrator"));
        assert!(!out.contains("unfinished"));
    }

    #[test]
    fn startup_summary_includes_pending_when_nonzero() {
        let out = startup_notice(&[], 1);
        assert!(out.contains("Resumed 1 unfinished thread(s)"));
    }

    #[test]
    fn online_notice_matches_bun_shape() {
        let out = online_notice("myhost", "botname", "1.2.3", &["/repo/one".into()], 0);
        assert!(out.starts_with("🟢 *myhost* is online — as botname, agentgw v1.2.3\n"), "{out}");
        assert!(out.contains("Started 1 warm agent(s)"));
    }

    #[test]
    fn offline_notice_names_the_reason() {
        let out = offline_notice("myhost", "1.2.3", "restart");
        assert_eq!(out, "🔴 *myhost* is going offline (restarting) — agentgw v1.2.3");
    }

    #[test]
    fn restart_checklist_renders() {
        let first = RestartPhase::Received.render(None);
        assert!(first.starts_with("• Restart requested\n◌ Stopping agentgw…"), "{first}");
        let done = RestartPhase::Done.render(None);
        assert!(done.ends_with("• agentgw is back online\n✅ Restart complete"), "{done}");
        let failed = RestartPhase::Received.render(Some("could not restart"));
        assert!(failed.contains("💥"));
    }

    /// A warm-pool nomination whose conversation was never saved can't be resumed: `claude
    /// --resume` exits at once ("No conversation found"). Seen on a machine where that exit
    /// and the re-launch 5 s later repeated for weeks. Start a fresh session and nominate it.
    #[tokio::test]
    async fn a_pool_nomination_without_history_is_replaced_not_resumed() {
        let (d, _slack, agent, _clock) = flow_deps("pool-nohistory");
        let (mut b, _fx) = Bridge::for_test(d);
        let home = Host::home();
        b.pools.nominate(&home, "gone-sid");
        b.start_missing_pool_workers(&LogCtx::default());
        let spawned = agent.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1);
        assert!(spawned[0].resume_from.is_none(), "there is nothing to resume");
        let sid = spawned[0].session_id.as_str().to_string();
        assert_ne!(sid, "gone-sid");
        assert_eq!(b.pools.session_of(&home), Some(sid.as_str()), "the new session is nominated");
    }

    #[tokio::test]
    async fn a_pool_nomination_with_history_is_resumed() {
        let (d, _slack, agent, _clock) = flow_deps("pool-history");
        agent.histories.lock().unwrap().push("kept-sid".to_string());
        let (mut b, _fx) = Bridge::for_test(d);
        let home = Host::home();
        b.pools.nominate(&home, "kept-sid");
        b.start_missing_pool_workers(&LogCtx::default());
        let spawned = agent.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].resume_from.as_ref().map(|s| s.as_str()), Some("kept-sid"));
        assert_eq!(b.pools.session_of(&home), Some("kept-sid"));
    }

    /// Claude Code sometimes reports a failed turn with no `error_type`. When its own record
    /// says the sign-in expired, re-sending can't help: say so instead of "send it again".
    #[tokio::test]
    async fn a_signed_out_agent_is_reported_not_retried() {
        let (d, slack, agent, clock) = flow_deps("signed-out");
        let (mut b, _fx) = Bridge::for_test(d);
        let sid = running_thread(&mut b, &agent).await;
        *agent.failure_type.lock().unwrap() = Some("authentication_failed");
        let delivered_before = agent.delivered.lock().unwrap().len();
        b.on_hook(HookEvent {
            kind: "error".into(),
            session_id: sid,
            payload: serde_json::json!({ "hook_event_name": "StopFailure", "error_type": null }),
            respond: None,
        })
        .await;
        settle().await;
        assert_eq!(agent.delivered.lock().unwrap().len(), delivered_before, "no re-send");
        assert!(
            slack.calls().iter().any(|c| c.contains("signed out")),
            "{:?}",
            slack.calls()
        );
        // Names the machine and sends the person to `login` in this very thread: the thread's
        // messages reach this machine, so `login` here signs this machine in
        assert!(
            slack.calls().iter().any(|c| c.contains("*test-machine*") && c.contains("Send `login` here")),
            "{:?}",
            slack.calls()
        );
        // Telling the person is the answer: the thread settles, and the silence watchdog
        // doesn't put "is thinking…" back on a turn that is already over
        assert!(b.ledger.pending(&ThreadKey::new("C1", ROOT)).is_empty());
        clock.advance(slack::SILENCE_MS + 1);
        b.stall_tick();
        settle().await;
        let thinking = format!("status C1 {ROOT} {}", slack::THINKING_STATUS);
        assert!(!slack.calls().contains(&thinking), "{:?}", slack.calls());
        // …and the status that was up ("is typing…" from the delivery) is cleared
        let last_status = slack.calls().into_iter().rev().find(|c| c.starts_with("status C1"));
        assert_eq!(last_status, Some(format!("status C1 {ROOT} ")), "{:?}", slack.calls());
    }

    /// A failure that re-sending can fix, on an agent that isn't ready yet: the message stays
    /// unanswered so the recovery paths can still deliver it.
    #[tokio::test]
    async fn a_retryable_failure_on_a_starting_agent_keeps_the_message() {
        let (d, _slack, agent, _clock) = flow_deps("retry-starting");
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_inbound(&channel_msg(ROOT, "U_OWNER", "<@U_BOT> fix the tests")).await;
        let sid = agent.spawned.lock().unwrap()[0].session_id.as_str().to_string();
        b.on_hook(HookEvent {
            kind: "error".into(),
            session_id: sid,
            payload: serde_json::json!({ "hook_event_name": "StopFailure", "error_type": "server_error" }),
            respond: None,
        })
        .await;
        settle().await;
        assert!(!b.ledger.pending(&ThreadKey::new("C1", ROOT)).is_empty());
    }

    /// Once the re-sends are spent the person is told, and the thread settles like any other
    /// reported failure.
    #[tokio::test]
    async fn a_failure_past_the_retry_budget_settles_the_thread() {
        let (d, _slack, agent, _clock) = flow_deps("retry-spent");
        let (mut b, _fx) = Bridge::for_test(d);
        let sid = running_thread(&mut b, &agent).await;
        for _ in 0..=crate::bridge::turn::TURN_FAILURE_RETRY_CAP {
            b.on_hook(HookEvent {
                kind: "error".into(),
                session_id: sid.clone(),
                payload: serde_json::json!({ "hook_event_name": "StopFailure", "error_type": "server_error" }),
                respond: None,
            })
            .await;
        }
        settle().await;
        assert!(b.ledger.pending(&ThreadKey::new("C1", ROOT)).is_empty());
    }

    /// An agent that exits before it reads its first message (a start-up screen it couldn't pass, a
    /// crash) must not leave the thread silent. The next message starts a fresh agent: the session
    /// never began, so there is nothing to resume.
    #[tokio::test]
    async fn an_agent_that_exits_before_its_first_message_is_reported() {
        use crate::agent::{Agent, SpawnOutcome, Window};
        let (d, slack, agent, _clock) = flow_deps("exit-before-prompt");
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_inbound(&channel_msg(ROOT, "U_OWNER", "<@U_BOT> fix the tests")).await;
        let sid = agent.spawned.lock().unwrap()[0].session_id.as_str().to_string();
        agent.terminate(&Window::of("@0")).unwrap(); // the window is gone
        let key = ThreadKey::new("C1", ROOT);
        b.on_cmd_fx(CmdFx::SpawnScreen {
            outcome: SpawnOutcome::Exited,
            what: format!("thread={key}"),
            key: Some(key.clone()),
            session_id: sid,
        })
        .await;
        settle().await;
        assert!(
            slack.calls().iter().any(|c| c.contains("stopped before it could read")),
            "{:?}",
            slack.calls()
        );
        assert!(b.ledger.pending(&key).is_empty());
        b.on_inbound(&in_thread("1782000000.000200", "again")).await;
        let spawned = agent.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 2, "a new agent for the next message");
        assert!(spawned[1].resume_from.is_none(), "nothing to resume");
    }

    /// A window that is still there when the watch loses sight of it (a tmux hiccup) is not an exit.
    #[tokio::test]
    async fn a_live_agent_is_not_reported_as_exited() {
        use crate::agent::SpawnOutcome;
        let (d, slack, agent, _clock) = flow_deps("exit-but-alive");
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_inbound(&channel_msg(ROOT, "U_OWNER", "<@U_BOT> fix the tests")).await;
        let sid = agent.spawned.lock().unwrap()[0].session_id.as_str().to_string();
        let key = ThreadKey::new("C1", ROOT);
        b.on_cmd_fx(CmdFx::SpawnScreen {
            outcome: SpawnOutcome::Exited,
            what: format!("thread={key}"),
            key: Some(key.clone()),
            session_id: sid,
        })
        .await;
        settle().await;
        assert!(!slack.calls().iter().any(|c| c.contains("stopped before")), "{:?}", slack.calls());
        assert!(!b.ledger.pending(&key).is_empty());
    }

    /// When the agent can't even be started, the person is told the same way.
    #[tokio::test]
    async fn a_failed_spawn_is_reported() {
        let (d, slack, agent, _clock) = flow_deps("spawn-fails");
        agent.fail_spawn.store(true, std::sync::atomic::Ordering::SeqCst);
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_inbound(&channel_msg(ROOT, "U_OWNER", "<@U_BOT> fix the tests")).await;
        settle().await;
        assert!(
            slack.calls().iter().any(|c| c.contains("stopped before it could read")),
            "{:?}",
            slack.calls()
        );
        assert!(b.ledger.pending(&ThreadKey::new("C1", ROOT)).is_empty());
    }

    /// `channels` and `pwd <machine>` are the gateway's to answer, and a follow-up in a running thread
    /// never reaches it (no mention). The machine that owns the thread passes them up.
    #[tokio::test]
    async fn the_machine_passes_gateway_commands_up() {
        use crate::bridge::gateway::link::LinkFrame;
        let (d, slack, _agent, _clock) = flow_deps("ask-gateway");
        let (mut b, _fx) = Bridge::for_test(d);
        let (up, mut up_rx) = mpsc::unbounded_channel();
        b.ask_gateway = Some(up);

        b.on_inbound(&channel_msg("1782000001.000100", "U_OWNER", "<@U_BOT> channels")).await;
        b.on_inbound(&channel_msg("1782000001.000200", "U_OWNER", "<@U_BOT> pwd dock:~/x")).await;
        b.on_inbound(&channel_msg("1782000001.000300", "U_OWNER", "<@U_BOT> set-home")).await;
        settle().await;

        assert_eq!(
            up_rx.try_recv(),
            Ok(LinkFrame::Channels { channel: "C1".into(), thread_ts: "1782000001.000100".into() })
        );
        assert_eq!(
            up_rx.try_recv(),
            Ok(LinkFrame::PwdOn {
                channel: "C1".into(),
                thread_ts: "1782000001.000200".into(),
                machine: "dock".into(),
                path: Some("~/x".into()),
            })
        );
        assert_eq!(
            up_rx.try_recv(),
            Ok(LinkFrame::SetHome { channel: "C1".into(), thread_ts: "1782000001.000300".into() }),
            "one notice channel for the whole fleet, so the gateway decides"
        );
        // The gateway answers in the thread; the machine says nothing of its own
        assert!(slack.calls().is_empty(), "{:?}", slack.calls());
    }

    /// On connecting, a machine says where it works for each of its channels — the gateway may have
    /// started with no copy at all, and `channels` would then show machines with no folders.
    #[test]
    fn a_machine_reports_its_folders_when_it_connects() {
        use crate::bridge::gateway::link::LinkFrame;
        let dir = crate::state_dir::StateDir::at(
            std::env::temp_dir().join(format!("sc-report-{}", std::process::id())),
        );
        let _ = std::fs::remove_dir_all(dir.path());
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            dir.join("access.json"),
            r#"{"owner":"U1","routes":{"C_WORK":{"repo_path":"/srv/app"},"C_NONE":{"warm":true}}}"#,
        )
        .unwrap();
        let (up, mut up_rx) = mpsc::unbounded_channel();

        report_folders(&dir, &up);

        assert_eq!(
            up_rx.try_recv(),
            Ok(LinkFrame::ProjectSet {
                channel: "C_WORK".into(),
                thread_ts: String::new(),
                result: Ok("/srv/app".into()),
            })
        );
        assert_eq!(up_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty), "nothing to say for a channel with no folder");
        let _ = std::fs::remove_dir_all(dir.path());
    }

    /// A folder set on the machine itself is reported up, so the gateway's `channels` doesn't go stale.
    #[tokio::test]
    async fn a_folder_set_here_is_reported_to_the_gateway() {
        use crate::bridge::gateway::link::LinkFrame;
        let (d, _slack, _agent, _clock) = flow_deps("pwd-report");
        let (mut b, _fx) = Bridge::for_test(d);
        let (up, mut up_rx) = mpsc::unbounded_channel();
        b.ask_gateway = Some(up);
        b.on_inbound(&channel_msg("1782000001.000100", "U_OWNER", "<@U_BOT> pwd /work/app")).await;
        settle().await;
        assert_eq!(
            up_rx.try_recv(),
            Ok(LinkFrame::ProjectSet {
                channel: "C1".into(),
                thread_ts: String::new(),
                result: Ok("/work/app".into()),
            })
        );
    }

    /// `pwd` names the machine in front of the path — the same path is a different folder elsewhere.
    #[tokio::test]
    async fn pwd_says_which_machine_the_folder_is_on() {
        let (d, slack, _agent, _clock) = flow_deps("pwd-machine");
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_inbound(&channel_msg("1782000001.000100", "U_OWNER", "<@U_BOT> pwd")).await;
        settle().await;
        assert!(
            slack.calls().iter().any(|c| c.contains(&format!("\"test-machine:{}\"", Host::home()))),
            "{:?}",
            slack.calls()
        );
    }

    /// A message that starts with `pwd` but isn't one of the forms gets the usage — it used to go to the
    /// agent as a sentence, so a mistyped path just vanished into the conversation.
    #[tokio::test]
    async fn a_pwd_in_no_known_shape_answers_with_the_usage() {
        let (d, slack, agent, _clock) = flow_deps("pwd-usage");
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_inbound(&channel_msg("1782000001.000100", "U_OWNER", "<@U_BOT> pwd 何か変な値")).await;
        settle().await;
        assert!(agent.spawned.lock().unwrap().is_empty(), "not handed to an agent");
        assert!(
            slack.calls().iter().any(|c| c.contains("`pwd <machine>:~/dev/app`")),
            "{:?}",
            slack.calls()
        );
    }

    /// `pwd ~/…` is stored as the absolute path on this machine (no shell ever expands `~` later).
    #[tokio::test]
    async fn pwd_expands_the_home_directory() {
        let (d, slack, _agent, _clock) = flow_deps("pwd-home");
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_inbound(&channel_msg("1782000001.000100", "U_OWNER", "<@U_BOT> pwd ~/dev/proj")).await;
        settle().await;
        let want = format!("{}/dev/proj", Host::home());
        assert_eq!(b.access.repo_path("C1", "/home").0, want, "{:?}", slack.calls());
    }

    /// A folder that isn't there is refused at `pwd`. Otherwise tmux quietly starts the agent in the
    /// home directory and it works somewhere nobody asked for.
    #[tokio::test]
    async fn pwd_refuses_a_folder_that_does_not_exist() {
        let (d, slack, agent, _clock) = flow_deps("pwd-missing");
        agent.missing_dirs.lock().unwrap().push("/nope/proj".into());
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_inbound(&channel_msg("1782000001.000100", "U_OWNER", "<@U_BOT> pwd /nope/proj")).await;
        settle().await;
        assert!(b.access.repo_path("C1", "/home").1, "nothing registered");
        assert!(
            slack.calls().iter().any(|c| c.contains("/nope/proj") && c.contains("*test-machine*")),
            "{:?}",
            slack.calls()
        );
    }

    /// No warm agent for a folder that isn't there (it would sit in the home directory instead).
    #[tokio::test]
    async fn no_warm_agent_for_a_missing_folder() {
        let (d, _slack, agent, _clock) = flow_deps("pool-missing");
        agent.missing_dirs.lock().unwrap().push(Host::home());
        let (mut b, _fx) = Bridge::for_test(d);
        b.start_missing_pool_workers(&LogCtx::default());
        assert!(agent.spawned.lock().unwrap().is_empty());
    }

    /// A registered folder that has since gone away: say so instead of starting in the home directory.
    #[tokio::test]
    async fn a_missing_project_folder_stops_the_start() {
        let (d, slack, agent, _clock) = flow_deps("repo-gone");
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_inbound(&channel_msg("1782000001.000100", "U_OWNER", "<@U_BOT> pwd /work/proj")).await;
        agent.missing_dirs.lock().unwrap().push("/work/proj".into());
        b.on_inbound(&channel_msg(ROOT, "U_OWNER", "<@U_BOT> fix the tests")).await;
        settle().await;
        let spawned = agent.spawned.lock().unwrap();
        assert!(spawned.iter().all(|r| r.prompt.is_none()), "no agent for the message");
        assert!(agent.delivered.lock().unwrap().is_empty(), "nor handed to a warm one");
        drop(spawned);
        assert!(
            slack.calls().iter().any(|c| c.contains("/work/proj") && c.contains("pwd")),
            "{:?}",
            slack.calls()
        );
        assert!(b.ledger.pending(&ThreadKey::new("C1", ROOT)).is_empty());
    }

    #[tokio::test]
    async fn login_on_a_signed_out_machine_starts_the_sign_in_here() {
        let (d, slack, agent, _clock) = flow_deps("login-signed-out");
        *agent.signed_in.lock().unwrap() = Some(Some(false));
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_inbound(&channel_msg("1782000001.000100", "U_OWNER", "<@U_BOT> login")).await;
        settle().await;
        let calls = slack.calls();
        // The sign-in started here (the link follows after the first poll of the sign-in screen)
        assert!(calls.iter().any(|c| c.starts_with("status C1") && c.contains("Signing in")), "{calls:?}");
        assert!(!calls.iter().any(|c| c.contains("Already signed in")), "{calls:?}");
    }

    #[tokio::test]
    async fn login_on_a_signed_in_machine_says_so() {
        let (d, slack, _agent, _clock) = flow_deps("login-signed-in");
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_inbound(&channel_msg("1782000001.000100", "U_OWNER", "<@U_BOT> login")).await;
        settle().await;
        assert!(slack.calls().iter().any(|c| c.contains("Already signed in")), "{:?}", slack.calls());
    }

    #[tokio::test]
    async fn signing_in_again_keeps_the_owner() {
        let (d, _slack, _agent, _clock) = flow_deps("login-keeps-owner");
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_cmd_fx(CmdFx::LoginFinished { channel: "C1".into(), bound: Some("U_SOMEONE".into()) })
            .await;
        assert_eq!(b.access.owner, "U_OWNER");
    }

    /// Each machine checks its own Claude Code sign-in at start and every 12 hours, and says so
    /// once in the notice channel when it finds it signed out. Seen: a machine signed out for
    /// days while its gateway (and so its owner) was fine.
    #[tokio::test]
    async fn a_signed_out_machine_is_announced_once_until_it_signs_in_again() {
        const HOURS_12: u64 = 12 * 60 * 60 * 1000;
        let (d, slack, agent, clock) = flow_deps("sign-in-watch");
        let (mut b, _fx) = Bridge::for_test(d);
        let said = |slack: &FakeChat| slack.calls().iter().filter(|c| c.contains("is signed out")).count();
        let set = |v: Option<bool>| *agent.signed_in.lock().unwrap() = Some(v);

        set(Some(false));
        b.sign_in_tick().await; // the first check runs at start
        settle().await;
        assert_eq!(said(&slack), 1, "{:?}", slack.calls());
        assert!(slack.calls().iter().any(|c| c.contains("*test-machine*")), "names the machine");

        clock.advance(60 * 60 * 1000);
        b.sign_in_tick().await; // not due yet
        clock.advance(HOURS_12);
        b.sign_in_tick().await; // due, still signed out: no second notice
        settle().await;
        assert_eq!(said(&slack), 1);

        set(Some(true));
        clock.advance(HOURS_12);
        b.sign_in_tick().await; // signed in again: nothing to say
        set(None);
        clock.advance(HOURS_12);
        b.sign_in_tick().await; // couldn't tell: nothing changes
        settle().await;
        assert_eq!(said(&slack), 1);

        set(Some(false));
        clock.advance(HOURS_12);
        b.sign_in_tick().await; // signed out again: say it again
        settle().await;
        assert_eq!(said(&slack), 2);
    }

    /// With an owner set, `login` on a signed-out machine starts the sign-in; the code the owner
    /// pastes next in that channel must reach the sign-in, not the agent. (Seen 2026-09-18: the
    /// code went to the agent as a normal message and the sign-in never finished.)
    #[tokio::test]
    async fn the_code_pasted_after_login_goes_to_the_sign_in_when_an_owner_is_set() {
        let (d, _slack, agent, _clock) = flow_deps("login-code");
        *agent.signed_in.lock().unwrap() = Some(Some(false));
        let (mut b, _fx) = Bridge::for_test(d);
        b.on_inbound(&channel_msg("1782000001.000100", "U_OWNER", "<@U_BOT> login")).await;
        let mut code = channel_msg("1782000002.000100", "U_OWNER", "abc-123#xyz");
        code.thread_ts = Some("1782000001.000100".into());
        b.on_inbound(&code).await;
        settle().await;
        assert_eq!(*agent.submitted_codes.lock().unwrap(), ["abc-123#xyz"]);
        assert!(
            !agent.delivered.lock().unwrap().iter().any(|(_, t)| t.contains("abc-123#xyz")),
            "the code must not reach the agent"
        );
        assert!(agent.spawned.lock().unwrap().is_empty(), "no agent started for the code");
    }
}
