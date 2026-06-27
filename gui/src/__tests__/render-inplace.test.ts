// @vitest-environment jsdom
//
// Keystone regression (mirror of center-focus.test.ts, extended to the rail,
// tab strip and block list): selection + the running-block clock are applied
// IN-PLACE, never via a structural rebuild. The render regions skip their
// replaceChildren on a no-op poll tick (fingerprint unchanged), and these
// in-place passes move the `.active` highlight / advance the elapsed text on the
// EXISTING nodes — preserving node identity so hover survives, clicks aren't lost
// mid-rebuild, and the BLOCKS panel stops flickering.
//
// rail.ts / tabs.ts / blocks.ts pull in the Tauri invoke layer (../actions,
// ../api) transitively, so we mock those to let the modules load in jsdom. The
// functions under test only read getState() and the live DOM.

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";

vi.mock("../actions", () => ({
  closeSessionAction: vi.fn(),
  newSession: vi.fn(),
  renameSessionAction: vi.fn(),
  switchSession: vi.fn(),
  toggleRail: vi.fn(),
  newPaneAction: vi.fn(),
  switchWindow: vi.fn(),
  closeWindowAction: vi.fn(),
  renameWindowAction: vi.fn(),
  runBlockSearch: vi.fn(),
  toggleBlockExpanded: vi.fn(),
  toggleFailuresOnly: vi.fn(),
  rerunBlock: vi.fn(),
}));
vi.mock("../api", () => ({ blockStdout: vi.fn() }));

import { applyActiveSessionInPlace } from "../render/rail";
import { applyActiveWindowInPlace } from "../render/tabs";
import { applyBlockElapsedInPlace } from "../render/blocks";
import { fmtDuration, blockDurationMs } from "../render/dom";
import { setState } from "../state";
import type { Block, SessionInfo, WindowInfo } from "../types";

function railRow(session: string): HTMLElement {
  const el = document.createElement("button");
  el.className = "rail-row";
  el.dataset["session"] = session;
  return el;
}

function tabPill(window: string): HTMLElement {
  const el = document.createElement("button");
  el.className = "tab tab-window";
  el.dataset["window"] = window;
  return el;
}

function blockCardNode(id: string, durText: string): {
  card: HTMLElement;
  dur: HTMLElement;
} {
  const card = document.createElement("div");
  card.className = "block-card running";
  card.dataset["block"] = id;
  const dur = document.createElement("span");
  dur.className = "block-dur";
  dur.textContent = durText;
  card.appendChild(dur);
  return { card, dur };
}

function session(id: string): SessionInfo {
  return { id, name: id, pane_count: 1 };
}

function win(id: string, position: number): WindowInfo {
  return { id, session: "s1", name: id, position, pane_count: 1 };
}

function runningBlock(id: string): Block {
  return {
    id,
    pane: "p1",
    session: "s1",
    command: `cmd ${id}`,
    started_at: "2026-06-20T09:00:00.000Z",
    ended_at: null,
    exit_code: null,
    running: true,
  };
}

function finishedBlock(id: string): Block {
  return {
    id,
    pane: "p1",
    session: "s1",
    command: `cmd ${id}`,
    started_at: "2026-06-20T09:00:00.000Z",
    ended_at: "2026-06-20T09:00:02.000Z",
    exit_code: 0,
    running: false,
  };
}

beforeEach(() => {
  // jsdom 29 ships no CSS.escape; the live WebKitGTK/Chromium webview does. The
  // in-place updaters use it to build attribute selectors (as session-ops.ts
  // does), so provide a minimal spec-equivalent polyfill for the test runtime.
  if (typeof globalThis.CSS === "undefined" || typeof globalThis.CSS.escape !== "function") {
    (globalThis as unknown as { CSS: { escape(v: string): string } }).CSS = {
      escape: (v: string) => v.replace(/[^a-zA-Z0-9_-]/g, (c) => `\\${c}`),
    };
  }
  document.body.innerHTML = "";
});

afterEach(() => {
  // Reset shared store fields these tests touch so cases don't leak.
  setState({
    sessions: [],
    activeSession: null,
    windows: new Map(),
    activeWindow: new Map(),
    blocks: [],
    searchResults: null,
    blocksFailuresOnly: false,
    rightCollapsed: false,
  });
});

describe("applyActiveSessionInPlace (rail)", () => {
  it("puts .active only on the active session's row", () => {
    const a = railRow("s1");
    const b = railRow("s2");
    document.body.append(a, b);

    setState({ sessions: [session("s1"), session("s2")], activeSession: "s1" });
    applyActiveSessionInPlace();

    expect(a.classList.contains("active")).toBe(true);
    expect(b.classList.contains("active")).toBe(false);
  });

  it("moves .active in-place without recreating rows (node identity preserved)", () => {
    const a = railRow("s1");
    const b = railRow("s2");
    document.body.append(a, b);

    setState({ activeSession: "s1" });
    applyActiveSessionInPlace();
    expect(a.classList.contains("active")).toBe(true);

    setState({ activeSession: "s2" });
    applyActiveSessionInPlace();

    // SAME element objects — selection moved by a class toggle, not a rebuild.
    expect(document.querySelector('.rail-row[data-session="s1"]')).toBe(a);
    expect(document.querySelector('.rail-row[data-session="s2"]')).toBe(b);
    expect(a.classList.contains("active")).toBe(false);
    expect(b.classList.contains("active")).toBe(true);
  });

  it("is idempotent — re-running with the same active session is a no-op", () => {
    const a = railRow("s1");
    document.body.append(a);

    setState({ activeSession: "s1" });
    applyActiveSessionInPlace();
    applyActiveSessionInPlace();

    expect(a.classList.contains("active")).toBe(true);
  });
});

describe("applyActiveWindowInPlace (tabs)", () => {
  beforeEach(() => {
    setState({
      activeSession: "s1",
      windows: new Map([["s1", [win("w1", 0), win("w2", 1)]]]),
      activeWindow: new Map([["s1", "w1"]]),
    });
  });

  it("puts .active + aria-current only on the active window's pill", () => {
    const a = tabPill("w1");
    const b = tabPill("w2");
    document.body.append(a, b);

    applyActiveWindowInPlace();

    expect(a.classList.contains("active")).toBe(true);
    expect(a.getAttribute("aria-current")).toBe("true");
    expect(b.classList.contains("active")).toBe(false);
    expect(b.getAttribute("aria-current")).toBeNull();
  });

  it("moves the active pill in-place without recreating pills", () => {
    const a = tabPill("w1");
    const b = tabPill("w2");
    document.body.append(a, b);

    applyActiveWindowInPlace();
    expect(a.classList.contains("active")).toBe(true);

    setState({ activeWindow: new Map([["s1", "w2"]]) });
    applyActiveWindowInPlace();

    expect(document.querySelector('.tab-window[data-window="w1"]')).toBe(a);
    expect(document.querySelector('.tab-window[data-window="w2"]')).toBe(b);
    expect(a.classList.contains("active")).toBe(false);
    expect(a.getAttribute("aria-current")).toBeNull();
    expect(b.classList.contains("active")).toBe(true);
    expect(b.getAttribute("aria-current")).toBe("true");
  });
});

describe("applyBlockElapsedInPlace (blocks)", () => {
  it("advances a running block's elapsed text in place (node identity preserved)", () => {
    const { card, dur } = blockCardNode("blk-run", "STALE");
    document.body.append(card);

    setState({ blocks: [runningBlock("blk-run")], searchResults: null });
    applyBlockElapsedInPlace();

    const expected = fmtDuration(
      blockDurationMs("2026-06-20T09:00:00.000Z", null),
    );
    expect(dur.textContent, "elapsed text was refreshed").toBe(expected);
    expect(dur.textContent).not.toBe("STALE");
    // SAME card + same dur node — the clock ticked without a rebuild.
    expect(document.querySelector('.block-card[data-block="blk-run"]')).toBe(card);
    expect(card.querySelector(".block-dur")).toBe(dur);
  });

  it("does not touch a finished block's elapsed text", () => {
    const { card, dur } = blockCardNode("blk-done", "1.0s");
    card.className = "block-card ok";
    document.body.append(card);

    setState({ blocks: [finishedBlock("blk-done")], searchResults: null });
    applyBlockElapsedInPlace();

    expect(dur.textContent, "a settled block's duration is frozen").toBe("1.0s");
  });

  it("no-ops when the right panel is collapsed", () => {
    const { dur } = blockCardNode("blk-run", "STALE");
    document.body.append(dur.parentElement as HTMLElement);

    setState({
      blocks: [runningBlock("blk-run")],
      searchResults: null,
      rightCollapsed: true,
    });
    applyBlockElapsedInPlace();

    expect(dur.textContent, "collapsed panel skips the in-place pass").toBe("STALE");
  });
});
