% PYREC(1) pyre 0.1.0 | pyre Manual
% pyre contributors
% May 2026

# NAME

pyrec - pyre client: spawn, attach, and control terminal sessions

# SYNOPSIS

**pyrec** [--socket *PATH*] [--shell *SHELL*] [*SUBCOMMAND*]

# DESCRIPTION

**pyrec** is the command-line client for the **pyred** daemon. Without a
subcommand it spawns a new session and pane, puts the local TTY in raw mode,
and bridges stdin/stdout to the PTY — behaving like an interactive terminal.

All subcommands connect to the running **pyred** daemon over the Unix domain
socket at **$XDG_RUNTIME_DIR/pyre.sock**.

# OPTIONS

**--socket** *PATH*
: Override the default socket path (applies to all subcommands).

**--shell** *SHELL*
: Override the shell for the interactive attach (default subcommand only).

**-h**, **--help**
: Print help and exit.

**-V**, **--version**
: Print version and exit.

# SUBCOMMANDS

**sessions**
: List all active sessions owned by the daemon.

**panes** *SESSION*
: List panes in SESSION (UUID or ≥8-char prefix).

**attach** *SESSION* [--pane *PANE*]
: Attach to an existing session, optionally targeting a specific pane.

**new-pane** --session *SESSION* [--shell *SHELL*] [--cwd *DIR*]
: Open a new pane in SESSION without attaching.

**list** [--session *SESSION*] [--n *N*]
: List the last N blocks (default 20). Optionally filter by session.

**search** *QUERY* [--limit *N*]
: Full-text search across block stdout via Tantivy. Returns ranked hits.

**capture-pane** --session *SESSION* --pane *PANE* [--lines *N*] [--copy]
: Capture the last N lines (default 40) from the pane ring buffer.
  With --copy, also write to the system clipboard.

**send-keys** --session *SESSION* --pane *PANE* -- *KEYS*
: Write raw bytes to a pane. Append \\n to simulate Enter.

**kill-session** *SESSION*
: Terminate SESSION and all its panes.

**split-window** --session *SESSION*
: Open a new pane in SESSION (layout managed by `pyre`).

**display-message** *MSG*
: Print MSG to stderr (tmux-compat stub).

**list-sessions**
: Alias for **sessions**.

**list-panes**
: Alias for **panes**.

**list-windows**
: Alias for **panes**.

**new-session**
: Alias for interactive spawn (no args: same as default subcommand).

# EXAMPLES

Spawn a new zsh session:

    pyrec --shell /bin/zsh

List sessions:

    pyrec sessions

Attach to a session by UUID prefix:

    pyrec attach 3f2a1b

Search for a pattern in block output:

    pyrec search "cargo error"

Send a command to a pane:

    pyrec send-keys --session <id> --pane <id> -- "ls -la\n"

Capture pane output and copy to clipboard:

    pyrec capture-pane --session <id> --pane <id> --copy

# SEE ALSO

**pyred**(1), **pyre**(1), **pyre-mcp**(1)
