// The action catalog. Every user-triggerable operation lives here exactly once;
// the command palette, pane toolbars, rail buttons, and keybindings all dispatch
// through these functions. This is the single inventory the ⌘K palette renders.

import {
  closePane,
  closeSession,
  closeWindow,
  detachPaneStream,
  newWindow,
  openPane,
  openSplit,
  renamePane,
  renameSession,
  renameWindow,
  searchBlocks,
  sendKeys,
  spawnSession,
} from "./api";
import {
  getState,
  setState,
  windowTabs,
  activeWindowOf,
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
  reloadWindows,
  reloadPaneStates,
  focusFirstLeaf,
  reloadFocusedBlocks,
} from "./session-ops";
import { dlog } from "./debug";
import { toast } from "./toast";
import { startGitHubLink, disconnectGitHub } from "./github-link";

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
    toast("Couldn't create the session — the daemon rejected it.", "error");
  }
}

export async function switchSession(session: string): Promise<void> {
  // Clear stale blocks synchronously so the inspector never flashes the OLD
  // session's blocks before the new pane's load.
  setState({ activeSession: session, zoomedPane: null, blocks: [] });
  await reloadSession(session);
  focusFirstLeaf(session);
  // Load the new pane's blocks at once (the same fn the ~750ms poll uses) so the
  // sequence is stale → empty → new, with no empty gap lingering until the poll.
  await reloadFocusedBlocks();
}

export async function promptRenameSession(session: string): Promise<void> {
  const s = getState().sessions.find((x) => x.id === session);
  const next = window.prompt("Rename session", s?.name ?? "");
  if (next == null || next.trim() === "") return;
  await renameSessionAction(session, next.trim());
}

/**
 * Rename a session and refresh the rail IMMEDIATELY (not waiting for the next
 * `list_sessions` poll) so the new name appears at once. Shared by the command
 * palette's prompt path and the rail's double-click inline editor.
 */
export async function renameSessionAction(
  session: string,
  name: string,
): Promise<void> {
  const next = name.trim();
  if (!next) return;
  dlog("[pyre-rename] commit session=", session, 'name="' + next + '"');
  try {
    await renameSession(session, next);
    dlog("[pyre-rename] rpc ok session=", session);
    // Refresh the rail immediately so the new name appears without waiting for
    // the next list_sessions poll.
    await reloadSessions();
    const afterName =
      getState().sessions.find((x) => x.id === session)?.name ?? null;
    dlog("[pyre-rename] after-reload session=", session, 'name="' + afterName + '"');
    if (afterName !== next) {
      dlog(
        "[pyre-rename] MISMATCH: sent='" + next + "' got='" + afterName +
        "' — daemon read-path did not reflect the session rename on reload",
      );
    }
  } catch (err) {
    dlog("[pyre-rename] rpc FAILED session=", session, err);
    toast("Rename failed.", "error");
  }
}

/**
 * Fully terminate a session: kill all its panes on the daemon, tear down its
 * xterm instances + output streams locally, reload the session list, and pick a
 * new active session (spawning a fresh one if none remain).
 */
export async function closeSessionAction(session: string): Promise<void> {
  dlog("[pyre-session] closeSession action fired:", session);

  // Capture the leaves across ALL of the session's windows BEFORE we drop the
  // layouts, so we can detach each stream.
  const sessionWindows = windowTabs(session);
  const leaves = sessionWindows.flatMap((w) =>
    leafPanes(getState().layouts.get(w.id)),
  );
  const wasActive = getState().activeSession === session;

  try {
    dlog("[pyre-session] invoking close_session command:", session);
    await closeSession(session);
    dlog("[pyre-session] close_session command ok:", session);
  } catch (err) {
    console.error("[pyre-session] close_session command FAILED:", session, err);
    toast("Couldn't close the session.", "error");
    return;
  }

  // Detach each leaf's output stream and dispose its xterm so nothing lingers.
  for (const pane of leaves) {
    detachPaneStream(pane).catch((err) =>
      console.error(`[pyre-session] detach_pane_stream(${pane}) failed:`, err),
    );
    disposePaneTerminal(pane);
  }

  // Drop the closed session's cached per-window layouts, its window list, and
  // its active-window pointer.
  const layouts = new Map(getState().layouts);
  for (const w of sessionWindows) layouts.delete(w.id);
  const windows = new Map(getState().windows);
  windows.delete(session);
  const activeWindow = new Map(getState().activeWindow);
  activeWindow.delete(session);
  setState({ layouts, windows, activeWindow });

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
    // The split mutated the ACTIVE window's tree on the daemon; reload the
    // session's windows + layouts and inspect the active window's new tree.
    await reloadSession(session);
    const win = activeWindowOf(session);
    const layout = win ? getState().layouts.get(win) : undefined;
    logLayoutTree(layout, 0);
    setState({ focusedPane: newPane });
    refitAll();
  } catch (err) {
    console.error("[pyre-split] open_split failed:", err);
    toast("Couldn't split the pane.", "error");
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
    toast("Couldn't close the pane.", "error");
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

/**
 * Rename a pane (standalone tab pill OR split-layout pane card). Wired
 * DEFENSIVELY: if the daemon lacks `rename_pane` the invoke rejects, we catch +
 * dlog, and the pane keeps its fallback label rather than breaking. On success
 * we refresh pane states IMMEDIATELY (not waiting for the next poll) so the new
 * name appears at once across the pane card, tab pill, and agent overview.
 */
export async function renamePaneAction(
  pane: string,
  name: string,
): Promise<void> {
  const next = name.trim();
  if (!next) return;
  dlog("[pyre-rename] commit pane=", pane, 'name="' + next + '"');
  try {
    await renamePane(pane, next);
    dlog("[pyre-rename] rpc ok pane=", pane);
    // Authoritative refresh so the committed name lands in paneStates and every
    // surface that reads it (tab pill, pane card, agent overview) repaints.
    await reloadPaneStates();
    // Verify the name the daemon returned for this pane after the reload.
    const afterName = getState().paneStates.get(pane)?.name ?? null;
    dlog("[pyre-rename] after-reload pane=", pane, 'name="' + afterName + '"');
    if (afterName !== next) {
      dlog(
        "[pyre-rename] MISMATCH: sent='" + next + "' got='" + afterName +
        "' — daemon read-path did not reflect the rename on the next poll",
      );
    }
  } catch (err) {
    dlog("[pyre-rename] rpc FAILED pane=", pane, err);
    toast("Rename failed.", "error");
  }
}

// ── Window actions (tab strip = a session's windows) ──────────────────────────

/** Switch the active session's visible WINDOW. Each window owns its own
 *  splittable layout tree; the center re-renders to show it and focus its first
 *  leaf so keystrokes route. */
export function switchWindow(windowId: string): void {
  const session = getState().activeSession;
  if (!session) return;
  const activeWindow = new Map(getState().activeWindow);
  activeWindow.set(session, windowId);
  setState({ activeWindow });
  // Focus the new active window's first leaf so keystrokes route there.
  focusFirstLeaf(session);
  // The center render mounts the window's terminal(s); refit once it settles.
  requestAnimationFrame(() => refitAll());
}

/**
 * Create a NEW window in the active session and switch to it. The daemon's
 * `new_window` makes an EMPTY window, so we immediately `open_pane` it to give
 * the window its first terminal. Wired DEFENSIVELY: if the bridge lacks
 * `new_window`/`open_pane` the invoke rejects, we catch + dlog, and the tab
 * strip is unaffected (the `+` pill simply did nothing).
 *
 * Named `newPaneAction` for call-site compatibility (keybind Ctrl+Shift+T and
 * the command palette) — "another terminal" now means "another window".
 */
export async function newPaneAction(): Promise<void> {
  const session = getState().activeSession;
  if (!session) return;
  dlog("[pyre-window] new-window: start session=", session);
  try {
    const windowId = await newWindow(session);
    dlog("[pyre-window] new-window: daemon window=", windowId);
    // Give the fresh (empty) window its first terminal.
    await openPane(windowId, session, 80, 24);
    // Refresh pane_states + the session's windows/layouts so the new window and
    // its pane are known, then switch to it. The "spawned" lifecycle event also
    // fires, but we update eagerly for instant feedback.
    await reloadPaneStates();
    await reloadSession(session);
    switchWindow(windowId);
  } catch (err) {
    // new_window / open_pane rejected (or the bridge predates the Window level).
    dlog("[pyre-window] new-window: failed:", err);
    toast("Couldn't open a new window.", "error");
  }
}

/**
 * Rename a window via the daemon-authoritative `rename_window`, then refresh the
 * session's window list so the new name appears at once. Wired DEFENSIVELY: a
 * missing/failed command keeps the old name rather than crashing.
 */
export async function renameWindowAction(
  windowId: string,
  name: string,
): Promise<void> {
  const next = name.trim();
  if (!next) return;
  const session = getState().activeSession;
  dlog("[pyre-rename] commit window=", windowId, 'name="' + next + '"');
  try {
    await renameWindow(windowId, next);
    if (session) {
      await reloadWindows(session).catch((err) =>
        console.error(`reload windows(${session}) failed:`, err),
      );
    }
  } catch (err) {
    dlog("[pyre-rename] window rpc FAILED window=", windowId, err);
    toast("Rename failed.", "error");
  }
}

/**
 * Close a window: kills all its panes on the daemon, tears down their terminals
 * + streams locally, drops the cached layout, and — if it was active — falls
 * back to another window (or another session, spawning a fresh one if the close
 * left the session, and the app, with none).
 */
export async function closeWindowAction(windowId: string): Promise<void> {
  const session = getState().activeSession;
  // Capture the window's leaves BEFORE we drop its layout so we can detach.
  const leaves = leafPanes(getState().layouts.get(windowId));
  try {
    await closeWindow(windowId);
  } catch (err) {
    console.error("close_window failed:", windowId, err);
    toast("Couldn't close the window.", "error");
  }
  for (const pane of leaves) {
    detachPaneStream(pane).catch(() => {
      /* pane already gone — ignore */
    });
    disposePaneTerminal(pane);
  }

  // Drop the closed window's cached layout + clear it if it was active.
  const layouts = new Map(getState().layouts);
  layouts.delete(windowId);
  const activeWindow = new Map(getState().activeWindow);
  if (session && activeWindow.get(session) === windowId) {
    activeWindow.delete(session);
  }
  setState({ layouts, activeWindow });

  // Authoritative refresh: the daemon may have evicted the session if that was
  // its last window.
  await reloadSessions();
  if (session && getState().sessions.some((s) => s.id === session)) {
    await reloadSession(session);
    focusFirstLeaf(session);
  } else if (session) {
    // The session is gone (its last window closed). Pick another, or spawn one.
    const first = getState().sessions[0]?.id ?? null;
    setState({ activeSession: first, focusedPane: null, zoomedPane: null });
    if (first) {
      await reloadSession(first);
      focusFirstLeaf(first);
    } else {
      await newSession();
    }
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
    toast("Search failed.", "error");
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
  void sendKeys(pane, bytes).catch((e) => {
    console.error("rerun send_keys failed:", e);
    toast("Couldn't rerun the command.", "error");
  });
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
      id: "new-window",
      title: "New window in this session",
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
    // GitHub: Connect when disconnected, Disconnect when linked.
    s.github.account
      ? {
          id: "github-disconnect",
          title: "Disconnect GitHub",
          hint: `@${s.github.account.login}`,
          run: () => void disconnectGitHub(),
        }
      : {
          id: "github-connect",
          title: "Connect GitHub",
          hint: "account",
          run: () => void startGitHubLink(),
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
