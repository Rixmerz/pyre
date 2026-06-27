// In-app toast notifications. A single bottom-corner stack (`.toast-stack`)
// holds transient `.toast` cards; each dismisses via the exit-animation contract
// (the CSS agent owns the actual transform — this module only times the removal
// and toggles the modifier classes the CSS keys off).
//
// SHARED CLASS CONTRACT (must match styles.css exactly):
//   .toast-stack                        — the container (created once, on <body>)
//   .toast.toast--{error|info|success}  — one notification card
//   .toast--out                         — exit state; CSS animates out, JS removes
//
// Kept dependency-free and textContent-only (never innerHTML) so a daemon error
// string can flow through without an XSS footgun.

export type ToastKind = "error" | "info" | "success";

// ponytail: hard cap on visible toasts so a flapping error loop can't grow an
// unbounded stack — past this, the oldest toast is evicted early.
const MAX_VISIBLE = 4;

/** Auto-dismiss delays. Errors linger longer (more to read / act on). */
const DISMISS_MS: Record<ToastKind, number> = {
  error: 6000,
  info: 4000,
  success: 4000,
};

/** Fallback removal delay when `animationend` never fires (matches --dur-fast).
 *  Reduced-motion paths skip the CSS animation, so this timer does the cleanup. */
const EXIT_FALLBACK_MS = 120;

let stack: HTMLElement | null = null;

/** Lazily create (once) and return the shared toast stack on `<body>`. */
function ensureStack(): HTMLElement {
  if (stack?.isConnected) return stack;
  const existing = document.querySelector<HTMLElement>(".toast-stack");
  if (existing) {
    stack = existing;
    return existing;
  }
  const el = document.createElement("div");
  el.className = "toast-stack";
  el.setAttribute("role", "region");
  el.setAttribute("aria-label", "Notifications");
  el.setAttribute("aria-live", "polite");
  document.body.appendChild(el);
  stack = el;
  return el;
}

/**
 * Show a transient toast. `message` is rendered as TEXT (never HTML).
 * Auto-dismisses after a kind-dependent delay with the exit animation.
 */
export function toast(message: string, kind: ToastKind = "info"): void {
  const root = ensureStack();

  const node = document.createElement("div");
  node.className = `toast toast--${kind}`;
  // Errors interrupt (assertive); info/success are polite status updates.
  node.setAttribute("role", kind === "error" ? "alert" : "status");
  node.textContent = message;
  root.appendChild(node);

  // Evict the oldest LIVE toast(s) over the cap. Count only those not already
  // exiting (`.toast--out`) — a dismissed node lingers a frame for its exit
  // animation, so counting it would re-evict it and spin. Oldest = first in DOM.
  const live = root.querySelectorAll<HTMLElement>(".toast:not(.toast--out)");
  for (let i = 0; i < live.length - MAX_VISIBLE; i++) {
    dismiss(live[i]!);
  }

  window.setTimeout(() => dismiss(node), DISMISS_MS[kind]);
}

/** Begin a toast's exit: add `.toast--out`, then remove on animationend/fallback. */
function dismiss(node: HTMLElement): void {
  if (!node.isConnected || node.classList.contains("toast--out")) return;
  node.classList.add("toast--out");

  let removed = false;
  const remove = (): void => {
    if (removed) return;
    removed = true;
    node.remove();
  };
  node.addEventListener("animationend", remove, { once: true });
  window.setTimeout(remove, EXIT_FALLBACK_MS);
}
