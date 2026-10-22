//! Process identity and signals (ADR 0048 decisions 4 and 9).
//!
//! A pid alone is not an identity: the operating system reuses one. An attempt record keeps
//! the process's **start time** as well (`ps -o lstart=`), and every signal arc sends checks
//! both, so a reused pid is never signalled.
//!
//! The start time is the `ps` spelling, recorded verbatim: it is compared with the same
//! command's output later, never parsed into a date.

use std::process::Command;

/// The process's start time, as `ps` prints it. `None` when the process is gone or `ps`
/// cannot report it.
pub fn start_time(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        // `lstart` is formatted in the caller's locale. The record and every later check
        // must use one spelling, whatever environment the wrapper or the owner's shell has.
        .env("LC_ALL", "C")
        .env("LC_TIME", "C")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Whether `pid` is alive **and** is the process that started at `started_at`. A live pid
/// with another start time is a different process that reused the number.
pub fn same_process(pid: u32, started_at: &str) -> bool {
    start_time(pid).is_some_and(|start| start == started_at.trim())
}

/// Sends `signal` to `pid`. `false` when the process could not be signalled (already gone,
/// or not ours).
pub fn signal(pid: u32, signal: i32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: `kill(2)` with a positive pid sends to one process; the arguments are plain
    // integers and the call has no memory contract.
    unsafe { libc::kill(pid as libc::pid_t, signal) == 0 }
}

/// Sends `signal` to the process group whose id is `pgid`. `false` when the signal could
/// not be sent. The caller must know that `pgid` is still a live group: a group id is the
/// leader's pid, and the operating system can reuse it once the group is empty.
pub fn signal_group(pgid: u32, signal: i32) -> bool {
    if pgid == 0 {
        return false;
    }
    // SAFETY: `killpg(2)` takes plain integers and has no memory-safety preconditions. A
    // negative pid would name a group; the caller passes the leader's own pid.
    unsafe { libc::killpg(pgid as libc::pid_t, signal) == 0 }
}

/// The signal numbers the attempt lifecycle uses.
pub const SIGTERM: i32 = libc::SIGTERM;
pub const SIGKILL: i32 = libc::SIGKILL;

#[cfg(test)]
mod tests {
    use super::*;

    /// A reused pid is not the process the attempt recorded: the start time is compared too.
    /// Mutation: drop the start-time comparison (return `start_time(pid).is_some()`) and this
    /// fails.
    #[test]
    fn a_reused_pid_with_another_start_time_is_not_the_same_process() {
        let me = std::process::id();
        let mine = start_time(me).expect("the test process has a start time");
        assert!(same_process(me, &mine));
        assert!(
            !same_process(me, "Thu Jan  1 00:00:00 1970"),
            "a live pid with another start time is another process"
        );
        assert!(!same_process(0, &mine));
    }

    #[test]
    fn signalling_a_pid_that_is_gone_reports_false() {
        // A freshly used pid is very unlikely to be alive again; `signal` must not panic.
        let _ = signal(0, SIGTERM);
        assert!(!signal(0, SIGTERM));
        assert!(!signal_group(0, SIGTERM));
    }
}
