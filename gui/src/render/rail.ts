// Left rail: session list. Each row = a heat dot (hottest pane state in that
// session) + name + pane count. Collapsible to an icon strip. Bottom = new session.

import { h, replaceChildren } from "./dom";
import { icon } from "./icons";
import { getState, panesOfSession } from "../state";
import { heatVar, hottest } from "../heat";
import {
  closeSessionAction,
  newSession,
  switchSession,
  toggleRail,
} from "../actions";
import type { PaneState } from "../types";

export function renderRail(root: HTMLElement): void {
  const s = getState();
  root.classList.toggle("collapsed", s.railCollapsed);

  const header = h(
    "div",
    { class: "rail-header" },
    !s.railCollapsed && h("span", { class: "section-label" }, "Sessions"),
    h(
      "button",
      {
        class: "icon-btn rail-toggle",
        title: s.railCollapsed ? "Expand rail" : "Collapse rail",
        onclick: toggleRail,
      },
      h("span", { html: icon("rail") }),
    ),
  );

  const list = h("div", { class: "rail-list" });
  for (const sess of s.sessions) {
    const states = panesOfSession(sess.id).map((p) => p.state as PaneState);
    const hot = hottest(states);
    const isActive = sess.id === s.activeSession;
    // Close (×) control. Span (not <button>) to avoid nesting interactive
    // elements inside the row button; click stops propagation so it doesn't
    // also switch session. Hidden until the row is hovered (CSS).
    const closeBtn =
      !s.railCollapsed &&
      h(
        "span",
        {
          class: "rail-close",
          role: "button",
          title: "Close session",
          onclick: (e: Event) => {
            e.stopPropagation();
            e.preventDefault();
            void closeSessionAction(sess.id);
          },
        },
        h("span", { class: "rail-close-icon", html: icon("close") }),
      );

    const row = h(
      "button",
      {
        class: "rail-row" + (isActive ? " active" : ""),
        title: `${sess.name} · ${sess.pane_count} pane${sess.pane_count === 1 ? "" : "s"}`,
        onclick: () => void switchSession(sess.id),
      },
      heatDot(hot),
      !s.railCollapsed &&
        h(
          "span",
          { class: "rail-name" },
          h("span", { class: "rail-session-name" }, sess.name),
          h("span", { class: "rail-pane-count" }, `${sess.pane_count}`),
        ),
      closeBtn,
    );
    list.appendChild(row);
  }

  if (s.sessions.length === 0 && !s.railCollapsed) {
    list.appendChild(
      h("div", { class: "rail-empty" }, "No sessions yet."),
    );
  }

  const newBtn = h(
    "button",
    {
      class: "rail-new",
      title: "New session",
      onclick: () => void newSession(),
    },
    h("span", { class: "rail-new-icon", html: icon("plus") }),
    !s.railCollapsed && h("span", {}, "New session"),
  );

  replaceChildren(root, header, list, newBtn);
}

function heatDot(state: PaneState): HTMLElement {
  const dot = h("span", { class: "heat-dot", "data-state": state });
  dot.style.setProperty("--dot-heat", heatVar(state));
  return dot;
}
