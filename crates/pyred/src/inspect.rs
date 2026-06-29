//! `/proc`-based PID inspection for the `inspect_pid` RPC.
//!
//! Linux-only implementation. On other platforms every field is empty /
//! zeroed. Callers should not panic on empty results; they just mean the
//! process died between the RPC call and the read.

use pyre_proto::PidInspect;

/// Return process metadata for `pid`.
///
/// Errors inside individual `/proc` reads are silently skipped — the function
/// never panics and always returns a well-formed `PidInspect`.
#[cfg(target_os = "linux")]
pub fn inspect_pid(pid: u32) -> PidInspect {
    let comm = read_comm(pid);
    let env = read_env(pid);
    let fds = read_fds(pid);
    let children = read_children(pid);
    PidInspect {
        pid,
        comm,
        env,
        fds,
        children,
    }
}

#[cfg(not(target_os = "linux"))]
pub fn inspect_pid(pid: u32) -> PidInspect {
    PidInspect {
        pid,
        comm: "unsupported".to_owned(),
        env: Vec::new(),
        fds: Vec::new(),
        children: Vec::new(),
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn read_comm(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Read /proc/{pid}/environ (NUL-separated KEY=VALUE pairs). Caps at 50
/// entries; values truncated to 80 chars.
#[cfg(target_os = "linux")]
fn read_env(pid: u32) -> Vec<(String, String)> {
    let raw = match std::fs::read(format!("/proc/{pid}/environ")) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    raw.split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .take(50)
        .filter_map(|entry| {
            let s = String::from_utf8_lossy(entry);
            let mut parts = s.splitn(2, '=');
            let key = parts.next()?.to_owned();
            let val_full = parts.next().unwrap_or("").to_owned();
            let val = if val_full.len() > 80 {
                // Use floor_char_boundary so we never split a multibyte
                // codepoint (a byte-index slice would panic in that case).
                let end = val_full.floor_char_boundary(80);
                val_full[..end].to_owned()
            } else {
                val_full
            };
            Some((key, val))
        })
        .collect()
}

/// Read symlinks from /proc/{pid}/fd; cap at 50.
#[cfg(target_os = "linux")]
fn read_fds(pid: u32) -> Vec<String> {
    let dir = match std::fs::read_dir(format!("/proc/{pid}/fd")) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    dir.filter_map(|entry| entry.ok())
        .take(50)
        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// Collect direct child PIDs by reading /proc/{pid}/task/*/children.
#[cfg(target_os = "linux")]
fn read_children(pid: u32) -> Vec<u32> {
    let task_dir = match std::fs::read_dir(format!("/proc/{pid}/task")) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut children: Vec<u32> = task_dir
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let children_path = entry.path().join("children");
            std::fs::read_to_string(children_path).ok()
        })
        .flat_map(|content| {
            content
                .split_whitespace()
                .filter_map(|s| s.parse::<u32>().ok())
                .collect::<Vec<u32>>()
        })
        .collect();
    children.sort_unstable();
    children.dedup();
    children
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn inspect_self_pid() {
        let pid = std::process::id();
        let info = inspect_pid(pid);
        assert_eq!(info.pid, pid);
        // comm should be non-empty (the test binary name).
        assert!(!info.comm.is_empty(), "comm should not be empty for self");
        // env should have entries (test runs with environment).
        assert!(!info.env.is_empty(), "env should be non-empty for self");
        // fds should contain at least stdin/stdout/stderr.
        assert!(!info.fds.is_empty(), "fds should be non-empty for self");
        // No value should exceed 80 chars.
        for (_k, v) in &info.env {
            assert!(v.len() <= 80, "env value exceeds 80 chars");
        }
    }

    /// Env value where a 3-byte UTF-8 codepoint (€ = 0xE2 0x82 0xAC) straddles
    /// byte index 80 must not panic and must produce valid UTF-8.
    #[test]
    fn env_value_multibyte_at_boundary_does_not_panic() {
        // Build a string: 78 ASCII 'a' chars followed by '€' (3 bytes).
        // Total len = 81 bytes — boundary at 80 falls inside the 3-byte char.
        let mut val = "a".repeat(78);
        val.push('€'); // 3-byte char; bytes 78-80 inclusive
        assert_eq!(val.len(), 81);

        // Simulate what read_env does.
        let end = val.floor_char_boundary(80);
        let truncated = val[..end].to_owned();

        // Must not have panicked, must be valid UTF-8, and must be <= 80 bytes.
        assert!(truncated.len() <= 80, "truncated value exceeds 80 bytes");
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok(), "truncated value is not valid UTF-8");
        // The '€' must have been excluded (byte 78 is where it starts, < 80).
        assert!(!truncated.contains('€'), "multibyte char crossing boundary must be excluded");
    }

    #[test]
    fn inspect_nonexistent_pid_does_not_panic() {
        // Use a very large unlikely PID.
        let info = inspect_pid(u32::MAX);
        assert_eq!(info.pid, u32::MAX);
        // All fields empty on Linux (proc read will fail).
        // On non-Linux comm == "unsupported".
        let _ = info.comm;
    }
}
