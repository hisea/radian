# ADR 0016: Workspace Tier Network and Devices

Status: accepted

## Decisions

### 1. Two tiers

- `workspace`: the workspace is writable. Outbound and localhost loopback TCP and UDP are all allowed. At least pty is available.
- `full`: no sandbox, no built-in approvals raised (`no prompts`; user-written `deny` and `ask` rules plus `approval = always` are unaffected). With no narrower CLI, configuration, project, or user source, it is the TUI and `radian exec` default (ADR 0043). A mid-session switch from another tier to `full` needs explicit human confirmation.

Old names `workspace-write` and `full-access` stay as **aliases** (accepted as usual, unlisted in help). Internal type names need not change.

`workspace` still constrains file paths: writes land inside the workspace only. `.git` write scope follows ADR 0038. Sensitive directories are unreadable by default (ADR 0012 P4, ADR 0020).

### 2. `workspace` devices

At least pty (`openpty`) is available. Signals are already opened per `(target same-sandbox)`. The opened scope is a minimum usable set plus escape tests:

- On macOS, adding `(allow file-write* (literal "/dev/ptmx") (regex #"^/dev/ttys[0-9]+$"))` and `file-ioctl` to the existing profile makes `os.openpty()` succeed. `file-ioctl` alone, `/dev/ptmx` write alone, or `(allow pseudo-tty)` alone is not enough.
- **TIOCSTI injection stays blocked first**: once `ioctl` on terminal devices is allowed, a command inside the sandbox can push keys into terminal input with TIOCSTI. The shell tool calls `setsid` before exec, so the child has no controlling terminal (Linux bwrap `--new-session` behaves the same). `file-ioctl` opens only for pty devices. The code never uses an unfiltered `(allow file-ioctl)`. The escape matrix carries "TIOCSTI against a terminal held outside the sandbox must fail".

### 3. Unix socket allowlist (macOS) and Linux seccomp denial

`(allow network*)` also allows connections to any same-user local Unix socket. The `workspace` tier therefore:

- **Denies all outbound Unix socket connections**, with one fixed exception: `/private/var/run/mDNSResponder` (macOS DNS resolution uses it).
- `[sandbox] allow_unix_sockets = ["<path>", ...]`: only stated absolute paths (`~` expands), no wildcards supported. Each entry is one seatbelt rule: `(allow network-outbound (remote unix-socket (path-literal …)))`. Paths canonicalize first (`/tmp` becomes `/private/tmp`). An entry that fails canonicalization is skipped, with an explanation in the startup notice.
- **It lives only in user-level configuration**. The same key in project-level configuration is a configuration error and takes no effect: a cloned repository cannot open holes for itself.
- The startup notice lists currently allowed sockets (when non-empty).
- `full` is unchanged (no sandbox). The radian process itself (TUI, herdr reporting, MCP processes) runs outside the sandbox.

**Linux (bwrap, slice 0)**: the `workspace` tier denies `socket(AF_UNIX, …)` directly with seccomp (returns `EPERM`). Path and abstract sockets both block (both must create a socket first). seccomp cannot see paths, so `[sandbox] allow_unix_sockets` cannot allow by path on Linux: **on Linux the key is ignored with one warning in startup output** (only narrows, fail closed). The macOS seatbelt path allowlist is unchanged. seccomp also denies `io_uring_setup` (`IORING_OP_SOCKET` can create a socket without `socket`). Everything else passes (`socketpair` stays available: unnamed pairs cannot reach outside). Cost: seccomp first checks that `seccomp_data.arch` is native (x86_64 also bans the x32 ABI bit). 32-bit binaries cannot run inside the sandbox.

### 4. Nested sandboxes: reuse the outer layer when already inside a sandbox

The macOS kernel forbids a deny-default-constrained process from applying another profile: `sandbox-exec` inside a `workspace` sandbox is denied. Therefore:

1. **Detection asks only the kernel**: before it probes backends at startup, radian asks whether it is already constrained (macOS: `sandbox_check(getpid(), NULL, 0) != 0`). **It never accepts** environment variables, configuration keys, or command-line flags as the source of "already inside a sandbox". A missing symbol or a failed call reads as "not constrained". The code follows the normal probe-and-deny path (fail closed).
2. **When already constrained and the tier is `workspace`, the code builds no sandbox layer of its own**: the shell tool and `!`/`!!` run commands directly, and constraint comes from the outer sandbox. Backend name: `inherited`. Probing is skipped (it would necessarily fail). `full` is unchanged (a `full` tier inside an outer sandbox still gets the outer constraint; the startup line says so honestly).
3. **This is not an exception to "unavailable backends must fail explicitly". It is its boundary**: a process cannot constrain itself less than its parent. Reuse of the outer layer is not a downgrade. Behavior outside sandboxes is unchanged by one word: failed probes still deny.
4. **Copy stays honest**: the startup line and `/access` write `workspace · sandbox inherited (an outer sandbox confines this process; radian's own rules are not applied)`. The copy never claims any item from the denylist, `.git` read-only, network state, or the Unix socket allowlist. The outer layer decides them. Host-side hard deny lists still apply. They never relied on the sandbox.
5. **Tier records are unchanged**: the session record tier is still `workspace`. Switch rules are unchanged.
6. **Tests that rely on sandbox behavior skip under `inherited` and count as "not run"**: a skipped test prints one fixed marker `radian-test: skipped inside a sandbox: <test name>` then returns. Cargo still counts it as passed, so verify output and reports must list these markers as "not run", never as passed. Other PTY tests run as usual (they test UI, not sandboxes).
7. **Linux excludes this rule**: Linux keeps "failed probe means deny" and never detects an outer sandbox.

## Consequences

- After `workspace` opens network, content readable inside the sandbox can in theory all leave. Main protection becomes the denylist and the workspace write boundary. The startup notice and the shell tool description must honestly write `network is on`. No `network is off` text remains.
- Local TCP services are reachable. Unix sockets deny by default on macOS (except the allowlist). On Linux the `workspace`-tier seccomp denies all (the allowlist key is ignored with a warning).
- MCP still runs outside the sandbox (ADR 0012 P6). The startup notice still states it plainly.
- A user nested inside an outer sandbox gets only the outer guarantees. When the private `sandbox_check` fails some day, the code returns to the deny path. It never runs bare.

## Verification

The `radian-sandbox` escape matrix asserts per backend tier: `workspace` outbound and local bind succeed, writes outside the workspace fail, `.git` read-only and writable per ADR 0038, the denylist, the macOS `unix_socket` group (deny, allowlist contrast, DNS) and the Linux `unix_socket` group (path and abstract denial, allowlist still denied, DNS), and the `radian_home` group (sessions, memory, credentials, and trust unreadable; config, cache, themes, and skills readable). `confined_by_outer_sandbox()` is false outside sandboxes and true under an outer `sandbox-exec`. The `inherited` startup line outputs no denylist, `.git`, network, or socket sentences. Environment variables, configuration keys, and flags can never trigger `inherited`.
