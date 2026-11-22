//! Killing the process group of a hook.

/// Kills a hook child's whole session group when dropped. `spawn_piped` puts the child in its
/// own group (`setsid`), and the future that waits on it can be dropped — a cancelled turn
/// drops it at an `await` — so a kill that only runs on the timeout path would leak a hook
/// process nobody waits for. `kill_tree` is best effort; the spawn-blocking waiter still
/// reaps the child.
pub(super) struct KillGroupOnDrop(Option<u32>);

impl KillGroupOnDrop {
    pub(super) fn new(pid: u32) -> Self {
        Self(Some(pid))
    }

    /// The run ended normally (or the caller kills it itself): never touch the group again.
    pub(super) fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            kill_tree(pid);
        }
    }
}

/// Kills a timed-out hook's whole session group (`setsid` in `spawn_piped` put it in one).
/// Best effort, like the shell tool's own kill paths: the waiter still reaps the child.
pub(super) fn kill_tree(pid: u32) {
    kill_group(pid);
}

/// Sends `SIGKILL` to the process group `pid` leads, and reports whether the signal was
/// delivered. `false` means the group is already gone (`ESRCH`): its leader was reaped, so
/// nothing is running any more. On a host without process groups the signal is a no-op and a
/// caller must still treat the run as cancelled, so this reports `true`.
pub(super) fn kill_group(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // Negative pid = the group. The leader may already have been reaped (a hook that
        // forked and exited), so an error here is not a failure.
        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}
