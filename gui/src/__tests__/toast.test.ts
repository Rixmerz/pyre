// @vitest-environment jsdom
//
// DOM behavior tests for the in-app toast channel. They cover the contract the
// CSS agent keys off and the lifecycle this module owns:
//   1. toast() lazily creates a single `.toast-stack` and appends a kind-tagged
//      `.toast--<kind>` card with the message as TEXT (never parsed as HTML);
//   2. a toast auto-dismisses (adds `.toast--out`, then removes the node) after
//      its kind delay + the exit fallback;
//   3. the visible stack is capped — extra toasts evict the oldest.
//
// jsdom never fires `animationend`, so removal rides the 120ms fallback timer;
// fake timers drive it deterministically (no sleeps).

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { toast } from "../toast";

function stackEl(): HTMLElement {
  const el = document.querySelector<HTMLElement>(".toast-stack");
  if (!el) throw new Error("expected a .toast-stack to exist");
  return el;
}

beforeEach(() => {
  document.body.innerHTML = "";
});

afterEach(() => {
  vi.useRealTimers();
  document.body.innerHTML = "";
});

describe("toast", () => {
  it("toast_firstCall_createsStackWithKindTaggedCard", () => {
    // Arrange / Act
    toast("Couldn't split the pane.", "error");

    // Assert
    const stack = stackEl();
    expect(stack.childElementCount, "one toast appended").toBe(1);
    const card = stack.firstElementChild as HTMLElement;
    expect(card.classList.contains("toast"), "base class").toBe(true);
    expect(card.classList.contains("toast--error"), "kind modifier").toBe(true);
    expect(card.textContent).toBe("Couldn't split the pane.");
  });

  it("toast_secondCall_reusesSameStack", () => {
    // Arrange
    toast("first", "info");
    // Act
    toast("second", "success");
    // Assert — still exactly one stack, two cards in it
    expect(document.querySelectorAll(".toast-stack").length).toBe(1);
    expect(stackEl().childElementCount).toBe(2);
  });

  it("toast_messageWithMarkup_rendersAsTextNotHtml", () => {
    // Arrange / Act — an error string that looks like an HTML/script injection
    toast("<img src=x onerror=alert(1)>", "error");

    // Assert — no child element was parsed; the raw text is preserved verbatim
    const card = stackEl().firstElementChild as HTMLElement;
    expect(card.children.length, "no HTML parsed").toBe(0);
    expect(card.textContent).toBe("<img src=x onerror=alert(1)>");
  });

  it("toast_afterDelayAndExit_removesNode", () => {
    // Arrange
    vi.useFakeTimers();
    toast("vanishing", "info");
    const stack = stackEl();
    expect(stack.childElementCount).toBe(1);

    // Act — info auto-dismisses at 4000ms, marking `.toast--out`...
    vi.advanceTimersByTime(4000);
    expect(
      stack.firstElementChild?.classList.contains("toast--out"),
      "exit state applied before removal",
    ).toBe(true);
    // ...then the 120ms fallback removes the node (animationend never fires here).
    vi.advanceTimersByTime(120);

    // Assert
    expect(stack.childElementCount, "node removed after exit").toBe(0);
  });

  it("toast_overCap_evictsOldestLiveToasts", () => {
    // Arrange
    vi.useFakeTimers();

    // Act — push 6 toasts; the cap is 4, so the 2 oldest must be evicted.
    for (let i = 0; i < 6; i++) toast(`t${i}`, "info");

    // Assert — the 2 oldest are marked exiting immediately; live count is capped.
    const stack = stackEl();
    expect(stack.querySelectorAll(".toast--out").length, "2 evicted").toBe(2);
    expect(
      stack.querySelectorAll(".toast:not(.toast--out)").length,
      "live count capped at 4",
    ).toBe(4);

    // After the exit fallback, the evicted nodes are gone (4 remain).
    vi.advanceTimersByTime(120);
    expect(stack.childElementCount, "stack trimmed to cap").toBe(4);
    expect(stack.textContent, "the survivors are the 4 newest").toBe(
      "t2t3t4t5",
    );
  });
});
