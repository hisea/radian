"""Harbor's built-in opencode agent, with the opencode-go key passed through.

Harbor 0.24.0 lists `opencode` but not `opencode-go` in its provider table, so
`-m opencode-go/<model>` resolves no env and opencode starts without a key. This
subclass adds `OPENCODE_API_KEY` from the `harbor run` environment.

It also skips the stock install's `apt-get install nodejs npm` on glibc images:
the stock install then installs Node through nvm anyway, and Ubuntu's npm package
alone outlasted the 360 s setup timeout on fix-git. musl images keep the packaged
Node, exactly as the stock agent does. Nothing about the run itself changes.

`capture_port`/`capture_ca` (`ab/capture.py`) add the request capture; without
them the arm runs as it always did.

The hosts keep jobs under $HOME/tb2/jobs/ and binaries under $HOME/tb2/bin/.
"""

import os
from dataclasses import replace

from harbor.agents.installed.opencode import OpenCode, OpenCodeOptions
from harbor.agents.model_connection import ResolvedModelConnection
from harbor.environments.base import BaseEnvironment
from pydantic import Field

from capture import CaptureMixin

_MUSL_CHECK = "ldd --version 2>&1 | grep -qi musl || [ -f /etc/alpine-release ]"


class OpenCodeGoOptions(OpenCodeOptions):
    """The stock opencode options plus the two adapter-side capture fields."""

    capture_port: int | None = Field(
        default=None,
        description="Port of this arm's mitmdump; enables the request capture.",
    )
    capture_ca: str | None = Field(
        default=None,
        description="Host path of the arm's mitmproxy CA, uploaded to the "
        "container when `capture_port` is set.",
    )


class OpenCodeGo(CaptureMixin, OpenCode):
    options_model = OpenCodeGoOptions
    options: OpenCodeGoOptions

    @property
    def model_connection(self) -> ResolvedModelConnection:
        access = super().model_connection
        key = os.environ.get("OPENCODE_API_KEY")
        if not key:
            raise ValueError("OPENCODE_API_KEY is not set in the harbor run environment")
        return replace(access, env={**access.env, "OPENCODE_API_KEY": key})

    async def ensure_system_dependencies(
        self,
        environment: BaseEnvironment,
        dependencies: tuple[str, ...],
    ) -> None:
        probe = await environment.exec(command=_MUSL_CHECK, timeout_sec=30)
        if probe.return_code != 0:
            dependencies = tuple(d for d in dependencies if d not in ("nodejs", "npm"))
        await super().ensure_system_dependencies(environment, dependencies)
