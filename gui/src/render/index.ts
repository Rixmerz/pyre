// App shell: builds the static grid skeleton once, then re-renders each region
// on every state change. Also owns global keyboard shortcuts (⌘K, Esc).

import { getState, subscribe } from "../state";
import { renderTopbar } from "./topbar";
import { renderRail, applyActiveSessionInPlace } from "./rail";
import { renderCenter, applyFocusInPlace } from "./center";
import { renderTabs, applyActiveWindowInPlace } from "./tabs";
import { renderBlocks, applyBlockElapsedInPlace } from "./blocks";
import { renderStatusbar } from "./statusbar";
import { renderPalette, resetPalette, handlePaletteKey } from "./palette";
import { renderThemePicker } from "./themepicker";
import { renderAgents } from "./agents";
import { isInlineEditing } from "./inline-edit";
import { openPalette, closePalette, unzoom, closeAgents } from "../actions";

interface Regions {
  topbar: HTMLElement;
  rail: HTMLElement;
  center: HTMLElement;
  tabs: HTMLElement;
  paneArea: HTMLElement;
  right: HTMLElement;
  statusbar: HTMLElement;
  palette: HTMLElement;
  themepicker: HTMLElement;
  agents: HTMLElement;
  shell: HTMLElement;
}

let regions: Regions | null = null;
let lastPaletteOpen = false;

/** Build the static DOM skeleton into #app and wire global keys. */
export function mountShell(app: HTMLElement): void {
  app.innerHTML = "";

  const topbar = el("header", "topbar");
  const rail = el("aside", "rail");
  // The center stacks a per-session TAB STRIP over the pane area. The strip is a
  // fixed-height chrome row; the pane area is the flex body renderCenter owns
  // (its xterm-survival fingerprint logic mutates ONLY the pane area, never the
  // strip, so switching tabs never tears down terminals in hidden tabs).
  const center = el("section", "center");
  const tabs = el("nav", "tabstrip");
  tabs.setAttribute("aria-label", "Session tabs");
  const paneArea = el("div", "pane-area");
  center.append(tabs, paneArea);

  const right = el("aside", "right-panel");
  const statusbar = el("footer", "statusbar");
  const palette = el("div", "palette-layer");
  const themepicker = el("div", "themepicker-layer");
  const agents = el("div", "agents-layer");

  const shell = el("div", "shell");
  shell.append(rail, center, right);

  app.append(topbar, shell, statusbar, palette, themepicker, agents);

  regions = { topbar, rail, center, tabs, paneArea, right, statusbar, palette, themepicker, agents, shell };

  wireGlobalKeys();
  subscribe(renderAll);
  renderAll();
}

/** Re-render every region from current state. */
export function renderAll(): void {
  if (!regions) return;
  renderTopbar(regions.topbar);
  // While an inline rename editor is open, SKIP rebuilding the three surfaces
  // that host one (rail, tabs, center). renderRail/renderTabs replaceChildren
  // unconditionally, so a poll-driven setState (heat change every ≤750ms) would
  // otherwise tear out the live <input> and discard the user's keystrokes before
  // Enter/blur commits. The editor itself commits on blur/Enter and the caller's
  // reload then triggers a fresh renderAll with editing already settled. The
  // non-editor chrome (blocks, statusbar, palette) still refreshes normally.
  if (!isInlineEditing()) {
    renderRail(regions.rail);
    renderTabs(regions.tabs);
    renderCenter(regions.paneArea);
  }
  // Apply the in-place selection/focus passes on EVERY render, even when the
  // region renderers early-return on an unchanged fingerprint (active session,
  // active window and focused pane are all excluded from their fingerprints so a
  // selection change no longer rebuilds the region). These move the `.active` /
  // `.focused` highlights between the EXISTING nodes — preserving node identity
  // so hover survives, clicks aren't lost mid-rebuild, and CSS transitions play.
  // All three are idempotent.
  applyActiveSessionInPlace();
  applyActiveWindowInPlace();
  applyFocusInPlace();
  renderBlocks(regions.right);
  // Advance running blocks' elapsed-time text in place (excluded from the block
  // list fingerprint so a tick doesn't rebuild the cards). Idempotent.
  applyBlockElapsedInPlace();
  renderStatusbar(regions.statusbar);

  const s = getState();
  // Reset palette transient state on open edge.
  if (s.paletteOpen && !lastPaletteOpen) resetPalette();
  lastPaletteOpen = s.paletteOpen;
  renderPalette(regions.palette);
  renderThemePicker(regions.themepicker);
  renderAgents(regions.agents);

  // Reflect chrome collapse on the shell grid.
  regions.shell.classList.toggle("rail-collapsed", s.railCollapsed);
  regions.shell.classList.toggle("right-collapsed", s.rightCollapsed);
}

function wireGlobalKeys(): void {
  window.addEventListener("keydown", (e) => {
    const s = getState();
    // ⌘K / Ctrl+K toggles the palette.
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      if (s.paletteOpen) closePalette();
      else openPalette();
      return;
    }
    if (s.paletteOpen && regions) {
      handlePaletteKey(e, regions.palette);
      return;
    }
    if (e.key === "Escape") {
      if (s.agentsOpen) {
        e.preventDefault();
        closeAgents();
      } else if (s.themePickerOpen) {
        e.preventDefault();
        import("../actions").then((m) => m.closeThemePicker());
      } else if (s.zoomedPane) {
        e.preventDefault();
        unzoom();
      }
    }
  });
}

function el(tag: string, cls: string): HTMLElement {
  const e = document.createElement(tag);
  e.className = cls;
  return e;
}
