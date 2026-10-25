//! Process identity and signals (ADR 0048 decisions 4 and 9).
//!
//! A pid alone is not an identity: the operating system reuses one. An attempt record keeps
//! the process's **start time** as well (`ps -o lstart=`), and a signal arc sends compares
//! both, so a reused pid is not signalled. That check needs `ps` to answer: where it cannot
//! report a start time (the `workspace` sandbox refuses it), the pid alone decides, as ADR
//! 0048 decision 9 requires, so a live process at the recorded pid is signalled rather than
//! assumed gone.
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

/// Whether the process a record names is still the one to signal. A recorded start time is
/// compared with `ps`, so a reused pid is never signalled. When `ps` cannot report the time
/// at all (the `workspace` sandbox refuses it), the pid alone decides, as ADR 0048 decision 9
/// says: a live pid without a comparable start time is signalled rather than assumed gone.
#[must_use]
pub fn still_the_process(pid: u32, started_at: Option<&str>) -> bool {
    match started_at.filter(|start| !start.is_empty()) {
        Some(start) => match start_time(pid) {
            Some(actual) => actual == start.trim(),
            None => alive(pid),
        },
        None => alive(pid),
    }
}

/// Whether `pid` exists, whatever it is running. This is `kill(pid, 0)`: a signal probe,
/// not a `ps` call, so it works where the `workspace` sandbox refuses `ps`.
pub fn alive(pid: u32) -> bool {
    pid != 0 && signal(pid, 0)
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
    /// Mutation: drop the start-time comparison (`start_time(pid).is_some()`) and this fails
    /// where `ps` can answer.
    #[test]
    fn a_reused_pid_with_another_start_time_is_not_the_same_process() {
        let me = std::process::id();
        // Without a recorded start time, the pid alone is the identity.
        assert!(still_the_process(me, None));
        assert!(!still_the_process(0, None));
        // `ps` is read only where it can answer. A host that refuses it (the `workspace`
        // sandbox) falls back to the pid alone above; the comparison is checked where `ps`
        // reports, and `attempt::a_reused_pid_with_another_start_time_is_not_signalled`
        // checks it with a `ps` the test controls.
        if let Some(mine) = start_time(me) {
            assert!(still_the_process(me, Some(&mine)));
            assert!(
                !still_the_process(me, Some("Thu Jan  1 00:00:00 1970")),
                "a live pid with another start time is another process"
            );
        }
        // `alive` is the probe used where `ps` is refused, and needs no start time.
        assert!(alive(me));
        assert!(!alive(0));
    }

    #[test]
    fn signalling_a_pid_that_is_gone_reports_false() {
        // A freshly used pid is very unlikely to be alive again; `signal` must not panic.
        let _ = signal(0, SIGTERM);
        assert!(!signal(0, SIGTERM));
        assert!(!signal_group(0, SIGTERM));
    }
}
