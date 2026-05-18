---
name: auto-delegation
description: Every non-trivial request is dispatched to a specialized subagent. Subagents that touch source reindex livespec/DCC before returning so the main agent always sees fresh state.
---

# Auto-Delegation

> Always apply when `JIG_DELEGATION_ONLY=1`. Most of these rules are also
> good practice when delegation is off — only the gate enforcement is
> conditional.

## The dispatch table

Main agent picks a specialist by request shape. **Pick one — never do
the work yourself** when delegation mode is on.

| Request shape | Specialist (subagent_type) | Notes |
|---|---|---|
| "investigate / map / give me an overview" | run `/context-swarm` first; then pick by sub-question | parallel dcc+livespec |
| "what calls X?", "who uses Y?", "find dead code", "RF coverage" | `livespec-specialist` | call graph + RFs |
| "is this file too central?", "centrality / smells / tensions / debt" | `dcc-specialist` | quality geometry |
| "where does code handle X concept", "find duplicated logic", semantic search | `sca-specialist` | LLM-described atoms; requires `sca` proxy connected |
| "fix a bug in <code>" | `debugger` → `fixer` | diagnose then patch |
| "add an API endpoint / handler" | `backend-python` / `backend-go` / `backend-typescript` | language-specific |
| "Hono route / middleware / RPC client" | `hono` | multi-runtime web framework |
| "Deno task / permissions / Deploy / JSR" | `deno` | Deno 2.x runtime work |
| "Fresh route / island / partial / plugin" | `fresh` | Deno-native islands framework |
| "Prisma schema / migration / client" | `prisma` | ORM; defers raw SQL to `@dba` |
| "Neon branch / pooled connection / serverless driver" | `neon` | hosted Postgres; pairs with `@prisma` or `@dba` |
| "build a component / page / UI change" | `frontend` | |
| "write tests for X" | `tester-<lang>` | |
| "schema migration / db change" | `db-migrator` | |
| "performance regression / profile this" | `performance-engineer` | |
| "security review / scan for findings" | `security-auditor` | |
| "PR review / pre-merge audit" | `reviewer` | |
| "produce a project notion / digest" | `codebase-analyst` | writes `.claude/notions/<topic>.md` |

If the request spans multiple shapes (e.g. "add an endpoint with tests
and a migration"), fan out in **one** parallel wave with `run_in_background=true`
on independents (backend + db-migrator) and a follow-up wave for
dependents (tester after backend).

## Subagent post-conditions (mandatory)

When a subagent **edits or creates source** under the project (outside
`.claude/`), its final action before returning MUST be a reindex of the
files it touched, so the next subagent (or analyzer) sees fresh state:

1. **livespec** — call once:
   ```
   execute_mcp_tool("livespec", "index_project", {"force": false})
   ```
   The xxh3 hash check makes this cheap (no re-parse if the file body is unchanged).

2. **DCC** — per edited file:
   ```
   execute_mcp_tool("deltacodecube", "cube_reindex", {"path": "<absolute file path>"})
   ```
   This records the Delta and surfaces any new Tensions immediately —
   essential signal the main agent reads before delegating again.

If the subagent did NOT touch source (analysis-only, notion-write, doc
edit), skip reindex.

## Main agent post-conditions

After a Task returns, the main agent:

1. Reads the subagent's summary and any new files in `.claude/notions/`.
2. Checks `.claude/state/pending-reindex.txt` (managed by
   `reindex_reminder.py` hook). If non-empty, calls
   `execute_mcp_tool("livespec", "index_project", {})` and
   `cube_reindex` for each listed path BEFORE dispatching the next
   subagent. The hook clears the file after the reminder fires once.
3. Never re-reads source itself — if more context is needed, dispatch
   again.

## DON'T

- Don't dispatch the same specialist twice in a row without changing
  scope — if the first call answered the question, summarize and move on.
- Don't pass partial context to subagents. Each Task prompt is read
  with a fresh window; include file paths, prior notion paths, and
  explicit acceptance criteria.
- Don't reindex when the subagent only read source (no Edit/Write) —
  index_project is cheap but cube_reindex is not free.
- Don't skip the reindex on the assumption "the next subagent will do
  it" — they often forget. Make it part of the subagent's own contract.

## Why this matters

The whole point of delegation-only mode is brutal context economy. If
livespec/DCC indexes go stale, the next specialist returns *wrong*
context, the main agent reasons over wrong notions, and the savings
evaporate into a debugging trail. Reindex-on-edit is what keeps the
specialists' state authoritative.
