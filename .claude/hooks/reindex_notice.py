#!/usr/bin/env python3
"""Reindex Notice — PostToolUse hook for Edit/Write inside subagents.

When a subagent (transcript_path contains '/subagents/') edits or writes
a file outside `.claude/`, this hook appends that path to
``$CLAUDE_PROJECT_DIR/.claude/state/pending-reindex.txt``. The companion
``reindex_reminder.py`` UserPromptSubmit hook injects the list into the
main agent's next turn and clears the file.

The goal is to give the main agent a fresh, deterministic signal of
*what changed during the last subagent run* so it can drive
``execute_mcp_tool("livespec", "index_project", ...)`` and
``cube_reindex(path=...)`` before dispatching the next specialist.

Protocol:
  stdin:  {tool_name, tool_input, transcript_path, ...}
  env:    CLAUDE_PROJECT_DIR
  stdout: (none; hook is informational)
  exit 0: always (fail-silent)
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

_GATED_TOOLS = {"Edit", "Write", "NotebookEdit"}
_ALLOW_ROOT = ".claude"


def _path_inside_claude(raw: str, project_dir: Path) -> bool:
    if not raw:
        return False
    p = Path(raw)
    if not p.is_absolute():
        p = (project_dir / p).resolve()
    else:
        try:
            p = p.resolve()
        except OSError:
            return False
    claude_dir = (project_dir / _ALLOW_ROOT).resolve()
    try:
        p.relative_to(claude_dir)
        return True
    except ValueError:
        return False


def main() -> None:
    try:
        payload = json.loads(sys.stdin.read() or "{}")
    except json.JSONDecodeError:
        sys.exit(0)

    tool = payload.get("tool_name", "")
    if tool not in _GATED_TOOLS:
        sys.exit(0)

    tp = payload.get("transcript_path", "") or ""
    if "/subagents/" not in tp and "\\subagents\\" not in tp:
        sys.exit(0)

    tin = payload.get("tool_input", {}) or {}
    path = (
        tin.get("file_path")
        or tin.get("path")
        or tin.get("notebook_path")
        or ""
    )
    if not path:
        sys.exit(0)

    project_dir = Path(os.environ.get("CLAUDE_PROJECT_DIR", os.getcwd()))
    # Ignore edits inside .claude/ — those are notions/state, not source.
    if _path_inside_claude(path, project_dir):
        sys.exit(0)

    # Resolve to absolute path so the main agent can pass it verbatim.
    abs_path = path if Path(path).is_absolute() else str((project_dir / path).resolve())

    state_dir = project_dir / ".claude" / "state"
    state_dir.mkdir(parents=True, exist_ok=True)
    pending = state_dir / "pending-reindex.txt"
    try:
        existing = pending.read_text(encoding="utf-8").splitlines() if pending.exists() else []
    except OSError:
        existing = []
    if abs_path in existing:
        sys.exit(0)
    try:
        with pending.open("a", encoding="utf-8") as f:
            f.write(abs_path + "\n")
    except OSError:
        pass
    sys.exit(0)


if __name__ == "__main__":
    main()
