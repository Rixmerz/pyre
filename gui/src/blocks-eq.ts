// Pure structural equality + running detection for block snapshots.
//
// Extracted from session-ops.ts (exactly like pane-state-eq.ts) so it can be
// unit-tested headless: session-ops transitively imports xterm + Tauri (DOM-only)
// modules, but THESE predicates are pure (arrays in, boolean out) and gate
// whether a 750 ms focused-blocks poll tick triggers any re-render at all.

import type { Block } from "./types";

/**
 * "Stable equality" for the focused-pane block list: true when both lists hold
 * the same blocks, in the same order, with identical STABLE fields — id, running
 * flag, exit_code, ended_at, command, started_at. Deliberately EXCLUDES a running
 * block's computed elapsed time (which is not a field — it's derived from
 * started_at vs `Date.now()` and patched in place by `applyBlockElapsedInPlace`,
 * mirroring how `paneStatesEqual` excludes nothing volatile). Both inputs are
 * sorted newest-first by the caller, so the comparison is order-sensitive.
 *
 * A `true` result means: nothing the block list RENDERS structurally has moved —
 * the gate may skip `setState` (so no notify/renderAll) PROVIDED nothing is
 * running. A `false` result (block added/removed/finished, command changed, …)
 * is the signal to repaint.
 */
export function blocksStableEqual(
  a: readonly Block[],
  b: readonly Block[],
): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    const x = a[i];
    const y = b[i];
    if (x.id !== y.id) return false;
    if (x.running !== y.running) return false;
    if ((x.exit_code ?? null) !== (y.exit_code ?? null)) return false;
    if ((x.ended_at ?? null) !== (y.ended_at ?? null)) return false;
    if (x.command !== y.command) return false;
    if (x.started_at !== y.started_at) return false;
  }
  return true;
}

/**
 * True iff any block is still running — no exit code AND not ended. Uses the SAME
 * definition the render layer keys off (`render/blocks.ts` / the in-place elapsed
 * updater both treat `exit_code == null && ended_at == null` as running), NOT the
 * daemon's `running` boolean, so it fires exactly when a card's elapsed clock
 * needs to keep advancing. The gate uses this to keep notifying every poll tick
 * while a block runs — `renderAll` → `applyBlockElapsedInPlace` then ticks the
 * counter in place even though `listFingerprint` (which excludes elapsed) would
 * otherwise skip the rebuild.
 */
export function hasRunningBlock(blocks: readonly Block[]): boolean {
  return blocks.some((b) => b.exit_code == null && b.ended_at == null);
}
