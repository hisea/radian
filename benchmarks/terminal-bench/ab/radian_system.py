"""RadianAgent with the `<role>` layer replaced by `--system`, for system-prompt A/B runs.

The text comes from the file named by RADIAN_AB_SYSTEM_FILE in the `harbor run` environment;
without it the agent is plain RadianAgent. `radian exec --system` replaces only `<role>`
(ADR 0036): runtime, skills and session layers stay as they are.

The hosts keep jobs under $HOME/tb2/jobs/ and binaries under $HOME/tb2/bin/.
"""

import os
import shlex
from dataclasses import replace
from pathlib import Path

import radian_agent
import radian_command
from radian_agent import RadianAgent

_PATH = os.environ.get("RADIAN_AB_SYSTEM_FILE")
_TEXT = Path(_PATH).read_text(encoding="utf-8") if _PATH else None
# radian_command owns the builder; radian_agent imported it by name. Save the
# source so a module reload never wraps a previous wrapper.
_build = radian_command.build_command


def build_command(binary, invocation, *rest):
    if _TEXT is not None:
        invocation = replace(
            invocation, extra_args=tuple(invocation.extra_args) + ("--system", shlex.quote(_TEXT))
        )
    return _build(binary, invocation, *rest)


# run() looks the builder up in the radian_agent module namespace.
radian_agent.build_command = build_command


class RadianSystem(RadianAgent):
    pass
