"""Harbor's built-in pi agent, with the opencode-go key passed through.

Harbor 0.24.0 lists `opencode` but not `opencode-go` in its provider table, so
`-m opencode-go/<model>` resolves no env and pi starts without a key. This
subclass adds `OPENCODE_API_KEY` from the `harbor run` environment; nothing else
about the stock agent changes.

`capture_port`/`capture_ca` (`ab/capture.py`) add the request capture; without
them the arm runs as it always did.

The hosts keep jobs under $HOME/tb2/jobs/ and binaries under $HOME/tb2/bin/.
"""

import os
from dataclasses import replace

from harbor.agents.installed.pi import Pi, PiOptions
from harbor.agents.model_connection import ResolvedModelConnection
from pydantic import Field

from capture import CaptureMixin


class PiGoOptions(PiOptions):
    """The stock pi options plus the two adapter-side capture fields."""

    capture_port: int | None = Field(
        default=None,
        description="Port of this arm's mitmdump; enables the request capture.",
    )
    capture_ca: str | None = Field(
        default=None,
        description="Host path of the arm's mitmproxy CA, uploaded to the "
        "container when `capture_port` is set.",
    )


class PiOpencodeGo(CaptureMixin, Pi):
    options_model = PiGoOptions
    options: PiGoOptions

    @property
    def model_connection(self) -> ResolvedModelConnection:
        access = super().model_connection
        key = os.environ.get("OPENCODE_API_KEY")
        if not key:
            raise ValueError("OPENCODE_API_KEY is not set in the harbor run environment")
        return replace(access, env={**access.env, "OPENCODE_API_KEY": key})
