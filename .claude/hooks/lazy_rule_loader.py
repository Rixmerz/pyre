#!/usr/bin/env python3
"""Lazy Rule Loader — UserPromptSubmit hook.

Scans .claude/rules/lazy/*.md for rules with YAML frontmatter containing
`keywords` lists. Injects matched rules as additional context when the user
prompt contains any of the keywords (single regex pass, cheap).

Protocol:
  stdin:  {"session_id": "...", "hook_event_name": "UserPromptSubmit", "prompt": "..."}
  stdout: injected context block (shown to Claude as additional context)
  exit 0: always — never block user prompts
"""
from __future__ import annotations

import json
import os
import re
import sys
from pathlib import Path


def _parse_frontmatter(text: str) -> tuple[dict, str]:
    """Parse YAML-ish frontmatter. Returns (meta, body). No PyYAML dep for simple lists."""
    if not text.startswith("---"):
        return {}, text
    end = text.find("---", 3)
    if end == -1:
        return {}, text
    fm: dict = {}
    current_list_key: str | None = None
    for line in text[3:end].splitlines():
        stripped = line.strip()
        if stripped.startswith("- "):
            val = stripped[2:].strip()
            if current_list_key is not None:
                if not isinstance(fm.get(current_list_key), list):
                    fm[current_list_key] = []
                fm[current_list_key].append(val)
        elif ":" in line and not line.startswith(" "):
            key, _, val = line.partition(":")
            key = key.strip()
            val = val.strip()
            # Inline list: keywords: [a, b, c]
            if val.startswith("[") and val.endswith("]"):
                items = [x.strip().strip("'\"") for x in val[1:-1].split(",") if x.strip()]
                fm[key] = items
                current_list_key = None
            else:
                fm[key] = val if val else None
                current_list_key = key if not val else None
        else:
            current_list_key = None
    body = text[end + 3:].lstrip("\n")
    return fm, body


def _find_lazy_rules_dir() -> Path | None:
    project_dir = os.environ.get("CLAUDE_PROJECT_DIR", "")
    if project_dir:
        p = Path(project_dir) / ".claude" / "rules" / "lazy"
        if p.is_dir():
            return p
    # Fallback: walk up from cwd
    cwd = Path.cwd()
    for parent in [cwd, *cwd.parents]:
        p = parent / ".claude" / "rules" / "lazy"
        if p.is_dir():
            return p
    return None


def load_lazy_rules(lazy_dir: Path) -> list[tuple[list[str], str]]:
    """Load all lazy rules. Returns list of (keywords, body) tuples."""
    rules = []
    for f in sorted(lazy_dir.glob("*.md")):
        try:
            text = f.read_text(encoding="utf-8")
            fm, body = _parse_frontmatter(text)
            kws = fm.get("keywords", [])
            if isinstance(kws, list) and kws:
                rules.append((kws, body))
        except Exception:
            continue
    return rules


def match_rules(prompt: str, rules: list[tuple[list[str], str]]) -> list[str]:
    """Return bodies of rules whose keywords appear in the lowercased prompt."""
    lower = prompt.lower()
    matched = []
    for keywords, body in rules:
        pattern = r"\b(?:" + "|".join(re.escape(kw) for kw in keywords) + r")\b"
        if re.search(pattern, lower):
            matched.append(body)
    return matched


def main() -> None:
    try:
        payload = json.load(sys.stdin)
    except Exception:
        sys.exit(0)

    prompt = payload.get("prompt", "")
    if not prompt:
        sys.exit(0)

    lazy_dir = _find_lazy_rules_dir()
    if lazy_dir is None:
        sys.exit(0)

    rules = load_lazy_rules(lazy_dir)
    if not rules:
        sys.exit(0)

    matched = match_rules(prompt, rules)
    if not matched:
        sys.exit(0)

    print("## Lazy Rules (loaded on demand)\n")
    print("\n\n---\n\n".join(matched))
    sys.exit(0)


if __name__ == "__main__":
    main()
