//! The MCP server agents use to talk to Slack. **Shared by all agents** —
//! it's not a claude-only tool, so it doesn't live under `agent/`.
//!
//! **Must be stateless** — rmcp streamable http's `stateful_mode` defaults to true, and its
//! session table lives in memory, so a Bridge restart kills MCP for every surviving agent
//! ("can hear but can't speak").
//!
//! The six tools' names, schemas and descriptions are kept **identical** to the original connector.
//! The anti-narration wording is a behavior contract: don't shorten or paraphrase it.

use crate::agent::HookEvent;
use crate::bridge::state::{LogCtx, StateDir};
use axum::{Router, http::StatusCode};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParam, CallToolResult, Content, InitializeRequestParam, InitializeResult,
        ListToolsResult, PaginatedRequestParam, ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Executes agents' MCP tool calls. The real implementation is `slack::ToolExec`.
pub trait ToolExecutor: Send + Sync + 'static {
    fn execute(
        &self,
        session_id: String,
        tool: String,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;
}

/// The MCP endpoint: tool definitions, and writing the config file handed to agents.
pub struct Mcp;

impl Mcp {
    /// The six MCP tool definitions (names, schemas and descriptions ported
    /// faithfully). The anti-narration wording is a behavior contract — don't shorten or paraphrase it.
    pub fn tool_definitions() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({
                "name": "reply",
                "description": "Reply on Slack. Pass channel_id (the conversation from the inbound event). Use thread_ts for threading. Pass files (absolute paths) for attachments. `text` is standard Markdown by default — write **bold**, tables and ```lang code fences normally. Do NOT narrate this tool: never write \"I should use reply\" / \"I have reply\" / \"I will reply\" / \"I replied\" or any similar preamble. Do NOT output any text after calling reply — the tool call is the entire turn.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "channel_id": { "type": "string", "description": "Channel or DM ID (C.../D.../G...)" },
                        "text": { "type": "string", "description": "Message text. Standard Markdown by default — see `markdown`." },
                        "markdown": { "type": "boolean", "description": "Defaults to TRUE: `text` is read as standard Markdown, so headings (##), tables, task lists and fenced code blocks with language all render. Slack mentions (<@U…>), channel links (<#C…>) and :emoji: still work. Set FALSE only to fall back to Slack mrkdwn — the legacy dialect where bold is *single asterisks* and there is no table syntax." },
                        "thread_ts": { "type": "string", "description": "Thread timestamp for threaded replies" },
                        "files": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Absolute file paths to upload as attachments (max 50MB each)"
                        },
                        "message_ids": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "ALWAYS pass the inbound message_id(s) this reply answers — used for correlation/audit (and to annotate if the user deletes the message). Internal only — does not change where the reply is posted."
                        },
                    },
                    "required": ["channel_id", "text"],
                },
            }),
            serde_json::json!({
                "name": "react",
                "description": "Add an emoji reaction to a Slack message. Do NOT narrate this tool: never write \"I should use react\" / \"I have react\" / \"I will react\" / \"I reacted\" or any similar preamble. Do NOT output any text after calling react — the tool call is the entire turn.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "channel_id": { "type": "string", "description": "Channel or DM ID (C.../D.../G...) the message lives in." },
                        "message_ts": { "type": "string", "description": "Timestamp of the message to react to" },
                        "emoji": { "type": "string", "description": "Emoji name without colons (e.g. \"thumbsup\")" },
                    },
                    "required": ["channel_id", "message_ts", "emoji"],
                },
            }),
            serde_json::json!({
                "name": "no_reply",
                "description": "Record that you are INTENTIONALLY not replying to one or more delivered Slack messages (e.g. a message aimed at someone else, small talk, or already-handled). Posts nothing to Slack — it tells the bridge your silence is deliberate (not a failure). Pass the message_id(s) you are choosing not to answer. Do NOT narrate this tool: never write \"I should use no_reply\" / \"I have no_reply\" / \"I will not reply\" or any similar preamble. Do NOT output any text after calling no_reply — the tool call is the entire turn (any trailing text leaks into Slack as a stray message).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "channel_id": { "type": "string", "description": "Channel or DM ID" },
                        "message_ids": { "type": "array", "items": { "type": "string" }, "description": "The inbound message_id(s) you are intentionally not answering (coverage range — a single no_reply may cover several)." },
                        "reason": { "type": "string", "description": "Optional short reason (e.g. \"aimed at another user\")." },
                        "thread_ts": { "type": "string", "description": "Optional thread timestamp (to clear the thinking indicator)." },
                    },
                    "required": ["channel_id", "message_ids"],
                },
            }),
            serde_json::json!({
                "name": "edit_message",
                "description": "Edit a message the bot previously sent (message_ts). Use for progress updates, OR to deliver your final answer by editing an earlier message instead of posting a new reply. Like reply, `text` is standard Markdown by default.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "channel_id": { "type": "string", "description": "Channel or DM ID (C.../D.../G...) the message to edit lives in." },
                        "message_ts": { "type": "string", "description": "Timestamp of the bot message to edit." },
                        "markdown": { "type": "boolean", "description": "Defaults to TRUE (standard Markdown). Set FALSE for legacy Slack mrkdwn." },
                        "text": { "type": "string", "description": "The message's new full text (it REPLACES the old text, not appended). Standard Markdown by default — see `markdown`." },
                        "thread_ts": { "type": "string", "description": "Thread root timestamp. Pass it together with message_ids when this edit is your answer (marks the message(s) answered and clears the thinking indicator)." },
                        "message_ids": { "type": "array", "items": { "type": "string" }, "description": "If this edit IS your answer to one or more inbound messages, pass their message_id(s) — marks them answered (correlation/audit, and so they are not re-delivered after a worker reconnect). OMIT for a mere progress-update edit (the turn is not done yet)." },
                    },
                    "required": ["channel_id", "message_ts", "text"],
                },
            }),
            serde_json::json!({
                "name": "download_attachment",
                "description": "Download a Slack file to the local inbox. Returns the file path for Claude to Read.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file_id": { "type": "string", "description": "Slack file ID from the inbound message meta" },
                    },
                    "required": ["file_id"],
                },
            }),
            serde_json::json!({
                "name": "fetch_messages",
                "description": "Fetch recent messages from a Slack channel or thread. Returns oldest-first with timestamps.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "channel": { "type": "string", "description": "Channel ID" },
                        "limit": { "type": "number", "description": "Max messages (default 20, max 100)" },
                        "thread_ts": { "type": "string", "description": "If provided, fetch thread replies instead of channel history" },
                    },
                    "required": ["channel"],
                },
            }),
        ]
    }

    /// The contents of `--mcp-config` handed to agents.
    ///
    /// **The server name is `agentgw`, same as the binary.** Agents see tool names like
    /// `mcp__agentgw__reply`, and `is_denied_tool` in `slack.rs` and
    /// `OWN_MCP_PREFIX` in `command.rs` match on this prefix too.
    ///
    /// **If you rename it, running agents can't reply until they reconnect with `/mcp`** —
    /// they hold the tool definitions from session start and keep calling the old name.
    pub fn config_json(mcp_port: u16, session_id: &str, mcp_token: &str) -> serde_json::Value {
        serde_json::json!({ "mcpServers": { "agentgw": {
            "type": "http",
            "url": format!("http://127.0.0.1:{mcp_port}/mcp"),
            "headers": {
                "Authorization": format!("Bearer {mcp_token}"),
                "X-Agentgw-Session": session_id,
            },
        }}})
    }

    pub fn write_config(
        dir: &StateDir,
        mcp_port: u16,
        session_id: &str,
        mcp_token: &str,
    ) -> std::io::Result<PathBuf> {
        // Generated, so it goes to the runtime area, not state ([`StateDir::runtime_dir`])
        dir.write_runtime_json(
            &format!("mcp/{session_id}.json"),
            &Self::config_json(mcp_port, session_id, mcp_token),
        )
    }

    /// Start the MCP endpoint and return (port, token).
    pub async fn serve(
        state_dir: &StateDir,
        executor: Arc<dyn ToolExecutor>,
        hook_tx: mpsc::Sender<HookEvent>,
    ) -> std::io::Result<(u16, String)> {
        let port = state_dir.remembered_port("mcp", StateDir::free_port);
        let token = state_dir.remembered_token("mcp");
        let expected = format!("Bearer {token}");
        let service = StreamableHttpService::new(
            move || {
                Ok(McpServer {
                    exec: executor.clone(),
                    hooks: hook_tx.clone(),
                })
            },
            LocalSessionManager::default().into(),
            // stateless — the MCP session table lives only in the Bridge's memory; when a restart
            // wipes it, inherited agents' stale Mcp-Session-Id gets 401 and every tool fails (E2E).
            // We identify the agent ourselves via the X-Agentgw-Session header, so the table isn't needed.
            StreamableHttpServerConfig {
                stateful_mode: false,
                ..Default::default()
            },
        );
        let app = Router::new()
            .nest_service("/mcp", service)
            .layer(axum::middleware::from_fn(
                move |req: axum::extract::Request, next: axum::middleware::Next| {
                    let expected = expected.clone();
                    async move {
                        let auth = req
                            .headers()
                            .get("authorization")
                            .and_then(|v| v.to_str().ok());
                        if auth != Some(expected.as_str()) {
                            LogCtx::default().info("mcp", "rejected: bad bearer");
                            return Err(StatusCode::UNAUTHORIZED);
                        }
                        Ok(next.run(req).await)
                    }
                },
            ));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                LogCtx::default().error("mcp", &format!("mcp endpoint stopped: {e}"));
            }
        });
        LogCtx::default().info("mcp", &format!("mcp endpoint on 127.0.0.1:{port}/mcp"));
        Ok((port, token))
    }
}

/// The handler plugged into rmcp. Holds tool execution (`exec`) and the hook entry point (`hooks`).
///
/// Holds no session state — streamable http **must be stateless** (with a session table, a Bridge
/// restart kills MCP for every surviving agent).
#[derive(Clone)]
struct McpServer {
    exec: Arc<dyn ToolExecutor>,
    hooks: mpsc::Sender<HookEvent>,
}

impl McpServer {
    /// Which agent the call came from. The raw HTTP parts are in extensions
    /// (rmcp 0.8.5 injects them on every request, including initialize — tower.rs:334).
    fn session_header(context: &RequestContext<RoleServer>) -> String {
        context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|p| p.headers.get("x-agentgw-session"))
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            // The attachment sentence is copied verbatim from the original connector's `RESPONDER`
            // This is the only place that tells the agent what file_paths on the envelope mean
            instructions: Some(
                "Slack bridge — reply to the Slack thread with these tools. \
                 Events: <channel source=\"slack\" channel_id=... message_id=...>; attachments \
                 are pre-downloaded (`file_paths` = local files to Read, `file_errors` = \
                 failures). Use `download_attachment` only for a file from another \
                 message/thread."
                    .into(),
            ),
            ..Default::default()
        }
    }

    /// The moment an agent gets hold of MCP — the only signal that it can call tools.
    async fn initialize(
        &self,
        request: InitializeRequestParam,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        let session_id = Self::session_header(&context);
        if !session_id.is_empty() {
            let _ = self
                .hooks
                .send(HookEvent {
                    kind: "mcp_initialized".into(),
                    session_id,
                    payload: serde_json::Value::Null,
                    respond: None,
                })
                .await;
        }
        // The rest is the default implementation as-is (rmcp 0.8.5 handler/server.rs:110)
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        Ok(self.get_info())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = Mcp::tool_definitions()
            .into_iter()
            .map(|t| {
                let schema = t["inputSchema"].as_object().cloned().unwrap_or_default();
                Tool::new(
                    t["name"].as_str().unwrap_or_default().to_string(),
                    t["description"].as_str().unwrap_or_default().to_string(),
                    Arc::new(schema),
                )
            })
            .collect();
        Ok(ListToolsResult {
            tools,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParam,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let session_id = Self::session_header(&context);
        let tool = request.name.to_string();
        let args = serde_json::Value::Object(request.arguments.unwrap_or_default());
        let ctx = LogCtx {
            session_id: Some(session_id.clone()),
            thread_key: None,
        };
        if session_id.is_empty() {
            ctx.error("mcp", &format!("{tool} call without session header"));
            return Ok(CallToolResult::error(vec![Content::text(
                "no session — the bridge cannot tell which thread this is",
            )]));
        }
        ctx.info("mcp", &format!("tool {tool}"));
        // **Calling a tool = holding MCP.** After a Bridge restart `initialize` never comes
        // again (the agent's claude doesn't reconnect), so for inherited agents this is the
        // only evidence. Without the mark, stop stays fail-open forever and can't stop a turn
        // that ends without replying. Idempotent, so sending it every time is fine (no milestone —
        // it's a plain fact, not a turning point)
        let _ = self
            .hooks
            .send(HookEvent {
                kind: "mcp_ready".into(),
                session_id: session_id.clone(),
                payload: serde_json::Value::Null,
                respond: None,
            })
            .await;
        match self.exec.execute(session_id, tool.clone(), args).await {
            Ok(out) => Ok(CallToolResult::success(vec![Content::text(out)])),
            Err(e) => {
                ctx.error("mcp", &format!("tool {tool} failed: {e}"));
                Ok(CallToolResult::error(vec![Content::text(e)]))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::CONTENT_TYPE;

    struct FakeExec;
    impl ToolExecutor for FakeExec {
        fn execute(
            &self,
            session_id: String,
            tool: String,
            args: serde_json::Value,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
            Box::pin(async move {
                Ok(format!(
                    "{session_id}:{tool}:{}",
                    args["text"].as_str().unwrap_or("")
                ))
            })
        }
    }

    #[tokio::test]
    async fn tool_executor_works_behind_arc_dyn() {
        let exec: Arc<dyn ToolExecutor> = Arc::new(FakeExec);
        let out = exec
            .execute(
                "sid-1".into(),
                "reply".into(),
                serde_json::json!({"text": "hi"}),
            )
            .await
            .unwrap();
        assert_eq!(out, "sid-1:reply:hi");
    }

    /// E2E: restarting the Bridge wiped the MCP session table (memory only), so the
    /// Mcp-Session-Id baked into inherited agents became an "unknown session" and every tool call failed.
    /// Stateless doesn't look the table up, so it passes — pinned together with the repro against stateful.
    #[tokio::test]
    async fn stale_mcp_session_id_survives_a_bridge_restart() {
        async fn status(stateful_mode: bool) -> u16 {
            let (tx, _rx) = mpsc::channel(1);
            let svc = StreamableHttpService::new(
                move || {
                    Ok(McpServer {
                        exec: Arc::new(FakeExec),
                        hooks: tx.clone(),
                    })
                },
                LocalSessionManager::default().into(),
                StreamableHttpServerConfig {
                    stateful_mode,
                    ..Default::default()
                },
            );
            let req = axum::http::Request::post("/mcp")
                .header(CONTENT_TYPE, "application/json")
                .header("accept", "application/json, text/event-stream")
                .header("mcp-session-id", "baked-in-before-the-restart")
                .body(axum::body::Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                ))
                .unwrap();
            svc.handle(req).await.status().as_u16()
        }
        assert_eq!(
            status(false).await,
            200,
            "stateless: 旧セッション ID は無視して通す"
        );
        assert_eq!(
            status(true).await,
            401,
            "stateful だと弾かれる(以前踏んだ不具合の再現)"
        );
    }

    #[test]
    fn mcp_config_shape() {
        let v = Mcp::config_json(8790, "sid-1", "secret");
        let s = &v["mcpServers"]["agentgw"];
        assert_eq!(s["type"], "http");
        assert_eq!(s["url"], "http://127.0.0.1:8790/mcp");
        assert_eq!(s["headers"]["Authorization"], "Bearer secret");
        assert_eq!(s["headers"]["X-Agentgw-Session"], "sid-1");
    }

    #[test]
    fn all_six_tools_are_defined() {
        let tools = Mcp::tool_definitions();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "reply",
                "react",
                "no_reply",
                "edit_message",
                "download_attachment",
                "fetch_messages"
            ]
        );
        for t in &tools {
            assert!(t["inputSchema"]["properties"].is_object(), "{t}");
            assert!(t["description"].as_str().unwrap().len() > 20, "{t}");
        }
    }

    #[test]
    fn anti_narration_wording_is_verbatim() {
        let tools = Mcp::tool_definitions();
        let by = |n: &str| -> String {
            tools.iter().find(|t| t["name"] == n).unwrap()["description"]
                .as_str()
                .unwrap()
                .to_string()
        };
        // Behavior contract — don't shorten or paraphrase (original connector wording)
        assert!(by("reply").contains(
            r#"Do NOT narrate this tool: never write "I should use reply" / "I have reply" / "I will reply" / "I replied" or any similar preamble. Do NOT output any text after calling reply — the tool call is the entire turn."#
        ));
        assert!(by("react").contains(
            r#"Do NOT narrate this tool: never write "I should use react" / "I have react" / "I will react" / "I reacted" or any similar preamble."#
        ));
        assert!(
            by("no_reply").contains(r#"any trailing text leaks into Slack as a stray message"#)
        );
    }

    /// **Every argument gets a description.** The writer (the LLM) reads the argument descriptions,
    /// and when they disagree with the tool's description it trusts the argument and gets it wrong
    /// (`text` actually kept saying "supports Slack mrkdwn" and went on lying
    /// after the default switched to Markdown).
    #[test]
    fn every_tool_argument_explains_itself() {
        for t in Mcp::tool_definitions() {
            let name = t["name"].as_str().unwrap();
            let props = t["inputSchema"]["properties"].as_object().unwrap();
            assert!(!props.is_empty(), "{name}: 引数が1つも無い");
            for (arg, spec) in props {
                // `items` is the array's element type, not an argument
                if arg == "items" {
                    continue;
                }
                let d = spec["description"].as_str().unwrap_or("");
                assert!(
                    d.len() >= 10,
                    "{name}.{arg}: 説明が無い(または短すぎる): {d:?}"
                );
            }
        }
    }

    #[test]
    fn required_fields_match_the_current_connector() {
        let tools = Mcp::tool_definitions();
        let req = |n: &str| -> Vec<String> {
            tools.iter().find(|t| t["name"] == n).unwrap()["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(req("reply"), ["channel_id", "text"]);
        assert_eq!(req("react"), ["channel_id", "message_ts", "emoji"]);
        assert_eq!(req("no_reply"), ["channel_id", "message_ids"]);
        assert_eq!(req("edit_message"), ["channel_id", "message_ts", "text"]);
        assert_eq!(req("download_attachment"), ["file_id"]);
        assert_eq!(req("fetch_messages"), ["channel"]);
    }
}
