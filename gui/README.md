# pyre GUI spike (throwaway)

A **minimal Tauri v2 desktop spike** evaluating whether pyre should pivot its
flagship frontend from a TUI to a GUI. It proves two things and nothing more:

- **Q1** — a Warp-style command "block" renders better in a webview than in a
  terminal UI (two static ember-themed block cards at the top).
- **Q2** — a Tauri Rust backend can bridge the existing `pyred` daemon's
  streaming pane output into an `xterm.js` terminal in the webview (the live
  pane below the cards).

> This is **throwaway, spike-quality** code. No settings, no multi-pane, no
> tabs, no error-recovery UI. One window, one live terminal, two static cards.
> Delete it once the pivot decision is made.

## Architecture (why the Rust bridge is mandatory)

The webview cannot speak tarpc/bincode over the Unix socket, so a Rust layer
bridges it:

```
xterm.js (webview)
   │  invoke("start_pane") / invoke("send_keys", bytes)
   ▼
Tauri Rust backend  (gui/src-tauri/src/lib.rs)
   │  control conn (0x01) → tarpc PyreDaemonClient → spawn(80x24)
   │  stream  conn (0x02 + session16 + pane16) → OutputFrame loop
   ▼  emit("pty-output", Vec<u8>) ───────────► term.write(Uint8Array)
pyred daemon (Unix socket: $XDG_RUNTIME_DIR/pyre.sock)
```

The connection/stream pattern is copied verbatim from the reference TUI client
(`crates/pyre-tui/src/app/pane_ops.rs` + `crates/pyre-tui/src/rpc/client.rs`).
It path-depends on the real `pyre-proto` crate; nothing in `pyre-proto` was
modified.

`gui/src-tauri` is **excluded** from the repo cargo workspace (see the
`[workspace] exclude` line in the root `Cargo.toml`) so the Tauri/webkit deps
never enter the main pyre build or CI.

## Run steps

1. **Start the daemon.** The live terminal shows nothing unless `pyred` is
   running and listening on its socket:

   ```sh
   # from the repo root
   cargo run -p pyred        # or: ./target/release/pyred
   ```

   Socket resolution mirrors the other pyre clients: `$PYRE_SOCK` →
   `$PYRE_SOCKET` → `$XDG_RUNTIME_DIR/pyre.sock` → `/tmp/pyre-<uid>.sock`.

2. **Install frontend deps:**

   ```sh
   cd gui
   pnpm install
   ```

3. **Run the GUI in dev mode:**

   ```sh
   pnpm tauri dev
   ```

   First run compiles the whole Tauri Rust backend — that is slow (minutes);
   let it finish. A window titled "pyre — GUI spike" opens with the two block
   cards on top and a live shell terminal below.

If `pyred` is **not** running, the terminal area shows a clear ember error
message instead of crashing.

## Build / typecheck only (no display needed)

```sh
# Rust backend compiles:
cd gui/src-tauri && cargo build

# Frontend typechecks + builds:
cd gui && pnpm install && pnpm build
```

## Prereqs

- Node + pnpm (uses the pnpm-based `@tauri-apps/cli` — no global `cargo-tauri`).
- `webkit2gtk-4.1` + `libsoup-3.0` (Tauri v2 Linux webview deps).
- Rust stable.
