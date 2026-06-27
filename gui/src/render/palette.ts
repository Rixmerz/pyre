// Command palette (⌘K): centered modal, fuzzy-filter input + scrollable list of
// ALL actions. Keyboard nav (↑ ↓ enter esc). Supports one level of submenu
// (e.g. "Switch session…", "Pick theme…").

import { h, replaceChildren } from "./dom";
import { icon } from "./icons";
import { getState } from "../state";
import { buildCommands, closePalette, type Command } from "../actions";

let selectedIndex = 0;
let filter = "";
let submenu: Command | null = null;
/** True while the layer is playing its exit animation (guards re-entry). */
let closing = false;

// Persistent shell refs. The <input> is created ONCE per open and reused across
// keystrokes, so the browser preserves its caret/focus/value naturally. Typing
// re-renders ONLY the results list — never the input — which is what fixes the
// reversed-characters bug (a full rebuild used to destroy the live input node and
// reset the caret to index 0, prepending every new char). Refs are dropped on
// close so the next open builds a fresh, focused input.
let shellRoot: HTMLElement | null = null;
let inputEl: HTMLElement | null = null;
let listEl: HTMLElement | null = null;
let shellSubmenu: Command | null = null;

/** Reset transient palette UI state when it opens. */
export function resetPalette(): void {
  selectedIndex = 0;
  filter = "";
  submenu = null;
}

function activeCommands(): Command[] {
  const base = submenu?.children ? submenu.children() : buildCommands();
  if (!filter.trim()) return base;
  const f = filter.toLowerCase();
  return base.filter((c) => fuzzy(c.title.toLowerCase(), f));
}

export function renderPalette(root: HTMLElement): void {
  const s = getState();
  if (!s.paletteOpen) {
    closePaletteOverlay(root);
    // Drop shell refs so the next open rebuilds a fresh, focused input.
    shellRoot = null;
    inputEl = null;
    listEl = null;
    shellSubmenu = null;
    return;
  }

  // Opening / staying open: cancel any in-flight exit and reveal the layer.
  closing = false;
  root.classList.remove("is-closing");
  root.style.removeProperty("display");
  root.classList.add("is-open");

  // Rebuild the shell (backdrop + modal + persistent input) only when it does not
  // yet exist for this root, or the submenu context changed (placeholder + back
  // button differ and the filter is reset). Keystrokes never reach here — the
  // oninput handler re-renders the list only — so the live <input> survives intact.
  const needsShell =
    inputEl === null || shellRoot !== root || shellSubmenu !== submenu;
  if (needsShell) buildShell(root);

  renderList(root);
}

/** Build the palette container with the persistent input. Focuses the input once. */
function buildShell(root: HTMLElement): void {
  const input = h("input", {
    class: "palette-input",
    type: "text",
    placeholder: submenu ? `${submenu.title}` : "Type a command…",
    value: filter,
    spellcheck: false,
    oninput: (e: Event) => {
      filter = (e.target as HTMLInputElement).value;
      selectedIndex = 0;
      // Re-render ONLY the results list — never the input — so the caret, focus
      // and value the browser is tracking survive the keystroke.
      renderList(root);
    },
  });

  const list = h("div", { class: "palette-list", role: "listbox" });

  const modal = h(
    "div",
    { class: "palette-modal", onclick: (e: Event) => e.stopPropagation() },
    h(
      "div",
      { class: "palette-search" },
      submenu &&
        h(
          "button",
          {
            class: "palette-back",
            title: "Back",
            onclick: () => {
              submenu = null;
              filter = "";
              selectedIndex = 0;
              renderPalette(root);
            },
          },
          h("span", { html: icon("chevronLeft") }),
        ),
      input,
    ),
    list,
  );

  const backdrop = h(
    "div",
    { class: "palette-backdrop", onclick: () => closePalette() },
    modal,
  );

  replaceChildren(root, backdrop);

  shellRoot = root;
  inputEl = input;
  listEl = list;
  shellSubmenu = submenu;

  // Focus the input ONCE on open. Subsequent keystrokes keep focus naturally.
  requestAnimationFrame(() => input.focus());
}

/** Render the filtered command rows into the persistent list container only. */
function renderList(root: HTMLElement): void {
  const list = listEl;
  if (!list) return;

  const cmds = activeCommands();
  if (selectedIndex >= cmds.length) selectedIndex = Math.max(0, cmds.length - 1);

  const rows: Node[] = cmds.map((c, i) =>
    h(
      "div",
      {
        class: "palette-row" + (i === selectedIndex ? " selected" : ""),
        role: "option",
        onmouseenter: () => {
          selectedIndex = i;
          markSelected(list, i);
        },
        onclick: () => runCommand(c, root),
      },
      h("span", { class: "palette-title" }, c.title),
      c.children &&
        h("span", {
          class: "palette-submenu-hint",
          html: icon("chevronRight"),
        }),
      c.hint && !c.children && h("span", { class: "palette-hint" }, c.hint),
    ),
  );

  if (cmds.length === 0) {
    rows.push(h("div", { class: "palette-empty" }, "No matching commands."));
  }

  list.replaceChildren(...rows);
}

/** Keyboard handler wired globally while the palette is open. */
export function handlePaletteKey(e: KeyboardEvent, root: HTMLElement): void {
  const cmds = activeCommands();
  if (e.key === "ArrowDown") {
    e.preventDefault();
    selectedIndex = Math.min(cmds.length - 1, selectedIndex + 1);
    markSelected(root.querySelector(".palette-list"), selectedIndex);
  } else if (e.key === "ArrowUp") {
    e.preventDefault();
    selectedIndex = Math.max(0, selectedIndex - 1);
    markSelected(root.querySelector(".palette-list"), selectedIndex);
  } else if (e.key === "Enter") {
    e.preventDefault();
    const c = cmds[selectedIndex];
    if (c) runCommand(c, root);
  } else if (e.key === "Escape") {
    e.preventDefault();
    if (submenu) {
      submenu = null;
      filter = "";
      selectedIndex = 0;
      renderPalette(root);
    } else {
      closePalette();
    }
  }
}

function runCommand(c: Command, root: HTMLElement): void {
  if (c.children) {
    submenu = c;
    filter = "";
    selectedIndex = 0;
    renderPalette(root);
    return;
  }
  closePalette();
  void c.run?.();
}

function markSelected(list: Element | null, idx: number): void {
  if (!list) return;
  const rows = list.querySelectorAll(".palette-row");
  rows.forEach((r, i) => r.classList.toggle("selected", i === idx));
  rows[idx]?.scrollIntoView({ block: "nearest" });
}

/** Subsequence fuzzy match: every char of `needle` appears in order in `hay`. */
function fuzzy(hay: string, needle: string): boolean {
  let i = 0;
  for (const ch of hay) {
    if (ch === needle[i]) i++;
    if (i === needle.length) return true;
  }
  return needle.length === 0;
}

// ── Overlay open/close animation (shared class contract) ─────────────────────
// CSS adds the enter animation off `.is-open`; on close we add `.is-closing`
// (keeping `.is-open` so it stays visible) and wait for the exit animation to end
// before clearing the layer. Reduced motion hides immediately.

/** Fallback when `animationend` never fires (matches --dur-fast). */
const OVERLAY_FALLBACK_MS = 120;

/** Play the exit animation, then clear + hide the palette layer. */
function closePaletteOverlay(root: HTMLElement): void {
  if (!root.classList.contains("is-open")) {
    closing = false; // already hidden — nothing to animate
    return;
  }
  if (closing) return; // exit already in flight

  if (prefersReducedMotion()) {
    hidePaletteNow(root);
    return;
  }

  closing = true;
  root.classList.add("is-closing");
  onceAnimationEnd(root, () => {
    // Re-opened mid-close — the open path already restored the layer; abort.
    if (getState().paletteOpen) {
      closing = false;
      return;
    }
    hidePaletteNow(root);
    closing = false;
  }, OVERLAY_FALLBACK_MS);
}

/** Clear the layer's contents and fully hide it (end-of-exit state). */
function hidePaletteNow(root: HTMLElement): void {
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
