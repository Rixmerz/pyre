#!/usr/bin/env python3
"""UserPromptSubmit hook: inject the current goal + confidence at the top of
every user prompt, so the agent does not lose track of the objective across
turns or rotations.

Gated on env JIG_AUTONOMY=1.
"""
from __future__ import annotations

import os
import sys


def main() -> int:
    if os.environ.get("JIG_AUTONOMY", "0") != "1":
        return 0
    try:
        from pathlib import Path

        from jig.engines import goal_state

        project_dir = os.environ.get("CLAUDE_PROJECT_DIR") or str(Path.cwd())
        g = goal_state.get_goal(project_dir)
        if not g or g.status != goal_state.GoalStatus.ACTIVE.value:
            return 0
        last_run = g.last_results[-3:] if g.last_results else []
        bullets = "\n".join(
            f"- {r.name}: {'pass' if r.passed else 'fail'} ({r.confidence_contribution:.2f}/{r.weight:.2f})"
            for r in last_run
        ) or "- (no validators run yet)"
        print(
            "## Active goal\n"
            f"**{g.goal}**\n\n"
            f"Confidence: **{g.confidence:.2f}** / target {g.target_confidence:.2f} "
            f"(attempts: {g.attempts})\n"
            f"Last validators:\n{bullets}\n"
        )
        return 0
    except Exception as e:
        print(f"# goal_context_injector error: {e}", file=sys.stderr)
        return 0


if __name__ == "__main__":
    sys.exit(main())
