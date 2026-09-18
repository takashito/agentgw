//! エージェント抽象化レイヤー。Bridge はここから下を直接知らない。
//!
//! `trait` ではなく `enum` なのは、`spawn` などが async で dyn 互換にできず、
//! `async-trait` を足すと依存が8つになるため(依存を増やさない方針)。
//! 実体を足すときは match の対応漏れをコンパイラが指摘する。

pub mod claude;
pub mod screen;
pub mod tmux;

use crate::bridge::state::{LogCtx, ThreadKey};
use tmux::{Pid, Window};

/// ワーカー1本のセッション識別子。
///
/// **使い捨て** — 使用済み ID で起動すると claude は即死する。
/// spawn は毎回新規、継続は実体側の再開機構に任せる。
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(String);

impl SessionId {
    /// spawn ごとに新しい ID。使用済み ID での起動は claude に拒否される(スパイク実測)。
    pub fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or_default();
        let pid = std::process::id();
        let h = format!("{nanos:016x}{pid:08x}{:08x}", nanos as u32);
        SessionId(format!(
            "{}-{}-4{}-8{}-{}",
            &h[0..8],
            &h[8..12],
            &h[13..16],
            &h[17..20],
            &h[20..32]
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// ワーカーの tmux 窓名。**プールも割当済みも同じ1つの規則**。
    /// 窓名が session_id だけで決まるから、window_id を忘れた Bridge 再起動後でも名前で引き直せる
    /// (プール専用名は作らない — 引き当て時に改名しないのが現行の形)。
    pub fn window_name(&self) -> String {
        let mut name = String::from("w-");
        name.extend(self.0.chars().map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-' => c,
            _ => '-',
        }));
        name
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        SessionId(s)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        SessionId(s.to_string())
    }
}

/// `new()` は毎回**新しい**値を発行する — `Default` が同じ意味を持つのはそのため
/// (使い捨て ID なので「既定値」という概念が無く、既定 = 新規発行が唯一の解釈)。
impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// ワーカーを1本起こす要求。
pub struct SpawnReq {
    pub session_id: SessionId,
    pub cwd: String,
    /// 最初に流し込む本文。`None` なら実体側の待機プロンプト(プールの空焚き)。
    /// 実体が向き付けの前口上を付けるので、ここには**素の封筒**を入れる。
    pub prompt: Option<String>,
    /// 継続なら直前の session id。
    pub resume_from: Option<SessionId>,
    /// 座席の窓名([`SessionId::window_name`] の規則)。改名されるので追跡には使わない。
    pub window: String,
    /// 座席の現状。`Absent` 以外への spawn は実体が拒否する(spawn を kill にしない)。
    /// `WorkerState` は `bridge` の型 — 依存の向きの**唯一の例外**。座席チェックの
    /// エラー文言が `{state:?}` を含むので型を伏せると文面が変わる(`bridge/state.rs` の
    /// `WorkerState` の doc に同じ規約が書いてある)。
    pub state: crate::bridge::inbound::WorkerState,
    /// 実体に渡す hook 設定ファイルの絶対パス。
    pub hooks_file: String,
    /// このセッション用に書き出した MCP 設定ファイルの絶対パス。
    pub mcp_config: String,
}

/// ワーカーに渡す封筒 — 1通の受信メッセージを、そのまま prompt に流せる形にしたもの。
///
/// 属性の並びは現行の meta 構築順。
/// `message_id` は**受信メッセージの ts**、`ts` は配達時点の now(呼び手が入れる)。
pub struct Envelope {
    pub channel_id: String,
    pub message_id: String,
    pub user: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub text: String,
    /// 先読みダウンロード済みの添付のローカルパス。`file_count` はこの数(現行)。
    pub file_paths: Vec<String>,
    /// 落とせなかった添付の劣化ノート。黙って捨てず、ワーカーに見せて人に伝えさせる。
    pub file_errors: Vec<String>,
    /// ループ遮断が働いた配達(`loop_guard="true"`)。載っていたら、その bot には返さず
    /// **人に判断を仰げ**という合図。`loop_notify` は呼ぶ相手(Owner の mention)。
    pub loop_guard: Option<String>,
}

/// Claude Code が push 通知を描くときの source。同じ値にして、注入した1通目を
/// push された1通と同じバイト形にする。
///
/// **読み手はプラグイン側の名前。** 2026-09-18 時点で `~/.claude/plugins` に
/// 相当するプラグインは無いので、バイナリ名に揃えてある。将来プラグインを作るときは
/// その名前とここを一致させる。
const CHANNEL_SOURCE: &str = "plugin:agentgw:agentgw";

impl Envelope {
    /// 移植 — cold start のワーカーに、押し込みと同じ形で1通目を渡す。
    pub fn render(&self) -> String {
        let e = self;
        // 現行は `if (threadTs) meta.thread_ts = threadTs` — 無ければ属性ごと出さない
        let thread_ts = e
            .thread_ts
            .as_deref()
            .map_or(String::new(), |t| format!(" thread_ts=\"{t}\""));
        // 添付は受信時に落とし済み。成功があれば
        // file_count/file_paths、失敗があれば file_errors。無いものは属性ごと出さない
        let files = if e.file_paths.is_empty() {
            String::new()
        } else {
            format!(
                " file_count=\"{}\" file_paths=\"{}\"",
                e.file_paths.len(),
                e.file_paths.join(",")
            )
        };
        let file_errors = if e.file_errors.is_empty() {
            String::new()
        } else {
            format!(" file_errors=\"{}\"", e.file_errors.join("\n"))
        };
        // ループ遮断— 立った配達にだけ属性が付く
        let loop_guard = match e.loop_guard.as_deref() {
            None => String::new(),
            Some("") => " loop_guard=\"true\"".to_string(),
            Some(who) => format!(" loop_guard=\"true\" loop_notify=\"{who}\""),
        };
        format!(
            "<channel source=\"{CHANNEL_SOURCE}\" channel_id=\"{}\" message_id=\"{}\" user=\"{}\" \
             user_id=\"{}\" ts=\"{}\"{thread_ts}{files}{file_errors}{loop_guard}>\n{}\n</channel>",
            e.channel_id, e.message_id, e.user, e.user, e.ts, e.text
        )
    }
}

/// 実体から上がってきた出来事1件。
///
/// claude は hook から作り、別の実体は別の方法で作る — Bridge から見ればどちらも同じ型。
/// `respond` は答えを待っている hook(stop)だけに載る — 受け手が送るか落とすかで決まる
/// (落とせば `{}` = 辞退)。Sender は複製できないので Clone は付けない。
#[derive(Debug)]
pub struct HookEvent {
    pub kind: String,
    pub session_id: String,
    pub payload: serde_json::Value,
    pub respond: Option<tokio::sync::oneshot::Sender<serde_json::Value>>,
}

/// 表の1行。`(名前, トークン, %)` — すべて**印字されたまま**の文字列。
pub type ContextCategory = (String, String, String);

/// パース済みの `/context`。
///
/// 数値化はしない(描画に必要な桁は現物の印字が持っている)。
#[derive(Debug)]
pub struct ContextReport {
    /// 例 `claude-opus-4-8[1m]`
    pub model: String,
    /// 使用トークン(印字のまま。例 `43.8k`)
    pub used: String,
    /// 窓の大きさ(印字のまま。例 `1m`)
    pub total: String,
    /// 使用率(印字のまま。例 `4%`)
    pub pct: String,
    pub categories: Vec<ContextCategory>,
}

/// `/usage` の上限行1本(例 `Current week (all models): 66% used · resets Jul 1 at 5pm`)。
#[derive(Debug)]
pub struct UsageRow {
    pub label: String,
    pub pct: String,
    /// `· resets <when>` の中身。0% の行にはこの節が無く、空文字になる
    pub reset: String,
}

/// 生きた圧縮スピナーから読めるもの。数値化するのは秒と % だけで、トークン数は**印字のまま**。
#[derive(Debug, Default, PartialEq)]
pub struct CompactProgress {
    pub active: bool,
    /// 経過秒(`(1m 4s)` → 64)。まだタイマーが出ていなければ None
    pub seconds: Option<u32>,
    /// 印字のままのトークン数(例 `876` / `1.6k`)
    pub tokens: Option<String>,
    /// トークン数の矢印(`↑` / `↓`)
    pub tokens_dir: Option<char>,
    /// スピナー直下のバー行から読んだ完了率(0–100)
    pub percent: Option<u8>,
}

/// コードを投げた**後**の pane の判定。
#[derive(Debug, PartialEq, Eq)]
pub enum LoginOutcome {
    Success,
    Error,
    Pending,
}

/// 圧縮の結末。**文面は持たない** — 何と言うかは Bridge が決める。
#[derive(Debug, PartialEq, Eq)]
pub enum CompactOutcome {
    Done,
    /// 履歴が短くて圧縮するものが無かった。
    Nothing,
    Failed,
}

/// headless probe の失敗2種。**Owner に返す文言が変わる**ので型で分ける。
///
/// 現行は「走ったが答えなかった」を `{kind:'failed'}` で、「そもそも
/// 起動できなかった」を外側の catch で受け、Owner に返す文言を分ける
/// 再試行を勧めてよいのは前者だけ。
pub enum ProbeErr {
    /// 非0 終了 / 60秒タイムアウト
    Failed(String),
    /// spawn できない(実体が PATH に無い等)
    Errored(String),
}

/// エージェント実体。Bridge はこの型のメソッドしか呼ばない。
pub enum Agent {
    Claude(claude::Claude),
}

impl Agent {
    /// この機で使う実体。**Bridge が実体名を書くのはここだけ** — spawn したタスクの中で
    /// 作り直すときも、Bridge 側は `Agent::real()` しか知らない。
    /// (`Tmux` が `Box<dyn Fn>` を抱えていて Clone できないため、`self.agent` を
    /// `'static` なタスクへ持ち越せない。実体は状態を持たないので作り直しでよい)
    pub fn real() -> Self {
        Agent::Claude(claude::Claude::new(tmux::Tmux::real()))
    }

    /// ログと status に出す実体の名前。
    pub fn id(&self) -> &'static str {
        match self {
            Agent::Claude(_) => "claude",
        }
    }

    /// 起こしたワーカーの窓。
    pub fn spawn(&self, req: &SpawnReq) -> Result<Window, String> {
        match self {
            Agent::Claude(c) => c.spawn(req),
        }
    }

    pub fn deliver(&self, w: &Window, text: &str) -> Result<(), String> {
        match self {
            Agent::Claude(c) => c.deliver(w, text),
        }
    }

    /// 起動直後の窓の見張り。**spawn した直後に必ず起こす** — 起動画面に答える手は
    /// 他に無く、答えないと配達は queue に溜まり続ける。
    pub async fn watch_spawn_screens(
        &self,
        w: &Window,
        budget_ms: u64,
        poll_ms: u64,
        ctx: &LogCtx,
    ) -> screen::SpawnOutcome {
        match self {
            Agent::Claude(c) => c.watch_spawn_screens(w, budget_ms, poll_ms, ctx).await,
        }
    }

    /// ワーカー本体のプロセス。生きているかを問うのはこれだけ。
    pub fn pid_of(&self, window_id: Option<&str>, window_name: &str) -> Option<Pid> {
        match self {
            Agent::Claude(c) => c.pid_of(window_id, window_name),
        }
    }

    pub fn terminate(&self, w: &Window) -> Result<(), String> {
        match self {
            Agent::Claude(c) => c.terminate(w),
        }
    }

    /// サインイン用セッションの掃除。非対応の実体(`caps.auth == false`)では何もしない。
    pub fn login_kill(&self) {
        match self {
            Agent::Claude(c) => c.login_kill(),
        }
    }

    // ── 任意機能 ────────────────────────────────────────────────────────────
    // どれも**非対応の実体では `None`** を返す。Err ではないのは、非対応が失敗ではないから —
    // 「このエージェントは対応していません」と言うかどうかは呼び手(Bridge)が決める。
    // 実体が Claude 1つの今は常に `Some`。対応表(旧 `Caps`)は持たない — 読み手が
    // 1つも無い表は嘘になるだけで、非対応は**この `None` 自身**が唯一の表明。

    /// 会話を圧縮する。進捗は1つずつ `on_progress` に渡る — Slack に何を
    /// どう出すかは呼び手の判断で、実体は結末だけ返す。
    pub async fn compact<F, Fut>(
        &self,
        w: &Window,
        key: &ThreadKey,
        session_id: &str,
        on_progress: F,
    ) -> Option<CompactOutcome>
    where
        F: Fn(CompactProgress) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        match self {
            Agent::Claude(c) => Some(c.compact(w, key, session_id, on_progress).await),
        }
    }

    /// いまの effort level。非対応でも読めなかったときも `None` — 呼び手はどちらも
    /// 同じ断りを出すので区別が要らない。
    pub async fn effort(
        &self,
        w: &Window,
        key: &ThreadKey,
        session_id: &str,
    ) -> Option<&'static str> {
        match self {
            Agent::Claude(c) => c.effort(w, key, session_id).await,
        }
    }

    /// effort level を設定する。`Some(効いたか)` / `None` = 非対応。
    pub async fn set_effort(
        &self,
        w: &Window,
        level: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) -> Option<bool> {
        match self {
            Agent::Claude(c) => Some(c.set_effort(w, level, key, ctx).await),
        }
    }

    /// いまの権限モード。`None` = 非対応。
    pub fn mode(&self, w: &Window, ctx: &LogCtx) -> Option<&'static str> {
        match self {
            Agent::Claude(c) => Some(c.mode(w, ctx)),
        }
    }

    /// 権限モードを切り替える(shift+tab)。`Some(効いたか)` / `None` = 非対応。
    pub async fn set_mode(
        &self,
        w: &Window,
        name: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) -> Option<bool> {
        match self {
            Agent::Claude(c) => Some(c.set_mode(w, name, key, ctx).await),
        }
    }

    /// モデルを切り替える。`Some(効いたか)` / `None` = 非対応。
    pub async fn set_model(
        &self,
        w: &Window,
        name: &str,
        key: &ThreadKey,
        ctx: &LogCtx,
    ) -> Option<bool> {
        match self {
            Agent::Claude(c) => Some(c.set_model(w, name, key, ctx).await),
        }
    }

    // ── セッションの履歴(transcript)について訊くこと ──────────────────────
    // 実体ごとに置き場も形も違う。Bridge は「どこで走っていたか」「いつ働いたか」
    // 「いま何のモデルか」だけを訊き、ファイルの形は知らない。

    /// そのセッションが走っていた作業ディレクトリ。`remembered` は hook が運んできた
    /// 履歴のパス(worktree に入ったワーカーは履歴ごと移るので第一候補)。
    pub fn session_cwd(&self, remembered: Option<&str>, session_id: &str) -> Option<String> {
        match self {
            Agent::Claude(_) => {
                claude::Transcript::locate(remembered, session_id).and_then(|t| t.cwd())
            }
        }
    }

    /// 履歴が見つかるか(見つからなければ `--resume` はまず失敗する)。
    pub fn session_history_exists(&self, remembered: Option<&str>, session_id: &str) -> bool {
        match self {
            Agent::Claude(_) => claude::Transcript::locate(remembered, session_id).is_some(),
        }
    }

    /// 最後に**本当に働いた**時刻(履歴の mtime)。idle 判定の基準。
    pub fn last_activity_ms(&self, remembered: Option<&str>, session_id: &str) -> Option<u64> {
        match self {
            Agent::Claude(_) => {
                claude::Transcript::locate(remembered, session_id).and_then(|t| t.mtime_ms())
            }
        }
    }

    /// いま名乗っているモデル id。`Ok(None)` = 履歴は読めたが id が無い。
    pub fn current_model(
        &self,
        remembered: Option<&str>,
        session_id: &str,
    ) -> Option<std::io::Result<Option<String>>> {
        match self {
            Agent::Claude(_) => {
                claude::Transcript::locate(remembered, session_id).map(|t| t.model_id(256 * 1024))
            }
        }
    }

    /// このセッションは「上限に当たった」と Claude Code 自身が書いているか。
    /// `None` = 履歴が見つからない。`Ok(None)` = 読めたが上限の記録は無い(または既に解けた)。
    pub fn session_limit_error(
        &self,
        remembered: Option<&str>,
        session_id: &str,
        now_ms: u64,
    ) -> Option<std::io::Result<Option<crate::bridge::command::LimitHit>>> {
        match self {
            Agent::Claude(_) => claude::Transcript::locate(remembered, session_id)
                .map(|t| t.limit_error(256 * 1024, now_ms)),
        }
    }

    /// モデル id の短い呼び名(`sonnet` など)。無ければ id をそのまま出す。
    pub fn model_alias(&self, model_id: &str) -> Option<&'static str> {
        match self {
            Agent::Claude(_) => screen::ModelId::new(model_id).alias(),
        }
    }

    /// 履歴の `offset` から先を読み、`(本文, 新しい offset)` を返す。
    pub fn new_history_lines(
        &self,
        path: String,
        offset: u64,
        ctx: &LogCtx,
    ) -> Option<(String, u64)> {
        match self {
            Agent::Claude(_) => {
                let mut t = claude::Transcript::at_offset(path, offset);
                t.new_lines(ctx).map(|text| (text, t.offset()))
            }
        }
    }

    /// `/context` を訊く argv。実体ごとに違う(claude は `--fork-session`)。
    pub fn context_argv(&self, session_id: &str) -> Vec<String> {
        match self {
            Agent::Claude(_) => claude::Claude::context_argv(session_id),
        }
    }

    /// `/usage` を訊く argv。
    pub fn usage_argv(&self) -> Vec<String> {
        match self {
            Agent::Claude(_) => claude::Claude::usage_argv(),
        }
    }

    /// サインインを始める。**URL が出るまで**は呼び手が [`Agent::login_url`] で待つ。
    pub fn login_begin(&self, cwd: &str) -> Result<(), String> {
        match self {
            Agent::Claude(c) => c.login_begin(cwd),
        }
    }

    /// 認証 URL が画面に出ていれば返す。
    pub fn login_url(&self) -> Option<String> {
        match self {
            Agent::Claude(c) => c.login_url(),
        }
    }

    pub fn login_submit_code(&self, code: &str) -> Result<(), String> {
        match self {
            Agent::Claude(c) => c.login_submit_code(code),
        }
    }

    pub fn login_outcome(&self) -> LoginOutcome {
        match self {
            Agent::Claude(c) => c.login_outcome(),
        }
    }

    pub async fn logout(&self) -> Result<(), ProbeErr> {
        match self {
            Agent::Claude(c) => c.logout().await,
        }
    }

    pub async fn probe(&self, argv: Vec<String>, cwd: String) -> Result<String, ProbeErr> {
        match self {
            Agent::Claude(c) => c.probe(argv, cwd).await,
        }
    }

    /// probe の出力から `/context` を読む。読めなければ `None`。
    pub fn context_report(&self, raw: &str) -> Option<ContextReport> {
        match self {
            Agent::Claude(_) => screen::Pane::new(raw).context_report(),
        }
    }

    /// probe の出力から `/usage` の上限行を読む。1本も無ければ `None`。
    pub fn usage_rows(&self, raw: &str) -> Option<Vec<UsageRow>> {
        match self {
            Agent::Claude(_) => screen::Pane::new(raw).usage_rows(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_window_name_is_w_plus_the_sanitized_session_id() {
        // 普通の session_id(英数と `-`)はそのまま頭に `w-` が付くだけ
        assert_eq!(
            SessionId::from("18c6a2a8-be4e-4a80-8000-8b08be4e7a80").window_name(),
            "w-18c6a2a8-be4e-4a80-8000-8b08be4e7a80"
        );
        // `[^A-Za-z0-9_-]` は1文字ずつ `-` に
        assert_eq!(
            SessionId::from("a:b.c/d e_f").window_name(),
            "w-a-b-c-d-e_f"
        );
        assert_eq!(SessionId::from("あ").window_name(), "w--");
    }

    #[test]
    fn session_id_is_uuid_v4_shaped() {
        let id = SessionId::new();
        let s = id.as_str();
        let g: Vec<&str> = s.split('-').collect();
        assert_eq!(
            g.iter().map(|x| x.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "{s}"
        );
        assert!(g[2].starts_with('4'), "{s}");
        assert!(g[3].starts_with('8'), "{s}");
        assert!(s.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "{s}");
    }

    #[test]
    fn envelope_matches_current_shape() {
        let e = Envelope {
            channel_id: "C1".into(),
            message_id: "171.002".into(),
            user: "U1".into(),
            ts: "2026-07-28T00:00:00.000Z".into(),
            thread_ts: Some("171.001".into()),
            text: "hello\nworld".into(),
            file_paths: Vec::new(),
            file_errors: Vec::new(),
            loop_guard: None,
        };
        assert_eq!(
            e.render(),
            "<channel source=\"plugin:agentgw:agentgw\" channel_id=\"C1\" \
             message_id=\"171.002\" user=\"U1\" user_id=\"U1\" ts=\"2026-07-28T00:00:00.000Z\" \
             thread_ts=\"171.001\">\nhello\nworld\n</channel>"
        );
        // 現行 `if (threadTs) meta.thread_ts = threadTs` — 無いときは属性ごと出さない
        let bare = Envelope {
            thread_ts: None,
            ..e
        };
        assert!(!bare.render().contains("thread_ts"));
        // 添付が無ければ file_* も属性ごと出さない
        assert!(!bare.render().contains("file_"));
    }

    fn envelope_with(paths: Vec<String>, errors: Vec<String>) -> String {
        Envelope {
            channel_id: "C1".into(),
            message_id: "171.002".into(),
            user: "U1".into(),
            ts: "2026-07-28T00:00:00.000Z".into(),
            thread_ts: Some("171.001".into()),
            text: "check the screenshot".into(),
            file_paths: paths,
            file_errors: errors,
            loop_guard: None,
        }
        .render()
    }

    /// ループ遮断が立った配達にだけ属性が付く。呼ぶ相手が居なければ印だけ。
    #[test]
    fn the_loop_guard_rides_the_envelope_only_when_it_tripped() {
        let env = |guard: Option<&str>| {
            Envelope {
                channel_id: "C1".into(),
                message_id: "171.002".into(),
                user: "B1".into(),
                ts: "2026-07-28T00:00:00.000Z".into(),
                thread_ts: Some("171.001".into()),
                text: "また返してきた".into(),
                file_paths: Vec::new(),
                file_errors: Vec::new(),
                loop_guard: guard.map(str::to_string),
            }
            .render()
        };
        assert!(!env(None).contains("loop_guard"));
        assert!(env(Some("<@U1>")).contains(r#" loop_guard="true" loop_notify="<@U1>""#));
        let no_owner = env(Some(""));
        assert!(no_owner.contains(r#" loop_guard="true""#));
        assert!(!no_owner.contains("loop_notify"));
    }

    #[test]
    fn envelope_carries_downloaded_attachments() {
        // file_count は**成功した**ダウンロード数、file_paths はカンマ結合
        let out = envelope_with(vec!["/i/a.png".into(), "/i/b.png".into()], Vec::new());
        assert!(
            out.contains(
                " thread_ts=\"171.001\" file_count=\"2\" file_paths=\"/i/a.png,/i/b.png\">"
            ),
            "{out}"
        );
        assert!(!out.contains("file_errors"), "{out}");
    }

    #[test]
    fn envelope_carries_failure_notes_without_paths() {
        // 1つも落とせなくても本文は配る — 劣化ノートだけが載る
        let note = "[attachment shot.png not downloaded: file too large]".to_string();
        let out = envelope_with(Vec::new(), vec![note.clone()]);
        assert!(out.contains(&format!(" file_errors=\"{note}\">")), "{out}");
        assert!(!out.contains("file_count"), "{out}");
    }

    #[test]
    fn envelope_mixes_successes_and_failures() {
        // 属性の並びは現行の meta 挿入順 — file_count, file_paths, file_errors
        let out = envelope_with(
            vec!["/i/a.png".into()],
            vec!["[attachment b.png not downloaded: boom]".into()],
        );
        assert!(
            out.contains(
                " file_count=\"1\" file_paths=\"/i/a.png\" \
                 file_errors=\"[attachment b.png not downloaded: boom]\">"
            ),
            "{out}"
        );
        // 複数ノートは改行結合(meta.file_errors = fileErrors.join('\n'))
        let two = envelope_with(Vec::new(), vec!["[a]".into(), "[b]".into()]);
        assert!(two.contains("file_errors=\"[a]\n[b]\">"), "{two}");
    }
}
