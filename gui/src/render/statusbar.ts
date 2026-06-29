// Status bar: daemon connectivity + socket, total pane count, focused session,
// active theme name — plus a live PROCESS readout for the focused pane (process
// name + pid, and child count if any) polled from inspect_pid (~2s). The daemon
// exposes process-tree METADATA only (no CPU/mem), so we show what exists. The
// process line degrades silently: if inspect_pid is missing or the pane is gone,
// the line is simply omitted.

import { h, replaceChildren } from "./dom";
import {
  activeSessionInfo,
  getState,
  setState,
  totalPaneCount,
  type AppState,
  type PidReadout,
} from "../state";
import { inspectPid, reconnect } from "../api";
import type { PidInfo } from "../types";

const PID_POLL_MS = 2000;

let pidTimer: number | null = null;
let pidInFlight = false;

/** Project the raw inspect_pid result for `pane` into the distilled readout the
 *  status line shows (full comm + child count + pid). */
function toReadout(pane: string, info: PidInfo): PidReadout {
  return {
    pane,
    pid: info.pid,
    comm: info.comm,
    childCount: info.children?.length ?? 0,
  };
}

/** True when two readouts would render an identical process line (or both none).
 *  The change-gate that stops the 2 s poll notifying on an unchanged process. */
function readoutEqual(a: PidReadout | null, b: PidReadout | null): boolean {
  if (a === null && b === null) return true;
  if (a === null || b === null) return false;
  return (
    a.pane === b.pane &&
    a.pid === b.pid &&
    a.comm === b.comm &&
    a.childCount === b.childCount
  );
}

/** Store the readout in state, notifying ONLY when the displayed projection
 *  actually moved (mirrors `setSessionGit`). An idle poll → zero notify. */
function setPidReadout(next: PidReadout | null): void {
  if (readoutEqual(getState().pidReadout, next)) return;
  setState({ pidReadout: next });
}

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

  // No focus or disconnected → drop any stale readout (gated: notifies only if
  // there WAS one), then skip the call.
  if (!pane || !s.connected) {
    setPidReadout(null);
    return;
  }

  pidInFlight = true;
  try {
    const info = await inspectPid(pane);
    // Change-gated: a steady process (same pid/comm/child count) → no notify, so
    // the idle 2 s poll no longer forces a renderAll for nothing.
    setPidReadout(toReadout(pane, info));
  } catch {
    // inspect_pid missing, pane gone, or daemon hiccup — degrade gracefully.
    setPidReadout(null);
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
  const readout = s.pidReadout;
  // Only show when the readout matches the currently focused pane.
  if (!readout || readout.pane !== s.focusedPane) return null;

  const parts: (Node | string)[] = [];
  const comm = shortComm(readout.comm);
  if (comm) parts.push(h("span", { class: "status-proc-cmd", title: readout.comm }, comm));

  // Process-tree metadata only — the daemon does not expose CPU/mem. Show the
  // child-process count when the foreground process has any.
  const childCount = readout.childCount;
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
    { class: "status-group status-proc", title: `pid ${readout.pid}` },
    ...parts,
  );
}

/**
 * Canonical string of every DYNAMIC value the status bar RENDERS — so an idle
 * poll tick (heat 750 ms, PID 2 s) keeps the same string and skips the
 * unconditional `replaceChildren` rebuild. The bar carries no entrance keyframe
 * today, so it doesn't *flicker* yet — but it was the one renderAll region with
 * no guard, so any motion/hover added later would flicker instantly. Closing it
 * matches topbar/agents/blocks. Inputs, each grepped from the render below:
 *   - connected            → status dot class, "pyred connected/down" text, AND
 *                            the Reconnect button (shown only when `!connected`)
 *   - socket               → the `.status-socket` span (text + presence)
 *   - process line         → only when the readout's pane === focusedPane: full
 *                            comm (drives both the basename text and the title),
 *                            child count, and pid (the proc group's title). When
 *                            it doesn't match focus, nothing renders ⇒ empty seg.
 *   - totalPaneCount()     → the "N panes" item (state.paneStates.size)
 *   - active session name  → the `.status-item` name span (absent when no active
 *                            session). NAME only, not id: the bar renders just the
 *                            name and captures no id in any handler (unlike the
 *                            topbar switcher), so id is not load-bearing here.
 *   - activeTheme          → the theme `.status-item`
 * Separator `\x01` (fields) / `\x02` (proc sub-fields) can't collide with text.
 */
function statusbarFingerprint(s: Readonly<AppState>): string {
  const active = activeSessionInfo();
  const r = s.pidReadout;
  const proc =
    r && r.pane === s.focusedPane
      ? `${r.comm}\x02${r.childCount}\x02${r.pid}`
      : "";
  return [
    s.connected ? "1" : "0",
    s.socket,
    proc,
    String(totalPaneCount()),
    active?.name ?? "",
    s.activeTheme,
  ].join("\x01");
}

/** Last fingerprint that triggered a full status-bar rebuild. */
let lastStatusbarFp = "";

export function renderStatusbar(root: HTMLElement): void {
  const s = getState();
  const fp = statusbarFingerprint(s);
  if (fp === lastStatusbarFp && root.childElementCount > 0) {
    // Connectivity, socket, process line, pane count, active name and theme are
    // all unchanged — skip the rebuild (childElementCount > 0 forces the first
    // paint). Keeps node identity so any future hover/transition survives.
    return;
  }
  lastStatusbarFp = fp;

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
