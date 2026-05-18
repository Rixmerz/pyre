---
id: refactor-impact
scope: dcc-specialist
when: "orchestrator runs pre-refactor-audit · 'is it safe to touch X' · 'what breaks if I split Y'"
tools: [cube_get_centrality, cube_analyze_impact, cube_simulate_wave, cube_predict_impact, cube_detect_smells]
sla: ~30s wall
---

## Steps

1. Confirm target file is indexed: `cube_get_position(path=<abs path>)`. If error → `cube_index_file(path=<abs path>)`.
2. `cube_get_centrality(path=<abs path>)` — interpret PageRank, in_degree, authority.
3. `cube_analyze_impact(path=<abs path>)` — who depends on this + distances.
4. `cube_simulate_wave(source_path=<abs path>, intensity=1.0)` — propagation.
5. `cube_predict_impact(path=<abs path>)` — risk + recommendations.
6. `cube_detect_smells(smell_type=None, limit=10)` filtered to target file (manual filter in response).
7. Return JSON verdict.

## Tools (specific calls)

```
execute_mcp_tool("deltacodecube", "cube_get_position",   {"path": "<abs>"})
execute_mcp_tool("deltacodecube", "cube_get_centrality", {"path": "<abs>"})
execute_mcp_tool("deltacodecube", "cube_analyze_impact", {"path": "<abs>"})
execute_mcp_tool("deltacodecube", "cube_simulate_wave",  {"source_path": "<abs>", "intensity": 1.0})
execute_mcp_tool("deltacodecube", "cube_predict_impact", {"path": "<abs>"})
```

## Response format

```json
{
  "target": "<abs path>",
  "pagerank": 0.0,
  "in_degree": 0,
  "out_degree": 0,
  "role_interpretation": "<authority|hub|bridge|leaf>",
  "blast_radius_files": ["<path>", "..."],
  "wave_intensity_by_hop": {"1": 0.0, "2": 0.0, "3": 0.0},
  "smells_in_target": [{"type": "...", "severity": "..."}],
  "verdict": "go | caution | block",
  "rationale": "<one paragraph>"
}
```

## Failure modes

- Target not indexed → index then retry.
- `cube_predict_impact` returns empty → file has no contracts; refactor is safe but verify with livespec callers.
- Wave intensity > 0.5 at hop 3 → very high coupling; recommend smaller, incremental refactor.
