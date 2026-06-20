// Per-pane xterm.js instance manager. This generalizes the working single-pane
// spike wiring (one Terminal, listen "pty-output", invoke "send_keys") to N
// panes: one Terminal per pane id, output routed by pane, keystrokes tagged
// with the originating pane, and a fit-addon-driven resize that informs the
// daemon via resize_pane so the PTY matches the on-screen geometry.

import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { resizePane, sendKeys } from "./api";
import { getState } from "./state";

interface PaneTerm {
  term: Terminal;
  fit: FitAddon;
  el: HTMLElement;
  session: string;
  /** debounce timer id for resize_pane RPC */
  resizeTimer?: number;
}

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
    fontFamily: '"JetBrains Mono", ui-monospace, monospace',
    fontSize: 12.5,
    lineHeight: 1.2,
    cursorBlink: true,
    scrollback: 5000,
    theme: readTermTheme(),
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.open(el);

  entry = { term, fit, el, session };
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
  entry.term.dispose();
  terms.delete(pane);
}

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
