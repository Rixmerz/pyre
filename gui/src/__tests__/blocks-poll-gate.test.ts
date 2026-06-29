// @vitest-environment jsdom
//
// Regression for the focused-blocks poll change-gate (session-ops.reloadFocusedBlocks,
// runs every 750 ms). The bug: it called setState({ blocks }) UNCONDITIONALLY every
// tick → notify → renderAll → the whole UI re-rendered ~1.3×/s at idle. The gate now
// notifies ONLY when a stable block field moved OR a block is running. We assert the
// store's notify() (the single thing renderAll subscribes to) fires exactly when it
// should — directly proving the idle re-render is gone AND that a running block's
// clock keeps ticking (because the poll keeps notifying while it runs).
//
// session-ops transitively imports xterm (../terminals) and the Tauri invoke layer
// (../api, ../notify), so we mock those to load it headless. reloadFocusedBlocks
// itself only reads the real store and listBlocks.

import { describe, it, expect, beforeEach, afterEach, vi, type Mock } from "vitest";

vi.mock("../api", () => ({
  listBlocks: vi.fn(),
  // Imported by session-ops at module load — stub so the module resolves.
  attachPaneStream: vi.fn(),
  detachPaneStream: vi.fn(),
  listSessions: vi.fn(),
  listWindows: vi.fn(),
  paneStates: vi.fn(),
  windowLayout: vi.fn(),
}));
vi.mock("../terminals", () => ({
  disposePaneTerminal: vi.fn(),
  mountedPanes: vi.fn(() => new Set()),
}));
vi.mock("../notify", () => ({
  maybeNotifyTransition: vi.fn(),
  forgetPane: vi.fn(),
}));

import { reloadFocusedBlocks } from "../session-ops";
import { getState, setState, subscribe } from "../state";
import { listBlocks } from "../api";
import type { Block } from "../types";

const listBlocksMock = listBlocks as unknown as Mock;

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

function running(id: string, over: Partial<Block> = {}): Block {
  return block({ id, ended_at: null, exit_code: null, running: true, ...over });
}

function finished(id: string, over: Partial<Block> = {}): Block {
  return block({ id, ended_at: "2026-06-20T09:00:02.000Z", exit_code: 0, running: false, ...over });
}

let unsub: (() => void) | null = null;

beforeEach(() => {
  listBlocksMock.mockReset();
});

afterEach(() => {
  unsub?.();
  unsub = null;
  setState({ focusedPane: null, blocks: [] });
});

/** Subscribe a notify spy AFTER seeding, so only the call under test is counted. */
function spyAfterSeed(): Mock {
  const spy = vi.fn();
  unsub = subscribe(spy);
  return spy;
}

describe("reloadFocusedBlocks change-gate", () => {
  it("does NOT notify when blocks are unchanged and nothing is running (idle tick)", async () => {
    setState({ focusedPane: "p1", blocks: [finished("b1")] });
    listBlocksMock.mockResolvedValue([finished("b1")]); // fresh array, same stable fields
    const spy = spyAfterSeed();

    await reloadFocusedBlocks();

    expect(spy, "idle no-op tick must fire zero notify()").not.toHaveBeenCalled();
  });

  it("DOES notify every tick while a block is running (keeps the elapsed clock ticking)", async () => {
    setState({ focusedPane: "p1", blocks: [running("b1")] });
    // Same stable fields as current — only the running block's elapsed advances.
    listBlocksMock.mockResolvedValue([running("b1")]);
    const spy = spyAfterSeed();

    await reloadFocusedBlocks();

    // notify → renderAll → applyBlockElapsedInPlace advances the counter in place.
    expect(spy, "a running block must keep the poll notifying so its clock ticks").toHaveBeenCalledTimes(1);
  });

  it("notifies when a stable field changes while idle (command differs, both finished)", async () => {
    setState({ focusedPane: "p1", blocks: [finished("b1", { command: "ls" })] });
    listBlocksMock.mockResolvedValue([finished("b1", { command: "pwd" })]);
    const spy = spyAfterSeed();

    await reloadFocusedBlocks();

    expect(spy, "a stable-field change must repaint").toHaveBeenCalledTimes(1);
  });

  it("does NOT notify with no focused pane when blocks are already empty", async () => {
    setState({ focusedPane: null, blocks: [] });
    const spy = spyAfterSeed();

    await reloadFocusedBlocks();

    expect(spy).not.toHaveBeenCalled();
    expect(listBlocksMock).not.toHaveBeenCalled();
  });

  it("clears blocks (one notify) when focus drops but stale blocks remain", async () => {
    setState({ focusedPane: null, blocks: [finished("b1")] });
    const spy = spyAfterSeed();

    await reloadFocusedBlocks();

    expect(spy).toHaveBeenCalledTimes(1);
    expect(getState().blocks).toEqual([]);
  });
});
