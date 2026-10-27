"""Tests for the A/B request capture: the addon, the mixin and the checker.

Standard library only, no Harbor, no Docker, no model:

    python3 benchmarks/terminal-bench/tests/test_ab_capture.py

The addon's summary and the mixin's parsers import no mitmproxy and no Harbor,
so they are always tested. Two groups skip cleanly instead of failing:

- the mitmproxy hook tests need `mitmproxy` (the addon's 403 response);
- the agent tests import the real Harbor 0.24.0 package, like
  `tests/test_radian_agent.py`.
"""

import asyncio
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

TESTS_DIR = Path(__file__).resolve().parent
BENCH_DIR = TESTS_DIR.parent
AB = BENCH_DIR / "ab"
FIXTURES = TESTS_DIR / "fixtures" / "capture"

# The scripts put both directories on PYTHONPATH; direct runs need them too.
sys.path.insert(0, str(AB))
sys.path.insert(0, str(BENCH_DIR))

import capture  # noqa: E402
import capture_addon  # noqa: E402
import capture_check  # noqa: E402
import radian_command  # noqa: E402

try:  # Harbor is present only under Harbor's own Python.
    import harbor  # noqa: F401

    HAS_HARBOR = True
except ImportError:  # pragma: no cover - depends on the interpreter
    HAS_HARBOR = False

try:  # The 403 hook builds a mitmproxy response.
    import mitmproxy  # noqa: F401

    HAS_MITMPROXY = True
except ImportError:  # pragma: no cover - depends on the interpreter
    HAS_MITMPROXY = False


def fixture_body(name):
    return (FIXTURES / name).read_bytes()


def fixture_json(name):
    return json.loads(fixture_body(name))


def result(stdout="", return_code=0):
    """What the fake container hands back for one exec."""
    return SimpleNamespace(stdout=stdout, stderr="", return_code=return_code)


ROUTE = (
    "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n"
    "eth0\t00000000\t010012AC\t0003\t0\t0\t0\t00000000\t0\t0\t0\n"
    "eth0\t000012AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0\n"
)

FIB_TRIE = """Main:
  +-- 0.0.0.0/0 3 0 5
     |-- 0.0.0.0
        /0 universe UNICAST
     |-- 172.18.0.3
        /32 host LOCAL
"""

GATEWAY = "172.18.0.1"
CONTAINER_IP = "172.18.0.3"


class FakeContainer:
    """Records exec/upload calls; serves a fixed `/proc` table."""

    def __init__(self, route=ROUTE, fib=FIB_TRIE):
        self.route = route
        self.fib = fib
        self.exec_calls = []
        self.uploads = []
        self.default_user = None

    async def exec(self, command, cwd=None, env=None, timeout_sec=None, user=None):
        self.exec_calls.append(
            {
                "command": command,
                "cwd": cwd,
                "env": dict(env or {}),
                "timeout_sec": timeout_sec,
                "user": user,
            }
        )
        if "net/route" in command:
            return result(stdout=self.route)
        if "fib_trie" in command:
            return result(stdout=self.fib)
        return result()

    async def upload_file(self, source_path, target_path):
        self.uploads.append({"source": str(source_path), "target": target_path})


class SummaryTest(unittest.TestCase):
    """`summarize(body, path)`: the parameters of one request body, nothing else."""

    def test_openai_chat_body(self):
        body = fixture_body("chat.json")
        summary = capture_addon.summarize(body, "/v1/chat/completions")
        self.assertEqual(
            summary,
            {
                "body_bytes": len(body),
                "model": "deepseek-v4.1-flash",
                "stream": True,
                "stream_options": {"include_usage": True},
                "max_tokens": None,
                "max_completion_tokens": None,
                "max_output_tokens": None,
                "temperature": 0.2,
                "top_p": None,
                "reasoning_effort": "high",
                "tools": {
                    "count": 2,
                    "chars": 264,
                    "strict": 1,
                    "names": ["read_file", "run_shell"],
                },
                "system": {"chars": 31, "sha256_prefix": "481ace4539d8"},
                "messages": {"count": 5, "assistant": 2, "assistant_reasoning": 1},
            },
        )

    def test_thinking_body(self):
        body = fixture_body("thinking.json")
        summary = capture_addon.summarize(body, "/v1/messages")
        self.assertEqual(
            summary,
            {
                "body_bytes": len(body),
                "model": "some-thinking-model",
                "stream": None,
                "stream_options": None,
                "max_tokens": 8000,
                "max_completion_tokens": None,
                "max_output_tokens": None,
                "temperature": None,
                "top_p": None,
                "thinking": {"type": "enabled", "budget_tokens": 4000},
                "tools": None,
                "system": {"chars": 26, "sha256_prefix": "0b8196193949"},
                "messages": {"count": 2, "assistant": 0, "assistant_reasoning": 0},
            },
        )

    def test_anthropic_system_as_text_blocks(self):
        body = fixture_body("system_blocks.json")
        summary = capture_addon.summarize(body, "/v1/messages")
        self.assertEqual(
            summary,
            {
                "body_bytes": len(body),
                "model": "claude-sonnet-4-5",
                "stream": True,
                "stream_options": None,
                "max_tokens": 4096,
                "max_completion_tokens": None,
                "max_output_tokens": None,
                "temperature": None,
                "top_p": None,
                "tools": None,
                # The two text blocks, joined like a message's content.
                "system": {"chars": 50, "sha256_prefix": "a94c3dd93e3b"},
                "messages": {"count": 2, "assistant": 1, "assistant_reasoning": 0},
            },
        )

    def test_instructions_as_text_blocks(self):
        body = json.dumps(
            {
                "model": "some-responses-model",
                "instructions": [{"type": "text", "text": "Be terse."}],
                "input": [],
            }
        ).encode()
        summary = capture_addon.summarize(body, "/v1/responses")
        self.assertEqual(
            summary["system"], {"chars": 9, "sha256_prefix": "28c7339ead79"}
        )

    def test_responses_body_uses_instructions_and_input(self):
        body = fixture_body("responses.json")
        summary = capture_addon.summarize(body, "/v1/responses")
        self.assertEqual(
            summary,
            {
                "body_bytes": len(body),
                "model": "some-responses-model",
                "stream": None,
                "stream_options": None,
                "max_tokens": None,
                "max_completion_tokens": None,
                "max_output_tokens": 512,
                "temperature": None,
                "top_p": None,
                "reasoning": {"effort": "high", "summary": "auto"},
                "tools": None,
                "system": {"chars": 54, "sha256_prefix": "39b884c77c22"},
                "messages": {"count": 2, "assistant": 1, "assistant_reasoning": 0},
            },
        )

    def test_a_non_json_body_reports_only_its_size(self):
        self.assertEqual(
            capture_addon.summarize(b"not json", "/v1/chat/completions"),
            {"body_bytes": 8},
        )

    def test_an_empty_body_has_no_parameters(self):
        summary = capture_addon.summarize(b"", "/v1/responses")
        self.assertEqual(summary["body_bytes"], 0)
        self.assertIsNone(summary["model"])
        self.assertIsNone(summary["messages"])

    def test_model_paths(self):
        for path in (
            "/v1/chat/completions",
            "/v1/responses",
            "/v1/messages",
            "/v1/chat/completions?beta=true",
        ):
            with self.subTest(path=path):
                self.assertTrue(capture_addon.is_model_path(path))
        for path in ("/v1/models", "/health", "/v1/chat", ""):
            with self.subTest(path=path):
                self.assertFalse(capture_addon.is_model_path(path))


class FakeFlow:
    """The parts of a mitmproxy flow the addon's hooks read."""

    def __init__(self, path, body, headers=None, peer=CONTAINER_IP, status=200):
        self.client_conn = SimpleNamespace(peername=(peer, 50000))
        self.request = SimpleNamespace(
            path=path,
            method="POST",
            pretty_host="api.example.test",
            headers=headers or {},
            get_content=lambda **kwargs: body,
        )
        self.response = None if status is None else SimpleNamespace(status_code=status)


class LineTest(unittest.TestCase):
    """A written line holds parameters only: no header value, no message text."""

    def write_line(self, path, body, headers, tmp, status=200, arm="radian-a"):
        out = Path(tmp) / "capture" / f"{arm}.jsonl"
        addon = capture_addon.CaptureAddon(out)
        addon.response(FakeFlow(path, body, headers, status=status))
        lines = out.read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(lines), 1)
        return json.loads(lines[0])

    def test_no_header_value_and_no_message_text(self):
        request = fixture_json("chat.request.json")
        body = fixture_body("chat.json")
        with tempfile.TemporaryDirectory() as tmp:
            line = self.write_line(request["path"], body, request["headers"], tmp)
        self.assertEqual(line["arm"], "radian-a")
        self.assertEqual(line["client_ip"], CONTAINER_IP)
        self.assertEqual(line["host"], "api.example.test")
        self.assertEqual(line["method"], "POST")
        self.assertEqual(line["path"], request["path"])
        self.assertEqual(line["status"], 200)
        self.assertTrue(line["ts"].endswith("Z"), line["ts"])
        text = json.dumps(line)
        for forbidden in (
            request["headers"]["authorization"],
            request["headers"]["x-api-key"],
            "sk-capture-fixture",
        ):
            self.assertNotIn(forbidden, text)
        for message_text in (
            "careful coding agent",
            "Fix the failing test",
            "Let me read the test first",
            "The failing test names the function",
            "1 passed, 1 failed",
            "The patch is applied",
            # The tool schemas are counted, never copied.
            "properties",
            "parameters",
        ):
            self.assertNotIn(message_text, text)

    def test_a_non_model_path_writes_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "capture" / "arm.jsonl"
            addon = capture_addon.CaptureAddon(out)
            # The scripts create the arm's file before the proxy starts.
            out.touch()
            addon.response(FakeFlow("/v1/models", b"{}"))
            self.assertEqual(out.read_text(encoding="utf-8"), "")

    def test_a_failed_request_is_recorded_without_a_status(self):
        with tempfile.TemporaryDirectory() as tmp:
            line = self.write_line(
                "/v1/chat/completions", fixture_body("chat.json"), {}, tmp, status=None
            )
        self.assertIsNone(line["status"])

    def test_the_file_is_one_json_object_per_line(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "capture" / "arm.jsonl"
            addon = capture_addon.CaptureAddon(out)
            for _ in range(3):
                addon.response(FakeFlow("/v1/messages", b'{"model": "m"}'))
            lines = out.read_text(encoding="utf-8").splitlines()
            self.assertEqual(len(lines), 3)
            self.assertEqual({json.loads(line)["model"] for line in lines}, {"m"})


class ClientFilterTest(unittest.TestCase):
    def test_default_list(self):
        networks = capture_addon.parse_clients(None)
        self.assertFalse(capture_addon.client_allowed("192.168.1.5", networks))
        self.assertTrue(capture_addon.client_allowed("172.18.0.3", networks))
        # A dual-stack listener reports the mapped form for an IPv4 client.
        self.assertTrue(capture_addon.client_allowed("::ffff:172.18.0.3", networks))
        self.assertTrue(capture_addon.client_allowed("172.31.255.254", networks))
        self.assertFalse(capture_addon.client_allowed("172.32.0.1", networks))
        self.assertFalse(capture_addon.client_allowed(None, networks))
        self.assertFalse(capture_addon.client_allowed("not-an-address", networks))

    def test_an_explicit_list_replaces_the_default(self):
        networks = capture_addon.parse_clients("10.0.0.0/8, 192.168.1.0/24")
        self.assertTrue(capture_addon.client_allowed("10.1.2.3", networks))
        self.assertTrue(capture_addon.client_allowed("192.168.1.5", networks))
        self.assertFalse(capture_addon.client_allowed("172.18.0.3", networks))

    def test_a_malformed_entry_is_refused(self):
        with self.assertRaises(ValueError):
            capture_addon.parse_clients("172.18.0.0/not-a-mask")

    @unittest.skipUnless(HAS_MITMPROXY, "needs mitmproxy")
    def test_a_client_outside_the_list_gets_a_403_and_no_line(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "capture" / "arm.jsonl"
            addon = capture_addon.CaptureAddon(out)
            out.touch()
            flow = FakeFlow("/v1/chat/completions", b"{}", peer="192.168.1.5")
            addon.request(flow)
            self.assertIsNotNone(flow.response)
            self.assertEqual(flow.response.status_code, 403)
            addon.response(flow)
            self.assertEqual(out.read_text(encoding="utf-8"), "")

    @unittest.skipUnless(HAS_MITMPROXY, "needs mitmproxy")
    def test_an_allowed_client_is_not_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            addon = capture_addon.CaptureAddon(Path(tmp) / "capture" / "arm.jsonl")
            flow = FakeFlow("/v1/chat/completions", b"{}", status=None)
            addon.request(flow)
            self.assertIsNone(flow.response)


class ParserTest(unittest.TestCase):
    def test_default_gateway(self):
        self.assertEqual(capture.default_gateway(ROUTE), GATEWAY)
        self.assertIsNone(capture.default_gateway("Iface\tDestination\n"))
        self.assertIsNone(capture.default_gateway(""))
        # A table with only a host route has no default gateway.
        self.assertIsNone(
            capture.default_gateway(
                "eth0\t000012AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0\n"
            )
        )

    def test_primary_ip(self):
        self.assertEqual(capture.primary_ip(FIB_TRIE), CONTAINER_IP)
        self.assertIsNone(capture.primary_ip(""))


class UnmixedAgent:
    """The agent the mixin must not change: its own setup only."""

    def __init__(self, tmp):
        self.logs_dir = Path(tmp)
        self.options = SimpleNamespace(capture_port=None, capture_ca=None)

    async def setup(self, environment):
        await environment.exec(command="echo today")


class MixedAgent(capture.CaptureMixin, UnmixedAgent):
    """`UnmixedAgent` with the capture mixin in front of it."""

    async def exec_as_agent(self, environment, command, env=None, cwd=None, **kwargs):
        return await environment.exec(command=command, env=env, cwd=cwd, **kwargs)

    async def exec_as_root(self, environment, command, env=None, cwd=None, **kwargs):
        return await environment.exec(
            command=command, env=env, cwd=cwd, user="root", **kwargs
        )


def mixed(tmp, port=None, ca=None):
    agent = MixedAgent(tmp)
    agent.options = SimpleNamespace(capture_port=port, capture_ca=ca)
    return agent


class MixinTest(unittest.TestCase):
    def test_without_the_option_nothing_changes(self):
        with tempfile.TemporaryDirectory() as tmp:
            mixed_agent = mixed(tmp)
            plain_agent = UnmixedAgent(tmp)
            mixed_env = FakeContainer()
            plain_env = FakeContainer()
            asyncio.run(mixed_agent.setup(mixed_env))
            asyncio.run(plain_agent.setup(plain_env))
            self.assertEqual(mixed_env.exec_calls, plain_env.exec_calls)
            self.assertEqual(mixed_env.uploads, plain_env.uploads)
            self.assertEqual(mixed_agent.capture_env(), {})
            self.assertEqual(mixed_agent._trial_env(), {})
            self.assertFalse((Path(tmp) / "capture.json").exists())

    def test_before_setup_no_environment_is_added(self):
        with tempfile.TemporaryDirectory() as tmp:
            ca = Path(tmp) / "mitmproxy-ca-cert.pem"
            ca.write_text("pem\n", encoding="utf-8")
            agent = mixed(tmp, port=18080, ca=str(ca))
            self.assertEqual(agent.capture_env(), {})
            self.assertEqual(agent._trial_env(), {})

    def test_with_the_option_it_adds_the_proxy_cert_and_node_variables(self):
        with tempfile.TemporaryDirectory() as tmp:
            ca = Path(tmp) / "mitmproxy-ca-cert.pem"
            ca.write_text("-----BEGIN CERTIFICATE-----\n", encoding="utf-8")
            agent = mixed(tmp, port=18080, ca=str(ca))
            environment = FakeContainer()
            asyncio.run(agent.setup(environment))

            self.assertIn(
                {"source": str(ca), "target": capture.REMOTE_CA}, environment.uploads
            )
            commands = [call["command"] for call in environment.exec_calls]
            # Today's setup command still runs first.
            self.assertEqual(commands[0], "echo today")
            self.assertIn("cat /proc/net/route", commands)
            bundle = next(
                command
                for command in commands
                if capture.REMOTE_BUNDLE in command and "for f in" in command
            )
            for system_bundle in capture.SYSTEM_BUNDLES:
                self.assertIn(system_bundle, bundle)
            self.assertIn(capture.REMOTE_CA, bundle)
            self.assertTrue(
                any(
                    call["user"] == "root" and "update-ca-certificates" in call["command"]
                    for call in environment.exec_calls
                )
            )

            self.assertEqual(
                agent.capture_env(),
                {
                    "HTTPS_PROXY": f"http://{GATEWAY}:18080",
                    "HTTP_PROXY": f"http://{GATEWAY}:18080",
                    "NO_PROXY": "localhost,127.0.0.1",
                    "SSL_CERT_FILE": capture.REMOTE_BUNDLE,
                    "NODE_EXTRA_CA_CERTS": capture.REMOTE_CA,
                    "NODE_USE_ENV_PROXY": "1",
                },
            )
            # radian's own hook gets the proxy and the bundle, not the Node ones.
            self.assertEqual(
                agent._trial_env(),
                {
                    "HTTPS_PROXY": f"http://{GATEWAY}:18080",
                    "HTTP_PROXY": f"http://{GATEWAY}:18080",
                    "NO_PROXY": "localhost,127.0.0.1",
                    "SSL_CERT_FILE": capture.REMOTE_BUNDLE,
                },
            )

            record = json.loads((Path(tmp) / "capture.json").read_text(encoding="utf-8"))
            self.assertEqual(record["container_ip"], CONTAINER_IP)
            self.assertEqual(record["gateway"], GATEWAY)
            self.assertEqual(record["port"], 18080)

    def test_a_missing_ca_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            agent = mixed(tmp, port=18080, ca=str(Path(tmp) / "nope.pem"))
            with self.assertRaises(FileNotFoundError):
                asyncio.run(agent.setup(FakeContainer()))

    def test_an_unset_ca_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            agent = mixed(tmp, port=18080, ca=None)
            with self.assertRaises(ValueError):
                asyncio.run(agent.setup(FakeContainer()))

    def test_a_container_without_a_default_route_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            ca = Path(tmp) / "ca.pem"
            ca.write_text("pem\n", encoding="utf-8")
            agent = mixed(tmp, port=18080, ca=str(ca))
            with self.assertRaises(RuntimeError):
                asyncio.run(agent.setup(FakeContainer(route="")))


@unittest.skipUnless(HAS_HARBOR, "needs Harbor's Python")
class HarborAgentTest(unittest.TestCase):
    """The real agents: capture changes the env, never the command."""

    TRIAL = "do the task"

    @classmethod
    def setUpClass(cls):
        import opencode_go
        import pi_opencode_go
        import radian_agent

        cls.radian_agent = radian_agent
        cls.opencode_go = opencode_go
        cls.pi_opencode_go = pi_opencode_go
        cls.classes = {
            "radian": radian_agent.RadianAgent,
            "opencode": opencode_go.OpenCodeGo,
            "pi": pi_opencode_go.PiOpencodeGo,
        }

    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.tmp = Path(self.tmpdir.name)
        self.ca = self.tmp / "mitmproxy-ca-cert.pem"
        self.ca.write_text("-----BEGIN CERTIFICATE-----\n", encoding="utf-8")

    def args(self, kind, **extra):
        key_env = "RADIAN_BENCH_API_KEY" if kind == "radian" else "OPENCODE_API_KEY"
        kwargs = {
            "model_name": "opencode-go/deepseek-v4.1-flash",
            "extra_env": {key_env: "test-key"},
            "version": {"radian": None, "opencode": "1.19.4", "pi": "1.4.0"}[kind],
        }
        kwargs.update(extra)
        return kwargs

    def agent(self, kind, name, **extra):
        kwargs = self.args(kind, **extra)
        if kwargs["version"] is None:
            kwargs.pop("version")
        return self.classes[kind](logs_dir=self.tmp / name, **kwargs)

    def run_trial(self, agent, environment):
        from harbor.models.agent.context import AgentContext

        asyncio.run(agent.run(self.TRIAL, environment, AgentContext()))

    def trial_call(self, environment):
        return environment.exec_calls[-1]

    def test_with_capture_off_the_trial_is_exactly_todays(self):
        for kind in ("radian", "opencode", "pi"):
            with self.subTest(agent=kind):
                plain_env, captured_env = FakeContainer(), FakeContainer()
                with mock.patch.dict(os.environ, {"OPENCODE_API_KEY": "test-key"}):
                    self.run_trial(self.agent(kind, f"{kind}-plain"), plain_env)
                    self.run_trial(
                        self.agent(
                            kind,
                            f"{kind}-off",
                            capture_port=None,
                            capture_ca=None,
                        ),
                        captured_env,
                    )
                self.assertEqual(plain_env.exec_calls, captured_env.exec_calls)
                # The uploaded temp files live in a fresh directory each time;
                # what matters is that the same targets were written.
                self.assertEqual(
                    [upload["target"] for upload in plain_env.uploads],
                    [upload["target"] for upload in captured_env.uploads],
                )

    def capture_vars(self, node):
        extra = {
            "HTTPS_PROXY": f"http://{GATEWAY}:18080",
            "HTTP_PROXY": f"http://{GATEWAY}:18080",
            "NO_PROXY": "localhost,127.0.0.1",
            "SSL_CERT_FILE": capture.REMOTE_BUNDLE,
        }
        if node:
            extra["NODE_EXTRA_CA_CERTS"] = capture.REMOTE_CA
            extra["NODE_USE_ENV_PROXY"] = "1"
        return extra

    def capture_on(self, kind):
        plain_env, captured_env = FakeContainer(), FakeContainer()
        with mock.patch.dict(os.environ, {"OPENCODE_API_KEY": "test-key"}):
            plain = self.agent(kind, f"{kind}-plain")
            captured = self.agent(
                kind,
                f"{kind}-capture",
                capture_port=18080,
                capture_ca=str(self.ca),
            )
            self.run_trial(plain, plain_env)
            asyncio.run(captured.capture_setup(captured_env))
            self.run_trial(captured, captured_env)
        return plain_env, captured_env

    def assert_only_the_env_differs(self, plain_env, captured_env, node):
        plain_call = self.trial_call(plain_env)
        captured_call = self.trial_call(captured_env)
        self.assertEqual(plain_call["command"], captured_call["command"])
        self.assertEqual(
            captured_call["env"], {**plain_call["env"], **self.capture_vars(node)}
        )
        self.assertEqual(
            [call["cwd"] for call in plain_env.exec_calls],
            [call["cwd"] for call in captured_env.exec_calls[-len(plain_env.exec_calls):]],
        )

    def test_radian_sends_the_same_command_with_capture_on(self):
        plain_env, captured_env = self.capture_on("radian")
        self.assert_only_the_env_differs(plain_env, captured_env, node=False)

    def test_opencode_sends_the_same_command_with_capture_on(self):
        plain_env, captured_env = self.capture_on("opencode")
        self.assert_only_the_env_differs(plain_env, captured_env, node=True)

    def test_pi_sends_the_same_command_with_capture_on(self):
        plain_env, captured_env = self.capture_on("pi")
        self.assert_only_the_env_differs(plain_env, captured_env, node=True)

    def test_the_capture_writes_the_trials_capture_json(self):
        self.capture_on("pi")
        record = json.loads(
            (self.tmp / "pi-capture" / "capture.json").read_text(encoding="utf-8")
        )
        self.assertEqual(record["container_ip"], CONTAINER_IP)
        self.assertEqual(record["port"], 18080)

    def test_radian_system_inherits_the_capture(self):
        stub = types.ModuleType("radian_agent")
        stub.build_command = radian_command.build_command
        stub.RadianAgent = self.radian_agent.RadianAgent
        spec = importlib.util.spec_from_file_location(
            "radian_system_under_test", AB / "radian_system.py"
        )
        module = importlib.util.module_from_spec(spec)
        with mock.patch.dict(sys.modules, {"radian_agent": stub}):
            spec.loader.exec_module(module)
        self.assertTrue(issubclass(module.RadianSystem, capture.CaptureMixin))
        self.assertTrue(hasattr(module.RadianSystem, "capture_setup"))


class CheckerTest(unittest.TestCase):
    """`capture_check.py` on the two fixture arm files under `label/`."""

    FIELDS = (
        "model",
        "reasoning_effort",
        "thinking",
        "max_tokens",
        "stream_options",
    )

    def test_the_default_fields_are_the_ones_the_brief_names(self):
        self.assertEqual(capture_check.DEFAULT_EXPECT_EQUAL, self.FIELDS)

    def copy_label(self, tmp) -> Path:
        """A writable copy of `fixtures/capture/label`."""
        label = Path(tmp) / "label"
        shutil.copytree(FIXTURES / "label", label)
        return label

    def run_checker(self, label, *args):
        return subprocess.run(
            [sys.executable, str(AB / "capture_check.py"), str(label), *args],
            capture_output=True,
            text=True,
        )

    def test_equal_arms_exit_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            checked = self.run_checker(self.copy_label(tmp))
            self.assertEqual(checked.returncode, 0, checked.stdout + checked.stderr)
            self.assertIn("requests: 2", checked.stdout)
            self.assertIn("first request of each trial (1)", checked.stdout)
            self.assertIn("all requests (2)", checked.stdout)
            self.assertIn("172.18.0.2 -> a/2026-10-26__trial/agent", checked.stdout)
            self.assertIn("DIFFERENCES", checked.stdout)
            self.assertIn("none in:", checked.stdout)

    def test_a_differing_max_tokens_exits_one_and_names_the_field(self):
        with tempfile.TemporaryDirectory() as tmp:
            label = self.copy_label(tmp)
            arm = label / "capture" / "b.jsonl"
            arm.write_text(
                arm.read_text(encoding="utf-8").replace(
                    '"max_tokens": null', '"max_tokens": 8000'
                ),
                encoding="utf-8",
            )
            checked = self.run_checker(label)
            self.assertEqual(checked.returncode, 1, checked.stdout + checked.stderr)
            differences = checked.stdout.split("DIFFERENCES", 1)[1]
            self.assertIn("max_tokens", differences)
            self.assertIn("8000", differences)
            self.assertNotIn("model", differences)

    def test_expect_equal_selects_the_fields(self):
        with tempfile.TemporaryDirectory() as tmp:
            label = self.copy_label(tmp)
            arm = label / "capture" / "b.jsonl"
            arm.write_text(
                arm.read_text(encoding="utf-8").replace(
                    '"thinking": null', '"thinking": {"type": "enabled"}'
                ),
                encoding="utf-8",
            )
            self.assertEqual(self.run_checker(label).returncode, 1)
            selected = self.run_checker(label, "--expect-equal", "model,max_tokens")
            self.assertEqual(selected.returncode, 0, selected.stdout)

    def test_an_empty_arm_differs_from_an_arm_with_requests(self):
        with tempfile.TemporaryDirectory() as tmp:
            label = self.copy_label(tmp)
            # A proxy the arm ignored: the script created the file, nothing wrote it.
            (label / "capture" / "b.jsonl").write_text("", encoding="utf-8")
            checked = self.run_checker(label)
            self.assertEqual(checked.returncode, 1, checked.stdout + checked.stderr)
            differences = checked.stdout.split("DIFFERENCES", 1)[1]
            for field in self.FIELDS:
                with self.subTest(field=field):
                    self.assertIn(field, differences)
            self.assertIn("b: no captured request", differences)

    def test_every_arm_empty_is_not_a_clean_comparison(self):
        with tempfile.TemporaryDirectory() as tmp:
            label = self.copy_label(tmp)
            for path in (label / "capture").glob("*.jsonl"):
                path.write_text("", encoding="utf-8")
            checked = self.run_checker(label)
            self.assertEqual(checked.returncode, 1, checked.stdout + checked.stderr)
            self.assertNotIn("Traceback", checked.stderr)
            self.assertIn("every arm captured no request", checked.stdout)

    def test_a_label_without_capture_exits_two(self):
        with tempfile.TemporaryDirectory() as tmp:
            checked = self.run_checker(Path(tmp) / "nothing")
            self.assertEqual(checked.returncode, 2)
            self.assertIn("no capture", checked.stderr)


if __name__ == "__main__":
    unittest.main()
