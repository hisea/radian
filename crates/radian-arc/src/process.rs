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

/// The child `pid`'s exit status, without reaping it: `waitid` with `WNOWAIT`. A caller that
/// keeps the child unreaped keeps its process-group id reserved, because a zombie is still a
/// member of the group. The caller can then signal `pid`'s process group without a reused
/// group id naming another process, and reap the child after the signal. `None` while the
/// child still runs.
pub fn exited_status(pid: u32) -> std::io::Result<Option<std::process::ExitStatus>> {
    use std::os::unix::process::ExitStatusExt as _;

    if pid == 0 {
        return Ok(None);
    }
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `waitid` writes one `siginfo_t` through the pointer, which is valid for the
    // duration of the call, and the arguments name our own child.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // POSIX leaves the structure untouched when `WNOHANG` finds no state change. It was
    // zeroed, so a zero `si_pid` means the child still runs.
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    let status = unsafe { info.si_status() };
    let raw = match info.si_code {
        libc::CLD_EXITED => (status & 0xff) << 8,
        libc::CLD_KILLED => status & 0x7f,
        libc::CLD_DUMPED => (status & 0x7f) | 0x80,
        _ => 0,
    };
    Ok(Some(std::process::ExitStatus::from_raw(raw)))
}

/// Whether the process group `pgid` has a member that is not a zombie. A group whose only
/// member is an exited-but-unreaped child is worth no signal: `exited_status` keeps that child
/// unreaped, so the caller can skip the group signal and reap it at once. The excluded
/// zombie is what keeps the group id from being reused while this check runs.
#[must_use]
pub fn group_has_live_member(pgid: u32) -> bool {
    if pgid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        linux_group_has_live_member(pgid).unwrap_or_else(|| signal_group(pgid, 0))
    }
    #[cfg(not(target_os = "linux"))]
    {
        // On macOS `killpg` reports an exited leader's group as not signallable (EPERM)
        // once the zombie is its only member, so this is the same check without /proc.
        signal_group(pgid, 0)
    }
}

/// [`group_has_live_member`] on Linux, where `killpg` still reaches a zombie: read every
/// process's group and state from `/proc`. `None` when `/proc` cannot be read, so the caller
/// falls back to the probe.
#[cfg(target_os = "linux")]
fn linux_group_has_live_member(pgid: u32) -> Option<bool> {
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let Some(_pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // `pid (comm) state ppid pgrp ...`; `comm` can hold spaces and parentheses, so the
        // fields start after the last `)`.
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let mut fields = stat[close + 1..].split_whitespace();
        let Some(state) = fields.next() else {
            continue;
        };
        let _ppid = fields.next();
        let Some(group) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        if group == pgid && state != "Z" {
            return Some(true);
        }
    }
    Some(false)
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

    /// An exited child's status is available while the child stays reapable: that is what
    /// keeps its process-group id reserved for a later group signal.
    ///
    /// Mutation: drop `WNOWAIT` from `exited_status`; `waitid` reaps the child and `try_wait`
    /// below has nothing left to report.
    #[test]
    fn an_exited_child_is_seen_before_it_is_reaped() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 7")
            .spawn()
            .expect("the child starts");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            if let Some(status) = exited_status(child.id()).expect("waitid works") {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the child exits and waitid sees it"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(7));
        let reaped = child
            .try_wait()
            .expect("the child is still ours")
            .expect("the unreaped exited child is reapable");
        assert_eq!(reaped.code(), Some(7));
    }

    /// A group whose only member is its exited but unreaped leader has no live member, so a
    /// caller can skip the group signal and reap it; a running member is seen.
    #[cfg(unix)]
    #[test]
    fn a_groups_live_members_exclude_its_exited_leader() {
        use std::os::unix::process::CommandExt as _;

        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .process_group(0)
            .spawn()
            .expect("the child starts");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while exited_status(child.id()).expect("waitid works").is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the child exits and waitid sees it"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !group_has_live_member(child.id()),
            "the zombie leader is not a live member"
        );
        child.try_wait().expect("the child is still ours");

        let mut running = Command::new("sh")
            .arg("-c")
            .arg("sleep 10")
            .process_group(0)
            .spawn()
            .expect("the running child starts");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !group_has_live_member(running.id()) {
            assert!(
                std::time::Instant::now() < deadline,
                "the running child is a live member"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = signal_group(running.id(), SIGKILL);
        let _ = running.wait();
    }
}
