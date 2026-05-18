---
id: quality-snapshot
scope: dcc-specialist
when: "produce .claude/notions/dcc-snapshot.md · /context-swarm fan-out · pre-refactor baseline"
tools: [cube_get_stats, cube_analyze_graph, cube_detect_smells, cube_get_tensions, cube_get_debt, cube_index_directory]
sla: ~90s wall
---

## Steps

1. **Always** run `cube_index_directory(path=<absolute project root>, recursive=true)` first. DCC's database is global (`~/.local/share/jig/dcc.db`) and shared across projects — `cube_get_stats()` returns counts for ALL indexed files, not just the current project. Skipping the explicit index call risks reporting another project's metrics by accident.
2. `cube_get_stats()` → record total_files / LoC AFTER the targeted index. Verify the count matches the project's expected file count — if it's wildly different (e.g. 124 files when you indexed a 4-file repo), the global DB is mixing projects; filter results by `path` prefix in subsequent calls or note the discrepancy in the notion.
3. `cube_analyze_graph(top_n=5)` — top files by PageRank, hub, authority. Keep `top_n` small to avoid MCP chunk limit.
4. `cube_detect_smells(min_severity='medium', summary_only=True)` for counts; then a second call with `summary_only=False, limit=10` for detail on critical/high.
5. `cube_get_tensions(status='detected')` — pending anomalies.
6. `cube_get_debt()` — codebase grade + top-3 offenders. If chunk-limit error → skip; record gap in notion.
7. Write notion `.claude/notions/dcc-snapshot.md` using the response format below. Include the project root path in the `sources_scanned` frontmatter so reviewers can verify scope.

## Tools (specific calls)

```
execute_mcp_tool("deltacodecube", "cube_get_stats", {})
execute_mcp_tool("deltacodecube", "cube_analyze_graph", {"top_n": 5})
execute_mcp_tool("deltacodecube", "cube_detect_smells", {"min_severity": "medium", "summary_only": true})
execute_mcp_tool("deltacodecube", "cube_detect_smells", {"min_severity": "high", "limit": 10})
execute_mcp_tool("deltacodecube", "cube_get_tensions", {"status": "detected", "limit": 20})
execute_mcp_tool("deltacodecube", "cube_get_debt", {})
```

## Response format

```markdown
---
topic: dcc-snapshot
produced_by: dcc-specialist
produced_at: <ISO8601>
sources_scanned: via dcc (<N> files, <LoC> LoC)
---

## Summary
<2-3 sentences: size + dominant domain + headline quality signal>

## Top files by centrality
- `<abs path>` — PageRank=<v>, role=<authority|hub|bridge>
- ...

## Smell census
- Critical: <n> · High: <n> · Medium: <n>
- Top offenders:
  - `<path>` — <smell_type> (<severity>): <one-line rationale>

## Tensions (detected)
- `<tension_id>` on `<file>`: <one-line>

## Debt
- Codebase grade: <A-F>
- Top-3 offenders: ...

## Open questions
- ...
```

## Failure modes

- Index empty → run `cube_index_directory` and retry.
- Chunk limit on `cube_get_debt` / `cube_analyze_graph(top_n>5)` → reduce scope, document gap in notion.
- Tensions table reports `status='detected'` count high but no recent edits → likely stale baseline; consider `cube_reindex` on listed files.
