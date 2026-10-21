# Security Policy

## Reporting a vulnerability

Report a sandbox escape or any other security problem in private through
GitHub private vulnerability reporting: the "Security" tab, then
"Report a vulnerability". Never open a public issue for a suspected
vulnerability, and do not post a reproduction publicly.

## What a report needs

- The radian version or commit.
- The OS and the sandbox backend: Seatbelt (`sandbox-exec`) on macOS,
  bubblewrap (`bwrap`) on Linux.
- The access tier (`workspace` or `full`).
- The steps to reproduce.
- What the escape reached (files written, files read, network, processes).

## Documented limits

`docs/sandbox.md` describes what the sandbox enforces on each backend.
Check it first so that a documented limit is not reported as an escape.
