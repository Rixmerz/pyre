# pyre — Roadmap

## Sprints

| Sprint | Goal | Key deliverables |
|--------|------|------------------|
| **S0** | Bootstrap | Workspace layout, crate skeletons (`pyre-proto`, `pyred`, `pyrec`, `pyre-tui`), CI scaffold, `SPEC.md`, `ARCHITECTURE.md`, `ROADMAP.md`, ADR-001 draft (IPC choice). |
| **S1** | Daemon + PTY | `pyred` spawns and owns a PTY via `portable-pty`, streams raw bytes to one `pyrec` over UDS, basic `Spawn` / `Attach` / `Detach` / `Kill`. No persistence yet. |
| **S2** | Blocks + persistence | Integrate `alacritty_terminal` parser, recognise OSC 133, materialise `Block` records, write to SQLite + zstd stdout blobs, basic `pyrec list` and `pyrec search` (linear scan). |
| **S3** | Multi-pane + reattach | Multiple panes per session, multiple sessions, full reattach with grid snapshot + last N blocks replay, multi-client mirror mode with serialized input. Hybrid supervisor + per-session workers landed (ADR-002 Accepted, 2026-05-19) — unblocks per-session isolation and clean reattach across daemon restarts; remaining S3 work tracks the multi-client + replay surface. |
| **S4** | TUI dogfood (**MVP**) | `pyre-tui` (ratatui) replaces tmux+alacritty in daily use: tabs, splits, block ribbon, scrollback navigation, Tantivy-backed search UI. **MVP criterion met.** |
| **S5** | **Agent multiplexer** | Hybrid profile docs; `AgentKind` detection; sidebar rollup + seen/done UX; `wait_pane_state` RPC; `pyrec` orchestration (`wait-pane`, `pane read`, `pane run`); MCP `wait_pane_state` + `pane://` / `block://` resources; `docs/AGENTS.md` playbook; doc hygiene (pyre as standalone product). |
| **S6** | GPU render | **S6.1 landed:** `pyre-gpu` glyph atlas, Ctrl+Tab multi-pane, **Ctrl+/ block search** (`!` = failures), ADR-003. **Later:** layout parity with TUI, optional `wgpu`. |
| **S5.1** | Agent ops hardened | `proto_version` handshake; hybrid replay snapshot; macOS `ps` agent detect + `osascript` notify; `pyrec doctor`; search `failures_only` / TUI `!` prefix; `integration install` scripts; `pyrec select-pane` + TUI focus file. |

## MVP criterion

End of **S4**: the user runs `pyred` + `pyre-tui` as the daily driver
and removes `tmux` and `alacritty` from their workflow without
regressions on reattach, multiplexing, and search. GPU render is
post-MVP (S6).

## S5 success criterion

End of **S5**, on Linux with `process_model = "hybrid"`:

1. Run **≥2 agent sessions** (one worker each) without cross-session crashes.
2. Sidebar shows **blocked / working / idle** with detected agent kind.
3. Orchestrate via **`pyrec wait-pane`**, **`pyrec pane read`**, **`pyrec pane run`** without the TUI.
4. Orchestrate via **MCP** (`block_search`, `pane_capture`, `wait_pane_state`) using [docs/AGENTS.md](docs/AGENTS.md).
5. **Reattach** with stable block replay on hybrid.
6. Search past failures with `pyrec search` / MCP `block_search`.

## Risks

| Risk | Mitigation |
|------|------------|
| ANSI parser is a time sink. | Reuse `alacritty_terminal`. Do NOT reimplement. Wrap, don't fork. |
| IPC schema churn S0–S2. | `proto_version: u32` on every message from day one; integration test that rejects mismatched versions. |
| Scope creep — agent integrations. | S5 limits install hooks to a small set of binaries; everything else uses heuristic `AgentKind` detection. |
| `portable-pty` quirks on Linux distros. | Linux-first; Windows code paths `#[cfg]`-gated and untested until post-S6. |
| SQLite write contention with high-frequency Block events. | WAL mode; batch BlockEvent writes per pane on a 50 ms tick. |
| GPU renderer rewrite temptation. | ADR-003 forces a binary swap, not a rewrite. `pyre-gpu` consumes the same `pyre-proto` streams as `pyre-tui`. |

## pyre capabilities (product surface)

`pyre` is a standalone terminal product — not a framework. Prefer extending
these surfaces before adding new named subsystems:

| Capability | Surface |
|------------|---------|
| Multiplexing + PTY | `pyred`, `pyrec`, `pyre-tui` |
| Command history + search | Blocks (OSC 133), SQLite, Tantivy, `block_search` |
| Per-session isolation | Hybrid supervisor + workers (ADR-002) |
| Agent orchestration | `wait_pane_state`, `pyrec wait-pane`, MCP tools |
| External automation | Any MCP client via `pyre-mcp` |
| User hooks | `hooks.toml` / Lua (`on_block_end`, `on_pane_state`) |
