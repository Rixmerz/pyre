---
name: setup-agents
description: Deploy specialized subagents, skills, and rules for the current project's tech stack.
disable-model-invocation: true
argument-hint: "[tech_stack...]"
---

# /setup-agents

Deploy specialized subagents + skills + rules for the current project's tech stack.

## What it does

1. Detect tech stack from project files (pyproject.toml, package.json, go.mod, etc.).
2. Call `deploy_project_agents(project_path=<cwd>, tech_stack=<detected>)` to copy:
   - Agent definitions → `.claude/agents/`
   - Skills (testing, validation, language-specific patterns) → `.claude/skills/`
   - Rules (commit-discipline, quality-feedback, etc.) → `.claude/rules/`
3. Report what was deployed.

## Usage

The user invokes `/setup-agents` after `jig init` (or after a stack change). Execute one MCP tool call and summarize:

```
deploy_project_agents(project_path=<cwd>, tech_stack=<detected list>)
```

If the user requests it explicitly, you may also call `goal_set(...)` to seed an initial goal. Do NOT auto-create goals.

## Manual restart

Subagent deployment changes `.claude/agents/`. Claude Code reads agents at session start, so to load the newly deployed agents the user must:

- Type `/exit` and re-run `claude`, OR
- Open a new pane and run `claude` there.

This is intentional — restart is manual. Do not attempt to automate it.

## DON'T

- Don't call `autonomy_set` (deleted).
- Don't call `tmux_handoff_goal` (deleted).
- Don't call `workflow_synthesize_from_goal` automatically — that's a separate user-driven step.
- Don't `/clear` after deploying; the next session will pick up new agents naturally.
