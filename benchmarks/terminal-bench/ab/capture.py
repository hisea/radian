"""Capture mixin: send one arm's model traffic through that arm's mitmdump.

`capture_addon.py` writes one line per model request on the host. This mixin
makes the agent inside the task container reach it, and changes nothing else:
with neither `--ak capture_port=<port>` nor `--ak capture_ca=<host path>` the
agent runs exactly as before — no extra command, no extra environment variable.

Once per trial, from `setup()` (after `install()`, so the image's CA tooling is
in place):

- the arm's mitmproxy CA is uploaded to `/tmp/harbor-ab-capture/ca.pem`;
- the image's own bundle is copied with the CA appended
  (`/tmp/harbor-ab-capture/ca-bundle.crt`), and `update-ca-certificates`
  registers the CA when the image ships that tool;
- `capture.json` is written to the trial's agent log dir with the container's
  own IP (the first non-loopback local address, preferring the trie block that
  contains the gateway), so `capture_check.py` maps a captured `client_ip` to a
  trial.

The proxy host is the container's default gateway, read from `/proc/net/route`
(`ip` is not installed in every task image). `HTTPS_PROXY`/`HTTP_PROXY` and
`NO_PROXY` are added to the trial command's environment; `SSL_CERT_FILE` points
at the copy of the bundle, so a client that reads it trusts the CA.

Which variable each agent reads, from its source, is in `README.md` here.

No Harbor import: the pure parsers and the mixin are testable with the standard
library alone (`tests/test_ab_capture.py`).
"""

from __future__ import annotations

import ipaddress
import json
import os
import re
import shlex
from dataclasses import replace
from pathlib import Path

# Everything the mixin touches inside the container. One fixed directory keeps
# the trial log readable and the upload paths stable.
REMOTE_DIR = "/tmp/harbor-ab-capture"
REMOTE_CA = f"{REMOTE_DIR}/ca.pem"
REMOTE_BUNDLE = f"{REMOTE_DIR}/ca-bundle.crt"
REMOTE_ANCHOR = "/usr/local/share/ca-certificates/ab-capture.crt"

# The system bundles to copy the CA into, in the order they are tried. The
# first one that exists and is non-empty wins; an image with none gets the CA
# alone.
SYSTEM_BUNDLES = (
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/cert.pem",
)

NO_PROXY_VALUE = "localhost,127.0.0.1"

CAPTURE_FILENAME = "capture.json"

# The two kinds of line in `/proc/net/fib_trie` this reads: a block header
# (`+-- 172.18.0.0/16 2 0 2`, also spelled `|--`) and a leaf address
# (`|-- 172.18.0.2`, followed by its `/32 host LOCAL` line).
_BLOCK_LINE = re.compile(
    r"^(?P<indent>\s*)(?:\+--|\|--)\s+(?P<network>\d+\.\d+\.\d+\.\d+)"
    r"/(?P<prefix>\d{1,2})(?:\s|$)"
)
_ADDRESS_LINE = re.compile(
    r"^(?P<indent>\s*)(?:\+--|\|--)\s+(?P<address>\d+\.\d+\.\d+\.\d+)(?:\s|$)"
)


def _is_ipv4(text: str) -> bool:
    parts = text.split(".")
    return len(parts) == 4 and all(
        part.isdigit() and 0 <= int(part) <= 255 for part in parts
    )


def _is_loopback(address: str) -> bool:
    try:
        return ipaddress.ip_address(address).is_loopback
    except ValueError:
        return False


def _in_network(address: str, network: tuple[str, int]) -> bool:
    try:
        return ipaddress.ip_address(address) in ipaddress.ip_network(
            f"{network[0]}/{network[1]}", strict=False
        )
    except ValueError:
        return False


def default_gateway(route_table: str) -> str | None:
    """The default gateway in a `/proc/net/route` dump, as dotted quad.

    The table is little-endian hex fields; the default route is the row whose
    destination is `00000000`. Returns `None` when there is no default route.
    """
    for line in (route_table or "").splitlines():
        fields = line.split()
        if len(fields) < 3:
            continue
        destination, gateway = fields[1], fields[2]
        if destination != "00000000" or gateway == "00000000":
            continue
        try:
            raw = bytes.fromhex(gateway)
        except ValueError:
            continue
        if len(raw) != 4:
            continue
        return ".".join(str(byte) for byte in reversed(raw))
    return None


def primary_ip(fib_trie: str, gateway: str | None = None) -> str | None:
    """The container's own non-loopback IPv4 address in a `/proc/net/fib_trie` dump.

    The trie names an address before the `/32 host LOCAL` line that describes
    it, and `ip` is not available in every image, so this reads the proc file.
    The loopback block comes first and its address is the one the kernel lists
    first, so `127.0.0.0/8` is skipped. When `gateway` is known
    (`default_gateway`), an address in the trie block that contains the gateway
    is preferred, so a container with a second interface records the address
    the proxy sees.
    """
    blocks: list[tuple[int, str, int]] = []
    entries: list[tuple[str, tuple[str, int] | None]] = []
    pending: tuple[str, int] | None = None
    for line in (fib_trie or "").splitlines():
        block = _BLOCK_LINE.match(line)
        if block:
            blocks.append(
                (len(block["indent"]), block["network"], int(block["prefix"]))
            )
            pending = None
            continue
        address = _ADDRESS_LINE.match(line)
        if address:
            pending = (address["address"], len(address["indent"]))
            continue
        if "/32 host LOCAL" in line and pending is not None:
            ip, indent = pending
            # The innermost block above this address is the network the trie
            # files it under (`172.18.0.0/16`, `10.0.0.0/24`, ...).
            network = None
            for block_indent, block_ip, prefix in blocks:
                if block_indent < indent:
                    network = (block_ip, prefix)
            entries.append((ip, network))
            pending = None
    local = [(ip, network) for ip, network in entries if not _is_loopback(ip)]
    if not local:
        return None
    if gateway and _is_ipv4(gateway):
        for ip, network in local:
            if (
                network is not None
                and network[1] > 0
                and _in_network(gateway, network)
            ):
                return ip
    return local[0][0]


def _bundle_command() -> str:
    """Copy the image's bundle and append the CA; fail unless a bundle exists."""
    candidates = " ".join(shlex.quote(path) for path in SYSTEM_BUNDLES)
    bundle = shlex.quote(REMOTE_BUNDLE)
    return (
        f": > {bundle}; "
        f"for f in {candidates}; do "
        f'if [ -s "$f" ]; then cat "$f" >> {bundle}; break; fi; '
        f"done; "
        f"cat {shlex.quote(REMOTE_CA)} >> {bundle}; "
        f"test -s {bundle}"
    )


def _register_command() -> str:
    """Register the CA with the image's trust store when it has the tool."""
    return (
        "if command -v update-ca-certificates >/dev/null 2>&1; then "
        f"mkdir -p {shlex.quote(str(Path(REMOTE_ANCHOR).parent))}; "
        f"cp {shlex.quote(REMOTE_CA)} {shlex.quote(REMOTE_ANCHOR)}; "
        "update-ca-certificates >/dev/null 2>&1 || true; "
        "fi; true"
    )


class CaptureMixin:
    """Adds the capture environment to an A/B agent; inert without the option.

    Mixed in first (`class RadianAgent(CaptureMixin, BaseInstalledAgent)`), so
    the two hooks below win over the agent's own:
    `model_connection` (both Node agents read the trial env from it) and
    `_trial_env` (radian builds its trial env in `run()`).
    """

    #: Set by `capture_setup()`; None until then.
    _capture_gateway: str | None = None
    _capture_ip: str | None = None

    def capture_option(self, name: str):
        """An adapter-side option value (`capture_port`, `capture_ca`)."""
        options = getattr(self, "options", None)
        return getattr(options, name, None) if options is not None else None

    @property
    def capture_enabled(self) -> bool:
        return self.capture_option("capture_port") is not None

    def capture_ca_path(self) -> str | None:
        return self.capture_option("capture_ca")

    async def setup(self, environment) -> None:
        await super().setup(environment)
        if self.capture_enabled:
            await self.capture_setup(environment)

    async def capture_setup(self, environment) -> None:
        """Upload the CA, trust it, and write the trial's `capture.json`."""
        port = self.capture_option("capture_port")
        if port is None:
            return
        ca = self.capture_ca_path()
        if not ca:
            raise ValueError(
                "capture_port is set but capture_ca is not: pass "
                "--ak capture_ca=<this arm's mitmproxy-ca-cert.pem>"
            )
        if not os.path.isfile(ca):
            raise FileNotFoundError(f"capture CA not found: {ca}")

        await self.exec_as_agent(
            environment, command=f"mkdir -p {shlex.quote(REMOTE_DIR)}"
        )
        await environment.upload_file(ca, REMOTE_CA)

        route = await self.exec_as_agent(environment, command="cat /proc/net/route")
        gateway = default_gateway(getattr(route, "stdout", "") or "")
        if gateway is None:
            raise RuntimeError(
                "ab capture: no default route in the container's /proc/net/route, "
                "so the proxy's address on the host is unknown"
            )

        await self.exec_as_agent(environment, command=_bundle_command())
        await self.exec_as_root(environment, command=_register_command())

        fib = await self.exec_as_agent(environment, command="cat /proc/net/fib_trie")
        self._capture_gateway = gateway
        self._capture_ip = primary_ip(getattr(fib, "stdout", "") or "", gateway)

        record = {
            "container_ip": self._capture_ip,
            "gateway": gateway,
            "port": port,
            "ca_remote": REMOTE_CA,
            "bundle_remote": REMOTE_BUNDLE,
        }
        logs_dir = Path(self.logs_dir)
        logs_dir.mkdir(parents=True, exist_ok=True)
        (logs_dir / CAPTURE_FILENAME).write_text(
            json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )

    def capture_proxy_env(self) -> dict[str, str]:
        """The proxy and trust variables for the trial command."""
        if not self.capture_enabled:
            return {}
        gateway = self._capture_gateway
        port = self.capture_option("capture_port")
        if gateway is None or port is None:
            # setup() has not run: Harbor reads `model_connection` on paths that
            # precede it (agent_info), so this must not raise. capture_check.py
            # reports the trials that have no capture.json.
            return {}
        proxy = f"http://{gateway}:{port}"
        return {
            "HTTPS_PROXY": proxy,
            "HTTP_PROXY": proxy,
            "NO_PROXY": NO_PROXY_VALUE,
            "SSL_CERT_FILE": REMOTE_BUNDLE,
        }

    def capture_node_env(self) -> dict[str, str]:
        """The variables a Node client needs to trust the CA and use the proxy.

        Empty until `capture_setup()` has the proxy address, so an arm captures
        either in full or not at all.
        """
        if not self.capture_proxy_env():
            return {}
        return {"NODE_EXTRA_CA_CERTS": REMOTE_CA, "NODE_USE_ENV_PROXY": "1"}

    def capture_env(self) -> dict[str, str]:
        """Every capture variable: both Node agents pass it as the trial env."""
        return {**self.capture_proxy_env(), **self.capture_node_env()}

    # Hook for RadianAgent.run(): it builds its trial env itself.
    def _trial_env(self) -> dict[str, str]:
        return self.capture_proxy_env()

    # Hook for OpenCode/Pi: their run() passes `model_connection.env` along.
    @property
    def model_connection(self):
        access = super().model_connection
        extra = self.capture_env()
        if not extra:
            return access
        return replace(access, env={**access.env, **extra})
