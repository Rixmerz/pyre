// @vitest-environment jsdom
//
// DOM behavior tests for the shared rename affordance and the inline editor.
// These cover the THREE things that were broken across the rail / tab / subpane
// surfaces and the contract that fixes them:
//   1. double-click on a label opens the inline <input> editor;
//   2. a double-click does NOT also fire the single-click action (switch/focus);
//   3. while an editor is open, isInlineEditing() reports true so the render
//      loop skips the poll-driven rebuild that used to wipe the editor;
//   4. Enter commits the trimmed value, Esc cancels (restores the label).
//
// jsdom does not synthesize a real `dblclick` from two `click`s, so each test
// dispatches the exact events the browser would (click ×2 then dblclick, or a
// lone dblclick) to assert the handler wiring deterministically.

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import {
  attachRenameAffordance,
  beginInlineEdit,
  isInlineEditing,
} from "../render/inline-edit";

function makeLabel(text: string): HTMLElement {
  const parent = document.createElement("div");
  const span = document.createElement("span");
  span.className = "label";
  span.textContent = text;
  parent.appendChild(span);
  document.body.appendChild(parent);
  return span;
}

function dispatch(el: HTMLElement, type: string): void {
  el.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true }));
}

beforeEach(() => {
  vi.useFakeTimers();
  document.body.innerHTML = "";
});

afterEach(() => {
  // Settle any editor a test left open so the module-level activeEditors counter
  // (and isInlineEditing) drains to zero — otherwise it leaks into the next test.
  for (const el of Array.from(document.querySelectorAll("input.inline-edit-input"))) {
    el.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  }
  vi.runOnlyPendingTimers();
  vi.useRealTimers();
  document.body.innerHTML = "";
});

describe("attachRenameAffordance — double-click opens the editor", () => {
  it("dblclick_onLabel_opensInlineInputSeededWithValue", () => {
    const label = makeLabel("orig");
    const parent = label.parentNode as HTMLElement;
    attachRenameAffordance({
      label,
      value: () => "orig",
      onCommit: () => {},
    });

    dispatch(label, "dblclick");

    const input = parent.querySelector("input.inline-edit-input");
    expect(input, "an <input> editor replaces the label on dblclick").not.toBeNull();
    expect((input as HTMLInputElement).value).toBe("orig");
    expect(parent.contains(label)).toBe(false);
  });
});

describe("attachRenameAffordance — single vs double click", () => {
  it("singleClick_firesSwitchImmediately_noDebounce", () => {
    const label = makeLabel("p");
    const onSingleClick = vi.fn();
    attachRenameAffordance({
      label,
      value: () => "p",
      onSingleClick,
      onCommit: () => {},
    });

    dispatch(label, "click");

    // No grace window: the select fires synchronously. Selection is applied
    // in-place (active-class toggle), so it never rebuilds the surface — the row
    // that owns this span survives, which is what makes the immediate fire safe.
    expect(
      onSingleClick,
      "a click switches/focuses immediately, with no debounce",
    ).toHaveBeenCalledTimes(1);
  });

  it("doubleClick_stillOpensEditor_afterImmediateSelect", () => {
    const label = makeLabel("p");
    const parent = label.parentNode as HTMLElement;
    const onSingleClick = vi.fn();
    attachRenameAffordance({
      label,
      value: () => "p",
      onSingleClick,
      onCommit: () => {},
    });

    // A real double-click: two clicks then the dblclick. Each click selects
    // immediately (harmless — in-place active toggle never tears out the row),
    // and the dblclick STILL opens the inline editor. This is the critical
    // rename-a-non-active edge case: the immediate select must not destroy the
    // span before dblclick lands.
    dispatch(label, "click");
    dispatch(label, "click");
    dispatch(label, "dblclick");

    const input = parent.querySelector("input.inline-edit-input");
    expect(
      input,
      "dblclick opens the editor even though the click already selected",
    ).not.toBeNull();
    expect(
      onSingleClick,
      "the first click selects (in-place active makes this harmless)",
    ).toHaveBeenCalled();
  });

  it("clickOnLabel_stopsPropagationToParent", () => {
    const label = makeLabel("p");
    const parent = label.parentNode as HTMLElement;
    const parentClick = vi.fn();
    parent.addEventListener("click", parentClick);
    attachRenameAffordance({
      label,
      value: () => "p",
      onSingleClick: () => {},
      onCommit: () => {},
    });

    dispatch(label, "click");

    expect(parentClick, "label click must not bubble to the row/card").not.toHaveBeenCalled();
  });

  it("mousedownOnLabel_stopsPropagationToParent", () => {
    const label = makeLabel("p");
    const parent = label.parentNode as HTMLElement;
    const parentMousedown = vi.fn();
    parent.addEventListener("mousedown", parentMousedown);
    attachRenameAffordance({
      label,
      value: () => "p",
      onCommit: () => {},
    });

    dispatch(label, "mousedown");

    expect(
      parentMousedown,
      "mousedown must not reach the card (which would focusPane → destructive re-render)",
    ).not.toHaveBeenCalled();
  });
});

describe("isInlineEditing — render-loop guard", () => {
  it("falseWhenNoEditorOpen", () => {
    expect(isInlineEditing()).toBe(false);
  });

  it("trueWhileEditorOpen_falseAfterCommit", () => {
    const label = makeLabel("orig");
    const onCommit = vi.fn();
    const input = beginInlineEdit({ label, value: "orig", onCommit })!;

    expect(isInlineEditing(), "guard is set while the editor is mounted").toBe(true);

    input.value = "renamed";
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));

    expect(onCommit).toHaveBeenCalledWith("renamed");
    expect(isInlineEditing(), "guard clears once the edit settles").toBe(false);
  });

  it("trueWhileEditorOpen_falseAfterEscapeCancel", () => {
    const label = makeLabel("orig");
    const parent = label.parentNode as HTMLElement;
    const onCommit = vi.fn();
    const input = beginInlineEdit({ label, value: "orig", onCommit })!;

    expect(isInlineEditing()).toBe(true);

    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));

    expect(onCommit, "Esc cancels — no round-trip").not.toHaveBeenCalled();
    expect(parent.contains(label), "Esc restores the original label").toBe(true);
    expect(isInlineEditing(), "guard clears on cancel too").toBe(false);
  });
});

describe("beginInlineEdit — commit semantics", () => {
  it("unchangedValue_treatedAsCancel_noCallback", () => {
    const label = makeLabel("same");
    const parent = label.parentNode as HTMLElement;
    const onCommit = vi.fn();
    const input = beginInlineEdit({ label, value: "same", onCommit })!;

    input.value = "  same  "; // trims to the seed → no-op
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));

    expect(onCommit, "renaming to the same name must not round-trip").not.toHaveBeenCalled();
    expect(parent.contains(label)).toBe(true);
    expect(isInlineEditing()).toBe(false);
  });

  it("commit_restoresLabelWithNewText_removesInput", () => {
    // Arrange
    const label = makeLabel("orig");
    const parent = label.parentNode as HTMLElement;
    const onCommit = vi.fn();
    const input = beginInlineEdit({ label, value: "orig", onCommit })!;

    // Act
    input.value = "renamed";
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));

    // Assert — BUG-1: the center skips a name-only repaint, so commit must
    // restore the label itself rather than leave a frozen <input> behind.
    expect(
      parent.querySelector("input.inline-edit-input"),
      "the editor input is removed on commit",
    ).toBeNull();
    expect(parent.contains(label), "the label node is restored in place").toBe(true);
    expect(label.textContent, "the restored label shows the committed name").toBe("renamed");
    expect(onCommit).toHaveBeenCalledWith("renamed");
  });

  it("blur_commitsTheTrimmedValue", () => {
    const label = makeLabel("orig");
    const onCommit = vi.fn();
    const input = beginInlineEdit({ label, value: "orig", onCommit })!;

    input.value = "  fresh ";
    input.dispatchEvent(new FocusEvent("blur"));

    expect(onCommit, "clicking away commits").toHaveBeenCalledWith("fresh");
    expect(isInlineEditing()).toBe(false);
  });
});

describe("beginInlineEdit — focus handoff (WebKitGTK regression)", () => {
  // In Tauri/WebKitGTK, input.focus() alone does not evict keyboard focus from
  // xterm's hidden textarea, so Enter keeps routing to the PTY. beginInlineEdit
  // must blur the previously-focused element BEFORE focusing the editor input.
  // jsdom focus semantics are enough to assert the blur side effect here.
  it("blursThePreviouslyFocusedElement_beforeFocusingTheInput", () => {
    // Arrange — stand in for xterm's hidden textarea, focused as in the live app.
    const ta = document.createElement("textarea");
    ta.className = "xterm-helper-textarea";
    document.body.appendChild(ta);
    const blurred = vi.fn();
    ta.addEventListener("blur", blurred);
    ta.focus();
    expect(document.activeElement, "precondition: the textarea holds focus").toBe(ta);

    // Act
    const label = makeLabel("orig");
    const input = beginInlineEdit({ label, value: "orig", onCommit: () => {} })!;

    // Assert — the prior element was blurred and focus moved into the editor.
    expect(blurred, "the previously-focused element is blurred").toHaveBeenCalledTimes(1);
    expect(document.activeElement, "focus is handed to the rename input").toBe(input);
  });
});
