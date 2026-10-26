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
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import metrics  # noqa: E402

JOBS = Path(__file__).resolve().parent / "fixtures" / "jobs"
# One trial per producer for the `gen_tok` rule, including an ATIF producer that
# has no rule. Kept apart from `jobs/` so its expected values stay unchanged.
GEN_TOK_JOBS = Path(__file__).resolve().parent / "fixtures" / "gen-tok-jobs"


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
        self.assertEqual(radian["generated_tokens"], 21_500)
        self.assertEqual(radian["reasoning_tokens"], 1_300)
        self.assertEqual(radian["over_16k_calls"], 1)
        self.assertAlmostEqual(radian["generation_minutes"], 1.0 + 40.0 / 60.0)
        self.assertAlmostEqual(radian["tool_minutes"], 110.0 / 60.0)
        self.assertEqual(radian["max_tokens_trials"], 1)

        pi = self.report["totals"]["pi"]
        self.assertEqual((pi["trials"], pi["pass"], pi["timeout"]), (2, 1, 1))
        self.assertEqual(pi["model_calls"], 3)
        self.assertEqual(pi["output_tokens"], 20_600)
        self.assertEqual(pi["generated_tokens"], 20_600)
        self.assertEqual(pi["reasoning_tokens"], 420)
        self.assertEqual(pi["over_16k_calls"], 1)
        self.assertIsNone(pi["max_tokens_trials"])
        self.assertIsNone(pi["generation_minutes"])

        opencode = self.report["totals"]["opencode"]
        self.assertEqual(
            (opencode["trials"], opencode["pass"], opencode["fail"]), (2, 1, 1)
        )
        self.assertEqual(opencode["output_tokens"], 1_200)
        self.assertEqual(opencode["generated_tokens"], 1_250)
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
                + '{"event":"ModelUsageRecorded","data":{"usage":{"output":999999}}}',
                encoding="utf-8",
            )
            trial = metrics.load_trial(target)
            # The tail is complete JSON without the final LF; only a committed
            # record counts, so its 999,999 tokens and >16k call are dropped.
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


class GeneratedTokensTest(unittest.TestCase):
    """`gen_tok` = visible output plus reasoning, by the per-producer rule."""

    def setUp(self):
        self.report = metrics.build_report([GEN_TOK_JOBS])

    def test_opencode_adds_its_reasoning_to_the_completion_count(self):
        trial = find(self.report, "opencode", "reasoning-heavy")
        self.assertEqual(trial["output_tokens"], 78)
        self.assertEqual(trial["reasoning_tokens"], 992)
        # The step's reasoning is larger than its completion count: the rule,
        # not the numbers, decides that opencode's count excludes reasoning.
        self.assertEqual(trial["generated_tokens"], 78 + 992)

    def test_pi_output_already_counts_reasoning(self):
        trial = find(self.report, "pi", "reasoning-heavy")
        self.assertEqual(trial["output_tokens"], 521)
        self.assertEqual(trial["reasoning_tokens"], 420)
        self.assertEqual(trial["generated_tokens"], 521)

    def test_radian_journal_output_already_counts_reasoning(self):
        trial = find(self.report, "radian", "reasoning-heavy")
        self.assertEqual(trial["output_tokens"], 2_392)
        self.assertEqual(trial["reasoning_tokens"], 2_154)
        self.assertEqual(trial["generated_tokens"], 2_392)

    def test_an_unruled_atif_producer_is_unknown_and_named_once(self):
        trial = find(self.report, "mystery", "reasoning-heavy")
        self.assertEqual(trial["output_tokens"], 100)
        self.assertIsNone(trial["generated_tokens"])
        # The producer's own name, not the directory label.
        self.assertEqual(self.report["gen_unknown_agents"], ["mystery-agent"])
        text = metrics.format_report(self.report)
        self.assertEqual(text.count("mystery-agent"), 1)
        self.assertIn("gen_tok has no rule for: mystery-agent", text)
        self.assertEqual(metrics.format_report(metrics.build_report([JOBS])).count("no rule"), 0)

    def test_totals_add_generated_tokens_across_trials(self):
        self.assertEqual(self.report["totals"]["opencode"]["generated_tokens"], 1_070)
        self.assertEqual(self.report["totals"]["pi"]["generated_tokens"], 521)
        self.assertEqual(self.report["totals"]["radian"]["generated_tokens"], 2_392)
        self.assertIsNone(self.report["totals"]["mystery"]["generated_tokens"])
        self.assertEqual(
            self.report["common_totals"]["opencode"]["generated_tokens"], 1_070
        )

    def test_a_step_without_reasoning_adds_its_completion_only(self):
        trajectory = {
            "agent": {"name": "opencode"},
            "steps": [
                {"metrics": {"completion_tokens": 10}},
                {
                    "metrics": {
                        "completion_tokens": 5,
                        "extra": {"reasoning_tokens": 7},
                    }
                },
            ],
        }
        self.assertEqual(metrics._trajectory_metrics(trajectory)["generated_tokens"], 22)

    def test_no_completion_count_leaves_generated_unknown(self):
        trajectory = {
            "agent": {"name": "opencode"},
            "steps": [{"metrics": {"prompt_tokens": 3}}],
        }
        self.assertIsNone(metrics._trajectory_metrics(trajectory)["generated_tokens"])

    def test_final_metrics_give_generated_tokens_only_when_the_output_includes_reasoning(
        self,
    ):
        pi = metrics._trajectory_metrics(
            {
                "agent": {"name": "pi"},
                "steps": [],
                "final_metrics": {"total_completion_tokens": 42},
            }
        )
        self.assertEqual(pi["generated_tokens"], 42)
        opencode = metrics._trajectory_metrics(
            {
                "agent": {"name": "opencode"},
                "steps": [],
                "final_metrics": {"total_completion_tokens": 42},
            }
        )
        self.assertIsNone(opencode["generated_tokens"])

    def test_the_rule_table_names_the_journal_and_both_atif_producers(self):
        self.assertEqual(metrics.GENERATED_RULES["radian"], "includes_reasoning")
        self.assertEqual(metrics.GENERATED_RULES["pi"], "includes_reasoning")
        self.assertEqual(metrics.GENERATED_RULES["opencode"], "excludes_reasoning")


class BoundedReadTest(unittest.TestCase):
    """The jobs directory is input: a journal is streamed, a JSON file is capped."""

    def test_trial_files_are_never_read_whole(self):
        def refuse(*args, **kwargs):
            raise AssertionError("read_text on a trial file")

        with mock.patch.object(Path, "read_text", refuse):
            report = metrics.build_report([JOBS])
        self.assertEqual(len(report["trials"]), 7)

    def test_oversized_json_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "trajectory.json"
            path.write_text('{"steps":[]}' + " " * 64, encoding="utf-8")
            self.assertIsNone(metrics._read_json(path, limit=16))
            self.assertEqual(metrics._read_trajectory(path, limit=16), {})

    def test_a_long_line_is_skipped_without_being_read(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "journal.jsonl"
            good = '{"a":1}\n'
            path.write_text("x" * 100 + "\n" + good, encoding="utf-8")
            self.assertEqual(list(metrics._iter_lines(path, limit=16)), [good])

    def test_an_unterminated_tail_is_a_line_the_journal_drops(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "journal.jsonl"
            path.write_text('{"seq":1}\n{"seq":2', encoding="utf-8")
            self.assertEqual(
                list(metrics._iter_lines(path)), ['{"seq":1}\n', '{"seq":2']
            )


def read_bound_docs():
    """The prose describing the read bounds, whitespace-normalized."""
    readme = (Path(metrics.__file__).resolve().parent / "README.md").read_text(
        encoding="utf-8"
    )
    return {
        "benchmarks/terminal-bench/README.md": " ".join(readme.split()),
        "metrics.py module docstring": " ".join((metrics.__doc__ or "").split()),
    }


class ReadBoundDocsTest(unittest.TestCase):
    """The prose says which files are streamed and which are read whole."""

    def test_no_document_claims_that_no_trial_file_is_read_whole(self):
        # `_read_json` reads a result.json or trajectory.json whole up to the
        # cap, so a blanket "nothing is read whole" contradicts the same text.
        false_claims = (
            "no trial file is read whole",
            "no trial file is ever read whole",
            "no trial file is loaded whole",
            "no trial file is read in full",
            "a trial file is never read whole",
            "a trial file is never read in full",
        )
        for name, prose in read_bound_docs().items():
            for claim in false_claims:
                with self.subTest(doc=name, claim=claim):
                    self.assertNotIn(claim, prose.lower())

    def test_each_document_names_both_bounds_and_both_read_styles(self):
        for name, prose in read_bound_docs().items():
            with self.subTest(doc=name):
                self.assertIn("MAX_LINE_CHARS", prose)
                self.assertIn("MAX_JSON_BYTES", prose)
                # Journals are streamed; the two JSON inputs are read whole.
                self.assertIn("streamed", prose)
                self.assertIn("read whole", prose)


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


class VerifyGateTest(unittest.TestCase):
    def test_verify_runs_the_fixture_tests(self):
        root = Path(__file__).resolve().parents[3]
        verify = (root / "scripts" / "verify.sh").read_text(encoding="utf-8")
        self.assertIn(
            "run_step python3 benchmarks/terminal-bench/metrics.py --self-test", verify
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

    def test_json_has_generated_tokens_for_trials_and_totals(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            status = metrics.main(["--json", str(GEN_TOK_JOBS)])
        self.assertEqual(status, 0)
        report = json.loads(output.getvalue())
        for trial in report["trials"]:
            self.assertIn("generated_tokens", trial)
        for section in ("totals", "common_totals"):
            for totals in report[section].values():
                self.assertIn("generated_tokens", totals)
        self.assertEqual(report["gen_unknown_agents"], ["mystery-agent"])

    def test_text_output_has_a_gen_tok_column(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            status = metrics.main([str(JOBS)])
        self.assertEqual(status, 0)
        headings = [
            line
            for line in output.getvalue().splitlines()
            if "out_tok" in line and "gen_tok" in line
        ]
        # TRIALS, AGENT TOTALS and COMMON-TASK TOTALS each carry the column.
        self.assertEqual(len(headings), 3)

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
