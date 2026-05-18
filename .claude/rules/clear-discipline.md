# Clear Discipline

> Always apply while an active workflow or active goal exists.

Rules for clearing the conversation context inside the autonomous loop.

## Why this rule exists

`/clear` wipes the conversation context. If the main agent is mid-
workflow — subagents in flight, planning state in head, or simply
mid-wave — a bare `/clear` strands the workflow: the post-clear turn
arrives with no idea what to do next. Because `/clear` does not fire
a `Stop` event, `goal_gate` cannot intervene. The pane goes from
"actively working" to "frozen" in one keystroke.

Observed failure mode (jig-test4, 2026-05-16): the agent used `/clear`
while two subagents were running. After they finished, the post-clear
turn had no context describing what was waiting on them or what came
next, so it did nothing.

## DO

- Use `tmux_clear_and_prompt(session, prompt=<resume>)` instead of a
  bare `/clear`. The MCP tool clears AND immediately pastes a resume
  prompt, so the fresh turn arrives with explicit state and next step.
- Before any clearing call, persist continuity with
  `next_task_record(summary=<state + next step>, task_description=<...>,
  files_changed=[...])`. The fresh turn reads it via `next_task_get`
  at Step 0.
- The resume prompt MUST state both:
  1. What was happening before the clear (e.g. "Waiting for subagents
     X and Y to return from Wave 1.").
  2. What the next action is (e.g. "Verify both returned green, then
     graph_traverse to the next node.").
- When context-pressure thresholds fire (`ctx_pct >= 70` or
  `session_pct >= 85`), use `tmux_clear_and_prompt(session=..., prompt=<resume>)`
  — it combines `/clear` with auto-injection of the resume prompt.

## DON'T

- Don't send `/clear` to the productive pane via any path that does
  not paste a resume prompt afterwards.
- Don't `send_slash(target, "/clear")` standalone from any tool.
- Don't `inject_after_clear(target, "")` with an empty resume string.
- Don't tell the user to manually type `/clear` mid-workflow. If
  continuity is needed, use `tmux_clear_and_prompt` instead.
- Don't `/clear` while subagents are still running — their tool_use
  results would land in a turn that does not recognise them.

## Enforcement

The `tmux_clear_and_prompt` MCP tool refuses an empty prompt as the
engine-level guard. This rule is the prompt-level guard so the agent
never even attempts the bare-`/clear` path.
