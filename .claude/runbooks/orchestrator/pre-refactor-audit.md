---
id: pre-refactor-audit
scope: orchestrator
when: "user wants to refactor / split / extract / consolidate · 'too coupled' · 'too big' · 'hard to test'"
tools: [Task, execute_mcp_tool]
sla: ~4 min wall, ~60k subagent tokens
---

## Steps

1. Read latest `.claude/notions/dcc-snapshot.md` and `livespec-overview.md`. If stale (> 24h or post-merge) → `/context-swarm` first.
2. **Dispatch dcc-specialist** with the target file/module to get centrality + blast radius + smells specific to the target.
3. **Dispatch livespec-specialist** with the target symbol/file to get callers + RFs touching it.
4. **Synthesize go/no-go**: if target has high PageRank + many RFs + active tensions → refactor is risky, propose smaller scope.
5. **If go**: spawn the language-specific backend specialist with the refactor plan (split / extract / move + constraints).
6. **After edits**: ensure reindex hooks fire. Optionally rerun dcc-specialist to compare before/after debt grade.

## Tools (specific calls)

```
Task(subagent_type="dcc-specialist", prompt="""
Audit target before refactor:
  execute_mcp_tool('deltacodecube','cube_get_centrality',{'path': '<abs path>'})
  execute_mcp_tool('deltacodecube','cube_analyze_impact',{'path': '<abs path>'})
  execute_mcp_tool('deltacodecube','cube_simulate_wave',{'source_path': '<abs path>', 'intensity': 1.0})
  execute_mcp_tool('deltacodecube','cube_predict_impact',{'path': '<abs path>'})
  execute_mcp_tool('deltacodecube','cube_detect_smells',{'min_severity': 'medium', 'limit': 20})
Return JSON: {pagerank, in_degree, blast_radius_files, smells_in_target, recommendation: go|caution|noop}
""")

Task(subagent_type="livespec-specialist", prompt="""
Pre-refactor structural audit:
  execute_mcp_tool('livespec','analyze_impact',{'target_type':'file','target':'<abs path>','max_depth':3,'summary_only':true})
  execute_mcp_tool('livespec','audit_coverage',{'summary_only':true})
Return JSON: {caller_count, affected_RFs, suggested_tests}
""")
```

## Response format

```
**Target**: <file or symbol>
**Centrality**: PageRank=<v>, in_degree=<v>, blast_radius=<n files>
**RFs touched**: <list>
**Smells in target**: <list with severity>
**Verdict**: GO | CAUTION | NO-OP
**Plan**: <bulleted steps or "rejected: reason">
```

## Failure modes

- Target file not indexed yet → run `cube_index_file` + `index_project` before audit.
- `cube_analyze_graph` chunk-limit error → call with `top_n<=5`.
- Refactor approved but later regression → check `cube_get_tensions(status='detected')` post-edit; high tension = baseline distance broke.
