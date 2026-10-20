"""Pure command builders for running `radian exec` inside a Harbor task container.

This module has no Harbor imports so it can be unit-tested with the standard
library alone. `radian_agent.py` wires these builders into Harbor's
`BaseInstalledAgent`.
"""

from __future__ import annotations

import hashlib
import json
import shlex
import struct
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import urlsplit, urlunsplit

# The env var the trial container carries the model credential under. It is the
# only place the secret appears: the command line and `auth.json` hold this
# name, never the value (ADR 0013: the key never reaches stdout or a file).
CREDENTIAL_ENV_NAME = "RADIAN_BENCH_API_KEY"
# The radian built-in provider descriptor ids
# (`crates/radian-host/src/provider.rs`, `DESCRIPTORS`). A model name with one of
# these prefixes runs as `radian exec --provider <id> --model <rest>` with no
# `--base-url`; radian talks to the descriptor's own endpoint. Note `chatgpt` is
# OAuth-only, so an `api_key` auth entry cannot log into it — the adapter routes
# the prefix uniformly and radian reports the credential error.
BUILTIN_PROVIDER_IDS = (
    "opencode-go",
    "chatgpt",
    "anthropic",
    "openai",
    "openai-compatible",
)
# `auth.json` entry id for the generic OpenAI-compatible descriptor (ADR 0026).
GENERIC_PROVIDER_ID = "openai-compatible"
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
    provider: str | None,
    configured_base_url: str | None,
    base_url: str | None,
) -> tuple[str, str | None]:
    """Pick the radian `--provider` and `--base-url` for a Harbor connection.

    `provider` is the framework-resolved provider slug (the model name prefix,
    e.g. `opencode-go` in `opencode-go/deepseek-v4.1-flash`); `configured_base_url`
    is an explicitly configured endpoint (a `*_BASE_URL` env var the trial set);
    `base_url` is the resolved endpoint, explicit or provider default. Returns
    `(provider, base_url)`.
    """
    if configured_base_url is not None:
        # An explicitly configured endpoint (e.g. a proxy): radian's built-in
        # descriptors use their own endpoints, so endpoint traffic always runs
        # as the generic `openai-compatible` provider with `--base-url` —
        # including proxied Anthropic traffic, since radian's `anthropic`
        # provider takes no `--base-url` (ADR 0026 D0).
        return GENERIC_PROVIDER_ID, configured_base_url
    normalized = (provider or "").lower()
    if normalized in BUILTIN_PROVIDER_IDS:
        return normalized, None
    return GENERIC_PROVIDER_ID, base_url


def build_auth_json(provider: str, base_url: str | None) -> str:
    """Render the `auth.json` content naming the credential by reference.

    The key is always the `$RADIAN_BENCH_API_KEY` reference, so the file never
    holds the secret. The generic descriptor additionally carries its endpoint
    under `env.RADIAN_BASE_URL` (ADR 0026 D10); a built-in descriptor needs no
    endpoint because radian uses its own.
    """
    if provider == GENERIC_PROVIDER_ID:
        env: dict[str, str] = {}
        if base_url is not None:
            env[RADIAN_BASE_URL_ENV] = base_url
        entry: dict[str, object] = {
            "type": "api_key",
            "key": f"${CREDENTIAL_ENV_NAME}",
            "env": env,
        }
    else:
        entry = {
            "type": "api_key",
            "key": f"${CREDENTIAL_ENV_NAME}",
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


# Journal event names that carry billed model usage
# (`crates/radian-session/src/reader.rs`): a conversation request and a
# summarization call. Both count — the summarizer's calls are real money.
_USAGE_EVENT_NAMES = ("ModelUsageRecorded", "CompactionUsageRecorded")


@dataclass(frozen=True)
class SessionUsage:
    """Token and cost totals summed across one trial's session journals.

    `None` means "not reported", never zero: a counter no record mentioned
    stays `None` so Harbor shows it as unknown rather than as a measured 0.
    """

    n_input_tokens: int | None = None
    n_cache_tokens: int | None = None
    n_output_tokens: int | None = None
    cost_usd: float | None = None


def _as_count(value: object) -> int | None:
    """A reported counter, or `None` when the field is absent or misshapen."""
    if isinstance(value, bool) or not isinstance(value, int):
        return None
    return value


def summarize_session_usage(sessions_dir: str | Path) -> SessionUsage:
    """Sum usage across the `*.jsonl` session journals in `sessions_dir`.

    Adds `data.usage` input/output/cache_read over both usage event names and
    converts summed `data.fee.micros` (micro-dollars) to USD. A missing
    directory, an unreadable file, a record without `data.usage`, and an
    unparsable line are all skipped, never fatal: a half-written journal
    (the trial was killed mid-turn) still reports what it recorded.
    """
    input_tokens = 0
    cache_tokens = 0
    output_tokens = 0
    micros = 0
    seen_input = False
    seen_cache = False
    seen_output = False
    seen_fee = False
    try:
        files = sorted(Path(sessions_dir).glob("*.jsonl"))
    except OSError:
        return SessionUsage()
    for path in files:
        try:
            text = path.read_text(encoding="utf-8")
        except OSError:
            continue
        for line in text.splitlines():
            if not line.strip():
                continue
            try:
                record = json.loads(line)
            except ValueError:
                continue
            if not isinstance(record, dict):
                continue
            if record.get("event") not in _USAGE_EVENT_NAMES:
                continue
            data = record.get("data")
            if not isinstance(data, dict):
                continue
            usage = data.get("usage")
            if isinstance(usage, dict):
                count = _as_count(usage.get("input"))
                if count is not None:
                    input_tokens += count
                    seen_input = True
                count = _as_count(usage.get("cache_read"))
                if count is not None:
                    cache_tokens += count
                    seen_cache = True
                count = _as_count(usage.get("output"))
                if count is not None:
                    output_tokens += count
                    seen_output = True
            fee = data.get("fee")
            if isinstance(fee, dict):
                fee_micros = _as_count(fee.get("micros"))
                if fee_micros is not None:
                    micros += fee_micros
                    seen_fee = True
    return SessionUsage(
        n_input_tokens=input_tokens if seen_input else None,
        n_cache_tokens=cache_tokens if seen_cache else None,
        n_output_tokens=output_tokens if seen_output else None,
        cost_usd=micros / 1_000_000 if seen_fee else None,
    )


# Seconds radian keeps clear of the exec kill so the turn ends and the journal
# is written before Harbor kills it. The task's own agent timeout never reaches
# the agent object (Harbor applies it as an external `asyncio.wait_for` around
# `run()`), so the only reachable deadline is the adapter-side exec timeout.
_TURN_TIMEOUT_MARGIN_SECS = 60
_TURN_TIMEOUT_FLOOR_SECS = 60


def derive_turn_timeout_secs(
    explicit: int | None, exec_timeout_secs: int | None
) -> int | None:
    """Settle the `--turn-timeout-secs` for one trial.

    An explicit option always wins. Otherwise, when the adapter-side exec
    timeout is set, the turn timeout is that deadline minus a margin (floored),
    so radian wraps up and writes its journal instead of dying mid-turn. When
    neither is set there is no reachable deadline and the flag stays off.
    """
    if explicit is not None:
        return explicit
    if exec_timeout_secs is None:
        return None
    return max(_TURN_TIMEOUT_FLOOR_SECS, exec_timeout_secs - _TURN_TIMEOUT_MARGIN_SECS)


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


# The ELF program-header type of the interpreter path entry: present in a
# dynamically linked binary, absent in a static (including static-pie) one.
_PT_INTERP = 3


def assert_binary_static(path: str) -> None:
    """Refuse a dynamically linked (or non-ELF) radian binary with a clear message.

    Reads the ELF program headers in pure Python (no new dependency): a
    `PT_INTERP` entry means the binary needs the container's dynamic loader,
    which the task images (glibc mismatches, missing loader) cannot satisfy.
    Rebuild a static musl binary with `build-static.sh`.
    """
    with open(path, "rb") as handle:
        header = handle.read(64)
        if len(header) < 54 or header[:4] != b"\x7fELF":
            raise ValueError(f"Radian binary is not an ELF executable: {path}")
        identity = header[4]
        if identity == 2:  # 64-bit
            phoff_at, phentsize_at = 0x20, 0x36
            addr_fmt = "Q"
        elif identity == 1:  # 32-bit
            phoff_at, phentsize_at = 0x1C, 0x2A
            addr_fmt = "I"
        else:
            raise ValueError(f"Radian binary has an unknown ELF class: {path}")
        endian = "<" if header[5] == 1 else ">"
        addr_size = 8 if identity == 2 else 4
        (phoff,) = struct.unpack(
            endian + addr_fmt, header[phoff_at : phoff_at + addr_size]
        )
        (phentsize, phnum) = struct.unpack(endian + "HH", header[phentsize_at : phentsize_at + 4])
        if phnum == 0:
            raise ValueError(f"Radian binary has no program headers: {path}")
        if phentsize * phnum > 1024 * 1024:
            raise ValueError(f"Radian binary has a corrupt program header table: {path}")
        handle.seek(phoff)
        table = handle.read(phentsize * phnum)
    for index in range(phnum):
        entry = table[index * phentsize : (index + 1) * phentsize]
        if len(entry) < 4:
            break
        (segment_type,) = struct.unpack(endian + "I", entry[:4])
        if segment_type == _PT_INTERP:
            raise ValueError(
                f"Radian binary is dynamically linked (ELF PT_INTERP present): "
                f"{path}; rebuild a static musl binary with build-static.sh"
            )
