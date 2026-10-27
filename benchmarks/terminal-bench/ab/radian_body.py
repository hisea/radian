"""RadianAgent with `RADIAN_EXTRA_BODY` set from the file named by `RADIAN_AB_BODY_FILE`.

The file holds one JSON object; its text is passed to `radian exec` in the command's
environment (ADR 0026 D13), so both arms run the same binary and differ in that one input.
`radian_body.py` is a Harbor agent loaded by import path (`-a radian_body:RadianBody`); it is
not run directly and adds no dependency to the Rust workspace.

Validation happens at import, before a trial starts: the file must exist and parse as a JSON
object. Without `RADIAN_AB_BODY_FILE` the agent is plain `RadianAgent` and the command's
environment is unchanged.

The hosts keep jobs under $HOME/tb2/jobs/ and binaries under $HOME/tb2/bin/.
"""

from __future__ import annotations

import json
import os
from pathlib import Path

from radian_agent import RadianAgent

# The variable the radian binary reads at startup. Kept as a name here so the test can assert
# it without repeating the literal.
BODY_ENV = "RADIAN_EXTRA_BODY"
BODY_FILE_ENV = "RADIAN_AB_BODY_FILE"


def load_body(path: str) -> str:
    """The file's JSON object as a compact one-line JSON string.

    Raises `ValueError` with the path named when the file is missing, does not parse, or is
    not a JSON object. The compact form keeps the environment value on one line.
    """
    try:
        text = Path(path).read_text(encoding="utf-8")
    except OSError as error:
        raise ValueError(f"{BODY_FILE_ENV} {path!r} cannot be read: {error}") from error
    try:
        value = json.loads(text)
    except ValueError as error:
        raise ValueError(
            f"{BODY_FILE_ENV} {path!r} is not valid JSON: {error}"
        ) from error
    if not isinstance(value, dict):
        raise ValueError(f"{BODY_FILE_ENV} {path!r} must be a JSON object")
    return json.dumps(value, separators=(",", ":"))


_PATH = os.environ.get(BODY_FILE_ENV)
_BODY = load_body(_PATH) if _PATH else None


class RadianBody(RadianAgent):
    """The stock Radian agent, plus `RADIAN_EXTRA_BODY` when the file names one."""

    def _trial_env(self) -> dict[str, str]:
        env = dict(super()._trial_env())
        if _BODY is not None:
            env[BODY_ENV] = _BODY
        return env
