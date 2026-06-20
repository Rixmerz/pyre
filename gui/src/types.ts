// Domain types mirroring the pyred daemon's client-facing API.
// These shapes match what the Tauri Rust bridge serializes to the webview.
// A parallel agent owns the Rust side; we code against these stable names.

/** Agent lifecycle state — the heat ramp is bound to this. */
export type PaneState =
  | "idle"
  | "running"
  | "waiting"
  | "interactive"
  | "crashed"
  | "done";

/** All known pane states, in heat order (coolest → hottest-then-resolved). */
export const PANE_STATES: PaneState[] = [
  "idle",
  "running",
  "waiting",
  "interactive",
  "crashed",
  "done",
];

/** One entry from `pane_states()`. */
export interface PaneStateInfo {
  pane: string;
  session: string;
  state: PaneState;
  title: string | null;
}

/** Session metadata from `list_sessions()`. */
export interface SessionInfo {
  id: string;
  name: string;
  pane_count: number;
  created_at?: string;
  last_active_at?: string;
}

/** A command block from `list_blocks(pane)` / `search_blocks(...)`. */
export interface Block {
  id: string;
  pane: string;
  session: string;
  command: string;
  cwd?: string | null;
  started_at: string;
  ended_at?: string | null;
  exit_code?: number | null;
  duration_ms?: number | null;
  running: boolean;
  /** present on search results */
  snippet?: string;
}

/** Recursive layout tree from `session_layout(session)`. */
export type LayoutNode = LayoutLeaf | LayoutSplit;

export interface LayoutLeaf {
  kind: "leaf";
  pane: string;
}

export interface LayoutSplit {
  kind: "split";
  /**
   * Daemon wire convention (pyre-proto layout.rs → bridge layout_to_dto):
   * 'v' = VSplit = side-by-side columns (new pane to the RIGHT),
   * 'h' = HSplit = top-to-bottom stack (new pane BELOW).
   * Rust names by split AXIS, not child arrangement — do not flip these.
   * Serialized as `dir` on the wire (Rust LayoutDto::Split.dir).
   */
  dir: "h" | "v";
  children: LayoutNode[];
  /** per-child weights, parallel to `children` (Rust LayoutDto::Split.weights). */
  weights?: number[];
}

/** Split direction as the daemon names it. */
export type SplitOrient = "h" | "v";

/** Daemon connectivity snapshot from `daemon_status()`. */
export interface DaemonStatus {
  connected: boolean;
  socket: string;
}

/** A theme entry from `list_themes()`. */
export interface ThemeMeta {
  name: string;
  display_name: string;
  kind: "dark" | "light";
  /** swatch hints — accent over background */
  accent: string;
  bg: string;
}

/** Full palette from `get_theme(name)` — colors are "#rrggbb" strings. */
export interface ThemePalette {
  name: string;
  display_name: string;
  bg: string;
  bg_dim: string;
  fg: string;
  fg_dim: string;
  border: string;
  border_focus: string;
  cursor: string;
  accent: string;
  ok: string;
  warn: string;
  error: string;
  /** 16-entry ANSI table */
  ansi: string[];
}

/** Event payload from the `pty-output` Tauri event. */
export interface PtyOutputPayload {
  pane: string;
  bytes: number[];
}

/** Event payload from the `pane-closed` Tauri event. */
export interface PaneClosedPayload {
  pane: string;
}
