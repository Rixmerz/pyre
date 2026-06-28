// GitHub account-linking flow (OAuth App + Device Flow).
//
// Orchestrates the device-code dance the Tauri Rust layer exposes: start →
// open the browser → poll on the daemon-given interval → on Authorized fetch
// the account, on Denied/Expired surface a toast. pyre NEVER sees the user's
// password; the user authorizes in their own browser and the Tauri layer keeps
// only the token (OS keychain). This module owns the timers + the expiry
// deadline; render/github.ts owns the modal DOM and reads `getLinkDeadline()`
// for the live countdown. See `.claude/notions/feature-github-oauth.md`.

import {
  githubAccount,
  githubDevicePoll,
  githubDeviceStart,
  githubDisconnect,
} from "./api";
import { getState, setGithub, setState } from "./state";
import { toast } from "./toast";
import { dlog } from "./debug";

/** Server-side revocation page for the pyre OAuth App. A local disconnect only
 *  forgets the token — the user revokes the grant here. Surfaced as the account
 *  menu's "Manage on GitHub" link. */
const REVOKE_URL =
  "https://github.com/settings/connections/applications/Ov23li1g0XoYJex02nIG";

/** Live poll timer + expiry deadline for the open link attempt (null when idle). */
let pollTimer: number | null = null;
let deadline: number | null = null;
/** Guards `startGitHubLink` against a double-start while the start RPC is inflight. */
let starting = false;
/** Count of consecutive untagged poll errors; resets on any successful response. */
let consecutivePollErrors = 0;
/** Max consecutive untagged errors before treating the flow as terminally broken. */
const POLL_ERROR_LIMIT = 5;

/** Epoch-ms the current device code expires, or null when no link is open.
 *  render/github.ts reads this to paint the countdown in place. */
export function getLinkDeadline(): number | null {
  return deadline;
}

/** The OAuth App's server-side revocation URL (for the account menu). */
export function getRevokeUrl(): string {
  return REVOKE_URL;
}

/**
 * Open a URL in the user's real browser. In the production Tauri host this goes
 * through the shell plugin so it leaves the webview; in the browser mock loop
 * (no Tauri host) it falls back to `window.open`. Never throws — a failed open
 * degrades to the fallback so the link flow keeps going.
 */
export async function openExternalUrl(url: string): Promise<void> {
  if (import.meta.env.VITE_MOCK) {
    window.open(url, "_blank", "noopener,noreferrer");
    return;
  }
  try {
    const { open } = await import("@tauri-apps/plugin-shell");
    await open(url);
  } catch (err) {
    console.warn("[pyre-github] shell open failed — falling back to window.open:", err);
    window.open(url, "_blank", "noopener,noreferrer");
  }
}

/**
 * Begin a link attempt: start the device flow, open the verification URL, and
 * poll on the daemon-given interval. Idempotent guard against a double-start
 * (in-flight RPC OR an already-open link). On failure the status flips to
 * "error" and a toast explains it.
 */
export async function startGitHubLink(): Promise<void> {
  if (starting || getState().github.linking) {
    dlog("[pyre-github] startGitHubLink: ignored (already starting/linking)");
    return;
  }
  starting = true;
  try {
    const start = await githubDeviceStart();
    deadline = Date.now() + start.expires_in * 1000;
    setGithub({ linking: start, status: "linking" });
    dlog("[pyre-github] device start ok — code=", start.user_code);
    // Open the verification page; don't block polling on the opener.
    void openExternalUrl(start.verification_uri);
    startPolling(Math.max(1, start.interval));
  } catch (err) {
    console.error("github_device_start failed:", err);
    deadline = null;
    setGithub({ linking: null, status: "error" });
    toast("Couldn't start GitHub linking.", "error");
  } finally {
    starting = false;
  }
}

/** Cancel an open link attempt: stop polling, clear the modal, back to idle. */
export function cancelGitHubLink(): void {
  stopPolling();
  deadline = null;
  setGithub({ linking: null, status: "idle" });
}

/**
 * Forget the local token. This does NOT revoke the grant on GitHub — the account
 * menu's "Manage on GitHub" link sends the user to the server-side revoke page.
 */
export async function disconnectGitHub(): Promise<void> {
  setState({ ghMenuOpen: false });
  try {
    await githubDisconnect();
  } catch (err) {
    console.error("github_disconnect failed:", err);
    toast("Couldn't disconnect GitHub.", "error");
    return;
  }
  setGithub({ account: null, status: "idle" });
  toast("Disconnected GitHub.", "info");
}

/**
 * Load the linked account once on boot so a previously-linked account shows
 * immediately. Independent of daemon connectivity (GitHub is owned by the Tauri
 * layer, not pyred), so it runs whether or not the daemon is up. Best-effort —
 * a missing command or a transient error leaves the chip in its "Connect" state.
 */
export async function loadGitHubAccount(): Promise<void> {
  try {
    const account = await githubAccount();
    setGithub({ account, status: account ? "authorized" : "idle" });
  } catch (err) {
    console.warn("[pyre-github] initial account load failed:", err);
  }
}

// ── Account menu (topbar chip popover) ────────────────────────────────────────

export function toggleGhMenu(): void {
  setState({ ghMenuOpen: !getState().ghMenuOpen });
}

export function closeGhMenu(): void {
  if (getState().ghMenuOpen) setState({ ghMenuOpen: false });
}

// ── Polling internals ─────────────────────────────────────────────────────────

function startPolling(intervalSec: number): void {
  stopPolling();
  consecutivePollErrors = 0;
  pollTimer = window.setInterval(() => void pollOnce(), intervalSec * 1000);
}

function stopPolling(): void {
  if (pollTimer != null) {
    window.clearInterval(pollTimer);
    pollTimer = null;
  }
}

async function pollOnce(): Promise<void> {
  // Local expiry guard: if we're past the deadline, treat it as expired even if
  // a poll response is lagging — the device code is dead either way.
  if (deadline != null && Date.now() > deadline) {
    finishWithError("GitHub link expired.");
    return;
  }
  let res;
  try {
    res = await githubDevicePoll();
  } catch (err) {
    const msg = String(err);
    if (msg.includes("TOKEN_STORE_FAILED") || msg.includes("no device flow")) {
      // Terminal: GitHub authorized but we couldn't store the token locally, or the flow was lost.
      finishWithError(
        msg.includes("TOKEN_STORE_FAILED")
          ? "Couldn't save the GitHub token to ~/.config/pyre. Check the folder's permissions."
          : "GitHub link failed. Please try connecting again.",
      );
      return;
    }
    // Untagged transient error — keep retrying until the deadline or the backstop fires.
    consecutivePollErrors += 1;
    if (consecutivePollErrors >= POLL_ERROR_LIMIT) {
      finishWithError("GitHub link failed after repeated errors. Please try connecting again.");
      return;
    }
    console.warn("[pyre-github] device poll failed (will retry):", err);
    return;
  }
  // A response arrived — reset the transient-error backstop.
  consecutivePollErrors = 0;
  switch (res.status) {
    case "pending":
      return;
    case "authorized":
      await finishAuthorized();
      return;
    case "denied":
      finishWithError("GitHub authorization denied.");
      return;
    case "expired":
      finishWithError("GitHub link expired.");
      return;
  }
}

async function finishAuthorized(): Promise<void> {
  stopPolling();
  deadline = null;
  try {
    const account = await githubAccount();
    setGithub({
      account,
      linking: null,
      status: account ? "authorized" : "error",
    });
    if (account) toast(`Connected as @${account.login}.`, "success");
    else toast("Linked, but GitHub returned no account.", "error");
  } catch (err) {
    console.error("github_account failed after authorization:", err);
    setGithub({ linking: null, status: "error" });
    toast("Linked, but couldn't load your GitHub account.", "error");
  }
}

function finishWithError(message: string): void {
  stopPolling();
  deadline = null;
  setGithub({ linking: null, status: "error" });
  toast(message, "error");
}
