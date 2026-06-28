// Left rail: session list. Each row = a heat dot (hottest pane state in that
// session) + name + pane count. Collapsible to an icon strip. Bottom = new session.

import { h, replaceChildren } from "./dom";
import { icon } from "./icons";
import { attachRenameAffordance } from "./inline-edit";
import { getSessionGit, getState, panesOfSession, type AppState } from "../state";
import { heatVar, hottest } from "../heat";
import {
  closeSessionAction,
  newSession,
  renameSessionAction,
  switchSession,
  toggleRail,
} from "../actions";
import type { GitInfo, PaneState } from "../types";

/**
 * Canonical string of everything the rail rows RENDER from — per session: id,
 * name, pane-count and hottest pane state — plus the collapsed flag. The ACTIVE
 * session is deliberately EXCLUDED: selecting a session must not rebuild the rail
 * (it would destroy hover state and lose clicks that land mid-rebuild). The
 * active class is moved IN-PLACE by applyActiveSessionInPlace instead. Mirrors
 * center.ts windowsFingerprint: a no-op poll tick keeps the same string and skips
 * the replaceChildren tear-down; a genuine change (rename, new/closed session,
 * heat shift) changes the string and forces a rebuild.
 */
function railFingerprint(s: Readonly<AppState>): string {
  const rows = s.sessions.map((sess) => {
    const states = panesOfSession(sess.id).map((p) => p.state as PaneState);
    // Git is folded into the fingerprint so a real change (branch switch,
    // dirty count, ahead/behind shift) rebuilds the row exactly once. Paired
    // with setSessionGit's change-gate, a steady repo keeps the same string ->
    // zero rebuilds; only an actual git delta moves it.
    const g = getSessionGit(sess.id);
    const git = `${g?.branch ?? ""}:${g?.dirty ?? 0}:${g?.ahead ?? 0}:${g?.behind ?? 0}`;
    return `${sess.id}${sess.name}${sess.pane_count}${hottest(states)}${git}`;
  });
  return `c:${s.railCollapsed ? 1 : 0}|[${rows.join("")}]`;
}

/** Last fingerprint that triggered a full rail rebuild. */
let lastRailFingerprint = "";

export function renderRail(root: HTMLElement): void {
  const s = getState();
  root.classList.toggle("collapsed", s.railCollapsed);

  const fp = railFingerprint(s);
  if (fp === lastRailFingerprint && root.childElementCount > 0) {
    // Session set, names, counts, heat and collapse are all unchanged. Only the
    // active selection may have moved — that is applied in-place by
    // applyActiveSessionInPlace (render/index.ts). Skip the rebuild so hover
    // state survives and a click never lands on a node being replaced.
    return;
  }
  lastRailFingerprint = fp;

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

    // Session name span — double-click swaps it for an inline editor (commit →
    // rename_session). Single-click switches session. The name span OWNS both
    // (via attachRenameAffordance): it stops propagation to the row button and
    // debounces the single-click so a double-click never first fires
    // switchSession (whose async reload used to rebuild the rail between the two
    // clicks and destroy this span before dblclick could land on it).
    const nameSpan =
      !s.railCollapsed &&
      (h("span", { class: "rail-session-name" }, sess.name) as HTMLElement);
    if (nameSpan) {
      attachRenameAffordance({
        label: nameSpan,
        value: () => sess.name,
        onSingleClick: () => void switchSession(sess.id),
        inputClass: "inline-edit-rail",
        ariaLabel: "Rename session",
        onCommit: (name) => renameSessionAction(sess.id, name),
      });
    }

    const row = h(
      "button",
      {
        class: "rail-row" + (isActive ? " active" : ""),
        "data-session": sess.id,
        title: `${sess.name} · ${sess.pane_count} pane${sess.pane_count === 1 ? "" : "s"}`,
        onclick: () => void switchSession(sess.id),
      },
      heatDot(hot),
      !s.railCollapsed &&
        h(
          "span",
          { class: "rail-name" },
          nameSpan,
          gitChip(getSessionGit(sess.id)),
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

/**
 * Move the `.active` highlight to the current session's rail row IN-PLACE — no
 * rebuild. Mirrors applyFocusInPlace (center.ts): query the live rows by
 * data-session and toggle `.active` so only state.activeSession's row carries it.
 * Because the row node is NOT replaced (active was dropped from railFingerprint),
 * selecting a session never tears out the row — hover survives and a click can't
 * be lost to a mid-rebuild node swap. Called from render/index.ts after every
 * render so it lands even when renderRail early-returns on an unchanged
 * fingerprint. Idempotent.
 */
export function applyActiveSessionInPlace(): void {
  const active = getState().activeSession;
  document
    .querySelectorAll<HTMLElement>(".rail-row[data-session]")
    .forEach((row) => {
      row.classList.toggle("active", row.dataset["session"] === active);
    });
}

function heatDot(state: PaneState): HTMLElement {
  const dot = h("span", { class: "heat-dot", "data-state": state });
  dot.style.setProperty("--dot-heat", heatVar(state));
  return dot;
}

/**
 * Compact per-session git chip: `⎇ <branch>` always, then `●<dirty>` / `↑<ahead>`
 * / `↓<behind>` only when each is non-zero. Returns null when git is unknown (not
 * a repo, or not polled yet) so the row carries no chip — lives inside `.rail-name`
 * so it hides with the rail when collapsed. Long branch names truncate via CSS
 * (`.git-branch` max-width + ellipsis), never JS. Display-only — no handlers. The
 * `title` carries the full detail for hover.
 */
function gitChip(git: GitInfo | undefined): HTMLElement | null {
  if (!git) return null;
  const detail =
    git.branch +
    (git.upstream ? ` (${git.upstream})` : "") +
    ` · ${git.dirty} dirty · ↑${git.ahead} ↓${git.behind}`;
  return h(
    "span",
    { class: "git-chip", title: detail },
    h("span", { class: "git-branch" }, `⎇ ${git.branch}`),
    git.dirty > 0 && h("span", { class: "git-dirty" }, `●${git.dirty}`),
    git.ahead > 0 && h("span", { class: "git-ahead" }, `↑${git.ahead}`),
    git.behind > 0 && h("span", { class: "git-behind" }, `↓${git.behind}`),
  );
}
