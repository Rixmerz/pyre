// Agent-awareness notifications. When a pane TRANSITIONS into a state the user
// cares about — Done (finished) or Waiting (needs input) — fire a native OS
// notification via the Tauri `notify(title, body)` command (implemented by the
// Rust agent). Wired DEFENSIVELY: a missing/rejecting command must never break
// the lifecycle loop.
//
// Discipline (per brief):
//   • Fire only on the TRANSITION into done/waiting, never on repeats.
//   • Debounce so a flapping pane can't spam the OS notification centre.
//   • Skip a Done notification for the pane the user is already looking at
//     (focused + visible) — they can see it finished; a toast would be noise.
//     A Waiting pane still notifies even if focused, because "needs input" is
//     actionable regardless of visibility.

import { invoke } from "./invoke";
import { getState } from "./state";
import type { PaneState } from "./types";

/** States that warrant a notification, mapped to a title. */
const NOTIFY_TITLES: Partial<Record<PaneState, string>> = {
  done: "Pane finished",
  waiting: "Needs your input",
};

/** Per-pane debounce: ignore a repeat notification within this window. */
const DEBOUNCE_MS = 4000;

/** Last notified {state, time} per pane, to suppress repeats + debounce. */
interface LastNotify {
  state: PaneState;
  at: number;
}
const lastByPane = new Map<string, LastNotify>();

let enabled = false;

/** Arm notifications. Called once at boot. */
export function initNotifications(): void {
  enabled = true;
}

/** Resolve a human label for a session, falling back to the session id. */
function sessionLabel(session: string | undefined): string {
  if (!session) return "session";
  const s = getState().sessions.find((x) => x.id === session);
  return s?.name ?? session;
}

/** Fire the Tauri notify command, swallowing any error. */
async function emit(title: string, body: string): Promise<void> {
  try {
    await invoke("notify", { title, body });
  } catch (err) {
    // Expected in `vite dev` (no Tauri host) or before the Rust command lands.
    console.warn("[pyre-notify] notify unavailable — skipping:", err);
  }
}

/**
 * Consider a pane state transition for notification. Call from the lifecycle
 * `state_changed` handler with the PREVIOUS and NEXT state.
 *
 * @param pane    pane id that changed
 * @param prev    state before this event (undefined if unknown/first-seen)
 * @param next    state after this event
 * @param session session the pane belongs to (for the body text)
 */
export function maybeNotifyTransition(
  pane: string,
  prev: PaneState | undefined,
  next: PaneState,
  session: string | undefined,
): void {
  if (!enabled) return;

  const title = NOTIFY_TITLES[next];
  if (!title) return; // not a notify-worthy state

  // Only on the TRANSITION into the state, never on repeats of the same state.
  if (prev === next) return;

  // First-seen panes report their initial state with no prior — don't notify
  // a pane into existence as "done"/"waiting"; only real transitions count.
  if (prev === undefined) {
    lastByPane.set(pane, { state: next, at: Date.now() });
    return;
  }

  // Skip a "done" toast for the pane the user is actively looking at (focused
  // and not hidden behind another window). "waiting" always notifies because it
  // is actionable even when visible. document.hidden guards the focused case:
  // if the whole window is backgrounded, even the focused pane warrants a ping.
  if (next === "done") {
    const st = getState();
    const isFocusedVisible =
      st.focusedPane === pane && !document.hidden;
    if (isFocusedVisible) {
      lastByPane.set(pane, { state: next, at: Date.now() });
      return;
    }
  }

  // Debounce: suppress a repeat for the same pane+state inside the window.
  const now = Date.now();
  const last = lastByPane.get(pane);
  if (last && last.state === next && now - last.at < DEBOUNCE_MS) return;

  lastByPane.set(pane, { state: next, at: now });
  void emit(title, sessionLabel(session));
}

/** Forget a pane's notify history (call when a pane is closed/disposed). */
export function forgetPane(pane: string): void {
  lastByPane.delete(pane);
}
