"""Tests for the Terminal-Bench A/B scripts under ``benchmarks/terminal-bench/ab/``.

The shell and Python helpers are checked without Docker, Harbor or a model:
every shell script must parse, every Python file must compile, no file may
carry a host name, a key or the old host-only import path, and the task-set
helper and the three A/B scripts are driven with a stub ``harbor`` on PATH.
The one Python behaviour test drives ``radian_system.py`` with a stub agent
module, because the real ``radian_agent`` needs Harbor's package (the adapter
tests that import Harbor run under Harbor's own Python).

Run with the standard library alone:

    python3 benchmarks/terminal-bench/tests/test_ab_scripts.py
"""

import importlib.util
import json
import os
import re
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
TASKS_DIR = AB / "tasks"

# The ten tasks the scripts hardcoded before task sets existed, in that order.
OLD_TASKS = [
    "llm-inference-batching-scheduler",
    "largest-eigenval",
    "write-compressor",
    "reshard-c4-data",
    "torch-tensor-parallelism",
    "password-recovery",
    "break-filter-js-from-html",
    "portfolio-optimization",
    "gpt2-codegolf",
    "merge-diff-arc-agi-task",
]

CALIBRATE_TASKS = [
    "largest-eigenval",
    "llm-inference-batching-scheduler",
    "merge-diff-arc-agi-task",
    "torch-tensor-parallelism",
    "write-compressor",
    "kv-store-grpc",
    "custom-memory-heap-crash",
    "db-wal-recovery",
    "regex-log",
    "sqlite-db-truncate",
    "build-cython-ext",
    "query-optimize",
    "adaptive-rejection-sampler",
    "git-multibranch",
    "extract-elf",
]

TASK_NAME = re.compile(r"^[a-z0-9][a-z0-9-]*$")

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


def parse_task_file(path):
    """The task names in a task-set file, comments and blanks removed."""
    names = []
    for line in path.read_text(encoding="utf-8").splitlines():
        name = line.split("#", 1)[0].strip()
        if name:
            names.append(name)
    return names


class TaskSetFilesTest(unittest.TestCase):
    def test_every_task_file_parses(self):
        files = sorted(path for path in TASKS_DIR.rglob("*") if path.is_file())
        self.assertTrue(files, f"no task-set files under {TASKS_DIR}")
        for path in files:
            with self.subTest(task_set=path.name):
                names = parse_task_file(path)
                self.assertTrue(names, f"{path} names no tasks")
                self.assertEqual(
                    len(names), len(set(names)), f"{path} repeats a task name"
                )
                for name in names:
                    self.assertRegex(name, TASK_NAME)

    def test_ab3_is_the_old_hardcoded_list(self):
        self.assertEqual(parse_task_file(TASKS_DIR / "ab3.txt"), OLD_TASKS)

    def test_calibrate_file_lists_the_calibration_tasks(self):
        self.assertEqual(
            parse_task_file(TASKS_DIR / "calibrate-2026-10.txt"), CALIBRATE_TASKS
        )


def run_task_set(value, tasks_dir=TASKS_DIR):
    """Run the shared task-set helper; return its CompletedProcess."""
    return subprocess.run(
        [sys.executable, str(AB / "task_set.py"), "resolve", str(value), str(tasks_dir)],
        capture_output=True,
        text=True,
    )


class TaskSetHelperTest(unittest.TestCase):
    def assert_one_line(self, text):
        self.assertEqual(len(text.splitlines()), 1, text)

    def test_a_bare_name_resolves_next_to_the_scripts(self):
        result = run_task_set("ab3")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), OLD_TASKS)

    def test_a_path_resolves(self):
        result = run_task_set(TASKS_DIR / "ab3.txt")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), OLD_TASKS)

    def test_an_unknown_name_is_refused(self):
        result = run_task_set("no-such-set")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no-such-set", result.stderr)
        self.assert_one_line(result.stderr)

    def test_an_empty_file_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "empty.txt"
            path.write_text("# only a comment\n\n", encoding="utf-8")
            result = run_task_set(path)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("empty", result.stderr)
            self.assert_one_line(result.stderr)

    def test_a_duplicate_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "duplicate.txt"
            path.write_text("largest-eigenval\nextract-elf\nlargest-eigenval\n")
            result = run_task_set(path)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("duplicate", result.stderr)
            self.assertIn("largest-eigenval", result.stderr)
            self.assert_one_line(result.stderr)

    def test_a_bad_task_name_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "bad-name.txt"
            path.write_text("Not_A_Task\n")
            result = run_task_set(path)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Not_A_Task", result.stderr)
            self.assert_one_line(result.stderr)


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


# A stub `harbor` on PATH records each arm it was asked to run: the `-o` arm
# label, the role file in the environment, and every remaining argument. It
# exits with HARBOR_STUB_RC. The real Harbor, Docker and model stay out of
# these tests.
STUB_HARBOR = """\
#!/bin/sh
out=
args=
while [ $# -gt 0 ]; do
  case $1 in
    -o) out=$2; shift 2 ;;
    *) args="$args $1"; shift ;;
  esac
done
printf '%s\\t%s\\t%s\\n' "${out##*/}" "${RADIAN_AB_SYSTEM_FILE-<unset>}" "$args" >> "$HARBOR_STUB_LOG"
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
    # Test-only: ab.sh and ab-system.sh skip the load sampler, which reads
    # /proc/loadavg (absent on macOS).
    env["AB_TEST_SKIP_LOADAVG"] = "1"
    return home, env


def stub_invocations(root):
    """Every stub harbor call: its arm label, role-file value and arguments."""
    log = root / "harbor-stub.log"
    if not log.exists():
        return []
    calls = []
    for line in log.read_text(encoding="utf-8").splitlines():
        arm, system, args = line.split("\t", 2)
        calls.append({"arm": arm, "system": system, "args": shlex.split(args)})
    return calls


def flag_values(call, flag):
    """The values a stub call passed for a repeated harbor flag."""
    args = call["args"]
    return [args[index + 1] for index, arg in enumerate(args) if arg == flag]


def run_script(script, args, env, cwd):
    """Run a script with file-backed output.

    A pipe would keep ``subprocess.run`` waiting for a background child that
    still holds the write end, so both streams go to files.
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
            records = {call["arm"]: call for call in stub_invocations(root)}
            self.assertEqual(records["base"]["system"], "<unset>", stderr)
            self.assertEqual(records["concise"]["system"], str(out / "role.txt"))
            lines = sorted((out / "done").read_text(encoding="utf-8").splitlines())
            self.assertEqual(lines, ["base rc=0", "concise rc=0"])


class AbTaskSelectionTest(unittest.TestCase):
    """AB_TASKS selects the task list and AB_K the attempts for every script."""

    def run_ok(self, script, args, env, cwd):
        rc, stderr = run_script(script, args, env, cwd)
        self.assertEqual(rc, 0, stderr)
        return stub_invocations(Path(cwd))

    def assert_used(self, calls, tasks, k):
        self.assertTrue(calls)
        for call in calls:
            self.assertEqual(flag_values(call, "-i"), tasks)
            self.assertEqual(flag_values(call, "-k"), [str(k)])

    def write_set(self, root, name, tasks):
        path = root / name
        path.write_text("\n".join(tasks) + "\n", encoding="utf-8")
        return path

    def test_ab_sh_defaults_to_ab3_and_k_two(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            home, env = stub_host(root)
            calls = self.run_ok(
                AB / "ab.sh", ["defaults", "radian-test"], env, tmp
            )
            out = home / "tb2" / "jobs" / "defaults"
            self.assert_used(calls, OLD_TASKS, 2)
            self.assertEqual(
                sorted(call["arm"] for call in calls),
                ["opencode", "pi", "radian-test"],
            )
            self.assertEqual(
                (out / "tasks.txt").read_text(encoding="utf-8").splitlines(),
                OLD_TASKS,
            )
            self.assertEqual(
                sorted((out / "done").read_text(encoding="utf-8").splitlines()),
                [
                    "opencode rc=0",
                    "pi rc=0",
                    "radian-test rc=0",
                ],
            )

    def test_ab_sh_reads_ab_k_and_a_path_set(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            home, env = stub_host(root)
            tasks = ["largest-eigenval", "write-compressor"]
            env["AB_TASKS"] = str(self.write_set(root, "subset.txt", tasks))
            env["AB_K"] = "5"
            calls = self.run_ok(AB / "ab.sh", ["chosen", "radian"], env, tmp)
            out = home / "tb2" / "jobs" / "chosen"
            self.assert_used(calls, tasks, 5)
            self.assertEqual(
                (out / "tasks.txt").read_text(encoding="utf-8").splitlines(),
                tasks,
            )

    def test_ab_system_reads_a_named_set_and_defaults_to_k_three(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            home, env = stub_host(root)
            role = root / "role.md"
            role.write_text("You are terse.\n", encoding="utf-8")
            env["AB_TASKS"] = "calibrate-2026-10"
            calls = self.run_ok(
                AB / "ab-system.sh", ["calibrate", "radian-test", str(role)], env, tmp
            )
            out = home / "tb2" / "jobs" / "calibrate"
            self.assert_used(calls, CALIBRATE_TASKS, 3)
            self.assertEqual(
                sorted(call["arm"] for call in calls), ["base", "concise"]
            )
            self.assertEqual(
                (out / "tasks.txt").read_text(encoding="utf-8").splitlines(),
                CALIBRATE_TASKS,
            )
            self.assertEqual(
                sorted((out / "done").read_text(encoding="utf-8").splitlines()),
                ["base rc=0", "concise rc=0"],
            )

    def test_ab_pi_reads_ab_k_and_writes_the_list(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            home, env = stub_host(root)
            tasks = ["extract-elf", "git-multibranch"]
            env["AB_TASKS"] = str(self.write_set(root, "subset.txt", tasks))
            env["AB_K"] = "4"
            calls = self.run_ok(AB / "ab-pi.sh", ["pirelaunch"], env, tmp)
            out = home / "tb2" / "jobs" / "pirelaunch"
            self.assert_used(calls, tasks, 4)
            self.assertEqual([call["arm"] for call in calls], ["pi"])
            self.assertEqual(
                (out / "tasks.txt").read_text(encoding="utf-8").splitlines(),
                tasks,
            )
            self.assertEqual(
                (out / "done").read_text(encoding="utf-8").splitlines(),
                ["pi rc=0"],
            )


class AbPiSavedTaskSetTest(unittest.TestCase):
    """ab-pi.sh re-runs a label's own list; AB_TASKS may not replace it."""

    def saved_label(self, root, tasks):
        out = root / "home" / "tb2" / "jobs" / "saved"
        out.mkdir(parents=True)
        (out / "tasks.txt").write_text("\n".join(tasks) + "\n", encoding="utf-8")
        return out

    def test_it_uses_the_saved_file_when_ab_tasks_is_unset(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, env = stub_host(root)
            tasks = ["largest-eigenval", "write-compressor"]
            self.saved_label(root, tasks)
            rc, stderr = run_script(AB / "ab-pi.sh", ["saved"], env, tmp)
            self.assertEqual(rc, 0, stderr)
            calls = stub_invocations(root)
            self.assertEqual(flag_values(calls[0], "-i"), tasks)

    def test_it_accepts_ab_tasks_that_names_the_same_list(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, env = stub_host(root)
            self.saved_label(root, OLD_TASKS)
            env["AB_TASKS"] = "ab3"
            rc, stderr = run_script(AB / "ab-pi.sh", ["saved"], env, tmp)
            self.assertEqual(rc, 0, stderr)
            calls = stub_invocations(root)
            self.assertEqual(flag_values(calls[0], "-i"), OLD_TASKS)

    def test_it_refuses_a_different_ab_tasks(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, env = stub_host(root)
            self.saved_label(root, ["largest-eigenval"])
            env["AB_TASKS"] = "ab3"
            rc, stderr = run_script(AB / "ab-pi.sh", ["saved"], env, tmp)
            self.assertNotEqual(rc, 0)
            self.assertEqual(len(stderr.splitlines()), 1, stderr)
            self.assertIn("different", stderr)
            self.assertEqual(stub_invocations(root), [])


class BadTaskSetStopsTheScriptTest(unittest.TestCase):
    """A bad set stops ab.sh before Harbor; one stderr line names it."""

    def run_with(self, value):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, env = stub_host(root)
            env["AB_TASKS"] = value
            rc, stderr = run_script(AB / "ab.sh", ["bad", "radian"], env, tmp)
            return rc, stderr, stub_invocations(root)

    def test_an_unknown_name_stops_the_script(self):
        rc, stderr, calls = self.run_with("no-such-set")
        self.assertNotEqual(rc, 0)
        self.assertIn("no-such-set", stderr)
        self.assertEqual(len(stderr.splitlines()), 1, stderr)
        self.assertEqual(calls, [])

    def test_an_empty_file_stops_the_script(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "empty.txt"
            path.write_text("# no tasks\n", encoding="utf-8")
            rc, stderr, calls = self.run_with(str(path))
        self.assertNotEqual(rc, 0)
        self.assertIn("empty", stderr)
        self.assertEqual(len(stderr.splitlines()), 1, stderr)
        self.assertEqual(calls, [])

    def test_a_duplicate_stops_the_script(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "duplicate.txt"
            path.write_text("extract-elf\nextract-elf\n", encoding="utf-8")
            rc, stderr, calls = self.run_with(str(path))
        self.assertNotEqual(rc, 0)
        self.assertIn("extract-elf", stderr)
        self.assertEqual(len(stderr.splitlines()), 1, stderr)
        self.assertEqual(calls, [])

    def test_ab_pi_refuses_an_unknown_name_without_a_saved_list(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, env = stub_host(root)
            env["AB_TASKS"] = "no-such-set"
            rc, stderr = run_script(AB / "ab-pi.sh", ["fresh"], env, tmp)
            self.assertNotEqual(rc, 0)
            self.assertIn("no-such-set", stderr)
            self.assertEqual(len(stderr.splitlines()), 1, stderr)
            self.assertEqual(stub_invocations(root), [])


if __name__ == "__main__":
    unittest.main()
