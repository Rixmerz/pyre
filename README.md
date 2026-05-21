# pyre

Daemon-owned terminal multiplexer with block-level history, full-text search, and agent observability.

[![status](https://img.shields.io/badge/status-S3%20multi--pane%20%2B%20reattach-green)]()
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)]()

## Features

- **Block model (OSC 133)** — every command is a first-class `Block`: command string, cwd, stdout, exit code, timestamps. OSC 133 markers feed `BlockParser` → `BlobWriter` → SQLite + Tantivy. `pyrec list` and `pyrec search` query the index.
- **Persistent sessions** — `pyred` owns every PTY; client crashes and SSH drops do not kill the session. SQLite store survives daemon restart and the supervisor reattaches workers on init.
- **Multi-client mirror** — N TUIs can attach to the same pane simultaneously. Output is broadcast to all attached clients; input is serialized through the daemon.
- **Reattach with replay** — restarting a `pyre` TUI restores the grid snapshot from the per-pane ring buffer and replays the last 20 blocks via the `replay` RPC.
- **Hybrid daemon (ADR-002)** — selectable process model: a single `pyred` (default for v0.1.0) or a thin supervisor on `pyre.sock` plus per-session worker processes on `pyre/session-<id>.sock`. Worker crash kills only its session.
- **Full-text search** — Tantivy indexes all block output; `pyrec search <query>` returns ranked hits with snippets in milliseconds.
- **Agent state monitoring** — a dedicated state tracker classifies each pane as idle, running, waiting for input, or error; exposed via the MCP server.
- **MCP server (`pyre-mcp`)** — seven tools: `session_spawn`, `session_close`, `pane_open`, `pane_send_keys`, `pane_capture`, `pane_set_state`, `block_search`. Sessions, panes, and blocks are also exposed as MCP resources.
- **Mouse-first TUI (Ember theme)** — ratatui + crossterm, Ember palette (amber on near-black), mouse click-to-focus, scroll wheel.
- **Pyre fire motion** — startup splash and in-TUI accents use a shared procedural fire engine (`fire_motion.rs`): no animation libraries, no sprite packs. Blocked agents pulse like embers on the same palette as the launch animation.
- **tmux-compatible CLI** — `pyrec` accepts `list-sessions`, `new-session`, `kill-session`, `send-keys`, `split-window`, and more.
- **Clipboard integration** — `pyrec capture-pane --copy` copies output to the system clipboard via `wl-copy` (Wayland) or `xclip` (X11).
- **Searchable scrollback** — ring buffer per pane with configurable depth; `PgUp`/`PgDn` in `pyre` or `capture-pane -S` in `pyrec`.

## Quickstart

### 1. Build

```sh
git clone https://github.com/Rixmerz/pyre.git
cd pyre
cargo build --release
# binaries: target/release/{pyred,pyrec,pyre,pyre-gpu,pyre-mcp}
```

For a size- and performance-optimised binary use the `release-prod` profile:

```sh
cargo build --profile release-prod --workspace
```

### 2. Enable the systemd user unit

```sh
sudo install -Dm755 target/release/pyred /usr/bin/pyred
sudo install -Dm644 dist/systemd/pyred.service /usr/lib/systemd/user/pyred.service
systemctl --user daemon-reload
systemctl --user enable --now pyred
```

To opt into the hybrid supervisor/worker model, set `process_model = "hybrid"`
in your `pyred` config (see [docs/CONFIG.md](docs/CONFIG.md)). Default is
`"single"` for v0.1.0; see [docs/adr/0002-daemon-process-architecture.md](docs/adr/0002-daemon-process-architecture.md).

### 3. Spawn your first session

```sh
# Open pyre (recommended for interactive use)
pyre

# Or use pyrec directly
pyrec              # spawn + attach default shell
pyrec sessions     # list active sessions
pyrec list         # list recent blocks
```

### Multi-agent quickstart (hybrid)

With `process_model = "hybrid"` in config (see [docs/CONFIG.md](docs/CONFIG.md)):

```sh
pyrec session-new --name api --cwd ~/projects/api -d
pyrec session-new --name web --cwd ~/projects/web -d
pyre   # sidebar shows per-pane blocked/working + session rollup
```

From scripts or an MCP client: `pyrec wait-pane --pane <id> --state waiting`,
`pyrec pane read --pane <id> --source block-last`. Playbook:
[docs/AGENTS.md](docs/AGENTS.md).

### GPU viewer (S6 Phase 1)

```sh
pyre-gpu   # windowed attach; Ctrl+/ search; Ctrl+Tab switch panes
pyrec doctor
```

See [docs/adr/0003-render-backend.md](docs/adr/0003-render-backend.md).

## Key bindings (pyre)

Prefix: `Ctrl-B`

| Keys | Action |
|------|--------|
| `Ctrl-B q` | Quit / detach |
| `Ctrl-B c` | New pane in current session |
| `Ctrl-B x` | Close current pane |
| `Ctrl-B n` | Next tab |
| `Ctrl-B p` | Previous tab |
| `Ctrl-B "` | Horizontal split |
| `Ctrl-B %` | Vertical split |
| Arrow keys | Move focus between panes |
| `Ctrl-B [` | Enter scrollback mode |
| `Ctrl-B ]` | Exit scrollback mode |
| `Ctrl-B /` | Search blocks (Tantivy query dialog) |
| `Ctrl-B z` | Zoom (toggle fullscreen) current pane |
| `Ctrl-B y` | Copy last block stdout to clipboard |
| `Ctrl-B s` | Toggle sidebar |
| `Ctrl-B S` | New session |
| `PgUp` / `PgDn` | Scroll in scrollback mode |
| Mouse click | Focus pane under cursor |
| Mouse wheel | Scroll output |

## pyrec basics

```sh
# Spawn a new session with zsh
pyrec --shell /bin/zsh

# List sessions
pyrec sessions

# Attach to an existing session (UUID prefix accepted)
pyrec attach <session-id>

# Open a second pane in a session
pyrec new-pane --session <session-id>

# Send keys to a pane (tmux-compat)
pyrec send-keys --session <session-id> --pane <pane-id> -- "ls -la\n"

# Capture last 40 lines of a pane
pyrec capture-pane --session <session-id> --pane <pane-id> --lines 40

# Full-text search across all block stdout
pyrec search "cargo error"

# Kill a session
pyrec kill-session <session-id>
```

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full process diagram, crate
map, block lifecycle, and data flow. The hybrid supervisor/worker layout
is specified in [docs/adr/0002-daemon-process-architecture.md](docs/adr/0002-daemon-process-architecture.md).

## Documentation

| File | Contents |
|------|----------|
| [docs/USAGE.md](docs/USAGE.md) | All subcommands, tmux mapping table, TUI bindings, troubleshooting |
| [docs/CONFIG.md](docs/CONFIG.md) | `hooks.toml` schema, `process_model` flag, future config knobs |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Crate map, process diagram, block lifecycle, state engine |
| [SPEC.md](SPEC.md) | Full feature specification and IPC method reference |
| [docs/adr/0002-daemon-process-architecture.md](docs/adr/0002-daemon-process-architecture.md) | Hybrid supervisor/worker decision |
| [ROADMAP.md](ROADMAP.md) | Sprint table, MVP criterion, risks |
| [CHANGELOG.md](CHANGELOG.md) | Release history |

## Contributing

1. Fork the repo and create a feature branch.
2. Run `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` before pushing.
3. All tests must pass: `cargo test --workspace`.
4. Open a PR with a description of what changed and why.

Commit style: [Conventional Commits](https://www.conventionalcommits.org/) with a `Why:` body line.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-Apache) at your option.
