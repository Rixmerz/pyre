// Minimal DOM helper. No framework — just a typed createElement so the render
// modules read declaratively without innerHTML string-building (which would be
// an XSS footgun the moment block command text or session names flow through).

type Attrs = Record<string, string | number | boolean | EventListener | undefined>;
type Child = Node | string | null | undefined | false;

/** Create an element with attributes/handlers and children. */
export function h(
  tag: string,
  attrs: Attrs = {},
  ...children: Child[]
): HTMLElement {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k === "class") el.className = String(v);
    else if (k === "text") el.textContent = String(v);
    else if (k === "html") el.innerHTML = String(v); // only for trusted glyph SVG
    else if (k.startsWith("on") && typeof v === "function") {
      el.addEventListener(k.slice(2).toLowerCase(), v as EventListener);
    } else if (typeof v === "boolean") {
      if (v) el.setAttribute(k, "");
    } else {
      el.setAttribute(k, String(v));
    }
  }
  for (const c of children) {
    if (c == null || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

/** Replace all children of `parent` with `nodes`. */
export function replaceChildren(parent: HTMLElement, ...nodes: Child[]): void {
  parent.replaceChildren(
    ...nodes.filter((n): n is Node | string => n != null && n !== false)
      .map((n) => (n instanceof Node ? n : document.createTextNode(n))),
  );
}

/** Format a duration in ms as a compact human string (e.g. "1.2s", "340ms"). */
export function fmtDuration(ms: number | null | undefined): string {
  if (ms == null) return "";
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  const m = Math.floor(ms / 60_000);
  const sec = Math.round((ms % 60_000) / 1000);
  return `${m}m ${sec}s`;
}

/** Compute a block's wall-clock duration in ms from its timestamps. */
export function blockDurationMs(
  startedAt: string,
  endedAt?: string | null,
): number | null {
  const start = Date.parse(startedAt);
  if (Number.isNaN(start)) return null;
  const end = endedAt ? Date.parse(endedAt) : Date.now();
  if (Number.isNaN(end)) return null;
  return Math.max(0, end - start);
}
