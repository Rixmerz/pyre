---
name: jig-goal
description: Set the active jig goal — classify complexity, attach validators, enable autonomy. Auto-injected by /setup-agents into the fresh post-restart session.
disable-model-invocation: true
argument-hint: "<task description>"
---

# /jig-goal — goal + validators in the fresh session

This command is the **auto-injected resume prompt** that
`/setup-agents` queues. The scheduler daemon pastes `/jig-goal <task>`
into the freshly-restarted Claude session, and that's when the goal
is actually set — together with classification and validators, so the
autonomy supervisor sees a coherent state from turn one.

Pre-conditions when this fires from the auto-injection path:

- Subagents have already been deployed by `/setup-agents`
- Autonomy is already enabled (`JIG_AUTONOMY=1` in settings)
- No goal is set yet — this command sets it

When invoked, do exactly:

## 1. Classify complexity

Pick exactly one: `simple` | `medium` | `complex` | `unknown`.

| Signal | Class |
|---|---|
| One function, one file, no UI, no I/O | `simple` |
| Multi-file change, library or CLI surface only | `medium` |
| Web UI, multi-service, integrations, migrations, async | `complex` |
| Cannot decide | `unknown` |

## 2. Pick validators

Default to including these when the project has the tool:

- `tests_pass` weight 0.4
- `lint_pass` weight 0.15

Layer in by description text:

| Mention | Add |
|---|---|
| Specific file path in description | `files_exist` weight 0.10 |
| Build / generate / compile step | `command_exit` weight 0.10 |

Validator config shape — always use `type`, not `name`:

```python
validator_configs=[
    {"type": "tests_pass",   "weight": 0.4},
    {"type": "lint_pass",    "weight": 0.15},
    {"type": "files_exist",  "weight": 0.10, "paths": ["src/auth/oauth.py"]},
    {"type": "command_exit", "weight": 0.10, "cmd": ["python", "-m", "build"]},
]
```

The key is `type`, not `name`. Using `name` instead causes zero
validators to be registered and confidence to stay at 0 forever.

Built-in validator types: `tests_pass`, `lint_pass`, `command_exit`,
`files_exist`. HTTP/HTML/contrast/screenshot validators were removed.

## 3. Bootstrap (ONE call)

```python
goal_bootstrap(
    goal=<task verbatim>,
    complexity=<class>,
    target_confidence=<1.0 simple | 0.95 medium | 0.9 complex | 0.9 unknown>,
    acceptance_criteria=[<1-2 sentence list extracted from description>],
    validator_configs=[...],
    preferred_model=<spec or "">,
    enable_autonomy=True,
    deploy_subagents=False,     # already done by /setup-agents
    synthesize_workflow=False,  # no-op since the synthesizer was removed
)
```

`deploy_subagents=False` is what keeps this from looping — with it
False, `goal_bootstrap` will NOT enqueue another `restart_and_prompt`.

## 4. Report ONE block

```
Goal: <first 80 chars>
Complexity: <c> · target: <target>
Validators: <names>
Autonomy: enabled
```

## 5. Pick execution strategy

After the goal is set, classify how to drive it. ONE of:

| Signal in task description | Strategy | Action |
|---|---|---|
| "validar", "validate market", "20 calls", "demand", "user interviews", no concrete artifact | `validation` | Produce a validation plan (script + tracker CSV). No code. |
| Single file/module, one domain (only backend OR only frontend OR only tests) | `single` | Just describe what's next — auto-orchestration (JIG_AUTO_ACTIVATE) picks the workflow. |
| "SaaS", "app + web", "backend + frontend", >=2 of {domain model, API, UI, migration, integration} | `multi` | Decompose into milestones. For the FIRST milestone only: invoke `/sprint <milestone> --needs-e2e`. Subsequent milestones come from goal validators / next-task chain. |
| "investigate", "find out why", "explore", "trace", no concrete change | `explore` | Spawn `Explore` or `debugger` subagent. No workflow activation. |

Default if ambiguous: `single`. Never default to `multi` — that was the
old failure mode (full SaaS shoved into one wave).

The `--needs-e2e` hint on the `multi` row tells /sprint to force-activate
the bundled `sprint-e2e` graph regardless of its own internal heuristic.
This is the canonical setting for any goal that touches a service
boundary; it ensures a Contract pre-wave and an E2E live-server wave
both run before Validation. See
`.claude/runbooks/orchestrator/workflow-catalog.md`.

## 6. Proceed to orient

After picking strategy, call `graph_status` to see the active workflow
phase and start the orient phase of the work. Subsequent turns advance
through validators → autonomy supervisor decides ROTATE / WAIT_RESET /
GOAL_COMPLETE.

## Manual / standalone use

If you call `/jig-goal` directly (without prior `/setup-agents`) in a
session where agents haven't been deployed, pass `deploy_subagents=True`
instead — that path triggers the full bootstrap including the restart.
Most flows should go through `/setup-agents <task>` for clarity.

## Don't

- Don't pass `synthesize_workflow=True`. It's a no-op since the
  workflow-synth pieces were removed; an existing
  `.claude/workflows/*.yaml` graph still works via `graph_activate`.
- Don't list HTTP/HTML/contrast/screenshot validators — they were
  removed from the registry.
- Don't ask the user to confirm validator weights — use the defaults above.
