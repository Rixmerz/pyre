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
import { mountPaneTerminal } from "../terminals";
import type { LayoutNode, PaneState } from "../types";

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
  const el = h("div", {
    class: "split " + (node.dir === "v" ? "split-h" : "split-v"),
  });
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
    el.appendChild(wrap);
  });
  return el;
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
  const header = h(
    "div",
    { class: "pane-header" },
    dot,
    h("span", { class: "pane-title" }, title),
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
