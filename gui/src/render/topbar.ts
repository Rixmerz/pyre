// Topbar: ember wordmark, session switcher, palette button, theme toggle, settings.

import { h, replaceChildren } from "./dom";
import { icon } from "./icons";
// The pyre mark — angular ember shards. Imported raw so it inlines into the
// wordmark and inherits no external request (offline-first). It carries its own
// ember gradient, so it does NOT use currentColor like the stroke icons.
import logoSvg from "../assets/logo.svg?raw";
import { activeSessionInfo, getState } from "../state";
import {
  openPalette,
  openThemePicker,
  promptRenameSession,
} from "../actions";
import { toggleLightDark } from "../themes";

export function renderTopbar(root: HTMLElement): void {
  const s = getState();
  const active = activeSessionInfo();
  const paneChip = active
    ? h(
        "span",
        { class: "chip", title: "panes in this session" },
        `${active.pane_count}`,
      )
    : null;

  const wordmark = h(
    "div",
    { class: "wordmark" },
    h("span", { class: "ember-mark", html: logoSvg, "aria-hidden": "true" }),
    h("span", { class: "wordmark-text" }, "PYRE"),
  );

  const switcher = h(
    "button",
    {
      class: "session-switcher",
      title: active ? "Rename session" : "No session",
      onclick: () => {
        if (active) void promptRenameSession(active.id);
      },
    },
    h("span", { class: "session-name" }, active?.name ?? "no session"),
    h("span", { class: "switcher-chevron", html: icon("chevronDown") }),
    paneChip,
  );

  const right = h(
    "div",
    { class: "topbar-right" },
    h(
      "button",
      {
        class: "icon-btn palette-btn",
        title: "Command palette  (Cmd/Ctrl+K)",
        onclick: openPalette,
      },
      h("span", { html: icon("command") }),
      h(
        "span",
        { class: "kbd" },
        h("span", { class: "kbd-icon", html: icon("command") }),
        "K",
      ),
    ),
    h(
      "button",
      {
        class: "icon-btn",
        title: "Toggle light / dark",
        "aria-label": "Toggle theme",
        onclick: () => void toggleLightDark(),
      },
      h("span", { html: icon("theme") }),
    ),
    h(
      "button",
      {
        class: "icon-btn",
        title: "Themes",
        "aria-label": "Pick theme",
        onclick: openThemePicker,
      },
      h("span", { html: icon("settings") }),
    ),
  );

  void s;
  replaceChildren(root, wordmark, switcher, right);
}
