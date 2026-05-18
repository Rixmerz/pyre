---
name: runbook-system
description: How runbooks plug into the delegation flow. Complementary to auto-delegation (the what) and workflow graphs (the when) — runbooks are the concrete how.
---

# Runbook System

> Always check runbooks before improvising procedure for a known case.

## What runbooks are

A **runbook** is a single-case operational procedure: when a specific
situation arrives, here are the exact steps, the exact tool calls, and
the exact response format. They live at:

```
.claude/runbooks/
  orchestrator/<case>.md       ← main agent intake patterns
  <agent-name>/<case>.md       ← per-specialist procedures
```

Each runbook has frontmatter (`id`, `scope`, `when`, `tools`, `sla`) and
four fixed sections: **Steps**, **Tools (specific calls)**, **Response
format**, **Failure modes**.

## How they fit (complementary, not redundant)

| Layer | Question answered | Where it lives |
|-------|-------------------|----------------|
| `auto-delegation.md` rule | **Which** agent for this request shape | tabla request→agent |
| Workflow graph (`graph_activate`) | **When** each macro phase fires | yaml in `.claude/workflows/` |
| **Runbook** | **How** to execute a known case end-to-end | `.claude/runbooks/<scope>/<case>.md` |

The flow at runtime:

1. Main agent reads request → consults `auto-delegation.md` → picks specialist.
2. Before dispatching, main agent scans `runbooks/orchestrator/` for a matching `when:` clause.
3. If found → follows the orchestrator runbook (which prescribes which sub-runbook the specialist runs).
4. Specialist subagent reads `runbooks/<its-name>/` and picks the matching case runbook.
5. Subagent executes Steps + Tools verbatim, fills the Response format template, returns it.

If no runbook matches, fall back to general guidance in the agent's `.md` definition.

## Authoring rules

- **Single case per file.** "Quality snapshot" and "Refactor impact" are different runbooks even though both belong to `dcc-specialist`.
- **Concrete tool calls.** `execute_mcp_tool("livespec", "quick_orient", {"qname": "<qname>"})` not "use livespec to orient".
- **Response format is a contract.** Specialists return data shaped like the template so the orchestrator can pipe results without re-asking.
- **Failure modes are mandatory.** What goes wrong, what to do. Updates this section first when reality teaches new failure modes.
- **No prose.** Bullets, code blocks, tables. Long paragraphs in a runbook = you're writing documentation, not a procedure.

## DO

- Read `runbooks/orchestrator/*` before dispatching subagents on a known shape (new repo, bug intake, refactor audit, PR review).
- Subagent prompts SHOULD include `"runbook: <id>"` so the specialist knows exactly which procedure applies.
- Add a new runbook when the same case has been improvised more than twice — codify before drift.
- Keep response-format templates stable; downstream code may parse them.

## DON'T

- Don't duplicate the auto-delegation table in a runbook. Reference it.
- Don't reinvent workflow phases inside a runbook. Use the workflow graph for multi-phase enforcement.
- Don't write runbooks for trivial cases (single tool call, no failure modes worth listing).
- Don't let runbooks rot. If a referenced tool name changes, update every runbook that calls it.

## Discovery

- `ls .claude/runbooks/orchestrator/` — what the main agent knows.
- `ls .claude/runbooks/<agent>/` — what that specialist knows.
- Each agent's `.md` SHOULD list its runbooks in a `## Runbooks` section so the subagent loads them up-front in its first turn.
