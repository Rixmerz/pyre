// Status bar: daemon connectivity + socket, total pane count, focused session,
// active theme name.

import { h, replaceChildren } from "./dom";
import { activeSessionInfo, getState, totalPaneCount } from "../state";
import { reconnect } from "../api";
import { setState } from "../state";

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

  const right = h(
    "div",
    { class: "status-group right" },
    h("span", { class: "status-item" }, `${totalPaneCount()} panes`),
    active && h("span", { class: "status-item" }, active.name),
    h("span", { class: "status-item theme" }, s.activeTheme),
  );

  replaceChildren(root, daemon, right);
}
