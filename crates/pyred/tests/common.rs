//! Shared integration-test helpers.
//!
//! Include with:
//!   ```rust
//!   #[path = "common.rs"] mod common;
//!   ```
//!
//! `#[allow(dead_code)]` on the module declaration silences warnings in test
//! binaries that only use a subset of the helpers here.

/// A guard that sends SIGTERM to the daemon's entire process group and then
/// reaps the direct child on drop.
///
/// ## Why process-group kill?
///
/// Hybrid-mode tests spawn a `pyred --mode supervisor` that in turn spawns
/// `pyred --mode worker` children.  If only the supervisor's direct PID
/// receives SIGTERM, the workers become orphans reparented to PID 1 and are
/// never reaped by the test harness.  Worse, because the workers inherit the
/// test process's stdout/stderr pipes, the write-end stays open until they
/// finally die — which means `cargo test -p pyred 2>&1 | tail` never sees
/// EOF and hangs forever.
///
/// ## Fix — two complementary halves:
///
/// 1. **Detach stdio** (at the spawn site): every daemon is spawned with
///    `.stdin(Stdio::null())`, `.stdout(Stdio::null())`, `.stderr(Stdio::null())`.
///    This ensures no daemon or worker inherits a live pipe end.
///
/// 2. **Process-group kill** (here, in `drop`): every daemon is spawned with
///    `.process_group(0)` so it becomes a new process group leader
///    (PGID == child PID).  `drop` calls `killpg(PGID, SIGTERM)` to terminate
///    the supervisor *and* all worker descendants simultaneously, then waits
///    for the direct child to prevent a zombie.
///
/// Delegate methods `id()`, `try_wait()`, and `kill()` are provided so that
/// callers that previously used a bare `std::process::Child` need minimal
/// changes.
pub struct ChildGuard(pub std::process::Child);

impl ChildGuard {
    /// Return the child's OS PID.
    pub fn id(&self) -> u32 {
        self.0.id()
    }

    /// Poll whether the child has exited without blocking.
    #[allow(dead_code)]
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.0.try_wait()
    }

    /// Send SIGKILL (used as last resort after SIGTERM times out).
    #[allow(dead_code)]
    pub fn kill(&mut self) {
        let _ = self.0.kill();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Kill the entire process group so double-forked/daemonized worker
        // children die with the supervisor, not just the direct child.
        //
        // When the daemon is launched with `.process_group(0)` it becomes a
        // new process group leader: PGID == child PID.  killpg signals every
        // process in that group (supervisor + workers) simultaneously.
        //
        // Errors are intentionally swallowed: the process may have already
        // exited and been reaped by the manual teardown at the end of the test.
        let pgid = nix::unistd::Pid::from_raw(self.0.id() as i32);
        let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGTERM);
        // Reap the direct child to prevent a zombie.  A second wait() after
        // the child was already waited returns an Err which we intentionally
        // ignore.
        let _ = self.0.wait();
    }
}
