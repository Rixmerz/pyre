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
  PollEventsResult,
  PtyOutputPayload,
  SessionInfo,
  SplitOrient,
  ThemeMeta,
  ThemePalette,
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

export const sessionLayout = (session: string): Promise<LayoutNode> =>
  invoke("session_layout", { session });

// ── Panes ─────────────────────────────────────────────────────────────────
export const paneStates = (): Promise<PaneStateInfo[]> => invoke("pane_states");

export const closePane = (pane: string): Promise<void> =>
  invoke("close_pane", { pane });

export const resizePane = (
  pane: string,
  cols: number,
  rows: number,
): Promise<void> => invoke("resize_pane", { pane, cols, rows });

export const openSplit = (
  pane: string,
  orient: SplitOrient,
): Promise<{ pane: string }> => invoke("open_split", { pane, direction: orient });

export const setWeight = (pane: string, weight: number): Promise<void> =>
  invoke("set_weight", { pane, weight });

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
