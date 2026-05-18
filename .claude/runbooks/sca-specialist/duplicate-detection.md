---
id: duplicate-detection
scope: sca-specialist
when: "'is there duplicated logic' · pre-refactor consolidation · 'find candidates to merge'"
tools: [atlas_duplicates, atlas_describe, atlas_impact]
sla: shallow ~30s · deep ~3min (LLM validation)
---

## Steps

1. `atlas_duplicates(args={min_score=0.78, mode='shallow'})` for a fast pass. Report how many candidates and their score range.
2. If the user wants higher precision, re-run with `mode='deep'` — LLM validates the top-N (default 50), filtering structural false positives.
3. For each candidate pair worth inspecting:
   - `atlas_describe(args={path_or_id=<atom_id>})` on both sides → confirm intent overlap.
   - `atlas_impact(args={atom_id=<a>})` and `atlas_impact(args={atom_id=<b>})` → understand consolidation cost.
4. Recommend a consolidation plan: which atom to keep, which callers must migrate.

## Tools (specific calls)

```
execute_mcp_tool("sca", "atlas_duplicates", {"args": {"min_score": 0.78, "mode": "shallow"}})
execute_mcp_tool("sca", "atlas_duplicates", {"args": {"min_score": 0.78, "mode": "deep"}})
execute_mcp_tool("sca", "atlas_describe",   {"args": {"path_or_id": "<atom_id>"}})
execute_mcp_tool("sca", "atlas_impact",     {"args": {"atom_id": "<atom_id>"}})
```

## Response format

```markdown
## Duplicate candidates (mode=<shallow|deep>, min_score=<v>)

| Pair | Score | A | B | Recommendation |
|------|-------|---|---|----------------|
| 1 | 0.91 | `<atom_a_id>` at `a.py:42` | `<atom_b_id>` at `b.py:71` | Consolidate into A. B has <n> callers — migrate. |

## Risky consolidations
- Pair <i> — both atoms have callers across distinct bounded contexts. Refactor as a shared utility, not in-place merge.
```

## Failure modes

- Empty result with permissive `min_score=0.5` → atlas is too small or too heterogeneous. Index more code or scope the duplicate search.
- High false positive rate in `shallow` → bump to `deep`.
- A reported "duplicate" turns out to be intentional polymorphism (e.g. interface implementations) → record in notion, lower priority, don't dispatch a fixer.
