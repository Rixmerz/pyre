% PYRE(1) pyre 0.1.0 | pyre Manual
% pyre contributors
% May 2026

# NAME

pyre - pyre terminal multiplexer TUI renderer

# SYNOPSIS

**pyre** [--socket *PATH*]

# DESCRIPTION

**pyre** is the interactive TUI front end for the **pyre** terminal
multiplexer. It renders sessions and panes using **ratatui** and **crossterm**,
using the Ember color theme (dark amber/orange on near-black). It connects to
the running **pyred** daemon over the Unix domain socket.

Each session is displayed as a tab. Each pane within a session occupies a
cell in a grid layout. The block ribbon at the bottom shows recent command
blocks for the focused pane.

pyre is mouse-first: click to focus a pane, scroll to navigate output.
Keyboard control uses the Ctrl-B prefix, consistent with tmux muscle memory.

# OPTIONS

**--socket** *PATH*
: Override the default socket path.

**-h**, **--help**
: Print help and exit.

**-V**, **--version**
: Print version and exit.

# KEY BINDINGS

## Prefix: Ctrl-B

**Ctrl-B c**
: Open a new pane in the current session.

**Ctrl-B n**
: Focus the next pane.

**Ctrl-B p**
: Focus the previous pane.

**Ctrl-B "**
: Horizontal split (new pane below).

**Ctrl-B %**
: Vertical split (new pane right).

**Ctrl-B q**
: Close the current pane.

**Ctrl-B y**
: Copy pane scrollback to system clipboard.

**Ctrl-B z**
: Toggle zoom (fullscreen) on current pane.

**Ctrl-B s**
: Open block search dialog.

**Ctrl-B [**
: Enter scroll mode.

**Ctrl-B ]**
: Exit scroll mode.

**Ctrl-B d**
: Detach (leave daemon running, exit pyre).

**Ctrl-B ?**
: Show key binding help overlay.

## Scroll mode

**PgUp** / **PgDn**
: Scroll one screen up / down.

**↑** / **↓**
: Scroll one line.

**g** / **G**
: Jump to top / bottom.

**q**
: Exit scroll mode.

## Mouse

**Left click**
: Focus the pane under the cursor.

**Scroll wheel**
: Scroll pane output up or down.

# EXAMPLES

Start pyre connected to the default daemon socket:

    pyre

Start with a custom socket:

    pyre --socket /tmp/pyre-test.sock

# SEE ALSO

**pyred**(1), **pyrec**(1), **pyre-mcp**(1)
