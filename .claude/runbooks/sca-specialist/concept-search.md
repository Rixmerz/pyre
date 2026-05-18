---
id: concept-search
scope: sca-specialist
when: "'where does this codebase do X' · 'find the code that handles Y' · concept-level navigation"
tools: [atlas_search, atlas_describe, atlas_callers]
sla: ~10s per query
---

## Steps

1. Phrase the question as a **concept**, not a literal token. Bad: `def login`. Good: `"user login flow that validates credentials"`.
2. `atlas_search(args={query=<concept>, top_k=10, language?, module?, kind?})`. Filters narrow the haystack — `language='python'` or `module='auth'` when applicable.
3. For each high-scoring hit, `atlas_describe` to get the LLM summary. Decide which atom answers the question.
4. If the user is going to *modify* the found code, also run `atlas_callers(args={atom_id=<id>, depth=1})` so the orchestrator knows blast radius before dispatching an editor.

## Tools (specific calls)

```
execute_mcp_tool("sca", "atlas_search",   {"args": {"query": "<concept>", "top_k": 10}})
execute_mcp_tool("sca", "atlas_describe", {"args": {"path_or_id": "<atom_id>"}})
execute_mcp_tool("sca", "atlas_callers",  {"args": {"atom_id": "<atom_id>", "depth": 1}})
```

## Response format

```markdown
## Concept search: "<query>"

| Rank | Score | Atom | File:line | Intent (one-line) |
|------|-------|------|-----------|-------------------|
| 1 | 0.84 | `<atom_id>` | `<file>:<line>` | … |

### Recommended entry point
`<atom_id>` — <why this is the best match>
Direct callers: <n>. Touching it impacts <list of file paths>.
```

## Failure modes

- Top score < 0.5 → no good semantic match. The concept may not be implemented yet, OR the project is too small / atoms too sparse. Suggest livespec's `find_endpoints` or `find_symbol` instead.
- Top hits all in test directories → narrow with `module='<production_module>'` or filter post-hoc.
- LLM description is generic ("a function that does X") → re-run `atlas_reindex(args={phases=["describe"]})` to refresh descriptions; the underlying code may have shifted.
