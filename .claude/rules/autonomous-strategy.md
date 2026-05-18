# Autonomous Strategy — Decision Tree

> Always evaluate this decision tree before responding to any task.

Before responding, evaluate which approach fits:

## 1. Direct Response
Use when: knowledge question, explanation, isolated debug, obvious 1-3 line change.
Just respond — no structure needed.

## 2. Plan Mode
Use when: multi-file feature, architectural decisions needing approval, refactor with regression risk, one-off tasks.
Say "entering plan mode" and use EnterPlanMode.

## 3. Workflow
Use when: well-defined phases (understand → reproduce → fix → verify), recurrent process (debugging, code review, feature dev), phase enforcement matters.

**Why workflows over plan mode:**
- Context persistence across very long tasks — resume mid-flow without losing state
- Dynamic DCC injection — real-time code change tracking, diffs, and quality metrics
- Memory injections — past mistakes and project conventions injected at the right moment
- Phase enforcement — cannot skip steps, each node injects phase-specific context

**Reuse or create?**
- First run `graph_list_available` to check existing workflows
- Reuse if one covers the case (e.g. `debug` for bugs, `feature-dev` for features)
- Create new only if the process is unique — use `graph_builder_create`

## 4. Always

- **LSP First**: check LSP diagnostics before and after code changes when LSP is available
- **Pipeline continuity**: once a workflow starts, complete it without interruption unless blocked by an error or explicit user request
- This decision is yours — do not ask the user which to prefer unless genuinely ambiguous
- **No confirmation pauses mid-workflow.** Once a goal is active and a workflow is traversing, DO NOT end a turn with "shall I proceed?", "should I continue?", "want me to dispatch X?". Make the call from workflow context and execute. The agent ending its turn to ask permission is the failure mode that `goal_gate` blocks at Stop — do not author the prompt that triggers it. Ask only when (a) the user explicitly stops the run, (b) two acceptable paths fork and the workflow truly cannot decide, or (c) you need a secret/credential only the user has.
- **Never `/clear` without a resume prompt.** See `clear-discipline.md`. Use `tmux_clear_and_prompt(session=..., prompt=...)` — never a bare `/clear`.

## 5. Delegation economics (main-context only)

You are the only context that holds the full conversation, the architectural intent, and the user's evolving preferences. Subagents cannot spawn subagents — orchestration is your job. Protect your context aggressively and push mechanical execution down.

**Delegate when two or more apply:**
- Task needs ≥5 file reads/edits/shell commands.
- Task has ≥3 sequential phases.
- Task involves grepping or scanning code you have not loaded.
- Task is well-specified — you can name files, line numbers, and acceptance criteria.
- Output you need is small (a diff, a summary, a yes/no) compared to the work to produce it.

**Do it yourself when any apply:**
- Task is short (<3 tool calls) — briefing overhead dominates.
- Relevant files already loaded in your context.
- User is iterating tightly, correcting course turn by turn.
- Task is judgment: architecture, naming, tradeoff analysis. **Never delegate thinking.**
- Task touches sensitive shared state (production push, force-push, deletes).

**Briefing & verification:**
- Give each subagent a self-contained prompt: goal, file paths, prior decisions, deliverable, constraints. Pick `Explore` for read-only lookups, `general-purpose` for implementation, specialized agents (`@backend`, `@frontend`, `@tester`, `@reviewer`, etc.) when the charter fits.
- Run independent agents in parallel (multiple `Task` calls in one message) — never on overlapping files.
- After a subagent reports, **inspect the actual diff** (`git status` / `git diff`) and run the smallest possible verification. A subagent's "done" is a hypothesis, not a fact.
- If verification fails, re-brief with the specific failure — do not loop the same prompt.
