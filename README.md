# pyre

Daemon-owned terminal multiplexer with block-level history, full-text search, and agent observability.

[![status](https://img.shields.io/badge/status-S6%20production--ready-green)]()
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)]()

## Features

- **Block model (OSC 133)** — every command is a first-class `Block`: command string, cwd, stdout, exit code, timestamps; persisted to SQLite and indexed in Tantivy.
- **Persistent sessions** — `pyred` owns every PTY; client crashes and SSH drops do not kill the session. Reattach and restore scrollback + block history instantly.
- **Multi-pane with mirror** — split the terminal into independent panes within a session; the `pyre` TUI renders them in a ratatui grid.
- **Full-text search** — Tantivy indexes all block output; `pyrec search <query>` returns ranked hits with snippets in milliseconds.
- **Agent state monitoring** — a dedicated state tracker classifies each pane as idle, running, waiting for input, or error; exposed to AI agents via the MCP server.
- **MCP server (`pyre-mcp`)** — exposes sessions, panes, and blocks as MCP resources; tools like `jig` can read terminal output and drive panes programmatically.
- **Mouse-first TUI (Ember theme)** — `pyre` renders with ratatui + crossterm; Ember palette (dark amber/orange on near-black), mouse click-to-focus, scroll with wheel.
- **tmux-compatible CLI** — `pyrec` accepts `list-sessions`, `new-session`, `kill-session`, `send-keys`, `split-window`, and more; scripts that drive tmux need minimal changes.
- **Clipboard integration** — `pyrec capture-pane --copy` copies output to the system clipboard via `wl-copy` (Wayland) or `xclip` (X11).
- **Searchable scrollback** — ring buffer per pane with configurable depth; `PgUp`/`PgDn` in `pyre` or `capture-pane -S` in pyrec.

## Quickstart

### 1. Build

```sh
git clone https://github.com/<TODO>/pyre.git
cd pyre
cargo build --release
# binaries: target/release/{pyred,pyrec,pyre,pyre-mcp}
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

### 3. Spawn your first session

```sh
# Open pyre (recommended for interactive use)
pyre

# Or use pyrec directly
pyrec              # spawn + attach default shell
pyrec sessions     # list active sessions
pyrec list         # list recent blocks
```

## Key bindings (pyre)

Prefix: `Ctrl-B`

| Keys | Action |
|------|--------|
| `Ctrl-B c` | New pane in current session |
| `Ctrl-B n` | Next pane |
| `Ctrl-B p` | Previous pane |
| `Ctrl-B "` | Horizontal split |
| `Ctrl-B %` | Vertical split |
| `Ctrl-B q` | Close current pane |
| `Ctrl-B y` | Copy scrollback to clipboard |
| `Ctrl-B z` | Zoom (toggle fullscreen) current pane |
| `Ctrl-B s` | Search blocks (opens Tantivy query dialog) |
| `Ctrl-B [` | Enter scroll mode |
| `Ctrl-B ]` | Exit scroll mode |
| `PgUp` / `PgDn` | Scroll up / down in scroll mode |
| Arrow keys | Move focus between panes |
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

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full process diagram, crate map, block lifecycle, and data flow.

## Documentation

| File | Contents |
|------|----------|
| [docs/USAGE.md](docs/USAGE.md) | All subcommands, tmux mapping table, TUI bindings, troubleshooting |
| [docs/CONFIG.md](docs/CONFIG.md) | `hooks.toml` schema and future config knobs |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Crate map, process diagram, block lifecycle, state engine |
| [SPEC.md](SPEC.md) | Full feature specification and IPC method reference |
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
