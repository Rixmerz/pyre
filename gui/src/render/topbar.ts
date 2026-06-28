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
  toggleAgents,
} from "../actions";
import { fleetWaitingCount } from "./agents";
import { startGitHubLink, toggleGhMenu } from "../github-link";
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

  const waiting = fleetWaitingCount();
  const agentsBtn = h(
    "button",
    {
      class: "icon-btn agents-btn" + (waiting > 0 ? " has-waiting" : ""),
      title:
        waiting > 0
          ? `Agents — ${waiting} pane${waiting === 1 ? "" : "s"} need input  (Ctrl+Shift+A)`
          : "Agent overview  (Ctrl+Shift+A)",
      "aria-label": "Agent overview",
      onclick: () => toggleAgents(),
    },
    h("span", { html: icon("agents") }),
    waiting > 0 && h("span", { class: "agents-btn-badge" }, String(waiting)),
  );

  // GitHub chip: connected → avatar + @login (click opens the account menu
  // popover, which lives in its own poll-survivable layer); disconnected → a
  // "Connect GitHub" button that starts the device flow. The topbar rebuilds
  // every poll, so the chip carries NO entrance keyframe (render-discipline LOW
  // region) and the popover deliberately lives outside the topbar.
  const gh = s.github;
  const ghChip = gh.account
    ? h(
        "button",
        {
          class: "gh-chip gh-chip-connected",
          title: `GitHub — @${gh.account.login}`,
          "aria-label": `GitHub account @${gh.account.login}`,
          onclick: () => toggleGhMenu(),
        },
        h("img", {
          class: "gh-avatar",
          src: gh.account.avatar_url,
          alt: "",
          width: 18,
          height: 18,
        }),
        h("span", { class: "gh-chip-login" }, `@${gh.account.login}`),
      )
    : h(
        "button",
        {
          class: "gh-chip",
          title: "Connect a GitHub account",
          "aria-label": "Connect GitHub",
          onclick: () => void startGitHubLink(),
        },
        h("span", { class: "gh-chip-icon", html: icon("github") }),
        h("span", { class: "gh-chip-login" }, "Connect GitHub"),
      );

  const right = h(
    "div",
    { class: "topbar-right" },
    agentsBtn,
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
    ghChip,
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
