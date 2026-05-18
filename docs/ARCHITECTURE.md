# pyre — Architecture

## Process diagram

```
 ┌─────────────────────────────────────────────────────────────────────┐
 │                              pyred                                  │
 │                                                                     │
 │  UDS listener (pyre.sock, mode 0700)                                │
 │  ┌──────────────┐   ┌──────────────┐   ┌─────────────────────────┐ │
 │  │  tarpc/ctrl  │   │  stream mux  │   │      MCP server         │ │
 │  │  RPC handler │   │  (raw bytes) │   │  (resources + tools)    │ │
 │  └──────┬───────┘   └──────┬───────┘   └───────────┬─────────────┘ │
 │         │                  │                        │               │
 │  ┌──────▼──────────────────▼────────────────────────▼────────────┐ │
 │  │                   SessionRegistry                              │ │
 │  │   Session { panes: HashMap<PaneId, PaneHandle> }               │ │
 │  └───────────────────────┬────────────────────────────────────────┘ │
 │                          │ per pane                                  │
 │  ┌───────────────────────▼────────────────────────────────────────┐ │
 │  │  PaneHandle                                                    │ │
 │  │  ┌──────────────┐  ┌───────────────┐  ┌──────────────────────┐│ │
 │  │  │  PTY master  │  │  ANSI parser  │  │  RingBuf (scrollback)││ │
 │  │  │ (portable-   │  │ (alacritty_   │  │  StateTracker        ││ │
 │  │  │  pty)        │  │  terminal)    │  │  BlockAccumulator    ││ │
 │  │  └──────┬───────┘  └──────┬────────┘  └──────────────────────┘│ │
 │  └─────────│─────────────────│───────────────────────────────────┘ │
 │            │ raw bytes       │ Block events                         │
 │  ┌─────────▼─────────┐  ┌───▼──────────────────────────────────┐   │
 │  │  Shell / process  │  │  Store (SQLite via sqlx)             │   │
 │  │  (child of PTY)   │  │  BlockIndex (Tantivy)                │   │
 │  └───────────────────┘  │  HookRunner (hooks.toml)             │   │
 │                          └──────────────────────────────────────┘   │
 └─────────────────────────────────────────────────────────────────────┘
        ▲ UDS              ▲ UDS                    ▲ UDS (MCP)
        │                  │                        │
 ┌──────┴──────┐   ┌───────┴──────┐   ┌────────────┴────────────────┐
 │   pyrec     │   │  pyre-tui    │   │    MCP client (jig, etc.)   │
 │  (CLI)      │   │  (ratatui)   │   │                             │
 └─────────────┘   └──────────────┘   └─────────────────────────────┘
```

All three client processes speak `pyre-proto` over the same UDS.
`pyred` is the only process that ever opens a PTY file descriptor.

System clipboard is reached by the clients directly via `wl-copy` / `xclip`;
the daemon does not touch the clipboard.

---

## Crate map

| Crate | Role | Key deps |
|-------|------|----------|
| `pyre-proto` | Wire types, versioned IPC schema (`PyreDaemon` tarpc service), codec helpers. Shared by daemon and all clients. | `tarpc`, `serde`, `uuid`, `chrono` |
| `pyred` | Daemon. Owns every PTY, runs the ANSI parser, emits Block events, persists state, serves IPC + MCP over UDS. | `portable-pty`, `tokio`, `alacritty_terminal`, `sqlx` (sqlite), `tantivy`, `toml`, `serde` |
| `pyrec` | Thin CLI client. All subcommands listed in `docs/USAGE.md`. Stdio bridge to a pane in interactive mode. | `pyre-proto`, `tokio`, `clap`, `crossterm` |
| `pyre-tui` | MVP renderer. ratatui-based multiplexed view: tab = session, cells = panes, block ribbon at bottom. Dogfood target. | `pyre-proto`, `ratatui`, `crossterm`, `tokio` |
| `pyre-mcp` | MCP server. Exposes sessions, panes, blocks as resources; exposes spawn/send-keys/capture as tools. | `pyre-proto`, `tokio`, `serde_json` |
| `pyre-gpu` | S6 GPU renderer using `wgpu`. Drop-in replacement for `pyre-tui` — same `pyre-proto` client. | `pyre-proto`, `wgpu`, `winit`, `tokio` |

---

## Data flow: PTY → parser → storage

```
User keystroke
  │
  ▼
pyrec / pyre-tui: InputFrame { session, data: Bytes }
  │  (tarpc stream connection)
  ▼
pyred stream mux → PTY master write
  │
  ▼
Shell reads from PTY slave → executes → writes stdout to PTY slave
  │
  ▼
pyred reads PTY master (raw bytes)
  │
  ├─► RingBuf.push(bytes)          — scrollback for capture-pane
  │
  ├─► ANSI parser (alacritty_terminal::Grid)
  │     └─► OSC 133 A  → StateTracker: PromptStart
  │     └─► OSC 133 B  → BlockAccumulator: open Block { command, cwd }
  │     └─► OSC 133 C  → BlockAccumulator: output phase start
  │     └─► OSC 133 D;N → BlockAccumulator: finalize(exit_code=N)
  │              │
  │              ├─► Store.insert_block(block) [SQLite]
  │              ├─► BlockIndex.index(block)   [Tantivy]
  │              └─► HookRunner.on_block_end(block)
  │
  └─► OutputFrame broadcast → subscribed stream clients
```

---

## Block lifecycle

1. Shell emits `OSC 133 ; A` — prompt displayed. `StateTracker` records `PromptStart`.
2. User runs a command. Shell emits `OSC 133 ; B` — command start. `BlockAccumulator` opens a new `Block` with the command string and cwd.
3. Shell emits `OSC 133 ; C` — output phase starts. Subsequent PTY bytes are appended to the block's stdout buffer.
4. Command finishes. Shell emits `OSC 133 ; D ; <exit_code>` — block end.
   - `BlockAccumulator` finalises the block (timestamps, exit code, stdout length).
   - `Store` writes the block row to SQLite.
   - `BlockIndex` indexes the document in Tantivy.
   - `HookRunner` fires `on_block_end` hooks in `hooks.toml`.
   - `BlockEvent::BlockEnd` is broadcast to all subscribed stream clients.
5. Clients (pyrec / pyre-tui / MCP) update their views from `BlockEvent` — they never parse ANSI themselves.

---

## State engine heuristic

The state engine classifies each pane using OSC 133 markers plus a fallback
based on cursor activity:

| State | Condition |
|-------|-----------|
| `idle` | OSC 133 A received (prompt visible) |
| `running` | OSC 133 B received, OSC 133 D not yet seen |
| `waiting` | Running for >2 s with no new PTY bytes (heuristic: waiting for input) |
| `error` | OSC 133 D received with non-zero exit code |

Shells that do not emit OSC 133 always show `idle` (no marker = no state
transitions). The MCP server exposes the current state so agents can wait for
a pane to become idle before reading its output.

---

## Decision log

| ADR | Decision | Status |
|-----|----------|--------|
| ADR-001 | IPC transport: **tarpc** chosen over tonic. Native Rust types, no protobuf toolchain, lower ceremony for a single-process UDS service. | Resolved S0 |
| ADR-002 | Stdout blob encoding: raw bytes stored in SQLite blob column, compressed with zstd. ANSI sequences preserved; clients may strip on display. | Resolved S2 |
| ADR-003 | Render backend swap: `pyre-tui` → `pyre-gpu` in S6. Both consume identical `OutputFrame` and `BlockEvent` streams; the swap is a binary replacement, not a rewrite. | Planned S6 |

---

## Invariants

- Only `pyred` ever opens a PTY file descriptor.
- Every tarpc message carries `proto_version`; mismatched clients are rejected, never silently downgraded.
- SQLite is the single source of truth for session/pane/block metadata. Tantivy is rebuildable from it.
- No client logic depends on ANSI parsing — the daemon ships parsed state and events.
- The UDS socket is created with mode 0700 and owned by the user; no other user can connect.
- Hooks are run with a 5-second timeout and do not block the PTY read loop.
