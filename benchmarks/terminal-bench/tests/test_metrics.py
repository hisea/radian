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
sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

import journal_metrics  # noqa: E402
import metrics  # noqa: E402

JOBS = Path(__file__).resolve().parent / "fixtures" / "jobs"
# One trial per producer for the `gen_tok` rule, including an ATIF producer that
# has no rule. Kept apart from `jobs/` so its expected values stay unchanged.
GEN_TOK_JOBS = Path(__file__).resolve().parent / "fixtures" / "gen-tok-jobs"
# Three common tasks where one task carries the whole gen_tok/cost difference,
# with three trials of that task for `tall`. `base` and `tall` are the labels;
# both trials were produced by pi, so the ATIF `gen_tok` rule applies.
ANALYSIS = Path(__file__).resolve().parent / "fixtures" / "analysis-jobs"
# The fixed-definition extractor's own fixtures: one radian journal with every
# column, one pi session, a latency bucket boundary, read ranges and a torn tail.
JOURNAL_METRICS = Path(__file__).resolve().parent / "fixtures" / "journal-metrics"
FULL = JOURNAL_METRICS / "full" / "run-full.jsonl"
PI_SESSION = JOURNAL_METRICS / "pi" / "session.jsonl"
LATENCY = JOURNAL_METRICS / "latency" / "boundary.jsonl"
READ_RANGES = JOURNAL_METRICS / "read" / "ranges.jsonl"
TORN = JOURNAL_METRICS / "torn" / "torn.jsonl"
PREFIX = JOURNAL_METRICS / "prefix"
# ADR 0007 cache accounting: one journal per `input_includes_cache` state.
USAGE = JOURNAL_METRICS / "usage"
# A session directory with a nested JSONL artifact that is not a session journal.
NESTED = JOURNAL_METRICS / "nested"


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


class RescanCounter(list):
    """A trial list that counts how many times its elements are iterated."""

    def __init__(self, trials):
        super().__init__(trials)
        self.scans = 0

    def __iter__(self):
        self.scans += 1
        return super().__iter__()


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


class AnalysisFixtureTest(unittest.TestCase):
    """SPREAD, LEAVE-ONE-TASK-OUT, cost/pass and INTERVALS on a known fixture."""

    def setUp(self):
        self.report = metrics.build_report([ANALYSIS])

    def test_baseline_defaults_to_the_first_agent_in_report_order(self):
        self.assertEqual(self.report["agents"], ["base", "tall"])
        self.assertEqual(self.report["baseline"], "base")

    def test_spread_reports_min_median_max_for_three_trials(self):
        cell = self.report["spread"]["cells"]["dominant"]["tall"]
        self.assertEqual((cell["min"], cell["median"], cell["max"]), (300, 500, 700))
        self.assertEqual(cell["trials"], 3)
        single = self.report["spread"]["cells"]["small-a"]["tall"]
        self.assertEqual(
            (single["min"], single["median"], single["max"]), (100, 100, 100)
        )
        text = metrics.format_report(self.report)
        self.assertIn("300/500/700", text)

    def test_spread_ratio_sums_an_agents_trials_of_the_task(self):
        # tall: 300 + 500 + 700; base: 100 on the same task.
        self.assertAlmostEqual(self.report["spread"]["ratios"]["dominant"]["tall"], 15.0)
        self.assertAlmostEqual(self.report["spread"]["ratios"]["small-a"]["tall"], 1.0)

    def test_cost_per_pass_is_cost_over_passes(self):
        base = self.report["common_totals"]["base"]
        tall = self.report["common_totals"]["tall"]
        self.assertAlmostEqual(base["cost_usd"], 0.07)
        self.assertEqual(base["pass"], 2)
        self.assertAlmostEqual(base["cost_per_pass"], 0.035)
        self.assertAlmostEqual(tall["cost_usd"], 0.12)
        self.assertEqual(tall["pass"], 4)
        self.assertAlmostEqual(tall["cost_per_pass"], 0.03)
        self.assertIn("cost_per_pass", metrics.format_report(self.report))

    def test_cost_per_pass_is_missing_without_a_pass_or_cost(self):
        # opencode passed no trial in the common task, so its cost/pass is `-`.
        text = metrics.format_report(metrics.build_report([JOBS]))
        self.assertIn("cost_per_pass", text)
        self.assertIsNone(
            metrics.summarize(
                [
                    {
                        "excluded": False,
                        "result": "fail",
                        "cost_usd": 1.0,
                        "minutes": None,
                        "model_calls": None,
                        "output_tokens": None,
                        "generated_tokens": None,
                        "reasoning_tokens": None,
                        "over_16k_calls": None,
                        "generation_minutes": None,
                        "tool_minutes": None,
                        "max_tokens": None,
                    }
                ]
            )["cost_per_pass"]
        )

    def test_leave_one_out_marks_the_task_that_carries_the_difference(self):
        rows = self.report["leave_one_out"]["rows"]
        self.assertIsNone(rows[0]["task"])  # the (none) row comes first
        self.assertAlmostEqual(rows[0]["gen_tok"]["tall"], 1700 / 300)
        self.assertAlmostEqual(rows[0]["cost"]["tall"], 0.12 / 0.07)
        without_dominant = next(row for row in rows if row["task"] == "dominant")
        # Without the task that carries the whole difference both ratios are 1.
        self.assertAlmostEqual(without_dominant["gen_tok"]["tall"], 1.0)
        self.assertAlmostEqual(without_dominant["cost"]["tall"], 1.0)
        self.assertEqual(self.report["leave_one_out"]["marked"], "dominant")
        self.assertIn("dominant*", metrics.format_report(self.report))

    def test_the_none_row_equals_the_common_task_totals_ratio(self):
        rows = self.report["leave_one_out"]["rows"]
        tall = self.report["common_totals"]["tall"]["generated_tokens"]
        base = self.report["common_totals"]["base"]["generated_tokens"]
        self.assertAlmostEqual(rows[0]["gen_tok"]["tall"], tall / base)
        cost_ratio = (
            self.report["common_totals"]["tall"]["cost_usd"]
            / self.report["common_totals"]["base"]["cost_usd"]
        )
        self.assertAlmostEqual(rows[0]["cost"]["tall"], cost_ratio)

    def test_interval_for_a_task_carrying_the_difference_holds_one(self):
        # Three common tasks: tall's gen_tok is 15x base on `dominant` and equal
        # on the other two. `dominant` is absent from (2/3)**3 = 8/27 (~30%) of
        # the task resamples, above the 2.5% tail, and then every gen_tok and
        # cost ratio is exactly 1.0. The 2.5th percentile is therefore 1.0 and
        # both ratio intervals hold 1.0: the difference is inconclusive.
        intervals = self.report["intervals"]
        gen_tok = intervals["agents"]["tall"]["gen_tok"]
        cost = intervals["agents"]["tall"]["cost"]
        self.assertEqual(gen_tok["low"], 1.0)
        self.assertGreater(gen_tok["high"], 1.0)
        self.assertTrue(gen_tok["inconclusive"])
        self.assertEqual(cost["low"], 1.0)
        self.assertTrue(cost["inconclusive"])
        # The pass difference is in percentage points, not a fraction, and its
        # interval here reaches 100 pp.
        pass_rate = intervals["agents"]["tall"]["pass_rate"]
        self.assertGreater(pass_rate["high"], 1.0)
        self.assertTrue(pass_rate["inconclusive"])
        self.assertIn("inconclusive", metrics.format_report(self.report))


class BaselineOptionTest(unittest.TestCase):
    def test_a_known_baseline_flips_every_ratio(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            status = metrics.main(
                ["--json", "--baseline", "tall", str(ANALYSIS)]
            )
        self.assertEqual(status, 0)
        report = json.loads(output.getvalue())
        self.assertEqual(report["baseline"], "tall")
        self.assertAlmostEqual(
            report["leave_one_out"]["rows"][0]["gen_tok"]["base"], 300 / 1700
        )
        self.assertEqual(report["spread"]["ratios"]["dominant"]["base"], 100 / 1500)

    def test_an_unknown_baseline_is_a_usage_error_that_lists_the_agents(self):
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            with self.assertRaises(SystemExit) as raised:
                metrics.main(["--baseline", "nope", str(ANALYSIS)])
        self.assertEqual(raised.exception.code, 2)
        message = stderr.getvalue()
        self.assertIn("nope", message)
        self.assertIn("base", message)
        self.assertIn("tall", message)


class IntervalOutputTest(unittest.TestCase):
    def test_fewer_than_three_common_tasks_prints_one_line(self):
        report = metrics.build_report([JOBS])
        self.assertEqual(len(report["common_tasks"]), 1)
        self.assertEqual(report["intervals"]["note"], metrics.INTERVAL_MIN_TASKS_NOTE)
        text = metrics.format_report(report)
        self.assertIn("the interval needs at least 3 common tasks", text)
        self.assertNotIn("gen_tok ratio", text)

    def test_bootstrap_output_is_identical_across_two_runs(self):
        runs = []
        for _ in range(2):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                status = metrics.main(["--json", str(ANALYSIS)])
            self.assertEqual(status, 0)
            runs.append(json.loads(output.getvalue()))
        self.assertEqual(runs[0]["intervals"], runs[1]["intervals"])
        self.assertEqual(runs[0], runs[1])

    def test_json_carries_the_analysis_and_cost_per_pass(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            status = metrics.main(["--json", str(ANALYSIS)])
        self.assertEqual(status, 0)
        report = json.loads(output.getvalue())
        for key in ("baseline", "spread", "leave_one_out", "intervals"):
            self.assertIn(key, report)
        for section in ("totals", "common_totals"):
            for totals in report[section].values():
                self.assertIn("cost_per_pass", totals)
        self.assertEqual(report["intervals"]["resamples"], 2000)
        self.assertEqual(report["intervals"]["seed"], 0)


class IntervalCostTest(unittest.TestCase):
    def test_the_bootstrap_does_not_rescan_the_trials_on_every_draw(self):
        trials = metrics.collect([ANALYSIS])
        agents = ["base", "tall"]
        common = metrics.common_tasks(trials, agents)
        self.assertEqual(len(common), 3)
        counted = RescanCounter(trials)
        metrics.intervals(counted, agents, common, "base")
        # A rescan per (draw, agent, task) would iterate the whole list
        # BOOTSTRAP_RESAMPLES * len(common) * len(agents) times. Indexing the
        # non-excluded trials once keeps this to a couple of passes.
        self.assertLess(counted.scans, metrics.BOOTSTRAP_RESAMPLES)


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


def write_journal(path, records, final_newline=True):
    """Write journal records as JSONL, with or without the last commit LF."""
    text = "".join(json.dumps(record) + "\n" for record in records)
    if not final_newline and text.endswith("\n"):
        text = text[:-1]
    path.write_text(text, encoding="utf-8")


class JournalMetricsTest(unittest.TestCase):
    """The fixed-definition extractor on its own small fixtures."""

    def row(self, path):
        return journal_metrics.build_report([path])["journals"][0]

    def test_a_radian_journal_yields_every_column(self):
        row = self.row(FULL)
        self.assertEqual(row["source"], "radian")
        # Two assistant responses plus one summary call.
        self.assertEqual(row["calls"], 3)
        # input 1000 includes a 200 cache read; the two other calls are uncached.
        self.assertEqual(row["input_uncached"], 110_800)
        self.assertEqual(row["input_cache_read"], 200)
        self.assertEqual(row["input_total"], 111_000)
        self.assertEqual(row["output"], 1_300)
        self.assertEqual(row["reasoning"], 75)
        self.assertAlmostEqual(row["fee_usd"], 0.0035)
        self.assertEqual(row["tool_calls"], {"read": 2})
        self.assertEqual(row["tool_bytes"], {"read": 8})
        self.assertEqual(row["read_ranged_calls"], 1)
        self.assertEqual(row["read_ranged_bytes"], 5)
        self.assertEqual(row["read_unranged_calls"], 1)
        self.assertEqual(row["read_unranged_bytes"], 3)
        self.assertEqual(row["read_large_calls"], 0)
        self.assertEqual(row["compactions"], 1)
        self.assertEqual(row["deterministic_compactions"], 0)
        self.assertEqual(row["outcomes"], {"failed": 1})
        self.assertEqual(row["wall_seconds"], 17)
        self.assertEqual(row["latency"]["0-99999"]["calls"], 3)
        self.assertAlmostEqual(row["latency"]["0-99999"]["p50_ms"], 2_000)
        self.assertAlmostEqual(row["latency"]["0-99999"]["p90_ms"], 8_400)

    def test_a_pi_session_uses_the_same_columns_and_bash_is_shell(self):
        row = self.row(PI_SESSION)
        self.assertEqual(row["source"], "pi")
        self.assertEqual(row["calls"], 2)
        self.assertEqual(row["input_uncached"], 3_000)
        self.assertEqual(row["input_cache_read"], 300)
        self.assertEqual(row["input_total"], 3_300)
        self.assertEqual(row["output"], 500)
        self.assertEqual(row["reasoning"], 40)
        self.assertAlmostEqual(row["fee_usd"], 0.03)
        self.assertEqual(row["tool_calls"], {"shell": 1})
        self.assertNotIn("bash", row["tool_calls"])
        self.assertEqual(row["tool_bytes"], {"shell": 5})
        self.assertEqual(row["outcomes"], {"completed": 1})
        self.assertEqual(row["wall_seconds"], 20)
        self.assertEqual(row["latency"]["0-99999"]["calls"], 2)
        self.assertAlmostEqual(row["latency"]["0-99999"]["p50_ms"], 9_500)
        self.assertAlmostEqual(row["latency"]["0-99999"]["p90_ms"], 9_900)

    def test_latency_uses_a_bucket_per_100k_prompt_tokens(self):
        row = self.row(LATENCY)
        self.assertEqual(sorted(row["latency"]), ["0-99999", "100000-199999"])
        low = row["latency"]["0-99999"]
        high = row["latency"]["100000-199999"]
        self.assertEqual(low["calls"], 2)
        # 99,999 and 100,000 prompt tokens are a bucket apart.
        self.assertEqual(high["calls"], 1)
        # Bucket 0 samples are 1 s and 3 s; bucket 1 is 2 s.
        self.assertAlmostEqual(low["p50_ms"], 2_000)
        self.assertAlmostEqual(low["p90_ms"], 2_800)
        self.assertAlmostEqual(high["p50_ms"], 2_000)
        self.assertAlmostEqual(high["p90_ms"], 2_000)

    def test_read_counts_ranged_and_unranged_by_offset_or_limit(self):
        row = self.row(READ_RANGES)
        self.assertEqual(row["read_ranged_calls"], 1)
        self.assertEqual(row["read_ranged_bytes"], len("hello"))
        self.assertEqual(row["read_unranged_calls"], 1)
        self.assertEqual(row["read_unranged_bytes"], len("world!"))
        self.assertEqual(row["read_large_calls"], 0)

    def test_a_read_result_counts_as_large_from_20_480_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "j.jsonl"
            write_journal(
                path,
                [
                    {
                        "event": "ToolExecutionStarted",
                        "data": {"call_id": "big", "name": "read", "arguments": {}},
                    },
                    {
                        "event": "ToolExecutionStarted",
                        "data": {"call_id": "small", "name": "read", "arguments": {}},
                    },
                    {
                        "event": "ToolExecutionSettled",
                        "data": {
                            "call_id": "big",
                            "result": {"content": [{"text": {"text": "x" * 20_480}}]},
                        },
                    },
                    {
                        "event": "ToolExecutionSettled",
                        "data": {
                            "call_id": "small",
                            "result": {"content": [{"text": {"text": "x" * 20_479}}]},
                        },
                    },
                ],
            )
            row = self.row(path)
        self.assertEqual(row["read_large_calls"], 1)
        self.assertEqual(row["read_unranged_calls"], 2)
        self.assertEqual(row["read_unranged_bytes"], 20_480 + 20_479)

    def test_a_torn_last_line_is_dropped(self):
        row = self.row(TORN)
        self.assertEqual(row["output"], 7)
        self.assertEqual(row["input_uncached"], 10)
        self.assertEqual(row["calls"], 0)

    def test_json_and_table_report_the_same_rows(self):
        report = journal_metrics.build_report([FULL])
        table = journal_metrics.format_report(report)
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            status = journal_metrics._main(["--json", str(FULL)])
        self.assertEqual(status, 0)
        printed = json.loads(output.getvalue())
        self.assertEqual(printed, report)
        self.assertEqual(journal_metrics.format_report(printed), table)
        self.assertIn("run-full.jsonl", table)

    def test_a_prefix_filter_reads_only_matching_files(self):
        all_files = journal_metrics.build_report([PREFIX])
        self.assertEqual(len(all_files["journals"]), 2)
        only_a = journal_metrics.build_report([PREFIX], prefix="run-a")
        self.assertEqual(len(only_a["journals"]), 1)
        self.assertTrue(only_a["journals"][0]["journal"].endswith("run-a.jsonl"))
        self.assertEqual(only_a["journals"][0]["input_uncached"], 10)

    def test_cache_accounting_follows_adr_0007(self):
        rows = {
            Path(row["journal"]).name: row
            for row in journal_metrics.build_report([USAGE])["journals"]
        }
        # `input_includes_cache: true`: `input` is the whole prompt, and both
        # cache counters are inside it (ADR 0007 `uncached_input()`).
        includes = rows["marked-includes.jsonl"]
        self.assertEqual(includes["input_uncached"], 500)
        self.assertEqual(includes["input_cache_read"], 200)
        self.assertEqual(includes["input_total"], 1_000)
        # `false`: `input` is the uncached part; both counters are siblings.
        excludes = rows["marked-excludes.jsonl"]
        self.assertEqual(excludes["input_uncached"], 1_000)
        self.assertEqual(excludes["input_cache_read"], 200)
        self.assertEqual(excludes["input_total"], 1_500)
        # Unmarked (written before ADR 0007): `input` stands alone, so its
        # cache read is reported but not added into the total again.
        unmarked = rows["unmarked.jsonl"]
        self.assertEqual(unmarked["input_uncached"], 1_000)
        self.assertEqual(unmarked["input_cache_read"], 200)
        self.assertEqual(unmarked["input_total"], 1_000)
        # pi's `input` never includes cache, so both counters add to the total.
        pi = rows["pi-cache-write.jsonl"]
        self.assertEqual(pi["input_uncached"], 1_000)
        self.assertEqual(pi["input_cache_read"], 200)
        self.assertEqual(pi["input_total"], 1_500)

    def test_journal_metrics_reads_only_the_session_directory(self):
        # The Harbor report read `sessions_dir.glob("*.jsonl")`: a nested JSONL
        # artifact must not become a model call or a token.
        values = metrics._journal_metrics(NESTED)
        self.assertEqual(values["model_calls"], 1)
        self.assertEqual(values["output_tokens"], 10)
        self.assertEqual(values["reasoning_tokens"], None)
        self.assertAlmostEqual(values["cost_usd"], 0.001)
        self.assertEqual(values["max_tokens"], False)

    def test_journal_metrics_keeps_the_harbor_report_values(self):
        fix_git = JOBS / "radian" / "2026-01-01T00-00-00" / "fix-git__radian001"
        prove = JOBS / "radian" / "2026-01-01T00-00-00" / "prove-plus-comm__radian002"
        sessions = fix_git / "agent" / "radian-sessions"
        self.assertEqual(
            metrics._journal_metrics(sessions),
            {
                "model_calls": 2,
                "output_tokens": 17_500,
                "generated_tokens": 17_500,
                "reasoning_tokens": 1_300,
                "over_16k_calls": 1,
                "generation_minutes": 40.0 / 60.0,
                "tool_minutes": 110.0 / 60.0,
                "cost_usd": 0.012,
                "max_tokens": False,
            },
        )
        self.assertEqual(
            metrics._journal_metrics(prove / "agent" / "radian-sessions"),
            {
                "model_calls": 1,
                "output_tokens": 4_000,
                "generated_tokens": 4_000,
                "reasoning_tokens": 0,
                "over_16k_calls": 0,
                "generation_minutes": 1.0,
                "tool_minutes": 0.0,
                "cost_usd": 0.003,
                "max_tokens": True,
            },
        )
        self.assertEqual(
            metrics._journal_metrics(sessions),
            journal_metrics.usage_metrics([sessions], recursive=False),
        )

    def test_the_help_and_readme_carry_the_generated_column_table(self):
        output = io.StringIO()
        with self.assertRaises(SystemExit) as raised:
            with contextlib.redirect_stdout(output):
                journal_metrics._main(["--help"])
        self.assertEqual(raised.exception.code, 0)
        self.assertIn(journal_metrics.columns_text(), output.getvalue())
        readme = (Path(metrics.__file__).resolve().parent / "README.md").read_text(
            encoding="utf-8"
        )
        self.assertIn(journal_metrics.columns_markdown(), readme)


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
