// In-memory fake pyred — the dev-only mock daemon behind `pnpm dev:mock`.
//
// It implements EVERY `invoke("<cmd>", …)` command that `api.ts` (plus the two
// direct callers `notify.ts` and `main.ts`) sends, holding a mutable model of
// sessions → windows → panes (layout trees) + per-pane blocks. READ commands
// return current state; MUTATION commands actually mutate the model and return
// the right shape, so the UI reacts: closing removes, splitting adds a pane,
// renaming updates the label. Because every call still flows through the real
// `api.ts` wrappers, a GUI-side bug reproduces here exactly as in production.
//
// Constraints honored: dependency-free, single file, no `Math.random`/`Date.now`
// at module top (ids come from an incrementing counter; finished-block timestamps
// derive from a fixed seed constant), strict TS (no `any`, no non-null `!`). The
// one running block is the exception: its start time is relative to load and a
// dev-only timer completes it shortly after boot, so the running→done lifecycle
// is visibly demonstrated — but both `Date.now()` calls run lazily inside
// `seed()` / its timer, never at module top, so the module stays side-effect-free
// at the top level (the model is built lazily on first call) and still tree-shakes
// out of the production bundle when VITE_MOCK is unset.

import type {
  Block,
  DaemonStatus,
  GhAccount,
  GhDeviceStart,
  GhPoll,
  GitInfo,
  LayoutNode,
  PaneState,
  PaneStateInfo,
  PidInfo,
  PollEventsResult,
  SessionInfo,
  ThemeMeta,
  ThemePalette,
  WindowInfo,
} from "./types";

// ── Internal model ───────────────────────────────────────────────────────────

interface MockBlock {
  block: Block;
  stdout: string;
}

interface MockPane {
  id: string;
  session: string;
  window: string;
  pid: number;
  state: PaneState;
  title: string | null;
  agent: string;
  name: string | null;
  blocks: MockBlock[];
}

interface MockWindow {
  id: string;
  session: string;
  name: string;
  position: number;
  /** Layout tree for this window. `null` while the window has no pane yet
   *  (the daemon's `new_window` makes an EMPTY window; `open_pane` fills it). */
  layout: LayoutNode | null;
}

interface MockSession {
  id: string;
  name: string;
  createdAt: string;
  lastActiveAt: string;
}

interface MockState {
  sessions: Map<string, MockSession>;
  windows: Map<string, MockWindow>;
  panes: Map<string, MockPane>;
  /** Monotonic id counter — deterministic, no Math.random/Date.now. */
  seq: number;
  /** Monotonic fake-pid counter. */
  pidSeq: number;
  /** GitHub link state. `authorized` is false until the demo's explicit
   *  "Simulate authorization" button calls `mockAuthorizeGitHub()` — only then
   *  does poll report `authorized` and `github_account` return the account.
   *  Reset to false on each `github_device_start` (and on disconnect). The mock
   *  NEVER auto-authorizes, so the device-code modal stays an honest demo. */
  github: { authorized: boolean };
}

/** The account the mock device flow "authorizes" into after a couple of polls. */
const MOCK_GH_ACCOUNT: GhAccount = {
  login: "mockuser",
  name: "Mock User",
  avatar_url: "https://avatars.githubusercontent.com/u/9919?s=80",
  html_url: "https://github.com/mockuser",
};

/** Fixed seed timestamp (no Date.now at module top). */
const SEED_TS = "2026-06-20T09:00:00.000Z";
const SOCKET = "/run/user/1000/pyre/mock.sock";

/** How long before load the demo's one running block started — keeps its elapsed
 *  time sane ("0m Ns") instead of days-since the fixed seed. Computed lazily
 *  inside `seed()`, never at module top. */
const RUNNING_STARTED_MS_AGO = 4_000;
/** Dev-only: how long after boot that running block completes, so the demo
 *  visibly shows the running→done lifecycle on the next 750 ms poll tick. */
const RUNNING_COMPLETES_AFTER_MS = 6_000;

// Lazily built so the module body stays side-effect-free → tree-shakeable.
let state: MockState | null = null;
function model(): MockState {
  if (!state) state = seed();
  return state;
}

// ── Id / pid helpers ─────────────────────────────────────────────────────────

function nextId(s: MockState, prefix: string): string {
  s.seq += 1;
  return `${prefix}-${s.seq}`;
}

function nextPid(s: MockState): number {
  s.pidSeq += 1;
  return s.pidSeq;
}

// ── Layout-tree helpers ──────────────────────────────────────────────────────

function leaf(pane: string): LayoutNode {
  return { kind: "leaf", pane };
}

/** All pane ids under a node (depth-first), `[]` for a null layout. */
function leaves(node: LayoutNode | null): string[] {
  if (!node) return [];
  if (node.kind === "leaf") return [node.pane];
  return node.children.flatMap(leaves);
}

/** Replace the `target` leaf with a split of [target, newPane] in `dir`. */
function splitLeaf(
  node: LayoutNode,
  target: string,
  dir: "h" | "v",
  newPane: string,
): LayoutNode {
  if (node.kind === "leaf") {
    if (node.pane !== target) return node;
    return {
      kind: "split",
      dir,
      children: [leaf(target), leaf(newPane)],
      weights: [50, 50],
    };
  }
  return {
    ...node,
    children: node.children.map((c) => splitLeaf(c, target, dir, newPane)),
  };
}

/** Remove a leaf, collapsing single-child splits (mirrors LayoutNode::close). */
function removeLeaf(node: LayoutNode, pane: string): LayoutNode | null {
  if (node.kind === "leaf") return node.pane === pane ? null : node;
  const kept: LayoutNode[] = [];
  const keptWeights: number[] = [];
  node.children.forEach((child, i) => {
    const r = removeLeaf(child, pane);
    if (r) {
      kept.push(r);
      keptWeights.push(node.weights?.[i] ?? 50);
    }
  });
  if (kept.length === 0) return null;
  if (kept.length === 1) {
    const only = kept[0];
    return only ?? null;
  }
  return { kind: "split", dir: node.dir, children: kept, weights: keptWeights };
}

/** Set the weight of `pane`'s slot in whichever split directly contains it. */
function setLeafWeight(node: LayoutNode, pane: string, weight: number): void {
  if (node.kind === "leaf") return;
  const weights = node.weights ?? node.children.map(() => 50);
  node.children.forEach((child, i) => {
    if (child.kind === "leaf" && child.pane === pane) {
      weights[i] = weight;
    } else {
      setLeafWeight(child, pane, weight);
    }
  });
  node.weights = weights;
}

function windowOfPane(s: MockState, pane: string): MockWindow | undefined {
  for (const w of s.windows.values()) {
    if (leaves(w.layout).includes(pane)) return w;
  }
  return undefined;
}

function sessionWindows(s: MockState, session: string): MockWindow[] {
  return [...s.windows.values()]
    .filter((w) => w.session === session)
    .sort((a, b) => a.position - b.position);
}

function paneCount(s: MockState, session: string): number {
  return sessionWindows(s, session).reduce(
    (n, w) => n + leaves(w.layout).length,
    0,
  );
}

// Deep clone so callers can't mutate the model by reference (layouts are
// JSON-safe trees, so JSON round-trip is the dependency-free clone).
function clone<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

// ── Arg coercion (args arrive as an opaque record from the wrapper) ──────────

type Args = Record<string, unknown>;

function reqStr(a: Args, k: string): string {
  const v = a[k];
  if (typeof v !== "string") {
    throw new Error(`mock daemon: expected string arg "${k}"`);
  }
  return v;
}

function reqNum(a: Args, k: string): number {
  const v = a[k];
  if (typeof v !== "number") {
    throw new Error(`mock daemon: expected number arg "${k}"`);
  }
  return v;
}

function optStr(a: Args, k: string): string | null {
  const v = a[k];
  return typeof v === "string" ? v : null;
}

// ── Pane / block factories ───────────────────────────────────────────────────

function makePane(
  s: MockState,
  opts: {
    id?: string;
    session: string;
    window: string;
    state?: PaneState;
    title?: string | null;
    agent?: string;
    name?: string | null;
  },
): MockPane {
  const pane: MockPane = {
    id: opts.id ?? nextId(s, "pane"),
    session: opts.session,
    window: opts.window,
    pid: nextPid(s),
    state: opts.state ?? "idle",
    title: opts.title ?? null,
    agent: opts.agent ?? "shell",
    name: opts.name ?? null,
    blocks: [],
  };
  s.panes.set(pane.id, pane);
  return pane;
}

function makeBlock(
  pane: MockPane,
  id: string,
  command: string,
  stdout: string,
  exitCode: number | null,
  durationMs: number | null,
  startedAt: string = SEED_TS,
): MockBlock {
  const running = exitCode === null;
  // Derive ended_at from start + duration so the inspector shows a realistic
  // elapsed: blockDurationMs (render/dom.ts) reads the timestamps, not
  // duration_ms. Without this, finished demo blocks rendered "0ms" because
  // started_at === ended_at (both SEED_TS).
  const endedAt = running
    ? null
    : new Date(Date.parse(startedAt) + (durationMs ?? 0)).toISOString();
  const block: Block = {
    id,
    pane: pane.id,
    session: pane.session,
    command,
    cwd: "/home/dev/pyre",
    started_at: startedAt,
    ended_at: endedAt,
    exit_code: exitCode,
    duration_ms: durationMs,
    running,
  };
  return { block, stdout };
}

// ── Seed: realistic, non-trivial fake world ──────────────────────────────────

function seed(): MockState {
  const s: MockState = {
    sessions: new Map(),
    windows: new Map(),
    panes: new Map(),
    seq: 0,
    pidSeq: 1000,
    github: { authorized: false },
  };

  // Sessions ──────────────────────────────────────────────────────────────
  s.sessions.set("sess-dev", {
    id: "sess-dev",
    name: "dev",
    createdAt: SEED_TS,
    lastActiveAt: SEED_TS,
  });
  s.sessions.set("sess-infra", {
    id: "sess-infra",
    name: "infra",
    createdAt: SEED_TS,
    lastActiveAt: SEED_TS,
  });

  // dev / window "1" — a 2-pane split (HSplit = stacked), varied heat ───────
  const devA = makePane(s, {
    id: "pane-dev-a",
    session: "sess-dev",
    window: "win-dev-1",
    state: "running",
    agent: "claude",
    name: "agent",
    title: "claude — implementing",
  });
  const devB = makePane(s, {
    id: "pane-dev-b",
    session: "sess-dev",
    window: "win-dev-1",
    state: "idle",
    agent: "shell",
    title: null,
  });
  // The one RUNNING block: started a few seconds before load (sane elapsed) and
  // completed shortly after boot by completeBlockSoon — demonstrating running→done.
  const runStartedAt = new Date(Date.now() - RUNNING_STARTED_MS_AGO).toISOString();
  const runningBlock = makeBlock(devA, "blk-dev-a-3", "cargo test -p pyred",
    "running 42 tests\ntest store::tests::window_migration ... ok\ntest store::tests::crud ... ok\n",
    null, null, runStartedAt);
  devA.blocks = [
    makeBlock(devA, "blk-dev-a-1", "cargo check",
      "    Checking pyre-proto v0.4.0\n    Checking pyred v0.4.0\n     Finished dev [unoptimized + debuginfo] in 2.41s\n",
      0, 2410),
    makeBlock(devA, "blk-dev-a-2", "git status",
      "On branch main\nYour branch is up to date with 'origin/main'.\n\nnothing to commit, working tree clean\n",
      0, 38),
    runningBlock,
  ];
  completeBlockSoon(devA, runningBlock);
  s.windows.set("win-dev-1", {
    id: "win-dev-1",
    session: "sess-dev",
    name: "1",
    position: 0,
    layout: {
      kind: "split",
      dir: "h",
      children: [leaf(devA.id), leaf(devB.id)],
      weights: [55, 45],
    },
  });

  // dev / window "build" — single pane, finished build ──────────────────────
  const build = makePane(s, {
    id: "pane-build",
    session: "sess-dev",
    window: "win-dev-build",
    state: "done",
    agent: "shell",
    title: "cargo build --release",
  });
  build.blocks = [
    makeBlock(build, "blk-build-1", "cargo build --release",
      "   Compiling pyre-proto v0.4.0\n   Compiling pyred v0.4.0\n   Compiling pyre-tui v0.4.0\n    Finished release [optimized] in 48.21s\n",
      0, 48210),
  ];
  s.windows.set("win-dev-build", {
    id: "win-dev-build",
    session: "sess-dev",
    name: "build",
    position: 1,
    layout: leaf(build.id),
  });

  // infra / window "1" — side-by-side split (VSplit), waiting + interactive ──
  const infraA = makePane(s, {
    id: "pane-infra-a",
    session: "sess-infra",
    window: "win-infra-1",
    state: "waiting",
    agent: "claude",
    name: "deploy",
    title: "deploy to prod? (y/n)",
  });
  const infraB = makePane(s, {
    id: "pane-infra-b",
    session: "sess-infra",
    window: "win-infra-1",
    state: "interactive",
    agent: "shell",
    title: "ssh prod-01",
  });
  s.windows.set("win-infra-1", {
    id: "win-infra-1",
    session: "sess-infra",
    name: "1",
    position: 0,
    layout: {
      kind: "split",
      dir: "v",
      children: [leaf(infraA.id), leaf(infraB.id)],
      weights: [50, 50],
    },
  });

  // infra / window "logs" — single pane, crashed ───────────────────────────
  const logs = makePane(s, {
    id: "pane-logs",
    session: "sess-infra",
    window: "win-infra-logs",
    state: "crashed",
    agent: "shell",
    title: "journalctl -fu pyred",
  });
  logs.blocks = [
    makeBlock(logs, "blk-logs-1", "journalctl -fu pyred",
      "pyred[4821]: accepted control connection\npyred[4821]: panic: socket closed unexpectedly\n",
      1, 1200),
  ];
  s.windows.set("win-infra-logs", {
    id: "win-infra-logs",
    session: "sess-infra",
    name: "logs",
    position: 1,
    layout: leaf(logs.id),
  });

  return s;
}

/**
 * Dev-only: finish the seeded running block a few seconds after boot so the demo
 * shows a real running→done transition — the 750 ms block poll in main.ts
 * (reloadFocusedBlocks → list_blocks) picks up the mutation on its next tick.
 * Mutates the model in place; no production effect (the whole module is dev-only
 * and tree-shaken when VITE_MOCK is unset). Self-guarding: only completes a block
 * still running, and the timer is unref'd so it never keeps a test process alive.
 */
function completeBlockSoon(pane: MockPane, mb: MockBlock): void {
  if (typeof setTimeout !== "function") return;
  const handle = setTimeout(() => {
    if (!mb.block.running) return; // already finished / superseded
    const endedAt = new Date().toISOString();
    const startMs = Date.parse(mb.block.started_at);
    const endMs = Date.parse(endedAt);
    mb.block.running = false;
    mb.block.exit_code = 0;
    mb.block.ended_at = endedAt;
    mb.block.duration_ms = Number.isNaN(startMs) ? null : Math.max(0, endMs - startMs);
    mb.stdout += "test result: ok. 42 passed; 0 failed; 0 ignored\n";
    // The pane that was running the command returns to idle once it completes.
    pane.state = "idle";
  }, RUNNING_COMPLETES_AFTER_MS);
  // In Node (vitest), don't let the pending timer keep the process alive.
  (handle as { unref?: () => void }).unref?.();
}

// ── Themes ───────────────────────────────────────────────────────────────────

// ember ANSI table (kept as a named constant; it doubles as the canonical
// dark reference and is already correct after the AAA-floor commit e76b3c5).
const ANSI_EMBER: string[] = [
  "#15171c", "#e25563", "#7fb958", "#e3b341",
  "#4f9cf2", "#b886f0", "#3fb8b8", "#c5c8d0",
  "#2c2f39", "#ff6b7a", "#8fd99a", "#ffcf5a",
  "#6bb0ff", "#cf9cff", "#5fd6d6", "#eceef4",
];

// All 18 daemon themes in registry order, seeded from
// crates/pyre-themes/src/palettes/<name>.rs (post-e76b3c5 AAA nudges).
const THEME_META: ThemeMeta[] = [
  { name: "ember",             display_name: "Ember Black",       kind: "dark",  accent: "#e8743b", bg: "#0a0b0e" },
  { name: "catppuccin-mocha",  display_name: "Catppuccin Mocha",  kind: "dark",  accent: "#caa9f5", bg: "#1e1e2e" },
  { name: "catppuccin-latte",  display_name: "Catppuccin Latte",  kind: "light", accent: "#6469a7", bg: "#eff1f5" },
  { name: "tokyo-night",       display_name: "Tokyo Night",       kind: "dark",  accent: "#7aa2f7", bg: "#1a1b26" },
  { name: "tokyo-night-light", display_name: "Tokyo Night Light", kind: "light", accent: "#34548a", bg: "#d5d6db" },
  { name: "gruvbox-dark",      display_name: "Gruvbox Dark",      kind: "dark",  accent: "#fe8019", bg: "#282828" },
  { name: "gruvbox-light",     display_name: "Gruvbox Light",     kind: "light", accent: "#af3a03", bg: "#fbf1c7" },
  { name: "one-dark",          display_name: "One Dark",          kind: "dark",  accent: "#61afef", bg: "#282c34" },
  { name: "one-light",         display_name: "One Light",         kind: "light", accent: "#3b6edd", bg: "#fafafa" },
  { name: "solarized-dark",    display_name: "Solarized Dark",    kind: "dark",  accent: "#2893dd", bg: "#002b36" },
  { name: "solarized-light",   display_name: "Solarized Light",   kind: "light", accent: "#2076b3", bg: "#fdf6e3" },
  { name: "kanagawa",          display_name: "Kanagawa",          kind: "dark",  accent: "#7e9cd8", bg: "#1f1f28" },
  { name: "rose-pine",         display_name: "Rosé Pine",   kind: "dark",  accent: "#3a8aaa", bg: "#191724" },
  { name: "rose-pine-dawn",    display_name: "Rosé Pine Dawn", kind: "light", accent: "#286b83", bg: "#faf4ed" },
  { name: "vesper",            display_name: "Vesper",            kind: "dark",  accent: "#fdc173", bg: "#101010" },
  { name: "nord",              display_name: "Nord",              kind: "dark",  accent: "#81a1c1", bg: "#2e3440" },
  { name: "dracula",           display_name: "Dracula",           kind: "dark",  accent: "#bd93f9", bg: "#282a36" },
  { name: "terminal",          display_name: "Terminal Default",  kind: "dark",  accent: "#00afff", bg: "#000000" },
];

const THEME_PALETTES: Record<string, ThemePalette> = {
  "ember": {
    name: "ember",
    display_name: "Ember Black",
    bg: "#0a0b0e",
    bg_dim: "#070809",
    fg: "#eceef4",
    fg_dim: "#969ca6",
    border: "#23252d",
    border_focus: "#e8743b",
    cursor: "#e8743b",
    accent: "#e8743b",
    ok: "#3fb950",
    warn: "#f5b042",
    error: "#ff5630",
    ansi: ANSI_EMBER,
  },
  "catppuccin-mocha": {
    name: "catppuccin-mocha",
    display_name: "Catppuccin Mocha",
    bg: "#1e1e2e",
    bg_dim: "#181825",
    fg: "#cdd6f4",
    fg_dim: "#80859f",
    border: "#313244",
    border_focus: "#caa9f5",
    cursor: "#f5e0dc",
    accent: "#caa9f5",
    ok: "#a6e3a1",
    warn: "#f9e2af",
    error: "#f38ba8",
    ansi: [
      "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af",
      "#89b4fa", "#caa9f5", "#89dceb", "#bac2de",
      "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af",
      "#89b4fa", "#caa9f5", "#89dceb", "#a6adc8",
    ],
  },
  "catppuccin-latte": {
    name: "catppuccin-latte",
    display_name: "Catppuccin Latte",
    bg: "#eff1f5",
    bg_dim: "#e6e9ef",
    fg: "#4c4f69",
    fg_dim: "#6b6d78",
    border: "#cccfd4",
    border_focus: "#8087d7",
    cursor: "#dc8a78",
    accent: "#6469a7",
    ok: "#40a02b",
    warn: "#df8e1d",
    error: "#d20f39",
    ansi: [
      "#5c5f77", "#d20f39", "#40a02b", "#df8e1d",
      "#1e66f5", "#ea76cb", "#179abc", "#acb0be",
      "#6c6f85", "#d20f39", "#40a02b", "#df8e1d",
      "#1e66f5", "#ea76cb", "#179abc", "#bcc0cc",
    ],
  },
  "tokyo-night": {
    name: "tokyo-night",
    display_name: "Tokyo Night",
    bg: "#1a1b26",
    bg_dim: "#161720",
    fg: "#c0caf5",
    fg_dim: "#7480b9",
    border: "#292e42",
    border_focus: "#7aa2f7",
    cursor: "#c0caf5",
    accent: "#7aa2f7",
    ok: "#9ece6a",
    warn: "#e0af68",
    error: "#f7768e",
    ansi: [
      "#15161e", "#f7768e", "#9ece6a", "#e0af68",
      "#7aa2f7", "#bb9af7", "#7dcfff", "#a9b1d6",
      "#414c6f", "#f7768e", "#9ece6a", "#e0af68",
      "#7aa2f7", "#bb9af7", "#7dcfff", "#c0caf5",
    ],
  },
  "tokyo-night-light": {
    name: "tokyo-night-light",
    display_name: "Tokyo Night Light",
    bg: "#d5d6db",
    bg_dim: "#cacbcf",
    fg: "#34354a",
    fg_dim: "#5b5c70",
    border: "#b8b9c2",
    border_focus: "#34548a",
    cursor: "#34354a",
    accent: "#34548a",
    ok: "#385f0e",
    warn: "#885d0d",
    error: "#8c4151",
    ansi: [
      "#d5d6db", "#8c4151", "#385f0e", "#885d0d",
      "#34548a", "#5a3796", "#0f4b6e", "#71728a",
      "#b8b9c2", "#8c4151", "#385f0e", "#885d0d",
      "#34548a", "#5a3796", "#0f4b6e", "#34354a",
    ],
  },
  "gruvbox-dark": {
    name: "gruvbox-dark",
    display_name: "Gruvbox Dark",
    bg: "#282828",
    bg_dim: "#1d2021",
    fg: "#ebdbb2",
    fg_dim: "#a89984",
    border: "#3c3836",
    border_focus: "#fe8019",
    cursor: "#fbf1c7",
    accent: "#fe8019",
    ok: "#b8bb26",
    warn: "#fabd2f",
    error: "#cc241d",
    ansi: [
      "#282828", "#cc241d", "#98971a", "#d79921",
      "#458588", "#b16286", "#689689", "#a89984",
      "#928374", "#fb4934", "#b8bb26", "#fabd2f",
      "#83a598", "#d3869b", "#8ec07c", "#ebdbb2",
    ],
  },
  "gruvbox-light": {
    name: "gruvbox-light",
    display_name: "Gruvbox Light",
    bg: "#fbf1c7",
    bg_dim: "#f2e5bc",
    fg: "#3c3836",
    fg_dim: "#786b61",
    border: "#d5c4a1",
    border_focus: "#af3a03",
    cursor: "#282828",
    accent: "#af3a03",
    ok: "#79740e",
    warn: "#b57614",
    error: "#9d0006",
    ansi: [
      "#fbf1c7", "#9d0006", "#79740e", "#b57614",
      "#076678", "#8f3f71", "#427b58", "#7c6f64",
      "#928374", "#cc241d", "#98971a", "#d79921",
      "#458588", "#b16286", "#689689", "#3c3836",
    ],
  },
  "one-dark": {
    name: "one-dark",
    display_name: "One Dark",
    bg: "#282c34",
    bg_dim: "#21252b",
    fg: "#b0b8c5",
    fg_dim: "#8893a6",
    border: "#3e4451",
    border_focus: "#61afef",
    cursor: "#528bff",
    accent: "#61afef",
    ok: "#98c379",
    warn: "#e5c07b",
    error: "#e06c75",
    ansi: [
      "#282c34", "#e06c75", "#98c379", "#e5c07b",
      "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
      "#5c6370", "#be5046", "#7a9f60", "#d19a66",
      "#3b84c0", "#9a52af", "#358b96", "#ffffff",
    ],
  },
  "one-light": {
    name: "one-light",
    display_name: "One Light",
    bg: "#fafafa",
    bg_dim: "#f0f0f0",
    fg: "#383a42",
    fg_dim: "#737378",
    border: "#e5e5e6",
    border_focus: "#4078f2",
    cursor: "#526fff",
    accent: "#3b6edd",
    ok: "#50a14f",
    warn: "#c18801",
    error: "#e45649",
    ansi: [
      "#383a42", "#e45649", "#50a14f", "#c18801",
      "#4078f2", "#a626a4", "#0999b3", "#fbfbfb",
      "#a0a1a7", "#e45649", "#50a14f", "#c18801",
      "#4078f2", "#a626a4", "#0999b3", "#383a42",
    ],
  },
  "solarized-dark": {
    name: "solarized-dark",
    display_name: "Solarized Dark",
    bg: "#002b36",
    bg_dim: "#073642",
    fg: "#839496",
    fg_dim: "#74929b",
    border: "#073642",
    border_focus: "#268bd2",
    cursor: "#93a1a1",
    accent: "#2893dd",
    ok: "#859900",
    warn: "#b58900",
    error: "#dc322f",
    ansi: [
      "#073642", "#dc322f", "#859900", "#b58900",
      "#268bd2", "#d33682", "#2aa198", "#839496",
      "#002b36", "#cb4b16", "#586e75", "#657b83",
      "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
    ],
  },
  "solarized-light": {
    name: "solarized-light",
    display_name: "Solarized Light",
    bg: "#fdf6e3",
    bg_dim: "#eee8d5",
    fg: "#657b83",
    fg_dim: "#697373",
    border: "#eee8d5",
    border_focus: "#268bd2",
    cursor: "#586e75",
    accent: "#2076b3",
    ok: "#859900",
    warn: "#b58900",
    error: "#dc322f",
    ansi: [
      "#073642", "#dc322f", "#859900", "#b58900",
      "#268bd2", "#d33682", "#2aa198", "#839496",
      "#002b36", "#cb4b16", "#586e75", "#657b83",
      "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
    ],
  },
  "kanagawa": {
    name: "kanagawa",
    display_name: "Kanagawa",
    bg: "#1f1f28",
    bg_dim: "#16161d",
    fg: "#dcd7ba",
    fg_dim: "#87877c",
    border: "#2a2a37",
    border_focus: "#7e9cd8",
    cursor: "#c8c093",
    accent: "#7e9cd8",
    ok: "#76946a",
    warn: "#ff9e3b",
    error: "#c34243",
    ansi: [
      "#090b10", "#c34243", "#76946a", "#c0a36e",
      "#7e9cd8", "#957fb8", "#6a9f89", "#c8c093",
      "#727269", "#e82b2b", "#98bb6c", "#e6c384",
      "#7fb4ca", "#938aa8", "#7aa89f", "#dcd7ba",
    ],
  },
  "rose-pine": {
    name: "rose-pine",
    display_name: "Rosé Pine",
    bg: "#191724",
    bg_dim: "#12101e",
    fg: "#e0def4",
    fg_dim: "#817d9d",
    border: "#26233a",
    border_focus: "#31748f",
    cursor: "#eb6f92",
    accent: "#3a8aaa",
    ok: "#9ccfd8",
    warn: "#f6c177",
    error: "#eb6f92",
    ansi: [
      "#26233a", "#eb6f92", "#31748f", "#f6c177",
      "#9ccfd8", "#c4a7e7", "#eae9fc", "#e0def4",
      "#6e6a86", "#eb6f92", "#31748f", "#f6c177",
      "#9ccfd8", "#c4a7e7", "#eae9fc", "#ffffff",
    ],
  },
  "rose-pine-dawn": {
    name: "rose-pine-dawn",
    display_name: "Rosé Pine Dawn",
    bg: "#faf4ed",
    bg_dim: "#f2e9e1",
    fg: "#534f74",
    fg_dim: "#766d7d",
    border: "#dfd8d9",
    border_focus: "#286b83",
    cursor: "#b4637a",
    accent: "#286b83",
    ok: "#56949f",
    warn: "#ea9d34",
    error: "#b4637a",
    ansi: [
      "#f2e9e1", "#b4637a", "#286b83", "#ea9d34",
      "#56949f", "#907aa9", "#d7827e", "#575279",
      "#9e93a8", "#b4637a", "#286b83", "#ea9d34",
      "#56949f", "#907aa9", "#d7827e", "#575279",
    ],
  },
  "vesper": {
    name: "vesper",
    display_name: "Vesper",
    bg: "#101010",
    bg_dim: "#0a0a0a",
    fg: "#c9c9c9",
    fg_dim: "#7c7c7c",
    border: "#222222",
    border_focus: "#fdc173",
    cursor: "#fdc173",
    accent: "#fdc173",
    ok: "#6a9389",
    warn: "#fdc173",
    error: "#c66b6b",
    ansi: [
      "#101010", "#c66b6b", "#6a9389", "#fdc173",
      "#86afc4", "#ad89b5", "#5fa89a", "#c9c9c9",
      "#575757", "#d98888", "#86b3a8", "#fed59a",
      "#a3c5d6", "#c4a8cc", "#7ec0b3", "#ffffff",
    ],
  },
  "nord": {
    name: "nord",
    display_name: "Nord",
    bg: "#2e3440",
    bg_dim: "#242a35",
    fg: "#d8dee9",
    fg_dim: "#899cc0",
    border: "#3b4252",
    border_focus: "#81a1c1",
    cursor: "#d8dee9",
    accent: "#81a1c1",
    ok: "#a3be8c",
    warn: "#ebcb8b",
    error: "#bf616a",
    ansi: [
      "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b",
      "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
      "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b",
      "#5e81ac", "#b48ead", "#8fbcbb", "#eceff4",
    ],
  },
  "dracula": {
    name: "dracula",
    display_name: "Dracula",
    bg: "#282a36",
    bg_dim: "#1e1f29",
    fg: "#f8f8f2",
    fg_dim: "#7b8fcd",
    border: "#44475a",
    border_focus: "#bd93f9",
    cursor: "#f8f8f2",
    accent: "#bd93f9",
    ok: "#50fa7b",
    warn: "#f1fa8c",
    error: "#ff5555",
    ansi: [
      "#21222c", "#ff5555", "#50fa7b", "#f1fa8c",
      "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2",
      "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5",
      "#d6acff", "#ff92df", "#a4ffff", "#ffffff",
    ],
  },
  "terminal": {
    name: "terminal",
    display_name: "Terminal Default",
    bg: "#000000",
    bg_dim: "#121212",
    fg: "#d0d0d0",
    fg_dim: "#808080",
    border: "#404040",
    border_focus: "#00afff",
    cursor: "#ffffff",
    accent: "#00afff",
    ok: "#00d700",
    warn: "#ffd700",
    error: "#ff0000",
    ansi: [
      "#000000", "#800000", "#008000", "#808000",
      "#000080", "#800080", "#008080", "#c0c0c0",
      "#808080", "#ff0000", "#00ff00", "#ffff00",
      "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
    ],
  },
};

// ── DTO projections ──────────────────────────────────────────────────────────

function sessionDto(s: MockState, m: MockSession): SessionInfo {
  return {
    id: m.id,
    name: m.name,
    pane_count: paneCount(s, m.id),
    created_at: m.createdAt,
    last_active_at: m.lastActiveAt,
  };
}

function windowDto(w: MockWindow): WindowInfo {
  return {
    id: w.id,
    session: w.session,
    name: w.name,
    position: w.position,
    pane_count: leaves(w.layout).length,
    created_at: SEED_TS,
  };
}

function paneStateDto(p: MockPane): PaneStateInfo {
  return {
    pane: p.id,
    session: p.session,
    state: p.state,
    title: p.title,
    agent: p.agent,
    name: p.name,
    window: p.window,
  };
}

// ── Command dispatch ─────────────────────────────────────────────────────────

function handle(s: MockState, cmd: string, a: Args): unknown {
  switch (cmd) {
    // ── Connectivity ──
    case "daemon_status":
    case "reconnect":
      return { connected: true, socket: SOCKET } satisfies DaemonStatus;

    // ── Sessions ──
    case "list_sessions":
      return [...s.sessions.values()].map((m) => sessionDto(s, m));

    // Fake per-session git status so the chip is visible + varied in the browser
    // mock loop. Seeded sessions get distinct shapes (dirty / ahead vs behind);
    // any other session falls back to a clean `main`. Returning a value (never
    // null) keeps the chip on screen — null ("not a repo") is reserved for a
    // manual one-off test. Reads `args.session`, matching the real `{ session }`.
    case "git_status": {
      const session = reqStr(a, "session");
      const seeded: Record<string, GitInfo> = {
        "sess-dev": { branch: "main", dirty: 3, ahead: 1, behind: 0, upstream: "origin/main" },
        "sess-infra": { branch: "feat/windows", dirty: 0, ahead: 0, behind: 2, upstream: "origin/feat/windows" },
      };
      return (
        seeded[session] ??
        ({ branch: "main", dirty: 0, ahead: 0, behind: 0, upstream: "origin/main" } satisfies GitInfo)
      );
    }

    case "spawn_session": {
      const sess = makeSession(s);
      const win = makeWindow(s, sess.id, "1", 0);
      const pane = makePane(s, {
        session: sess.id,
        window: win.id,
        state: "idle",
        agent: "shell",
      });
      win.layout = leaf(pane.id);
      return { session: sess.id, pane: pane.id };
    }

    case "rename_session": {
      const sess = s.sessions.get(reqStr(a, "session"));
      if (sess) sess.name = reqStr(a, "name");
      return undefined;
    }

    case "close_session": {
      const session = reqStr(a, "session");
      for (const w of sessionWindows(s, session)) {
        for (const pane of leaves(w.layout)) s.panes.delete(pane);
        s.windows.delete(w.id);
      }
      s.sessions.delete(session);
      return undefined;
    }

    case "session_layout": {
      const win = sessionWindows(s, reqStr(a, "session"))[0];
      if (!win || !win.layout) throw new Error("mock daemon: session has no layout");
      return clone(win.layout);
    }

    // ── Windows ──
    case "list_windows":
      return sessionWindows(s, reqStr(a, "session")).map((w) => windowDto(w));

    case "new_window": {
      const session = reqStr(a, "session");
      const existing = sessionWindows(s, session);
      const name = optStr(a, "name") ?? String(existing.length + 1);
      const win = makeWindow(s, session, name, existing.length);
      return win.id; // EMPTY window; caller follows with open_pane
    }

    case "rename_window": {
      const win = s.windows.get(reqStr(a, "window"));
      if (win) win.name = reqStr(a, "name");
      return undefined;
    }

    case "close_window": {
      const win = s.windows.get(reqStr(a, "window"));
      if (win) {
        for (const pane of leaves(win.layout)) s.panes.delete(pane);
        s.windows.delete(win.id);
        // Daemon evicts a session left with zero windows.
        if (sessionWindows(s, win.session).length === 0) {
          s.sessions.delete(win.session);
        }
      }
      return undefined;
    }

    case "get_window_layout": {
      const win = s.windows.get(reqStr(a, "window"));
      if (!win || !win.layout) throw new Error("mock daemon: window has no panes yet");
      return clone(win.layout);
    }

    // ── Panes ──
    case "pane_states":
      return [...s.panes.values()].map(paneStateDto);

    case "rename_pane": {
      const pane = s.panes.get(reqStr(a, "pane"));
      if (pane) pane.name = reqStr(a, "name");
      return undefined;
    }

    case "close_pane": {
      const paneId = reqStr(a, "pane");
      const win = windowOfPane(s, paneId);
      if (win && win.layout) {
        win.layout = removeLeaf(win.layout, paneId);
        if (!win.layout) s.windows.delete(win.id); // window emptied
      }
      s.panes.delete(paneId);
      return undefined;
    }

    case "open_split": {
      const parent = reqStr(a, "pane");
      const dir = reqStr(a, "direction");
      if (dir !== "h" && dir !== "v") {
        throw new Error(`mock daemon: invalid direction "${dir}"`);
      }
      const win = windowOfPane(s, parent);
      if (!win || !win.layout) throw new Error("mock daemon: split target not found");
      const np = makePane(s, {
        session: win.session,
        window: win.id,
        state: "idle",
        agent: "shell",
      });
      win.layout = splitLeaf(win.layout, parent, dir, np.id);
      return { pane: np.id };
    }

    case "open_pane": {
      const window = reqStr(a, "window");
      const session = reqStr(a, "session");
      const win = s.windows.get(window);
      if (!win) throw new Error("mock daemon: open_pane unknown window");
      const np = makePane(s, {
        session,
        window,
        state: "idle",
        agent: "shell",
      });
      if (!win.layout) {
        win.layout = leaf(np.id); // first pane of a fresh window
      } else {
        // A populated window gaining another pane: append as a sibling column.
        win.layout = {
          kind: "split",
          dir: "v",
          children: [win.layout, leaf(np.id)],
          weights: [50, 50],
        };
      }
      return { pane: np.id };
    }

    case "set_weight": {
      const win = windowOfPane(s, reqStr(a, "pane"));
      const weight = Math.max(5, Math.min(95, reqNum(a, "weight")));
      if (win && win.layout) setLeafWeight(win.layout, reqStr(a, "pane"), weight);
      return undefined;
    }

    case "resize_pane":
    case "attach_pane_stream":
    case "detach_pane_stream":
    case "send_keys":
      return undefined; // no PTY in the mock; the terminal echoes locally

    // ── Process inspection ──
    case "inspect_pid": {
      const pane = s.panes.get(reqStr(a, "pane"));
      if (!pane) throw new Error("mock daemon: inspect_pid unknown pane");
      return {
        pid: pane.pid,
        comm: pane.agent === "claude" ? "claude" : "bash",
        env: [
          ["PWD", "/home/dev/pyre"],
          ["TERM", "xterm-256color"],
          ["PYRE_PANE", pane.id],
        ],
        fds: ["/dev/pts/3", "pipe:[44210]", "pipe:[44211]"],
        children: [pane.pid + 1],
      } satisfies PidInfo;
    }

    // ── Blocks ──
    case "list_blocks": {
      const pane = s.panes.get(reqStr(a, "pane"));
      return pane ? pane.blocks.map((b) => clone(b.block)) : [];
    }

    case "block_stdout": {
      const id = reqStr(a, "block");
      for (const pane of s.panes.values()) {
        const hit = pane.blocks.find((b) => b.block.id === id);
        if (hit) return hit.stdout;
      }
      return "";
    }

    case "search_blocks":
      return searchBlocks(s, a);

    // ── Themes ──
    case "list_themes":
      return clone(THEME_META);

    case "get_theme": {
      const name = reqStr(a, "name");
      return clone(THEME_PALETTES[name] ?? THEME_PALETTES.ember);
    }

    // ── GitHub account linking ──
    // The whole modal → connected → disconnect UX is iterable in the browser
    // mock loop with no real GitHub — but the mock is an HONEST demo: it NEVER
    // auto-authorizes. device_start hands back a fixed code + interval 1 (fast)
    // and resets to not-authorized; poll returns `pending` FOREVER until the
    // demo's "Simulate authorization" button calls `mockAuthorizeGitHub()`; only
    // then does poll report `authorized` and `github_account` return @mockuser.
    // disconnect clears the flag. This keeps the device-code modal on screen so a
    // tester can read it and can't mistake the mock for the real GitHub flow.
    case "github_account":
      return s.github.authorized ? clone(MOCK_GH_ACCOUNT) : null;

    case "github_device_start":
      s.github.authorized = false; // honest demo: never pre-authorized
      return {
        user_code: "WDJB-MJHT",
        verification_uri: "https://github.com/login/device",
        expires_in: 900,
        interval: 1,
      } satisfies GhDeviceStart;

    case "github_device_poll":
      // Pending until the demo button explicitly authorizes — no auto-complete.
      return s.github.authorized
        ? ({ status: "authorized" } satisfies GhPoll)
        : ({ status: "pending" } satisfies GhPoll);

    case "github_disconnect":
      s.github.authorized = false;
      return undefined;

    // ── Lifecycle long-poll ──
    case "poll_events": {
      // Quiet: no async lifecycle events in the mock. The periodic poll in
      // main.ts keeps the UI fresh; returning the same cursor avoids a busy loop.
      const after = typeof a["after_seq"] === "number" ? a["after_seq"] : 0;
      return { events: [], last_seq: after } satisfies PollEventsResult;
    }

    // ── Misc fire-and-forget (notify.ts, main.ts) ──
    case "notify":
    case "close_splash":
    case "start_pane":
      return undefined;

    default:
      console.warn(`[pyre-mock] unhandled command "${cmd}" — returning undefined`);
      return undefined;
  }
}

function makeSession(s: MockState): MockSession {
  const id = nextId(s, "sess");
  const count = s.sessions.size + 1;
  const sess: MockSession = {
    id,
    name: `session ${count}`,
    createdAt: SEED_TS,
    lastActiveAt: SEED_TS,
  };
  s.sessions.set(id, sess);
  return sess;
}

function makeWindow(
  s: MockState,
  session: string,
  name: string,
  position: number,
): MockWindow {
  const win: MockWindow = {
    id: nextId(s, "win"),
    session,
    name,
    position,
    layout: null,
  };
  s.windows.set(win.id, win);
  return win;
}

function searchBlocks(s: MockState, a: Args): Array<{ block: Block; snippet: string }> {
  const query = reqStr(a, "query").toLowerCase();
  const failuresOnly = a["failures_only"] === true;
  const session = optStr(a, "session");
  const hits: Array<{ block: Block; snippet: string }> = [];
  for (const pane of s.panes.values()) {
    if (session && pane.session !== session) continue;
    for (const b of pane.blocks) {
      if (failuresOnly && (b.block.exit_code ?? 0) === 0) continue;
      const hay = `${b.block.command}\n${b.stdout}`.toLowerCase();
      const at = hay.indexOf(query);
      if (query && at < 0) continue;
      const start = Math.max(0, at - 20);
      const snippet = b.stdout.slice(start, start + 80) || b.block.command;
      hits.push({ block: clone(b.block), snippet });
    }
  }
  return hits;
}

// ── Public transport surface (consumed by ./invoke) ──────────────────────────

/**
 * Delay applied to `poll_events` responses in the mock. The real daemon
 * long-polls the request open until an event arrives OR a timeout elapses
 * (~750 ms in practice). Without an equivalent delay the `while` loop in
 * `runEventLoop()` (main.ts) spins as an **infinite microtask chain**:
 * `Promise.resolve()` resolves in the microtask queue, which the browser
 * drains to completion before returning to the macrotask queue. This starves
 * `setTimeout`/`setInterval` callbacks, painting, and the `load` event,
 * leaving the page blank and frozen. Using `setTimeout` here yields back to
 * the macrotask queue each iteration, so the browser can render and timers
 * can fire normally.
 */
const MOCK_POLL_DELAY_MS = 750;

/** Mock replacement for Tauri's `invoke`. Resolves with the command's DTO. */
export function mockInvoke<T>(cmd: string, args?: Args): Promise<T> {
  const result = handle(model(), cmd, args ?? {});
  // poll_events must not resolve instantly — see MOCK_POLL_DELAY_MS above.
  if (cmd === "poll_events") {
    return new Promise<T>((resolve) =>
      setTimeout(() => resolve(result as T), MOCK_POLL_DELAY_MS),
    );
  }
  return Promise.resolve(result as T);
}

/** Mock replacement for Tauri's `listen`. No push events in the mock — returns a
 *  no-op unlisten so subscribers (pty-output, pane-closed) wire up harmlessly. */
export function mockListen(): Promise<() => void> {
  return Promise.resolve(() => {
    /* nothing to unsubscribe */
  });
}

/**
 * Mock-ONLY demo affordance: explicitly authorize the in-memory GitHub flow.
 * The device-code modal's "Simulate authorization" button calls this (dynamically
 * imported, so the real bundle never pulls in this module). After this, the next
 * `github_device_poll` tick reports `authorized` and `github_account` returns the
 * mock account, driving the normal success path (modal closes, chip shows
 * @mockuser). Idempotent and side-effect-free beyond flipping the flag; safe to
 * call more than once. Has no effect in production — the whole module is dev-only
 * and tree-shaken when VITE_MOCK is unset.
 */
export function mockAuthorizeGitHub(): void {
  model().github.authorized = true;
}

// Reset the in-memory mock state on Vite hot-reload so demo state (e.g. a
// simulated GitHub auth) doesn't linger across edits. Prod builds have no
// import.meta.hot, so this compiles out.
if (import.meta.hot) {
  import.meta.hot.dispose(() => {
    state = null;
  });
}
