"""Tests for the Terminal-Bench A/B scripts under ``benchmarks/terminal-bench/ab/``.

The shell and Python helpers are checked without Docker, Harbor or a model:
every shell script must parse, every Python file must compile, and no file may
carry a host name, a key or the old host-only import path. The one behaviour
test drives ``radian_system.py`` with a stub agent module, because the real
``radian_agent`` needs Harbor's package (the adapter tests that import Harbor
run under Harbor's own Python).

Run with the standard library alone:

    python3 benchmarks/terminal-bench/tests/test_ab_scripts.py
"""

import importlib.util
import json
import os
import shlex
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parent.parent
AB = ROOT / "ab"

# The test imports radian_command and the script under test; neither may leave a
# __pycache__ directory in the checked tree.
sys.dont_write_bytecode = True

sys.path.insert(0, str(ROOT))

import radian_command  # noqa: E402
from radian_command import RadianInvocation  # noqa: E402

# A host address, a key prefix, and the host-only import path the move removes.
FORBIDDEN = ("192.168.", "oc_sk", "sk-", "~/tb2/")


class ScriptSyntaxTest(unittest.TestCase):
    def test_every_shell_script_parses(self):
        scripts = sorted(AB.glob("*.sh"))
        self.assertTrue(scripts, f"no shell scripts under {AB}")
        for script in scripts:
            with self.subTest(script=script.name):
                result = subprocess.run(
                    ["bash", "-n", str(script)],
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_every_python_file_compiles(self):
        scripts = sorted(AB.glob("*.py"))
        self.assertTrue(scripts, f"no Python files under {AB}")
        for script in scripts:
            with self.subTest(script=script.name):
                # compile() writes no __pycache__, unlike py_compile.
                compile(script.read_text(encoding="utf-8"), str(script), "exec")

    def test_no_host_names_keys_or_old_import_paths(self):
        files = sorted(path for path in AB.rglob("*") if path.is_file())
        self.assertTrue(files, f"no files under {AB}")
        for path in files:
            # errors="replace" keeps a stray non-text file from hiding an
            # ASCII needle; the needles themselves are ASCII.
            text = path.read_text(encoding="utf-8", errors="replace")
            for needle in FORBIDDEN:
                with self.subTest(file=path.name, needle=needle):
                    self.assertNotIn(needle, text)


def load_radian_system(system_file):
    """Import ``radian_system.py`` with a stub ``radian_agent`` module.

    The real module needs Harbor; this test only exercises the command builder
    the module installs. The stub carries the two names the module imports.
    """
    stub = types.ModuleType("radian_agent")
    stub.build_command = radian_command.build_command
    stub.RadianAgent = object
    spec = importlib.util.spec_from_file_location(
        "radian_system_under_test", AB / "radian_system.py"
    )
    module = importlib.util.module_from_spec(spec)
    saved = os.environ.pop("RADIAN_AB_SYSTEM_FILE", None)
    try:
        if system_file is not None:
            os.environ["RADIAN_AB_SYSTEM_FILE"] = str(system_file)
        with mock.patch.dict(sys.modules, {"radian_agent": stub}):
            # Importing by file path would otherwise leave __pycache__ in ab/.
            saved_bytecode = sys.dont_write_bytecode
            sys.dont_write_bytecode = True
            try:
                spec.loader.exec_module(module)
            finally:
                sys.dont_write_bytecode = saved_bytecode
    finally:
        os.environ.pop("RADIAN_AB_SYSTEM_FILE", None)
        if saved is not None:
            os.environ["RADIAN_AB_SYSTEM_FILE"] = saved
    return module


class RadianSystemTest(unittest.TestCase):
    def invocation(self):
        return RadianInvocation(provider="opencode-go", model="deepseek-v4.1-flash")

    def test_system_file_appends_the_system_argument(self):
        text = "You are a terse assistant; answer with the patch only."
        with tempfile.NamedTemporaryFile(
            "w", suffix=".md", delete=False, encoding="utf-8"
        ) as handle:
            handle.write(text)
            path = Path(handle.name)
        try:
            module = load_radian_system(path)
            invocation = self.invocation()
            base = radian_command.build_command(
                "/tmp/radian", invocation, "do it", "/o", "/e"
            )
            command = module.build_command(
                "/tmp/radian", invocation, "do it", "/o", "/e"
            )
            system_arg = "--system " + shlex.quote(text)
            self.assertIn(system_arg, command)
            self.assertEqual(
                command,
                base.replace("--output jsonl", f"{system_arg} --output jsonl", 1),
            )
            # The agent imports the builder by name, so the module patches that
            # binding too; otherwise run() would still call the plain builder.
            self.assertIs(module.radian_agent.build_command, module.build_command)
        finally:
            path.unlink()

    def test_without_system_file_the_command_is_unchanged(self):
        module = load_radian_system(None)
        invocation = self.invocation()
        base = radian_command.build_command(
            "/tmp/radian", invocation, "do it", "/o", "/e"
        )
        command = module.build_command(
            "/tmp/radian", invocation, "do it", "/o", "/e"
        )
        self.assertEqual(command, base)


# A stub `harbor` on PATH records each arm it was asked to run and exits with
# HARBOR_STUB_RC. The real Harbor, Docker and model stay out of these tests.
STUB_HARBOR = """\
#!/bin/sh
out=
while [ $# -gt 0 ]; do
  case $1 in
    -o) out=$2; shift 2 ;;
    *) shift ;;
  esac
done
printf '%s %s\\n' "${out##*/}" "${RADIAN_AB_SYSTEM_FILE-<unset>}" >> "$HARBOR_STUB_LOG"
exit "${HARBOR_STUB_RC:-0}"
"""


def stub_host(root):
    """Lay out a HOME the shell scripts can run in; return its environment."""
    home = root / "home"
    credentials = home / ".radian" / "credentials"
    credentials.mkdir(parents=True)
    (credentials / "auth.json").write_text(
        json.dumps({"opencode-go": {"key": "test-key"}}), encoding="utf-8"
    )
    tb_bin = home / "tb2" / "bin"
    tb_bin.mkdir(parents=True)
    for name in ("radian", "radian-test"):
        (tb_bin / name).write_text("", encoding="utf-8")
        (tb_bin / f"{name}.sha256").write_text(
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n",
            encoding="utf-8",
        )
    stub_dir = root / "stub-bin"
    stub_dir.mkdir()
    harbor = stub_dir / "harbor"
    harbor.write_text(STUB_HARBOR, encoding="utf-8")
    harbor.chmod(0o755)
    env = dict(os.environ)
    env["HOME"] = str(home)
    env["PATH"] = f"{stub_dir}:{env.get('PATH', '')}"
    env["HARBOR_STUB_LOG"] = str(root / "harbor-stub.log")
    return home, env


def stub_records(root):
    """The arm name and role-file value each stub harbor invocation saw."""
    log = root / "harbor-stub.log"
    if not log.exists():
        return {}
    records = {}
    for line in log.read_text(encoding="utf-8").splitlines():
        arm, value = line.split(" ", 1)
        records[arm] = value
    return records


def run_script(script, args, env, cwd):
    """Run a script with file-backed output.

    ``ab-system.sh`` leaves a load sampler in the background; a pipe would keep
    ``subprocess.run`` waiting for that child's EOF, so both streams go to files.
    """
    streams = {}
    for name in ("stdout", "stderr"):
        streams[name] = open(Path(cwd) / f"script.{name}", "w", encoding="utf-8")
    try:
        result = subprocess.run(
            ["bash", str(script), *args],
            env=env,
            cwd=cwd,
            stdout=streams["stdout"],
            stderr=streams["stderr"],
        )
    finally:
        for stream in streams.values():
            stream.close()
    stderr = (Path(cwd) / "script.stderr").read_text(encoding="utf-8")
    return result.returncode, stderr


class RunScriptDoneLineTest(unittest.TestCase):
    """A failed Harbor arm must still write its `done` line."""

    def test_a_failed_arm_still_writes_its_done_line(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            home, env = stub_host(root)
            env["HARBOR_STUB_RC"] = "7"
            env["RADIAN_BENCH_BINARY"] = str(home / "tb2" / "bin" / "radian")
            rc, stderr = run_script(AB / "run.sh", ["rework"], env, tmp)
            self.assertEqual(rc, 0, stderr)
            done = home / "tb2" / "jobs" / "rework" / "done"
            self.assertTrue(done.exists(), f"no done file; stderr: {stderr}")
            lines = sorted(done.read_text(encoding="utf-8").splitlines())
            self.assertEqual(
                lines, ["opencode rc=7", "pi rc=7", "radian rc=7"]
            )


class AbSystemEnvironmentTest(unittest.TestCase):
    """The base arm must not inherit the caller's role file."""

    def test_base_arm_ignores_an_exported_system_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            home, env = stub_host(root)
            role = root / "role.md"
            role.write_text("You are terse.\n", encoding="utf-8")
            env["RADIAN_AB_SYSTEM_FILE"] = "/sentinel/from/the/caller"
            rc, stderr = run_script(
                AB / "ab-system.sh",
                ["rework", "radian-test", str(role)],
                env,
                tmp,
            )
            self.assertEqual(rc, 0, stderr)
            out = home / "tb2" / "jobs" / "rework"
            records = stub_records(root)
            self.assertEqual(records.get("base"), "<unset>", stderr)
            self.assertEqual(records.get("concise"), str(out / "role.txt"))
            lines = sorted((out / "done").read_text(encoding="utf-8").splitlines())
            self.assertEqual(lines, ["base rc=0", "concise rc=0"])


if __name__ == "__main__":
    unittest.main()
