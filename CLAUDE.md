# pyre — Claude Code handoff

This file is the entry point for any Claude Code session resuming work
on `pyre`. Read it first, then follow the links below.

## Next step

**v0.4.0 stabilization sprint complete.** Key deliverables:

- god-file split: `pyre-tui/src/main.rs` reduced from 6000+ lines to 204 lines; logic extracted into `app/`, `render/`, `input/`, `rpc/`, `model/` modules across ~15 commits.
- PaneId/SessionId newtypes workspace-wide (commit e8259de) — type-safe IDs throughout all crates.
- `docs/INVARIANTS.md` codifying 8 architectural invariants for the TUI state machine.
- TUI test harness scaffolded: `tests/render_smoke.rs` + `tests/close_pane_dispatch.rs` (commit 4833a06).
- P5 bugs fixed: close-X real root cause was `LayoutNode::close` on a single-leaf tree producing a zombie session; fixed in commit 19cb49b. Also fixed: close-X hit rect (7d8080b), stale session filter (c4c71ee), `pick_attach_session` empty-list auto-spawn (cc67ab6).
- 36 RFs (requirements) tracked via livespec at `.claude/notions/refactor-plan-v04.md`.

v0.4.0 release tag cut. See [ROADMAP.md](ROADMAP.md) `## v0.4 — stabilization sprint` for full commit table.
v0.3.0 surface (M7-A..F), v0.2.0 surface (M1..M5), and v0.1.0 surface (S1..S7) remain complete and documented below.

## Files to read first

1. [SPEC.md](SPEC.md) — full feature surface, glossary, IPC methods,
   Block model, security stance.
2. [ARCHITECTURE.md](ARCHITECTURE.md) — crate map, process diagram,
   Block lifecycle, decision points.
3. [ROADMAP.md](ROADMAP.md) — sprint table, v0.1.0 banner, risks (all closed).
4. [docs/AGENTS.md](docs/AGENTS.md) — agent multiplexer playbook.
5. [docs/adr/ADR-001-ipc.md](docs/adr/ADR-001-ipc.md) — IPC decision record.
6. [docs/adr/0002-daemon-process-architecture.md](docs/adr/0002-daemon-process-architecture.md) —
   single vs hybrid daemon (Accepted 2026-05-19).
7. [docs/adr/ADR-003.md](docs/adr/ADR-003.md) — GPU renderer binary-swap
   contract (`pyre-gpu` / `pyre-tui` parity surface).

## Project rules

- **Rust toolchain**: stable. No nightly features. `rustfmt` + `clippy
  -D warnings` are CI gates.
- **Commits**: Conventional Commits with a `Why:` body. Always include
  `Co-Authored-By: Claude <noreply@anthropic.com>` when Claude
  participated.
- **No `git push` to `main` without an explicit `!` prefix from the
  user.** The sandbox blocks it; do not work around the block.
- **Backup before destructive ops**: `git push --force`, schema
  resets, `cargo clean` on shared state, anything that nukes
  `state.db` or the Tantivy index. Use a `*.bak.YYYYMMDD-HHMMSS`
  suffix.
- **Linux-first.** Windows paths are `#[cfg]`-gated and untested.
  Do not invest in cross-platform polish before S6.
- **No telemetry, no Electron, no cloud default.** These are
  identity-level constraints, not preferences.
- **No framework coinage.** `pyre` is a standalone product. Prefer
  extending existing pyre surfaces (blocks, MCP, hooks) over new
  named abstractions.

## Expected blockers

- **`portable-pty` on Windows** — ignore. Linux first. Do not chase
  Windows test failures during S0–S6.
- **`tonic` vs `tarpc`** — undecided. Both work over UDS in Rust;
  `tonic` gives gRPC tooling + a forced schema language (protobuf),
  `tarpc` gives native Rust types + lower ceremony. Decide in
  ADR-001, do not start writing IPC code until then.
- **ANSI parser scope guard** — if you find yourself touching
  `alacritty_terminal` internals or reading the VT100 spec, stop.
  Wrap, don't fork. Open an issue instead of patching.
- **Scope guard** — S5 agent work stays on detection, orchestration,
  and MCP; no remote thin client, no GPU renderer, no nested tab model.

## Workflow

- Once an S1 graph exists, run `graph_activate pyre-s1` and let the
  workflow drive phases. Until then, follow the S1 deliverables in
  `ROADMAP.md` directly.
- Use `experience_query(file_path=…)` before touching unfamiliar
  files — there may already be a lesson recorded.
- Reindex on edit (livespec `index_project` + DCC `cube_reindex`) as
  required by `.claude/rules/auto-delegation.md`.
- When context pressure crosses HUD thresholds, persist with
  `next_task_record` and rotate via `tmux_clear_and_prompt` — never a
  bare `/clear`.
