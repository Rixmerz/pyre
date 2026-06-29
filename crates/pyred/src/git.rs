//! Git repository status for a pane's working directory.
//!
//! Shells out to `git -C <cwd> status --porcelain=v1 --branch` and parses the
//! output without holding any blocking I/O on the async runtime.  Uses
//! `tokio::process::Command` so the exec is non-blocking.
//!
//! Returns `None` rather than an error on any "not a repo / git absent" path
//! — callers hide the chip instead of showing an error.

use std::path::Path;

use pyre_proto::GitInfo;

// ─────────────────────────────────────────────────────────────────────────────
// Pure parser — testable without spawning git
// ─────────────────────────────────────────────────────────────────────────────

/// Parse the stdout of `git status --porcelain=v1 --branch`.
///
/// The first line has the form `## <branch_info>`; every subsequent non-empty
/// line represents one modified or untracked file and is counted toward
/// `dirty`.
fn parse_porcelain(out: &str) -> GitInfo {
    let mut lines = out.lines();

    // The first line must start with "## "; anything else means the output is
    // unexpected — return a zero-value struct rather than panicking.
    let header = match lines.next() {
        Some(h) if h.starts_with("## ") => &h[3..],
        _ => {
            return GitInfo {
                branch: String::new(),
                dirty: 0,
                ahead: 0,
                behind: 0,
                upstream: None,
                cwd: None,
            }
        }
    };

    // Declared without an initializer so the compiler verifies every branch
    // below assigns exactly once before `branch` is used in `GitInfo { .. }`.
    let branch: String;
    let mut upstream: Option<String> = None;
    let mut ahead: u32 = 0;
    let mut behind: u32 = 0;

    if let Some(rest) = header.strip_prefix("No commits yet on ") {
        // "## No commits yet on main"
        branch = rest.to_string();
    } else if header.starts_with("HEAD (no branch)") {
        // Detached HEAD state
        branch = "HEAD".to_string();
    } else if let Some(dot_pos) = header.find("...") {
        // "## main...origin/main" or "## main...origin/main [ahead 1, behind 2]"
        branch = header[..dot_pos].to_string();
        let after = &header[dot_pos + 3..];
        if let Some(bracket_pos) = after.find(" [") {
            upstream = Some(after[..bracket_pos].to_string());
            let bracket_content = &after[bracket_pos + 2..];
            if let Some(n) = extract_count(bracket_content, "ahead ") {
                ahead = n;
            }
            if let Some(n) = extract_count(bracket_content, "behind ") {
                behind = n;
            }
        } else {
            upstream = Some(after.to_string());
        }
    } else {
        // "## main" — local branch, no upstream configured
        branch = header.to_string();
    }

    // Every non-empty line after the header represents one changed or
    // untracked file (XY + space + name in porcelain v1 format).
    let dirty = lines.filter(|l| !l.is_empty()).count() as u32;

    GitInfo {
        branch,
        dirty,
        ahead,
        behind,
        upstream,
        cwd: None,
    }
}

/// Find the numeric value following `prefix` inside `s`.
///
/// E.g. `extract_count("ahead 3, behind 1]", "ahead ")` → `Some(3)`.
fn extract_count(s: &str, prefix: &str) -> Option<u32> {
    let start = s.find(prefix)? + prefix.len();
    let rest = &s[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

// ─────────────────────────────────────────────────────────────────────────────
// Async entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Query `git` for repository status at `cwd`.
///
/// Returns `None` when `cwd` is not inside a git repository, when `git` is
/// not found on `PATH`, or on any other error.  Uses `tokio::process::Command`
/// so the subprocess does not block the async runtime.
pub async fn git_info(cwd: &Path) -> Option<GitInfo> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("status")
        .arg("--porcelain=v1")
        .arg("--branch")
        // Suppress pager and interactive prompts; inherit stdout/stderr is
        // the default for Command::output() which captures them.
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .ok()?;

    // Non-zero exit: not a repo, git absent, or other error → None.
    if !output.status.success() {
        return None;
    }

    let stdout = std::str::from_utf8(&output.stdout).ok()?;
    let mut info = parse_porcelain(stdout);
    info.cwd = Some(cwd.to_string_lossy().into_owned());
    Some(info)
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests — pure parser only, no git subprocess
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_with_upstream_and_ahead_behind() {
        let out = "## main...origin/main [ahead 2, behind 3]\n";
        let info = parse_porcelain(out);
        assert_eq!(info.branch, "main");
        assert_eq!(info.upstream.as_deref(), Some("origin/main"));
        assert_eq!(info.ahead, 2);
        assert_eq!(info.behind, 3);
        assert_eq!(info.dirty, 0);
    }

    #[test]
    fn dirty_count_no_upstream() {
        let out = "## feature-branch\n M src/foo.rs\n?? scratch.txt\n";
        let info = parse_porcelain(out);
        assert_eq!(info.branch, "feature-branch");
        assert!(info.upstream.is_none());
        assert_eq!(info.ahead, 0);
        assert_eq!(info.behind, 0);
        assert_eq!(info.dirty, 2);
    }

    #[test]
    fn clean_with_upstream_only() {
        let out = "## main...origin/main\n";
        let info = parse_porcelain(out);
        assert_eq!(info.branch, "main");
        assert_eq!(info.upstream.as_deref(), Some("origin/main"));
        assert_eq!(info.ahead, 0);
        assert_eq!(info.behind, 0);
        assert_eq!(info.dirty, 0);
    }

    #[test]
    fn detached_head() {
        let out = "## HEAD (no branch)\nM  Cargo.toml\n";
        let info = parse_porcelain(out);
        assert_eq!(info.branch, "HEAD");
        assert!(info.upstream.is_none());
        assert_eq!(info.dirty, 1);
    }

    #[test]
    fn no_commits_yet() {
        let out = "## No commits yet on main\n";
        let info = parse_porcelain(out);
        assert_eq!(info.branch, "main");
        assert!(info.upstream.is_none());
        assert_eq!(info.dirty, 0);
    }

    #[test]
    fn ahead_only() {
        let out = "## dev...origin/dev [ahead 1]\n";
        let info = parse_porcelain(out);
        assert_eq!(info.ahead, 1);
        assert_eq!(info.behind, 0);
    }

    #[test]
    fn behind_only() {
        let out = "## main...origin/main [behind 5]\n";
        let info = parse_porcelain(out);
        assert_eq!(info.ahead, 0);
        assert_eq!(info.behind, 5);
    }
}
