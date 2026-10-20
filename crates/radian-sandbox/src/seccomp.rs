//! The Linux `workspace`-tier seccomp filter: no Unix sockets, no `io_uring_setup`.
//!
//! `bwrap --seccomp FD` reads a compiled classic-BPF program (an array of `struct
//! sock_filter`) from that fd and installs it with `PR_SET_NO_NEW_PRIVS` before exec'ing
//! the command. The filter is written by hand against `libc`'s constants:
//!
//! 1. `seccomp_data.arch` must be the native arch (`AUDIT_ARCH_X86_64` on x86_64,
//!    `AUDIT_ARCH_AARCH64` on aarch64, picked at compile time): on x86_64 a 32-bit process
//!    could otherwise reach `socketcall` through `int 0x80`. Cost: 32-bit binaries cannot
//!    run in the sandbox (see `docs/sandbox.md`).
//! 2. On x86_64, `seccomp_data.nr & 0x40000000` (the x32 ABI bit) is denied: an x32 process
//!    has its own syscall numbers under that bit.
//! 3. `nr == SYS_socket` with `args[0] == AF_UNIX` (low 32 bits) is denied with `EPERM`.
//!    Pathname and abstract sockets are both covered: both need `socket` first, and seccomp
//!    cannot see paths, so there is no per-path whitelist on Linux.
//! 4. `nr == SYS_io_uring_setup` is denied with `EPERM` (`IORING_OP_SOCKET` would create a
//!    socket without the `socket` syscall).
//! 5. Everything else is allowed. `socketpair` stays allowed: an unnamed connected pair
//!    cannot reach anything outside.
//!
//! The filter is installed only in the `workspace` tier (`SandboxMode::WorkspaceWrite`);
//! `full` runs no sandbox and gets no `--seccomp`.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::error::SandboxError;

/// `seccomp_data` field offsets (`linux/seccomp.h`, arch-independent): `nr` is a 32-bit
/// int at 0, `arch` a 32-bit int at 4, then the 64-bit instruction pointer, then `args[6]`.
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARG0: u32 = 16;

/// `AUDIT_ARCH_*` is not in the `libc` version in `Cargo.lock`, so it is defined here from
/// `linux/audit.h`: `AUDIT_ARCH_X86_64 = EM_X86_64 (62) | __AUDIT_ARCH_64BIT (0x80000000) |
/// __AUDIT_ARCH_LE (0x40000000) = 0xC000003E`; `AUDIT_ARCH_AARCH64 = EM_AARCH64 (183) |
/// __AUDIT_ARCH_64BIT | __AUDIT_ARCH_LE = 0xC00000B7`. Checked against
/// `/usr/include/linux/audit.h` on x86_64.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_NATIVE: u32 = 0xC000003E;
/// See above (`linux/audit.h`).
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH_NATIVE: u32 = 0xC00000B7;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("the workspace seccomp filter only supports x86_64 and aarch64");

/// The x32 ABI bit on x86_64: syscall numbers with it set are a different table.
#[cfg(target_arch = "x86_64")]
const X32_BIT: u32 = 0x4000_0000;

/// Builds the filter program. Jump targets are absolute positions in the returned vector;
/// the unit tests emulate the program, so a miscounted jump fails there, not in production.
pub(crate) fn program() -> Vec<libc::sock_filter> {
    const LD_W_ABS: u32 = libc::BPF_LD | libc::BPF_W | libc::BPF_ABS;
    const JEQ_K: u32 = libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K;
    const AND_K: u32 = libc::BPF_ALU | libc::BPF_AND | libc::BPF_K;
    const RET_K: u32 = libc::BPF_RET | libc::BPF_K;
    const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
    const DENY: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;

    fn stmt(code: u32, k: u32) -> libc::sock_filter {
        libc::sock_filter {
            code: code as u16,
            jt: 0,
            jf: 0,
            k,
        }
    }
    fn jump(code: u32, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
        libc::sock_filter {
            code: code as u16,
            jt,
            jf,
            k,
        }
    }

    let socket_nr = libc::SYS_socket as u32;
    let uring_nr = libc::SYS_io_uring_setup as u32;
    let unix = libc::AF_UNIX as u32;

    // The arch prefix is shared; x86_64 inserts the x32-bit check after it.
    let mut prog = vec![
        stmt(LD_W_ABS, OFF_ARCH), // 0: A = arch
        jump(JEQ_K, AUDIT_ARCH_NATIVE, 0, 0), // 1: patched below
    ];
    #[cfg(target_arch = "x86_64")]
    prog.extend([
        stmt(LD_W_ABS, OFF_NR), // 2: A = nr
        stmt(AND_K, X32_BIT),   // 3: A &= x32-bit
        jump(JEQ_K, 0, 0, 0),   // 4: patched below
    ]);
    let tail = vec![
        stmt(LD_W_ABS, OFF_NR),       // A = nr
        jump(JEQ_K, socket_nr, 2, 0), // socket -> check AF_UNIX
        jump(JEQ_K, uring_nr, 0, 0),  // patched below; else ALLOW
        stmt(RET_K, ALLOW),           // ALLOW
        stmt(LD_W_ABS, OFF_ARG0),    // A = args[0] (low 32 bits)
        jump(JEQ_K, unix, 1, 0),      // AF_UNIX -> DENY, else ALLOW
        stmt(RET_K, ALLOW),           // ALLOW
        stmt(RET_K, DENY),            // DENY
    ];
    // Patch the forward jumps: deny lands on the final DENY, allow falls through.
    let deny: u8 = (prog.len() + tail.len() - 1) as u8;
    let pc_arch_check = 1;
    prog[pc_arch_check].jf = deny - pc_arch_check as u8 - 1;
    #[cfg(target_arch = "x86_64")]
    {
        let pc_x32_check = 4;
        prog[pc_x32_check].jf = deny - pc_x32_check as u8 - 1;
    }
    let pc_uring_check = (prog.len() + 2) as u8;
    let mut tail = tail;
    tail[2].jt = deny - pc_uring_check - 1;
    prog.extend(tail);
    prog
}

/// The compiled program as bytes (what `bwrap --seccomp` reads from the fd).
fn program_bytes() -> Vec<u8> {
    let prog = program();
    // SAFETY: `sock_filter` is `#[repr(C)]` (`u16, u8, u8, u32`, 8 bytes, no padding on
    // either target); the byte view is alive as long as `bytes`, which is returned.
    let raw =
        unsafe { std::slice::from_raw_parts(prog.as_ptr().cast::<u8>(), prog.len() * 8) };
    let bytes = raw.to_vec();
    std::mem::forget(prog);
    bytes
}

/// A fresh read end holding one copy of the compiled program, for one sandboxed spawn.
///
/// Guarantee: every `workspace` command owns a freshly written pipe, so concurrent builds
/// can never share an offset — a shared pipe would hand the second `bwrap` an EOF and fail
/// it with `EINVAL` (measured). The write end is closed before returning; the read end is
/// an `OwnedFd` with `FD_CLOEXEC` cleared, so the `bwrap` child inherits it under the
/// `--seccomp` number. Closing is tied to the owning [`crate::SandboxCommand`]: dropping
/// the command (after spawn, or unspawned) closes the fd, so building commands in a loop
/// cannot leak fds (a test in `tests/seccomp.rs` pins the fd count). No `pre_exec` hook is
/// used: `radian-coding`'s shell registers its own `pre_exec` for `setsid` afterwards,
/// which would replace one set here (measured: the second hook wins). `bwrap` reads the
/// program before exec'ing the command and installs it with `PR_SET_NO_NEW_PRIVS`, so the
/// filter confines only the sandboxed child — never `bwrap`'s own setup, and never this
/// process (which never installs it).
///
/// The program is identical for every `workspace` command (the native arch is fixed at
/// compile time), so rewriting it per command is pure overhead (~4 syscalls) for a
/// race-free guarantee.
pub(crate) fn pipe_for_program() -> Result<(OwnedFd, u32), SandboxError> {
    let unavailable = |detail: String| SandboxError::BackendUnavailable {
        backend: "linux-bwrap",
        detail: format!("could not stage the seccomp filter: {detail}"),
    };
    let bytes = program_bytes();
    let mut fds = [0 as std::os::fd::RawFd; 2];
    // SAFETY: `pipe` writes two fresh fds on success; both are owned here and closed below
    // on every path.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(unavailable(io_error()));
    }
    let (read, write) = (fds[0], fds[1]);
    // SAFETY: `read` came from `pipe` and is still open; from here the `OwnedFd` owns it.
    let read = unsafe { OwnedFd::from_raw_fd(read) };
    // The program is far below the pipe buffer (13 instructions x 8 bytes on x86_64), so a
    // short blocking write loop suffices and the write end is closed before returning.
    let mut off = 0;
    while off < bytes.len() {
        // SAFETY: `bytes` outlives the loop; a partial write or `EINTR` just retries.
        let n = unsafe {
            libc::write(
                write,
                bytes[off..].as_ptr().cast::<libc::c_void>(),
                bytes.len() - off,
            )
        };
        if n < 0 {
            let detail = io_error();
            // SAFETY: `write` came from `pipe` and is still open on this path.
            unsafe {
                libc::close(write);
            }
            return Err(unavailable(detail));
        }
        off += n as usize;
    }
    // SAFETY: `write` came from `pipe` and is still open on this path; `bwrap` must see EOF.
    unsafe {
        libc::close(write);
    }
    // `pipe` fds have no `FD_CLOEXEC`, but clear it explicitly: `std::process::Command`
    // closes fds marked `CLOEXEC` in the child, and the whole point is that `bwrap`
    // inherits this one.
    // SAFETY: the fd is owned by `read` and still open.
    if unsafe { libc::fcntl(read.as_raw_fd(), libc::F_SETFD, 0) } != 0 {
        return Err(unavailable(io_error()));
    }
    let number = read.as_raw_fd() as u32;
    Ok((read, number))
}

fn io_error() -> String {
    std::io::Error::last_os_error().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal classic-BPF interpreter over a fake `seccomp_data`: enough to pin what the
    /// filter allows and denies. It implements exactly the instructions the filter emits
    /// (`LD W ABS`, `AND K`, `JEQ K`, `RET K`) and panics on anything else, so an edit that
    /// introduces a new instruction fails loudly instead of being silently misread.
    fn emulate(prog: &[libc::sock_filter], arch: u32, nr: u32, arg0: u32) -> u32 {
        // `seccomp_data`: nr@0, arch@4, ip@8 (zeroed), args[0]@16, the rest zeroed.
        let mut data = [0u8; 64];
        data[0..4].copy_from_slice(&nr.to_le_bytes());
        data[4..8].copy_from_slice(&arch.to_le_bytes());
        data[16..20].copy_from_slice(&arg0.to_le_bytes());
        let load = |off: u32| {
            u32::from_le_bytes(data[off as usize..off as usize + 4].try_into().unwrap())
        };
        let mut acc = 0u32;
        let mut pc = 0usize;
        loop {
            assert!(pc < prog.len(), "jumped past the program end");
            let ins = &prog[pc];
            match (ins.code as u32, ins.jt, ins.jf) {
                (code, 0, 0)
                    if code == libc::BPF_LD | libc::BPF_W | libc::BPF_ABS =>
                {
                    acc = load(ins.k);
                    pc += 1;
                }
                (code, _, _)
                    if code == libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K =>
                {
                    pc += if acc == ins.k {
                        ins.jt as usize + 1
                    } else {
                        ins.jf as usize + 1
                    };
                }
                (code, 0, 0)
                    if code == libc::BPF_ALU | libc::BPF_AND | libc::BPF_K =>
                {
                    acc &= ins.k;
                    pc += 1;
                }
                (code, 0, 0) if code == libc::BPF_RET | libc::BPF_K => return ins.k,
                other => panic!("unexpected instruction at {pc}: {other:?}"),
            }
        }
    }

    const DENY: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;

    fn native() -> u32 {
        AUDIT_ARCH_NATIVE
    }

    /// Every jump stays inside the program: a miscounted `jt`/`jf` would send the kernel
    /// past the end (the kernel rejects the load) or at the wrong rule.
    #[test]
    fn all_jumps_land_inside_the_program() {
        let prog = program();
        for (pc, ins) in prog.iter().enumerate() {
            if ins.code as u32 == libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K {
                assert!(
                    pc + ins.jt as usize + 1 < prog.len(),
                    "jt at {pc} leaves the program"
                );
                assert!(
                    pc + ins.jf as usize + 1 < prog.len(),
                    "jf at {pc} leaves the program"
                );
            }
        }
    }

    /// `socket(AF_UNIX, …)` is denied with `EPERM` (pathname and abstract alike: both need
    /// `socket` first). Removing the `AF_UNIX` rule turns this into `ALLOW`.
    #[test]
    fn socket_af_unix_is_denied() {
        let prog = program();
        assert_eq!(
            emulate(&prog, native(), libc::SYS_socket as u32, libc::AF_UNIX as u32),
            DENY
        );
    }

    /// Any other domain still works: the `workspace` tier keeps its network (`AF_INET`
    /// here; `AF_VSOCK`/netlink are equally untouched). Narrowing the match to all of
    /// `socket` fails this.
    #[test]
    fn socket_other_domains_are_allowed() {
        let prog = program();
        for domain in [1 + 1, 10, 16, 31] {
            // AF_INET, AF_INET6, AF_NETLINK, AF_BLUETOOTH: none is AF_UNIX (1).
            assert_eq!(
                emulate(&prog, native(), libc::SYS_socket as u32, domain),
                libc::SECCOMP_RET_ALLOW,
                "domain {domain}"
            );
        }
    }

    /// `io_uring_setup` is denied with `EPERM`: `IORING_OP_SOCKET` would create a socket
    /// without the `socket` syscall. Removing the rule turns this into `ALLOW`.
    #[test]
    fn io_uring_setup_is_denied() {
        let prog = program();
        assert_eq!(
            emulate(&prog, native(), libc::SYS_io_uring_setup as u32, 0),
            DENY
        );
    }

    /// `socketpair` stays allowed: an unnamed connected pair cannot reach anything outside.
    /// Blocking it would break innocent IPC (and the harness's own process plumbing).
    #[test]
    fn socketpair_stays_allowed() {
        let prog = program();
        assert_eq!(
            emulate(&prog, native(), libc::SYS_socketpair as u32, 0),
            libc::SECCOMP_RET_ALLOW
        );
    }

    /// Unrelated syscalls pass through: the filter is a deny-list, not a default-deny.
    #[test]
    fn unrelated_syscalls_are_allowed() {
        let prog = program();
        for nr in [0u32, 1, 59, 257] {
            // read, write, execve, openat.
            assert_eq!(
                emulate(&prog, native(), nr, 0),
                libc::SECCOMP_RET_ALLOW,
                "nr {nr}"
            );
        }
    }

    /// A foreign arch is denied outright: on x86_64 a 32-bit process could otherwise reach
    /// `socketcall` through `int 0x80`. Removing the arch check turns this into `ALLOW`.
    #[test]
    fn a_foreign_arch_is_denied() {
        let prog = program();
        let foreign = if native() == 0xC000003E {
            0x40000003u32 // AUDIT_ARCH_I386
        } else {
            0xC000003Eu32 // AUDIT_ARCH_X86_64, foreign on aarch64
        };
        assert_eq!(
            emulate(&prog, foreign, libc::SYS_socket as u32, 0),
            DENY,
            "a non-native arch must not reach any syscall"
        );
    }

    /// The x32 ABI bit is denied on x86_64: an x32 process has its own syscall numbers under
    /// that bit. (x86_64-only: other arches have no such bit.)
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_x32_bit_is_denied_on_x86_64() {
        let prog = program();
        let x32_socket = 0x4000_0000 | libc::SYS_socket as u32;
        assert_eq!(emulate(&prog, native(), x32_socket, 0), DENY);
    }
}
