// Global keyboard control — multiplexer parity (herdr-style). All binds dispatch
// through the existing action catalog (actions.ts); this module owns ONLY key →
// action mapping, spatial pane navigation, and the cheatsheet overlay.
//
// COLLISION DISCIPLINE
// ────────────────────
// A pane's xterm owns a hidden <textarea> that must receive normal typing
// untouched. So every global bind here requires a modifier the shell won't emit
// for ordinary input:
//   • Pane management binds use Ctrl+Shift+<key>.
//   • Directional focus uses Ctrl+<hjkl> / Ctrl+<arrows> (herdr parity) — these
//     are intercepted at the window (capture phase) and preventDefault'd so they
//     drive focus instead of reaching the PTY. They are pure navigation, never
//     text, so claiming them is the multiplexer's job.
//   • Session cycling uses Ctrl+Tab / Ctrl+Shift+Tab.
//   • The cheatsheet toggles on ? (Shift+/) or F1 and is the only bare-ish key —
//     guarded so it only fires when NOT typing into an input/textarea.
// ⌘K (palette) and Esc are owned by render/index.ts; we do not re-bind them.

import { getState, setState } from "./state";
import {
  closePaneAction,
  newPaneAction,
  newSession,
  splitDown,
  splitRight,
  switchSession,
  toggleAgents,
  toggleRightPanel,
  zoomPane,
} from "./actions";
import { leafPanes } from "./session-ops";
import { focusPaneTerminal, openFindBar } from "./terminals";

type Dir = "left" | "down" | "up" | "right";

/** One row in the cheatsheet, grouped by section. */
interface KeyRow {
  keys: string;
  desc: string;
}
interface KeyGroup {
  title: string;
  rows: KeyRow[];
}

/** The single source of truth for the cheatsheet (and self-documentation). */
export const KEYBIND_GROUPS: KeyGroup[] = [
  {
    title: "Focus",
    rows: [
      { keys: "Ctrl + H / ←", desc: "Focus pane left" },
      { keys: "Ctrl + J / ↓", desc: "Focus pane down" },
      { keys: "Ctrl + K / ↑", desc: "Focus pane up" },
      { keys: "Ctrl + L / →", desc: "Focus pane right" },
    ],
  },
  {
    title: "Panes",
    rows: [
      { keys: "Ctrl + Shift + T", desc: "New pane in this session" },
      { keys: "Ctrl + Shift + E", desc: "Split right" },
      { keys: "Ctrl + Shift + O", desc: "Split down" },
      { keys: "Ctrl + Shift + Z", desc: "Zoom pane (toggle)" },
      { keys: "Ctrl + Shift + W", desc: "Close pane" },
    ],
  },
  {
    title: "Sessions",
    rows: [
      { keys: "Ctrl + Tab", desc: "Next session" },
      { keys: "Ctrl + Shift + Tab", desc: "Previous session" },
      { keys: "Ctrl + Shift + N", desc: "New session" },
      { keys: "Ctrl + Shift + A", desc: "Agent overview" },
    ],
  },
  {
    title: "Search & palette",
    rows: [
      { keys: "Ctrl / ⌘ + K", desc: "Command palette" },
      { keys: "Ctrl + F", desc: "Find in pane" },
      { keys: "Ctrl + Shift + F", desc: "Search blocks" },
      { keys: "? · F1", desc: "Toggle this cheatsheet" },
      { keys: "Esc", desc: "Close overlay / unzoom" },
    ],
  },
];

let cheatsheetOpen = false;
let cheatEl: HTMLElement | null = null;

/** Install the global keydown handler. Idempotent-safe (call once at boot). */
export function installKeybinds(): void {
  // Capture phase so directional binds win over xterm's textarea handler.
  window.addEventListener("keydown", onKeyDown, { capture: true });
}

function onKeyDown(e: KeyboardEvent): void {
  // The cheatsheet swallows its own keys (Esc to close) before anything else.
  if (cheatsheetOpen) {
    if (e.key === "Escape") {
      e.preventDefault();
      closeCheatsheet();
    }
    return;
  }

  // Cheatsheet toggle: "?" or F1, but only when not typing into a form field
  // (so "?" still types literally into the palette / block search inputs).
  if ((e.key === "?" || e.key === "F1") && !typingInField(e)) {
    e.preventDefault();
    openCheatsheet();
    return;
  }

  // The palette owns its own keyspace while open — don't fight it.
  if (getState().paletteOpen) return;

  // The agent overview owns Esc (handled in render/index.ts); while it's open we
  // still allow Ctrl+Shift+A to toggle it closed below, but block other binds.
  const agentsOpen = getState().agentsOpen;

  const ctrl = e.ctrlKey && !e.metaKey && !e.altKey;
  if (!ctrl) return;

  // ── In-pane find: Ctrl+F (no Shift) ─────────────────────────────────────
  // Opens a find bar over the FOCUSED pane's terminal. Distinct from
  // Ctrl+Shift+F (block search). Guard against the agents overlay.
  if (e.key.toLowerCase() === "f" && !e.shiftKey && !agentsOpen) {
    const pane = getState().focusedPane;
    if (pane) {
      e.preventDefault();
      openFindBar(pane);
      return;
    }
  }

  if (agentsOpen) {
    // Only the toggle (Ctrl+Shift+A) is honoured while the overview is open.
    if (e.shiftKey && e.key.toLowerCase() === "a") {
      e.preventDefault();
      toggleAgents();
    }
    return;
  }

  // ── Session cycling: Ctrl+Tab / Ctrl+Shift+Tab ──────────────────────────
  if (e.key === "Tab") {
    e.preventDefault();
    cycleSession(e.shiftKey ? -1 : +1);
    return;
  }

  // ── Pane management: Ctrl+Shift+<key> ───────────────────────────────────
  if (e.shiftKey) {
    switch (e.key.toLowerCase()) {
      case "t":
        // New STANDALONE pane in the active session (its own tab) + switch to it.
        e.preventDefault();
        void newPaneAction();
        return;
      case "e":
        e.preventDefault();
        void splitRight(null);
        return;
      case "o":
        e.preventDefault();
        void splitDown(null);
        return;
      case "z":
        e.preventDefault();
        zoomPane(null);
        return;
      case "w":
        e.preventDefault();
        void closePaneAction(null);
        return;
      case "n":
        e.preventDefault();
        void newSession();
        return;
      case "f":
        e.preventDefault();
        openBlockSearch();
        return;
      case "a":
        e.preventDefault();
        toggleAgents();
        return;
      default:
        return; // unbound Ctrl+Shift combo — let it pass
    }
  }

  // ── Directional focus: Ctrl+H/J/K/L and Ctrl+Arrow ──────────────────────
  const dir = dirForKey(e.key);
  if (dir) {
    e.preventDefault();
    focusInDirection(dir);
  }
}

/** Map a key to a navigation direction, or null. */
function dirForKey(key: string): Dir | null {
  switch (key) {
    case "h":
    case "H":
    case "ArrowLeft":
      return "left";
    case "j":
    case "J":
    case "ArrowDown":
      return "down";
    case "k":
    case "K":
    case "ArrowUp":
      return "up";
    case "l":
    case "L":
    case "ArrowRight":
      return "right";
    default:
      return null;
  }
}

/** True if the event target is a text input/textarea (excluding xterm's). */
function typingInField(e: KeyboardEvent): boolean {
  const t = e.target as HTMLElement | null;
  if (!t) return false;
  const tag = t.tagName;
  // xterm's helper textarea carries this class; the cheatsheet check above
  // already gates terminal input, but block-search / palette inputs are real
  // form fields where "?" must type literally.
  if (t.classList.contains("xterm-helper-textarea")) return false;
  return tag === "INPUT" || tag === "TEXTAREA" || t.isContentEditable;
}

// ── Session cycling ───────────────────────────────────────────────────────

/** Move to the next (+1) or previous (-1) session in the rail order. */
function cycleSession(delta: number): void {
  const { sessions, activeSession } = getState();
  if (sessions.length < 2) return;
  const idx = sessions.findIndex((s) => s.id === activeSession);
  const base = idx < 0 ? 0 : idx;
  const next = (base + delta + sessions.length) % sessions.length;
  const target = sessions[next];
  if (target && target.id !== activeSession) void switchSession(target.id);
}

// ── Block search ──────────────────────────────────────────────────────────

/** Reveal the blocks panel (if collapsed) and focus its search input. */
function openBlockSearch(): void {
  if (getState().rightCollapsed) toggleRightPanel();
  // Focus after the panel re-renders (toggleRightPanel → setState → render).
  requestAnimationFrame(() => {
    const input = document.querySelector<HTMLInputElement>(
      ".block-search-input",
    );
    input?.focus();
    input?.select();
  });
}

// ── Directional pane focus (spatial, geometry-based) ──────────────────────

/**
 * Move focus to the nearest pane in a direction, using the on-screen geometry
 * of the rendered pane cards. Geometry beats tree-walking here: it matches what
 * the user SEES regardless of how the layout tree nests, and it is robust to
 * weighted/asymmetric splits.
 */
function focusInDirection(dir: Dir): void {
  const st = getState();
  const session = st.activeSession;
  if (!session) return;

  // Zoomed view has exactly one visible pane — nowhere to navigate.
  if (st.zoomedPane) return;

  const panes = leafPanes(st.layouts.get(session));
  if (panes.length < 2) return;

  const cur = st.focusedPane;
  const rects = new Map<string, DOMRect>();
  for (const pane of panes) {
    const card = document.querySelector<HTMLElement>(
      `.pane-card[data-pane="${CSS.escape(pane)}"]`,
    );
    if (card) rects.set(pane, card.getBoundingClientRect());
  }

  // No focused pane yet → focus the first one.
  if (!cur || !rects.has(cur)) {
    const first = panes.find((p) => rects.has(p));
    if (first) doFocus(first);
    return;
  }

  const from = rects.get(cur)!;
  const fromCx = from.left + from.width / 2;
  const fromCy = from.top + from.height / 2;

  let best: string | null = null;
  let bestScore = Infinity;

  for (const [pane, r] of rects) {
    if (pane === cur) continue;
    const cx = r.left + r.width / 2;
    const cy = r.top + r.height / 2;
    const dx = cx - fromCx;
    const dy = cy - fromCy;

    // Must lie in the requested half-plane (with a small tolerance so panes
    // that only partially overlap still qualify).
    const TOL = 4;
    const inDir =
      (dir === "left" && dx < -TOL) ||
      (dir === "right" && dx > TOL) ||
      (dir === "up" && dy < -TOL) ||
      (dir === "down" && dy > TOL);
    if (!inDir) continue;

    // Score: primary axis distance dominates; cross-axis offset is a tiebreak,
    // so we prefer the pane most directly in line with the current one.
    const primary = dir === "left" || dir === "right" ? Math.abs(dx) : Math.abs(dy);
    const cross = dir === "left" || dir === "right" ? Math.abs(dy) : Math.abs(dx);
    const score = primary + cross * 2;
    if (score < bestScore) {
      bestScore = score;
      best = pane;
    }
  }

  if (best) doFocus(best);
}

/** Focus a pane: update state AND move terminal focus so keystrokes route. */
function doFocus(pane: string): void {
  setState({ focusedPane: pane });
  focusPaneTerminal(pane);
}

// ── Cheatsheet overlay ────────────────────────────────────────────────────

/** Fallback when `animationend` never fires (matches --dur-fast). */
const CHEAT_FALLBACK_MS = 120;

function openCheatsheet(): void {
  if (cheatsheetOpen) return;
  cheatsheetOpen = true;
  cheatEl = buildCheatsheet();
  document.body.appendChild(cheatEl);
  // CSS plays the enter animation off `.is-open` (keyframe runs from first paint
  // on the freshly-inserted node).
  cheatEl.classList.add("is-open");
}

/** Close the cheatsheet: play the exit animation (add `.is-closing`, keep
 *  `.is-open`), then remove the node on animationend / fallback. Reduced motion
 *  removes immediately. */
function closeCheatsheet(): void {
  cheatsheetOpen = false;
  const el = cheatEl;
  cheatEl = null;
  if (!el) return;

  if (prefersReducedMotion()) {
    el.remove();
    return;
  }

  el.classList.add("is-closing");
  onceAnimationEnd(el, () => el.remove(), CHEAT_FALLBACK_MS);
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

/** Build the themed cheatsheet overlay DOM. */
function buildCheatsheet(): HTMLElement {
  const backdrop = document.createElement("div");
  backdrop.className = "cheatsheet-backdrop";
  backdrop.setAttribute("role", "dialog");
  backdrop.setAttribute("aria-modal", "true");
  backdrop.setAttribute("aria-label", "Keyboard shortcuts");
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) closeCheatsheet();
  });

  const modal = document.createElement("div");
  modal.className = "cheatsheet-modal";

  const header = document.createElement("div");
  header.className = "cheatsheet-header";
  header.textContent = "Keyboard shortcuts";
  modal.appendChild(header);

  const grid = document.createElement("div");
  grid.className = "cheatsheet-grid";
  for (const group of KEYBIND_GROUPS) {
    const col = document.createElement("div");
    col.className = "cheatsheet-group";

    const gt = document.createElement("div");
    gt.className = "cheatsheet-group-title";
    gt.textContent = group.title;
    col.appendChild(gt);

    for (const row of group.rows) {
      const r = document.createElement("div");
      r.className = "cheatsheet-row";

      const keys = document.createElement("kbd");
      keys.className = "cheatsheet-keys";
      keys.textContent = row.keys;

      const desc = document.createElement("span");
      desc.className = "cheatsheet-desc";
      desc.textContent = row.desc;

      r.append(keys, desc);
      col.appendChild(r);
    }
    grid.appendChild(col);
  }
  modal.appendChild(grid);

  const footer = document.createElement("div");
  footer.className = "cheatsheet-footer";
  footer.textContent = "Press Esc to close";
  modal.appendChild(footer);

  backdrop.appendChild(modal);
  return backdrop;
}
