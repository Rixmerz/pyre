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

## UI — themes and notifications

`pyre-tui` and `pyre-gpu` read user-facing UI knobs from
`$XDG_CONFIG_HOME/pyre/config.toml` (default
`~/.config/pyre/config.toml`).

### `[ui]`

```toml
[ui]
theme = "catppuccin-mocha"   # default: "ember"
```

`theme` is the machine-readable name from the `pyre-themes` registry.
Valid values (18 built-in palettes):

| Name | Variant |
|------|---------|
| `catppuccin-mocha` | dark |
| `catppuccin-latte` | light |
| `tokyo-night` | dark |
| `tokyo-night-light` | light |
| `gruvbox-dark` | dark |
| `gruvbox-light` | light |
| `one-dark` | dark |
| `one-light` | light |
| `solarized-dark` | dark |
| `solarized-light` | light |
| `kanagawa` | dark |
| `rose-pine` | dark |
| `rose-pine-dawn` | light |
| `vesper` | dark |
| `nord` | dark |
| `dracula` | dark |
| `terminal` | follows terminal palette |
| `ember` | dark (pyre default) |

Unknown names fall back to `ember`. In `pyre-tui`, `Ctrl-Space T` opens
a live picker that mutates the active theme and rewrites this key on
disk. `pyre-gpu` reads the key at startup; live switch is TUI-only
for now.

### `[ui.notifications]`

In-TUI toast deck for pane lifecycle events (`Spawned`, `Closed`,
`WaitingInput`, `Done`, `Crashed`). `Idle` and `Running` transitions
are suppressed.

```toml
[ui.notifications]
enabled     = true   # master toggle; matches Ctrl-Space N initial state
ttl_ms      = 4000   # per-toast lifetime in milliseconds
max_visible = 5      # cap on simultaneous toasts; oldest evicted first
```

`Ctrl-Space N` flips `enabled` at runtime. M2 of the v0.2 UX sprint
extends this with desktop bridges (`notify-send` / D-Bus on Linux,
`osascript` on macOS) and per-kind routing — in flight, not landed.

---

## Future configuration knobs (planned)

The following knobs are not yet wired to `config.toml`. Ring buffer capacity is
fixed in code today (`RingBuf::new` in `crates/pyred/src/pty.rs`). They will land
in `hooks.toml` or `pyre.toml` in a later sprint.

- `[ringbuf]` — `lines_per_pane = 10000` scrollback depth.
- `[search]` — `index_path`, `writer_heap_mb = 64`.
- `[tui]` — font family, font size (theme lives under `[ui]`, above).
- `[mcp]` — bind address for the MCP UDS, list of allowed tool names.
- `[lua]` — path to `init.lua`, sandbox memory limit.
- `[clipboard]` — prefer `wl-copy` vs `xclip`; custom copy command.
