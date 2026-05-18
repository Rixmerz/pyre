---
id: bug-report-intake
scope: orchestrator
when: "user reports a bug · 'X is broken' · 'why does Y fail' · stack trace pasted"
tools: [Task, execute_mcp_tool, Read]
sla: ~5 min wall, ~80k subagent tokens
---

## Steps

1. **Capture the failure signal**. Stack trace, error message, repro steps. Save verbatim in a scratch notion at `.claude/notions/bug-<slug>.md` if non-trivial.
2. **Dispatch to debugger** (subagent). Prompt includes the verbatim error, suspected file paths from the trace, and the latest notions to read.
3. **debugger returns a diagnosis**: root cause + suggested fix. Read it.
4. **Dispatch to fixer** with the diagnosis + explicit acceptance criteria ("no new failing tests, lint clean").
5. **Dispatch to tester** in the matching language to add a regression test.
6. **Read `.claude/state/pending-reindex.txt`** (managed by `reindex_notice.py`). Drive the auto-reindex calls before closing.
7. **Reply with**: cause, fix summary, regression test path, files changed.

## Tools (specific calls)

```
Task(subagent_type="debugger", prompt="""
Bug: <verbatim error>
Repro: <user's steps>
Read first: .claude/notions/dcc-snapshot.md, livespec-overview.md
Investigate via:
  - execute_mcp_tool('livespec','quick_orient', {'qname': '<symbol from trace>'})
  - execute_mcp_tool('livespec','who_calls',   {'qname': '<symbol>', 'max_depth': 2})
  - execute_mcp_tool('deltacodecube','cube_get_centrality', {'path': '<file>'})
Return: {root_cause, suspected_files, recommended_fix}
""")
```

## Response format

```
**Bug**: <one-line restatement>
**Root cause**: <from debugger>
**Fix**: <summary of fixer diff>
**Regression test**: <path + assertion>
**Files changed**: <list>
**Reindex**: livespec ✓ · dcc_reindex ✓ (or pending)
```

## Failure modes

- debugger can't reproduce → escalate with `Task(reviewer, ...)` to audit related code paths.
- fix introduces new high-severity DCC smell → don't merge; loop back to fixer with the smell as constraint.
- Test fails after fix → diagnosis was wrong; spawn debugger again with the new evidence.
