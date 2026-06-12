# pyre — Usage Guide

## Shell integration (OSC 133)

pyre segments terminal output into **blocks** using OSC 133 markers emitted by
the shell. Blocks carry the command text, exit code, and stdout — they power
`pyrec search`, `pyrec list`, exit-code badges in the TUI ribbon, and the MCP
`pane_run_command` / `pane_last_block` tools.

**Without shell integration** pyre still works as a terminal multiplexer with
searchable scrollback, but no blocks are created: search returns no results,
exit codes are unavailable, and `pane_run_command` cannot detect command
completion.

### One-line install

```sh
# bash — add to ~/.bashrc
eval "$(pyrec shell-init bash)"

# zsh — add to ~/.zshrc
eval "$(pyrec shell-init zsh)"

# fish — add to ~/.config/fish/config.fish
pyrec shell-init fish | source
```

The script is idempotent: re-sourcing it (e.g. on shell reload) is safe.
The guard variable `PYRE_SHELL_INTEGRATION=1` prevents double-registration.

### What the hooks emit

| Hook | Markers emitted | Parser effect |
|------|-----------------|---------------|
| `precmd` / `fish_prompt` | `D;<exit>` (if a prior command ran), then `A` | Close previous block with exit code; start command-text capture |
| `preexec` / `fish_preexec` | `C` | Snapshot captured command text → open new block |

The first-command guard (`PYRE_CMD_STARTED`) prevents a stray `D` on the very
first prompt before any command has run.

### What works without it

| Feature | Without integration | With integration |
|---------|---------------------|------------------|
| PTY output / scrollback | Yes | Yes |
| Block history (`pyrec list`) | No | Yes |
| Full-text search (`pyrec search`) | No | Yes |
| Exit codes in TUI ribbon | No | Yes |
| `pane_run_command` exit code | No | Yes |
| `pane_last_block` | No | Yes |

### Troubleshooting shell integration

If `pyrec list` returns no blocks after running commands:

1. Confirm the hook is active: `echo $PYRE_SHELL_INTEGRATION` should print `1`.
2. Check that `pyrec` is on your `PATH`: `which pyrec`.
3. Verify pyre is connected to the daemon: `pyrec doctor`.
4. Run a command and check `pyrec list` — blocks appear only after the command
   finishes (OSC 133 D fires in `precmd`, not during execution).

If you use a prompt framework (Starship, Powerlevel10k, Oh My Zsh), check
whether it already emits OSC 133 — loading both may double-register hooks.
Starship 1.16+ emits OSC 133 natively; in that case, skip `shell-init`.

---

## pyrec subcommands

All subcommands accept `--socket <path>` to override the default UDS path
(`$XDG_RUNTIME_DIR/pyre.sock`).

### Interactive attach (default, no subcommand)

```sh
pyrec                          # spawn new session + pane, attach stdin/stdout
pyrec --shell /bin/zsh         # use zsh instead of $SHELL
pyrec --socket /tmp/pyre.sock  # connect to a non-default socket
```

### sessions

List all sessions the daemon currently owns.

```sh
pyrec sessions
# Output: one line per session — UUID  name  pane-count  created-at
```

### panes

List panes within a session.

```sh
pyrec panes <session-id>
pyrec panes 3f2a1b              # ≥8-char UUID prefix accepted
```

### attach

Attach to an existing session (and optionally a specific pane).

```sh
pyrec attach <session-id>
pyrec attach <session-id> --pane <pane-id>
```

### new-pane

Open a new pane in a session without attaching.

```sh
pyrec new-pane --session <session-id>
pyrec new-pane --session <session-id> --shell /bin/fish --cwd /tmp
```

### list

List recent blocks (command history) across all sessions or one session.

```sh
pyrec list                        # last 20 blocks, all sessions
pyrec list --session <id> --n 50  # last 50 blocks in one session
```

### search

Full-text search across block stdout via Tantivy.

```sh
pyrec search "cargo error"
pyrec search "permission denied" --limit 10
```

Output: ranked hits with block id, command, exit code, and a stdout snippet.

### capture-pane

Capture the last N lines from a pane's ring buffer.

```sh
pyrec capture-pane --session <id> --pane <id>           # last 40 lines (default)
pyrec capture-pane --session <id> --pane <id> --lines 100
pyrec capture-pane --session <id> --pane <id> --copy    # also write to clipboard
```

### send-keys

Write raw bytes to a pane via the stream connection. Append `\n` to simulate Enter.

```sh
pyrec send-keys --session <id> --pane <id> -- "ls -la\n"
pyrec send-keys --session <id> --pane <id> -- $'\x03'   # Ctrl-C
```

### kill-session

Terminate a session and all its panes.

```sh
pyrec kill-session <session-id>
```

### split-window

Open a new pane in the same session (layout is managed by `pyre`).

```sh
pyrec split-window --session <id>
```

### display-message

Print a message to stderr (stub for tmux script compatibility).

```sh
pyrec display-message "hello from script"
```

---

## tmux compatibility mapping

| tmux command | pyrec equivalent |
|---|---|
| `tmux list-sessions` | `pyrec sessions` |
| `tmux new-session` | `pyrec` (interactive) or `pyrec new-pane` |
| `tmux kill-session -t <id>` | `pyrec kill-session <id>` |
| `tmux list-panes -t <session>` | `pyrec panes <session>` |
| `tmux list-windows -t <session>` | `pyrec panes <session>` |
| `tmux split-window -t <session>` | `pyrec split-window --session <id>` |
| `tmux send-keys -t <pane> "cmd" Enter` | `pyrec send-keys --session <s> --pane <p> -- "cmd\n"` |
| `tmux capture-pane -p -t <pane>` | `pyrec capture-pane --session <s> --pane <p>` |
| `tmux select-pane -t <pane>` | `pyrec select-pane -t <pane>` (requires running `pyre` TUI) |
| `tmux display-message "msg"` | `pyrec display-message "msg"` |
| `tmux attach-session -t <session>` | `pyrec attach <session>` |
| `tmux detach-client` | `Ctrl-Space d` in `pyre`, or close the pyrec process |

---

## pyre key bindings

### Prefix: `Ctrl-Space`

All bindings require pressing `Ctrl-Space` first, then the listed key.

#### Panes

| Binding | Action |
|---------|--------|
| `Ctrl-Space c` | New pane in current session (new tab) |
| `Ctrl-Space "` | Horizontal split (new pane below) |
| `Ctrl-Space %` | Vertical split (new pane right) |
| `Ctrl-Space x` | Close focused pane |
| `Ctrl-Space z` | Toggle zoom (fullscreen) on current pane |
| `Ctrl-Space →` / `↓` | Focus next pane |
| `Ctrl-Space ←` / `↑` | Focus previous pane |

#### Sessions and tabs

| Binding | Action |
|---------|--------|
| `Ctrl-Space n` | Next tab |
| `Ctrl-Space p` | Previous tab |
| `Ctrl-Space S` | New session (opens name prompt) |
| `Ctrl-Space ,` | Rename active session |
| `Ctrl-Space d` | Detach — exit TUI, leave daemon and sessions running; run `pyre` to reattach |
| `Ctrl-Space q` | Quit TUI (exits the event loop; daemon sessions keep running) |

Note: `q` and `d` are intentionally distinct. `d` prints a reattach hint to stdout after exit; `q` exits silently. Both leave the daemon running — neither kills panes.

#### Scrollback / blocks

| Binding | Action |
|---------|--------|
| `Ctrl-Space [` | Enter scrollback mode (block ribbon) |
| `Ctrl-Space ]` | Exit scrollback mode |
| `Ctrl-Space y` | Copy last block stdout to clipboard |

#### Overlays and misc

| Binding | Action |
|---------|--------|
| `Ctrl-Space /` | Open full-text block search (Tantivy) |
| `Ctrl-Space s` | Toggle sidebar (pane list) |
| `Ctrl-Space T` | Open theme picker (live preview, persists to config) |
| `Ctrl-Space N` | Toggle toast notifications on/off |
| `Ctrl-Space ?` | Show key binding help overlay (this list, in-TUI) |

### Scroll mode

| Key | Action |
|-----|--------|
| `PgUp` | Scroll up one screen |
| `PgDn` | Scroll down one screen |
| `↑` / `↓` | Scroll one line |
| `g` | Jump to top |
| `G` | Jump to bottom (exit scroll mode) |
| `q` | Exit scroll mode |

### Mouse actions

| Action | Effect |
|--------|--------|
| Left click on pane | Focus that pane |
| Scroll wheel up/down | Scroll pane output |
| Click block ribbon entry | Expand block detail |

---

## Agent orchestration (S5)

These commands work against a running `pyred` (single or hybrid). See
[AGENTS.md](AGENTS.md) for the multi-agent layout.

```sh
# Named session (hybrid: one worker)
pyrec session-new --name api --cwd ~/projects/api -d

# Wait until the agent is blocked on input (30s default)
pyrec wait-pane --pane <pane-prefix> --state waiting --timeout 30

# Read last command output (OSC 133 block) or ring buffer
pyrec pane read --pane <pane-prefix> --source block-last
pyrec pane read --pane <pane-prefix> --lines 80

# Run a command in a session
pyrec pane-run --session <session-prefix> -- echo hello

# Install a hook snippet for self-reporting pane state
pyrec integration install claude
```

Example: wait for work to finish, then read output:

```sh
pyrec wait-pane --pane abc12345 --state done --timeout 600
pyrec pane read --pane abc12345 --source block-last
pyrec search "error"
```

---

## Troubleshooting

### Daemon not starting

1. Check the socket path: `ls -la $XDG_RUNTIME_DIR/pyre.sock`
2. Inspect logs: `journalctl --user -u pyred -n 50`
3. Verify the binary is in `PATH`: `which pyred`
4. Check socket permissions (should be mode 0700): `stat $XDG_RUNTIME_DIR/pyre.sock`
5. If another instance is running: `systemctl --user restart pyred`

### Clipboard not working

- **Wayland**: install `wl-clipboard` (`sudo pacman -S wl-clipboard` / `sudo apt install wl-clipboard`).
- **X11**: install `xclip` (`sudo pacman -S xclip` / `sudo apt install xclip`).
- Verify the correct tool is in PATH: `which wl-copy` or `which xclip`.
- Under Wayland, ensure `WAYLAND_DISPLAY` is set in the environment where `pyrec` runs.

### Search returns no results

Tantivy indexing happens at block-end (OSC 133 D marker). If the shell does
not emit OSC 133 markers, blocks are not created and the index stays empty.

Install shell integration with one line — see the
[Shell integration](#shell-integration-osc-133) section at the top of this
document:

```sh
eval "$(pyrec shell-init bash)"   # or zsh / fish
```

### pyre-gpu (windowed viewer)

GPU-backed viewer for a single session (ADR-003). Same daemon sockets as `pyre`.

```sh
pyre-gpu
pyre-gpu --session <prefix> --pane <prefix>
```

| Key | Action |
|-----|--------|
| Ctrl+/ | Open block search overlay (`!query` = failures only) |
| Ctrl+Tab / Ctrl+Shift+Tab | Cycle panes in the active session |
| Esc | Close search overlay |

### pyre shows garbled characters

Ensure your terminal emulator is set to UTF-8 and that `$TERM` is `xterm-256color`
or `tmux-256color`. `pyre` requires true-color support for the palettes.

---

## Themes

`pyre-tui` and `pyre-gpu` consume the `pyre-themes` registry. 18 built-in
palettes: `catppuccin-mocha`, `catppuccin-latte`, `tokyo-night`,
`tokyo-night-light`, `gruvbox-dark`, `gruvbox-light`, `one-dark`, `one-light`,
`solarized-dark`, `solarized-light`, `kanagawa`, `rose-pine`, `rose-pine-dawn`,
`vesper`, `nord`, `dracula`, `terminal`, `ember`.

### Live picker

`Ctrl-Space T` opens the theme picker overlay inside `pyre`.

| Key | Action |
|-----|--------|
| `↑` / `↓` (or `j` / `k`) | Move cursor across palette list |
| `Enter` | Apply selection + persist to `config.toml` |
| `Esc` | Close without changing the active theme |

### Config

Default theme is read from `$XDG_CONFIG_HOME/pyre/config.toml`:

```toml
[ui]
theme = "catppuccin-mocha"
```

`pyre-gpu` reads the same key at startup; live switch from the picker is
TUI-only today — `pyre-gpu` requires restart to pick up a change.

Schema reference: [CONFIG.md](CONFIG.md).

---

## Notifications

Pane lifecycle events render as toast cards bottom-right of the TUI. The
deck suppresses high-frequency transitions (`Idle`, `Running`) — only
`Spawned`, `Closed`, `WaitingInput`, `Done`, and `Crashed` push toasts.

### Bindings

| Binding | Action |
|---------|--------|
| `Ctrl-Space N` | Toggle the deck on / off |

### Config

```toml
[ui.notifications]
enabled     = true   # master toggle (matches Ctrl-Space N initial state)
ttl_ms      = 4000   # per-toast lifetime
max_visible = 5      # cap on simultaneous toasts; oldest evicted first
```

Status: in-TUI deck is live. **M2 of the v0.2 UX sprint** wires desktop
bridges (`notify-send` / D-Bus on Linux, `osascript` on macOS) and per-kind
routing — in flight, not landed.

---

## Remote attach (M5, pending)

`pyred` binds a Unix Domain Socket only — no TCP listener. To attach a local
`pyre` / `pyrec` to a remote `pyred`, tunnel the socket over SSH.

### `pyrec remote`

```sh
pyrec remote <host>                              # prints the ssh -L command
pyrec remote <host> --exec                       # forks ssh in foreground
pyrec remote <host> --remote-socket <path>       # override remote pyred socket
pyrec remote <host> --local-socket <path>        # override local tunnel endpoint
```

Defaults:
- Remote socket: `~/.local/share/pyre/socket` (resolved on remote).
- Local socket: `$XDG_RUNTIME_DIR/pyre-remote-<host>.sock` (falls back to
  `/tmp/pyre-remote-<host>.sock`).

Once the tunnel is up, point `pyre` at the local endpoint:

```sh
pyre --socket $XDG_RUNTIME_DIR/pyre-remote-<host>.sock
```

Auth, encryption, and discovery are SSH's job. v0.3 may add native TLS; the
trade-off is recorded in [docs/adr/0004-remote-attach.md](adr/0004-remote-attach.md)
(Proposed). Polish on auto-detected `XDG_RUNTIME_DIR`, keepalives, and
reconnect is pending.
