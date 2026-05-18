# Changelog

All notable changes to `pyre` are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.1.0] - 2026-05-18

### Added

- `pyred` daemon: owns all PTYs via `portable-pty`; survives client disconnects.
- `pyrec` CLI client: `sessions`, `panes`, `attach`, `new-pane`, `list`, `search`,
  `capture-pane`, `kill-session`, `send-keys`, `split-window` + full tmux-compat
  alias set.
- `pyre-tui`: ratatui + crossterm multiplexed view; Ember theme (dark amber/orange);
  mouse-first navigation; Ctrl-B prefix key bindings; scroll mode.
- `pyre-mcp`: MCP server exposing sessions, panes, and blocks as resources and tools.
- `pyre-proto`: shared tarpc service definition (`PyreDaemon`), wire types, codec
  helpers. Versioned; mismatched clients are rejected.
- Block model (OSC 133 A/B/C/D): command, cwd, stdout, exit code, timestamps.
  Persisted to SQLite; indexed in Tantivy; hooks via `hooks.toml`.
- Full-text search: Tantivy index rebuilt from SQLite; `pyrec search` with ranked
  snippet results.
- Per-pane ring buffer (scrollback); `capture-pane` RPC.
- State engine: classifies pane as idle / running / waiting / error; heuristic
  based on OSC 133 + cursor position.
- ANSI/VT parser via `alacritty_terminal`; daemon ships parsed state to clients.
- Multi-session, multi-pane: unlimited sessions; panes can be opened/closed
  independently; pyre-tui renders a tab-per-session grid.
- Clipboard integration via `wl-copy` (Wayland) or `xclip` (X11).
- `dist/systemd/pyred.service` — systemd user unit.
- `dist/arch/PKGBUILD` — Arch Linux package.
- `dist/debian/` — Debian debhelper skeleton (compat 13).
- `dist/rpm/pyre.spec` — Fedora RPM spec.
- `Makefile` — `man` target builds pandoc man pages from `docs/man/src/`.
- `docs/man/src/{pyred,pyrec,pyre-tui,pyre-mcp}.md` — pandoc-friendly man page sources.
- `docs/USAGE.md`, `docs/CONFIG.md`, `docs/ARCHITECTURE.md` — full user and
  developer documentation.
- CI matrix: ubuntu-latest, fedora:latest, archlinux:latest — fmt + clippy +
  build + test on every PR.
- Release workflow: tagged `v*` builds produce tarball + checksums + `.deb` + `.rpm`.
- `release-prod` Cargo profile: strip + fat LTO + `opt-level = 3` + single codegen unit.
- Integration smoke test (`prod_smoke.rs`): full session lifecycle — spawn, open
  pane, send-keys, capture, list, close.

### Changed

- ADR-001 resolved: tarpc chosen over tonic for IPC (native Rust types, lower
  ceremony, no protobuf toolchain dependency).
- Socket at `$XDG_RUNTIME_DIR/pyre.sock`, mode 0700; `PYRE_DATA_DIR` overrides
  the data directory for tests.

### Removed

- No public API removed in 0.1.0 (first release).
