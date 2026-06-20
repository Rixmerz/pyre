// Session orchestration: load sessions, layouts, pane states, and blocks from
// the daemon into the store, and attach an output stream per leaf pane. These
// are the side-effecting loaders the actions and the poll loop call.

import {
  attachPaneStream,
  listBlocks,
  listSessions,
  paneStates,
  sessionLayout,
} from "./api";
import { getState, setState } from "./state";
import { mountedPanes } from "./terminals";
import type { LayoutNode, PaneStateInfo } from "./types";

/** Walk a layout tree and collect every leaf pane id, in render order. */
export function leafPanes(node: LayoutNode | undefined): string[] {
  if (!node) return [];
  if (node.kind === "leaf") return [node.pane];
  return node.children.flatMap((c) => leafPanes(c));
}

/** Reload the session list into the store. */
export async function reloadSessions(): Promise<void> {
  try {
    const sessions = await listSessions();
    setState({ sessions });
  } catch (err) {
    console.error("list_sessions failed:", err);
  }
}

/** Reload one session's layout, then ensure each leaf pane has a live stream. */
export async function reloadSession(session: string): Promise<void> {
  try {
    const layout = await sessionLayout(session);
    const layouts = new Map(getState().layouts);
    layouts.set(session, layout);
    setState({ layouts });
    await ensureStreams(session, layout);
  } catch (err) {
    console.error(`session_layout(${session}) failed:`, err);
  }
}

/** Attach a stream for every leaf pane that doesn't already have a terminal. */
async function ensureStreams(
  session: string,
  layout: LayoutNode,
): Promise<void> {
  const live = mountedPanes();
  const leaves = leafPanes(layout);
  await Promise.all(
    leaves
      .filter((pane) => !live.has(pane))
      .map((pane) =>
        attachPaneStream(session, pane).catch((err) =>
          console.error(`attach_pane_stream(${pane}) failed:`, err),
        ),
      ),
  );
}

/** Poll pane states across all sessions into the store (drives heat).
 *
 *  IMPORTANT: paneStates updates MUST NOT trigger a full layout re-render —
 *  doing so causes every pane card to be torn down and recreated, which
 *  re-parents the xterm DOM node (blurring its hidden textarea) and breaks
 *  keyboard input. We therefore:
 *   1. Skip setState entirely when the serialised snapshot is identical.
 *   2. When it does differ, apply heat/state updates IN-PLACE on existing
 *      pane-card elements rather than letting renderCenter reconstruct the tree.
 */
export async function reloadPaneStates(): Promise<void> {
  try {
    const list = await paneStates();
    const map = new Map<string, PaneStateInfo>();
    for (const ps of list) map.set(ps.pane, ps);

    // Serialize both maps for a cheap structural equality check. If the
    // snapshot is identical to what we already have, skip setState entirely
    // so that notify() / renderAll() / renderCenter() are never called.
    const current = getState().paneStates;
    if (paneStatesEqual(current, map)) {
      return; // [pyre-render] heat-only tick — no state change, no re-render
    }

    // Something changed. Apply heat updates IN-PLACE on already-rendered pane
    // cards so we don't have to tear down the layout tree and re-parent xterms.
    applyHeatInPlace(map);

    // Persist to state so derived views (status bar, rail) see fresh data.
    // renderCenter is guarded separately to prevent a structural re-render when
    // only paneStates changed (see center.ts renderCenter guard).
    setState({ paneStates: map });
  } catch (err) {
    console.error("pane_states failed:", err);
  }
}

/** Shallow equality check for pane state maps. */
function paneStatesEqual(
  a: Map<string, PaneStateInfo>,
  b: Map<string, PaneStateInfo>,
): boolean {
  if (a.size !== b.size) return false;
  for (const [pane, infoA] of a) {
    const infoB = b.get(pane);
    if (!infoB) return false;
    if (infoA.state !== infoB.state) return false;
    if (infoA.title !== infoB.title) return false;
  }
  return true;
}

/**
 * Apply heat/state changes directly on the existing pane-card DOM elements.
 * This avoids a full layout re-render (and the xterm re-parent / focus-loss
 * that would follow) when only heat/state changed but the layout is the same.
 */
function applyHeatInPlace(map: Map<string, PaneStateInfo>): void {
  for (const [pane, info] of map) {
    const card = document.querySelector<HTMLElement>(
      `.pane-card[data-pane="${CSS.escape(pane)}"]`,
    );
    if (!card) continue; // pane not yet rendered or layout changed — full render will handle it

    const heat = heatVarForState(info.state);
    card.style.setProperty("--heat", heat);
    card.dataset["state"] = info.state;

    // Toggle pulse class without touching the rest of the tree.
    const shouldPulse = info.state === "waiting" || info.state === "crashed";
    card.classList.toggle("pulse", shouldPulse);

    // Update the dot heat
    const dot = card.querySelector<HTMLElement>(".pane-dot");
    if (dot) dot.style.setProperty("--dot-heat", heat);

    // Update state label
    const lbl = card.querySelector<HTMLElement>(".pane-state-label");
    if (lbl) lbl.textContent = stateLabel(info.state);

    // Update title
    const titleEl = card.querySelector<HTMLElement>(".pane-title");
    if (titleEl) titleEl.textContent = info.title || "pane";

    console.log("[pyre-render] heat-in-place pane", pane, info.state);
  }
}

/** Inline heat-var lookup (mirrors heat.ts to avoid circular import). */
function heatVarForState(state: string): string {
  switch (state) {
    case "running": return "var(--heat-running)";
    case "waiting": return "var(--heat-waiting)";
    case "interactive": return "var(--heat-interactive)";
    case "crashed": return "var(--heat-crashed)";
    case "done": return "var(--heat-done)";
    default: return "var(--heat-idle)";
  }
}

/** Human-readable label (mirrors heat.ts to avoid circular import). */
function stateLabel(state: string): string {
  switch (state) {
    case "running": return "Running";
    case "waiting": return "Waiting for input";
    case "interactive": return "Interactive";
    case "crashed": return "Crashed";
    case "done": return "Done";
    default: return "Idle";
  }
}

/** Poll the focused pane's blocks into the store (drives the inspector). */
export async function reloadFocusedBlocks(): Promise<void> {
  const pane = getState().focusedPane;
  if (!pane) {
    setState({ blocks: [] });
    return;
  }
  try {
    const blocks = await listBlocks(pane);
    // Newest-first: the daemon returns ascending; reverse for display.
    blocks.sort((a, b) => b.started_at.localeCompare(a.started_at));
    setState({ blocks });
  } catch (err) {
    console.error(`list_blocks(${pane}) failed:`, err);
  }
}

/** Focus the first leaf pane of a session (after switch/spawn/close). */
export function focusFirstLeaf(session: string): void {
  const layout = getState().layouts.get(session);
  const first = leafPanes(layout)[0] ?? null;
  setState({ focusedPane: first });
}
