// Per-session git-status poller. Every 3 s it asks the daemon for each known
// session's git state and funnels the result through `setSessionGit`, whose
// change-detection gate drops no-op ticks — so a quiet repo never triggers a rail
// rebuild (the chronic-flicker fix discipline, commit 27888ba). A transient
// daemon error leaves the prior value untouched: a hiccup must never blank a chip
// that was correct a moment ago.
//
// Runs over the same `invoke` transport seam as the rest of the app, so it works
// identically in the browser mock loop (`pnpm dev:mock`) and against real pyred.

import { invoke } from "./invoke";
import { getState, setSessionGit } from "./state";
import type { GitInfo } from "./types";

const GIT_POLL_MS = 3000;

/** Interval handle; non-null once started. Guards against a double-start. */
let timer: number | null = null;

/**
 * Poll git status for every session currently in state, once. Sequential is fine
 * — sessions are few; a per-call try/catch isolates failures so one erroring
 * session never aborts the sweep and never clears another session's chip.
 */
async function pollOnce(): Promise<void> {
  for (const sess of getState().sessions) {
    try {
      const git = await invoke<GitInfo | null>("git_status", {
        session: sess.id,
      });
      setSessionGit(sess.id, git);
    } catch {
      // Transient daemon hiccup — keep the prior value, do not clear the chip.
    }
  }
}

/**
 * Start the 3 s git-status poll loop. Idempotent — a second call is a no-op so the
 * boot path can call it unconditionally. Fires once immediately so chips appear on
 * the first list, then every `GIT_POLL_MS`.
 */
export function startGitPolling(): void {
  if (timer != null) return;
  void pollOnce();
  timer = window.setInterval(() => void pollOnce(), GIT_POLL_MS);
}
