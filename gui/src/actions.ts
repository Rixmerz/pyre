// The action catalog. Every user-triggerable operation lives here exactly once;
// the command palette, pane toolbars, rail buttons, and keybindings all dispatch
// through these functions. This is the single inventory the ⌘K palette renders.

import {
  closePane,
  closeSession,
  detachPaneStream,
  openPane,
  openSplit,
  renameSession,
  searchBlocks,
  sendKeys,
  spawnSession,
} from "./api";
import {
  getState,
  setState,
  SPLIT_TAB,
  activeTabOf,
} from "./state";
import {
  disposePaneTerminal,
  focusPaneTerminal,
  refitAll,
} from "./terminals";
import { selectTheme } from "./themes";
import {
  leafPanes,
  reloadSession,
  reloadSessions,
  reloadPaneStates,
  focusFirstLeaf,
} from "./session-ops";
import { dlog } from "./debug";

// ── Session actions ─────────────────────────────────────────────────────────

export async function newSession(): Promise<void> {
  dlog("[pyre-session] new-session: start");
  try {
    const s = await spawnSession(80, 24);
    dlog("[pyre-session] new-session: spawn result", s.session, "pane", s.pane);
    await reloadSessions();
    setState({ activeSession: s.session });
    dlog("[pyre-session] new-session: adopted active=", s.session);
    await reloadSession(s.session);
    focusFirstLeaf(s.session);
    dlog("[pyre-session] new-session: rendered session=", s.session);
  } catch (err) {
    console.error("spawn_session failed:", err);
  }
}

export async function switchSession(session: string): Promise<void> {
  setState({ activeSession: session, zoomedPane: null });
  await reloadSession(session);
  focusFirstLeaf(session);
}

export async function promptRenameSession(session: string): Promise<void> {
  const s = getState().sessions.find((x) => x.id === session);
  const next = window.prompt("Rename session", s?.name ?? "");
  if (next == null || next.trim() === "") return;
  try {
    await renameSession(session, next.trim());
    await reloadSessions();
  } catch (err) {
    console.error("rename_session failed:", err);
  }
}

/**
 * Fully terminate a session: kill all its panes on the daemon, tear down its
 * xterm instances + output streams locally, reload the session list, and pick a
 * new active session (spawning a fresh one if none remain).
 */
export async function closeSessionAction(session: string): Promise<void> {
  dlog("[pyre-session] closeSession action fired:", session);

  // Capture the leaves BEFORE we drop the layout, so we can detach each stream.
  const leaves = leafPanes(getState().layouts.get(session));
  const wasActive = getState().activeSession === session;

  try {
    dlog("[pyre-session] invoking close_session command:", session);
    await closeSession(session);
    dlog("[pyre-session] close_session command ok:", session);
  } catch (err) {
    console.error("[pyre-session] close_session command FAILED:", session, err);
    return;
  }

  // Detach each leaf's output stream and dispose its xterm so nothing lingers.
  for (const pane of leaves) {
    detachPaneStream(pane).catch((err) =>
      console.error(`[pyre-session] detach_pane_stream(${pane}) failed:`, err),
    );
    disposePaneTerminal(pane);
  }

  // Drop the closed session's cached layout.
  const layouts = new Map(getState().layouts);
  layouts.delete(session);
  setState({ layouts });

  // Reload the session list from the daemon (authoritative post-close view).
  await reloadSessions();
  const remaining = getState().sessions;
  dlog(
    "[pyre-session] session list reloaded, new count:",
    remaining.length,
  );

  // If we closed the active session, switch to another one — or spawn a fresh
  // one if the close left us with no sessions at all.
  if (wasActive) {
    const next = remaining[0]?.id ?? null;
    if (next) {
      dlog("[pyre-session] active session switched to:", next);
      setState({ activeSession: next, focusedPane: null, zoomedPane: null });
      await reloadSession(next);
      focusFirstLeaf(next);
    } else {
      dlog("[pyre-session] no sessions remain — spawning a fresh one");
      setState({ activeSession: null, focusedPane: null, zoomedPane: null });
      await newSession();
    }
  }
  refitAll();
}

// ── Pane actions ────────────────────────────────────────────────────────────

export async function splitRight(pane: string | null): Promise<void> {
  const target = pane ?? getState().focusedPane;
  if (!target) return;
  // "Split right" = new pane side-by-side → daemon VSplit → wire dir "v".
  // (The daemon names by split AXIS, not child arrangement: VSplit = columns.)
  await doSplit(target, "v");
}

export async function splitDown(pane: string | null): Promise<void> {
  const target = pane ?? getState().focusedPane;
  if (!target) return;
  // "Split down" = new pane stacked below → daemon HSplit → wire dir "h".
  await doSplit(target, "h");
}

async function doSplit(pane: string, orient: "h" | "v"): Promise<void> {
  const session = getState().activeSession;
  if (!session) return;
  dlog("[pyre-split] action orient=", orient, "pane=", pane);
  try {
    const result = await openSplit(pane, orient);
    const newPane = result.pane;
    dlog("[pyre-split] daemon new pane=", newPane);
    await reloadSession(session);
    const layout = getState().layouts.get(session);
    logLayoutTree(layout, 0);
    setState({ focusedPane: newPane });
    refitAll();
  } catch (err) {
    console.error("[pyre-split] open_split failed:", err);
  }
}

function logLayoutTree(node: import("./types").LayoutNode | undefined, depth: number): void {
  if (!node) { dlog("[pyre-split] tree: (none)"); return; }
  const indent = "  ".repeat(depth);
  if (node.kind === "leaf") {
    dlog(`[pyre-split] ${indent}leaf pane=${node.pane}`);
  } else {
    const cssDir = node.dir === "v" ? "row (split-h)" : "column (split-v)";
    dlog(`[pyre-split] ${indent}split dir=${node.dir} flex=${cssDir} weights=[${(node.weights ?? []).join(",")}]`);
    node.children.forEach(c => logLayoutTree(c, depth + 1));
  }
}

export function zoomPane(pane: string | null): void {
  const target = pane ?? getState().focusedPane;
  if (!target) return;
  const cur = getState().zoomedPane;
  setState({ zoomedPane: cur === target ? null : target, focusedPane: target });
  refitAll();
}

export function unzoom(): void {
  if (getState().zoomedPane) {
    setState({ zoomedPane: null });
    refitAll();
  }
}

export async function closePaneAction(pane: string | null): Promise<void> {
  const target = pane ?? getState().focusedPane;
  if (!target) return;
  const session = getState().activeSession;
  try {
    await closePane(target);
  } catch (err) {
    console.error("close_pane failed:", err);
  }
  disposePaneTerminal(target);
  if (getState().zoomedPane === target) setState({ zoomedPane: null });
  await reloadSessions();
  if (session) {
    // The session may now be gone (last pane closed); guard the reload.
    const stillThere = getState().sessions.some((s) => s.id === session);
    if (stillThere) {
      await reloadSession(session);
      focusFirstLeaf(session);
    } else {
      const first = getState().sessions[0]?.id ?? null;
      setState({ activeSession: first, focusedPane: null, zoomedPane: null });
      if (first) {
        await reloadSession(first);
        focusFirstLeaf(first);
      } else {
        // Last pane of the last session was closed — spawn a fresh session so
        // the UI is never stranded at the empty state with no auto-recovery.
        dlog("[pyre-session] last pane closed, no sessions remain — spawning fresh session");
        await newSession();
      }
    }
  }
}

export function focusPane(pane: string): void {
  setState({ focusedPane: pane });
  focusPaneTerminal(pane);
}

// ── Tab actions (split tab + standalone panes) ────────────────────────────────

/** Switch the active session's tab. `tab` is SPLIT_TAB or a standalone paneId. */
export function switchTab(tab: string): void {
  const session = getState().activeSession;
  if (!session) return;
  const activeTab = new Map(getState().activeTab);
  activeTab.set(session, tab);
  setState({ activeTab });
  if (tab === SPLIT_TAB) {
    // Returning to the split tab: focus its first leaf so keystrokes route.
    focusFirstLeaf(session);
  } else {
    // A standalone pane tab is a single full-area pane — focus it directly.
    focusPane(tab);
  }
  // The center render mounts the tab's terminal(s); refit once it settles.
  requestAnimationFrame(() => refitAll());
}

/**
 * Open a NEW standalone pane in the active session and switch to its tab. Wired
 * DEFENSIVELY: if the daemon lacks `open_pane` the invoke rejects, we catch +
 * dlog, and the tab strip is unaffected (the `+` pill simply did nothing).
 */
export async function newPaneAction(): Promise<void> {
  const session = getState().activeSession;
  if (!session) return;
  dlog("[pyre-tab] new-pane: start session=", session);
  try {
    const res = await openPane(session, 80, 24);
    const pane = res.pane;
    dlog("[pyre-tab] new-pane: daemon pane=", pane);
    // Refresh pane_states so the new standalone pane is known, then refresh the
    // session layout + streams (covers the standalone set too). The lifecycle
    // "spawned" event will also fire, but we update eagerly for instant feedback.
    await reloadPaneStates();
    await reloadSession(session);
    switchTab(pane);
  } catch (err) {
    // open_pane not implemented yet (parallel agent) — degrade gracefully.
    dlog("[pyre-tab] new-pane: open_pane unavailable, no-op:", err);
  }
}

/**
 * Close a standalone pane tab. Kills the pane on the daemon, disposes its
 * terminal, and — if it was the active tab — falls back to the split tab.
 */
export async function closeStandalonePane(pane: string): Promise<void> {
  const session = getState().activeSession;
  try {
    await closePane(pane);
  } catch (err) {
    console.error("close_pane (standalone) failed:", pane, err);
  }
  disposePaneTerminal(pane);
  detachPaneStream(pane).catch(() => {
    /* pane already gone — ignore */
  });

  // If this was the active tab, return to the split tab (the safe default).
  if (session && activeTabOf(session) === pane) {
    const activeTab = new Map(getState().activeTab);
    activeTab.set(session, SPLIT_TAB);
    setState({ activeTab });
    focusFirstLeaf(session);
  }

  // Authoritative refresh so the closed pane drops from pane_states + the strip.
  await reloadPaneStates();
  if (session && getState().sessions.some((s) => s.id === session)) {
    await reloadSession(session);
  }
  refitAll();
}

// ── Block panel actions ─────────────────────────────────────────────────────

export async function runBlockSearch(query: string): Promise<void> {
  const trimmed = query.trim();
  setState({ blockQuery: query });
  if (trimmed === "") {
    setState({ searchResults: null });
    return;
  }
  try {
    const results = await searchBlocks(trimmed, false, getState().activeSession);
    setState({
      searchResults: results.map((h) => ({ ...h.block, snippet: h.snippet })),
    });
  } catch (err) {
    console.error("search_blocks failed:", err);
    setState({ searchResults: [] });
  }
}

/** Toggle the panel's "failures only" filter (non-zero exit codes). */
export function toggleFailuresOnly(): void {
  setState({ blocksFailuresOnly: !getState().blocksFailuresOnly });
}

/** Expand or collapse a single block's output preview. */
export function toggleBlockExpanded(blockId: string): void {
  const next = new Set(getState().expandedBlocks);
  if (next.has(blockId)) next.delete(blockId);
  else next.add(blockId);
  setState({ expandedBlocks: next });
}

/**
 * Rerun a block's command by sending it (plus Enter) to the block's pane. If the
 * pane is the focused one its terminal regains focus so the user sees it run.
 */
export function rerunBlock(pane: string, command: string): void {
  const bytes = Array.from(new TextEncoder().encode(command + "\n"));
  void sendKeys(pane, bytes).catch((e) =>
    console.error("rerun send_keys failed:", e),
  );
  if (getState().focusedPane === pane) focusPaneTerminal(pane);
}

// ── Agent overview overlay ────────────────────────────────────────────────────

export function openAgents(): void {
  setState({ agentsOpen: true });
}

export function closeAgents(): void {
  setState({ agentsOpen: false });
}

export function toggleAgents(): void {
  setState({ agentsOpen: !getState().agentsOpen });
}

/**
 * Jump to a pane anywhere across all sessions: switch to its session (if needed)
 * and focus it. Used by the agent overview rows.
 */
export async function gotoPane(session: string, pane: string): Promise<void> {
  setState({ agentsOpen: false });
  if (getState().activeSession !== session) {
    await switchSession(session);
  }
  focusPane(pane);
}

// ── Chrome toggles ──────────────────────────────────────────────────────────

export function toggleRail(): void {
  setState({ railCollapsed: !getState().railCollapsed });
  refitAll();
}

export function toggleRightPanel(): void {
  setState({ rightCollapsed: !getState().rightCollapsed });
  refitAll();
}

export function openPalette(): void {
  setState({ paletteOpen: true });
}

export function closePalette(): void {
  setState({ paletteOpen: false });
}

export function openThemePicker(): void {
  setState({ themePickerOpen: true });
}

export function closeThemePicker(): void {
  setState({ themePickerOpen: false });
}

// ── Command catalog (consumed by the ⌘K palette) ────────────────────────────

export interface Command {
  id: string;
  title: string;
  hint?: string;
  /** dynamic submenu: returns child commands instead of running */
  children?: () => Command[];
  run?: () => void | Promise<void>;
}

/** Build the full, context-aware command list for the palette. */
export function buildCommands(): Command[] {
  const s = getState();
  const cmds: Command[] = [
    { id: "new-session", title: "New session", hint: "spawn", run: newSession },
    {
      id: "new-pane",
      title: "New pane in this session",
      hint: "Ctrl+Shift+T",
      run: () => newPaneAction(),
    },
    {
      id: "split-right",
      title: "Split right",
      hint: "pane",
      run: () => splitRight(null),
    },
    {
      id: "split-down",
      title: "Split down",
      hint: "pane",
      run: () => splitDown(null),
    },
    { id: "zoom", title: "Zoom pane", hint: "pane", run: () => zoomPane(null) },
    {
      id: "close-pane",
      title: "Close pane",
      hint: "pane",
      run: () => closePaneAction(null),
    },
    {
      id: "rename-session",
      title: "Rename session",
      hint: "session",
      run: () => {
        if (s.activeSession) void promptRenameSession(s.activeSession);
      },
    },
    {
      id: "close-session",
      title: "Close session",
      hint: "session",
      run: () => {
        if (s.activeSession) void closeSessionAction(s.activeSession);
      },
    },
    {
      id: "toggle-rail",
      title: s.railCollapsed ? "Expand session rail" : "Collapse session rail",
      hint: "view",
      run: toggleRail,
    },
    {
      id: "toggle-blocks",
      title: s.rightCollapsed ? "Show blocks panel" : "Hide blocks panel",
      hint: "view",
      run: toggleRightPanel,
    },
    {
      id: "toggle-failures",
      title: s.blocksFailuresOnly
        ? "Show all blocks"
        : "Show failed blocks only",
      hint: "blocks",
      run: toggleFailuresOnly,
    },
    {
      id: "agent-overview",
      title: "Agent overview",
      hint: "Ctrl+Shift+A",
      run: openAgents,
    },
  ];

  // Switch session… submenu
  if (s.sessions.length > 0) {
    cmds.push({
      id: "switch-session",
      title: "Switch session…",
      hint: "session",
      children: () =>
        getState().sessions.map((sess) => ({
          id: `switch-${sess.id}`,
          title: sess.name,
          hint: `${sess.pane_count} pane${sess.pane_count === 1 ? "" : "s"}`,
          run: () => switchSession(sess.id),
        })),
    });
  }

  // Pick theme… submenu
  cmds.push({
    id: "pick-theme",
    title: "Pick theme…",
    hint: "appearance",
    children: () =>
      getState().themes.map((t) => ({
        id: `theme-${t.name}`,
        title: t.display_name,
        hint: t.kind,
        run: () => selectTheme(t.name),
      })),
  });

  return cmds;
}
