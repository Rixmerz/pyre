---
topic: pyre-bug-surface
produced_by: livespec-specialist
produced_at: 2026-05-18T00:00:00Z
sources_scanned:
  - crates/pyre-tui/src/main.rs
  - crates/pyred/src/main.rs
  - crates/pyred/src/session.rs
  - crates/pyred/src/pty.rs
  - crates/pyred/src/inspect.rs
  - crates/pyre-proto/src/lib.rs
  - crates/pyre-proto/src/sessions.rs
  - git show e2288c5, 70c1378, 9202ba8
---

## Summary

Six bug-class touch-points mapped from livespec index + source reads.
Resize is partially fixed (parser sync per-frame); the remaining gap is
a missing `resize_pane` RPC to notify the child PTY. Mouse selection
infra exists but copy-to-clipboard is unhooked from drag. Scrollback and
pane/session model are fully wired. No auto-spawn-daemon logic exists in
pyre-tui.

## Bug class 1 — Daemon bootstrap / socket missing

**Symbol / file:line**
- `crates.pyred.src.main.socket_path` — `crates/pyred/src/main.rs:43-51`
  Computes `$XDG_RUNTIME_DIR/pyre.sock`; falls back to `/tmp/pyre-{uid}.sock`.
- `crates.pyre-tui.src.main.default_socket` — `crates/pyre-tui/src/main.rs:105-112`
  Identical path logic on the TUI side. Both must agree.
- `crates.pyre-tui.src.main.control_client` — `crates/pyre-tui/src/main.rs:131-142`
  Connects via `UnixStream::connect`, writes `MODE_CONTROL` tag. Errors
  surface as `"connect $path: ..."` — immediate fatal exit, no retry.
- `crates.pyre-tui.src.main.main` — `crates/pyre-tui/src/main.rs:2838-2884`
  Entry point: calls `control_client` before entering `run_tui`. If the
  socket is absent it crashes with the connect error.
- `crates.pyred.src.main.main` — `crates/pyred/src/main.rs:87-159`
  Daemon start: removes stale socket, binds, sets mode 0700, accepts loop.
- `crates.pyred.src.inspect.inspect_pid` — `crates/pyred/src/inspect.rs:14-26`
  Linux `/proc`-based PID inspection (untracked file, no livespec symbol yet).

**What to fix:** No auto-spawn logic exists. pyre-tui will fail immediately
with a socket-connect error if pyred is not already running. Fix is to add
a "spawn pyred if socket absent" path in `main()` before `control_client`,
using `std::process::Command::new("pyred")` + `wait_for_socket` (already
implemented in `crates/pyred/tests/prod_smoke.rs:46-57`).

**Open question:** Should pyre-tui embed a fixed binary path or rely on PATH?
---

## Bug class 2 — TUI resize on child app launch

**Symbol / file:line**
- `crates.pyre-tui.src.main.render_pane` — `crates/pyre-tui/src/main.rs:769-947`
  Lines 833-847: parser is synced to `text_area` each render frame via
  `slot.parser.set_size(target_rows, target_cols)`. This resolves the
  *vt100 viewport* desync on split. But line 843-845 has an explicit TODO:
  no `resize_pane` RPC exists yet; the child shell still believes 80x24.
- `crates.pyre-tui.src.main.run_tui` — `crates/pyre-tui/src/main.rs:2820-2824`
  `Event::Resize` handler resizes ALL parsers to the full terminal dimensions
  (`new_rows, new_cols`), not to each pane's split-proportioned `text_area`.
  This is wrong for split panes — it overwrites the per-frame correction done
  in `render_pane` with the outer terminal size.
- `crates.pyre-proto.src.lib.PaneSize` — `crates/pyre-proto/src/lib.rs:56-59`
  Struct exists (`rows: u16, cols: u16`) but no RPC uses it for resize yet.
- `crates.pyred.src.session.PaneState` — `crates/pyred/src/session.rs:19-42`
  `master: Arc<Mutex<Box<dyn MasterPty>>>` is annotated
  `#[allow(dead_code)] // phase 6+: resize RPC`; the PTY master is retained
  but `MasterPty::resize(PtySize{rows,cols,..})` is never called at runtime.

**What to fix:**
1. Add a `resize_pane(PaneId, PaneSize)` method to `pyre-proto` service.
2. Implement it in `crates.pyred.src.server.DaemonImpl` calling
   `pane.master.lock().await.resize(PtySize{..})`.
3. In `render_pane`, after the `set_size` block, `tokio::spawn` the RPC call
   when dimensions actually changed (gate on `cur_rows != target_rows || cur_cols != target_cols`).
4. Fix `Event::Resize` handler to remove the blanket `set_size(new_rows, new_cols)` —
   let `render_pane` handle per-pane resizing each frame instead.

**Open question:** Should the resize RPC be debounced to avoid flooding on drag-resize?
---

## Bug class 3 — Mouse text selection

**Symbol / file:line**
- `crates.pyre-tui.src.main.TermGuard::enter` — `crates/pyre-tui/src/main.rs:213-217`
  `EnableMouseCapture` IS enabled at startup. Mouse events reach the TUI.
- `crates.pyre-tui.src.main.handle_mouse` — `crates/pyre-tui/src/main.rs:1828-1998`
  Handles `ScrollUp`, `ScrollDown`, `Down(Left)` (focus + drag-resize start),
  `Drag(Left)` (split-boundary drag only), `Up(Left)` (drag end). No text
  selection logic exists in the `Drag` arm — drag is consumed entirely by
  split-resize. There is no "start text selection on Down, extend on Drag,
  copy on Up" path.
- `crates.pyre-tui.src.main.Selection` — `crates/pyre-tui/src/main.rs:401-437`
  Full struct + `normalized()` + `contains()` implemented and marked
  `#[allow(dead_code)]`.
- `crates.pyre-tui.src.main.SelectionBase` — `crates/pyre-tui/src/main.rs:394-399`
  `Live` and `Scrollback(usize)` variants, also `#[allow(dead_code)]`.
- `crates.pyre-tui.src.main.ClickTracker` — `crates/pyre-tui/src/main.rs:443-449`
  Double/triple-click state, also `#[allow(dead_code)]`.
- `crates.pyre-tui.src.main.AppState` — `crates/pyre-tui/src/main.rs:519-558`
  `selection: Option<Selection>` field at line 543 exists but is never
  written by `handle_mouse` (only read in `render_pane` for highlight overlay).
- Render highlight overlay — `crates/pyre-tui/src/main.rs:898-926`
  Already renders REVERSED cells for any `selection` in `slot.scroll_offset == 0`.
  The render side is wired; the input side is not.
- Bracketed paste: not mentioned anywhere in the codebase.

**What to fix:** Wire `handle_mouse` `Down(Left)` (inside a pane, not near
a boundary) to set `state.selection = Some(Selection{..dragging:true})`.
Wire `Drag(Left)` (when no `tab.drag` split-drag active) to update
`selection.end`. Wire `Up(Left)` to set `dragging: false` and extract the
selected text from the vt100 screen for clipboard. The render side and the
`Selection` struct are already complete.

**Open question:** Shift+click selection needs disambiguation from split-drag initiation.
---

## Bug class 4 — Scrollback

**Symbol / file:line**
- `crates.pyre-tui.src.main.PaneSlot` — `crates/pyre-tui/src/main.rs:281-305`
  Fields `scroll_offset: usize` (line 296) and `scrollback_capacity: usize`
  (line 300). Capacity cached via peek/restore in `render_pane`.
- `crates.pyre-tui.src.main.render_pane` — `crates/pyre-tui/src/main.rs:769-947`
  Lines 817-820: capacity peek, clamp offset, set scrollback position each frame.
- `handle_mouse` ScrollUp/ScrollDown — `crates/pyre-tui/src/main.rs:1833-1861`
  `scroll_offset += 3` / `saturating_sub(3)`, both clamped to capacity.
- PgUp/PgDn — `crates/pyre-tui/src/main.rs:2783-2805`
  `scroll_offset += half_page` / `saturating_sub(half_page)`, clamped.
- Key forward reset — `crates/pyre-tui/src/main.rs:2813`
  Any non-scroll key forwarded to PTY resets `scroll_offset = 0` (returns to live).
- `crates.pyre-tui.src.main.attach_pane` — `crates/pyre-tui/src/main.rs:690-762`
  vt100 `Parser` initialized with scrollback via `vt100::Parser::new(rows, cols, scrollback)`.
  The scrollback ring size is set here.

**What to fix:** Scrollback mechanics are correct post-commits 70c1378 and
e2288c5. The remaining issue may be that `attach_pane` initializes the
parser with the *current terminal size* (full screen), not the pane's split
size — so scrollback capacity can appear to be 0 until the first render
calls `set_size`. Verify the scrollback ring size passed to `Parser::new`.

**Open question:** Verify the scrollback ring-size passed to `vt100::Parser::new` in `attach_pane`; if 0, scrollback never grows.
---

## Bug class 5 — Render loop / event loop

**Symbol / file:line**
- `crates.pyre-tui.src.main.run_tui` — `crates/pyre-tui/src/main.rs:2282-2832`
  Main loop: draw → `crossterm::event::poll(16ms)` → read. Effective
  frame rate: 60 fps maximum (16 ms poll). No explicit redraw-on-data flag;
  the loop always draws before polling, so output latency ≤ 16 ms per frame.
- `Event::Resize` — line 2820: sets all parsers to full terminal size (bug,
  see class 2). Does NOT force an extra `terminal.clear()`, so stale cells
  from the prior size may remain visible briefly.
- No separate "dirty" flag; every 16 ms the full frame is redrawn unconditionally.
- Block poll is on a 500 ms wall-clock throttle (line 2326).
- Sidebar poll is on a 1 s throttle (line 2381).

**What to fix:** `Event::Resize` should call `terminal.clear()` (or
`terminal.resize(Rect)`) before the next draw to prevent cell artifacts.
The blanket `set_size` in the Resize handler should be removed (see class 2).

**Open question:** 60 fps may be adequate; the 500 ms block-poll throttle may cause ribbon lag.
---

## Bug class 6 — Pane / session model

**Symbol / file:line**
- `pyre_proto::PaneId` / `pyre_proto::SessionId` — defined in
  `crates/pyre-proto/src/lib.rs` (uuid newtypes).
- `crates.pyred.src.session.PaneState` — `crates/pyred/src/session.rs:19-42`
  Owns `id: PaneId`, `session: SessionId`, PTY master, broadcast channels,
  `close_token`, `ringbuf`, `state_tracker`.
- `crates.pyred.src.session.SessionRegistry::open_pane` — `crates/pyred/src/session.rs:136-169`
  Spawns PTY → inserts into `session.panes` map.
- `crates.pyred.src.session.SessionRegistry::remove_pane` — `crates/pyred/src/session.rs:281-297`
  Called by child-wait task (pty.rs:187). Removes pane; drops session if no
  panes remain. Implements the 9202ba8 fix.
- `crates.pyred.src.pty.spawn_pty` child-wait task — `crates/pyred/src/pty.rs:175-190`
  `spawn_blocking` → `child.wait()` → `token.cancel()` → `registry.remove_pane(pane_id)`.
  Ordering: close_token fires first (stream clients get EOF), then removal.
- `crates.pyred.src.session.SessionRegistry::close_pane` — `crates/pyred/src/session.rs:187-197`
  RPC-driven close: kills child, sets `closed_at`, removes pane from map.
  Does NOT call `remove_pane`; the session is not dropped when its last pane
  is closed via RPC (only via natural exit). Potential ghost-session leak.
- `crates.pyre-tui.src.main.close_focused_pane` — `crates/pyre-tui/src/main.rs:2224-2231`
  Removes slot from `AppState`; does not send a `close_pane` RPC.
- `crates.pyred.src.server.DaemonImpl::open_pane` — `crates/pyred/src/server.rs:142-159`
  RPC handler for opening additional panes in an existing session.

**What to fix:** `close_pane` in `session.rs:187-197` kills and removes the
pane from the map but does not check if the session is now empty and remove
it — unlike `remove_pane`. This means RPC-driven pane kills can leave empty
sessions in the registry. The fix is to add the same empty-session check
in `close_pane` (or refactor both to call a shared `remove_pane_inner`).

**Open question:** Should `close_focused_pane` also send a `close_pane` RPC?
the TUI only removes the local slot?
