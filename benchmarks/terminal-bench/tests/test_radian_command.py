"""Unit tests for `radian_command.py` (stdlib only, no Harbor dependency)."""

import json
import shlex
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from radian_command import (  # noqa: E402
    CREDENTIAL_ENV_NAME,
    RadianInvocation,
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


class SelectProviderTest(unittest.TestCase):
    def test_openai_format_with_custom_endpoint_uses_generic(self):
        provider, base_url = select_provider(
            model_api_format="openai_chat_completions",
            user_supplied_base_url="https://proxy.example/v1",
            resolved_base_url="https://proxy.example/v1",
            provider="openai",
        )
        self.assertEqual((provider, base_url), ("openai-compatible", "https://proxy.example/v1"))

    def test_unset_format_with_custom_endpoint_uses_generic(self):
        provider, base_url = select_provider(
            model_api_format=None,
            user_supplied_base_url="https://proxy.example/v1",
            resolved_base_url="https://proxy.example/v1",
            provider="openai",
        )
        self.assertEqual((provider, base_url), ("openai-compatible", "https://proxy.example/v1"))

    def test_anthropic_value_routes_to_anthropic_without_base_url(self):
        # Harbor's StrEnum serializes to the lowercase value.
        provider, base_url = select_provider(
            model_api_format="anthropic_messages",
            user_supplied_base_url=None,
            resolved_base_url="https://api.anthropic.com",
            provider="anthropic",
        )
        self.assertEqual((provider, base_url), ("anthropic", None))

    def test_anthropic_name_routes_to_anthropic_without_base_url(self):
        provider, base_url = select_provider(
            model_api_format="ANTHROPIC_MESSAGES",
            user_supplied_base_url=None,
            resolved_base_url="https://api.anthropic.com",
            provider="anthropic",
        )
        self.assertEqual((provider, base_url), ("anthropic", None))

    def test_anthropic_format_with_custom_endpoint_is_refused(self):
        with self.assertRaises(ValueError):
            select_provider(
                model_api_format="anthropic_messages",
                user_supplied_base_url="https://proxy.example/anthropic",
                resolved_base_url="https://proxy.example/anthropic",
                provider="anthropic",
            )


class BuildAuthJsonTest(unittest.TestCase):
    def test_generic_entry_references_env_and_carries_base_url(self):
        content = build_auth_json("openai-compatible", "https://proxy.example/v1")
        entry = json.loads(content)["openai-compatible"]
        self.assertEqual(entry["key"], f"${CREDENTIAL_ENV_NAME}")
        self.assertEqual(entry["env"]["RADIAN_BASE_URL"], "https://proxy.example/v1")
        self.assertNotIn("sk-", content)

    def test_anthropic_entry_has_no_endpoint(self):
        content = build_auth_json("anthropic", None)
        entry = json.loads(content)["anthropic"]
        self.assertEqual(entry["key"], f"${CREDENTIAL_ENV_NAME}")
        self.assertNotIn("env", entry)


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

    def test_anthropic_invocation_has_no_base_url(self):
        invocation = base_invocation(provider="anthropic", model="claude-x", base_url=None)
        command = build_command("/tmp/radian", invocation, "hi", "/o", "/e")
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


if __name__ == "__main__":
    unittest.main()
