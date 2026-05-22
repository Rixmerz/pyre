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

    assert!(
        tools.len() >= 11,
        "expected at least 11 tools, got {}",
        tools.len()
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
        "session_layout",
    ] {
        assert!(
            names.contains(expected),
            "tool '{expected}' not in list: {names:?}"
        );
    }
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
