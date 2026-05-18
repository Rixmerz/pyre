---
id: semantic-overview
scope: sca-specialist
when: "produce .claude/notions/sca-overview.md · /context-swarm fan-out · first contact with codebase"
tools: [atlas_overview, atlas_search, atlas_reindex]
sla: ~2 min wall (slower if atlas empty — first describe pass dominates)
---

## Steps

1. `atlas_overview(args={depth=2})` → if atom count is 0, run `atlas_reindex(args={})` (full pipeline) and retry. First indexing is slow (LLM describes each atom).
2. Run 3-5 `atlas_search` queries that surface project intent. Suggested seed queries:
   - "entry point / main function"
   - "authentication / authorization"
   - "data persistence / database access"
   - "external API client"
   - "test fixtures / mocks"
3. For each top match, `atlas_describe(args={path_or_id=<id>})` to pull the LLM description.
4. Write `.claude/notions/sca-overview.md` using the response format. Cite atom ids so a later subagent can `atlas_describe` them directly.

## Tools (specific calls)

```
execute_mcp_tool("sca", "atlas_overview", {"args": {"depth": 2}})
execute_mcp_tool("sca", "atlas_reindex",  {"args": {}})
execute_mcp_tool("sca", "atlas_search",   {"args": {"query": "user authentication flow", "top_k": 5}})
execute_mcp_tool("sca", "atlas_describe", {"args": {"path_or_id": "<atom_id>"}})
```

## Response format

```markdown
---
topic: sca-overview
produced_by: sca-specialist
produced_at: <ISO8601>
sources_scanned: via sca (<n> atoms across <m> files)
---

## Summary
<2-3 sentences synthesizing what the codebase is *about*, from LLM-described atoms.>

## Modules
- `<module>` — <atom_count> atoms, dominant intent: <phrase>

## Concept index
| Query | Top atom | File:line | Intent (one-line) |
|-------|----------|-----------|-------------------|
| auth flow | `<atom_id>` | `<file>:<line>` | … |
| db access | … | … | … |

## Notable atoms (LLM-described)
- `<atom_id>` — <one-line description>

## Open questions
- ...
```

## Failure modes

- `atlas_overview` returns `{atoms: 0}` → run `atlas_reindex` (full). If still zero, the project's languages are outside `include_languages` in `sca.config.toml`.
- LLM describe stalls → `atlas_reindex(args={phases=["index","embed"]})` to skip describe; semantic search still works on raw text vectors.
- Search returns 0 hits → query too literal; rephrase as a concept ("retry policy with backoff" beats "retry_with_backoff").
