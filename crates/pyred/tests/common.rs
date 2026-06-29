//! Shared integration-test helpers.
//!
//! Include with:
//!   ```rust
//!   #[path = "common.rs"] mod common;
//!   ```
//!
//! `#[allow(dead_code)]` on the module declaration silences warnings in test
//! binaries that only use a subset of the helpers here.

/// A guard that sends SIGTERM and then reaps a child process on drop.
///
/// Without this guard a test that returns early via `?` (e.g. on a socket
/// timeout) or that is cancelled by `tokio::time::timeout` will drop the
/// `std::process::Child` without killing or waiting for it, turning the
/// spawned daemon into an orphan reparented to PID 1.
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
        // Graceful shutdown request.  Ignoring errors: the process may have
        // already exited and been reaped by the manual teardown at the end of
        // the test.
        let pid = nix::unistd::Pid::from_raw(self.0.id() as i32);
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
        // Reap: prevents a zombie.  A second wait() after the child was
        // already waited returns an Err which we intentionally ignore.
        let _ = self.0.wait();
    }
}
