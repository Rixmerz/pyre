# Tensions and Quality Gates

## What's a tension

A "tension" is a contract violation detected by DeltaCodeCube (DCC): two files that depend on each other but belong to modules that shouldn't be coupled. Tensions accumulate as architecture drifts.

Severity ladder: `critical > high > medium > low`.

## Tension gates in workflows

A workflow node can declare:

```yaml
tension_gate:
  blocks_on: [god_file, circular_dependency]
  max_new:
    critical: 0
    high: 3
```

`blocks_on` means new findings of that category block the phase from advancing. `max_new` sets per-severity budgets for the delta since the phase started.

## When the gate blocks

You see:
```
Tension gate blocked: 2 new critical findings in phase "implement".
Call cube_get_tensions(severity="critical") for details, then resolve or
acknowledge via graph_acknowledge_block(reason="...").
```

Two paths:

1. **Resolve the tensions.** Re-structure, remove the coupling, simplify. Then `graph_traverse` again — the gate re-evaluates.
2. **Acknowledge the block.** If the tensions are intentional (`"temporary scaffolding, will refactor in the next story"`), use `graph_acknowledge_block(reason="...")` — the reason is append-only audit logged.

Never acknowledge without a concrete reason — that defeats the entire enforcement.

## Reading DCC output

```
execute_mcp_tool("dcc", "cube_get_tensions", {"severity": "critical", "limit": 10})
```

Returns tension records with `source`, `target`, `type`, `rationale`, and an `impact_score` (ripple effect if that coupling were exercised).

## Between-phase delta

After `graph_traverse`, the PostToolUse hook computes the delta: new tensions, resolved tensions, new smells. Only delta is injected — the agent isn't drowned in pre-existing issues.

## Tuning thresholds

Per node:
- `max_new.critical: 0` — any new critical blocks
- `max_new.high: 5` — tolerate up to 5 new highs
- Omit entries to not gate that severity

Per project (global default): set `[dcc.tension_gate]` in `~/.config/jig/config.toml` (Sprint 5 feature).
