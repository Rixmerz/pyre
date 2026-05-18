# pyre

> Persistent terminal, process-first.

[![status](https://img.shields.io/badge/status-S0%20bootstrap-orange)]()

## Vision

`pyre` is a Linux-first terminal emulator built around a persistent
daemon (`pyred`) that owns PTYs and survives client disconnects, paired
with a thin CLI client (`pyrec`) and a TUI renderer (`pyre-tui`) today —
GPU renderer (`pyre-gpu`) later. It replaces the `tmux + alacritty`
combo with a single process-first model: every command is a `Block`
(Warp-style, keyed by OSC 133), persisted to SQLite, searchable via
Tantivy, scriptable in Lua, and exposed to AI agents through MCP so
panes become first-class resources for tools like `jig`. No Electron.
No telemetry. No cloud default.

## Install / build

```sh
git clone https://github.com/rixmerz/pyre.git
cd pyre
cargo build --release
```

Binaries land in `target/release/`: `pyred`, `pyrec`, `pyre-tui`.

## Quickstart

```sh
pyred &                  # start the daemon (UDS at $XDG_RUNTIME_DIR/pyre.sock)
pyrec attach             # attach a client; spawns default shell in a new pane
pyrec list               # list sessions/panes the daemon owns
pyrec detach             # detach without killing the pane
```

The daemon keeps every PTY alive across client crashes, SSH drops, and
laptop suspends. Reattaching restores the scrollback, the block
history, and the cursor state.

## Status

Sprint S0 — bootstrap. Crate skeletons + protocol scaffolding in
progress. See [ROADMAP.md](ROADMAP.md) for the S0–S6 plan; MVP lands at
the end of S4 (TUI dogfood replaces `tmux + alacritty` in daily use).

## Docs

- [SPEC.md](SPEC.md) — full feature specification.
- [ARCHITECTURE.md](ARCHITECTURE.md) — crate map and dataflow.
- [ROADMAP.md](ROADMAP.md) — sprints, deliverables, risks.
- [CLAUDE.md](CLAUDE.md) — handoff notes for Claude Code sessions.

## License

Dual-licensed under MIT or Apache-2.0 at your option.
