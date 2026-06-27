/// <reference types="vite/client" />

// Dev-only mock-daemon flag. Set by `pnpm dev:mock` (VITE_MOCK=1) to route every
// RPC through the in-memory fake daemon (src/mock-invoke.ts) instead of Tauri.
// Typed here so `import.meta.env.VITE_MOCK` is `string | undefined`, not `any`.
interface ImportMetaEnv {
  readonly VITE_MOCK?: string;
}

// Raw text imports (e.g. inline SVG): `import logo from "./logo.svg?raw"`.
declare module "*?raw" {
  const content: string;
  export default content;
}

// URL imports (asset path string): `import iconUrl from "./icon.png?url"`.
declare module "*?url" {
  const url: string;
  export default url;
}
