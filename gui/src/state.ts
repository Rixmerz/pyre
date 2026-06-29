// Central app store. Single source of truth for everything the UI renders.
// A tiny pub/sub: mutators call `notify()`, views subscribe and re-render.
// No framework — just a typed object and a Set of listeners.

import type {
  Block,
  GhAccount,
  GhDeviceStart,
  GitInfo,
  LayoutNode,
  PaneStateInfo,
  PrCiInfo,
  SessionInfo,
  ThemeMeta,
  WindowInfo,
} from "./types";

/**
 * GitHub account-linking slice. `account` is the linked identity (null when
 * disconnected); `linking` holds the in-flight device-code response while a link
 * attempt is open (drives the modal — non-null ⇒ modal visible); `status` is the
 * coarse phase the chip + palette read. Mutated only via `setGithub`.
 */
export interface GithubState {
  account: GhAccount | null;
  linking: GhDeviceStart | null;
  status: "idle" | "linking" | "authorized" | "error";
}

/**
 * What the status bar's process line currently SHOWS for the focused pane — a
 * distilled projection of `inspect_pid` (the daemon exposes process-tree metadata
 * only: no CPU/mem). Null ⇒ no readout (no focus, disconnected, or the command
 * degraded). Owned by the 2 s PID poll in `render/statusbar.ts`, which writes it
 * CHANGE-GATED (mirrors `setSessionGit`): only when this projection actually moves
 * does it call `setState`, so an idle poll tick on an unchanged process fires no
 * `notify()` / `renderAll()`. The statusbar fingerprint reads exactly these fields
 * — keep them in sync with what `processGroup()` renders.
 */
export interface PidReadout {
  /** Pane the readout describes — a focus change invalidates a stale readout. */
  pane: string;
  /** Foreground process pid (shown as the proc group's title). */
  pid: number;
  /** Full process name; the line shows its basename, the title shows it whole. */
  comm: string;
  /** Number of child processes (the "N child/children" metric; 0 ⇒ omitted). */
  childCount: number;
}

export interface AppState {
  // Connectivity
  connected: boolean;
  socket: string;

  // Sessions & focus
  sessions: SessionInfo[];
  activeSession: string | null;
  focusedPane: string | null;
  zoomedPane: string | null;

  // Layout & live data. `layouts` is keyed by WINDOW id — every window owns its
  // own tiling tree; `paneStates` is keyed by pane id.
  layouts: Map<string, LayoutNode>;
  paneStates: Map<string, PaneStateInfo>;

  /**
   * The windows of each session (from `list_windows`), keyed by session id and
   * ordered by position. The tab strip renders this list; each window owns its
   * own splittable layout tree in `layouts` (keyed by window id). Window names
   * are authoritative from the daemon — there is no GUI-side label store.
   */
  windows: Map<string, WindowInfo[]>;

  /**
   * The active (visible) window per session, keyed by session id → window id.
   * Absent ⇒ default to the session's first window (see `activeWindowOf`).
   */
  activeWindow: Map<string, string>;
  blocks: Block[]; // blocks for the focused pane, newest-first
  blocksLoading: boolean;

  /**
   * Distilled process readout for the focused pane (status bar process line).
   * Written change-gated by the PID poll; null ⇒ no line. See `PidReadout`.
   */
  pidReadout: PidReadout | null;

  /**
   * Per-session git status, keyed by session id. Kept SEPARATE from `sessions`
   * so a `list_sessions` refresh (which replaces the whole array) never clobbers
   * git that the slower 3 s git-poll owns. Absent key ⇒ unknown/not-a-repo ⇒ no
   * chip. Mutated only via `setSessionGit`, whose change-gate stops the poll from
   * notifying on no-op ticks (flicker fix, commit 27888ba).
   */
  gitBySession: Map<string, GitInfo>;

  /**
   * Per-session PR / CI status, keyed by session id. Absent key ⇒ not yet
   * fetched ⇒ chip hidden. Mutated only via `setSessionPrCi`, which is
   * change-gated (mirrors `setSessionGit`) so the 30 s poll never triggers a
   * render rebuild when the PR state is unchanged. `null` from the Tauri command
   * deletes the key (no-token / no-PR / error → chip hidden).
   */
  prCiBySession: Map<string, PrCiInfo>;

  // Block panel search
  blockQuery: string;
  searchResults: Block[] | null; // null = not searching, show live blocks
  blocksFailuresOnly: boolean; // panel filter: only show non-zero-exit blocks
  expandedBlocks: Set<string>; // block ids whose output preview is expanded

  // Agent overview overlay
  agentsOpen: boolean;

  // Theme
  themes: ThemeMeta[];
  activeTheme: string;

  // GitHub account linking
  github: GithubState;
  /** The account menu popover (anchored to the topbar chip) is open. Kept as a
   *  chrome flag alongside the other overlay flags so the menu can live in its
   *  own poll-survivable layer instead of inside the poll-rebuilt topbar. */
  ghMenuOpen: boolean;

  // UI chrome
  railCollapsed: boolean;
  rightCollapsed: boolean;
  paletteOpen: boolean;
  themePickerOpen: boolean;
}

const state: AppState = {
  connected: false,
  socket: "",
  sessions: [],
  activeSession: null,
  focusedPane: null,
  zoomedPane: null,
  layouts: new Map(),
  paneStates: new Map(),
  windows: new Map(),
  activeWindow: new Map(),
  blocks: [],
  blocksLoading: false,
  pidReadout: null,
  gitBySession: new Map(),
  prCiBySession: new Map(),
  blockQuery: "",
  searchResults: null,
  blocksFailuresOnly: false,
  expandedBlocks: new Set(),
  agentsOpen: false,
  themes: [],
  activeTheme: "ember",
  github: { account: null, linking: null, status: "idle" },
  ghMenuOpen: false,
  railCollapsed: false,
  rightCollapsed: false,
  paletteOpen: false,
  themePickerOpen: false,
};

type Listener = () => void;
const listeners = new Set<Listener>();

/** Read the current state (treat as immutable; mutate only via setState). */
export function getState(): Readonly<AppState> {
  return state;
}

/** Subscribe to any state change. Returns an unsubscribe fn. */
export function subscribe(fn: Listener): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

/** Notify all subscribers. Called by setState; also exported for batch edits. */
export function notify(): void {
  for (const fn of listeners) fn();
}

/** Shallow-merge a patch into the store and notify subscribers. */
export function setState(patch: Partial<AppState>): void {
  Object.assign(state, patch);
  notify();
}

/**
 * Merge a patch into the nested `github` slice and notify. A dedicated setter (vs
 * `setState({ github: {...} })`) keeps callers from having to spread the previous
 * sub-fields by hand — `setGithub({ status: "linking" })` leaves `account` and
 * `linking` untouched. Follows the store's mutate-then-notify convention.
 */
export function setGithub(patch: Partial<GithubState>): void {
  Object.assign(state.github, patch);
  notify();
}

// ── Derived helpers ─────────────────────────────────────────────────────────

/** Pane-state info for one pane, if known. */
export function paneStateOf(pane: string): PaneStateInfo | undefined {
  return state.paneStates.get(pane);
}

/**
 * Display name for a pane: the user-assigned `name` when set, else the caller's
 * `fallback` (e.g. the pane title, agent kind, or a short id). Centralizes the
 * "name overrides label" rule so the tab pill, pane-card header, and any future
 * surface stay consistent. `name` is wired DEFENSIVELY — undefined/null/empty on
 * a daemon that predates the field simply falls through to `fallback`.
 */
export function paneDisplayName(pane: string, fallback: string): string {
  const name = (paneStateOf(pane)?.name ?? "").trim();
  return name || fallback;
}

/** Git status for one session, if known (not a repo / not yet polled ⇒ undefined). */
export function getSessionGit(session: string): GitInfo | undefined {
  return state.gitBySession.get(session);
}

/** True when two git snapshots differ on any of the 5 fields (null vs present counts). */
function gitChanged(a: GitInfo | undefined, b: GitInfo | null): boolean {
  if (a == null && b == null) return false;
  if (a == null || b == null) return true;
  return (
    a.branch !== b.branch ||
    a.dirty !== b.dirty ||
    a.ahead !== b.ahead ||
    a.behind !== b.behind ||
    a.upstream !== b.upstream
  );
}

/**
 * Set (or clear, on `null`) a session's git status. CHANGE-GATED: shallow-compares
 * the 5 fields against the stored value and returns WITHOUT notifying when nothing
 * moved — this is the discipline that stops the 3 s git poll from rebuilding the
 * rail every tick (chronic-flicker fix, commit 27888ba). Only a genuine change
 * mutates the map and calls notify(), so steady state = zero rebuilds and a real
 * git change = exactly one. `null` deletes the key (no chip).
 */
export function setSessionGit(session: string, git: GitInfo | null): void {
  if (!gitChanged(state.gitBySession.get(session), git)) return;
  if (git === null) state.gitBySession.delete(session);
  else state.gitBySession.set(session, git);
  notify();
}

/** PR / CI info for one session, if known (not yet polled / no PR ⇒ undefined). */
export function getSessionPrCi(session: string): PrCiInfo | undefined {
  return state.prCiBySession.get(session);
}

/** True when two PrCiInfo snapshots differ on any rendered field (null vs present counts). */
function prCiChanged(a: PrCiInfo | undefined, b: PrCiInfo | null): boolean {
  if (a == null && b == null) return false;
  if (a == null || b == null) return true;
  return (
    a.pr_number !== b.pr_number ||
    a.ci_state !== b.ci_state ||
    a.pr_url !== b.pr_url
  );
}

/**
 * Set (or clear, on `null`) a session's PR / CI status. CHANGE-GATED: compares
 * the three rendered fields against the stored value and returns WITHOUT notifying
 * when nothing moved — mirrors `setSessionGit` so the 30 s poll never rebuilds the
 * rail on a no-op tick. A genuine change mutates the map and calls notify().
 * `null` deletes the key (no chip).
 */
export function setSessionPrCi(session: string, prCi: PrCiInfo | null): void {
  if (!prCiChanged(state.prCiBySession.get(session), prCi)) return;
  if (prCi === null) state.prCiBySession.delete(session);
  else state.prCiBySession.set(session, prCi);
  notify();
}

/** All pane states belonging to a given session. */
export function panesOfSession(session: string): PaneStateInfo[] {
  const out: PaneStateInfo[] = [];
  for (const ps of state.paneStates.values()) {
    if (ps.session === session) out.push(ps);
  }
  return out;
}

/** The session object currently active, if any. */
export function activeSessionInfo(): SessionInfo | undefined {
  if (!state.activeSession) return undefined;
  return state.sessions.find((s) => s.id === state.activeSession);
}

/** Total live pane count across all known sessions. */
export function totalPaneCount(): number {
  return state.paneStates.size;
}

// ── Window model ─────────────────────────────────────────────────────────────
// A session's tab strip = its WINDOWS (from `list_windows`), each owning its own
// splittable layout tree (keyed by window id in `state.layouts`). Window names
// are authoritative from the daemon — renamed via `rename_window`, never a
// GUI-side label store. These helpers are pure reads over the store so the
// render + keybind layers share one derivation.

/** The ordered window list for a session, or [] when not loaded yet. */
export function windowTabs(session: string | null): WindowInfo[] {
  if (!session) return [];
  return state.windows.get(session) ?? [];
}

/** Display label for a window: the daemon name, else its 1-based position. */
export function windowLabel(window: WindowInfo): string {
  const name = window.name.trim();
  return name || String(window.position + 1);
}

/**
 * The active window id for a session — the explicitly-selected one when it still
 * exists, else the session's first window. Null when the session has no windows
 * (e.g. not loaded yet).
 */
export function activeWindowOf(session: string | null): string | null {
  if (!session) return null;
  const wins = state.windows.get(session);
  if (!wins || wins.length === 0) return null;
  const explicit = state.activeWindow.get(session);
  if (explicit && wins.some((w) => w.id === explicit)) return explicit;
  return wins[0]?.id ?? null;
}
