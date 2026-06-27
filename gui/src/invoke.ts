// Swappable RPC transport seam.
//
// Production (VITE_MOCK unset): re-exports the REAL Tauri `invoke`/`listen`
// unchanged. `import.meta.env.VITE_MOCK` is statically replaced by Vite at build
// time, so the ternary constant-folds to the real binding and Rollup drops the
// mock module entirely — behavior is byte-for-byte identical to importing
// `@tauri-apps/api/*` directly.
//
// Dev (VITE_MOCK=1, via `pnpm dev:mock`): routes every RPC to an in-memory fake
// daemon (`./mock-invoke`) so the GUI runs in a plain browser with no Tauri host
// and no pyred. This is the visual feedback loop — and a faithful bisection tool,
// since calls still flow through the same `api.ts` wrappers the production app uses.

import { invoke as realInvoke } from "@tauri-apps/api/core";
import { listen as realListen } from "@tauri-apps/api/event";
import { mockInvoke, mockListen } from "./mock-invoke";

// Re-export the event type so callers (api.ts) depend only on this seam, never
// on `@tauri-apps/api/event` directly. Pure type re-export — erased at build.
export type { UnlistenFn } from "@tauri-apps/api/event";

// The casts bridge the mock's simplified signature to Tauri's overloaded one —
// the single seam where that mismatch is allowed to live. They go through
// `unknown` (never `any`) to keep the assertion explicit and contained.
export const invoke: typeof realInvoke = import.meta.env.VITE_MOCK
  ? (mockInvoke as unknown as typeof realInvoke)
  : realInvoke;

export const listen: typeof realListen = import.meta.env.VITE_MOCK
  ? (mockListen as unknown as typeof realListen)
  : realListen;
