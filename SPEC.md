# pyre — Specification

## Goals

- Persistent terminal sessions that survive client disconnects, SSH
  drops, and laptop suspends. PTY ownership lives in `pyred`, not in
  any UI process.
- Process-first model: every command run is a structured `Block` with
  start/end/exit-code/duration, not just a stream of bytes.
- Single binary set replaces the `tmux + alacritty` combination for a
  Linux daily driver — multiplexing, reattach, and rendering converge.
- First-class agent integration: panes are MCP resources that any MCP
  client can read, search, and act on without screen-scraping.
- Scriptable via Lua (mlua 5.4) at well-defined hook points.
- Searchable history across all sessions via Tantivy.

## Non-goals

- Windows / macOS support in S0–S6. Linux-first, no portability tax.
- Electron, web UI, or cloud sync by default.
- Telemetry of any kind.
- Reimplementing an ANSI parser — reuse `alacritty_terminal`.
- Replacing the user's shell. `pyre` runs `$SHELL`; it does not embed
  one.

## Glossary

| Term     | Definition |
|----------|------------|
| Session  | A named collection of panes owned by `pyred`, persisted across client lifetimes. |
| Pane     | A single PTY + parsed grid + scrollback + block log. Belongs to one session. |
| Block    | One command execution: prompt boundary → user input → command output → exit. Keyed by OSC 133 markers. |
| Command  | The text the user submitted that started a Block. |

## Architecture overview

`pyrec` (CLI) and `pyre` (renderer) are clients of `pyred` over a
local Unix domain socket. `pyred` owns every PTY via `portable-pty`,
runs the ANSI parser (`alacritty_terminal`), emits Block events to the
SQLite store, and streams updated grid frames + block deltas back to
attached clients. The wire format is defined in the `pyre-proto` crate
and versioned from day one (`proto_version: u32`).

### Process model (single vs hybrid)

`pyred` ships two process models, selectable via
`pyred.process_model = "single" | "hybrid"` (default `"single"`).

- **Single** (Option A in ADR-002): one `pyred` process owns the
  public socket, the `SessionRegistry`, all PTYs, the SQLite pool, and
  the Tantivy writer. Simplest, lowest memory, no crash isolation.
- **Hybrid** (Option C in ADR-002, accepted 2026-05-19): a thin
  supervisor binds the public `pyre.sock` and owns the aggregated
  Tantivy index. Each session runs as a worker process, owning its own
  PTYs and a per-session UDS at
  `$XDG_RUNTIME_DIR/pyre/session-<id>.sock`. Stream-mode (`0x02`)
  attaches are proxied transparently from the supervisor to the
  worker. Worker crashes evict only that session; the supervisor
  survives.

See [docs/adr/0002-daemon-process-architecture.md](docs/adr/0002-daemon-process-architecture.md)
for the full decision record and migration triggers.

## Block model (OSC 133)

Blocks are demarcated by the standard OSC 133 prompt sequences emitted
by a cooperating shell (zsh/bash with a small rc snippet, fish native):

| Marker | Meaning |
|--------|---------|
| OSC 133 ; A | Prompt start. Closes any open Block. |
| OSC 133 ; B | Prompt end / command start. Captures the command text. |
| OSC 133 ; C | Command output start. |
| OSC 133 ; D ; <exit> | Command end with exit code. Block finalized. |

Each finalized Block carries `{id, pane_id, session_id, command, started_at, ended_at, exit_code, cwd, stdout_offset, stdout_len}`.

## IPC protocol surface

Defined in `pyre-proto`. Request/response + a streaming output channel.
Transport TBD in **ADR-001** (`tonic` over UDS vs `tarpc` over UDS).

| Method        | Purpose |
|---------------|---------|
| `Spawn`       | Create a new pane in a session, run `$SHELL` or given argv. |
| `Attach`      | Subscribe to a pane's output stream and current grid snapshot. |
| `Detach`      | Drop a client subscription without killing the pane. |
| `Resize`      | Change cols/rows for a pane. |
| `Kill`        | Terminate a pane (SIGTERM, then SIGKILL after grace). |
| `ListSessions`| Enumerate sessions + panes + last-active timestamps. |
| `OutputFrame` (stream) | Server-push grid delta or raw bytes + block events. |
| `BlockEvent` (stream)  | Push on Block start/end with full Block record. |

Every message carries `proto_version`. Mismatched clients are rejected
with a versioned error envelope; no silent fallback.

## Persistence (SQLite)

Single file at `$XDG_DATA_HOME/pyre/state.db`. WAL mode. Three core
tables:

| Table     | Columns (essentials) |
|-----------|----------------------|
| `sessions`| `id, name, created_at, last_active_at` |
| `panes`   | `id, session_id, argv, cwd, cols, rows, created_at, closed_at` |
| `blocks`  | `id, pane_id, command, started_at, ended_at, exit_code, cwd, stdout_blob_path` |

Stdout is stored as compressed blobs on disk (zstd) referenced from
`blocks.stdout_blob_path`; the SQLite row carries metadata only.

## Search (Tantivy)

One index per user at `$XDG_DATA_HOME/pyre/index/`. Documents are
Blocks. Fields: `command`, `stdout` (tokenized), `cwd` (raw),
`exit_code` (i64), `started_at` (date). Queries from `pyrec search
"<query>"` or via MCP.

## Reattach semantics

- A pane survives every client disconnect. `pyred` keeps reading the
  PTY and updating the parsed grid + block log.
- On `Attach`, the daemon sends the current grid snapshot + the last
  N (configurable, default 1000) Blocks for that pane, then begins
  streaming live deltas.
- Multiple clients may attach to the same pane simultaneously
  (collaboration / mirror mode). Input from any client is serialized
  by `pyred` before being written to the PTY.

## Scripting (Lua via mlua 5.4)

User config at `$XDG_CONFIG_HOME/pyre/init.lua`. Hooks:

| Hook            | Fires on                                  | Receives |
|-----------------|-------------------------------------------|----------|
| `on_pane_spawn` | New pane created                          | `pane` table |
| `on_block_end`  | Block finalized (OSC 133 ; D)             | `block` table |
| `on_attach`     | Client attaches to a pane                 | `pane`, `client` |
| `on_keybind`    | User-defined chord matched in TUI         | `event` |

Hooks run in a sandboxed Lua VM owned by `pyred` (no `io`, no `os.execute`
unless explicitly enabled in config).

## MCP surface

`pyred` exposes an MCP server on the same UDS (or a sibling socket).
Resources:

- `pane://<session>/<pane_id>` — read current grid + last K Blocks.
- `block://<block_id>` — read one Block (command + stdout).
- Tools: `search_blocks(query)`, `spawn(argv, session)`, `send_input(pane, text)`.

This lets any MCP client consume pane state without screen-scraping.
See [docs/AGENTS.md](docs/AGENTS.md) for the agent multiplexer playbook.

## Security model

- UDS at `$XDG_RUNTIME_DIR/pyre.sock` with mode `0700`, owned by the
  invoking user. No network listener by default.
- Lua sandbox: no filesystem or process access unless `[lua].unsafe =
  true` in config.
- MCP tool calls that mutate state (`spawn`, `send_input`) require an
  explicit `[mcp].allow_mutations = true` in config; read-only by
  default.
- No telemetry, no auto-update, no cloud calls. Ever.

## Out of scope

- Windows and macOS support.
- Built-in shell, multiplexer protocol compatibility with tmux.
- Cloud sync, account systems, network-exposed daemon.
- Reimplementing the ANSI parser, a cron scheduler, or an external
  experiential memory layer — pyre exposes blocks and hooks locally.
