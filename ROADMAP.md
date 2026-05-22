# pyre — Roadmap

## v0.2.0 ready

All five UX milestones landed. pyre now matches herdr on theme breadth,
in-TUI notifications, mouse polish, and remote attach helper; pulls ahead
on process isolation, MCP-first orchestration, and Tantivy block-indexed
search.

## v0.2 — UX parity-plus with herdr

Goal: close the UX gap with herdr while preserving pyre's architectural
advantages (process isolation, MCP-first orchestration, blocks + Tantivy,
two renderers on one daemon).

| Milestone | Scope | Status | Commits / Artifacts |
|-----------|-------|--------|---------------------|
| **M1 — Theme system** | `pyre-themes` registry with 18 built-in palettes (catppuccin × 2, tokyo-night × 2, gruvbox × 2, one × 2, solarized × 2, kanagawa, rose-pine × 2, vesper, nord, dracula, terminal, ember). `Ctrl-B T` live picker overlay in `pyre-tui` that persists to `config.toml`. `[ui] theme` schema in `config.toml`, consumed by both renderers. | ✅ landed | `7ae5089`, `bd87faf` |
| **M2 — Toast notifications** | In-TUI deck for pane lifecycle (`Spawned`, `Closed`, `WaitingInput`, `Done`, `Crashed`). `Ctrl-B N` toggle. `[ui.notifications]` schema. Desktop bridge (`notify-send` / D-Bus on Linux, `osascript` on macOS) and per-kind routing rules. | ✅ landed | `c1ecbc0` |
| **M3 — Mouse polish** | Drag-to-resize splits, right-click pane context menu, hover affordances on block ribbon, mouse-driven scroll-back regions matching herdr's polish. | ✅ landed | `1221f54` |
| **M4 — TUI ↔ GPU parity** | Theme live-switch in `pyre-gpu`, toast deck mirror, ribbon parity for block navigation. | ✅ landed | `1795fa0` |
| **M5 — Remote attach helper** | `pyrec remote <host>` derives SSH `-L` tunnel for `pyred.sock`; local `pyre --socket <path>` then attaches over a UDS at the local end of the tunnel. SSH owns auth + transport; daemon unchanged. v0.3 may add native TLS. | ✅ landed | ADR-0004, `2f72dd1` |

Risks specific to v0.2 (all closed):

- **M2 desktop bridge** — resolved with graceful no-op fallback for
  Wayland headless / minimal WMs.
- **M3 scope** — locked to drag-resize + context + hover; delivered
  without over-investment.
- **M4 GPU theme live-switch** — flicker-free atlas recreation landed
  in `1795fa0`.
- **M5 reconnect/keepalive** — auto-reconnect and `XDG_RUNTIME_DIR`
  auto-detection are explicit v0.3 follow-ups in ADR-0004.

---

## v0.1.0 ready

All sprints S1..S7 have landed. Branch `feat/s5-s6-gpu-search-agent-ops`
carries the final commits; merge to `main` is the only remaining step before
tagging v0.1.0.

| Sprint | Status | Commit(s) |
|--------|--------|-----------|
| S1 — Daemon + PTY | ✅ done | landed pre-branch |
| S2 — Blocks + persistence | ✅ done | landed pre-branch |
| S3 — Multi-pane + reattach | ✅ done | landed pre-branch |
| S4 — TUI dogfood (MVP) | ✅ done | `3ae7301` (pager, search jump-to-pane, smoke walkthrough; S5.1 absorbed architectural acceptance) |
| S5 — Agent multiplexer | ✅ done | `6e207b2` |
| S5.1 — Agent ops hardened | ✅ done | `6e207b2` + follow-ups |
| S6.1 — pyre-gpu viewer | ✅ done | `6e207b2` |
| S6.2 — GPU tiling | ✅ done | `53354b4` |
| S7 — Risk closure | ✅ done | `7a5f683` Tantivy facet + v2, `914e6d4` libproc, `5036d1d` push events, `7f85a5a` hybrid StateChanged |

Next action: cut v0.1.0 release tag after merge to main.

---

## Sprints

| Sprint | Goal | Key deliverables |
|--------|------|------------------|
| **S0** | Bootstrap | Workspace layout, crate skeletons (`pyre-proto`, `pyred`, `pyrec`, `pyre-tui`), CI scaffold, `SPEC.md`, `ARCHITECTURE.md`, `ROADMAP.md`, ADR-001 draft (IPC choice). |
| **S1** | Daemon + PTY | `pyred` spawns and owns a PTY via `portable-pty`, streams raw bytes to one `pyrec` over UDS, basic `Spawn` / `Attach` / `Detach` / `Kill`. No persistence yet. |
| **S2** | Blocks + persistence | Integrate `alacritty_terminal` parser, recognise OSC 133, materialise `Block` records, write to SQLite + zstd stdout blobs, basic `pyrec list` and `pyrec search` (linear scan). |
| **S3** | Multi-pane + reattach | Multiple panes per session, multiple sessions, full reattach with grid snapshot + last N blocks replay, multi-client mirror mode with serialized input. Hybrid supervisor + per-session workers landed (ADR-002 Accepted, 2026-05-19) — unblocks per-session isolation and clean reattach across daemon restarts; remaining S3 work tracks the multi-client + replay surface. |
| **S4** | TUI dogfood (**MVP**) | `pyre-tui` (ratatui) replaces tmux+alacritty in daily use: tabs, splits, block ribbon, scrollback navigation, Tantivy-backed search UI. **MVP criterion met.** |
| **S5** | **Agent multiplexer** | Hybrid profile docs; `AgentKind` detection; sidebar rollup + seen/done UX; `wait_pane_state` RPC; `pyrec` orchestration (`wait-pane`, `pane read`, `pane run`); MCP `wait_pane_state` + `pane://` / `block://` resources; `docs/AGENTS.md` playbook; doc hygiene (pyre as standalone product). |
| **S6** | GPU render | **S6.1 landed:** `pyre-gpu` glyph atlas, Ctrl+Tab multi-pane, **Ctrl+/ block search** (`!` = failures), ADR-003. **S6.2 landed:** real multi-pane tiling with Ctrl+w keybindings. |
| **S5.1** | Agent ops hardened | `proto_version` handshake; hybrid replay snapshot; macOS `ps` agent detect + `osascript` notify; `pyrec doctor`; search `failures_only` / TUI `!` prefix; `integration install` scripts; `pyrec select-pane` + TUI focus file. |
| **S7** | Risk closure | Tantivy native exit_code facet + schema v2 migration; libproc on macOS (replaces `ps` shell-out); broadcast `next_pane_event` push RPC (replaces 1 s polling); hybrid `StateChanged` emitted through supervisor. |

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

| Risk | Status | Mitigation / Resolution |
|------|--------|-------------------------|
| ANSI parser is a time sink. | ✅ closed | Reuse `alacritty_terminal`. Do NOT reimplement. Wrap, don't fork. |
| IPC schema churn S0–S2. | ✅ closed | `proto_version: u32` on every message from day one; integration test that rejects mismatched versions. |
| Scope creep — agent integrations. | ✅ closed | S5 limits install hooks to a small set of binaries; everything else uses heuristic `AgentKind` detection. |
| `portable-pty` quirks on Linux distros. | ✅ closed | Linux-first; Windows code paths `#[cfg]`-gated and untested until post-S6. |
| SQLite write contention with high-frequency Block events. | ✅ closed | WAL mode; batch BlockEvent writes per pane on a 50 ms tick. |
| GPU renderer rewrite temptation. | ✅ closed | ADR-003 forces a binary swap, not a rewrite. `pyre-gpu` consumes the same `pyre-proto` streams as `pyre-tui`. |
| Tantivy facet + schema v2 migration. | ✅ closed | `7a5f683` — native exit_code facet, v2 index with migration path. |
| macOS `ps` shell-out latency. | ✅ closed | `914e6d4` — libproc on macOS replaces subprocess. |
| Pane state polling (1 s tick). | ✅ closed | `5036d1d` — broadcast `next_pane_event` push RPC. |
| Hybrid supervisor StateChanged not propagated. | ✅ closed | `7f85a5a` — StateChanged emitted through supervisor. |

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
| GPU render | `pyre-gpu` — multi-pane tiling, glyph atlas, wgpu (ADR-003) |
