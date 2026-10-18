"""Unit tests for `radian_agent.py` with stubbed Harbor modules.

The stubs mirror the Harbor surfaces the adapter touches (`BaseInstalledAgent`
construction/options, `model_connection`, `exec_as_agent`, `upload_file`,
`AgentContext`); they are not a Harbor simulator. Anything about trial
orchestration must be verified in a real Harbor smoke run, not here.
"""

import asyncio
import enum
import functools
import json
import sys
import tempfile
import types
import unittest
from pathlib import Path, PurePosixPath

BENCH_DIR = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(BENCH_DIR))


# ---------------------------------------------------------------------------
# Harbor stubs
# ---------------------------------------------------------------------------

class _Cli:
    def __init__(self, flag, *, format=None, fallback=None, sensitive=False):
        self.flag = flag


def _build_harbor_stubs():
    from pydantic import BaseModel, ConfigDict, Field

    harbor = types.ModuleType("harbor")
    agents = types.ModuleType("harbor.agents")
    installed = types.ModuleType("harbor.agents.installed")
    installed_base = types.ModuleType("harbor.agents.installed.base")
    auth_resolution = types.ModuleType("harbor.agents.agent_auth_resolution")
    capabilities_mod = types.ModuleType("harbor.agents.capabilities")
    options_mod = types.ModuleType("harbor.agents.options")
    environments = types.ModuleType("harbor.environments")
    environments_base = types.ModuleType("harbor.environments.base")
    models = types.ModuleType("harbor.models")
    models_agent = types.ModuleType("harbor.models.agent")
    context_mod = types.ModuleType("harbor.models.agent.context")
    api_format_mod = types.ModuleType("harbor.models.model_api_format")

    class InstalledAgentOptions(BaseModel):
        model_config = ConfigDict(extra="forbid")
        version: str | None = None
        prompt_template_path: str | None = None
        config: object | None = None

    class AgentCapabilities:
        def __init__(self, **kwargs):
            self.__dict__.update(kwargs)

    class ModelApiFormat(str, enum.Enum):
        OPENAI_CHAT_COMPLETIONS = "openai_chat_completions"
        OPENAI_RESPONSES = "openai_responses"
        ANTHROPIC_MESSAGES = "anthropic_messages"

    class ProviderAccess:
        def __init__(self, api_key_candidate_names=(), base_url_candidate_names=()):
            self.api_key_candidate_names = list(api_key_candidate_names)
            self.base_url_candidate_names = list(base_url_candidate_names)

    class ModelConnectionSpec:
        def __init__(self, agent_provider_access=None, supported_api_formats=(),
                     default_api_format_by_provider=None, **kwargs):
            self.agent_provider_access = agent_provider_access
            self.supported_api_formats = list(supported_api_formats)

    class MissingCredentialError(ValueError):
        pass

    def require_model_api_key(connection, agent_name):
        if not getattr(connection, "api_key", None):
            raise MissingCredentialError(f"{agent_name}: no model API key selected")

    class NonZeroAgentExitCodeError(RuntimeError):
        pass

    def with_prompt_template(fn):
        @functools.wraps(fn)
        async def wrapper(self, instruction, *args, **kwargs):
            return await fn(self, instruction, *args, **kwargs)
        return wrapper

    class BaseInstalledAgent:
        options_model = None

        def __init__(self, logs_dir, model_name=None, *, extra_env=None,
                     model_base_url=None, model_api_key_name=None, model_api_format=None,
                     skip_model_parsing=False, load_trajectory=None,
                     environment_logs_dir=None, **kwargs):
            self.logs_dir = Path(logs_dir)
            self.model_name = model_name
            self.environment_logs_dir = environment_logs_dir or PurePosixPath("/logs/agent")
            model = type(self).options_model or InstalledAgentOptions
            self.options = model.model_validate(kwargs)
            self._version = getattr(self.options, "version", None)
            self.model_connection = None

        def _parse_model_name(self, model_name):
            raise NotImplementedError

        async def exec_as_agent(self, environment, command, env=None, cwd=None,
                                timeout_sec=None):
            return await environment.exec(command, cwd=cwd, env=env,
                                          timeout_sec=timeout_sec)

    class ExecResult:
        def __init__(self, stdout="", stderr="", return_code=0):
            self.stdout = stdout
            self.stderr = stderr
            self.return_code = return_code

    class BaseEnvironment:
        pass

    class AgentContext(BaseModel):
        model_config = ConfigDict(extra="allow")
        metadata: dict | None = None

    installed_base.BaseInstalledAgent = BaseInstalledAgent
    installed_base.NonZeroAgentExitCodeError = NonZeroAgentExitCodeError
    installed_base.with_prompt_template = with_prompt_template
    auth_resolution.ModelConnectionSpec = ModelConnectionSpec
    auth_resolution.ProviderAccess = ProviderAccess
    auth_resolution.require_model_api_key = require_model_api_key
    capabilities_mod.AgentCapabilities = AgentCapabilities
    options_mod.Cli = _Cli
    options_mod.InstalledAgentOptions = InstalledAgentOptions
    environments_base.BaseEnvironment = BaseEnvironment
    environments_base.ExecResult = ExecResult
    context_mod.AgentContext = AgentContext
    api_format_mod.ModelApiFormat = ModelApiFormat

    modules = {
        "harbor": harbor,
        "harbor.agents": agents,
        "harbor.agents.installed": installed,
        "harbor.agents.installed.base": installed_base,
        "harbor.agents.agent_auth_resolution": auth_resolution,
        "harbor.agents.capabilities": capabilities_mod,
        "harbor.agents.options": options_mod,
        "harbor.environments": environments,
        "harbor.environments.base": environments_base,
        "harbor.models": models,
        "harbor.models.agent": models_agent,
        "harbor.models.agent.context": context_mod,
        "harbor.models.model_api_format": api_format_mod,
    }
    for name, module in modules.items():
        sys.modules[name] = module
    return modules


_build_harbor_stubs()

from harbor.agents.installed.base import NonZeroAgentExitCodeError  # noqa: E402
from harbor.environments.base import ExecResult  # noqa: E402
from harbor.models.agent.context import AgentContext  # noqa: E402
from harbor.models.model_api_format import ModelApiFormat  # noqa: E402

import radian_agent  # noqa: E402
from radian_agent import RadianAgent  # noqa: E402


class FakeEnvironment:
    """Records exec/upload calls; serves programmed results by substring match."""

    def __init__(self):
        self.exec_calls = []
        self.uploads = []
        self.stubs = []

    def on(self, substring, result):
        self.stubs.append((substring, result))
        return self

    async def exec(self, command, cwd=None, env=None, timeout_sec=None, user=None):
        self.exec_calls.append(
            {"command": command, "cwd": cwd, "env": dict(env or {}),
             "timeout_sec": timeout_sec}
        )
        for substring, result in self.stubs:
            if substring in command:
                return result
        return ExecResult(stdout="", stderr="", return_code=0)

    async def upload_file(self, source_path, target_path):
        content = Path(source_path).read_bytes()
        self.uploads.append({"source": source_path, "target": target_path,
                             "content": content})
        return None

    def last_exec(self):
        return self.exec_calls[-1]


class Connection:
    def __init__(self, *, provider="openai", api_key="sk-test-key",
                 resolved_base_url="https://proxy.example/v1",
                 user_supplied_base_url="https://proxy.example/v1",
                 model_api_format=None, env=None):
        self.provider = provider
        self.api_key = api_key
        self.resolved_base_url = resolved_base_url
        self.user_supplied_base_url = user_supplied_base_url
        self.model_api_format = model_api_format
        self.env = env or {}


def make_agent(tmp, **kwargs):
    agent = RadianAgent(logs_dir=Path(tmp) / "logs",
                        model_name=kwargs.pop("model_name", "openai/gpt-5"),
                        **kwargs)
    agent._parse_model_name = lambda name: tuple(name.split("/", 1))
    return agent


class InstallTest(unittest.TestCase):
    def test_install_uploads_binary_and_records_version(self):
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "radian"
            binary.write_bytes(b"fake-elf")
            env = FakeEnvironment().on("exec --version",
                                       ExecResult(stdout="radian 0.1.0\n"))
            agent = make_agent(tmp, binary_path=str(binary))
            asyncio.run(agent.install(env))
            self.assertEqual(
                [(u["source"], u["target"]) for u in env.uploads],
                [(str(binary), "/tmp/harbor-radian-bin/radian")],
            )
            record = json.loads((Path(tmp) / "logs" / "radian-binary.json").read_text())
            self.assertEqual(record["version"], "radian 0.1.0")
            self.assertEqual(len(record["sha256"]), 64)
            self.assertEqual(agent.version(), "radian 0.1.0")

    def test_install_refuses_sha256_mismatch(self):
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "radian"
            binary.write_bytes(b"fake-elf")
            agent = make_agent(tmp, binary_path=str(binary),
                               binary_sha256="0" * 64)
            with self.assertRaises(ValueError):
                asyncio.run(agent.install(FakeEnvironment()))

    def test_install_requires_a_binary(self):
        with tempfile.TemporaryDirectory() as tmp:
            agent = make_agent(tmp)
            with self.assertRaises(ValueError):
                asyncio.run(agent.install(FakeEnvironment()))

    def test_install_rejects_version_kwarg(self):
        with tempfile.TemporaryDirectory() as tmp:
            agent = make_agent(tmp, binary_path="/tmp/radian", version="1.2.3")
            with self.assertRaises(ValueError):
                asyncio.run(agent.install(FakeEnvironment()))


class RunTest(unittest.TestCase):
    def run_trial(self, tmp, connection, **kwargs):
        binary = Path(tmp) / "radian"
        binary.write_bytes(b"fake-elf")
        agent_kwargs = {"binary_path": str(binary)}
        agent_kwargs.update(kwargs)
        agent = make_agent(tmp, **agent_kwargs)
        agent.model_connection = connection
        agent._radian_version = "radian 0.1.0"
        env = FakeEnvironment()
        env.on("pwd", ExecResult(stdout="/work/task\n"))
        context = AgentContext()
        return agent, env, context

    def test_openai_trial_is_unattended_and_keeps_the_secret(self):
        with tempfile.TemporaryDirectory() as tmp:
            connection = Connection(
                model_api_format=ModelApiFormat.OPENAI_CHAT_COMPLETIONS)
            agent, env, context = self.run_trial(tmp, connection)
            asyncio.run(agent.run("Fix the failing test.", env, context))
            main = env.last_exec()
            command = main["command"]
            for flag in ("--yes", "--no-trust", "--offline-catalog",
                         "--profile", "full", "--provider", "openai-compatible",
                         "--model", "gpt-5", "--workspace", "/work/task"):
                self.assertIn(flag, command)
            self.assertIn("--base-url https://proxy.example/v1", command)
            self.assertNotIn("sk-test-key", command)
            # The credential travels only as the named variable.
            self.assertEqual(main["env"]["RADIAN_BENCH_API_KEY"], "sk-test-key")
            self.assertEqual(main["env"]["RADIAN_HOME"], "/tmp/harbor-radian-home")
            self.assertEqual(main["cwd"], "/work/task")
            auth = next(u for u in env.uploads if u["target"].endswith("auth.json"))
            entry = json.loads(auth["content"])["openai-compatible"]
            self.assertEqual(entry["key"], "$RADIAN_BENCH_API_KEY")
            self.assertEqual(entry["env"]["RADIAN_BASE_URL"], "https://proxy.example/v1")
            self.assertNotIn(b"sk-test-key", auth["content"])
            self.assertEqual(context.metadata["agent"], "radian")
            self.assertEqual(context.metadata["return_code"], 0)

    def test_anthropic_trial_has_no_base_url(self):
        with tempfile.TemporaryDirectory() as tmp:
            connection = Connection(
                provider="anthropic",
                resolved_base_url="https://api.anthropic.com",
                user_supplied_base_url=None,
                model_api_format=ModelApiFormat.ANTHROPIC_MESSAGES)
            agent, env, context = self.run_trial(
                tmp, connection, model_name="anthropic/claude-x")
            asyncio.run(agent.run("Do it.", env, context))
            command = env.last_exec()["command"]
            self.assertIn("--provider anthropic", command)
            self.assertNotIn("--base-url", command)

    def test_anthropic_with_custom_endpoint_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            connection = Connection(
                provider="anthropic",
                user_supplied_base_url="https://proxy.example/anthropic",
                resolved_base_url="https://proxy.example/anthropic",
                model_api_format=ModelApiFormat.ANTHROPIC_MESSAGES)
            agent, env, context = self.run_trial(
                tmp, connection, model_name="anthropic/claude-x")
            with self.assertRaises(ValueError):
                asyncio.run(agent.run("Do it.", env, context))

    def test_nonzero_exit_raises_without_leaking_the_secret(self):
        with tempfile.TemporaryDirectory() as tmp:
            connection = Connection(
                model_api_format=ModelApiFormat.OPENAI_CHAT_COMPLETIONS)
            agent, env, context = self.run_trial(tmp, connection)
            env.on("radian-stdout",
                   ExecResult(stdout="partial", stderr="boom", return_code=3))
            with self.assertRaises(NonZeroAgentExitCodeError) as raised:
                asyncio.run(agent.run("Do it.", env, context))
            message = str(raised.exception)
            self.assertIn("exit 3", message)
            self.assertNotIn("sk-test-key", message)

    def test_missing_api_key_fails_fast(self):
        with tempfile.TemporaryDirectory() as tmp:
            connection = Connection(
                api_key=None,
                model_api_format=ModelApiFormat.OPENAI_CHAT_COMPLETIONS)
            agent, env, context = self.run_trial(tmp, connection)
            with self.assertRaises(ValueError):
                asyncio.run(agent.run("Do it.", env, context))


if __name__ == "__main__":
    unittest.main()
