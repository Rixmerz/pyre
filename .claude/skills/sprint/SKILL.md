---
name: sprint
description: Wave-based parallel sprint orchestrator. PRIMARY caller is /jig-goal — it invokes /sprint internally when it classifies a task as multi-domain vertical slice. Manual /sprint <task> invocation is still supported when the user explicitly wants parallel waves. TRIGGER when the work spans multiple domains (e.g. backend + frontend, API + DB migration + UI), has independent streams that could run in parallel, or is framed as a vertical slice. SKIP for single-file edits, bug fixes, refactors within one module, or any task one subagent could finish in one phase (just describe the task — auto-orchestration picks the workflow). SKIP if no active jig workflow infrastructure is desired. Reads `next_task_get` for continuity across /clear.
user-invocable: true
argument-hint: "[sprint description]"
---

# Sprint

Plan and execute a wave-based, parallel sprint. When auto-fired, the
user's most recent request is the sprint scope; when invoked explicitly
via the user typing the slash form, `$ARGUMENTS` carries the
description.

You are the main Claude Code context. You do NOT implement code
yourself. You scope the work, build a graph, delegate each wave to
subagents, verify their output, and close out.

## Step 0 — Continuity hand-off

Call `next_task_get`. If `found: true`, inspect the summary:

- **Post-handoff execution mode.** If the summary contains
  `graph_id=` or starts with `Sprint plan ready`, this turn is the
  fresh execution context spawned by Step 3.5 of a prior planning
  turn. Skip Steps 0.5 — 3.5 entirely and jump to Step 4. The
  graph, goal, and milestone are persisted; you do not re-plan.

- **Planning mode with prior context.** Otherwise read the injection
  block — it preserves the user's previous task summary across
  `/clear`. Use it to inform scope decisions: do not redo done work,
  align with prior choices.

If `found: false`, proceed with planning from scratch.

## Step 0.5 — Scope check (do this before planning waves)

State the user-visible deliverable in one sentence. Example: "User can
create, edit, and archive playlists from the web UI, persisted to the
database."

Then test scope:

- **Too small for a sprint** if the deliverable is "add a field", "fix
  a bug", "rename a function across the repo", or anything a single
  subagent could finish in one phase. Stop and tell the user to just
  describe the task without ceremony — auto-orchestration (JIG_AUTO_ACTIVATE)
  picks a workflow if one fits.
- **Right-sized for a sprint** if the deliverable is a vertical slice
  that touches at least two of: domain model, backend handlers,
  frontend UI, integration tests, infra/migrations.
- **Too big for one sprint** if the deliverable would take more than a
  few subagents per wave, or if waves cannot enumerate cleanly. Suggest
  the user break it into 2–3 sprints with explicit hand-off summaries.

If the scope is wrong, stop here. Do not build a graph for under-scoped
work — the ceremony will outweigh the value.

### Multi-surface trigger

If the vertical slice crosses a service boundary (backend ↔ frontend,
service ↔ service, MCP server ↔ client), do NOT build a graph by hand.
Activate the canonical sprint-e2e graph:

```
graph_activate(graph_id="sprint-e2e")
```

That graph encodes the Contract + E2E phases that catch wire-format
drift between parallel implementation agents (the pharma-MVP 422 class
of bug). See `.claude/runbooks/orchestrator/workflow-catalog.md` for the
full default-graph catalog.

Multi-surface signals (any one is enough):
- The Implementation wave will contain ≥2 agents from
  `{backend, frontend, mcp-developer, db-migrator}` writing in parallel.
- The deliverable mentions both a server surface and a client surface
  (login, form, page, endpoint, API, mobile).
- `--needs-e2e` is present in `$ARGUMENTS` (the canonical hint
  /jig-goal passes for `multi` strategy).
- jig-goal classified the task as `multi`.

If any of those fires, skip Step 1 wave-planning and Step 2 graph-build
below — jump straight to `graph_activate(graph_id="sprint-e2e")` and
drive the existing nodes via `graph_traverse`.

## Step 1 — Plan the waves (minimum-waves principle)

Group all the work into the **smallest** number of waves that respect
dependencies. Defaults that bias toward fewer, more substantial waves:

- A **2-wave** sprint (Implementation + Validation) is fine and often optimal.
- A **3-wave** sprint typically looks like Foundation → Implementation
  (parallel backend + frontend agents in the same wave) → Validation.
- A **4+ wave** sprint should be rare. If you reach it, ask whether the
  deliverable is actually one sprint or two.

Wave shapes (use only the ones the deliverable actually requires — do
not pad):

| Wave | Purpose | Skip when |
|------|---------|-----------|
| Foundation | Domain types, migrations, shared schemas | No new shared types/schema needed |
| Contract (pre-wave) | OpenAPI yaml + generated TS types + curl cheat-sheet | Single-surface sprint (only backend OR only frontend) |
| Implementation | Backend handlers, frontend UI, glue — parallel agents in the same wave when files do not collide | Trivial vertical |
| E2E Tests (cross-cutting) | Live backend + live frontend, scripted user flows, contract diff | Single-surface sprint; otherwise MANDATORY |
| Validation | Single serial agent runs the project check pipeline (build, lint, type-check, tests) | Never skip — this is the gate |
| Docs | README, changelog, status updates | Internal-only change |

**Tests travel with their implementation by default.** The backend
agent in Implementation writes the backend's tests in the same wave.
The frontend agent does the same. Only spin up a separate Tests wave
when the testing is genuinely cross-cutting (E2E that spans multiple
agents' surfaces, contract tests between services).

**When the sprint is multi-surface** (≥2 of backend/frontend/migration/mcp),
the E2E wave is **mandatory** and is NOT replaced by in-wave unit tests —
unit tests cannot catch wire-format drift between services. Use the
`sprint-e2e` default graph rather than hand-rolling the waves.

**Validation is one serial agent**, not a parallel wave. Build + lint +
type-check + tests in one place avoids coordination overhead and makes
failures easy to read.

## Step 2 — Build the graph (via jig's internal proxy)

> **Skip this whole step** if Step 0.5's multi-surface trigger fired. The
> bundled `sprint-e2e` graph already encodes the canonical
> orient → contract → implement → e2e → validate → close shape. Activate
> it with `graph_activate(graph_id="sprint-e2e")` and drive its nodes via
> `graph_traverse`. Hand-built graphs are for genuinely unique shapes that
> none of the default graphs in
> `.claude/runbooks/orchestrator/workflow-catalog.md` covers.

The graph builder tools live behind the `graph` internal proxy — not on
jig's top-level surface.

1. Discover schemas:
   ```
   proxy_tools_search(query="graph_builder")
   ```

2. Create the graph:
   ```
   execute_mcp_tool("graph", "graph_builder_create", {
     "graph_id": "sprint-<short-slug>",
     "name": "<descriptive name>",
     "description": "<one-line deliverable>"
   })
   ```

3. Add one node per wave with:
   - `id` (e.g. `wave_implementation`)
   - `name`
   - `prompt_injection` — the exact instructions you will paste into
     each subagent of that wave (file paths, acceptance criteria,
     constraints)
   - `tools_blocked` if you want to enforce read-only phases
   - Add via `execute_mcp_tool("graph", "graph_builder_add_node", {...})`

4. Add edges between waves with phrase conditions:
   ```
   execute_mcp_tool("graph", "graph_builder_add_edge", {
     "graph_id": "...",
     "from_node": "wave_implementation",
     "to_node": "wave_validation",
     "id": "impl_to_validate",
     "phrase": "implementation complete"
   })
   ```

5. Validate + save:
   ```
   execute_mcp_tool("graph", "graph_builder_validate", {"graph_id": "..."})
   execute_mcp_tool("graph", "graph_builder_save", {"graph_id": "..."})
   ```
   Writes YAML to `<project>/.claude/workflows/<graph_id>.yaml`.

6. Activate:
   ```
   graph_activate(graph_id="<your id>")
   ```

> **Why a proxy:** the graph builder API is large (10+ tools) and most
> sessions never touch it. Proxying keeps jig's top-level surface tight
> while staying one search away.

## Step 3 — Present the plan

Before kicking anything off, show the user:

```
Deliverable: <one sentence>

| Wave | Purpose | Agents | Parallel? |
|------|---------|--------|-----------|
```

Ask for approval. The user might want to merge waves, change agents, or
change scope. Wait for their go-ahead before building.

## Step 3.5 — Hand off planning context to fresh execution

By the time the graph is built and approved, this conversation carries
the classification reasoning, MoSCoW scope, wave-table draft, and graph
build calls — none of which the execution phase needs. The graph,
goal_state, and a one-line milestone summary are the only artifacts
that must survive. Restart the turn before dispatching subagents.

This step is **mandatory** when:

- The sprint activated the bundled `sprint-e2e` graph (multi-surface
  trigger fired), OR
- The hand-built graph has ≥3 nodes, OR
- The user's request reached /sprint via /jig-goal with `multi`
  strategy (i.e. `$ARGUMENTS` contains `--needs-e2e`).

Skip only for a 2-node hand-built sprint where planning bloat is
negligible.

1. Persist continuity. Be explicit so Step 0 of the fresh turn
   detects execution mode:
   ```
   next_task_record(
       summary=(
           "Sprint plan ready. "
           f"graph_id=<active graph id>, start_node=<first node name>. "
           f"Milestone: <one-line description of the slice this sprint covers>. "
           f"Waves: <comma-separated wave names>. "
           "Next: graph_status -> graph_traverse -> dispatch wave per "
           "the active node's prompt_injection."
       ),
       task_description=<the original sprint argument verbatim>,
       files_changed=[]    # nothing implemented yet
   )
   ```
   The `graph_id=` and `Sprint plan ready` markers are what Step 0
   of the next turn matches on to skip back to Step 4.

2. Resolve the tmux session. `tmux display-message` is whitelisted by
   `delegation_gate.py`; run it directly:
   ```bash
   tmux display-message -p -t "$TMUX_PANE" '#S'
   ```
   ABORT the handoff if the trimmed result is empty, starts with `%`
   (pane id), or is a bare digit. Those are fallback values from a
   failed lookup and would send the handoff worker against a
   nonexistent session.

3. Hand off:
   ```
   tmux_clear_and_prompt(
       session=<resolved session>,
       prompt=(
           "Resume sprint execution. Read next_task_get to see the "
           "active graph_id and milestone. Then call graph_status and "
           "graph_traverse the next phase. Dispatch subagents per the "
           "active node's prompt_injection."
       )
   )
   ```

4. Stop. Emit nothing else. `/clear` fires immediately and a fresh
   Claude turn picks up.

## Step 4 — Execute waves (post-handoff, fresh context)

You arrive here either after Step 3.5's handoff (the normal path for
multi-node sprints) or directly from Step 3 (only for tiny 2-node
hand-built sprints where Step 3.5 was skipped).

Sanity-check the resume state before dispatching anything:

1. `next_task_get` — confirm the summary matches the workflow you are
   about to drive.
2. `goal_get` — confirm the goal is `active`.
3. `graph_status` — confirm the active workflow and current node.

If any of those is missing, stop and report — do not guess.

For each wave:

1. `graph_traverse` into the next wave's node. The fresh context
   arrives on whatever node `graph_status` reports as active (usually
   the start node).
2. Read the wave's `prompt_injection`.
3. Launch all agents within the wave **concurrently** with
   `run_in_background: true`. Each agent receives:
   - The wave's `prompt_injection`.
   - The user's original sprint description verbatim.
   - The Step 0 continuity injection if any.
   - Concrete file paths to read first (use Glob/Grep yourself if you
     need to surface them — do not make agents guess).
   - Acceptance criteria for *this wave only*.
   - Self-contained context (assume zero shared memory with sibling
     agents).
   - "Write the tests for your own work in this same wave unless told
     otherwise."
4. **Never** launch agents in the same wave that write to the same
   files — they will conflict.
5. After all agents complete, **verify their output** before advancing.
   Read the files they claim to have changed. Do not trust summaries.
6. If the analysis provider reports new high/critical findings, resolve
   them before traversing.

## Step 5 — Validate (single agent, serial)

Run the project's check pipeline through one agent (or directly if
simple):

- Build: `npm run build`, `cargo check`, `pytest --collect-only`, etc.
- Lint: `ruff check`, `eslint`, `clippy`.
- Type check: `mypy`, `tsc --noEmit`, etc.
- Tests: `pytest`, `npm test`, `cargo test`.

If anything fails, fix the root cause before declaring done. Do not
split the validation across parallel agents — failures are easier to
read and act on when serial.

## Step 6 — Close out

1. `graph_reset` to clear the workflow.
2. **Save the hand-off:** call `next_task_record(summary=<final
   summary>, task_description=<original sprint description>,
   files_changed=[<paths>])`. This is what makes the next `/clear` +
   new sprint preserve continuity.
3. Final summary for the user (3–5 sentences): deliverables shipped,
   files changed, validation result.

## Recovery — if jig MCP disconnects mid-sprint

If `graph_status` / `graph_reset` become unreachable, the user can
clear persisted graph state from a terminal:

```bash
jig graph reset --project <PROJECT_PATH>
```

After they restart Claude Code, you can resume.

## Hard rules

- Run Step 0.5 scope check first. Refuse to build a sprint for trivial
  work — recommend just describing the task (auto-orchestration handles it).
- Bias toward **fewer, more substantial waves**. Two waves is often
  correct.
- Tests travel with their implementation by default; separate Tests
  wave only for cross-cutting testing.
- Validation is **one serial agent**, never parallel.
- For multi-surface sprints, **Validation MUST NOT traverse until E2E is
  green**. A 422 on the login or primary flow indicates contract drift —
  dispatch `fixer` against the contract artifact, not against either silo
  in isolation. The bundled `sprint-e2e` graph encodes this edge.
- The `sprint-e2e` graph's `e2e` phase is **real-browser by default**
  (Playwright/Cypress). Mock-network suites (msw, httpx replay,
  in-process `app.request`) DO NOT satisfy the "e2e green" signal for
  auth, cookie, redirect, CORS, or upload flows — they miss the exact
  class of bug E2E exists to catch.
- You are the main context — delegate **all** implementation to
  specialized subagents.
- Launch independent agents within a wave concurrently with
  `run_in_background: true`.
- Never launch agents that write to the same files in the same wave.
- After all agents in a wave complete, verify their output before
  advancing.
- Do not commit until the Validation wave passes. Do not commit unless
  the user explicitly asks.
- If the analysis provider reports new high/critical findings in
  Validation, fix before declaring done.
- Architecture review criteria come from project CLAUDE.md, not
  invented ad-hoc.
- Always end with `next_task_record` so `/clear` is safe between sprints.
