//! Commands aimed at the thread's agent: stop / exit / resume / context / usage / compact /
//! model / effort / mode, and signing in and out (login / logout).

use super::CmdFx;
use crate::agent::screen::SpawnOutcome;
use crate::agent::tmux::Window;
use crate::agent::{CompactOutcome, CompactProgress, LoginOutcome, ProbeErr, SessionId};
use crate::bridge::inbound::{self, InboundMsg};
use crate::bridge::state::{LogCtx, ThreadKey};
use crate::bridge::{Bridge, Host};
use crate::chat::slack;
use tokio::sync::mpsc;

/// サインインのポーリング(定数どおり)。URL は普通 1〜3 秒で出る。
const LOGIN_POLL: std::time::Duration = std::time::Duration::from_secs(1);

const URL_POLL_MAX: u32 = 20;

const CODE_POLL_MAX: u32 = 30;

/// セッションの無いスレッドに返す1行(全コマンド共通)。
fn no_session() -> String {
    crate::t!(
        "This thread has no session running yet. Send a message to start one first.",
        "このスレッドには、まだ動いているセッションがありません。先にメッセージを送ってセッションを始めてください。"
    )
}

impl Bridge {
    pub(in crate::bridge) fn user_stop(&mut self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let sid = self.threads.get(root_ts).and_then(|e| e.agent_id.clone());
        let pending = self.ledger.pending(key);
        let name = sid
            .as_deref()
            .map(|s| SessionId::from(s).window_name())
            .unwrap_or_default();
        let window_id = sid
            .as_deref()
            .and_then(|s| self.workers.warm(s))
            .and_then(|h| h.window_id.clone());
        // 順に短絡する — 未応答が無い stop で tmux を叩きに行かない
        let running = sid.is_some()
            && !pending.is_empty()
            && self.deps.agent.pid_of(window_id.as_deref(), &name).is_some();
        let Some(sid) = sid.as_deref().filter(|_| running) else {
            ctx.info(
                "bridge",
                &format!(
                    "user stop: key={key} nothing running (session={} pending={}) — no ESC",
                    sid.as_deref().unwrap_or("none"),
                    pending.len()
                ),
            );
            ctx.info(
                "bridge",
                &format!(
                    "slack-events: user stop → nothing running for {}:{root_ts}",
                    msg.channel
                ),
            );
            self.post(
                &msg.channel,
                root_ts,
                crate::t!("Nothing is running right now.", "いま止めるものはありません。"),
                key,
            );
            return;
        };
        // 台帳を**先に**落とす — そうしないと ESC で切られたターンが未応答を抱えたまま終わり、
        // stop hook が「応答待ち」の再プロンプトを撃つ
        self.ledger.disposed(key, &pending);
        let target = Window::of(window_id.as_deref().unwrap_or(&name));
        match self.deps.agent.interrupt(&target) {
            Ok(()) => ctx.info(
                "bridge",
                &format!(
                    "user stop: sent ESC to worker window {target} (session {sid}) key={key}, \
                     disposed pending=[{}]",
                    pending.join(",")
                ),
            ),
            Err(e) => ctx.error("bridge", &format!("user stop: ESC to {target} failed: {e}")),
        }
        self.sticky.on_interrupted(key);
        ctx.info(
            "bridge",
            &format!(
                "slack-events: user stop → interrupted running turn for {}:{root_ts}",
                msg.channel
            ),
        );
    }

    /// `exit` / `bye` / `done`。終わらせるのは
    /// **ワーカーであってスレッドではない** — threads.json の entry は残すので、次のメッセージが
    /// `--resume` で同じセッションを継ぐ。走っているターンは切らずに待つ(stop との違い):
    /// 別れの挨拶がワーカーの最後の返信より**下**に着くように。
    pub(super) async fn user_exit(&mut self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let farewell = Some((msg.channel.clone(), root_ts.to_string()));
        let sid = self.threads.get(root_ts).and_then(|e| e.agent_id.clone());
        let name = sid
            .as_deref()
            .map(|s| SessionId::from(s).window_name())
            .unwrap_or_default();
        let window_id = sid
            .as_deref()
            .and_then(|s| self.workers.warm(s))
            .and_then(|h| h.window_id.clone());
        // 短絡する — セッションの無いスレッドの exit で tmux を叩きに行かない
        let live = sid.is_some() && self.deps.agent.pid_of(window_id.as_deref(), &name).is_some();
        // ワーカーが居ない exit も別れは告げる(現行 performUserExit は session 無しでも
        // farewell まで行く)。待つものが無いので予約せずその場で片付ける
        let Some(sid) = sid.filter(|_| live) else {
            self.terminate(key, None, farewell).await;
            return;
        };
        self.push_drain(key, sid, farewell, None, ctx); // exit は別れの挨拶を出すので shimmer 不要
    }

    /// `resume`。手元の端末で続きを開く1行を
    /// 出し、**線を本当に渡す** — ワーカーが生きていれば exit と同じドレイン後に終わらせる
    /// (別れの挨拶は無し。報告文が終了を告げ終えている)。
    pub(super) fn user_resume(&mut self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let entry = self.threads.get(root_ts).cloned().unwrap_or_default();
        // セッション ID は**発行しない** — 走った覚えの無いスレッドに渡す id は端末で失敗するだけ
        let Some(sid) = entry.agent_id.filter(|s| !s.is_empty()) else {
            ctx.info(
                "bridge",
                &format!("resume: no bound session for thread tts={root_ts} — nothing to resume"),
            );
            let none = crate::bridge::render::ResumeInfo {
                session_id: None,
                cwd: None,
                transcript_missing: false,
                worker_running: false,
            };
            self.post(&msg.channel, root_ts, none.render(), key);
            return;
        };
        // 履歴の在処は hook が運んできた道が第一(worktree に入ったワーカーは transcript ごと
        // 別プロジェクトに移る — 記録した repo_path は当てにならない。)
        let remembered = self
            .workers
            .warm(&sid)
            .and_then(|h| h.transcript_path.clone());
        let history_cwd = self.deps.agent.session_cwd(remembered.as_deref(), &sid);
        let history_exists = self
            .deps.agent
            .session_history_exists(remembered.as_deref(), &sid);
        let name = SessionId::from(sid.clone()).window_name();
        let window_id = self.workers.window_of(&sid);
        let worker_running = self.deps.agent.pid_of(window_id.as_deref(), &name).is_some();
        let info = crate::bridge::render::ResumeInfo {
            // cwd は id と同じくらい大事 — claude は cwd ごとに履歴を仕舞うので、
            // 違う場所で --resume すると見つからない
            cwd: Some(history_cwd.or(entry.repo_path).unwrap_or_else(Host::home)),
            transcript_missing: !history_exists,
            worker_running,
            session_id: Some(sid.clone()),
        };
        // 他の6箇所と同じ文字単位の頭8字(バイト添字は非 ASCII の id で panic の芽 —
        // ここは select ループの中)。transcript は現行と同じく在処ではなく2値
        let short: String = sid.chars().take(8).collect();
        ctx.info(
            "bridge",
            &format!(
                "resume: session={short} cwd={} transcript={} worker={}",
                info.cwd.as_deref().unwrap_or(""),
                if history_exists { "found" } else { "MISSING" },
                if worker_running { "RUNNING" } else { "ABSENT" }
            ),
        );
        self.post(&msg.channel, root_ts, info.render(), key);
        if worker_running {
            ctx.info(
                "bridge",
                &format!(
                    "slack-events: resume command → terminating live worker for {key} \
                     (session handed to the Owner's terminal)"
                ),
            );
            // shimmer は**ドレインに預ける**。resume 本体は tmux を1回覗くだけで終わるので、
            // ここで guard を持っても set と clear が連続して飛ぶだけで一度も描画されない。
            // 実際に待つのは「返事が捌けるか30秒」を待つドレイン側(現行 Bun に原文なし)
            let thinking =
                slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, &slack::Status::Resume.text());
            self.push_drain(key, sid, None, Some(thinking), ctx);
        }
    }

    /// `context` / `ctx`。このスレッド自身の
    /// セッションを `--fork-session` で複製して `/context` を訊く — 走っているワーカーには
    /// 触らないので、ターンの最中でも答えが出る。probe は10〜20秒かかるので投げっぱなし。
    pub(super) fn user_context(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let entry = self.threads.get(root_ts);
        // セッションが無いスレッドには訊く先が無い
        let Some(sid) = entry.and_then(|e| e.agent_id.clone()) else {
            ctx.info(
                "bridge",
                &format!("context: no bound session for thread tts={root_ts} — nothing to probe"),
            );
            self.post(&msg.channel, root_ts, no_session(), key);
            return;
        };
        let cwd = entry
            .and_then(|e| e.repo_path.clone())
            .unwrap_or_else(Host::home);
        let short: String = sid.chars().take(8).collect();
        ctx.info(
            "bridge",
            &format!("context: probing /context tts={root_ts} session={short} cwd={cwd}"),
        );
        let argv = self.deps.agent.context_argv(&sid);
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        let thinking =
            slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, &slack::Status::Context.text());
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア(probe が失敗しても消える)
            let ctx = LogCtx {
                session_id: Some(sid),
                thread_key: Some(key.clone()),
            };
            let out = match agent.probe(argv, cwd).await {
                Ok(raw) => {
                    ctx.info(
                        "bridge",
                        &format!(
                            "context: probe ok tts={root} session={short} bytes={}",
                            raw.len()
                        ),
                    );
                    match agent.context_report(&raw) {
                        Some(r) => r.render(),
                        None => {
                            ctx.error(
                                "bridge",
                                &format!(
                                    "slack-events: context command — could not parse /context \
                                     output for {channel}:{root}"
                                ),
                            );
                            crate::t!("Couldn't read the context usage.", "コンテキストの使用量を読み取れませんでした。")
                        }
                    }
                }
                // 失敗の理由は英語の内部文字列 — ログに置き、スレッドには流さない
                Err(ProbeErr::Failed(e)) => {
                    ctx.error(
                        "bridge",
                        &format!("context: probe failed tts={root} session={short}: {e}"),
                    );
                    ctx.error(
                        "bridge",
                        &format!(
                            "slack-events: context command probe failed for {channel}:{root}: {e}"
                        ),
                    );
                    crate::t!("Couldn't get the context usage. Try again.", "コンテキストの使用量を取得できませんでした。もう一度試してください。")
                }
                Err(ProbeErr::Errored(e)) => {
                    ctx.error(
                        "bridge",
                        &format!("slack-events: context command errored for {channel}:{root}: {e}"),
                    );
                    crate::t!("Something went wrong while getting the context usage.", "コンテキストの使用量を取得する途中でエラーが起きました。")
                }
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// `usage` / `usg`。アカウント全体の話
    /// なのでスレッドもセッションも要らない — Home で `claude -p /usage` を回すだけ。
    pub(super) fn user_usage(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let cwd = Host::home();
        ctx.info("bridge", &format!("usage: probing /usage cwd={cwd}"));
        let argv = self.deps.agent.usage_argv();
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, &slack::Status::Usage.text());
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア
            let ctx = LogCtx {
                session_id: None,
                thread_key: Some(key.clone()),
            };
            let out = match agent.probe(argv, cwd).await {
                Ok(raw) => {
                    ctx.info("bridge", &format!("usage: probe ok bytes={}", raw.len()));
                    match agent.usage_rows(&raw) {
                        // 300 = "Current session" の窓(5h)。"Current week" 行の窓は
                        // format_usage_report_with_projection が内側で差し替える
                        Some(rows) => crate::bridge::render::UsageReport {
                            rows: &rows,
                            projection: Some((crate::bridge::state::WallClock::now(), 300)),
                        }
                        .render(),
                        None => {
                            ctx.error(
                                "bridge",
                                &format!(
                                    "slack-events: usage command — could not parse /usage \
                                     output for {channel}:{root}"
                                ),
                            );
                            crate::t!("Couldn't read your usage.", "使用状況を読み取れませんでした。")
                        }
                    }
                }
                Err(ProbeErr::Failed(e)) => {
                    ctx.error("bridge", &format!("usage: probe failed: {e}"));
                    ctx.error(
                        "bridge",
                        &format!(
                            "slack-events: usage command probe failed for {channel}:{root}: {e}"
                        ),
                    );
                    crate::t!("Couldn't get your usage. Try again.", "使用状況を取得できませんでした。もう一度試してください。")
                }
                Err(ProbeErr::Errored(e)) => {
                    ctx.error(
                        "bridge",
                        &format!("slack-events: usage command errored for {channel}:{root}: {e}"),
                    );
                    crate::t!("Something went wrong while getting your usage.", "使用状況を取得する途中でエラーが起きました。")
                }
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// TUI を叩くコマンド(compact / model / effort)の共通の入口。
    /// セッションと窓を解決し、ターンが走っている間は断る — 走行中の TUI には打ち込めない。
    /// `Ok((target, session_id))` の時だけ tmux を叩いてよい。`Err` はそのまま返す1行。
    pub(super) fn tui_guard(
        &self,
        label: &str,
        no_session_tail: &str,
        busy_tail: &str,
        key: &ThreadKey,
        root_ts: &str,
        ctx: &LogCtx,
    ) -> Result<(Window, String), String> {
        let Some(sid) = self.threads.get(root_ts).and_then(|e| e.agent_id.clone()) else {
            ctx.info(
                "bridge",
                &format!("{label}: no bound session for thread tts={root_ts} — {no_session_tail}"),
            );
            return Err(no_session());
        };
        let name = SessionId::from(sid.clone()).window_name();
        let window_id = self.workers.window_of(&sid);
        // 未応答を先に見る — 空なら tmux を叩きに行かない(user_stop と同じ短絡)
        let pending = self.ledger.pending(key);
        if !pending.is_empty() && self.deps.agent.pid_of(window_id.as_deref(), &name).is_some() {
            ctx.info(
                "bridge",
                &format!(
                    "{label}: turn in-flight for key={key} (pending={}) — {busy_tail}",
                    pending.len()
                ),
            );
            return Err(crate::t!(
                "The agent is busy. Send `stop` first, then `{label}`.",
                "エージェントが作業中です。`stop` で止めてから `{label}` を送ってください。"
            ));
        }
        Ok((Window::of(window_id.as_deref().unwrap_or(&name)), sid))
    }

    /// `compact`。**生きているセッション**の
    /// TUI に `/compact` を打ち込み、進捗スピナーを Slack の1本の付箋に流し込む
    /// (context/usage と違って使い捨ての probe ではない — 圧縮するのは走っている会話そのもの)。
    /// 最長6分かかるので select ループの外(spawn)で回す。
    pub(super) fn user_compact(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let (target, sid) = match self.tui_guard(
            "compact",
            "nothing to compact",
            "refusing (busy)",
            key,
            root_ts,
            ctx,
        ) {
            Ok(v) => v,
            Err(text) => {
                self.post(&msg.channel, root_ts, text, key);
                return;
            }
        };
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        // compact に専用の thinking status は**付けない**(Bun からの逸脱 — ユーザー判断)。
        // 以前の実装はここで「考え中」を張り、tick ごとに秒数付きへ張り直していた。
        // こちらは進捗チェックリスト(run_compact が投稿して編集し続ける sticky)が同じことを
        // 見せているので、shimmer と二重で冗長という判断(**移植漏れではない**)。
        tokio::spawn(Self::run_compact(api, self.deps.agent.clone(), channel, root, key, target, sid));
    }

    /// `model`。名前付きは TUI に
    /// `/model <名前>` を打ち込む。素の `model` は transcript を読むだけ — ワーカーに触らない。
    pub(super) fn user_model(
        &self,
        msg: &InboundMsg,
        name: Option<String>,
        key: &ThreadKey,
        root_ts: &str,
        ctx: &LogCtx,
    ) {
        let Some(name) = name else {
            self.model_show(msg, key, root_ts, ctx);
            return;
        };
        let (target, sid) = match self.tui_guard(
            "model",
            "nothing to switch",
            "refusing (busy)",
            key,
            root_ts,
            ctx,
        ) {
            Ok(v) => v,
            Err(text) => {
                self.post(&msg.channel, root_ts, text, key);
                return;
            }
        };
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, &slack::Status::Model.text());
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア(TUI が確定しなくても消える)
            let ctx = LogCtx {
                session_id: Some(sid),
                thread_key: Some(key.clone()),
            };
            // None = このエージェントが model 切替に非対応。実体が1つの今は起きない
            let done = agent.set_model(&target, &name, &key, &ctx).await == Some(true);
            let out = if done {
                crate::t!("✅ Switched this thread's model to *{name}*.", "✅ このスレッドのモデルを *{name}* に切り替えました。")
            } else {
                crate::t!("Couldn't switch the model. Try again.", "モデルを切り替えられませんでした。もう一度試してください。")
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// 素の `model`。今のモデルを名乗るのは transcript の**最後の**
    /// assistant レコード。TUI も probe も要らないので select ループの中で答える
    /// (読むのは末尾 256KB だけ — 長いセッションの .jsonl は数十 MB ある)。
    pub(super) fn model_show(&self, msg: &InboundMsg, key: &ThreadKey, root_ts: &str, ctx: &LogCtx) {
        let Some(sid) = self.threads.get(root_ts).and_then(|e| e.agent_id.clone()) else {
            ctx.info(
                "bridge",
                &format!("model: no bound session for thread tts={root_ts} — no current model"),
            );
            self.post(&msg.channel, root_ts, no_session(), key);
            return;
        };
        let short: String = sid.chars().take(8).collect();
        // 履歴の在処は hook が運んできた道が第一(worktree に入ったワーカーは transcript ごと
        // 移る)。忘れていれば総当たりで探す — resume と同じ解決順
        let remembered = self
            .workers
            .warm(&sid)
            .and_then(|h| h.transcript_path.clone());
        let found = self.deps.agent.current_model(remembered.as_deref(), &sid);
        let failed = || crate::t!("Couldn't read the current model.", "今のモデルを読み取れませんでした。");
        let out = match found {
            None => {
                ctx.info(
                    "bridge",
                    &format!(
                        "model: no transcript for session={short} tts={root_ts} — \
                         cannot read current model"
                    ),
                );
                failed()
            }
            Some(read) => match read {
                Err(e) => {
                    ctx.error(
                        "bridge",
                        &format!("model: transcript read failed for session={short}: {e}"),
                    );
                    failed()
                }
                Ok(model) => match model {
                    None => {
                        ctx.info(
                            "bridge",
                            &format!("model: no model id in transcript tail of session={short}"),
                        );
                        failed()
                    }
                    Some(model) => {
                        ctx.info(
                            "bridge",
                            &format!("model: current={model} session={short} key={key}"),
                        );
                        match self.deps.agent.model_alias(&model) {
                            Some(alias) => crate::t!("Model: *{alias}* (`{model}`)", "モデル: *{alias}*(`{model}`)"),
                            None => crate::t!("Model: `{model}`", "モデル: `{model}`"),
                        }
                    }
                },
            },
        };
        self.post(&msg.channel, root_ts, out, key);
    }

    /// `effort`。level 付きは model と同じ
    /// TUI 駆動。素の `effort` は現在値を**どこにも記録が無い**ので TUI に訊く —
    /// スライダを開いて Escape で閉じ、TUI が出す状態行を読む。
    ///
    /// **現行 TUI との差分**(2026-07-29 実機・実弾で3経路とも確認): 履歴なしは即
    /// `Set effort level to …`、履歴ありは確認ダイアログを挟んで**状態行しか出さず**、
    /// 同じ level の選び直しは `Kept effort level as …`。移植元の Bun が知るのは1つ目だけ。
    pub(super) fn user_effort(
        &self,
        msg: &InboundMsg,
        level: Option<String>,
        key: &ThreadKey,
        root_ts: &str,
        ctx: &LogCtx,
    ) {
        let (tail, busy) = match &level {
            Some(_) => ("nothing to set", "refusing (busy)"),
            None => ("no current level", "refusing read (busy)"),
        };
        let (target, sid) = match self.tui_guard("effort", tail, busy, key, root_ts, ctx) {
            Ok(v) => v,
            Err(text) => {
                self.post(&msg.channel, root_ts, text, key);
                return;
            }
        };
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        // 素の `effort` は現在値を読むだけ — 現行も status を出さない(出典は set 側の 3182)
        let Some(level) = level else {
            tokio::spawn(Self::run_effort_show(
                api,
                self.deps.agent.clone(),
                channel,
                root,
                key,
                target,
                sid,
            ));
            return;
        };
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, &slack::Status::Effort.text());
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア
            let ctx = LogCtx {
                session_id: Some(sid),
                thread_key: Some(key.clone()),
            };
            // None = このエージェントが effort に非対応。実体が1つの今は起きない
            let done = agent.set_effort(&target, &level, &key, &ctx).await == Some(true);
            let out = if done {
                crate::t!("✅ Set this thread's effort level to *{level}*.", "✅ このスレッドの effort を *{level}* にしました。")
            } else {
                crate::t!("Couldn't set the effort level. Try again.", "effort を設定できませんでした。もう一度試してください。")
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// `mode`(移植元に無い — Rust 版の新機能)。Claude Code の権限モードは TUI の
    /// shift+tab でしか変えられないので、`/effort` のようなスラッシュコマンドは使わず
    /// **キーを押して**目当てのモードに着くまで回す。素の `mode` はフッタを1回読むだけ。
    pub(super) fn user_mode(
        &self,
        msg: &InboundMsg,
        name: Option<String>,
        key: &ThreadKey,
        root_ts: &str,
        ctx: &LogCtx,
    ) {
        let (tail, busy) = match &name {
            Some(_) => ("nothing to switch", "refusing (busy)"),
            None => ("no current mode", "refusing read (busy)"),
        };
        let (target, sid) = match self.tui_guard("mode", tail, busy, key, root_ts, ctx) {
            Ok(v) => v,
            Err(text) => {
                self.post(&msg.channel, root_ts, text, key);
                return;
            }
        };
        let ctx = LogCtx {
            session_id: Some(sid),
            thread_key: Some(key.clone()),
        };
        // 読むだけなら tmux を1回叩くだけ — spawn も shimmer も要らない
        let Some(name) = name else {
            let out = match self.deps.agent.mode(&target, &ctx) {
                Some(m) => crate::t!("Permission mode: *{m}*", "権限モード: *{m}*"),
                None => crate::t!("Couldn't read the permission mode.", "権限モードを読み取れませんでした。"),
            };
            self.post(&msg.channel, root_ts, out, key);
            return;
        };
        let (api, channel, root, key) = (
            self.deps.slack.clone(),
            msg.channel.clone(),
            root_ts.to_string(),
            key.clone(),
        );
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &msg.channel, root_ts, &slack::Status::Mode.text());
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア
            let done = agent.set_mode(&target, &name, &key, &ctx).await == Some(true);
            let out = if done {
                crate::t!("✅ Switched this thread's permission mode to *{name}*.", "✅ このスレッドの権限モードを *{name}* にしました。")
            } else {
                crate::t!("Couldn't switch the permission mode. Try again.", "権限モードを切り替えられませんでした。もう一度試してください。")
            };
            api.post_now(&channel, &root, out, &key).await;
        });
    }

    /// `/compact` を打ち込み、pane のスピナーを Slack の付箋に流す。付箋は**最初の進捗が出てから**作る — busy / no-session の
    /// 断りが1本で済むのはそのため。最後の1行は付箋があれば書き換え、無ければ新規投稿。
    ///
    /// TUI を回すのはエージェント側(`Agent::compact`)。ここは**進捗を Slack に描く側**だけ —
    /// 最長6分かかるので select ループの外(spawn)で回す。
    pub(super) async fn run_compact(
        api: crate::chat::ChatRef,
        agent: crate::agent::AgentRef,
        channel: String,
        root: String,
        key: ThreadKey,
        target: Window,
        sid: String,
    ) {

        let ctx = LogCtx {
            session_id: Some(sid.clone()),
            thread_key: Some(key.clone()),
        };
        // The agent sends each reading; this task draws them one at a time, in order.
        // Capacity 1 keeps the agent at most one reading ahead of the drawing.
        let (tx, mut rx) = mpsc::channel::<CompactProgress>(1);
        let draw = async {
            let mut progress_ts: Option<String> = None;
            let mut last_rendered = String::new();
            while let Some(st) = rx.recv().await {
                let rendered = st.render();
                // 同じ絵を描き直さない(Slack の編集回数はタダではない)
                if rendered == last_rendered {
                    continue;
                }
                last_rendered.clone_from(&rendered);
                let posted = match &progress_ts {
                    Some(ts) => api
                        .update_message(&channel, ts, &rendered)
                        .await
                        .map(|()| None),
                    None => api
                        .post_message_no_unfurl(&channel, &rendered, Some(&root))
                        .await
                        .map(Some),
                };
                match posted {
                    Ok(Some(ts)) => progress_ts = Some(ts),
                    Ok(None) => {}
                    // 描き損ねても圧縮は続く
                    Err(e) => ctx.error("bridge", &format!(
                        "slack-events: compact progress render failed for {channel}:{root}: {e}"
                    )),
                }
            }
            progress_ts
        };
        let (outcome, progress_ts) = tokio::join!(agent.compact(&target, &key, &sid, tx), draw);
        // None = このエージェントが compact に非対応。実体が1つの今は起きない
        let final_text = match outcome {
            Some(CompactOutcome::Done) => crate::t!("✅ Compacted the context.", "✅ コンテキストを圧縮しました。"),
            Some(CompactOutcome::Nothing) => crate::t!(
                "There isn't enough history to compact yet.",
                "圧縮するほどの履歴がまだありません。"
            ),
            Some(CompactOutcome::Failed) | None => crate::t!(
                "Couldn't compact the context. Try again.",
                "コンテキストを圧縮できませんでした。もう一度試してください。"
            ),
        };
        let posted = match &progress_ts {
            Some(ts) => api.update_message(&channel, ts, &final_text).await,
            None => api
                .post_message_no_unfurl(&channel, &final_text, Some(&root))
                .await
                .map(|_| ()),
        };
        if let Err(e) = posted {
            ctx.error(
                "bridge",
                &format!("slack-events: compact final post failed for {channel}:{root}: {e}"),
            );
        }
    }

    /// 素の `effort`。今の level を TUI に訊くのはエージェント側
    /// (`Agent::effort`)。ここは答えを1行にして投げるだけ。
    pub(super) async fn run_effort_show(
        api: crate::chat::ChatRef,
        agent: crate::agent::AgentRef,
        channel: String,
        root: String,
        key: ThreadKey,
        target: Window,
        sid: String,
    ) {
        let failed = || crate::t!("Couldn't read the current effort level.", "今の effort を読み取れませんでした。");
        // None = 非対応か、状態行が読めなかったか — どちらも同じ断りを返す
        let out = match agent.effort(&target, &key, &sid).await {
            Some(level) => crate::t!("Effort level: *{level}*", "effort: *{level}*"),
            None => failed(),
        };
        api.post_now(&channel, &root, out, &key).await;
    }

    /// Owner がまだ居ないときだけ通る、サインインの抜け道。
    /// **true = 消費した** — 呼び手はそこで打ち切る。届く場所は2つ:
    ///   • 人間の DM — 進行中のサインインがあれば次の1通を貼り付けコードと読む。素の `login` は
    ///     新しいサインイン。それ以外には短い案内を返す
    ///   • Owner が route したチャンネル — 受けるのは**貼り付けコードだけ**、しかも
    ///     そのサインインを始めた本人からのものだけ(相席の第三者にコードプロンプトを触らせない)
    pub(in crate::bridge) fn login_carve_out(&mut self, msg: &InboundMsg) -> bool {
        let dm = msg.channel_kind == inbound::ChannelKind::Dm;
        let sender = msg.user.as_deref().unwrap_or("");
        let pending = self.sign_in.pending.get(&msg.channel).cloned();
        if !dm && pending.as_deref() != Some(sender) {
            return false; // 通りすがりはこの枝の外 — 普通に gate へ落とす
        }
        // 返信は「その人が喋ったメッセージ」の下に吊る。DM/チャンネルの根に出すと、
        // 本人が見ているスレッドと違う場所に着く
        let reply_ts = msg.thread_ts.clone().unwrap_or_else(|| msg.ts.clone());
        let key = ThreadKey::new(&msg.channel, &reply_ts);
        let venue = if dm { "dm" } else { "channel" };
        let ch = &msg.channel;
        let ctx = LogCtx::default();
        match pending {
            Some(user) => {
                ctx.info(
                    "bridge",
                    &format!("slack-events: login code from {sender} ({venue} {ch})"),
                );
                self.submit_code(ch.clone(), msg.text.trim().to_string(), reply_ts, user);
            }
            None if crate::bridge::command::Message::new(
                &msg.text,
                self.bot_user_id.as_deref(),
            )
            .is("login") =>
            {
                ctx.info(
                    "bridge",
                    &format!("slack-events: login command from {sender} ({venue} {ch})"),
                );
                self.start_login(ch.clone(), sender.to_string(), reply_ts);
            }
            None => {
                ctx.info(
                    "bridge",
                    &format!(
                        "slack-events: owner-less {venue} from {sender} — login guidance \
                         ({venue} {ch})"
                    ),
                );
                self.post(
                    ch,
                    &reply_ts,
                    crate::t!(
                        "Claude Code isn't signed in yet. Send `login` to sign in.",
                        "Claude Code はまだサインインしていません。`login` と送るとサインインを始めます。"
                    ),
                    &key,
                );
            }
        }
        true
    }

    /// サインインの開始。専用の tmux セッションで `claude auth login` を
    /// 回し、印字される認証 URL を掬って返す。ここから先はコード待ち。
    pub(super) fn start_login(&mut self, channel: String, user: String, reply_ts: String) {
        let key = ThreadKey::new(&channel, &reply_ts);
        let ctx = LogCtx::default();
        // SECURITY: サインインは**全員で1つの** tmux セッションを使う。
        // 別チャンネルの2本目にセッションを作り直させると、1人目のポーリングが2人目の pane を
        // 読み、2人目の成功で**1人目**が Owner になる(他人の認証への相乗り)。同じチャンネルの
        // 撃ち直しは自分の流れをやり直すだけなので通す
        if let Some(other) = self.sign_in.pending.keys().find(|k| **k != channel) {
            ctx.info(
                "bridge",
                &format!(
                    "login: refusing concurrent sign-in for {user} (channel {channel}) — \
                     a sign-in is already in progress (dm {other})"
                ),
            );
            self.post(
                &channel,
                &reply_ts,
                crate::t!(
                    "Another sign-in is in progress. Wait a moment, then send `login` again.",
                    "別のサインインが進行中です。少し待ってから、もう一度 `login` と送ってください。"
                ),
                &key,
            );
            return;
        }
        ctx.info(
            "bridge",
            &format!("login: starting sign-in for {user} (channel {channel})"),
        );
        // 席は判定の**直後**に取る。取るのを spawn の中(URL が出た後)にすると、URL を待つ
        // 最大20秒の間に別チャンネルの2本目が同じ判定をすり抜け、セッションを作り直して
        // 1本目の pane を奪う。席は成否どちらでも LoginFinished が外す。
        // 代償: URL が届くまでの間にこのチャンネルへ来た1通はコード扱いになり失敗の返事になる
        // (Owner は `login` を撃ち直せばよい)
        self.sign_in.pending.insert(channel.clone(), user.clone());
        let (api, cmd_tx, home) = (self.deps.slack.clone(), self.cmd_tx.clone(), Host::home());
        // サインインは URL を出してからコードを待つ数十秒 — その間ずっと shimmer を出す
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &channel, &reply_ts, &slack::Status::Login.text());
        // `thinking` は本文で触るので async move が丸ごと持っていく(どの経路で抜けても Drop = クリア)
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let ctx = LogCtx::default();
            let started = agent.login_begin(&home);
            let mut url = None;
            if let Err(e) = started {
                ctx.error(
                    "bridge",
                    &format!(
                        "login: could not start the sign-in session for {user} \
                         (channel {channel}): {e}"
                    ),
                );
            } else {
                // 待つのは URL **だけ**。ここで広いエラー判定を回すと、URL より前の飾りに
                // 紛れた "error/failed" で誤って諦める。本物の早期失敗は
                // 「URL が出ないまま時間切れ」という形で下に現れる
                for _ in 0..URL_POLL_MAX {
                    tokio::time::sleep(LOGIN_POLL).await;
                    // URL が出るまで最大 URL_POLL_MAX 秒 — Slack はそれより早く status を
                    // 失効させるので tick ごとに張り直す(compact と同じ理由)。
                    // 張り直さないと途中で shimmer が消えて「止まった」に見える
                    thinking.set(&slack::Status::Login.text());
                    url = agent.login_url();
                    if url.is_some() {
                        break;
                    }
                }
            }
            let Some(url) = url else {
                ctx.error(
                    "bridge",
                    &format!(
                        "login: no sign-in URL captured within {URL_POLL_MAX}s for {user} \
                         (channel {channel}) — aborting"
                    ),
                );
                // 席を空けるのは main(セッションの kill も向こうがやる)。ここで返さないと
                // 二度と `login` を受け付けなくなる
                let _ = cmd_tx
                    .send(CmdFx::LoginFinished {
                        channel: channel.clone(),
                        bound: None,
                    })
                    .await;
                api.post_now(&channel, &reply_ts, crate::t!(
                        "Couldn't start the sign-in. Wait a moment, then send `login` again.",
                        "サインインを始められませんでした。少し待ってから、もう一度 `login` と送ってください。"
                    ), &key)
                .await;
                return;
            };
            api.post_now(&channel, &reply_ts, crate::t!(
                    "🔐 Open this link in your browser and sign in to Claude, then paste the code it shows as your *next message* in this thread:\n{url}",
                    "🔐 このリンクをブラウザで開いて Claude にサインインし、表示されたコードを、このスレッドの *次のメッセージ* として貼ってください:\n{url}"
                ), &key)
            .await;
            ctx.info(
                "bridge",
                &format!(
                    "login: sign-in URL sent to {user} (channel {channel}) — awaiting pasted code"
                ),
            );
        });
    }

    /// 貼られたコードを待っている CLI に流し込み、画面の判定を待つ。
    /// Owner が縛られるのは**明示の成功マーカーを見たときだけ** — 沈黙も中断も成功ではない。
    ///
    /// ponytail: CLI がシェルに戻ったことの検知(現行の paneCommand)は持たない — 30 秒の
    /// 時間切れで代替する(最悪、失敗の返事が最大 30 秒遅れるだけ)
    pub(super) fn submit_code(&self, channel: String, code: String, reply_ts: String, user: String) {
        let key = ThreadKey::new(&channel, &reply_ts);
        LogCtx::default().info(
            "bridge",
            &format!("login: submitting pasted code for {user} (channel {channel})"),
        );
        let (api, cmd_tx) = (self.deps.slack.clone(), self.cmd_tx.clone());
        // サインインの後半(貼られたコードの判定、最大 CODE_POLL_MAX 秒)も待ち時間 —
        // login_start の guard は URL を出した時点で落ちているので、ここで張り直す
        let thinking = slack::Thinking::new(self.deps.slack.clone(), &channel, &reply_ts, &slack::Status::Login.text());
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let ctx = LogCtx::default();
            let mut outcome = "timeout";
            if let Err(e) = agent.login_submit_code(&code) {
                ctx.error(
                    "bridge",
                    &format!(
                        "login: could not submit the pasted code for {user} \
                         (channel {channel}): {e}"
                    ),
                );
                outcome = "error";
            } else {
                for _ in 0..CODE_POLL_MAX {
                    tokio::time::sleep(LOGIN_POLL).await;
                    thinking.set(&slack::Status::Login.text()); // 失効させない(URL 待ちと同じ)
                    // scrollback ごと読む: "Login successful." を出した直後に CLI はシェルへ
                    // 戻り、次のポーリングまでに印が画面外へ流れる
                    match agent.login_outcome() {
                        LoginOutcome::Success => {
                            outcome = "success";
                            break;
                        }
                        LoginOutcome::Error => {
                            outcome = "error";
                            break;
                        }
                        LoginOutcome::Pending => {}
                    }
                }
            }
            // セッションの片付けと pending の削除、Owner の書き込みは main の仕事
            let (text, bound) = if outcome == "success" {
                (
                    crate::t!(
                        "Signed in ✅ — you're now the owner of this bot.",
                        "サインインしました ✅ — あなたがこのボットの Owner になりました。"
                    ),
                    Some(user),
                )
            } else {
                ctx.error(
                    "bridge",
                    &format!(
                        "login: sign-in {outcome} for {user} (channel {channel}) — no Owner bound"
                    ),
                );
                (
                    crate::t!(
                        "Sign-in failed — the code may be wrong or expired. Send `login` to try again.",
                        "サインインできませんでした。コードが違うか、期限が切れた可能性があります。`login` と送ってやり直してください。"
                    ),
                    None,
                )
            };
            let _ = cmd_tx
                .send(CmdFx::LoginFinished {
                    channel: channel.clone(),
                    bound,
                })
                .await;
            api.post_now(&channel, &reply_ts, text.to_string(), &key)
                .await;
        });
    }

    /// `logout`。CLI のサインアウトだけを
    /// spawn で回し、ワーカーの後片付けと Owner の解除は main に戻してやる。
    pub(super) fn user_logout(&mut self, channel: &str, root_ts: &str) {
        // restart の札と違ってこれは**本当に効く** — user_logout は spawn を撒いてすぐ返るので、
        // `claude auth logout` が走っている数秒の間に2通目の logout が届きうる
        if self.sign_in.signing_out {
            LogCtx {
                session_id: None,
                thread_key: Some(ThreadKey::new(channel, root_ts)),
            }
            .info("bridge", "logout ignored — already signing out");
            return;
        }
        self.sign_in.signing_out = true;
        // shimmer は `claude auth logout` が返るまで。この後のワーカー畳みは main 側
        // (CmdFx::LogoutFinished)なので、ここで持たせておけば **必ず** 消える
        let thinking = slack::Thinking::new(self.deps.slack.clone(), channel, root_ts, &slack::Status::Logout.text());
        let (cmd_tx, channel, thread_ts) = (
            self.cmd_tx.clone(),
            channel.to_string(),
            root_ts.to_string(),
        );
        let agent = self.deps.agent.clone();
        tokio::spawn(async move {
            let _thinking = thinking; // Drop = クリア
            let ctx = LogCtx::default();
            ctx.info("bridge", "logout: signing out (claude auth logout)");
            let ok = match agent.logout().await {
                Ok(()) => {
                    ctx.info("bridge", "logout: claude auth logout OK");
                    true
                }
                Err(ProbeErr::Failed(status)) => {
                    ctx.error(
                        "bridge",
                        &format!("logout: 'claude auth logout' exited {status}"),
                    );
                    false
                }
                Err(ProbeErr::Errored(e)) => {
                    ctx.error(
                        "bridge",
                        &format!("logout: 'claude auth logout' threw: {e}"),
                    );
                    false
                }
            };
            // サインアウトの成否に関わらず片付けは走らせる — 半端にワーカーだけ生き残る方が悪い
            let _ = cmd_tx
                .send(CmdFx::LogoutFinished {
                    ok,
                    channel,
                    thread_ts,
                })
                .await;
        });
    }

    /// spawn したサインイン・サインアウトが戻してきた状態変更を、main の側で1つずつ適用する。
    pub(in crate::bridge) async fn on_cmd_fx(&mut self, fx: CmdFx) {
        let ctx = LogCtx::default();
        match fx {
            CmdFx::LoginFinished { channel, bound } => {
                self.deps.agent.login_kill();
                self.sign_in.pending.remove(&channel);
                let Some(user) = bound else { return };
                let mut access = self.access.clone();
                access.owner.clone_from(&user);
                self.adopt_access(access, &ctx);
                ctx.info(
                    "bridge",
                    &format!("login: SUCCESS — {user} bound as Owner (channel {channel})"),
                );
                // サインインは仕切り直し — 前の認証状態で諦めた枠をもう一度試す
                // (これが無いと一時的な失敗で枠が Bridge の寿命いっぱい空く)
                if !self.workers.gave_up_count() == 0 {
                    ctx.info(
                        "bridge",
                        &format!(
                            "login: retrying {} pool(s) given up on earlier",
                            self.workers.gave_up_count()
                        ),
                    );
                }
                self.workers.clear_gave_up();
                // Owner が決まって初めて pool_targets が実体を持つ — ここが初回の在庫作り
                self.start_missing_pool_workers(&ctx);
            }
            CmdFx::LogoutFinished {
                ok,
                channel,
                thread_ts,
            } => {
                self.teardown_all_workers(
                    "logout",
                    &format!(" (claude auth logout ok={ok})"),
                    &ctx,
                )
                .await;
                let mut access = self.access.clone();
                access.owner.clear();
                self.adopt_access(access, &ctx);
                self.sign_in.pending.clear();
                self.sign_in.signing_out = false;
                ctx.info(
                    "bridge",
                    "logout: Owner cleared — bot is now Owner-less (login required)",
                );
                let key = ThreadKey::new(&channel, &thread_ts);
                self.post(
                    &channel,
                    &thread_ts,
                    crate::t!(
                        "Signed out. Send `login` to sign in again.",
                        "サインアウトしました。また使うときは `login` と送ってサインインしてください。"
                    ),
                    &key,
                );
            }
            CmdFx::SpawnScreen { outcome, what } => {
                let ctx = LogCtx::default();
                match outcome {
                    // pane は**疑い**でしかない。裏を取るのは既存の /usage 見張りの仕事なので、
                    // 次の flush で見に行くよう期限を過去に倒すだけ。**0 は使わない** —
                    // `usage_tick` が「初回なので少し待つ」の合図に使っているので、0 を書くと
                    // 確認が走らないどころか次の probe が 60 秒先送りになる
                    SpawnOutcome::UsageLimited => {
                        ctx.error(
                            "bridge",
                            &format!(
                                "spawn screen of {what} reads like the USAGE-LIMIT modal — \
                                 asking the usage monitor to confirm against /usage now \
                                 (the pane alone does NOT gate the fleet)"
                            ),
                        );
                        self.usage_polled_at_ms = 1;
                    }
                    // フリートゲートは張らない(`claude auth status` の裏取りが無い)。
                    // 言うだけ。Owner は `login` を送れる
                    SpawnOutcome::LoginRequired => {
                        ctx.error(
                            "bridge",
                            &format!(
                                "spawn screen of {what} reads like the LOGIN screen — the worker \
                                 is stuck at it and no key clears it. NOT gating the fleet \
                                 (no auth-status confirmation here yet); telling the Owner"
                            ),
                        );
                        self.post_notice(
                            &crate::t!(
                                "⚠️ An agent stopped at Claude's sign-in screen. Send `login` to sign in again.",
                                "⚠️ エージェントが Claude のサインイン画面で止まりました。`login` と送ってサインインし直してください。"
                            ),
                            &ctx,
                        )
                        .await;
                    }
                    SpawnOutcome::Answered | SpawnOutcome::NoScreen => {}
                }
            }
        }
    }
}
