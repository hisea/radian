"""Harbor's built-in pi agent, with the opencode-go key passed through.

Harbor 0.24.0 lists `opencode` but not `opencode-go` in its provider table, so
`-m opencode-go/<model>` resolves no env and pi starts without a key. This
subclass adds `OPENCODE_API_KEY` from the `harbor run` environment; nothing else
about the stock agent changes.

The hosts keep jobs under $HOME/tb2/jobs/ and binaries under $HOME/tb2/bin/.
"""

import os
from dataclasses import replace

from harbor.agents.installed.pi import Pi
from harbor.agents.model_connection import ResolvedModelConnection


class PiOpencodeGo(Pi):
    @property
    def model_connection(self) -> ResolvedModelConnection:
        access = super().model_connection
        key = os.environ.get("OPENCODE_API_KEY")
        if not key:
            raise ValueError("OPENCODE_API_KEY is not set in the harbor run environment")
        return replace(access, env={**access.env, "OPENCODE_API_KEY": key})
