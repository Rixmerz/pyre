// Reusable inline rename editor — the one place a label turns into a text
// input. Used by the session rail, the standalone-pane tab pills, and the
// split-layout pane card headers, so all three rename affordances share the
// exact same behavior and "hearth" styling (quiet input, ember focus ring).
//
// Behavior contract:
//   - Enter  → commit (calls onCommit with the trimmed value)
//   - Esc    → cancel (restore the original label, no callback)
//   - blur   → commit (same as Enter — clicking away saves)
// A no-op commit (empty, or unchanged from the seed) is treated as a cancel so
// renaming to "" or to the same name never round-trips to the daemon.
//
// The helper swaps the input IN PLACE of the label element and restores the
// label on cancel; on commit it leaves the input in place and fires the
// callback — the caller's refresh re-renders the surface with the new name.

import { dlog } from "../debug";

// How many inline editors are currently mounted in the DOM. The render loop
// reads this (via `isInlineEditing`) to SKIP rebuilding a surface while the user
// is mid-rename — otherwise the 750ms heat poll (which calls setState →
// renderAll on any heat change) would tear out the <input> and wipe the edit
// before Enter/blur commits. Each surface that re-renders unconditionally
// (rail, tabs) guards on this; the center is already fingerprint-guarded but
// also defers tab/focus-driven structural rebuilds while editing.
let activeEditors = 0;

/** True while at least one inline rename editor is open. Render loop guards on
 *  this so a poll re-render never destroys an open editor mid-edit. */
export function isInlineEditing(): boolean {
  return activeEditors > 0;
}

/** Delay (ms) a single click waits to see if it's actually the first half of a
 *  double-click. Just under the OS double-click window so a real double-click
 *  always lands inside it. */
const DBLCLICK_GRACE_MS = 250;

export interface RenameAffordanceOpts {
  /** The label span the affordance is attached to (becomes the editor target). */
  label: HTMLElement;
  /** Current name to seed the editor with on rename. */
  value: () => string;
  /** Single-click action (switch session / focus pane). Suppressed if the click
   *  turns out to be the first half of a double-click. Omit for no-op. */
  onSingleClick?: () => void;
  /** Commit callback for the rename (wired into beginInlineEdit's onCommit). */
  onCommit: (name: string) => void | Promise<void>;
  /** Extra class on the editor input (per-surface width/metrics). */
  inputClass?: string;
  /** aria-label for the editor input. */
  ariaLabel?: string;
}

/**
 * Wire the SHARED rename affordance onto a label span so all three surfaces
 * (rail session name, tab pill label, pane-card title) behave identically:
 *
 *   - single-click  → onSingleClick (debounced; cancelled if a double-click follows)
 *   - double-click  → open the inline editor seeded with value()
 *   - mousedown/click/dblclick → stopPropagation, so the parent's own
 *     click/mousedown handler (row switch, card focus) never fires on this span
 *     and never triggers a destructive re-render that would tear the span out
 *     from under the pending double-click.
 *
 * The label OWNS its single-click here rather than letting it bubble to the
 * parent, which is the fix for the rail + subpane: the parent's switch/focus
 * setState used to rebuild the surface between the two clicks of a double-click,
 * destroying the span before `dblclick` could fire on it.
 */
export function attachRenameAffordance(opts: RenameAffordanceOpts): void {
  const { label, onSingleClick, value, onCommit } = opts;
  label.title = "Double-click to rename";

  let clickTimer: number | undefined;

  // Stop mousedown so a parent's onmousedown (e.g. the pane card's focusPane)
  // can't fire a structural re-render mid-double-click.
  label.addEventListener("mousedown", (e) => e.stopPropagation());

  label.addEventListener("click", (e) => {
    e.stopPropagation();
    if (!onSingleClick) return;
    // Defer the single-click action; a following dblclick clears this timer so
    // the switch/focus never runs when the user meant to rename.
    if (clickTimer !== undefined) window.clearTimeout(clickTimer);
    clickTimer = window.setTimeout(() => {
      clickTimer = undefined;
      onSingleClick();
    }, DBLCLICK_GRACE_MS);
  });

  label.addEventListener("dblclick", (e) => {
    e.stopPropagation();
    e.preventDefault();
    if (clickTimer !== undefined) {
      window.clearTimeout(clickTimer);
      clickTimer = undefined;
    }
    beginInlineEdit({
      label,
      value: value(),
      inputClass: opts.inputClass,
      ariaLabel: opts.ariaLabel,
      onCommit,
    });
  });
}

export interface InlineEditOpts {
  /** The label element to replace with the editor (its parentNode hosts the swap). */
  label: HTMLElement;
  /** Current name to seed the input with. */
  value: string;
  /** Called with the new trimmed name when the edit commits to a real change. */
  onCommit: (name: string) => void | Promise<void>;
  /** Optional extra class on the input (e.g. to scope width per surface). */
  inputClass?: string;
  /** Optional aria-label for the input. Defaults to "Rename". */
  ariaLabel?: string;
}

/**
 * Replace `label` with a seeded text input and wire commit/cancel. Returns the
 * input element (already focused + selected) in case the caller wants a handle.
 * Safe to call once per double-click; guards against re-entrancy via a flag on
 * the label so a double-double-click doesn't stack two editors.
 */
export function beginInlineEdit(opts: InlineEditOpts): HTMLInputElement | null {
  const { label, value, onCommit } = opts;
  const parent = label.parentNode;
  if (!parent) return null;

  // Re-entrancy guard: if this label is already being edited, ignore.
  if (label.dataset["editing"] === "1") return null;
  label.dataset["editing"] = "1";
  // Count this editor as live so the render loop (renderAll → renderRail/
  // renderTabs/renderCenter) skips rebuilding the surface and wiping the input
  // while the user types. Decremented exactly once when the edit settles.
  activeEditors += 1;

  const input = document.createElement("input");
  input.type = "text";
  input.className = "inline-edit-input" + (opts.inputClass ? ` ${opts.inputClass}` : "");
  input.value = value;
  input.spellcheck = false;
  input.setAttribute("aria-label", opts.ariaLabel ?? "Rename");
  // Stop the parent's single-click handlers (focus/switch) from firing while
  // the user clicks within the input to position the caret.
  const stop = (e: Event): void => e.stopPropagation();
  input.addEventListener("mousedown", stop);
  input.addEventListener("click", stop);
  input.addEventListener("dblclick", stop);

  let settled = false;

  // Flip the settled flag and balance the live-editor count exactly once. Both
  // cancel and commit funnel through here so `activeEditors` can never leak
  // (which would freeze the render loop) nor under-count (which would let a poll
  // wipe the input).
  const markSettled = (): boolean => {
    if (settled) return false;
    settled = true;
    activeEditors = Math.max(0, activeEditors - 1);
    return true;
  };

  const restoreLabel = (): void => {
    delete label.dataset["editing"];
    if (input.parentNode === parent) parent.replaceChild(label, input);
  };

  const cancel = (): void => {
    if (!markSettled()) return;
    restoreLabel();
  };

  const commit = (): void => {
    if (!markSettled()) return;
    const next = input.value.trim();
    // No-op: empty or unchanged → treat as cancel (restore label, no round-trip).
    if (next === "" || next === value.trim()) {
      restoreLabel();
      return;
    }
    // Leave the input in place; the caller's refresh repaints the surface with
    // the committed name (so we don't flash the stale label before the reload).
    delete label.dataset["editing"];
    void Promise.resolve(onCommit(next)).catch((err) =>
      dlog("[pyre-rename] onCommit rejected — kept fallback label:", err),
    );
  };

  input.addEventListener("keydown", (e: KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      e.stopPropagation();
      commit();
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      cancel();
    }
  });
  // Blur = commit (clicking away saves). Fires after Enter/Esc already settled,
  // so the `settled` guard makes it a no-op in those paths.
  input.addEventListener("blur", () => commit());

  parent.replaceChild(input, label);
  input.focus();
  input.select();
  return input;
}
