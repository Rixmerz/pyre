// Theme system. Selecting a theme calls get_theme(name) and writes the returned
// palette into CSS custom properties on :root, reskinning the chrome. The
// --heat-* ramp is deliberately NOT touched here — agent-state heat is a fixed
// visual language that must read the same under every theme.

import { getTheme, listThemes } from "./api";
import { getState, setState } from "./state";
import { restyleAll } from "./terminals";
import type { ThemePalette } from "./types";

const THEME_STORAGE_KEY = "pyre.theme";

/**
 * Map a daemon ThemePalette onto the chrome CSS vars. The daemon's palette
 * fields are semantic (bg, fg, accent, border, …); we route each to the GUI
 * token it drives. Heat vars are intentionally absent — they stay put.
 */
function applyPalette(p: ThemePalette): void {
  const root = document.documentElement.style;
  // Surfaces — derive a 3-step depth ramp from bg / bg_dim.
  root.setProperty("--surface-0", p.bg);
  root.setProperty("--surface-1", p.bg_dim);
  root.setProperty("--surface-2", mix(p.bg_dim, p.fg, 0.06));
  root.setProperty("--term-bg", p.bg);

  // Lines.
  root.setProperty("--border", p.border);
  root.setProperty("--hairline", mix(p.border, p.fg, 0.12));

  // Text.
  root.setProperty("--fg", p.fg);
  root.setProperty("--fg-muted", p.fg_dim);
  root.setProperty("--fg-faint", mix(p.fg_dim, p.bg, 0.45));

  // Accent / ember family — the theme's accent becomes the ember chrome accent.
  root.setProperty("--ember", p.accent);
  root.setProperty("--ember-bright", mix(p.accent, "#ffffff", 0.25));
  root.setProperty("--ember-deep", mix(p.accent, "#000000", 0.3));
  root.setProperty("--focus", p.border_focus || p.accent);

  // Status semantics.
  root.setProperty("--ok", p.ok);
  root.setProperty("--warn", p.warn);
  root.setProperty("--error", p.error);

  // Re-theme any live xterm instances to match.
  restyleAll();
}

/** Apply a theme by name; persists the choice and updates state. */
export async function selectTheme(name: string): Promise<void> {
  try {
    const palette = await getTheme(name);
    applyPalette(palette);
    localStorage.setItem(THEME_STORAGE_KEY, name);
    setState({ activeTheme: name, themePickerOpen: false });
  } catch (err) {
    console.error(`get_theme(${name}) failed:`, err);
    setState({ themePickerOpen: false });
  }
}

/** Load the theme list and apply the persisted (or default) theme on boot. */
export async function initThemes(): Promise<void> {
  try {
    const themes = await listThemes();
    setState({ themes });
  } catch (err) {
    console.error("list_themes failed:", err);
  }
  const stored = localStorage.getItem(THEME_STORAGE_KEY) ?? "ember";
  await selectTheme(stored).catch(() => {
    /* keep the CSS-default ember palette already in styles.css */
    setState({ activeTheme: stored });
  });
}

/** Toggle between the active theme and its light/dark counterpart, if listed. */
export async function toggleLightDark(): Promise<void> {
  const s = getState();
  const current = s.themes.find((t) => t.name === s.activeTheme);
  if (!current) return;
  const want = current.kind === "dark" ? "light" : "dark";
  const counterpart = s.themes.find((t) => t.kind === want);
  if (counterpart) await selectTheme(counterpart.name);
}

// ── color helpers ───────────────────────────────────────────────────────────

/** Mix two hex colors by ratio t (0 = a, 1 = b). Returns "#rrggbb". */
function mix(a: string, b: string, t: number): string {
  const ca = hex(a);
  const cb = hex(b);
  if (!ca || !cb) return a;
  const r = Math.round(ca[0] + (cb[0] - ca[0]) * t);
  const g = Math.round(ca[1] + (cb[1] - ca[1]) * t);
  const bl = Math.round(ca[2] + (cb[2] - ca[2]) * t);
  return `#${[r, g, bl].map((n) => n.toString(16).padStart(2, "0")).join("")}`;
}

/** Parse "#rgb" / "#rrggbb" → [r,g,b], or null if unparseable. */
function hex(s: string): [number, number, number] | null {
  let h = s.trim().replace(/^#/, "");
  if (h.length === 3) h = h.split("").map((c) => c + c).join("");
  if (h.length !== 6) return null;
  const n = parseInt(h, 16);
  if (Number.isNaN(n)) return null;
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}
