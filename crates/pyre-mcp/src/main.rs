//! pyre-mcp — Newline-delimited JSON-RPC 2.0 stdio server bridging Claude Code /
//! jig / any MCP client to a running pyred daemon over UDS.
//!
//! Transport: newline-delimited JSON over stdio (each request = one line; each
//! response = one line). "Content-Length" framing (LSP variant) is NOT used —
//! Claude Code and jig both speak newline-delimited.
//!
//! Streaming subscribe: polled at 1 s intervals against `list_all_panes`.
//! Adequate for v0.1; can be upgraded to true tarpc event pushing later.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use pyre_proto::{
    ListBlocksReq, PaneStateKind, PyreDaemonClient, SearchBlocksReq, SessionId, SpawnReq,
    MODE_CONTROL,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::codec::LengthDelimitedCodec;
use tracing::{debug, warn};
use tracing_subscriber::EnvFilter;

// ──────────────────────────────────────────────────────────────────────────────
// JSON-RPC 2.0 types
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Request {
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
}

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

// ──────────────────────────────────────────────────────────────────────────────
// Socket helpers (mirrors pyrec pattern)
// ──────────────────────────────────────────────────────────────────────────────

fn default_socket() -> PathBuf {
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("pyre.sock");
    }
    // SAFETY: getuid() is always safe to call.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/pyre-{uid}.sock"))
}

async fn control_client(socket: &Path) -> Result<PyreDaemonClient> {
    let mut sock = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect {}", socket.display()))?;
    tokio::io::AsyncWriteExt::write_all(&mut sock, &[MODE_CONTROL]).await?;

    let transport = tarpc::serde_transport::new(
        tokio_util::codec::Framed::new(sock, LengthDelimitedCodec::new()),
        Bincode::default(),
    );
    Ok(PyreDaemonClient::new(client::Config::default(), transport).spawn())
}

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
        control_client(&self.socket).await
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
            Err(e) => Some(Response::err(id, -32000, e.to_string())),
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
            "serverInfo": { "name": "pyre-mcp", "version": "0.1.0" }
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
            let mut last_snapshot: Option<String> = None;

            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

                let Ok(client) = control_client(&socket).await else {
                    continue;
                };
                let Ok(Ok(panes)) = client.list_all_panes(tarpc::context::current()).await else {
                    continue;
                };

                let Ok(snapshot) = serde_json::to_string(&panes) else {
                    continue;
                };

                let changed = last_snapshot
                    .as_ref()
                    .map(|prev| prev != &snapshot)
                    .unwrap_or(true);

                if changed {
                    last_snapshot = Some(snapshot);
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
                    "description": "Capture the last N lines of a pane's ring buffer with CSI sequences stripped.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "pane": { "type": "string", "description": "Pane id or ≥8-char prefix" },
                            "lines": { "type": "integer", "description": "Number of lines to capture (default 50)", "default": 50 }
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
                    "description": "Full-text search across all blocks (stdout history).",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" },
                            "limit": { "type": "integer", "default": 20 }
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
            0 => Err(anyhow!("no pane matches prefix '{prefix}'")),
            1 => Ok(matches[0].id),
            _ => Err(anyhow!(
                "{} panes match prefix '{prefix}'; provide a longer prefix",
                matches.len()
            )),
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
            0 => Err(anyhow!("no session matches prefix '{prefix}'")),
            1 => Ok(matches[0].id),
            _ => Err(anyhow!(
                "{} sessions match prefix '{prefix}'; provide a longer prefix",
                matches.len()
            )),
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
            .send_keys(
                tarpc::context::current(),
                pane_id,
                payload.into_bytes(),
            )
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

        let client = self.client().await?;
        let pane_id = self.resolve_pane_id(&client, pane_prefix).await?;
        let bytes = client
            .capture_pane(tarpc::context::current(), pane_id, lines)
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;

        Ok(String::from_utf8_lossy(&bytes).into_owned())
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

        let client = self.client().await?;
        let hits = client
            .search_blocks(tarpc::context::current(), SearchBlocksReq { query, limit })
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
