---
name: context-swarm
description: Build project context in parallel by deploying dcc-specialist and livespec-specialist as Task subagents. Each produces a notion file under .claude/notions/ so the main agent can reason over digests instead of source.
---

# /context-swarm

Goal: in one shot, hydrate the main agent's view of the project with both
**structural** (livespec — symbols, call graph, RFs, frameworks) and
**quality** (DCC — centrality, smells, tensions, debt, clones) context.
Run the two specialists **in parallel** via the Task tool.

## Execute exactly

Spawn all available specialists in a single message (parallel tool calls).
Always include livespec + dcc. Also include `sca-specialist` **only if**
the `sca` proxy is **available** (per `proxy_list`):

- Available = entry exists AND `last_error` is null. `connected: false`
  alone is NOT a skip signal — subprocess proxies spawn lazily on first
  call, so they all read `connected: false` while idle.
- Skip = entry missing, OR `last_error` non-null (e.g. `"init failed:
  Connection closed"`).

If sca is unavailable, skip silently and proceed with the other two.

1. **Task → `livespec-specialist`** with prompt:

   > Follow runbook `livespec-specialist/find-call-graph.md` for shape;
   > but the deliverable here is a project overview, not a per-symbol
   > query. Produce `.claude/notions/livespec-overview.md` covering:
   > languages + modules, top 10 symbols by PageRank (filter
   > infrastructure), framework entry points (`find_endpoints`), RF
   > coverage (`audit_coverage`, summary_only=True), dead-code
   > candidates (`find_dead_code`, summary_only=True). Index first if
   > needed (`index_project(force=false)`). ≤ 200 lines.

2. **Task → `dcc-specialist`** with prompt:

   > Follow runbook `dcc-specialist/quality-snapshot.md`. Project root
   > is `<absolute path to this project — pass it explicitly, do NOT
   > rely on cwd>`. ALWAYS start with
   > `cube_index_directory(path=<project root>, recursive=true)` —
   > DCC's database is global and shared across projects; skipping the
   > explicit index will mix in another repo's metrics. Produce
   > `.claude/notions/dcc-snapshot.md`. ≤ 200 lines.

3. **Task → `sca-specialist`** *(only if `sca` proxy is connected)* with prompt:

   > Follow runbook `sca-specialist/semantic-overview.md`. Produce
   > `.claude/notions/sca-overview.md` using the response format
   > defined there. If the atlas is empty, run `atlas_reindex(args={})`
   > before producing the notion. ≤ 200 lines.

After all finish, read every notion file written and produce a one-paragraph
*executive summary* to the user: dominant tech, biggest risk, where to
look first. Do not re-read source.

## When to use

- First contact with an unfamiliar repo.
- After a long break (notion files older than ~24h or post big merge).
- Before a refactor — establish baseline before changes.

## When NOT to use

- The notions already exist and are < 24h old (read them instead).
- The repo is tiny (< 20 files) — main agent's project-tree notion is
  enough; spawning both specialists is overkill.
- You're mid-workflow phase and the active node prohibits exploration —
  resolve the workflow gate first.

## Parallel execution

The Task tool calls MUST go out in a single assistant message. If you
send them sequentially the swarm becomes a queue and you waste the
parallelization win. Use `run_in_background: true` on both if you have
unrelated work to do while they run.
