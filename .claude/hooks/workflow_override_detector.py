#!/usr/bin/env python3
"""Workflow Override Detector — PostToolUse hook.

Detects when the user explicitly rejects an auto-activated workflow by
calling ``graph_reset`` or activating a different workflow shortly after
an ``auto_activate_hit`` event. Emits ``user_override`` telemetry so M-F
metrics can compute false-positive rate of the auto-activate classifier.

Heuristic:
  - Tail orchestration.jsonl, find most recent ``auto_activate_hit``
  - If within OVERRIDE_WINDOW_SECONDS and not yet overridden, emit:
    * graph_reset                            → reason="reset"
    * graph_activate(name=X) where X != auto → reason="switched"

Protocol:
  stdin:  {"tool_name": "mcp__jig__graph_reset"|"mcp__jig__graph_activate",
           "tool_input": {...}, "tool_result": {...}}
  stdout: {"decision": "approve"}
  exit 0: always
"""
from __future__ import annotations

import json
import os
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

_APPROVE = json.dumps({"decision": "approve"})

OVERRIDE_WINDOW_SECONDS = 30 * 60  # 30 minutes
TAIL_BYTES = 64 * 1024  # last 64 KB of telemetry log


def _telemetry_path() -> Path:
    base = os.environ.get("JIG_TELEMETRY_DIR")
    if base:
        return Path(base) / "orchestration.jsonl"
    return Path.home() / ".local" / "share" / "jig" / "telemetry" / "orchestration.jsonl"


def _tail_events(path: Path) -> list[dict]:
    if not path.exists():
        return []
    try:
        size = path.stat().st_size
        with path.open("rb") as fh:
            if size > TAIL_BYTES:
                fh.seek(size - TAIL_BYTES)
                fh.readline()  # discard partial line
            blob = fh.read().decode("utf-8", errors="ignore")
    except OSError:
        return []
    out: list[dict] = []
    for line in blob.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return out


def _find_recent_hit(events: list[dict]) -> dict | None:
    """Return most recent auto_activate_hit within window, unless followed by user_override."""
    cutoff = datetime.now(tz=timezone.utc) - timedelta(seconds=OVERRIDE_WINDOW_SECONDS)
    last_hit: dict | None = None
    last_hit_idx: int = -1
    for i, ev in enumerate(events):
        if ev.get("kind") != "auto_activate_hit":
            continue
        ts_raw = ev.get("ts", "")
        try:
            ts = datetime.fromisoformat(ts_raw)
        except (TypeError, ValueError):
            continue
        if ts >= cutoff:
            last_hit = ev
            last_hit_idx = i
    if last_hit is None:
        return None
    # If a user_override already followed this hit (same prompt_hash), skip.
    target_hash = last_hit.get("prompt_hash")
    for ev in events[last_hit_idx + 1:]:
        if ev.get("kind") == "user_override" and ev.get("prompt_hash") == target_hash:
            return None
    return last_hit


def _emit(prompt_hash: str, extra: dict) -> None:
    try:
        from jig.engines.telemetry import record_intervention
        record_intervention("user_override", prompt_hash, extra)
    except Exception:
        pass


def main() -> None:
    try:
        payload = json.load(sys.stdin)
    except Exception:
        print(_APPROVE)
        return

    tool_name = payload.get("tool_name", "")
    if tool_name not in (
        "mcp__jig__graph_reset",
        "mcp__jig__graph_activate",
    ):
        print(_APPROVE)
        return

    events = _tail_events(_telemetry_path())
    hit = _find_recent_hit(events)
    if hit is None:
        print(_APPROVE)
        return

    auto_wf = (hit.get("extra") or {}).get("workflow", "")
    prompt_hash = hit.get("prompt_hash", "")

    if tool_name == "mcp__jig__graph_reset":
        _emit(prompt_hash, {"reason": "reset", "from": auto_wf})
    else:
        # graph_activate — only override if user picked a different workflow
        tool_input = payload.get("tool_input") or {}
        new_wf = tool_input.get("name") or tool_input.get("workflow_name") or ""
        if new_wf and new_wf != auto_wf:
            _emit(prompt_hash, {"reason": "switched", "from": auto_wf, "to": new_wf})

    print(_APPROVE)


if __name__ == "__main__":
    try:
        main()
    except Exception:
        print(_APPROVE)
