// Right panel: BLOCKS inspector for the focused pane. A search field (→ search_blocks)
// then real blocks newest-first as cards with hover actions [copy command, copy
// output, rerun]. Older blocks desaturate (recency cooling).

import { blockDurationMs, fmtDuration, h, replaceChildren } from "./dom";
import { icon } from "./icons";
import { getState } from "../state";
import { runBlockSearch } from "../actions";
import { blockStdout, sendKeys } from "../api";
import type { Block } from "../types";

export function renderBlocks(root: HTMLElement): void {
  const s = getState();
  root.classList.toggle("collapsed", s.rightCollapsed);
  if (s.rightCollapsed) {
    replaceChildren(root);
    return;
  }

  const header = h(
    "div",
    { class: "panel-header" },
    h("span", { class: "section-label" }, "Blocks"),
  );

  const search = h("div", { class: "block-search" }, searchInput(s.blockQuery));

  const listEl = h("div", { class: "block-list" });
  const items = s.searchResults ?? s.blocks;

  if (items.length === 0) {
    listEl.appendChild(
      h(
        "div",
        { class: "block-empty" },
        s.searchResults
          ? "No matching blocks."
          : "No blocks yet — run a command to see it here.",
      ),
    );
  } else {
    const total = items.length;
    items.forEach((b, i) => {
      // Recency cooling: newest = full color, older = progressively desaturated.
      const cool = total > 1 ? (i / (total - 1)) * 0.4 : 0;
      listEl.appendChild(blockCard(b, cool));
    });
  }

  replaceChildren(root, header, search, listEl);
}

function searchInput(value: string): HTMLElement {
  const input = h("input", {
    class: "block-search-input",
    type: "text",
    placeholder: "Search blocks…",
    value,
    spellcheck: false,
    oninput: (e: Event) => {
      void runBlockSearch((e.target as HTMLInputElement).value);
    },
  });
  return h(
    "div",
    { class: "block-search-wrap" },
    h("span", { class: "block-search-icon", html: icon("search") }),
    input,
  );
}

function blockCard(b: Block, cool: number): HTMLElement {
  const running = b.exit_code == null && b.ended_at == null;
  const failed = b.exit_code != null && b.exit_code !== 0;
  const statusClass = running ? "running" : failed ? "error" : "ok";

  const status = running
    ? h("span", { class: "block-status running" }, h("span", { html: icon("spinner") }), "running")
    : failed
      ? h(
          "span",
          { class: "block-status error" },
          h("span", { class: "block-status-icon", html: icon("cross") }),
          `exit ${b.exit_code}`,
        )
      : h(
          "span",
          { class: "block-status ok" },
          h("span", { class: "block-status-icon", html: icon("check") }),
          "exit 0",
        );

  const dur = fmtDuration(blockDurationMs(b.started_at, b.ended_at));

  const head = h(
    "div",
    { class: "block-head" },
    h("code", { class: "block-cmd" }, b.command || "(empty)"),
  );

  const meta = h(
    "div",
    { class: "block-meta" },
    status,
    dur && h("span", { class: "block-dur" }, dur),
  );

  const actions = h(
    "div",
    { class: "block-actions" },
    iconAction("Copy command", icon("copy"), () => {
      void navigator.clipboard?.writeText(b.command);
    }),
    iconAction("Copy output", icon("copy"), async () => {
      try {
        const text = await blockStdout(b.id);
        void navigator.clipboard?.writeText(text);
      } catch (err) {
        console.error("block_stdout failed:", err);
      }
    }),
    iconAction("Rerun in pane", icon("rerun"), () => {
      const bytes = Array.from(new TextEncoder().encode(b.command + "\n"));
      void sendKeys(b.pane, bytes).catch((e) =>
        console.error("rerun send_keys failed:", e),
      );
    }),
  );

  const card = h(
    "div",
    { class: `block-card ${statusClass}` },
    head,
    meta,
    actions,
  );
  card.style.setProperty("--cool", String(cool));
  return card;
}

function iconAction(
  title: string,
  glyph: string,
  onclick: () => void,
): HTMLElement {
  return h(
    "button",
    { class: "block-action", title, "aria-label": title, onclick },
    h("span", { html: glyph }),
  );
}
