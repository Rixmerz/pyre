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

  const restoreLabel = (): void => {
    delete label.dataset["editing"];
    if (input.parentNode === parent) parent.replaceChild(label, input);
  };

  const cancel = (): void => {
    if (settled) return;
    settled = true;
    restoreLabel();
  };

  const commit = (): void => {
    if (settled) return;
    settled = true;
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
