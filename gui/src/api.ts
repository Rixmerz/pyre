// Thin, typed wrappers over the Tauri command surface the Rust bridge exposes.
// Every daemon interaction the UI needs goes through here, so the rest of the
// app never touches `invoke` strings directly. A parallel agent implements the
// Rust side; we code against these names (see the task brief's command list).

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  Block,
  DaemonStatus,
  LayoutNode,
  PaneClosedPayload,
  PaneStateInfo,
  PidInfo,
  PollEventsResult,
  PtyOutputPayload,
  SessionInfo,
  SplitOrient,
  ThemeMeta,
  ThemePalette,
  WindowInfo,
} from "./types";

// ── Connectivity ──────────────────────────────────────────────────────────
export const daemonStatus = (): Promise<DaemonStatus> =>
  invoke("daemon_status");

export const reconnect = (): Promise<DaemonStatus> => invoke("reconnect");

// ── Sessions ────────────────────────────────────────────────────────────────
export const listSessions = (): Promise<SessionInfo[]> =>
  invoke("list_sessions");

export const spawnSession = (
  cols: number,
  rows: number,
): Promise<{ session: string; pane: string }> =>
  invoke("spawn_session", { cols, rows });

export const renameSession = (session: string, name: string): Promise<void> =>
  invoke("rename_session", { session, name });

/** Fully terminate a session: closes all its panes and evicts it. */
export const closeSession = (session: string): Promise<void> =>
  invoke("close_session", { session });

/**
 * DEPRECATED compat shim — returns the session's first/default window layout.
 * New code uses `windowLayout(window)`. Kept while the bridge still exposes the
 * `session_layout` command for one release.
 */
export const sessionLayout = (session: string): Promise<LayoutNode> =>
  invoke("session_layout", { session });

// ── Windows ───────────────────────────────────────────────────────────────
/**
 * The windows of a session (its tab strip), ordered by position. A parallel
 * Rust agent implements the `list_windows` bridge command; callers wire it
 * DEFENSIVELY — if the command is missing the promise rejects and the caller
 * catches + dlogs, leaving the strip intact.
 */
export const listWindows = (session: string): Promise<WindowInfo[]> =>
  invoke("list_windows", { session });

/**
 * Create a NEW (empty) window in a session and return its id. The daemon's
 * `new_window` does NOT spawn a pane, so callers follow it with `openPane(window,
 * session)` to give the window its first terminal. `name` defaults to the next
 * 1-based position on the daemon when omitted.
 */
export const newWindow = (session: string, name?: string): Promise<string> =>
  invoke("new_window", { session, name: name ?? null });

/** Rename a window (sets its daemon-authoritative display name). */
export const renameWindow = (window: string, name: string): Promise<void> =>
  invoke("rename_window", { window, name });

/** Close a window: kills all its panes and removes it from the session. */
export const closeWindow = (window: string): Promise<void> =>
  invoke("close_window", { window });

/** The layout tree for one window (Session → Window → Pane). */
export const windowLayout = (window: string): Promise<LayoutNode> =>
  invoke("get_window_layout", { window });

// ── Panes ─────────────────────────────────────────────────────────────────
export const paneStates = (): Promise<PaneStateInfo[]> => invoke("pane_states");

export const closePane = (pane: string): Promise<void> =>
  invoke("close_pane", { pane });

/**
 * Rename a pane (sets its user-facing display name). A parallel Rust agent
 * implements the `rename_pane` command + the `name` field on `pane_states`;
 * callers wire it DEFENSIVELY — if the command is missing the promise rejects,
 * the caller catches + dlogs, and the pane keeps its fallback label rather than
 * crashing the UI.
 */
export const renamePane = (pane: string, name: string): Promise<void> =>
  invoke("rename_pane", { pane, name });

export const resizePane = (
  pane: string,
  cols: number,
  rows: number,
): Promise<void> => invoke("resize_pane", { pane, cols, rows });

export const openSplit = (
  pane: string,
  orient: SplitOrient,
): Promise<{ pane: string }> => invoke("open_split", { pane, direction: orient });

/**
 * Open a pane INTO a window — the daemon inserts it as that window's first leaf
 * (a fresh window) or returns it standalone for the GUI to place. `window` is
 * required by the daemon's `OpenPaneReq`; `session` is kept so the bridge can
 * validate window ∈ session.
 *
 * A parallel Rust agent implements the `open_pane` command; callers wire it
 * DEFENSIVELY — if the command is missing or the daemon predates it, the promise
 * rejects, the caller catches + dlogs, and the tab strip stays intact (the `+`
 * pill simply does nothing rather than crashing the UI).
 */
export const openPane = (
  window: string,
  session: string,
  cols = 80,
  rows = 24,
): Promise<{ pane: string }> =>
  invoke("open_pane", { window, session, cols, rows });

export const setWeight = (pane: string, weight: number): Promise<void> =>
  invoke("set_weight", { pane, weight });

// ── Process inspection ──────────────────────────────────────────────────────
/**
 * Inspect the OS process backing a pane. A parallel Rust agent implements the
 * `inspect_pid` command; callers wire it DEFENSIVELY — if the command is missing
 * or the pane is gone the promise rejects, and the status bar simply omits the
 * process line rather than erroring.
 */
export const inspectPid = (pane: string): Promise<PidInfo> =>
  invoke("inspect_pid", { pane });

// ── Pane streams ────────────────────────────────────────────────────────────
export const attachPaneStream = (
  session: string,
  pane: string,
): Promise<void> => invoke("attach_pane_stream", { session, pane });

export const detachPaneStream = (pane: string): Promise<void> =>
  invoke("detach_pane_stream", { pane });

export const sendKeys = (pane: string, bytes: number[]): Promise<void> =>
  invoke("send_keys", { pane, bytes });

// ── Blocks ──────────────────────────────────────────────────────────────────
export const listBlocks = (pane: string): Promise<Block[]> =>
  invoke("list_blocks", { pane });

export const blockStdout = (block: string): Promise<string> =>
  invoke("block_stdout", { block });

export const searchBlocks = (
  query: string,
  failuresOnly: boolean,
  session: string | null,
): Promise<Array<{ block: Block; snippet: string }>> =>
  invoke("search_blocks", {
    query,
    failures_only: failuresOnly,
    session,
  });

// ── Themes ──────────────────────────────────────────────────────────────────
export const listThemes = (): Promise<ThemeMeta[]> => invoke("list_themes");

export const getTheme = (name: string): Promise<ThemePalette> =>
  invoke("get_theme", { name });

// ── Lifecycle events (long-poll) ─────────────────────────────────────────────
/**
 * Long-poll the daemon for lifecycle events after `afterSeq`. The daemon holds
 * the request open until an event arrives or it times out, then returns a batch
 * plus the new cursor (`last_seq`) to pass on the next call. A parallel agent
 * implements the Rust `poll_events` command; if it is not yet registered this
 * rejects, which the caller catches to fall back to the periodic poll.
 */
export const pollEvents = (afterSeq: number): Promise<PollEventsResult> =>
  invoke("poll_events", { after_seq: afterSeq, afterSeq });

// ── Events ──────────────────────────────────────────────────────────────────
/** Subscribe to per-pane PTY output. The handler routes bytes by pane id. */
export const onPtyOutput = (
  handler: (p: PtyOutputPayload) => void,
): Promise<UnlistenFn> =>
  listen<PtyOutputPayload | number[]>("pty-output", (e) => {
    // Tolerate two shapes: the multi-pane {pane,bytes} payload (new bridge),
    // and the legacy raw number[] payload (single-pane spike). The latter is
    // routed to the focused pane by the caller via a null pane sentinel.
    const payload = e.payload as PtyOutputPayload | number[];
    if (Array.isArray(payload)) {
      handler({ pane: "", bytes: payload });
    } else {
      handler(payload);
    }
  });

/** Subscribe to pane-closed lifecycle events. */
export const onPaneClosed = (
  handler: (p: PaneClosedPayload) => void,
): Promise<UnlistenFn> =>
  listen<PaneClosedPayload | null>("pane-closed", (e) => {
    const payload = (e.payload ?? { pane: "" }) as PaneClosedPayload;
    handler(payload);
  });

/** Legacy single-pane spike event, kept so the old bridge still shows output. */
export const onPtyClosedLegacy = (
  handler: () => void,
): Promise<UnlistenFn> => listen("pty-closed", () => handler());
