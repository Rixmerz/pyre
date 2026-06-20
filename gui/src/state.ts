// Central app store. Single source of truth for everything the UI renders.
// A tiny pub/sub: mutators call `notify()`, views subscribe and re-render.
// No framework — just a typed object and a Set of listeners.

import type {
  Block,
  LayoutNode,
  PaneStateInfo,
  SessionInfo,
  ThemeMeta,
} from "./types";

export interface AppState {
  // Connectivity
  connected: boolean;
  socket: string;

  // Sessions & focus
  sessions: SessionInfo[];
  activeSession: string | null;
  focusedPane: string | null;
  zoomedPane: string | null;

  // Layout & live data (keyed by session / pane)
  layouts: Map<string, LayoutNode>;
  paneStates: Map<string, PaneStateInfo>;
  blocks: Block[]; // blocks for the focused pane, newest-first
  blocksLoading: boolean;

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
  blocks: [],
  blocksLoading: false,
  blockQuery: "",
  searchResults: null,
  blocksFailuresOnly: false,
  expandedBlocks: new Set(),
  agentsOpen: false,
  themes: [],
  activeTheme: "ember",
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

// ── Derived helpers ─────────────────────────────────────────────────────────

/** Pane-state info for one pane, if known. */
export function paneStateOf(pane: string): PaneStateInfo | undefined {
  return state.paneStates.get(pane);
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
