---
id: workflow-catalog
scope: orchestrator
when: deciding which default graph to activate
tools:
  - graph_activate
  - graph_list_available
  - graph_status
---

# Default workflow graphs

Every project scaffolded by `jig init` receives the graphs below at
`.claude/workflows/*.yaml`. List them at runtime with
`graph_list_available`. Activate by id with `graph_activate(graph_id=...)`.
Pick by case, not by habit.

## debug-graph

Bug hunting: reproduce -> diagnose -> fix -> verify. Use when the user
reports "bug", "error", "regression", "broken", or pastes a traceback /
failing test output. Auto-activated by the intent classifier with high
confidence on those keywords.

Activation: `graph_activate(graph_id="debug")`.

## feature-dev-graph

Single-domain feature work. Six phases: orient -> design -> implement ->
test -> validate -> commit. Use when the deliverable lives in one domain
(only backend OR only frontend OR only CLI). Auto-fires from /jig-goal
when its strategy router classifies the task as `single`.

Activation: `graph_activate(graph_id="feature-dev")`.

## demo-feature

Lightweight three-phase graph for short demos, spikes, or proofs of
concept. No formal validate gate. Use sparingly — `feature-dev-graph`
is preferred for anything that survives the spike.

Activation: `graph_activate(graph_id="demo-feature")`.

## pr-review-graph

Pull-request review automation: load-diff -> analyze-risk -> check-tests
-> summarize. Activated by `/ultrareview` or invoked manually when
reviewing a remote branch.

Activation: `graph_activate(graph_id="pr-review")`.

## sprint-e2e-graph

Multi-surface vertical slice — the deliverable crosses at least two
service boundaries (backend + frontend, service + service, MCP server +
client). Six phases: orient -> contract -> implement (parallel) -> e2e ->
validate -> close. The `contract` pre-phase produces an OpenAPI yaml and
matching TS types so parallel implementation agents do not drift on wire
format (the pharma-MVP 422 class of bug). The `e2e` phase runs read-only
against live backend + frontend via a **live browser (Playwright or
Cypress)** and diffs observed requests against the yaml. Real-browser
E2E is MANDATORY (not httpx-replay) for any flow touching auth, cookies
(`Secure`/`SameSite`/`HttpOnly`), CSRF, OAuth/OIDC redirects,
cross-origin requests, file upload/download, or service workers —
in-process mocks miss cookie drops on http://localhost, CORS preflight
rejections, redirect loops, and missing same-origin proxies.

Activation: `graph_activate(graph_id="sprint-e2e")`. Auto-fires from
/sprint when invoked with `--needs-e2e` or when the multi-surface
heuristic detects two or more service surfaces in the deliverable.

## When no graph fits

Build a one-off graph via the internal proxy:
```
proxy_tools_search(query="graph_builder")
execute_mcp_tool("graph", "graph_builder_create", {...})
```
Save it under `.claude/workflows/` so the next session reuses it.
