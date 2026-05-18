---
id: context-rotation
scope: orchestrator
when: the post-traverse hook injected a usage block showing ctx ≥ 70% OR session ≥ 85%, OR the autonomy supervisor's Stop-hook output flagged `ROTATE`/`WAIT_RESET`.
tools: next_task_record, tmux_clear_and_prompt
sla: complete the rotation within the current turn; do not start the next task first
---

# Steps

1. Confirm the trigger. The usage block emitted by the workflow_post_traverse hook is the source of truth. Do not rotate on a stale value.
2. Wrap up any in-flight workflow phase: commit if there are uncommitted changes; mark the current ``graph_traverse`` node complete only if the work for that node is done. Do not rotate mid-phase.
3. Record the handoff with ``next_task_record``. Summary ≤ 1 line. Task description includes: what is next, which files matter, any blocker.
4. Call ``tmux_clear_and_prompt`` with a resume prompt that names the prior state and next action.
5. Stop responding in the current session — the next prompt arrives in a fresh context.

# Tools (specific calls)

```python
# 1. Persist the handoff so the fresh session has continuity.
next_task_record(
    project_dir=<cwd>,
    summary="<one-line summary>",
    task_description="<what to do next, file paths, constraints>",
    files_changed=<list of paths touched this session, optional>,
)

# 2. Atomic /clear + paste.
tmux_clear_and_prompt(
    session="<session name>",
    prompt="Prior state: <summary>. Next action: <what to do>.",
)
```

# Response format

Single line on stdout, then stop:

```
Rotating: ctx <pct>% session <pct>%. Next task recorded → <summary>. tmux_clear_and_prompt sent to <session>.
```

# Failure modes

- ``tmux_clear_and_prompt`` returns ``ok: false, error: session not found``: wrong session name. Ask the user for the correct session (`tmux list-sessions`). Do NOT continue working.
- ``next_task_record`` fails (disk full, permission): abort rotation. Without a handoff, the new session has nothing to resume on.
- Phase still mid-flight: do NOT rotate. Print ``phase <node> incomplete; finish before rotating`` and continue the phase.
