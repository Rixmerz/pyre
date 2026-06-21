// Inline SVG glyphs. Stroke uses currentColor so they inherit text color.
// Kept tiny and trusted (no external data flows in here) so `html:` use is safe.

const svg = (paths: string, vb = "0 0 16 16"): string =>
  `<svg viewBox="${vb}" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${paths}</svg>`;

export const ICON = {
  // ◢ ember triangle wordmark mark (filled)
  ember: `<svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true"><path d="M2 14 L14 14 L14 2 Z" fill="currentColor"/></svg>`,
  chevronDown: svg(`<path d="M4 6 L8 10 L12 6"/>`),
  chevronUp: svg(`<path d="M4 10 L8 6 L12 10"/>`),
  chevronRight: svg(`<path d="M6 4 L10 8 L6 12"/>`),
  chevronLeft: svg(`<path d="M10 4 L6 8 L10 12"/>`),
  check: svg(`<path d="M3.5 8.5 L6.5 11.5 L12.5 4.5"/>`),
  cross: svg(`<path d="M4 4l8 8M12 4l-8 8"/>`),
  command: svg(
    `<path d="M5 3a2 2 0 1 0 0 4h6a2 2 0 1 0 0-4 2 2 0 0 0-2 2v6a2 2 0 1 0 2-2H5a2 2 0 1 0 2 2V5a2 2 0 0 0-2-2Z"/>`,
  ),
  theme: svg(`<circle cx="8" cy="8" r="6"/><path d="M8 2 a6 6 0 0 1 0 12 Z" fill="currentColor" stroke="none"/>`),
  settings: svg(
    `<circle cx="8" cy="8" r="2"/><path d="M8 1v2M8 13v2M1 8h2M13 8h2M3 3l1.4 1.4M11.6 11.6 13 13M13 3l-1.4 1.4M4.4 11.6 3 13"/>`,
  ),
  plus: svg(`<path d="M8 3v10M3 8h10"/>`),
  splitRight: svg(`<rect x="2" y="2.5" width="12" height="11" rx="1"/><path d="M8 2.5v11"/>`),
  splitDown: svg(`<rect x="2" y="2.5" width="12" height="11" rx="1"/><path d="M2 8h12"/>`),
  // ⊞ — the split-layout tab mark: a framed box quartered into panes.
  split: svg(`<rect x="2" y="2.5" width="12" height="11" rx="1"/><path d="M8 2.5v11M2 8h12"/>`),
  zoom: svg(`<path d="M3 6V3h3M13 6V3h-3M3 10v3h3M13 10v3h-3"/>`),
  close: svg(`<path d="M4 4l8 8M12 4l-8 8"/>`),
  search: svg(`<circle cx="7" cy="7" r="4.5"/><path d="M10.5 10.5 14 14"/>`),
  copy: svg(`<rect x="5" y="5" width="8" height="8" rx="1"/><path d="M3 11V3h8"/>`),
  rerun: svg(`<path d="M13 8a5 5 0 1 1-1.5-3.5M13 2v3h-3"/>`),
  rail: svg(`<rect x="2" y="2.5" width="12" height="11" rx="1"/><path d="M6 2.5v11"/>`),
  panel: svg(`<rect x="2" y="2.5" width="12" height="11" rx="1"/><path d="M10 2.5v11"/>`),
  // agent control plane — stacked rows with a leading dot (a "fleet" glyph)
  agents: svg(`<circle cx="3.5" cy="4" r="1.2" fill="currentColor" stroke="none"/><path d="M7 4h7"/><circle cx="3.5" cy="8" r="1.2" fill="currentColor" stroke="none"/><path d="M7 8h7"/><circle cx="3.5" cy="12" r="1.2" fill="currentColor" stroke="none"/><path d="M7 12h7"/>`),
  spinner: `<svg viewBox="0 0 16 16" width="13" height="13" class="spin" aria-hidden="true"><circle cx="8" cy="8" r="5.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-dasharray="20 12"/></svg>`,
} as const;

export type IconName = keyof typeof ICON;

/**
 * Resolve an icon glyph by name. Unlike a bare `ICON[name]` access, an unknown
 * key WARNS (so a typo or a missing glyph is visible in the console) instead of
 * silently feeding `undefined` into `h("span", { html })`, which renders blank.
 * Returns a visible placeholder box for unknown keys so the gap is also visible
 * in the UI, never an empty/invisible node.
 */
export function icon(name: string): string {
  const glyph = (ICON as Record<string, string>)[name];
  if (glyph == null) {
    console.warn(`icon("${name}"): unknown icon key — rendering placeholder`);
    return svg(`<rect x="2" y="2" width="12" height="12" rx="2"/><path d="M5 11 11 5M11 11 5 5"/>`);
  }
  return glyph;
}
