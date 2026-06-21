// Session orchestration: load sessions, layouts, pane states, and blocks from
// the daemon into the store, and attach an output stream per leaf pane. These
// are the side-effecting loaders the actions and the poll loop call.

import {
  attachPaneStream,
  detachPaneStream,
  listBlocks,
  listSessions,
  paneStates,
  sessionLayout,
} from "./api";
import { getState, setState, standalonePanes, SPLIT_TAB } from "./state";
import { disposePaneTerminal, mountedPanes } from "./terminals";
import { maybeNotifyTransition, forgetPane } from "./notify";
import { dlog } from "./debug";
import { paneStatesEqual } from "./pane-state-eq";
import type {
  LayoutNode,
  LifecycleEvent,
  PaneStateInfo,
  PaneState,
} from "./types";

/** Walk a layout tree and collect every leaf pane id, in render order. */
export function leafPanes(node: LayoutNode | undefined): string[] {
  if (!node) return [];
  if (node.kind === "leaf") return [node.pane];
  return node.children.flatMap((c) => leafPanes(c));
}

/** Reload the session list into the store.
 *
 *  This FULLY REPLACES the list — sessions the daemon no longer reports are
 *  dropped, and their cached layouts are evicted so a removed session can't
 *  linger in the rail or in `state.layouts`. If the active session vanished,
 *  the active pointer is cleared so the caller can pick a new one. */
export async function reloadSessions(): Promise<void> {
  try {
    const sessions = await listSessions();
    const liveIds = new Set(sessions.map((s) => s.id));

    // Evict cached layouts for sessions that no longer exist.
    const layouts = new Map(getState().layouts);
    for (const id of [...layouts.keys()]) {
      if (!liveIds.has(id)) layouts.delete(id);
    }

    const active = getState().activeSession;
    const patch: Parameters<typeof setState>[0] = { sessions, layouts };
    if (active && !liveIds.has(active)) {
      patch.activeSession = null;
      patch.focusedPane = null;
      patch.zoomedPane = null;
    }
    setState(patch);
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

/**
 * Attach a stream for every pane of the session that doesn't already have a
 * terminal — BOTH the layout-tree leaves AND the standalone panes (the ones
 * surfaced as their own tabs). A standalone pane lives in `paneStates` for the
 * session but is not a leaf of the layout tree; without this its tab would mount
 * a terminal that never receives output.
 */
async function ensureStreams(
  session: string,
  layout: LayoutNode,
): Promise<void> {
  const live = mountedPanes();
  const leaves = leafPanes(layout);
  const standalone = standalonePanes(session);
  // De-dupe (a pane can only be in one set, but be defensive).
  const all = [...new Set([...leaves, ...standalone])];
  await Promise.all(
    all
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

// paneStatesEqual lives in ./pane-state-eq (pure, headless-testable) and is
// imported above; re-exported here so existing importers keep working.
export { paneStatesEqual };

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

    // Update the per-pane agent chip in place (text + tint class).
    const chip = card.querySelector<HTMLElement>(".pane-agent-chip");
    if (chip) {
      const agent = (info.agent ?? "").trim() || "unknown";
      chip.textContent = agent;
      chip.title = `agent: ${agent}`;
      chip.className = `agent-chip pane-agent-chip agent-${agentChipKind(agent)}`;
    }

    dlog("[pyre-render] heat-in-place pane", pane, info.state);
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

/** Agent chip-kind class (mirrors agents.ts chipKind to avoid circular import). */
function agentChipKind(agent: string): string {
  const a = agent.toLowerCase();
  if (a === "claude") return "claude";
  if (a === "shell") return "shell";
  return "unknown";
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

// ── Event-driven lifecycle ───────────────────────────────────────────────────

/**
 * Remove a session from the store IMMEDIATELY (rail + layouts + active pointer)
 * without waiting for the next `list_sessions` poll. Disposes any terminals and
 * detaches any streams that belonged to it. If the removed session was active,
 * switches to another (spawning a fresh one if none remain).
 */
async function removeSessionNow(session: string): Promise<void> {
  const st = getState();
  const wasActive = st.activeSession === session;

  // Tear down terminals + streams for every pane the session held — both layout
  // leaves AND standalone (tabbed) panes — so nothing lingers.
  const panes = new Set([
    ...leafPanes(st.layouts.get(session)),
    ...standalonePanes(session),
  ]);
  for (const pane of panes) {
    detachPaneStream(pane).catch(() => {
      /* pane already gone — ignore */
    });
    disposePaneTerminal(pane);
  }

  // Drop from the session list, the layout cache, and the per-session active-tab
  // map in one atomic patch.
  const sessions = st.sessions.filter((s) => s.id !== session);
  const layouts = new Map(st.layouts);
  layouts.delete(session);
  const activeTab = new Map(st.activeTab);
  activeTab.delete(session);

  if (wasActive) {
    const next = sessions[0]?.id ?? null;
    setState({
      sessions,
      layouts,
      activeTab,
      activeSession: next,
      focusedPane: null,
      zoomedPane: null,
    });
    if (next) {
      await reloadSession(next);
      focusFirstLeaf(next);
    } else {
      // No sessions remain — spawn a fresh one so the UI is never empty.
      const { newSession } = await import("./actions");
      await newSession();
    }
  } else {
    setState({ sessions, layouts, activeTab });
  }
}

/**
 * If a session's active tab points at a standalone pane that no longer exists
 * (closed), fall back to the split tab so the center never renders a dead pane.
 * Returns true if the active tab was reset.
 */
function reconcileActiveTab(session: string): boolean {
  const active = getState().activeTab.get(session);
  if (!active || active === SPLIT_TAB) return false;
  // The active tab is a standalone pane id — still present?
  const stillThere = standalonePanes(session).includes(active);
  if (stillThere) return false;
  const activeTab = new Map(getState().activeTab);
  activeTab.set(session, SPLIT_TAB);
  setState({ activeTab });
  if (getState().activeSession === session) focusFirstLeaf(session);
  return true;
}

/**
 * Apply a single daemon lifecycle event to the store, driving INSTANT updates
 * (closed sessions vanish immediately, layouts reload on split/close, heat
 * updates in place) rather than waiting for the periodic poll.
 */
export async function applyLifecycleEvent(ev: LifecycleEvent): Promise<void> {
  switch (ev.kind) {
    case "spawned": {
      // A new pane/session appeared. Refresh the session list (so a brand-new
      // session shows in the rail) and pane_states (so a new STANDALONE pane is
      // known before ensureStreams runs), then reload the affected session's
      // layout — which also attaches streams for the standalone set.
      await reloadSessions();
      await reloadPaneStates();
      const session = ev.session ?? getState().activeSession;
      if (session && getState().sessions.some((s) => s.id === session)) {
        await reloadSession(session);
      }
      // If nothing was active yet, adopt the new session.
      if (!getState().activeSession && session) {
        setState({ activeSession: session });
        await reloadSession(session);
        focusFirstLeaf(session);
      }
      break;
    }

    case "closed": {
      const pane = ev.pane;
      if (pane) {
        disposePaneTerminal(pane);
        forgetPane(pane);
      }

      // Resolve which session the closed pane belonged to: prefer the event's
      // own session, fall back to cached pane-state.
      const session =
        ev.session ?? (pane ? getState().paneStates.get(pane)?.session : undefined);

      // Authoritative recount from the daemon.
      await reloadSessions();
      const live = getState().sessions.find((s) => s.id === session);

      if (session && (!live || live.pane_count === 0)) {
        // The session has no panes left — remove it from the rail NOW.
        await removeSessionNow(session);
      } else if (session) {
        // Session survives but lost a pane. Refresh pane_states first so the
        // standalone set is current, then reload the layout. A standalone pane
        // closing leaves the layout tree unchanged but must drop its tab.
        await reloadPaneStates();
        if (getState().activeSession === session) {
          await reloadSession(session);
          // If the active tab pointed at the closed standalone pane, fall back to
          // the split tab; otherwise keep focus valid if the closed pane was it.
          if (!reconcileActiveTab(session) && getState().focusedPane === pane) {
            focusFirstLeaf(session);
          }
        } else {
          // Inactive session lost a standalone pane — still reconcile its tab.
          reconcileActiveTab(session);
        }
      }
      break;
    }

    case "layout_changed": {
      const session = ev.session ?? getState().activeSession;
      if (session && getState().sessions.some((s) => s.id === session)) {
        await reloadSession(session);
      }
      break;
    }

    case "state_changed": {
      // Heat changed for a pane. Update the cached pane-state in place and let
      // the existing applyHeatInPlace path repaint without a structural render.
      if (ev.pane && ev.state) {
        const map = new Map(getState().paneStates);
        const prev = map.get(ev.pane);
        const nextState = ev.state as PaneState;
        const session =
          ev.session ?? prev?.session ?? getState().activeSession ?? "";
        const next: PaneStateInfo = {
          pane: ev.pane,
          session,
          state: nextState,
          title: prev?.title ?? null,
          agent: prev?.agent ?? null,
        };
        map.set(ev.pane, next);
        applyHeatInPlace(map);
        setState({ paneStates: map });
        // Agent-awareness: ping the user on a real transition into done/waiting.
        maybeNotifyTransition(ev.pane, prev?.state, nextState, session);
      } else {
        // Underspecified event — fall back to an authoritative pane-state poll.
        await reloadPaneStates();
      }
      break;
    }
  }
}
