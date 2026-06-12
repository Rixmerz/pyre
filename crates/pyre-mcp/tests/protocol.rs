//! Integration tests for pyre-mcp JSON-RPC protocol.
//!
//! Test 1: initialize handshake — mandatory, always runs.
//! Test 3 (`test_live_session_spawn`) spawns `pyred` via `CARGO_BIN_EXE_pyred`.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Write a JSON-RPC request line to a child's stdin and read one response line.
fn rpc_roundtrip(child: &mut std::process::Child, req: &Value) -> Value {
    let stdin = child.stdin.as_mut().expect("stdin piped");
    let mut line = serde_json::to_string(req).expect("serialize");
    line.push('\n');
    stdin.write_all(line.as_bytes()).expect("write stdin");
    stdin.flush().expect("flush stdin");

    // Read one response line from stdout.
    use std::io::BufRead;
    let stdout = child.stdout.as_mut().expect("stdout piped");
    let mut reader = std::io::BufReader::new(stdout);
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).expect("read stdout");
    serde_json::from_str(resp_line.trim()).expect("parse JSON response")
}

/// Locate the pyre-mcp binary built by cargo.
fn pyre_mcp_bin() -> std::path::PathBuf {
    // When run via `cargo test`, CARGO_BIN_EXE_pyre-mcp is set.
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_pyre-mcp") {
        return p.into();
    }
    // Fallback: workspace target/debug.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let mut p = std::path::PathBuf::from(manifest);
    p.pop(); // crates/pyre-mcp -> crates
    p.pop(); // crates -> workspace root
    p.push("target/debug/pyre-mcp");
    p
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 1: initialize handshake (no daemon required)
// ──────────────────────────────────────────────────────────────────────────────

/// Spawn pyre-mcp against a non-existent socket path, send an initialize
/// request, and assert the capabilities shape. The daemon connection is only
/// attempted on tool/resource calls, not on initialize — so this test does
/// NOT require a live pyred.
#[test]
fn test_initialize_handshake() {
    let bin = pyre_mcp_bin();
    assert!(
        bin.exists(),
        "pyre-mcp binary not found at {}: run `cargo build` first",
        bin.display()
    );

    let mut child = Command::new(&bin)
        .env("PYRE_SOCK", "/tmp/pyre-mcp-test-nonexistent.sock")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pyre-mcp");

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    });

    let resp = rpc_roundtrip(&mut child, &req);

    child.kill().ok();

    assert_eq!(resp["jsonrpc"], "2.0", "jsonrpc field");
    assert_eq!(resp["id"], 1, "id echo");
    assert!(resp["result"].is_object(), "result is object");

    let result = &resp["result"];
    assert_eq!(result["protocolVersion"], "2024-11-05", "protocolVersion");

    let caps = &result["capabilities"];
    assert_eq!(
        caps["resources"]["subscribe"], true,
        "resources.subscribe == true"
    );
    assert_eq!(
        caps["tools"]["listChanged"], false,
        "tools.listChanged == false"
    );

    let info = &result["serverInfo"];
    assert_eq!(info["name"], "pyre-mcp", "serverInfo.name");
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 2: tools/list (no daemon required)
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn test_tools_list() {
    let bin = pyre_mcp_bin();
    if !bin.exists() {
        eprintln!("pyre-mcp binary not found, skipping");
        return;
    }

    let mut child = Command::new(&bin)
        .env("PYRE_SOCK", "/tmp/pyre-mcp-test-nonexistent.sock")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pyre-mcp");

    // Must initialize first.
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
    rpc_roundtrip(&mut child, &init);

    let req = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} });
    let resp = rpc_roundtrip(&mut child, &req);

    child.kill().ok();

    let tools = resp["result"]["tools"].as_array().expect("tools is array");

    // We now have 17 tools: 15 original + pane_last_block + pane_run_command.
    assert_eq!(
        tools.len(),
        17,
        "expected exactly 17 tools, got {}: {:?}",
        tools.len(),
        tools
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect::<Vec<_>>()
    );

    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    for expected in &[
        "pane_send_keys",
        "pane_capture",
        "pane_set_state",
        "block_search",
        "session_spawn",
        "session_close",
        "pane_open",
        "wait_pane_state",
        "list_sessions",
        "list_panes",
        "gc_stale_sessions",
        "session_layout",
        "set_pane_weight",
        "get_session_layout",
        "open_pane_split",
        "pane_last_block",
        "pane_run_command",
    ] {
        assert!(
            names.contains(expected),
            "tool '{expected}' not in list: {names:?}"
        );
    }

    // Verify set_pane_weight schema shape.
    let spw = tools
        .iter()
        .find(|t| t["name"] == "set_pane_weight")
        .expect("set_pane_weight tool");
    let spw_props = &spw["inputSchema"]["properties"];
    assert!(
        spw_props["pane_id"].is_object(),
        "set_pane_weight must have pane_id property"
    );
    assert!(
        spw_props["weight"].is_object(),
        "set_pane_weight must have weight property"
    );
    assert_eq!(
        spw_props["weight"]["minimum"], 0,
        "weight minimum must be 0"
    );
    assert_eq!(
        spw_props["weight"]["maximum"], 100,
        "weight maximum must be 100"
    );

    // Verify get_session_layout schema shape.
    let gsl = tools
        .iter()
        .find(|t| t["name"] == "get_session_layout")
        .expect("get_session_layout tool");
    let gsl_props = &gsl["inputSchema"]["properties"];
    assert!(
        gsl_props["session_id"].is_object(),
        "get_session_layout must have session_id property"
    );
    assert_eq!(
        gsl["inputSchema"]["required"],
        json!(["session_id"]),
        "get_session_layout requires session_id"
    );

    // Verify session_layout has a layout property with orient + panes.
    let sl = tools
        .iter()
        .find(|t| t["name"] == "session_layout")
        .expect("session_layout tool");
    let sl_props = &sl["inputSchema"]["properties"];
    assert!(
        sl_props["layout"].is_object(),
        "session_layout must have layout property"
    );
    let layout_props = &sl_props["layout"]["properties"];
    assert!(
        layout_props["orient"].is_object(),
        "session_layout.layout must have orient"
    );
    assert!(
        layout_props["panes"].is_object(),
        "session_layout.layout must have panes"
    );

    // Verify pane_last_block schema.
    let plb = tools
        .iter()
        .find(|t| t["name"] == "pane_last_block")
        .expect("pane_last_block tool");
    let plb_props = &plb["inputSchema"]["properties"];
    assert!(
        plb_props["pane"].is_object(),
        "pane_last_block must have pane property"
    );
    assert!(
        plb_props["include_output"].is_object(),
        "pane_last_block must have include_output property"
    );
    assert_eq!(
        plb["inputSchema"]["required"],
        json!(["pane"]),
        "pane_last_block requires pane"
    );

    // Verify pane_run_command schema.
    let prc = tools
        .iter()
        .find(|t| t["name"] == "pane_run_command")
        .expect("pane_run_command tool");
    let prc_props = &prc["inputSchema"]["properties"];
    assert!(
        prc_props["pane"].is_object(),
        "pane_run_command must have pane property"
    );
    assert!(
        prc_props["command"].is_object(),
        "pane_run_command must have command property"
    );
    assert!(
        prc_props["timeout_secs"].is_object(),
        "pane_run_command must have timeout_secs property"
    );
    assert!(
        prc_props["include_output"].is_object(),
        "pane_run_command must have include_output property"
    );

    // Verify block_search now has session/pane/exit_code params.
    let bs = tools
        .iter()
        .find(|t| t["name"] == "block_search")
        .expect("block_search tool");
    let bs_props = &bs["inputSchema"]["properties"];
    assert!(
        bs_props["session"].is_object(),
        "block_search must have session filter property"
    );
    assert!(
        bs_props["pane"].is_object(),
        "block_search must have pane filter property"
    );
    assert!(
        bs_props["exit_code"].is_object(),
        "block_search must have exit_code filter property"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 3: live daemon interaction (requires pyred)
// ──────────────────────────────────────────────────────────────────────────────

/// Spawns pyred + pyre-mcp, calls session_spawn, pane_send_keys, then
/// resources/read on the output, and asserts "hi" appears.
///
/// Spawns `pyred` via `CARGO_BIN_EXE_pyred` (set by `cargo test`) with an
/// isolated runtime dir, then exercises session_spawn → pane_send_keys → capture.
#[test]
fn test_live_session_spawn() {
    use std::time::Duration;

    let rt_dir = tempfile::tempdir().expect("tempdir");
    let data_dir = rt_dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("mkdir data");
    let sock_path = rt_dir.path().join("pyre.sock");

    let pyred_bin = std::env::var("CARGO_BIN_EXE_pyred").unwrap_or_else(|_| {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let mut p = std::path::PathBuf::from(manifest);
        p.pop();
        p.pop();
        p.push("target/debug/pyred");
        p.to_string_lossy().into_owned()
    });

    let mut daemon = Command::new(&pyred_bin)
        .env_clear()
        .env(
            "HOME",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
        )
        .env("XDG_RUNTIME_DIR", rt_dir.path())
        .env("PYRE_DATA_DIR", &data_dir)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pyred");

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !sock_path.exists() {
        if std::time::Instant::now() >= deadline {
            panic!("pyred socket never appeared at {}", sock_path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let bin = pyre_mcp_bin();
    let mut child = Command::new(&bin)
        .env("PYRE_SOCK", &sock_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pyre-mcp");

    // Initialize.
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
    rpc_roundtrip(&mut child, &init);

    // Spawn a session.
    let spawn_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "session_spawn",
            "arguments": { "shell": "/bin/sh", "cols": 80, "rows": 24 }
        }
    });
    let spawn_resp = rpc_roundtrip(&mut child, &spawn_req);
    let spawn_text = spawn_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("spawn text");

    // Parse pane_id from "session_id=... pane_id=..."
    let pane_id = spawn_text
        .split_whitespace()
        .find(|s| s.starts_with("pane_id="))
        .and_then(|s| s.strip_prefix("pane_id="))
        .expect("pane_id in response")
        .to_owned();

    let pane_prefix = &pane_id[..8];

    // Give the shell time to start.
    std::thread::sleep(Duration::from_millis(300));

    // Send "echo hi".
    let send_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "pane_send_keys",
            "arguments": { "pane": pane_prefix, "text": "echo hi", "append_enter": true }
        }
    });
    rpc_roundtrip(&mut child, &send_req);

    // Give the shell time to execute.
    std::thread::sleep(Duration::from_millis(300));

    // Read pane output — retry up to 5 times.
    let mut found = false;
    for _ in 0..5 {
        // Need to resolve session prefix for the URI — just use pane_capture tool.
        let cap_req = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "pane_capture",
                "arguments": { "pane": pane_prefix, "lines": 50 }
            }
        });
        let cap_resp = rpc_roundtrip(&mut child, &cap_req);
        let output = cap_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("");
        if output.contains("hi") {
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    child.kill().ok();
    let _ = child.wait();
    daemon.kill().ok();
    let _ = daemon.wait();

    assert!(found, "expected 'hi' in pane output");
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 4: structured error for bogus pane id (no daemon required via connection error)
// ──────────────────────────────────────────────────────────────────────────────

/// Call pane_last_block with a bogus pane prefix against a non-existent socket.
/// The daemon connection failure must return a structured error with
/// error.data.code == "daemon_unreachable".
#[test]
fn test_structured_error_daemon_unreachable() {
    let bin = pyre_mcp_bin();
    if !bin.exists() {
        eprintln!("pyre-mcp binary not found, skipping");
        return;
    }

    let mut child = Command::new(&bin)
        .env("PYRE_SOCK", "/tmp/pyre-mcp-test-nonexistent-9999.sock")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pyre-mcp");

    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
    rpc_roundtrip(&mut child, &init);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "pane_last_block",
            "arguments": { "pane": "deadbeef" }
        }
    });
    let resp = rpc_roundtrip(&mut child, &req);

    child.kill().ok();

    // Must be an error response.
    assert!(
        resp["error"].is_object(),
        "expected error response, got: {resp}"
    );

    let error = &resp["error"];
    let data = &error["data"];
    assert!(
        data.is_object(),
        "error.data must be an object, got: {error}"
    );
    assert_eq!(
        data["code"], "daemon_unreachable",
        "error.data.code must be daemon_unreachable, got: {data}"
    );
    assert!(
        data["hint"].as_str().is_some(),
        "error.data.hint must be a string"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 5: live daemon — pane_last_block on fresh pane (no blocks yet)
// ──────────────────────────────────────────────────────────────────────────────

/// Spawn a fresh session/pane; immediately call pane_last_block.
/// Since no command has run yet, block must be null (not an error).
#[test]
fn test_pane_last_block_fresh_pane() {
    use std::time::Duration;

    let rt_dir = tempfile::tempdir().expect("tempdir");
    let data_dir = rt_dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("mkdir data");
    let sock_path = rt_dir.path().join("pyre.sock");

    let pyred_bin = std::env::var("CARGO_BIN_EXE_pyred").unwrap_or_else(|_| {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let mut p = std::path::PathBuf::from(manifest);
        p.pop();
        p.pop();
        p.push("target/debug/pyred");
        p.to_string_lossy().into_owned()
    });

    let mut daemon = Command::new(&pyred_bin)
        .env_clear()
        .env(
            "HOME",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
        )
        .env("XDG_RUNTIME_DIR", rt_dir.path())
        .env("PYRE_DATA_DIR", &data_dir)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pyred");

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !sock_path.exists() {
        if std::time::Instant::now() >= deadline {
            panic!("pyred socket never appeared at {}", sock_path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let bin = pyre_mcp_bin();
    let mut child = Command::new(&bin)
        .env("PYRE_SOCK", &sock_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pyre-mcp");

    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
    rpc_roundtrip(&mut child, &init);

    // Spawn a session.
    let spawn_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "session_spawn",
            "arguments": { "shell": "/bin/sh", "cols": 80, "rows": 24 }
        }
    });
    let spawn_resp = rpc_roundtrip(&mut child, &spawn_req);
    let spawn_text = spawn_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("spawn text");

    let pane_id = spawn_text
        .split_whitespace()
        .find(|s| s.starts_with("pane_id="))
        .and_then(|s| s.strip_prefix("pane_id="))
        .expect("pane_id")
        .to_owned();

    // Immediately query pane_last_block before any command runs.
    let plb_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "pane_last_block",
            "arguments": { "pane": &pane_id[..8] }
        }
    });
    let plb_resp = rpc_roundtrip(&mut child, &plb_req);

    child.kill().ok();
    let _ = child.wait();
    daemon.kill().ok();
    let _ = daemon.wait();

    // Must be a successful response (not an error).
    assert!(
        plb_resp["error"].is_null(),
        "pane_last_block on fresh pane must not error, got: {plb_resp}"
    );
    assert!(
        plb_resp["result"].is_object(),
        "expected result object, got: {plb_resp}"
    );

    // Parse the returned text as JSON and verify block is null.
    let text = plb_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    let parsed: serde_json::Value = serde_json::from_str(text).expect("parse tool output as JSON");

    assert!(
        parsed["block"].is_null(),
        "block must be null for fresh pane with no commands, got: {parsed}"
    );
    // hint should be present to guide the agent.
    assert!(
        parsed["hint"].as_str().is_some(),
        "hint must be present when block is null"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 6: live daemon — structured no_such_pane error
// ──────────────────────────────────────────────────────────────────────────────

/// With a live daemon, call pane_last_block with a bogus prefix that matches no
/// pane. Expect error.data.code == "no_such_pane".
#[test]
fn test_structured_error_no_such_pane() {
    use std::time::Duration;

    let rt_dir = tempfile::tempdir().expect("tempdir");
    let data_dir = rt_dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("mkdir data");
    let sock_path = rt_dir.path().join("pyre.sock");

    let pyred_bin = std::env::var("CARGO_BIN_EXE_pyred").unwrap_or_else(|_| {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let mut p = std::path::PathBuf::from(manifest);
        p.pop();
        p.pop();
        p.push("target/debug/pyred");
        p.to_string_lossy().into_owned()
    });

    let mut daemon = Command::new(&pyred_bin)
        .env_clear()
        .env(
            "HOME",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
        )
        .env("XDG_RUNTIME_DIR", rt_dir.path())
        .env("PYRE_DATA_DIR", &data_dir)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pyred");

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !sock_path.exists() {
        if std::time::Instant::now() >= deadline {
            panic!("pyred socket never appeared at {}", sock_path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let bin = pyre_mcp_bin();
    let mut child = Command::new(&bin)
        .env("PYRE_SOCK", &sock_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pyre-mcp");

    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
    rpc_roundtrip(&mut child, &init);

    // Use a prefix that definitely won't match any real pane.
    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "pane_last_block",
            "arguments": { "pane": "00000000" }
        }
    });
    let resp = rpc_roundtrip(&mut child, &req);

    child.kill().ok();
    let _ = child.wait();
    daemon.kill().ok();
    let _ = daemon.wait();

    assert!(
        resp["error"].is_object(),
        "expected error response, got: {resp}"
    );

    let data = &resp["error"]["data"];
    assert!(
        data.is_object(),
        "error.data must be present, got: {}",
        resp["error"]
    );
    assert_eq!(
        data["code"], "no_such_pane",
        "error.data.code must be no_such_pane, got: {data}"
    );
    assert!(
        data["hint"].as_str().unwrap_or("").contains("list_panes"),
        "hint should mention list_panes: {data}"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 7: live daemon — block_search with exit_code filter
// ──────────────────────────────────────────────────────────────────────────────

/// Verify block_search accepts the exit_code param without error (schema test).
/// With no prior commands the result should be "no results".
#[test]
fn test_block_search_exit_code_filter() {
    use std::time::Duration;

    let rt_dir = tempfile::tempdir().expect("tempdir");
    let data_dir = rt_dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("mkdir data");
    let sock_path = rt_dir.path().join("pyre.sock");

    let pyred_bin = std::env::var("CARGO_BIN_EXE_pyred").unwrap_or_else(|_| {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let mut p = std::path::PathBuf::from(manifest);
        p.pop();
        p.pop();
        p.push("target/debug/pyred");
        p.to_string_lossy().into_owned()
    });

    let mut daemon = Command::new(&pyred_bin)
        .env_clear()
        .env(
            "HOME",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
        )
        .env("XDG_RUNTIME_DIR", rt_dir.path())
        .env("PYRE_DATA_DIR", &data_dir)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pyred");

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !sock_path.exists() {
        if std::time::Instant::now() >= deadline {
            panic!("pyred socket never appeared at {}", sock_path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let bin = pyre_mcp_bin();
    let mut child = Command::new(&bin)
        .env("PYRE_SOCK", &sock_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pyre-mcp");

    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
    rpc_roundtrip(&mut child, &init);

    // block_search with exit_code=0 — should return no results (no blocks yet).
    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "block_search",
            "arguments": {
                "query": "anything",
                "exit_code": 0
            }
        }
    });
    let resp = rpc_roundtrip(&mut child, &req);

    child.kill().ok();
    let _ = child.wait();
    daemon.kill().ok();
    let _ = daemon.wait();

    // Must succeed (not error).
    assert!(
        resp["error"].is_null(),
        "block_search with exit_code filter must not error, got: {resp}"
    );
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert_eq!(text, "no results", "expected no results on fresh daemon");
}
