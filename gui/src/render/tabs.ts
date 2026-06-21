// Per-session TAB STRIP — disciplined chrome, not a bold element. A thin row of
// quiet pills above the pane area: the "split" tab (the layout tree), then one
// pill per STANDALONE pane (a pane in pane_states for this session that is NOT a
// leaf of the layout tree — see open_pane in api.ts), then a "+" pill to spawn a
// new standalone pane. The signature stays the heat; the strip is restraint.
//
// Always rendered (even with only the split tab) so the "+" is discoverable.

import { h, replaceChildren } from "./dom";
import { icon } from "./icons";
import {
  getState,
  paneStateOf,
  sessionTabs,
  activeTabOf,
  SPLIT_TAB,
  type SessionTab,
} from "../state";
import { heatVar, pulses, stateLabel } from "../heat";
import { newPaneAction, switchTab, closeStandalonePane } from "../actions";
import type { PaneState } from "../types";

/** Short, stable label for a standalone pane tab: its agent, else an 8-char id. */
function paneLabel(pane: string): string {
  const info = paneStateOf(pane);
  const agent = (info?.agent ?? "").trim();
  if (agent) return agent;
  return pane.slice(0, 8);
}

/** Render the tab strip for the active session into `root`. Hidden if no session. */
export function renderTabs(root: HTMLElement): void {
  const s = getState();
  const session = s.activeSession;

  // No active session (or disconnected) → nothing to tab over.
  if (!session || !s.connected) {
    root.classList.remove("has-strip");
    replaceChildren(root);
    return;
  }

  root.classList.add("has-strip");
  const tabs = sessionTabs(session);
  const active = activeTabOf(session);

  const pills = tabs.map((tab) => pill(tab, active));

  const addPill = h(
    "button",
    {
      class: "tab tab-add",
      type: "button",
      title: "New pane in this session",
      "aria-label": "New pane in this session",
      onclick: () => void newPaneAction(),
    },
    h("span", { class: "tab-add-icon", html: icon("plus") }),
  );

  replaceChildren(root, h("div", { class: "tabstrip-row" }, ...pills, addPill));
}

/** One pill: the split tab, or a standalone pane tab (heat dot + label + ×). */
function pill(tab: SessionTab, active: string): HTMLElement {
  if (tab.kind === "split") {
    const isActive = active === SPLIT_TAB;
    return h(
      "button",
      {
        class: "tab tab-split" + (isActive ? " active" : ""),
        type: "button",
        title: "split",
        "aria-label": "split layout tab",
        "aria-current": isActive ? "true" : undefined,
        onclick: () => switchTab(SPLIT_TAB),
      },
      h("span", { class: "tab-icon", html: icon("split") }),
      h("span", { class: "tab-label" }, "split"),
    );
  }

  const pane = tab.pane;
  const info = paneStateOf(pane);
  const state: PaneState = info?.state ?? "idle";
  const isActive = active === pane;

  const dot = h("span", { class: "tab-dot", title: stateLabel(state) });
  dot.style.setProperty("--dot-heat", heatVar(state));

  const label = paneLabel(pane);

  const close = h(
    "button",
    {
      class: "tab-close",
      type: "button",
      title: "Close pane",
      "aria-label": "Close pane",
      onclick: (e: Event) => {
        e.stopPropagation();
        void closeStandalonePane(pane);
      },
    },
    h("span", { html: icon("cross") }),
  );

  return h(
    "button",
    {
      class:
        "tab tab-pane" +
        (isActive ? " active" : "") +
        (pulses(state) ? " pulse" : ""),
      type: "button",
      title: `${label} — ${stateLabel(state)}`,
      "aria-current": isActive ? "true" : undefined,
      "data-pane": pane,
      onclick: () => switchTab(pane),
    },
    dot,
    h("span", { class: "tab-label" }, label),
    close,
  );
}
