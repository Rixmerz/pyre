---
id: tension-triage
scope: dcc-specialist
when: "cube_get_tensions returns detected items · graph_traverse blocked by tension gate · post-edit anomaly"
tools: [cube_get_tensions, cube_suggest_fix, cube_reindex, cube_resolve_tension]
sla: ~20s per tension
---

## Steps

1. `cube_get_tensions(status='detected', limit=50)` — list pending.
2. For each tension (high severity first):
   - `cube_suggest_fix(tension_id=<id>)` — get change_type, severity, likely_causes, suggested_actions, code snippets.
   - Decide: real issue, false positive, or intentional.
3. If real: hand off to language-specialist (backend-python / backend-go / …) with the suggest_fix payload as constraint. After fix lands:
   - `cube_reindex(path=<both endpoints>)`.
   - `cube_resolve_tension(tension_id=<id>, status='resolved')`.
4. If false positive / intentional:
   - `cube_resolve_tension(tension_id=<id>, status='ignored')`.
   - Document reason in `.claude/notions/dcc-tensions-log.md` (append-only).
5. If post-fix `cube_get_tensions` still lists the same id → reindex didn't run on the correct path; retry.

## Tools (specific calls)

```
execute_mcp_tool("deltacodecube", "cube_get_tensions",   {"status": "detected", "limit": 50})
execute_mcp_tool("deltacodecube", "cube_suggest_fix",    {"tension_id": "<id>"})
execute_mcp_tool("deltacodecube", "cube_reindex",        {"path": "<abs>"})
execute_mcp_tool("deltacodecube", "cube_resolve_tension",{"tension_id": "<id>", "status": "resolved"})
```

## Response format

```markdown
## Tension triage report

| id | endpoints | severity | verdict | action |
|----|-----------|----------|---------|--------|
| t-001 | a.py ↔ b.py | high | real | dispatched backend-python |
| t-002 | x.go ↔ y.go | medium | ignored | documented in dcc-tensions-log.md |
```

## Failure modes

- `suggest_fix` returns generic guidance only → inspect both endpoint files via livespec `quick_orient` first.
- Tension reappears after reindex → baseline distance hasn't shifted enough; consider editing both endpoints together.
- Mass tensions (>20 detected at once) → probably stale index from a big merge; run `cube_index_directory` full pass then re-evaluate.
