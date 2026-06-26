// Unit tests for the per-session WINDOW model — the pure reads that turn the
// daemon's `list_windows` data (held in `state.windows` / `state.activeWindow`)
// into the GUI tab strip. The rule: the tab strip = a session's windows, ordered
// by position; each window owns its own layout tree and a daemon-authoritative
// name; the active window defaults to the first when none is explicitly chosen.
//
// These functions read the module-level store, so each test seeds the store via
// setState and resets it in afterEach to keep tests isolated (no order
// dependency, no shared mutable state leaking between cases).

import { describe, it, expect, afterEach } from "vitest";
import {
  setState,
  windowTabs,
  windowLabel,
  activeWindowOf,
} from "../state";
import type { WindowInfo } from "../types";

const SESSION = "s1";

function win(over: Partial<WindowInfo> & { id: string }): WindowInfo {
  return {
    id: over.id,
    session: over.session ?? SESSION,
    name: over.name ?? "",
    position: over.position ?? 0,
    pane_count: over.pane_count ?? 1,
    created_at: over.created_at,
  };
}

function seed(opts: {
  windows?: WindowInfo[];
  activeWindow?: [string, string][];
}): void {
  setState({
    windows: opts.windows ? new Map([[SESSION, opts.windows]]) : new Map(),
    activeWindow: new Map(opts.activeWindow ?? []),
  });
}

afterEach(() => {
  // Reset the shared store so cases don't leak into one another.
  setState({ windows: new Map(), activeWindow: new Map() });
});

describe("windowTabs", () => {
  it("returns the session's windows in stored order", () => {
    const a = win({ id: "w1", position: 0 });
    const b = win({ id: "w2", position: 1 });
    seed({ windows: [a, b] });
    expect(windowTabs(SESSION)).toEqual([a, b]);
  });

  it("returns an empty list for a session with no windows loaded", () => {
    seed({ windows: [] });
    expect(windowTabs(SESSION)).toEqual([]);
  });

  it("returns an empty list for a null session", () => {
    expect(windowTabs(null)).toEqual([]);
  });

  it("does not leak windows from another session", () => {
    seed({ windows: [win({ id: "w1" })] });
    expect(windowTabs("other-session")).toEqual([]);
  });
});

describe("windowLabel", () => {
  it("uses the daemon name when present", () => {
    expect(windowLabel(win({ id: "w1", name: "backend", position: 0 }))).toBe(
      "backend",
    );
  });

  it("falls back to the 1-based position when the name is empty", () => {
    expect(windowLabel(win({ id: "w1", name: "", position: 2 }))).toBe("3");
  });

  it("falls back when the name is whitespace only", () => {
    expect(windowLabel(win({ id: "w1", name: "   ", position: 0 }))).toBe("1");
  });
});

describe("activeWindowOf", () => {
  it("defaults to the first window when none is explicitly selected", () => {
    seed({ windows: [win({ id: "w1" }), win({ id: "w2", position: 1 })] });
    expect(activeWindowOf(SESSION)).toBe("w1");
  });

  it("returns the explicitly-selected window when it still exists", () => {
    seed({
      windows: [win({ id: "w1" }), win({ id: "w2", position: 1 })],
      activeWindow: [[SESSION, "w2"]],
    });
    expect(activeWindowOf(SESSION)).toBe("w2");
  });

  it("falls back to the first window when the selected one is gone", () => {
    seed({
      windows: [win({ id: "w1" }), win({ id: "w2", position: 1 })],
      activeWindow: [[SESSION, "stale-window-id"]],
    });
    expect(activeWindowOf(SESSION)).toBe("w1");
  });

  it("returns null when the session has no windows", () => {
    seed({ windows: [] });
    expect(activeWindowOf(SESSION)).toBeNull();
  });

  it("returns null for a null session", () => {
    expect(activeWindowOf(null)).toBeNull();
  });
});
