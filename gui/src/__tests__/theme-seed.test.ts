// Regression guard for the mock daemon's theme seed.
//
// Every edit to THEME_META or THEME_PALETTES in mock-invoke.ts must preserve:
//   • all 18 theme ids present in BOTH tables,
//   • every required palette token non-empty,
//   • ansi table exactly 16 entries,
//   • the 6 light themes marked kind "light".
//
// Tested through the real mockInvoke entrypoints (list_themes / get_theme) —
// the same path the GUI exercises at runtime. No internal constants are
// imported; no export was added.

import { describe, it, expect } from "vitest";
import { mockInvoke } from "../mock-invoke";
import type { ThemeMeta, ThemePalette } from "../types";

const EXPECTED_IDS = [
  "ember",
  "catppuccin-mocha",
  "catppuccin-latte",
  "tokyo-night",
  "tokyo-night-light",
  "gruvbox-dark",
  "gruvbox-light",
  "one-dark",
  "one-light",
  "solarized-dark",
  "solarized-light",
  "kanagawa",
  "rose-pine",
  "rose-pine-dawn",
  "vesper",
  "nord",
  "dracula",
  "terminal",
] as const;

const LIGHT_IDS = new Set<string>([
  "catppuccin-latte",
  "tokyo-night-light",
  "gruvbox-light",
  "one-light",
  "solarized-light",
  "rose-pine-dawn",
]);

/** String-valued keys of ThemePalette (excluding ansi which is an array). */
const PALETTE_STRING_KEYS: ReadonlyArray<keyof Omit<ThemePalette, "ansi" | "name" | "display_name">> = [
  "bg",
  "bg_dim",
  "fg",
  "fg_dim",
  "border",
  "border_focus",
  "cursor",
  "accent",
  "ok",
  "warn",
  "error",
];

describe("mock theme seed — list_themes and get_theme entrypoints", () => {
  it("list_themes returns exactly 18 entries", async () => {
    // Arrange + Act
    const meta = await mockInvoke<ThemeMeta[]>("list_themes");

    // Assert
    expect(meta).toHaveLength(18);
  });

  it("list_themes contains every expected theme id", async () => {
    // Arrange + Act
    const meta = await mockInvoke<ThemeMeta[]>("list_themes");
    const seededIds = new Set(meta.map((m) => m.name));

    // Assert — any missing id is a dropped or typo'd theme
    for (const id of EXPECTED_IDS) {
      expect(seededIds.has(id), `THEME_META is missing id "${id}"`).toBe(true);
    }
  });

  it("get_theme returns a palette entry for every expected id", async () => {
    // Assert — any missing id in THEME_PALETTES falls back to ember (bug)
    for (const id of EXPECTED_IDS) {
      const palette = await mockInvoke<ThemePalette>("get_theme", { name: id });
      expect(palette.name, `THEME_PALETTES missing entry for "${id}" (get_theme fell back to ember)`).toBe(id);
    }
  });

  it("every palette has all 11 required color tokens non-empty", async () => {
    for (const id of EXPECTED_IDS) {
      const palette = await mockInvoke<ThemePalette>("get_theme", { name: id });

      for (const key of PALETTE_STRING_KEYS) {
        const value = palette[key];
        expect(
          typeof value === "string" && value.length > 0,
          `palette["${id}"].${key} is missing or empty (got ${JSON.stringify(value)})`,
        ).toBe(true);
      }
    }
  });

  it("every palette ansi table has exactly 16 entries, all non-empty strings", async () => {
    for (const id of EXPECTED_IDS) {
      const palette = await mockInvoke<ThemePalette>("get_theme", { name: id });

      expect(
        palette.ansi,
        `palette["${id}"].ansi must have 16 entries`,
      ).toHaveLength(16);

      palette.ansi.forEach((entry, i) => {
        expect(
          typeof entry === "string" && entry.length > 0,
          `palette["${id}"].ansi[${i}] is empty or missing`,
        ).toBe(true);
      });
    }
  });

  it("the 6 light themes are marked kind 'light' and the remaining 12 are 'dark'", async () => {
    const meta = await mockInvoke<ThemeMeta[]>("list_themes");
    const byId = new Map(meta.map((m) => [m.name, m]));

    for (const id of EXPECTED_IDS) {
      const entry = byId.get(id);
      expect(entry, `THEME_META is missing entry for "${id}"`).toBeDefined();

      const expectedKind: ThemeMeta["kind"] = LIGHT_IDS.has(id) ? "light" : "dark";
      expect(
        entry?.kind,
        `theme "${id}" should be kind "${expectedKind}" but got "${entry?.kind}"`,
      ).toBe(expectedKind);
    }
  });
});
