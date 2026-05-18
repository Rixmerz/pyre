#!/usr/bin/env python3
"""Auto Pre-Plan — UserPromptSubmit hook.

On clearly multi-step prompts (≥3 imperative verbs or explicit phase
markers) where no workflow is active, emits a *directive* injection
block pushing the agent to plan or create a workflow before writing code.

This is a stronger sibling to ``workflow_suggester.py``; both hooks are
intentionally decoupled (no shared imports).

Protocol:
  stdin:  {"prompt": "...", "hook_event_name": "UserPromptSubmit", ...}
  stdout: optional injection block (shown to Claude)
  exit 0: always
"""
from __future__ import annotations

import hashlib
import json
import os
import re
import sys
from pathlib import Path

# ---------------------------------------------------------------------------
# Tunables
# ---------------------------------------------------------------------------

MIN_PROMPT_CHARS: int = 60


def _prompt_hash(prompt: str) -> str:
    return hashlib.sha256(prompt.encode()).hexdigest()[:12]


def _emit_pre_plan(prompt: str, variant: str, reason: str) -> None:
    try:
        from jig.engines.telemetry import record_intervention
        record_intervention(
            "pre_plan_emit",
            _prompt_hash(prompt),
            {"variant": variant, "reason": reason},
        )
    except Exception:
        pass
VERB_THRESHOLD: int = 3

# ---------------------------------------------------------------------------
# Regex patterns
# ---------------------------------------------------------------------------

QUESTION_PATTERNS: re.Pattern[str] = re.compile(
    r"^\s*(why|how|what|when|where|which|que|por\s*que|como|cuando|donde|"
    r"can\s+you\s+explain|explain|explica|tell\s+me)\b",
    re.IGNORECASE,
)

# Bilingual imperative verb list (EN + ES)
_VERBS: str = (
    r"implement(a)?|build|construye|create|crea|"
    r"refactor(iza)?|migrate|migra|"
    r"fix|arregla|"
    r"add|agrega|"
    r"write|escribe|"
    r"deploy|despliega|"
    r"test(ea)?|"
    r"design|disena|"
    r"document(a)?"
)
IMPERATIVE_VERB_PATTERN: re.Pattern[str] = re.compile(
    rf"\b({_VERBS})\b",
    re.IGNORECASE,
)

SEQUENCE_MARKER_PATTERN: re.Pattern[str] = re.compile(
    r"\b(then|y\s+luego|y\s+despues|after\s+that|"
    r"first.{0,30}(then|second|next)|"
    r"step\s*\d|fase|phase|wave)\b",
    re.IGNORECASE,
)

# Variant classification
BUG_KEYWORDS: re.Pattern[str] = re.compile(
    r"\b(bug|fix|regression|debug|broken|failing|crash|error)\b",
    re.IGNORECASE,
)
REFACTOR_KEYWORDS: re.Pattern[str] = re.compile(
    r"\b(refactor|cleanup|clean.?up|restructure|reorganize|extract|simplify)\b",
    re.IGNORECASE,
)

# ---------------------------------------------------------------------------
# Workflow state detection (copied from workflow_suggester — no import)
# ---------------------------------------------------------------------------

def _state_path() -> Path | None:
    project_dir = os.environ.get("CLAUDE_PROJECT_DIR")
    if not project_dir:
        return None
    name = Path(project_dir).name
    xdg = (
        Path.home()
        / ".local"
        / "share"
        / "jig"
        / "states"
        / name
        / "graph_state.json"
    )
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
    return bool(
        data.get("graph_id") or data.get("graph_name") or data.get("current_node")
    )


# ---------------------------------------------------------------------------
# Detection helpers
# ---------------------------------------------------------------------------

def _count_distinct_verbs(prompt: str) -> int:
    """Return the number of distinct imperative verb stems found in *prompt*."""
    distinct: set[str] = set()
    for m in IMPERATIVE_VERB_PATTERN.finditer(prompt):
        distinct.add(m.group(0).lower())
    return len(distinct)


def _pick_variant(prompt: str) -> str:
    if BUG_KEYWORDS.search(prompt):
        return "debug"
    if REFACTOR_KEYWORDS.search(prompt):
        return "refactor"
    return "feature-dev"


_VARIANT_NODE_SHAPES: dict[str, str] = {
    "feature-dev": "orient → design → implement → test → validate → commit",
    "debug": "reproduce → diagnose → fix → verify → commit",
    "refactor": "analyze → extract → refactor → verify",
}

_VARIANT_REASONS: dict[str, str] = {
    "≥3 imperative verbs": "feature-dev",
    "explicit phase markers": "feature-dev",
}


def _is_multi_step(prompt: str) -> tuple[bool, str]:
    """Return (is_multi_step, reason_string)."""
    if len(prompt) < MIN_PROMPT_CHARS:
        return False, ""
    if QUESTION_PATTERNS.match(prompt):
        return False, ""
    if SEQUENCE_MARKER_PATTERN.search(prompt):
        return True, "explicit phase markers"
    if _count_distinct_verbs(prompt) >= VERB_THRESHOLD:
        return True, "≥3 imperative verbs"
    return False, ""


# ---------------------------------------------------------------------------
# Injection template
# ---------------------------------------------------------------------------

def _build_injection(reason: str, variant: str) -> str:
    node_shape = _VARIANT_NODE_SHAPES[variant]
    return (
        f"## Multi-step task detected — plan before coding\n\n"
        f"This prompt looks like a multi-step task ({reason}). Before editing code:\n"
        f"1. List ≥3 concrete sub-tasks.\n"
        f"2. If this is a recurring pattern: build a workflow with `graph_builder_create` "
        f"(`{variant}` shape works well: {node_shape}).\n"
        f"3. If one-shot: enter plan mode (EnterPlanMode) and confirm scope before writing.\n\n"
        f"Suggested `{variant}` node shape: {node_shape}\n\n"
        f"Skip this preamble only if the work is genuinely single-shot."
    )


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

def main() -> None:
    if os.environ.get("JIG_AUTO_PRE_PLAN", "1") == "0":
        sys.exit(0)

    try:
        payload = json.load(sys.stdin)
    except Exception:
        sys.exit(0)

    prompt: str = (payload.get("prompt") or "").strip()
    if not prompt:
        sys.exit(0)
    # Slash commands have their own orchestration — don't prepend a plan
    # template that fights the command.
    if prompt.startswith("/"):
        sys.exit(0)

    triggered, reason = _is_multi_step(prompt)
    if not triggered:
        sys.exit(0)

    if _has_active_workflow():
        sys.exit(0)

    variant = _pick_variant(prompt)
    _emit_pre_plan(prompt, variant, reason)
    print(_build_injection(reason, variant))
    sys.exit(0)


if __name__ == "__main__":
    main()
