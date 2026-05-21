# pyre — Configuration

## hooks.toml

`pyred` loads `hooks.toml` from `$PYRE_DATA_DIR/hooks.toml` at startup
(default `$XDG_DATA_HOME/pyre/hooks.toml` or `~/.local/share/pyre/hooks.toml`).

The file is optional. If it does not exist, no hooks run.

### Schema

```toml
# hooks.toml

# Runs after every block ends (OSC 133 D received).
# The hook receives block metadata as environment variables:
#   PYRE_BLOCK_ID       — UUID of the block
#   PYRE_SESSION_ID     — UUID of the session
#   PYRE_PANE_ID        — UUID of the pane
#   PYRE_COMMAND        — command string (may be empty for OSC-less shells)
#   PYRE_EXIT_CODE      — integer exit code, or empty string if unknown
#   PYRE_CWD            — working directory at command start
#   PYRE_STARTED_AT     — RFC 3339 timestamp
#   PYRE_ENDED_AT       — RFC 3339 timestamp
#   PYRE_STDOUT_LEN     — byte length of captured stdout
#
# stdout and stderr of the hook script are discarded.
# The hook is run with a 5-second timeout; slow hooks are killed.

[[on_block_end]]
command = "/usr/local/bin/my-notifier"
args    = ["--session", "$PYRE_SESSION_ID"]

[[on_block_end]]
command = "bash"
args    = ["-c", "echo $PYRE_COMMAND >> /tmp/pyre-history.log"]

# Runs when a pane's state changes (idle → running, running → error, etc.).
# Additional env vars:
#   PYRE_PANE_STATE     — one of: idle, running, waiting, error
[[on_pane_state]]
command = "/usr/local/bin/status-bar-update"
args    = []

# Runs once when pyred starts (after socket is ready).
[[on_daemon_start]]
command = "notify-send"
args    = ["pyre daemon started"]

# Runs once just before pyred shuts down.
[[on_daemon_stop]]
command = "notify-send"
args    = ["pyre daemon stopped"]
```

### Full example: desktop notification on command failure

```toml
[[on_block_end]]
command = "bash"
args = [
  "-c",
  """
  if [ -n "$PYRE_EXIT_CODE" ] && [ "$PYRE_EXIT_CODE" != "0" ]; then
    notify-send -u critical "pyre: command failed" "$PYRE_COMMAND (exit $PYRE_EXIT_CODE)"
  fi
  """
]
```

---

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `XDG_RUNTIME_DIR` | `/run/user/$UID` | Socket directory. Socket is at `$XDG_RUNTIME_DIR/pyre.sock`. |
| `PYRE_DATA_DIR` | `$XDG_DATA_HOME/pyre` | State database (`state.db`), Tantivy index (`index/`), stdout blobs, and `hooks.toml`. |
| `PYRE_LOG` | `warn` | `tracing-subscriber` filter string. Set to `pyred=debug` for verbose daemon logs. |
| `PYRE_SOCKET` | (derived from `XDG_RUNTIME_DIR`) | Override the socket path for both daemon and client. |

---

## Multi-agent profile (hybrid)

For **one heavy agent per session** (recommended for S5), run the supervisor in
hybrid mode so each session gets an isolated worker process:

```toml
[pyred]
process_model = "hybrid"
```

| Pattern | Setting |
|---------|---------|
| Agent A (project `api`) | `pyrec session-new --name api --cwd ~/api` |
| Agent B (project `web`) | `pyrec session-new --name web --cwd ~/web` |
| Orchestration without TUI | `pyrec wait-pane`, `pyrec pane read`, MCP `wait_pane_state` |

Crash isolation: killing one worker does not take down other sessions. Block
search and replay are centralized in the supervisor store. Stream mirror
(two clients on one pane) uses the same proxy path as single-mode; see
`crates/pyred/tests/multi.rs` and `PaneMirrorHub` in the supervisor.

See [docs/AGENTS.md](AGENTS.md) and
[docs/adr/0002-daemon-process-architecture.md](adr/0002-daemon-process-architecture.md).

---

## Future configuration knobs (planned)

The following knobs are not yet wired to `config.toml`. Ring buffer capacity is
fixed in code today (`RingBuf::new` in `crates/pyred/src/pty.rs`). They will land
in `hooks.toml` or `pyre.toml` in a later sprint.

- `[ringbuf]` — `lines_per_pane = 10000` scrollback depth.
- `[search]` — `index_path`, `writer_heap_mb = 64`.
- `[tui]` — theme name (`ember` | `dracula` | `custom`), font family, font size.
- `[mcp]` — bind address for the MCP UDS, list of allowed tool names.
- `[lua]` — path to `init.lua`, sandbox memory limit.
- `[clipboard]` — prefer `wl-copy` vs `xclip`; custom copy command.
