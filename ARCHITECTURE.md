# pyre — Architecture

## Crate map

| Crate       | Role                                              | Key deps |
|-------------|---------------------------------------------------|----------|
| `pyre-proto`| Wire types, versioned IPC schema, codec helpers. Shared by daemon and clients. | `serde`, `tokio`, (`tonic` OR `tarpc` — ADR-001) |
| `pyred`     | The daemon. Owns every PTY, runs the ANSI parser, emits Block events, persists state, serves IPC + MCP over UDS. | `portable-pty`, `tokio`, `alacritty_terminal`, `sqlx` (sqlite), `tantivy`, `mlua`, `toml`, `serde` |

Process model: see [docs/adr/0002-daemon-process-architecture.md](docs/adr/0002-daemon-process-architecture.md).
| `pyrec`     | Thin CLI client. `attach`, `detach`, `list`, `spawn`, `search`, `kill`. Stdio bridge to a pane. | `pyre-proto`, `tokio`, `clap`, `crossterm` (raw-mode passthrough) |
| `pyre-tui`  | MVP renderer. ratatui-based multiplexed view: tabs = sessions, splits = panes, block ribbon at the bottom. The dogfood target for S4. | `pyre-proto`, `ratatui`, `crossterm`, `tokio` |
| `pyre-gpu`  | S6 GPU renderer using `wgpu`. Drop-in replacement for `pyre-tui` as the user-facing front end. Same `pyre-proto` client. | `pyre-proto`, `wgpu`, `winit`, `tokio` |

## Process / socket diagram

### Single mode (default, `process_model = "single"`)

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
   | pyre-mcp |  <----------------->  |  (MCP resources)      |
   +----------+                       +-----------------------+
```

### Hybrid mode (`process_model = "hybrid"`, ADR-002 Accepted)

```
                       supervisor.sock                   session-<id>.sock
   +----------+    ($XDG_RUNTIME_DIR/pyre.sock)     ($XDG_RUNTIME_DIR/pyre/...)
   |  pyrec   |  <-------------------------+          +--------+    PTY    +-------+
   +----------+                            |          |        | <-------> | shell |
                                           v          v        |  worker  |+-------+
   +----------+                       +----+----------+----+   |  (one    |
   | pyre-tui |  <----------------->  |    supervisor      | <-+  per     | <-+
   +----------+                       |  (proxy + index +  |   |  session)|   |
                                      |   SessionRegistry) | --+----------+   |
                                      +--------+-----------+                  |
                                               | <--- sqlite ---> state.db    |
                                               | <--- tantivy --> index/      |
                                               +------------------------------+
                                            stream mode (0x02) proxied to worker
```

In hybrid, the supervisor binds the single public socket and proxies
RPCs to the right per-session worker. Stream frames flow
worker → supervisor → client. Workers own PTYs; the supervisor owns
the aggregated Tantivy index and `SessionRegistry`.

All three client crates speak `pyre-proto` over the same public UDS.
In single mode `pyred` is the only process that ever touches a PTY;
in hybrid mode that role moves to the per-session worker, and the
supervisor never opens a PTY directly.

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
