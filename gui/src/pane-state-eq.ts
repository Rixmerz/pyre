// Pure structural equality for pane-state snapshots.
//
// Extracted from session-ops.ts so it can be unit-tested headless: session-ops
// transitively imports xterm + Tauri (DOM-only) modules, but THIS predicate is
// pure (Maps in, boolean out) and is the gate that decides whether a heat poll
// tick triggers any re-render at all — exactly the kind of logic worth a test.

import type { PaneStateInfo } from "./types";

/**
 * Shallow equality check for pane-state maps. Two snapshots are equal when they
 * have the same panes and, for each, the same `state`, `title`, and `agent`
 * (treating missing/undefined agent as null). Anything else differing — a pane
 * added or removed, a state change, an agent change — makes them unequal, which
 * is the signal to re-render.
 */
export function paneStatesEqual(
  a: Map<string, PaneStateInfo>,
  b: Map<string, PaneStateInfo>,
): boolean {
  if (a.size !== b.size) return false;
  for (const [pane, infoA] of a) {
    const infoB = b.get(pane);
    if (!infoB) return false;
    if (infoA.state !== infoB.state) return false;
    if (infoA.title !== infoB.title) return false;
    if ((infoA.agent ?? null) !== (infoB.agent ?? null)) return false;
  }
  return true;
}
