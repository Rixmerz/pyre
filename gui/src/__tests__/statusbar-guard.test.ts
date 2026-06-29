// @vitest-environment jsdom
//
// Regression: the status bar is fingerprint-guarded like every other renderAll
// region. On an idle poll tick (same connectivity, socket, process line, pane
// count, active-session name and theme) renderStatusbar MUST skip its
// replaceChildren rebuild so node identity survives — this is what stops the last
// unguarded region from flickering the moment any hover/transition is added. A
// genuine state change (here: theme) MUST still rebuild.
//
// statusbar.ts pulls in the Tauri invoke layer (../api: inspectPid, reconnect),
// so we mock it to let the module load in jsdom. renderStatusbar only reads
// getState() and writes the live DOM.

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";

vi.mock("../api", () => ({ inspectPid: vi.fn(), reconnect: vi.fn() }));

import { renderStatusbar } from "../render/statusbar";
import { setState } from "../state";
import type { PaneStateInfo, SessionInfo } from "../types";

function session(id: string, name: string): SessionInfo {
  return { id, name, pane_count: 1 };
}

function paneState(pane: string): PaneStateInfo {
  return { pane, session: "s1", state: "idle", title: null };
}

/** A deterministic, fully-populated baseline so the fingerprint is stable. */
function baseline(): void {
  setState({
    connected: true,
    socket: "/run/pyred.sock",
    sessions: [session("s1", "main")],
    activeSession: "s1",
    paneStates: new Map([["p1", paneState("p1")]]),
    focusedPane: "p1",
    pidReadout: null,
    activeTheme: "ember",
  });
}

beforeEach(() => {
  document.body.innerHTML = "";
  baseline();
});

afterEach(() => {
  // Reset the fields this file touches so cases don't leak into other suites.
  setState({
    connected: false,
    socket: "",
    sessions: [],
    activeSession: null,
    paneStates: new Map(),
    focusedPane: null,
    pidReadout: null,
    activeTheme: "ember",
  });
});

describe("renderStatusbar fingerprint guard", () => {
  it("skips the rebuild on an identical re-render (node identity preserved)", () => {
    const root = document.createElement("footer");
    document.body.append(root);

    renderStatusbar(root); // first paint (childElementCount 0 → always builds)
    const probed = root.firstElementChild as HTMLElement;
    expect(probed, "first render produced children").toBeTruthy();
    probed.dataset["probe"] = "1";

    renderStatusbar(root); // identical state → fingerprint unchanged → skip

    // The SAME tagged node is still mounted — the guard skipped replaceChildren.
    expect(root.querySelector('[data-probe="1"]')).toBe(probed);
  });

  it("rebuilds when a rendered value changes (theme), dropping the probe node", () => {
    const root = document.createElement("footer");
    document.body.append(root);

    renderStatusbar(root);
    const probed = root.firstElementChild as HTMLElement;
    probed.dataset["probe"] = "1";

    setState({ activeTheme: "frost" }); // a value the bar renders → fingerprint moves
    renderStatusbar(root);

    // replaceChildren ran → the tagged node was torn out, fresh nodes replaced it.
    expect(root.querySelector('[data-probe="1"]')).toBeNull();
    expect(root.querySelector(".status-item.theme")?.textContent).toBe("frost");
  });

  it("rebuilds when the process line changes (pidReadout), reflecting the new pid", () => {
    const root = document.createElement("footer");
    document.body.append(root);

    renderStatusbar(root);
    const probed = root.firstElementChild as HTMLElement;
    probed.dataset["probe"] = "1";

    // A readout for the focused pane appears → the process line must paint.
    setState({
      pidReadout: { pane: "p1", pid: 4242, comm: "/usr/bin/bash", childCount: 2 },
    });
    renderStatusbar(root);

    expect(root.querySelector('[data-probe="1"]')).toBeNull();
    const proc = root.querySelector(".status-proc");
    expect(proc?.getAttribute("title")).toBe("pid 4242");
    expect(proc?.querySelector(".status-proc-metric")?.textContent).toBe("2 children");
  });

  it("treats an unchanged process readout as a no-op (no rebuild)", () => {
    const root = document.createElement("footer");
    document.body.append(root);

    setState({
      pidReadout: { pane: "p1", pid: 99, comm: "bash", childCount: 0 },
    });
    renderStatusbar(root);
    const probed = root.firstElementChild as HTMLElement;
    probed.dataset["probe"] = "1";

    // A NEW object with identical displayed fields — fingerprint must be stable.
    setState({
      pidReadout: { pane: "p1", pid: 99, comm: "bash", childCount: 0 },
    });
    renderStatusbar(root);

    expect(root.querySelector('[data-probe="1"]')).toBe(probed);
  });
});
