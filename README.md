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
- **MCP server (`pyre-mcp`)** — 19 tools including `session_spawn`, `pane_run_command` (send + wait + exit code in one call), `pane_last_block`, `block_search`, `session_layout`, `open_pane_split`, `wait_pane_state`, and more. Sessions, panes, and blocks are also exposed as MCP resources (`pane://`, `block://`, `state://panes`). See [docs/AGENTS.md](docs/AGENTS.md) for the full tool table.
- **Shell integration (OSC 133)** — `eval "$(pyrec shell-init bash)"` (or `zsh`/`fish`) installs shell hooks that segment output into blocks with exit codes and command text. Required for search, exit-code badges, and `pane_run_command` completion detection.
- **Mouse-first TUI** — ratatui + crossterm, mouse click-to-focus, scroll wheel. Ships with 18 built-in palettes via the `pyre-themes` registry; switch live with `Ctrl-Space T`.
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

Prefix: `Ctrl-Space`

| Keys | Action |
|------|--------|
| `Ctrl-Space q` | Quit / detach |
| `Ctrl-Space c` | New pane in current session |
| `Ctrl-Space x` | Close current pane |
| `Ctrl-Space n` | Next tab |
| `Ctrl-Space p` | Previous tab |
| `Ctrl-Space "` | Horizontal split |
| `Ctrl-Space %` | Vertical split |
| Arrow keys | Move focus between panes |
| `Ctrl-Space [` | Enter scrollback mode |
| `Ctrl-Space ]` | Exit scrollback mode |
| `Ctrl-Space /` | Search blocks (Tantivy query dialog) |
| `Ctrl-Space z` | Zoom (toggle fullscreen) current pane |
| `Ctrl-Space y` | Copy last block stdout to clipboard |
| `Ctrl-Space s` | Toggle sidebar |
| `Ctrl-Space S` | New session |
| `Ctrl-Space T` | Open theme picker overlay (live switch across 18 palettes) |
| `Ctrl-Space N` | Toggle toast notifications on/off |
| `PgUp` / `PgDn` | Scroll in scrollback mode |
| Mouse click | Focus pane under cursor |
| Mouse wheel | Scroll output |

## Quickstart smoke test (10 min)

Exercises daemon, TUI, `pyrec`, and search end-to-end on a single machine.
Assumes binaries are built and `pyred` is not already running.

1. `pyred &` — start the daemon in single-process mode (default). Use `pyred --config <path>` with `process_model = "hybrid"` for the supervisor model.
2. `pyre` — launch the TUI; a fresh session opens automatically (the daemon spawns a default pane).
3. `Ctrl-Space "` — horizontal split; a second pane appears below the first.
4. Type `echo hello && sleep 1 && echo done` in the lower pane and press Enter; wait for the block to finish (exit badge `b<id>` appears in the ribbon).
5. `Ctrl-Space [` — enter block ribbon mode on the focused pane; `←`/`→` (or `h`/`l`) move the cursor between recent blocks.
6. Press `Enter` on a block — the modal pager opens showing the full stdout. `↑`/`↓` or `PgUp`/`PgDn` scroll; `q` or `Esc` closes.
7. In a second terminal: `pyrec search "hello"` — confirm the block is indexed (output includes a snippet line).
8. Back in the TUI: `Ctrl-Space /` — open the search overlay; type `hello`; `↑`/`↓` navigate hits; `Enter` jumps focus to the source pane and highlights the block in the ribbon.
9. `Ctrl-Space y` — copy the last block's stdout to the clipboard (requires `wl-copy` on Wayland or `xclip` on X11).
10. `pyrec kill-session <session-id>` (UUID prefix accepted) — close the session cleanly; `pyrec sessions` should return an empty list.

Note: `pyrec wait-pane --pane <pane-id> --state waiting` can be used in step 5 instead of watching the TUI — it returns as soon as the pane transitions to `WaitingInput` (or the shell prompt reappears with OSC 133 integration installed).

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

## Themes

`pyre-tui` and `pyre-gpu` consume the shared `pyre-themes` registry. 18 built-in
palettes ship today:

`catppuccin-mocha`, `catppuccin-latte`, `tokyo-night`, `tokyo-night-light`,
`gruvbox-dark`, `gruvbox-light`, `one-dark`, `one-light`, `solarized-dark`,
`solarized-light`, `kanagawa`, `rose-pine`, `rose-pine-dawn`, `vesper`, `nord`,
`dracula`, `terminal`, `ember`.

- Open the live picker with `Ctrl-Space T` in `pyre` — `↑`/`↓` (or `j`/`k`) navigate,
  `Enter` applies and persists, `Esc` cancels.
- Set the startup default in `$XDG_CONFIG_HOME/pyre/config.toml`:

  ```toml
  [ui]
  theme = "catppuccin-mocha"
  ```

- `pyre-gpu` reads the same `[ui] theme` key on startup; live switch lands in
  a later commit (restart required for now).

See [docs/CONFIG.md](docs/CONFIG.md) for the full `[ui]` schema.

## Notifications

Pane lifecycle events (spawn, close, state change to `WaitingInput` / `Done` /
`Crashed`) surface as toast cards rendered bottom-right of the TUI. Idle and
running transitions are suppressed to avoid spam.

- Toggle on/off at runtime with `Ctrl-Space N`.
- Configure defaults under `[ui.notifications]` in `config.toml` (see
  [docs/CONFIG.md](docs/CONFIG.md)).

M2 of the v0.2 UX sprint is in flight: desktop bridge (`notify-send` / D-Bus
on Linux, `osascript` on macOS) and per-kind routing rules are pending. Today
the toast deck is in-TUI only.

## Remote attach (M5, pending)

`pyred` binds a Unix Domain Socket and does not listen on TCP. For laptop ↔
remote-server attach, `pyrec remote <host>` derives the local + remote socket
paths and prints (or with `--exec` runs) an `ssh -L` tunnel command. The
local `pyre --socket <path>` then attaches as if pyred were local — zero
daemon changes, SSH owns auth + transport.

```sh
pyrec remote alice@dev.box                     # prints the ssh -L command
pyrec remote alice@dev.box --exec              # runs the tunnel in foreground
pyrec remote alice@dev.box --remote-socket /run/user/1000/pyre.sock
```

The wire-format decision (SSH tunnel for v0.2, native TLS for v0.3) is recorded
in [docs/adr/0004-remote-attach.md](docs/adr/0004-remote-attach.md) (Proposed).
Implementation polish (auto-detection of `XDG_RUNTIME_DIR` on the remote,
keepalive defaults, reconnect) is pending.

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
