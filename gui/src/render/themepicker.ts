// Theme picker popover: a grid of swatches (accent over bg) for every theme from
// list_themes. Selecting calls selectTheme → get_theme → writes CSS vars.

import { h, replaceChildren } from "./dom";
import { getState } from "../state";
import { closeThemePicker } from "../actions";
import { selectTheme } from "../themes";

export function renderThemePicker(root: HTMLElement): void {
  const s = getState();
  if (!s.themePickerOpen) {
    replaceChildren(root);
    root.classList.remove("open");
    return;
  }
  root.classList.add("open");

  const grid = h("div", { class: "theme-grid" });
  for (const t of s.themes) {
    const swatch = h(
      "button",
      {
        class: "theme-swatch" + (t.name === s.activeTheme ? " active" : ""),
        title: `${t.display_name} (${t.kind})`,
        onclick: () => void selectTheme(t.name),
      },
      h("span", {
        class: "swatch-chip",
        style: `background:${t.bg};`,
      }),
      h("span", {
        class: "swatch-accent",
        style: `background:${t.accent};`,
      }),
      h("span", { class: "swatch-name" }, t.display_name),
    );
    // Paint the chip accent via inline style children created above.
    grid.appendChild(swatch);
  }

  if (s.themes.length === 0) {
    grid.appendChild(h("div", { class: "theme-empty" }, "No themes available."));
  }

  const modal = h(
    "div",
    { class: "theme-modal", onclick: (e: Event) => e.stopPropagation() },
    h(
      "div",
      { class: "theme-header" },
      h("span", { class: "section-label" }, "Themes"),
    ),
    grid,
  );

  const backdrop = h(
    "div",
    { class: "theme-backdrop", onclick: () => closeThemePicker() },
    modal,
  );

  replaceChildren(root, backdrop);
}
