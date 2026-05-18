#!/usr/bin/env python3
"""Session Bootstrap — SessionStart hook.

Injects pending task context and DCC health warning at session open.
Self-contained: no jig imports.

Protocol:
  stdin:  {"session_id": "...", "hook_event_name": "SessionStart"}
  stdout: context block shown to Claude at session start
  stderr: warnings (DCC scope issues)
  exit 0: always
"""
from __future__ import annotations

import json
import os
import re
import signal
import sqlite3
import subprocess
import sys
from pathlib import Path


def _timeout_handler(signum: int, frame: object) -> None:
    sys.exit(0)


def _read_next_task(project_dir: str) -> str | None:
    """Load next_task entry for this project and format it as markdown."""
    project_name = Path(project_dir).name
    task_path = Path.home() / ".local" / "share" / "jig" / "next_task" / f"{project_name}.json"
    if not task_path.exists():
        return None
    try:
        data = json.loads(task_path.read_text(encoding="utf-8"))
    except Exception:
        return None
    task_desc = data.get("task_description", "")
    summary = data.get("summary", "")
    files = data.get("files_changed", [])
    saved_at = data.get("saved_at", "")
    if not task_desc and not summary:
        return None
    lines = ["### Pending Task"]
    if task_desc:
        lines.append(f"**Task:** {task_desc}")
    if summary:
        lines.append(f"**Summary:** {summary}")
    if files:
        files_str = ", ".join(files[:5])
        if len(files) > 5:
            files_str += f" (+{len(files) - 5} more)"
        lines.append(f"**Files touched:** {files_str}")
    if saved_at:
        lines.append(f"**Saved:** {saved_at}")
    return "\n".join(lines)


def _changed_files_vs_main(project_dir: str) -> list[str]:
    """Return files changed on current branch vs main/master (no fetch). Empty list on any error."""
    base_candidates = ("origin/main", "main", "origin/master", "master")
    for base in base_candidates:
        try:
            r = subprocess.run(
                ["git", "diff", "--name-only", f"{base}...HEAD"],
                cwd=project_dir, capture_output=True, text=True, timeout=2,
            )
            if r.returncode == 0:
                files = [ln.strip() for ln in r.stdout.splitlines() if ln.strip()]
                if files:
                    return files
        except Exception:
            continue
    try:
        r = subprocess.run(
            ["git", "status", "--porcelain"],
            cwd=project_dir, capture_output=True, text=True, timeout=2,
        )
        if r.returncode == 0:
            return [ln[3:].strip() for ln in r.stdout.splitlines() if ln.strip()]
    except Exception:
        pass
    return []


def _entry_matches_file(entry: dict, file_path: str) -> bool:
    pattern = entry.get("file_pattern", "")
    if not pattern:
        return False
    try:
        regex = pattern.replace(".", r"\.").replace("*", ".*")
        if re.fullmatch(regex, file_path):
            return True
        if str(Path(pattern).parent) and str(Path(pattern).parent) == str(Path(file_path).parent):
            return True
    except re.error:
        return False
    return False


def _read_recent_experience(project_dir: str, n: int = 3) -> str | None:
    """Return a markdown block with experience entries.

    Prefers entries that match files on the current branch (vs main/master);
    falls back to global top-n by occurrences when no diff is available
    or no matches are found.
    """
    project_name = Path(project_dir).name
    exp_path = (
        Path.home()
        / ".local"
        / "share"
        / "jig"
        / "project_memories"
        / project_name
        / "experience_memory.json"
    )
    if not exp_path.exists():
        return None
    try:
        data = json.loads(exp_path.read_text(encoding="utf-8"))
        entries = data.get("entries", [])
        if not entries:
            return None
        changed = _changed_files_vs_main(project_dir)
        scoped: list[dict] = []
        if changed:
            seen_ids = set()
            for f in changed:
                for e in entries:
                    eid = e.get("id") or e.get("file_pattern") or id(e)
                    if eid in seen_ids:
                        continue
                    if _entry_matches_file(e, f):
                        seen_ids.add(eid)
                        scoped.append(e)
            scoped.sort(key=lambda e: e.get("occurrences", 0), reverse=True)
        if scoped:
            top = scoped[:n]
            header = f"### Recent Experience (matches on {len(changed)} changed file{'s' if len(changed) != 1 else ''})"
        else:
            top = sorted(entries, key=lambda e: e.get("occurrences", 0), reverse=True)[:n]
            header = "### Recent Experience"
        lines = [header]
        for e in top:
            pattern = e.get("file_pattern", "?")
            etype = e.get("type", "?")
            resolution = (e.get("resolution") or "")[:80]
            occ = e.get("occurrences", 0)
            lines.append(f"- `{pattern}` ({etype}, ×{occ}): {resolution}")
        return "\n".join(lines)
    except Exception:
        return None


def _read_project_metadata(project_dir: str) -> str | None:
    """Return a compact tech-stack summary from cached project metadata.

    Loads only — never triggers discovery (cache miss → silent skip).
    """
    try:
        from jig.engines.project_metadata import ProjectMetadata
        from jig.engines.graph_state import _get_centralized_state_dir
    except Exception:
        return None
    try:
        state_dir = str(_get_centralized_state_dir(project_dir))
        meta = ProjectMetadata.load(project_dir, state_dir)
    except Exception:
        return None
    if meta is None:
        return None
    try:
        all_data = meta.get()
    except Exception:
        return None

    parts: list[str] = ["### Project Metadata"]
    ts = all_data.get("tech_stack") or {}
    langs = ts.get("languages") or []
    frameworks = ts.get("frameworks") or []
    test_runners = ts.get("test_runners") or ts.get("test_patterns") or []
    if langs:
        parts.append(f"- **Languages:** {', '.join(langs[:6])}")
    if frameworks:
        fw_names = frameworks if isinstance(frameworks, list) else [str(frameworks)]
        parts.append(f"- **Frameworks:** {', '.join(str(f) for f in fw_names[:6])}")
    if test_runners:
        tr_names = test_runners if isinstance(test_runners, list) else [str(test_runners)]
        parts.append(f"- **Test runners:** {', '.join(str(t) for t in tr_names[:4])}")

    bc = all_data.get("bounded_contexts") or {}
    bc_count = bc.get("count")
    if bc_count:
        names = bc.get("names") or bc.get("contexts") or []
        if names:
            parts.append(f"- **Bounded contexts ({bc_count}):** {', '.join(str(n) for n in names[:6])}")
        else:
            parts.append(f"- **Bounded contexts:** {bc_count}")

    mig = all_data.get("migration_number") or {}
    if mig.get("last_number") is not None:
        parts.append(f"- **Migrations:** last={mig.get('last_number')}, next={mig.get('next_number')}")

    if len(parts) == 1:
        return None
    return "\n".join(parts)


def _check_dcc_scope(project_dir: str) -> str | None:
    """Return warning string if dcc.db has no files from this project."""
    db_path = Path.home() / ".local" / "share" / "jig" / "dcc.db"
    if not db_path.exists() or db_path.stat().st_size == 0:
        return None  # not indexed at all — doctor will catch this
    try:
        conn = sqlite3.connect(str(db_path), timeout=2)
        cur = conn.execute(
            "SELECT COUNT(*) FROM code_points WHERE file_path LIKE ? ESCAPE '\\'",
            (project_dir.rstrip("/") + "/%",),
        )
        count: int = cur.fetchone()[0]
        conn.close()
    except Exception:
        return None
    if count == 0:
        return (
            f"[DCC] dcc.db has data but 0 files from {Path(project_dir).name} — "
            "run cube_index_directory(path='src/') to index this project"
        )
    return None


def main() -> None:
    signal.signal(signal.SIGALRM, _timeout_handler)
    signal.alarm(5)

    try:
        json.load(sys.stdin)
    except Exception:
        pass

    project_dir = os.environ.get("CLAUDE_PROJECT_DIR", "")
    if not project_dir:
        sys.exit(0)

    sections: list[str] = []

    next_task_block = _read_next_task(project_dir)
    if next_task_block:
        sections.append(next_task_block)

    try:
        metadata_block = _read_project_metadata(project_dir)
        if metadata_block:
            sections.append(metadata_block)
    except Exception:
        pass

    try:
        experience_block = _read_recent_experience(project_dir)
        if experience_block:
            sections.append(experience_block)
    except Exception:
        pass

    dcc_warning = _check_dcc_scope(project_dir)
    if dcc_warning:
        print(dcc_warning, file=sys.stderr)

    if sections:
        print("## Session Context\n")
        print("\n\n".join(sections))

    sys.exit(0)


if __name__ == "__main__":
    main()
