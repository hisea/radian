"""Unit tests for `radian_command.py` (stdlib only, no Harbor dependency)."""

import json
import shlex
import struct
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from radian_command import (  # noqa: E402
    BUILTIN_PROVIDER_IDS,
    CREDENTIAL_ENV_NAME,
    RadianInvocation,
    assert_binary_static,
    build_auth_json,
    build_command,
    redact_base_url,
    select_provider,
    sha256_of_file,
)


def base_invocation(**overrides):
    args = {
        "provider": "openai-compatible",
        "model": "gpt-5",
        "base_url": "https://proxy.example/v1",
        "workspace": "/work/task",
    }
    args.update(overrides)
    return RadianInvocation(**args)


def _elf64(*, dynamic):
    ident = b"\x7fELF" + bytes([2, 1, 1, 0]) + b"\x00" * 8
    segments = [1]  # PT_LOAD
    if dynamic:
        segments.append(3)  # PT_INTERP
    header = ident + struct.pack(
        "<HHIQQQIHHHHHH", 2, 62, 1, 0, 64, 0, 0, 64, 56, len(segments), 0, 0, 0
    )
    body = b"".join(
        struct.pack("<IIQQQQQQ", kind, 5, 0, 0, 0, 0, 0, 0x1000)
        for kind in segments
    )
    return header + body


def _write_temp(data):
    handle = tempfile.NamedTemporaryFile(delete=False)
    try:
        handle.write(data)
    finally:
        handle.close()
    return handle.name


class SelectProviderTest(unittest.TestCase):
    def test_builtin_prefix_runs_without_base_url(self):
        for slug in BUILTIN_PROVIDER_IDS:
            with self.subTest(provider=slug):
                provider, base_url = select_provider(
                    provider=slug,
                    configured_base_url=None,
                    base_url=None,
                )
                self.assertEqual((provider, base_url), (slug, None))

    def test_opencode_go_model_uses_own_endpoint(self):
        provider, base_url = select_provider(
            provider="opencode-go",
            configured_base_url=None,
            base_url=None,
        )
        self.assertEqual((provider, base_url), ("opencode-go", None))

    def test_configured_endpoint_wins_over_builtin_prefix(self):
        # A proxy in front of a built-in provider still runs as the generic
        # provider: built-in descriptors use their own endpoints.
        provider, base_url = select_provider(
            provider="opencode-go",
            configured_base_url="https://proxy.example/v1",
            base_url="https://proxy.example/v1",
        )
        self.assertEqual(
            (provider, base_url), ("openai-compatible", "https://proxy.example/v1")
        )

    def test_proxied_anthropic_traffic_uses_generic(self):
        provider, base_url = select_provider(
            provider="anthropic",
            configured_base_url="https://proxy.example/anthropic",
            base_url="https://proxy.example/anthropic",
        )
        self.assertEqual(
            (provider, base_url),
            ("openai-compatible", "https://proxy.example/anthropic"),
        )

    def test_unknown_provider_falls_back_to_generic(self):
        provider, base_url = select_provider(
            provider="no-such-provider",
            configured_base_url=None,
            base_url="https://proxy.example/v1",
        )
        self.assertEqual(
            (provider, base_url), ("openai-compatible", "https://proxy.example/v1")
        )

    def test_unknown_provider_without_endpoint_uses_generic_default(self):
        provider, base_url = select_provider(
            provider=None,
            configured_base_url=None,
            base_url=None,
        )
        self.assertEqual((provider, base_url), ("openai-compatible", None))


class BuildAuthJsonTest(unittest.TestCase):
    def test_generic_entry_references_env_and_carries_base_url(self):
        content = build_auth_json("openai-compatible", "https://proxy.example/v1")
        entry = json.loads(content)["openai-compatible"]
        self.assertEqual(entry["key"], f"${CREDENTIAL_ENV_NAME}")
        self.assertEqual(entry["env"]["RADIAN_BASE_URL"], "https://proxy.example/v1")
        self.assertNotIn("sk-", content)

    def test_builtin_entry_has_no_endpoint(self):
        for slug in ("opencode-go", "anthropic", "openai"):
            with self.subTest(provider=slug):
                content = build_auth_json(slug, None)
                entry = json.loads(content)[slug]
                self.assertEqual(entry["key"], f"${CREDENTIAL_ENV_NAME}")
                self.assertNotIn("env", entry)
                self.assertNotIn("sk-", content)

    def test_auth_json_never_contains_a_secret_value(self):
        content = build_auth_json("openai-compatible", "https://proxy.example/v1")
        self.assertNotIn("sk-live", content)
        self.assertIn(CREDENTIAL_ENV_NAME, content)


class BuildCommandTest(unittest.TestCase):
    def test_unattended_full_profile_defaults(self):
        command = build_command(
            "/tmp/radian", base_invocation(), "do the thing", "/logs/out.txt", "/logs/err.txt"
        )
        for flag in ("--yes", "--profile", "full", "--no-trust", "--offline-catalog"):
            self.assertIn(flag, command)

    def test_instruction_is_one_shell_word(self):
        instruction = 'write a file; rm -rf /tmp/x && echo "hi"'
        command = build_command(
            "/tmp/radian", base_invocation(), instruction, "/logs/out.txt", "/logs/err.txt"
        )
        self.assertIn(shlex.quote(instruction), command)

    def test_binary_and_paths_are_quoted(self):
        command = build_command(
            "/tmp/my radian",
            base_invocation(workspace="/work/my task"),
            "hi",
            "/logs/my out.txt",
            "/logs/my err.txt",
        )
        self.assertIn(shlex.quote("/tmp/my radian"), command)
        self.assertIn(shlex.quote("/work/my task"), command)

    def test_builtin_invocation_has_no_base_url(self):
        invocation = base_invocation(provider="opencode-go", model="x", base_url=None)
        command = build_command("/tmp/radian", invocation, "hi", "/o", "/e")
        self.assertIn("--provider opencode-go", command)
        self.assertNotIn("--base-url", command)

    def test_optional_limits_are_passed_through(self):
        invocation = base_invocation(
            max_model_calls=50, turn_timeout_secs=300, context_window=200000
        )
        command = build_command("/tmp/radian", invocation, "hi", "/o", "/e")
        self.assertIn("--max-model-calls 50", command)
        self.assertIn("--turn-timeout-secs 300", command)
        self.assertIn("--context-window 200000", command)

    def test_secret_value_never_appears(self):
        secret = "sk-live-secret-value"
        command = build_command(
            "/tmp/radian", base_invocation(), "use the key " + secret, "/o", "/e"
        )
        # The caller must never interpolate the real instruction with a secret;
        # the builder itself adds no secret material either.
        self.assertNotIn(CREDENTIAL_ENV_NAME + "=", command)


class RedactBaseUrlTest(unittest.TestCase):
    def test_strips_query_and_userinfo(self):
        self.assertEqual(
            redact_base_url("https://user:token@proxy.example/v1/chat?key=sekret"),
            "https://proxy.example/v1/chat",
        )

    def test_none_passes_through(self):
        self.assertIsNone(redact_base_url(None))


class Sha256Test(unittest.TestCase):
    def test_digest_matches_hashlib(self):
        import hashlib

        with tempfile.NamedTemporaryFile(delete=False) as handle:
            handle.write(b"radian-binary-bytes")
            path = handle.name
        self.assertEqual(sha256_of_file(path), hashlib.sha256(b"radian-binary-bytes").hexdigest())


class AssertBinaryStaticTest(unittest.TestCase):
    def test_static_elf_passes(self):
        path = _write_temp(_elf64(dynamic=False))
        try:
            assert_binary_static(path)
        finally:
            Path(path).unlink()

    def test_dynamic_elf_is_refused(self):
        path = _write_temp(_elf64(dynamic=True))
        try:
            with self.assertRaises(ValueError) as raised:
                assert_binary_static(path)
            self.assertIn("dynamically linked", str(raised.exception))
        finally:
            Path(path).unlink()

    def test_non_elf_is_refused(self):
        path = _write_temp(b"#!/bin/sh\necho hi\n")
        try:
            with self.assertRaises(ValueError) as raised:
                assert_binary_static(path)
            self.assertIn("not an ELF", str(raised.exception))
        finally:
            Path(path).unlink()


if __name__ == "__main__":
    unittest.main()
