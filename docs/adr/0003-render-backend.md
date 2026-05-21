# ADR-003: Render backend swap (`pyre-tui` → `pyre-gpu`)

**Status:** Accepted (Phase 1 — 2026-05-20)  
**Context:** S6 GPU render

## Decision

`pyre-gpu` is a drop-in **viewer binary** that consumes the same `pyre-proto`
control + stream sockets as `pyre-tui`. It does not fork the daemon or block
pipeline. Users choose the front end at launch time:

| Binary | Backend | MVP scope |
|--------|---------|-----------|
| `pyre` (`pyre-tui`) | ratatui + crossterm | Full multiplexing, sidebar, search, agents |
| `pyre-gpu` | winit + softbuffer | Single-pane attach, VT grid rasterization |

Phase 1 (`pyre-gpu` v0.1): windowed attach to one pane, keyboard input over the
stream UDS, cell-colored rasterization.

S6.1 (`pyre-gpu` v0.1.1): fontdue glyph atlas from system monospace font;
Ctrl+Tab / Ctrl+Shift+Tab cycles panes in the active session (stream reconnect);
Ctrl+/ opens Tantivy `search_blocks` overlay (`!query` = failures only).
Full layout parity with `pyre` TUI and optional `wgpu` remain future work.

### Phase 1 — single-pane-window constraint

`pyre-gpu` is a **single-pane-window** viewer. Ctrl+Tab in `pyre-gpu`
performs a stream reconnect — it detaches from the current pane and
reattaches to the next one in the session. It does **not** tile or
split the window; only one pane is visible at a time.

`pyre` (the TUI) tiles multiple panes simultaneously using ratatui
layout. Users expecting split-pane display should use `pyre` until
real tiling lands in `pyre-gpu` (planned for S6.2).

## Rationale

- **Binary swap, not rewrite:** `OutputFrame` / `InputFrame` framing stays stable;
  ADR-001 IPC and ADR-002 process model are unchanged.
- **softbuffer first:** proves the GPU path on macOS/Linux without binding to a
  specific GPU API revision; `wgpu` text atlas lands when glyph metrics are shared
  with the TUI.
- **TUI remains default:** `pyre` stays the dogfood driver until `pyre-gpu`
  reaches layout/search parity.

## Consequences

- Two front-end crates to maintain; shared VT parsing stays in `alacritty_terminal`
  until a `pyre-render` crate is extracted.
- CI builds both `pyre` and `pyre-gpu` targets.
- Packaging may ship both binaries; config does not select renderer yet.

## Alternatives considered

1. **Embed wgpu inside `pyre-tui`** — rejected; violates swap semantics and couples
   ratatui lifecycle to GPU drivers.
2. **Remote thin client** — out of scope (identity constraint: no cloud default).
3. **Keep TUI-only forever** — rejected; roadmap S6 targets scrollback perf on
   large grids.
