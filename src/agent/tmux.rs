//! tmux でプロセスを飼う道具。**ここに claude という語は出てこない** —
//! どのエージェント実体も同じ窓の作り方・送り方をする。

/// ワーカーが住む tmux セッション。**tmux は前方一致で解決する**ので、手で打つときも
/// 最後まで書く(`-t agentgw` は `agentgw-workers` に当たる)。
pub const TMUX_SESSION: &str = "agentgw-workers";

/// 本文を打ち込んでから Enter を撃つまでの待ち。**縮めるな**。
///
/// 直後に撃つと Enter(CR)が本文の残りと同じ read にまとまり、TUI は「貼り付けの続き」と
/// 見て改行として飲む。入力欄に本文+末尾改行が残ったまま送信されず、UserPromptSubmit も
/// 発火しないので、スレッドが無言で固まる(2026-07-30 実機: 1.3KB の封筒で発生)。
///
/// 実測(1253B を 5 回ずつ、読み手の1 read あたりの詰まりを 60ms / 250ms に固定して計測):
/// - 待ちなし    → CR が本文と同じ read に混ざる 5/5(両条件とも)
/// - 200ms 待ち  → 単独の read で届く 5/5(両条件とも)
///
/// 読み手が暇なら待ちなしでも単独で届く — pty のバッファ(1022B)を超える本文で TUI が
/// 描画に詰まっている間だけ起きる競合なので、短い封筒では再現しない。
const SETTLE_BEFORE_ENTER: std::time::Duration = std::time::Duration::from_millis(200);

/// `-t` に渡せる形まで解決済みの窓。
///
/// 生の窓名(`1-1`)と解決済みターゲット(`agentgw-workers:1-1`)を同じ `&str` で
/// 扱っていたのが取り違えの温床だった — 生の名前を `capture-pane -t` に渡すと
/// エラーにならず「いま居るセッションの同名窓」を読んでしまう。ここを通せば書けない。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window(String);

impl Window {
    /// 生の窓(`@N` か窓名)から。`@N`(window_id)はそのまま使える — 窓名と違って
    /// 改名で動かない。窓名で来たものだけセッションで修飾する(素の名前を送ると
    /// tmux が「今いるセッション」に当ててしまう)。
    pub fn of(window: &str) -> Self {
        if window.starts_with('@') {
            Window(window.to_string())
        } else {
            Window(format!("{TMUX_SESSION}:{window}"))
        }
    }

    /// すでに `-t` にそのまま渡せる形の文字列から(サインイン用セッションなど、
    /// ワーカーセッションの窓ではないもの)。
    pub(crate) fn raw(target: impl Into<String>) -> Self {
        Window(target.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// ログ行にそのまま埋められるように — 現行のログは解決済みターゲットを出している。
impl std::fmt::Display for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 現行の`WORKER_KILL_GRACE_MS` / `WORKER_KILL_HARD_MS` = 1500ms、
/// ポーリング間隔 100ms(`pollMs`)。
pub const KILL_GRACE_MS: u64 = 1_500;
const KILL_HARD_MS: u64 = 1_500;
const KILL_POLL_MS: u64 = 100;

/// ワーカー本体のプロセス。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pid(pub u32);

/// ログ行にそのまま埋められるように — 現行のログは裸の pid を出している。
impl std::fmt::Display for Pid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Pid {
    /// pid 指名の kill。**パターン kill(pkill -f)は本番のワーカーを巻き込むので使わない。**
    /// TERM → grace_ms まで生存ポーリング → 残っていれば -9 → さらに 1500ms。
    /// 返すのは「生きていた pid を実際に落としたか」。元から居ない・-9 でも死なない、はどちらも false。
    pub async fn kill_graceful(&self, grace_ms: u64) -> bool {
        let pid = self.0;
        if !self.alive() {
            return false;
        }
        self.signal("-TERM");
        if self.wait_until_gone(grace_ms).await {
            return true;
        }
        crate::bridge::state::LogCtx::default().info(
            "worker",
            &format!("SIGKILL pid {pid} — survived SIGTERM within {grace_ms}ms"),
        );
        self.signal("-9");
        let gone = self.wait_until_gone(KILL_HARD_MS).await;
        if !gone {
            crate::bridge::state::LogCtx::default()
                .error("worker", &format!("pid {pid} still alive after SIGKILL"));
        }
        gone
    }

    /// SIGTERM だけ撃って**待たない**。複数のワーカーを畳むとき、`kill_graceful` の猶予が
    /// 直列に積み上がるのを避ける先撃ち用(撃ってから改めて `kill_graceful` を回すと、
    /// 猶予が重なって消化される)。
    pub fn term(&self) -> bool {
        self.signal("-TERM")
    }

    /// `kill` の exit code だけ見る。`output()` なのは `-0` の "No such process" を
    /// ログに漏らさないため。
    fn signal(&self, sig: &str) -> bool {
        std::process::Command::new("kill")
            .args([sig, &self.0.to_string()])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// `kill -0` = 生存プローブ(シグナルは飛ばない)。
    fn alive(&self) -> bool {
        self.signal("-0")
    }

    async fn wait_until_gone(&self, budget_ms: u64) -> bool {
        for _ in 0..budget_ms.div_ceil(KILL_POLL_MS).max(1) {
            tokio::time::sleep(std::time::Duration::from_millis(KILL_POLL_MS)).await;
            if !self.alive() {
                return true;
            }
        }
        false
    }
}

/// tmux 実行の注入口。テストは fake、実弾は本物。
pub struct Tmux {
    #[allow(clippy::type_complexity)]
    pub run: Box<dyn Fn(&[&str]) -> Result<String, String> + Send + Sync>,
}

impl Tmux {
    /// 本物の tmux。
    pub fn real() -> Tmux {
        Tmux {
            run: Box::new(|args| {
                let out = std::process::Command::new("tmux")
                    .args(args)
                    .output()
                    .map_err(|e| format!("tmux {args:?}: {e}"))?;
                if !out.status.success() {
                    return Err(format!(
                        "tmux {args:?} failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    ));
                }
                Ok(String::from_utf8_lossy(&out.stdout).to_string())
            }),
        }
    }

    /// 押し込み: literal タイプ → 一拍 → Enter。3KB 一発で通ることは実測済み。
    pub fn deliver(&self, w: &Window, text: &str) -> Result<(), String> {
        let t = w.as_str();
        // `--` は先頭が `-` のテキスト対策
        (self.run)(&["send-keys", "-t", t, "-l", "--", text])?;
        std::thread::sleep(SETTLE_BEFORE_ENTER);
        (self.run)(&["send-keys", "-t", t, "Enter"])?;
        Ok(())
    }

    /// 窓の見えている範囲のテキスト。TUI の状態(/compact の進捗、モデル名)はここから読む。
    pub fn capture(&self, w: &Window) -> Result<String, String> {
        (self.run)(&["capture-pane", "-p", "-t", w.as_str()])
    }

    /// scrollback ごと読む。`claude auth login` は "Login successful." を出した直後にシェルへ戻り、
    /// 次のポーリングまでにマーカーが画面外へ流れる。
    pub fn capture_history(&self, w: &Window, lines: u32) -> Result<String, String> {
        (self.run)(&[
            "capture-pane",
            "-p",
            "-S",
            &format!("-{lines}"),
            "-t",
            w.as_str(),
        ])
    }

    /// tmux のキー名を1発(`Escape` / `Enter` / `BTab` = shift+tab)。
    pub fn send_key(&self, w: &Window, key: &str) -> Result<(), String> {
        (self.run)(&["send-keys", "-t", w.as_str(), key]).map(|_| ())
    }

    /// Escape 1発(TUI のプロンプト取り消し)。
    pub fn send_escape(&self, w: &Window) -> Result<(), String> {
        self.send_key(w, "Escape")
    }

    /// Enter 1発。
    pub fn send_enter(&self, w: &Window) -> Result<(), String> {
        self.send_key(w, "Enter")
    }

    /// TUI へ1行打ち込む(`/compact` などのスラッシュコマンド)。`deliver` と同じ literal + Enter。
    pub fn send_command(&self, w: &Window, cmd: &str) -> Result<(), String> {
        (self.run)(&["send-keys", "-t", w.as_str(), "-l", "--", cmd])?;
        self.send_enter(w)
    }

    /// 窓ごと落とす。**window_id(`@N`)指名で** — 名前は改名で動く。
    pub fn kill_window(&self, w: &Window) -> Result<(), String> {
        (self.run)(&["kill-window", "-t", w.as_str()]).map(|_| ())
    }

    /// 窓を建てて `line` を走らせる。返すのは window_id(`@N`)。
    /// 窓名は中で走るプログラムの画面タイトルで改名されるので当てにしない。
    pub fn spawn(&self, window: &str, cwd: &str, line: &str) -> Result<Window, String> {
        // セッションが無ければ先に作る(has-session は非0で「無い」を返すので Err を無視)
        let id = if (self.run)(&["has-session", "-t", TMUX_SESSION]).is_err() {
            (self.run)(&[
                "new-session",
                "-d",
                "-s",
                TMUX_SESSION,
                "-n",
                window,
                "-c",
                cwd,
                "-P",
                "-F",
                "#{window_id}",
                line,
            ])?
        } else {
            (self.run)(&[
                "new-window",
                "-d",
                "-t",
                TMUX_SESSION,
                "-n",
                window,
                "-c",
                cwd,
                "-P",
                "-F",
                "#{window_id}",
                line,
            ])?
        };
        let id = id.trim().to_string();
        // 改名を封じる。名前フォールバックが効き続けるように(失敗しても spawn は成功)
        for opt in ["automatic-rename", "allow-rename"] {
            if let Err(e) = (self.run)(&["set-option", "-w", "-t", &id, opt, "off"]) {
                crate::bridge::state::LogCtx::default()
                    .error("worker", &format!("could not turn off {opt} on {id}: {e}"));
            }
        }
        Ok(Window::of(&id))
    }

    /// 窓の棚卸し。**tmux が唯一の権威** — Bridge の記憶は再起動で消えるが、窓は残る。
    ///
    /// **窓名は最後に置く** — 画面タイトルに改名されて空白が入りうるので、先頭3つを
    /// 切ったあとの残り全部が名前、と読めるようにしておく。
    pub fn rows(&self) -> Vec<WindowRow> {
        let Ok(out) = (self.run)(&[
            "list-windows",
            "-t",
            TMUX_SESSION,
            "-F",
            "#{window_id} #{pane_pid} #{pane_current_command} #{window_name}",
        ]) else {
            return Vec::new();
        };
        out.lines()
            .filter_map(|line| {
                let mut f = line.splitn(4, ' ');
                Some(WindowRow {
                    id: f.next()?.to_string(),
                    pid: Pid(f.next()?.trim().parse().ok()?),
                    command: f.next()?.to_string(),
                    name: f.next()?.to_string(),
                })
            })
            .collect()
    }

    /// 窓の中で走っているプロセスの pid。**list-windows の列挙で見る** —
    /// display-message は存在しない窓について別の窓の答えを exit 0 で返す(既知の罠)。
    ///
    /// window_id が第一の手がかり。名前照合はフォールバック — Bridge 再起動後で id を
    /// 忘れていても窓を再発見できないと、毎回 respawn して古いワーカーがリークする。
    pub fn pid_of(&self, window_id: Option<&str>, name: &str) -> Option<Pid> {
        let rows = self.rows();
        rows.iter()
            .find(|r| window_id.is_some_and(|id| id == r.id))
            .or_else(|| rows.iter().find(|r| r.name == name))
            .map(|r| r.pid)
    }
}

/// 棚卸しで見えた窓1つ。
#[derive(Clone)]
pub struct WindowRow {
    pub id: String,
    pub pid: Pid,
    /// pane で**いま**走っているコマンド(`claude` / `zsh` …)。
    pub command: String,
    pub name: String,
}

impl WindowRow {
    /// この窓が抱えているワーカーの session_id(`w-<sid>`)。
    /// `None` = ワーカーの窓ではない(アンカー窓・人が開いた窓) → **触らない**。
    pub fn session_id(&self) -> Option<&str> {
        self.name.strip_prefix("w-").filter(|s| !s.is_empty())
    }

    /// claude が居なくなって shell だけが残った殻か(判定)。
    /// ログインシェルは `-zsh` のように頭に `-` が付く。
    pub fn is_empty_shell(&self) -> bool {
        matches!(
            self.command.trim().trim_start_matches('-'),
            "zsh" | "bash" | "sh" | "fish" | "dash" | "ksh"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording() -> (std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>, Tmux) {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let sink = calls.clone();
        let tmux = Tmux {
            run: Box::new(move |args| {
                sink.lock()
                    .unwrap()
                    .push(args.iter().map(|s| s.to_string()).collect());
                Ok(String::new())
            }),
        };
        (calls, tmux)
    }

    #[test]
    fn deliver_types_then_sends_enter() {
        let (calls, tmux) = recording();
        tmux.deliver(&Window::of("1-1"), "hello").unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "type then Enter: {calls:?}");
        // 先頭が `-` のテキストでも引数として渡るよう `--` を置く
        assert_eq!(
            calls[0],
            [
                "send-keys",
                "-t",
                "agentgw-workers:1-1",
                "-l",
                "--",
                "hello"
            ]
        );
        assert_eq!(
            calls[1],
            ["send-keys", "-t", "agentgw-workers:1-1", "Enter"]
        );
    }

    #[test]
    fn deliver_targets_a_window_id_directly() {
        let (calls, tmux) = recording();
        tmux.deliver(&Window::of("@42"), "hello").unwrap();
        assert_eq!(
            calls.lock().unwrap()[0][2],
            "@42",
            "window id needs no session prefix"
        );
    }

    /// list-windows の出力を返すだけの fake。
    fn listing_tmux(out: &'static str) -> Tmux {
        Tmux {
            run: Box::new(move |_| Ok(out.to_string())),
        }
    }

    #[test]
    fn finds_the_worker_by_window_id_after_tmux_renames_the_window() {
        // 窓名は claude の画面タイトルに改名済み(空白入り)。名前照合はもう当たらない。
        let tmux = listing_tmux(
            "@22 3493 claude ✳ building the thing\n@23 5773 claude 1785161156-915759\n",
        );
        assert_eq!(tmux.pid_of(Some("@22"), "2-1-220"), Some(Pid(3493)));
    }

    /// 棚卸しは「窓名に空白が入っていても最後まで名前」と読めること。ここがずれると
    /// 改名された窓の session_id を取り違えて、**生きている窓を掃除**しかねない。
    #[test]
    fn the_window_listing_keeps_a_renamed_name_whole() {
        let tmux = listing_tmux(
            "@22 3493 claude w-abc-123\n\
             @23 5773 zsh w-dead-9\n\
             @24 91 claude ✳ building the thing\n\
             @25 92 -bash _anchor\n",
        );
        let rows = tmux.rows();
        assert_eq!(rows.len(), 4);

        // ワーカーの窓 = 名前が `w-` で始まるものだけ
        let ids: Vec<Option<&str>> = rows.iter().map(|r| r.session_id()).collect();
        assert_eq!(ids, [Some("abc-123"), Some("dead-9"), None, None]);

        // claude が居るか、shell だけの殻か
        let shells: Vec<bool> = rows.iter().map(|r| r.is_empty_shell()).collect();
        assert_eq!(
            shells,
            [false, true, false, true],
            "ログインシェルの `-bash` も殻"
        );

        assert_eq!(rows[2].name, "✳ building the thing", "空白ごと名前");
        assert_eq!(rows[0].pid, Pid(3493));
    }

    #[test]
    fn falls_back_to_the_window_name_when_the_id_is_unknown() {
        // Bridge 再起動後: id を忘れていても窓を再発見できないと respawn でリークする
        let tmux = listing_tmux("@22 3493 claude 2-1-220\n@23 5773 claude other\n");
        assert_eq!(tmux.pid_of(None, "2-1-220"), Some(Pid(3493)));
        assert_eq!(tmux.pid_of(Some("@99"), "nope"), None);
    }

    #[test]
    fn spawn_returns_the_window_id_and_forbids_renaming() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let sink = calls.clone();
        let tmux = Tmux {
            run: Box::new(move |args| {
                sink.lock()
                    .unwrap()
                    .push(args.iter().map(|s| s.to_string()).collect());
                Ok(if args[0] == "new-window" {
                    "@42\n".to_string()
                } else {
                    String::new()
                })
            }),
        };
        let w = tmux.spawn("1-1", "/repo", "the launch line").unwrap();
        assert_eq!(w.as_str(), "@42");
        let calls = calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|c| c == &["set-option", "-w", "-t", "@42", "automatic-rename", "off"]),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c == &["set-option", "-w", "-t", "@42", "allow-rename", "off"]),
            "{calls:?}"
        );
    }

    #[tokio::test]
    async fn kill_pid_graceful_returns_false_for_dead_pid() {
        // 元から居ない pid は「落とした」ではない
        assert!(!Pid(4_000_000).kill_graceful(100).await);
    }

    #[tokio::test]
    async fn kill_pid_graceful_kills_a_live_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pid = Pid(child.id());
        // 死んだ子は wait するまでゾンビとして pid 表に残り `kill -0` に応える。実弾で殺す
        // claude は tmux の子(こちらの子ではない)なので出ない現象 — テスト側で刈り取る。
        let reaper = std::thread::spawn(move || child.wait().unwrap());
        assert!(pid.kill_graceful(KILL_GRACE_MS).await);
        reaper.join().unwrap();
    }
}
