# pyre — Usage Guide

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
| `tmux detach-client` | `Ctrl-B d` in `pyre`, or close the pyrec process |

---

## pyre key bindings

### Prefix: `Ctrl-B`

| Binding | Action |
|---------|--------|
| `Ctrl-B c` | Open new pane in current session |
| `Ctrl-B n` | Focus next pane |
| `Ctrl-B p` | Focus previous pane |
| `Ctrl-B "` | Horizontal split (new pane below) |
| `Ctrl-B %` | Vertical split (new pane right) |
| `Ctrl-B q` | Close current pane |
| `Ctrl-B y` | Copy current pane scrollback to clipboard |
| `Ctrl-B z` | Toggle zoom (fullscreen) on current pane |
| `Ctrl-B s` | Open block search dialog (Tantivy) |
| `Ctrl-B [` | Enter scroll mode |
| `Ctrl-B ]` | Exit scroll mode |
| `Ctrl-B d` | Detach (leave daemon running, exit TUI) |
| `Ctrl-B ?` | Show key binding help overlay |

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

Tantivy indexing happens at block-end (OSC 133 D). If the shell does not emit
OSC 133 markers, blocks are not created and the index stays empty.

Enable OSC 133 in your shell:
- **bash**: add `source /usr/share/bash-preexec/bash-preexec.sh && precmd() { printf '\033]133;A\007'; }; preexec() { printf '\033]133;B\007'; }` to `.bashrc`.
- **zsh**: use the `precmd`/`preexec` hooks; many zsh prompt frameworks (Starship, Powerlevel10k) emit OSC 133 automatically.
- **fish**: add `function fish_prompt; printf '\033]133;A\007'; end` to `config.fish`.

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
or `tmux-256color`. `pyre` requires true-color support for the Ember palette.
