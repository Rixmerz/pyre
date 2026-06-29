// Unit tests for the focused-blocks change-gate predicates. blocksStableEqual
// decides whether a 750 ms blocks-poll tick re-renders; hasRunningBlock keeps the
// poll notifying while a block runs so the elapsed clock stays live. A false
// negative would flicker the panel every tick (the bug this gate kills); a false
// positive would freeze a finished/added block. The elapsed time MUST be excluded
// from stable equality (it's patched in place), so two snapshots differing only
// in elapsed/computed fields must compare equal. Pure arrays in, boolean out.

import { describe, it, expect } from "vitest";
import { blocksStableEqual, hasRunningBlock } from "../blocks-eq";
import type { Block } from "../types";

function block(over: Partial<Block> & { id: string }): Block {
  return {
    id: over.id,
    pane: over.pane ?? "p1",
    session: over.session ?? "s1",
    command: over.command ?? `cmd ${over.id}`,
    started_at: over.started_at ?? "2026-06-20T09:00:00.000Z",
    ended_at: over.ended_at ?? null,
    exit_code: over.exit_code ?? null,
    duration_ms: over.duration_ms ?? null,
    running: over.running ?? over.ended_at == null,
  };
}

/** A running block: no exit code, not ended. */
function running(id: string, over: Partial<Block> = {}): Block {
  return block({ id, ended_at: null, exit_code: null, running: true, ...over });
}

/** A finished block: ended + exit code set. */
function finished(id: string, over: Partial<Block> = {}): Block {
  return block({
    id,
    ended_at: "2026-06-20T09:00:02.000Z",
    exit_code: 0,
    running: false,
    ...over,
  });
}

describe("blocksStableEqual", () => {
  it("two empty lists are stable-equal", () => {
    expect(blocksStableEqual([], [])).toBe(true);
  });

  it("identical single-block lists are stable-equal", () => {
    expect(blocksStableEqual([finished("b1")], [finished("b1")])).toBe(true);
  });

  it("returns TRUE for two snapshots differing only in computed/elapsed fields", () => {
    // duration_ms is a computed/volatile field (and a running block's elapsed is
    // not a field at all) — it MUST NOT break stable equality, or the running
    // counter would force a rebuild every tick instead of a tick-in-place.
    const a = [running("b1", { duration_ms: 1000 })];
    const b = [running("b1", { duration_ms: 2000 })];
    expect(blocksStableEqual(a, b)).toBe(true);
  });

  it("returns FALSE when a stable field (command) differs", () => {
    const a = [finished("b1", { command: "ls" })];
    const b = [finished("b1", { command: "pwd" })];
    expect(blocksStableEqual(a, b)).toBe(false);
  });

  it("returns FALSE when a block finishes (exit_code + ended_at appear)", () => {
    expect(blocksStableEqual([running("b1")], [finished("b1")])).toBe(false);
  });

  it("returns FALSE when the exit code changes (0 → 1)", () => {
    const a = [finished("b1", { exit_code: 0 })];
    const b = [finished("b1", { exit_code: 1 })];
    expect(blocksStableEqual(a, b)).toBe(false);
  });

  it("returns FALSE when started_at changes", () => {
    const a = [finished("b1", { started_at: "2026-06-20T09:00:00.000Z" })];
    const b = [finished("b1", { started_at: "2026-06-20T09:05:00.000Z" })];
    expect(blocksStableEqual(a, b)).toBe(false);
  });

  it("returns FALSE when a block is added (length mismatch)", () => {
    expect(blocksStableEqual([finished("b1")], [finished("b1"), finished("b2")])).toBe(false);
  });

  it("returns FALSE when a block is removed", () => {
    expect(blocksStableEqual([finished("b1"), finished("b2")], [finished("b1")])).toBe(false);
  });

  it("is order-sensitive (caller sorts newest-first, so order is meaningful)", () => {
    const a = [finished("b1"), finished("b2")];
    const b = [finished("b2"), finished("b1")];
    expect(blocksStableEqual(a, b)).toBe(false);
  });

  it("multi-block lists: equal only when EVERY block matches stably", () => {
    const a = [running("b1"), finished("b2")];
    const same = [running("b1"), finished("b2")];
    const oneChanged = [running("b1"), finished("b2", { exit_code: 7 })];
    expect(blocksStableEqual(a, same)).toBe(true);
    expect(blocksStableEqual(a, oneChanged)).toBe(false);
  });
});

describe("hasRunningBlock", () => {
  it("is false for an empty list", () => {
    expect(hasRunningBlock([])).toBe(false);
  });

  it("is false when every block is finished", () => {
    expect(hasRunningBlock([finished("b1"), finished("b2")])).toBe(false);
  });

  it("is true when any block is running (no exit code, not ended)", () => {
    expect(hasRunningBlock([finished("b1"), running("b2")])).toBe(true);
  });

  it("keys off exit_code/ended_at, not the daemon `running` boolean", () => {
    // A block the daemon marked running:false but with no exit code/ended_at is
    // still mid-flight to the RENDER layer (which is what advances the clock), so
    // it counts as running here — matching applyBlockElapsedInPlace exactly.
    const ambiguous = block({ id: "b1", running: false, ended_at: null, exit_code: null });
    expect(hasRunningBlock([ambiguous])).toBe(true);
  });
});
