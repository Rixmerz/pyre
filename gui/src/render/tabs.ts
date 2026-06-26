// Per-session TAB STRIP — disciplined chrome, not a bold element. A thin row of
// quiet pills above the pane area: one pill per WINDOW of the active session
// (from `list_windows`), then a "+" pill to spawn a new window. Each window owns
// its own splittable layout tree; its name is authoritative from the daemon
// (renamed in place via `rename_window`). The signature stays the heat; the
// strip is restraint.
//
// Always rendered (even with a single window) so the "+" is discoverable.

import { h, replaceChildren } from "./dom";
import { icon } from "./icons";
import { attachRenameAffordance } from "./inline-edit";
import {
  getState,
  windowTabs,
  windowLabel,
  activeWindowOf,
} from "../state";
import {
  newPaneAction,
  switchWindow,
  closeWindowAction,
  renameWindowAction,
} from "../actions";
import type { WindowInfo } from "../types";

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
  const windows = windowTabs(session);
  const active = activeWindowOf(session);

  // Only offer a per-pill close when more than one window exists — the last
  // window is closed via "Close session" rather than stranding the session.
  const closeable = windows.length > 1;
  const pills = windows.map((w) => pill(w, active, closeable));

  const addPill = h(
    "button",
    {
      class: "tab tab-add",
      type: "button",
      title: "New window in this session",
      "aria-label": "New window in this session",
      onclick: () => void newPaneAction(),
    },
    h("span", { class: "tab-add-icon", html: icon("plus") }),
  );

  replaceChildren(root, h("div", { class: "tabstrip-row" }, ...pills, addPill));
}

/** One pill: a window (icon + daemon-named label + optional ×). */
function pill(
  window: WindowInfo,
  active: string | null,
  closeable: boolean,
): HTMLElement {
  const isActive = window.id === active;
  const label = windowLabel(window);

  // The window label OWNS its double-click rename affordance (commit →
  // rename_window): single-click switches to the window, double-click opens the
  // inline editor; the affordance stops propagation so the outer pill's onclick
  // never fires mid-rename and no destructive re-render tears the span out.
  const labelSpan = h("span", { class: "tab-label" }, label);
  attachRenameAffordance({
    label: labelSpan,
    value: () => windowLabel(window),
    onSingleClick: () => switchWindow(window.id),
    inputClass: "inline-edit-tab",
    ariaLabel: "Rename window",
    onCommit: (name) => renameWindowAction(window.id, name),
  });

  const children: (HTMLElement | undefined)[] = [
    h("span", { class: "tab-icon", html: icon("split") }),
    labelSpan,
  ];

  if (closeable) {
    children.push(
      h(
        "button",
        {
          class: "tab-close",
          type: "button",
          title: "Close window",
          "aria-label": "Close window",
          onclick: (e: Event) => {
            e.stopPropagation();
            void closeWindowAction(window.id);
          },
        },
        h("span", { html: icon("cross") }),
      ),
    );
  }

  return h(
    "button",
    {
      class: "tab tab-window" + (isActive ? " active" : ""),
      type: "button",
      title: label,
      "aria-label": "window tab",
      "aria-current": isActive ? "true" : undefined,
      "data-window": window.id,
      onclick: () => switchWindow(window.id),
    },
    ...children,
  );
}
