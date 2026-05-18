#!/usr/bin/env python3
"""Workflow Auto-Suggest — UserPromptSubmit hook.

Detects multi-step task intent in the user prompt and, when no workflow
is currently active, injects a hint nudging the agent to call
``graph_list_available`` and pick a workflow before diving in.

Quiet by default: emits nothing for trivial prompts, questions, or when
a graph is already active. The hint is advisory — the agent can still
proceed without a workflow.

Protocol:
  stdin:  {"prompt": "...", "hook_event_name": "UserPromptSubmit", ...}
  stdout: optional context block (shown to Claude)
  exit 0: always
"""
from __future__ import annotations

import hashlib
import json
import os
import re
import sys
from pathlib import Path

MIN_PROMPT_CHARS = 60


def _prompt_hash(prompt: str) -> str:
    return hashlib.sha256(prompt.encode()).hexdigest()[:12]


def _emit(kind: str, prompt: str, **extra: object) -> None:
    try:
        from jig.engines.telemetry import record_intervention
        record_intervention(kind, _prompt_hash(prompt), dict(extra) if extra else None)
    except Exception:
        pass

INTENT_PATTERNS = re.compile(
    r"\b("
    r"implement|implementa|build|construye|create|crea|design|disena|"
    r"refactor|refactoriza|migrate|migra|port|portea|"
    r"add\s+(feature|endpoint|module|page|component|support)|"
    r"agrega(r)?\s+(feature|endpoint|modulo|pagina|componente|soporte)|"
    r"fix\s+(bug|issue|regression|failing|broken)|"
    r"arregla|debug(ear)?|"
    r"deploy|despliega|integrate|integra|"
    r"set\s*up|setup|scaffold|"
    r"write\s+(tests|integration|e2e)|"
    r"audit|review|revisa|"
    r"optimi[sz]e|optimiza|"
    r"sprint|roadmap|epic|"
    r"plan\s+(out|the|a)|disena(r)?"
    r")\b",
    re.IGNORECASE,
)

MULTI_STEP_HINTS = re.compile(
    r"\b(then|y\s+despues|y\s+luego|after\s+that|next\s+step|first|second|"
    r"step\s+\d|fase|phase|wave|primero|luego|finalmente|finally)\b",
    re.IGNORECASE,
)

QUESTION_PATTERNS = re.compile(
    r"^\s*(why|how|what|when|where|which|que|por\s*que|como|cuando|donde|"
    r"can\s+you\s+explain|explain|explica|tell\s+me)\b",
    re.IGNORECASE,
)


def _state_path() -> Path | None:
    project_dir = os.environ.get("CLAUDE_PROJECT_DIR")
    if not project_dir:
        return None
    name = Path(project_dir).name
    xdg = Path.home() / ".local" / "share" / "jig" / "states" / name / "graph_state.json"
    if xdg.exists():
        return xdg
    local = Path(project_dir) / ".claude" / "workflow" / "graph_state.json"
    return local if local.exists() else None


def _has_active_workflow() -> bool:
    p = _state_path()
    if not p:
        return False
    try:
        data = json.loads(p.read_text(encoding="utf-8"))
    except Exception:
        return False
    if not data.get("active", False):
        return False
    return bool(data.get("graph_id") or data.get("graph_name") or data.get("current_node"))


def _looks_multi_step(prompt: str) -> bool:
    if len(prompt) < MIN_PROMPT_CHARS:
        return False
    if QUESTION_PATTERNS.match(prompt):
        return False
    if MULTI_STEP_HINTS.search(prompt):
        return True
    return bool(INTENT_PATTERNS.search(prompt))


_AUTO_ACTIVATE_THRESHOLD = 0.85
_SUGGEST_THRESHOLD = 0.65


def _try_auto_activate(workflow_name: str, project_dir: str) -> str | None:
    """Best-effort sync activation. Returns activated graph name or None."""
    try:
        from jig.engines.workflow_scope import resolve_workflow_dirs
        from jig.engines.graph_parser import load_graph_from_file
        from jig.engines.graph_state import (
            get_graph_file,
            initialize_graph_state,
        )
    except Exception:
        return None

    graph_file = None
    for _scope, workflows_dir in reversed(resolve_workflow_dirs(project_dir)):
        for stem in (f"{workflow_name}-graph.yaml", f"{workflow_name}.yaml"):
            cand = workflows_dir / stem
            if cand.exists():
                graph_file = cand
                break
        if graph_file is not None:
            break
    if graph_file is None:
        return None

    try:
        graph = load_graph_from_file(graph_file)
        target = get_graph_file(project_dir)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(graph_file.read_text(encoding="utf-8"), encoding="utf-8")
        initialize_graph_state(project_dir, graph, workflow_name)
        return graph.metadata.get("name", workflow_name)
    except Exception:
        return None


def main() -> None:
    if os.environ.get("JIG_WORKFLOW_SUGGEST", "1") == "0":
        sys.exit(0)
    try:
        payload = json.load(sys.stdin)
    except Exception:
        sys.exit(0)

    prompt = (payload.get("prompt") or "").strip()
    # Slash commands carry their own orchestration logic — don't reclassify
    # them and risk auto-activating a workflow that fights the command.
    if prompt.startswith("/"):
        sys.exit(0)
    if not prompt or not _looks_multi_step(prompt):
        sys.exit(0)

    if _has_active_workflow():
        sys.exit(0)

    # Try richer intent classification (regex tier for auto-activate).
    match = None
    try:
        from jig.orchestration import classify_intent

        match = classify_intent(prompt)
    except Exception:
        match = None

    auto_on = os.environ.get("JIG_AUTO_ACTIVATE", "0") == "1"
    project_dir = os.environ.get("CLAUDE_PROJECT_DIR", "")

    if (
        match is not None
        and match.confidence >= _AUTO_ACTIVATE_THRESHOLD
        and auto_on
        and project_dir
    ):
        activated = _try_auto_activate(match.workflow_name, project_dir)
        if activated:
            _emit("auto_activate_hit", prompt, workflow=activated, confidence=match.confidence)
            print(
                f"## Workflow auto-activated\n"
                f"Activated `{activated}` "
                f"(confidence {match.confidence:.2f}, reason `{match.reason}`).\n"
                f"Use `graph_status` to see the current phase."
            )
            sys.exit(0)

    if match is not None and match.confidence >= _SUGGEST_THRESHOLD:
        _emit("auto_activate_miss", prompt, workflow=match.workflow_name, confidence=match.confidence)
        print(
            f"## Workflow suggestion\n"
            f"Detected intent: `{match.workflow_name}` "
            f"(confidence {match.confidence:.2f}, reason `{match.reason}`).\n"
            f"  1. `graph_activate(name=\"{match.workflow_name}\")` to start.\n"
            f"  2. Or `graph_list_available` to pick another.\n"
            f"Set `JIG_AUTO_ACTIVATE=1` to skip this prompt next time."
        )
        sys.exit(0)

    print(
        "## Workflow suggestion\n"
        "Multi-step task detected. Before implementing, consider:\n"
        "  1. `graph_list_available` — discover existing workflows (debug, feature-dev, etc.)\n"
        "  2. `graph_activate(name=...)` — pick one if it fits, or build a new one with `graph_builder_create`.\n"
        "Workflows enforce phase discipline, persist context, and inject DCC/experience feedback at the right moments.\n"
        "Skip only if the task truly is single-shot."
    )
    sys.exit(0)


if __name__ == "__main__":
    main()
