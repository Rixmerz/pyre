// @vitest-environment jsdom
//
// Tests for the PR/CI chip — data layer and render-discipline (flicker) guard.
//
// Covers:
//   1. Chip renders PR# + CI dot when PrCiInfo is present.
//   2. Chip is hidden (no .git-pr-number / .git-ci-dot) when prCi is absent.
//   3. CI dot is hidden when ci_state === "none".
//   4. Correct CSS class per CI state (success/failure/pending/running).
//   5. Fingerprint guard — no DOM rebuild when prCi is unchanged (node identity).
//   6. Fingerprint guard — rebuilds when pr_number changes.
//   7. Fingerprint guard — rebuilds when ci_state changes.
//
// renderRail imports actions + inline-edit; both are mocked so the test has no
// side-effects outside the render module.

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";

vi.mock("../actions", () => ({
  closeSessionAction: vi.fn(),
  newSession: vi.fn(),
  renameSessionAction: vi.fn(),
  switchSession: vi.fn(),
  toggleRail: vi.fn(),
}));
vi.mock("../render/inline-edit", () => ({
  attachRenameAffordance: vi.fn(),
  isInlineEditing: vi.fn(() => false),
}));

import { renderRail } from "../render/rail";
import { setState, setSessionGit, setSessionPrCi } from "../state";
import type { PrCiInfo, SessionInfo, PaneStateInfo } from "../types";

// ── Helpers ───────────────────────────────────────────────────────────────────

function session(id: string, name: string): SessionInfo {
  return { id, name, pane_count: 1 };
}

function paneState(pane: string, sessId: string): PaneStateInfo {
  return { pane, session: sessId, state: "idle", title: null };
}

function prCi(
  pr_number: number,
  ci_state: PrCiInfo["ci_state"],
): PrCiInfo {
  return {
    pr_number,
    pr_url: `https://github.com/r/p/pull/${pr_number}`,
    ci_state,
  };
}

function baseline(): void {
  setState({
    sessions: [session("s1", "main")],
    activeSession: "s1",
    paneStates: new Map([["p1", paneState("p1", "s1")]]),
    railCollapsed: false,
  });
  setSessionGit("s1", {
    branch: "main",
    dirty: 0,
    ahead: 0,
    behind: 0,
    upstream: "origin/main",
  });
}

function freshRoot(): HTMLElement {
  const root = document.createElement("aside");
  document.body.append(root);
  return root;
}

// ── Setup / teardown ──────────────────────────────────────────────────────────

beforeEach(() => {
  document.body.innerHTML = "";
  baseline();
});

afterEach(() => {
  // Reset all state this file touches so tests don't bleed into each other.
  setSessionPrCi("s1", null);
  setSessionGit("s1", null);
  setState({
    sessions: [],
    activeSession: null,
    paneStates: new Map(),
    railCollapsed: false,
  });
});

// ── Chip render ───────────────────────────────────────────────────────────────

describe("PR/CI chip rendering", () => {
  it("shows PR# and CI dot when PrCiInfo has pr_number and a non-none ci_state", () => {
    setSessionPrCi("s1", prCi(42, "success"));
    const root = freshRoot();

    renderRail(root);

    expect(root.querySelector(".git-pr-number")?.textContent).toBe("#42");
    expect(root.querySelector(".git-ci-dot")).toBeTruthy();
    expect(root.querySelector(".git-ci-success")).toBeTruthy();
  });

  it("hides PR# and CI dot when prCi is absent (not fetched / no PR)", () => {
    // prCi not set — absent key means hidden
    const root = freshRoot();

    renderRail(root);

    expect(root.querySelector(".git-pr-number")).toBeNull();
    expect(root.querySelector(".git-ci-dot")).toBeNull();
  });

  it("shows PR# but hides CI dot when ci_state is 'none'", () => {
    setSessionPrCi("s1", prCi(10, "none"));
    const root = freshRoot();

    renderRail(root);

    expect(root.querySelector(".git-pr-number")?.textContent).toBe("#10");
    expect(root.querySelector(".git-ci-dot")).toBeNull();
  });

  it("applies the correct CSS class for each CI state", () => {
    const states: Array<PrCiInfo["ci_state"]> = [
      "success",
      "failure",
      "pending",
      "running",
    ];
    for (const ciState of states) {
      document.body.innerHTML = "";
      setSessionPrCi("s1", prCi(1, ciState));
      const root = freshRoot();

      renderRail(root);

      expect(
        root.querySelector(`.git-ci-${ciState}`),
        `expected .git-ci-${ciState} to be present`,
      ).toBeTruthy();
    }
  });

  it("hides PR# when pr_number is null (no PR despite non-none ci_state)", () => {
    setSessionPrCi("s1", {
      pr_number: null,
      pr_url: null,
      ci_state: "pending",
    });
    const root = freshRoot();

    renderRail(root);

    expect(root.querySelector(".git-pr-number")).toBeNull();
    // ci_state is "pending" (non-none) so the CI dot still renders
    expect(root.querySelector(".git-ci-pending")).toBeTruthy();
  });
});

// ── Fingerprint guard (flicker-safe) ─────────────────────────────────────────

describe("PR/CI fingerprint guard", () => {
  it("does NOT rebuild when prCi is unchanged (node identity preserved)", () => {
    setSessionPrCi("s1", prCi(42, "success"));
    const root = freshRoot();

    renderRail(root); // first paint — builds, captures fingerprint
    const probe = root.firstElementChild as HTMLElement;
    probe.dataset["probe"] = "1";

    renderRail(root); // identical state → fingerprint unchanged + childElementCount > 0 → skip

    expect(root.querySelector('[data-probe="1"]')).toBe(probe);
  });

  it("rebuilds and updates when pr_number changes", () => {
    setSessionPrCi("s1", prCi(42, "success"));
    const root = freshRoot();

    renderRail(root);
    (root.firstElementChild as HTMLElement).dataset["probe"] = "1";

    setSessionPrCi("s1", prCi(99, "success")); // pr_number changed → fingerprint moves
    renderRail(root);

    expect(root.querySelector('[data-probe="1"]')).toBeNull();
    expect(root.querySelector(".git-pr-number")?.textContent).toBe("#99");
  });

  it("rebuilds and updates when ci_state changes", () => {
    setSessionPrCi("s1", prCi(42, "pending"));
    const root = freshRoot();

    renderRail(root);
    (root.firstElementChild as HTMLElement).dataset["probe"] = "1";

    setSessionPrCi("s1", prCi(42, "success")); // ci_state changed → fingerprint moves
    renderRail(root);

    expect(root.querySelector('[data-probe="1"]')).toBeNull();
    expect(root.querySelector(".git-ci-success")).toBeTruthy();
  });

  it("does NOT rebuild when a new PrCiInfo object has identical fields", () => {
    setSessionPrCi("s1", prCi(42, "running"));
    const root = freshRoot();

    renderRail(root);
    const probe = root.firstElementChild as HTMLElement;
    probe.dataset["probe"] = "1";

    // A new object — setSessionPrCi's change-gate should detect no real diff
    // and return WITHOUT calling notify(), so the fingerprint stays the same
    // and renderRail skips the rebuild.
    setSessionPrCi("s1", prCi(42, "running"));
    renderRail(root);

    expect(root.querySelector('[data-probe="1"]')).toBe(probe);
  });
});
