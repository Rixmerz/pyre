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
// Test 8: live daemon — pane_run_command with auto-injected bash integration
// ──────────────────────────────────────────────────────────────────────────────

/// Verify that panes spawned by pyred auto-inject bash integration so that
/// `pane_run_command` returns `completed: true` and the correct exit_code.
///
/// Flow:
///   1. Spawn pyred with isolated XDG dirs (same pattern as other live tests).
///   2. session_spawn with shell="/bin/bash" — triggers auto-injection.
///   3. Wait for bash to reach the prompt (OSC 133 A emitted by precmd).
///   4. pane_run_command "printf 'marker-ok\n'; true"
///      → expect completed=true, exit_code=0, output contains "marker-ok".
///   5. pane_run_command "false"
///      → expect completed=true, exit_code=1.
///
/// The test is NOT marked #[ignore]: the existing live tests (e.g.
/// test_pane_last_block_fresh_pane, test_live_session_spawn) run the same
/// self-spawning harness without ignore.  We match that convention.
///
/// If /bin/bash is unavailable in the test environment the test skips
/// gracefully rather than failing.
#[test]
fn test_pane_run_command_auto_integration() {
    use std::time::Duration;

    // Skip gracefully if bash is not available.
    if !std::path::Path::new("/bin/bash").exists() {
        eprintln!("skip: /bin/bash not found");
        return;
    }

    let rt_dir = tempfile::tempdir().expect("tempdir");
    let data_dir = rt_dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("mkdir data");
    let sock_path = rt_dir.path().join("pyre.sock");

    let pyred_bin = std::env::var("CARGO_BIN_EXE_pyred").unwrap_or_else(|_| {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let mut p = std::path::PathBuf::from(manifest);
        p.pop(); // crates/pyre-mcp -> crates
        p.pop(); // crates -> workspace root
        p.push("target/debug/pyred");
        p.to_string_lossy().into_owned()
    });

    // Use a clean HOME so the spawned bash does not source the user's real
    // ~/.bashrc (which may override PROMPT_COMMAND, set incompatible hooks,
    // or simply take a long time).  The auto-injected rcfile sources
    // ~/.bashrc first — pointing HOME at a clean tmpdir means it silently
    // skips the non-existent bashrc and proceeds directly to the integration
    // hooks.
    let home_dir = rt_dir.path().join("home");
    std::fs::create_dir_all(&home_dir).expect("mkdir home");

    // XDG_CONFIG_HOME must point to a clean directory so pyred does not read
    // the user's real config.toml (which may set process_model = "hybrid").
    // Hybrid mode does NOT run a BlockParser, so OSC 133 markers are never
    // consumed and blocks are never created.  Single-process mode (the
    // default when no config exists) runs the full BlockParser pipeline in
    // pty.rs and is what we need here.
    let config_dir = rt_dir.path().join("config");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");

    // PATH and TERM are needed so bash spawned by pyred has a sane env.
    // Without PATH bash still works (builtins are fine), but some distros'
    // bash configs reference external commands during startup that would fail.
    // TERM=dumb avoids ncurses/tput confusion with no terminfo available.
    let path_val = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    let mut daemon = Command::new(&pyred_bin)
        .env_clear()
        .env("HOME", &home_dir)
        .env("PATH", &path_val)
        .env("TERM", "dumb")
        .env("XDG_RUNTIME_DIR", rt_dir.path())
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("PYRE_DATA_DIR", &data_dir)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pyred");

    // Wait for socket.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !sock_path.exists() {
        if std::time::Instant::now() >= deadline {
            daemon.kill().ok();
            panic!("pyred socket never appeared at {}", sock_path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let bin = pyre_mcp_bin();
    let mut mcp = Command::new(&bin)
        .env("PYRE_SOCK", &sock_path)
        // Set HOME to the clean dir so that session_spawn's std::env::vars()
        // forwards a clean HOME to the bash pane.  Without this, session_spawn
        // passes the test-runner's real HOME to pyred, which causes bash to
        // source the user's real ~/.bashrc (potentially containing oh-my-posh
        // or other tools that rewrite PROMPT_COMMAND and break OSC 133 hooks).
        .env("HOME", &home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pyre-mcp");

    // Initialize.
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
    rpc_roundtrip(&mut mcp, &init);

    // Spawn a session with bash — triggers auto-injection of OSC 133 hooks.
    let spawn_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "session_spawn",
            "arguments": { "shell": "/bin/bash", "cols": 80, "rows": 24 }
        }
    });
    let spawn_resp = rpc_roundtrip(&mut mcp, &spawn_req);
    let spawn_text = spawn_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("spawn text");

    let pane_id = spawn_text
        .split_whitespace()
        .find(|s| s.starts_with("pane_id="))
        .and_then(|s| s.strip_prefix("pane_id="))
        .expect("pane_id in response")
        .to_owned();
    let pane_prefix = &pane_id[..8];

    // Give bash time to source the rcfile, run precmd, and emit OSC 133 A
    // (PromptStart).  The first precmd fires after the rcfile is sourced and
    // the prompt is drawn — typically within 500 ms on CI.
    std::thread::sleep(Duration::from_millis(800));

    // ── Run 1: printf + true → completed=true, exit_code=0, output has marker ──

    let cmd1 = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "pane_run_command",
            "arguments": {
                "pane": pane_prefix,
                "command": "printf 'marker-ok\\n'; true",
                "timeout_secs": 15,
                "include_output": true
            }
        }
    });
    let resp1 = rpc_roundtrip(&mut mcp, &cmd1);

    // ── Run 2: false → completed=true, exit_code=1 ──────────────────────────

    // Wait for the pane to return to the prompt before issuing the second command.
    std::thread::sleep(Duration::from_millis(300));

    let cmd2 = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "pane_run_command",
            "arguments": {
                "pane": pane_prefix,
                "command": "false",
                "timeout_secs": 15,
                "include_output": true
            }
        }
    });
    let resp2 = rpc_roundtrip(&mut mcp, &cmd2);

    // Cleanup before asserting so the daemon doesn't linger on failure.
    mcp.kill().ok();
    let _ = mcp.wait();
    daemon.kill().ok();
    let _ = daemon.wait();

    // ── Assertions for run 1 ─────────────────────────────────────────────────

    assert!(
        resp1["error"].is_null(),
        "pane_run_command (run 1) must not return an error: {resp1}"
    );
    let text1 = resp1["result"]["content"][0]["text"]
        .as_str()
        .expect("run 1 text content");
    let parsed1: serde_json::Value =
        serde_json::from_str(text1).expect("run 1: parse tool output as JSON");

    assert_eq!(
        parsed1["completed"], true,
        "run 1: completed must be true (OSC 133 integration active); full response: {parsed1}"
    );
    assert_eq!(
        parsed1["exit_code"], 0,
        "run 1: exit_code must be 0; full response: {parsed1}"
    );
    let output1 = parsed1["output"].as_str().unwrap_or("");
    assert!(
        output1.contains("marker-ok"),
        "run 1: output must contain 'marker-ok'; got: {output1:?}"
    );

    // ── Assertions for run 2 ─────────────────────────────────────────────────

    assert!(
        resp2["error"].is_null(),
        "pane_run_command (run 2) must not return an error: {resp2}"
    );
    let text2 = resp2["result"]["content"][0]["text"]
        .as_str()
        .expect("run 2 text content");
    let parsed2: serde_json::Value =
        serde_json::from_str(text2).expect("run 2: parse tool output as JSON");

    assert_eq!(
        parsed2["completed"], true,
        "run 2: completed must be true (OSC 133 integration active); full response: {parsed2}"
    );
    assert_eq!(
        parsed2["exit_code"], 1,
        "run 2: exit_code must be 1 for `false`; full response: {parsed2}"
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
