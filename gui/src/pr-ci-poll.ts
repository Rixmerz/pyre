// PR / CI status poller. Fetches `github_pr_ci` for each session at two cadences:
//  1. SLOW interval (>=30 s) — GitHub is a network call; hammering it would burn
//     the rate limit and slow the app. Change-gated via setSessionPrCi so a stable
//     PR/CI state produces zero render rebuilds each tick.
//  2. IMMEDIATE on session/branch change — subscribes to state updates and fires
//     once for the active session whenever the focused session id or its branch
//     changes, so the chip is live the moment the user switches to a PR branch.
//
// CWD derivation: `inspect_pid(pane)` → env → PWD entry. The same inspect_pid
// path already powers the statusbar process readout, so it is proven reliable in
// both mock (always returns /home/dev/pyre) and against real pyred. A missing PWD
// entry or a failed inspect_pid silently skips the session for this tick and
// leaves the prior chip value intact — no flicker on transient errors.

import { githubPrCi, inspectPid } from "./api";
import {
  getSessionGit,
  getState,
  panesOfSession,
  setSessionPrCi,
  subscribe,
} from "./state";

/** 30 s minimum between full sweeps — GitHub rate-limit friendly. */
const PR_CI_POLL_MS = 30_000;

/** Interval handle; non-null once started. Guards against a double-start. */
let timer: number | null = null;

/**
 * "session:branch" key for the currently tracked active session. When this key
 * changes (new session selected or branch switched) an immediate poll fires for
 * the new context so the chip updates at once rather than waiting up to 30 s.
 */
let lastActiveKey = "";

// ── Per-session fetch ─────────────────────────────────────────────────────────

/**
 * Fetch PR/CI for one session and store it change-gated. Silently skips when:
 *  - no git info yet (branch unknown),
 *  - no pane available to inspect for PWD,
 *  - inspect_pid rejects or env lacks PWD,
 *  - github_pr_ci rejects (keeps prior chip value).
 */
async function pollSession(sessId: string): Promise<void> {
  const git = getSessionGit(sessId);
  if (!git) return;

  const panes = panesOfSession(sessId);
  const paneId = panes[0]?.pane;
  if (!paneId) return;

  let cwd: string;
  try {
    const pidInfo = await inspectPid(paneId);
    const entry = pidInfo.env.find(([k]) => k === "PWD");
    if (!entry) return;
    cwd = entry[1];
  } catch {
    // inspect_pid missing or pane gone — skip silently.
    return;
  }

  try {
    const prCi = await githubPrCi(cwd, git.branch);
    setSessionPrCi(sessId, prCi);
  } catch {
    // Transient network/daemon error — keep the prior chip value.
  }
}

/** Poll all known sessions (the 30 s sweep). Sequential is fine — sessions few. */
async function pollAll(): Promise<void> {
  for (const sess of getState().sessions) {
    await pollSession(sess.id);
  }
}

// ── Immediate-on-change detection ─────────────────────────────────────────────

/**
 * Called on every state change. Fires an immediate `pollSession` when the active
 * session or its branch changes. The `lastActiveKey` guard ensures only genuine
 * changes trigger a fetch — an idle 750 ms poll tick where the key is unchanged
 * is a no-op at this check level, never reaching the network.
 */
function onStateChange(): void {
  const s = getState();
  const active = s.activeSession;
  if (!active) return;
  const git = getSessionGit(active);
  const key = `${active}:${git?.branch ?? ""}`;
  if (key === lastActiveKey) return;
  lastActiveKey = key;
  void pollSession(active);
}

// ── Public API ────────────────────────────────────────────────────────────────

/**
 * Start the PR/CI poll loop. Idempotent — a second call is a no-op. Fires one
 * full sweep immediately (chips appear on first render), then every 30 s. Also
 * subscribes to state so session/branch changes get an immediate fetch.
 */
export function startPrCiPolling(): void {
  if (timer != null) return;
  void pollAll();
  timer = window.setInterval(() => void pollAll(), PR_CI_POLL_MS);
  subscribe(onStateChange);
}
