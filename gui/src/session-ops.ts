// Session orchestration: load sessions, layouts, pane states, and blocks from
// the daemon into the store, and attach an output stream per leaf pane. These
// are the side-effecting loaders the actions and the poll loop call.

import {
  attachPaneStream,
  detachPaneStream,
  listBlocks,
  listSessions,
  listWindows,
  paneStates,
  windowLayout,
} from "./api";
import { getState, setState, windowTabs, activeWindowOf } from "./state";
import { disposePaneTerminal, mountedPanes } from "./terminals";
import { maybeNotifyTransition, forgetPane } from "./notify";
import { dlog } from "./debug";
import { paneStatesEqual } from "./pane-state-eq";
import type {
  LayoutNode,
  LifecycleEvent,
  PaneStateInfo,
  PaneState,
  WindowInfo,
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
 *  dropped, and their cached windows + window-keyed layouts are evicted so a
 *  removed session can't linger in the rail or in `state.layouts`. If the active
 *  session vanished, the active pointer is cleared so the caller can pick a new
 *  one. */
export async function reloadSessions(): Promise<void> {
  try {
    const sessions = await listSessions();
    const liveIds = new Set(sessions.map((s) => s.id));

    // Evict windows + window-keyed layouts for sessions that no longer exist.
    const windows = new Map(getState().windows);
    const activeWindow = new Map(getState().activeWindow);
    const layouts = new Map(getState().layouts);
    for (const sid of [...windows.keys()]) {
      if (liveIds.has(sid)) continue;
      for (const w of windows.get(sid) ?? []) layouts.delete(w.id);
      windows.delete(sid);
      activeWindow.delete(sid);
    }

    const active = getState().activeSession;
    const patch: Parameters<typeof setState>[0] = {
      sessions,
      windows,
      activeWindow,
      layouts,
    };
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

/** Reload just the WINDOW list for a session (names, positions, pane counts)
 *  into the store, reconciling the active-window pointer. Returns the windows so
 *  callers can iterate them (e.g. to fetch each window's layout). */
export async function reloadWindows(session: string): Promise<WindowInfo[]> {
  const windows = await listWindows(session);
  const winMap = new Map(getState().windows);
  winMap.set(session, windows);

  // Reconcile the active window: keep it if still present, else fall back to the
  // first window (or clear it when the session has no windows left).
  const activeWindow = new Map(getState().activeWindow);
  const cur = activeWindow.get(session);
  if (windows.length === 0) {
    activeWindow.delete(session);
  } else if (!cur || !windows.some((w) => w.id === cur)) {
    activeWindow.set(session, windows[0]!.id);
  }

  setState({ windows: winMap, activeWindow });
  return windows;
}

/** Reload a session's windows AND each window's layout tree, then ensure every
 *  window's leaf panes have a live stream (so hidden window tabs keep buffering
 *  output, exactly as the old standalone tabs did). */
export async function reloadSession(session: string): Promise<void> {
  try {
    const windows = await reloadWindows(session);
    const layouts = new Map(getState().layouts);
    for (const w of windows) {
      try {
        layouts.set(w.id, await windowLayout(w.id));
      } catch (err) {
        console.error(`get_window_layout(${w.id}) failed:`, err);
      }
    }
    setState({ layouts });

    const allLeaves = windows.flatMap((w) => leafPanes(layouts.get(w.id)));
    await ensureStreams(session, allLeaves);
  } catch (err) {
    console.error(`reload windows for session(${session}) failed:`, err);
  }
}

/**
 * Attach a stream for every given pane of the session that doesn't already have
 * a terminal. Callers pass the union of every window's layout leaves, so hidden
 * window tabs keep a live stream and buffer output even while off-screen.
 */
async function ensureStreams(
  session: string,
  panes: string[],
): Promise<void> {
  const live = mountedPanes();
  await Promise.all(
    [...new Set(panes)]
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

    // Update title — the user-assigned name wins over the daemon title, falling
    // back to the title, else "pane". Skip while an inline rename editor has
    // replaced the label (the span is detached; nothing to update mid-edit).
    const titleEl = card.querySelector<HTMLElement>(".pane-title");
    if (titleEl) {
      const name = (info.name ?? "").trim();
      titleEl.textContent = name || info.title || "pane";
    }

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

/** Focus the first leaf pane of a session's ACTIVE window (after switch/spawn/
 *  close / window switch). */
export function focusFirstLeaf(session: string): void {
  const win = activeWindowOf(session);
  const layout = win ? getState().layouts.get(win) : undefined;
  const first = leafPanes(layout)[0] ?? null;
  // Clear stale blocks on ANY focus change to a new pane (not just switchSession)
  // so the inspector never shows the previous pane's blocks while the new pane's
  // blocks load. The poll's reloadFocusedBlocks (or switchSession's immediate
  // call) then fills in the new pane's blocks.
  setState({ focusedPane: first, blocks: [] });
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

  // Tear down terminals + streams for every pane the session held — across all
  // of its windows' layout trees — so nothing lingers.
  const sessionWindows = windowTabs(session);
  const panes = new Set(
    sessionWindows.flatMap((w) => leafPanes(st.layouts.get(w.id))),
  );
  for (const pane of panes) {
    detachPaneStream(pane).catch(() => {
      /* pane already gone — ignore */
    });
    disposePaneTerminal(pane);
  }

  // Drop from the session list, every window-keyed layout, the per-session
  // window list, and the active-window map in one atomic patch.
  const sessions = st.sessions.filter((s) => s.id !== session);
  const layouts = new Map(st.layouts);
  for (const w of sessionWindows) layouts.delete(w.id);
  const windows = new Map(st.windows);
  windows.delete(session);
  const activeWindow = new Map(st.activeWindow);
  activeWindow.delete(session);

  if (wasActive) {
    const next = sessions[0]?.id ?? null;
    setState({
      sessions,
      layouts,
      windows,
      activeWindow,
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
    setState({ sessions, layouts, windows, activeWindow });
  }
}

/**
 * Apply a single daemon lifecycle event to the store, driving INSTANT updates
 * (closed sessions vanish immediately, layouts reload on split/close, heat
 * updates in place) rather than waiting for the periodic poll.
 */
export async function applyLifecycleEvent(ev: LifecycleEvent): Promise<void> {
  switch (ev.kind) {
    case "spawned": {
      // A new pane/session/window appeared. Refresh the session list (so a
      // brand-new session shows in the rail) and pane_states, then reload the
      // affected session's windows + layouts — which also attaches a stream for
      // every window's leaves.
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
        // Session survives but lost a pane. Refresh pane_states, then reload the
        // session's windows + layouts: the closed pane collapses its window's
        // tree (or drops the window entirely if it was that window's last pane),
        // and reloadWindows reconciles the active-window pointer.
        await reloadPaneStates();
        if (getState().activeSession === session) {
          await reloadSession(session);
          if (getState().focusedPane === pane) focusFirstLeaf(session);
        } else {
          // Inactive session lost a pane — refresh just its window list so a
          // dropped window leaves the cached tab strip without a layout fetch.
          await reloadWindows(session).catch((err) =>
            console.error(`reload windows(${session}) failed:`, err),
          );
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
          name: prev?.name ?? null,
          window: prev?.window,
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
