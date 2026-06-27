// @vitest-environment jsdom
//
// Keystone regression: pane focus is applied IN-PLACE, never via a structural
// rebuild. `applyFocusInPlace` must move the `.focused` glow between the EXISTING
// pane-card nodes — preserving node identity so the CSS transition can play and
// xterms are not re-parented — and must NOT replace or recreate any card.
//
// center.ts pulls in xterm (../terminals) and the Tauri invoke layer (../api,
// ../actions), so we mock those to let the module load in jsdom. The function
// under test only reads getState().focusedPane (the real store) and the live DOM.

import { describe, it, expect, beforeEach, vi } from "vitest";

vi.mock("../terminals", () => ({
  mountPaneTerminal: vi.fn(),
  refitAll: vi.fn(),
}));
vi.mock("../api", () => ({ setWeight: vi.fn() }));
vi.mock("../actions", () => ({
  closePaneAction: vi.fn(),
  focusPane: vi.fn(),
  newSession: vi.fn(),
  renamePaneAction: vi.fn(),
  splitDown: vi.fn(),
  splitRight: vi.fn(),
  zoomPane: vi.fn(),
  closeAgents: vi.fn(),
  gotoPane: vi.fn(),
}));

import { applyFocusInPlace } from "../render/center";
import { setState } from "../state";

function card(pane: string): HTMLElement {
  const el = document.createElement("div");
  el.className = "pane-card";
  el.dataset["pane"] = pane;
  return el;
}

describe("applyFocusInPlace", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
    setState({ focusedPane: null });
  });

  it("puts .focused only on the focused pane's card", () => {
    const a = card("p1");
    const b = card("p2");
    document.body.append(a, b);

    setState({ focusedPane: "p1" });
    applyFocusInPlace();

    expect(a.classList.contains("focused")).toBe(true);
    expect(b.classList.contains("focused")).toBe(false);
  });

  it("moves .focused in-place without recreating cards (node identity preserved)", () => {
    const a = card("p1");
    const b = card("p2");
    document.body.append(a, b);

    setState({ focusedPane: "p1" });
    applyFocusInPlace();
    expect(a.classList.contains("focused")).toBe(true);

    setState({ focusedPane: "p2" });
    applyFocusInPlace();

    // The SAME element objects are still mounted — focus moved by a class toggle,
    // not a rebuild. This is the keystone guarantee that keeps xterms alive and
    // lets the .focused transition animate.
    expect(document.querySelector('.pane-card[data-pane="p1"]')).toBe(a);
    expect(document.querySelector('.pane-card[data-pane="p2"]')).toBe(b);
    expect(a.classList.contains("focused")).toBe(false);
    expect(b.classList.contains("focused")).toBe(true);
  });

  it("is idempotent — re-running with the same focus is a no-op", () => {
    const a = card("p1");
    document.body.append(a);

    setState({ focusedPane: "p1" });
    applyFocusInPlace();
    applyFocusInPlace();

    expect(a.classList.contains("focused")).toBe(true);
  });
});
