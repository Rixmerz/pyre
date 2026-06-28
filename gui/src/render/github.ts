// GitHub device-code modal + account-menu popover.
//
// Render-discipline (gui/.claude/rules/render-discipline.md):
//
//   • The modal is PATTERN B — a persistent `.is-open` shell whose entrance
//     keyframes are bound to the CLASS FLIP (`.is-open .gh-modal`), so opening
//     plays the rise exactly once and the ~750ms poll never replays it. The
//     device code + verification URI are stable for a whole link attempt, so a
//     content fingerprint that EXCLUDES the per-second countdown skips the
//     rebuild on every idle tick. The expiry countdown updates IN PLACE: a
//     dedicated 1s interval sets `textContent` on a captured <span> and never
//     touches the modal structure — so the live timer cannot reintroduce the
//     flicker. (The interval is independent of the daemon poll on purpose: the
//     timer must keep ticking even while the daemon is down, since GitHub is
//     owned by the Tauri layer, not pyred.)
//
//   • The account menu is PATTERN A — a fingerprint-guarded popover in its own
//     layer. The topbar that hosts the chip rebuilds every poll, which would
//     tear a child menu down; a separate guarded layer survives the poll.

import { h, replaceChildren } from "./dom";
import { icon } from "./icons";
import { getState } from "../state";
import type { GhAccount } from "../types";
import {
  cancelGitHubLink,
  closeGhMenu,
  disconnectGitHub,
  getLinkDeadline,
  getRevokeUrl,
  openExternalUrl,
} from "../github-link";
import { toast } from "../toast";

/** Fallback when `animationend` never fires (matches --dur-fast). */
const OVERLAY_FALLBACK_MS = 120;

// ── Device-code modal (Pattern B + in-place countdown) ───────────────────────

/** True while the modal is playing its exit animation (guards re-entry). */
let modalClosing = false;
/** Captured countdown <span> — updated in place by the 1s tick, never rebuilt. */
let countdownEl: HTMLElement | null = null;
/** Last content fingerprint that triggered a modal rebuild (countdown excluded). */
let lastModalFp = "";
/** The 1s countdown interval handle (null when the modal is closed). */
let countdownTimer: number | null = null;

export function renderGhModal(root: HTMLElement): void {
  const s = getState();
  const linking = s.github.linking;
  if (!linking) {
    closeGhModalOverlay(root);
    // Reset so a fresh open rebuilds and plays the entrance animation once.
    lastModalFp = "";
    return;
  }

  // Opening / staying open: cancel any in-flight exit and reveal the layer.
  modalClosing = false;
  root.classList.remove("is-closing");
  root.style.removeProperty("display");
  root.classList.add("is-open");

  // Fingerprint = ONLY the stable content (code, URI, status). The countdown is
  // deliberately excluded — it changes every second and is painted in place.
  const fp = `${linking.user_code}\x01${linking.verification_uri}\x01${s.github.status}`;
  if (fp === lastModalFp && root.childElementCount > 0) {
    // Content unchanged: skip the rebuild (so the rise keyframe doesn't replay)
    // but keep the countdown fresh between 1s ticks.
    applyCountdownInPlace();
    return;
  }
  lastModalFp = fp;
  buildModal(root, linking.user_code, linking.verification_uri);
  ensureCountdownTick();
  applyCountdownInPlace();
}

function buildModal(root: HTMLElement, code: string, uri: string): void {
  const codeBtn = h(
    "button",
    {
      class: "gh-code",
      title: "Click to copy",
      "aria-label": `Device code ${code} — click to copy`,
      onclick: () => void copyCode(code),
    },
    code,
  );

  const countdown = h("span", { class: "gh-countdown" }, "—");
  countdownEl = countdown;

  const modal = h(
    "div",
    { class: "gh-modal", onclick: (e: Event) => e.stopPropagation() },
    h(
      "div",
      { class: "gh-modal-head" },
      h("span", { class: "gh-mark", html: icon("github") }),
      h("span", { class: "gh-modal-title" }, "Connect GitHub"),
    ),
    h(
      "p",
      { class: "gh-modal-sub" },
      "Enter this code at GitHub to authorize pyre:",
    ),
    codeBtn,
    h(
      "div",
      { class: "gh-uri-row" },
      h("span", { class: "gh-uri" }, uri),
      h(
        "button",
        {
          class: "gh-btn gh-btn-primary",
          onclick: () => void openExternalUrl(uri),
        },
        "Open in browser",
      ),
    ),
    h(
      "div",
      { class: "gh-waiting" },
      h("span", { class: "gh-spinner", html: icon("spinner") }),
      h("span", { class: "gh-waiting-text" }, "Waiting for authorization…"),
      h("span", { class: "gh-countdown-wrap" }, "expires in ", countdown),
    ),
    h(
      "div",
      { class: "gh-actions" },
      h(
        "button",
        { class: "gh-btn", onclick: () => cancelGitHubLink() },
        "Cancel",
      ),
    ),
  );

  const backdrop = h(
    "div",
    {
      class: "gh-backdrop",
      role: "dialog",
      "aria-modal": "true",
      "aria-label": "Connect GitHub",
      onclick: () => cancelGitHubLink(),
    },
    modal,
  );

  replaceChildren(root, backdrop);
}

async function copyCode(code: string): Promise<void> {
  try {
    await navigator.clipboard?.writeText(code);
    toast("Device code copied.", "success");
  } catch (err) {
    console.warn("[pyre-github] clipboard write failed:", err);
    toast("Couldn't copy — type the code manually.", "error");
  }
}

/** Format remaining ms as "m:ss" (clamped at zero). */
function fmtRemaining(ms: number): string {
  const total = Math.max(0, Math.round(ms / 1000));
  const m = Math.floor(total / 60);
  const sec = total % 60;
  return `${m}:${String(sec).padStart(2, "0")}`;
}

/** Paint ONLY the countdown <span> — never the modal structure (no flicker). */
function applyCountdownInPlace(): void {
  if (!countdownEl) return;
  const dl = getLinkDeadline();
  countdownEl.textContent = dl == null ? "—" : fmtRemaining(dl - Date.now());
}

function ensureCountdownTick(): void {
  if (countdownTimer != null) return;
  countdownTimer = window.setInterval(applyCountdownInPlace, 1000);
}

function stopCountdownTick(): void {
  if (countdownTimer != null) {
    window.clearInterval(countdownTimer);
    countdownTimer = null;
  }
}

// ── Modal open/close animation (shared `.is-open`/`.is-closing` contract) ─────
// Mirrors palette.ts / themepicker.ts: CSS adds the enter animation off
// `.is-open`; on close we add `.is-closing` (keeping `.is-open`) and wait for the
// exit animation before clearing the layer. Reduced motion hides immediately.

function closeGhModalOverlay(root: HTMLElement): void {
  // Stop the per-second tick and drop the ref the moment we begin closing.
  stopCountdownTick();
  countdownEl = null;

  if (!root.classList.contains("is-open")) {
    modalClosing = false; // already hidden — nothing to animate
    return;
  }
  if (modalClosing) return; // exit already in flight

  if (prefersReducedMotion()) {
    hideGhModalNow(root);
    return;
  }

  modalClosing = true;
  root.classList.add("is-closing");
  onceAnimationEnd(
    root,
    () => {
      // Re-opened mid-close — the open path already restored the layer; abort.
      if (getState().github.linking) {
        modalClosing = false;
        return;
      }
      hideGhModalNow(root);
      modalClosing = false;
    },
    OVERLAY_FALLBACK_MS,
  );
}

function hideGhModalNow(root: HTMLElement): void {
  replaceChildren(root);
  root.style.display = "none";
  root.classList.remove("is-open", "is-closing");
}

// ── Account menu popover (Pattern A — fingerprint-guarded) ────────────────────

/** Last menu fingerprint that triggered a rebuild. */
let lastMenuFp = "";

export function renderGhMenu(root: HTMLElement): void {
  const s = getState();
  const account = s.github.account;
  const open = s.ghMenuOpen && account != null;
  root.classList.toggle("open", open);
  if (!open || account == null) {
    replaceChildren(root);
    lastMenuFp = "";
    return;
  }

  const fp = `${account.login}\x01${account.name ?? ""}\x01${account.html_url}`;
  if (fp === lastMenuFp && root.childElementCount > 0) return; // skip rebuild
  lastMenuFp = fp;
  buildMenu(root, account);
}

function buildMenu(root: HTMLElement, account: GhAccount): void {
  const menu = h(
    "div",
    { class: "gh-menu", role: "menu", onclick: (e: Event) => e.stopPropagation() },
    h(
      "div",
      { class: "gh-menu-head" },
      h("img", {
        class: "gh-avatar",
        src: account.avatar_url,
        alt: "",
        width: 28,
        height: 28,
      }),
      h(
        "div",
        { class: "gh-menu-id" },
        h("span", { class: "gh-menu-login" }, `@${account.login}`),
        account.name && h("span", { class: "gh-menu-name" }, account.name),
      ),
    ),
    h(
      "button",
      {
        class: "gh-menu-item",
        role: "menuitem",
        onclick: () => {
          closeGhMenu();
          void openExternalUrl(account.html_url);
        },
      },
      "Open profile",
    ),
    h(
      "button",
      {
        class: "gh-menu-item",
        role: "menuitem",
        title: "Revoke the grant on GitHub (a local disconnect keeps it active)",
        onclick: () => {
          closeGhMenu();
          void openExternalUrl(getRevokeUrl());
        },
      },
      "Manage on GitHub",
    ),
    h(
      "button",
      {
        class: "gh-menu-item gh-menu-danger",
        role: "menuitem",
        onclick: () => {
          void disconnectGitHub();
        },
      },
      "Disconnect",
    ),
  );

  const backdrop = h(
    "div",
    { class: "gh-menu-backdrop", onclick: () => closeGhMenu() },
    menu,
  );
  replaceChildren(root, backdrop);
}

// ── Shared helpers (mirror palette.ts / themepicker.ts) ───────────────────────

function prefersReducedMotion(): boolean {
  return (
    typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches
  );
}

function onceAnimationEnd(
  el: HTMLElement,
  cb: () => void,
  fallbackMs: number,
): void {
  let done = false;
  const run = (): void => {
    if (done) return;
    done = true;
    el.removeEventListener("animationend", run);
    cb();
  };
  el.addEventListener("animationend", run, { once: true });
  window.setTimeout(run, fallbackMs);
}
