#!/usr/bin/env python3
"""Run one command under a wall-time cap on every platform.

    timeout_run.py [--kill-after SECONDS] SECONDS -- COMMAND [ARG]...

GNU coreutils' `timeout` is not on a normal macOS installation, so the shell
scripts used to call the agent without a cap when neither `gtimeout` nor
`timeout` was installed. This program is the cap everywhere: the command runs in
its own process group, receives SIGTERM (to the whole group) when SECONDS
elapse, and SIGKILL after `--kill-after` more seconds (default 30). Any run that
reached the cap exits 124, like GNU `timeout`, so `run.sh` and `review.sh`
record it as `timeout`; otherwise the command's own exit code passes through
(128 + the signal when a signal ended it).

Stdlib only.
"""

from __future__ import annotations

import os
import signal
import subprocess
import sys

DEFAULT_KILL_AFTER = 30.0


def _usage(message: str) -> int:
    print(f"timeout_run.py: {message}", file=sys.stderr)
    print(__doc__.strip(), file=sys.stderr)
    return 2


def main(argv: list[str]) -> int:
    kill_after = DEFAULT_KILL_AFTER
    if argv[:1] == ["--kill-after"]:
        if len(argv) < 2:
            return _usage("--kill-after needs a number")
        try:
            kill_after = float(argv[1])
        except ValueError:
            return _usage(f"--kill-after is not a number: {argv[1]}")
        argv = argv[2:]
    if len(argv) < 3 or argv[1] != "--":
        return _usage("usage: timeout_run.py [--kill-after SECONDS] SECONDS -- COMMAND...")
    try:
        seconds = float(argv[0])
    except ValueError:
        return _usage(f"the cap is not a number: {argv[0]}")
    command = argv[2:]
    try:
        process = subprocess.Popen(command, start_new_session=True)
    except OSError as error:
        print(f"timeout_run.py: {error}", file=sys.stderr)
        return 127
    try:
        returncode = process.wait(timeout=max(0.0, seconds))
    except subprocess.TimeoutExpired:
        # The session leader's pid is the group id `start_new_session` made.
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=max(0.0, kill_after))
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
        return 124
    return 128 - returncode if returncode < 0 else returncode


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
