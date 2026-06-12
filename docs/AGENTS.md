# pyre — Agent multiplexer playbook

pyre is a standalone terminal product: persistent sessions, block-level
command history, full-text search, and MCP orchestration. Use this guide
when running multiple coding agents in parallel.

## Agent quickstart (MCP)

The minimal flow for an agent that needs to run a command and observe its
result:

```
1. session_spawn          → get session_id + pane_id
2. pyrec shell-init bash | source   (run once inside the pane via pane_send_keys)
3. pane_run_command       → send command, wait, get exit_code + output in one call
4. branch on exit_code    → 0 = success, non-zero = investigate
5. block_search           → find relevant failures across all session history
```

`pane_run_command` is the preferred way to run commands from an agent. It
replaces the error-prone `pane_send_keys → sleep → pane_capture` pattern.
It polls for a new finalized block (OSC 133 D marker) and returns
`{completed, exit_code, duration_ms, command, output, block_id}`. If
`completed` is false or `block_id` is absent, OSC 133 shell integration is
not active in the pane — install it with `pyrec shell-init bash` (or zsh /
fish) sourced inside the pane.

Structured error codes in every MCP error response (`data.code`):

| Code | Meaning | Action |
|------|---------|--------|
| `no_such_pane` | Prefix matched nothing | Call `list_panes` to refresh IDs |
| `ambiguous_pane_id` | Prefix matched >1 pane | Extend prefix to ≥12 chars |
| `no_such_session` | Session prefix matched nothing | Call `list_sessions` |
| `ambiguous_session_id` | Session prefix matched >1 | Extend prefix |
| `daemon_unreachable` | Cannot connect to pyred | Start daemon: `pyred` |

---

## Quickstart smoke test (10 min)

Exercises daemon, TUI, `pyrec`, and search end-to-end. Run this after a
fresh build to confirm the stack is wired correctly.

1. `pyred &` — start daemon in single-process mode. For hybrid: set `process_model = "hybrid"` in config and restart.
2. `pyre` — launch TUI; a fresh session opens automatically.
3. `Ctrl-Space "` — horizontal split; lower pane gets a new shell.
4. Install shell integration in the lower pane: `eval "$(pyrec shell-init bash)"` then press Enter.
5. Type `echo hello && sleep 1 && echo done` in the lower pane; wait for the block to finish (exit badge appears in the block ribbon below the pane).
6. In a second terminal: `pyrec wait-pane --pane <pane-id> --state waiting` — confirms the pane returned to the prompt.
7. `pyrec search "hello"` — confirms the block is indexed; output shows a snippet.
8. Back in TUI: `Ctrl-Space /` — open search overlay; type `hello`; `Enter` jumps focus to the source pane and sets the ribbon cursor on the matching block.
9. `Ctrl-Space [` — enter block ribbon mode; `←`/`→` navigate blocks; `Enter` opens the modal pager (full stdout, scrollable with `↑`/`↓`/`PgUp`/`PgDn`; `q` closes).
10. `Ctrl-Space y` — copy the last block's stdout to the clipboard.
11. `pyrec kill-session <session-id>` — clean up; `pyrec sessions` should return empty.

Pane and session IDs are UUIDs. Eight-character prefixes are accepted by all
`pyrec` subcommands that take `--pane` or `--session`.

---

## MCP tools reference (`pyre-mcp`)

Point any MCP client at `pyre-mcp` (stdio JSON-RPC 2.0). The server exposes
17 tools and MCP resources (`pane://`, `session://`, `block://`,
`state://panes`).

Mutating tools require `[mcp].allow_mutations = true` in the daemon config.

### Session and pane lifecycle

| Tool | Description |
|------|-------------|
| `session_spawn` | Spawn a new session with one pane. Returns `session_id` and `pane_id`. Accepts `shell`, `cwd`, `cols`, `rows`. |
| `session_close` | Close a session and all its panes. |
| `session_layout` | Create a session with multiple panes from a split spec (`layout.orient` + `layout.panes[]`). Each pane accepts `name`, `cwd`, `cmd`. |
| `pane_open` | Open a new pane inside an existing session. Returns `pane_id`. |
| `open_pane_split` | Split an existing pane. `orient`: `horizontal` (side-by-side) or `vertical` (stacked). |
| `gc_stale_sessions` | Evict sessions with no live panes. Returns list of evicted session UUIDs. |

### Pane I/O and state

| Tool | Description |
|------|-------------|
| `pane_send_keys` | Inject keystrokes into a pane. Set `append_enter: true` to simulate Enter. |
| `pane_capture` | Capture pane output: `source=ring` (ring buffer, default) or `source=block-last` (last finalized block stdout). |
| `pane_set_state` | Self-report pane lifecycle state. States: `Running`, `WaitingInput`, `Idle`, `Interactive`, `Crashed`, `Done`. |
| `pane_run_command` | Send a command and wait for completion. Returns `{completed, exit_code, duration_ms, cwd, command, output, block_id}`. Preferred over send+sleep+capture. Requires OSC 133 shell integration. |
| `pane_last_block` | Return metadata for the most recently finalized block on a pane. Set `include_output: true` to include stdout (truncated to 8 KB). Returns null when no block exists yet. |
| `wait_pane_state` | Wait until a pane reaches a lifecycle state. States: `waiting`, `running`, `idle`, `done`, `crashed`, `interactive`. |

### Inspection and search

| Tool | Description |
|------|-------------|
| `list_sessions` | List all active sessions with metadata. |
| `list_panes` | List panes, optionally filtered by `session_id`. |
| `block_search` | Full-text search across all block stdout (Tantivy). Filters: `failures_only`, `session`, `pane`, `exit_code`. |
| `get_session_layout` | Return the `LayoutNode` tree for a session as JSON (recursive HSplit/VSplit/Leaf nodes with weights). |

### Layout control

| Tool | Description |
|------|-------------|
| `set_pane_weight` | Adjust a pane's weight within its parent split (0–100, clamped to 5–95). Persists to SQLite; emits `LayoutChanged`. |

---

## Recommended layout (hybrid mode)

Enable the hybrid supervisor so each agent session runs in an isolated
worker process:

```toml
# ~/.config/pyre/pyred.toml (or your pyred config path)
process_model = "hybrid"
```

Pattern: **one heavy agent = one session = one worker**.

```bash
systemctl --user enable --now pyred   # or pyred in foreground
pyrec session-new --name api --cwd ~/projects/api -d
pyrec session-new --name web --cwd ~/projects/web -d
pyre   # TUI: sidebar shows per-session rollup
```

If one agent OOMs or panics, only that worker dies; the supervisor and
other sessions keep running. Block metadata and search stay unified in
the supervisor index.

---

## Pane states (UI labels)

The daemon tracks `PaneStateKind`. The TUI maps them to agent-friendly
labels:

| Daemon state   | UI label    | Meaning |
|----------------|-------------|---------|
| `WaitingInput` | blocked     | Prompt visible; agent or shell needs input |
| `Running`      | working     | Active output or command running |
| `Idle`         | idle        | Quiet after a block ended |
| `Done`         | done        | Process exited cleanly |
| `Crashed`      | crashed     | Non-zero exit or shell died |
| `Interactive`  | interactive | Full-screen TUI (vim, less, …) |

Agents can override state via MCP `pane_set_state` or `pyrec integration
install` after installing a hook snippet.

---

## Detected agents

pyre heuristically classifies the foreground process per pane:

| Kind        | Typical binary / argv |
|-------------|------------------------|
| ClaudeCode  | `claude` |
| Codex       | `codex` |
| Pi          | `pi` |
| OpenCode    | `opencode` |
| CursorAgent | `cursor`, `cursor-agent` |
| Shell       | bash, zsh, fish, … |
| Unknown     | everything else |

Detection runs on the state-engine poll (~500 ms). Integrations can
report state earlier via `set_pane_state`.

---

## Orchestration without the TUI

```bash
# Wait until a pane needs input (30s timeout)
pyrec wait-pane --pane <id> --state waiting --timeout 30

# Read recent ring-buffer lines
pyrec pane read --pane <id> --lines 50 --source ring

# Read stdout of the last finalized block
pyrec pane read --pane <id> --source block-last

# Run a command in a session pane
pyrec pane-run --session <id> -- ls -la

# Search all command history
pyrec search "error E0425"

# Install shell integration (one-time, per pane)
eval "$(pyrec shell-init bash)"
```

---

## Renderer choice

Launch `pyre` (TUI) for full multi-pane tiling. Launch `pyre-gpu` for
a windowed view with GPU-accelerated cell rasterization.

`pyre-gpu` is a single-pane-window today. Ctrl+Tab swaps the attached
pane via stream reconnect — it does not tile. Use `pyre` if you need
multiple agent panes visible at once. See [ADR-003](adr/0003-render-backend.md)
for the gap and timeline (S6.2).

---

## Hooks

Optional `hooks.toml` runs local scripts on `on_block_end` and
`on_pane_state` (e.g. desktop notify when a pane becomes `waiting`).
See [CONFIG.md](CONFIG.md).

---

## Competitive positioning — pyre vs herdr

herdr is the closest comparable in the agent-multiplexer niche. The
honest picture as of v0.4 sprint:

### Where pyre wins today

- **Process isolation.** Hybrid supervisor (ADR-002) puts each agent
  session in its own worker process. One claude OOM kills its worker,
  not the multiplexer or the other agents. herdr has no published
  process boundary between agents.
- **MCP-first orchestration.** Any MCP client can drive pyre via 17
  tools: `session_spawn`, `pane_run_command`, `pane_capture`,
  `wait_pane_state`, `block_search`, plus `pane://` and `block://`
  resources. herdr ships a single bundled chat UI; external agents
  have to screen-scrape.
- **Block-indexed Tantivy search** with native `exit_code` facet
  (`failures_only`) exposed across CLI, MCP, TUI overlay, and GPU
  overlay. herdr surfaces scrollback, not a searchable command index.
- **Two renderers, one daemon.** ratatui TUI (`pyre`) and softbuffer
  windowed viewer (`pyre-gpu`) consume the same `pyre-proto` stream.
  ADR-003 forbids forking the daemon to chase GPU.
- **Versioned wire protocol.** `PROTO_VERSION=2` handshake rejects
  mismatched clients at connect time; upgrade path is explicit.

### Where herdr still leads (closing in v0.4)

- **Themes maturity** — herdr ships polished palettes out of the box.
  pyre's `pyre-themes` registry now ships 18 built-in palettes
  selectable via `Ctrl-Space T`.
- **Mouse polish** — herdr's mouse handling (drag-to-resize splits,
  right-click context, hover affordances) is more refined.
- **Remote attach** — herdr advertises remote attach in marketing.
  pyre ships `pyrec remote` as an `ssh -L` UDS-tunnel helper per
  ADR-0004 — same Unix Domain Socket protocol, SSH owns auth and
  transport, zero daemon changes.
- **Desktop notifications** — herdr surfaces toasts via the host OS.
  pyre's toast deck is in-TUI only today.

### Roadmap parity

The v0.4 stabilization sprint closes the major gaps. See
[ROADMAP.md](../ROADMAP.md) for milestone status.
