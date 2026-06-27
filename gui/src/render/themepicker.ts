// Theme picker popover: a grid of swatches (accent over bg) for every theme from
// list_themes. Selecting calls selectTheme → get_theme → writes CSS vars.

import { h, replaceChildren } from "./dom";
import { getState } from "../state";
import { closeThemePicker } from "../actions";
import { selectTheme } from "../themes";

/** True while the layer is playing its exit animation (guards re-entry). */
let closing = false;

export function renderThemePicker(root: HTMLElement): void {
  const s = getState();
  if (!s.themePickerOpen) {
    closeThemeOverlay(root);
    return;
  }

  // Opening / staying open: cancel any in-flight exit and reveal the layer.
  closing = false;
  root.classList.remove("is-closing");
  root.style.removeProperty("display");
  root.classList.add("is-open");

  const grid = h("div", { class: "theme-grid" });
  for (const t of s.themes) {
    const swatch = h(
      "button",
      {
        class: "theme-swatch" + (t.name === s.activeTheme ? " active" : ""),
        title: `${t.display_name} (${t.kind})`,
        onclick: () => void selectTheme(t.name),
      },
      h("span", {
        class: "swatch-chip",
        style: `background:${t.bg};`,
      }),
      h("span", {
        class: "swatch-accent",
        style: `background:${t.accent};`,
      }),
      h("span", { class: "swatch-name" }, t.display_name),
    );
    // Paint the chip accent via inline style children created above.
    grid.appendChild(swatch);
  }

  if (s.themes.length === 0) {
    grid.appendChild(h("div", { class: "theme-empty" }, "No themes available."));
  }

  const modal = h(
    "div",
    { class: "theme-modal", onclick: (e: Event) => e.stopPropagation() },
    h(
      "div",
      { class: "theme-header" },
      h("span", { class: "section-label" }, "Themes"),
    ),
    grid,
  );

  const backdrop = h(
    "div",
    { class: "theme-backdrop", onclick: () => closeThemePicker() },
    modal,
  );

  replaceChildren(root, backdrop);
}

// ── Overlay open/close animation (shared class contract) ─────────────────────
// Mirrors palette.ts: CSS adds the enter animation off `.is-open`; on close we
// add `.is-closing` (keeping `.is-open`) and wait for the exit animation to end
// before clearing the layer. Reduced motion hides immediately.

/** Fallback when `animationend` never fires (matches --dur-fast). */
const OVERLAY_FALLBACK_MS = 120;

/** Play the exit animation, then clear + hide the theme-picker layer. */
function closeThemeOverlay(root: HTMLElement): void {
  if (!root.classList.contains("is-open")) {
    closing = false; // already hidden — nothing to animate
    return;
  }
  if (closing) return; // exit already in flight

  if (prefersReducedMotion()) {
    hideThemeNow(root);
    return;
  }

  closing = true;
  root.classList.add("is-closing");
  onceAnimationEnd(root, () => {
    // Re-opened mid-close — the open path already restored the layer; abort.
    if (getState().themePickerOpen) {
      closing = false;
      return;
    }
    hideThemeNow(root);
    closing = false;
  }, OVERLAY_FALLBACK_MS);
}

/** Clear the layer's contents and fully hide it (end-of-exit state). */
function hideThemeNow(root: HTMLElement): void {
  replaceChildren(root);
  root.style.display = "none";
  root.classList.remove("is-open", "is-closing");
}

/** True when the user asked the OS to reduce motion. */
function prefersReducedMotion(): boolean {
  return (
    typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches
  );
}

/** Run `cb` once on the next `animationend`, or after `fallbackMs` — whichever
 *  comes first. Tolerant of jsdom (no animations) via the fallback timer. */
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
