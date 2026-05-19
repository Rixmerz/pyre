# pyre — Claude Code handoff

This file is the entry point for any Claude Code session resuming work
on `pyre`. Read it first, then follow the links below.

## Next step

S1 (daemon + PTY) and S2 (Blocks + persistence) have landed. The
active surface is **S3 — multi-pane + reattach**, now unblocked by
the hybrid daemon (ADR-002 Accepted, 2026-05-19): supervisor on
`pyre.sock` + per-session workers on `pyre/session-<id>.sock`,
selectable via `pyred.process_model = "single" | "hybrid"`.

Current work front:

- Drive the TUI dogfood loop on hybrid: multi-pane within a session,
  multi-session switching, scrollback + block ribbon polish.
- Harden reattach: grid snapshot + last-N block replay over the
  proxied stream-mode (`0x02`) path.
- Multi-client mirror mode with serialized input through the
  supervisor → worker proxy.
- S4 (TUI dogfood / MVP criterion) follows once S3 stabilises.

Hybrid is opt-in; `"single"` remains the default for v0.1.0. AI/MCP
work is still blocked until S5.

## Files to read first

1. [SPEC.md](SPEC.md) — full feature surface, glossary, IPC methods,
   Block model, security stance.
2. [ARCHITECTURE.md](ARCHITECTURE.md) — crate map, process diagram,
   Block lifecycle, decision points.
3. [ROADMAP.md](ROADMAP.md) — sprint table, MVP criterion, risks,
   catalog-overlap verification.
4. [docs/adr/ADR-001-ipc.md](docs/adr/ADR-001-ipc.md) — the
   `tonic` vs `tarpc` decision record.
5. [docs/adr/0002-daemon-process-architecture.md](docs/adr/0002-daemon-process-architecture.md) —
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
- **No framework coinage.** `pyre` is a product. Before introducing a
  new abstraction with a name, check against `jig`, `schedule-mcp`,
  MEMI, `commit-guardian`, `delta-cube` (see ROADMAP catalog-overlap
  table) and prefer fusion to a new name.

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
- **AI / MCP temptation** — explicitly blocked until S5. Do not merge
  any `mcp::` module into `main` before S4 ships. The MVP is the
  terminal, not the agent integration.

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
