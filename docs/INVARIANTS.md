# pyre invariants

Operational invariants every contributor + AI agent must respect.
Violation = recurring regression class.

## I-1 active_session preservation by ID

**Rule:** When `list_sessions` returns a refreshed list, the previously active session is
re-selected by its `SessionId`, not by slot index. If the ID is absent (session closed), fall
back to index 0.

**Rationale:** Two separate regressions reset `active_session` to index 0 after a refresh
because the list order changed. Using the slot index conflates identity with position — if the
daemon reorders sessions, the wrong session becomes active silently.

**How to check:** `grep -n "active_session" crates/pyre-tui/src/app/active.rs` — verify
selection logic compares `SessionId`, not `usize`. Test: `test_active_session_preserved_across_list_refresh`.

**Known violations:** regressed twice in main.rs before Wave 1 extraction; no linked PR.

---

## I-2 Layout source-of-truth: pyred canonical

**Rule:** pyred is canonical for layout. The TUI applies layout changes optimistically on user
action, then reconciles when a `LayoutChanged` push event arrives from pyred. On conflict the
pyred version wins. Never trust the local `LayoutNode` cache as ground truth after a close.

**Rationale:** Optimistic local state is needed for responsiveness, but the daemon is the
authoritative source. Treating the TUI-local cache as truth after a destructive operation (pane
close, session eviction) causes ghost panes and stale split trees.

**How to check:** `grep -rn "LayoutChanged" crates/pyre-tui/src/rpc/layout.rs` — reconcile
path must overwrite local cache unconditionally on receipt of `LayoutChanged`. Test:
`test_layout_split_then_close_reconciles`.

**Known violations:** no linked commit; discovered during refactor-plan-v04 audit.

---

## I-3 Mouse coordinates in terminal cell space

**Rule:** All mouse events are in terminal cell space (col, row), not pixels. Hit-rect tests on
pane borders, close-X buttons, and tab strips must use cell coordinates. The close-X hit rect
is `[right_edge - 1, right_edge]` — not the entire title bar.

**Rationale:** crossterm delivers events in cells. Mixing pixels (GPU renderer) with cells (TUI
renderer) causes misaligned hit tests. The close-X regression closed the wrong pane because the
hit rect covered the full title bar width.

**How to check:** `grep -n "close\|hit" crates/pyre-tui/src/input/mouse.rs` — all rect
comparisons must use `Rect` fields (x, y, width, height in cells). Test:
`test_close_x_click_hits_only_target_pane`.

**Known violations:** close-X hit rect covered full title bar pre-extraction; fixed in Wave 1
input/mouse extraction.

---

## I-4 Session/pane lifecycle

**Rule:** The lifecycle is: spawn → running → close → evict. A pane with `pane_count = 0` at
startup is stale and must be skipped — not rendered, not auto-focused.

**Rationale:** The daemon may return sessions whose workers crashed between list and render. A
zero-pane-count session has no live PTY. Rendering it produces a blank pane slot; auto-focusing
it causes a silent no-op RPC that can mask the real error.

**How to check:** `grep -n "pane_count" crates/pyre-tui/src/app/sessions.rs` — filtering must
reject sessions with `pane_count == 0` before constructing `SessionView`. Test:
`test_stale_pane_pruned_from_layout`.

**Known violations:** no linked commit; documented during invariant audit 2026-05-22.

---

## I-5 Stale session handling

**Rule:** `list_sessions` may return sessions whose worker crashed. Skip any session whose
`pane_count == 0`. Do not render them, do not auto-spawn into them.

**Rationale:** A stale session is distinct from a live empty session (auto-spawn target). The
distinction is `pane_count`: zero means the worker is gone. Treating stale sessions as spawn
targets fires duplicate `spawn_session` RPCs and creates confusing duplicate-name entries in the
session strip.

**How to check:** `grep -n "pane_count == 0\|skip.*stale" crates/pyre-tui/src/app/sessions.rs`.
Test: `test_list_sessions_skips_stale_workers`.

**Known violations:** no linked commit; documented during invariant audit 2026-05-22.

---

## I-6 SessionLost overlay rules

**Rule:** The `SessionLost` overlay is shown when the attached pane's worker exits unexpectedly
mid-session. The overlay is dismissed only by explicit user detach + re-list. Never auto-dismiss
silently.

**Rationale:** Auto-dismissal hides the fact that a session was lost — the user sees a blank TUI
then normal operation and does not know the daemon restarted. Explicit dismissal ensures the user
is aware the session state is gone.

**How to check:** `grep -n "session_lost" crates/pyre-tui/src/render/overlay/session_lost.rs` —
the only code path that sets `session_lost = false` must be in an explicit key handler (q / Esc
/ Ctrl-C), not in a background poll or timeout. Test coverage: manual smoke walkthrough.

**Known violations:** no linked commit; documented during invariant audit 2026-05-22.

---

## I-7 Auto-spawn rules

**Rule:** If `list_sessions` returns an empty list after connect, fire exactly one
`spawn_session` RPC. Do not re-spawn if the list is non-empty, even if all sessions are stale
(covered by I-5).

**Rationale:** Firing multiple spawns on empty-list produces duplicate sessions. The guard must
be: empty list after connect → one spawn, then re-list. A non-empty list (even all stale)
inhibits auto-spawn because stale sessions must be handled by the user, not silently replaced.

**How to check:** `grep -n "auto_spawn\|spawn_session" crates/pyre-tui/src/app/sessions.rs` —
the spawn path must be guarded by `sessions.is_empty()` and a per-connect boolean to prevent
repeat fires. Test: `test_auto_spawn_on_empty_session_list`.

**Known violations:** no linked commit; documented during invariant audit 2026-05-22.

---

## I-8 Mutex guard drop before `.await`

**Rule:** Any `MutexGuard` held across an `.await` point will deadlock in single-threaded tokio.
Pattern: `let val = { guard.clone() }; drop(guard); do_async(val).await`. Apply everywhere a
`Mutex::lock()` result is used near an async call.

**Rationale:** tokio's single-threaded executor does not yield a thread between `.await` points.
If a task holds a `MutexGuard` and suspends at `.await`, no other task can acquire the lock —
permanent deadlock. Two incidents already occurred in pyre before this invariant was codified.

**How to check:** `grep -n "lock()" crates/pyre-tui/src/**/*.rs` — audit every site where the
result crosses an `.await`. CI grep target:
`grep -rn "lock()" crates/ | xargs -I{} grep -c "await" {}`.
Test: no dedicated regression test (deadlock tests require threading harnesses); enforce via CI
grep + clippy.

**Known violations:** two deadlock incidents pre-2026 in pyre-tui main loop; no linked commit.
