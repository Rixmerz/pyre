#!/usr/bin/env python3
"""Reindex Reminder — UserPromptSubmit hook.

Reads ``$CLAUDE_PROJECT_DIR/.claude/state/pending-reindex.txt`` written
by the ``reindex_notice.py`` PostToolUse hook. If non-empty, prints an
``additionalContext`` block instructing the main agent to call
``livespec.index_project`` and ``cube_reindex(path=...)`` for each
listed file *before* dispatching the next subagent. Clears the file
once the reminder has been emitted.

Protocol:
  stdin:  {prompt, session_id, transcript_path, ...}
  env:    CLAUDE_PROJECT_DIR
  stdout: {"hookSpecificOutput": {"hookEventName":"UserPromptSubmit",
                                  "additionalContext": "..."}}
  exit 0: always
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path


def main() -> None:
    project_dir = Path(os.environ.get("CLAUDE_PROJECT_DIR", os.getcwd()))
    pending = project_dir / ".claude" / "state" / "pending-reindex.txt"
    if not pending.exists():
        sys.exit(0)

    try:
        files = [
            line.strip()
            for line in pending.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
    except OSError:
        sys.exit(0)
    if not files:
        try:
            pending.unlink()
        except OSError:
            pass
        sys.exit(0)

    bullet_list = "\n".join(f"  - `{f}`" for f in files[:50])
    extra = ""
    if len(files) > 50:
        extra = f"\n  - …and {len(files) - 50} more"

    msg = (
        "## Auto-reindex required\n\n"
        f"The previous subagent edited {len(files)} file(s) outside "
        "`.claude/`. Indexes are stale. Before dispatching the next "
        "specialist, call (in a single parallel batch):\n\n"
        "```\n"
        'execute_mcp_tool("livespec", "index_project", {"force": false})\n'
        + "\n".join(
            f'execute_mcp_tool("deltacodecube", "cube_reindex", {{"path": "{f}"}})'
            for f in files[:50]
        )
        + "\n```\n\n"
        "Files queued:\n"
        f"{bullet_list}{extra}\n"
    )

    output = {
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": msg,
        }
    }
    sys.stdout.write(json.dumps(output))

    try:
        pending.unlink()
    except OSError:
        pass
    sys.exit(0)


if __name__ == "__main__":
    main()
