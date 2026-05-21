# pyre — Claude Code handoff

This file is the entry point for any Claude Code session resuming work
on `pyre`. Read it first, then follow the links below.

## Next step

S1 (daemon + PTY) and S2 (Blocks + persistence) have landed. The
active surface is **S3 — multi-pane + reattach**, now unblocked by
the hybrid daemon (ADR-002 Accepted, 2026-05-19): supervisor on
`pyre.sock` + per-session workers on `pyre/session-<id>.sock`,
selectable via `pyred.process_model = "single" | "hybrid"`.

**S5 — Agent multiplexer** has landed (detection, `wait_pane_state`, pyrec/MCP
orchestration, sidebar rollup, [docs/AGENTS.md](docs/AGENTS.md)).

**S5.1 — Agent ops hardened** landed (`proto_version`, hybrid replay snapshot,
`pyrec doctor`, search failures filter, integration scripts).

**S6.1** landed: `pyre-gpu` glyph atlas, Ctrl+Tab multi-pane, Ctrl+/ block search;
`pyrec select-pane` writes `focus.request` for the TUI.

Hybrid is opt-in; `"single"` remains the default for v0.1.0. Dogfood hybrid on
Linux with `process_model = "hybrid"` when validating S5 criteria.

## Files to read first

1. [SPEC.md](SPEC.md) — full feature surface, glossary, IPC methods,
   Block model, security stance.
2. [ARCHITECTURE.md](ARCHITECTURE.md) — crate map, process diagram,
   Block lifecycle, decision points.
3. [ROADMAP.md](ROADMAP.md) — sprint table, MVP criterion, risks.
4. [docs/AGENTS.md](docs/AGENTS.md) — agent multiplexer playbook.
5. [docs/adr/ADR-001-ipc.md](docs/adr/ADR-001-ipc.md) — the
   `tonic` vs `tarpc` decision record.
6. [docs/adr/0002-daemon-process-architecture.md](docs/adr/0002-daemon-process-architecture.md) —
   single vs hybrid daemon (Accepted 2026-05-19, hybrid implemented
   behind `pyred.process_model = "hybrid"`).

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
