"""Harbor custom agent that benchmarks Radian on Terminal-Bench tasks.

Harbor loads this file by import path (`-a radian_agent:RadianAgent` with
`PYTHONPATH=benchmarks/terminal-bench`); it is not part of the Rust workspace
and adds no dependency to it.

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
from pathlib import Path, PurePosixPath
from typing import Annotated, Literal, override

from pydantic import Field

from harbor.agents.capabilities import AgentCapabilities
from harbor.agents.installed.base import (
    BaseInstalledAgent,
    NonZeroAgentExitCodeError,
    with_prompt_template,
)
from harbor.agents.model_connection import ModelConnectionSpec
from harbor.agents.options import Cli, InstalledAgentOptions
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from capture import CaptureMixin
from radian_command import (
    CREDENTIAL_ENV_NAME,
    GENERIC_PROVIDER_ID,
    RadianInvocation,
    assert_binary_static,
    build_auth_json,
    build_command,
    derive_turn_timeout_secs,
    parse_result_line,
    redact_base_url,
    select_provider,
    sha256_of_file,
    summarize_session_usage,
)

_DEFAULT_REMOTE_BINARY = "/tmp/harbor-radian-bin/radian"
_DEFAULT_RADIAN_HOME = "/tmp/harbor-radian-home"
_BINARY_ENV_FALLBACK = "RADIAN_BENCH_BINARY"
# The container path whose existence (`test -s`: present and non-empty) proves
# the image ships its own CA bundle. This is the Debian/Ubuntu OpenSSL default
# (`python:slim`, `ubuntu:24.04`, `debian:bullseye-slim` all use it); images
# without `ca-certificates` have neither the file nor the directory. `-s`
# rather than `-e` because an empty placeholder would break radian's HTTP
# client exactly like a missing file.
SYSTEM_CA_BUNDLE = "/etc/ssl/certs/ca-certificates.crt"
_DEFAULT_CA_BUNDLE_PATH = "/etc/ssl/certs/ca-certificates.crt"
_SSL_CERT_FILE = "SSL_CERT_FILE"


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
    ca_bundle_path: str = Field(
        default=_DEFAULT_CA_BUNDLE_PATH,
        description="Host CA bundle uploaded next to the binary for containers "
        "without a system bundle.",
    )
    remote_ca_bundle: str | None = Field(
        default=None,
        description="Container path the CA bundle is uploaded to. Defaults to "
        "the remote binary's directory.",
    )
    profile: Literal["full", "workspace"] = Field(
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
    timeout_sec: int | None = Field(
        default=None,
        ge=1,
        description="Container exec timeout for the whole trial in seconds. "
        "Omit to let the task's own agent timeout govern: the framework then "
        "ends the trial gracefully and still runs the verifier.",
    )
    # Capture is adapter-side only (`ab/capture.py`): neither field reaches the
    # radian command line. Without `capture_port` the trial runs unchanged.
    capture_port: int | None = Field(
        default=None,
        ge=1,
        description="Port of this arm's mitmdump; enables the request capture.",
    )
    capture_ca: str | None = Field(
        default=None,
        description="Host path of the arm's mitmproxy CA, uploaded to the "
        "container when `capture_port` is set.",
    )


class RadianAgent(CaptureMixin, BaseInstalledAgent):
    """Runs the radian binary under test against one task instruction."""

    capabilities = AgentCapabilities()
    # The only credential source: Harbor resolves the trial key from this env
    # var (trial `--ae` first, then the `harbor run` process environment), for
    # any provider slug — including `opencode-go`, which Harbor's own
    # `PROVIDERS` table does not list. The base URL stays Harbor-resolved: an
    # explicitly configured endpoint becomes the generic provider's
    # `--base-url`, a built-in descriptor id needs none.
    MODEL_CONNECTION = ModelConnectionSpec(api_key_envs=(CREDENTIAL_ENV_NAME,))
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

    def _remote_ca_bundle_path(self) -> str:
        """Where the CA bundle lives in the container."""
        if self.options.remote_ca_bundle is not None:
            return self.options.remote_ca_bundle
        parent = PurePosixPath(self.options.remote_binary).parent
        return (parent / Path(SYSTEM_CA_BUNDLE).name).as_posix()

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
        assert_binary_static(binary)
        digest = sha256_of_file(binary)
        if self.options.binary_sha256 is not None and digest != self.options.binary_sha256:
            raise ValueError(
                f"Radian binary sha256 mismatch: expected {self.options.binary_sha256}, "
                f"got {digest}."
            )
        ca_bundle = self.options.ca_bundle_path
        if not os.path.isfile(ca_bundle):
            raise ValueError(
                f"CA bundle not found: {ca_bundle}; pass `ca_bundle_path` to a "
                f"host PEM bundle (default {_DEFAULT_CA_BUNDLE_PATH})."
            )
        ca_digest = sha256_of_file(ca_bundle)
        remote = self.options.remote_binary
        parent = str(PurePosixPath(remote).parent)
        await self.exec_as_agent(
            environment,
            command=f"mkdir -p {shlex.quote(parent)}",
        )
        await environment.upload_file(binary, remote)
        remote_ca = self._remote_ca_bundle_path()
        await environment.upload_file(ca_bundle, remote_ca)
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
                    "ca_bundle_path": ca_bundle,
                    "ca_bundle_sha256": ca_digest,
                    "ca_bundle_remote": remote_ca,
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
        model_id = self._parsed_model_name or self.model_name
        access = self.model_connection
        if not access.api_key:
            raise ValueError(
                "No model API key selected: set RADIAN_BENCH_API_KEY in the "
                "`harbor run` environment for this trial."
            )
        provider, base_url = select_provider(
            provider=access.provider or self._parsed_model_provider,
            configured_base_url=access.configured_base_url,
            base_url=access.base_url,
        )
        workdir_result = await self.exec_as_agent(environment, command="pwd")
        workdir = (workdir_result.stdout or "").strip() or "/"
        home = self.options.radian_home
        auth_json = build_auth_json(provider, base_url)
        credentials_dir = f"{home}/credentials"
        remote_auth = f"{credentials_dir}/auth.json"
        await self.exec_as_agent(
            environment,
            command=(
                f"mkdir -p {shlex.quote(credentials_dir)} && "
                f"chmod 700 {shlex.quote(home)} {shlex.quote(credentials_dir)}"
            ),
        )
        # The chown path keeps auth.json readable when the image's agent user
        # is not root. The file still holds only the `$RADIAN_BENCH_API_KEY`
        # reference, never the value. The chmod keeps today's 0600 posture
        # when there is no non-root default user to chown to.
        await self._upload_config_text(
            environment,
            content=auth_json,
            remote_path=remote_auth,
            filename="auth.json",
        )
        await self.exec_as_agent(
            environment,
            command=f"chmod 600 {shlex.quote(remote_auth)}",
        )
        probe = await environment.exec(
            command=f"test -s {shlex.quote(SYSTEM_CA_BUNDLE)}",
            timeout_sec=30,
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
            turn_timeout_secs=derive_turn_timeout_secs(
                self.options.turn_timeout_secs, self.options.timeout_sec
            ),
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
        if probe.return_code != 0:
            env[_SSL_CERT_FILE] = self._remote_ca_bundle_path()
        # `ab/capture.py` adds the proxy and its CA bundle here; empty otherwise.
        env.update(self._trial_env())
        result = await self.exec_as_agent(
            environment,
            command=command,
            env=env,
            cwd=workdir,
            timeout_sec=self.options.timeout_sec,
        )
        # Trial facts for populate_context_post_run: Harbor only calls it
        # while the context is still empty, so run() leaves the context alone
        # and the metadata lands there, together with the usage totals. The
        # exit code and the stop reason come from the closing `Result` record
        # (ADR 0044); without one (a killed run, an old binary) they fall back
        # to the process exit code with no stop reason.
        result_record = parse_result_line(result.stdout) or {}
        self._last_trial = {
            "provider": provider,
            "model": model_id,
            "return_code": result.return_code,
            "exit_code": result_record.get("exit_code", result.return_code),
        }
        stop_reason = result_record.get("stop_reason")
        if stop_reason is not None:
            self._last_trial["stop_reason"] = stop_reason
        if result.return_code != 0:
            shown_url = redact_base_url(base_url)
            raise NonZeroAgentExitCodeError(
                f"radian exec failed (exit {result.return_code}): "
                f"provider={provider} model={model_id} base_url={shown_url}\n"
                f"stdout tail: {self._tail(result.stdout)}\n"
                f"stderr tail: {self._tail(result.stderr)}"
            )

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        """Backfill Harbor's token/cost totals from radian's session journals.

        Harbor calls this after the agent logs (including `radian-sessions/`)
        are synced back to `self.logs_dir`, and only while the context is
        still empty — which is why `run()` sets neither metadata nor usage.
        Each model call's `ModelUsageRecorded` event and each summarization
        call's `CompactionUsageRecorded` event contribute their reported
        counters; a fee's `micros` become USD. Counters no record reported
        stay `None` ("not reported", never 0); unparseable lines are skipped.
        """
        last = getattr(self, "_last_trial", None) or {}
        metadata = dict(context.metadata or {})
        metadata.update(
            {
                "agent": "radian",
                "radian_version": self.version(),
                "radian_profile": self.options.profile,
            }
        )
        if last:
            metadata.update(
                {
                    "radian_provider": last.get("provider"),
                    "radian_model": last.get("model"),
                    "return_code": last.get("return_code"),
                    "exit_code": last.get("exit_code", last.get("return_code")),
                }
            )
            if last.get("stop_reason") is not None:
                metadata["stop_reason"] = last["stop_reason"]
        context.metadata = metadata
        usage = summarize_session_usage(self.logs_dir / self._SESSIONS_DIRNAME)
        context.n_input_tokens = usage.n_input_tokens
        context.n_cache_tokens = usage.n_cache_tokens
        context.n_output_tokens = usage.n_output_tokens
        context.cost_usd = usage.cost_usd

    @staticmethod
    def _tail(output: str | None, limit: int = 2000) -> str:
        if not output:
            return "None"
        if len(output) <= limit:
            return output
        return f"... [{len(output) - limit} chars truncated] ... {output[-limit:]}"
