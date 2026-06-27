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
// label on both cancel (original text) and commit (the new text), then fires
// the callback — so the committed name is visible immediately even on surfaces
// whose refresh skips a name-only repaint (the center's fingerprint guard).

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
 *   - single-click  → onSingleClick fires IMMEDIATELY (no debounce)
 *   - double-click  → open the inline editor seeded with value()
 *   - mousedown/click/dblclick → stopPropagation, so the parent's own
 *     click/mousedown handler (row switch, card focus) never fires on this span
 *     and never triggers a destructive re-render that would tear the span out
 *     from under the pending double-click.
 *
 * Single-click is no longer debounced. It previously waited a grace window so a
 * double-click wouldn't first fire switchSession/switchWindow — whose async
 * reload rebuilt the surface between the two clicks and destroyed this span
 * before `dblclick` could land. Selection is now applied IN-PLACE (rail/tabs
 * toggle the active class on the existing rows via applyActiveSessionInPlace /
 * applyActiveWindowInPlace), so a single-click never rebuilds the surface; the
 * span survives the first click of a double-click and the editor still opens on
 * dblclick. The first click of a genuine double-click therefore selects
 * harmlessly (idempotent when already active), then dblclick opens rename.
 */
export function attachRenameAffordance(opts: RenameAffordanceOpts): void {
  const { label, onSingleClick, value, onCommit } = opts;
  label.title = "Double-click to rename";

  // Stop mousedown so a parent's onmousedown (e.g. the pane card's focusPane)
  // can't fire a structural re-render mid-double-click.
  label.addEventListener("mousedown", (e) => e.stopPropagation());

  label.addEventListener("click", (e) => {
    e.stopPropagation();
    if (onSingleClick) onSingleClick();
  });

  label.addEventListener("dblclick", (e) => {
    e.stopPropagation();
    e.preventDefault();
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
    // Restore the label optimistically with the new name BEFORE the async RPC.
    // The center surface's renderCenter early-returns on name-only changes
    // (layout fingerprint unchanged), so it never repaints the card — leaving
    // the <input> in place would strand a frozen editor and hide .pane-title
    // from applyHeatInPlace. Showing `next` in the restored label makes the new
    // name visible immediately (no flash) and is harmless for rail/tabs, which
    // replaceChildren on the next renderAll. Order: clear editing flag
    // (markSettled, above) → restore label with new text → fire onCommit.
    label.textContent = next;
    restoreLabel();
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
  // WebKitGTK (the Tauri webview) does NOT reliably move keyboard focus into a
  // plain <input> via input.focus() alone while xterm's hidden textarea still
  // holds focus — keystrokes (including Enter) keep routing to the PTY and this
  // input's keydown never fires. Blur whatever currently holds focus first (the
  // mirror of focusPaneTerminal's BOTH-focus workaround in terminals.ts) so
  // WebKitGTK releases the terminal textarea before we steal focus into the
  // editor. In jsdom this is a harmless no-op.
  const active = document.activeElement as HTMLElement | null;
  if (active && active !== input) active.blur();
  input.focus();
  input.select();
  return input;
}
