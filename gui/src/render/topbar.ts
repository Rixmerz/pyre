// Topbar: ember wordmark, session switcher, palette button, theme toggle, settings.

import { h, replaceChildren } from "./dom";
import { icon } from "./icons";
// The pyre mark — angular ember shards. Imported raw so it inlines into the
// wordmark and inherits no external request (offline-first). It carries its own
// ember gradient, so it does NOT use currentColor like the stroke icons.
import logoSvg from "../assets/logo.svg?raw";
import { activeSessionInfo, getState, type AppState } from "../state";
import {
  openPalette,
  openThemePicker,
  promptRenameSession,
  toggleAgents,
} from "../actions";
import { fleetWaitingCount } from "./agents";
import { startGitHubLink, toggleGhMenu } from "../github-link";
import { toggleLightDark } from "../themes";

/**
 * Canonical string of every DYNAMIC value the topbar RENDERS — so an idle poll
 * tick keeps the same string and skips the replaceChildren rebuild (which would
 * re-create the session switcher pill, the agents/palette/github/theme buttons
 * mid-:hover → the flicker). A genuine change rebuilds it exactly once. Inputs,
 * each grepped from the render below:
 *   - active session id   (switcher onclick captures `active.id`; the rename
 *     would target the WRONG session if a same-name/same-count swap skipped the
 *     rebuild, so id is load-bearing, not decorative)
 *   - active session name  (the `.session-name` span; "no session" when null)
 *   - active session pane_count  (the `.chip` pane count; absent when null)
 *   - github account login  ("disconnected" when no account → the chip flips
 *     between connected avatar+@login and the "Connect GitHub" button)
 *   - fleet "waiting" count  (the agents button's has-waiting class, title and
 *     `.agents-btn-badge`)
 * Nothing else in the topbar is dynamic: the wordmark, palette, theme-toggle and
 * settings buttons are static, and the github account-menu popover lives in its
 * own poll-survivable layer (not this DOM). Separator `\x01` can't collide with
 * rendered text. Mirrors railFingerprint / agentsFingerprint.
 */
function topbarFingerprint(s: Readonly<AppState>): string {
  const active = activeSessionInfo();
  const ghLogin = s.github.account?.login ?? "disconnected";
  const waiting = fleetWaitingCount();
  return [
    active?.id ?? "",
    active?.name ?? "",
    active?.pane_count ?? "",
    ghLogin,
    waiting,
  ].join("\x01");
}

/** Last fingerprint that triggered a full topbar rebuild. */
let lastTopbarFp = "";

export function renderTopbar(root: HTMLElement): void {
  const s = getState();
  const fp = topbarFingerprint(s);
  if (fp === lastTopbarFp && root.childElementCount > 0) {
    // Active session (id/name/count), github login and waiting count are all
    // unchanged — skip the rebuild so the topbar buttons and the switcher pill
    // survive :hover (childElementCount > 0 forces a build on first paint).
    return;
  }
  lastTopbarFp = fp;

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
