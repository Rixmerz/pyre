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

import { invoke } from "./invoke";
import { daemonStatus, pollEvents, reconnect, onPaneClosed, onPtyClosedLegacy, onPtyOutput } from "./api";
import { getState, setState } from "./state";
import { mountShell } from "./render/index";
import { startPidPoll } from "./render/statusbar";
import { initThemes } from "./themes";
import { installKeybinds } from "./keybinds";
import { initNotifications } from "./notify";
import {
  applyLifecycleEvent,
  reloadFocusedBlocks,
  reloadPaneStates,
  reloadSession,
  reloadSessions,
  focusFirstLeaf,
} from "./session-ops";
import { newSession } from "./actions";
import { loadGitHubAccount } from "./github-link";
import { startGitPolling } from "./git-poll";
import {
  disposePaneTerminal,
  refitAll,
  writePaneOutput,
} from "./terminals";

const POLL_MS = 750;
/** Backoff after a failed `poll_events` call so a missing/erroring command
 *  doesn't spin a tight loop. The periodic poll keeps the UI fresh meanwhile. */
const EVENT_POLL_BACKOFF_MS = 2000;
/** Boot connection retries: the daemon socket may not be ready the instant the
 *  webview loads (startup race). Probe a handful of times before giving up. */
const BOOT_RETRIES = 5;
const BOOT_BACKOFF_MS = 300;
/** While disconnected, a background poll re-probes so the UI self-heals once
 *  the daemon comes up — no manual reload needed. */
const RECONNECT_POLL_MS = 3000;

let reconnectTimer: number | null = null;

// ── Event long-poll state ─────────────────────────────────────────────────────
/** Cursor for `poll_events`; advances as we consume batches. */
let eventSeq = 0;
/** Guard so we never run two long-poll loops concurrently. */
let eventLoopRunning = false;
/** When false, the running loop exits at its next checkpoint. */
let eventLoopActive = false;

const sleep = (ms: number): Promise<void> =>
  new Promise((r) => window.setTimeout(r, ms));

/** Guard so `close_splash` is only invoked once (boot OR first reconnect). */
let splashClosed = false;

/**
 * Close the frameless splash window and reveal the main window. The Rust agent
 * implements the `close_splash` command; wired DEFENSIVELY — if the command is
 * missing or rejects (e.g. the splash was never spawned in plain `vite dev`),
 * we swallow the error and proceed so boot is never blocked. Idempotent.
 */
async function closeSplash(): Promise<void> {
  if (splashClosed) return;
  splashClosed = true;
  try {
    await invoke("close_splash");
  } catch (err) {
    // Expected in `vite dev` (no Tauri host) or before the Rust command lands.
    console.warn("[pyre-splash] close_splash unavailable — continuing:", err);
  }
}

/**
 * Event-driven lifecycle loop. Long-polls `poll_events(eventSeq)`; the daemon
 * returns on the next event or a timeout. Each event is applied for an INSTANT
 * UI update (dead sessions vanish, layouts reload, heat repaints) — the periodic
 * `startPolling` loop stays as a fallback/refresh.
 *
 * Defensive by design: if `poll_events` is not yet implemented by the Rust
 * bridge (parallel agent), the invoke rejects; we log once, back off, and keep
 * looping so the app still works on the periodic poll alone. A drop in
 * connectivity ends the loop until reconnect restarts it.
 */
async function runEventLoop(): Promise<void> {
  if (eventLoopRunning) return;
  eventLoopRunning = true;
  eventLoopActive = true;
  let warnedMissing = false;

  while (eventLoopActive) {
    if (!getState().connected) break;
    try {
      const res = await pollEvents(eventSeq);
      // Reset the missing-command warning latch on a successful round-trip.
      warnedMissing = false;
      for (const ev of res.events) {
        try {
          await applyLifecycleEvent(ev);
        } catch (err) {
          console.error("[pyre-events] apply failed:", ev, err);
        }
      }
      // Advance the cursor; tolerate a non-advancing/absent last_seq.
      if (typeof res.last_seq === "number" && res.last_seq >= eventSeq) {
        eventSeq = res.last_seq;
      }
    } catch (err) {
      if (!warnedMissing) {
        console.warn(
          "[pyre-events] poll_events unavailable — falling back to periodic poll:",
          err,
        );
        warnedMissing = true;
      }
      await sleep(EVENT_POLL_BACKOFF_MS);
    }
  }

  eventLoopRunning = false;
}

/** Start the event loop if connected and not already running. */
function startEventLoop(): void {
  if (eventLoopRunning) return;
  if (!getState().connected) return;
  void runEventLoop();
}

/** Stop the event loop (e.g. on disconnect). */
function stopEventLoop(): void {
  eventLoopActive = false;
}

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
  installKeybinds();
  initNotifications();
  // Load any previously-linked GitHub account so the chip shows it immediately.
  // Owned by the Tauri layer (not pyred), so it runs regardless of daemon
  // connectivity — fire-and-forget, never blocking boot.
  void loadGitHubAccount();

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
    startEventLoop();
  } else {
    startReconnectPoll();
  }

  // First paint is done (session loaded, or the daemon-down panel is showing).
  // Reveal the main window and dismiss the splash either way — leaving the splash
  // up while the daemon is down would strand the user on a frozen splash.
  await closeSplash();

  startPolling();
  startPidPoll();
  // Per-session git chip poll. Idempotent + self-healing: it reads whatever
  // sessions are in state each 3 s tick, so it works whether we booted connected
  // or pick sessions up after a later reconnect — same pattern as startPidPoll.
  startGitPolling();
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
    startEventLoop();
  } else {
    stopEventLoop();
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
      // Dropped (or never connected): stop the event loop and ensure the
      // background reconnect poll is running so the UI self-heals when the
      // daemon returns.
      stopEventLoop();
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
