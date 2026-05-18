---
id: pr-diff-impact
scope: livespec-specialist
when: "PR review · 'what breaks if I merge this' · pre-merge check · git base..head provided"
tools: [git_diff_impact, quick_orient, audit_coverage]
sla: ~20s wall
---

## Steps

1. `git_diff_impact(base_ref=<base>, head_ref=<head>, summary_only=True)` first to size payload.
2. If summary's `impacted_callers_count > 0`, fetch detail with `impacted_limit=200`. Page with `impacted_cursor` if needed.
3. For each affected RF returned, `get_requirement_implementation(rf_id=...)` to map the RF to current implementing symbols.
4. Report `changed_files`, `changed_symbols`, `impacted_callers`, `affected_requirements`, `suggested_tests`.
5. If `suggested_tests` is empty but `changed_symbols` non-empty → there's a test coverage gap. Flag it.

## Tools (specific calls)

```
execute_mcp_tool("livespec", "git_diff_impact", {"base_ref": "<base>", "head_ref": "<head>", "summary_only": true})
execute_mcp_tool("livespec", "git_diff_impact", {"base_ref": "<base>", "head_ref": "<head>", "impacted_limit": 200})
execute_mcp_tool("livespec", "get_requirement_implementation", {"rf_id": "<id>"})
```

## Response format

```markdown
## PR impact (<base>..<head>)

**Changed files**: <n>
**Changed symbols**: <n>
**Impacted callers**: <n> (depth ≤ <max_depth>)
**Affected RFs**: <list of rf_id>
**Suggested tests to run**: <list of paths>

### Risk hotspots
- <qname> — called by <n> downstream symbols across <m> RFs.

### Test coverage gap
- <changed_symbol> has no test in suggested_tests. Recommend a regression test before merge.
```

## Failure modes

- `error` field set on response → unknown git ref; verify `base_ref` / `head_ref` exist locally.
- Payload truncated → `summary_only=True` and page from there.
- `affected_requirements` empty in a project with RFs → diff didn't touch any rf-linked symbol; still verify via `audit_coverage` whether the changed modules are RF-orphan.
