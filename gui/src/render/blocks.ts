// Right panel: BLOCKS inspector for the focused pane. A search field (→ search_blocks)
// then real blocks newest-first as cards. Each card carries hover actions
// [copy command, copy output, rerun, collapse/expand output] and a status glyph
// by exit code (ok / err / running). A "failures only" toggle in the panel
// header filters to non-zero-exit blocks. Older blocks desaturate (recency cooling).

import { blockDurationMs, fmtDuration, h, replaceChildren } from "./dom";
import { icon } from "./icons";
import { getState, setState } from "../state";
import { runBlockSearch, toggleBlockExpanded, toggleFailuresOnly, rerunBlock } from "../actions";
import { blockStdout } from "../api";
import type { Block } from "../types";

/**
 * Lazy cache of fetched block output, keyed by block id. Populated on first
 * expand so re-rendering the panel (every poll tick) doesn't re-fetch. Lives at
 * module scope — the store stays serialisable.
 */
const outputCache = new Map<string, string>();

function blockFailed(b: Block): boolean {
  return b.exit_code != null && b.exit_code !== 0;
}

export function renderBlocks(root: HTMLElement): void {
  const s = getState();
  root.classList.toggle("collapsed", s.rightCollapsed);
  if (s.rightCollapsed) {
    replaceChildren(root);
    return;
  }

  const failBtn = h(
    "button",
    {
      class: "block-filter" + (s.blocksFailuresOnly ? " active" : ""),
      title: s.blocksFailuresOnly
        ? "Showing failures only — click to show all"
        : "Show failures only",
      "aria-pressed": s.blocksFailuresOnly,
      onclick: () => toggleFailuresOnly(),
    },
    h("span", { class: "block-filter-icon", html: icon("cross") }),
    h("span", { class: "block-filter-label" }, "Failures"),
  );

  const header = h(
    "div",
    { class: "panel-header" },
    h("span", { class: "section-label" }, "Blocks"),
    failBtn,
  );

  const search = h("div", { class: "block-search" }, searchInput(s.blockQuery));

  const listEl = h("div", { class: "block-list" });

  // Source: search results when searching, else live blocks. Apply the
  // failures-only filter client-side on top (works for both sources).
  let items = s.searchResults ?? s.blocks;
  if (s.blocksFailuresOnly) items = items.filter(blockFailed);

  if (items.length === 0) {
    const msg = s.blocksFailuresOnly
      ? "No failed blocks."
      : s.searchResults
        ? "No matching blocks."
        : "No blocks yet — run a command to see it here.";
    listEl.appendChild(h("div", { class: "block-empty" }, msg));
  } else {
    const total = items.length;
    items.forEach((b, i) => {
      // Recency cooling: newest = full color, older = progressively desaturated.
      const cool = total > 1 ? (i / (total - 1)) * 0.4 : 0;
      listEl.appendChild(blockCard(b, cool, s.expandedBlocks.has(b.id)));
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

function blockCard(b: Block, cool: number, expanded: boolean): HTMLElement {
  const running = b.exit_code == null && b.ended_at == null;
  const failed = blockFailed(b);
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
        const text = outputCache.get(b.id) ?? (await blockStdout(b.id));
        outputCache.set(b.id, text);
        void navigator.clipboard?.writeText(text);
      } catch (err) {
        console.error("block_stdout failed:", err);
      }
    }),
    iconAction("Rerun in pane", icon("rerun"), () => rerunBlock(b.pane, b.command)),
    iconAction(
      expanded ? "Collapse output" : "Expand output",
      icon(expanded ? "chevronUp" : "chevronDown"),
      () => toggleBlockExpanded(b.id),
    ),
  );

  const card = h(
    "div",
    { class: `block-card ${statusClass}` + (expanded ? " expanded" : "") },
    head,
    meta,
    actions,
  );

  if (expanded) card.appendChild(outputBlock(b));

  card.style.setProperty("--cool", String(cool));
  return card;
}

/**
 * The expanded output preview. Renders cached output immediately; otherwise
 * shows a loading line and fetches `block_stdout` once, then patches the <pre>
 * in place (no full re-render — the panel re-renders on its own poll cadence,
 * but the cache means the fetched text survives that).
 */
function outputBlock(b: Block): HTMLElement {
  const pre = h("pre", { class: "block-output" });
  const cached = outputCache.get(b.id);
  if (cached != null) {
    pre.textContent = cached || "(no output)";
  } else {
    pre.textContent = "Loading output…";
    blockStdout(b.id)
      .then((text) => {
        outputCache.set(b.id, text);
        // Patch only if still expanded and this <pre> is still in the document.
        if (getState().expandedBlocks.has(b.id) && pre.isConnected) {
          pre.textContent = text || "(no output)";
        } else if (!pre.isConnected) {
          // A later render replaced the node; nudge a re-render so the now-cached
          // text paints on the next pass.
          setState({});
        }
      })
      .catch((err) => {
        console.error("block_stdout failed:", err);
        if (pre.isConnected) pre.textContent = "(failed to load output)";
      });
  }
  return pre;
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
