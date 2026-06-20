# Pyre GUI

A **Tauri v2 desktop client** for the `pyred` daemon. It is a full-fledged
graphical front end alongside the TUI (`pyre-tui`): both speak the same
`pyred` protocol over the same Unix socket, so the GUI is a peer surface,
not a wrapper around the terminal client.

The GUI renders the complete pyre surface:

- **Sessions** — attach to, switch between, and spawn `pyred` sessions.
- **Layout-tree panes** — the recursive split layout (`LayoutNode`), with
  live PTY output streamed into `xterm.js` terminals.
- **Blocks** — the Warp-style command-block model (prompt + output framed
  as discrete cards).
- **Search** — query across blocks/sessions via the daemon's search index.
- **18 themes** — the built-in palettes exposed by the `pyre-themes` crate.
- **Heat signature** — the activity/heat visualization for panes.

## Architecture

```
gui/
├── src/            vanilla-TypeScript frontend (xterm.js, theme UI, blocks)
└── src-tauri/      Rust bridge — Tauri commands + tarpc client to pyred
```

The webview cannot speak tarpc/bincode over the Unix socket, so the Rust
layer in `gui/src-tauri` bridges it: webview `invoke(...)` calls cross into
Tauri commands, which hold a `tarpc` `PyreDaemonClient` (control conn) plus a
raw streaming connection (`OutputFrame` loop) and emit pane bytes back to the
webview via Tauri events. The wire format is the existing pyre proto — the GUI
path-depends on the real `pyre-proto` and `pyre-themes` crates and modifies
neither.

```
xterm.js (webview)
   │  invoke("start_pane") / invoke("send_keys", bytes)
   ▼
Tauri Rust backend  (gui/src-tauri/src/lib.rs)
   │  control conn  → tarpc PyreDaemonClient
   │  stream  conn  → OutputFrame loop
   ▼  emit("pty-output", bytes) ───────────► term.write(...)
pyred daemon  (Unix socket: $XDG_RUNTIME_DIR/pyre.sock)
```

> **Workspace note:** `gui/src-tauri` is deliberately **excluded** from the
> repo cargo workspace (`[workspace] exclude = ["gui/src-tauri"]` in the root
> `Cargo.toml`). It pins its own Tauri/webkit dependency versions so those
> deps never enter the main pyre build or CI. Build it from inside
> `gui/src-tauri` (or via `pnpm tauri`), not from the workspace root.

Socket resolution mirrors the other pyre clients:
`$PYRE_SOCK` → `$PYRE_SOCKET` → `$XDG_RUNTIME_DIR/pyre.sock` →
`/tmp/pyre-<uid>.sock`.

## Run

```sh
pyre
```

The `pyre` wrapper (`~/.local/bin/pyre`) ensures `pyred` is running, sets the
NVIDIA/Wayland webkit workaround env vars, then execs `~/.local/bin/pyre-gui`.
It also retries once on a sub-3s cold-start crash (a transient
NVIDIA/webkit2gtk init race).

## Develop

> **pnpm only.** `npm` is forbidden in this project. The Tauri CLI is the
> pnpm-scoped `@tauri-apps/cli` — there is no global `cargo-tauri` dependency.

```sh
cd gui
pnpm install
pnpm tauri dev      # opens the GUI; first run compiles the Rust backend (slow)
```

Backend-only / typecheck-only checks (no display needed):

```sh
cd gui/src-tauri && cargo build      # Rust bridge compiles
cd gui && pnpm build                 # frontend typechecks + builds
```

## Build

```sh
cd gui
pnpm tauri build --no-bundle         # release binary, no OS installer bundle
```

This produces `gui/src-tauri/target/release/pyre-gui`. (The cargo binary
name stays `pyre-gui`; the `pyre` wrapper execs it. The Tauri product name is
`Pyre` and the app identifier is `dev.pyre.gui`.)

## Install

The desktop integration assets live in `gui/packaging/`:

1. **Binaries** — copy the release `pyre-gui` and `pyred` to `~/.local/bin/`,
   along with the `pyre` wrapper script. Ensure `~/.local/bin` is on `PATH`.

2. **Desktop entry** — so Pyre shows up in Hyprland/app launchers:

   ```sh
   cp gui/packaging/pyre.desktop ~/.local/share/applications/
   # icon (so Icon=pyre resolves)
   mkdir -p ~/.local/share/icons/hicolor/128x128/apps
   cp gui/src-tauri/icons/128x128.png ~/.local/share/icons/hicolor/128x128/apps/pyre.png
   cp gui/src-tauri/icons/icon.png    ~/.local/share/icons/pyre.png
   update-desktop-database ~/.local/share/applications 2>/dev/null || true
   ```

3. **systemd user service (optional).** The `pyre` wrapper already ensures
   `pyred`, so the daemon service is **opt-in**. Install it, then enable
   only if you want `pyred` managed by systemd:

   ```sh
   mkdir -p ~/.config/systemd/user
   cp gui/packaging/pyred.service ~/.config/systemd/user/
   systemctl --user daemon-reload
   systemctl --user enable --now pyred.service   # opt-in; not enabled by default
   ```

## Prereqs

- Node + pnpm (`npm` is not supported).
- `webkit2gtk-4.1` + `libsoup-3.0` (Tauri v2 Linux webview deps).
- Rust stable.
- A running `pyred` daemon (the `pyre` wrapper starts one automatically).
