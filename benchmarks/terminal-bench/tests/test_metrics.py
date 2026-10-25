"""Tests for `metrics.py` on the checked-in fixture job directories.

Run with the standard library alone:

    python3 benchmarks/terminal-bench/tests/test_metrics.py
"""

import contextlib
import io
import json
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import metrics  # noqa: E402

JOBS = Path(__file__).resolve().parent / "fixtures" / "jobs"


def find(report, agent, task):
    return next(
        trial
        for trial in report["trials"]
        if trial["agent"] == agent and trial["task"] == task
    )


def write_trial(
    root,
    *,
    agent="radian",
    info_name=None,
    task="a-task",
    reward=1.0,
    exception=None,
    metadata=None,
    stdout=None,
    journal=None,
):
    trial_dir = root / agent / "2026-01-01T00-00-00" / f"{task}__{agent}001"
    (trial_dir / "agent").mkdir(parents=True, exist_ok=True)
    result = {
        "task_name": task,
        "trial_name": f"{task}__{agent}001",
        "agent_info": {"name": info_name or agent, "version": "test"},
        "agent_result": {"cost_usd": None, "metadata": metadata or {}},
        "verifier_result": {"rewards": {"reward": reward}} if reward is not None else None,
        "exception_info": exception,
        "agent_execution": {
            "started_at": "2026-01-01T00:00:00Z",
            "finished_at": "2026-01-01T00:01:00Z",
        },
    }
    (trial_dir / "result.json").write_text(json.dumps(result), encoding="utf-8")
    if stdout is not None:
        (trial_dir / "agent" / "radian-stdout.txt").write_text(stdout, encoding="utf-8")
    if journal is not None:
        sessions = trial_dir / "agent" / "radian-sessions"
        sessions.mkdir(parents=True, exist_ok=True)
        (sessions / "s.jsonl").write_text(journal, encoding="utf-8")
    return trial_dir


class FixtureReportTest(unittest.TestCase):
    def setUp(self):
        self.report = metrics.build_report([JOBS])

    def test_agents_tasks_and_matrix(self):
        self.assertEqual(self.report["agents"], ["opencode", "pi", "radian"])
        self.assertEqual(self.report["tasks"], ["fix-git", "prove-plus-comm"])
        self.assertEqual(
            self.report["matrix"]["fix-git"]["radian"],
            {"trials": 1, "passed": 1, "failed": 0, "timeout": 0},
        )
        self.assertEqual(
            self.report["matrix"]["prove-plus-comm"]["pi"],
            {"trials": 1, "passed": 0, "failed": 0, "timeout": 1},
        )
        # A task known only from an excluded trial has no matrix row.
        self.assertNotIn("cancelled-scheduling", self.report["matrix"])

    def test_per_agent_totals(self):
        radian = self.report["totals"]["radian"]
        self.assertEqual(radian["trials"], 2)
        self.assertEqual(radian["excluded"], 1)
        self.assertEqual(
            (radian["pass"], radian["fail"], radian["timeout"]), (1, 1, 0)
        )
        self.assertAlmostEqual(radian["minutes"], 9.0)
        self.assertAlmostEqual(radian["cost_usd"], 0.0155)
        self.assertEqual(radian["model_calls"], 3)
        self.assertEqual(radian["output_tokens"], 21_500)
        self.assertEqual(radian["reasoning_tokens"], 1_300)
        self.assertEqual(radian["over_16k_calls"], 1)
        self.assertAlmostEqual(radian["generation_minutes"], 1.0 + 40.0 / 60.0)
        self.assertAlmostEqual(radian["tool_minutes"], 110.0 / 60.0)
        self.assertEqual(radian["max_tokens_trials"], 1)

        pi = self.report["totals"]["pi"]
        self.assertEqual((pi["trials"], pi["pass"], pi["timeout"]), (2, 1, 1))
        self.assertEqual(pi["model_calls"], 3)
        self.assertEqual(pi["output_tokens"], 20_600)
        self.assertEqual(pi["reasoning_tokens"], 420)
        self.assertEqual(pi["over_16k_calls"], 1)
        self.assertIsNone(pi["max_tokens_trials"])
        self.assertIsNone(pi["generation_minutes"])

        opencode = self.report["totals"]["opencode"]
        self.assertEqual(
            (opencode["trials"], opencode["pass"], opencode["fail"]), (2, 1, 1)
        )
        self.assertEqual(opencode["output_tokens"], 1_200)
        self.assertEqual(opencode["reasoning_tokens"], 50)

    def test_common_tasks_need_every_agent_to_finish(self):
        # pi timed out on prove-plus-comm, so only fix-git is common.
        self.assertEqual(self.report["common_tasks"], ["fix-git"])
        self.assertEqual(self.report["common_totals"]["radian"]["trials"], 1)
        self.assertEqual(self.report["common_totals"]["pi"]["trials"], 1)
        self.assertEqual(self.report["common_totals"]["opencode"]["trials"], 1)
        self.assertAlmostEqual(self.report["common_totals"]["radian"]["minutes"], 4.0)

    def test_cancelled_trial_is_marked_and_counted_separately(self):
        skipped = find(self.report, "radian", "cancelled-scheduling")
        self.assertTrue(skipped["excluded"])
        self.assertEqual(skipped["result"], "skip")
        self.assertEqual(
            sum(trial["excluded"] for trial in self.report["trials"]), 1
        )
        self.assertEqual(
            sum(trial["trials"] for trial in self.report["totals"].values()), 6
        )


class RadianJournalTest(unittest.TestCase):
    def test_generation_and_tool_minutes_come_from_journal_timestamps(self):
        trial = find(metrics.build_report([JOBS]), "radian", "fix-git")
        # 30 s before the first response, then 10 s after the tool settled.
        self.assertAlmostEqual(trial["generation_minutes"], 40.0 / 60.0)
        self.assertAlmostEqual(trial["tool_minutes"], 110.0 / 60.0)
        self.assertEqual(trial["model_calls"], 2)
        self.assertEqual(trial["output_tokens"], 17_500)
        self.assertEqual(trial["reasoning_tokens"], 1_300)
        self.assertEqual(trial["over_16k_calls"], 1)
        self.assertFalse(trial["max_tokens"])

    def test_incomplete_max_tokens_turn_is_reported(self):
        trial = find(metrics.build_report([JOBS]), "radian", "prove-plus-comm")
        self.assertTrue(trial["max_tokens"])
        self.assertEqual(trial["result"], "fail")

    def test_incomplete_turn_alone_marks_max_tokens(self):
        journal = (
            '{"event":"TurnFinished","data":{"turn_id":"t",'
            '"outcome":"incomplete","reason":"max_tokens"}}\n'
        )
        with tempfile.TemporaryDirectory() as tmp:
            trial_dir = write_trial(Path(tmp), journal=journal)
            trial = metrics.load_trial(trial_dir)
            self.assertTrue(trial["max_tokens"])

    def test_exec_exit_four_in_metadata_marks_max_tokens(self):
        with tempfile.TemporaryDirectory() as tmp:
            trial_dir = write_trial(
                Path(tmp),
                metadata={"exit_code": 4, "stop_reason": "incomplete"},
            )
            trial = metrics.load_trial(trial_dir)
            self.assertTrue(trial["max_tokens"])

    def test_result_line_with_exit_four_marks_max_tokens(self):
        result_line = json.dumps(
            {"type": "Result", "exit_code": 4, "stop_reason": "max_tokens"}
        )
        with tempfile.TemporaryDirectory() as tmp:
            trial_dir = write_trial(Path(tmp), stdout=result_line + "\n")
            trial = metrics.load_trial(trial_dir)
            self.assertTrue(trial["max_tokens"])

    def test_journal_fee_is_the_cost_fallback(self):
        journal = (
            '{"event":"ModelUsageRecorded","data":{"usage":{"output":1},'
            '"fee":{"micros":2500}}}\n'
        )
        with tempfile.TemporaryDirectory() as tmp:
            trial_dir = write_trial(Path(tmp), journal=journal)
            trial = metrics.load_trial(trial_dir)
            self.assertAlmostEqual(trial["cost_usd"], 0.0025)

    def test_torn_journal_tail_is_not_a_committed_record(self):
        with tempfile.TemporaryDirectory() as tmp:
            source = JOBS / "radian" / "2026-01-01T00-00-00" / "fix-git__radian001"
            target = Path(tmp) / "radian" / "2026-01-01T00-00-00" / source.name
            shutil.copytree(source, target)
            journal = next((target / "agent" / "radian-sessions").glob("*.jsonl"))
            journal.write_text(
                journal.read_text(encoding="utf-8")
                + '{"event":"ModelUsageRecorded","data":{"usage":{"output":999999}}',
                encoding="utf-8",
            )
            trial = metrics.load_trial(target)
            self.assertEqual(trial["output_tokens"], 17_500)
            self.assertEqual(trial["over_16k_calls"], 1)


class AtifTest(unittest.TestCase):
    def test_step_metrics_and_reasoning_extra(self):
        trial = find(metrics.build_report([JOBS]), "pi", "fix-git")
        self.assertEqual(trial["model_calls"], 2)
        self.assertEqual(trial["output_tokens"], 20_500)
        self.assertEqual(trial["reasoning_tokens"], 420)
        self.assertEqual(trial["over_16k_calls"], 1)
        self.assertAlmostEqual(trial["cost_usd"], 0.02)

    def test_unreported_reasoning_stays_none(self):
        trial = find(metrics.build_report([JOBS]), "opencode", "prove-plus-comm")
        self.assertIsNone(trial["reasoning_tokens"])

    def test_nested_completion_details_reasoning_is_read(self):
        trajectory = {
            "steps": [
                {
                    "source": "agent",
                    "metrics": {
                        "completion_tokens": 5,
                        "extra": {
                            "completion_tokens_details": {"reasoning_tokens": 3}
                        },
                    },
                }
            ]
        }
        self.assertEqual(
            metrics._trajectory_metrics(trajectory)["reasoning_tokens"], 3
        )
        self.assertEqual(
            metrics._reasoning({"output_tokens_details": {"thinking_tokens": 7}}), 7
        )

    def test_final_metrics_are_the_fallback_without_step_metrics(self):
        metrics_ = metrics._trajectory_metrics(
            {
                "steps": [],
                "final_metrics": {
                    "total_completion_tokens": 42,
                    "total_cost_usd": 0.5,
                },
            }
        )
        self.assertIsNone(metrics_["model_calls"])
        self.assertEqual(metrics_["output_tokens"], 42)
        self.assertAlmostEqual(metrics_["cost_usd"], 0.5)


class ClassificationTest(unittest.TestCase):
    def test_fractional_reward_is_not_a_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            trial_dir = write_trial(Path(tmp), reward=0.5)
            self.assertEqual(metrics.load_trial(trial_dir)["result"], "fail")

    def test_timeout_and_cancelled_exception_names(self):
        with tempfile.TemporaryDirectory() as tmp:
            timeout = write_trial(
                Path(tmp),
                agent="pi",
                task="timeout",
                exception={"exception_type": "VerifierTimeoutError"},
            )
            self.assertEqual(metrics.load_trial(timeout)["result"], "T/O")
            cancelled = write_trial(
                Path(tmp),
                agent="pi",
                task="cancelled",
                exception={"exception_type": "asyncio.CancelledError"},
            )
            self.assertTrue(metrics.load_trial(cancelled)["excluded"])

    def test_job_level_result_json_is_not_a_trial(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "result.json").write_text(
                json.dumps({"id": "job", "metrics": {}}), encoding="utf-8"
            )
            self.assertEqual(metrics.collect([root]), [])

    def test_agent_directory_label_keeps_radian_variants_apart(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_trial(root, agent="radian-a", info_name="radian")
            write_trial(root, agent="radian-b", info_name="radian")
            report = metrics.build_report([root])
            self.assertEqual(report["agents"], ["radian-a", "radian-b"])
            self.assertEqual(report["common_tasks"], ["a-task"])
            # Both were read as radian trials: the label, not the journal, differs.
            self.assertTrue(
                all(trial["max_tokens"] is False for trial in report["trials"])
            )

    def test_repeat_matrix_cell(self):
        self.assertEqual(
            metrics._cell_text({"trials": 2, "passed": 1, "failed": 1, "timeout": 0}),
            "P1/2",
        )
        self.assertEqual(
            metrics._cell_text({"trials": 3, "passed": 2, "failed": 0, "timeout": 1}),
            "P2/3 T1",
        )


class CommandLineTest(unittest.TestCase):
    def test_json_output_is_machine_readable(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            status = metrics.main(["--json", str(JOBS)])
        self.assertEqual(status, 0)
        report = json.loads(output.getvalue())
        self.assertEqual(report["common_tasks"], ["fix-git"])
        self.assertEqual(len(report["trials"]), 7)

    def test_text_output_has_every_section(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            status = metrics.main([str(JOBS)])
        self.assertEqual(status, 0)
        text = output.getvalue()
        for heading in ("TRIALS", "AGENT TOTALS", "COMMON TASKS", "COMMON-TASK TOTALS"):
            self.assertIn(heading, text)
        self.assertIn("fix-git", text)

    def test_empty_jobs_directory_exits_nonzero(self):
        with tempfile.TemporaryDirectory() as tmp:
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(metrics.main([tmp]), 1)


if __name__ == "__main__":
    unittest.main()
