//! pyre-mcp — Newline-delimited JSON-RPC 2.0 stdio server bridging any MCP
//! client to a running pyred daemon over UDS.
//!
//! Transport: newline-delimited JSON over stdio (each request = one line; each
//! response = one line). "Content-Length" framing (LSP variant) is NOT used.
//!
//! Streaming subscribe: polled at 1 s intervals against `list_all_panes`.
//! Adequate for v0.1; can be upgraded to true tarpc event pushing later.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use pyre_proto::{
    connect_control, default_socket, ListBlocksReq, OpenPaneReq, OpenPaneSplitReq, Orient,
    PaneStateKind, PyreDaemonClient, SearchBlocksReq, SessionId, SpawnReq, WindowId,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use tracing_subscriber::EnvFilter;

// ──────────────────────────────────────────────────────────────────────────────
// JSON-RPC 2.0 types
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Request {
    // dead_code: JSON-RPC 2.0 spec requires this field but we only validate via serde.
    #[allow(dead_code)]
    jsonrpc: String,
    /// None for notifications.
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct Response {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

/// Machine-readable error detail carried in the `data` field of every RPC
/// error response. Agents can branch on `code` without parsing the human
/// message.
#[derive(Debug, Serialize)]
struct RpcErrorData {
    /// Short snake_case code for programmatic handling by agents.
    code: &'static str,
    /// Actionable next step the agent should take.
    hint: String,
}

impl RpcErrorData {
    fn no_such_pane(prefix: &str) -> Self {
        Self {
            code: "no_such_pane",
            hint: format!(
                "No pane matches prefix '{prefix}'. Call list_panes to see active pane IDs."
            ),
        }
    }

    fn ambiguous_pane_id(prefix: &str, count: usize) -> Self {
        Self {
            code: "ambiguous_pane_id",
            hint: format!(
                "{count} panes match prefix '{prefix}'. Provide a longer prefix (≥12 chars)."
            ),
        }
    }

    fn no_such_session(prefix: &str) -> Self {
        Self {
            code: "no_such_session",
            hint: format!(
                "No session matches prefix '{prefix}'. Call list_sessions to see active session IDs."
            ),
        }
    }

    fn ambiguous_session_id(prefix: &str, count: usize) -> Self {
        Self {
            code: "ambiguous_session_id",
            hint: format!(
                "{count} sessions match prefix '{prefix}'. Provide a longer prefix (≥12 chars)."
            ),
        }
    }

    fn daemon_unreachable() -> Self {
        Self {
            code: "daemon_unreachable",
            hint: "Cannot connect to pyred. Start the daemon with `pyred` or check PYRE_SOCK."
                .to_owned(),
        }
    }
}

/// An error that carries structured data for agent consumption.
#[derive(Debug)]
struct StructuredError {
    message: String,
    data: RpcErrorData,
}

impl StructuredError {
    fn new(message: impl Into<String>, data: RpcErrorData) -> Self {
        Self {
            message: message.into(),
            data,
        }
    }
}

impl std::fmt::Display for StructuredError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for StructuredError {}

impl Response {
    fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn err(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    fn notification(method: &str, params: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        })
    }
}

// Socket helpers are provided by pyre_proto::{connect_control, default_socket}.

// ──────────────────────────────────────────────────────────────────────────────
// Server state
// ──────────────────────────────────────────────────────────────────────────────

type SubscriptionMap = Arc<Mutex<HashMap<String, JoinHandle<()>>>>;

struct Server {
    socket: PathBuf,
    /// Channel used to write lines back to stdout.
    tx: mpsc::Sender<String>,
    subscriptions: SubscriptionMap,
}

impl Server {
    fn new(socket: PathBuf, tx: mpsc::Sender<String>) -> Self {
        Self {
            socket,
            tx,
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn client(&self) -> Result<PyreDaemonClient> {
        connect_control(&self.socket).await.map_err(|e| {
            // Wrap connection errors with structured data so the top-level
            // handler can produce a machine-readable response.
            let structured = StructuredError::new(
                format!("daemon connection failed: {e}"),
                RpcErrorData::daemon_unreachable(),
            );
            anyhow::Error::new(structured)
        })
    }

    async fn send_line(&self, value: &Value) -> Result<()> {
        let line = serde_json::to_string(value)?;
        self.tx
            .send(line)
            .await
            .map_err(|_| anyhow!("stdout closed"))
    }

    // ── dispatch ─────────────────────────────────────────────────────────────

    async fn handle(&self, req: Request) -> Option<Response> {
        let id = req.id.clone();
        let result = self.dispatch(&req.method, req.params, req.id.clone()).await;
        match result {
            Ok(Some(val)) => Some(Response::ok(id, val)),
            Ok(None) => None, // notification sent inline
            Err(e) => {
                // Check if this is a StructuredError and include data field.
                if let Some(se) = e.downcast_ref::<StructuredError>() {
                    let data_val = serde_json::to_value(&se.data).ok();
                    Some(Response {
                        jsonrpc: "2.0",
                        id,
                        result: None,
                        error: Some(RpcError {
                            code: -32000,
                            message: se.message.clone(),
                            data: data_val,
                        }),
                    })
                } else {
                    Some(Response::err(id, -32000, e.to_string()))
                }
            }
        }
    }

    async fn dispatch(
        &self,
        method: &str,
        params: Value,
        _id: Option<Value>,
    ) -> Result<Option<Value>> {
        match method {
            "initialize" => Ok(Some(self.handle_initialize())),
            "initialized" => Ok(None), // ack, no response needed
            "resources/list" => Ok(Some(self.handle_resources_list().await?)),
            "resources/read" => Ok(Some(self.handle_resources_read(params).await?)),
            "resources/subscribe" => {
                self.handle_resources_subscribe(params).await?;
                Ok(Some(json!({})))
            }
            "resources/unsubscribe" => {
                self.handle_resources_unsubscribe(params).await?;
                Ok(Some(json!({})))
            }
            "tools/list" => Ok(Some(self.handle_tools_list())),
            "tools/call" => Ok(Some(self.handle_tools_call(params).await?)),
            // Silently ignore unknown notifications (no id).
            _ => Err(anyhow!("method not found: {method}")),
        }
    }

    // ── initialize ───────────────────────────────────────────────────────────

    fn handle_initialize(&self) -> Value {
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "resources": { "subscribe": true, "listChanged": false },
                "tools": { "listChanged": false }
            },
            "serverInfo": { "name": "pyre-mcp", "version": env!("CARGO_PKG_VERSION") }
        })
    }

    // ── resources/list ───────────────────────────────────────────────────────

    async fn handle_resources_list(&self) -> Result<Value> {
        let client = self.client().await?;

        let sessions = client
            .list_sessions(tarpc::context::current())
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;

        let mut resources: Vec<Value> = vec![json!({
            "uri": "state://panes",
            "name": "All panes (live state)",
            "description": "JSON array of every PaneInfo across all sessions",
            "mimeType": "application/json"
        })];

        for s in &sessions {
            let short = &s.id.0.to_string()[..8];
            resources.push(json!({
                "uri": format!("session://{short}"),
                "name": format!("Session {short}"),
                "description": format!("Session info + panes for {}", s.name),
                "mimeType": "application/json"
            }));

            let panes = client
                .list_panes(tarpc::context::current(), s.id)
                .await
                .context("rpc")?
                .map_err(|e| anyhow!("{e}"))?;

            for p in &panes {
                let ps = &p.id.0.to_string()[..8];
                resources.push(json!({
                    "uri": format!("pane://{short}/{ps}"),
                    "name": format!("Pane {ps}"),
                    "description": "PaneInfo metadata",
                    "mimeType": "application/json"
                }));
                resources.push(json!({
                    "uri": format!("pane://{short}/{ps}/output"),
                    "name": format!("Pane {ps} output"),
                    "description": "Last 200 lines of pane ring buffer",
                    "mimeType": "text/plain"
                }));
            }
        }

        // Top 100 blocks
        let blocks = client
            .list_blocks(
                tarpc::context::current(),
                ListBlocksReq {
                    session: None,
                    limit: 100,
                },
            )
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;

        for b in &blocks {
            let bs = &b.id.0.to_string()[..8];
            resources.push(json!({
                "uri": format!("block://{}", b.id.0),
                "name": format!("Block {bs}"),
                "description": format!("Block stdout: {}", b.command),
                "mimeType": "text/plain"
            }));
        }

        Ok(json!({ "resources": resources }))
    }

    // ── resources/read ───────────────────────────────────────────────────────

    async fn handle_resources_read(&self, params: Value) -> Result<Value> {
        let uri = params["uri"]
            .as_str()
            .ok_or_else(|| anyhow!("missing uri"))?
            .to_owned();

        let (mime, text) = self.read_uri(&uri).await?;

        Ok(json!({
            "contents": [{
                "uri": uri,
                "mimeType": mime,
                "text": text
            }]
        }))
    }

    async fn read_uri(&self, uri: &str) -> Result<(&'static str, String)> {
        let client = self.client().await?;

        if uri == "state://panes" {
            let panes = client
                .list_all_panes(tarpc::context::current())
                .await
                .context("rpc")?
                .map_err(|e| anyhow!("{e}"))?;
            return Ok(("application/json", serde_json::to_string_pretty(&panes)?));
        }

        if let Some(rest) = uri.strip_prefix("session://") {
            let prefix = rest;
            let sessions = client
                .list_sessions(tarpc::context::current())
                .await
                .context("rpc")?
                .map_err(|e| anyhow!("{e}"))?;
            let sess = sessions
                .iter()
                .find(|s| s.id.0.to_string().starts_with(prefix))
                .ok_or_else(|| anyhow!("no session matches '{prefix}'"))?;
            let panes = client
                .list_panes(tarpc::context::current(), sess.id)
                .await
                .context("rpc")?
                .map_err(|e| anyhow!("{e}"))?;
            let out = json!({ "session": sess, "panes": panes });
            return Ok(("application/json", serde_json::to_string_pretty(&out)?));
        }

        if let Some(rest) = uri.strip_prefix("pane://") {
            // formats: {sess}/{pane}  or  {sess}/{pane}/output
            let parts: Vec<&str> = rest.splitn(3, '/').collect();
            if parts.len() < 2 {
                return Err(anyhow!("invalid pane URI: {uri}"));
            }
            let sess_prefix = parts[0];
            let pane_prefix = parts[1];
            let is_output = parts.get(2) == Some(&"output");

            let all = client
                .list_all_panes(tarpc::context::current())
                .await
                .context("rpc")?
                .map_err(|e| anyhow!("{e}"))?;

            let pane = all
                .iter()
                .find(|p| {
                    p.session.0.to_string().starts_with(sess_prefix)
                        && p.id.0.to_string().starts_with(pane_prefix)
                })
                .ok_or_else(|| anyhow!("no pane matches '{uri}'"))?;

            if is_output {
                let bytes = client
                    .capture_pane(tarpc::context::current(), pane.id, 200)
                    .await
                    .context("rpc")?
                    .map_err(|e| anyhow!("{e}"))?;
                return Ok(("text/plain", String::from_utf8_lossy(&bytes).into_owned()));
            } else {
                return Ok(("application/json", serde_json::to_string_pretty(pane)?));
            }
        }

        if let Some(rest) = uri.strip_prefix("block://") {
            let block_id_str = rest;
            // Parse UUID
            let uuid = uuid::Uuid::parse_str(block_id_str)
                .map_err(|_| anyhow!("invalid block UUID: {block_id_str}"))?;
            let block_id = pyre_proto::BlockId(uuid);
            let bytes = client
                .get_block_stdout(tarpc::context::current(), block_id)
                .await
                .context("rpc")?
                .map_err(|e| anyhow!("{e}"))?;
            return Ok(("text/plain", String::from_utf8_lossy(&bytes).into_owned()));
        }

        Err(anyhow!("unknown URI scheme: {uri}"))
    }

    // ── resources/subscribe ──────────────────────────────────────────────────

    async fn handle_resources_subscribe(&self, params: Value) -> Result<()> {
        let uri = params["uri"]
            .as_str()
            .ok_or_else(|| anyhow!("missing uri"))?
            .to_owned();

        let mut subs = self.subscriptions.lock().await;
        if subs.contains_key(&uri) {
            return Ok(()); // already subscribed — idempotent
        }

        let socket = self.socket.clone();
        let tx = self.tx.clone();
        let uri_clone = uri.clone();

        let handle = tokio::spawn(async move {
            // Long-poll `next_pane_event` instead of 1 s polling on list_all_panes.
            // seq=0 means "give me everything from the beginning"; each iteration
            // advances seq to the last event seen so reconnects don't re-notify.
            let mut seq: u64 = 0;
            let mut backoff = std::time::Duration::from_millis(100);

            loop {
                let client = match connect_control(&socket).await {
                    Ok(c) => c,
                    Err(_) => {
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(std::time::Duration::from_secs(5));
                        continue;
                    }
                };

                match client
                    .next_pane_event(tarpc::context::current(), seq, 30_000)
                    .await
                {
                    Ok(Ok(events)) if !events.is_empty() => {
                        // Advance cursor to the highest seq we received.
                        if let Some(last) = events.last() {
                            seq = last.seq;
                        }
                        backoff = std::time::Duration::from_millis(100);
                        // Any lifecycle event on a pane resource triggers an update
                        // notification so the MCP client re-reads the resource.
                        let notif = Response::notification(
                            "notifications/resources/updated",
                            json!({ "uri": uri_clone }),
                        );
                        if let Ok(line) = serde_json::to_string(&notif) {
                            if tx.send(line).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(Ok(_)) => {
                        // Empty response = normal long-poll timeout; loop immediately.
                        backoff = std::time::Duration::from_millis(100);
                    }
                    _ => {
                        // RPC error or transport failure — back off and reconnect.
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(std::time::Duration::from_secs(5));
                    }
                }
            }
        });

        subs.insert(uri, handle);
        Ok(())
    }

    // ── resources/unsubscribe ────────────────────────────────────────────────

    async fn handle_resources_unsubscribe(&self, params: Value) -> Result<()> {
        let uri = params["uri"]
            .as_str()
            .ok_or_else(|| anyhow!("missing uri"))?;
        let mut subs = self.subscriptions.lock().await;
        if let Some(handle) = subs.remove(uri) {
            handle.abort();
        }
        Ok(())
    }

    // ── tools/list ───────────────────────────────────────────────────────────

    fn handle_tools_list(&self) -> Value {
        json!({
            "tools": [
                {
                    "name": "pane_send_keys",
                    "description": "Inject keystrokes into a pane. The pane argument accepts an 8+ char prefix of the pane UUID.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "pane": { "type": "string", "description": "Pane id or ≥8-char prefix" },
                            "text": { "type": "string", "description": "Text to inject" },
                            "append_enter": { "type": "boolean", "description": "Append \\r (Enter) after text", "default": false }
                        },
                        "required": ["pane", "text"]
                    }
                },
                {
                    "name": "pane_capture",
                    "description": "Capture pane output: ring buffer (default) or last finalized block stdout (block-last).",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "pane": { "type": "string", "description": "Pane id or ≥8-char prefix" },
                            "lines": { "type": "integer", "description": "Ring lines when source=ring (default 50)", "default": 50 },
                            "source": { "type": "string", "enum": ["ring", "block-last"], "default": "ring" }
                        },
                        "required": ["pane"]
                    }
                },
                {
                    "name": "pane_set_state",
                    "description": "Self-report pane state. Valid states: Running, WaitingInput, Idle, Interactive, Crashed, Done.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "pane": { "type": "string", "description": "Pane id or ≥8-char prefix" },
                            "state": { "type": "string", "enum": ["Running", "WaitingInput", "Idle", "Interactive", "Crashed", "Done"] },
                            "reason": { "type": "string", "description": "Human-readable reason for the state change" }
                        },
                        "required": ["pane", "state"]
                    }
                },
                {
                    "name": "block_search",
                    "description": "Full-text search across all blocks (stdout history). Optionally scoped to a session, pane, or exact exit code.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string", "description": "Full-text search query against block stdout." },
                            "limit": { "type": "integer", "default": 20, "description": "Maximum number of hits to return." },
                            "failures_only": {
                                "type": "boolean",
                                "default": false,
                                "description": "When true, restrict to blocks with non-zero exit code. Ignored when exit_code is set."
                            },
                            "session": {
                                "type": "string",
                                "description": "Optional session UUID prefix (≥8 chars) — restrict results to this session."
                            },
                            "pane": {
                                "type": "string",
                                "description": "Optional pane UUID prefix (≥8 chars) — restrict results to this pane."
                            },
                            "exit_code": {
                                "type": "integer",
                                "description": "Optional exact exit code filter. When set, failures_only is ignored."
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "session_spawn",
                    "description": "Spawn a new session with one pane. Returns session_id and pane_id.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "shell": { "type": "string", "description": "Shell binary (default: $SHELL)" },
                            "cwd": { "type": "string", "description": "Working directory" },
                            "cols": { "type": "integer", "default": 80 },
                            "rows": { "type": "integer", "default": 24 }
                        }
                    }
                },
                {
                    "name": "session_close",
                    "description": "Close a session and all its panes.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session": { "type": "string", "description": "Session id or ≥8-char prefix" }
                        },
                        "required": ["session"]
                    }
                },
                {
                    "name": "pane_open",
                    "description": "Open a new pane inside an existing session. Returns pane_id.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session": { "type": "string", "description": "Session id or ≥8-char prefix" },
                            "name": { "type": "string", "description": "Optional human-readable label for the pane" },
                            "cols": { "type": "integer", "default": 80 },
                            "rows": { "type": "integer", "default": 24 },
                            "cwd": { "type": "string", "description": "Working directory (optional)" },
                            "shell": { "type": "string", "description": "Shell binary (default: $SHELL)" }
                        },
                        "required": ["session"]
                    }
                },
                {
                    "name": "wait_pane_state",
                    "description": "Wait until a pane reaches a lifecycle state (blocked=WaitingInput, working=Running, etc.).",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "pane": { "type": "string" },
                            "state": { "type": "string", "description": "waiting, running, idle, done, crashed, interactive" },
                            "timeout_secs": { "type": "integer", "default": 30 }
                        },
                        "required": ["pane", "state"]
                    }
                },
                {
                    "name": "list_sessions",
                    "description": "List all active pyre sessions with metadata.",
                    "inputSchema": { "type": "object", "properties": {}, "required": [] }
                },
                {
                    "name": "list_panes",
                    "description": "List panes optionally filtered by session_id.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session_id": { "type": "string", "description": "Filter to a session (optional)." }
                        },
                        "required": []
                    }
                },
                {
                    "name": "gc_stale_sessions",
                    "description": "Evict sessions that have no live panes. Returns the list of evicted session UUIDs.",
                    "inputSchema": { "type": "object", "properties": {}, "required": [] }
                },
                {
                    "name": "session_layout",
                    "description": "Create a session with panes preconfigured via a split spec. Accepts either a flat `panes` array (back-compat) or a `layout` object with `orient` and `panes`. When `layout` is provided, panes are created using open_pane_split RPC calls so the daemon tracks topology. The `orient` field (\"horizontal\"|\"vertical\") is applied uniformly: each subsequent pane is split off the previous one at that orientation. Nested specs are not yet supported — all panes are placed at the same split level.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "description": "Session name." },
                            "panes": {
                                "type": "array",
                                "description": "Flat pane list (back-compat). First pane reuses the session's initial pane.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "name": { "type": "string" },
                                        "cwd": { "type": "string" },
                                        "cmd": { "type": "string", "description": "Command sent via send_keys with trailing \\n." }
                                    },
                                    "required": []
                                }
                            },
                            "layout": {
                                "type": "object",
                                "description": "Split spec. When present, `panes` at root level is ignored.",
                                "properties": {
                                    "orient": { "type": "string", "enum": ["horizontal", "vertical"], "description": "Split orientation applied to all pane splits." },
                                    "panes": {
                                        "type": "array",
                                        "description": "Ordered list of pane specs. First pane reuses the session's initial pane; each subsequent pane is created via open_pane_split.",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "name": { "type": "string" },
                                                "cwd": { "type": "string" },
                                                "cmd": { "type": "string", "description": "Command sent via send_keys with trailing \\n." }
                                            },
                                            "required": []
                                        }
                                    }
                                },
                                "required": ["orient", "panes"]
                            }
                        },
                        "required": ["name"]
                    }
                },
                {
                    "name": "set_pane_weight",
                    "description": "Adjust the weight of a pane within its parent split (0-100). The daemon clamps the value to [5, 95] and rebalances siblings so all weights sum to 100. Persists to SQLite and emits LayoutChanged.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "pane_id": { "type": "string", "description": "Full pane UUID or ≥8-char prefix." },
                            "weight": { "type": "integer", "minimum": 0, "maximum": 100, "description": "Desired weight (0-100). Clamped to [5, 95] by the daemon." }
                        },
                        "required": ["pane_id", "weight"]
                    }
                },
                {
                    "name": "get_session_layout",
                    "description": "Return the LayoutNode tree for a session as JSON. The tree is a recursive structure of HSplit/VSplit nodes (each child has a weight 0-100) and Leaf nodes (carrying a PaneId UUID).",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session_id": { "type": "string", "description": "Full session UUID or ≥8-char prefix." }
                        },
                        "required": ["session_id"]
                    }
                },
                {
                    "name": "open_pane_split",
                    "description": "Split an existing pane: create a new pane next to it. orient = horizontal | vertical.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "parent_pane_id": { "type": "string", "description": "UUID prefix of the existing pane to split off." },
                            "orient": { "type": "string", "enum": ["horizontal", "vertical"], "description": "horizontal = side-by-side, vertical = stacked." },
                            "name": { "type": "string" },
                            "cwd": { "type": "string" },
                            "cmd": { "type": "string", "description": "Optional command sent via send_keys after spawn." }
                        },
                        "required": ["parent_pane_id", "orient"]
                    }
                },
                {
                    "name": "pane_last_block",
                    "description": "Return metadata for the most recently finalized block (command) on a pane.\n\nUse this when you need the exit code, duration, or cwd of the last command that ran in a pane. Returns null when no block has been recorded yet (e.g. the shell is at a fresh prompt and OSC 133 integration has not fired).\n\nWhen include_output is true the response includes the block's stdout, truncated to the last 8 KB with a truncation marker when the full output is larger.\n\nErrors: no_such_pane when the prefix does not match any live pane; ambiguous_pane_id when multiple panes match.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "pane": {
                                "type": "string",
                                "description": "Pane UUID or ≥8-char prefix."
                            },
                            "include_output": {
                                "type": "boolean",
                                "default": false,
                                "description": "When true, fetch and include the block's stdout (truncated to 8 KB)."
                            }
                        },
                        "required": ["pane"]
                    }
                },
                {
                    "name": "pane_run_command",
                    "description": "Send a shell command to a pane and wait for it to finish, returning the exit code and output in one call.\n\nThis is the preferred tool for running a command and observing its result. It replaces the pane_send_keys → sleep → pane_capture antipattern.\n\nBehavior:\n1. Records the current last block id for the pane.\n2. Sends `command` followed by Enter.\n3. Polls every 150 ms until a NEW finalized block appears (exit code set) or timeout_secs elapses.\n4. Returns {completed, exit_code, duration_ms, cwd, command, output, block_id}.\n\nWhen completed is false the command either timed out or no block was detected. If block_id is absent it means no new block formed — this usually indicates OSC 133 shell integration is not active. Run `pyrec shell-init` in the shell to enable it.\n\noutput is included when include_output is true (default). Output is truncated to 8 KB; the truncation marker shows the full byte size.\n\nErrors: no_such_pane, ambiguous_pane_id, daemon_unreachable.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "pane": {
                                "type": "string",
                                "description": "Pane UUID or ≥8-char prefix."
                            },
                            "command": {
                                "type": "string",
                                "description": "Shell command to run (Enter is appended automatically)."
                            },
                            "timeout_secs": {
                                "type": "integer",
                                "default": 30,
                                "description": "Seconds to wait for the command to finish. Returns completed=false on timeout."
                            },
                            "include_output": {
                                "type": "boolean",
                                "default": true,
                                "description": "When true (default), include stdout in the response (truncated to 8 KB)."
                            }
                        },
                        "required": ["pane", "command"]
                    }
                }
            ]
        })
    }

    // ── tools/call ───────────────────────────────────────────────────────────

    async fn handle_tools_call(&self, params: Value) -> Result<Value> {
        let name = params["name"]
            .as_str()
            .ok_or_else(|| anyhow!("missing name"))?;
        let args = &params["arguments"];

        let text = match name {
            "pane_send_keys" => self.tool_pane_send_keys(args).await?,
            "pane_capture" => self.tool_pane_capture(args).await?,
            "pane_set_state" => self.tool_pane_set_state(args).await?,
            "block_search" => self.tool_block_search(args).await?,
            "session_spawn" => self.tool_session_spawn(args).await?,
            "session_close" => self.tool_session_close(args).await?,
            "pane_open" => self.tool_pane_open(args).await?,
            "wait_pane_state" => self.tool_wait_pane_state(args).await?,
            "list_sessions" => self.tool_list_sessions().await?,
            "list_panes" => self.tool_list_panes(args).await?,
            "session_layout" => self.tool_session_layout(args).await?,
            "gc_stale_sessions" => self.tool_gc_stale_sessions().await?,
            "set_pane_weight" => self.tool_set_pane_weight(args).await?,
            "get_session_layout" => self.tool_get_session_layout(args).await?,
            "open_pane_split" => self.tool_open_pane_split(args).await?,
            "pane_last_block" => self.tool_pane_last_block(args).await?,
            "pane_run_command" => self.tool_pane_run_command(args).await?,
            other => return Err(anyhow!("unknown tool: {other}")),
        };

        Ok(json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false
        }))
    }

    async fn resolve_pane_id(
        &self,
        client: &PyreDaemonClient,
        prefix: &str,
    ) -> Result<pyre_proto::PaneId> {
        let all = client
            .list_all_panes(tarpc::context::current())
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;

        let matches: Vec<_> = all
            .iter()
            .filter(|p| p.id.0.to_string().starts_with(prefix))
            .collect();

        match matches.len() {
            0 => Err(anyhow::Error::new(StructuredError::new(
                format!("no pane matches prefix '{prefix}'"),
                RpcErrorData::no_such_pane(prefix),
            ))),
            1 => Ok(matches[0].id),
            n => Err(anyhow::Error::new(StructuredError::new(
                format!("{n} panes match prefix '{prefix}'; provide a longer prefix"),
                RpcErrorData::ambiguous_pane_id(prefix, n),
            ))),
        }
    }

    async fn resolve_session_id(
        &self,
        client: &PyreDaemonClient,
        prefix: &str,
    ) -> Result<SessionId> {
        let sessions = client
            .list_sessions(tarpc::context::current())
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;

        let matches: Vec<_> = sessions
            .iter()
            .filter(|s| s.id.0.to_string().starts_with(prefix))
            .collect();

        match matches.len() {
            0 => Err(anyhow::Error::new(StructuredError::new(
                format!("no session matches prefix '{prefix}'"),
                RpcErrorData::no_such_session(prefix),
            ))),
            1 => Ok(matches[0].id),
            n => Err(anyhow::Error::new(StructuredError::new(
                format!("{n} sessions match prefix '{prefix}'; provide a longer prefix"),
                RpcErrorData::ambiguous_session_id(prefix, n),
            ))),
        }
    }

    async fn tool_pane_send_keys(&self, args: &Value) -> Result<String> {
        let pane_prefix = args["pane"]
            .as_str()
            .ok_or_else(|| anyhow!("missing pane"))?;
        let text = args["text"]
            .as_str()
            .ok_or_else(|| anyhow!("missing text"))?;
        let append_enter = args["append_enter"].as_bool().unwrap_or(false);

        let mut payload = text.to_owned();
        if append_enter {
            payload.push('\r');
        }

        let client = self.client().await?;
        let pane_id = self.resolve_pane_id(&client, pane_prefix).await?;

        client
            .send_keys(tarpc::context::current(), pane_id, payload.into_bytes())
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;

        Ok(format!("keys sent to pane {}", &pane_id.0.to_string()[..8]))
    }

    async fn tool_pane_capture(&self, args: &Value) -> Result<String> {
        let pane_prefix = args["pane"]
            .as_str()
            .ok_or_else(|| anyhow!("missing pane"))?;
        let lines = args["lines"].as_u64().unwrap_or(50) as u32;
        let source = args["source"].as_str().unwrap_or("ring");

        let client = self.client().await?;
        let pane_id = self.resolve_pane_id(&client, pane_prefix).await?;

        let bytes = if source == "block-last" {
            let block = client
                .last_block_for_pane(tarpc::context::current(), pane_id)
                .await
                .context("rpc")?
                .map_err(|e| anyhow!("{e}"))?;
            match block {
                Some(b) => client
                    .get_block_stdout(tarpc::context::current(), b.id)
                    .await
                    .context("rpc")?
                    .map_err(|e| anyhow!("{e}"))?,
                None => Vec::new(),
            }
        } else {
            client
                .capture_pane(tarpc::context::current(), pane_id, lines)
                .await
                .context("rpc")?
                .map_err(|e| anyhow!("{e}"))?
        };

        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn tool_wait_pane_state(&self, args: &Value) -> Result<String> {
        let pane_prefix = args["pane"]
            .as_str()
            .ok_or_else(|| anyhow!("missing pane"))?;
        let state_str = args["state"]
            .as_str()
            .ok_or_else(|| anyhow!("missing state"))?;
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(30) as u32;

        let kind = match state_str.to_lowercase().as_str() {
            "running" | "working" => PaneStateKind::Running,
            "waiting" | "waitinginput" | "blocked" => PaneStateKind::WaitingInput,
            "idle" => PaneStateKind::Idle,
            "interactive" => PaneStateKind::Interactive,
            "crashed" => PaneStateKind::Crashed,
            "done" => PaneStateKind::Done,
            other => return Err(anyhow!("unknown state: {other}")),
        };

        let client = self.client().await?;
        let pane_id = self.resolve_pane_id(&client, pane_prefix).await?;
        let reached = client
            .wait_pane_state(
                tarpc::context::current(),
                pane_id,
                kind,
                timeout_secs.saturating_mul(1000),
            )
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;

        if reached {
            Ok(format!(
                "pane {} reached {state_str}",
                &pane_id.0.to_string()[..8]
            ))
        } else {
            Err(anyhow!("timeout after {timeout_secs}s"))
        }
    }

    async fn tool_pane_set_state(&self, args: &Value) -> Result<String> {
        let pane_prefix = args["pane"]
            .as_str()
            .ok_or_else(|| anyhow!("missing pane"))?;
        let state_str = args["state"]
            .as_str()
            .ok_or_else(|| anyhow!("missing state"))?;
        let reason = args["reason"].as_str().unwrap_or("set via MCP").to_owned();

        let state = match state_str {
            "Running" => PaneStateKind::Running,
            "WaitingInput" => PaneStateKind::WaitingInput,
            "Idle" => PaneStateKind::Idle,
            "Interactive" => PaneStateKind::Interactive,
            "Crashed" => PaneStateKind::Crashed,
            "Done" => PaneStateKind::Done,
            other => return Err(anyhow!("unknown state: {other}")),
        };

        let client = self.client().await?;
        let pane_id = self.resolve_pane_id(&client, pane_prefix).await?;
        client
            .set_pane_state(tarpc::context::current(), pane_id, state, reason)
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;

        Ok(format!(
            "pane {} state set to {state_str}",
            &pane_id.0.to_string()[..8]
        ))
    }

    async fn tool_block_search(&self, args: &Value) -> Result<String> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| anyhow!("missing query"))?
            .to_owned();
        let limit = args["limit"].as_u64().unwrap_or(20) as u32;
        let failures_only = args["failures_only"].as_bool().unwrap_or(false);

        // Resolve optional session filter by prefix.
        let client = self.client().await?;
        let session = if let Some(s) = args["session"].as_str() {
            Some(self.resolve_session_id(&client, s).await?)
        } else {
            None
        };

        // Resolve optional pane filter by prefix.
        let pane = if let Some(p) = args["pane"].as_str() {
            Some(self.resolve_pane_id(&client, p).await?)
        } else {
            None
        };

        let exit_code = args["exit_code"].as_i64().map(|v| v as i32);

        let hits = client
            .search_blocks(
                tarpc::context::current(),
                SearchBlocksReq {
                    query,
                    limit,
                    failures_only,
                    session,
                    pane,
                    exit_code,
                },
            )
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;

        if hits.is_empty() {
            return Ok("no results".to_owned());
        }

        let mut out = String::new();
        for hit in &hits {
            let short = &hit.block.id.0.to_string()[..8];
            out.push_str(&format!("[{short}] {}\n", hit.block.command));
            out.push_str(&format!("    {}\n", hit.snippet.replace('\n', " ")));
        }
        Ok(out)
    }

    async fn tool_session_spawn(&self, args: &Value) -> Result<String> {
        let shell = args["shell"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| std::env::var("SHELL").ok());
        let cwd = args["cwd"].as_str().map(PathBuf::from);
        let cols = args["cols"].as_u64().unwrap_or(80) as u16;
        let rows = args["rows"].as_u64().unwrap_or(24) as u16;

        let client = self.client().await?;
        let req = SpawnReq {
            shell,
            cwd,
            cols,
            rows,
            env: std::env::vars().collect(),
            name: None,
        };
        let resp = client
            .spawn(tarpc::context::current(), req)
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;

        Ok(format!(
            "session_id={} pane_id={}",
            resp.session.0, resp.pane.0
        ))
    }

    async fn tool_session_close(&self, args: &Value) -> Result<String> {
        let prefix = args["session"]
            .as_str()
            .ok_or_else(|| anyhow!("missing session"))?;

        let client = self.client().await?;
        let session_id = self.resolve_session_id(&client, prefix).await?;
        client
            .close_session(tarpc::context::current(), session_id)
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;

        Ok(format!("session {} closed", &session_id.0.to_string()[..8]))
    }

    async fn tool_list_sessions(&self) -> Result<String> {
        let client = self.client().await?;
        let sessions = client
            .list_sessions(tarpc::context::current())
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;

        let out: Vec<Value> = sessions
            .iter()
            .map(|s| {
                json!({
                    "session_id": s.id.0.to_string(),
                    "name": s.name,
                    "pane_count": s.pane_count,
                })
            })
            .collect();

        Ok(serde_json::to_string_pretty(&json!({ "sessions": out }))?)
    }

    async fn tool_list_panes(&self, args: &Value) -> Result<String> {
        let session_filter = args["session_id"].as_str().map(str::to_owned);

        let client = self.client().await?;
        let all = client
            .list_all_panes(tarpc::context::current())
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;

        let panes: Vec<Value> = all
            .iter()
            .filter(|p| {
                session_filter
                    .as_deref()
                    .map(|f| p.session.0.to_string().starts_with(f))
                    .unwrap_or(true)
            })
            .map(|p| {
                json!({
                    "pane_id": p.id.0.to_string(),
                    "session_id": p.session.0.to_string(),
                    "name": p.name,
                    "agent": p.agent.label(),
                    "state": p.state.to_string(),
                    "seen": p.seen,
                })
            })
            .collect();

        Ok(serde_json::to_string_pretty(&json!({ "panes": panes }))?)
    }

    async fn tool_session_layout(&self, args: &Value) -> Result<String> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| anyhow!("missing name"))?
            .to_owned();

        // Determine whether to use the new split-spec path or the legacy flat path.
        if let Some(layout_spec) = args.get("layout").filter(|v| v.is_object()) {
            return self.tool_session_layout_split(name, layout_spec).await;
        }

        // ── Legacy flat-panes path (back-compat) ──────────────────────────────
        let pane_specs = args["panes"].as_array().cloned().unwrap_or_default();

        // Derive initial cwd from the first pane spec, if any.
        let first_cwd = pane_specs
            .first()
            .and_then(|p| p["cwd"].as_str())
            .map(PathBuf::from);

        let client = self.client().await?;

        // 1. Spawn the session (one default pane is created by pyred).
        let spawn_req = SpawnReq {
            shell: std::env::var("SHELL").ok(),
            cwd: first_cwd,
            cols: 80,
            rows: 24,
            env: std::env::vars().collect(),
            name: Some(name),
        };
        let spawn_resp = client
            .spawn(tarpc::context::current(), spawn_req)
            .await
            .context("rpc spawn")?
            .map_err(|e| anyhow!("{e}"))?;

        let session_id = spawn_resp.session;
        let initial_pane_id = spawn_resp.pane;

        let mut result_panes: Vec<Value> = Vec::new();

        // 2. Process each pane spec.
        for (idx, spec) in pane_specs.iter().enumerate() {
            let pane_name = spec["name"].as_str().unwrap_or("").to_owned();
            let cwd = spec["cwd"].as_str().map(PathBuf::from);
            let cmd = spec["cmd"].as_str().map(str::to_owned);

            let pane_id = if idx == 0 {
                // Reuse the pane already created by session_spawn.
                initial_pane_id
            } else {
                // Open an additional pane in the session.
                let req = OpenPaneReq {
                    session: session_id,
                    window: WindowId::default(),
                    shell: std::env::var("SHELL").ok(),
                    cwd,
                    cols: 80,
                    rows: 24,
                    env: std::env::vars().collect(),
                    name: if pane_name.is_empty() {
                        None
                    } else {
                        Some(pane_name.clone())
                    },
                };
                client
                    .open_pane(tarpc::context::current(), req)
                    .await
                    .context("rpc open_pane")?
                    .map_err(|e| anyhow!("{e}"))?
            };

            // Send optional startup command.
            if let Some(c) = cmd {
                let payload = format!("{c}\n").into_bytes();
                client
                    .send_keys(tarpc::context::current(), pane_id, payload)
                    .await
                    .context("rpc send_keys")?
                    .map_err(|e| anyhow!("{e}"))?;
            }

            result_panes.push(json!({
                "pane_id": pane_id.0.to_string(),
                "name": pane_name,
                "path": [idx],
            }));
        }

        // When no pane specs were provided, still report the initial pane.
        if pane_specs.is_empty() {
            result_panes.push(json!({
                "pane_id": initial_pane_id.0.to_string(),
                "name": "",
                "path": [0],
            }));
        }

        Ok(serde_json::to_string_pretty(&json!({
            "session_id": session_id.0.to_string(),
            "panes": result_panes,
        }))?)
    }

    /// Inner helper for the new split-spec path in `session_layout`.
    ///
    /// Accepts a `layout` object:
    /// ```json
    /// { "orient": "horizontal" | "vertical", "panes": [ {name?, cwd?, cmd?}, ... ] }
    /// ```
    ///
    /// The first pane reuses the session's initial pane.  Each subsequent
    /// pane is created via `open_pane_split` so the daemon tracks the topology
    /// and emits `LayoutChanged`.  All splits share the same `orient`.
    async fn tool_session_layout_split(
        &self,
        session_name: String,
        layout_spec: &Value,
    ) -> Result<String> {
        let orient_str = layout_spec["orient"]
            .as_str()
            .ok_or_else(|| anyhow!("layout.orient is required"))?;
        let orient = match orient_str {
            "horizontal" => Orient::Horizontal,
            "vertical" => Orient::Vertical,
            other => {
                return Err(anyhow!(
                    "unknown orient '{other}'; expected horizontal|vertical"
                ))
            }
        };

        let pane_specs = layout_spec["panes"]
            .as_array()
            .ok_or_else(|| anyhow!("layout.panes must be an array"))?
            .clone();

        if pane_specs.is_empty() {
            return Err(anyhow!("layout.panes must contain at least one entry"));
        }

        // Derive initial cwd from the first pane spec.
        let first_cwd = pane_specs
            .first()
            .and_then(|p| p["cwd"].as_str())
            .map(PathBuf::from);

        let client = self.client().await?;

        // 1. Spawn the session — pyred creates the initial pane automatically.
        let spawn_req = SpawnReq {
            shell: std::env::var("SHELL").ok(),
            cwd: first_cwd,
            cols: 80,
            rows: 24,
            env: std::env::vars().collect(),
            name: Some(session_name),
        };
        let spawn_resp = client
            .spawn(tarpc::context::current(), spawn_req)
            .await
            .context("rpc spawn")?
            .map_err(|e| anyhow!("{e}"))?;

        let session_id = spawn_resp.session;
        // The pane that `open_pane_split` will split off from.
        let mut last_pane_id = spawn_resp.pane;

        let mut result_panes: Vec<Value> = Vec::new();

        // 2. Walk the spec; first entry reuses initial pane, rest use open_pane_split.
        for (idx, spec) in pane_specs.iter().enumerate() {
            let pane_name = spec["name"].as_str().unwrap_or("").to_owned();
            let cwd = spec["cwd"].as_str().map(PathBuf::from);
            let cmd = spec["cmd"].as_str().map(str::to_owned);

            let pane_id = if idx == 0 {
                // Reuse the initial pane; optionally send its command below.
                last_pane_id
            } else {
                // Split the previous pane, producing a new sibling.
                let split_req = OpenPaneSplitReq {
                    parent_pane: last_pane_id,
                    orient,
                    name: if pane_name.is_empty() {
                        None
                    } else {
                        Some(pane_name.clone())
                    },
                    cwd,
                    cmd: None, // cmd is delivered via send_keys below
                };
                let new_id = client
                    .open_pane_split(tarpc::context::current(), split_req)
                    .await
                    .context("rpc open_pane_split")?
                    .map_err(|e| anyhow!("{e}"))?;
                last_pane_id = new_id;
                new_id
            };

            // Send optional startup command via send_keys.
            if let Some(c) = cmd {
                let payload = format!("{c}\n").into_bytes();
                client
                    .send_keys(tarpc::context::current(), pane_id, payload)
                    .await
                    .context("rpc send_keys")?
                    .map_err(|e| anyhow!("{e}"))?;
            }

            result_panes.push(json!({
                "pane_id": pane_id.0.to_string(),
                "name": pane_name,
                "path": [idx],
            }));
        }

        Ok(serde_json::to_string_pretty(&json!({
            "session_id": session_id.0.to_string(),
            "panes": result_panes,
        }))?)
    }

    async fn tool_set_pane_weight(&self, args: &Value) -> Result<String> {
        let pane_prefix = args["pane_id"]
            .as_str()
            .ok_or_else(|| anyhow!("missing pane_id"))?;
        let weight = args["weight"]
            .as_u64()
            .ok_or_else(|| anyhow!("missing weight"))?;

        if weight > 100 {
            return Err(anyhow!("weight must be 0-100"));
        }

        let client = self.client().await?;
        let pane_id = self.resolve_pane_id(&client, pane_prefix).await?;

        client
            .set_pane_weight(tarpc::context::current(), pane_id, weight as u16)
            .await
            .context("rpc set_pane_weight")?
            .map_err(|e| anyhow!("{e}"))?;

        Ok(format!(
            "pane {} weight set to {weight}",
            &pane_id.0.to_string()[..8]
        ))
    }

    async fn tool_get_session_layout(&self, args: &Value) -> Result<String> {
        let session_prefix = args["session_id"]
            .as_str()
            .ok_or_else(|| anyhow!("missing session_id"))?;

        let client = self.client().await?;
        let session_id = self.resolve_session_id(&client, session_prefix).await?;

        let layout = client
            .get_session_layout(tarpc::context::current(), session_id)
            .await
            .context("rpc get_session_layout")?
            .map_err(|e| anyhow!("{e}"))?;

        Ok(serde_json::to_string_pretty(&layout)?)
    }

    async fn tool_gc_stale_sessions(&self) -> Result<String> {
        let client = self.client().await?;
        let evicted = client
            .gc_stale_sessions(tarpc::context::current())
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;
        let count = evicted.len();
        Ok(serde_json::to_string_pretty(
            &json!({ "evicted_count": count, "evicted": evicted }),
        )?)
    }

    async fn tool_pane_open(&self, args: &Value) -> Result<String> {
        let session_prefix = args["session"]
            .as_str()
            .ok_or_else(|| anyhow!("missing session"))?;
        let shell = args["shell"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| std::env::var("SHELL").ok());
        let cwd = args["cwd"].as_str().map(PathBuf::from);
        let cols = args["cols"].as_u64().unwrap_or(80) as u16;
        let rows = args["rows"].as_u64().unwrap_or(24) as u16;

        let client = self.client().await?;
        let session_id = self.resolve_session_id(&client, session_prefix).await?;

        let pane_name = args["name"].as_str().map(str::to_owned);
        let req = OpenPaneReq {
            session: session_id,
            window: WindowId::default(),
            shell,
            cwd,
            cols,
            rows,
            env: std::env::vars().collect(),
            name: pane_name,
        };
        let pane_id = client
            .open_pane(tarpc::context::current(), req)
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;

        Ok(format!("pane_id={}", pane_id.0))
    }

    async fn tool_open_pane_split(&self, args: &Value) -> Result<String> {
        let parent_prefix = args["parent_pane_id"]
            .as_str()
            .ok_or_else(|| anyhow!("missing parent_pane_id"))?;
        let orient_str = args["orient"]
            .as_str()
            .ok_or_else(|| anyhow!("missing orient"))?;
        let orient = match orient_str {
            "horizontal" => Orient::Horizontal,
            "vertical" => Orient::Vertical,
            other => {
                return Err(anyhow!(
                    "unknown orient '{other}'; expected horizontal|vertical"
                ))
            }
        };
        let name = args["name"].as_str().map(str::to_owned);
        let cwd = args["cwd"].as_str().map(PathBuf::from);
        let cmd = args["cmd"].as_str().map(str::to_owned);

        let client = self.client().await?;
        let parent_pane_id = self.resolve_pane_id(&client, parent_prefix).await?;

        let req = OpenPaneSplitReq {
            parent_pane: parent_pane_id,
            orient,
            name,
            cwd,
            cmd: None, // cmd delivered via send_keys below
        };
        let new_pane_id = client
            .open_pane_split(tarpc::context::current(), req)
            .await
            .context("rpc open_pane_split")?
            .map_err(|e| anyhow!("{e}"))?;

        // Send optional startup command via send_keys.
        if let Some(c) = cmd {
            let payload = format!("{c}\n").into_bytes();
            client
                .send_keys(tarpc::context::current(), new_pane_id, payload)
                .await
                .context("rpc send_keys")?
                .map_err(|e| anyhow!("{e}"))?;
        }

        Ok(serde_json::to_string(&serde_json::json!({
            "pane_id": new_pane_id.0.to_string()
        }))?)
    }

    // ── pane_last_block ───────────────────────────────────────────────────────

    async fn tool_pane_last_block(&self, args: &Value) -> Result<String> {
        let pane_prefix = args["pane"]
            .as_str()
            .ok_or_else(|| anyhow!("missing pane"))?;
        let include_output = args["include_output"].as_bool().unwrap_or(false);

        let client = self.client().await?;
        let pane_id = self.resolve_pane_id(&client, pane_prefix).await?;

        let block = client
            .last_block_for_pane(tarpc::context::current(), pane_id)
            .await
            .context("rpc last_block_for_pane")?
            .map_err(|e| anyhow!("{e}"))?;

        let Some(block) = block else {
            return Ok(serde_json::to_string_pretty(&json!({
                "block": null,
                "hint": "No block recorded yet. The shell may be at a fresh prompt or OSC 133 integration is not active. Run `pyrec shell-init` to enable shell integration."
            }))?);
        };

        let mut result = json!({
            "block_id": block.id.0.to_string(),
            "command": block.command,
            "exit_code": block.exit_code,
            "duration_ms": block.duration_ms(),
            "cwd": block.cwd,
            "started_at": block.started_at.to_rfc3339(),
            "ended_at": block.ended_at.map(|t| t.to_rfc3339()),
            "stdout_len": block.stdout_len,
        });

        if include_output {
            let raw = client
                .get_block_stdout(tarpc::context::current(), block.id)
                .await
                .context("rpc get_block_stdout")?
                .map_err(|e| anyhow!("{e}"))?;

            let (output_text, truncated) = truncate_output(&raw);
            result["output"] = json!(output_text);
            if truncated {
                result["output_truncated"] = json!(true);
                result["output_full_bytes"] = json!(raw.len());
            }
        }

        Ok(serde_json::to_string_pretty(&result)?)
    }

    // ── pane_run_command ──────────────────────────────────────────────────────

    async fn tool_pane_run_command(&self, args: &Value) -> Result<String> {
        let pane_prefix = args["pane"]
            .as_str()
            .ok_or_else(|| anyhow!("missing pane"))?;
        let command = args["command"]
            .as_str()
            .ok_or_else(|| anyhow!("missing command"))?
            .to_owned();
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(30);
        let include_output = args["include_output"].as_bool().unwrap_or(true);

        let client = self.client().await?;
        let pane_id = self.resolve_pane_id(&client, pane_prefix).await?;

        // 1. Snapshot the current last block id so we can detect when a new one appears.
        let baseline_block_id = client
            .last_block_for_pane(tarpc::context::current(), pane_id)
            .await
            .context("rpc last_block_for_pane (baseline)")?
            .map_err(|e| anyhow!("{e}"))?
            .map(|b| b.id);

        // 2. Send the command with a trailing newline (Enter).
        let payload = format!("{command}\n").into_bytes();
        client
            .send_keys(tarpc::context::current(), pane_id, payload)
            .await
            .context("rpc send_keys")?
            .map_err(|e| anyhow!("{e}"))?;

        // 3. Poll until a new finalized block appears or timeout expires.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        let poll_interval = std::time::Duration::from_millis(150);

        let mut new_block: Option<pyre_proto::Block> = None;

        loop {
            tokio::time::sleep(poll_interval).await;

            let latest = client
                .last_block_for_pane(tarpc::context::current(), pane_id)
                .await
                .context("rpc last_block_for_pane (poll)")?
                .map_err(|e| anyhow!("{e}"))?;

            if let Some(block) = latest {
                // The block must be different from our baseline and must be finalized
                // (ended_at and exit_code are both set).
                let is_new = match baseline_block_id {
                    Some(bid) => block.id != bid,
                    None => true,
                };
                let is_finished = block.ended_at.is_some() && block.exit_code.is_some();

                if is_new && is_finished {
                    new_block = Some(block);
                    break;
                }
            }

            if std::time::Instant::now() >= deadline {
                break;
            }
        }

        // 4. Build the response.
        if let Some(block) = new_block {
            let mut result = json!({
                "completed": true,
                "block_id": block.id.0.to_string(),
                "command": command,
                "exit_code": block.exit_code,
                "duration_ms": block.duration_ms(),
                "cwd": block.cwd,
            });

            if include_output {
                let raw = client
                    .get_block_stdout(tarpc::context::current(), block.id)
                    .await
                    .context("rpc get_block_stdout")?
                    .map_err(|e| anyhow!("{e}"))?;

                let (output_text, truncated) = truncate_output(&raw);
                result["output"] = json!(output_text);
                if truncated {
                    result["output_truncated"] = json!(true);
                    result["output_full_bytes"] = json!(raw.len());
                }
            }

            Ok(serde_json::to_string_pretty(&result)?)
        } else {
            // Timeout path — gather whatever info we have.
            let partial_block = client
                .last_block_for_pane(tarpc::context::current(), pane_id)
                .await
                .context("rpc last_block_for_pane (timeout)")?
                .map_err(|e| anyhow!("{e}"))?;

            let (current_block_id, hint) = match &partial_block {
                Some(b) if partial_block.as_ref().map(|b| b.id) != baseline_block_id => {
                    // A new block started but didn't finish within the timeout.
                    (Some(b.id.0.to_string()), "Command started but did not finish within timeout_secs. Increase timeout_secs or the command is long-running.".to_owned())
                }
                _ => {
                    // No new block at all — shell integration probably missing.
                    (None, "No new block detected. OSC 133 shell integration may be missing. Run `pyrec shell-init` in the shell and re-source your shell profile to enable block tracking.".to_owned())
                }
            };

            Ok(serde_json::to_string_pretty(&json!({
                "completed": false,
                "command": command,
                "exit_code": null,
                "duration_ms": null,
                "block_id": current_block_id,
                "hint": hint,
            }))?)
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Output truncation helper
// ──────────────────────────────────────────────────────────────────────────────

/// Maximum bytes of stdout to include in a response.
const OUTPUT_MAX_BYTES: usize = 8 * 1024; // 8 KB

/// Truncate raw PTY output to `OUTPUT_MAX_BYTES` (last N bytes) and decode as
/// UTF-8 lossy. Returns `(text, was_truncated)`.
fn truncate_output(raw: &[u8]) -> (String, bool) {
    if raw.len() <= OUTPUT_MAX_BYTES {
        (String::from_utf8_lossy(raw).into_owned(), false)
    } else {
        let start = raw.len() - OUTPUT_MAX_BYTES;
        let truncated_bytes = &raw[start..];
        let text = format!(
            "[... {} bytes truncated, showing last {} bytes ...]\n{}",
            raw.len() - OUTPUT_MAX_BYTES,
            OUTPUT_MAX_BYTES,
            String::from_utf8_lossy(truncated_bytes)
        );
        (text, true)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Main: stdio loop
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let socket = std::env::var("PYRE_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_socket());

    // Stdout writer channel — single writer avoids interleaving.
    let (tx, mut rx) = mpsc::channel::<String>(256);

    // Spawn stdout writer task.
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = rx.recv().await {
            debug!(line = %line, "→ client");
            if let Err(e) = stdout.write_all(line.as_bytes()).await {
                warn!("stdout write error: {e}");
                break;
            }
            if let Err(e) = stdout.write_all(b"\n").await {
                warn!("stdout write error: {e}");
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    let server = Arc::new(Server::new(socket, tx));

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_owned();
        if line.is_empty() {
            continue;
        }
        debug!(line = %line, "← client");

        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response::err(None, -32700, format!("parse error: {e}"));
                let json_resp = serde_json::to_value(&resp)?;
                server.send_line(&json_resp).await?;
                continue;
            }
        };

        let req_id = req.id.clone();
        let server_clone = Arc::clone(&server);

        tokio::spawn(async move {
            if let Some(resp) = server_clone.handle(req).await {
                let val = match serde_json::to_value(&resp) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("serialize response: {e}");
                        return;
                    }
                };
                // Only send response if there was an id (not a notification).
                if req_id.is_some() {
                    if let Err(e) = server_clone.send_line(&val).await {
                        warn!("send response: {e}");
                    }
                }
            }
        });
    }

    Ok(())
}
