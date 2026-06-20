import { defineConfig } from "vite";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL(".", import.meta.url));

// Tauri spike Vite config. Fixed port 1420 matches tauri.conf.json devUrl.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  // Produce relative asset paths so the bundled webview can load them.
  build: {
    target: "esnext",
    minify: false,
    sourcemap: true,
    rollupOptions: {
      // Multi-page build: the main app AND the frameless splash window. Both
      // .html files are emitted to dist/ so the Tauri windows (main → index.html,
      // splashscreen → splashscreen.html) resolve against frontendDist.
      input: {
        main: resolve(root, "index.html"),
        splashscreen: resolve(root, "splashscreen.html"),
      },
    },
  },
});
