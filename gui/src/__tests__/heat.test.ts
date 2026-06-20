// Unit tests for the heat ramp — pyre's signature "agent state = temperature"
// mapping. Pure functions, no DOM, no daemon. These lock the invariant ordering
// and the state→CSS-var contract so JS and CSS can never silently disagree.

import { describe, it, expect } from "vitest";
import { heatVar, pulses, hottest, stateLabel } from "../heat";
import { PANE_STATES } from "../types";
import type { PaneState } from "../types";

describe("heatVar", () => {
  it("maps each PaneState to its dedicated --heat-* CSS variable", () => {
    const expected: Record<PaneState, string> = {
      idle: "var(--heat-idle)",
      running: "var(--heat-running)",
      waiting: "var(--heat-waiting)",
      interactive: "var(--heat-interactive)",
      crashed: "var(--heat-crashed)",
      done: "var(--heat-done)",
    };
    for (const state of PANE_STATES) {
      expect(heatVar(state)).toBe(expected[state]);
    }
  });

  it("returns a distinct variable for every state (no collisions)", () => {
    const vars = PANE_STATES.map(heatVar);
    expect(new Set(vars).size).toBe(PANE_STATES.length);
  });
});

describe("pulses", () => {
  it("pulses ONLY for attention-demanding states (waiting, crashed)", () => {
    expect(pulses("waiting")).toBe(true);
    expect(pulses("crashed")).toBe(true);
  });

  it("does NOT pulse for steady states (idle, running, interactive, done)", () => {
    expect(pulses("idle")).toBe(false);
    expect(pulses("running")).toBe(false);
    expect(pulses("interactive")).toBe(false);
    expect(pulses("done")).toBe(false);
  });
});

describe("hottest", () => {
  it("returns 'idle' for an empty set (nothing demanding attention)", () => {
    expect(hottest([])).toBe("idle");
  });

  it("returns the single state when given one", () => {
    expect(hottest(["running"])).toBe("running");
  });

  it("ranks crashed above waiting above interactive above running", () => {
    expect(hottest(["running", "crashed"])).toBe("crashed");
    expect(hottest(["running", "waiting"])).toBe("waiting");
    expect(hottest(["interactive", "waiting"])).toBe("waiting");
    expect(hottest(["running", "interactive"])).toBe("interactive");
  });

  it("treats idle and done as the coolest (lose to anything active)", () => {
    expect(hottest(["idle", "running"])).toBe("running");
    expect(hottest(["done", "running"])).toBe("running");
    expect(hottest(["idle", "done"])).toBe("done"); // done (1) outranks idle (0)
  });

  it("picks crashed as the hottest across the full ramp regardless of order", () => {
    expect(hottest(["idle", "done", "running", "interactive", "waiting", "crashed"]))
      .toBe("crashed");
    expect(hottest(["crashed", "idle", "running"])).toBe("crashed");
  });
});

describe("stateLabel", () => {
  it("gives a human-readable label for every state", () => {
    const expected: Record<PaneState, string> = {
      idle: "Idle",
      running: "Running",
      waiting: "Waiting for input",
      interactive: "Interactive",
      crashed: "Crashed",
      done: "Done",
    };
    for (const state of PANE_STATES) {
      expect(stateLabel(state)).toBe(expected[state]);
    }
  });
});
