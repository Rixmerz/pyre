// Unit tests for the per-session tab model — the pure derivation that turns the
// daemon's (single-layout + standalone panes) model into a GUI tab list. The
// rule: tab 0 = "split" (the layout tree); each pane in pane_states for the
// session that is NOT a leaf of the layout tree = its own standalone tab.
//
// These functions read the module-level store, so each test seeds the store via
// setState and resets it in afterEach to keep tests isolated (no order
// dependency, no shared mutable state leaking between cases).

import { describe, it, expect, afterEach } from "vitest";
import {
  setState,
  standalonePanes,
  sessionTabs,
  activeTabOf,
  SPLIT_TAB,
} from "../state";
import type { LayoutNode, PaneStateInfo } from "../types";

const SESSION = "s1";

function paneInfo(pane: string, session = SESSION): PaneStateInfo {
  return { pane, session, state: "idle", title: null, agent: null };
}

function seed(opts: {
  layout?: LayoutNode;
  panes?: PaneStateInfo[];
  activeTab?: [string, string][];
}): void {
  setState({
    layouts: opts.layout ? new Map([[SESSION, opts.layout]]) : new Map(),
    paneStates: new Map((opts.panes ?? []).map((p) => [p.pane, p])),
    activeTab: new Map(opts.activeTab ?? []),
  });
}

// A few layout fixtures.
const leaf = (pane: string): LayoutNode => ({ kind: "leaf", pane });
const splitOf = (a: string, b: string): LayoutNode => ({
  kind: "split",
  dir: "v",
  children: [leaf(a), leaf(b)],
  weights: [50, 50],
});

afterEach(() => {
  // Reset shared store so cases don't leak into one another.
  setState({ layouts: new Map(), paneStates: new Map(), activeTab: new Map() });
});

describe("standalonePanes", () => {
  it("returns empty when every pane is a layout leaf", () => {
    seed({ layout: splitOf("a", "b"), panes: [paneInfo("a"), paneInfo("b")] });
    expect(standalonePanes(SESSION)).toEqual([]);
  });

  it("returns panes present in pane_states but absent from the layout tree", () => {
    seed({
      layout: leaf("a"),
      panes: [paneInfo("a"), paneInfo("standalone1"), paneInfo("standalone2")],
    });
    expect(standalonePanes(SESSION)).toEqual(["standalone1", "standalone2"]);
  });

  it("ignores panes belonging to other sessions", () => {
    seed({
      layout: leaf("a"),
      panes: [paneInfo("a"), paneInfo("other", "s2")],
    });
    expect(standalonePanes(SESSION)).toEqual([]);
  });

  it("treats all panes as standalone when the session has no layout", () => {
    seed({ panes: [paneInfo("x"), paneInfo("y")] });
    expect(standalonePanes(SESSION)).toEqual(["x", "y"]);
  });
});

describe("sessionTabs", () => {
  it("always leads with the split tab", () => {
    seed({ layout: leaf("a"), panes: [paneInfo("a")] });
    expect(sessionTabs(SESSION)).toEqual([{ kind: "split" }]);
  });

  it("appends one pane tab per standalone pane after the split tab", () => {
    seed({
      layout: leaf("a"),
      panes: [paneInfo("a"), paneInfo("p2"), paneInfo("p3")],
    });
    expect(sessionTabs(SESSION)).toEqual([
      { kind: "split" },
      { kind: "pane", pane: "p2" },
      { kind: "pane", pane: "p3" },
    ]);
  });
});

describe("activeTabOf", () => {
  it("defaults to the split tab when none is set", () => {
    seed({ layout: leaf("a"), panes: [paneInfo("a")] });
    expect(activeTabOf(SESSION)).toBe(SPLIT_TAB);
  });

  it("returns the stored active tab for the session", () => {
    seed({
      layout: leaf("a"),
      panes: [paneInfo("a"), paneInfo("p2")],
      activeTab: [[SESSION, "p2"]],
    });
    expect(activeTabOf(SESSION)).toBe("p2");
  });

  it("returns the split tab for a null session", () => {
    expect(activeTabOf(null)).toBe(SPLIT_TAB);
  });
});
