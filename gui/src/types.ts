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
  /**
   * Detected agent kind for this pane — e.g. "shell", "claude", "unknown".
   * Added by a parallel Rust agent; wired DEFENSIVELY: missing/undefined means
   * the daemon predates the field, so the UI degrades to "unknown" rather than
   * crashing. Never assume a specific set of values.
   */
  agent?: string | null;
  /**
   * User-assigned display name for this pane (set via inline rename → the Rust
   * `rename_pane` command). Added by a parallel Rust agent; wired DEFENSIVELY:
   * missing/undefined/null means the daemon predates the field, so the UI falls
   * back to the pane's title / agent / short id rather than showing nothing.
   */
  name?: string | null;
  /**
   * The window this pane belongs to (Session → Window → Pane). Mirrors the
   * daemon's `PaneInfo.window` (`#[serde(default)]`); wired DEFENSIVELY as
   * optional so a bridge that predates the window model still decodes — the
   * tab strip derives windows from `list_windows`, not from this field.
   */
  window?: string;
}

/**
 * Result of `inspect_pid(pane)` — process-tree metadata for the focused pane.
 * Mirrors the Rust `PidInspectDto` EXACTLY: the daemon only exposes process-tree
 * metadata (no CPU% / memory sampling). Wired DEFENSIVELY — the status bar
 * degrades to omitting the line if the command rejects or the pane is gone.
 */
export interface PidInfo {
  pid: number;
  /** process name (argv[0] basename), e.g. "bash", "claude". */
  comm: string;
  /** environment as [key, value] pairs. */
  env: [string, string][];
  /** open file descriptors (string targets). */
  fds: string[];
  /** child PIDs of the foreground process. */
  children: number[];
}

/** Session metadata from `list_sessions()`. */
export interface SessionInfo {
  id: string;
  name: string;
  pane_count: number;
  created_at?: string;
  last_active_at?: string;
}

/**
 * Window metadata from `list_windows(session)` — one entry per tab in a
 * session's tab strip. A window owns its own splittable layout tree (keyed by
 * `id` in `state.layouts`). `name` is authoritative from the daemon (renamed via
 * `rename_window`); `position` is the 0-based order within the session.
 */
export interface WindowInfo {
  id: string;
  session: string;
  name: string;
  position: number;
  pane_count: number;
  created_at?: string;
}

/**
 * Per-session git status from `git_status(session)`. The daemon resolves the
 * session's cwd to a repo; `null` (not modeled here — the command returns
 * `GitInfo | null`) means "not a git repo" ⇒ the rail renders NO chip. Counts are
 * non-negative; `upstream` is null when the branch has no tracking remote.
 */
export interface GitInfo {
  branch: string;
  dirty: number;
  ahead: number;
  behind: number;
  upstream: string | null;
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

/** The kinds of lifecycle event the daemon emits through `poll_events`. */
export type LifecycleEventKind =
  | "spawned"
  | "closed"
  | "state_changed"
  | "layout_changed";

/** One entry returned by the `poll_events` long-poll command. */
export interface LifecycleEvent {
  kind: LifecycleEventKind;
  pane?: string;
  session?: string;
  state?: string;
}

/** Result of `poll_events(after_seq)` — a batch plus the new cursor. */
export interface PollEventsResult {
  events: LifecycleEvent[];
  last_seq: number;
}

// ── GitHub account linking (OAuth App + Device Flow) ──────────────────────────
// The device flow + keychain + GitHub REST calls live in the Tauri Rust layer
// (gui/src-tauri/src/github.rs, owned by a parallel agent); the GUI codes against
// these four shapes. pyre NEVER sees the user's password — the user authorizes in
// their own browser and the Tauri layer stores only the access token in the OS
// keychain. See `.claude/notions/feature-github-oauth.md`.

/** The linked GitHub account from `github_account()` — `null` when disconnected. */
export interface GhAccount {
  login: string;
  name: string | null;
  avatar_url: string;
  html_url: string;
}

/** Result of `github_device_start()` — the device/user code pair to authorize. */
export interface GhDeviceStart {
  /** The short code the user types at `verification_uri` (e.g. "WDJB-MJHT"). */
  user_code: string;
  /** Where the user enters the code (e.g. github.com/login/device). */
  verification_uri: string;
  /** Seconds until the device code expires (drives the modal countdown). */
  expires_in: number;
  /** Minimum seconds between `github_device_poll()` calls. */
  interval: number;
}

/** Result of `github_device_poll()` — the authorization status of the link. */
export type GhPoll = { status: "pending" | "authorized" | "denied" | "expired" };
