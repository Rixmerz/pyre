// In-app toast notifications. A single bottom-corner stack (`.toast-stack`)
// holds transient `.toast` cards; each dismisses via the exit-animation contract
// (the CSS agent owns the actual transform — this module only times the removal
// and toggles the modifier classes the CSS keys off).
//
// SHARED CLASS CONTRACT (must match styles.css exactly):
//   .toast-stack                        — the container (created once, on <body>)
//   .toast.toast--{error|info|success}  — one notification card
//   .toast--out                         — exit state; CSS animates out, JS removes
//   .toast__count                       — ×N repeat badge (coalesced duplicates)
//
// Kept dependency-free and textContent-only (never innerHTML) so a daemon error
// string can flow through without an XSS footgun.
//
// coalesce: when toast() is called and the MOST-RECENT still-visible card has the
// SAME message AND kind, we DON'T append a duplicate — we bump a repeat count
// (shown as a "×N" badge from N=2) and reset that card's auto-dismiss timer so it
// lives the full duration from the latest hit. Only the newest live card is
// checked (consecutive identical failures are the spam case); the whole stack is
// never scanned/merged. No entrance animation is replayed on a repeat — the badge
// text just updates in place — so coalescing never reintroduces flicker.

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

/** Per-card coalesce state, keyed off the card node (auto-GC'd when removed).
 *  `timer` is the live auto-dismiss handle so a repeat can reset it; `badge` is
 *  lazily created on the 2nd occurrence. Kept off the DOM node (no textContent
 *  pollution) so the identity check below compares the message, not message+×N. */
interface CardState {
  message: string;
  kind: ToastKind;
  count: number;
  timer: number;
  badge: HTMLElement | null;
}
const cards = new WeakMap<HTMLElement, CardState>();

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

  // coalesce: if the most-recent still-visible card is identical (same message
  // AND kind), bump its repeat count + reset its dismiss timer instead of stacking
  // a duplicate. Newest live = last `.toast:not(.toast--out)` in DOM order.
  const liveBefore = root.querySelectorAll<HTMLElement>(".toast:not(.toast--out)");
  const newest = liveBefore.length ? liveBefore[liveBefore.length - 1]! : null;
  if (newest) {
    const state = cards.get(newest);
    if (state && state.message === message && state.kind === kind) {
      state.count += 1;
      updateBadge(newest, state);
      window.clearTimeout(state.timer);
      state.timer = window.setTimeout(() => dismiss(newest), DISMISS_MS[kind]);
      return;
    }
  }

  const node = document.createElement("div");
  node.className = `toast toast--${kind}`;
  // Errors interrupt (assertive); info/success are polite status updates.
  node.setAttribute("role", kind === "error" ? "alert" : "status");
  node.textContent = message;
  root.appendChild(node);

  const timer = window.setTimeout(() => dismiss(node), DISMISS_MS[kind]);
  cards.set(node, { message, kind, count: 1, timer, badge: null });

  // Evict the oldest LIVE toast(s) over the cap. Count only those not already
  // exiting (`.toast--out`) — a dismissed node lingers a frame for its exit
  // animation, so counting it would re-evict it and spin. Oldest = first in DOM.
  const live = root.querySelectorAll<HTMLElement>(".toast:not(.toast--out)");
  for (let i = 0; i < live.length - MAX_VISIBLE; i++) {
    dismiss(live[i]!);
  }
}

/** Render/update the "×N" repeat badge in place (shown from N=2). The badge is
 *  `aria-hidden` so a coalesced repeat doesn't re-fire the card's alert/status
 *  live-region announcement — silencing the spam is the whole point of coalescing.
 *  No class toggle / re-animation: updating the text avoids reintroducing flicker. */
function updateBadge(node: HTMLElement, state: CardState): void {
  if (state.count < 2) return;
  if (!state.badge) {
    const badge = document.createElement("span");
    badge.className = "toast__count";
    badge.setAttribute("aria-hidden", "true");
    node.appendChild(badge);
    state.badge = badge;
  }
  state.badge.textContent = `×${state.count}`;
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
