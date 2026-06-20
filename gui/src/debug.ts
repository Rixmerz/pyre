// Debug logger — gated so trace noise stays OUT of the release build.
//
// During the marathon, high-frequency trace logs ([pyre-input], [pyre-render],
// [pyre-split], [pyre-session], …) were sprinkled through the input, render and
// session paths. They're invaluable when chasing a focus-loss or a re-parent
// bug, and useless (and noisy) in a shipped build. `dlog` keeps them in the
// source but silent unless explicitly switched on:
//
//   - In dev (`vite`), import.meta.env.DEV is true → logs are on.
//   - In a release build they're off, UNLESS the user flips the switch from the
//     devtools console:  localStorage.setItem("pyre-debug", "1")  (reload).
//
// Genuine failures (console.error) and real degradation warnings (console.warn)
// are NOT routed through here — they always fire.

function debugEnabled(): boolean {
  if (import.meta.env.DEV) return true;
  try {
    return localStorage.getItem("pyre-debug") === "1";
  } catch {
    // localStorage can throw in locked-down webviews — treat as "off".
    return false;
  }
}

// Evaluated once at module load. import.meta.env.DEV is a compile-time constant,
// so in a release build the whole branch dead-code-eliminates to `false` and the
// localStorage flag is the only way back in.
const DEBUG = debugEnabled();

/** Gated debug log. No-op in release builds unless `pyre-debug` is set to "1". */
export const dlog = (...args: unknown[]): void => {
  if (DEBUG) console.log(...args);
};
