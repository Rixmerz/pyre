#!/usr/bin/env python3
"""Delegation Gate — PreToolUse hook enforcing "main agent never touches code".

When JIG_DELEGATION_ONLY=1 in the environment, the main Claude agent is
restricted to reading/writing files under `.claude/` only. All other
filesystem access (Read/Edit/Write/Glob/Grep) and shell execution
(Bash) on paths outside `.claude/` is blocked.

Subagents (Task tool invocations) bypass the gate. Detection cascade
(any match → bypass):

1. ``parent_tool_use_id`` field on the hook payload (older Claude Code).
2. Most recent assistant message in the transcript has
   ``isSidechain: true`` — Claude Code's canonical marker for subagent
   conversations.
3. Most recent assistant message has a ``parentUuid`` linking it to a
   prior Task tool_use call.

Fails open: any parse error → approve. The intent is to discipline the
main agent, not to break subagents.

Protocol:
  stdin:  {tool_name, tool_input, session_id, transcript_path, cwd, ...}
  env:    JIG_DELEGATION_ONLY=1 to enable; otherwise no-op
          JIG_DELEGATION_DEBUG=1 to log payloads to
            ~/.local/share/jig/telemetry/delegation_gate.jsonl
  stdout: {"decision":"block","reason":"..."} when blocking
  exit 0: always
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

_ALLOW_ROOT = ".claude"
_GATED_TOOLS = {"Read", "Edit", "Write", "Glob", "Grep", "NotebookEdit"}


def _approve() -> None:
    sys.stdout.write(json.dumps({"decision": "approve"}))
    sys.exit(0)


def _block(reason: str) -> None:
    sys.stdout.write(json.dumps({"decision": "block", "reason": reason}))
    sys.exit(0)


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
            p = Path(raw)
    claude_dir = (project_dir / _ALLOW_ROOT).resolve()
    try:
        p.relative_to(claude_dir)
        return True
    except ValueError:
        return False


def _tail_mtime(path: Path) -> float:
    try:
        return path.stat().st_mtime
    except OSError:
        return 0.0


def _is_subagent_call(payload: dict) -> bool:
    """Detect whether this tool call originates inside a subagent.

    Claude Code 2.x sends the *main session* ``transcript_path`` to
    hooks even when the call originates from a subagent (Task tool).
    Subagent tool calls are written only to the subagent's own
    transcript at ``<session_dir>/subagents/agent-<id>.jsonl``, not
    to the main transcript. Therefore the reliable signal is
    cross-file mtime comparison: if any subagent jsonl was modified
    more recently than the main transcript, the active write is
    from a subagent.

    Detection order (cheap → expensive):
      1. ``parent_tool_use_id`` / ``parentToolUseID`` payload field.
      2. ``/subagents/`` in ``transcript_path``.
      3. mtime of newest ``subagents/agent-*.jsonl`` > mtime of main
         transcript (with a small grace window for clock skew).
    """
    if payload.get("parent_tool_use_id") or payload.get("parentToolUseID"):
        return True
    tp = payload.get("transcript_path", "") or ""
    if "/subagents/" in tp or "\\subagents\\" in tp:
        return True
    if not tp:
        return False
    try:
        main_path = Path(tp)
        sess_dir = main_path.with_suffix("")
        subagent_dir = sess_dir / "subagents"
        if not subagent_dir.is_dir():
            return False
        main_mtime = _tail_mtime(main_path)
        for sf in subagent_dir.glob("agent-*.jsonl"):
            if _tail_mtime(sf) >= main_mtime - 10.0:
                return True
    except OSError:
        pass
    return False


def _debug_log(payload: dict, decision: str, reason: str = "") -> None:
    if os.environ.get("JIG_DELEGATION_DEBUG", "0") != "1":
        return
    try:
        log_dir = Path.home() / ".local" / "share" / "jig" / "telemetry"
        log_dir.mkdir(parents=True, exist_ok=True)
        log_dir.joinpath("delegation_gate.jsonl").open("a").write(
            json.dumps({
                "tool": payload.get("tool_name"),
                "decision": decision,
                "reason": reason,
                "is_subagent_keys": [
                    k for k in ("parent_tool_use_id", "parentToolUseID") if payload.get(k)
                ],
                "payload_keys": sorted(payload.keys()),
                "cwd": payload.get("cwd"),
                "transcript_path": payload.get("transcript_path"),
                "session_id": payload.get("session_id"),
            }) + "\n"
        )
    except OSError:
        pass


def main() -> None:
    if os.environ.get("JIG_DELEGATION_ONLY", "0") != "1":
        _approve()

    try:
        payload = json.loads(sys.stdin.read() or "{}")
    except json.JSONDecodeError:
        _approve()

    # Subagent bypass.
    if _is_subagent_call(payload):
        _debug_log(payload, "approve", "subagent")
        _approve()

    tool = payload.get("tool_name", "")
    tin = payload.get("tool_input", {}) or {}
    project_dir = Path(os.environ.get("CLAUDE_PROJECT_DIR", os.getcwd()))

    if tool in _GATED_TOOLS:
        target = (
            tin.get("file_path")
            or tin.get("path")
            or tin.get("pattern")
            or tin.get("notebook_path")
            or ""
        )
        scope = tin.get("path") or ""
        candidate = target if target else scope
        if not candidate:
            reason = (
                f"{tool} blocked: delegation-only mode. Spawn a specialized "
                f"subagent (dcc-specialist, livespec-specialist, reviewer, "
                f"backend, …) instead. Main agent can only touch .claude/."
            )
            _debug_log(payload, "block", reason)
            _block(reason)
        if _path_inside_claude(candidate, project_dir):
            _debug_log(payload, "approve", f".claude path: {candidate}")
            _approve()
        reason = (
            f"{tool} on '{candidate}' blocked: delegation-only mode. "
            f"Main agent may only read/write under .claude/. Delegate code "
            f"inspection to a specialized subagent via the Task tool."
        )
        _debug_log(payload, "block", reason)
        _block(reason)

    if tool == "Bash":
        cmd = (tin.get("command") or "").strip()
        safe_starts = (
            "ls .claude", "ls ./.claude", "cat .claude/", "cat ./.claude/",
            "find .claude", "find ./.claude",
            "git status", "git log", "git diff --stat", "git branch",
            "pwd", "whoami", "date", "echo ",
            # Read-only tmux metadata queries — required by /setup-agents and
            # /sprint Step 3.5 to resolve the current session name before
            # calling tmux_handoff_goal / tmux_clear_and_prompt. These commands
            # cannot edit project code: they only read tmux's own state.
            "tmux display-message ", "tmux display-message\t",
            "tmux list-sessions", "tmux ls",
            "tmux list-panes", "tmux list-windows",
            "tmux has-session",
        )
        if cmd.startswith(safe_starts):
            _debug_log(payload, "approve", f"safe bash: {cmd[:40]}")
            _approve()
        reason = (
            f"Bash blocked: delegation-only mode. Command '{cmd[:80]}' "
            f"may touch project code. Delegate to a specialized subagent."
        )
        _debug_log(payload, "block", reason)
        _block(reason)

    _approve()


if __name__ == "__main__":
    main()
