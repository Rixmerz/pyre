// pyre GUI — bootstrap. Builds the shell, wires daemon events, runs the poll
// loop that drives heat + the block panel, and loads the initial session.
//
// The xterm <-> Tauri wiring from the spike is preserved and generalized: output
// is routed to per-pane terminals (see terminals.ts), keystrokes carry the pane
// id (see api.sendKeys), and the legacy single-pane events still feed the
// focused pane so the old Rust bridge keeps showing output during the swap.

// Self-hosted fonts (offline-first; no CDN). Geist for UI, JetBrains Mono for data.
import "@fontsource/geist/400.css";
import "@fontsource/geist/500.css";
import "@fontsource/geist/700.css";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";
import "@fontsource/jetbrains-mono/600.css";
import "@fontsource/jetbrains-mono/700.css";
import "./styles.css";

import { daemonStatus, reconnect, onPaneClosed, onPtyClosedLegacy, onPtyOutput } from "./api";
import { getState, setState } from "./state";
import { mountShell } from "./render/index";
import { initThemes } from "./themes";
import {
  reloadFocusedBlocks,
  reloadPaneStates,
  reloadSession,
  reloadSessions,
  focusFirstLeaf,
} from "./session-ops";
import { newSession } from "./actions";
import {
  disposePaneTerminal,
  refitAll,
  writePaneOutput,
} from "./terminals";

const POLL_MS = 750;
/** Boot connection retries: the daemon socket may not be ready the instant the
 *  webview loads (startup race). Probe a handful of times before giving up. */
const BOOT_RETRIES = 5;
const BOOT_BACKOFF_MS = 300;
/** While disconnected, a background poll re-probes so the UI self-heals once
 *  the daemon comes up — no manual reload needed. */
const RECONNECT_POLL_MS = 3000;

let reconnectTimer: number | null = null;

const sleep = (ms: number): Promise<void> =>
  new Promise((r) => window.setTimeout(r, ms));

/** Probe the daemon once. Returns the status, never throws. */
async function probe(): Promise<{ connected: boolean; socket: string }> {
  try {
    return await daemonStatus();
  } catch (err) {
    console.warn("daemon_status probe failed:", err);
    return { connected: false, socket: getState().socket };
  }
}

/** Load the active (or first / new) session once connected. */
async function loadInitial(): Promise<void> {
  await reloadSessions();
  const first = getState().sessions[0];
  if (first) {
    setState({ activeSession: first.id });
    await reloadSession(first.id);
    focusFirstLeaf(first.id);
  } else {
    await newSession();
  }
  await reloadPaneStates();
  await reloadFocusedBlocks();
}

async function boot(): Promise<void> {
  const app = document.getElementById("app");
  if (!app) return;

  mountShell(app);
  await initThemes();
  await wireEvents();

  // Connectivity → initial load. Retry a few times before surfacing the dead
  // state: the daemon may still be binding its socket when the webview loads.
  let status = await probe();
  for (let i = 1; i < BOOT_RETRIES && !status.connected; i++) {
    await sleep(BOOT_BACKOFF_MS);
    status = await probe();
  }
  setState({ connected: status.connected, socket: status.socket });

  if (status.connected) {
    await loadInitial();
  } else {
    startReconnectPoll();
  }

  startPolling();
  window.addEventListener("resize", () => refitAll());
}

/** Background reconnect: while disconnected, re-probe every few seconds and
 *  load on success. Idempotent — starting twice is a no-op. The Reconnect
 *  button (see render/center.ts) shares this path via `attemptReconnect`. */
function startReconnectPoll(): void {
  if (reconnectTimer != null) return;
  reconnectTimer = window.setInterval(() => {
    if (getState().connected) {
      stopReconnectPoll();
      return;
    }
    void attemptReconnect(false);
  }, RECONNECT_POLL_MS);
}

function stopReconnectPoll(): void {
  if (reconnectTimer != null) {
    window.clearInterval(reconnectTimer);
    reconnectTimer = null;
  }
}

/** Try to (re)establish the daemon connection and load. Used by the background
 *  poll (`force=false` → cheap status probe) and the Reconnect button
 *  (`force=true` → drops the cached client first via the reconnect command). */
export async function attemptReconnect(force: boolean): Promise<boolean> {
  let connected: boolean;
  let socket = getState().socket;
  try {
    if (force) {
      const r = await reconnect();
      connected = r.connected;
      socket = (r as { socket?: string }).socket ?? socket;
    } else {
      const s = await probe();
      connected = s.connected;
      socket = s.socket;
    }
  } catch (err) {
    console.warn("reconnect attempt failed:", err);
    connected = false;
  }
  setState({ connected, socket });
  if (connected) {
    stopReconnectPoll();
    await loadInitial();
  } else {
    startReconnectPoll();
  }
  return connected;
}

// Expose for the daemon-down panel's Reconnect button without a circular import.
(window as unknown as { __pyreReconnect?: () => Promise<boolean> }).__pyreReconnect =
  () => attemptReconnect(true);

async function wireEvents(): Promise<void> {
  // Multi-pane PTY output: route bytes to the right terminal by pane id. The
  // legacy single-pane bridge emits a bare number[] (pane ""), which we route
  // to the focused pane so old output still lands somewhere visible.
  await onPtyOutput((p) => {
    const pane = p.pane || getState().focusedPane;
    if (pane) writePaneOutput(pane, p.bytes);
  });

  await onPaneClosed((p) => {
    const pane = p.pane || getState().focusedPane;
    if (pane) {
      disposePaneTerminal(pane);
      // Refresh the active session's layout so the closed leaf drops out.
      const session = getState().activeSession;
      if (session) void reloadSession(session);
    }
  });

  // Legacy spike event — treat a close as a focused-pane close.
  await onPtyClosedLegacy(() => {
    const pane = getState().focusedPane;
    if (pane) disposePaneTerminal(pane);
  });
}

function startPolling(): void {
  window.setInterval(() => {
    if (!getState().connected) {
      // Dropped (or never connected): ensure the background reconnect poll is
      // running so the UI self-heals when the daemon returns.
      startReconnectPoll();
      return;
    }
    void reloadPaneStates();
    void reloadFocusedBlocks();
  }, POLL_MS);
}

window.addEventListener("DOMContentLoaded", () => {
  void boot();
});
