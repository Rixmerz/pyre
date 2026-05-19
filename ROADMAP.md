# pyre — Roadmap

## Sprints

| Sprint | Goal | Key deliverables |
|--------|------|------------------|
| **S0** | Bootstrap | Workspace layout, crate skeletons (`pyre-proto`, `pyred`, `pyrec`, `pyre-tui`), CI scaffold, `SPEC.md`, `ARCHITECTURE.md`, `ROADMAP.md`, ADR-001 draft (IPC choice). |
| **S1** | Daemon + PTY | `pyred` spawns and owns a PTY via `portable-pty`, streams raw bytes to one `pyrec` over UDS, basic `Spawn` / `Attach` / `Detach` / `Kill`. No persistence yet. |
| **S2** | Blocks + persistence | Integrate `alacritty_terminal` parser, recognise OSC 133, materialise `Block` records, write to SQLite + zstd stdout blobs, basic `pyrec list` and `pyrec search` (linear scan). |
| **S3** | Multi-pane + reattach | Multiple panes per session, multiple sessions, full reattach with grid snapshot + last N blocks replay, multi-client mirror mode with serialized input. Hybrid supervisor + per-session workers landed (ADR-002 Accepted, 2026-05-19) — unblocks per-session isolation and clean reattach across daemon restarts; remaining S3 work tracks the multi-client + replay surface. |
| **S4** | TUI dogfood (**MVP**) | `pyre-tui` (ratatui) replaces tmux+alacritty in daily use: tabs, splits, block ribbon, scrollback navigation, Tantivy-backed search UI. **MVP criterion met.** |
| **S5** | MCP + AI + Tantivy polish | MCP server exposing `pane://` and `block://` resources + `search_blocks` / `spawn` / `send_input` tools, Lua hooks (`on_pane_spawn`, `on_block_end`), evaluate MEMI feed from finalized Blocks. |
| **S6** | GPU render | `pyre-gpu` using `wgpu` + `winit`, same `pyre-proto` client surface as `pyre-tui`, behavior parity, then performance gains. ADR-003 closes. |

## MVP criterion

End of **S4**: the user runs `pyred` + `pyre-tui` as the daily driver
and removes `tmux` and `alacritty` from their workflow without
regressions on reattach, multiplexing, and search. AI/MCP and GPU
render are explicitly post-MVP.

## Risks

| Risk | Mitigation |
|------|------------|
| ANSI parser is a time sink. | Reuse `alacritty_terminal`. Do NOT reimplement. Wrap, don't fork. |
| IPC schema churn S0–S2. | `proto_version: u32` on every message from day one; integration test that rejects mismatched versions. |
| Scope creep — AI before core. | AI work blocked until S5. No MCP code merges into `main` before S4 ships. |
| `portable-pty` quirks on Linux distros. | Linux-first; Windows code paths `#[cfg]`-gated and untested until post-S6. |
| SQLite write contention with high-frequency Block events. | WAL mode; batch BlockEvent writes per pane on a 50 ms tick. |
| GPU renderer rewrite temptation. | ADR-003 forces a binary swap, not a rewrite. `pyre-gpu` consumes the same `pyre-proto` streams as `pyre-tui`. |

## Catalog-overlap verification

`pyre` is a product, not a framework — no new coinage. Verify against
existing catalog before adding anything:

| Concern | Existing tool | pyre stance |
|---------|---------------|-------------|
| Per-project + cross-project memory | `jig` | Consume via MCP. Do not duplicate the memory layer. |
| Cron / scheduled agent runs | `schedule-mcp` | Call it. Do not build a scheduler in `pyred`. |
| Experiential feed from finalized work units | MEMI | Evaluate in S5: feed `on_block_end` events into MEMI; do not reinvent. |
| Commit-quality enforcement | `commit-guardian` | Orthogonal; `pyre` is a terminal, not a git tool. No overlap. |
| Code analysis / smells | `delta-cube` (via `jig`) | Orthogonal to terminal scope. |

If a future need looks like a sixth framework with similar scope, stop
and fuse with one of the above instead of coining a new name.
