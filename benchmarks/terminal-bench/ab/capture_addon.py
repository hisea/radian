"""mitmproxy addon: one JSON line per model request of one A/B arm.

`smoke.sh`, `ab.sh` and `ab-system.sh` start one `mitmdump` per arm when
`AB_CAPTURE=1` and point that arm's agent at it (see `capture.py`):

    AB_CAPTURE_OUT=<label>/capture/<arm>.jsonl mitmdump --mode regular \\
      --listen-host 0.0.0.0 --listen-port <port> \\
      -s ab/capture_addon.py --set confdir=<label>/capture/<arm>/confdir

The capture is meant to prove that the arms send the same model parameters. It
therefore copies *parameters only*: a line holds no message text, no tool
schema, no header value and no key. A request from a client outside
`AB_CAPTURE_CLIENTS` (CIDR list, default `172.16.0.0/12`) is refused with a 403
and writes no line.

The summary (`summarize`) is a pure function of the body bytes and the request
path, so it is testable without mitmproxy; the hooks below are the only part
that needs it.
"""

from __future__ import annotations

import hashlib
import ipaddress
import json
import os
import time
from pathlib import Path

# The Docker bridge networks the task containers are on. A client from anywhere
# else (a stray browser pointed at the proxy) is refused and not recorded.
DEFAULT_CLIENTS = "172.16.0.0/12"

# A model request is any request whose path ends with one of these. The query
# string is ignored; everything else (health checks, telemetry, tool downloads)
# is forwarded and not recorded.
MODEL_PATH_SUFFIXES = ("/chat/completions", "/responses", "/messages")

# Body fields copied verbatim (a missing field is recorded as null, so two arms
# that differ in *sending* a parameter show up as differing values).
PARAM_KEYS = (
    "model",
    "stream",
    "max_tokens",
    "max_completion_tokens",
    "max_output_tokens",
    "temperature",
    "top_p",
    "stream_options",
)

# Every top-level body key whose name contains one of these (lowercase) is also
# copied: `reasoning_effort`, `thinking`, `reasoning`, `reasoning_effort`, ...
REASON_KEY_PARTS = ("reason", "think")

# Bookkeeping keys of one capture line (not model parameters).
LINE_KEYS = ("ts", "arm", "client_ip", "method", "host", "path", "status")


def is_model_path(path: str) -> bool:
    """True when a request path is a model request (query string ignored)."""
    base = (path or "").split("?", 1)[0]
    return any(base.endswith(suffix) for suffix in MODEL_PATH_SUFFIXES)


def parse_clients(spec: str | None) -> tuple:
    """Parse a CIDR list; unset or blank means `DEFAULT_CLIENTS`.

    A malformed entry raises `ValueError`: a silently empty allow list would
    refuse every client and a silently open one would accept strangers.
    """
    text = (spec or "").strip() or DEFAULT_CLIENTS
    networks = []
    for item in text.split(","):
        item = item.strip()
        if item:
            networks.append(ipaddress.ip_network(item, strict=False))
    return tuple(networks)


def normalize_ip(text: str | None):
    """An address for comparison, with an IPv4-mapped IPv6 form unwrapped.

    A dual-stack listener reports `::ffff:172.18.0.3` for a Docker bridge
    client; the rule lists are IPv4.
    """
    if not text:
        return None
    try:
        address = ipaddress.ip_address(text)
    except ValueError:
        return None
    if isinstance(address, ipaddress.IPv6Address) and address.ipv4_mapped:
        return address.ipv4_mapped
    return address


def client_allowed(peer: str | None, networks=None) -> bool:
    """True when the client address is inside the allow list."""
    lists = parse_clients(DEFAULT_CLIENTS) if networks is None else networks
    address = normalize_ip(peer)
    if address is None:
        return False
    for network in lists:
        # A cross-version membership test raises TypeError in ipaddress.
        if address.version == network.version and address in network:
            return True
    return False


def _parts_text(content) -> str | None:
    """The text of a message content: a string, or a list of text blocks."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for part in content:
            if isinstance(part, str):
                parts.append(part)
            elif isinstance(part, dict) and isinstance(part.get("text"), str):
                parts.append(part["text"])
        return "\n".join(parts)
    return None


def _system_text(payload: dict) -> str | None:
    """The system prompt text: a `system` message, `instructions`, `system`.

    Every form may be a string or a list of text blocks: Anthropic's `/messages`
    sends a top-level `system` as blocks, so `_parts_text` reads it the same way
    it reads a `system` message's content.
    """
    messages = payload.get("messages")
    if isinstance(messages, list):
        for message in messages:
            if isinstance(message, dict) and message.get("role") == "system":
                text = _parts_text(message.get("content"))
                if text is not None:
                    return text
    for key in ("instructions", "system"):
        text = _parts_text(payload.get(key))
        if text is not None:
            return text
    return None


def system_summary(payload: dict) -> dict | None:
    """Length and a short digest of the system prompt, never the text itself."""
    text = _system_text(payload)
    if text is None:
        return None
    digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
    return {"chars": len(text), "sha256_prefix": digest[:12]}


def _tool_name(tool: dict) -> str | None:
    function = tool.get("function")
    if isinstance(function, dict) and isinstance(function.get("name"), str):
        return function["name"]
    name = tool.get("name")
    return name if isinstance(name, str) else None


def _tool_is_strict(tool: dict) -> bool:
    function = tool.get("function")
    if isinstance(function, dict) and function.get("strict") is True:
        return True
    return tool.get("strict") is True


def tools_summary(tools) -> dict | None:
    """Count, compact JSON size, `strict` count and sorted names of the tools."""
    if not isinstance(tools, list):
        return None
    names = sorted(
        name
        for name in (_tool_name(tool) for tool in tools if isinstance(tool, dict))
        if name
    )
    return {
        "count": len(tools),
        "chars": len(json.dumps(tools, separators=(",", ":"), ensure_ascii=False)),
        "strict": sum(
            1 for tool in tools if isinstance(tool, dict) and _tool_is_strict(tool)
        ),
        "names": names,
    }


def _messages_for(payload: dict, path: str):
    """The conversation array: `messages`, or `input` for the Responses API."""
    first, second = ("input", "messages") if path.endswith("/responses") else (
        "messages",
        "input",
    )
    if first in payload:
        return payload[first]
    return payload.get(second)


def messages_summary(messages) -> dict | None:
    """How many messages, how many assistant turns, how many carry reasoning."""
    if isinstance(messages, str):
        return {"count": 1, "assistant": 0, "assistant_reasoning": 0}
    if not isinstance(messages, list):
        return None
    assistant = 0
    with_reasoning = 0
    for message in messages:
        if not isinstance(message, dict) or message.get("role") != "assistant":
            continue
        assistant += 1
        if (
            message.get("reasoning_content") is not None
            or message.get("reasoning") is not None
        ):
            with_reasoning += 1
    return {
        "count": len(messages),
        "assistant": assistant,
        "assistant_reasoning": with_reasoning,
    }


def summarize(body: bytes, path: str) -> dict:
    """The parameter summary of one model request body.

    Pure: body bytes and path in, a JSON-serializable dict out. The dict holds
    no header, no key and no message or tool text — only counts, lengths,
    digests and the model's own parameters.
    """
    record: dict = {"body_bytes": len(body or b"")}
    try:
        payload = json.loads(body) if body else {}
    except (UnicodeDecodeError, ValueError):
        # Not JSON (a truncated body, a non-model POST): only its size is known.
        return record
    if not isinstance(payload, dict):
        return record
    for key in PARAM_KEYS:
        record[key] = payload.get(key)
    for key, value in payload.items():
        lowered = key.lower()
        if any(part in lowered for part in REASON_KEY_PARTS):
            record[key] = value
    record["tools"] = tools_summary(payload.get("tools"))
    record["system"] = system_summary(payload)
    record["messages"] = messages_summary(_messages_for(payload, path))
    return record


def _peer(flow) -> str | None:
    """The client address of a flow, as a string."""
    connection = getattr(flow, "client_conn", None)
    peername = getattr(connection, "peername", None)
    if isinstance(peername, (tuple, list)) and peername:
        return str(peername[0])
    return None


def _forbidden():
    """A 403 response body for a client outside the allow list."""
    from mitmproxy import http  # Only the hooks need mitmproxy.

    return http.Response.make(
        403,
        b"ab capture: client is not in AB_CAPTURE_CLIENTS\n",
        {"content-type": "text/plain"},
    )


class CaptureAddon:
    """Appends one line per model request; refuses clients outside the list."""

    def __init__(self, out_path, clients=None, arm: str | None = None):
        self.out_path = Path(out_path)
        # One file per arm, so the arm name is the file's stem.
        self.arm = arm or self.out_path.stem
        self.networks = parse_clients(DEFAULT_CLIENTS) if clients is None else clients
        self.out_path.parent.mkdir(parents=True, exist_ok=True)

    @classmethod
    def from_env(cls) -> "CaptureAddon":
        out = os.environ.get("AB_CAPTURE_OUT")
        if not out:
            raise RuntimeError(
                "AB_CAPTURE_OUT is not set: the capture addon writes one file per arm"
            )
        return cls(out, parse_clients(os.environ.get("AB_CAPTURE_CLIENTS")))

    # mitmproxy hooks. A flow is duck-typed here so the tests need no mitmproxy.

    def request(self, flow) -> None:
        if not client_allowed(_peer(flow), self.networks):
            flow.response = _forbidden()

    def response(self, flow) -> None:
        status = getattr(getattr(flow, "response", None), "status_code", None)
        self._record(flow, status)

    def error(self, flow) -> None:
        # The request never got a response: its size and parameters are still
        # worth a line, with `status: null`.
        self._record(flow, None)

    def _record(self, flow, status) -> None:
        request = flow.request
        path = getattr(request, "path", "")
        peer = _peer(flow)
        if not is_model_path(path) or not client_allowed(peer, self.networks):
            return
        line = {
            "ts": time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime()) + "Z",
            "arm": self.arm,
            "client_ip": peer,
            "method": getattr(request, "method", ""),
            "host": getattr(request, "pretty_host", ""),
            "path": path,
            "status": status,
        }
        # `strict=False`: an unsupported content-encoding leaves the body as-is
        # (only its size is then known) instead of raising inside a proxy hook.
        body = request.get_content(strict=False) or b""
        line.update(summarize(body, path))
        with self.out_path.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(line, sort_keys=True) + "\n")


# mitmproxy loads this name from the script. The addon is built on `load()`
# (mitmproxy's first call), not at import time, so `capture_check.py` and the
# tests can import the pure summary without an AB_CAPTURE_OUT.
class _Script:
    """The mitmproxy entry point: one `CaptureAddon`, resolved on first use."""

    _addon: CaptureAddon | None = None

    def resolve(self) -> CaptureAddon:
        if self._addon is None:
            self._addon = CaptureAddon.from_env()
        return self._addon

    def load(self, loader) -> None:
        # A missing AB_CAPTURE_OUT must stop mitmdump at start-up, not mid-run.
        self.resolve()

    def request(self, flow) -> None:
        self.resolve().request(flow)

    def response(self, flow) -> None:
        self.resolve().response(flow)

    def error(self, flow) -> None:
        self.resolve().error(flow)


addons = [_Script()]
