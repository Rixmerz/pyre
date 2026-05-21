% PYRE-MCP(1) pyre 0.1.0 | pyre Manual
% pyre contributors
% May 2026

# NAME

pyre-mcp - pyre MCP server: expose terminal sessions to AI agents

# SYNOPSIS

**pyre-mcp** [--socket *PATH*] [--mcp-socket *PATH*]

# DESCRIPTION

**pyre-mcp** is the Model Context Protocol server for the **pyre** terminal
multiplexer. It connects to the **pyred** daemon over the pyre UDS and exposes
sessions, panes, and blocks as MCP resources. It also provides MCP tools that
let AI agents spawn sessions, send keystrokes, and read pane output.

pyre-mcp is designed to be registered with any MCP client by pointing the
client at the pyre-mcp socket or running it as a stdio subprocess. See
**docs/AGENTS.md** for the agent multiplexer playbook.

# OPTIONS

**--socket** *PATH*
: pyre daemon socket path. Default: **$XDG_RUNTIME_DIR/pyre.sock**.

**--mcp-socket** *PATH*
: Unix domain socket for MCP clients to connect to. If omitted, pyre-mcp
  runs in stdio mode (suitable for subprocess invocation by MCP clients).

**-h**, **--help**
: Print help and exit.

**-V**, **--version**
: Print version and exit.

# MCP RESOURCES

**pyre://sessions**
: List of all active sessions with id, name, pane count, and creation time.

**pyre://sessions/{session_id}**
: Detail for one session: id, name, list of pane ids.

**pyre://sessions/{session_id}/panes/{pane_id}**
: Detail for one pane: id, state (idle/running/waiting/error), last command,
  cwd, dimensions.

**pyre://sessions/{session_id}/blocks**
: Recent blocks for a session: command, exit code, started_at, ended_at.

**pyre://blocks/{block_id}/stdout**
: Raw stdout blob for one block (may be large; clients should page).

# MCP TOOLS

**spawn_session**
: Spawn a new session and pane. Parameters: shell (optional), cwd (optional),
  cols, rows. Returns session_id and pane_id.

**send_keys**
: Send raw bytes to a pane. Parameters: session_id, pane_id, data (string).

**capture_pane**
: Capture the last N lines from a pane ring buffer. Parameters: session_id,
  pane_id, lines (default 40). Returns text.

**search_blocks**
: Full-text search across block stdout. Parameters: query, limit. Returns
  ranked hits with block metadata and stdout snippet.

**kill_session**
: Terminate a session and all its panes. Parameters: session_id.

**wait_for_idle**
: Poll a pane until its state becomes idle or a timeout elapses. Parameters:
  session_id, pane_id, timeout_secs (default 30). Useful for waiting after
  send_keys before reading output.

# EXAMPLES

Run as a stdio subprocess (register in .mcp.json):

    {
      "mcpServers": {
        "pyre": {
          "command": "pyre-mcp",
          "args": []
        }
      }
    }

Run as a UDS server:

    pyre-mcp --mcp-socket $XDG_RUNTIME_DIR/pyre-mcp.sock

# SEE ALSO

**pyred**(1), **pyrec**(1), **pyre-tui**(1)
