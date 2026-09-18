//! One turn of an agent's work as seen from Slack: hooks from the agent, the start and
//! failure of a turn, the usage-limit watch, the silence watch, tool permission prompts,
//! and writing the progress message.

use super::{Bridge, Host};
use crate::agent::tmux::Window;
use crate::agent::{HookEvent, ProbeErr, SessionId};
use crate::bridge::state as bridge;
use crate::bridge::state::{Disposition, LogCtx, ThreadKey};
use crate::slack;

const NARRATION_CAP: usize = 600;

/// ターン失敗で1つのメッセージを再送してよい回数(現行 `TURN_FAILURE_RETRY_CAP`)。
/// **1回は意図** — 再送で直る失敗(混雑・API の瞬断・型無しの一発)は次の試行で晴れる。
/// 同じ失敗が2度出るなら本物なので、ループを見せるより人に伝える。
const TURN_FAILURE_RETRY_CAP: u32 = 1;

/// 上限の見張りを始めるまでの猶予(立ち上がりに probe をぶつけない)。
const USAGE_MONITOR_STARTUP_DELAY_MS: u64 = 60_000;

/// 人を待つ上限。hook の宣言(125s)と Bridge の待ち(120s)より内側で畳む。
const PERM_WAIT_MS: u64 = 115_000;

/// 人のクリックを待っているツール許可1件。
///
/// **ワーカーの hook はこの `respond` を握ったまま開いている** — 押されるか満期が来るまで返らない。
pub(super) struct PermPending {
    /// hook へ返す口。押された/諦めた時にここへ答えを流す。
    respond: tokio::sync::oneshot::Sender<serde_json::Value>,
    pub(super) channel: String,
    pub(super) thread_ts: String,
    tool_name: String,
    /// Deny のときに**そのツールの行**を 🚫 にするための鍵。
    tool_use_id: String,
    /// 投稿したプロンプトの ts。満期のときに**消す**ためだけに持つ。
    pub(super) prompt_ts: String,
    /// 満期(epoch ms)。hook 側の待ちより少し内側で切る。
    deadline_ms: u64,
}

/// 1スレッドぶんの沈黙見張り。
///
/// **そのスレッド宛のステータス送信口を丸ごと抱える**のが肝で、
/// 配達 / hook / 見張り発火 / 決着の送信が全て `thinking` の1本の直列タスクを通る = 互いに
/// 追い越さない。別々に `tokio::spawn` していた頃は「配達の `is typing…`」と「見張り解除の
/// クリア」が順不同に飛んで打ち消し合っていた。
///
/// entry を落とすと `slack::Thinking` の Drop が最後にクリアを流す — **キューの最後尾に並ぶ**ので、
/// 直前に流した set を必ず追い越さずに消える。
pub(super) struct Stall {
    /// 最後に活動があった時刻(epoch ms)。
    pub(super) last_activity_ms: u64,
    /// 見張りが `is thinking…` を出しているか(現行 `Entry.stalled`)。
    pub(super) shown: bool,
    /// 許可プロンプトが出ていて、ワーカーは**意図的に**黙っている。
    /// 見張りを止める(待っていることは「Permission requested」のプロンプト自身が示す)。
    pub(super) awaiting_perm: bool,
    pub(super) thinking: slack::Thinking,
}

impl Bridge {
    /// スレッドが引けないうちは milestone を出さない(現行4103 と同じ)。
    pub(super) fn milestone(&mut self, key: Option<&ThreadKey>, event: &str, ctx: &LogCtx) {
        let Some(key) = key else {
            ctx.debug("bridge", &format!("{event} before its thread is known"));
            return;
        };
        let m = self.lifecycle.record(key, event, self.deps.clock.now_ms());
        ctx.info("lifecycle", &m.message(key, event));
    }

    pub(super) async fn on_hook(&mut self, mut ev: HookEvent) {
        // hook はセッション ID しか運ばない — スレッドは threads.json から引き戻す
        let owning = self
            .threads
            .find_by_session(&ev.session_id)
            .map(|(ts, _)| ts.clone());
        let key = self.key_of_session(&ev.session_id);
        let ctx = LogCtx {
            session_id: Some(ev.session_id.clone()),
            thread_key: key.clone(),
        };
        // hook が来た = ワーカーは生きて動いている。沈黙見張りを張り直す(現行は turn 開始と
        // progress で `armWatchdog` — hook 全種はその上位集合で、どれも「活動の証拠」)
        if let Some(k) = key.clone() {
            self.touch_thread(&k, ""); // 活動 = 見張り解除。配達と違い新しく出すものは無い
        }
        // どの hook も transcript の在処を運んでくる — 最初に来たもので受信確認の tail を張る
        if let Some(path) = ev.payload["transcript_path"].as_str() {
            let h = self.workers.warm_mut(&ev.session_id);
            if h.transcript_path.as_deref() != Some(path) {
                h.transcript_path = Some(path.to_string());
                h.transcript_offset = 0; // 別ファイルになった(resume 等)なら読み直す
            }
        }
        match ev.kind.as_str() {
            "session_start" => {
                // **掛け金には触らない。** この合図は起動だけでなく compact / clear でも飛ぶので、
                // ここで「起動中」に戻すと動いているワーカー宛の配達が queue で詰まる
                // (2026-08-01 実機。詳しくは `Workers::starting`)
                self.workers.warm_mut(&ev.session_id).ended = false;
                self.milestone(key.as_ref(), "session_start", &ctx);
            }
            "session_end" => {
                self.workers.warm_mut(&ev.session_id).ended = true;
                // 在庫が死んだ = もう在庫ではない(clearForSession)。
                // 残すと ready:true のまま次の新規スレッドに引き当てられ、配達ごと失う
                self.drop_pool_worker(&ev.session_id, "ended", &ctx);
                self.milestone(key.as_ref(), "session_end", &ctx);
            }
            // ツールが呼べた = MCP を握っている(継承ワーカーは initialize を見せる機会が
            // 二度と無い — mcp.rs の call_tool が唯一の証拠を送ってくる)。milestone は出さない
            "mcp_ready" => self.workers.warm_mut(&ev.session_id).mcp_ready = true,
            // MCP を握った = disposition ツールが呼べる(Stop を強制してよい唯一の証拠)
            "mcp_initialized" => {
                // 在庫が「使える」になる唯一の瞬間でもある(`Workers::pool_ready` がこれを読む)
                self.workers.warm_mut(&ev.session_id).mcp_ready = true;
                self.milestone(key.as_ref(), "mcp_initialized", &ctx);
            }
            "user_prompt" => {
                // 最初のターンを踏んだ = TUI がキーを取れている。掛け金を外して queue を流す
                self.workers.clear_starting(&ev.session_id);
                self.milestone(key.as_ref(), "user_prompt", &ctx);
                self.on_turn_start(key.as_ref(), &ctx);
                self.flush_queued(&ev.session_id, owning, key.as_ref(), &ctx);
            }
            "perm" => self.on_perm(&mut ev, key.as_ref(), &ctx).await,
            "error" => self.on_turn_failure(&ev, key.as_ref(), &ctx),
            "progress" => self.on_progress(key.as_ref(), &ev.payload),
            "narration" => self.on_narration(key.as_ref(), &ev.payload, &ctx),
            "stop" => {
                let out = self.stop_decision(key.as_ref(), &ev, &ctx);
                // block したならターンは**続く**(再プロンプト)ので、まだ終わりではない
                if let Some(key) = key.as_ref().filter(|_| out.get("decision").is_none()) {
                    self.sticky.on_turn_end(key);
                }
                if let Some(respond) = ev.respond.take() {
                    let _ = respond.send(out);
                }
            }
            other => ctx.debug("bridge", &format!("unhandled hook {other}")),
        }
    }

    /// ターンが失敗した(`StopFailure`)。理由を分類し、スレッドで待っている人に**日本語で**伝える
    /// (`case 'error'`)。黙って失敗すると、人からは
    /// 「ボットが無視した」ようにしか見えない。
    ///
    /// **ログはスレッドが引ける前に、無条件で出す** — ターン失敗は決して黙る経路ではない。
    /// 生の payload も一緒に残す: `error_type` は実機で**空のまま**届いたことがあり、記録が
    /// 無いと「Claude が型を送らなかった」と「こちらが落とした」を区別できない。
    ///
    /// 手順は現行のまま崩さない: ログ → スレッド門番 → **上限ゲート** → **再配達** → 文面。
    fn on_turn_failure(&mut self, ev: &HookEvent, key: Option<&ThreadKey>, ctx: &LogCtx) {
        let reason = ev.payload["error_type"].as_str().unwrap_or("");
        let (klass, text) = crate::bridge::render::TurnFailureClass::of(reason);
        let raw = serde_json::json!({
            "hook_event_name": ev.payload["hook_event_name"],
            "error_type": ev.payload["error_type"],
            "session_id": ev.session_id,
        });
        ctx.info(
            "bridge",
            &format!(
                "slack-events: disposition=turn_failure thread={} reason={} class={klass} raw={raw}",
                key.map_or("?", ThreadKey::as_str),
                if reason.is_empty() {
                    "(none sent)"
                } else {
                    reason
                },
            ),
        );
        // スレッドが引けなければここまで。home チャンネルへの写しは出さない
        // 失敗は1つのスレッドのもので、そこで待っている人には下で伝わる
        let Some((channel, Some(thread_ts))) = key.map(ThreadKey::split) else {
            return;
        };
        let key = key.expect("thread split above implies a key");
        // 何よりも先に「これは上限か」。`error_type` は答えられない(実機で空のまま届いたし、
        // `rate_limit` は混雑にも使われる)ので、ワーカー自身の履歴に訊く。上限への再送は
        // **唯一絶対に効かない答え**で、しかも人が必要としている1文を沈黙に置き換える
        if !ev.session_id.is_empty() && self.turn_hit_usage_limit(&ev.session_id, key, ctx) {
            return;
        }
        // retry 級は「もう一度配達する」で晴れるもの。配達できたら何も投稿しない —
        // 人が見るべきは再送したターンの結末そのもの
        if klass == crate::bridge::render::TurnFailureClass::Retry
            && self.retry_turn_failure(key, reason, ctx)
        {
            return;
        }
        self.post_error_frame(channel, thread_ts, text);
    }

    /// このターンは「上限に当たった」で死んだのか(`turnHitUsageLimit`)。再送の**手前**で訊く —
    /// 再送だけは絶対に効かないから(壁はリセットまで動かない)。
    ///
    /// 現行がこれを入れた実測(2026-07-17): 上限に当たった1秒後に StopFailure が届いた。
    /// `error_type` は**空**だったので `retry` と読まれ、モーダルで固まったワーカーへ静かに
    /// 再送された。Claude Code はその答え(リセット時刻つき)をそのワーカーの履歴に**既に
    /// 書いていた**。誰も読まず、沈黙は90分続いた。
    ///
    /// true = ゲートを立ててスレッドにも伝えた(呼び手は再送してはいけない)。ゲートは文面と
    /// 同じくらい大事で、立っていれば**次の**メッセージは入口でリセット時刻を貰える。
    fn turn_hit_usage_limit(&mut self, session_id: &str, key: &ThreadKey, ctx: &LogCtx) -> bool {
        let remembered = self
            .workers
            .warm(session_id)
            .and_then(|h| h.transcript_path.clone());
        let hit =
            match self
                .deps.agent
                .session_limit_error(remembered.as_deref(), session_id, self.deps.clock.now_ms())
            {
                Some(Ok(hit)) => hit,
                // 履歴が読めない・見つからないのは「上限ではない」の証拠にならないが、これ以上
                // 訊く先が無い。現行と同じく通常のターン失敗の扱いに落とす
                Some(Err(e)) => {
                    ctx.info(
                        "bridge",
                        &format!("could not read the transcript tail of session={session_id}: {e}"),
                    );
                    return false;
                }
                None => return false,
            };
        let Some(hit) = hit else { return false };
        self.limited_until_ms = hit.reset_ms;
        ctx.info(
            "bridge",
            &format!(
                "turn failure for {key} is the USAGE LIMIT, by Claude Code's own record \
                 (\"{}\") — not re-sending (the wall does not move); hard-limit gate set \
                 until {}",
                hit.detail, hit.reset_ms
            ),
        );
        let (channel, thread_ts) = key.split();
        if let Some(ts) = thread_ts {
            let text = crate::bridge::render::Notice::Limited {
                until_ms: hit.reset_ms,
            }
            .render();
            self.post(&channel, &ts, text, key);
        }
        true
    }

    /// ターンが失敗し、答えられなかったメッセージがまだ未応答 — 「諦めました」と人に言う前に
    /// **同じワーカーへもう一度配達する**(`retryTurnFailure`)。
    ///
    /// なぜタイマーではなくここか: 失敗は**もう分かっている**(Claude Code がターンの死んだ
    /// 瞬間に報告する)。これまで未応答を配り直す唯一の道はワーカーの**死**だったので、
    /// 生きたワーカーの下で失敗したターンは、誰にも答えられないまま台帳に残り続けた。
    ///
    /// 再送するのはワーカーが**本当に聞こえる**とき(Ready)だけ。それ以外は受領タイマーと
    /// 回収経路が既にそのメッセージの持ち主で、押し込んでも2度目の黙殺になる。
    /// 戻り値 true = 配達した(呼び手は何も投稿しない)。
    fn retry_turn_failure(&mut self, key: &ThreadKey, reason: &str, ctx: &LogCtx) -> bool {
        let why = if reason.is_empty() {
            "no type sent"
        } else {
            reason
        };
        let undisposed = self.ledger.undisposed(key);
        if undisposed.is_empty() {
            ctx.info(
                "bridge",
                &format!(
                    "turn failure ({why}) for {key} — nothing undisposed to re-send; \
                     telling the user"
                ),
            );
            return false;
        }
        let (_, root) = key.split();
        let root_ts = root.unwrap_or_default();
        let entry = self.threads.get(&root_ts).cloned();
        let sid = entry.as_ref().and_then(|e| e.agent_id.clone());
        let window = sid
            .as_deref()
            .map(|s| SessionId::from(s.to_string()).window_name())
            .unwrap_or_default();
        let state = self.workers.state_of(entry.as_ref(), &window, self.deps.agent.as_ref());
        if state != crate::agent::WorkerState::Ready {
            ctx.info(
                "bridge",
                &format!(
                    "turn failure ({why}) for {key} — worker is {state:?}, not READY: leaving \
                     the message to the receipt/recovery paths and telling the user"
                ),
            );
            return false;
        }
        let ids: Vec<String> = undisposed.iter().map(|(id, _)| id.clone()).collect();
        if !self.ledger.spend_retry(key, &ids, TURN_FAILURE_RETRY_CAP) {
            ctx.info(
                "bridge",
                &format!(
                    "turn failure ({why}) for {key} message_id=[{}] — the re-send budget \
                     ({TURN_FAILURE_RETRY_CAP}) is spent; telling the user",
                    ids.join(",")
                ),
            );
            return false;
        }
        // 配達先は window_id を優先(窓名は改名されうる)— Dispatch::Deliver と同じ引き方
        let target = sid
            .as_deref()
            .and_then(|s| self.workers.warm(s))
            .and_then(|h| h.window_id.clone())
            .unwrap_or(window);
        for (id, envelope) in &undisposed {
            if let Err(e) = self.deps.agent.deliver(&Window::of(&target), envelope) {
                ctx.error(
                    "bridge",
                    &format!("turn-failure re-send failed for {id}: {e} — telling the user"),
                );
                return false;
            }
            // 同じ message_id → 台帳は1メッセージ1件なので冪等。received が倒れて受信確認が
            // 張り直る = これは**新しい配達**なので、それが正しい
            self.ledger.track(key, id);
        }
        ctx.info(
            "bridge",
            &format!(
                "turn failure ({why}) for {key} — the worker is READY, so re-sending ids=[{}] \
                 (attempt 1/{TURN_FAILURE_RETRY_CAP}) instead of telling the user the bot gave up",
                ids.join(",")
            ),
        );
        self.touch_thread(key, slack::TYPING_STATUS);
        true
    }

    /// `error` フレームの ⚠️ を1本投げる。perm プロンプトと同じ**消えたスレッド根の門番**を
    /// 通す(消えた根に `thread_ts` 付きで投げると Slack がチャンネル直下に落とす)。
    /// 呼び手は待たない — probe は数秒かかることがあり、main ループを吊ってはならない。
    pub(super) fn post_error_frame(&self, channel: String, thread_ts: String, text: String) {
        let api = self.deps.slack.clone();
        tokio::spawn(async move {
            let key = ThreadKey::new(&channel, &thread_ts);
            let ctx = LogCtx {
                session_id: None,
                thread_key: Some(key.clone()),
            };
            if !channel.starts_with('D') && api.thread_root_gone(&channel, &thread_ts).await {
                ctx.info(
                    "bridge",
                    &format!(
                        "error frame suppressed: thread-root {thread_ts} gone in chan {channel} \
                         (no channel spam)"
                    ),
                );
                return;
            }
            api.post_now(&channel, &thread_ts, format!("⚠️ {text}"), &key)
                .await;
        });
    }

    /// 上限の見張り。**これが無いと上限ゲートは自然に発火しない**
    /// (ターン失敗の文面から気づく道しか無い)。
    ///
    /// 間隔は平常 60分・上限が見込まれるときは 15分。読むのは Home で `/usage` を1回回すだけで、
    /// スレッドもセッションも要らない(`user_usage` と同じ道)。
    ///
    /// やることは2つ:
    /// - **上限に達している窓があればゲートを立てる**。見るのは全部の窓で、解ける時刻は
    ///   いちばん遅いものを採る(週の壁はセッションが低くても効く)
    /// - 80% / 90% を新しく跨いだら、生きているスレッドに1回ずつ警告する
    pub(super) async fn usage_tick(&mut self) {
        let now = self.deps.clock.now_ms();
        let due = self.usage_polled_at_ms
            + if self.usage_at_risk {
                crate::bridge::command::UsageWatch::POLL_AT_RISK_MS
            } else {
                crate::bridge::command::UsageWatch::POLL_MS
            };
        // 起動直後は少し待つ(立ち上がりに probe をぶつけない)
        if self.usage_polled_at_ms == 0 {
            self.usage_polled_at_ms = self.started_at_ms + USAGE_MONITOR_STARTUP_DELAY_MS;
            return;
        }
        if now < due || self.access.owner.is_empty() {
            return;
        }
        self.usage_polled_at_ms = now;
        let ctx = LogCtx::default();
        let raw = match self.deps.agent
            .probe(self.deps.agent.usage_argv(), Host::home())
            .await
        {
            Ok(raw) => raw,
            Err(ProbeErr::Failed(m) | ProbeErr::Errored(m)) => {
                ctx.error(
                    "bridge",
                    &format!("usage-monitor: /usage probe failed: {m}"),
                );
                return;
            }
        };
        let Some(rows) = self.deps.agent.usage_rows(&raw) else {
            ctx.info(
                "bridge",
                "usage-monitor: /usage had no readable row — holding state",
            );
            return;
        };
        let session = rows
            .iter()
            .find(|r| r.label.to_lowercase().contains("current session"));
        let pct = session
            .and_then(|r| r.pct.parse::<f64>().ok())
            .unwrap_or(0.0);
        let reset_text = session.map(|r| r.reset.clone()).unwrap_or_default();

        // 使用率が下がった = 窓が変わった。警告の掛け金を戻す
        if (pct as u32) < self.usage_warned_pct {
            ctx.info(
                "bridge",
                &format!(
                    "usage-monitor: usage dropped {}%→{}% (window reset) — re-arming warnings",
                    self.usage_warned_pct, pct as u32
                ),
            );
            self.usage_warned_pct = 0;
        }
        // 予測(この先どれくらいで上限に当たるか)。間隔もこれで決まる
        let w = crate::bridge::state::WallClock::now();
        let projection = crate::bridge::state::WallClock::parse_reset(&reset_text, &w)
            .map(|reset| crate::bridge::render::UsageProjection::of(pct, &reset, &w, 300));
        self.usage_at_risk = projection
            .as_ref()
            .is_some_and(|p| p.enough_data && p.at_risk);
        ctx.info(
            "bridge",
            &format!(
                "usage-monitor: session {}% used, reset={}, atRisk={}",
                pct as u32,
                if reset_text.is_empty() {
                    "?"
                } else {
                    &reset_text
                },
                self.usage_at_risk
            ),
        );

        // 上限のゲート — 達している窓があれば、いちばん遅いリセットまで塞ぐ
        if let Some(reset) =
            crate::bridge::command::UsageWatch::binding_limit_reset(&rows, now, 100.0)
            && reset != self.limited_until_ms
        {
            ctx.info(
                "bridge",
                &format!(
                    "usage-monitor: hard limit — gating spawn/delivery/pre-warm until epoch={reset}"
                ),
            );
            self.limited_until_ms = reset;
        }

        let crossed =
            crate::bridge::command::UsageWatch::newly_crossed(pct as u32, self.usage_warned_pct);
        let Some(highest) = crossed.iter().max().copied() else {
            return;
        };
        let text = crate::bridge::render::Notice::UsageWarning {
            pct: pct as u32,
            reset: reset_text,
            projected_hit: projection
                .filter(|p| p.enough_data && p.at_risk)
                .and_then(|p| p.projected_hit),
        }
        .render();
        // 生きているワーカーを抱えたスレッドにだけ1回ずつ
        let targets: Vec<ThreadKey> = self.live_thread_keys();
        ctx.info(
            "bridge",
            &format!(
                "usage-monitor: crossed {}% — warning {} live thread(s)",
                crossed
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join("/"),
                targets.len()
            ),
        );
        for key in targets {
            let (channel, thread) = key.split();
            if let Some(t) = thread {
                self.post(&channel, &t, text.clone(), &key);
            }
        }
        self.usage_warned_pct = highest;
    }

    /// 生きたワーカーを抱えているスレッドの鍵。上限警告の宛先(`liveThreads`)。
    fn live_thread_keys(&self) -> Vec<ThreadKey> {
        self.threads
            .entries
            .iter()
            .filter_map(|(root_ts, e)| {
                let sid = e.agent_id.clone()?;
                let channel = e.channel_id.clone()?;
                let name = SessionId::from(sid).window_name();
                self.deps.agent
                    .pid_of(None, &name)
                    .map(|_| ThreadKey::new(&channel, root_ts))
            })
            .collect()
    }

    /// 沈黙見張りの一撃(500ms tick から。現行はスレッドごとの setTimeout)。
    /// 判定は [`bridge::stall_action`] — 立てる / 畳む / 何もしない の3値。
    ///
    /// 畳むのは未応答が空になった鍵。`on_disposition` が即座に落とすのが本筋だが、台帳は
    /// `terminate`(exit / logout / resume)でも空になる — **台帳を空にした全経路**をここ1箇所で
    /// 受けるので、呼び手ごとに後始末を書き足さなくても shimmer が居座らない。
    ///
    /// ponytail: 一度立てたら張り直さない(現行 `showStall` の冪等と同じ)。Slack が失効させれば
    /// shimmer は静かに消えるだけで、消し忘れの居座りより害が小さい。張り直すなら compact と
    /// 同じく tick ごとに `set` を撃つ形にする
    pub(super) fn stall_tick(&mut self) {
        self.save_ledger();
        let now = self.deps.clock.now_ms();
        let acts: Vec<(ThreadKey, bridge::StallAction)> = self
            .stall
            .iter()
            .map(|(key, s)| {
                // 許可待ちは沈黙ではない — 撃たない。ただし決着(Settle)は通す
                let act = bridge::StallAction::of(
                    !self.ledger.pending(key).is_empty(),
                    s.last_activity_ms,
                    now,
                    s.shown || s.awaiting_perm,
                    slack::SILENCE_MS,
                );
                (key.clone(), act)
            })
            .filter(|(_, act)| *act != bridge::StallAction::Nothing)
            .collect();
        for (key, act) in acts {
            match act {
                // 落とすだけでよい — slack::Thinking の Drop がキューの最後尾からクリアを流す
                bridge::StallAction::Settle => drop(self.stall.remove(&key)),
                bridge::StallAction::Fire => {
                    let Some(e) = self.stall.get_mut(&key) else {
                        continue;
                    };
                    e.shown = true;
                    e.thinking.set(slack::THINKING_STATUS);
                    LogCtx {
                        session_id: None,
                        thread_key: Some(key.clone()),
                    }
                    .info(
                        "bridge",
                        "slack-events: stall watchdog fired — setting the thinking status",
                    );
                }
                bridge::StallAction::Nothing => {}
            }
        }
    }

    /// 台帳が変わっていれば pending.json に落とす。tick と、降りる直前から呼ぶ1箇所きり —
    /// 台帳を触る側に save を撒かないので、経路が増えても書き忘れが起きない。
    pub(super) fn save_ledger(&mut self) {
        if let Some(Err(e)) = self.ledger.flush(&mut self.threads) {
            LogCtx::default().error("bridge", &format!("pending.json save failed: {e}"));
        }
    }

    /// ワーカーが入力を取り込んだ瞬間。ここが**受領の証拠** — 配達済みを 👀 から 🤖 に替え、
    /// 前ラウンドの付箋を畳んで新しい付箋を始める。
    fn on_turn_start(&mut self, key: Option<&ThreadKey>, ctx: &LogCtx) {
        let Some(key) = key else { return };
        let ids = self.ledger.mark_received(key);
        self.received(key, ids, ctx);
        self.sticky.on_turn_start(key);
    }

    fn key_of_session(&self, session_id: &str) -> Option<ThreadKey> {
        self.threads
            .find_by_session(session_id)
            .and_then(|(ts, e)| e.channel_id.as_ref().map(|ch| ThreadKey::new(ch, ts)))
    }

    /// 受信確認のバックストップ。**ターン中に押し込んだ分は UserPromptSubmit が発火しない**
    /// (ステアリングとして消費される)ので、ワーカーの transcript に封筒の message_id が
    /// 現れたことを受領の証拠にする。移植元 (transcript-watch)。
    ///
    /// ponytail: 500ms ポーリングの素朴な tail。fs 通知や JSON 行パースが要るなら
    /// Bun connector の transcript-watch(実装)を移植
    pub(super) fn scan_transcripts(&mut self) {
        let tails = self.workers.transcripts();
        for (sid, path, offset) in tails {
            let Some(key) = self.key_of_session(&sid) else {
                continue;
            };
            let unreceived = self.ledger.unreceived(&key);
            if unreceived.is_empty() {
                // 探すものが無い間の出力は読まずに飛ばす(封筒が書かれるのは配達より後 =
                // track より後なので取りこぼさない)。据え置くと最初のターン中配達で
                // ターン1回分を一括同期読みして main ループが止まる
                if let Ok(m) = std::fs::metadata(&path) {
                    self.workers.warm_mut(&sid).transcript_offset = m.len();
                }
                continue;
            }
            let ctx = LogCtx {
                session_id: Some(sid.clone()),
                thread_key: Some(key.clone()),
            };
            let Some((text, next)) = self.deps.agent.new_history_lines(path, offset, &ctx) else {
                continue;
            };
            self.workers.warm_mut(&sid).transcript_offset = next;
            let found = bridge::Ledger::find_received_ids(&text, &unreceived);
            let ids = self.ledger.mark_received_ids(&key, &found);
            let taken_in = !ids.is_empty();
            self.received(&key, ids, &ctx);
            // 受領は新しいラウンドの始まり。UserPromptSubmit が来る道と同じ扱いにする —
            // ここで畳まないと、フォローアップより**上**にある古い付箋に、その後の
            // ナレーションとツール行が足され続ける(見た目には「返事が過去へ遡って伸びる」)
            if taken_in {
                self.sticky.on_turn_start(&key);
            }
        }
    }

    /// PermissionRequest hook の答え。Bridge は**この判断の唯一の権限者**ではなく、
    /// 常設規則で決まるものだけ即答し、決まらないものは辞退して Claude Code 自身の
    /// 許可経路に任せる(`{}` = 口を出さない)。
    ///
    /// **常設規則が最初に効く**のが肝: これが無いとワーカーは自分の返信ツールの許可を
    /// 人に訊きにいき、誰も押さないまま止まる(警句)。
    fn perm_decision(&mut self, ev: &HookEvent, ctx: &LogCtx) -> Option<serde_json::Value> {
        let p = &ev.payload;
        let tool = p["tool_name"].as_str().unwrap_or_default();
        let input = &p["tool_input"];
        Some(
            match crate::bridge::command::ToolPermission::decide(tool, input, self.deps.dir.path()) {
                crate::bridge::command::ToolPermission::Allow(because) => {
                    ctx.debug(
                        "bridge",
                        &format!("perm tool={tool} -> auto-allow ({because})"),
                    );
                    crate::bridge::command::ToolPermission::decision_output(
                        "allow",
                        &format!("Slack bridge ({because})"),
                    )
                }
                crate::bridge::command::ToolPermission::Deny(because) => {
                    ctx.info("bridge", &format!("perm tool={tool} -> DENIED: {because}"));
                    crate::bridge::command::ToolPermission::decision_output("deny", because)
                }
                // 規則では決まらない — 人に訊く。答えは後から来るのでここでは返さない
                crate::bridge::command::ToolPermission::Ask => return None,
            },
        )
    }

    /// 常設規則で決まればその場で答え、決まらなければ Slack に訊きにいって `respond` を預かる。
    /// スレッドが引けない・投稿に失敗した場合は辞退(`{}`)して Claude Code 自身の経路に落とす —
    /// 黙って拒むより手が残る。
    async fn on_perm(&mut self, ev: &mut HookEvent, key: Option<&ThreadKey>, ctx: &LogCtx) {
        let Some(respond) = ev.respond.take() else {
            return; // 答えを待っていない = 何もしない
        };
        if let Some(out) = self.perm_decision(ev, ctx) {
            let _ = respond.send(out);
            return;
        }
        let tool = ev.payload["tool_name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let Some((channel, Some(thread_ts))) = key.map(|k| k.split()) else {
            ctx.info(
                "bridge",
                &format!("perm tool={tool} — no thread for this session; standing aside"),
            );
            let _ = respond.send(serde_json::json!({}));
            return;
        };
        // 人が前に押した範囲なら訊かない。狭い方(スレッド)から見る
        for (granted, scope) in [
            (
                self.threads.thread_tool_allowed(&thread_ts, &tool),
                "thread-grant",
            ),
            (
                self.access.channel_tool_allowed(&channel, &tool),
                "channel-grant",
            ),
        ] {
            if granted {
                ctx.debug("bridge", &format!("perm tool={tool} -> {scope} auto-allow"));
                let _ = respond.send(crate::bridge::command::ToolPermission::decision_output(
                    "allow",
                    &format!("Slack bridge ({scope})"),
                ));
                return;
            }
        }
        // 消えたスレッド根に thread_ts 付きで投げると、Slack はそれを**チャンネル直下の
        // 発言**として落とす = チャンネルが荒れる。投げる前に根の生存を確かめ、消えていれば
        // プロンプトを出さずに deny する(DM は根がそうやって消えないので確認しない)。
        if !channel.starts_with('D') && self.deps.slack.thread_root_gone(&channel, &thread_ts).await {
            ctx.info(
                "bridge",
                &format!(
                    "perm tool={tool} -> thread-root {thread_ts} gone in chan {channel}; \
                     suppressing prompt (deny, no channel spam)"
                ),
            );
            let _ = respond.send(crate::bridge::command::ToolPermission::decision_output(
                "deny",
                "Slack bridge (thread-gone)",
            ));
            return;
        }
        // reqId は Bridge の中だけで意味を持つ札。時刻 + pid + 連番で十分に一意
        let req_id = format!("{:x}-{}", self.deps.clock.now_ms(), self.perm_pending.len());
        let input = ev.payload["tool_input"].clone();
        let prompt_ts = match self
            .deps.slack
            .post_perm_prompt(&channel, &thread_ts, &req_id, &tool, &input)
            .await
        {
            Ok(ts) => ts,
            Err(e) => {
                ctx.error(
                    "bridge",
                    &format!("perm tool={tool}: prompt post failed: {e}"),
                );
                let _ = respond.send(serde_json::json!({}));
                return;
            }
        };
        ctx.info(
            "bridge",
            &format!(
                "perm_request reqId={req_id} tool={tool} chan={channel} -> posted Slack prompt, \
                 awaiting click"
            ),
        );
        // ここから先ワーカーは人待ちで黙る。見張りを止める
        self.suspend_stall_for_perm(&ThreadKey::new(&channel, &thread_ts));
        self.perm_pending.insert(
            req_id,
            PermPending {
                respond,
                channel,
                thread_ts,
                tool_name: tool,
                tool_use_id: ev.payload["tool_use_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                prompt_ts,
                deadline_ms: self.deps.clock.now_ms() + PERM_WAIT_MS,
            },
        );
    }

    /// 許可プロンプトを出した = ワーカーは**意図的に**黙る。見張りを止め、
    /// 既に出ている `is thinking…` も消す(待っていることはプロンプト自身が示している)。
    fn suspend_stall_for_perm(&mut self, key: &ThreadKey) {
        let Some(e) = self.stall.get_mut(key) else {
            return; // このラウンドの見張りがまだ無い(初回ターン)— 止めるものが無い
        };
        e.awaiting_perm = true;
        if std::mem::replace(&mut e.shown, false) {
            e.thinking.set("");
        }
    }

    /// 許可が決着した。`rearm` = 人が押した(沈黙の計測をやり直す)。
    /// 満期のときは **false** — 今まさに「時間切れ」と言ったのに見張りを張り直さない
    /// (本当の活動が来たら touch_thread が張り直す)。
    fn resume_stall_after_perm(&mut self, key: &ThreadKey, rearm: bool) {
        let Some(e) = self.stall.get_mut(key) else {
            return;
        };
        e.awaiting_perm = false;
        if std::mem::replace(&mut e.shown, false) {
            e.thinking.set("");
        }
        if rearm {
            e.last_activity_ms = self.deps.clock.now_ms();
        }
    }

    /// 承認ボタンが押された。**押した瞬間にワーカーへ答える** — その後でプロンプトを
    /// 押された結果に描き替える(投稿の書き替えが遅れてもワーカーは待たない)。
    pub(super) async fn on_perm_click(&mut self, click: slack::PermClick) {
        let Some(p) = self.perm_pending.remove(&click.req_id) else {
            // 既に押された / 満期で畳んだ。二度目のクリックは何もしない
            return;
        };
        let ctx = LogCtx {
            session_id: None,
            thread_key: Some(ThreadKey::new(&p.channel, &p.thread_ts)),
        };
        let allow = click.action != "deny";
        let by = &click.action;
        ctx.info(
            "bridge",
            &format!(
                "perm reqId={} tool={} -> {} (by {} {})",
                click.req_id,
                p.tool_name,
                if allow { "allow" } else { "deny" },
                click.by,
                by
            ),
        );
        let _ = p
            .respond
            .send(crate::bridge::command::ToolPermission::decision_output(
                if allow { "allow" } else { "deny" },
                &format!("Slack bridge ({by})"),
            ));
        let key = ThreadKey::new(&p.channel, &p.thread_ts);
        self.resume_stall_after_perm(&key, true);
        if !allow {
            // 断ったツールの行を 🚫 に。満期の注記行とは**別の道**
            self.sticky.on_perm_denied(&key, &p.tool_use_id);
        }
        // 以後この範囲では訊かない、を覚える。**人が押したときにしか書かれない**
        match click.action.as_str() {
            "allow-thread" => self.grant_tool(&p.thread_ts, &p.tool_name, true, &ctx),
            "allow-channel" => self.grant_tool(&p.channel, &p.tool_name, false, &ctx),
            _ => {}
        }
        let mark = if allow { "✅" } else { "🚫" };
        let done = format!(
            "{mark} `{}` — {} by <@{}>",
            p.tool_name, click.action, click.by
        );
        if let Err(e) = self
            .deps.slack
            .update_message(&p.channel, &p.prompt_ts, &done)
            .await
        {
            ctx.debug("bridge", &format!("perm prompt update failed: {e}"));
        }
    }

    /// 人が押さないまま満期。ワーカーは既に諦めて次へ進んでいるので、**残ったプロンプトを消す** —
    /// 残すと後から押された Allow が「効いたのに何も起きない」になる(#2)。
    pub(super) async fn expire_perm_prompts(&mut self) {
        let now = self.deps.clock.now_ms();
        let expired: Vec<String> = self
            .perm_pending
            .iter()
            .filter(|(_, p)| now >= p.deadline_ms)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            let Some(p) = self.perm_pending.remove(&id) else {
                continue;
            };
            let ctx = LogCtx {
                session_id: None,
                thread_key: Some(ThreadKey::new(&p.channel, &p.thread_ts)),
            };
            ctx.info(
                "bridge",
                &format!(
                    "perm prompt reqId={id} EXPIRED (no click) tool={} — deleting stale prompt",
                    p.tool_name
                ),
            );
            let _ = p.respond.send(serde_json::json!({}));
            let key = ThreadKey::new(&p.channel, &p.thread_ts);
            // 張り直さない — いま「時間切れ」と言ったばかりなので、本当の活動が来るまで黙る
            self.resume_stall_after_perm(&key, false);
            self.sticky.on_perm_timeout(&key);
            if let Err(e) = self.deps.slack.delete_message(&p.channel, &p.prompt_ts).await {
                ctx.debug("bridge", &format!("perm prompt delete failed: {e}"));
            }
        }
    }

    /// 「以後このスレッド/チャンネルでは訊かない」を access.json に書く。
    /// **人がボタンを押したときにしか呼ばれない** — セッションが自分で書く道は無い。
    fn grant_tool(&mut self, scope: &str, tool: &str, thread: bool, ctx: &LogCtx) {
        let (where_, saved) = if thread {
            self.threads.grant_thread_tool(scope, tool);
            ("thread", self.threads.save().map_err(|e| e.to_string()))
        } else {
            self.access.grant_channel_tool(scope, tool);
            (
                "channel",
                self.access.save(&self.deps.dir).map_err(|e| e.to_string()),
            )
        };
        match saved {
            Ok(()) => ctx.info("bridge", &format!("perm: granted {tool} for this {where_}")),
            Err(e) => ctx.error("bridge", &format!("perm: {where_}-grant save failed: {e}")),
        }
    }

    /// PreToolUse / PostToolUse → 付箋のツール行。行に**しない**ツールの判定は board 側の不変条件。
    fn on_progress(&mut self, key: Option<&ThreadKey>, p: &serde_json::Value) {
        let Some(key) = key else { return };
        let name = p["tool_name"].as_str().unwrap_or_default();
        // ツール名の無い progress は「ツールの素性がまだ決まっていない」活動 ping。
        // **行にしない**(見張りの張り直しは呼び手が hook 種別に関わらず済ませている)。
        // 行にすると名前が空の行が生まれ、畳めないので**連続した Read/検索の run を分断する**
        // (位置の門番)。
        if name.is_empty() {
            return;
        }
        let content = &p["tool_response"]["content"];
        let result_text = content
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| serde_json::to_string(content).unwrap_or_default());
        let status = slack::ToolStatus::of(
            p["hook_event_name"].as_str().unwrap_or_default(),
            p["tool_response"]["is_error"].as_bool().unwrap_or(false),
            &result_text,
        );
        let summary = slack::StickyBoard::summarize(name, &p["tool_input"]);
        // id が無いと全ツールが1行を上書きし合う — 名前+引数で代用する
        let id = match p["tool_use_id"].as_str().unwrap_or_default() {
            "" => format!("{name}:{summary}"),
            id => id.to_string(),
        };
        // subagent の素性。top-level の agent_id/agent_type は
        // **subagent の中で走ったツールにだけ**付く(main セッションでは無い)。
        // 起こした側の id は tool_response に載り、foreground は camelCase、
        // background/teammate は snake_case で来る。両方受ける。
        let agent = slack::sticky::AgentRef {
            agent_id: p["agent_id"].as_str().map(str::to_string),
            agent_type: p["agent_type"].as_str().map(str::to_string),
            // 拾うのは **Agent/Task の PostToolUse だけ**。他のツールの結果に同名の
            // フィールドがあっても掴まない。3つの形があり、
            // background/teammate は content が null なので最後の落穂拾いが効かない
            spawned_agent_id: ((name == "Agent" || name == "Task")
                && p["hook_event_name"].as_str() == Some("PostToolUse"))
            .then(|| {
                p["tool_response"]["agentId"]
                    .as_str()
                    .or_else(|| p["tool_response"]["agent_id"].as_str())
                    .map(str::to_string)
                    .or_else(|| Self::agent_id_in_text(&result_text))
            })
            .flatten(),
            // background/teammate を名前で結ぶための起動名(`input.name`)
            launched_name: p["tool_input"]["name"].as_str().map(str::to_string),
        };
        self.sticky
            .upsert_tool(key, &id, name, &summary, status, &agent, &p["tool_input"]);
    }

    /// 結果テキストに素で書かれた `agentId: <id>`(最後の落穂拾い。`/agentId:\s*(\w+)/`)。
    fn agent_id_in_text(result_text: &str) -> Option<String> {
        let rest = result_text.split_once("agentId:")?.1.trim_start();
        let id: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        (!id.is_empty()).then_some(id)
    }

    /// MessageDisplay の delta を message_id ごとに繋ぎ、`final` で1行に確定する。
    fn on_narration(&mut self, key: Option<&ThreadKey>, p: &serde_json::Value, ctx: &LogCtx) {
        let Some(key) = key else { return };
        let msg_id = p["message_id"].as_str().unwrap_or_default();
        // message_id は空で届きうる — スレッドで名前空間を切らないと全スレッドが1本の
        // バッファを共有して混線する(NUL は Slack の id に現れない区切り)
        let slot = format!("{key}\u{0}{msg_id}");
        let buf = self.narration.entry(slot.clone()).or_default();
        // 蓄積は放っておくと無限に伸びる — 1行の予算を超えたらもう足さない
        if buf.len() < NARRATION_CAP {
            buf.push_str(p["delta"].as_str().unwrap_or_default());
        }
        if !p["final"].as_bool().unwrap_or(false) {
            return;
        }
        let Some(text) = self.narration.remove(&slot) else {
            return;
        };
        let text = if text.len() > NARRATION_CAP {
            ctx.debug(
                "bridge",
                &format!("narration clipped msg={msg_id} len={}", text.len()),
            );
            slack::StickyBoard::clip(&text, NARRATION_CAP)
        } else {
            text
        };
        if !text.is_empty() {
            self.sticky.push_narration(key, &text);
        }
    }

    /// ターンを終わらせてよいか。答えないとワーカーが最大5秒吊るので、必ず値を返す。
    fn stop_decision(
        &self,
        key: Option<&ThreadKey>,
        ev: &HookEvent,
        ctx: &LogCtx,
    ) -> serde_json::Value {
        let pending = key.map(|k| self.ledger.pending(k)).unwrap_or_default();
        let mcp_ready = self
            .workers
            .warm(&ev.session_id)
            .is_some_and(|h| h.mcp_ready);
        let block = bridge::Disposition::should_block_stop(
            key.is_some(),
            ev.payload["stop_hook_active"].as_bool().unwrap_or(false),
            mcp_ready,
            pending.len(),
        );
        if let Some(key) = key {
            if block {
                ctx.info(
                    "bridge",
                    &format!(
                        "stop blocked: thread={key} ended with undisposed message(s) [{}] \
                         → re-prompting worker to reply/react/no_reply",
                        pending.join(",")
                    ),
                );
            } else if !mcp_ready && !pending.is_empty() {
                ctx.info(
                    "bridge",
                    &format!(
                        "stop allowed (fail-open): thread={key} has undisposed message(s) [{}] \
                         but MCP not ready (session={}) — leaving it for warm-push/recovery, \
                         not re-prompting",
                        pending.join(","),
                        ev.session_id
                    ),
                );
            }
        }
        if block {
            bridge::Disposition::block_output(&pending)
        } else {
            serde_json::json!({})
        }
    }

    /// ツールが「答えた」— 台帳から落とし、付箋を決着させる。
    pub(super) async fn on_disposition(&mut self, d: Disposition) {
        // reply / no_reply / edit は thread_ts を運ばないことがある — セッションから根を引き戻す
        let root = d.thread_ts.clone().or_else(|| {
            self.threads
                .find_by_session(&d.session_id)
                .map(|(ts, _)| ts.clone())
        });
        let Some(root) = root else {
            LogCtx {
                session_id: Some(d.session_id.clone()),
                thread_key: None,
            }
            .info(
                "bridge",
                &format!(
                    "disposition={} thread unresolved — ledger and sticky left untouched",
                    d.kind
                ),
            );
            return;
        };
        let key = ThreadKey::new(&d.channel_id, &root);
        let ctx = LogCtx {
            session_id: Some(d.session_id.clone()),
            thread_key: Some(key.clone()),
        };
        // 答えが出た = shimmer の役目は終わり(reply / no_reply / edit_message のどれでも)。
        // 落とすだけでよい — slack::Thinking の Drop がキューの最後尾からクリアを流すので、
        // 直前に見張りが投げた `is thinking…` を追い越さずに必ず後から消える
        self.stall.remove(&key);
        let before = self.ledger.pending(&key).len();
        if d.message_ids.is_empty() {
            self.ledger.dispose_all(&key); // ids 無しはスレッド全消化
        } else {
            self.ledger.disposed(&key, &d.message_ids);
        }
        ctx.debug(
            "bridge",
            &format!(
                "ledger: {before} → {} undisposed after {}",
                self.ledger.pending(&key).len(),
                d.kind
            ),
        );
        // 返信・編集は最後の絵を出してから記録として残す。沈黙とリアクションは付箋ごと消す
        if matches!(d.kind, "reply" | "edit")
            && let Some((posted, body)) = self.sticky.take_final(&key)
        {
            self.flush_sticky(&key, posted, body).await;
        }
        if let slack::StickyAction::Delete(ts) = self.sticky.settle(&key, d.kind) {
            let (api, channel) = (self.deps.slack.clone(), d.channel_id.clone());
            tokio::spawn(async move {
                if let Err(e) = api.delete_message(&channel, &ts).await {
                    ctx.debug("bridge", &format!("sticky delete failed: {e}"));
                }
            });
        }
    }

    pub(super) async fn flush_stickies(&mut self) {
        for (key, posted, body) in self.sticky.take_dirty(self.deps.clock.now_ms()) {
            self.flush_sticky(&key, posted, body).await;
        }
    }

    /// 付箋の1枚を Slack に反映する。post だけは ts を覚えるので待つ(update は投げっぱなし)。
    async fn flush_sticky(&mut self, key: &ThreadKey, posted: Option<String>, body: String) {
        if body.is_empty() {
            return; // Slack は空 text の update を弾く
        }
        let (channel, root) = key.split();
        let ctx = LogCtx {
            session_id: None,
            thread_key: Some(key.clone()),
        };
        match posted {
            Some(ts) => {
                let api = self.deps.slack.clone();
                tokio::spawn(async move {
                    if let Err(e) = api.update_message(&channel, &ts, &body).await {
                        ctx.error("bridge", &format!("sticky update failed: {e}"));
                    }
                });
            }
            None => match self
                .deps.slack
                .post_message(&channel, &body, root.as_deref())
                .await
            {
                Ok(ts) => self.sticky.set_posted(key, &ts),
                Err(e) => ctx.error("bridge", &format!("sticky post failed: {e}")),
            },
        }
    }
}
