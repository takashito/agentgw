//! claude 実体 — 起動行 / 生存判定 / サインイン用セッション / 画面と出力の読み取り。
//! **claude という語が出てよいのはこのファイルだけ。**
//!
//! hooks / mcp の設定ファイルは spawn ごとに Bridge が書き出す — この型は道具(tmux)しか
//! 持たず、パスは [`SpawnReq`] で受け取る(現行の受け渡しをそのまま型に閉じた形)。
//!
//! 画面(`Pane`)・transcript(`Transcript`)・モデル id(`ModelId`)の読み取りもここ。
//! **Slack へ出す文面は1つも持たない** — 描くのは Bridge の仕事(`command.rs`)。

use super::tmux::{Pid, Tmux, Window};
use super::{
    CompactOutcome, CompactProgress, ContextReport, LoginOutcome, ProbeErr, SpawnReq, UsageRow,
};
use crate::bridge::state::{LogCtx, StateDir, ThreadKey, WorkerState};
use std::time::Duration;

/// spawn ごとに新規 ID。使用済み ID での起動は claude に拒否される(スパイク実測)。
pub enum SessionMode {
    New(String),
    Resume(String),
}

impl SessionMode {
    fn session_id(&self) -> &str {
        match self {
            SessionMode::New(s) | SessionMode::Resume(s) => s,
        }
    }

    fn flag(&self) -> String {
        match self {
            SessionMode::New(s) => format!("--session-id {s}"),
            SessionMode::Resume(s) => format!("--resume {s}"),
        }
    }
}

/// 本番 Bun 版の `slack-login` と衝突させない dev 名。切替時に戻す。
const LOGIN_SESSION: &str = "slack-login-rs";

/// NEW スレッドの向き付け。原文コピー — 1字も変えない。
const NEW_PENDING_PREFIX: &str = "This is a NEW Slack thread; the message(s) below are the first you have received — \
     treat them as one burst (a single reply may cover several; a later one may correct an \
     earlier). Reply to them now, then wait for pushed messages; do not poll.";

/// RESUME の向き付け。原文コピー — 1字も変えない。
const RESUME_PENDING_PREFIX: &str = "You are RESUMING this Slack thread; the previous worker process was replaced and your \
     bridge/MCP is freshly reconnected, so agentgw tools work NOW. The message(s) below \
     arrived while the thread was down and are the ones to handle now — treat them as one burst \
     (a single reply may cover several; a later one may correct an earlier). Reply to them now, \
     then wait for pushed messages; do not poll.";

/// backlog が空のまま起動する worker の待受プロンプト。原文コピー — 1字も変えない。
const WORKER_STARTUP_PROMPT: &str = "You are a Slack thread worker. There is no message waiting right now — wait for messages \
     to be pushed to you and reply when they arrive. Do not poll and do not call any startup tool.";

/// 配達した本文が入力欄に残っていたとき、Enter を押し直す回数と、その間の一拍。
/// スラッシュコマンドの `SUBMIT_RETRY_CAP` と同じ考えで、対象が本文になっただけ。
const DELIVER_SUBMIT_RETRIES: u32 = 4;
const DELIVER_SUBMIT_POLL: Duration = Duration::from_millis(200);

/// claude を tmux の窓で飼う実体。
pub struct Claude {
    tmux: Tmux,
}

impl Claude {
    pub fn new(tmux: Tmux) -> Self {
        Self { tmux }
    }

    /// 同ファイルの argv 固定テスト専用。**本番の経路はここを通らない** — 窓を叩くのは
    /// `Claude` のメソッド(`deliver` / `capture` / `login_*` / `drive` …)だけ。
    #[cfg(test)]
    fn tmux(&self) -> &Tmux {
        &self.tmux
    }

    /// spawn する claude の起動行。
    fn launch_line(
        &self,
        mode: &SessionMode,
        hooks_file: &str,
        mcp_config: &str,
        prompt: &str,
    ) -> String {
        format!(
            // --strict-mcp-config: 本番プラグイン由来の MCP を載せない(dev ワーカーの隔離)
            "AGENTGW_SESSION_ID={} claude --settings {hooks_file} --mcp-config {mcp_config} \
             --strict-mcp-config {} {}",
            mode.session_id(),
            mode.flag(),
            Self::single_quote(prompt)
        )
    }

    /// シェルのシングルクォート括り。中の `'` は `'\''` で閉じ直す。
    fn single_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    /// 最初の1通を spawn プロンプトに同梱する(配達レースを構造的に避ける)。
    /// 封筒どうしは空行1つ(`\n\n`)で区切ってつなぐ。
    fn spawn_prompt(&self, envelope: &str, mode: &SessionMode) -> String {
        let prefix = match mode {
            SessionMode::New(_) => NEW_PENDING_PREFIX,
            SessionMode::Resume(_) => RESUME_PENDING_PREFIX,
        };
        format!("{prefix}\n\n{envelope}")
    }

    /// プール worker は配達待ちの本文を持たない — 常に待受プロンプトで起動する。
    fn pool_prompt(&self) -> &'static str {
        WORKER_STARTUP_PROMPT
    }

    /// 窓に claude を建てる。**Absent のときだけ** — spawn を kill にしない。
    /// 返すのは window_id(`@N`)。窓名は claude の画面タイトルで改名されるので当てにしない。
    pub fn spawn(&self, req: &SpawnReq) -> Result<Window, String> {
        if req.state != WorkerState::Absent {
            return Err(format!(
                "seat occupied ({:?}) — refusing to spawn over a live worker",
                req.state
            ));
        }
        let mode = match &req.resume_from {
            Some(id) => SessionMode::Resume(id.as_str().to_string()),
            None => SessionMode::New(req.session_id.as_str().to_string()),
        };
        // 本文が無い(= プールの空焚き)なら待受プロンプト、あれば向き付けを頭に付けた封筒
        let prompt = match &req.prompt {
            Some(envelope) => self.spawn_prompt(envelope, &mode),
            None => self.pool_prompt().to_string(),
        };
        let line = self.launch_line(&mode, &req.hooks_file, &req.mcp_config, &prompt);
        self.tmux.spawn(&req.window, &req.cwd, &line)
    }

    /// 押し込むのは tmux の仕事。**送信されたことを確かめるのはこちら** — 入力欄(`❯`)を
    /// 知っているのはこのファイルだけだから。
    ///
    /// `Tmux::deliver` の一拍は 1.3KB で実測した 200ms で、**長い封筒では足りない**
    /// (2026-08-02 実機: 4017B / 76行 で Enter が取り込み中の TUI に飲まれ、本文末尾の改行に
    /// なって入力欄に残った)。送信されていないので UserPromptSubmit も飛ばず、Bridge は
    /// send-keys の成功を配達成功と記録したまま40分沈黙した。
    ///
    /// 一拍を伸ばしても当て推量にしかならない — 詰まるのは TUI の描画で、長さと負荷で変わる。
    /// **入力欄が空くまで Enter を押し直す**のが唯一確かめられる形(`send_command` と同じ)。
    pub fn deliver(&self, w: &Window, text: &str) -> Result<(), String> {
        // **打つ前に、キーを取れる相手か確かめる。** モーダルが出ている窓に send-keys すると、
        // 届かないだけでなく本文がそのまま操作になる — 選択肢リストでは本文中の数字が選択、
        // 続く Enter が確定。押し直し(tick の再配達)まで含めると、人が答える前に勝手に選ばれる。
        // 画面が読めないときは進む(読めない = モーダルの証拠ではない。従来どおり)。
        if let Ok(pane) = self.tmux.capture(w)
            && let Some(why) = Self::not_accepting_keys(&pane)
        {
            return Err(format!("{w}: {why}"));
        }
        self.tmux.deliver(w, text)?;
        for attempt in 0..=DELIVER_SUBMIT_RETRIES {
            std::thread::sleep(DELIVER_SUBMIT_POLL);
            // 画面が読めないなら押さない — 見えていない窓に余計な Enter を落とす方が危ない
            let Ok(pane) = self.tmux.capture(w) else {
                return Ok(());
            };
            // 送っている最中に出たモーダルもここで捕まえる。押し直しは**ダイアログの確定**に
            // なるので、1発も撃たずに降りる
            if let Some(why) = Self::not_accepting_keys(&pane) {
                return Err(format!("{w}: {why}"));
            }
            if Pane::new(&pane).input_box_empty() {
                return Ok(());
            }
            if attempt < DELIVER_SUBMIT_RETRIES {
                self.tmux.send_enter(w)?;
            }
        }
        Err(format!(
            "typed into {w} but it never submitted — the input box still holds it after \
             {DELIVER_SUBMIT_RETRIES} extra Enter(s)"
        ))
    }

    /// この窓は打ち込みを受け取れる状態か。受け取れないなら**人に見せられる理由**を返す。
    ///
    /// **`❯` の有無では決まらない。** 2026-08-18 に実物3枚で確かめた TUI の形:
    ///
    /// | 画面 | 最後の `❯` 行 | `Esc to cancel` |
    /// |---|---|---|
    /// | 通常(待機中) | `❯ `(空) | 無し |
    /// | auto mode オンボーディング | `❯ `(空 — **箱は生きたまま**モーダルが前に出る) | あり |
    /// | `/model` セレクタ | `❯ 2. Opus …`(**`❯` を選択カーソルに奪われる**) | あり |
    ///
    /// 最初の実装は「`❯` の行が消える」を前提にしていて、**どちらのモーダルでも発火しなかった**。
    /// しかも `/model` 型は `input_box_empty()` が「本文が残っている」と読むので、押し直しの
    /// Enter がダイアログの確定になる。両方に共通するのは取り消しの案内行だけ — 走行中の
    /// `esc to interrupt` とは別の文字列なので、これで割れる。
    ///
    /// `❯` が1本も無い画面も断る。箱が見えない以上「送れた」とは言えない(未知のモーダル)。
    fn not_accepting_keys(pane: &str) -> Option<String> {
        let p = Pane::new(pane);
        if let Some(footer) = p.modal_footer() {
            return Some(format!(
                "a dialog has the keyboard — {}",
                Self::dialog_title(pane, footer)
            ));
        }
        p.input_line()
            .is_empty()
            .then(|| "no input box on screen".to_string())
    }

    /// ⚠️ に載せる1行。既知の文言表([`Pane::spawn_screen`])は使わない — 未知のモーダルこそが
    /// 詰まりの正体なので、画面が自分で書いた見出しをそのまま借りる。案内行の**手前**だけを
    /// 遡って問いかけの行(`?`)を探し、無ければ案内行そのもの(それでも「ダイアログだ」は伝わる)。
    /// pane 全体から `?` を拾うと、スクロールバックに残った人の発言を掴む。
    fn dialog_title<'a>(pane: &'a str, footer: &'a str) -> &'a str {
        const LOOK_BACK: usize = 30;
        let lines: Vec<&str> = pane.split('\n').collect();
        let at = lines.iter().position(|l| *l == footer).unwrap_or(0);
        lines[at.saturating_sub(LOOK_BACK)..at]
            .iter()
            .rev()
            .map(|l| strip_modal_decoration(l).trim_end())
            .find(|l| l.ends_with('?'))
            .unwrap_or_else(|| footer.trim())
    }

    pub fn terminate(&self, w: &Window) -> Result<(), String> {
        self.tmux.kill_window(w)
    }

    /// ワーカー本体のプロセス。**唯一の生存証明** — connector の無い Rust では、
    /// session_end の飛ばない `kill -9` で死んだワーカーはここでしか気付けない。
    pub fn pid_of(&self, window_id: Option<&str>, window_name: &str) -> Option<Pid> {
        self.tmux.pid_of(window_id, window_name)
    }

    /// pane を1枚。取れなければログして空文字 — 現行も capture の失敗を握って判定を続ける
    /// (一度の取りこぼしでコマンドを諦めない)。
    fn capture(&self, w: &Window, label: &str, ctx: &LogCtx) -> String {
        self.tmux.capture(w).unwrap_or_else(|e| {
            ctx.error("bridge", &format!("{label}: capture-pane {w} failed: {e}"));
            String::new()
        })
    }

    /// サインイン用セッションのターゲット。ワーカーセッションの窓ではないので修飾しない。
    fn login_window(&self) -> Window {
        Window::raw(LOGIN_SESSION)
    }

    /// サインイン用のセッション。`-x 400` の幅広ペインは、認証 URL を1行で印字させるため
    /// (折り返すと URL が拾えない)。
    fn login_start(&self, cwd: &str) -> Result<(), String> {
        (self.tmux.run)(&[
            "new-session",
            "-d",
            "-s",
            LOGIN_SESSION,
            "-x",
            "400",
            "-y",
            "50",
            "-c",
            cwd,
        ])
        .map(|_| ())
    }

    /// 掃除。無ければ tmux が非0で返すだけ — それは「掃除するものが無かった」なので黙る。
    pub fn login_kill(&self) {
        let _ = (self.tmux.run)(&["kill-session", "-t", LOGIN_SESSION]);
    }

    /// TUI に1行打ち込んで、効いたことを画面で確かめる。
    ///
    /// 訊き方を2つとも間違えると毎コマンド 20 秒の空振りになる、というのが現行の学び:
    /// 「まだ入力欄に居るか」は pane 全体ではなく**入力ボックス**に訊く(送信済みのコマンドは
    /// echo として画面に残り続ける)。「確認が出たか」は新しさを問わず、**頼んだ相手を名指しする
    /// 行が画面にあるか**だけを訊く(古い行が同じ相手を名乗っているなら、ワーカーは既にそこに居る)。
    ///
    /// **移植元との差分**(2026-07-29 実機): 現行 TUI は `/effort <level>` の前に確認ダイアログを
    /// 挟むことがある(履歴のあるセッション)。Enter で確定するまで待っても何も出ないので、
    /// ここで押す。移植元の Bun は 2026-07-17 時点の TUI で、この画面を知らない。
    async fn drive(
        &self,
        target: &Window,
        cmd: &str,
        confirmed: impl Fn(&str) -> bool,
        label: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) -> bool {
        const MAX_MS: u64 = 20_000;
        const POLL: Duration = Duration::from_millis(400);
        const SUBMIT_RETRY_CAP: u32 = 4;
        /// ダイアログの Enter は submit リトライとは別勘定 — 押しているのは入力欄ではなくボタンで、
        /// 確定後の再描画で1回空振りする分だけ余裕を持たせる。
        const DIALOG_CONFIRM_CAP: u32 = 2;
        let tmux = &self.tmux;
        ctx.info(
            "bridge",
            &format!("{label}: sending {cmd} to worker window {target} key={key}"),
        );
        if let Err(e) = tmux.send_command(target, cmd) {
            ctx.error(
                "bridge",
                &format!("{label}: send-keys {cmd} to {target} failed: {e}"),
            );
            return false;
        }
        // 入力ボックスに残るなら残るのはこの語 — `/effort`
        let word = cmd.split(' ').next().unwrap_or(cmd);
        let started = std::time::Instant::now();
        let mut retries = 0;
        let mut dialog_confirms = 0;
        // send_command が既に送った Enter を TUI が処理し終える前に判定しない
        tokio::time::sleep(Duration::from_millis(800)).await;
        while started.elapsed().as_millis() as u64 <= MAX_MS {
            let pane = self.capture(target, label, ctx);
            let unsubmitted = Pane::new(&pane).still_has_command(word);
            if !unsubmitted && confirmed(&pane) {
                ctx.info(
                    "bridge",
                    &format!(
                        "{label}: done key={key} after {}ms",
                        started.elapsed().as_millis()
                    ),
                );
                return true;
            }
            if unsubmitted && retries < SUBMIT_RETRY_CAP {
                retries += 1;
                ctx.info(
                    "bridge",
                    &format!(
                        "{label}: {word} still un-submitted — pressing Enter \
                         ({retries}/{SUBMIT_RETRY_CAP}) key={key}"
                    ),
                );
                if let Err(e) = tmux.send_enter(target) {
                    ctx.debug(
                        "bridge",
                        &format!("{label}: submit Enter failed key={key}: {e}"),
                    );
                }
            } else if Pane::new(&pane).effort_confirm_dialog_open()
                && dialog_confirms < DIALOG_CONFIRM_CAP
            {
                // 確認ダイアログ(2026-07-29 実機)。既定の選択肢が「Yes」なので Enter で確定する。
                // ここに来る時 `unsubmitted` は必ず false — 入力欄はもう空で、`❯` の行は選択肢
                // (`/model` は今のところダイアログを出さないが、出したら同じ場所で拾える)
                dialog_confirms += 1;
                ctx.info(
                    "bridge",
                    &format!(
                        "{label}: confirming the effort-change dialog (Enter) \
                         ({dialog_confirms}/{DIALOG_CONFIRM_CAP}) key={key}"
                    ),
                );
                if let Err(e) = tmux.send_enter(target) {
                    ctx.debug(
                        "bridge",
                        &format!("{label}: dialog Enter failed key={key}: {e}"),
                    );
                }
            }
            tokio::time::sleep(POLL).await;
        }
        // 諦める前に画面を元に戻す — 打ちかけのコマンドや開いたままのダイアログを残すと、
        // 次に届くメッセージが選択肢に食われる
        if let Err(e) = tmux.send_escape(target) {
            ctx.debug(
                "bridge",
                &format!("{label}: cleanup Escape failed key={key}: {e}"),
            );
        }
        ctx.error(
            "bridge",
            &format!(
                "{label}: timed out after {MAX_MS}ms without a confirmation for {cmd} key={key}"
            ),
        );
        false
    }

    /// 起動直後の窓を見張って、答えられる画面に答える。
    ///
    /// **これが唯一の答え手。** 立ち上がりの期限は他に無く(`starting` の掛け金を外すのは
    /// user_prompt hook だけ)、画面の前で止まった窓は永久に「起動中」のまま配達を queue に
    /// 溜める。2026-08-02 に実機で: trust ダイアログの後ろにメッセージが積まれ、
    /// Bridge は沈黙した。
    ///
    /// 期限まで**見続ける**のは、画面が遅れて出ることがあるから(現行の
    /// linger と同じ理由)。現行は spawn 枠を握る都合で 30 秒の同期待ち + 120 秒の背景見張りに
    /// 割っているが、こちらは最初から背景タスクなので1本にまとめてある。
    pub async fn watch_spawn_screens(
        &self,
        w: &Window,
        budget_ms: u64,
        poll_ms: u64,
        ctx: &LogCtx,
    ) -> SpawnOutcome {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(budget_ms);
        let mut answered = false;
        let mut trust_answered = false;
        // **confirm にも掛け金が要る。** 現行は confirm に答えた時点で見張りを畳むので
        // 掛け金を持たない。こちらは畳まないので、掛け金が無いと
        // 画面が消えるまで毎秒 Enter を撃ち続ける
        let mut confirm_answered = false;
        while std::time::Instant::now() < deadline {
            match Pane::new(&self.capture(w, "spawn-screen", ctx)).spawn_screen() {
                // 撃っても消えない画面。撃てば入力欄の中身を送ることになるので、撃たずに降りる
                SpawnScreen::LoginRequired => {
                    ctx.error(
                        "spawn",
                        &format!(
                            "{w}: pane reads like the LOGIN screen — abandoning the watch \
                             (no key clears it)"
                        ),
                    );
                    return SpawnOutcome::LoginRequired;
                }
                SpawnScreen::UsageLimited => {
                    ctx.error(
                        "spawn",
                        &format!(
                            "{w}: pane reads like the USAGE-LIMIT modal — abandoning the watch \
                             (no key clears it)"
                        ),
                    );
                    return SpawnOutcome::UsageLimited;
                }
                // 同じ画面が続く間に何度も撃たない(現行)
                SpawnScreen::Trust => {
                    if !trust_answered {
                        if let Err(e) = self.tmux.send_enter(w) {
                            ctx.error("spawn", &format!("{w}: trust dialog Enter failed: {e}"));
                        }
                        trust_answered = true;
                        answered = true;
                        ctx.info(
                            "spawn",
                            &format!("{w}: workspace-trust dialog seen — accepted"),
                        );
                    }
                }
                SpawnScreen::Confirm => {
                    if !confirm_answered {
                        if let Err(e) = self.tmux.send_enter(w) {
                            ctx.error("spawn", &format!("{w}: confirm prompt Enter failed: {e}"));
                        }
                        confirm_answered = true;
                        answered = true;
                        ctx.info("spawn", &format!("{w}: dev-channels confirm prompt seen"));
                    }
                }
                SpawnScreen::None_ => {}
            }
            tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
        }
        if answered {
            SpawnOutcome::Answered
        } else {
            SpawnOutcome::NoScreen
        }
    }

    /// `/model <名前>` を打ち込む。返すのは「頼んだモデルに居ると画面が言ったか」。
    pub async fn set_model(
        &self,
        target: &Window,
        name: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) -> bool {
        self.drive(
            target,
            &format!("/model {name}"),
            |pane| Pane::new(pane).model_confirmed(name),
            "model",
            key,
            ctx,
        )
        .await
    }

    /// `/effort <level>` を打ち込む。
    ///
    /// ダイアログを挟んだ経路(履歴のあるセッション)は確定しても `Set effort level to …` を
    /// 出さない — 出るのは状態行だけなので、その行が頼んだ level を名乗っていれば効いたと読む
    /// (2026-07-29 実機)。
    pub async fn set_effort(
        &self,
        target: &Window,
        level: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) -> bool {
        self.drive(
            target,
            &format!("/effort {level}"),
            |pane| {
                let p = Pane::new(pane);
                p.effort_confirmed(level) || p.effort_status() == Some(level)
            },
            "effort",
            key,
            ctx,
        )
        .await
    }

    /// いまの権限モード。フッタを1回読むだけ — キーは撃たないので走行中でも安全。
    pub fn mode(&self, target: &Window, ctx: &LogCtx) -> &'static str {
        Pane::new(&self.capture(target, "mode", ctx)).mode_status()
    }

    /// shift+tab(tmux の `BTab`)を目当てのモードに着くまで押す。
    ///
    /// 巡回の**順番は当てにしない** — 何段あるかは設定次第(bypass / don't ask は
    /// 出たり出なかったりする)。1発押しては読む、を1周ぶん繰り返すだけ。
    pub async fn set_mode(
        &self,
        target: &Window,
        want: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) -> bool {
        /// 巡回は最大6段(manual/plan/edit/auto/bypass/don't ask)— 1周して戻れば打ち止め。
        const MAX_PRESSES: u32 = 6;
        const SETTLE: Duration = Duration::from_millis(400);
        for i in 0..=MAX_PRESSES {
            let now = self.mode(target, ctx);
            if now == want {
                ctx.info(
                    "bridge",
                    &format!("mode: {want} after {i} shift+tab press(es) key={key}"),
                );
                return true;
            }
            if i == MAX_PRESSES {
                ctx.error(
                    "bridge",
                    &format!(
                        "mode: cycled {MAX_PRESSES} times without reaching {want} \
                         (stuck at {now}) key={key}"
                    ),
                );
                return false;
            }
            if let Err(e) = self.tmux.send_key(target, "BTab") {
                ctx.error(
                    "bridge",
                    &format!("mode: send-keys BTab to {target} failed: {e}"),
                );
                return false;
            }
            tokio::time::sleep(SETTLE).await;
        }
        unreachable!("the loop always returns at i == MAX_PRESSES")
    }

    /// `/compact` を打ち込み、pane のスピナーを `on_progress` に1つずつ渡す
    ///
    /// **Slack へは何も出さない** — 付箋を作るか編集するか、どんな文面にするかは Bridge の判断。
    /// ここが返すのは結末だけ。最長6分かかるので呼び手は select ループの外で回す。
    ///
    /// `on_progress` は `Fn`(`AsyncFnMut` ではない)— 返す future が引数の寿命に依存しない形で
    /// ないと、この future を `tokio::spawn` する時に slack-morphism の `Send` が
    /// 高階の寿命で解けない(実測: "implementation of `Send` is not general enough")。
    /// 呼び手は畳む状態を `RefCell` に置く。
    pub async fn compact<F, Fut>(
        &self,
        target: &Window,
        key: &ThreadKey,
        sid: &str,
        on_progress: F,
    ) -> CompactOutcome
    where
        F: Fn(CompactProgress) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        const MAX_MS: u64 = 6 * 60_000; // 詰まった圧縮が永遠にポーリングしないための天井
        const POLL: Duration = Duration::from_millis(800);
        const SUBMIT_RETRY_CAP: u32 = 4;
        let ctx = LogCtx {
            session_id: Some(sid.to_string()),
            thread_key: Some(key.clone()),
        };
        let tmux = &self.tmux;
        let short: String = sid.chars().take(8).collect();
        // 前回の圧縮が残した `Compacted (ctrl+o …)` は画面に居座る — 先に控えて、**新しい**印だけを
        // 完了と読む
        let baseline = tmux.capture(target).unwrap_or_default();
        let stale_done = baseline.to_lowercase().contains("compacted (ctrl+o");
        ctx.info(
            "bridge",
            &format!(
                "compact: sending /compact to worker window {target} (session {short}) key={key}"
            ),
        );
        if let Err(e) = tmux.send_command(target, "/compact") {
            ctx.error(
                "bridge",
                &format!("compact: send-keys /compact to {target} failed: {e}"),
            );
            return CompactOutcome::Failed;
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
        let started = std::time::Instant::now();
        let mut retries = 0;
        let mut seen = false;
        loop {
            if started.elapsed().as_millis() as u64 > MAX_MS {
                ctx.error(
                    "bridge",
                    &format!("compact: timed out after {MAX_MS}ms (seen={seen}) key={key}"),
                );
                break CompactOutcome::Failed;
            }
            let pane = self.capture(target, "compact", &ctx);
            let lower = pane.to_lowercase();
            if lower.contains("not enough messages to compact") {
                ctx.info(
                    "bridge",
                    &format!("compact: nothing to compact (session too small) key={key}"),
                );
                break CompactOutcome::Nothing;
            }
            let st = Pane::new(&pane).compact_progress();
            if st.active {
                seen = true;
                // 現行 Bun はここで shimmer を秒数付きに張り直す。
                // Rust 版は compact に専用 status を持たないので何もしない — 進捗は呼び手の
                // sticky が見せる(user_compact のコメント参照。意図的逸脱)
                on_progress(st).await;
            } else if seen || (lower.contains("compacted (ctrl+o") && !stale_done) {
                // 終わりを名乗るのは**肯定的な合図**だけ: スピナーが消えるのを見届けたか、
                // 新しい `Compacted` の印が出たか(速すぎてスピナーを1度も捉えられなかった時)。
                // 「まだスピナーが出ていない」で閉じない — 出遅れた圧縮の途中でバーを畳んだバグ
                ctx.info(
                    "bridge",
                    &format!(
                        "compact: done key={key} ({}) after {}ms",
                        if seen {
                            "spinner cleared"
                        } else {
                            "Compacted marker"
                        },
                        started.elapsed().as_millis()
                    ),
                );
                break CompactOutcome::Done;
            } else if Pane::new(&pane).still_has_command("/compact") && retries < SUBMIT_RETRY_CAP {
                // スラッシュコマンドの補完メニューが submit の Enter を食うことがある
                retries += 1;
                ctx.info(
                    "bridge",
                    &format!(
                        "compact: /compact still un-submitted — pressing Enter \
                         ({retries}/{SUBMIT_RETRY_CAP}) key={key}"
                    ),
                );
                if let Err(e) = tmux.send_enter(target) {
                    ctx.debug(
                        "bridge",
                        &format!("compact: submit Enter failed key={key}: {e}"),
                    );
                }
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// いまの effort level を TUI に訊く。level はどこにも記録が無いので、
    /// 唯一の正は TUI 自身: `/effort` でスライダを開き、Escape で閉じ、その時 TUI が入力欄の上に
    /// 出す状態行(`● high · /effort`)を読む。スライダそのものは解釈しない。
    /// 読めなければ None(断りの文面は呼び手が決める)。
    pub async fn effort(
        &self,
        target: &Window,
        key: &ThreadKey,
        sid: &str,
    ) -> Option<&'static str> {
        const MAX_MS: u64 = 15_000;
        const POLL: Duration = Duration::from_millis(800);
        const READ_MS: u64 = 5_000;
        const SUBMIT_RETRY_CAP: u32 = 4;
        let ctx = LogCtx {
            session_id: Some(sid.to_string()),
            thread_key: Some(key.clone()),
        };
        let tmux = &self.tmux;
        let short: String = sid.chars().take(8).collect();
        ctx.info(
            "bridge",
            &format!(
                "effort: opening /effort on worker window {target} (session {short}) key={key}"
            ),
        );
        if let Err(e) = tmux.send_command(target, "/effort") {
            ctx.error(
                "bridge",
                &format!("effort: send-keys /effort to {target} failed: {e}"),
            );
            return None;
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
        let started = std::time::Instant::now();
        let mut retries = 0;
        // 第1幕: スライダが開くのを待つ(補完メニューに Enter を食われている間は押し直す)
        let mut slider_seen = false;
        while started.elapsed().as_millis() as u64 <= MAX_MS {
            let pane = self.capture(target, "effort", &ctx);
            if Pane::new(&pane).effort_slider_open() {
                slider_seen = true;
                break;
            }
            if Pane::new(&pane).still_has_command("/effort") && retries < SUBMIT_RETRY_CAP {
                retries += 1;
                ctx.info(
                    "bridge",
                    &format!(
                        "effort: /effort still un-submitted — pressing Enter \
                         ({retries}/{SUBMIT_RETRY_CAP}) key={key}"
                    ),
                );
                if let Err(e) = tmux.send_enter(target) {
                    ctx.debug(
                        "bridge",
                        &format!("effort: submit Enter failed key={key}: {e}"),
                    );
                }
            }
            tokio::time::sleep(POLL).await;
        }
        if !slider_seen {
            // 打ちかけを入力欄に残さず片付けてから諦める
            if let Err(e) = tmux.send_escape(target) {
                ctx.debug(
                    "bridge",
                    &format!("effort: cleanup Escape failed key={key}: {e}"),
                );
            }
            ctx.error(
                "bridge",
                &format!(
                    "effort: timed out after {MAX_MS}ms without seeing the /effort slider key={key}"
                ),
            );
            return None;
        }
        // 第2幕: スライダを閉じる — TUI が現在の level を名乗る状態行で答える
        if let Err(e) = tmux.send_escape(target) {
            ctx.error(
                "bridge",
                &format!("effort: closing Escape failed key={key}: {e}"),
            );
            return None;
        }
        let read_started = std::time::Instant::now();
        while read_started.elapsed().as_millis() as u64 <= READ_MS {
            let pane = self.capture(target, "effort", &ctx);
            if let Some(level) = Pane::new(&pane).effort_status() {
                ctx.info(
                    "bridge",
                    &format!("effort: current={level} session={short} key={key}"),
                );
                return Some(level);
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        // 状態行は一過性の通知 — 別の通知(「Plugins updated」など)にその枠を取られると読めない。
        // 実機で観測済み。取り直しは Owner の一言の再試行に任せる
        ctx.error(
            "bridge",
            &format!(
                "effort: slider closed but no status line appeared within {READ_MS}ms key={key}"
            ),
        );
        None
    }

    /// headless の `claude` を1回だけ回して stdout を返す(`probe`)。
    ///
    /// `AGENTGW_SESSION_ID` を外すのが肝 — これが残っていると子プロセスの hook が自分をワーカーだと
    /// 名乗り、probe が本物のセッションとして扱われる。stderr は捨てる。
    /// `kill_on_drop` はタイムアウトの後始末 — 現行が `proc.kill()` でやっていること。
    /// `/context` を訊く argv(`--fork-session` で本体のセッションを汚さない)。
    pub fn context_argv(session_id: &str) -> Vec<String> {
        [
            "claude",
            "--resume",
            session_id,
            "--fork-session",
            "-p",
            "/context",
        ]
        .map(str::to_string)
        .to_vec()
    }

    /// `/usage` を訊く argv。アカウント全体の話なのでセッションは要らない。
    pub fn usage_argv() -> Vec<String> {
        ["claude", "-p", "/usage"].map(str::to_string).to_vec()
    }

    /// サインインを始める。**URL が出るまで**は呼び手が [`Claude::login_url`] で待つ。
    pub fn login_begin(&self, cwd: &str) -> Result<(), String> {
        // 毎回まっさらに — 残骸を落としてから幅広 pane で起こす(URL を1行で印字させる)
        self.login_kill();
        self.login_start(cwd).and_then(|()| {
            self.tmux
                .send_command(&self.login_window(), "claude auth login")
        })
    }

    /// 認証 URL が画面に出ていれば返す。
    pub fn login_url(&self) -> Option<String> {
        self.tmux
            .capture(&self.login_window())
            .ok()
            .and_then(|pane| Pane::new(&pane).auth_login_url())
    }

    /// 貼られたコードを送り込む。
    pub fn login_submit_code(&self, code: &str) -> Result<(), String> {
        self.tmux.send_command(&self.login_window(), code)
    }

    /// サインインの結末。scrollback ごと読む: "Login successful." を出した直後に CLI は
    /// シェルへ戻り、次のポーリングまでに印が画面外へ流れる。
    pub fn login_outcome(&self) -> LoginOutcome {
        let pane = self
            .tmux
            .capture_history(&self.login_window(), 200)
            .unwrap_or_default();
        Pane::new(&pane).login_outcome()
    }

    /// サインアウト。実体のコマンドを1回叩くだけ。`Failed` は非0終了(中身は終了状態の
    /// 説明)、`Errored` は起動そのものの失敗 — 呼び手はこの2つで別の文面を出す。
    pub async fn logout(&self) -> Result<(), ProbeErr> {
        let run = tokio::process::Command::new("claude")
            .args(["auth", "logout"])
            .stdin(std::process::Stdio::null())
            .output()
            .await;
        match run {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(ProbeErr::Failed(
                o.status
                    .code()
                    .map_or_else(|| "by signal".to_string(), |c| c.to_string()),
            )),
            Err(e) => Err(ProbeErr::Errored(e.to_string())),
        }
    }

    pub async fn probe(&self, argv: Vec<String>, cwd: String) -> Result<String, ProbeErr> {
        let Some((bin, args)) = argv.split_first() else {
            return Err(ProbeErr::Errored("empty probe argv".to_string()));
        };
        let run = tokio::process::Command::new(bin)
            .args(args)
            .current_dir(&cwd)
            .env_remove("AGENTGW_SESSION_ID")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(PROBE_TIMEOUT, run).await {
            Err(_) => Err(ProbeErr::Failed(format!(
                "probe timed out after {}ms",
                PROBE_TIMEOUT.as_millis()
            ))),
            // spawn そのものの失敗(claude が無い / cwd が無い)は再試行しても同じ
            Ok(Err(e)) => Err(ProbeErr::Errored(format!(
                "probe could not start in {cwd}: {e}"
            ))),
            Ok(Ok(o)) if !o.status.success() => Err(ProbeErr::Failed(format!(
                "probe: claude exited {}",
                o.status
                    .code()
                    .map_or_else(|| "by signal".to_string(), |c| c.to_string())
            ))),
            Ok(Ok(o)) => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        }
    }
}

/// headless probe を諦める時刻(`ms = 60_000`)。
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// `/model` にそのまま渡す形のモデル名。**claude の語彙**なので
/// ここが正 — コマンドの検出側(`command::parse_model_command`)もこの表を見る。
pub const MODEL_NAMES: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];

// ── 手書きの走査ヘルパ(regex を足さない) ───────────────────────────────────

/// ASCII 大小文字を無視した接頭辞一致(regex の `/i` の代わり)。
fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
}

/// regex の `\w`(語構成文字)。`\b` の判定は両側をこれで見る。
fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// ASCII 大小を無視した全出現位置(regex の `/i` + エンジンの位置送り相当)。
/// `to_ascii_lowercase` は ASCII しか変えないのでバイト位置は原文と同じ。`needle` は小文字で渡す。
fn match_indices_ci(hay: &str, needle: &str) -> Vec<usize> {
    hay.to_ascii_lowercase()
        .match_indices(needle)
        .map(|(i, _)| i)
        .collect()
}

/// `**Key:**` の**値**: 続く空白(改行も含む — 現物の `\s*` がそう)を飛ばした先の行。
/// 値が空なら None。
fn header_value(raw: &str, key: &str) -> Option<String> {
    let i = raw.find(key)?;
    let rest = raw[i + key.len()..].trim_start();
    let line = rest.split('\n').next().unwrap_or("").trim_end();
    (!line.is_empty()).then(|| line.to_string())
}

/// `**Tokens:** <used> / <total> (<pct>)`。 の3捕獲 regex 相当
/// 形が崩れているヘッダは飛ばして次の `**Tokens:**` を試す(regex が位置を進めるのと同じ)。
fn tokens_header(raw: &str) -> Option<(String, String, String)> {
    const KEY: &str = "**Tokens:**";
    for (i, _) in raw.match_indices(KEY) {
        let s = raw[i + KEY.len()..].trim_start();
        // `[^/\s]+` — スラッシュも空白も含まない一続き
        let used: String = s
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '/')
            .collect();
        if used.is_empty() {
            continue;
        }
        let Some(s) = s[used.len()..].trim_start().strip_prefix('/') else {
            continue;
        };
        let s = s.trim_start();
        let total: String = s
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '(')
            .collect();
        if total.is_empty() {
            continue;
        }
        let Some(s) = s[total.len()..].trim_start().strip_prefix('(') else {
            continue;
        };
        let Some(end) = s.find(')') else { continue };
        return Some((used, total, s[..end].trim().to_string()));
    }
    None
}

/// `used` の後ろの残り: `(?:.*?\bresets\s+(.+?))?\s*$` 相当。
/// `Some("")` = reset 節なしで行が終わっている / `Some(when)` = 節あり /
/// **None = 上限行ではない**(`resets` でない余計な文字が残っている)。
fn usage_reset_clause(tail: &str) -> Option<String> {
    let lower = tail.to_ascii_lowercase(); // ASCII 変換なのでバイト位置は tail と同じ
    let mut from = 0;
    while let Some(rel) = lower[from..].find("resets") {
        let i = from + rel;
        // `\b`: 直前が語構成文字なら境界でない(`presets` は resets ではない)
        let word_before = tail[..i]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let after = &tail[i + "resets".len()..];
        if !word_before && after.starts_with(char::is_whitespace) {
            return Some(after.trim().to_string());
        }
        from = i + "resets".len();
    }
    tail.trim().is_empty().then(String::new)
}

/// 1行を上限行として読む。`^\s*(.+?):\s*(\d+)%\s+used\b…$` の手書き版 —
/// ラベルは最短一致なので、**うまくパースできる最初のコロン**で切る。
fn parse_usage_line(line: &str) -> Option<UsageRow> {
    for (ci, _) in line.match_indices(':') {
        let label = line[..ci].trim();
        if label.is_empty() {
            continue; // `(.+?)` は1文字以上
        }
        let after = line[ci + 1..].trim_start();
        let pct: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if pct.is_empty() {
            continue;
        }
        let Some(rest) = after[pct.len()..].strip_prefix('%') else {
            continue;
        };
        let trimmed = rest.trim_start();
        if trimmed.len() == rest.len() {
            continue; // `\s+` は1文字以上
        }
        if !starts_with_ci(trimmed, "used") {
            continue;
        }
        let rest = &trimmed["used".len()..];
        // `used` の直後の `\b` — 語が続くなら別の語(`usedxx`)
        if rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        // ここも continue — 残りが reset 節でなければ「このコロンでの切り方が違った」だけで、
        // 現物の遅延一致は次のコロンを試す(`A: 12% used, X: 66% used` は2つ目で一致する)
        let Some(reset) = usage_reset_clause(rest) else {
            continue;
        };
        return Some(UsageRow {
            label: label.to_string(),
            pct,
            reset,
        });
    }
    None
}

/// `(33s)` / `(1m 4s · ↑ 876 tokens)` の経過秒。`\((?:(\d+)m\s*)?(\d+)s\b[^)]*\)` の手書き版 —
/// 数字は `(` の**直後**から始まる(`(elapsed 33s)` は一致しない)。
fn parse_elapsed(spinner: &str) -> Option<u32> {
    for (i, _) in spinner.match_indices('(') {
        let s = &spinner[i + 1..];
        let d1: String = s.chars().take_while(char::is_ascii_digit).collect();
        if d1.is_empty() {
            continue;
        }
        let after = &s[d1.len()..];
        // `Nm` があれば分。無ければ最初の数字がそのまま秒(regex の optional group の backtrack)
        let (mins, secs, rest) = match after.strip_prefix(['m', 'M']) {
            Some(r) => {
                let r = r.trim_start();
                let d2: String = r.chars().take_while(char::is_ascii_digit).collect();
                if d2.is_empty() {
                    continue;
                }
                (d1.parse().unwrap_or(0), d2.clone(), &r[d2.len()..])
            }
            None => (0u32, d1.clone(), after),
        };
        // `s\b` の後は `[^)]*\)` — `[^)]*` は `)` を跨げないので「後ろに `)` がある」と同義
        let Some(tail) = rest.strip_prefix(['s', 'S']) else {
            continue;
        };
        if tail.starts_with(is_word) || !tail.contains(')') {
            continue;
        }
        return Some(mins * 60 + secs.parse().unwrap_or(0));
    }
    None
}

/// スピナー行の括弧内のトークン数: `([↑↓])\s*([\d.]+[km]?)\s*tokens` 相当。
fn parse_tokens(spinner: &str) -> Option<(char, String)> {
    for (i, dir) in spinner
        .char_indices()
        .filter(|(_, c)| matches!(c, '↑' | '↓'))
    {
        let s = spinner[i + dir.len_utf8()..].trim_start();
        let mut num: String = s
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if num.is_empty() {
            continue;
        }
        let after = &s[num.len()..];
        // `[km]?` は捕獲の中(`1.6k` で1つの印字)
        let after = match after.strip_prefix(['k', 'm', 'K', 'M']) {
            Some(r) => {
                num.push_str(&after[..after.len() - r.len()]);
                r
            }
            None => after,
        };
        if starts_with_ci(after.trim_start(), "tokens") {
            return Some((dir, num));
        }
    }
    None
}

/// 1行から `(\d{1,3})\s*%` の**最初の**一致。`%` の手前の空白を飛ばして最大3桁を後ろから取る。
fn parse_percent_line(line: &str) -> Option<u32> {
    for (i, _) in line.match_indices('%') {
        let head = line[..i].trim_end();
        let digits: String = head
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if !digits.is_empty() {
            return digits.parse().ok(); // 行内の最初の一致だけ(現物も break せず次の行へ)
        }
    }
    None
}

/// スライダの状態行に出る表示レベル。`auto` は**入らない** — auto の時は解決先の
/// レベルが名乗られる。 の選択肢そのまま。
const STATUS_LEVELS: [&str; 6] = ["low", "medium", "high", "xhigh", "max", "ultracode"];

/// 権限モードのフッタ行 → こちらの呼び名。TUI は入力欄の下に
/// `⏵⏵ auto mode on (shift+tab to cycle)` の形で今のモードを名乗る
/// (cli 2.1.220 の表: default→`manual mode` / plan→`plan mode` / acceptEdits→`accept edits` /
/// auto→`auto mode` / bypassPermissions→`bypass permissions` / dontAsk→`don't ask`)。
///
/// **2026-07-31 実機**(dev ワーカーに `send-keys BTab` を4回)で巡回が
/// `auto → manual → accept edits → plan → auto` と1周することと、
/// manual でも `⏸ manual mode on` の行が出ることを確認。`(shift+tab to cycle)` の但し書きは
/// 出たり出なかったりするので目印にしない。
/// bypass / dontask は `mode` コマンドの引数には無いが、行が読めれば manual と
/// 取り違えずに済むので表に入れておく。
const MODE_LABELS: [(&str, &str); 6] = [
    ("plan mode on", "plan"),
    ("accept edits on", "edit"),
    ("auto mode on", "auto"),
    ("bypass permissions on", "bypass"),
    ("don't ask on", "dontask"),
    ("manual mode on", "manual"),
];

/// エラー側の目印(alternation 原文どおり)。
const LOGIN_ERROR_MARKERS: [&str; 9] = [
    "invalid code",
    "please make sure the full code",
    "invalid",
    "expired",
    "failed",
    "incorrect",
    "denied",
    "not valid",
    "try again",
];

/// 起動直後の窓に出うる画面。**誰かが答えるまで claude はプロンプトを1文字も読まない。**
///
/// 現行 `classifySpawnPane` の移植。4種のうち上2つは Enter で答えられ、
/// 下2つは**答えられない**(アカウントの事情なので、何度起こし直しても同じ壁に当たる)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnScreen {
    /// claude 自身の workspace-trust。cwd が `~/.claude.json` の
    /// `projects[cwd].hasTrustDialogAccepted` に無いと出る(2026-08-02、実機)
    Trust,
    /// dev channels の確認。既定の選択肢が Yes なので Enter で通る
    Confirm,
    /// サインイン画面。Enter では消えない
    LoginRequired,
    /// 上限モーダル。Enter では消えない
    UsageLimited,
    None_,
}

/// 見張りの結末。現行 `SpawnOutcome` の移植だが、**成功の意味が違う**:
/// Rust の「起動できた」は `starting` の掛け金を最初の user_prompt が外すことで表しているので、
/// ここは「答えるべき画面に答えたか」しか言わない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnOutcome {
    /// 答えられる画面に答えた(あるいは答えた上で期限まで見ていた)
    Answered,
    /// サインイン画面。**誰も答えられない** — Owner に言うしかない
    LoginRequired,
    /// 上限モーダル。同上(裏取りは呼び手の仕事)
    UsageLimited,
    /// 期限まで何も出なかった。**失敗ではない** — 普通に立ち上がった窓がこれ
    NoScreen,
}

/// 画面が**自分で印字した行**の頭に出うる飾り(枠線・選択子・選択肢の番号)。
/// 現行 `MODAL_LINE_DECORATION` と同じ集合。
const MODAL_DECORATION: &[char] = &[
    ' ', '\t', '│', '┃', '|', '┆', '╎', '▏', '▕', '>', '❯', '➤', '·', '•', '-', '–', '—',
];

/// 「省略可の前置き」×「語幹」。現行の正規表現を `regex` 無しで言うための形
/// (`(?:…)?` は空も許すので前置きに `""` が入っている)。
type Prompts = &'static [(&'static [&'static str], &'static [&'static str])];

/// 現行 `LIMIT_MODAL_PROMPTS`。
const LIMIT_PROMPTS: Prompts = &[
    (
        &["", "you've ", "you have "],
        &[
            "hit your session limit",
            "hit your usage limit",
            "hit your weekly limit",
        ],
    ),
    (
        &["", "claude ", "claude code "],
        &[
            "session limit reached",
            "weekly limit reached",
            "usage limit reached",
        ],
    ),
    (
        &["you've ", "youve "],
        &[
            "reached your session limit",
            "reached your usage limit",
            "reached your weekly limit",
        ],
    ),
    (
        &["", "stop and "],
        &["wait for limit to reset", "wait for the limit to reset"],
    ),
];

/// 現行 `LOGIN_SCREEN_PROMPTS`。
const LOGIN_PROMPTS: Prompts = &[(&[""], &["select login method:"])];

/// workspace-trust。**行頭アンカー付き**(現行は素の部分一致)。文面は 2026-08-02 に
/// 実機で撮った実物: ` ❯ 1. Yes, I trust this folder` → 飾りを落として `yes, i trust…`。
const TRUST_PROMPTS: Prompts = &[(&[""], &["yes, i trust this folder"])];

/// 行頭の飾りを落とす。現行 `MODAL_LINE_DECORATION` と同じ順(飾り → 選択肢番号 → 空白)。
fn strip_modal_decoration(line: &str) -> &str {
    let t = line.trim_start_matches(MODAL_DECORATION);
    let digits = t.len() - t.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return t;
    }
    match t[digits..]
        .strip_prefix('.')
        .or_else(|| t[digits..].strip_prefix(')'))
    {
        Some(rest) => rest.trim_start(),
        None => t,
    }
}

/// 「画面が**自分で**このプロンプトを印字しているか」。現行
/// `paneShowsPrompt` の移植 — **行頭アンカーが全部**。同じ語が文の途中にあるもの
/// (このリポのソース・grep の結果・引用された Slack の発言)は画面ではない。
fn shows_prompt(pane: &str, prompts: Prompts) -> bool {
    pane.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        let body = strip_modal_decoration(&lower);
        prompts.iter().any(|(prefixes, stems)| {
            prefixes.iter().any(|p| {
                body.strip_prefix(p)
                    .is_some_and(|rest| stems.iter().any(|s| rest.starts_with(s)))
            })
        })
    })
}

/// claude の画面(`capture-pane` の出力)と、headless 実行の出力。読むだけ。
///
/// TUI は日付が変わるとドリフトすることがある(実機で確定した2件)。
/// **ここのメソッドがそのドリフトを吸収する唯一の場所** — 見張っているのは下の `mod tests`。
pub struct Pane<'a>(&'a str);

impl<'a> Pane<'a> {
    pub fn new(text: &'a str) -> Self {
        Pane(text)
    }

    /// `claude -p "/context"` の Markdown をパース。前置きの警告文(「workspace not trusted」など)は
    /// 素通しし、Model/Tokens ヘッダが無ければ None — 呼び手は文字化けを投稿する代わりに
    /// 「読めなかった」と言える。
    /// 起動直後の窓に何が出ているか。現行 `classifySpawnPane`。
    ///
    /// **アカウントの拒絶を先に見る** — サインイン画面や上限モーダルは他の文字と同居できるし、
    /// 「遅い確認プロンプト」と取り違えて Enter を撃っても消えない(現行)。
    pub fn spawn_screen(&self) -> SpawnScreen {
        if self.0.is_empty() {
            return SpawnScreen::None_;
        }
        if shows_prompt(self.0, LOGIN_PROMPTS) {
            return SpawnScreen::LoginRequired;
        }
        if shows_prompt(self.0, LIMIT_PROMPTS) {
            return SpawnScreen::UsageLimited;
        }
        if shows_prompt(self.0, TRUST_PROMPTS) {
            return SpawnScreen::Trust;
        }
        // ここだけ素の部分一致(現行)。実物のキャプチャが無いので、
        // 飾りの形を推測してアンカーを掛けると**本物を取り逃がす**方が痛い
        if self.0.to_ascii_lowercase().contains("local development") {
            return SpawnScreen::Confirm;
        }
        SpawnScreen::None_
    }

    pub fn context_report(&self) -> Option<ContextReport> {
        let raw = self.0;
        let model = header_value(raw, "**Model:**")?;
        let (used, total, pct) = tokens_header(raw)?;
        let mut categories = Vec::new();
        let mut in_table = false;
        for line in raw.split('\n') {
            let t = line.trim();
            // `/^###\s+Estimated usage by category/i`
            if t.strip_prefix("###").is_some_and(|r| {
                r.starts_with(char::is_whitespace)
                    && starts_with_ci(r.trim_start(), "Estimated usage by category")
            }) {
                in_table = true;
                continue;
            }
            if !in_table {
                continue;
            }
            if t.starts_with('#') {
                break; // 次の見出しが表の終わり
            }
            if !t.starts_with('|') {
                if !categories.is_empty() {
                    break;
                }
                continue;
            }
            // `split('|')` の両端(表の縦罫の外側)を落とす
            let parts: Vec<&str> = t.split('|').map(str::trim).collect();
            let cells = parts.get(1..parts.len() - 1).unwrap_or(&[]);
            let [name, tokens, pct, ..] = cells else {
                continue;
            };
            if name.eq_ignore_ascii_case("category") {
                continue; // ヘッダ行
            }
            if !name.is_empty() && name.chars().all(|c| c == '-') {
                continue; // markdown の区切り行
            }
            categories.push((name.to_string(), tokens.to_string(), pct.to_string()));
        }
        Some(ContextReport {
            model,
            used,
            total,
            pct,
            categories,
        })
    }

    /// `claude -p "/usage"` の平文をパース。拾うのは `<label>: <n>% used [· resets <when>]` の
    /// 上限行だけで、後ろに続く「What's contributing…」の内訳は無視する。`· resets <when>` は
    /// **任意** — 0%(リセット直後の Current session など)では印字されないが、その行も残す
    /// (落としていたのが修正)。1本も無ければ None。
    pub fn usage_rows(&self) -> Option<Vec<UsageRow>> {
        let rows: Vec<UsageRow> = self.0.split('\n').filter_map(parse_usage_line).collect();
        (!rows.is_empty()).then_some(rows)
    }

    /// モーダルが**キーボードを持っている**印。Claude Code のダイアログは種類を問わず
    /// 取り消しの案内を足元に出す(2026-08-18 実物: auto mode オンボーディングの
    /// `Enter to confirm · Esc to cancel` / `/model` の
    /// `Enter to set as default · s to use this session only · Esc to cancel`)。
    ///
    /// 走行中のターンは `esc to interrupt` で**別の語** — 動いているワーカーを詰まりと
    /// 読み違えない。判定は種類を問わない: 名前を知らないモーダルこそが沈黙の正体なので、
    /// 文言表で当てにいかない。
    pub fn modal_footer(&self) -> Option<&'a str> {
        self.0
            .split('\n')
            .find(|l| l.to_ascii_lowercase().contains("esc to cancel"))
    }

    /// TUI の生きた入力ボックス = pane の**最後の** `❯` 行。**送信済み**のコマンドも
    /// echo された `❯ /effort high` として画面に残り、打鍵途中の行と形が同じ —
    /// 位置だけが両者を分ける。
    pub fn input_line(&self) -> &str {
        self.0
            .split('\n')
            .rfind(|l| l.trim_start().starts_with('❯'))
            .unwrap_or("")
    }

    /// `cmd`(例 `/effort`)が**未送信**のまま入力ボックスに居る間だけ true。pane 全体に訊くと
    /// 上記の echo のせいで永遠に yes になる。
    pub fn still_has_command(&self, cmd: &str) -> bool {
        self.input_line().contains(cmd)
    }

    /// 入力ボックスが空 = **送信された**。空でも `❯` の行そのものは残る(後ろは半角空白では
    /// なく U+00A0 — `trim` は White_Space なのでどちらも落ちる)。長い本文は箱の中で
    /// 折り返され、`❯` の行には**その時見えている先頭行**が乗る。中身が何であれ
    /// 「空でない = まだ送られていない」は同じ。
    ///
    /// `❯` の行が1本も無いとき(許可プロンプトが箱を覆っている / 画面が読めない)も空を返す —
    /// 見えていないものに Enter を撃たないため。
    pub fn input_box_empty(&self) -> bool {
        self.input_line()
            .trim_start()
            .trim_start_matches('❯')
            .trim()
            .is_empty()
    }

    /// `/effort <level>` が効いた時に出る行。どれも level を名指しする:
    /// `Set effort level to high (saved as your default …)`、auto の `Effort level set to auto`
    /// (ここまでregex を文字列走査に)、そして**同じ level を選び直した時**の
    /// `Kept effort level as xhigh`。
    ///
    /// **原文からの意図的差分**: 3つ目の `Kept effort level as` は 2026-07-29 実機で確認した文言
    /// (`model` の `Kept model as` と対)。Bun の移植元は 2026-07-17 時点の TUI で、これを取り
    /// こぼして毎回タイムアウトしていた。
    pub fn effort_confirmed(&self, level: &str) -> bool {
        let pane = self.0;
        [
            "set effort level to",
            "effort level set to",
            "kept effort level as",
        ]
        .iter()
        .any(|marker| {
            match_indices_ci(pane, marker).into_iter().any(|i| {
                let rest = &pane[i + marker.len()..];
                let t = rest.trim_start();
                // `\s+` は1文字以上 / level の後ろは `\b`(語が続くなら別の語 — `highest`)
                t.len() < rest.len()
                    && starts_with_ci(t, level)
                    && !t[level.len()..].starts_with(is_word)
            })
        })
    }

    /// `/model <名前>` が効いた時に出る行。モデルは**フレンドリ名**で名乗り、頼んだ alias を
    /// 含む(`opus` → "Set model to Opus 4.8 …")。既にそのモデルなら "Kept model as Opus 4.8"
    /// になるが、どちらも「頼んだモデルに居る」という答え。
    pub fn model_confirmed(&self, name: &str) -> bool {
        let pane = self.0;
        let name = name.to_ascii_lowercase();
        ["set model to", "kept model as"].iter().any(|marker| {
            match_indices_ci(pane, marker).into_iter().any(|i| {
                let rest = &pane[i + marker.len()..];
                // marker の後ろの `\b` と、`[^\n]*` — 同一行の中だけを見る
                !rest.starts_with(is_word) && {
                    let line = rest.split('\n').next().unwrap_or("");
                    // name の手前は `\b`(`myopus` は opus ではない)
                    match_indices_ci(line, &name)
                        .into_iter()
                        .any(|j| !line[..j].ends_with(is_word))
                }
            })
        })
    }

    /// pane に `/effort` のスライダが出ているか(フッタのヒント行が安定した目印)。
    pub fn effort_slider_open(&self) -> bool {
        self.0.contains("←/→ to adjust")
    }

    /// **履歴のある**セッションで `/effort <level>` を出すと、現行 TUI は確定の前に確認ダイアログを
    /// 挟む(2026-07-29 実機。キャッシュが効かなくなることの断り):
    /// ```text
    /// Change effort level?
    /// This conversation is cached for the current effort level. Switching to xhigh …
    /// ❯ 1. Yes, switch to xhigh
    ///   2. No, go back
    /// ```
    /// 見出しだけを目印にする — 選択肢の行は level を含んで揺れる。**移植元(2026-07-17 時点の
    /// TUI)にはダイアログが無く**、Bun 版はこの経路を知らない。
    pub fn effort_confirm_dialog_open(&self) -> bool {
        self.0.contains("Change effort level?")
    }

    /// `/effort` のスライダを Escape で閉じた後に TUI が出す状態行から、今の effort level を読む
    /// (入力ボックスの上の右寄せ行。両方の文言を実測):
    ///     ● high · /effort            (○ のこともある — 点は変わる)
    ///     ✦ ultracode · xhigh effort + dynamic workflows for maximum thoroughness
    /// 画面に無ければ None(まだ出ていない / 別の通知に置き換わった)。
    ///
    /// **原文からの意図的差分**: `◉`(U+25C9)を追加 — 2026-07-29 の実機は `◉ xhigh · /effort`。
    /// Bun の `●○✦` は 2026-07-17 時点の TUI で、点は今後も変わり得る(足すのはここだけ)。
    pub fn effort_status(&self) -> Option<&'static str> {
        let pane = self.0;
        for (i, glyph) in pane
            .char_indices()
            .filter(|(_, c)| matches!(c, '●' | '○' | '✦' | '◉'))
        {
            let rest = pane[i + glyph.len_utf8()..].trim_start();
            for lv in STATUS_LEVELS {
                if rest
                    .strip_prefix(lv)
                    .is_some_and(|r| r.trim_start().starts_with('·'))
                {
                    return Some(lv);
                }
            }
        }
        None
    }

    /// 入力欄の**下**のフッタが名乗る権限モード。行が無ければ `manual`(既定は名乗らない)。
    ///
    /// 見るのを入力欄より下に限るのは、会話の本文に同じ語(この機能の話をした transcript)が
    /// 出ても拾わないため — `input_line` と同じ錨。
    pub fn mode_status(&self) -> &'static str {
        let foot = match self.0.rfind('❯') {
            Some(i) => &self.0[i..],
            None => self.0,
        };
        MODE_LABELS
            .iter()
            .find(|(label, _)| foot.contains(label))
            .map(|(_, name)| *name)
            .unwrap_or("manual")
    }

    /// 捕った pane をパースする。錨は生きた圧縮スピナー — `Compacting conversation` を含む行 — で、
    /// そこから読むのは:
    ///   • その行の括弧の経過タイマー `(33s)` / `(1m 4s · …)`(まだ出ていれば);
    ///   • 直下のバー行の完了率(スピナー行 + 次の2行に**限る**ので、遠くの status line の
    ///     `ctx:NN%` を拾うことは無い)。スピナー行から見るので、% が同じ行にある版も拾える。
    /// 生きたスピナーは必ずどちらかを出す。単なる静的言及(コード・コメント・この機能を引用した
    /// transcript)はどちらも持たないので、進行中とは取り違えない(初版が数分ハングしたバグ)。
    pub fn compact_progress(&self) -> CompactProgress {
        let lines: Vec<&str> = self.0.split('\n').collect();
        let Some(idx) = lines
            .iter()
            .position(|l| !match_indices_ci(l, "compacting conversation").is_empty())
        else {
            return CompactProgress::default();
        };
        let spinner = lines[idx];
        let seconds = parse_elapsed(spinner);
        let tok = parse_tokens(spinner);
        let percent = lines[idx..(idx + 3).min(lines.len())]
            .iter()
            .filter(|l| !l.is_empty() && match_indices_ci(l, "ctx:").is_empty())
            .find_map(|l| parse_percent_line(l).filter(|v| *v <= 100));
        // 生きた進捗の合図が1つも無い → ただ言葉が出てくる静的なテキスト
        if seconds.is_none() && percent.is_none() {
            return CompactProgress::default();
        }
        CompactProgress {
            active: true,
            seconds,
            tokens: tok.as_ref().map(|(_, n)| n.clone()),
            tokens_dir: tok.map(|(d, _)| d),
            percent: percent.map(|v| v as u8),
        }
    }

    /// `claude auth login` の pane から `https://claude.com/cai/oauth/authorize?…` を取り出す。
    /// URL は `visit: ` の後に印字される。**広い** pane(driver は `-x 400`)なら1行に収まるが、
    /// 狭い pane が折り返した場合、折り返しの続き行は空白を含まないので再結合する(空行・
    /// 「Paste code」のような空白入りの行・末尾で止める)。無ければ None。
    pub fn auth_login_url(&self) -> Option<String> {
        const MARKER: &str = "visit: ";
        const URL: &str = "https://claude.com/cai/oauth/authorize?";
        let pane = self.0;
        let idx = pane.find(MARKER)?;
        let mut lines = pane[idx + MARKER.len()..].split('\n');
        let mut joined = lines.next().unwrap_or("").trim().to_string();
        for seg in lines {
            let seg = seg.trim();
            if seg.is_empty() || seg.contains(char::is_whitespace) {
                break; // 空行 = URL の終わり / 空白入りは本文の行(「Paste code」)で折り返しではない
            }
            joined.push_str(seg); // soft-wrap の続き: 再結合
        }
        let i = joined.find(URL)?;
        let url: String = joined[i..]
            .chars()
            .take_while(|c| !c.is_whitespace())
            .collect();
        (url.len() > URL.len()).then_some(url) // `\S+` は1文字以上
    }

    /// `claude auth login` の実際の目印から pane を分類する(成功・失敗の実出力で確認):
    ///   success → "Login successful."(「Paste code here …」の後にインラインで出る)
    ///   invalid → "Invalid code. Please make sure the full code was copied."
    /// success を名乗るには**明示の成功マーカーが必須** — 悪いコード(やクラッシュ・中断)が
    /// 成功に見えることは無い — ので、Owner が縛られるのは本物のサインインの時だけ。
    /// pending = まだ待っている。
    pub fn login_outcome(&self) -> LoginOutcome {
        let low = self.0.to_ascii_lowercase();
        if low.contains("login successful") {
            return LoginOutcome::Success;
        }
        // `\berror\b` だけは語境界つき(`errorless` や識別子の中は拾わない)
        let bare_error = low
            .match_indices("error")
            .any(|(i, m)| !low[..i].ends_with(is_word) && !low[i + m.len()..].starts_with(is_word));
        if bare_error || LOGIN_ERROR_MARKERS.iter().any(|m| low.contains(m)) {
            LoginOutcome::Error
        } else {
            LoginOutcome::Pending
        }
    }

    /// transcript の末尾から、**最後の** assistant レコードが名乗ったモデル id を拾う
    /// (`/"model"\s*:\s*"(claude-[^"]+)"/g` を文字列走査に)。
    /// 見るのは `claude-` 始まりだけなので、エラーレコードの `"model":"<synthetic>"` は
    /// 素通りする。末尾を途中から切って渡してよい(半端な行は形が合わず無視される)。
    pub fn last_model_id(&self) -> Option<String> {
        let tail = self.0;
        let mut last = None;
        for (i, m) in tail.match_indices("\"model\"") {
            let rest = tail[i + m.len()..].trim_start();
            let Some(rest) = rest.strip_prefix(':').map(str::trim_start) else {
                continue;
            };
            let Some(rest) = rest.strip_prefix("\"claude-") else {
                continue;
            };
            // `[^"]+` — `claude-` の後ろが空なら id ではない
            let Some(end) = rest.find('"').filter(|e| *e > 0) else {
                continue;
            };
            last = Some(format!("claude-{}", &rest[..end]));
        }
        last
    }
}

/// `[...]` を全部落として trim(`id.replace(/\[[^\]]*\]/g, '').trim()` 相当)。
/// 閉じない `[` は括弧でない — そのまま残す。
fn strip_brackets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('[') {
        let Some(j) = rest[i + 1..].find(']') else {
            break;
        };
        out.push_str(&rest[..i]);
        rest = &rest[i + 1 + j + 1..];
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// id の `[<n>m]` サフィックスから窓の大きさ(`"1M"`)。無ければ None。`/\[(\d+)\s*m\]/i`
fn id_context_suffix(id: &str) -> Option<String> {
    for (i, _) in id.match_indices('[') {
        let s = &id[i + 1..];
        let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            continue;
        }
        let r = s[digits.len()..].trim_start();
        if matches!(r.chars().next(), Some('m' | 'M')) && r[1..].starts_with(']') {
            return Some(format!("{digits}M"));
        }
    }
    None
}

fn capitalize(s: &str) -> String {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) => c.to_uppercase().collect::<String>() + cs.as_str(),
        None => String::new(),
    }
}

/// claude のモデル識別子(`claude-opus-4-8[1m]` のような**フルの id**)。
pub struct ModelId<'a>(&'a str);

impl<'a> ModelId<'a> {
    pub fn new(id: &'a str) -> Self {
        ModelId(id)
    }

    /// フルのモデル id に含まれる親しみ名(`claude-fable-5` → `fable`)。既知の名前を
    /// どれも含まなければ None。
    pub fn alias(&self) -> Option<&'static str> {
        let lower = self.0.to_lowercase();
        MODEL_NAMES.iter().find(|n| lower.contains(**n)).copied()
    }

    /// 見出し用に人が読めるモデル名: `("claude-opus-4-8", "1m")` → `Opus 4.8（1M context）`。
    /// 窓の注記は **Tokens の total** から採る(id の `[1m]` サフィックスではなく) — このサフィックスは
    /// 1M でも出る版と出ない版があるのに対し total は必ずある。total が読めない形のときだけ
    /// サフィックスに落ちる。claude 以外の id は生のまま。
    pub fn friendly(&self, total: &str) -> String {
        let id = self.0;
        let t = total.trim();
        // `/^[\d.]+[mk]$/i` — 末尾が m/k で、その手前は数字とドットだけ
        let numeric_total = matches!(t.chars().next_back(), Some('m' | 'M' | 'k' | 'K'))
            && t.len() > 1
            && t[..t.len() - 1]
                .chars()
                .all(|c| c.is_ascii_digit() || c == '.');
        let ctx = if numeric_total {
            t.to_ascii_uppercase()
        } else {
            id_context_suffix(id).unwrap_or_default()
        };
        let note = if ctx.is_empty() {
            String::new()
        } else {
            format!("（{ctx} context）")
        };
        let base = strip_brackets(id);
        let parts: Vec<&str> = base.split('-').collect();
        let label = match parts.split_first() {
            Some((&"claude", tail)) if !tail.is_empty() => {
                let family = capitalize(tail[0]);
                let ver = tail[1..].join(".");
                if ver.is_empty() {
                    family
                } else {
                    format!("{family} {ver}")
                }
            }
            _ => base,
        };
        format!("{label}{note}")
    }
}

/// ワーカーの `.jsonl` と、そこまで読んだ位置。
///
/// hook payload が毎回パスを運んでくるので、ファイルが移動しても自動で追従する(ファイルを探し回る処理は要らない)。
pub struct Transcript {
    path: String,
    /// 前回どこまで読んだか。`new_lines` が進める。
    offset: u64,
}

impl Transcript {
    /// hook が運んできたパスを第一候補に、無ければ session id から総当たりで探す
    /// (resume / model / status が同じ解決順を使う)。**外から開く道はこれだけ。**
    pub fn locate(remembered: Option<&str>, session_id: &str) -> Option<Self> {
        remembered
            .filter(|p| std::path::Path::new(p).is_file())
            .map(|p| Self::at(p.to_string()))
            .or_else(|| Self::find(session_id))
    }

    /// 前回位置を引き継いで開く。offset は台帳(`Hooked`)が持っているので、
    /// 続きを読む側はそれを預けて開く。
    pub fn at_offset(path: String, offset: u64) -> Self {
        Self { path, offset }
    }

    fn at(path: String) -> Self {
        Self { path, offset: 0 }
    }

    /// `~/.claude/projects/<プロジェクト>/<sid>.jsonl` の総当たり探索。
    /// プロジェクトの階層は1段だけなので readdir 2回で足りる。
    fn find(sid: &str) -> Option<Self> {
        let root = std::path::Path::new(&std::env::var("HOME").ok()?).join(".claude/projects");
        for e in std::fs::read_dir(root).ok()? {
            let Ok(e) = e else { continue };
            let p = e.path().join(format!("{sid}.jsonl"));
            if p.is_file() {
                return Some(Self::at(p.to_string_lossy().into_owned()));
            }
        }
        None
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// `offset` 以降の**完全な行だけ**を読み、offset を進める。半端な行は次回に回す
    /// (途中で切れた id を読み落とさないため)。読めないファイルは debug で握る — 受信確認は
    /// 観測シグナルであって台帳ではない。
    pub fn new_lines(&mut self, ctx: &LogCtx) -> Option<String> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&self.path)
            .inspect_err(|e| ctx.debug("bridge", &format!("transcript open failed: {e}")))
            .ok()?;
        // 短くなっていたら別物に置き換わっている — 頭から読み直す
        let start = if f.metadata().ok()?.len() < self.offset {
            0
        } else {
            self.offset
        };
        let mut buf = Vec::new();
        f.seek(SeekFrom::Start(start)).ok()?;
        f.read_to_end(&mut buf).ok()?;
        let end = buf.iter().rposition(|&b| b == b'\n')? + 1;
        self.offset = start + end as u64;
        Some(String::from_utf8_lossy(&buf[..end]).into_owned())
    }

    /// **先頭行**が持つ `cwd` = ワーカーが実際に立っていた場所。先頭行だけ読むのは
    /// この JSONL が数 MB になるから(main ループを止めない)。
    pub fn cwd(&self) -> Option<String> {
        use std::io::BufRead;
        let mut line = String::new();
        std::io::BufReader::new(std::fs::File::open(&self.path).ok()?)
            .read_line(&mut line)
            .ok()?;
        serde_json::from_str::<serde_json::Value>(&line).ok()?["cwd"]
            .as_str()
            .map(str::to_string)
    }

    /// 更新時刻(epoch ms)。読めなければ None。
    pub fn mtime_ms(&self) -> Option<u64> {
        std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_millis() as u64)
    }

    /// 末尾 `n` バイトが名乗る**最後の**モデル id。`Ok(None)` = 読めたが id が無い。
    pub fn model_id(&self, n: u64) -> std::io::Result<Option<String>> {
        Ok(Pane::new(&self.tail(n)?).last_model_id())
    }

    /// 末尾 `n` バイトが名乗る**最後の**上限エラー(`limitErrorForSession`)。
    /// 呼ばれるのは「ターンが既に失敗した」稀な道だけなので、末尾読み1回で足りる。
    pub fn limit_error(
        &self,
        n: u64,
        now_ms: u64,
    ) -> std::io::Result<Option<crate::bridge::command::LimitHit>> {
        Ok(crate::bridge::command::LimitHit::in_transcript_tail(
            &self.tail(n)?,
            now_ms,
        ))
    }

    /// ファイル末尾 `n` バイト。長いセッションの transcript は数十 MB あり、
    /// 答えはいつも末尾にある。
    fn tail(&self, n: u64) -> std::io::Result<String> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&self.path)?;
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(len.saturating_sub(n)))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
}

// ─── hook の受け口 ─────────────────────────────────────────────────────────

use crate::agent::HookEvent;
use axum::{
    Router, extract::State, http::HeaderMap, http::StatusCode, http::header::CONTENT_TYPE,
    routing::post,
};
use std::path::PathBuf;
use tokio::sync::mpsc;

/// hook の口が axum に持たせる共有状態。
///
/// `token` は起動ごとに切る合言葉で、ワーカー以外からの POST を弾く(口は 127.0.0.1 に閉じているが、同じマシンの
/// 別プロセスは叩けてしまうため)。
#[derive(Clone)]
struct HookState {
    tx: mpsc::Sender<HookEvent>,
    token: String,
}

/// claude の hook を受ける HTTP の口。
///
/// **UserPromptSubmit はターン中のステアリング消費では発火しない**ので、受信確認は
/// payload の `transcript_path` を使った transcript スキャン併用で成り立っている。
/// ここを触るときはその前提を壊さない。
pub struct HookIntake;

impl HookIntake {
    /// 答えを待つ hook の上限(現行)。
    /// perm は Slack の人間を待つので長い。stop はターン終了を吊らせないので短い。
    fn decision_cap(kind: &str) -> Option<Duration> {
        match kind {
            "perm" => Some(Duration::from_secs(120)),
            "stop" => Some(Duration::from_secs(5)),
            _ => None,
        }
    }

    /// 受け口を上げ、`(port, token)` を返す。ポートは記憶して再利用する —
    /// ワーカーは URL を焼き込むので Bridge 再起動をまたぐ。
    pub async fn serve(
        state_dir: &StateDir,
        tx: mpsc::Sender<HookEvent>,
    ) -> std::io::Result<(u16, String)> {
        let port = state_dir.remembered_port("hook", StateDir::free_port);
        let token = state_dir.remembered_token("hook");
        let app = Router::new()
            .route("/hook/{kind}", post(Self::on_hook))
            .with_state(HookState {
                tx,
                token: token.clone(),
            });
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                LogCtx::default().error("hooks", &format!("hook endpoint stopped: {e}"));
            }
        });
        LogCtx::default().info("hooks", &format!("hook endpoint on 127.0.0.1:{port}"));
        Ok((port, token))
    }

    /// 現行 bridge/hook-endpoint.ts の buildWorkerHooksSettings を移植。
    pub fn settings_json(port: u16, token: &str) -> serde_json::Value {
        let base = format!("http://127.0.0.1:{port}/hook");
        let http_ = |kind: &str, timeout: u32| {
            serde_json::json!([{ "matcher": "", "hooks": [{
                "type": "http",
                "url": format!("{base}/{kind}"),
                "timeout": timeout,
                // Claude Code は宣言した変数だけ ${VAR} を展開する
                "headers": { "x-agentgw-token": token, "x-agentgw-session": "${AGENTGW_SESSION_ID}" },
                "allowedEnvVars": ["AGENTGW_SESSION_ID"],
            }]}])
        };
        serde_json::json!({
            // ワーカーは Slack 駆動のセッション。--settings は1つしか効かないのでここに同居させる。
            "disableRemoteControl": true,
            "hooks": {
                // SessionStart だけ Claude Code が http を無視するので curl。ヘッダは運ばれない
                // ので session_id は body から取る(スパイク実測)。
                "SessionStart": [{ "matcher": "", "hooks": [{
                    "type": "command",
                    "timeout": 10,
                    "command": format!(
                        "/usr/bin/curl -sS --max-time 5 -X POST -H 'content-type: application/json' \
                         -H 'x-agentgw-token: {token}' --data-binary @- {base}/session_start || true"
                    ),
                }]}],
                "UserPromptSubmit": http_("user_prompt", 5),
                // 報告系は短い timeout — 遅い Bridge がワーカーのターンを止めないため。
                "PreToolUse": http_("progress", 3),
                "PostToolUse": http_("progress", 3),
                "MessageDisplay": http_("narration", 3),
                // ターン失敗の理由(`error_type`)。**決定 hook ではない** — 報告を受けて
                // 人に伝えるだけなので即 `{}` を返す。
                "StopFailure": http_("error", 5),
                // SessionEnd は既定 1500ms しか貰えない。30s 宣言で中断枠も広げる。
                "SessionEnd": http_("session_end", 30),
                // 答えなければならない2つ。perm は Slack の人間を待ち、stop はターン終了を
                // 絶対に吊らせない。125s は Bridge 側の 120s 待ちより少し広く取る
                "PermissionRequest": http_("perm", 125),
                "Stop": http_("stop", 15),
            }
        })
    }

    pub fn write_settings(dir: &StateDir, port: u16, token: &str) -> std::io::Result<PathBuf> {
        // 生成物なので state ではなく一時領域へ([`StateDir::runtime_dir`])
        dir.write_runtime_json("worker-hooks.json", &Self::settings_json(port, token))
    }

    /// hook 応答は本文が JSON — 現行のsend() が全応答に付けている。
    fn json_body(body: String) -> impl axum::response::IntoResponse {
        ([(CONTENT_TYPE, "application/json")], body)
    }

    /// 受け手に投げて答えを待つ。答えが無い・遅い・受け手が落ちた、はすべて `{}`(= 辞退)。
    async fn decide_or_default(
        tx: &mpsc::Sender<HookEvent>,
        mut ev: HookEvent,
        cap: Duration,
    ) -> String {
        let (respond, answer) = tokio::sync::oneshot::channel();
        ev.respond = Some(respond);
        if tx.send(ev).await.is_err() {
            return "{}".into();
        }
        match tokio::time::timeout(cap, answer).await {
            Ok(Ok(v)) => v.to_string(),
            _ => "{}".into(),
        }
    }

    async fn on_hook(
        State(st): State<HookState>,
        axum::extract::Path(kind): axum::extract::Path<String>,
        headers: HeaderMap,
        body: String,
    ) -> Result<impl axum::response::IntoResponse, StatusCode> {
        if headers.get("x-agentgw-token").and_then(|v| v.to_str().ok()) != Some(st.token.as_str()) {
            LogCtx::default().info("hooks", &format!("rejected {kind}: bad token"));
            return Err(StatusCode::UNAUTHORIZED);
        }
        let payload: serde_json::Value =
            serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        // SessionStart は curl なのでヘッダを運ばない — body の session_id を使う(スパイク実測)
        let session_id = headers
            .get("x-agentgw-session")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .or_else(|| payload["session_id"].as_str().map(str::to_string))
            .unwrap_or_default();
        if session_id.is_empty() {
            LogCtx::default().info("hooks", &format!("{kind} without session id — ignored"));
            return Ok(Self::json_body("{}".into()));
        }
        let ctx = LogCtx {
            session_id: Some(session_id.clone()),
            thread_key: None,
        };
        ctx.debug("hooks", &format!("hook {kind}"));
        let ev = HookEvent {
            kind,
            session_id,
            payload,
            respond: None,
        };
        // perm と stop は答えが要る。他は報告なので即 `{}` を返し、ターンを待たせない。
        if let Some(cap) = Self::decision_cap(&ev.kind) {
            return Ok(Self::json_body(
                Self::decide_or_default(&st.tx, ev, cap).await,
            ));
        }
        if let Err(e) = st.tx.send(ev).await {
            ctx.error("hooks", &format!("hook queue closed: {e}"));
        }
        Ok(Self::json_body("{}".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::SessionId;

    fn claude_with(
        run: impl Fn(&[&str]) -> Result<String, String> + Send + Sync + 'static,
    ) -> Claude {
        Claude::new(Tmux { run: Box::new(run) })
    }

    /// 起動行とプロンプトは tmux を叩かない — 黙って成功する fake で十分。
    fn quiet() -> Claude {
        claude_with(|_| Ok(String::new()))
    }

    fn req(session_id: &str, prompt: Option<&str>, state: WorkerState) -> SpawnReq {
        SpawnReq {
            session_id: SessionId::from(session_id.to_string()),
            cwd: "/repo".to_string(),
            prompt: prompt.map(str::to_string),
            resume_from: None,
            window: "1-1".to_string(),
            state,
            hooks_file: "/st/h.json".to_string(),
            mcp_config: "/st/m.json".to_string(),
        }
    }

    #[test]
    fn pool_spawn_uses_startup_prompt_not_thread_envelope() {
        let p = quiet().pool_prompt();
        assert_eq!(p, WORKER_STARTUP_PROMPT);
        assert!(!p.is_empty());
        // 継続行の結合部が二重空白/無空白になっていないこと(原文の一字一句性)
        assert!(p.contains("right now — wait for messages to be pushed to you and reply"));
        assert!(!p.contains("  "));
    }

    #[test]
    fn launch_line_snapshot() {
        let line = quiet().launch_line(
            &SessionMode::New("sid-1".into()),
            "/st/worker-hooks.json",
            "/st/mcp/sid-1.json",
            "it's here",
        );
        assert_eq!(
            line,
            "AGENTGW_SESSION_ID=sid-1 claude --settings /st/worker-hooks.json \
             --mcp-config /st/mcp/sid-1.json --strict-mcp-config --session-id sid-1 \
             'it'\\''s here'"
        );
    }

    #[test]
    fn resume_uses_the_resume_flag() {
        let line = quiet().launch_line(
            &SessionMode::Resume("sid-9".into()),
            "/st/h.json",
            "/st/m.json",
            "hi",
        );
        assert!(line.contains("--resume sid-9"), "{line}");
        assert!(!line.contains("--session-id"), "{line}");
        assert!(line.starts_with("AGENTGW_SESSION_ID=sid-9 "), "{line}");
    }

    #[test]
    fn spawn_prompt_orients_by_mode() {
        let c = quiet();
        let env = "<channel …>x</channel>";
        let new = c.spawn_prompt(env, &SessionMode::New("s".into()));
        assert!(new.starts_with("This is a NEW Slack thread;"), "{new}");
        assert!(new.contains(env), "{new}");
        // prefix と封筒は空行1つで結合
        assert!(new.ends_with(&format!("\n\n{env}")), "{new}");
        let res = c.spawn_prompt(env, &SessionMode::Resume("s".into()));
        assert!(
            res.starts_with("You are RESUMING this Slack thread;"),
            "{res}"
        );
        assert!(res.contains(env), "{res}");
    }

    /// `SpawnReq` から組み上がる起動行は、旧 `src/worker.rs` の spawn が tmux に渡していた
    /// 文字列と同じ形。
    /// `resume_from` があれば `--resume`、無ければ `--session-id`(使い捨て ID の性質)。
    #[test]
    fn spawn_hands_tmux_the_launch_line() {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = lines.clone();
        let c = claude_with(move |args| {
            if args[0] == "new-window" || args[0] == "new-session" {
                sink.lock().unwrap().push(args[args.len() - 1].to_string());
            }
            Ok("@7\n".to_string())
        });
        let sp = c
            .spawn(&req(
                "sid-1",
                Some("<channel …>x</channel>"),
                WorkerState::Absent,
            ))
            .unwrap();
        assert_eq!(sp.as_str(), "@7");
        let got = lines.lock().unwrap()[0].clone();
        assert!(
            got.starts_with(
                "AGENTGW_SESSION_ID=sid-1 claude --settings /st/h.json --mcp-config /st/m.json \
                 --strict-mcp-config --session-id sid-1 'This is a NEW Slack thread;"
            ),
            "{got}"
        );
        assert!(got.ends_with("<channel …>x</channel>'"), "{got}");
        // 本文なし = プールの空焚き — 待受プロンプトで起動する
        let mut pool = req("sid-2", None, WorkerState::Absent);
        pool.resume_from = Some(SessionId::from("sid-2".to_string()));
        c.spawn(&pool).unwrap();
        let got = lines.lock().unwrap()[1].clone();
        assert_eq!(
            got,
            format!(
                "AGENTGW_SESSION_ID=sid-2 claude --settings /st/h.json --mcp-config /st/m.json \
                 --strict-mcp-config --resume sid-2 '{WORKER_STARTUP_PROMPT}'"
            )
        );
    }

    #[test]
    fn tmux_helpers_compose_correct_argv() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let c2 = calls.clone();
        let claude = claude_with(move |args| {
            c2.lock().unwrap().push(args.join(" "));
            Ok(String::new())
        });
        let tmux = claude.tmux();
        let w = Window::of("@5");
        tmux.capture(&w).unwrap();
        tmux.send_escape(&w).unwrap();
        tmux.send_command(&w, "/compact").unwrap();
        tmux.kill_window(&w).unwrap();
        claude.login_start("/home").unwrap();
        tmux.capture_history(&claude.login_window(), 200).unwrap();
        tmux.send_enter(&claude.login_window()).unwrap();
        claude.login_kill();
        let got = calls.lock().unwrap().clone();
        assert_eq!(got[0], "capture-pane -p -t @5");
        assert_eq!(got[1], "send-keys -t @5 Escape");
        assert_eq!(got[2], "send-keys -t @5 -l -- /compact");
        assert_eq!(got[3], "send-keys -t @5 Enter");
        assert_eq!(got[4], "kill-window -t @5");
        assert_eq!(
            got[5],
            "new-session -d -s slack-login-rs -x 400 -y 50 -c /home"
        );
        // 成功マーカーはシェルに戻った拍子にスクロールで消える — scrollback ごと読む
        assert_eq!(got[6], "capture-pane -p -S -200 -t slack-login-rs");
        assert_eq!(got[7], "send-keys -t slack-login-rs Enter");
        assert_eq!(got[8], "kill-session -t slack-login-rs");
    }

    #[test]
    fn login_session_kill_is_silent_when_there_is_no_session() {
        // has-session 相当の非0(= 無い)で騒がない — 起動時の掃除が毎回エラーを吐かないように
        claude_with(|_| Err("no such session".into())).login_kill();
    }

    #[test]
    fn spawn_refuses_when_the_seat_is_taken() {
        let err = quiet()
            .spawn(&req("sid", Some("hi"), WorkerState::Ready))
            .unwrap_err();
        assert!(
            err.contains("occupied"),
            "a spawn must never be a kill: {err}"
        );
    }

    // ── 画面と出力の読み取り ────────────────────────────────────────────────
    // ここから下は **TUI ドリフトの見張り**(実機で確定した2件)。
    // 1件も落とさないこと — 落とすと次のドリフトに気づけない。

    /// 旧 `command.rs` の同名 const(描く側のテストと同じ現物)。
    const CONTEXT_RAW: &str = "\
some preamble\n\n**Model:** claude-opus-4-8[1m]\n**Tokens:** 43.8k / 1m (4%)\n\n\
### Estimated usage by category\n\n| Category | Tokens | % |\n| --- | --- | --- |\n\
| System prompt | 3.2k | 0.3% |\n| Messages | 40.6k | 4.1% |\n| Free space | 956k | 95.6% |\n\n\
### Custom Agents\nignored\n";

    #[test]
    fn parses_context_output() {
        let r = Pane::new(CONTEXT_RAW).context_report().unwrap();
        assert_eq!(
            (
                r.model.as_str(),
                r.used.as_str(),
                r.total.as_str(),
                r.pct.as_str()
            ),
            ("claude-opus-4-8[1m]", "43.8k", "1m", "4%")
        );
        assert_eq!(r.categories.len(), 3);
        assert_eq!(r.categories[0].0, "System prompt");
        assert!(Pane::new("no headers here").context_report().is_none());
    }

    /// 1M 注記の出どころは **Tokens の total**(id の `[1m]` はある版と無い版がある)。
    #[test]
    fn context_window_note_comes_from_total() {
        assert_eq!(
            ModelId::new("claude-opus-4-8").friendly("1m"),
            "Opus 4.8（1M context）"
        );
        // total が読めないときだけ id の `[<n>m]` サフィックスに落ちる
        assert_eq!(
            ModelId::new("claude-opus-4-8[1m]").friendly("-"),
            "Opus 4.8（1M context）"
        );
        assert_eq!(
            ModelId::new("claude-haiku").friendly("200k"),
            "Haiku（200K context）"
        );
        assert_eq!(ModelId::new("gpt-4").friendly(""), "gpt-4"); // claude でなければ生の id
    }

    /// `model_id` と同じ「末尾 n バイトを読んで純関数に渡す」形であること。
    #[test]
    fn transcript_limit_error_reads_the_tail() {
        let dir = std::env::temp_dir().join(format!("scr-limit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let rec = serde_json::json!({
            "isApiErrorMessage": true,
            "timestamp": "2026-07-30T05:00:00.000Z",
            "message": { "content": [{ "text": "Claude usage limit reached. resets at 11pm" }] },
        });
        std::fs::write(&path, format!("{rec}\n")).unwrap();
        let t = Transcript::at_offset(path.to_string_lossy().into_owned(), 0);
        let now = 1_785_387_600_000u64; // 2026-07-30T05:00:00Z = 30日 14:00 JST
        let hit = t.limit_error(256 * 1024, now).unwrap().expect("limit hit");
        assert!(hit.detail.contains("usage limit reached"), "{hit:?}");
        assert_eq!(hit.reset_ms, 1_785_420_000_000); // 30日 23:00 JST
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 旧 `command::model_alias` / `command::last_model_id` の網
    /// (旧 `id_shapes_and_mentions` から分離 — 残りの id 形の網は `command.rs` に居る)。
    #[test]
    fn model_id_alias_and_transcript_tail() {
        assert_eq!(ModelId::new("claude-fable-5").alias().unwrap(), "fable");
        assert_eq!(ModelId::new("gpt-4").alias(), None);
        // 勝つのは**最後の** claude- id。`<synthetic>` はモデルではない
        let tail = "{\"model\":\"claude-opus-4-5\"}\n{\"model\" : \"claude-fable-5\"}\n\
                    {\"model\":\"<synthetic>\"}\n";
        assert_eq!(
            Pane::new(tail).last_model_id().as_deref(),
            Some("claude-fable-5")
        );
        assert_eq!(
            Pane::new("{\"model\":\"<synthetic>\"}").last_model_id(),
            None
        );
        assert_eq!(Pane::new("model claude-fable-5 の話").last_model_id(), None);
    }

    #[test]
    fn parses_usage_rows() {
        let raw = "\
Opening usage…\nCurrent session (all models): 12% used · resets Jul 29 at 5pm\n\
Current week (all models): 66% used · resets Aug 1 at 9am\nCurrent session: 0% used\n\
What's contributing to your limits\nignored: 55% something\n";
        let rows = Pane::new(raw).usage_rows().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].pct, "12");
        assert_eq!(rows[2].reset, ""); // 0% 行は resets 無しでも拾う
        assert!(Pane::new("garbage").usage_rows().is_none());
        // ラベルの遅延一致は**行を捨てず**次のコロンへ進む(bun で現物の出力を確認)
        let two = Pane::new("A: 12% used, X: 66% used").usage_rows().unwrap();
        assert_eq!(two.len(), 1);
        assert_eq!(
            (
                two[0].label.as_str(),
                two[0].pct.as_str(),
                two[0].reset.as_str()
            ),
            ("A: 12% used, X", "66", "")
        );
        // 2026-07 の実物(`claude -p /usage` をそのまま貼った): タイムゾーン付きの reset と、
        // `%` とコロンを含む「What's contributing」の内訳。上限行**だけ**拾うこと
        let live = "\
You are currently using your subscription to power your Claude Code usage\n\n\
Current session: 21% used · resets Jul 29 at 12:29am (Asia/Tokyo)\n\
Current week (all models): 42% used · resets Jul 29 at 4:59pm (Asia/Tokyo)\n\n\
What's contributing to your limits usage?\n\
Last 24h · 2120 requests · 21 sessions\n\
  98% of your usage came from subagent-heavy sessions\n\
  Top skills: /superpowers:writing-plans 2%, /claude-api 1%\n";
        let rows = Pane::new(live).usage_rows().unwrap();
        assert_eq!(
            rows.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(),
            ["Current session", "Current week (all models)"]
        );
        assert_eq!(rows[0].reset, "Jul 29 at 12:29am (Asia/Tokyo)"); // 行内の `12:29` で切らない
    }

    #[test]
    fn input_line_is_the_last_prompt() {
        let pane = "❯ /effort high\nSet effort level to high (saved)\n❯ ";
        assert_eq!(Pane::new(pane).input_line().trim(), "❯"); // 送信済み echo は入力行ではない
        assert!(!Pane::new(pane).still_has_command("/effort"));
        assert!(Pane::new("❯ /effort").still_has_command("/effort"));
    }

    #[test]
    fn an_empty_input_box_means_the_text_was_submitted() {
        // 空の箱の実物: `❯` の後ろは U+00A0(2026-08-02 実機の capture-pane)
        assert!(Pane::new("← U012ABC3DEF: やって\n❯ \u{a0}").input_box_empty());
        assert!(Pane::new("dialog with no prompt line").input_box_empty()); // `❯` が無い = 叩かない
        // 送信されずに残った本文。箱は折り返され、`❯` の行には見えている先頭行が乗る
        assert!(!Pane::new("❯ ==> 状態\n  </channel>\n").input_box_empty());

        // **罠の現物**: auto mode のモーダルは箱を生かしたまま前に出るので、ここは「空」=
        // 「送信された」と読める。この関数だけでは配達の可否を決められない
        // ([`Claude::not_accepting_keys`] が案内行で割る)。2026-08-18 に18分沈黙した形。
        let (_, onboarding, _) = REAL_MODALS[0];
        assert!(Pane::new(onboarding).input_box_empty());
        assert!(Pane::new(onboarding).modal_footer().is_some());
        // `/model` 型は逆に `❯` を選択カーソルに奪われるので「本文が残っている」に見える —
        // 押し直しの Enter がダイアログの確定になる側の罠
        let (_, selector, _) = REAL_MODALS[1];
        assert!(!Pane::new(selector).input_box_empty());
        assert!(Pane::new(selector).modal_footer().is_some());
    }

    /// **実物**の pane(2026-08-18、`slack-workers-rs` で採取)。飾りも折り返しもそのまま。
    /// ここを推測で書き換えないこと — 最初の修正が効かなかったのは、`❯` の振る舞いを
    /// 実物を見ずに決め打ちしたからだった。
    ///
    /// 1枚目: auto mode オンボーディング。**入力欄(`❯`)が生きたまま**モーダルが前に出る。
    /// 2枚目: `/model` セレクタ。**`❯` が選択カーソルに奪われる**(箱の行は消える)。
    const REAL_MODALS: &[(&str, &str, &str)] = &[
        (
            "auto mode onboarding",
            " Set up auto mode for your environment?\n\
             \n\
             Auto mode lets Claude act without asking first. Telling it which repos you trust\n\
             and what data is sensitive gives it clearer guardrails on what's safe to run.\n\
             \n\
             \u{276f} 1. Set it up\n\
             \u{a0} 2. Not now\n\
             \u{a0} 3. Don't show again\n\
             \n\
             Enter to confirm \u{b7} Esc to cancel\n\
             \n\
             Message #slack-multi-ch\n\
             \u{276f} \u{a0}\n\
             \u{a0} ctx:4%  15:08  Opus 5 (1M context)\n\
             \u{23f5}\u{23f5} auto mode on (shift+tab to cycle) \u{b7} \u{2190} 1 agent\n",
            "Set up auto mode for your environment?",
        ),
        (
            "/model selector",
            "   Select model\n\
             \n\
             \u{a0} 1. Default (recommended)  Opus 5 with 1M context\n\
             \u{276f} 2. Opus (1M context) \u{2714}    Opus 5 with 1M context\n\
             \u{a0} 3. Fable                  Fable 5\n\
             \n\
             \u{a0} \u{25cf} High effort (default) \u{2190}/\u{2192} to adjust\n\
             \n\
             Enter to set as default \u{b7} s to use this session only \u{b7} Esc to cancel\n",
            "Esc to cancel",
        ),
    ];

    /// 待機中の実物(同日、同じセッション)。`❯` の行は空で、案内行は出ていない。
    const IDLE_PANE: &str = "\u{2726} Cooked for 2s\n\
                             \n\
                             \u{2500}\u{2500}\u{2500}\u{2500}\n\
                             \u{276f} \u{a0}\n\
                             \u{2500}\u{2500}\u{2500}\u{2500}\n\
                             \u{a0} ~  ctx:4%  17:54  Opus 5 (1M context)\n\
                             \u{23f5}\u{23f5} auto mode on (shift+tab to cycle) \u{b7} \u{2190} 1 agent\n";

    /// 本文と Enter を送る fake。`capture-pane` には `panes` を頭から1枚ずつ返す。
    fn deliver_probe(
        panes: Vec<&'static str>,
    ) -> (std::sync::Arc<std::sync::Mutex<Vec<String>>>, Claude) {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = calls.clone();
        let shown = std::sync::Mutex::new(0usize);
        let c = claude_with(move |args| {
            sink.lock().unwrap().push(args.join(" "));
            if args[0] == "capture-pane" {
                let mut i = shown.lock().unwrap();
                let pane = panes[(*i).min(panes.len() - 1)];
                *i += 1;
                return Ok(pane.to_string());
            }
            Ok(String::new())
        });
        (calls, c)
    }

    fn enters(calls: &[String]) -> usize {
        calls.iter().filter(|c| c.ends_with(" Enter")).count()
    }

    #[test]
    fn deliver_presses_enter_again_while_the_box_still_holds_the_text() {
        // 4KB の封筒で Enter が取り込み中の TUI に飲まれた形(2026-08-02)。押し直しで通る
        // 1枚目は打つ前の在処確認が食う(箱はある = 打ってよい)
        let (calls, c) = deliver_probe(vec!["❯ \u{a0}", "❯ </channel>\n", "❯ \u{a0}"]);
        c.deliver(&Window::of("@42"), "hello").unwrap();
        assert_eq!(enters(&calls.lock().unwrap()), 2, "最初の1発 + 押し直し1回");
    }

    #[test]
    fn deliver_gives_up_loudly_instead_of_pressing_enter_forever() {
        let (calls, c) = deliver_probe(vec!["❯ </channel>\n"]); // 永遠に残ったまま
        let err = c.deliver(&Window::of("@42"), "hello").unwrap_err();
        assert!(err.contains("never submitted"), "{err}");
        assert_eq!(
            enters(&calls.lock().unwrap()),
            1 + DELIVER_SUBMIT_RETRIES as usize,
            "押し直しは上限で止まる"
        );
    }

    /// 2026-08-18 実機の詰まり。`❯` を覆うモーダルは `input_box_empty()` では**空**に見えるが、
    /// 打鍵はモーダルに食われて消えている。ここが `Ok` を返していた18分、Bridge は配達成功を
    /// 記録したまま2スレッドが沈黙した。
    ///
    /// **1文字も送らない**のが要 — 選択肢リストに本文を打つと、中の数字が選択、続く Enter が
    /// 確定になる。tick が押し直す(`Bridge::retry_pending`)ので、ここが漏れると人が答える前に
    /// 勝手に選ばれる。
    #[test]
    fn deliver_refuses_to_call_a_covered_input_box_a_delivery() {
        for (label, modal, want) in REAL_MODALS {
            let (calls, c) = deliver_probe(vec![modal]);
            let err = c.deliver(&Window::of("@42"), "hello").unwrap_err();
            assert!(err.contains("a dialog has the keyboard"), "{label}: {err}");
            // 何が止めているかを人に言える — Slack の ⚠️ に載る1行
            assert!(err.contains(want), "{label}: {err}");
            let calls = calls.lock().unwrap();
            assert_eq!(enters(&calls), 0, "{label}: Enter を撃たない");
            assert!(
                !calls
                    .iter()
                    .any(|c| c.contains("send-keys") && c.contains(" -l ")),
                "{label}: 本文も1文字も送らない: {calls:?}"
            );
        }
    }

    /// 待機中の窓は素通し。ここが誤爆すると**全部の配達が止まる**ので、モーダル判定と
    /// 同じテストで押さえる(`esc to interrupt` は走行中の語で、モーダルではない)。
    #[test]
    fn an_idle_worker_is_not_mistaken_for_a_dialog() {
        for (label, pane) in [
            ("待機中", IDLE_PANE),
            (
                "走行中",
                "✽ Fermenting… (1m 22s · ↓ 2.0k tokens)\n  (esc to interrupt)\n\
                 ────\n❯ \u{a0}\n────\n  ⏵⏵ auto mode on (shift+tab to cycle)\n",
            ),
        ] {
            assert!(
                Pane::new(pane).modal_footer().is_none(),
                "{label}: モーダル扱いされた"
            );
            let (calls, c) = deliver_probe(vec![pane, "❯ \u{a0}"]);
            c.deliver(&Window::of("@42"), "hello")
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(enters(&calls.lock().unwrap()), 1, "{label}");
        }
    }

    /// **本物の tmux と本物の claude を相手にした実弾**。単体テストは私が採った pane を
    /// 固定するだけなので、「capture して判定して打たない」の一気通貫はここでしか確かめられない
    /// (0.17.3 は単体テスト green のまま実機で効いていなかった)。
    ///
    /// 走らせ方: `cargo test -- --ignored real_tmux`。claude を1つ起こすので既定では回さない。
    #[test]
    #[ignore = "本物の tmux と claude を起こす"]
    fn real_tmux_deliver_refuses_a_live_claude_dialog() {
        // 本番セッション `agentgw-workers` に前方一致しない名前にする(2026-08-18 に踏んだ罠)
        const SESSION: &str = "deliver-probe-rs";
        let t = Tmux::real();
        let sh = |args: &[&str]| (t.run)(args);
        let _ = sh(&["kill-session", "-t", SESSION]);
        sh(&[
            "new-session",
            "-d",
            "-s",
            SESSION,
            "-x",
            "200",
            "-y",
            "50",
            "-c",
            "/home/me",
            "claude",
        ])
        .expect("tmux new-session");

        let c = Claude::new(Tmux::real());
        let w = Window::raw(SESSION);
        let pane =
            || (Tmux::real().run)(&["capture-pane", "-p", "-t", SESSION]).unwrap_or_default();
        let wait = |label: &str, ok: &dyn Fn(&str) -> bool| {
            for _ in 0..120 {
                if ok(&pane()) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            let _ = (Tmux::real().run)(&["kill-session", "-t", SESSION]);
            panic!("{label} を60秒待って出なかった:\n{}", pane());
        };

        wait("入力欄", &|p| !Pane::new(p).input_line().is_empty());
        // 待機中の窓は素通し = 誤爆しない(ここが落ちると全部の配達が止まる)
        assert!(Pane::new(&pane()).modal_footer().is_none(), "{}", pane());
        c.deliver(&w, "say ok").expect("待機中の窓には配達できる");

        // `/model` は**一度だけ**。待ちの中で送り直すと、開いたダイアログに打ち込むことになる
        wait("入力欄(応答後)", &|p| Pane::new(p).input_box_empty());
        sh(&["send-keys", "-t", SESSION, "-l", "--", "/model"]).expect("send /model");
        std::thread::sleep(Duration::from_millis(400));
        sh(&["send-keys", "-t", SESSION, "Enter"]).expect("send Enter");
        wait("ダイアログ", &|p| {
            Pane::new(p).modal_footer().is_some()
        });

        let before = pane();
        let err = c
            .deliver(&w, "この本文は1文字も入ってはいけない")
            .unwrap_err();
        let after = pane();
        let _ = sh(&["send-keys", "-t", SESSION, "Escape"]);
        std::thread::sleep(Duration::from_millis(800));
        let closed = pane();
        let _ = sh(&["kill-session", "-t", SESSION]);

        assert!(err.contains("a dialog has the keyboard"), "{err}");
        // 打っていない = ダイアログは開いたまま、選択も動いていない
        assert!(
            Pane::new(&after).modal_footer().is_some(),
            "ダイアログが閉じた(= Enter を撃った):\n{after}"
        );
        assert_eq!(
            Pane::new(&before).input_line(),
            Pane::new(&after).input_line(),
            "選択カーソルが動いた(= 本文が操作になった)"
        );
        assert!(
            Pane::new(&closed).modal_footer().is_none(),
            "Escape で閉じられなかった:\n{closed}"
        );
    }

    #[test]
    fn confirmations_match_both_wordings() {
        assert!(Pane::new("Set effort level to high (saved as default)").effort_confirmed("high"));
        assert!(Pane::new("Effort level set to auto").effort_confirmed("auto"));
        assert!(!Pane::new("Set effort level to highest").effort_confirmed("high")); // 語境界
        // 同じ level を選び直した時の3つ目の文言(2026-07-29 実機の行そのまま)
        assert!(Pane::new("Kept effort level as xhigh").effort_confirmed("xhigh"));
        assert!(!Pane::new("Kept effort level as xhigh").effort_confirmed("high")); // 語境界
        assert!(Pane::new("⏺ Set model to Opus 4.8 (claude-opus-4-8)").model_confirmed("opus"));
        assert!(Pane::new("Kept model as Opus 4.8").model_confirmed("opus"));
        assert!(!Pane::new("model: opus is nice").model_confirmed("opus"));
    }

    #[test]
    fn compact_pane_reads_live_signals_only() {
        let live = "✳ Compacting conversation… (1m 4s · ↑ 1.6k tokens)\n▐▏████░░░░ 31%\n";
        let p = Pane::new(live).compact_progress();
        assert!(p.active);
        assert_eq!((p.seconds, p.percent), (Some(64), Some(31)));
        assert_eq!(p.tokens.as_deref(), Some("1.6k"));
        // 静的な言及(タイマーも % も無い)は inactive
        assert!(
            !Pane::new("the words Compacting conversation appear in prose")
                .compact_progress()
                .active
        );
        // ctx:NN% は percent として拾わない
        let ctx = "✳ Compacting conversation… (3s)\n  ctx:42%\n";
        assert_eq!(Pane::new(ctx).compact_progress().percent, None);
    }

    /// フッタ行(2026-07-31 実機の現物)から今の権限モードを読む。
    #[test]
    fn mode_status_reads_the_footer_below_the_input_box() {
        let pane = |foot: &str| {
            format!(
                "user: mode を実装した\n\
                 ─────\n\
                 ❯ \n\
                 ─────\n\
                   ~  ctx:4%  14:38  Opus 5\n{foot}"
            )
        };
        assert_eq!(
            Pane::new(&pane("  ⏵⏵ auto mode on (shift+tab to cycle) · ← 1 agent")).mode_status(),
            "auto"
        );
        assert_eq!(
            Pane::new(&pane("  ⏸ plan mode on (shift+tab to cycle)")).mode_status(),
            "plan"
        );
        assert_eq!(
            Pane::new(&pane("  ⏵⏵ accept edits on (shift+tab to cycle)")).mode_status(),
            "edit"
        );
        assert_eq!(
            Pane::new(&pane("  ⏸ manual mode on · ← 1 agent")).mode_status(),
            "manual"
        );
        // 名乗る行が無ければ manual(将来また出なくなっても既定に落ちる)
        assert_eq!(Pane::new(&pane("")).mode_status(), "manual");
        // 会話の本文に同じ語が出ても入力欄より上なので拾わない
        assert_eq!(
            Pane::new(&pane("").replace("mode を実装した", "plan mode on の話")).mode_status(),
            "manual"
        );
    }

    #[test]
    fn effort_status_and_slider() {
        assert!(Pane::new("… ←/→ to adjust …").effort_slider_open());
        assert_eq!(
            Pane::new("  ● high · /effort").effort_status(),
            Some("high")
        );
        assert_eq!(
            Pane::new("✦ ultracode · xhigh effort + …").effort_status(),
            Some("ultracode")
        );
        // 2026-07-29 実機(E2E で 2 連続失敗した現物の行そのまま)— 点は ◉ にドリフトした
        assert_eq!(
            Pane::new("                  ◉ xhigh · /effort").effort_status(),
            Some("xhigh")
        );
        assert_eq!(Pane::new("nothing here").effort_status(), None);
    }

    #[test]
    fn effort_change_dialog_is_detected_and_not_mistaken_for_a_confirmation() {
        // 2026-07-29 実機のダイアログ全文(履歴のあるセッションの `/effort xhigh`)
        let dialog = "\
Change effort level?
Your next response will be slower and use more tokens
This conversation is cached for the current effort level. Switching to xhigh means \
the full history gets re-read on your next message.
❯ 1. Yes, switch to xhigh
  2. No, go back";
        assert!(Pane::new(dialog).effort_confirm_dialog_open());
        // ダイアログは**まだ**確定ではない。level を名指す行があっても done と読まない
        assert!(!Pane::new(dialog).effort_confirmed("xhigh"));
        assert_eq!(Pane::new(dialog).effort_status(), None);
        // 選択肢の `❯` 行は入力ボックスに見えるが、コマンドは残っていない(Enter リトライを誘わない)
        assert!(!Pane::new(dialog).still_has_command("/effort"));
        assert!(!Pane::new("… ←/→ to adjust …").effort_confirm_dialog_open());
    }

    #[test]
    fn login_pane_parsers() {
        let pane =
            "… visit: https://claude.com/cai/oauth/authorize?code=abc\ndef\n\nPaste code here";
        assert_eq!(
            Pane::new(pane).auth_login_url().unwrap(),
            "https://claude.com/cai/oauth/authorize?code=abcdef"
        ); // soft-wrap 再結合
        assert!(Pane::new("no marker").auth_login_url().is_none());
        assert!(matches!(
            Pane::new("… Login successful.").login_outcome(),
            LoginOutcome::Success
        ));
        assert!(matches!(
            Pane::new("Invalid code. …").login_outcome(),
            LoginOutcome::Error
        ));
        assert!(matches!(
            Pane::new("waiting").login_outcome(),
            LoginOutcome::Pending
        ));
    }

    fn hook_tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sc-hooks-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn worker_hooks_has_the_load_bearing_shape() {
        let v = HookIntake::settings_json(8791, "tok");
        // --settings は1つしか効かない → disableRemoteControl は同居必須
        assert_eq!(v["disableRemoteControl"], true);
        // SessionStart は http が無視されるので curl コマンド
        let ss = &v["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(ss["type"], "command");
        assert!(
            ss["command"].as_str().unwrap().contains("curl"),
            "SessionStart must be curl: {ss}"
        );
        assert!(
            ss["command"]
                .as_str()
                .unwrap()
                .contains("x-agentgw-token: tok")
        );
        // UserPromptSubmit / SessionEnd は http + session ヘッダ
        for kind in ["UserPromptSubmit", "SessionEnd"] {
            let h = &v["hooks"][kind][0]["hooks"][0];
            assert_eq!(h["type"], "http", "{kind}");
            assert_eq!(h["headers"]["x-agentgw-token"], "tok", "{kind}");
            assert_eq!(
                h["headers"]["x-agentgw-session"], "${AGENTGW_SESSION_ID}",
                "{kind}"
            );
            assert_eq!(h["allowedEnvVars"][0], "AGENTGW_SESSION_ID", "{kind}");
            assert!(
                h["url"]
                    .as_str()
                    .unwrap()
                    .starts_with("http://127.0.0.1:8791/hook/"),
                "{kind}"
            );
        }
    }

    #[test]
    fn worker_hooks_declare_visibility_hooks() {
        let v = HookIntake::settings_json(1234, "tok");
        for (claude_name, kind, timeout) in [
            ("PreToolUse", "progress", 3),
            ("PostToolUse", "progress", 3),
            ("MessageDisplay", "narration", 3),
            ("StopFailure", "error", 5),
            ("Stop", "stop", 15),
        ] {
            let h = &v["hooks"][claude_name][0]["hooks"][0];
            assert_eq!(h["type"], "http", "{claude_name}");
            assert_eq!(
                h["url"],
                format!("http://127.0.0.1:1234/hook/{kind}"),
                "{claude_name}"
            );
            assert_eq!(h["timeout"], timeout, "{claude_name}");
        }
        // 最初からある3つの hook はそのまま残っていること
        assert!(v["hooks"]["SessionStart"][0]["hooks"][0]["command"].is_string());
        assert_eq!(
            v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["url"],
            "http://127.0.0.1:1234/hook/user_prompt"
        );
    }

    #[tokio::test]
    async fn stop_decision_answers_or_declines() {
        use std::time::Duration;
        // main ループ役が block を返すケース
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            let ev: HookEvent = rx.recv().await.unwrap();
            ev.respond
                .unwrap()
                .send(serde_json::json!({"decision": "block"}))
                .unwrap();
        });
        let ev = HookEvent {
            kind: "stop".into(),
            session_id: "s1".into(),
            payload: serde_json::Value::Null,
            respond: None,
        };
        let body = HookIntake::decide_or_default(&tx, ev, Duration::from_secs(1)).await;
        assert_eq!(body, r#"{"decision":"block"}"#);
        // 受け口ごと閉じている(main が落ちた)→ 送れないので {} で辞退
        let (tx2, rx2) = tokio::sync::mpsc::channel(1);
        drop(rx2);
        let ev = HookEvent {
            kind: "stop".into(),
            session_id: "s1".into(),
            payload: serde_json::Value::Null,
            respond: None,
        };
        let body = HookIntake::decide_or_default(&tx2, ev, Duration::from_millis(50)).await;
        assert_eq!(body, "{}");
    }

    /// 受け取った側が respond を送らずに捨てた場合。oneshot が即 Err になるので、
    /// 上限の5秒を待たずに辞退できることが要件(ターンを吊らせない)。
    #[tokio::test]
    async fn stop_declines_at_once_when_the_answer_is_dropped() {
        use std::time::Duration;
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move { drop(rx.recv().await.unwrap()) });
        let ev = HookEvent {
            kind: "stop".into(),
            session_id: "s1".into(),
            payload: serde_json::Value::Null,
            respond: None,
        };
        let t = std::time::Instant::now();
        assert_eq!(
            HookIntake::decide_or_default(&tx, ev, Duration::from_secs(5)).await,
            "{}"
        );
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "respond drop は即返るはず: {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn hook_responses_are_json() {
        use axum::response::IntoResponse;
        let res = HookIntake::json_body("{}".into()).into_response();
        assert_eq!(res.headers()[CONTENT_TYPE], "application/json");
    }

    #[test]
    fn config_writers_land_on_disk() {
        let dir = hook_tmp("write");
        let dir = crate::bridge::state::StateDir::at(dir);
        let hooks = HookIntake::write_settings(&dir, 8791, "tok").unwrap();
        let mcp = crate::mcp::Mcp::write_config(&dir, 8790, "sid-1", "secret").unwrap();
        assert!(hooks.exists() && mcp.exists());
        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&mcp).unwrap()).unwrap();
        assert_eq!(
            back["mcpServers"]["agentgw"]["headers"]["X-Agentgw-Session"],
            "sid-1"
        );
    }

    // ── 起動画面の分類と見張り ──────────────────────────────────────────────
    // 現行 `classifySpawnPane` `answerSpawnScreens` の移植。
    // 実機の文面は 2026-08-02 に撮ったもの。

    /// 2026-08-02、実機の `tmux capture-pane` の原文。
    const TRUST_PANE: &str = "\
 Accessing workspace:

 /root

 Quick safety check: Is this a project you created or one you trust? (Like your
 own code, a well-known open source project, or work from your team). If not,
 take a moment to review what's in this folder first.

 Claude Code'll be able to read, edit, and execute files here.

 Security guide

 \u{276f} 1. Yes, I trust this folder
   2. No, exit

 Enter to confirm \u{b7} Esc to cancel";

    /// 現行のLOGIN(Claude Code v2.1.207 の実物と注記されている)。
    const LOGIN_PANE: &str = " Claude Code can be used with your Claude subscription\u{2026}\n \
Select login method:\n \u{276f} 1. Claude account with subscription \u{b7} Pro, Max, Team, or Enterprise\n \
  2. Anthropic Console account \u{b7} API usage billing";

    #[test]
    fn spawn_screen_reads_the_real_trust_dialog() {
        assert_eq!(Pane::new(TRUST_PANE).spawn_screen(), SpawnScreen::Trust);
    }

    #[test]
    fn spawn_screen_reads_the_real_login_screen() {
        assert_eq!(
            Pane::new(LOGIN_PANE).spawn_screen(),
            SpawnScreen::LoginRequired
        );
    }

    #[test]
    fn spawn_screen_reads_the_limit_modal_with_its_frame() {
        // 以前の実装が挙げていた実物の描かれ方(枠の中に1行)
        assert_eq!(
            Pane::new("\u{2502} You've hit your session limit").spawn_screen(),
            SpawnScreen::UsageLimited
        );
        // 番号つきの選択肢(現行)
        assert_eq!(
            Pane::new("\u{276f} 1. Stop and wait for the limit to reset").spawn_screen(),
            SpawnScreen::UsageLimited
        );
        assert_eq!(
            Pane::new("  Claude Code weekly limit reached").spawn_screen(),
            SpawnScreen::UsageLimited
        );
    }

    #[test]
    fn spawn_screen_reads_the_dev_channels_confirm() {
        // 現行 / 2865 の文面。**素の部分一致**(実物のキャプチャが無い)
        assert_eq!(
            Pane::new("Yes, proceed with local development").spawn_screen(),
            SpawnScreen::Confirm
        );
        assert_eq!(
            Pane::new("\u{2026}load local development channels?").spawn_screen(),
            SpawnScreen::Confirm
        );
    }

    #[test]
    fn an_account_refusal_wins_over_the_ordinary_prompts() {
        // 現行 と同じ順序の約束 — サインイン/上限は trust/confirm に
        // 「答えて消す」ものではない。Enter を撃っても消えないので、撃つ前に諦める
        let mixed = format!("{LOGIN_PANE}\n \u{276f} 1. Yes, I trust this folder");
        assert_eq!(Pane::new(&mixed).spawn_screen(), SpawnScreen::LoginRequired);
        assert_eq!(
            Pane::new("\u{2502} You've hit your session limit\nlocal development").spawn_screen(),
            SpawnScreen::UsageLimited
        );
    }

    #[test]
    fn the_words_inside_a_line_of_content_are_not_a_screen() {
        // 自分のソースや grep の結果を映しているワーカーの画面。ここへ Enter を撃つと
        // 入力欄の中身が送信される。行頭アンカーが唯一の防波堤(現行)
        assert_eq!(
            Pane::new("  if paneText.includes('I trust this folder') return 'trust'")
                .spawn_screen(),
            SpawnScreen::None_
        );
        assert_eq!(
            Pane::new("grep -n \"you've hit your session limit\" bridge/command.ts").spawn_screen(),
            SpawnScreen::None_
        );
        assert_eq!(
            Pane::new("  * the screen prints Select login method: as its own line").spawn_screen(),
            SpawnScreen::None_
        );
    }

    #[test]
    fn an_ordinary_booting_pane_is_not_a_screen() {
        // 誤検知はフリートを止めうる(現行)。疑わしきは None_
        assert_eq!(Pane::new("").spawn_screen(), SpawnScreen::None_);
        assert_eq!(
            Pane::new("still booting\u{2026}").spawn_screen(),
            SpawnScreen::None_
        );
        assert_eq!(Pane::new("\u{276f} ").spawn_screen(), SpawnScreen::None_);
    }

    /// 画面を1回ごとに差し替えられる `Claude` と、送ったキーの記録。
    fn claude_showing(
        panes: Vec<&'static str>,
    ) -> (std::sync::Arc<std::sync::Mutex<Vec<String>>>, Claude) {
        let keys = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = keys.clone();
        let seq = std::sync::Mutex::new(panes.into_iter());
        let last = std::sync::Mutex::new("");
        let c = claude_with(move |args| {
            if args.first() == Some(&"capture-pane") {
                let mut cur = last.lock().unwrap();
                if let Some(next) = seq.lock().unwrap().next() {
                    *cur = next;
                }
                return Ok(cur.to_string());
            }
            sink.lock().unwrap().push(args.join(" "));
            Ok(String::new())
        });
        (keys, c)
    }

    #[tokio::test]
    async fn the_trust_dialog_is_answered_once_and_the_watch_keeps_going() {
        // 同じ画面が2回続いても Enter は1回だけ(現行trustAnswered)
        let (keys, c) = claude_showing(vec![TRUST_PANE, TRUST_PANE, ""]);
        let out = c
            .watch_spawn_screens(&Window::of("1-1"), 30, 1, &LogCtx::default())
            .await;
        assert_eq!(out, SpawnOutcome::Answered);
        let keys = keys.lock().unwrap();
        assert_eq!(
            keys.iter().filter(|k| k.ends_with("Enter")).count(),
            1,
            "trust に撃つ Enter は1回だけ: {keys:?}"
        );
    }

    #[tokio::test]
    async fn the_login_screen_aborts_the_watch_without_pressing_anything() {
        let (keys, c) = claude_showing(vec![LOGIN_PANE]);
        let out = c
            .watch_spawn_screens(&Window::of("1-1"), 30, 1, &LogCtx::default())
            .await;
        assert_eq!(out, SpawnOutcome::LoginRequired);
        // Enter では消えない画面 — 撃つと入力欄の中身を送ることになるので撃たない
        let keys = keys.lock().unwrap();
        assert!(keys.is_empty(), "拒絶画面にはキーを送らない: {keys:?}");
    }

    #[tokio::test]
    async fn the_limit_modal_aborts_the_watch() {
        let (_keys, c) = claude_showing(vec!["\u{2502} You've hit your session limit"]);
        let out = c
            .watch_spawn_screens(&Window::of("1-1"), 30, 1, &LogCtx::default())
            .await;
        assert_eq!(out, SpawnOutcome::UsageLimited);
    }

    #[tokio::test]
    async fn a_quiet_pane_runs_out_the_budget_and_says_so() {
        let (keys, c) = claude_showing(vec!["still booting\u{2026}"]);
        let out = c
            .watch_spawn_screens(&Window::of("1-1"), 20, 1, &LogCtx::default())
            .await;
        assert_eq!(out, SpawnOutcome::NoScreen);
        assert!(keys.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_confirm_screen_is_answered_but_the_watch_does_not_stop() {
        // confirm に答えても「起動できた」ではない。掛け金を外すのは user_prompt
        let (keys, c) = claude_showing(vec!["Yes, proceed with local development", TRUST_PANE, ""]);
        let out = c
            .watch_spawn_screens(&Window::of("1-1"), 30, 1, &LogCtx::default())
            .await;
        assert_eq!(out, SpawnOutcome::Answered);
        let keys = keys.lock().unwrap();
        assert_eq!(
            keys.iter().filter(|k| k.ends_with("Enter")).count(),
            2,
            "confirm と trust に1回ずつ: {keys:?}"
        );
    }
}
