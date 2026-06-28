// Agent overview overlay — pyre's agent-control-plane differentiator.
//
// Lists EVERY pane across EVERY session as a row: agent type chip, a heat/state
// dot, the pane title, and its session name. Panes WAITING ON INPUT sort to the
// TOP and are visually flagged ("needs input") so a fleet of agents surfaces the
// one blocking on you first. Clicking a row jumps to that session and focuses
// that pane. herdr only labels state in its TUI; this is the cross-session
// dashboard pyre adds on top.

import { h, replaceChildren } from "./dom";
import { icon } from "./icons";
import { getState, paneDisplayName } from "../state";
import { heatVar, hottest, stateLabel } from "../heat";
import { closeAgents, gotoPane } from "../actions";
import type { PaneState, PaneStateInfo, SessionInfo } from "../types";

/** A flat row built from a pane-state plus its resolved session. */
interface AgentRow {
  pane: string;
  session: SessionInfo | undefined;
  sessionId: string;
  state: PaneState;
  title: string;
  agent: string;
}

/** Normalize the (optional) agent field to a stable display string. */
function agentLabel(info: PaneStateInfo): string {
  const a = (info.agent ?? "").trim();
  return a === "" ? "unknown" : a;
}

/** "Needs input" = the pane is blocked waiting on the user. */
function needsInput(state: PaneState): boolean {
  return state === "waiting";
}

/** Build, sort (needs-input first, then hottest), and return all agent rows. */
function buildRows(): AgentRow[] {
  const s = getState();
  const byId = new Map(s.sessions.map((x) => [x.id, x] as const));
  const rows: AgentRow[] = [];
  for (const info of s.paneStates.values()) {
    rows.push({
      pane: info.pane,
      session: byId.get(info.session),
      sessionId: info.session,
      state: info.state,
      // User-assigned name wins over the daemon title (falls back to title, "pane").
      title: paneDisplayName(info.pane, info.title || "pane"),
      agent: agentLabel(info),
    });
  }
  // needs-input to the top; within each group, hotter states first; then by
  // session name for a stable order.
  const rank: Record<PaneState, number> = {
    waiting: 6,
    crashed: 5,
    interactive: 4,
    running: 3,
    done: 2,
    idle: 1,
  };
  rows.sort((a, b) => {
    const ni = Number(needsInput(b.state)) - Number(needsInput(a.state));
    if (ni !== 0) return ni;
    const r = rank[b.state] - rank[a.state];
    if (r !== 0) return r;
    const an = a.session?.name ?? a.sessionId;
    const bn = b.session?.name ?? b.sessionId;
    return an.localeCompare(bn);
  });
  return rows;
}

/**
 * Canonical string of everything the overlay rows RENDER from — per row:
 * sessionId, pane id, state, the displayed title, the agent chip label, and the
 * resolved session name (rows render `session?.name ?? sessionId`, so a rename
 * must move the string). Row count and waiting-count shown in the header are both
 * derived from this set, so they need no separate term. Mirrors railFingerprint:
 * a no-op poll tick keeps the same string and skips the replaceChildren tear-down
 * (which re-runs the overlay-fade/overlay-rise entrance keyframes -> flicker); a
 * genuine change (new/closed pane, state shift, rename) changes the string and
 * forces exactly one rebuild. Separator `\x01` can't collide with rendered text.
 */
function agentsFingerprint(rows: readonly AgentRow[]): string {
  return rows
    .map((r) => {
      const sessName = r.session?.name ?? r.sessionId;
      return `${r.sessionId}\x01${r.pane}\x01${r.state}\x01${r.title}\x01${r.agent}\x01${sessName}`;
    })
    .join("\x02");
}

/** Last fingerprint that triggered a full overlay rebuild. */
let lastAgentsFingerprint = "";

export function renderAgents(root: HTMLElement): void {
  const s = getState();
  root.classList.toggle("open", s.agentsOpen);
  if (!s.agentsOpen) {
    replaceChildren(root);
    // Reset so a fresh open rebuilds and plays the entrance animation once.
    lastAgentsFingerprint = "";
    return;
  }

  const rows = buildRows();
  const fp = agentsFingerprint(rows);
  if (fp === lastAgentsFingerprint && root.childElementCount > 0) {
    // Agent set, states, titles and names are unchanged. Skip the rebuild so the
    // overlay-fade/overlay-rise entrance keyframes don't re-run every poll tick
    // (the flicker). childElementCount > 0 forces a build if the overlay is open
    // but empty (first open against a stale fingerprint).
    return;
  }
  lastAgentsFingerprint = fp;

  const waitingCount = rows.filter((r) => needsInput(r.state)).length;

  const backdrop = h("div", {
    class: "agents-backdrop",
    role: "dialog",
    "aria-modal": "true",
    "aria-label": "Agent overview",
    onclick: (e: Event) => {
      if (e.target === e.currentTarget) closeAgents();
    },
  });

  const titleBar = h(
    "div",
    { class: "agents-header" },
    h("span", { class: "agents-title-icon", html: icon("agents") }),
    h("span", { class: "agents-title" }, "Agents"),
    h(
      "span",
      { class: "agents-count" },
      `${rows.length} pane${rows.length === 1 ? "" : "s"}`,
    ),
    waitingCount > 0 &&
      h(
        "span",
        { class: "agents-waiting-badge" },
        `${waitingCount} need${waitingCount === 1 ? "s" : ""} input`,
      ),
    h(
      "button",
      { class: "agents-close", title: "Close (Esc)", "aria-label": "Close", onclick: () => closeAgents() },
      h("span", { html: icon("close") }),
    ),
  );

  const list = h("div", { class: "agents-list" });
  if (rows.length === 0) {
    list.appendChild(h("div", { class: "agents-empty" }, "No panes yet."));
  } else {
    for (const r of rows) list.appendChild(agentRow(r));
  }

  const modal = h("div", { class: "agents-modal" }, titleBar, list);
  backdrop.appendChild(modal);
  replaceChildren(root, backdrop);
}

function agentRow(r: AgentRow): HTMLElement {
  const flagged = needsInput(r.state);
  const dot = h("span", { class: "agents-dot", title: stateLabel(r.state) });
  dot.style.setProperty("--dot-heat", heatVar(r.state));

  const chip = h(
    "span",
    { class: `agent-chip agent-${chipKind(r.agent)}` },
    r.agent,
  );

  const main = h(
    "div",
    { class: "agents-row-main" },
    h("span", { class: "agents-row-title" }, r.title),
    h("span", { class: "agents-row-session" }, r.session?.name ?? r.sessionId),
  );

  const right = flagged
    ? h("span", { class: "agents-needs-input" }, "needs input")
    : h("span", { class: "agents-state" }, stateLabel(r.state));

  return h(
    "button",
    {
      class: "agents-row" + (flagged ? " flagged" : ""),
      onclick: () => void gotoPane(r.sessionId, r.pane),
    },
    dot,
    chip,
    main,
    right,
  );
}

/** Map an agent label to a stable chip-kind class for subtle tinting. */
export function chipKind(agent: string): string {
  const a = agent.toLowerCase();
  if (a === "claude") return "claude";
  if (a === "shell") return "shell";
  return "unknown";
}

/** Read the hottest state across all panes — used by the topbar badge. */
export function fleetWaitingCount(): number {
  let n = 0;
  for (const info of getState().paneStates.values()) {
    if (needsInput(info.state)) n++;
  }
  return n;
}

/** The hottest state across all panes (drives the topbar agents button tint). */
export function fleetHottest(): PaneState {
  return hottest([...getState().paneStates.values()].map((i) => i.state));
}
