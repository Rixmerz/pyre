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
// of the leaf pane IDs in tree order, the zoomed pane, and the focused pane.
// When only heat/state changes (paneStates updated, layout fingerprint unchanged)
// the function returns early and lets the in-place heat updates in session-ops.ts
// handle the visual change. Full replaceChildren only runs when the layout
// structure truly changes (pane added/removed/split/weight or focus/zoom changed).

import { h, replaceChildren } from "./dom";
import { icon } from "./icons";
import { getState, paneStateOf } from "../state";
import { heatVar, pulses, stateLabel } from "../heat";
import {
  closePaneAction,
  focusPane,
  splitDown,
  splitRight,
  zoomPane,
} from "../actions";
import { mountPaneTerminal, refitAll } from "../terminals";
import { setWeight } from "../api";
import { chipKind } from "./agents";
import type { LayoutNode, PaneState } from "../types";

/** First leaf pane id under a node — the representative weight target. */
function firstLeaf(node: LayoutNode): string {
  return node.kind === "leaf" ? node.pane : firstLeaf(node.children[0]!);
}

/** Canonical string of all leaf pane ids in render order + structural metadata. */
function layoutFingerprint(
  layout: LayoutNode | undefined,
  zoomedPane: string | null,
  focusedPane: string | null,
  connected: boolean,
): string {
  if (!connected) return "__disconnected__";
  if (!layout) return "__no-layout__";
  if (zoomedPane) return `zoom:${zoomedPane}:focus:${focusedPane ?? ""}`;
  return `tree:${fingerprintNode(layout)}:focus:${focusedPane ?? ""}`;
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
  const layout = s.activeSession ? s.layouts.get(s.activeSession) : undefined;

  const fp = layoutFingerprint(layout, s.zoomedPane, s.focusedPane, s.connected);

  if (fp === lastFingerprint) {
    // Layout structure, focus, and zoom are all unchanged.
    // Heat/state updates are applied in-place by session-ops.ts — skip the
    // full tear-down to prevent xterm re-parenting and focus-blur.
    console.log("[pyre-render] skip structural re-render — fingerprint unchanged");
    return;
  }

  console.log("[pyre-render] structural re-render — fingerprint changed:", lastFingerprint, "→", fp);
  lastFingerprint = fp;

  if (!s.connected) {
    replaceChildren(root, daemonDownPanel());
    return;
  }
  if (!layout) {
    replaceChildren(
      root,
      h(
        "div",
        { class: "center-empty" },
        h("p", {}, "No active session."),
        h(
          "button",
          { class: "btn primary", onclick: () => void import("../actions").then((m) => m.newSession()) },
          "New session",
        ),
      ),
    );
    return;
  }

  // Zoom: render only the zoomed pane full-bleed.
  if (s.zoomedPane) {
    const leaf: LayoutNode = { kind: "leaf", pane: s.zoomedPane };
    replaceChildren(root, renderNode(leaf));
    return;
  }

  replaceChildren(root, renderNode(layout));
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

    const onMove = (me: MouseEvent): void => {
      const delta = (horizontal ? me.clientX : me.clientY) - startPos;
      let newLeft = leftSize + delta;
      newLeft = Math.max(MIN_PX, Math.min(totalSize - MIN_PX, newLeft));
      const ratio = newLeft / totalSize;
      lastLeftW = weightSum * ratio;
      lastRightW = weightSum - lastLeftW;
      leftWrap.style.flex = `${lastLeftW} 1 0%`;
      rightWrap.style.flex = `${lastRightW} 1 0%`;
    };

    const onUp = (): void => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
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
      void setWeight(leftPane, Math.round(lastLeftW)).catch((err) =>
        console.error("set_weight failed:", leftPane, err),
      );
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
      console.log("[pyre-input] click pane", pane); // (a) click stage
      focusPane(pane);
    },
  });
  card.style.setProperty("--heat", heatVar(state));

  // Heat edge (2px top bar) — the signature.
  card.appendChild(h("div", { class: "heat-edge" }));

  // Header: heat dot + title + hover toolbar.
  const dot = h("span", { class: "pane-dot", title: stateLabel(state) });
  dot.style.setProperty("--dot-heat", heatVar(state));

  const title = info?.title || "pane";
  const agent = (info?.agent ?? "").trim() || "unknown";
  const header = h(
    "div",
    { class: "pane-header" },
    dot,
    h("span", { class: "pane-title" }, title),
    // Per-pane agent chip — subtly tinted by kind (claude / shell / unknown).
    h(
      "span",
      { class: `agent-chip pane-agent-chip agent-${chipKind(agent)}`, title: `agent: ${agent}` },
      agent,
    ),
    h("span", { class: "pane-state-label" }, stateLabel(state)),
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
