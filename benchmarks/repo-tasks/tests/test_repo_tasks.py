#!/usr/bin/env python3
"""Tests for the fixed repository task set's runner, checks, review prompt and
report. No test calls a model: the agents and cargo are stub scripts, and the
worktrees come from a temporary repository.

    python3 benchmarks/repo-tasks/tests/test_repo_tasks.py
    python3 benchmarks/repo-tasks/report.py --self-test     # the same tests

Stdlib only.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
BENCHMARKS = ROOT.parent
REPO = BENCHMARKS.parent
sys.path.insert(0, str(BENCHMARKS))
sys.path.insert(0, str(ROOT))

import journal_metrics  # noqa: E402
import lib  # noqa: E402
import prompt  # noqa: E402
import report  # noqa: E402

SCRATCH = REPO / ".radian" / "repo-tasks-tests"
TASK_FIELDS = ("id", "slug", "base", "outcome", "crates")


def scratch() -> Path:
    SCRATCH.mkdir(parents=True, exist_ok=True)
    return Path(tempfile.mkdtemp(prefix="t-", dir=SCRATCH))


def journal(records) -> str:
    return "".join(json.dumps(record) + "\n" for record in records)


def session_journal(input_tokens=100, output=10, fee_micros=1_000) -> str:
    return journal(
        [
            {
                "event": "MessageAppended",
                "data": {"message": {"assistant": {"text": "hi"}}},
                "ts": "2026-10-01T00:00:00Z",
            },
            {
                "event": "ModelUsageRecorded",
                "data": {
                    "usage": {
                        "input": input_tokens,
                        "output": output,
                        "input_includes_cache": True,
                    },
                    "fee": {"micros": fee_micros},
                },
                "ts": "2026-10-01T00:00:02Z",
            },
            {
                "event": "TurnFinished",
                "data": {"outcome": "completed"},
                "ts": "2026-10-01T00:00:03Z",
            },
        ]
    )


def write(path: Path, text: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    return path


def task_file(directory: Path, name: str, base: str, crates="crate-a, crate-b") -> Path:
    return write(
        directory / f"{name}.md",
        textwrap.dedent(
            f"""\
            ---
            id: {name.split("-")[0]}
            slug: demo
            base: {base}
            outcome: pass
            crates: {crates}
            ---
            ## Brief

            ## Problem
            Do the demo change.
            """
        ),
    )


def arm_file(directory: Path, name: str, agent: str = "radian") -> Path:
    return write(
        directory / f"{name}.md",
        f"agent = {agent}\nprovider = opencode-go\nmodel = a-model\nextra = --profile full\n",
    )


def git(repo: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True, check=True
    ).stdout.strip()


def make_repo(root: Path) -> Path:
    repo = root / "repo"
    repo.mkdir()
    git(repo, "init", "-q")
    git(repo, "config", "user.email", "test@example.com")
    git(repo, "config", "user.name", "test")
    write(repo / "README.md", "base\n")
    git(repo, "add", "README.md")
    git(repo, "commit", "-q", "-m", "base")
    return repo


def radian_stub(directory: Path, body: str) -> Path:
    """A radian stand-in that writes a session journal and exits with 0."""
    path = write(
        directory / "radian-stub.py",
        "#!/usr/bin/env python3\n"
        "import json, pathlib, sys\n"
        "args = sys.argv[1:]\n"
        "out = None\n"
        "for i, arg in enumerate(args):\n"
        "    if arg == '--session-dir':\n"
        "        out = args[i + 1]\n"
        "pathlib.Path(out).mkdir(parents=True, exist_ok=True)\n"
        + textwrap.dedent(body),
    )
    path.chmod(0o755)
    return path


STUB_RADIAN = (
    "text = "
    + repr(
        journal(
            [
                {
                    "event": "MessageAppended",
                    "data": {"message": {"assistant": {"text": "hi"}}},
                    "ts": "2026-10-01T00:00:00Z",
                },
                {
                    "event": "ModelUsageRecorded",
                    "data": {
                        "usage": {
                            "input": 100,
                            "output": 10,
                            "input_includes_cache": True,
                        },
                        "fee": {"micros": 1_000},
                    },
                    "ts": "2026-10-01T00:00:02Z",
                },
                {
                    "event": "TurnFinished",
                    "data": {"outcome": "completed"},
                    "ts": "2026-10-01T00:00:03Z",
                },
            ]
        )
    )
    + '\n(pathlib.Path(out) / "stub.jsonl").write_text(text)\n'
)

STUB_TIMEOUT = """\
import time
time.sleep(30)
"""

CARGO_STUB = """\
#!/bin/sh
echo "$@"
crate=""
previous=""
for arg in "$@"; do
  if [ "$previous" = "-p" ]; then crate="$arg"; fi
  previous="$arg"
done
if [ "$1" = "nextest" ] && [ "$crate" = "crate-b" ]; then exit 1; fi
exit 0
"""


class ScratchCase(unittest.TestCase):
    def setUp(self):
        self.root = scratch()
        self.addCleanup(shutil.rmtree, self.root, True)

    def env(self, **values):
        env = dict(os.environ)
        env.pop("REPO_TASKS_TASKS_DIR", None)
        env.pop("REPO_TASKS_ARMS_DIR", None)
        env.update({key: str(value) for key, value in values.items()})
        return env


class TaskSetTest(ScratchCase):
    def test_the_four_tasks_carry_every_field_and_the_brief(self):
        names = lib.task_names()
        self.assertEqual(len(names), 4)
        bases = set()
        crates = set()
        for name in names:
            with self.subTest(task=name):
                task = lib.parse_task(lib.TASKS_DIR / f"{name}.md")
                for field in TASK_FIELDS:
                    self.assertIn(field, task)
                self.assertRegex(str(task["base"]), r"^[0-9a-f]{40}$")
                self.assertIn(task["outcome"], ("pass", "fail"))
                self.assertTrue(task["crates"])
                self.assertIn("## Problem", task["brief"])
                self.assertNotIn("## Rework", task["brief"])
                # No credential may live in a task file.
                self.assertNotRegex(task["brief"].lower(), r"sk-[a-z0-9]|api[_-]?key\s*[:=]")
                bases.add(str(task["base"]))
                crates.update(task["crates"])
        self.assertGreaterEqual(len(bases), 3)
        self.assertGreaterEqual(len(crates), 4)

    def test_unknown_task_and_arm_list_the_valid_names(self):
        with self.assertRaises(lib.LookupError) as task_error:
            lib.find_task("nope")
        self.assertIn("unknown task `nope`", str(task_error.exception))
        self.assertIn("TK0226-cli-manual-exit-4-truncation-rule", str(task_error.exception))
        with self.assertRaises(lib.LookupError) as arm_error:
            lib.find_arm("nope")
        self.assertIn("unknown arm `nope`", str(arm_error.exception))
        for name in ("full", "pi-like", "reviewer"):
            self.assertIn(name, str(arm_error.exception))

    def test_the_cli_exits_nonzero_and_names_the_valid_ones(self):
        result = subprocess.run(
            [sys.executable, str(ROOT / "lib.py"), "arm", "nope", "agent"],
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("full", result.stderr)


class RunScriptTest(ScratchCase):
    def prepare(self, stub=STUB_RADIAN):
        repo = make_repo(self.root)
        base = git(repo, "rev-parse", "HEAD")
        tasks = self.root / "tasks"
        arms = self.root / "arms"
        task_file(tasks, "T1-demo", base)
        arm_file(arms, "full")
        return repo, base, tasks, arms, radian_stub(self.root, stub)

    def run_script(self, repo, tasks, arms, stub, *args, timeout="30"):
        return subprocess.run(
            ["bash", str(ROOT / "run.sh"), *args],
            cwd=repo,
            capture_output=True,
            text=True,
            env=self.env(
                REPO_TASKS_TASKS_DIR=tasks,
                REPO_TASKS_ARMS_DIR=arms,
                REPO_TASKS_SOURCE=repo,
                REPO_TASKS_RUNS=self.root / "runs",
                REPO_TASKS_CLONE=self.root / "clone",
                REPO_TASKS_RADIAN_BIN=stub,
                REPO_TASKS_TIMEOUT=timeout,
            ),
        )

    def test_the_callers_repository_config_is_byte_identical(self):
        repo, _, tasks, arms, stub = self.prepare()
        before = (repo / ".git" / "config").read_bytes()
        before_worktrees = git(repo, "worktree", "list", "--porcelain")
        result = self.run_script(repo, tasks, arms, stub, "full", "T1", "1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((repo / ".git" / "config").read_bytes(), before)
        self.assertEqual(git(repo, "worktree", "list", "--porcelain"), before_worktrees)
        # The worktree and the session are in the separate clone/run root.
        tree = self.root / "runs" / "trees" / "full-T1-1"
        self.assertTrue((tree / "README.md").is_file())
        self.assertTrue((self.root / "clone" / ".git").is_dir())
        self.assertTrue((self.root / "runs" / "full" / "T1" / "1" / "change.diff").is_file())
        data = json.loads((self.root / "runs" / "full" / "T1" / "1" / "run.json").read_text())
        self.assertFalse(data["timed_out"])
        self.assertEqual(data["status"], "ok")
        self.assertTrue(data["journal"])

    def test_a_task_with_two_crates_runs_both_checks(self):
        repo, _, tasks, arms, stub = self.prepare()
        result = self.run_script(repo, tasks, arms, stub, "full", "T1", "1")
        self.assertEqual(result.returncode, 0, result.stderr)
        cargo = write(self.root / "cargo-stub.sh", CARGO_STUB)
        cargo.chmod(0o755)
        checked = subprocess.run(
            ["bash", str(ROOT / "check.sh"), "full", "T1", "1"],
            cwd=repo,
            capture_output=True,
            text=True,
            env=self.env(
                REPO_TASKS_TASKS_DIR=tasks,
                REPO_TASKS_ARMS_DIR=arms,
                REPO_TASKS_RUNS=self.root / "runs",
                REPO_TASKS_CARGO=cargo,
            ),
        )
        # crate-b fails nextest; both crates were still checked.
        self.assertEqual(checked.returncode, 1)
        log = (self.root / "runs" / "full" / "T1" / "1" / "checks.log").read_text()
        for crate in ("crate-a", "crate-b"):
            self.assertIn(f"clippy {crate}", log.replace("-p ", ""))
            self.assertIn(f"nextest run {crate}", log.replace("-p ", ""))
        data = json.loads((self.root / "runs" / "full" / "T1" / "1" / "checks.json").read_text())
        self.assertFalse(data["checks_ok"])
        self.assertTrue(data["clippy_ok"]["crate-a"])
        self.assertFalse(data["tests_ok"]["crate-b"])

    def test_a_wall_timeout_is_recorded_as_timeout(self):
        repo, _, tasks, arms, stub = self.prepare(STUB_TIMEOUT)
        result = self.run_script(repo, tasks, arms, stub, "full", "T1", "1", timeout="1")
        self.assertIn(result.returncode, (0, 1))
        data = json.loads((self.root / "runs" / "full" / "T1" / "1" / "run.json").read_text())
        self.assertTrue(data["timed_out"])
        self.assertEqual(data["status"], "timeout")
        # The report keeps the run.
        built = report.build(self.root / "runs")
        self.assertEqual(built["runs"][0]["result"], "timeout")
        self.assertIn("timeout=1", report.format_report(built))

    def test_unknown_arm_and_task_exit_nonzero_and_name_the_valid_ones(self):
        repo, _, tasks, arms, stub = self.prepare()
        bad_arm = self.run_script(repo, tasks, arms, stub, "nope", "T1", "1")
        self.assertEqual(bad_arm.returncode, 2)
        self.assertIn("valid arms: full", bad_arm.stderr)
        bad_task = self.run_script(repo, tasks, arms, stub, "full", "nope", "1")
        self.assertEqual(bad_task.returncode, 2)
        self.assertIn("valid tasks: T1-demo", bad_task.stderr)


class CheckScriptTest(ScratchCase):
    def make_run_tree(self):
        tasks = self.root / "tasks"
        task_file(tasks, "T1-demo", "0" * 40, crates="crate-a, crate-b")
        run = self.root / "runs" / "full" / "T1" / "1"
        tree = self.root / "runs" / "trees" / "full-T1-1"
        run.mkdir(parents=True)
        tree.mkdir(parents=True)
        return tasks, run, tree

    def test_a_passing_stub_passes_and_runs_every_crate_once(self):
        tasks, run, _ = self.make_run_tree()
        cargo = write(self.root / "cargo-stub.sh", "#!/bin/sh\necho \"$@\"\nexit 0\n")
        cargo.chmod(0o755)
        result = subprocess.run(
            ["bash", str(ROOT / "check.sh"), "full", "T1", "1"],
            capture_output=True,
            text=True,
            env=self.env(
                REPO_TASKS_TASKS_DIR=tasks,
                REPO_TASKS_RUNS=self.root / "runs",
                REPO_TASKS_CARGO=cargo,
            ),
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        data = json.loads((run / "checks.json").read_text())
        self.assertTrue(data["checks_ok"])
        self.assertEqual(data["crates"], ["crate-a", "crate-b"])


class ReportTest(ScratchCase):
    def make_run(self, arm, task, k, input_tokens=100, timed_out=False):
        out = self.root / "runs" / arm / task / str(k)
        journal_path = write(out / "session" / "s.jsonl", session_journal(input_tokens))
        write(
            out / "run.json",
            json.dumps(
                {
                    "arm": arm,
                    "task": task,
                    "k": k,
                    "base": "0" * 40,
                    "agent": "radian",
                    "provider": "p",
                    "model": "m",
                    "status": "timeout" if timed_out else "ok",
                    "timed_out": timed_out,
                    "exit_code": 124 if timed_out else 0,
                    "wall_seconds": 2.0,
                    "journal": str(journal_path),
                    "session_dir": str(out / "session"),
                }
            ),
        )
        write(out / "checks.json", json.dumps({"checks_ok": not timed_out}))
        return out

    def test_every_number_comes_from_journal_metrics(self):
        out = self.make_run("full", "T1", 1)
        built = report.build(self.root / "runs")
        expected = journal_metrics.build_report([out / "session" / "s.jsonl"])["totals"]
        row = built["runs"][0]["metrics"]
        for key in (
            "calls",
            "input_uncached",
            "input_cache_read",
            "input_total",
            "output",
            "reasoning",
            "fee_usd",
            "wall_seconds",
        ):
            self.assertEqual(row[key], expected[key])
        self.assertEqual(built["totals"]["full"]["input_total"], expected["input_total"])

    def test_a_session_directory_with_two_journals_is_summed(self):
        out = self.make_run("full", "T1", 1, input_tokens=100)
        write(out / "session" / "second.jsonl", session_journal(50))
        built = report.build(self.root / "runs")
        self.assertEqual(built["runs"][0]["metrics"]["input_total"], 150)
        self.assertEqual(built["totals"]["full"]["input_total"], 150)

    def test_a_timeout_run_is_kept_and_reported(self):
        self.make_run("full", "T1", 1, timed_out=True)
        built = report.build(self.root / "runs")
        self.assertEqual(built["runs"][0]["result"], "timeout")
        self.assertIn("TASK T1", report.format_report(built))
        self.assertIn("timeout=1", report.format_report(built))

    def test_a_baseline_with_no_runs_is_a_clear_error(self):
        self.make_run("full", "T1", 1)
        with self.assertRaises(report.ReportError) as error:
            report.build(self.root / "runs", "nope")
        self.assertIn("unknown baseline arm `nope`", str(error.exception))
        self.assertIn("full", str(error.exception))

    def test_ratios_divide_each_arm_by_the_baseline(self):
        self.make_run("full", "T1", 1, input_tokens=200)
        self.make_run("pi-like", "T1", 1, input_tokens=100)
        built = report.build(self.root / "runs", "full")
        self.assertEqual(built["ratios"]["T1"]["pi-like"]["total_in"], "0.500")
        self.assertEqual(built["ratios"]["TOTAL"]["pi-like"]["total_in"], "0.500")
        text = report.format_report(built)
        self.assertIn("RATIOS (arm / full)", text)
        self.assertIn("0.500", text)


class PromptTest(ScratchCase):
    def test_the_prompt_is_read_from_review_rs(self):
        source = REPO / "crates/radian-arc/src/gate/review.rs"
        copied = self.root / "review.rs"
        copied.write_text(
            source.read_text(encoding="utf-8").replace(
                "You are reviewing one change for the task",
                "MARKER: You are reviewing one change for the task",
                1,
            ),
            encoding="utf-8",
        )
        shutil.copy(source.parent / "verdict.rs", self.root / "verdict.rs")
        text = prompt.build_prompt(
            copied, "T1-demo", "the brief", "base1", "head1", "abc one", "+new\n"
        )
        self.assertIn("MARKER:", text)
        for needle in (
            "`T1-demo`",
            "the brief",
            "base1..head1",
            "abc one",
            "+new",
            "Finish with exactly one fenced ```json block",
            "`critical`, `major`, `minor`",
        ):
            self.assertIn(needle, text)

    def test_the_real_source_yields_arcs_first_review_prompt(self):
        source = REPO / "crates/radian-arc/src/gate/review.rs"
        text = prompt.build_prompt(
            source, "TK0001-a", "the brief", "b", "h", "c one", "diff\n"
        )
        self.assertTrue(text.startswith("You are reviewing one change for the task `TK0001-a`."))
        self.assertIn("Check the change against the brief and the rules of this repository", text)
        self.assertIn('"summary"', text)


class ReviewTextTest(unittest.TestCase):
    def test_a_minor_blocking_item_blocks(self):
        text = (
            "```json\n"
            '{"summary": "s", "blocking": [{"level": "minor", "text": "x"}], '
            '"suggestions": [], "verified": []}\n'
            "```\n"
        )
        parsed = lib.parse_review_text(text)
        self.assertTrue(parsed["parsed"])
        self.assertEqual(parsed["blocking"], 1)
        self.assertEqual(parsed["levels"], ["minor"])

    def test_the_last_json_block_wins(self):
        text = (
            "```json\n"
            '{"summary": "old", "blocking": [{"text": "x"}], '
            '"suggestions": [], "verified": []}\n'
            "```\n```json\n"
            '{"summary": "new", "blocking": [], "suggestions": [], '
            '"verified": []}\n'
            "```\n"
        )
        parsed = lib.parse_review_text(text)
        self.assertEqual(parsed["blocking"], 0)
        self.assertEqual(parsed["verdict"]["summary"], "new")

    def test_no_json_block_is_unparsed(self):
        self.assertFalse(lib.parse_review_text("no report here")["parsed"])


if __name__ == "__main__":
    unittest.main()
