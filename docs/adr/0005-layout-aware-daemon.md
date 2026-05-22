# ADR-0005 — Layout-aware daemon

**Status:** Proposed
**Date:** 2026-05-22
**Sprint:** M7 (v0.2 layout SoT)

## Context

Today the split topology (`HSplit` / `VSplit`) lives entirely as
TUI-local cosmetic state. `pyre-tui` owns it in
`crates/pyre-tui/src/main.rs:788` (`enum LayoutNode { Leaf(usize),
HSplit(...), VSplit(...) }`) and uses `usize` slot indices into
`AppState.slots`. `pyre-gpu` ported the same enum in commit
`53354b4` to `crates/pyre-gpu/src/layout.rs:52`, but its leaves carry
`PaneId` rather than `usize`, and the tree is constructed locally
from `list_all_panes` results — the two clients cannot share a layout.

`pyred` is unaware of splits. The control surface
(`crates/pyre-proto/src/service.rs:139` `open_pane`) accepts a `cols`/
`rows` request and returns a `PaneId`; there is no notion of a parent
node, an orientation, or a sibling weight. The MCP `session_layout`
tool (`crates/pyre-mcp/src/main.rs:984`) spawns N flat panes and the
user has to manually `Ctrl-B "/%` inside `pyre-tui` to actually
split. SQLite (`crates/pyred/migrations/0001_init.sql`) persists
sessions, panes, and blocks — no `layout` column.

Result:

- Split topology is **fragile** — it dies with the TUI process; reattach
  reopens the session as a flat row.
- `pyre-gpu` and `pyre-tui` **cannot mirror** the same visual layout
  even when attached to the same session.
- MCP-spawned sessions never get to declare a layout, so agent-orchestrated
  fleets cannot describe their intended geometry.
- Drag-resize lives in `pyre-tui` (`main.rs:2787..2818`) and never
  reaches the daemon, so even a single-client restart loses sibling
  weights.

## Decision

**Move `LayoutNode` to `pyre-proto`** and make `pyred` the source
of truth for per-session layout. Persist it in `state.db`. Add three
RPCs so any client (TUI, GPU, MCP, CLI) can create splits and observe
geometry without touching local state directly.

### Wire shape (proto)

`LayoutNode` is lifted into `pyre-proto::layout` with `serde`
derives. Leaf variant carries `PaneId` (the GPU shape — UUID-stable
across the pane's lifetime, unlike the TUI's `usize` slot index which
is local-process-only):

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LayoutNode {
    Leaf(PaneId),
    HSplit(Vec<(LayoutNode, u16)>),  // weights sum to 100, top-to-bottom
    VSplit(Vec<(LayoutNode, u16)>),  // weights sum to 100, left-to-right
}
```

### RPC additions

```
open_pane_split(parent: PaneId, orient: Orient) -> PaneId
set_pane_weight(pane: PaneId, weight: u16) -> ()
get_session_layout(session: SessionId) -> LayoutNode
// close_pane already exists; document layout-side collapse below.
```

`Orient` is `{ Vertical, Horizontal }` (matching
`pyre-gpu/src/layout.rs:23`). `open_pane_split` spawns the new pane
inside `parent`'s session, then replaces the `Leaf(parent)` node
with a 50/50 two-child split of the requested orientation. The new
`PaneId` is returned. `close_pane` already removes the pane row;
the daemon now also collapses single-child splits in the persisted
layout (`pyre-gpu/src/layout.rs:241` `remove_leaf` is the reference
implementation). A new broadcast variant `PaneEventKind::LayoutChanged`
fires after every layout mutation so all attached clients (TUI, GPU,
MCP poll) re-render against fresh state.

### PROTO_VERSION bump

`crates/pyre-proto/src/handshake.rs:12` jumps from `3` → `4`. Old
v3 clients are refused at the handshake — same behaviour as the v2→v3
bump in commit `f886a0e`.

## Options compared

- **A — `LayoutNode` in the daemon (this proposal).** Single source
  of truth; clients render via `get_session_layout`, mutate via RPCs,
  react to broadcast. Pros: MCP splits, GPU↔TUI parity, exact reattach,
  drag-resize survives restart. Cons: per-split UDS roundtrip
  (≤5 ms); wire bump.
- **B — TUI-local + reattach snapshot.** `pyre-tui` writes a layout
  blob (`$XDG_STATE_HOME/pyre/layout-<session>.json`) on every
  mutation; reads on reattach. Pros: no wire bump or schema change.
  Cons: GPU still has its own copy, MCP still can't create splits,
  two clients on one session still diverge.
- **C — Daemon stores opaque blob; clients decode.** Pros: minimal
  daemon work. Cons: GPU↔TUI parity breaks the moment formats drift;
  defers design to "whichever client writes first wins".

**A wins** on durability + multi-client + MCP support. The wire-bump
cost is one-off; the schema cost is paid by `pyred` (the right place).

## Migration

`state.db` schema gains one column on `sessions` via
`crates/pyred/migrations/0002_session_layout.sql`:

```sql
ALTER TABLE sessions ADD COLUMN layout TEXT;  -- JSON-encoded LayoutNode
```

Sessions without a `layout` value fall back at load time to
`LayoutNode::Leaf(first_pane_by_creation_order)` — bit-identical to
current behaviour (one tab, one pane, no split). Sessions with two
or more panes but no layout row become a single-row VSplit with equal
weights (mirrors what `pyre-tui` produced via `Ctrl-B %` on the old
flat list).

Migration is **non-destructive** for any existing row. The only
column added defaults to `NULL`, and the fallback is byte-equivalent
to the pre-migration display.

## Consequences

**Positive**

- MCP `session_layout` can describe real splits, e.g.
  `{ orient: "vertical", panes: [...] }` → one RPC per split rather
  than user-driven keybinds.
- `pyre-gpu` deletes `crates/pyre-gpu/src/layout.rs` as a local store
  and pulls layout from the daemon — no more parallel implementation.
- Reattach (cold or warm) restores the exact split topology — no
  reliance on TUI keystroke recall.
- Drag-resize hits `set_pane_weight` instead of mutating local
  state; sibling weights survive restart.
- One state, two renderers (TUI + GPU): trivial to add a third
  (web client, ttyd, future).

**Negative**

- Wire bump 3→4 refuses old clients (same dance as v2→v3, commit
  `f886a0e`); users running mixed versions hit it once.
- Per-split UDS roundtrip (~1–5 ms); well under perceptual threshold.
- Drag-resize naively emits N RPCs per drag; client coalesces and
  only sends final weight on `MouseUp`.
- Layout JSON is verbose for deep trees; cost paid once per attach,
  not on the hot path.

## Implementation waves

- **W1** Lift `LayoutNode` (+ `Orient`, helpers) into
  `pyre-proto::layout`; bump `PROTO_VERSION` 3→4. Add
  `LayoutChanged` to `PaneEventKind`.
- **W2** `pyred`: persist `Session.layout` in memory, add the
  `0002_session_layout.sql` migration, fallback construction on
  load.
- **W3** `pyred`: implement `open_pane_split`,
  `set_pane_weight`, `get_session_layout`; extend `close_pane`
  to collapse single-child splits and emit `LayoutChanged`.
- **W4** `pyre-tui`: drop local `LayoutNode`, fetch from daemon on
  attach; `Ctrl-B "/%` calls `open_pane_split`; drag-resize calls
  `set_pane_weight` on release; subscribe to `LayoutChanged`.
- **W5** `pyre-gpu`: same as W4 — delete local layout store, mirror
  daemon. Read-only for M7.
- **W6** `pyre-mcp::tool_session_layout`: accept
  `{ orient, panes: [...] }` spec; expose a new `set_pane_weight`
  tool for agents that programmatically resize.
- **W7** Tests: round-trip serialization, migration idempotency,
  reattach restores split topology, `LayoutChanged` reaches
  both clients, drag-resize batching does not flood the daemon.

## Open questions

1. **Deep nesting.** Cap layout-tree depth? Both clients handle
   arbitrary depth, but JSON gets ugly above ~6 levels. Soft cap or
   document?
2. **Tabs.** `pyre-tui` has tabs (`Tab`, `main.rs:821`); GPU does not.
   M7 ships single-root layout; multi-tab persistence is M8.
3. **Concurrent mutations.** Two clients splitting the same pane
   race in `pyred`. Recommendation: serialize via an async mutex on
   `SessionState.layout`. Document; defer conflict-resolution UX.
4. **`close_pane` legacy.** New layout-collapse behaviour is a no-op
   for sessions whose `layout` column is `NULL`, so ship unconditionally.

## References

- ADRs [001](ADR-001-ipc.md), [002](0002-daemon-process-architecture.md),
  [003](0003-render-backend.md), [004](0004-remote-attach.md).
- `pyre-tui/src/main.rs:788` (TUI `LayoutNode`),
  `pyre-gpu/src/layout.rs:52` (parallel impl),
  `pyre-proto/src/service.rs:139` (`open_pane`),
  `pyre-proto/src/handshake.rs:12` (`PROTO_VERSION`),
  `pyred/migrations/0001_init.sql` (schema),
  `pyre-mcp/src/main.rs:984` (flat `session_layout`).
- `.claude/notions/m7-layout-design.md` — file:line implementation map.
