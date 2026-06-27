import { defineConfig } from "vite";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL(".", import.meta.url));

// Tauri spike Vite config. Fixed port 1420 matches tauri.conf.json devUrl.
export default defineConfig({
  clearScreen: false,
  server: {
    // Bind IPv4 loopback explicitly. Default "localhost" resolved to IPv6
    // [::1] only on this host, so browsers hitting 127.0.0.1 got connection
    // refused even though the server was up. 127.0.0.1 is what browsers
    // default to for "localhost", keeping http://localhost:1420/ reachable.
    host: "127.0.0.1",
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
