# Usage Discipline

> Always read the `## Usage` block when the HUD hook injects it. Treat the thresholds as hard rules, not hints.

## Thresholds (hard)

| HUD signal | Action |
|---|---|
| `ctx ≥ 70%` OR `session ≥ 85%` | Before starting the next non-trivial task: `next_task_record(...)` then `tmux_clear_and_prompt(session=..., prompt=<resume>)`. Do not start the task in the current context. |
| `session ≥ 95%` | Stop dispatching subagents. Finish mainline work only, then rotate. |
| `ctx < 70%` and `session < 85%` | Normal operation. |

## Caveman ultra is default

`/caveman ultra` is set as the session-start default. Do not run `/caveman full` or `/caveman lite` to "save the user from terseness". Cutting tokens is the point. Only switch if the user types `stop caveman` or `normal mode`.

## Do

- Read the HUD before spawning a wave of subagents. Each subagent run consumes tokens proportional to its own context — budget accordingly.
- After a heavy task (subagent wave, large refactor), check the HUD block emitted by the workflow_post_traverse hook for fresh state before the next decision.
- `model_switch` drives the `/model` TUI. Always `/clear`s first to skip the reindex token cost. No automatic policy — agent decides when to escalate or downshift.

## Don't

- Don't ignore the `## ⚠ Rotation advisable` Stop-hook block. It fires only at threshold.
- Don't rotate mid-workflow phase. Finish the phase, commit, then rotate.
- Don't run subagents past 80% ctx — cache misses on 1M context dominate at that level.
- Don't override caveman ultra without user request.
