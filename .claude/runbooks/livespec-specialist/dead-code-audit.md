---
id: dead-code-audit
scope: livespec-specialist
when: "'find dead code' · pre-cleanup audit · 'is X used anywhere'"
tools: [find_dead_code, quick_orient, audit_coverage, find_orphan_tests]
sla: ~30s wall for a medium repo
---

## Steps

1. `find_dead_code(summary_only=True)` — size the haystack.
2. Iterate filters off only if needed (`include_public=True` / `include_non_python=True`). Defaults already exclude tests/, build artifacts, public symbols (cross-crate callers invisible), non-Python files (Python-only reference scanner), and TS framework filesystem-routing files.
3. `find_orphan_tests(summary_only=True)` to size disconnected tests.
4. `audit_coverage(summary_only=True)` to spot RFs without implementation and modules without RF link — partially complementary.
5. For each dead candidate of interest, confirm via `quick_orient` — `is_entry_point=True` overturns dead-flag.
6. Return categorized list. Do NOT recommend deletion of public/exported symbols unless the user explicitly wants `include_public=True`.

## Tools (specific calls)

```
execute_mcp_tool("livespec", "find_dead_code",     {"summary_only": true})
execute_mcp_tool("livespec", "find_dead_code",     {"limit": 50})
execute_mcp_tool("livespec", "find_orphan_tests",  {"summary_only": true})
execute_mcp_tool("livespec", "audit_coverage",     {"summary_only": true})
execute_mcp_tool("livespec", "quick_orient",       {"qname": "<candidate>"})
```

## Response format

```markdown
## Dead-code audit

**Total candidates (filtered)**: <n>
**Excluded**: tests=<bool>, public=<bool>, non-Python=<bool>, framework-routes=<bool>

### Likely removable (verified via quick_orient)
- `<qname>` in `<file>:<line>` — kind=<func|class>, public=<bool>, RFs=<n>

### Needs confirmation
- `<qname>` — has 0 callers but is_entry_point=true (decorated by <framework>). KEEP.

### Coverage gaps (from audit_coverage)
- `modules_truly_orphan`: <n>
- `rfs_low_confidence`: <n>
```

## Failure modes

- Result list explosively long → user likely needs `include_public=False` (default) and to scope by module.
- Candidate has 0 callers but you find references via grep → cross-language call (e.g. Python calling shell script). livespec's scanner is Python-only at module level; document gap, don't delete.
