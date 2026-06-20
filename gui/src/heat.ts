// The HEAT RAMP — pyre's signature. Agent state is rendered as heat.
//
// This mapping is INVARIANT across themes: when a user picks a new theme the
// chrome reskins, but --heat-* never moves. That is what makes "agent state =
// temperature" a stable visual language rather than a per-theme accident.
//
// The actual color values live in styles.css as the --heat-* custom properties
// (the single source of truth). Here we only map a PaneState to the matching
// CSS variable name and carry the behavioural facts (which states pulse, which
// read as "hot"), so JS and CSS never disagree about the ramp.

import type { PaneState } from "./types";

/** CSS custom property holding the heat color for a given state. */
export function heatVar(state: PaneState): string {
  switch (state) {
    case "idle":
      return "var(--heat-idle)";
    case "running":
      return "var(--heat-running)";
    case "waiting":
      return "var(--heat-waiting)";
    case "interactive":
      return "var(--heat-interactive)";
    case "crashed":
      return "var(--heat-crashed)";
    case "done":
      return "var(--heat-done)";
  }
}

/**
 * States that PULSE (subtle keyframe) to demand attention: a pane waiting on
 * the user, and a crashed pane. Everything else is steady. The pulse animation
 * is itself gated behind prefers-reduced-motion in CSS.
 */
export function pulses(state: PaneState): boolean {
  return state === "waiting" || state === "crashed";
}

/**
 * Heat ordering for "hottest pane in a session" — a session rail dot shows the
 * temperature of its most-demanding pane. Higher = more attention-worthy.
 * crashed and waiting outrank a merely-running pane; idle/done are coolest.
 */
const HEAT_RANK: Record<PaneState, number> = {
  crashed: 5,
  waiting: 4,
  interactive: 3,
  running: 2,
  done: 1,
  idle: 0,
};

/** Pick the hottest (most attention-worthy) state from a set of pane states. */
export function hottest(states: PaneState[]): PaneState {
  if (states.length === 0) return "idle";
  return states.reduce((hot, s) =>
    HEAT_RANK[s] > HEAT_RANK[hot] ? s : hot,
  );
}

/** Human-readable label for a state (status bar, tooltips, palette). */
export function stateLabel(state: PaneState): string {
  switch (state) {
    case "idle":
      return "Idle";
    case "running":
      return "Running";
    case "waiting":
      return "Waiting for input";
    case "interactive":
      return "Interactive";
    case "crashed":
      return "Crashed";
    case "done":
      return "Done";
  }
}
