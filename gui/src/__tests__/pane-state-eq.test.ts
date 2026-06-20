// Unit tests for paneStatesEqual — the predicate that decides whether a heat
// poll tick re-renders. A false negative would cause needless re-renders (and
// the xterm re-parent / focus-loss this whole guard exists to prevent); a false
// positive would freeze the UI on stale heat. Pure Maps in, boolean out.

import { describe, it, expect } from "vitest";
import { paneStatesEqual } from "../pane-state-eq";
import type { PaneStateInfo } from "../types";

function info(over: Partial<PaneStateInfo> & { pane: string }): PaneStateInfo {
  return {
    pane: over.pane,
    session: over.session ?? "s1",
    state: over.state ?? "idle",
    title: over.title ?? null,
    agent: over.agent,
  };
}

function snapshot(...entries: PaneStateInfo[]): Map<string, PaneStateInfo> {
  return new Map(entries.map((e) => [e.pane, e]));
}

describe("paneStatesEqual", () => {
  it("two empty snapshots are equal", () => {
    expect(paneStatesEqual(new Map(), new Map())).toBe(true);
  });

  it("identical single-pane snapshots are equal", () => {
    const a = snapshot(info({ pane: "p1", state: "running", title: "build" }));
    const b = snapshot(info({ pane: "p1", state: "running", title: "build" }));
    expect(paneStatesEqual(a, b)).toBe(true);
  });

  it("differs when a pane's state changes (idle → running)", () => {
    const a = snapshot(info({ pane: "p1", state: "idle" }));
    const b = snapshot(info({ pane: "p1", state: "running" }));
    expect(paneStatesEqual(a, b)).toBe(false);
  });

  it("differs when a pane's title changes", () => {
    const a = snapshot(info({ pane: "p1", title: "npm test" }));
    const b = snapshot(info({ pane: "p1", title: "npm build" }));
    expect(paneStatesEqual(a, b)).toBe(false);
  });

  it("differs when a pane's agent changes (shell → claude)", () => {
    const a = snapshot(info({ pane: "p1", agent: "shell" }));
    const b = snapshot(info({ pane: "p1", agent: "claude" }));
    expect(paneStatesEqual(a, b)).toBe(false);
  });

  it("treats missing agent and explicit null agent as equal (defensive default)", () => {
    const a = snapshot(info({ pane: "p1", agent: undefined }));
    const b = snapshot(info({ pane: "p1", agent: null }));
    expect(paneStatesEqual(a, b)).toBe(true);
  });

  it("differs when a pane is added (size mismatch)", () => {
    const a = snapshot(info({ pane: "p1" }));
    const b = snapshot(info({ pane: "p1" }), info({ pane: "p2" }));
    expect(paneStatesEqual(a, b)).toBe(false);
  });

  it("differs when a pane is removed", () => {
    const a = snapshot(info({ pane: "p1" }), info({ pane: "p2" }));
    const b = snapshot(info({ pane: "p1" }));
    expect(paneStatesEqual(a, b)).toBe(false);
  });

  it("differs when one pane is replaced by another at the same size", () => {
    const a = snapshot(info({ pane: "p1" }), info({ pane: "p2" }));
    const b = snapshot(info({ pane: "p1" }), info({ pane: "p3" }));
    expect(paneStatesEqual(a, b)).toBe(false);
  });

  it("ignores session differences (not part of the heat-render contract)", () => {
    // session is intentionally NOT compared — only state/title/agent gate render.
    const a = snapshot(info({ pane: "p1", session: "s1" }));
    const b = snapshot(info({ pane: "p1", session: "s2" }));
    expect(paneStatesEqual(a, b)).toBe(true);
  });

  it("multi-pane snapshots: equal only when EVERY pane matches", () => {
    const a = snapshot(
      info({ pane: "p1", state: "running" }),
      info({ pane: "p2", state: "waiting" }),
    );
    const same = snapshot(
      info({ pane: "p1", state: "running" }),
      info({ pane: "p2", state: "waiting" }),
    );
    const oneChanged = snapshot(
      info({ pane: "p1", state: "running" }),
      info({ pane: "p2", state: "done" }),
    );
    expect(paneStatesEqual(a, same)).toBe(true);
    expect(paneStatesEqual(a, oneChanged)).toBe(false);
  });
});
