# Creating a Workflow — The Method

Designing a workflow is not template-filling. You are declaring the disciplined sequence in which an agent is **allowed** to work. Bad workflows feel arbitrary; good workflows feel inevitable.

## Step 1 — Clarify intent

Before writing YAML, answer in one sentence each:
- **What task** is this workflow for? (feature dev, bug fix, research, refactor, review, ...)
- **What phases** does the work naturally have? (≥2, ≤6 — more than 6 is usually bad decomposition)
- **What must NOT happen** in each phase? (this is what `tools_blocked` enforces)

If you can't answer without hedging, the workflow isn't ready. Stop.

## Step 2 — Discover capabilities

Do NOT hallucinate tool names. Call:
```
proxy_list()                                  # see all proxied MCPs
proxy_tools_search(query="analyze structure") # find tools by description
proxy_list_tools(name="serena")               # enumerate a specific MCP
```

Write down the tool names you'll rely on per phase. Reference them exactly in the YAML.

## Step 3 — Design phases

A well-designed phase has four parts:

```yaml
- id: understand
  tools_blocked: [Edit, Write]          # what the agent cannot do here
  mcps_enabled: [serena, sequentialthinking]  # only these proxies usable
  prompt_injection: |                   # what the agent MUST do here
    Read relevant files and map the code surface. Use find_symbol and
    get_symbols_overview. Do NOT modify anything. Advance via graph_traverse.
  tension_gate:                         # quality constraint to leave (optional)
    blocks_on: [god_file, circular_dependency]
```

### Principles

1. **prompt_injection is mandatory.** If you can't explain what the agent should DO in one paragraph, the phase is broken.
2. **tools_blocked needs justification.** Never block arbitrarily — the agent rebels when restrictions lack rationale. Explain the why in `prompt_injection`.
3. **tension_gate only where there's a baseline.** Use it after code-modification phases where DCC metrics matter, not after research phases.
4. **Name phases by verb of intent.** `understand`, `design`, `implement`, `validate` — not `phase-1`, `phase-2`.

## Step 4 — Archetypes

| Archetype | When |
|-----------|------|
| **Linear** | Clear sequential work: understand → design → implement → validate |
| **DAG parallel** | Backend/frontend/tests parallel once domain is fixed |
| **Review loop** | Implement ↔ Review iteration until gate passes |
| **Research spike** | Exploration only, Edit/Write blocked throughout |

## Step 5 — Validate

```
graph_manage(op="validate", yaml_path=".claude/workflows/my-flow.yaml")
```

Checks: schema, unreachable nodes, dead-end blocked tools, missing `prompt_injection`.

## Step 6 — Dry-run

```
graph_manage(op="dry_run", graph_id="my-flow")
```

Simulates transitions without actually activating, reports what would be blocked.

## Step 7 — Commit

Commit the workflow YAML with a `Why:` body explaining the process you've encoded. That message becomes experience memory for future sessions across projects.

## Worked example

See `assets/workflows/demo-feature.yaml` — a 4-phase tutorial that blocks Edit/Write in `understand`, restricts to docs in `design`, allows full access with a tension gate in `implement`, and returns to read-only for `validate`.
