// Per-pane xterm.js instance manager. This generalizes the working single-pane
// spike wiring (one Terminal, listen "pty-output", invoke "send_keys") to N
// panes: one Terminal per pane id, output routed by pane, keystrokes tagged
// with the originating pane, and a fit-addon-driven resize that informs the
// daemon via resize_pane so the PTY matches the on-screen geometry.

import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { SearchAddon } from "@xterm/addon-search";
import "@xterm/xterm/css/xterm.css";
import { resizePane, sendKeys } from "./api";
import { getState } from "./state";

/**
 * Font stack for the terminal. Resolved from the system at build time:
 * `fc-list` confirmed a full "JetBrainsMono Nerd Font" is installed
 * system-wide (the user runs starship/powerline), so we lead with the
 * Nerd Font for powerline + dev-icon glyph coverage, fall back through
 * plain JetBrains Mono, then an emoji font, then the platform monospace.
 * The DOM renderer (the one we keep — no WebGL, NVIDIA/WebKit risk)
 * honours this fallback chain glyph-by-glyph.
 */
const TERM_FONT_STACK =
  '"JetBrainsMono Nerd Font", "Symbols Nerd Font Mono", "JetBrains Mono", "Noto Color Emoji", "Noto Sans Symbols 2", ui-monospace, monospace';

interface PaneTerm {
  term: Terminal;
  fit: FitAddon;
  search: SearchAddon;
  el: HTMLElement;
  session: string;
  /** debounce timer id for resize_pane RPC */
  resizeTimer?: number;
  /** the find bar DOM (created on demand), parented into the pane body */
  findBar?: HTMLElement;
}

/** Highlight styling for in-pane find matches (current vs all). */
const SEARCH_DECORATIONS = {
  matchOverviewRuler: "#e8743b",
  activeMatchColorOverviewRuler: "#ff9152",
} as const;

const terms = new Map<string, PaneTerm>();

/** Read the chrome's terminal palette from CSS vars (so themes reskin xterm). */
function readTermTheme(): Record<string, string> {
  const cs = getComputedStyle(document.documentElement);
  const v = (name: string, fallback: string) =>
    cs.getPropertyValue(name).trim() || fallback;
  return {
    background: v("--term-bg", "#0a0b0e"),
    foreground: v("--fg", "#ECEDEE"),
    cursor: v("--ember", "#e8743b"),
    cursorAccent: v("--surface-0", "#0b0c0e"),
    selectionBackground: v("--hairline", "#2c2f39"),
  };
}

/** Write text to the system clipboard, tolerating webview quirks. */
async function writeClipboard(text: string): Promise<void> {
  if (!text) return;
  try {
    await navigator.clipboard.writeText(text);
  } catch (err) {
    console.warn("[pyre-clip] clipboard write failed:", err);
  }
}

/** Read text from the system clipboard (empty string on failure). */
async function readClipboard(): Promise<string> {
  try {
    return await navigator.clipboard.readText();
  } catch (err) {
    console.warn("[pyre-clip] clipboard read failed:", err);
    return "";
  }
}

/**
 * Custom key handler for copy/paste. Returns `false` to tell xterm we handled
 * the event (so it is NOT also sent to the PTY), `true` to let xterm process it.
 *
 *  - Ctrl+Shift+C → copy current selection to clipboard.
 *  - Ctrl+Shift+V → read clipboard, route bytes to the pane via send_keys.
 *  - plain Ctrl+C (no Shift) → falls through untouched, preserving SIGINT.
 */
function keyHandler(e: KeyboardEvent, term: Terminal, pane: string): boolean {
  if (e.type !== "keydown") return true;
  if (!e.ctrlKey || !e.shiftKey || e.altKey || e.metaKey) return true;
  const key = e.key.toLowerCase();

  if (key === "c") {
    const sel = term.getSelection();
    if (sel) {
      void writeClipboard(sel);
      return false; // consumed — don't forward to PTY
    }
    // No selection: let Ctrl+Shift+C fall through (harmless) rather than eat it.
    return true;
  }

  if (key === "v") {
    void readClipboard().then((text) => {
      if (!text) return;
      const bytes = Array.from(new TextEncoder().encode(text));
      void sendKeys(pane, bytes).catch((err) =>
        console.error("[pyre-clip] paste send_keys failed:", pane, err),
      );
    });
    return false; // consumed
  }

  return true;
}

/**
 * Get (or lazily create) the Terminal for a pane and mount it into `el`.
 * If the instance already exists, it is re-parented into the new element
 * (the layout tree re-renders DOM nodes, but terminals must survive).
 */
export function mountPaneTerminal(
  pane: string,
  session: string,
  el: HTMLElement,
): PaneTerm {
  if (!el.isConnected) {
    // The card body was rendered but isn't attached to the document yet. xterm
    // would mount into a detached node and measure 0×0 — warn so the ordering
    // bug is visible rather than producing a silently-blank terminal.
    console.warn(
      `mountPaneTerminal(${pane}): target element is not connected to the DOM; terminal may render blank`,
    );
  }
  let entry = terms.get(pane);
  if (entry) {
    // Re-attach the existing xterm DOM into the freshly rendered card body.
    if (entry.el !== el) {
      const node = entry.term.element;
      if (node && node.parentElement !== el) {
        console.log("[pyre-render] re-parenting terminal for pane", pane,
          "— will restore focus if focused");
        el.appendChild(node);
        // Re-parenting fires blur on the hidden textarea. If this pane is the
        // currently focused pane, restore focus immediately in the same frame
        // so onData keeps firing and keystrokes reach the daemon.
        if (getState().focusedPane === pane) {
          entry.term.focus();
          const ta = el.querySelector<HTMLElement>(".xterm-helper-textarea");
          if (ta) ta.focus();
          console.log("[pyre-input] focus restored after re-parent for pane", pane);
        }
      }
      entry.el = el;
    }
    queueFit(entry);
    return entry;
  }

  const term = new Terminal({
    // allowProposedApi is REQUIRED for the Unicode11 addon's activeVersion
    // switch below — without it xterm throws when you set unicode.activeVersion.
    allowProposedApi: true,
    fontFamily: TERM_FONT_STACK,
    fontSize: 12.5,
    lineHeight: 1.2,
    cursorBlink: true,
    scrollback: 10000,
    // Right-click selects the word under the cursor (terminal-app convention).
    rightClickSelectsWord: true,
    // Slightly smoother wheel scroll without flooding the PTY.
    scrollSensitivity: 3,
    // Keep mouse reporting ON so apps (vim, tmux, etc.) receive mouse events;
    // xterm only swallows the wheel for its own scrollback when the app has NOT
    // requested mouse tracking, which is exactly the behaviour we want.
    theme: readTermTheme(),
  });
  const fit = new FitAddon();
  term.loadAddon(fit);

  // In-pane find (Ctrl+F). The decorations API needs an open terminal, which is
  // why the addon is loaded after construction but before term.open below works
  // either way — xterm tolerates load-then-open.
  const search = new SearchAddon();
  term.loadAddon(search);

  // Unicode 11 width tables: fixes wide-char / emoji / CJK column width so the
  // cursor stays aligned with what the shell thinks it printed (the desync that
  // made powerline separators and emoji shove the cursor off by a cell).
  const unicode11 = new Unicode11Addon();
  term.loadAddon(unicode11);
  term.unicode.activeVersion = "11";

  // Clickable URLs. Open via the default handler (Tauri intercepts http(s) and
  // routes to the system browser); falls back to window.open in plain vite dev.
  term.loadAddon(new WebLinksAddon());

  term.open(el);

  // Copy/paste keymap. xterm's default keymap would let Ctrl+Shift+C/V fall
  // through to the PTY; intercept them here and keep plain Ctrl+C (SIGINT)
  // working by only acting on the Shift-modified chords.
  term.attachCustomKeyEventHandler((e) => keyHandler(e, term, pane));

  entry = { term, fit, search, el, session };
  terms.set(pane, entry);

  // Webview → daemon: forward keystrokes as UTF-8 bytes, tagged with this pane.
  term.onData((d) => {
    const bytes = Array.from(new TextEncoder().encode(d));
    console.log("[pyre-input] onData", pane, d.length); // (c) onData stage
    console.log("[pyre-input] send_keys ->", pane, bytes.length); // (d) sendKeys invocation
    void sendKeys(pane, bytes)
      .then(() => {
        console.log("[pyre-input] send_keys ok", pane); // (e-ok)
      })
      .catch((err) => {
        console.error("[pyre-input] send_keys FAILED", pane, err); // (e-err)
      });
  });

  // Copy-on-select: mirror the common terminal convention where a mouse
  // selection lands on the clipboard immediately (so Ctrl+Shift+V / middle
  // paste elsewhere just works). Cheap and non-destructive — only fires when a
  // non-empty selection exists.
  term.onSelectionChange(() => {
    const sel = term.getSelection();
    if (sel) void writeClipboard(sel);
  });

  // When the terminal is resized (cols/rows change), tell the daemon so the
  // PTY winsize matches — debounced to avoid a flood during drag-resize.
  term.onResize(({ cols, rows }) => {
    if (entry!.resizeTimer) window.clearTimeout(entry!.resizeTimer);
    entry!.resizeTimer = window.setTimeout(() => {
      void resizePane(pane, cols, rows).catch(() => {
        /* pane may be gone; ignore */
      });
    }, 120);
  });

  queueFit(entry);
  return entry;
}

/** Write daemon output bytes into a pane's terminal, if it exists. */
export function writePaneOutput(pane: string, bytes: number[]): void {
  const entry = terms.get(pane);
  if (entry) {
    entry.term.write(new Uint8Array(bytes));
  } else {
    // Output arrived for a pane with no live terminal — the bytes are dropped.
    // Warn so a routing/mount-ordering bug is visible rather than silent.
    console.warn(
      `pty-output for pane ${pane} dropped: no terminal mounted (${bytes.length} bytes)`,
    );
  }
}

/** Write a styled message into a pane (used for error/closed banners). */
export function writePaneMessage(pane: string, ansi: string): void {
  const entry = terms.get(pane);
  if (entry) entry.term.write(ansi);
}

/** Dispose a pane's terminal and free its resources. */
export function disposePaneTerminal(pane: string): void {
  const entry = terms.get(pane);
  if (!entry) return;
  if (entry.resizeTimer) window.clearTimeout(entry.resizeTimer);
  entry.findBar?.remove();
  entry.search.dispose();
  entry.term.dispose();
  terms.delete(pane);
}

// ── In-pane find ──────────────────────────────────────────────────────────────

/**
 * Open (or re-focus) the find bar over a pane's terminal. The bar is a small
 * DOM overlay parented into the pane card's header area, with an input plus
 * prev/next/close controls. Search runs through xterm's SearchAddon so matches
 * are highlighted natively in the buffer.
 */
export function openFindBar(pane: string): void {
  const entry = terms.get(pane);
  if (!entry) {
    console.warn("[pyre-find] no terminal for pane", pane);
    return;
  }
  // Mount inside the pane card so it overlays the right terminal, even in splits.
  const card = entry.el.closest<HTMLElement>(".pane-card") ?? entry.el;
  if (entry.findBar && entry.findBar.isConnected) {
    const input = entry.findBar.querySelector<HTMLInputElement>(".find-input");
    input?.focus();
    input?.select();
    return;
  }

  const input = document.createElement("input");
  input.className = "find-input";
  input.type = "text";
  input.placeholder = "Find in pane…";
  input.spellcheck = false;

  const count = document.createElement("span");
  count.className = "find-count";

  const runFind = (forward: boolean): void => {
    const q = input.value;
    if (!q) {
      entry.search.clearDecorations();
      count.textContent = "";
      return;
    }
    const opts = {
      decorations: SEARCH_DECORATIONS,
      caseSensitive: false,
    };
    if (forward) entry.search.findNext(q, opts);
    else entry.search.findPrevious(q, opts);
  };

  // Live result count via the addon's results callback (only fires when
  // decorations are enabled, which all our searches do).
  entry.search.onDidChangeResults((res) => {
    if (res.resultCount === 0) {
      count.textContent = input.value ? "0/0" : "";
    } else if (res.resultIndex < 0) {
      // -1 = match threshold exceeded; show the total only.
      count.textContent = `${res.resultCount}+`;
    } else {
      // resultIndex is 0-based; show 1-based for humans.
      count.textContent = `${res.resultIndex + 1}/${res.resultCount}`;
    }
  });

  input.addEventListener("input", () => runFind(true));
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      runFind(!e.shiftKey);
    } else if (e.key === "Escape") {
      e.preventDefault();
      closeFindBar(pane);
    }
    // Stop the global capture-phase handler from claiming these keys.
    e.stopPropagation();
  });

  const btn = (title: string, glyph: string, onClick: () => void): HTMLButtonElement => {
    const b = document.createElement("button");
    b.className = "find-btn";
    b.title = title;
    b.setAttribute("aria-label", title);
    b.innerHTML = glyph;
    b.addEventListener("click", onClick);
    return b;
  };

  const bar = document.createElement("div");
  bar.className = "find-bar";
  bar.append(
    input,
    count,
    btn("Previous (Shift+Enter)", CHEVRON_UP, () => runFind(false)),
    btn("Next (Enter)", CHEVRON_DOWN, () => runFind(true)),
    btn("Close (Esc)", CROSS, () => closeFindBar(pane)),
  );
  // Stop clicks inside the bar from bubbling to the pane (which would refocus
  // the terminal and steal focus from the input).
  bar.addEventListener("mousedown", (e) => e.stopPropagation());

  card.appendChild(bar);
  entry.findBar = bar;
  requestAnimationFrame(() => {
    input.focus();
    input.select();
  });
}

/** Close a pane's find bar and clear its match decorations. */
export function closeFindBar(pane: string): void {
  const entry = terms.get(pane);
  if (!entry) return;
  entry.search.clearDecorations();
  entry.findBar?.remove();
  entry.findBar = undefined;
  // Return focus to the terminal so typing resumes immediately.
  focusPaneTerminal(pane);
}

// Minimal inline glyphs for the find bar (avoids importing the render layer into
// the terminal manager — keeps the dependency arrow one-directional).
const CHEVRON_UP =
  `<svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M4 10 L8 6 L12 10"/></svg>`;
const CHEVRON_DOWN =
  `<svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M4 6 L8 10 L12 6"/></svg>`;
const CROSS =
  `<svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M4 4l8 8M12 4l-8 8"/></svg>`;

/** Focus a pane's terminal (so keystrokes route there). */
export function focusPaneTerminal(pane: string): void {
  const entry = terms.get(pane);
  if (!entry) {
    console.warn("[pyre-input] focusPaneTerminal: no terminal for pane", pane);
    return;
  }
  console.log("[pyre-input] focus", pane); // (b) terminal focus stage
  entry.term.focus();
  // WebKitGTK (the Tauri webview) sometimes needs an explicit focus on the
  // underlying hidden textarea rather than the xterm wrapper element.
  const ta = entry.el.querySelector<HTMLElement>(".xterm-helper-textarea");
  if (ta) ta.focus();
}

/** Refit every mounted terminal (call on window resize / layout change). */
export function refitAll(): void {
  for (const entry of terms.values()) queueFit(entry);
}

/** Re-read CSS vars and push the new palette into every live terminal. */
export function restyleAll(): void {
  const theme = readTermTheme();
  for (const entry of terms.values()) {
    entry.term.options.theme = theme;
  }
}

/** Which panes currently have a live terminal instance. */
export function mountedPanes(): Set<string> {
  return new Set(terms.keys());
}

/**
 * Fit the terminal once its element has non-zero size. A 0×0 fit renders
 * nothing, so we retry across a few animation frames while the layout settles
 * (the pane card is mounted via microtask + flex, so the first frame can still
 * measure 0×0). After fitting, xterm's `onResize` fires and pushes the new
 * cols/rows to the daemon via `resize_pane`.
 */
function queueFit(entry: PaneTerm, attempt = 0): void {
  const MAX_ATTEMPTS = 10;
  requestAnimationFrame(() => {
    // Terminal may have been disposed mid-flight.
    if (entry.term.element == null) return;
    const hasSize = entry.el.clientHeight > 0 && entry.el.clientWidth > 0;
    if (hasSize) {
      try {
        entry.fit.fit();
      } catch (err) {
        console.warn("FitAddon.fit() failed:", err);
      }
      return;
    }
    if (attempt < MAX_ATTEMPTS) {
      queueFit(entry, attempt + 1);
    } else {
      // Gave up: the pane stayed 0×0. Without a successful fit xterm has 0 rows
      // and shows nothing — surface it instead of silently rendering blank.
      console.warn(
        `terminal fit skipped: pane element stayed 0×0 after ${MAX_ATTEMPTS} frames (terminal will not render)`,
      );
    }
  });
}
