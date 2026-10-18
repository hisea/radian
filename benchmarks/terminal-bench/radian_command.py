"""Pure command builders for running `radian exec` inside a Harbor task container.

This module has no Harbor imports so it can be unit-tested with the standard
library alone. `radian_agent.py` wires these builders into Harbor's
`BaseInstalledAgent`.
"""

from __future__ import annotations

import hashlib
import json
import shlex
from dataclasses import dataclass, field
from urllib.parse import urlsplit, urlunsplit

# The env var the trial container carries the model credential under. It is the
# only place the secret appears: the command line and `auth.json` hold this
# name, never the value (ADR 0013: the key never reaches stdout or a file).
CREDENTIAL_ENV_NAME = "RADIAN_BENCH_API_KEY"
# `auth.json` entry id for the generic OpenAI-compatible descriptor (ADR 0026).
GENERIC_PROVIDER_ID = "openai-compatible"
ANTHROPIC_PROVIDER_ID = "anthropic"
RADIAN_BASE_URL_ENV = "RADIAN_BASE_URL"


@dataclass(frozen=True)
class RadianInvocation:
    """Everything `radian exec` needs for one trial, minus the secret value."""

    provider: str
    model: str
    base_url: str | None = None
    workspace: str = "."
    profile: str = "full"
    max_model_calls: int | None = None
    turn_timeout_secs: int | None = None
    context_window: int | None = None
    offline_catalog: bool = True
    catalog_provider: str | None = None
    no_trust: bool = True
    max_tokens: int | None = None
    thinking: str | None = None
    session_dir: str = "/logs/agent/radian-sessions"
    extra_args: tuple[str, ...] = field(default_factory=tuple)


def select_provider(
    *,
    model_api_format: str | None,
    user_supplied_base_url: str | None,
    resolved_base_url: str | None,
    provider: str | None,
) -> tuple[str, str | None]:
    """Pick the radian `--provider` and `--base-url` for a Harbor connection.

    `model_api_format` is the `ModelApiFormat` member name (`"ANTHROPIC_MESSAGES"`)
    or value (`"anthropic_messages"`), or `None` when Harbor fell back to the
    provider default. Comparison is case-insensitive. Returns
    `(provider, base_url)`.
    """
    normalized = str(model_api_format or "").upper()
    if normalized == "ANTHROPIC_MESSAGES":
        if user_supplied_base_url is not None:
            raise ValueError(
                "radian's `anthropic` provider does not accept `--base-url`; "
                "use an OpenAI-compatible endpoint for proxied Anthropic "
                "traffic."
            )
        return ANTHROPIC_PROVIDER_ID, None
    if user_supplied_base_url is not None:
        return GENERIC_PROVIDER_ID, user_supplied_base_url
    if resolved_base_url is not None:
        return GENERIC_PROVIDER_ID, resolved_base_url
    if (provider or "").lower() == ANTHROPIC_PROVIDER_ID:
        return ANTHROPIC_PROVIDER_ID, None
    return GENERIC_PROVIDER_ID, resolved_base_url


def build_auth_json(provider: str, base_url: str | None) -> str:
    """Render the `auth.json` content naming the credential by reference.

    The key is always the `$RADIAN_BENCH_API_KEY` reference, so the file never
    holds the secret. The generic descriptor additionally carries its endpoint
    under `env.RADIAN_BASE_URL` (ADR 0026 D10).
    """
    if provider == ANTHROPIC_PROVIDER_ID:
        entry: dict[str, object] = {
            "type": "api_key",
            "key": f"${CREDENTIAL_ENV_NAME}",
        }
    else:
        env: dict[str, str] = {}
        if base_url is not None:
            env[RADIAN_BASE_URL_ENV] = base_url
        entry = {
            "type": "api_key",
            "key": f"${CREDENTIAL_ENV_NAME}",
            "env": env,
        }
    return json.dumps({provider: entry}, indent=2) + "\n"


def build_command(
    binary: str,
    invocation: RadianInvocation,
    instruction: str,
    stdout_path: str,
    stderr_path: str,
) -> str:
    """Render the shell command running one trial. Never embeds the secret."""
    parts = [
        "set -o pipefail;",
        shlex.quote(binary),
        "exec",
        "--provider",
        shlex.quote(invocation.provider),
        "--model",
        shlex.quote(invocation.model),
    ]
    if invocation.base_url is not None:
        parts += ["--base-url", shlex.quote(invocation.base_url)]
    if invocation.max_tokens is not None:
        parts += ["--max-tokens", str(invocation.max_tokens)]
    if invocation.thinking is not None:
        parts += ["--thinking", shlex.quote(invocation.thinking)]
    parts += ["--workspace", shlex.quote(invocation.workspace)]
    parts += ["--profile", shlex.quote(invocation.profile)]
    parts += ["--yes"]
    if invocation.no_trust:
        parts += ["--no-trust"]
    if invocation.offline_catalog:
        parts += ["--offline-catalog"]
    if invocation.context_window is not None:
        parts += ["--context-window", str(invocation.context_window)]
    if invocation.catalog_provider is not None:
        parts += ["--catalog-provider", shlex.quote(invocation.catalog_provider)]
    if invocation.max_model_calls is not None:
        parts += ["--max-model-calls", str(invocation.max_model_calls)]
    if invocation.turn_timeout_secs is not None:
        parts += ["--turn-timeout-secs", str(invocation.turn_timeout_secs)]
    parts += ["--session-dir", shlex.quote(invocation.session_dir)]
    parts += list(invocation.extra_args)
    parts += ["-p", shlex.quote(instruction)]
    parts += [">", shlex.quote(stdout_path), "2>", shlex.quote(stderr_path)]
    return " ".join(parts)


def redact_base_url(raw: str | None) -> str | None:
    """Strip userinfo and query/fragment from an endpoint for error output."""
    if not raw:
        return raw
    try:
        split = urlsplit(raw)
    except ValueError:
        return "<unparseable-base-url>"
    if not split.netloc:
        return "<unparseable-base-url>"
    host = split.hostname or ""
    if split.port is not None:
        host = f"{host}:{split.port}"
    return urlunsplit((split.scheme, host, split.path, "", ""))


def sha256_of_file(path: str) -> str:
    """Hex digest of a host file, for pinning the benchmarked binary."""
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()
