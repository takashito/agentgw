//! Bridge が自分で覚え・自分で決めること。I/O は状態ファイルのみ。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 空の JSON オブジェクト。`read_json_or` の既定として何度も要る。
fn json_obj() -> serde_json::Value {
    serde_json::json!({})
}

/// 状態ディレクトリ。**ここを通さずに `~/.local/state` を触らない。**
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateDir(PathBuf);

impl StateDir {
    /// `$AGENTGW_STATE_DIR` → `$XDG_STATE_HOME/agentgw` → `~/.local/state/agentgw`
    ///
    /// 隔離ルールの最終防壁 — **テストビルドのフォールバックは本番を指さない**。
    /// env は set_var/remove_var でプロセス全体に効く(テストは並列)ので、
    /// 1本でも env を消すテストがあれば他のテストが本番 state dir に書いてしまう。
    pub fn resolve() -> Self {
        if let Ok(dir) = std::env::var("AGENTGW_STATE_DIR") {
            return StateDir(PathBuf::from(dir));
        }
        #[cfg(test)]
        return StateDir(std::env::temp_dir().join(format!("agentgw-test-{}", std::process::id())));
        #[cfg(not(test))]
        StateDir(Self::default_base().join("agentgw"))
    }

    /// 置き場の親(`$XDG_STATE_HOME` → `~/.local/state`)。改名の引っ越しもここを見る。
    pub fn default_base() -> PathBuf {
        std::env::var("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_default())
                    .join(".local")
                    .join("state")
            })
    }

    /// 明示のパスから(テストと、CLI が別の場所を指すとき)。
    pub fn at(path: impl Into<PathBuf>) -> Self {
        StateDir(path.into())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// ホームディレクトリ。ルート未設定チャンネルのワーカーが立つ場所の既定値。
    pub fn home() -> String {
        std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
    }

    pub fn read_json_or(&self, name: &str, default: serde_json::Value) -> serde_json::Value {
        read_json_at(&self.join(name), default)
    }

    pub fn write_json_atomic(&self, name: &str, value: &serde_json::Value) -> std::io::Result<()> {
        write_json_at(&self.join(name), value)
    }

    pub fn write_atomic(&self, name: &str, text: &str) -> std::io::Result<()> {
        write_atomic_at(&self.join(name), text)
    }

    /// ファイルの**1つのキーだけ**を差し替える。他のキーはディスクの現物のまま残す。
    ///
    /// access.json は持ち主が複数居る(設定はフリート側と Slack のコマンド、`endpoints` と
    /// `pools` は Bridge)。丸ごと上書きすると、相手が読んでから書くまでの間に自分が変えた
    /// 分が消える。**書き込みは常にキー単位**にしておけば、その窓が構造的に無くなる。
    pub fn patch_json(
        &self,
        name: &str,
        key: &str,
        value: serde_json::Value,
    ) -> std::io::Result<()> {
        let mut root = self.read_json_or(name, serde_json::json!({}));
        if !root.is_object() {
            root = serde_json::json!({});
        }
        root[key] = value;
        self.write_json_atomic(name, &root)
    }

    /// claude に渡す**生成物**の置き場(`--settings` / `--mcp-config`)。
    ///
    /// **状態ではないので state ディレクトリに置かない。** 起動のたびに書き直すもので、
    /// 消えても次の起動で作られる。claude は**起動時に読むだけ**(実測: 走っている claude の
    /// lsof に出てこない)なので、OS の一時領域に置いて掃除も任せる。state に置いていた頃は
    /// セッションごとの MCP 設定が消されないまま溜まっていた(2026-08-02 実測で 101 個)。
    ///
    /// dev と本番を分けるため、state ディレクトリの名前を後ろに付ける。
    pub fn runtime_dir(&self) -> PathBuf {
        let tag = self.0.file_name().unwrap_or_default().to_string_lossy();
        std::env::temp_dir().join(format!("agentgw-{tag}"))
    }

    /// 生成物を書いて、claude に渡すパスを返す。親ディレクトリは作る。
    pub fn write_runtime_json(
        &self,
        name: &str,
        value: &serde_json::Value,
    ) -> std::io::Result<PathBuf> {
        let path = self.runtime_dir().join(name);
        write_json_at(&path, value)?;
        Ok(path)
    }

    /// 再起動マーカーの置き場(bridge.ts の `paths().bridgeRestartMarker` 相当)。再起動は2つの
    /// Bridge プロセスをまたぐので、頼んだスレッドと進捗メッセージの ts をここに置いて引き継ぐ。
    /// **後継が読んだら消す** — 残すと次の起動が偽の「✅ 再起動が完了しました」を出す。
    pub fn restart_marker(&self) -> PathBuf {
        self.join("restart-marker.json")
    }

    pub fn load_env(&self) -> std::io::Result<Vec<(String, String)>> {
        Ok(parse_env(&std::fs::read_to_string(self.join(".env"))?))
    }

    /// ポートは記憶して再利用する — ワーカーは URL を焼き込んで Bridge 再起動をまたぐ。
    pub fn remembered_port(&self, which: &str, allocate: impl FnOnce() -> u16) -> u16 {
        if let Some(port) = self.endpoint(which)["port"].as_u64() {
            return port as u16;
        }
        let port = allocate();
        let _ = self.put_endpoint(which, "port", serde_json::json!(port));
        port
    }

    /// access.json の `"endpoints"` に置いた `hook` / `mcp` の1つ。
    ///
    /// **2026-08-02 に hook-endpoint.json / mcp-endpoint.json から移した。**
    /// 読み手は Bridge だけ(ワーカーへは焼き込んだ値が渡る)。
    fn endpoint(&self, which: &str) -> serde_json::Value {
        self.read_json_or("access.json", json_obj())["endpoints"][which].clone()
    }

    /// 片方の口の1フィールドだけを差す。`endpoints` の外(設定)には触らない。
    fn put_endpoint(
        &self,
        which: &str,
        field: &str,
        value: serde_json::Value,
    ) -> std::io::Result<()> {
        let mut endpoints = self.read_json_or("access.json", json_obj())["endpoints"].clone();
        if !endpoints.is_object() {
            endpoints = json_obj();
        }
        endpoints[which][field] = value;
        self.patch_json("access.json", "endpoints", endpoints)
    }

    /// ワーカーと共有する秘密。記憶して再利用(ワーカーは焼き込んで Bridge 再起動をまたぐ)。
    /// hook と MCP で同じ作り — `endpoints` の中のどちらか、だけが違う。
    pub fn remembered_token(&self, which: &str) -> String {
        if let Some(t) = self.endpoint(which)["token"].as_str() {
            return t.to_string();
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let token = format!("{:x}{nanos:x}", std::process::id());
        let _ = self.put_endpoint(which, "token", serde_json::json!(token));
        token
    }

    /// ログの出力先。thread_key あり → by-thread、session_id のみ → sessions、
    /// 無し → plugin-debug.log
    pub fn log_path(&self, ctx: &LogCtx) -> PathBuf {
        match (&ctx.thread_key, &ctx.session_id) {
            (Some(key), _) => self
                .join("logs")
                .join("by-thread")
                .join(ctx.sanitized_key().unwrap_or_else(|| key.to_string()))
                .join("bridge.log"),
            (None, Some(sid)) => self
                .join("logs")
                .join("sessions")
                .join(format!("{sid}.log")),
            (None, None) => self.join("plugin-debug.log"),
        }
    }

    /// 空きポートを OS に選ばせる。
    pub fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map(|a| a.port())
            .unwrap_or(0)
    }
}

/// いまの epoch ミリ秒。chrono は使わない(依存は確定7つ)。
///
/// 時刻は**生の `u64` のまま持ち回る** — newtype を被せても、この repo の時刻は
/// `deadline_ms` / `spawned_at_ms` / `until_ms` … と全部 epoch ms なので守るものが無い。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// `2026-07-27T12:00:00.000Z`
pub fn iso8601(ms: u64) -> String {
    let (secs, millis) = ((ms / 1000) as i64, (ms % 1000) as u32);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, dd) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{dd:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// days-since-epoch → (year, month, day)。Howard Hinnant の civil_from_days。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// ログ1行の宛先。
///
/// **どのファイルに書くか**をこれだけで決める(session ログ / スレッド別ログ /
/// plugin-debug.log の振り分け)。両方 None なら全体ログだけ。
#[derive(Default, Clone)]
pub struct LogCtx {
    pub session_id: Option<String>,
    pub thread_key: Option<ThreadKey>,
}

/// 1レコード16KB上限。現行 shared/state.ts の `sanitize` と同じ。
const MAX_RECORD: usize = 16 * 1024;

impl LogCtx {
    /// のログ行: `<ISO8601> <level> <component> pid=<pid> session=<sid|-> <message>`
    /// 現行と1文字同じでなければならない(既存の slack-e2e-measure スキルが読む)。
    ///
    /// `pub(crate)` なのは Relay が**同じ行を別の宛先(stdout)へ**出すため。書式を2か所に
    /// 持つと、片方だけ直った日に e2e スキルが読めなくなる。
    pub(crate) fn line(&self, level: &str, component: &str, message: &str) -> String {
        let mut msg = message.replace("\r\n", "\\n").replace('\n', "\\n");
        if msg.len() > MAX_RECORD {
            let extra = msg.len() - MAX_RECORD;
            msg.truncate(MAX_RECORD);
            msg.push_str(&format!(" …[+{extra} chars truncated]"));
        }
        let sid = self.session_id.as_deref().unwrap_or("-");
        format!(
            "{} {level} {component} pid={} session={sid} {msg}\n",
            iso8601(now_ms()),
            std::process::id()
        )
    }

    fn write(&self, level: &str, component: &str, message: &str) {
        // ログはホットパスを壊さない — 失敗は握りつぶす(現行 state.ts と同じ)
        let path = StateDir::resolve().log_path(self);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = f.write_all(self.line(level, component, message).as_bytes());
        }
    }

    pub fn info(&self, component: &str, message: &str) {
        self.write("info", component, message)
    }

    pub fn debug(&self, component: &str, message: &str) {
        self.write("debug", component, message)
    }

    pub fn error(&self, component: &str, message: &str) {
        self.write("error", component, message)
    }

    /// threadKey (`channel:thread_ts`) をファイル名に安全な形へ。現行 shared/state.ts と同一規則。
    /// **ログのディレクトリ名専用**(`logs/by-thread/<key>/`)— tmux の窓名には使わない。
    fn sanitized_key(&self) -> Option<String> {
        self.thread_key
            .as_ref()
            .map(|k| k.as_str().replace([':', '.'], "-"))
    }
}

/// `{channel}:{thread_ts}`。現行 bridge/threads.ts と同一規則。
///
/// **ログのディレクトリ名にそのまま使わない** — `:` と `.` を落とすのは
/// [`LogCtx::sanitized_key`] の仕事。
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct ThreadKey(String);

impl ThreadKey {
    pub fn new(channel: &str, thread_ts: &str) -> Self {
        ThreadKey(format!("{channel}:{thread_ts}"))
    }

    /// 既にある文字列から(threads.json / ログ / hook payload 由来)。
    pub fn parse(raw: &str) -> Self {
        ThreadKey(raw.to_string())
    }

    /// 最初の `:` で分割。`:` 無しはチャンネルのみのキー。
    pub fn split(&self) -> (String, Option<String>) {
        match self.0.split_once(':') {
            Some((ch, ts)) => (ch.to_string(), Some(ts.to_string())),
            None => (self.0.clone(), None),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ThreadKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// リテラルとの比較(ログ行やテストで読みやすい)。
impl PartialEq<&str> for ThreadKey {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// repo ごとの安定した tmux 安全なキー。djb2 → base36。作るのは [`PoolKey::of_cwd`]。
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PoolKey(String);

impl PoolKey {
    /// repo ごとの安定した tmux 安全なプールキー。djb2 → base36(移植
    /// i32 で回して u32 として base36)。パス長に関わらず窓名が短く収まる。
    pub fn of_cwd(cwd: &str) -> PoolKey {
        PoolKey(Self::key_str(cwd))
    }

    fn key_str(cwd: &str) -> String {
        let mut h: i32 = 5381;
        // charCodeAt 相当 = UTF-16 コードユニット
        for u in cwd.encode_utf16() {
            h = h.wrapping_shl(5).wrapping_add(h).wrapping_add(u as i32);
        }
        let mut n = h as u32;
        if n == 0 {
            return "repo-0".to_string();
        }
        let mut buf = Vec::new();
        while n > 0 {
            buf.push(char::from_digit(n % 36, 36).unwrap() as u8);
            n /= 36;
        }
        buf.reverse();
        format!("repo-{}", String::from_utf8(buf).unwrap())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PoolKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<&str> for PoolKey {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// ライフサイクルの節目1つ(何番目のイベントか / 前から何 ms か / spawn から何 ms か)。
///
/// 到着時刻を**1つの時計**で刻むのでイベント間の差分は権威的
/// (プロセス間の時計ずれが混ざらない)。移植元。
pub struct Milestone {
    pub seq: u32,
    pub since_prev_ms: u64,
    pub since_spawn_ms: u64,
}

impl Milestone {
    /// 現行のlifecycle ログ本文と1文字同じ。
    pub fn message(&self, key: &ThreadKey, event: &str) -> String {
        format!(
            "thread={key} #{} {event} +{}ms (since spawn +{}ms)",
            self.seq, self.since_prev_ms, self.since_spawn_ms
        )
    }
}

/// スレッドごとの時間軸そのもの。[`Milestone`] を刻む側。
///
/// **1つの時計**を持つのが仕事(プロセス間の時計ずれを混ぜないため、差分は必ずここから出す)。
#[derive(Default)]
pub struct Lifecycle {
    /// key → (spawn_at, prev_at, seq)
    state: HashMap<ThreadKey, (u64, u64, u32)>,
}

impl Lifecycle {
    pub fn new() -> Self {
        Self::default()
    }

    /// `spawn`(または未知キーの初回イベント)がそのキーの時間軸をリセットする。
    pub fn record(&mut self, key: &ThreadKey, event: &str, now_ms: u64) -> Milestone {
        let s = self.state.entry(key.clone()).or_insert((now_ms, now_ms, 0));
        if event == "spawn" {
            *s = (now_ms, now_ms, 0);
        }
        s.2 += 1;
        let m = Milestone {
            seq: s.2,
            // 壁時計は巻き戻りうる — 負の差分は 0 に潰す
            since_prev_ms: now_ms.saturating_sub(s.1),
            since_spawn_ms: now_ms.saturating_sub(s.0),
        };
        s.1 = now_ms;
        m
    }
}

// ─── ディスクの読み書き ─────────────────────────────────────────────────────
// `StateDir` のメソッドが唯一の入口。以下は `impl StateDir` 越しにしか呼ばれない下請けで、
// 状態ディレクトリの外(絶対パス指定の transcript / plist)を触る所だけが直に使う。

fn read_json_at(path: &Path, default: serde_json::Value) -> serde_json::Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(default)
}

fn write_json_at(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    write_atomic_at(path, &serde_json::to_string_pretty(value)?)
}

/// tmp 書き→rename。半端な JSON を読ませない。
pub(crate) fn write_atomic_at(path: &Path, text: &str) -> std::io::Result<()> {
    write_atomic_mode(path, text, None)
}

/// 同じ tmp→rename に、**作る瞬間からの mode** を足せる形。
/// トークンの入ったファイル(Relay の state.json)は 0600 で置く — 後から chmod すると、
/// その一瞬だけ他人に読める窓が開く。
pub(crate) fn write_atomic_mode(path: &Path, text: &str, mode: Option<u32>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    match mode {
        None => std::fs::write(&tmp, text)?,
        Some(m) => {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(m);
            }
            #[cfg(not(unix))]
            let _ = m;
            use std::io::Write;
            opts.open(&tmp)?.write_all(text.as_bytes())?;
        }
    }
    std::fs::rename(&tmp, path)
}

fn parse_env(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            let v = v.trim().trim_matches('"').trim_matches('\'');
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

// ─── 台帳: threads.json / access.json ───────────────────────────────────────
// 使うのは threads の agent_id/channel_id/repo_path、access の owner/routes だけ。
// 残りは flatten 受け皿で往復保存する(切替日に本番 JSON を無変換で継承するため)。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// threads.json のスレッド1本 = **Bridge がそのスレッドについて覚えていること**
/// (どのセッションが担当か / どこで動いているか / 何を許したか)。
///
/// 未知フィールドは `extra` で往復保存する — 切替日に本番の JSON を無変換で継承するため。
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct ThreadEntry {
    /// **ディスク上の名前は現行 Bun と同じ `session_id`**。
    /// 独自の `agent_id` で書いていた頃の dev の state も読めるよう alias で受ける —
    /// 名前が食い違うと、切替日に本番の threads.json を置いても全スレッドが
    /// 「セッション未設定」に見えて新規セッションで起き直る(無変換継承の要)。
    #[serde(
        rename = "session_id",
        alias = "agent_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub agent_id: Option<String>,
    /// このスレッドで「以後訊かない」と**人が押した**ツール名。
    /// セッションが自分で書く道は無い(prompt injection への構造的な守り)。
    #[serde(rename = "allowedTools", skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_path: Option<String>,
    /// スレッドの話題(冒頭メッセージの頭 60 文字)。`status` のリンク文字列になる。
    /// 現行 と同じ形で書く — 切替日に本番の値をそのまま読むため
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// 渡したが**まだ返事が来ていない**依頼(2026-08-02 に pending.json から移した)。
    ///
    /// entry の `pending`(`extra` の中)とは**別物** — あちらは「まだ渡していない」
    /// 配達待ちの queue で、こちらは「渡したのに返事がない」印。名前が紛らわしいので
    /// 現行 Bun の呼び名(inflight)を使う。書き手は [`Ledger::flush`] 1箇所きり。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inflight: Vec<Inflight>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl ThreadEntry {
    pub fn new(channel_id: &str, agent_id: &str) -> Self {
        Self {
            agent_id: Some(agent_id.to_string()),
            channel_id: Some(channel_id.to_string()),
            ..Default::default()
        }
    }
}

/// threads.json — トップレベルは thread_ts → entry。キー順安定のため BTreeMap。
#[derive(Default)]
pub struct Threads {
    pub entries: BTreeMap<String, ThreadEntry>,
    path: Option<PathBuf>,
}

impl Threads {
    pub fn from_str(src: &str) -> serde_json::Result<Self> {
        Ok(Self {
            entries: serde_json::from_str(src)?,
            path: None,
        })
    }

    /// **読めなかったことを黙らない。** 「ファイルが無い」(初回起動 = 正常)と「あるのに
    /// 読めない・壊れている」(事故)は意味が正反対なのに、どちらも `.ok()` で「空」に潰して
    /// いた。空の担当表は [`crate::Bridge::reap_stray_windows`] から見ると「どの窓も持ち主
    /// 不明」なので、**動いているワーカーが片端から閉じられる**(2026-08-03 実機で3本)。
    /// しかもログが1行も出ないので、後から理由を追えなかった。**無いのは正常、読めないのは
    /// 事故** — 分けて、事故だけ error に残す。
    pub fn load(dir: &StateDir) -> Self {
        let path = dir.join("threads.json");
        let entries = match std::fs::read_to_string(&path) {
            Ok(src) => serde_json::from_str(&src).unwrap_or_else(|e| {
                LogCtx::default().error(
                    "bridge",
                    &format!(
                        "threads.json is corrupt ({e}) — every thread lost its worker; \
                         live windows are NOT closed, but delivery starts from scratch"
                    ),
                );
                Default::default()
            }),
            // 初回起動。まだ1本もスレッドが無いだけなので黙って空で始める
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(e) => {
                LogCtx::default().error(
                    "bridge",
                    &format!("could not read threads.json ({e}) — every thread lost its worker"),
                );
                Default::default()
            }
        };
        Self {
            entries,
            path: Some(path),
        }
    }

    pub fn to_string_pretty(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(&self.entries)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        write_atomic_at(path, &self.to_string_pretty()?)
    }

    pub fn get(&self, thread_ts: &str) -> Option<&ThreadEntry> {
        self.entries.get(thread_ts)
    }

    /// このスレッドは**もう動いている**か(`isThreadActive`)。動いていれば、
    /// チャンネルでもメンション無しの続きを受け取る。`pending` / `paused` は動いていない扱い。
    pub fn is_active(&self, thread_ts: &str) -> bool {
        !matches!(self.status_of(thread_ts), "pending" | "paused" | "none")
    }

    /// `getThreadStatus` — 知らないスレッドは `none`、`status` の無いエントリは `active`。
    pub fn status_of(&self, thread_ts: &str) -> &str {
        match self.get(thread_ts) {
            None => "none",
            Some(e) => match e.extra.get("status").and_then(|v| v.as_str()) {
                Some(s @ ("pending" | "paused")) => s,
                _ => "active",
            },
        }
    }

    /// まだ渡していないメッセージを threads.json に逃がす(`enqueuePending`)。
    /// **message_id で冪等** — 同じメッセージを二度積まない。エントリが無ければ最小のものを作る
    /// (逃がす先が無いという理由で捨てない)。
    pub fn enqueue_pending(&mut self, thread_ts: &str, channel: &str, msg: &InboundMsg) {
        let e = self.entries.entry(thread_ts.to_string()).or_default();
        if e.channel_id.is_none() {
            e.channel_id = Some(channel.to_string());
        }
        let mut queue = match e.extra.get("pending") {
            Some(serde_json::Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        if queue
            .iter()
            .any(|m| m["meta"]["message_id"].as_str() == Some(msg.ts.as_str()))
        {
            return;
        }
        queue.push(serde_json::json!({
            "meta": {
                "channel_id": msg.channel,
                "message_id": msg.ts,
                "thread_ts": thread_ts,
                "user": msg.user,
            },
            // 本文と添付は**そのまま**持つ。復元したときに同じ封筒が組めるように
            "text": msg.text,
            "file_paths": msg.file_paths,
            "file_errors": msg.file_errors,
        }));
        e.extra
            .insert("pending".into(), serde_json::Value::Array(queue));
    }

    /// 逃がしてあった分を取り出して**消す**(`drainPending`)。
    pub fn drain_pending(&mut self, thread_ts: &str) -> Vec<InboundMsg> {
        let Some(e) = self.entries.get_mut(thread_ts) else {
            return Vec::new();
        };
        let Some(serde_json::Value::Array(queue)) = e.extra.remove("pending") else {
            return Vec::new();
        };
        let channel_kind = |c: &str| {
            if c.starts_with('D') {
                ChannelKind::Dm
            } else {
                ChannelKind::Channel
            }
        };
        queue
            .into_iter()
            .filter_map(|m| {
                let channel = m["meta"]["channel_id"].as_str()?.to_string();
                Some(InboundMsg {
                    channel_kind: channel_kind(&channel),
                    channel,
                    ts: m["meta"]["message_id"].as_str()?.to_string(),
                    thread_ts: Some(thread_ts.to_string()),
                    user: m["meta"]["user"].as_str().map(str::to_string),
                    is_bot: false,
                    bot_id: None,
                    text: m["text"].as_str().unwrap_or_default().to_string(),
                    files: Vec::new(),
                    file_paths: m["file_paths"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                    file_errors: m["file_errors"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                    reaction: None,
                    deleted_ts: None,
                    edited: None,
                })
            })
            .collect()
    }

    /// 逃がした分を抱えているスレッドの根(起動時に拾い直す先)。
    pub fn threads_with_pending(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, e)| {
                matches!(e.extra.get("pending"), Some(serde_json::Value::Array(a)) if !a.is_empty())
            })
            .map(|(ts, _)| ts.clone())
            .collect()
    }

    /// bot の連投を1つ数えて、その連続数を返す(`bumpBotStreak`)。エントリが無ければ 0 —
    /// **数えるものが無い**(まだ誰も喋っていないスレッド)。
    pub fn bump_bot_streak(&mut self, thread_ts: &str) -> u64 {
        let Some(e) = self.entries.get_mut(thread_ts) else {
            return 0;
        };
        let next = e
            .extra
            .get("bot_streak")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            + 1;
        e.extra.insert("bot_streak".into(), next.into());
        next
    }

    /// ループ遮断 — このスレッドを止める(`pauseThread`)。人が話しかけるまで bot は入れない。
    pub fn pause(&mut self, thread_ts: &str) {
        if let Some(e) = self.entries.get_mut(thread_ts) {
            e.extra
                .insert("status".into(), serde_json::Value::String("paused".into()));
        }
    }

    /// 人が話しかけた — 連続数を戻し、止めていたスレッドを動かす(`resetThreadStreak`)。
    /// `pending`(まだ立ち上がっていない)は触らない。返り値は「止まっていたか」。
    pub fn reset_bot_streak(&mut self, thread_ts: &str) -> bool {
        let was_paused = self.status_of(thread_ts) == "paused";
        let Some(e) = self.entries.get_mut(thread_ts) else {
            return false;
        };
        if e.extra.get("status").and_then(|v| v.as_str()) == Some("pending") {
            return false;
        }
        e.extra.insert("bot_streak".into(), 0u64.into());
        if was_paused {
            e.extra
                .insert("status".into(), serde_json::Value::String("active".into()));
        }
        was_paused
    }

    pub fn upsert(&mut self, thread_ts: &str, entry: ThreadEntry) {
        self.entries.insert(thread_ts.to_string(), entry);
    }

    /// 起動時に台帳へ載せ直してよい鍵だけを残す(`Bridge::restore_pending`)。
    ///
    /// **ワーカーが生きているスレッドだけ。** 死んだスレッドの未応答は誰も応えないので、
    /// 載せると沈黙の見張りが永久に居座る。スレッドが引けない鍵・セッションがまだ無い鍵も
    /// 同じ理由で落とす。`alive` は session_id → 生存(実体は tmux の pid 実測)。
    pub fn surviving(&self, keys: &[ThreadKey], alive: impl Fn(&str) -> bool) -> Vec<ThreadKey> {
        keys.iter()
            .filter(|key| {
                key.split()
                    .1
                    .and_then(|ts| self.get(&ts))
                    .and_then(|e| e.agent_id.as_deref())
                    .is_some_and(&alive)
            })
            .cloned()
            .collect()
    }

    pub fn find_by_session(&self, session_id: &str) -> Option<(&String, &ThreadEntry)> {
        self.entries
            .iter()
            .find(|(_, e)| e.agent_id.as_deref() == Some(session_id))
    }

    /// このスレッドで「以後訊かない」と押されたツールか。
    pub fn thread_tool_allowed(&self, thread_ts: &str, tool: &str) -> bool {
        self.get(thread_ts)
            .and_then(|e| e.allowed_tools.as_ref())
            .is_some_and(|v| v.iter().any(|t| t == tool))
    }

    /// 「以後このスレッドでは訊かない」を覚える。**人がボタンを押したときだけ**呼ばれる。
    /// スレッドの記録がまだ無ければ最小の器を作る。
    pub fn grant_thread_tool(&mut self, thread_ts: &str, tool: &str) {
        if thread_ts.is_empty() || tool.is_empty() {
            return;
        }
        let mut e = self.get(thread_ts).cloned().unwrap_or_default();
        let allowed = e.allowed_tools.get_or_insert_with(Vec::new);
        if !allowed.iter().any(|t| t == tool) {
            allowed.push(tool.to_string());
        }
        self.upsert(thread_ts, e);
    }
}

/// access.json の `"pools"` — cwd → 在庫に**指名**したセッション ID。中身は cwd
/// (人が読める形)で、[`PoolKey`] は cwd から導出できるので保存しない。キー順安定のため BTreeMap。
///
/// 持っているのは在庫の実体ではなく**指名**で、Bridge プロセスをまたいで残る。在庫を起こす
/// ときは指名があれば `--resume`、無ければ新規 ID を切ってここに指名する。これが無いと
/// 再起動のたびに使い捨てのセッションが切られ、claude 側のセッション履歴が在庫で埋まる。
///
/// 指名を捨てるのは4つだけ: スレッドへの引き当て(卒業)/ resume の失敗 / セッションの終了 /
/// プール対象から外れた cwd。
///
/// **2026-08-02 に pools.json から access.json の中へ移した。** 書き込みは
/// [`StateDir::patch_json`] のキー単位なので、設定を書く経路とは互いを潰さない。
#[derive(Default)]
pub struct Pools {
    entries: BTreeMap<String, String>,
    dir: Option<StateDir>,
}

impl Pools {
    pub fn load(dir: &StateDir) -> Self {
        Self {
            entries: serde_json::from_value(
                dir.read_json_or("access.json", json_obj())["pools"].clone(),
            )
            .unwrap_or_default(),
            dir: Some(dir.clone()),
        }
    }

    pub fn from_str(src: &str) -> serde_json::Result<Self> {
        Ok(Self {
            entries: serde_json::from_str(src)?,
            dir: None,
        })
    }

    pub fn to_string_pretty(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(&self.entries)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(dir) = &self.dir else {
            return Ok(());
        };
        dir.patch_json("access.json", "pools", serde_json::to_value(&self.entries)?)
    }

    /// 指名されている session_id ぜんぶ(cwd は問わない)。窓の掃除が「持ち主が居るか」を
    /// 引くのに使う — **メモリ上の在庫では足りない**。Bridge を起こし直した直後は在庫が
    /// まだ立っておらず、生きている在庫ワーカーが持ち主無しに見えてしまう。
    pub fn sessions(&self) -> impl Iterator<Item = &str> {
        self.entries.values().map(String::as_str)
    }

    /// この cwd の在庫に指名されているセッション。あれば `--resume` で起こす相手。
    pub fn session_of(&self, cwd: &str) -> Option<&str> {
        self.entries.get(cwd).map(String::as_str)
    }

    pub fn nominate(&mut self, cwd: &str, session_id: &str) {
        self.entries.insert(cwd.to_string(), session_id.to_string());
    }

    pub fn release(&mut self, cwd: &str) -> Option<String> {
        self.entries.remove(cwd)
    }

    /// この session_id の指名を外す(cwd が手元に無いところから呼ぶ)。外せたら cwd を返す。
    pub fn release_session(&mut self, session_id: &str) -> Option<String> {
        // ponytail: プールはせいぜい数個 — 逆引き表は要らない
        let cwd = self
            .entries
            .iter()
            .find(|(_, sid)| sid.as_str() == session_id)
            .map(|(cwd, _)| cwd.clone())?;
        self.entries.remove(&cwd);
        Some(cwd)
    }

    /// 指名されている `(cwd, session_id)` の一覧。起動時の拾い直しが舐める。
    pub fn rows(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .map(|(cwd, sid)| (cwd.clone(), sid.clone()))
            .collect()
    }
}

/// access.json のチャンネル1つ分の設定 = **このチャンネルで喋ったらどこで動くか**。
///
/// repo_path が作業ディレクトリ、warm が事前起動の可否、allowed_tools が常設の許可。
/// [`ThreadEntry`] と同じく未知フィールドは `extra` で往復保存する。
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Route {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// このチャンネルの repo を事前に起動するか。未設定 = 既定に従う。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warm: Option<bool>,
    /// このチャンネルで「以後訊かない」と**人が押した**ツール名。
    #[serde(rename = "allowedTools", skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    /// このチャンネルの担当マシン(子の名前 / 親自身の名前)。
    /// **未設定 = このマシンが自分で処理する**(単独 Bridge の既定はこれ)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// access.json。owner が空 = 誰も通さない(fail-closed)。
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Access {
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub routes: BTreeMap<String, Route>,
    /// 通してよい bot の id。現行 access.ts と同じく常に書き出す(空でもキーは残る)。
    #[serde(rename = "allowedBots", default)]
    pub allowed_bots: Vec<String>,
    #[serde(rename = "homeChannel", skip_serializing_if = "Option::is_none")]
    pub home_channel: Option<String>,
    /// 受信 ack のリアクション名。未設定なら slack::ack_emoji が "eyes" を返す。
    #[serde(rename = "ackReaction", skip_serializing_if = "Option::is_none")]
    pub ack_reaction: Option<String>,
    /// 1投稿あたりの本文の上限。既定・上限とも `slack::MAX_CHUNK_LIMIT`。
    #[serde(rename = "textChunkLimit", skip_serializing_if = "Option::is_none")]
    pub text_chunk_limit: Option<usize>,
    /// 切り方。`"newline"` は段落 → 行 → 単語の順に切れ目を探す。既定は上限で断ち切る。
    #[serde(rename = "chunkMode", skip_serializing_if = "Option::is_none")]
    pub chunk_mode: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Access {
    pub fn from_str(src: &str) -> serde_json::Result<Self> {
        serde_json::from_str(src)
    }

    /// 無ければ owner 空 = fail-closed。
    pub fn load(dir: &StateDir) -> Self {
        std::fs::read_to_string(dir.join("access.json"))
            .ok()
            .and_then(|s| Self::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn to_string_pretty(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// **丸ごと上書きしない** — ディスクの現物に、自分が持っているキーだけを重ねる。
    /// 同じファイルに Bridge しか触らないキー(`endpoints` / `pools`)が同居しているので、
    /// 設定を書いた側がそれを消してしまわないようにする。
    pub fn save(&self, dir: &StateDir) -> std::io::Result<()> {
        let mut root = dir.read_json_or("access.json", serde_json::json!({}));
        let mine = serde_json::to_value(self)?;
        match (root.as_object_mut(), mine.as_object()) {
            (Some(root), Some(mine)) => {
                for (k, v) in mine {
                    root.insert(k.clone(), v.clone());
                }
            }
            // ディスクが壊れている / 空 — 自分の姿をそのまま置く
            _ => root = mine,
        }
        dir.write_atomic("access.json", &serde_json::to_string_pretty(&root)?)
    }

    /// チャンネル → 担当マシン。**担当が書かれている行だけ**を集める
    /// (`routes` には作業パスだけの行も居るので、そのまま渡すと全部が「担当あり」になる)。
    pub fn bridges(&self) -> BTreeMap<String, String> {
        self.routes
            .iter()
            .filter_map(|(ch, r)| Some((ch.clone(), r.bridge.clone()?)))
            .collect()
    }

    /// このチャンネルの担当を決める。作業パスなど、同じ行の他の設定は触らない。
    pub fn set_bridge(&mut self, channel: &str, bridge_id: &str) {
        self.routes.entry(channel.to_string()).or_default().bridge = Some(bridge_id.to_string());
    }
}

/// アクセス台帳への1変異。`SetHome` の空文字は Home 解除。
#[derive(Clone, Debug)]
pub enum AccessOp {
    BotAllow(String),
    BotRemove(String),
    SetRepo { channel: String, path: String },
    SetWarm { channel: String, on: bool },
    SetHome(String),
}

/// 変異を純関数として適用する — prev は触らず、新しい Access と人間向けメッセージ・警告を返す。
/// 検証失敗は Err(呼び手が出す)。**認可はしない**。移植元。
/// メッセージ・警告・エラーの文言は原文コピー。
impl Access {
    /// 変異を純関数として適用する — self は触らず、新しい Access と人間向けメッセージ・
    /// 警告を返す。検証失敗は Err(呼び手が出す)。**認可はしない**。
    pub fn apply(&self, op: AccessOp) -> Result<(Access, String, Vec<String>), String> {
        let mut access = self.clone();
        let mut warnings = Vec::new();
        let message = match op {
            AccessOp::BotAllow(id) => {
                if !crate::bridge::command::SlackId::is_bot(&id) {
                    return Err(format!("bot_allow expects a bot id (B…), got \"{id}\""));
                }
                if !access.allowed_bots.contains(&id) {
                    access.allowed_bots.push(id.clone());
                }
                crate::t!("Added {id} to the allowed bots.", "{id} を許可 bot に追加しました。")
            }
            AccessOp::BotRemove(id) => {
                if !crate::bridge::command::SlackId::is_bot(&id) {
                    return Err(format!("bot_remove expects a bot id (B…), got \"{id}\""));
                }
                let had = access.allowed_bots.contains(&id);
                access.allowed_bots.retain(|x| *x != id);
                if had {
                    crate::t!("Removed {id} from the allowed bots.", "{id} を許可 bot から外しました。")
                } else {
                    crate::t!("{id} is not an allowed bot.", "{id} は許可 bot ではありません。")
                }
            }
            AccessOp::SetRepo { channel, path } => {
                if !crate::bridge::command::SlackId::is_channel(&channel) {
                    return Err(format!(
                        "set_repo expects a channel id (C…/G…), got \"{channel}\""
                    ));
                }
                if !path.starts_with('/') {
                    return Err(crate::t!(
                        "The project path must be absolute: \"{path}\"",
                        "プロジェクトのパスは絶対パスで指定してください: \"{path}\""
                    ));
                }
                access.routes.entry(channel.clone()).or_default().repo_path = Some(path.clone());
                // `<#C…>` は押せる `#channel-name` に化ける — Owner には自分が打った名前が見える
                // (生の内部 id ではなく)。。
                crate::t!("<#{channel}> now uses the project directory `{path}`.", "<#{channel}> をプロジェクト `{path}` に紐付けました。")
            }
            AccessOp::SetWarm { channel, on } => {
                // warm pool のキーは cwd なので、repo を持つチャンネルにしか意味がない
                // (repo 無しのチャンネルは常駐の Home pool が捌く)。フラグは残すが、そう言う。
                if !crate::bridge::command::SlackId::is_channel(&channel) {
                    return Err(format!(
                        "set_warm expects a channel id (C…/G…), got \"{channel}\""
                    ));
                }
                let route = access.routes.entry(channel.clone()).or_default();
                let has_repo = !route.repo_path.as_deref().unwrap_or("").is_empty();
                route.warm = Some(on);
                if !has_repo {
                    warnings.push(crate::t!("<#{channel}> has no project yet. This takes effect once you set one with `pwd <absolute path>` in that channel.", "<#{channel}> はまだプロジェクトに紐付いていません。そのチャンネルで `pwd ＜絶対パス＞` を設定すると効きます。"));
                }
                if on {
                    crate::t!(
                        "An agent for <#{channel}> will be started ahead of time, so the first message gets a faster reply.",
                        "<#{channel}> のエージェントを前もって起動しておきます。最初のメッセージへの返事が速くなります。"
                    )
                } else {
                    crate::t!(
                        "Agents for <#{channel}> will no longer be started ahead of time. The first message waits for one to start.",
                        "<#{channel}> のエージェントを前もって起動するのをやめました。最初のメッセージは起動を待つぶん遅くなります。"
                    )
                }
            }
            AccessOp::SetHome(channel) => {
                let ch = channel.trim();
                if ch.is_empty() {
                    access.home_channel = None;
                    crate::t!("The home channel is no longer set.", "Home チャンネルの設定を解除しました。")
                } else {
                    if !crate::bridge::command::SlackId::is_channel(ch) {
                        return Err(format!(
                            "set_home_channel expects a channel id (C…/G…), got \"{ch}\""
                        ));
                    }
                    access.home_channel = Some(ch.to_string());
                    crate::t!("<#{ch}> is now the home channel.", "<#{ch}> を Home チャンネルに設定しました。")
                }
            }
        };
        Ok((access, message, warnings))
    }

    /// そのチャンネルのワーカーが立つ場所。明示のルートが無ければ home に落ちる(2番目の戻り値が
    /// その旨 — pwd の「ルート未設定(Home フォールバック)」表示)。**pwd と spawn は同じこれを読む**。
    pub fn repo_path(&self, channel: &str, home: &str) -> (String, bool) {
        match self
            .routes
            .get(channel)
            .and_then(|r| r.repo_path.as_deref())
            .filter(|p| !p.is_empty())
        {
            Some(p) => (p.to_string(), false),
            None => (home.to_string(), true),
        }
    }
}

// ─── ウォームプール: プールの粒度(純ロジック) ──────────────────────

/// 在庫を保つべきプール集合 = **在庫を待たせる cwd の一覧**(ワーカー起動時に固定される)。
/// access のみから決まる純関数。
/// - owner 未設定なら空(誰にも仕えないので事前起動は純粋な無駄)
/// - HOME プールは常に1つ(次の新規 DM / repo 無しチャンネル。opt-out 不可)
/// - あとは routes の **distinct な repo_path** ごとに1つ。 フラグはチャンネル単位
///   だがプールは repo 単位なので、同じ repo を指すチャンネルが1つでも opt-in(`warm != Some(false)`、
///   未設定は opt-in)なら在庫する(OR)。opt-out したチャンネルも、その在庫があれば引き当てる。
impl Access {
    pub fn pool_targets(&self, home_dir: &str) -> Vec<String> {
        if self.owner.is_empty() {
            return Vec::new();
        }
        let mut targets = vec![home_dir.to_string()];
        for cfg in self.routes.values() {
            let repo = match cfg.repo_path.as_deref().filter(|p| !p.is_empty()) {
                // ponytail: 線形探索 — プール数は repo 数(せいぜい数個)。増えたら HashSet に。
                Some(r) if !targets.iter().any(|cwd| cwd == r) => r,
                _ => continue,
            };
            // opt-out は「そのプールを起動する理由」にならないだけ。採用しないので、
            // 同じ repo の別ルートが opt-in なら後から採用される。
            if cfg.warm == Some(false) {
                continue;
            }
            targets.push(repo.to_string());
        }
        targets
    }
}

/// 起動/終了の home 通知をどこへ出すか(判定だけを純関数に)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoticeTarget {
    Home(String),
    /// ponytail: Rust 版は DM を開く経路をまだ持たない。呼び出し側はログ1行で降りる。
    OwnerDm(String),
    None_,
}

impl NoticeTarget {
    /// home_channel > Owner DM > 沈黙(ただし呼び出し側でログは出す)。
    pub fn of(home_channel: Option<&str>, owner: &str) -> NoticeTarget {
        match home_channel {
            Some(ch) => NoticeTarget::Home(ch.to_string()),
            None if !owner.is_empty() => NoticeTarget::OwnerDm(owner.to_string()),
            None => NoticeTarget::None_,
        }
    }
}

/// registry から見た1プールの状態。起動判定に効くのはこの2ビットだけ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolStatus {
    pub present: bool,
    pub gave_up: bool,
}

/// プールを引き当ててよいか。**まだ知らないスレッドだけ**が対象
/// 既存スレッドは自分のセッションで resume/deliver しなければ会話の続きを失う。
impl Threads {
    pub fn should_claim_pool(entry: Option<&ThreadEntry>) -> bool {
        entry.is_none()
    }
}

/// プールから引き当てた worker をスレッドに縛るときの threads.json エントリの器
/// `agent_id` は呼び出し側が claimed.session_id を差し込む。
impl ThreadEntry {
    pub fn for_pool_assignment(channel_id: &str, cwd: &str, topic: Option<String>) -> ThreadEntry {
        ThreadEntry {
            channel_id: Some(channel_id.to_string()),
            repo_path: Some(cwd.to_string()),
            topic,
            ..Default::default()
        }
    }
}

impl PoolStatus {
    /// このプールを今から起動すべきか。**諦めた worker は再試行しない**
    /// (MCP が上がらない環境で spawn を無限に繰り返さないため)。
    pub fn needs_launch(this: Option<PoolStatus>) -> bool {
        match this {
            None => true,
            Some(s) => !s.present && !s.gave_up,
        }
    }

    /// 起動中のプールを諦める頃合いか(worker.ts の MCP_INIT_TIMEOUT_MS 判定と同型)。
    pub fn should_give_up(spawned_at_ms: u64, now_ms: u64, timeout_ms: u64) -> bool {
        now_ms.saturating_sub(spawned_at_ms) > timeout_ms
    }
}

/// 起動時、[`Pools`] に指名の残っている在庫1本をどう扱うか(純ロジック)。
#[derive(Debug, PartialEq, Eq)]
pub enum PoolRestore {
    /// 実体が生きている — このプロセスの在庫として引き取る(スレッドワーカーの継承と同じ)。
    Adopt,
    /// 実体は死んでいる — 指名はそのまま残し、補充が同じ session_id を `--resume` で起こす。
    Respawn,
    /// もうプール対象ではない cwd — 実体を畳んで指名も捨てる。
    Discard,
}

impl PoolRestore {
    pub fn decide(configured: bool, alive: bool) -> Self {
        match (configured, alive) {
            (false, _) => Self::Discard,
            (true, true) => Self::Adopt,
            (true, false) => Self::Respawn,
        }
    }
}

// ─── 判断: dedup → gate → decide ────────────────────────────────────────────

/// 受信メッセージの出どころ。
///
/// DM とチャンネルで**言い方も既定も変わる**(DM の `pwd` は拒む /
/// チャンネルは mention の要否が違う)ので、素性を型で持つ。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChannelKind {
    Dm,
    Channel,
}

/// 受信メッセージに付いていた添付1つ。先読みダウンロードの入力。
///
/// `name` は劣化ノートに出す表示名 — Slack が name を寄越さなければ id そのもの。
#[derive(Clone, Debug)]
pub struct InboundFile {
    pub id: String,
    pub name: String,
}

/// Bridge の語彙。slack.rs が生成し、worker/endpoints も参照する。
#[derive(Clone, Debug)]
pub struct InboundMsg {
    pub channel: String,
    pub channel_kind: ChannelKind,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub user: Option<String>,
    pub is_bot: bool,
    /// Slack が付ける bot の id。`allow-bot` で許した相手かを照合するのに要る。
    pub bot_id: Option<String>,
    pub text: String,
    /// Slack の `files[]`(slack.rs が埋める)。
    pub files: Vec<InboundFile>,
    /// 先読みダウンロードの結果 — 成功したローカルパスと、失敗の劣化ノート。
    /// 受信経路(main.rs)が配達の直前に埋める。queue された分もこの値ごと持ち越す。
    pub file_paths: Vec<String>,
    pub file_errors: Vec<String>,
    /// このメッセージがリアクション由来ならその素性。ワーカーには合成テキストで
    /// 届く(`text` に入っている)が、**進捗付箋に付いた stop 絵文字だけ**は配達せず停止に使う。
    pub reaction: Option<Reaction>,
    /// ユーザーが**消した**メッセージの ts。埋まっていれば、これは削除の合図で
    /// `text` は取り消しの指示文(`deletion_notice`)。
    pub deleted_ts: Option<String>,
    /// ユーザーが**書き換えた**メッセージの ts と、その改訂 id(dedup 用)。
    /// 埋まっていれば `text` は**新しい本文そのもの** — 指示文は Bridge が組む
    /// (「いま処理中のものか」で言い方が変わり、それを知っているのは Bridge だけ)。
    pub edited: Option<Edited>,
}

/// 書き換え1件。`revision` は Slack の `edited.ts`(同じ編集が再配達されたときの目印)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edited {
    pub ts: String,
    pub revision: String,
}

/// 書き換えをワーカーに伝える文。
/// `is_current` = いま処理中の依頼が書き換わった(捨てて新しい方をやる)/ そうでなければ
/// 過去の依頼の改訂(いまの仕事は続け、手が空いてから)。
pub fn edit_notice(edited_ts: &str, new_text: &str, had_files: bool, is_current: bool) -> String {
    const SNIPPET_MAX: usize = 1000;
    let trimmed = new_text.trim();
    let content_desc = if trimmed.is_empty() {
        if had_files {
            "It now has no text (a file/attachment only).".to_string()
        } else {
            "Its new text is unavailable.".to_string()
        }
    } else if trimmed.chars().count() > SNIPPET_MAX {
        let head: String = trimmed.chars().take(SNIPPET_MAX).collect();
        format!("The new content is:\n\"\"\"\n{head}… (truncated)\n\"\"\"")
    } else {
        format!("The new content is:\n\"\"\"\n{trimmed}\n\"\"\"")
    };
    let lead = if is_current {
        format!(
            "[message_edited] The user EDITED the message you are CURRENTLY working on \
             (id {edited_ts}) — they changed the request, so your in-progress turn was \
             interrupted. Throw away the work you were doing for the OLD wording and handle the \
             NEW content instead. {content_desc}"
        )
    } else {
        format!(
            "[message_edited] The user EDITED an earlier message (id {edited_ts}) that you had \
             already moved past — they revised that request. Do NOT abandon your current work; \
             once you are free, handle the revised request. {content_desc}"
        )
    };
    format!(
        "{lead}\n\nRespond to the now-edited request as you normally would; if the edit changes \
         nothing you need to do, call no_reply."
    )
}

/// 取り消しをワーカーに伝える文。
/// **「消されました」とだけ言い返させない** — まだ外に出していない作業は捨てる、
/// もう外に出した副作用があるときだけ説明する、という判断をさせる。
pub fn deletion_notice(deleted_ts: &str, text: &str, had_files: bool) -> String {
    const SNIPPET_MAX: usize = 1000;
    let trimmed = text.trim();
    let content_desc = if trimmed.is_empty() {
        if had_files {
            "It had no text (it was a file/attachment upload).".to_string()
        } else {
            "Its text is unavailable.".to_string()
        }
    } else if trimmed.chars().count() > SNIPPET_MAX {
        let head: String = trimmed.chars().take(SNIPPET_MAX).collect();
        format!("Its content was:\n\"\"\"\n{head}… (truncated)\n\"\"\"")
    } else {
        format!("Its content was:\n\"\"\"\n{trimmed}\n\"\"\"")
    };
    format!(
        "[message_deleted] The user DELETED their own message (id {deleted_ts}). They removed \
         that request from the conversation — handle it as if the message had never been sent. \
         {content_desc}\n\nDecide based on what you have ACTUALLY done for it so far:\n\
         • If you have NOT yet performed any external/irreversible action for it — including the \
         case where you only prepared, computed, or were about to send an answer — then DISCARD \
         that work: call no_reply and output nothing. A not-yet-sent answer is NOT a side-effect; \
         throw it away.\n\
         • Only reply if you ALREADY performed a real external side-effect that the user must know \
         about or that needs reverting (e.g. created/edited a file, ran a command with lasting \
         effects, or already posted a message), in which case briefly explain and/or undo it.\n\
         Never output any text merely stating that the message was deleted."
    )
}

/// ループ遮断のしきい値— 同じスレッドで bot の発言がこれだけ続いたら、
/// 人が話しかけるまで止める。
pub const LOOP_LIMIT: u64 = 5;

/// リアクション1つ。
///
/// `item_ts` は**付けられた側のメッセージ**の ts — 付箋かどうかをこれで見分ける
/// (付箋以外への stop 絵文字はただのリアクション)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reaction {
    pub emoji: String,
    pub item_ts: String,
    pub added: bool,
}

/// stop として扱うリアクション名(6つ)。`hand` と `raised_hand` は
/// 同じ ✋ の別名なので、どちらを選んでも通す。**コロンは付かない** — Slack のリアクション名は
/// `red_circle` の形で来る(本文の `:red_circle:` とは別物)。
const STOP_REACTION_NAMES: [&str; 6] = [
    "red_circle",
    "octagonal_sign",
    "black_square_for_stop",
    "hand",
    "raised_hand",
    "x",
];

impl Reaction {
    pub fn is_stop(&self) -> bool {
        self.added && STOP_REACTION_NAMES.contains(&self.emoji.as_str())
    }

    /// ワーカーに渡す合成テキスト。リアクションは
    /// 軽い合図なので、**黙らないように**と念を押す一文が付く(added のときだけ)。
    pub fn synthetic_text(&self, reactor: &str, item_text: &str) -> String {
        let truncated: String = if item_text.chars().count() > 280 {
            item_text.chars().take(280).chain(['…']).collect()
        } else {
            item_text.to_string()
        };
        let kind = if self.added { "added" } else { "removed" };
        let verb = if self.added {
            "reacted with"
        } else {
            "removed reaction"
        };
        let hint = if self.added {
            " Do not stay silent: at minimum call the `react` tool to acknowledge (e.g. mirror \
             the emoji, or use 👍/🙏/🤔 as appropriate). Reply with text only if the reaction \
             calls for a substantive response."
        } else {
            ""
        };
        format!(
            "[reaction {kind}] <@{reactor}> {verb} :{}: on your message: {truncated}{hint}",
            self.emoji
        )
    }
}

/// 門番の答え — この1通をワーカーに渡すか。
///
/// `Drop` の `&'static str` は落とした理由で、そのままログに出る(黙って捨てない)。
/// 判断そのものは [`Access::gate`]。
#[derive(PartialEq, Eq, Debug)]
pub enum GateVerdict {
    Serve,
    /// Owner 以外の人が、動いているスレッドで喋った。ワーカーには**文脈として**
    /// 渡す(返事は期待しない)。落とすとスレッドの会話が歯抜けになる。
    Context,
    Drop(&'static str),
}

/// ワーカーの生存。`bridge/worker.rs` の `Workers` が facts から導く。
///
/// `agent::SpawnReq` もこの型を借りている — 依存の向きの唯一の例外(座席チェックの
/// エラー文言が `{state:?}` を含むため)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WorkerState {
    Absent,
    Starting,
    Ready,
}

/// 門を通った1通をどう捌くか — 「起こす / 再開する / 渡す / 溜める」の4通りしかない。
///
/// スレッドの記録とワーカーの生死だけで決まる([`Action::decide`])。
#[derive(PartialEq, Eq, Debug)]
pub enum Action {
    SpawnNew,
    SpawnResume(String),
    Deliver,
    Queue,
}

/// 疑わしきは Drop(fail-closed)。owner 空の access は誰も通さない。
impl Access {
    /// このチャンネルで「以後訊かない」と押されたツールか。
    pub fn channel_tool_allowed(&self, channel: &str, tool: &str) -> bool {
        self.routes
            .get(channel)
            .and_then(|r| r.allowed_tools.as_ref())
            .is_some_and(|v| v.iter().any(|t| t == tool))
    }

    /// 「以後このチャンネルでは訊かない」を覚える。**人がボタンを押したときだけ**呼ばれる。
    pub fn grant_channel_tool(&mut self, channel: &str, tool: &str) {
        if channel.is_empty() || tool.is_empty() {
            return;
        }
        let allowed = self
            .routes
            .entry(channel.to_string())
            .or_default()
            .allowed_tools
            .get_or_insert_with(Vec::new);
        if !allowed.iter().any(|t| t == tool) {
            allowed.push(tool.to_string());
        }
    }

    /// 入れる / 文脈として入れる / 落とす の3値(`decideChannelAccess` と
    /// `decideDmAccess`)。
    ///
    /// **チャンネルが access.json に載っているかは見ない。** 登録(routes)が持っているのは
    /// 「そのチャンネルでどのフォルダを触るか」で、入れる判断には使わない — 未登録の
    /// チャンネルでも Owner のメンションには応える(現行の判定と同じ)。
    ///
    /// チャンネルでは**メンション**か**既に動いているスレッド**が要る。これが無いと、
    /// 登録済みチャンネルの雑談まで全部ワーカーに流れる。
    pub fn gate(&self, msg: &InboundMsg, is_mention: bool, is_active_thread: bool) -> GateVerdict {
        let dm = msg.channel_kind == ChannelKind::Dm;
        // bot を DM に入れる道は無い(`isBotDMBlocked`)
        if msg.is_bot && dm {
            return GateVerdict::Drop("bot-dm-blocked");
        }
        if self.owner.is_empty() {
            return GateVerdict::Drop(if dm { "dm-no-owner" } else { "no-owner" });
        }
        // Slack Web API 経由の投稿には**人が書いたものでも** bot_id が付く。
        // それでも Slack は本当の `user` を刻む(トークン由来なので本文からは詐称できない)ので、
        // その人が Owner なら人として扱う。Owner 以外・user 無しは bot(閉じる方に倒す)
        let is_owner = msg.user.as_deref() == Some(self.owner.as_str());
        if dm {
            return if is_owner {
                GateVerdict::Serve
            } else {
                GateVerdict::Drop("dm-not-owner")
            };
        }
        let reachable = is_mention || is_active_thread;
        if msg.is_bot && !is_owner {
            // Owner が allow-bot で許した bot だけ。それ以外は名指しでも通さない
            let allowed = msg
                .bot_id
                .as_deref()
                .is_some_and(|id| self.allowed_bots.iter().any(|b| b == id));
            if !allowed {
                return GateVerdict::Drop("drop-bot-not-allowed");
            }
            return if reachable {
                GateVerdict::Serve
            } else {
                GateVerdict::Drop("require-mention-unmet")
            };
        }
        if is_owner {
            return if reachable {
                GateVerdict::Serve
            } else {
                GateVerdict::Drop("require-mention-unmet")
            };
        }
        // Owner 以外の人 — 動いているスレッドの中でだけ**文脈として**渡す(返事はさせない)
        if is_active_thread {
            GateVerdict::Context
        } else {
            GateVerdict::Drop("drop-not-owner")
        }
    }
}

const DEDUP_CAP: usize = 512;

/// 同一 (channel, ts) の再配達を落とす覚え書き。1メッセージが複数イベントで届くため必須
///
/// ponytail: 上限512の線形スキャン。イベント率が上がるなら HashSet + VecDeque に。
#[derive(Default)]
pub struct RecentDeliveries {
    seen: std::collections::VecDeque<(String, String)>,
}

impl RecentDeliveries {
    pub fn new() -> Self {
        Self::default()
    }

    /// 既出なら true。初出なら覚えて false。
    pub fn seen(&mut self, channel: &str, ts: &str) -> bool {
        if self.seen.iter().any(|(c, t)| c == channel && t == ts) {
            return true;
        }
        if self.seen.len() >= DEDUP_CAP {
            self.seen.pop_front();
        }
        self.seen.push_back((channel.to_string(), ts.to_string()));
        false
    }
}

/// entry 無し → SpawnNew / Ready → Deliver / Starting → Queue / Absent → SpawnResume。
impl Action {
    pub fn decide(entry: Option<&ThreadEntry>, worker: WorkerState) -> Action {
        let Some(entry) = entry else {
            return Action::SpawnNew;
        };
        match worker {
            WorkerState::Ready => Action::Deliver,
            WorkerState::Starting => Action::Queue,
            // 過去のセッションを知らない entry は再開できない → 新規で建てる
            WorkerState::Absent => match entry.agent_id.as_deref() {
                Some(sid) => Action::SpawnResume(sid.to_string()),
                None => Action::SpawnNew,
            },
        }
    }
}

// ─── disposition: 台帳と Stop 契約 ──────────────────────────────────────────

/// 配達済みで**まだ応答されていない**メッセージの台帳。thread_key → 未応答の並び。
///
/// 状態は track / received / disposed の3つ。移植元のmakeInflightTracker。
///
/// ponytail: スレッドあたり数件の Vec 線形スキャン。挿入順が要る(pending の順序が
/// そのまま再送・ログの順序)ので HashMap は使わない。
#[derive(Default)]
pub struct Ledger {
    /// 空になったキーは消す(存在するキー = 未応答が1件以上ある)。
    by_key: HashMap<ThreadKey, Vec<Inflight>>,
    /// 変更あり。書き出すのは 500ms tick の [`Ledger::flush`] 1箇所だけ — 変更メソッドは
    /// ここを立てるだけなので、台帳を触る側が save を呼び忘れて落とす経路が作れない。
    dirty: bool,
}

/// 未応答1件。threads.json の entry の中に `inflight` として並ぶ。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Inflight {
    id: String,
    received: bool,
    /// 配達した封筒。**再送はこれが無いと成り立たない** — 現行の inflight はメッセージ本体を
    /// 持っているが、こちらは長く id しか持っていなかった。配達前は None。
    envelope: Option<String>,
    /// ターン失敗で使った再送回数。**項目に同居させる**ので、応答されて項目が落ちれば
    /// 予算も一緒に消える(現行は別の Map + 明示的な後始末を持っている)。
    retries: u32,
}

impl Ledger {
    /// pending.json から拾う(起動時に1回)。**台帳が揮発すると、再起動をまたいだスレッドは
    /// 沈黙の見張りの対象から外れる** — 未応答を1件も知らないので「考え中」が二度と出ない。
    /// どのスレッドを実際に載せ直すかは呼び手が [`Ledger::retain_keys`] で決める(生きている
    /// ワーカーの分だけ)。
    pub fn load(threads: &Threads) -> Self {
        let by_key = threads
            .entries
            .iter()
            .filter(|(_, e)| !e.inflight.is_empty())
            .filter_map(|(ts, e)| {
                Some((
                    ThreadKey::new(e.channel_id.as_deref()?, ts),
                    e.inflight.clone(),
                ))
            })
            .collect();
        Self {
            by_key,
            dirty: false,
        }
    }

    /// 変わっていれば threads.json へ落とす。呼ぶのは 500ms tick と、降りる直前の1回。
    ///
    /// **台帳の姿をそのまま entry に映す**(消えた鍵は空になる)。threads.json を書くのは
    /// ここも含めて `Threads::save` 1本なので、二重に書く経路は増えない。
    ///
    /// ponytail: 粒度は tick 1つ分。プロセスが即死すると最後の 500ms 分を失うが、
    /// 失うのは「答えを待っている印」だけで、メッセージ本体でも配達記録でもない。
    pub fn flush(&mut self, threads: &mut Threads) -> Option<std::io::Result<()>> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        let mut want: HashMap<String, Vec<Inflight>> = self
            .by_key
            .iter()
            .filter_map(|(k, v)| k.split().1.map(|ts| (ts, v.clone())))
            .collect();
        for (ts, e) in threads.entries.iter_mut() {
            let next = want.remove(ts).unwrap_or_default();
            if e.inflight != next {
                e.inflight = next;
            }
        }
        // entry を持たない鍵は落ちる。配達の前に entry ができるので普通は起きない
        for ts in want.keys() {
            LogCtx::default().debug(
                "bridge",
                &format!("ledger: dropping undisposed for an unknown thread tts={ts}"),
            );
        }
        Some(threads.save())
    }

    /// 起動時の復元 — `keep` にある鍵だけ残す。**生きているワーカーのスレッドだけ**を
    /// 渡すこと。死んだスレッドの未応答を載せると、返事が来ないまま見張りが永久に居座る。
    pub fn retain_keys(&mut self, keep: &[ThreadKey]) {
        let before = self.by_key.len();
        self.by_key.retain(|k, _| keep.contains(k));
        self.dirty = self.dirty || self.by_key.len() != before;
    }

    /// 配達を記録。再配達は同じ id の received をリセットする。
    pub fn track(&mut self, key: &ThreadKey, id: &str) {
        let entries = self.by_key.entry(key.clone()).or_default();
        match entries.iter_mut().find(|e| e.id == id) {
            Some(e) => e.received = false,
            None => entries.push(Inflight {
                id: id.to_string(),
                received: false,
                envelope: None,
                retries: 0,
            }),
        }
        self.dirty = true;
    }

    /// 配達した封筒を覚える。`track` は ack の直後(封筒がまだ無い時点)なので、
    /// 封筒を作った側から預ける。
    pub fn remember_envelope(&mut self, key: &ThreadKey, id: &str, envelope: &str) {
        if let Some(e) = self
            .by_key
            .get_mut(key)
            .and_then(|v| v.iter_mut().find(|e| e.id == id))
        {
            e.envelope = Some(envelope.to_string());
            self.dirty = true;
        }
    }

    /// 再送できる未応答 `(id, 封筒)`。封筒を覚えていない項目は出さない(再送しようが無い)。
    pub fn undisposed(&self, key: &ThreadKey) -> Vec<(String, String)> {
        self.by_key
            .get(key)
            .map(|v| {
                v.iter()
                    .filter_map(|e| e.envelope.clone().map(|env| (e.id.clone(), env)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 再送予算を1回分引く。**全員に残っているときだけ**引いて true。1人でも尽きていれば
    /// 誰からも引かず false(以前の実装は予算切れを見つけた時点で return する
    /// ので、引きかけを残さない)。
    pub fn spend_retry(&mut self, key: &ThreadKey, ids: &[String], cap: u32) -> bool {
        let Some(entries) = self.by_key.get_mut(key) else {
            return false;
        };
        let mine = |e: &&mut Inflight| ids.contains(&e.id);
        if entries.iter_mut().filter(mine).count() != ids.len()
            || entries.iter_mut().filter(mine).any(|e| e.retries >= cap)
        {
            return false;
        }
        for e in entries.iter_mut().filter(|e| ids.contains(&e.id)) {
            e.retries += 1;
        }
        self.dirty = true;
        true
    }

    /// 未受領を受領にし、**新しく受領になった id だけ**返す(冪等 — 2回目は空)。
    /// 🤖 flip と milestone received の駆動。受領しても応答済みではないので台帳には残る。
    pub fn mark_received(&mut self, key: &ThreadKey) -> Vec<String> {
        let Some(entries) = self.by_key.get_mut(key) else {
            return Vec::new();
        };
        let flipped: Vec<String> = entries
            .iter_mut()
            .filter(|e| !e.received)
            .map(|e| {
                e.received = true;
                e.id.clone()
            })
            .collect();
        self.dirty = self.dirty || !flipped.is_empty();
        flipped
    }

    /// disposition が覆った id を台帳から落とす。
    pub fn disposed(&mut self, key: &ThreadKey, ids: &[String]) {
        let Some(entries) = self.by_key.get_mut(key) else {
            return;
        };
        entries.retain(|e| !ids.contains(&e.id));
        if entries.is_empty() {
            self.by_key.remove(key);
        }
        self.dirty = true;
    }

    /// ids 無しの disposition はスレッドを全消化する。落とした id を返す。
    pub fn dispose_all(&mut self, key: &ThreadKey) -> Vec<String> {
        let dropped: Vec<String> = self
            .by_key
            .remove(key)
            .map(|entries| entries.into_iter().map(|e| e.id).collect())
            .unwrap_or_default();
        self.dirty = self.dirty || !dropped.is_empty();
        dropped
    }

    /// まだ受領していない id(非破壊 — transcript スキャンが「何を探すか」を知るため)。
    pub fn unreceived(&self, key: &ThreadKey) -> Vec<String> {
        self.by_key
            .get(key)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|e| !e.received)
                    .map(|e| e.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 指定 id のうち**新しく受領になった分だけ**返す(冪等)。transcript 経由の受信確認用 —
    /// スレッド一括の `mark_received` と違い、ワーカーが実際に読んだ分だけを立てる。
    pub fn mark_received_ids(&mut self, key: &ThreadKey, ids: &[String]) -> Vec<String> {
        let Some(entries) = self.by_key.get_mut(key) else {
            return Vec::new();
        };
        let flipped: Vec<String> = entries
            .iter_mut()
            .filter(|e| !e.received && ids.contains(&e.id))
            .map(|e| {
                e.received = true;
                e.id.clone()
            })
            .collect();
        self.dirty = self.dirty || !flipped.is_empty();
        flipped
    }

    /// 未応答を抱えたスレッド鍵(`pendingKeys`)。再起動の予告を出す先。
    pub fn pending_keys(&self) -> Vec<ThreadKey> {
        self.by_key.keys().cloned().collect()
    }

    /// この id を未応答に抱えているスレッド。削除イベントは根を寄越さないことが
    /// あるので、**消された ts から本当のスレッドを引く**のに使う。
    pub fn key_of_id(&self, id: &str) -> Option<ThreadKey> {
        self.by_key
            .iter()
            .find(|(_, entries)| entries.iter().any(|e| e.id == id))
            .map(|(key, _)| key.clone())
    }

    /// 未応答の id(非破壊 — Stop 契約が読む)。
    pub fn pending(&self, key: &ThreadKey) -> Vec<String> {
        self.by_key
            .get(key)
            .map(|entries| entries.iter().map(|e| e.id.clone()).collect())
            .unwrap_or_default()
    }
}

/// 封筒の `message_id` が transcript に現れた = ワーカーがそれを読んだ。ターン中に
/// send-keys した分は UserPromptSubmit が発火しない(ステアリング消費)ので、受信確認は
/// これが唯一の証拠になる。移植元 extractTranscriptMessageIds。
///
/// transcript は JSONL — 封筒は JSON 文字列の中にいるので**引用符はエスケープされている**
/// (実測: `message_id=\"1783500885.490429\"`)。そこで `message_id` の後ろの区切り文字の
/// 並びを読み飛ばして id に当てる(現行の正規表現 `message_id[\\"':=\s]*` と同じ集合)。
/// 複数形の `message_ids`(= 覆いの申告であって受領ではない)は末尾の `s` で弾かれる。
impl Ledger {
    pub fn find_received_ids(new_bytes: &str, ids: &[String]) -> Vec<String> {
        let sep = |c: char| matches!(c, '\\' | '"' | '\'' | ':' | '=' | ' ' | '\t' | '\n' | '\r');
        ids.iter()
            .filter(|id| {
                new_bytes.match_indices(id.as_str()).any(|(at, _)| {
                    // 直後が数字なら別の(より長い)id の一部
                    !new_bytes[at + id.len()..].starts_with(|c: char| c.is_ascii_digit())
                        && new_bytes[..at]
                            .trim_end_matches(sep)
                            .ends_with("message_id")
                })
            })
            .cloned()
            .collect()
    }
}

/// 沈黙見張りの 500ms tick が1スレッドに対して下す判断。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum StallAction {
    /// そのまま。
    Nothing,
    /// `is thinking…` を立てる。
    Fire,
    /// 見張りを畳む(= ステータスを消す)。未応答が空 = 会話として決着した。
    Settle,
}

/// tick の判断。`has_pending` は台帳に未応答が残っているか。
///
/// **決着が最優先** — 未応答が空なら、無音だろうが既に出していようが畳む。台帳が空になる
/// 経路は disposition だけでなく `terminate`(exit / logout / resume)もあり、そちらは
/// 自前でステータスを消さない。ここが唯一の受け皿なので取りこぼすと shimmer が居座る。
impl StallAction {
    pub fn of(
        has_pending: bool,
        last_activity_ms: u64,
        now_ms: u64,
        shown: bool,
        silence_ms: u64,
    ) -> StallAction {
        if !has_pending {
            return StallAction::Settle;
        }
        if Self::due(last_activity_ms, now_ms, shown, true, silence_ms) {
            return StallAction::Fire;
        }
        StallAction::Nothing
    }

    /// 発火条件だけ(見張りのタイマーが満期を迎える条件)。
    /// tick 全体の判断は [`StallAction::of`]。
    ///
    /// - `shown` = もう出している → 二度撃たない(現行 `showStall` の冪等)
    /// - `has_pending` = 未応答を抱えている。空 = 会話として決着済みで、現行が
    ///   `if (e.answered) return` で見張りを張り直さないのと同じ
    pub fn due(
        last_activity_ms: u64,
        now_ms: u64,
        shown: bool,
        has_pending: bool,
        silence_ms: u64,
    ) -> bool {
        !shown && has_pending && now_ms.saturating_sub(last_activity_ms) >= silence_ms
    }
}

/// disposition が起きたという通知(slack.rs → main)。kind は "reply"/"react"/"no_reply"/"edit"。
#[derive(Clone, Debug)]
pub struct Disposition {
    pub kind: &'static str,
    pub channel_id: String,
    pub thread_ts: Option<String>,
    pub message_ids: Vec<String>,
    pub session_id: String,
}

/// ワーカーのターンを終わらせてよいか。true なら Stop を block して disposition を促す。
/// 移植元 (shouldBlockStopForThread)。
///
/// - スレッドが引けない → 強制する相手がいない → 通す
/// - `stop_hook_active` = この停止自体が既に再プロンプト後 → 再プロンプトは1回で打ち止め
/// - MCP 未準備 → **fail-open**(disposition ツールがまだ無いのに「reply しろ」と
///   言っても空振りするだけ。取りこぼしは warm-push / 回復に任せる)
impl Disposition {
    pub fn should_block_stop(
        thread_resolved: bool,
        stop_hook_active: bool,
        mcp_ready: bool,
        pending: usize,
    ) -> bool {
        thread_resolved && mcp_ready && !stop_hook_active && pending > 0
    }

    /// Stop hook の block 応答。permission の `{decision:{behavior}}` とは別形の平たい形
    /// `pending` = まだ消化されていない message_id。**必ず文面に入れる** — 入れないと
    /// ワーカーは「どれが残っているか」を当てずっぽうで選び、既に返信済みの id に
    /// `no_reply` を撃って台帳が減らない。減らないので次の Stop でまた block され、
    /// その間ずっと見張りが `is thinking…` を張り直し続ける(応答後も shimmer が居座る)。
    pub fn block_output(pending: &[String]) -> serde_json::Value {
        let reason = format!(
            "{STOP_BLOCK_REASON} Outstanding message_ids: [{}]",
            pending.join(", ")
        );
        serde_json::json!({ "decision": "block", "reason": reason })
    }
}

/// block したワーカーに渡す文面。Claude Code 既定の枠組み(平文=配達、外向き
/// アクションは要確認)を明示的に打ち消す。**原文コピー** —。
pub const STOP_BLOCK_REASON: &str = "You ended your turn without delivering. Your prose streams to Slack live, but it is not the delivered answer — dispose the received message with `reply` (answer), `react` (emoji ack), or `no_reply` (nothing). A reply is your answer, not an outward action: do NOT ask whether to send it. Call reply/react/no_reply (with the message_id) now.";

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn port_is_remembered_across_calls() {
        let dir = std::env::temp_dir().join(format!("sc-bridge-port-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = StateDir::at(dir);
        let first = dir.remembered_port("hook", || 8791);
        assert_eq!(first, 8791);
        // 2回目は採番しない(呼ばれたら panic)
        let second = dir.remembered_port("hook", || panic!("must not re-allocate"));
        assert_eq!(
            second, 8791,
            "worker bakes the URL in — the port must not move"
        );
    }

    #[test]
    fn notice_target_prefers_home_channel() {
        assert!(matches!(NoticeTarget::of(Some("C1"), "U1"), NoticeTarget::Home(c) if c == "C1"));
    }

    #[test]
    fn notice_target_falls_back_to_owner_dm() {
        assert!(matches!(NoticeTarget::of(None, "U1"), NoticeTarget::OwnerDm(u) if u == "U1"));
    }

    #[test]
    fn notice_target_silent_without_either() {
        assert!(matches!(NoticeTarget::of(None, ""), NoticeTarget::None_));
    }

    /// 書き換えの文は「いま処理中か」で言い方が変わる。処理中なら古い作業を捨てさせ、
    /// 過去の依頼なら今の仕事を続けさせる。
    #[test]
    fn an_edit_tells_the_worker_to_redo_only_when_it_is_the_current_work() {
        let now = edit_notice("1.2", "こっちでお願い", false, true);
        assert!(now.contains("the message you are CURRENTLY working on (id 1.2)"));
        assert!(now.contains("Throw away the work you were doing for the OLD wording"));
        assert!(now.contains("The new content is:\n\"\"\"\nこっちでお願い\n\"\"\""));

        let past = edit_notice("1.2", "こっちでお願い", false, false);
        assert!(past.contains("EDITED an earlier message (id 1.2)"));
        assert!(past.contains("Do NOT abandon your current work"));
        // どちらも締めは同じ — 何も変わらないなら黙る
        for n in [&now, &past] {
            assert!(n.ends_with("if the edit changes nothing you need to do, call no_reply."));
        }
        // 本文が消えた編集は、添付だけになったのか読めないのかで言い分ける
        assert!(edit_notice("1.2", "", true, true).contains("a file/attachment only"));
        assert!(edit_notice("1.2", "", false, true).contains("Its new text is unavailable."));
        assert!(edit_notice("1.2", &"あ".repeat(1500), false, true).contains("… (truncated)"));
    }

    /// 取り消しの文は「消された」とだけ言い返させない形になっている。本文は引用し、
    /// 長すぎるものは切る。台帳からは消された id だけを引ける。
    #[test]
    fn a_deletion_tells_the_worker_to_discard_and_the_ledger_finds_its_thread() {
        let n = deletion_notice("1.2", "  やっぱりやめて  ", false);
        assert!(n.starts_with("[message_deleted] The user DELETED their own message (id 1.2)."));
        assert!(n.contains("Its content was:\n\"\"\"\nやっぱりやめて\n\"\"\""));
        assert!(n.contains("call no_reply and output nothing"));
        assert!(n.ends_with("Never output any text merely stating that the message was deleted."));
        // 本文が無いとき: 添付だったかどうかで言い分けが変わる
        assert!(deletion_notice("1.2", "", true).contains("it was a file/attachment upload"));
        assert!(deletion_notice("1.2", "", false).contains("Its text is unavailable."));
        // 1000 文字で切る
        assert!(deletion_notice("1.2", &"あ".repeat(1500), false).contains("… (truncated)"));

        // 削除イベントは根を寄越さないことがある — 消された id からスレッドを引く
        let mut l = Ledger::default();
        let key = ThreadKey::parse("C1:1.0");
        l.track(&key, "1.2");
        assert_eq!(l.key_of_id("1.2"), Some(key));
        assert_eq!(l.key_of_id("9.9"), None, "答え済み・無関係な id は引けない");
    }

    /// stop になるのは**付けられた**stop 絵文字だけ。合成テキストは Bun の原文。
    #[test]
    fn a_stop_reaction_is_only_a_stop_when_added() {
        let r = |emoji: &str, added: bool| Reaction {
            emoji: emoji.into(),
            item_ts: "1.1".into(),
            added,
        };
        assert!(r("red_circle", true).is_stop());
        assert!(r("raised_hand", true).is_stop(), "hand の別名も通す");
        assert!(!r("red_circle", false).is_stop(), "外したのは停止ではない");
        assert!(!r("eyes", true).is_stop());

        let added = r("tada", true).synthetic_text("U1", "done");
        assert!(
            added.starts_with("[reaction added] <@U1> reacted with :tada: on your message: done"),
            "{added}"
        );
        assert!(added.contains("Do not stay silent"));
        let removed = r("tada", false).synthetic_text("U1", "done");
        assert!(
            removed.starts_with(
                "[reaction removed] <@U1> removed reaction :tada: on your message: done"
            ),
            "{removed}"
        );
        assert!(
            !removed.contains("Do not stay silent"),
            "外した側は促さない"
        );
        // 長い本文は 280 文字で切る
        let long = r("eyes", true).synthetic_text("U1", &"あ".repeat(400));
        assert!(long.contains(&format!("{}…", "あ".repeat(280))));
    }

    fn msg(kind: ChannelKind, user: Option<&str>, is_bot: bool) -> InboundMsg {
        InboundMsg {
            channel: match kind {
                ChannelKind::Dm => "D1".into(),
                ChannelKind::Channel => "C1".into(),
            },
            channel_kind: kind,
            ts: "1.1".into(),
            thread_ts: None,
            user: user.map(str::to_string),
            is_bot,
            bot_id: is_bot.then(|| "B1".to_string()),
            text: "hi".into(),
            files: Vec::new(),
            file_paths: Vec::new(),
            file_errors: Vec::new(),
            reaction: None,
            deleted_ts: None,
            edited: None,
        }
    }

    fn access_with_route() -> Access {
        Access::from_str(r#"{"owner":"U1","routes":{"C1":{"repo_path":"/x"}}}"#).unwrap()
    }

    /// Web API 経由の投稿は**人が書いたものでも** bot_id が付く。Owner 本人の
    /// 投稿は人として通し、それ以外の bot は `allow-bot` されていなければ落とす。
    #[test]
    fn gate_drops_bots_but_honors_the_owners_web_api_post() {
        let a = access_with_route();
        assert_eq!(
            a.gate(&msg(ChannelKind::Channel, Some("U1"), true), true, false),
            GateVerdict::Serve
        );
        assert_eq!(
            a.gate(&msg(ChannelKind::Channel, Some("U2"), true), true, false),
            GateVerdict::Drop("drop-bot-not-allowed")
        );
        assert_eq!(
            a.gate(&msg(ChannelKind::Channel, None, true), true, false),
            GateVerdict::Drop("drop-bot-not-allowed")
        );
        // allow-bot された bot は、名指し(かアクティブスレッド)でだけ通る
        let allowed =
            Access::from_str(r#"{"owner":"U1","allowedBots":["B7"],"routes":{}}"#).unwrap();
        let mut b = msg(ChannelKind::Channel, Some("U9"), true);
        b.bot_id = Some("B7".into());
        assert_eq!(allowed.gate(&b, true, false), GateVerdict::Serve);
        assert_eq!(
            allowed.gate(&b, false, false),
            GateVerdict::Drop("require-mention-unmet")
        );
        // bot の DM は無条件で落とす
        let mut dm_bot = msg(ChannelKind::Dm, Some("U9"), true);
        dm_bot.bot_id = Some("B7".into());
        assert_eq!(
            allowed.gate(&dm_bot, true, false),
            GateVerdict::Drop("bot-dm-blocked")
        );
    }

    #[test]
    fn gate_serves_owner_dm() {
        let v = access_with_route().gate(&msg(ChannelKind::Dm, Some("U1"), false), true, false);
        assert_eq!(v, GateVerdict::Serve);
    }

    #[test]
    fn gate_drops_other_user_dm() {
        let v = access_with_route().gate(&msg(ChannelKind::Dm, Some("U2"), false), true, false);
        assert_eq!(v, GateVerdict::Drop("dm-not-owner"));
    }

    /// チャンネルで入れるかは **名指しか、動いているスレッドか** だけで決まる。
    /// **登録(routes)は見ない** — 未登録のチャンネルでも Owner の名指しには応える。
    #[test]
    fn gate_needs_a_mention_or_an_active_thread_not_a_route() {
        let a = access_with_route();
        let owner = msg(ChannelKind::Channel, Some("U1"), false);
        assert_eq!(a.gate(&owner, true, false), GateVerdict::Serve, "名指し");
        assert_eq!(
            a.gate(&owner, false, true),
            GateVerdict::Serve,
            "動いているスレッドの続き"
        );
        assert_eq!(
            a.gate(&owner, false, false),
            GateVerdict::Drop("require-mention-unmet"),
            "名指しでもスレッドの続きでもない雑談は流さない"
        );
        // 未登録チャンネル(C9)でも Owner の名指しは通る
        let mut elsewhere = owner.clone();
        elsewhere.channel = "C9".into();
        assert_eq!(a.gate(&elsewhere, true, false), GateVerdict::Serve);
    }

    /// 渡しそびれた依頼は threads.json に逃がし、次の起動で取り出す。
    /// 同じメッセージを二度積まない(message_id で冪等)。
    #[test]
    fn undelivered_messages_survive_a_restart_through_threads_json() {
        let mut t = Threads::from_str(r#"{}"#).unwrap();
        let msg = |ts: &str, text: &str| InboundMsg {
            channel: "C1".into(),
            channel_kind: ChannelKind::Channel,
            ts: ts.into(),
            thread_ts: Some("1.0".into()),
            user: Some("U1".into()),
            is_bot: false,
            bot_id: None,
            text: text.into(),
            files: Vec::new(),
            file_paths: vec!["/tmp/a.png".into()],
            file_errors: Vec::new(),
            reaction: None,
            deleted_ts: None,
            edited: None,
        };
        assert!(t.threads_with_pending().is_empty());
        t.enqueue_pending("1.0", "C1", &msg("1.1", "ひとつ目"));
        t.enqueue_pending("1.0", "C1", &msg("1.2", "ふたつ目"));
        t.enqueue_pending("1.0", "C1", &msg("1.1", "ひとつ目(再)"));
        assert_eq!(t.threads_with_pending(), vec!["1.0"], "1スレッドに溜まる");

        // 書き出して読み直しても残る(= 再起動をまたぐ)
        let round_tripped = Threads::from_str(&t.to_string_pretty().unwrap()).unwrap();
        let mut t = round_tripped;
        let out = t.drain_pending("1.0");
        assert_eq!(out.len(), 2, "同じ id は二度積まない");
        assert_eq!(out[0].ts, "1.1");
        assert_eq!(out[0].text, "ひとつ目");
        assert_eq!(out[0].thread_ts.as_deref(), Some("1.0"));
        assert_eq!(out[0].file_paths, vec!["/tmp/a.png"], "添付も持ち越す");
        assert!(t.threads_with_pending().is_empty(), "取り出したら消える");
        assert!(t.drain_pending("9.9").is_empty(), "知らないスレッドは空");
    }

    /// ループ遮断 — bot の連投を数え、上限で止め、人が話しかけたら戻す。
    #[test]
    fn the_loop_guard_counts_bot_messages_and_a_human_resumes_the_thread() {
        let mut t = Threads::from_str(r#"{}"#).unwrap();
        assert_eq!(t.bump_bot_streak("1.0"), 0, "知らないスレッドは数えない");
        t.upsert(
            "1.0",
            ThreadEntry {
                channel_id: Some("C1".into()),
                ..Default::default()
            },
        );
        for n in 1..LOOP_LIMIT {
            assert_eq!(t.bump_bot_streak("1.0"), n);
            assert_eq!(t.status_of("1.0"), "active", "上限までは止めない");
        }
        assert_eq!(t.bump_bot_streak("1.0"), LOOP_LIMIT);
        t.pause("1.0");
        assert_eq!(t.status_of("1.0"), "paused");
        assert!(!t.is_active("1.0"), "止まったスレッドは動いていない");

        // 人が話しかけた → 数え直しと再開
        assert!(t.reset_bot_streak("1.0"), "止まっていたことを返す");
        assert_eq!(t.status_of("1.0"), "active");
        assert_eq!(t.bump_bot_streak("1.0"), 1, "連続数は 0 に戻っている");
        assert!(!t.reset_bot_streak("1.0"), "止まっていなければ false");
    }

    /// 一度でも喋ったスレッドは**セッションがまだ無くても**動いている扱い(ユーザー指定)。
    /// status / usage のようにワーカーを起こさないコマンドで始まったスレッドも、続きは
    /// 名指し無しで受け取れる。
    #[test]
    fn a_thread_is_active_once_it_exists_even_without_a_session() {
        let mut t = Threads::from_str(r#"{}"#).unwrap();
        assert!(!t.is_active("1.0"), "知らないスレッドは動いていない");
        t.upsert(
            "1.0",
            ThreadEntry {
                channel_id: Some("C1".into()),
                ..Default::default()
            },
        );
        assert!(t.is_active("1.0"), "agent_id が無くても動いている扱い");
        // pending / paused だけは例外(Bun の getThreadStatus と同じ)
        for status in ["pending", "paused"] {
            let mut e = ThreadEntry {
                channel_id: Some("C1".into()),
                ..Default::default()
            };
            e.extra
                .insert("status".into(), serde_json::Value::String(status.into()));
            t.upsert("2.0", e);
            assert!(!t.is_active("2.0"), "{status} は動いていない");
        }
    }

    /// Owner 以外の人は、動いているスレッドの中でだけ**文脈として**渡す。
    #[test]
    fn gate_passes_a_non_owner_as_context_only_inside_an_active_thread() {
        let a = access_with_route();
        let other = msg(ChannelKind::Channel, Some("U2"), false);
        assert_eq!(a.gate(&other, false, true), GateVerdict::Context);
        assert_eq!(
            a.gate(&other, true, false),
            GateVerdict::Drop("drop-not-owner"),
            "名指しされても Owner でなければ動かさない"
        );
    }

    #[test]
    fn gate_is_fail_closed_without_owner() {
        let empty = Access::default();
        assert!(matches!(
            empty.gate(&msg(ChannelKind::Dm, Some("U1"), false), true, false),
            GateVerdict::Drop(_)
        ));
        assert!(matches!(
            empty.gate(&msg(ChannelKind::Dm, None, false), true, false),
            GateVerdict::Drop(_)
        ));
    }

    #[test]
    fn dedup_drops_second_delivery_of_same_event() {
        let mut d = RecentDeliveries::new();
        assert!(!d.seen("C1", "1.1"), "first sighting is new");
        assert!(
            d.seen("C1", "1.1"),
            "same (channel, ts) must be a duplicate"
        );
        assert!(!d.seen("C1", "1.2"));
        assert!(!d.seen("C2", "1.1"));
    }

    #[test]
    fn dedup_evicts_oldest_past_cap() {
        let mut d = RecentDeliveries::new();
        for i in 0..512 {
            assert!(!d.seen("C1", &format!("{i}")));
        }
        assert!(d.seen("C1", "511"), "newest must still be remembered");
        d.seen("C1", "512"); // 513件目 → 最古(0)が押し出される
        assert!(!d.seen("C1", "0"), "oldest must have been evicted");
    }

    #[test]
    fn decide_table() {
        let entry = ThreadEntry::new("C1", "sid-1");
        assert_eq!(Action::decide(None, WorkerState::Absent), Action::SpawnNew);
        assert_eq!(
            Action::decide(Some(&entry), WorkerState::Ready),
            Action::Deliver
        );
        assert_eq!(
            Action::decide(Some(&entry), WorkerState::Starting),
            Action::Queue
        );
        assert_eq!(
            Action::decide(Some(&entry), WorkerState::Absent),
            Action::SpawnResume("sid-1".into())
        );
    }

    #[test]
    fn decide_spawns_new_when_entry_has_no_session() {
        let entry = ThreadEntry::default();
        assert_eq!(
            Action::decide(Some(&entry), WorkerState::Absent),
            Action::SpawnNew
        );
    }

    #[test]
    fn parses_kv_ignores_comments_blanks_and_strips_quotes() {
        let text = "\
# comment
SLACK_APP_TOKEN=xapp-1-abc

SLACK_BOT_TOKEN=\"xoxb-def\"
EMPTY=
NOEQ_LINE
SPACES = padded ";
        let kv = parse_env(text);
        assert_eq!(
            kv,
            vec![
                ("SLACK_APP_TOKEN".to_string(), "xapp-1-abc".to_string()),
                ("SLACK_BOT_TOKEN".to_string(), "xoxb-def".to_string()),
                ("EMPTY".to_string(), "".to_string()),
                ("SPACES".to_string(), "padded".to_string()),
            ]
        );
    }

    #[test]
    fn state_dir_honors_override() {
        unsafe { std::env::set_var("AGENTGW_STATE_DIR", "/tmp/scdev-test") };
        assert_eq!(StateDir::resolve().path(), PathBuf::from("/tmp/scdev-test"));
        unsafe { std::env::remove_var("AGENTGW_STATE_DIR") };
    }

    /// access.json は持ち主が複数居る。**設定を書く側が、Bridge しか触らないキーを
    /// 消さない**こと — ここが崩れると在庫の指名やポートが黙って飛ぶ。
    #[test]
    fn writing_the_settings_keeps_the_keys_only_the_bridge_touches() {
        let dir = StateDir::at(std::env::temp_dir().join("scrs-access-merge-test"));
        let _ = std::fs::remove_file(dir.join("access.json"));

        // Bridge 側 — 口と在庫の指名を置く
        let port = dir.remembered_port("hook", || 8791);
        let token = dir.remembered_token("hook");
        let mut pools = Pools::load(&dir);
        pools.nominate("/repo", "sid-1");
        pools.save().unwrap();

        // フリート側 — 何も知らずに読んで、owner を足して、丸ごと書き戻す
        let mut access = Access::load(&dir);
        access.owner = "U1".into();
        access.save(&dir).unwrap();

        // 設定も、Bridge しか触らないキーも、両方残っている
        let after = Access::load(&dir);
        assert_eq!(after.owner, "U1");
        assert_eq!(
            dir.remembered_port("hook", || panic!("再割り当てされた")),
            port
        );
        assert_eq!(dir.remembered_token("hook"), token);
        assert_eq!(Pools::load(&dir).session_of("/repo"), Some("sid-1"));

        // 逆向き — Bridge が指名を変えても owner は残る
        let mut pools = Pools::load(&dir);
        pools.nominate("/repo", "sid-2");
        pools.save().unwrap();
        assert_eq!(Access::load(&dir).owner, "U1");
        assert_eq!(Pools::load(&dir).session_of("/repo"), Some("sid-2"));

        let _ = std::fs::remove_file(dir.join("access.json"));
    }

    /// claude に渡す生成物は **state ディレクトリの外**に落ちること。ここが state に
    /// 戻ると、セッションごとの MCP 設定が誰にも消されないまま溜まる(実測 101 個)。
    #[test]
    fn generated_files_land_outside_the_state_dir() {
        let dir = StateDir::at(std::env::temp_dir().join("scrs-runtime-test"));
        let path = dir
            .write_runtime_json("mcp/sid-1.json", &serde_json::json!({"k": 1}))
            .unwrap();

        assert!(
            !path.starts_with(dir.path()),
            "生成物が state に落ちている: {}",
            path.display()
        );
        assert!(path.ends_with("mcp/sid-1.json"));
        assert!(
            path.to_string_lossy().contains("scrs-runtime-test"),
            "dev と本番を分ける印が付いていない: {}",
            path.display()
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            "{\n  \"k\": 1\n}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn log_line_format_is_scp44_compatible() {
        let line = LogCtx {
            session_id: Some("abc".into()),
            thread_key: None,
        }
        .line("info", "bridge", "hello world");
        // 例: 2026-07-27T12:00:00.000Z info bridge pid=123 session=abc hello world
        let parts: Vec<&str> = line.trim_end().splitn(6, ' ').collect();
        assert_eq!(parts[1], "info");
        assert_eq!(parts[2], "bridge");
        assert!(parts[3].starts_with("pid="));
        assert_eq!(parts[4], "session=abc");
        assert_eq!(parts[5], "hello world");
    }

    #[test]
    fn log_line_timestamp_is_iso8601_millis_z() {
        let line = LogCtx {
            session_id: None,
            thread_key: None,
        }
        .line("info", "bridge", "x");
        let ts = line.split(' ').next().unwrap();
        // 2026-07-27T12:00:00.000Z
        assert_eq!(ts.len(), 24, "{ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[19..20], ".");
        assert!(ts.ends_with('Z'), "{ts}");
        assert!(line.contains("session=-"), "{line}");
    }

    #[test]
    fn log_message_newlines_are_escaped() {
        let line = LogCtx {
            session_id: None,
            thread_key: None,
        }
        .line("info", "bridge", "a\nb");
        assert!(line.contains("a\\nb"), "{line}");
        assert_eq!(
            line.matches('\n').count(),
            1,
            "record must be one line: {line}"
        );
    }

    #[test]
    fn iso8601_matches_known_instants() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        // `date -u -j -f %Y-%m-%dT%H:%M:%S 2026-07-27T12:34:56 +%s` = 1785155696
        assert_eq!(iso8601(1_785_155_696_007), "2026-07-27T12:34:56.007Z");
        assert_eq!(iso8601(1_709_164_800_000), "2024-02-29T00:00:00.000Z"); // 閏日
    }

    #[test]
    fn lifecycle_resets_on_spawn_and_counts() {
        let mut lc = Lifecycle::new();
        let m = lc.record(&ThreadKey::parse("C1:1.0"), "spawn", 1000);
        assert_eq!((m.seq, m.since_prev_ms, m.since_spawn_ms), (1, 0, 0));
        let m = lc.record(&ThreadKey::parse("C1:1.0"), "session_start", 1450);
        assert_eq!((m.seq, m.since_prev_ms, m.since_spawn_ms), (2, 450, 450));
        let m = lc.record(&ThreadKey::parse("C1:1.0"), "user_prompt", 2000);
        assert_eq!((m.seq, m.since_prev_ms, m.since_spawn_ms), (3, 550, 1000));
        // spawn は同キーの時間軸をリセット
        let m = lc.record(&ThreadKey::parse("C1:1.0"), "spawn", 5000);
        assert_eq!((m.seq, m.since_prev_ms, m.since_spawn_ms), (1, 0, 0));
        // 未知キーは最初のイベントが起点
        let m = lc.record(&ThreadKey::parse("C2:2.0"), "session_start", 100);
        assert_eq!((m.seq, m.since_spawn_ms), (1, 0));
    }

    #[test]
    fn milestone_message_matches_current_format() {
        let m = Milestone {
            seq: 3,
            since_prev_ms: 550,
            since_spawn_ms: 1000,
        };
        assert_eq!(
            m.message(&ThreadKey::parse("C1:1.0"), "user_prompt"),
            "thread=C1:1.0 #3 user_prompt +550ms (since spawn +1000ms)"
        );
    }

    #[test]
    fn sanitize_thread_key_replaces_colon_and_dot() {
        let ctx = LogCtx {
            session_id: None,
            thread_key: Some(ThreadKey::parse("C0AAA:123.456")),
        };
        assert_eq!(ctx.sanitized_key().unwrap(), "C0AAA-123-456");
    }

    #[test]
    fn thread_key_roundtrip() {
        assert_eq!(ThreadKey::new("C0AAA", "123.456").as_str(), "C0AAA:123.456");
        assert_eq!(
            ThreadKey::parse("C0AAA:123.456").split(),
            ("C0AAA".into(), Some("123.456".into()))
        );
        assert_eq!(ThreadKey::parse("C0AAA").split(), ("C0AAA".into(), None));
        assert_eq!(
            ThreadKey::parse("C0AAA:1:2").split(),
            ("C0AAA".into(), Some("1:2".into()))
        ); // 最初の ':' で分割
    }

    #[test]
    fn threads_unknown_fields_survive_roundtrip() {
        let src = r#"{"171.001":{"agent_id":"sid-1","channel_id":"C1","topic":"直近の話題","future_field":{"x":1}}}"#;
        let mut reg = Threads::from_str(src).unwrap();
        reg.upsert("172.002", ThreadEntry::new("C2", "sid-2"));
        let out = reg.to_string_pretty().unwrap();
        assert!(out.contains("future_field"), "unknown field lost: {out}");
        assert!(out.contains("sid-2"));
        // topic は型付きで読めて、書き戻しでも残る(status のリンク文字列がこれ)
        assert_eq!(
            reg.get("171.001").unwrap().topic.as_deref(),
            Some("直近の話題")
        );
        assert!(out.contains("直近の話題"), "topic lost: {out}");
    }

    #[test]
    fn the_session_id_is_read_and_written_under_the_name_the_current_bot_uses() {
        // 切替日はこれが全部 — 本番の threads.json をそのまま置いて、各スレッドが
        // 自分のセッションを `--resume` で拾えること(現行)
        let reg =
            Threads::from_str(r#"{"171.001":{"session_id":"S-bun","channel_id":"C1"}}"#).unwrap();
        assert_eq!(
            reg.get("171.001").unwrap().agent_id.as_deref(),
            Some("S-bun")
        );
        assert_eq!(reg.find_by_session("S-bun").unwrap().0, "171.001");
        let out = reg.to_string_pretty().unwrap();
        assert!(
            out.contains(r#""session_id": "S-bun""#),
            "書き戻しも現行の名前: {out}"
        );
        assert!(!out.contains("agent_id"), "独自の名前は残さない: {out}");
        // 独自の名前で書いた dev の state も読める(alias)
        let old =
            Threads::from_str(r#"{"171.001":{"agent_id":"S-rs","channel_id":"C1"}}"#).unwrap();
        assert_eq!(
            old.get("171.001").unwrap().agent_id.as_deref(),
            Some("S-rs")
        );
    }

    #[test]
    fn threads_find_by_session() {
        let reg =
            Threads::from_str(r#"{"171.001":{"agent_id":"sid-1","channel_id":"C1"}}"#).unwrap();
        let (ts, e) = reg.find_by_session("sid-1").unwrap();
        assert_eq!(ts, "171.001");
        assert_eq!(e.channel_id.as_deref(), Some("C1"));
    }

    /// 起動時に未応答の台帳へ載せ直す鍵の選別。**生きているワーカーのぶんだけ** —
    /// 死んだスレッドを載せると、誰も応えないまま沈黙の見張りが永久に居座る。
    #[test]
    fn surviving_keeps_only_threads_whose_worker_is_alive() {
        let reg = Threads::from_str(
            r#"{
              "171.001":{"agent_id":"sid-live","channel_id":"C1"},
              "171.002":{"agent_id":"sid-dead","channel_id":"C1"},
              "171.003":{"channel_id":"C1"}
            }"#,
        )
        .unwrap();
        let keys: Vec<ThreadKey> = ["C1:171.001", "C1:171.002", "C1:171.003", "C1:171.404", "C1"]
            .iter()
            .map(|k| ThreadKey::parse(k))
            .collect();

        let kept = reg.surviving(&keys, |sid| sid == "sid-live");

        assert_eq!(
            kept.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
            vec!["C1:171.001"],
            "生きている1本だけが残る(死んだ / セッション未割当 / 台帳に無い / スレッド無しは落ちる)"
        );
    }

    #[test]
    fn access_loads_real_shape_and_roundtrips() {
        let src = r#"{"owner":"U1","allowedBots":["B1"],"routes":{"C1":{"repo_path":"/x","zzz":1}},"homeChannel":"C9"}"#;
        let a = Access::from_str(src).unwrap();
        assert_eq!(a.owner, "U1");
        assert_eq!(a.routes["C1"].repo_path.as_deref(), Some("/x"));
        assert_eq!(a.home_channel.as_deref(), Some("C9"));
        let out = a.to_string_pretty().unwrap();
        assert!(out.contains("zzz"), "unknown route field lost: {out}"); // 未知フィールド保存
        assert!(
            out.contains("allowedBots"),
            "unknown top-level field lost: {out}"
        );
    }

    #[test]
    fn access_ack_reaction_roundtrips() {
        let src = r#"{"owner":"U1","ackReaction":"spiral_note_pad","zzz":1}"#;
        let a = Access::from_str(src).unwrap();
        assert_eq!(a.ack_reaction.as_deref(), Some("spiral_note_pad"));
        let out = a.to_string_pretty().unwrap();
        assert!(out.contains("ackReaction") && out.contains("zzz"), "{out}");
    }

    #[test]
    fn access_mutations_are_pure_and_verbatim() {
        let mut prev = Access::default();
        prev.owner = "U1".into();
        let (a, msg, warns) = prev
            .apply(AccessOp::SetRepo {
                channel: "C1".into(),
                path: "/repo".into(),
            })
            .unwrap();
        assert_eq!(a.routes["C1"].repo_path.as_deref(), Some("/repo"));
        assert_eq!(msg, "<#C1> now uses the project directory `/repo`.");
        assert!(warns.is_empty());
        assert_eq!(prev.routes.len(), 0); // prev は不変
        // 相対パスは拒否
        assert!(
            prev.apply(AccessOp::SetRepo {
                channel: "C1".into(),
                path: "rel".into()
            })
            .is_err()
        );
        // warm: repo 未設定チャンネルは警告付き
        let (a2, msg2, warns2) = prev
            .apply(AccessOp::SetWarm {
                channel: "C2".into(),
                on: true,
            })
            .unwrap();
        assert_eq!(a2.routes["C2"].warm, Some(true));
        assert!(msg2.contains("started ahead of time"));
        assert_eq!(warns2.len(), 1);
        // bot allow は重複しない
        let (a3, _, _) = prev.apply(AccessOp::BotAllow("B9".into())).unwrap();
        let (a4, _, _) = a3.apply(AccessOp::BotAllow("B9".into())).unwrap();
        assert_eq!(a4.allowed_bots, vec!["B9"]);
        let (_, msg5, _) = a4.apply(AccessOp::BotRemove("B0".into())).unwrap();
        assert_eq!(msg5, "B0 is not an allowed bot.");
    }

    #[test]
    fn access_typed_fields_roundtrip_with_unknowns() {
        let src = r#"{"owner":"U1","allowedBots":["B1"],"routes":{"C1":{"repo_path":"/r","warm":false,"zzz":1}},"yyy":2}"#;
        let a = Access::from_str(src).unwrap();
        assert_eq!(a.allowed_bots, vec!["B1"]);
        assert_eq!(a.routes["C1"].warm, Some(false));
        let out = a.to_string_pretty().unwrap();
        for k in ["allowedBots", "zzz", "yyy", "warm"] {
            assert!(out.contains(k), "{k}");
        }
    }

    #[test]
    fn repo_path_resolves_like_spawn() {
        let mut a = Access::default();
        a.routes.insert(
            "C1".into(),
            Route {
                repo_path: Some("/r".into()),
                ..Default::default()
            },
        );
        assert_eq!(a.repo_path("C1", "/home"), ("/r".into(), false));
        assert_eq!(a.repo_path("C2", "/home"), ("/home".into(), true));
    }

    #[test]
    fn pool_targets_empty_without_owner() {
        let mut a = Access::default();
        a.owner = String::new();
        assert!(a.pool_targets("/home").is_empty());
    }

    #[test]
    fn pool_targets_always_includes_home_plus_distinct_repos() {
        let mut a = Access::default();
        a.owner = "U1".to_string();
        a.routes.insert(
            "C1".into(),
            Route {
                repo_path: Some("/repo/a".into()),
                warm: None,
                ..Default::default()
            },
        );
        a.routes.insert(
            "C2".into(),
            Route {
                repo_path: Some("/repo/a".into()),
                warm: Some(false),
                ..Default::default()
            },
        );
        a.routes.insert(
            "C3".into(),
            Route {
                repo_path: Some("/repo/b".into()),
                warm: Some(true),
                ..Default::default()
            },
        );
        let pools = a.pool_targets("/home");
        let cwds: Vec<&str> = pools.iter().map(|p| p.as_str()).collect();
        assert!(cwds.contains(&"/home"));
        assert!(
            cwds.contains(&"/repo/a"),
            "opt-in on C1 stocks the pool even though C2 opted out"
        );
        assert!(cwds.contains(&"/repo/b"));
        assert_eq!(
            cwds.len(),
            3,
            "distinct repo_path collapses C1/C2 into one pool"
        );
    }

    #[test]
    fn pool_targets_excludes_repo_when_every_route_opts_out() {
        let mut a = Access::default();
        a.owner = "U1".to_string();
        a.routes.insert(
            "C1".into(),
            Route {
                repo_path: Some("/repo/c".into()),
                warm: Some(false),
                ..Default::default()
            },
        );
        let pools = a.pool_targets("/home");
        assert!(!pools.iter().any(|p| p == "/repo/c"));
    }

    #[test]
    fn pool_key_is_stable_and_repo_scoped() {
        assert_eq!(PoolKey::of_cwd("/repo/a"), PoolKey::of_cwd("/repo/a"));
        assert_ne!(PoolKey::of_cwd("/repo/a"), PoolKey::of_cwd("/repo/b"));
        assert!(PoolKey::of_cwd("/repo/a").as_str().starts_with("repo-"));
    }

    /// grant は**人がボタンを押したときだけ**書かれる。現行と同じ
    /// `allowedTools` キーで往復し、未知フィールドを巻き込まないことを固定する。
    #[test]
    fn tool_grants_round_trip_under_the_current_key() {
        // スレッド側: threads.json のエントリに載る
        let mut t = Threads::default();
        t.grant_thread_tool("1.0", "Bash");
        t.grant_thread_tool("1.0", "Bash"); // 二度押しても増えない
        t.grant_thread_tool("1.0", "Edit");
        assert!(t.thread_tool_allowed("1.0", "Bash"));
        assert!(t.thread_tool_allowed("1.0", "Edit"));
        assert!(!t.thread_tool_allowed("1.0", "Write"));
        assert!(
            !t.thread_tool_allowed("2.0", "Bash"),
            "別スレッドには効かない"
        );
        let json = t.to_string_pretty().unwrap();
        assert!(json.contains("\"allowedTools\""), "{json}");
        assert_eq!(
            json.matches("\"Bash\"").count(),
            1,
            "重複して積まれている: {json}"
        );

        // チャンネル側: access.json の route に載る。未知フィールドは巻き込まない
        let mut a = Access::from_str(
            r#"{"owner":"U1","routes":{"C1":{"repo_path":"/r","futureThing":{"k":1}}}}"#,
        )
        .unwrap();
        a.grant_channel_tool("C1", "Bash");
        assert!(a.channel_tool_allowed("C1", "Bash"));
        assert!(!a.channel_tool_allowed("C2", "Bash"));
        let json = a.to_string_pretty().unwrap();
        assert!(json.contains("\"allowedTools\""), "{json}");
        assert!(
            json.contains("futureThing"),
            "未知フィールドが消えた: {json}"
        );
    }

    #[test]
    fn should_claim_pool_only_for_brand_new_threads() {
        assert!(Threads::should_claim_pool(None));
        let e = ThreadEntry {
            agent_id: Some("s".into()),
            ..Default::default()
        };
        assert!(!Threads::should_claim_pool(Some(&e)));
    }

    #[test]
    fn pool_assignment_entry_carries_repo_and_channel() {
        let e = ThreadEntry::for_pool_assignment("C1", "/repo/a", Some("t".into()));
        assert_eq!(e.channel_id.as_deref(), Some("C1"));
        assert_eq!(e.repo_path.as_deref(), Some("/repo/a"));
        assert_eq!(e.topic.as_deref(), Some("t"));
        // agent_id は呼び出し側が claimed.session_id を差し込む — ここでは器だけ
        assert_eq!(e.agent_id, None);
    }

    #[test]
    fn pool_needs_launch_rules() {
        assert!(PoolStatus::needs_launch(None));
        assert!(!PoolStatus::needs_launch(Some(PoolStatus {
            present: true,
            gave_up: false
        })));
        assert!(!PoolStatus::needs_launch(Some(PoolStatus {
            present: false,
            gave_up: true
        })));
        // 諦めた在庫は**畳んで消す**ので、以後 present は必ず false になる。「居ない」だけを
        // 見ると再 spawn してしまう ↑ の一行が止め金 / 引き当て後の補充は ↓ で通る
        assert!(PoolStatus::needs_launch(Some(PoolStatus {
            present: false,
            gave_up: false
        })));
    }

    #[test]
    fn pool_give_up_after_timeout() {
        assert!(!PoolStatus::should_give_up(1_000, 1_000 + 49_999, 50_000));
        assert!(PoolStatus::should_give_up(1_000, 1_000 + 50_001, 50_000));
    }

    #[test]
    fn access_missing_file_is_fail_closed() {
        let a = Access::load(&StateDir::at("/nonexistent-dir-3f9"));
        assert_eq!(a.owner, "", "missing access.json must serve nobody");
    }

    #[test]
    fn json_atomic_roundtrip() {
        let dir = StateDir::at(std::env::temp_dir().join(format!("scjson-{}", std::process::id())));
        std::fs::create_dir_all(dir.path()).unwrap();
        dir.write_json_atomic("x.json", &serde_json::json!({"a": 1}))
            .unwrap();
        let v = dir.read_json_or("x.json", serde_json::json!({}));
        assert_eq!(v["a"], 1);
    }

    /// 指名はファイルに残る — これが「再起動のたびに新しい在庫セッションを切る」の止め金。
    #[test]
    fn pool_nominations_survive_a_reload() {
        let dir = StateDir::at(std::env::temp_dir().join(format!("scpool-{}", std::process::id())));
        std::fs::create_dir_all(dir.path()).unwrap();
        let _ = std::fs::remove_file(dir.join("pools.json"));

        let mut p = Pools::load(&dir);
        assert_eq!(p.session_of("/repo/a"), None, "空のうちは指名なし");
        p.nominate("/repo/a", "sid-a");
        p.nominate("/home/u", "sid-h");
        p.save().unwrap();

        // 別プロセスの起動に相当 — 指名がそのまま読み戻せる
        let again = Pools::load(&dir);
        assert_eq!(again.session_of("/repo/a"), Some("sid-a"));
        assert_eq!(again.session_of("/home/u"), Some("sid-h"));
        assert_eq!(
            again.rows(),
            vec![
                ("/home/u".to_string(), "sid-h".to_string()),
                ("/repo/a".to_string(), "sid-a".to_string()),
            ],
            "キー順は安定(BTreeMap)"
        );
    }

    /// 卒業(引き当て)は cwd で、セッションの終了は session_id で外す。
    #[test]
    fn pool_nominations_are_released_both_ways() {
        let mut p = Pools::from_str(r#"{"/repo/a":"sid-a","/repo/b":"sid-b"}"#).unwrap();
        assert_eq!(p.release("/repo/a").as_deref(), Some("sid-a"));
        assert_eq!(p.session_of("/repo/a"), None);
        assert_eq!(p.release_session("sid-b").as_deref(), Some("/repo/b"));
        assert!(p.rows().is_empty());
        // 知らない相手を外しても壊れない
        assert_eq!(p.release("/repo/zzz"), None);
        assert_eq!(p.release_session("sid-zzz"), None);
    }

    /// 未知フィールドではなく**壊れた** pools.json は空で始める(起動を止めない)。
    #[test]
    fn a_broken_pools_file_starts_empty() {
        assert!(Pools::from_str("{ not json").is_err());
        let p = Pools::load(&StateDir::at("/nonexistent-dir-3f9"));
        assert!(p.rows().is_empty());
        // path はあるので save は落ちない…わけではない(親が無い)。保存失敗は呼び側がログに落とす
        assert!(p.save().is_err());
    }

    #[test]
    fn pool_restore_decides_by_configuration_and_liveness() {
        // 生きている在庫は引き取る(前の Bridge が畳まずに降りた分)
        assert_eq!(PoolRestore::decide(true, true), PoolRestore::Adopt);
        // 死んでいたら指名を残したまま `--resume` で起こし直す
        assert_eq!(PoolRestore::decide(true, false), PoolRestore::Respawn);
        // プール対象から外れた cwd は生死に関わらず捨てる
        assert_eq!(PoolRestore::decide(false, true), PoolRestore::Discard);
        assert_eq!(PoolRestore::decide(false, false), PoolRestore::Discard);
    }

    #[test]
    fn ledger_tracks_receives_and_disposes() {
        let mut l = Ledger::default();
        l.track(&ThreadKey::parse("C1:1.0"), "1.1");
        l.track(&ThreadKey::parse("C1:1.0"), "1.2");
        assert_eq!(l.pending(&ThreadKey::parse("C1:1.0")), vec!["1.1", "1.2"]);
        // 未応答を抱えた鍵 = 再起動の予告を出す先(空になった鍵は消える)
        assert_eq!(l.pending_keys(), vec!["C1:1.0"]);
        // mark_received は新しく受領になった分だけ返す(2回目は空 = 冪等)
        assert_eq!(
            l.mark_received(&ThreadKey::parse("C1:1.0")),
            vec!["1.1", "1.2"]
        );
        assert!(l.mark_received(&ThreadKey::parse("C1:1.0")).is_empty());
        // 受領しても未応答 — pending には残る(受領と応答は別の状態)
        assert_eq!(l.pending(&ThreadKey::parse("C1:1.0")).len(), 2);
        l.disposed(&ThreadKey::parse("C1:1.0"), &["1.1".to_string()]);
        assert_eq!(l.pending(&ThreadKey::parse("C1:1.0")), vec!["1.2"]);
        // ids 無し disposition はスレッド全消化
        assert_eq!(l.dispose_all(&ThreadKey::parse("C1:1.0")), vec!["1.2"]);
        assert!(l.pending(&ThreadKey::parse("C1:1.0")).is_empty());
        assert!(
            l.pending_keys().is_empty(),
            "全消化した鍵は予告先に残らない"
        );
        // 再配達は received をリセット
        l.track(&ThreadKey::parse("C1:1.0"), "1.3");
        l.mark_received(&ThreadKey::parse("C1:1.0"));
        l.track(&ThreadKey::parse("C1:1.0"), "1.3");
        assert_eq!(l.mark_received(&ThreadKey::parse("C1:1.0")), vec!["1.3"]);
    }

    /// 再送の前提2つ。封筒を覚えていない項目は再送候補に出さない(送るものが無い)。
    /// 予算は**メッセージごと**で、全員分揃って初めて引く — 1人でも尽きていれば誰からも引かない。
    #[test]
    fn ledger_remembers_envelopes_and_spends_retry_budget_atomically() {
        let key = ThreadKey::parse("C1:1.0");
        let mut l = Ledger::default();
        l.track(&key, "m1");
        l.track(&key, "m2");
        assert!(
            l.undisposed(&key).is_empty(),
            "封筒が無ければ再送候補に出ない"
        );

        l.remember_envelope(&key, "m1", "envelope-1");
        assert_eq!(
            l.undisposed(&key),
            vec![("m1".to_string(), "envelope-1".to_string())]
        );

        // m2 は封筒を覚えていないので、2件まとめての予算引きは通らない
        let both = ["m1".to_string(), "m2".to_string()];
        l.remember_envelope(&key, "m2", "envelope-2");
        assert!(l.spend_retry(&key, &both, 1));
        assert!(!l.spend_retry(&key, &both, 1), "2回目は予算切れ");

        // 予算は項目と一緒に消える — 別メッセージは満額から始まる
        l.disposed(&key, &both);
        l.track(&key, "m3");
        l.remember_envelope(&key, "m3", "envelope-3");
        assert!(l.spend_retry(&key, &["m3".to_string()], 1));

        // 台帳に無い id が混ざっていたら引かない(数が合わない = 前提が崩れている)
        assert!(!l.spend_retry(&key, &["m3".to_string(), "nope".to_string()], 9));
    }

    /// 台帳は Bridge プロセスをまたぐ(threads.json の entry の中)。書くのは変更が
    /// あったときだけで、載せ直す鍵は呼び手が絞る(生きているワーカーのスレッドだけ)。
    #[test]
    fn ledger_round_trips_through_threads_json_and_keeps_only_the_kept_keys() {
        let dir = StateDir::at(std::env::temp_dir().join(format!("scled-{}", std::process::id())));
        let (live, dead) = (ThreadKey::parse("C1:1.0"), ThreadKey::parse("C1:2.0"));
        // 台帳が乗る先。channel_id が無い entry は鍵を作れないので、必ず持たせる
        let mut threads = Threads::load(&dir);
        for ts in ["1.0", "2.0"] {
            threads.upsert(ts, ThreadEntry::new("C1", "sid-1"));
        }
        let mut l = Ledger::load(&threads);
        assert!(l.flush(&mut threads).is_none(), "変更が無ければ書かない");

        l.track(&live, "m1");
        l.remember_envelope(&live, "m1", "envelope-1");
        l.track(&live, "m2");
        l.mark_received(&live);
        l.track(&dead, "m9");
        assert!(l.spend_retry(&live, &["m1".to_string()], 2));
        l.flush(&mut threads).unwrap().unwrap();
        assert!(
            l.flush(&mut threads).is_none(),
            "2回目は dirty が下りている"
        );

        // 後継プロセス: 生きている live だけ載せ、dead は捨てる
        let mut next = Ledger::load(&Threads::load(&dir));
        assert_eq!(next.pending_keys().len(), 2);
        next.retain_keys(std::slice::from_ref(&live));
        assert_eq!(next.pending(&live), vec!["m1", "m2"]);
        assert!(next.pending(&dead).is_empty());
        // 封筒・受領・使った予算まで引き継ぐ(再送の前提が復元後も同じ)
        assert_eq!(
            next.undisposed(&live),
            vec![("m1".to_string(), "envelope-1".to_string())]
        );
        assert!(next.unreceived(&live).is_empty(), "受領済みは受領のまま");
        assert!(
            !next.spend_retry(&live, &["m1".to_string()], 1),
            "予算は使用済み"
        );

        std::fs::remove_dir_all(dir.path()).ok();
    }

    #[test]
    fn ledger_marks_only_the_ids_the_worker_actually_read() {
        let mut l = Ledger::default();
        l.track(&ThreadKey::parse("C1:1.0"), "1.1");
        l.track(&ThreadKey::parse("C1:1.0"), "1.2");
        assert_eq!(
            l.unreceived(&ThreadKey::parse("C1:1.0")),
            vec!["1.1", "1.2"]
        );
        // transcript に出たのは 1.2 だけ → 1.1 はまだ未受領のまま
        assert_eq!(
            l.mark_received_ids(&ThreadKey::parse("C1:1.0"), &["1.2".to_string()]),
            vec!["1.2"]
        );
        assert_eq!(l.unreceived(&ThreadKey::parse("C1:1.0")), vec!["1.1"]);
        // 冪等 — 2回目は空
        assert!(
            l.mark_received_ids(&ThreadKey::parse("C1:1.0"), &["1.2".to_string()])
                .is_empty()
        );
        // 受領しても未応答 — pending には残る
        assert_eq!(l.pending(&ThreadKey::parse("C1:1.0")).len(), 2);
        assert!(
            l.mark_received_ids(&ThreadKey::parse("C9:9.9"), &["1.1".to_string()])
                .is_empty()
        );
    }

    #[test]
    fn transcript_scan_finds_the_envelope_id_escaped_in_jsonl() {
        let ids = vec!["1783500885.490429".to_string(), "1.2".to_string()];
        // 実物の transcript(JSONL)は封筒を JSON 文字列に入れる = 引用符がエスケープされる
        let real = r#"{"type":"user","message":{"content":"<channel source=\"plugin:agentgw:agentgw\" channel_id=\"C1\" message_id=\"1783500885.490429\" user=\"U1\">\nhi\n</channel>"}}"#;
        assert_eq!(
            Ledger::find_received_ids(real, &ids),
            vec!["1783500885.490429"]
        );
        // エスケープされていない生の形も拾う(ログや別経路)
        assert_eq!(
            Ledger::find_received_ids(r#"message_id="1.2""#, &ids),
            vec!["1.2"]
        );
        // message_ids(複数形)は「覆った」の申告であって受領ではない
        assert!(Ledger::find_received_ids(r#"message_ids=[\"1.2\"]"#, &ids).is_empty());
        // ただの言及も受領ではない
        assert!(Ledger::find_received_ids("the message 1.2 arrived", &ids).is_empty());
        // 別の id の一部を掴まない(1.2 は 1.25 の中に居るが別物)
        assert!(Ledger::find_received_ids(r#"message_id=\"1.25\""#, &ids).is_empty());
        assert!(Ledger::find_received_ids("", &ids).is_empty());
    }

    #[test]
    fn stop_block_truth_table() {
        assert!(Disposition::should_block_stop(true, false, true, 1)); // 未応答あり → block
        assert!(!Disposition::should_block_stop(false, false, true, 1)); // スレッド不明 → 通す
        assert!(!Disposition::should_block_stop(true, true, true, 1)); // 再プロンプト済み → 1回で打ち止め
        assert!(!Disposition::should_block_stop(true, false, false, 1)); // MCP 未準備 → fail-open で通す
        assert!(!Disposition::should_block_stop(true, false, true, 0)); // 全部応答済み → 通す
    }

    #[test]
    fn stall_watchdog_truth_table() {
        const S: u64 = 5_000; // 現行の silenceMs 既定値
        // ちょうど満期で撃つ(現行の setTimeout(…, silenceMs) と同じ境界)
        assert!(StallAction::due(0, S, false, true, S));
        assert!(
            !StallAction::due(0, S - 1, false, true, S),
            "1ms 手前ではまだ黙る"
        );
        assert!(
            !StallAction::due(0, S, true, true, S),
            "出している間は二度撃たない"
        );
        assert!(
            !StallAction::due(0, S, false, false, S),
            "未応答が無い = 決着済み。応答待ちを蒸し返さない"
        );
        // 活動が入れば時計は 0 に戻る(呼び手が last_activity を今にする)
        assert!(!StallAction::due(S, S, false, true, S));
        // 時計が巻き戻っても panic せず黙る(saturating_sub)
        assert!(!StallAction::due(S * 2, S, false, true, S));
    }

    #[test]
    fn stall_tick_action_table() {
        use StallAction::{Fire, Nothing, Settle};
        const S: u64 = 5_000;
        // 未応答あり + 無音が満期 → 立てる
        assert_eq!(StallAction::of(true, 0, S, false, S), Fire);
        // 未応答あり + まだ喋っている / もう出している → そのまま
        assert_eq!(StallAction::of(true, S, S, false, S), Nothing);
        assert_eq!(StallAction::of(true, 0, S, true, S), Nothing);
        // 未応答が空 = 決着 → 畳む。**無音でも / 出していても / 直前に活動があっても**畳む。
        // terminate(exit / logout / resume)で台帳が空になる経路の唯一の受け皿なので、
        // ここで Settle 以外を返すと shimmer が居座る
        assert_eq!(StallAction::of(false, 0, S, true, S), Settle);
        assert_eq!(StallAction::of(false, 0, S, false, S), Settle);
        assert_eq!(
            StallAction::of(false, S, S, false, S),
            Settle,
            "直前の活動より決着が優先"
        );
    }

    #[test]
    fn stop_block_output_shape() {
        let v = Disposition::block_output(&["1.1".to_string(), "2.2".to_string()]);
        assert_eq!(v["decision"], "block");
        let reason = v["reason"].as_str().unwrap();
        assert!(reason.starts_with("You ended your turn without delivering."));
        // 残っている id を名指しする — これが無いとワーカーは外し、台帳が減らない
        assert!(
            reason.ends_with("Outstanding message_ids: [1.1, 2.2]"),
            "{reason}"
        );
    }

    #[test]
    fn read_json_or_returns_default_when_missing() {
        let dir = StateDir::at(std::env::temp_dir());
        let _ = std::fs::remove_file(dir.join("scjson-does-not-exist-9e3.json"));
        let v = dir.read_json_or(
            "scjson-does-not-exist-9e3.json",
            serde_json::json!({"d": true}),
        );
        assert_eq!(v["d"], true);
    }
}
