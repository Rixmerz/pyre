---
id: find-call-graph
scope: livespec-specialist
when: "'who calls X' · 'what does Y call' · 'is Z dead' · backward-cone or forward-cone exploration"
tools: [quick_orient, who_calls, who_does_this_call, analyze_impact]
sla: ~20s wall
---

## Steps

1. `quick_orient(qname=<target>)` first. Returns kind, signature, file/line, docstring, top callers/callees, RF links, `is_entry_point` flag. Often answers the question without further calls.
2. If callers needed at depth >1: `who_calls(qname=..., max_depth=2)`. Default `min_weight=0.6` filters fan-out noise.
3. If forward cone needed: `who_does_this_call(qname=..., max_depth=2)`.
4. For a full impact view: `analyze_impact(target_type='symbol', target=<qname>, max_depth=3, summary_only=True)` first, then page details if needed.
5. Watch payload size: deep cones easily exceed 100KB. Use `summary_only=True` to size before fetching.

## Tools (specific calls)

```
execute_mcp_tool("livespec", "quick_orient",       {"qname": "<qname>"})
execute_mcp_tool("livespec", "who_calls",          {"qname": "<qname>", "max_depth": 2, "limit": 100})
execute_mcp_tool("livespec", "who_does_this_call", {"qname": "<qname>", "max_depth": 2, "limit": 100})
execute_mcp_tool("livespec", "analyze_impact",     {"target_type": "symbol", "target": "<qname>", "max_depth": 3, "summary_only": true})
```

## Response format

```markdown
## <qname>

**Kind**: <function|class|method>
**File**: `<path>:<line>`
**Signature**: `<sig>`
**Entry point**: <yes/no — framework=...>

### Direct callers (limit 10)
- `<qname>` in `<file>:<line>` (weight=<w>)

### Direct callees (limit 10)
- `<qname>` in `<file>:<line>` (weight=<w>)

### Linked RFs
- <rf_id>: <title>

### Notes
- `callers_count: 0` with `is_entry_point: true` ≠ dead code.
- Weight < 0.6 results filtered; pass `min_weight=0.0` if you need the noisy view.
```

## Failure modes

- `qname` not found → try `find_symbol(query=<short name>)` first to disambiguate.
- Multiple matches → return list, let orchestrator pick.
- `analyze_impact` payload truncated → switch to `summary_only=True`, then page with `cursor`.
