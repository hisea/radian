"""Harbor custom agent that benchmarks Radian on Terminal-Bench tasks.

Harbor loads this file by import path (`--agent
benchmarks.terminal-bench.radian_agent:RadianAgent`); it is not part of the
Rust workspace and adds no dependency to it.

Design (see `README.md` in this directory):

- Radian runs **inside** the task container as `radian exec -p <instruction>`,
  so the task verifier sees exactly what the agent did.
- The model credential travels only as the `$RADIAN_BENCH_API_KEY` environment
  variable. The command line and the uploaded `auth.json` name that variable;
  neither ever holds the value (ADR 0013).
- The default profile is `full`: the task container is already the isolation
  boundary, so a nested sandbox would only measure whether the host allows
  nested sandboxing. The score measures the agent loop, not our sandbox; the
  sandbox has its own escape-matrix tests in `crates/radian-sandbox`.
"""

from __future__ import annotations

import json
import os
import shlex
import tempfile
from pathlib import Path, PurePosixPath
from typing import Annotated, Literal, override

from pydantic import Field

from harbor.agents.agent_auth_resolution import (
    ModelConnectionSpec,
    ProviderAccess,
    require_model_api_key,
)
from harbor.agents.capabilities import AgentCapabilities
from harbor.agents.installed.base import (
    BaseInstalledAgent,
    NonZeroAgentExitCodeError,
    with_prompt_template,
)
from harbor.agents.options import Cli, InstalledAgentOptions
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.models.model_api_format import ModelApiFormat

from radian_command import (
    CREDENTIAL_ENV_NAME,
    GENERIC_PROVIDER_ID,
    RadianInvocation,
    build_auth_json,
    build_command,
    redact_base_url,
    select_provider,
    sha256_of_file,
)

_DEFAULT_REMOTE_BINARY = "/tmp/harbor-radian-bin/radian"
_DEFAULT_RADIAN_HOME = "/tmp/harbor-radian-home"
_BINARY_ENV_FALLBACK = "RADIAN_BENCH_BINARY"


class RadianOptions(InstalledAgentOptions):
    """Harbor kwargs for the Radian benchmark agent.

    Fields without a `Cli` annotation are adapter-side only (binary delivery,
    timeouts); they never reach the radian command line.
    """

    binary_path: str | None = Field(
        default=None,
        description="Host path to the radian binary under test. "
        f"Falls back to ${{{_BINARY_ENV_FALLBACK}}}.",
    )
    binary_sha256: str | None = Field(
        default=None,
        description="Expected sha256 of the binary; a mismatch refuses the run.",
    )
    remote_binary: str = Field(
        default=_DEFAULT_REMOTE_BINARY,
        description="Container path the binary is uploaded to.",
    )
    radian_home: str = Field(
        default=_DEFAULT_RADIAN_HOME,
        description="Container RADIAN_HOME holding the generated auth.json.",
    )
    profile: Literal["full", "workspace", "safe"] = Field(
        default="full",
        description="Radian permission profile inside the task container.",
    )
    max_model_calls: Annotated[int | None, Cli("--max-model-calls")] = Field(
        default=None, description="Radian per-turn model call limit."
    )
    turn_timeout_secs: Annotated[int | None, Cli("--turn-timeout-secs")] = Field(
        default=None, description="Radian per-turn time limit in seconds."
    )
    context_window: Annotated[int | None, Cli("--context-window")] = Field(
        default=None,
        description="Model context window in tokens; skips the models.dev lookup.",
    )
    offline_catalog: Annotated[bool | None, Cli("--offline-catalog")] = Field(
        default=True,
        description="Never fetch the models.dev catalog during a trial.",
    )
    catalog_provider: Annotated[str | None, Cli("--catalog-provider")] = Field(
        default=None, description="models.dev provider id for the lookup."
    )
    no_trust: Annotated[bool | None, Cli("--no-trust")] = Field(
        default=True,
        description="Do not trust project resources found in the task container.",
    )
    max_tokens: Annotated[int | None, Cli("--max-tokens")] = Field(
        default=None, description="Anthropic `max_tokens`; ignored otherwise."
    )
    thinking: Annotated[str | None, Cli("--thinking")] = Field(
        default=None, description="Anthropic thinking mode; ignored otherwise."
    )
    timeout_sec: int = Field(
        default=1800,
        ge=1,
        description="Container exec timeout for the whole trial in seconds.",
    )


class RadianAgent(BaseInstalledAgent):
    """Runs the radian binary under test against one task instruction."""

    capabilities = AgentCapabilities()
    MODEL_CONNECTION = ModelConnectionSpec(
        agent_provider_access=ProviderAccess(
            api_key_candidate_names=["OPENAI_API_KEY"],
            base_url_candidate_names=["OPENAI_BASE_URL", "OPENAI_API_BASE"],
        ),
        supported_api_formats=[
            ModelApiFormat.OPENAI_CHAT_COMPLETIONS,
            ModelApiFormat.ANTHROPIC_MESSAGES,
        ],
    )
    options_model = RadianOptions
    options: RadianOptions

    _OUTPUT_FILENAME = "radian-stdout.txt"
    _ERROR_FILENAME = "radian-stderr.txt"
    _SESSIONS_DIRNAME = "radian-sessions"

    @staticmethod
    @override
    def name() -> str:
        return "radian"

    @override
    def version(self) -> str | None:
        return self._radian_version if hasattr(self, "_radian_version") else None

    @classmethod
    @override
    def check_model_credentials(cls, connection, *, kwargs, env, explicit_env=None):
        require_model_api_key(connection, cls.name())

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        if self._version not in (None, "latest"):
            raise ValueError(
                "Radian is delivered as a binary under test: pass `binary_path` "
                "(or set $RADIAN_BENCH_BINARY), not `version`."
            )
        binary = self.options.binary_path or os.environ.get(_BINARY_ENV_FALLBACK)
        if not binary:
            raise ValueError(
                "No radian binary: pass `binary_path` or set "
                f"${_BINARY_ENV_FALLBACK} to a Linux binary of this commit."
            )
        if not os.path.isfile(binary):
            raise FileNotFoundError(f"Radian binary not found: {binary}")
        digest = sha256_of_file(binary)
        if self.options.binary_sha256 is not None and digest != self.options.binary_sha256:
            raise ValueError(
                f"Radian binary sha256 mismatch: expected {self.options.binary_sha256}, "
                f"got {digest}."
            )
        remote = self.options.remote_binary
        parent = str(PurePosixPath(remote).parent)
        await self.exec_as_agent(
            environment,
            command=f"mkdir -p {shlex.quote(parent)}",
        )
        await environment.upload_file(binary, remote)
        result = await self.exec_as_agent(
            environment,
            command=f"chmod 755 {shlex.quote(remote)} && {shlex.quote(remote)} exec --version",
        )
        self._radian_version = (result.stdout or "").strip()
        self.logs_dir.mkdir(parents=True, exist_ok=True)
        (self.logs_dir / "radian-binary.json").write_text(
            json.dumps(
                {
                    "binary_path": binary,
                    "sha256": digest,
                    "remote": remote,
                    "version": self._radian_version,
                },
                indent=2,
            )
            + "\n"
        )

    @override
    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        if not self.model_name:
            raise ValueError("Model name must be in the format provider/model_name")
        _, model_id = self._parse_model_name(self.model_name)
        access = self.model_connection
        if not access.api_key:
            raise ValueError(
                "No model API key selected: set the provider key (e.g. "
                "OPENAI_API_KEY) for this trial."
            )
        api_format = access.model_api_format
        format_name = (
            api_format.value
            if hasattr(api_format, "value")
            else (str(api_format) if api_format is not None else None)
        )
        provider, base_url = select_provider(
            model_api_format=format_name,
            user_supplied_base_url=access.user_supplied_base_url,
            resolved_base_url=access.resolved_base_url,
            provider=access.provider,
        )
        workdir_result = await self.exec_as_agent(environment, command="pwd")
        workdir = (workdir_result.stdout or "").strip() or "/"
        home = self.options.radian_home
        auth_json = build_auth_json(provider, base_url)
        with tempfile.TemporaryDirectory(prefix="harbor-radian-auth-") as temp_dir:
            local_path = Path(temp_dir) / "auth.json"
            local_path.write_text(auth_json)
            credentials_dir = f"{home}/credentials"
            await self.exec_as_agent(
                environment,
                command=(
                    f"mkdir -p {shlex.quote(credentials_dir)} && "
                    f"chmod 700 {shlex.quote(home)} {shlex.quote(credentials_dir)}"
                ),
            )
            await environment.upload_file(str(local_path), f"{credentials_dir}/auth.json")
            await self.exec_as_agent(
                environment,
                command=f"chmod 600 {shlex.quote(f'{credentials_dir}/auth.json')}",
            )
        logs = self.environment_logs_dir
        stdout_path = (logs / self._OUTPUT_FILENAME).as_posix()
        stderr_path = (logs / self._ERROR_FILENAME).as_posix()
        session_dir = (logs / self._SESSIONS_DIRNAME).as_posix()
        invocation = RadianInvocation(
            provider=provider,
            model=model_id,
            base_url=base_url if provider == GENERIC_PROVIDER_ID else None,
            workspace=workdir,
            profile=self.options.profile,
            max_model_calls=self.options.max_model_calls,
            turn_timeout_secs=self.options.turn_timeout_secs,
            context_window=self.options.context_window,
            offline_catalog=bool(self.options.offline_catalog),
            catalog_provider=self.options.catalog_provider,
            no_trust=bool(self.options.no_trust),
            max_tokens=self.options.max_tokens,
            thinking=self.options.thinking,
            session_dir=session_dir,
        )
        command = build_command(
            self.options.remote_binary, invocation, instruction, stdout_path, stderr_path
        )
        env = {
            "RADIAN_HOME": home,
            CREDENTIAL_ENV_NAME: access.api_key,
        }
        result = await self.exec_as_agent(
            environment,
            command=command,
            env=env,
            cwd=workdir,
            timeout_sec=self.options.timeout_sec,
        )
        metadata = dict(context.metadata or {})
        metadata.update(
            {
                "agent": "radian",
                "radian_version": self.version(),
                "radian_provider": provider,
                "radian_model": model_id,
                "radian_profile": self.options.profile,
                "return_code": result.return_code,
            }
        )
        context.metadata = metadata
        if result.return_code != 0:
            shown_url = redact_base_url(base_url)
            raise NonZeroAgentExitCodeError(
                f"radian exec failed (exit {result.return_code}): "
                f"provider={provider} model={model_id} base_url={shown_url}\n"
                f"stdout tail: {self._tail(result.stdout)}\n"
                f"stderr tail: {self._tail(result.stderr)}"
            )

    @staticmethod
    def _tail(output: str | None, limit: int = 2000) -> str:
        if not output:
            return "None"
        if len(output) <= limit:
            return output
        return f"... [{len(output) - limit} chars truncated] ... {output[-limit:]}"
