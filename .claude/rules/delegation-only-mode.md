---
name: delegation-only-mode
description: Forces the main agent to delegate all code work. Enabled when JIG_DELEGATION_ONLY=1.
---

# Delegation-Only Mode

> Active only when the env var `JIG_DELEGATION_ONLY=1` is set in `.claude/settings.json`. Otherwise these rules do not apply.

## The contract

The **main Claude agent never reads, edits, or executes against project source**. Its filesystem scope is restricted to `.claude/` (rules, agents, workflows, notions, telemetry). Project code is touched **only** by specialized subagents spawned via the Task tool.

Enforcement is hard: `delegation_gate.py` (PreToolUse hook) blocks `Read | Edit | Write | Glob | Grep | NotebookEdit` on any path outside `.claude/`, and blocks `Bash` except for a small read-only allowlist (`ls .claude`, `cat .claude/…`, `git status`, `git log`, `pwd`, `date`, `echo …`). Subagents bypass the gate via `parent_tool_use_id` detection.

## Why

- **Brutal context reduction** for the orchestrator. The main agent reasons over `.claude/notions/*.md` digests instead of raw source — keeps the long-running session lean.
- **Forced specialization.** Real code questions go to the agent that knows the right MCP (livespec for symbols/call-graph/RFs, dcc-specialist for centrality/smells/tensions, reviewer for validation, backend/frontend/tester for changes).
- **Auditable separation.** Every code touch is a Task invocation with a self-contained prompt → easier to review what the agent actually saw.

## Workflow for the main agent

1. **First contact with a new task → build a notion.** If `.claude/notions/<topic>.md` doesn't exist for what you're being asked about, spawn a subagent (usually `codebase-analyst` or `livespec-specialist` for structure, `dcc-specialist` for quality, `product-analyzer` for feature scope) and ask it to **produce a notion file** at `.claude/notions/<slug>.md`. The subagent reads the code; you read the notion.
2. **Reason over notions.** Treat `.claude/notions/*.md` as your authoritative view of the project. Update or supersede them as understanding evolves — never re-read source to refresh them, delegate.
3. **Delegate every change.** Bug fix → `debugger` + `fixer`. New endpoint → `backend` + `tester`. UI change → `frontend`. Migration → `db-migrator`. Each prompt must be self-contained (file paths, acceptance criteria, constraints).
4. **Validate via subagent.** `reviewer` runs the validation skill; you read its report, not the diff.

## Notion file format

```markdown
---
topic: <slug>
produced_by: <agent-name>
produced_at: <ISO8601>
sources_scanned: <list of paths or "via livespec/dcc">
---

## Summary
2-4 sentences of what this part of the codebase is and why it exists.

## Key symbols / files
- `path:line` — role / responsibility
- ...

## Invariants & gotchas
- ...

## Open questions
- ...
```

Keep notions short (≤ 200 lines). One topic per file. Refresh by spawning the same agent again with a "supersede notion X" instruction.

## DON'T

- Don't try to bypass the gate with cute path tricks — `..`, symlinks, etc. The gate resolves paths.
- Don't write notions yourself by guessing — always have a subagent produce them so the source-truth path is real.
- Don't enable `JIG_DELEGATION_ONLY=1` on projects without the specialized agents deployed. Run `/setup-agents` first.
- Don't delegate trivial work (single-line config edit) — toggle the env var off for that session, do it, toggle back on.

## Toggling

- Enable: set `"JIG_DELEGATION_ONLY": "1"` in `.claude/settings.json` `env`.
- Disable for a session: unset the env var, or set to `"0"` and reload Claude Code.
