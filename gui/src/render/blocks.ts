// Right panel: BLOCKS inspector for the focused pane. A search field (→ search_blocks)
// then real blocks newest-first as cards. Each card carries hover actions
// [copy command, copy output, rerun, collapse/expand output] and a status glyph
// by exit code (ok / err / running). A "failures only" toggle in the panel
// header filters to non-zero-exit blocks. Older blocks desaturate (recency cooling).

import { blockDurationMs, fmtDuration, h, replaceChildren } from "./dom";
import { icon } from "./icons";
import { getState, setState, type AppState } from "../state";
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

// Persistent panel shell refs. The search <input> is created ONCE and reused
// across renders — poll ticks AND keystrokes — so the browser keeps its caret,
// focus and value. renderBlocks rebuilds ONLY the header contents and the block
// list in place, never the input. This prevents the reversed-characters bug:
// typing fires runBlockSearch → setState → renderAll → renderBlocks, and the
// old code rebuilt the whole panel (a fresh <input>) on every keystroke, which
// destroyed the live input node and reset the caret to index 0. Refs are
// dropped when the panel collapses so a re-expand builds a fresh input.
let shellRoot: HTMLElement | null = null;
let searchInputEl: HTMLInputElement | null = null;
let headerEl: HTMLElement | null = null;
let listEl: HTMLElement | null = null;

// Last fingerprints that triggered a header / list rebuild. The header tracks
// only the failures-only toggle; the list tracks the focused pane + each block's
// STABLE fields (id, status, exit code, command, expanded) but NOT the volatile
// elapsed time of a running block — that ticks every poll and is updated in
// place by applyBlockElapsedInPlace instead, so a running block no longer forces
// a full list replaceChildren (the source of the "never finishes loading"
// flicker). Reset to "" in buildShell so a fresh shell always paints once.
let lastHeaderFp = "";
let lastListFp = "";

export function renderBlocks(root: HTMLElement): void {
  const s = getState();
  root.classList.toggle("collapsed", s.rightCollapsed);
  if (s.rightCollapsed) {
    replaceChildren(root);
    // Drop refs so the next expand rebuilds a fresh input.
    shellRoot = null;
    searchInputEl = null;
    headerEl = null;
    listEl = null;
    return;
  }

  // Build the shell (header host + persistent search input + list host) only
  // when it doesn't yet exist for this root. Keystrokes never reach here — they
  // re-render the header + list only — so the live <input> survives intact.
  if (searchInputEl === null || shellRoot !== root) buildShell(root, s);

  renderHeader(s);

  // Keep the input's value in sync with state WITHOUT clobbering the caret: only
  // write when the field isn't focused (e.g. an external reset of blockQuery),
  // never mid-keystroke (the user's own value is already in the field).
  const input = searchInputEl;
  if (input && document.activeElement !== input && input.value !== s.blockQuery) {
    input.value = s.blockQuery;
  }

  renderList(s);
}

/** Build the panel shell with the persistent search input. Created once per open. */
function buildShell(root: HTMLElement, s: Readonly<AppState>): void {
  const header = h("div", { class: "panel-header" });

  const input = h("input", {
    class: "block-search-input",
    type: "text",
    placeholder: "Search blocks…",
    value: s.blockQuery,
    spellcheck: false,
    oninput: (e: Event) => {
      void runBlockSearch((e.target as HTMLInputElement).value);
    },
  }) as HTMLInputElement;

  const search = h(
    "div",
    { class: "block-search" },
    h(
      "div",
      { class: "block-search-wrap" },
      h("span", { class: "block-search-icon", html: icon("search") }),
      input,
    ),
  );

  const list = h("div", { class: "block-list" });

  replaceChildren(root, header, search, list);

  shellRoot = root;
  searchInputEl = input;
  headerEl = header;
  listEl = list;
  // Fresh, empty header + list hosts — force the next renderHeader/renderList to
  // paint once rather than skip on a stale fingerprint left from a prior shell.
  lastHeaderFp = "";
  lastListFp = "";
}

/** Render the panel header (section label + failures toggle) into its host. */
function renderHeader(s: Readonly<AppState>): void {
  const host = headerEl;
  if (!host) return;

  // The header's only stateful bit is the failures-only toggle. Skip the rebuild
  // (which would recreate the toggle button and flicker its hover state) on the
  // common no-op poll tick where that flag is unchanged.
  const fp = s.blocksFailuresOnly ? "1" : "0";
  if (fp === lastHeaderFp && host.childElementCount > 0) return;
  lastHeaderFp = fp;

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

  replaceChildren(host, h("span", { class: "section-label" }, "Blocks"), failBtn);
}

/**
 * Canonical string of the rendered card list — focused pane + filter/search mode
 * + per block its STABLE fields (id, status, exit code, command, expanded). The
 * running-block elapsed time is deliberately excluded (it ticks every poll and is
 * patched in place by applyBlockElapsedInPlace), so a no-op tick keeps the same
 * string and skips the rebuild — killing the per-poll flicker. A genuine change
 * (block finishes, new/closed block, expand toggled, focus moves) changes the
 * string and forces a rebuild.
 */
function listFingerprint(s: Readonly<AppState>, items: readonly Block[]): string {
  const focused = s.focusedPane ?? "";
  const mode = `${s.blocksFailuresOnly ? 1 : 0}${s.searchResults ? 1 : 0}`;
  const parts = items.map((b) => {
    const running = b.exit_code == null && b.ended_at == null;
    const status = running ? "run" : String(b.exit_code ?? "?");
    const expanded = s.expandedBlocks.has(b.id) ? "1" : "0";
    return `${b.id}~${status}~${expanded}~${b.command}`;
  });
  return `${focused}|${mode}|[${parts.join("\n")}]`;
}

/** Render the filtered block cards into the persistent list container only. */
function renderList(s: Readonly<AppState>): void {
  const list = listEl;
  if (!list) return;

  // Source: search results when searching, else live blocks. Apply the
  // failures-only filter client-side on top (works for both sources).
  let items = s.searchResults ?? s.blocks;
  if (s.blocksFailuresOnly) items = items.filter(blockFailed);

  const fp = listFingerprint(s, items);
  if (fp === lastListFp && list.childElementCount > 0) {
    // Nothing structural changed; only a running block's elapsed time may have
    // advanced — applyBlockElapsedInPlace (render/index.ts) patches that text
    // node in place. Skip the replaceChildren tear-down.
    return;
  }
  lastListFp = fp;

  if (items.length === 0) {
    const msg = s.blocksFailuresOnly
      ? "No failed blocks."
      : s.searchResults
        ? "No matching blocks."
        : "No blocks yet — run a command to see it here.";
    replaceChildren(list, h("div", { class: "block-empty" }, msg));
    return;
  }

  const total = items.length;
  const cards = items.map((b, i) => {
    // Recency cooling: newest = full color, older = progressively desaturated.
    const cool = total > 1 ? (i / (total - 1)) * 0.4 : 0;
    return blockCard(b, cool, s.expandedBlocks.has(b.id));
  });
  replaceChildren(list, ...cards);
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
    {
      class: `block-card ${statusClass}` + (expanded ? " expanded" : ""),
      "data-block": b.id,
    },
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

/**
 * Advance the elapsed-time text of each RUNNING block's card IN-PLACE — no
 * rebuild. Mirrors applyHeatInPlace / applyFocusInPlace: query the live cards by
 * data-block and patch only the `.block-dur` text node. Because elapsed time is
 * excluded from listFingerprint, a no-op poll tick skips the list rebuild; this
 * keeps the running clock advancing without recreating the card (which was the
 * per-poll flicker). Called from render/index.ts after every render so it lands
 * even when renderList early-returns on an unchanged fingerprint. Idempotent —
 * it only writes when the formatted text actually changed.
 */
export function applyBlockElapsedInPlace(): void {
  const s = getState();
  if (s.rightCollapsed) return;
  let items = s.searchResults ?? s.blocks;
  if (s.blocksFailuresOnly) items = items.filter(blockFailed);

  for (const b of items) {
    const running = b.exit_code == null && b.ended_at == null;
    if (!running) continue;
    const card = document.querySelector<HTMLElement>(
      `.block-card[data-block="${CSS.escape(b.id)}"]`,
    );
    if (!card) continue;
    const durEl = card.querySelector<HTMLElement>(".block-dur");
    if (!durEl) continue;
    const dur = fmtDuration(blockDurationMs(b.started_at, b.ended_at));
    if (durEl.textContent !== dur) durEl.textContent = dur;
  }
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
