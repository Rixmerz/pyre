% PYRED(1) pyre 0.1.0 | pyre Manual
% pyre contributors
% May 2026

# NAME

pyred - pyre terminal multiplexer daemon

# SYNOPSIS

**pyred** [OPTIONS]

# DESCRIPTION

**pyred** is the daemon component of the **pyre** terminal multiplexer. It
owns all PTY file descriptors, runs the ANSI/VT parser, accumulates command
Blocks (keyed by OSC 133 markers), persists session and block state to SQLite,
indexes block stdout in Tantivy for full-text search, executes lifecycle hooks
from **hooks.toml**, and serves all clients over a Unix domain socket.

The daemon runs as an unprivileged user process. The UDS is created at
**$XDG_RUNTIME_DIR/pyre.sock** with mode **0700**; only the owning user can
connect. pyred is designed to run as a systemd user service (see the provided
**pyred.service** unit), but can also be started manually.

pyred survives client disconnects — PTYs and their associated shell processes
remain alive until the session is explicitly killed or the daemon exits.

# OPTIONS

**--socket** *PATH*
: Override the default UDS path. Default: **$XDG_RUNTIME_DIR/pyre.sock**.

**--data-dir** *PATH*
: Override the data directory for state.db, Tantivy index, and hooks.toml.
  Default: **$XDG_DATA_HOME/pyre** (or **~/.local/share/pyre**).

**--log-filter** *FILTER*
: tracing-subscriber filter string. Example: **pyred=debug,warn**.
  Also controlled by the **PYRE_LOG** environment variable.

**-h**, **--help**
: Print help and exit.

**-V**, **--version**
: Print version and exit.

# ENVIRONMENT

**XDG_RUNTIME_DIR**
: Directory for the socket file. Must be writable by the user.

**PYRE_DATA_DIR**
: Override the data directory (equivalent to --data-dir).

**PYRE_LOG**
: tracing-subscriber filter string for log output.

**PYRE_SOCKET**
: Override the socket path (equivalent to --socket).

# FILES

**$XDG_RUNTIME_DIR/pyre.sock**
: Unix domain socket. Created on start, removed on exit. Mode 0700.

**$XDG_DATA_HOME/pyre/state.db**
: SQLite database. Stores sessions, panes, and block metadata.

**$XDG_DATA_HOME/pyre/index/**
: Tantivy full-text search index for block stdout.

**$XDG_DATA_HOME/pyre/hooks.toml**
: Hook configuration file (optional). See **pyre-config**(5).

# EXAMPLES

Start the daemon in the foreground with debug logging:

    PYRE_LOG=pyred=debug pyred

Start via systemd user service:

    systemctl --user enable --now pyred

# SEE ALSO

**pyrec**(1), **pyre-tui**(1), **pyre-mcp**(1), **systemctl**(1)
