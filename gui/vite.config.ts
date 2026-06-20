import { defineConfig } from "vite";

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
  },
});
