// PR / CI status poller. Fetches `github_pr_ci` for each session at two cadences:
//  1. SLOW interval (>=30 s) — GitHub is a network call; hammering it would burn
//     the rate limit and slow the app. Change-gated via setSessionPrCi so a stable
//     PR/CI state produces zero render rebuilds each tick.
//  2. IMMEDIATE on session/branch change — subscribes to state updates and fires
//     once for the active session whenever the focused session id or its branch
//     changes, so the chip is live the moment the user switches to a PR branch.
//
// CWD source: `git_status(session)` now carries the resolved working directory
// (`GitInfo.cwd`, proto v7), so both the branch and the cwd come from the SAME
// call the poller already makes — no separate `inspect_pid` → env → PWD step.
// When `git_status` reports no cwd (null/absent) the session is skipped for the
// tick and the prior chip value is left intact — no flicker on transient gaps.

import { githubPrCi } from "./api";
import { getSessionGit, getState, setSessionPrCi, subscribe } from "./state";

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
 *  - git_status reports no cwd (not a repo / cwd unresolved) — never calls
 *    github_pr_ci with an empty cwd; the chip stays hidden,
 *  - github_pr_ci rejects (keeps prior chip value).
 */
async function pollSession(sessId: string): Promise<void> {
  const git = getSessionGit(sessId);
  if (!git) return;

  const cwd = git.cwd;
  if (!cwd) return; // no resolved cwd → skip, do not call github_pr_ci

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
