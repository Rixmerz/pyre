// Center: renders the active session's layout tree as nested flex split-panes.
// Each leaf is a pane CARD: slim header (heat dot + title) with a hover toolbar
// [split down, split right, zoom, close], an xterm body, and a heat edge.
// The focused pane gets a soft outer glow; waiting/crashed panes pulse.
//
// STRUCTURAL vs HEAT-ONLY rendering
// ──────────────────────────────────
// The 750ms poll loop calls reloadPaneStates → setState → notify → renderCenter
// on every tick. A full replaceChildren tears out the DOM including the xterm
// subtree, causing mountPaneTerminal to re-parent the terminal DOM node, which
// fires blur on the hidden textarea and silently kills keyboard input.
//
// To avoid this, renderCenter tracks a "layout fingerprint" — a canonical string
// of the leaf pane IDs in tree order and the zoomed pane. When only heat/state OR
// focus changes (paneStates updated or focusedPane moved, layout fingerprint
// unchanged) the function returns early: heat is repainted in-place in
// session-ops.ts and the `.focused` glow is moved in-place via applyFocusInPlace
// (called from render/index.ts). Full replaceChildren only runs when the layout
// structure truly changes (pane added/removed/split/weight or zoom changed).
//
// Focus is deliberately NOT in the fingerprint: when it was, every click / Ctrl+
// HJKL focus move re-mounted every xterm and re-flashed every card, and the
// `.focused` transition never played (it was applied to a newborn node).

import { h, replaceChildren } from "./dom";
import { icon } from "./icons";
import { attachRenameAffordance } from "./inline-edit";
import {
  getState,
  paneStateOf,
  paneDisplayName,
  windowTabs,
  activeWindowOf,
} from "../state";
import { heatVar, pulses, stateLabel } from "../heat";
import {
  closePaneAction,
  focusPane,
  newSession,
  renamePaneAction,
  splitDown,
  splitRight,
  zoomPane,
} from "../actions";
import { mountPaneTerminal, refitAll } from "../terminals";
import { setWeight } from "../api";
import { chipKind } from "./agents";
import { dlog } from "../debug";
import { toast } from "../toast";
import type { LayoutNode, PaneState, WindowInfo } from "../types";

/** First leaf pane id under a node — the representative weight target. */
function firstLeaf(node: LayoutNode): string {
  return node.kind === "leaf" ? node.pane : firstLeaf(node.children[0]!);
}

/**
 * Canonical string of the rendered structure: EVERY window's layout tree + the
 * active window + zoom. Changes to ANY of these force a structural re-render; a
 * heat- OR focus-only tick (same structure) skips it so xterms survive and the
 * `.focused` glow transition can play (focus is applied in-place — see
 * applyFocusInPlace + render/index.ts). Focus is intentionally excluded.
 */
function windowsFingerprint(
  windows: WindowInfo[],
  activeWindowId: string | null,
  layouts: Map<string, LayoutNode>,
  zoomedPane: string | null,
  connected: boolean,
): string {
  if (!connected) return "__disconnected__";
  if (windows.length === 0) return "__no-windows__";
  const parts = windows.map((w) => {
    const layout = layouts.get(w.id);
    return `${w.id}=${layout ? fingerprintNode(layout) : "0"}`;
  });
  const zoom = zoomedPane ? `:zoom:${zoomedPane}` : "";
  return `wins:[${parts.join("|")}]:active:${activeWindowId ?? ""}${zoom}`;
}

function fingerprintNode(node: LayoutNode): string {
  if (node.kind === "leaf") return node.pane;
  const childStr = node.children.map(fingerprintNode).join(",");
  return `(${node.dir}:[${childStr}])`;
}

/** Last fingerprint that triggered a full structural re-render. */
let lastFingerprint = "";

export function renderCenter(root: HTMLElement): void {
  const s = getState();
  const session = s.activeSession;
  const windows = windowTabs(session);
  const activeWin = activeWindowOf(session);

  const fp = windowsFingerprint(
    windows,
    activeWin,
    s.layouts,
    s.zoomedPane,
    s.connected,
  );

  if (fp === lastFingerprint) {
    // Window set, every tree, active window, and zoom are all unchanged. Heat/
    // state updates are applied in-place by session-ops.ts and the focus glow by
    // applyFocusInPlace (render/index.ts) — skip the full tear-down to prevent
    // xterm re-parenting and focus-blur.
    dlog("[pyre-render] skip structural re-render — fingerprint unchanged");
    return;
  }

  dlog("[pyre-render] structural re-render — fingerprint changed:", lastFingerprint, "→", fp);
  lastFingerprint = fp;

  if (!s.connected) {
    replaceChildren(root, daemonDownPanel());
    return;
  }
  if (windows.length === 0) {
    dlog("[pyre-session] rendered empty-state (activeSession=", s.activeSession, ")");
    replaceChildren(
      root,
      h(
        "div",
        { class: "center-empty" },
        h("p", {}, "No active session."),
        h(
          "button",
          { class: "btn primary", onclick: () => void newSession() },
          "New session",
        ),
      ),
    );
    return;
  }

  // Render EVERY window's tree into its own view; only the active window's view
  // is visible (display:flex), the rest are display:none. This keeps hidden
  // windows' terminals mounted and buffering output rather than disposing them
  // on a window switch.
  const views: HTMLElement[] = [];
  for (const w of windows) {
    const isActive = w.id === activeWin;
    const layout = s.layouts.get(w.id);
    let inner: HTMLElement;
    if (!layout) {
      inner = h("div", { class: "center-empty" }, h("p", {}, "Loading…"));
    } else if (s.zoomedPane && isActive) {
      // Zoom applies to the active window: render only the zoomed pane full-bleed.
      inner = renderNode({ kind: "leaf", pane: s.zoomedPane });
    } else {
      inner = renderNode(layout);
    }
    views.push(tabView(w.id, isActive, inner));
  }

  replaceChildren(root, ...views);
}

/**
 * Move the `.focused` glow to the active pane IN-PLACE — no structural rebuild.
 * Mirrors applyHeatInPlace (session-ops.ts): query the live pane-card DOM keyed
 * by data-pane and toggle `.focused` so only state.focusedPane's card carries it.
 * Because the card node is NOT replaced (focus was dropped from the structural
 * fingerprint), the CSS box-shadow/border transition on `.pane-card.focused`
 * actually plays. Called from render/index.ts after renderCenter on EVERY render
 * so it lands even when renderCenter early-returns on an unchanged fingerprint.
 * Idempotent: re-running with the same focus is a no-op (classList.toggle with an
 * explicit boolean).
 */
export function applyFocusInPlace(): void {
  const focused = getState().focusedPane;
  document
    .querySelectorAll<HTMLElement>(".pane-card[data-pane]")
    .forEach((card) => {
      card.classList.toggle("focused", card.dataset["pane"] === focused);
    });
}

/** Wrap a tab's content in a view container that's only shown when active. */
function tabView(key: string, active: boolean, inner: HTMLElement): HTMLElement {
  const view = h("div", { class: "tab-view", "data-tab": key });
  // display:none (not removal) so hidden tabs keep their mounted xterms alive
  // and buffering output. The active view fills the pane area via flex.
  view.style.display = active ? "flex" : "none";
  view.appendChild(inner);
  return view;
}

function renderNode(node: LayoutNode): HTMLElement {
  if (node.kind === "leaf") return renderLeaf(node.pane);

  // Daemon wire convention (see pyre-proto layout.rs + bridge layout_to_dto):
  //   dir "v" = VSplit = side-by-side columns  → flex-direction: row    (.split-h)
  //   dir "h" = HSplit = top-to-bottom stack   → flex-direction: column (.split-v)
  // (Rust names the split by its AXIS; CSS names it by child arrangement —
  // same letters, opposite meaning. Map explicitly to avoid the inversion.)
  const horizontal = node.dir === "v"; // row layout → vertical divider drags width
  const el = h("div", {
    class: "split " + (horizontal ? "split-h" : "split-v"),
  });

  const wraps: HTMLElement[] = [];
  node.children.forEach((child, i) => {
    const wrap = h("div", { class: "split-child" });
    const w = node.weights?.[i] ?? 50;
    // flex-grow + flex-shrink:1 + flex-basis:0% so weighted children divide the
    // parent box by weight without sizing to (zero) content. min-width/height:0
    // are the load-bearing pair: a flex child defaults to min-*:auto and refuses
    // to shrink below content, which collapses siblings to 0 down the recursion.
    wrap.style.flex = `${w} 1 0%`;
    wrap.style.minWidth = "0";
    wrap.style.minHeight = "0";
    wrap.appendChild(renderNode(child));
    wraps.push(wrap);

    // Insert a draggable divider BEFORE every child except the first, splitting
    // the boundary between child i-1 and child i.
    if (i > 0) {
      el.appendChild(
        divider(node, i - 1, i, wraps[i - 1]!, wrap, horizontal),
      );
    }
    el.appendChild(wrap);
  });
  return el;
}

/**
 * A draggable boundary between two sibling split children. Dragging recomputes
 * the two children's weights from their live on-screen pixel sizes, updates the
 * flex-grow inline style immediately (so the resize tracks the cursor), and
 * commits the new weights to the daemon via set_weight on each represented leaf.
 */
function divider(
  node: LayoutNode & { kind: "split" },
  leftIdx: number,
  rightIdx: number,
  leftWrap: HTMLElement,
  rightWrap: HTMLElement,
  horizontal: boolean,
): HTMLElement {
  const handle = h("div", {
    class: "split-divider " + (horizontal ? "vert" : "horiz"),
    role: "separator",
    "aria-orientation": horizontal ? "vertical" : "horizontal",
    title: "Drag to resize",
  });

  handle.addEventListener("mousedown", (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();

    const lr = leftWrap.getBoundingClientRect();
    const rr = rightWrap.getBoundingClientRect();
    const startPos = horizontal ? e.clientX : e.clientY;
    const leftSize = horizontal ? lr.width : lr.height;
    const rightSize = horizontal ? rr.width : rr.height;
    const totalSize = leftSize + rightSize;
    // Preserve the SUM of the two weights so siblings outside this pair are
    // unaffected; only the ratio between this pair changes.
    const leftW0 = node.children[leftIdx] && node.weights?.[leftIdx] != null
      ? node.weights[leftIdx]!
      : 50;
    const rightW0 = node.children[rightIdx] && node.weights?.[rightIdx] != null
      ? node.weights[rightIdx]!
      : 50;
    const weightSum = leftW0 + rightW0;

    const MIN_PX = 60; // don't let a pane collapse below this
    let lastLeftW = leftW0;
    let lastRightW = rightW0;

    document.body.classList.add(horizontal ? "resizing-col" : "resizing-row");

    // Coalesce terminal refits to ONE per animation frame while dragging so the
    // xterm grid reflows smoothly with the divider instead of stretching/clipping
    // and snapping only on mouseup. xterm's fit emits a resize_pane only when the
    // char grid actually changes, so sub-cell pixel moves don't spam the daemon.
    let pendingRefit = 0;
    const scheduleRefit = (): void => {
      if (pendingRefit !== 0) return; // a refit is already queued for next frame
      pendingRefit = requestAnimationFrame(() => {
        pendingRefit = 0;
        refitAll();
      });
    };

    const onMove = (me: MouseEvent): void => {
      const delta = (horizontal ? me.clientX : me.clientY) - startPos;
      let newLeft = leftSize + delta;
      newLeft = Math.max(MIN_PX, Math.min(totalSize - MIN_PX, newLeft));
      const ratio = newLeft / totalSize;
      lastLeftW = weightSum * ratio;
      lastRightW = weightSum - lastLeftW;
      leftWrap.style.flex = `${lastLeftW} 1 0%`;
      rightWrap.style.flex = `${lastRightW} 1 0%`;
      scheduleRefit();
    };

    const onUp = (): void => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      if (pendingRefit !== 0) {
        cancelAnimationFrame(pendingRefit);
        pendingRefit = 0;
      }
      document.body.classList.remove("resizing-col", "resizing-row");
      // Commit the final weights to the daemon. Mutate the cached node so the
      // next structural render keeps the new sizes, and refit terminals to the
      // new geometry.
      if (node.weights) {
        node.weights[leftIdx] = lastLeftW;
        node.weights[rightIdx] = lastRightW;
      }
      const leftPane = firstLeaf(node.children[leftIdx]!);
      const rightPane = firstLeaf(node.children[rightIdx]!);
      void setWeight(leftPane, Math.round(lastLeftW)).catch((err) => {
        console.error("set_weight failed:", leftPane, err);
        toast("Couldn't resize the panes.", "error");
      });
      void setWeight(rightPane, Math.round(lastRightW)).catch((err) =>
        console.error("set_weight failed:", rightPane, err),
      );
      refitAll();
    };

    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  });

  return handle;
}

function renderLeaf(pane: string): HTMLElement {
  const s = getState();
  const info = paneStateOf(pane);
  const state: PaneState = info?.state ?? "idle";
  const focused = s.focusedPane === pane;

  const card = h("div", {
    class:
      "pane-card" +
      (focused ? " focused" : "") +
      (pulses(state) ? " pulse" : ""),
    "data-pane": pane,
    "data-state": state,
    tabindex: "0",
    onmousedown: () => {
      dlog("[pyre-input] click pane", pane); // (a) click stage
      focusPane(pane);
    },
  });
  card.style.setProperty("--heat", heatVar(state));

  // Heat edge (2px top bar) — the signature.
  card.appendChild(h("div", { class: "heat-edge" }));

  // Header: heat dot + title + hover toolbar.
  const dot = h("span", { class: "pane-dot", title: stateLabel(state) });
  dot.style.setProperty("--dot-heat", heatVar(state));

  // Display name overrides the daemon title; fall back to title, else "pane".
  const fallbackTitle = info?.title || "pane";
  const title = paneDisplayName(pane, fallbackTitle);
  const agent = (info?.agent ?? "").trim() || "unknown";
  // Pane title span — double-click swaps it for an inline editor (commit →
  // rename_pane). CRITICAL: the card's onmousedown calls focusPane(pane), which
  // setState({focusedPane}) → renderAll → a STRUCTURAL renderCenter re-render
  // (the fingerprint includes focus). On an UNFOCUSED pane the very first
  // mousedown of a double-click would rebuild the card and destroy this span
  // before the dblclick lands on it, so the editor never opened. attachRename-
  // Affordance stops mousedown/click at the title so moving focus to the card
  // never tears the title out mid-double-click. No onSingleClick: the card body/
  // terminal owns focus on click; the title only renames on double-click.
  const titleSpan = h("span", { class: "pane-title" }, title);
  attachRenameAffordance({
    label: titleSpan,
    // Read the current name from LIVE state on each double-click. `title` is a
    // const captured at render time, and renderCenter does not rebuild on a
    // name-only change (fingerprint unchanged), so the captured const would seed
    // a second rename with the pre-rename name. Recompute from paneStates live.
    value: () => paneDisplayName(pane, paneStateOf(pane)?.title || "pane"),
    inputClass: "inline-edit-pane",
    ariaLabel: "Rename pane",
    onCommit: (name) => renamePaneAction(pane, name),
  });
  const header = h(
    "div",
    { class: "pane-header" },
    dot,
    titleSpan,
    // Per-pane agent chip — subtly tinted by kind (claude / shell / unknown).
    h(
      "span",
      { class: `agent-chip pane-agent-chip agent-${chipKind(agent)}`, title: `agent: ${agent}` },
      agent,
    ),
    h("span", { class: "pane-state-label" }, stateLabel(state)),
    // Every pane in every window is splittable: the full toolbar is always
    // present (split down/right, zoom, close).
    h(
      "div",
      { class: "pane-toolbar" },
      toolBtn("Split down", icon("splitDown"), () => void splitDown(pane)),
      toolBtn("Split right", icon("splitRight"), () => void splitRight(pane)),
      toolBtn(
        s.zoomedPane === pane ? "Restore" : "Zoom",
        icon("zoom"),
        () => zoomPane(pane),
      ),
      toolBtn("Close pane", icon("close"), () => void closePaneAction(pane)),
    ),
  );

  const body = h("div", { class: "pane-body" });
  card.appendChild(header);
  card.appendChild(body);

  // Mount (or re-attach) the terminal after the body is in the DOM tree.
  queueMicrotask(() => mountPaneTerminal(pane, s.activeSession ?? "", body));

  return card;
}

function toolBtn(
  title: string,
  glyph: string,
  onclick: () => void,
): HTMLElement {
  return h(
    "button",
    {
      class: "pane-tool",
      title,
      "aria-label": title,
      onclick: (e: Event) => {
        e.stopPropagation();
        onclick();
      },
    },
    h("span", { html: glyph }),
  );
}

function daemonDownPanel(): HTMLElement {
  const btn = h(
    "button",
    {
      class: "btn primary reconnect-btn",
      onclick: () => {
        btn.setAttribute("disabled", "");
        btn.textContent = "Reconnecting…";
        const fn = (
          window as unknown as { __pyreReconnect?: () => Promise<boolean> }
        ).__pyreReconnect;
        const done = () => {
          btn.removeAttribute("disabled");
          btn.textContent = "Reconnect";
        };
        if (fn) {
          void fn()
            .catch((err) => console.error("reconnect failed:", err))
            .finally(done);
        } else {
          console.warn("reconnect handler not yet registered");
          done();
        }
      },
    },
    "Reconnect",
  );
  return h(
    "div",
    { class: "center-empty daemon-down" },
    // CSS-drawn dot (no glyph) so it never tofus on a font without U+25CF.
    h("div", { class: "down-mark", "aria-hidden": "true" }),
    h("p", { class: "down-title" }, "Can't reach pyred."),
    h(
      "p",
      { class: "down-sub" },
      "Start the daemon, then reconnect. Retrying automatically…",
    ),
    btn,
  );
}
