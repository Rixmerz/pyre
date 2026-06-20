// Status bar: daemon connectivity + socket, total pane count, focused session,
// active theme name — plus a live PROCESS readout for the focused pane (process
// name + pid, and child count if any) polled from inspect_pid (~2s). The daemon
// exposes process-tree METADATA only (no CPU/mem), so we show what exists. The
// process line degrades silently: if inspect_pid is missing or the pane is gone,
// the line is simply omitted.

import { h, replaceChildren } from "./dom";
import { activeSessionInfo, getState, totalPaneCount } from "../state";
import { inspectPid, reconnect } from "../api";
import { setState } from "../state";
import type { PidInfo } from "../types";

const PID_POLL_MS = 2000;

/** Latest inspect_pid result for the focused pane (module-scoped, transient). */
let pidInfo: PidInfo | null = null;
/** The pane pidInfo describes — so a focus change invalidates a stale readout. */
let pidPane: string | null = null;
let pidTimer: number | null = null;
let pidInFlight = false;

/**
 * Start the process-inspection poll loop. Idempotent. Called once at boot from
 * the render layer; polls the CURRENT focused pane each tick so it follows focus
 * without needing to be restarted.
 */
export function startPidPoll(): void {
  if (pidTimer != null) return;
  void pollPidOnce();
  pidTimer = window.setInterval(() => void pollPidOnce(), PID_POLL_MS);
}

async function pollPidOnce(): Promise<void> {
  if (pidInFlight) return;
  const s = getState();
  const pane = s.focusedPane;

  // No focus or disconnected → clear any stale readout and skip the call.
  if (!pane || !s.connected) {
    if (pidInfo !== null || pidPane !== null) {
      pidInfo = null;
      pidPane = null;
      setState({}); // nudge a re-render to drop the line
    }
    return;
  }

  pidInFlight = true;
  try {
    const info = await inspectPid(pane);
    pidInfo = info;
    pidPane = pane;
    setState({}); // repaint with fresh process data
  } catch {
    // inspect_pid missing, pane gone, or daemon hiccup — degrade gracefully.
    if (pidInfo !== null || pidPane !== null) {
      pidInfo = null;
      pidPane = null;
      setState({});
    }
  } finally {
    pidInFlight = false;
  }
}

/** Compact a process name to its basename for the status line. */
function shortComm(comm: string): string {
  const trimmed = comm.trim();
  if (!trimmed) return "";
  const base = trimmed.split("/").pop() || trimmed;
  return base.length > 42 ? base.slice(0, 41) + "…" : base;
}

function processGroup(): HTMLElement | null {
  const s = getState();
  // Only show when the readout matches the currently focused pane.
  if (!pidInfo || pidPane == null || pidPane !== s.focusedPane) return null;

  const parts: (Node | string | false)[] = [];
  const comm = shortComm(pidInfo.comm);
  if (comm) parts.push(h("span", { class: "status-proc-cmd", title: pidInfo.comm }, comm));

  // Process-tree metadata only — the daemon does not expose CPU/mem. Show the
  // child-process count when the foreground process has any.
  const childCount = pidInfo.children?.length ?? 0;
  if (childCount > 0) {
    parts.push(
      h(
        "span",
        { class: "status-proc-metric" },
        `${childCount} ${childCount === 1 ? "child" : "children"}`,
      ),
    );
  }

  if (parts.length === 0) return null;
  return h(
    "div",
    { class: "status-group status-proc", title: `pid ${pidInfo.pid}` },
    ...parts.filter((p): p is Node | string => p !== false),
  );
}

export function renderStatusbar(root: HTMLElement): void {
  const s = getState();
  const active = activeSessionInfo();

  const dot = h("span", {
    class: "status-dot " + (s.connected ? "ok" : "down"),
  });

  const daemon = h(
    "div",
    { class: "status-group" },
    dot,
    h(
      "span",
      { class: "status-daemon" },
      s.connected ? "pyred connected" : "pyred down",
    ),
    s.socket && h("span", { class: "status-socket" }, s.socket),
    !s.connected &&
      h(
        "button",
        {
          class: "status-reconnect",
          onclick: async () => {
            try {
              const st = await reconnect();
              setState({ connected: st.connected, socket: st.socket });
            } catch {
              setState({ connected: false });
            }
          },
        },
        "Reconnect",
      ),
  );

  const proc = processGroup();

  const right = h(
    "div",
    { class: "status-group right" },
    h("span", { class: "status-item" }, `${totalPaneCount()} panes`),
    active && h("span", { class: "status-item" }, active.name),
    h("span", { class: "status-item theme" }, s.activeTheme),
  );

  replaceChildren(root, daemon, proc, right);
}
