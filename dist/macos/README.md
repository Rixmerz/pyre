# pyre on macOS

pyre is **Linux-first**; macOS is supported for local development and dogfooding with these caveats:

| Feature | macOS |
|---------|--------|
| PTY / sessions / TUI | Yes |
| Clipboard | `pbcopy` |
| Agent detection (`inspect_pid`) | Limited (no `/proc`; falls back to heuristics) |
| systemd | Not used — start `pyred` manually or via `--start-daemon` |

## Install / reinstall

From the repo root:

```sh
chmod +x dist/macos/install.sh
./dist/macos/install.sh --start-daemon
```

Hybrid multi-agent profile:

```sh
./dist/macos/install.sh --hybrid --start-daemon
```

Binaries land in `~/.cargo/bin` (ensure it is on your `PATH`).

## Manual steps

```sh
cargo build --release -p pyred -p pyrec -p pyre-tui -p pyre-mcp
cargo install --path crates/pyred --force
cargo install --path crates/pyrec --force
cargo install --path crates/pyre-tui --bin pyre --force
cargo install --path crates/pyre-mcp --force

# Daemon (background)
pkill -x pyred 2>/dev/null || true
pyred --mode supervisor &

# Client
pyre
```

Default socket: `/tmp/pyre-$(id -u).sock`

Config: `~/Library/Application Support/pyre/config.toml`

Data (SQLite, Tantivy): `~/Library/Application Support/pyre/`

## Skip startup animation

```sh
pyre --no-splash
# or
PYRE_NO_SPLASH=1 pyre
```
