# pyre — Architecture

## Crate map

| Crate       | Role                                              | Key deps |
|-------------|---------------------------------------------------|----------|
| `pyre-proto`| Wire types, versioned IPC schema, codec helpers. Shared by daemon and clients. | `serde`, `tokio`, (`tonic` OR `tarpc` — ADR-001) |
| `pyred`     | The daemon. Owns every PTY, runs the ANSI parser, emits Block events, persists state, serves IPC + MCP over UDS. | `portable-pty`, `tokio`, `alacritty_terminal`, `sqlx` (sqlite), `tantivy`, `mlua`, `toml`, `serde` |
| `pyrec`     | Thin CLI client. `attach`, `detach`, `list`, `spawn`, `search`, `kill`. Stdio bridge to a pane. | `pyre-proto`, `tokio`, `clap`, `crossterm` (raw-mode passthrough) |
| `pyre-tui`  | MVP renderer. ratatui-based multiplexed view: tabs = sessions, splits = panes, block ribbon at the bottom. The dogfood target for S4. | `pyre-proto`, `ratatui`, `crossterm`, `tokio` |
| `pyre-gpu`  | S6 GPU renderer using `wgpu`. Drop-in replacement for `pyre-tui` as the user-facing front end. Same `pyre-proto` client. | `pyre-proto`, `wgpu`, `winit`, `tokio` |

## Process / socket diagram

```
   +----------+      UDS (0700)       +-----------------------+
   |  pyrec   |  <----------------->  |                       |       PTY        +-------+
   +----------+                       |        pyred          | <-------------> | shell |
                                      |  (PTY owner +         |                  +-------+
   +----------+      UDS (0700)       |   parser + store)     |
   | pyre-tui |  <----------------->  |                       | <--- sqlite ---> state.db
   +----------+                       |                       | <--- tantivy --> index/
                                      |                       | <--- mlua    --> init.lua
   +----------+      UDS (MCP)        |                       |
   |   jig    |  <----------------->  |  (MCP resources)      |
   +----------+                       +-----------------------+
```

All three client crates speak `pyre-proto` over the same UDS. `pyred`
is the only process that ever touches a PTY file descriptor.

## Block lifecycle (dataflow)

1. User types `ls -la <Enter>` in an attached client.
2. Client sends raw bytes via `OutputFrame` input channel → `pyred`.
3. `pyred` writes bytes to the PTY master. Shell receives, runs `ls`.
4. Shell emits OSC 133 ; B (command start) → ANSI parser inside
   `pyred` recognises the marker, opens a new `Block { command: "ls -la", started_at: now }`.
5. Shell emits OSC 133 ; C, then the command stdout. Parser appends to
   the active Block's stdout buffer.
6. Shell emits OSC 133 ; D ; 0 (exit). Parser finalises the Block,
   writes the row to SQLite, writes the stdout blob to disk, indexes
   the document in Tantivy, runs the `on_block_end` Lua hook, and
   pushes a `BlockEvent` to every subscribed client.
7. Clients (pyrec / pyre-tui / MCP) update their views from the
   `BlockEvent` — they never parse ANSI themselves.

## Decision points

| ID       | Decision                                        | Resolves in |
|----------|-------------------------------------------------|-------------|
| ADR-001  | IPC transport: `tonic` (gRPC over UDS) vs `tarpc` (Rust-native RPC) | S0 |
| ADR-002  | Stdout blob encoding (raw vs ANSI-stripped + sidecar) | S2 |
| ADR-003  | Render backend swap: `pyre-tui` → `pyre-gpu` while keeping the same `pyre-proto` client surface | S6 |

The render backend split (`pyre-tui` first, `pyre-gpu` later) is the
strongest architectural seam: both consume identical `OutputFrame` and
`BlockEvent` streams, so the user-facing replacement in S6 is a
binary swap, not a rewrite.

## Invariants

- Only `pyred` ever opens a PTY.
- Every message on the wire carries `proto_version`; mismatched
  clients are rejected, never silently downgraded.
- SQLite is the single source of truth for session/pane/block
  metadata. Tantivy is rebuildable from it.
- No client logic depends on ANSI parsing; the daemon ships parsed
  state.
