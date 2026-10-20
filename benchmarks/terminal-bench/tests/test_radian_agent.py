"""Unit tests for `radian_agent.py` against the real Harbor 0.24.0 package.

Run with Harbor's Python (it provides `harbor` and `pydantic`):
`<home>/.local/share/uv/tools/harbor/bin/python
benchmarks/terminal-bench/tests/test_radian_agent.py`.

The fake environment below is only a process double for the container: it
records exec/upload calls and serves programmed results. It is not a Harbor
simulator — anything about trial orchestration must be verified in a real
Harbor smoke run, not here.
"""

import asyncio
import json
import os
import struct
import sys
import tempfile
import unittest
from pathlib import Path, PurePosixPath
from unittest import mock

BENCH_DIR = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(BENCH_DIR))

from harbor.agents.installed.base import NonZeroAgentExitCodeError  # noqa: E402
from harbor.environments.base import ExecResult  # noqa: E402
from harbor.models.agent.context import AgentContext  # noqa: E402

import radian_agent  # noqa: E402
from radian_agent import RadianAgent  # noqa: E402
from radian_command import CREDENTIAL_ENV_NAME  # noqa: E402

TEST_KEY = "sk-test-key"


def _elf64(*, dynamic):
    ident = b"\x7fELF" + bytes([2, 1, 1, 0]) + b"\x00" * 8
    segments = [1] if not dynamic else [1, 3]  # PT_LOAD, PT_INTERP
    header = ident + struct.pack(
        "<HHIQQQIHHHHHH", 2, 62, 1, 0, 64, 0, 0, 64, 56, len(segments), 0, 0, 0
    )
    return header + b"".join(
        struct.pack("<IIQQQQQQ", kind, 5, 0, 0, 0, 0, 0, 0x1000)
        for kind in segments
    )


class FakeEnvironment:
    """Records exec/upload calls; serves programmed results by substring match."""

    def __init__(self, default_user=None):
        self.default_user = default_user
        self.exec_calls = []
        self.uploads = []
        self.stubs = []

    def on(self, substring, result):
        self.stubs.append((substring, result))
        return self

    async def exec(self, command, cwd=None, env=None, timeout_sec=None, user=None):
        self.exec_calls.append(
            {"command": command, "cwd": cwd, "env": dict(env or {}),
             "timeout_sec": timeout_sec, "user": user}
        )
        for substring, result in self.stubs:
            if substring in command:
                return result
        return ExecResult(stdout="", stderr="", return_code=0)

    async def upload_file(self, source_path, target_path):
        content = Path(source_path).read_bytes()
        self.uploads.append({"source": str(source_path), "target": target_path,
                             "content": content})
        return None

    def last_exec(self):
        return self.exec_calls[-1]


def scrub_host_key():
    """Hide the owner's real key from the test process environment."""
    return mock.patch.dict(
        os.environ,
        {k: v for k, v in os.environ.items() if k != CREDENTIAL_ENV_NAME},
        clear=True,
    )


def make_agent(tmp, **kwargs):
    kwargs.setdefault("model_name", "opencode-go/deepseek-v4.1-flash")
    extra_env = kwargs.pop("extra_env", {CREDENTIAL_ENV_NAME: TEST_KEY})
    agent = RadianAgent(
        logs_dir=Path(tmp) / "logs", extra_env=extra_env, **kwargs
    )
    return agent


def write_binary(tmp, *, dynamic=False):
    binary = Path(tmp) / "radian"
    binary.write_bytes(_elf64(dynamic=dynamic))
    return binary


def write_ca_bundle(tmp):
    ca_bundle = Path(tmp) / "ca-certificates.crt"
    ca_bundle.write_bytes(b"fake-pem-bundle")
    return ca_bundle


def write_sessions(logs_dir, files):
    """Lay a fake `radian-sessions/` dir under the agent logs dir, as the
    synced-back trial logs would after Harbor downloads them."""
    sessions = Path(logs_dir) / "radian-sessions"
    sessions.mkdir(parents=True, exist_ok=True)
    for name, lines in files.items():
        (sessions / name).write_text("".join(lines), encoding="utf-8")
    return sessions


class InstallTest(unittest.TestCase):
    def test_install_uploads_binary_and_ca_and_records_version(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            binary = write_binary(tmp)
            ca_bundle = write_ca_bundle(tmp)
            env = FakeEnvironment().on(
                "exec --version", ExecResult(stdout="radian 0.1.0\n", return_code=0))
            agent = make_agent(
                tmp, binary_path=str(binary), ca_bundle_path=str(ca_bundle))
            asyncio.run(agent.install(env))
            self.assertEqual(
                [(u["source"], u["target"]) for u in env.uploads],
                [
                    (str(binary), "/tmp/harbor-radian-bin/radian"),
                    (str(ca_bundle), "/tmp/harbor-radian-bin/ca-certificates.crt"),
                ],
            )
            record = json.loads((Path(tmp) / "logs" / "radian-binary.json").read_text())
            self.assertEqual(record["version"], "radian 0.1.0")
            self.assertEqual(len(record["sha256"]), 64)
            self.assertEqual(len(record["ca_bundle_sha256"]), 64)
            self.assertEqual(record["ca_bundle_remote"],
                             "/tmp/harbor-radian-bin/ca-certificates.crt")
            self.assertEqual(agent.version(), "radian 0.1.0")

    def test_install_refuses_sha256_mismatch(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            binary = write_binary(tmp)
            ca_bundle = write_ca_bundle(tmp)
            agent = make_agent(tmp, binary_path=str(binary),
                               ca_bundle_path=str(ca_bundle),
                               binary_sha256="0" * 64)
            with self.assertRaises(ValueError):
                asyncio.run(agent.install(FakeEnvironment()))

    def test_install_refuses_dynamically_linked_binary(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            binary = write_binary(tmp, dynamic=True)
            ca_bundle = write_ca_bundle(tmp)
            agent = make_agent(tmp, binary_path=str(binary),
                               ca_bundle_path=str(ca_bundle))
            with self.assertRaises(ValueError) as raised:
                asyncio.run(agent.install(FakeEnvironment()))
            self.assertIn("dynamically linked", str(raised.exception))

    def test_install_refuses_missing_ca_bundle(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            binary = write_binary(tmp)
            agent = make_agent(tmp, binary_path=str(binary),
                               ca_bundle_path=str(Path(tmp) / "no-such-ca.crt"))
            with self.assertRaises(ValueError) as raised:
                asyncio.run(agent.install(FakeEnvironment()))
            self.assertIn("CA bundle not found", str(raised.exception))

    def test_install_requires_a_binary(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent = make_agent(tmp)
            with self.assertRaises(ValueError):
                asyncio.run(agent.install(FakeEnvironment()))

    def test_install_rejects_version_kwarg(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent = make_agent(tmp, binary_path="/tmp/radian", version="1.2.3")
            with self.assertRaises(ValueError):
                asyncio.run(agent.install(FakeEnvironment()))


class RunTest(unittest.TestCase):
    def test_default_exec_timeout_is_unset(self):
        # The task's own agent timeout governs; an adapter-side ceiling would
        # kill long tasks (e.g. compile-compcert's 2400 s) before the framework
        # ends the trial gracefully and runs the verifier.
        with scrub_host_key():
            agent = make_agent(tempfile.mkdtemp())
            self.assertIsNone(agent.options.timeout_sec)

    def run_trial(self, tmp, **kwargs):
        binary = Path(tmp) / "radian"
        binary.write_bytes(b"fake-elf")
        agent_kwargs = {"binary_path": str(binary)}
        agent_kwargs.update(kwargs)
        agent = make_agent(tmp, **agent_kwargs)
        agent._radian_version = "radian 0.1.0"
        env = FakeEnvironment()
        env.on("pwd", ExecResult(stdout="/work/task\n", return_code=0))
        context = AgentContext()
        return agent, env, context

    def test_opencode_go_trial_is_unattended_and_keeps_the_secret(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent, env, context = self.run_trial(tmp)
            asyncio.run(agent.run("Fix the failing test.", env, context))
            # Harbor syncs the logs, then backfills while the context is empty.
            agent.populate_context_post_run(context)
            main = env.last_exec()
            command = main["command"]
            for flag in ("--yes", "--no-trust", "--offline-catalog",
                         "--profile", "full", "--provider", "opencode-go",
                         "--model", "deepseek-v4.1-flash", "--workspace", "/work/task"):
                self.assertIn(flag, command)
            self.assertNotIn("--base-url", command)
            self.assertNotIn(TEST_KEY, command)
            # The credential travels only as the named variable.
            self.assertEqual(main["env"][CREDENTIAL_ENV_NAME], TEST_KEY)
            self.assertEqual(main["env"]["RADIAN_HOME"], "/tmp/harbor-radian-home")
            self.assertNotIn("SSL_CERT_FILE", main["env"])
            self.assertEqual(main["cwd"], "/work/task")
            auth = next(u for u in env.uploads if u["target"].endswith("auth.json"))
            entry = json.loads(auth["content"])["opencode-go"]
            self.assertEqual(entry["key"], f"${CREDENTIAL_ENV_NAME}")
            self.assertNotIn("env", entry)
            self.assertNotIn(TEST_KEY.encode(), auth["content"])
            self.assertEqual(context.metadata["agent"], "radian")
            self.assertEqual(context.metadata["radian_provider"], "opencode-go")
            self.assertEqual(context.metadata["radian_model"], "deepseek-v4.1-flash")
            self.assertEqual(context.metadata["return_code"], 0)

    def test_key_resolves_from_host_environment(self):
        # The smoke exports RADIAN_BENCH_API_KEY into the `harbor run`
        # process; no trial `--ae` is needed.
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent, env, context = self.run_trial(tmp, extra_env={})
            with mock.patch.dict(os.environ, {CREDENTIAL_ENV_NAME: TEST_KEY}):
                asyncio.run(agent.run("Do it.", env, context))
            self.assertEqual(env.last_exec()["env"][CREDENTIAL_ENV_NAME], TEST_KEY)

    def test_anthropic_trial_has_no_base_url(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent, env, context = self.run_trial(
                tmp, model_name="anthropic/claude-x")
            asyncio.run(agent.run("Do it.", env, context))
            command = env.last_exec()["command"]
            self.assertIn("--provider anthropic", command)
            self.assertNotIn("--base-url", command)

    def test_custom_endpoint_uses_generic_with_base_url(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent, env, context = self.run_trial(
                tmp,
                model_name="openai/gpt-5",
                extra_env={CREDENTIAL_ENV_NAME: TEST_KEY,
                           "OPENAI_BASE_URL": "https://proxy.example/v1"},
            )
            asyncio.run(agent.run("Do it.", env, context))
            command = env.last_exec()["command"]
            self.assertIn("--provider openai-compatible", command)
            self.assertIn("--base-url https://proxy.example/v1", command)
            auth = next(u for u in env.uploads if u["target"].endswith("auth.json"))
            entry = json.loads(auth["content"])["openai-compatible"]
            self.assertEqual(entry["env"]["RADIAN_BASE_URL"],
                             "https://proxy.example/v1")

    def test_proxied_anthropic_endpoint_uses_generic(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent, env, context = self.run_trial(
                tmp,
                model_name="anthropic/claude-x",
                extra_env={CREDENTIAL_ENV_NAME: TEST_KEY,
                           "ANTHROPIC_BASE_URL": "https://proxy.example/anthropic"},
            )
            asyncio.run(agent.run("Do it.", env, context))
            command = env.last_exec()["command"]
            self.assertIn("--provider openai-compatible", command)
            self.assertIn("--base-url https://proxy.example/anthropic", command)
            self.assertNotIn(TEST_KEY, command)

    def test_ssl_cert_file_set_only_when_container_bundle_missing(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent, env, context = self.run_trial(tmp)
            env.on("test -s", ExecResult(stdout="", stderr="", return_code=1))
            asyncio.run(agent.run("Do it.", env, context))
            main_env = env.last_exec()["env"]
            self.assertEqual(main_env["SSL_CERT_FILE"],
                             "/tmp/harbor-radian-bin/ca-certificates.crt")

    def test_ssl_cert_file_unset_when_container_bundle_present(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent, env, context = self.run_trial(tmp)
            env.on("test -s", ExecResult(stdout="", stderr="", return_code=0))
            asyncio.run(agent.run("Do it.", env, context))
            self.assertNotIn("SSL_CERT_FILE", env.last_exec()["env"])

    def test_nonzero_exit_raises_without_leaking_the_secret(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent, env, context = self.run_trial(tmp)
            env.on("radian-stdout",
                   ExecResult(stdout="partial", stderr="boom", return_code=3))
            with self.assertRaises(NonZeroAgentExitCodeError) as raised:
                asyncio.run(agent.run("Do it.", env, context))
            message = str(raised.exception)
            self.assertIn("exit 3", message)
            self.assertNotIn(TEST_KEY, message)

    def test_missing_api_key_fails_fast(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent, env, context = self.run_trial(tmp, extra_env={})
            with self.assertRaises(ValueError):
                asyncio.run(agent.run("Do it.", env, context))

    def test_run_leaves_the_context_empty_for_the_post_run_backfill(self):
        # Harbor only calls populate_context_post_run while the context is
        # empty; metadata and usage land there, not in run().
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent, env, context = self.run_trial(tmp)
            asyncio.run(agent.run("Do it.", env, context))
            self.assertTrue(context.is_empty())

    def test_populate_backfills_usage_cost_and_metadata(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent, env, context = self.run_trial(tmp)
            asyncio.run(agent.run("Fix the failing test.", env, context))
            write_sessions(agent.logs_dir, {
                "s1.jsonl": [
                    '{"event": "ModelUsageRecorded", "data": {"usage": '
                    '{"input": 1000, "output": 200, "cache_read": 800}, '
                    '"fee": {"micros": 1500}}}\n',
                    '{"event": "CompactionUsageRecorded", "data": {"revision": 3, '
                    '"model": "m", "usage": {"input": 500, "output": 100}, '
                    '"fee": {"micros": 500}}}\n',
                ],
            })
            agent.populate_context_post_run(context)
            self.assertEqual(context.n_input_tokens, 1500)
            self.assertEqual(context.n_output_tokens, 300)
            self.assertEqual(context.n_cache_tokens, 800)
            self.assertAlmostEqual(context.cost_usd, 0.002)
            self.assertEqual(context.metadata["agent"], "radian")
            self.assertEqual(context.metadata["radian_version"], "radian 0.1.0")
            self.assertEqual(context.metadata["radian_provider"], "opencode-go")
            self.assertEqual(context.metadata["radian_model"], "deepseek-v4.1-flash")
            self.assertEqual(context.metadata["radian_profile"], "full")
            self.assertEqual(context.metadata["return_code"], 0)

    def test_populate_leaves_unreported_counters_none(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent, env, context = self.run_trial(tmp)
            asyncio.run(agent.run("Do it.", env, context))
            write_sessions(agent.logs_dir, {
                "s1.jsonl": [
                    "not json\n",
                    '{"event": "ModelUsageRecorded", "data": {"usage": '
                    '{"input": 100, "output": 10}}}\n',
                ],
            })
            agent.populate_context_post_run(context)
            self.assertEqual(context.n_input_tokens, 100)
            self.assertEqual(context.n_output_tokens, 10)
            self.assertIsNone(context.n_cache_tokens)
            self.assertIsNone(context.cost_usd)

    def test_populate_without_sessions_still_sets_metadata(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent, env, context = self.run_trial(tmp)
            asyncio.run(agent.run("Do it.", env, context))
            agent.populate_context_post_run(context)
            self.assertIsNone(context.n_input_tokens)
            self.assertIsNone(context.cost_usd)
            self.assertEqual(context.metadata["return_code"], 0)

    def test_auth_json_upload_chowns_to_non_root_default_user(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent, env, context = self.run_trial(tmp)
            env.default_user = "tbench-agent"
            asyncio.run(agent.run("Do it.", env, context))
            auth = next(
                u for u in env.uploads if u["target"].endswith("credentials/auth.json"))
            entry = json.loads(auth["content"])["opencode-go"]
            self.assertEqual(entry["key"], f"${CREDENTIAL_ENV_NAME}")
            self.assertNotIn(TEST_KEY.encode(), auth["content"])
            chowns = [c for c in env.exec_calls
                      if c["user"] == "root" and "chown" in c["command"]]
            self.assertEqual(len(chowns), 1)
            self.assertIn("tbench-agent", chowns[0]["command"])
            self.assertIn("credentials/auth.json", chowns[0]["command"])
            self.assertIn("chmod 600", chowns[0]["command"])

    def test_unset_turn_timeout_derives_from_exec_timeout(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent, env, context = self.run_trial(tmp, timeout_sec=600)
            asyncio.run(agent.run("Do it.", env, context))
            command = env.last_exec()["command"]
            self.assertIn("--turn-timeout-secs 540", command)
            self.assertEqual(env.last_exec()["timeout_sec"], 600)

    def test_explicit_turn_timeout_wins_over_exec_timeout(self):
        with tempfile.TemporaryDirectory() as tmp, scrub_host_key():
            agent, env, context = self.run_trial(
                tmp, timeout_sec=600, turn_timeout_secs=100)
            asyncio.run(agent.run("Do it.", env, context))
            command = env.last_exec()["command"]
            self.assertIn("--turn-timeout-secs 100", command)
            self.assertNotIn("540", command)


if __name__ == "__main__":
    unittest.main()
