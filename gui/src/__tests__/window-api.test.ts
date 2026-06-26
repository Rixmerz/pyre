// Contract tests for the window RPC wrappers in api.ts. The Tauri bridge is
// wired blind (a parallel agent implements the commands), so these tests pin the
// exact command name + argument shape each wrapper sends through `invoke`. A
// drift here (renamed command, wrong arg key) would silently break the daemon
// round-trip at runtime; catching it at the wrapper boundary is the cheapest
// place to assert it.

import { describe, it, expect, vi, beforeEach } from "vitest";

// Mock the Tauri surface api.ts imports. `invoke` is the one under test; `listen`
// is mocked only so importing api.ts doesn't pull the real (browser-only) module.
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));

import { invoke } from "@tauri-apps/api/core";
import {
  listWindows,
  newWindow,
  renameWindow,
  closeWindow,
  windowLayout,
  openPane,
} from "../api";

const invokeMock = vi.mocked(invoke);

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
});

describe("window RPC wrappers", () => {
  it("listWindows → list_windows { session }", async () => {
    invokeMock.mockResolvedValue([]);
    await listWindows("s1");
    expect(invokeMock).toHaveBeenCalledWith("list_windows", { session: "s1" });
  });

  it("newWindow without a name passes name: null", async () => {
    invokeMock.mockResolvedValue("w-new");
    const id = await newWindow("s1");
    expect(invokeMock).toHaveBeenCalledWith("new_window", {
      session: "s1",
      name: null,
    });
    expect(id).toBe("w-new");
  });

  it("newWindow with a name passes it through", async () => {
    invokeMock.mockResolvedValue("w-new");
    await newWindow("s1", "backend");
    expect(invokeMock).toHaveBeenCalledWith("new_window", {
      session: "s1",
      name: "backend",
    });
  });

  it("renameWindow → rename_window { window, name }", async () => {
    await renameWindow("w1", "api");
    expect(invokeMock).toHaveBeenCalledWith("rename_window", {
      window: "w1",
      name: "api",
    });
  });

  it("closeWindow → close_window { window }", async () => {
    await closeWindow("w1");
    expect(invokeMock).toHaveBeenCalledWith("close_window", { window: "w1" });
  });

  it("windowLayout → get_window_layout { window }", async () => {
    invokeMock.mockResolvedValue({ kind: "leaf", pane: "p1" });
    await windowLayout("w1");
    expect(invokeMock).toHaveBeenCalledWith("get_window_layout", {
      window: "w1",
    });
  });

  it("openPane targets a window and carries default geometry", async () => {
    invokeMock.mockResolvedValue({ pane: "p1" });
    await openPane("w1", "s1");
    expect(invokeMock).toHaveBeenCalledWith("open_pane", {
      window: "w1",
      session: "s1",
      cols: 80,
      rows: 24,
    });
  });
});
