//! `attempt_metrics` and `task_metrics`: the one reader `radian arc show` and the arc TUI use
//! for an attempt's runs, times, report and costs (ADR 0048 decisions 7, 10 and 12).
//!
//! Every test writes the records an attempt leaves under `runs/<task>/attempts/<attempt>/` and
//! `attempts/<task>/<attempt>/`, so the reader is exercised without a model or a gate.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::Repo;
use radian_arc::read;
use radian_arc::store::{Store, cli};
use serde_json::{Value, json};

struct Fixture {
    repo: Repo,
    store: Store,
    task: String,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let repo = Repo::new(label);
        Store::init(&repo.clone, false).expect("arc init");
        let store = Store::open(&repo.clone).expect("the store opens");
        let plan = store
            .add_plan("m", "A plan", "the request\n")
            .expect("add a plan");
        let task = store
            .add_task(
                &plan,
                "task",
                "A task",
                Vec::new(),
                false,
                Vec::new(),
                "the brief\n",
            )
            .expect("add a task");
        Self { repo, store, task }
    }

    fn run_dir(&self, attempt: u32) -> PathBuf {
        self.repo.run_dir(&self.task, attempt)
    }

    /// Writes one JSON file in the attempt's run directory.
    fn write_run(&self, attempt: u32, name: &str, value: &Value) {
        write_json_at(&self.run_dir(attempt).join(name), value);
    }

    /// Writes a `review` verdict for the attempt.
    fn write_review(&self, attempt: u32, verdict: &Value) {
        write_json_at(&self.run_dir(attempt).join("verdicts/review.json"), verdict);
    }
}

fn write_json_at(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().expect("a parent")).expect("the parent directory");
    fs::write(path, format!("{value}\n")).expect("the JSON file");
}

/// Writes a jsonl stream from the values.
fn write_jsonl(path: &Path, values: &[Value]) {
    fs::create_dir_all(path.parent().expect("a parent")).expect("the parent directory");
    let mut text = String::new();
    for value in values {
        text.push_str(&value.to_string());
        text.push('\n');
    }
    fs::write(path, text).expect("the jsonl file");
}

/// A passing `review` verdict with one run and the given `exec_args_values` and metrics.
fn review_verdict(run: u32, exec_args: &[&str], metrics: Value) -> Value {
    json!({
        "schema_version": 1,
        "stage": "review",
        "sha": "a".repeat(40),
        "result": "pass",
        "reason": null,
        "commands": [],
        "started_at": "2026-01-01T00:00:00.000000Z",
        "ended_at": "2026-01-01T00:00:03.000000Z",
        "review_session_ids": ["s"],
        "review_cost_micros": 2000,
        "review_settings": {
            "round": "first",
            "reviewer": "default",
            "fix_level": "major",
            "exec_args": "gate.reviewers.default.exec_args",
            "exec_args_values": exec_args,
            "timeout_secs": 900,
            "timeout": "gate.reviewers.default.timeout_secs",
        },
        "runs": [{
            "run": run,
            "session_id": "s",
            "started_at": "2026-01-01T00:00:01.000000Z",
            "ended_at": "2026-01-01T00:00:02.000000Z",
            "metrics": metrics,
        }],
        "profile": "default",
    })
}

/// An old review verdict written before `runs` existed still reads: `show` and `show --json`
/// work, and the review row is not reported. Mutation: require the field and an old verdict
/// fails to read.
#[test]
fn an_old_review_verdict_without_runs_reads_as_no_review_runs() {
    let fixture = Fixture::new("old-verdict");
    let old = json!({
        "schema_version": 1,
        "stage": "review",
        "sha": "a".repeat(40),
        "result": "pass",
        "reason": null,
        "commands": [],
        "started_at": "2026-01-01T00:00:00.000000Z",
        "ended_at": "2026-01-01T00:00:03.000000Z",
        "review": {"summary": "fine", "findings": [], "verified": []},
        "profile": "default",
    });
    fixture.write_review(1, &old);

    let metrics = read::attempt_metrics(&fixture.store, &fixture.task, 1).expect("metrics");
    assert!(metrics.reviews.is_empty(), "an old verdict has no runs");

    let shown = cli::show_json(&fixture.store, &fixture.task).expect("show --json");
    assert_eq!(
        shown["metrics"]["attempts"][0]["reviews"],
        json!([]),
        "no review row is reported: {shown}"
    );
    let text = cli::show_text(&fixture.store, &fixture.task).expect("show");
    assert!(
        !text.contains(" review 1:"),
        "an old verdict prints no review run: {text}"
    );
}

/// A review run whose metrics carry no `models` takes `--provider`/`--model` from its recorded
/// `exec_args`; with neither it reports `-`. Mutation: ignore the recorded args and the model
/// route is always absent.
#[test]
fn a_review_runs_model_route_falls_back_to_its_exec_args() {
    let fixture = Fixture::new("provider-fallback");
    let metrics = json!({"requests": 1, "input_uncached": 10, "models": []});
    fixture.write_review(
        1,
        &review_verdict(
            1,
            &["--provider", "anthropic", "--model", "m"],
            metrics.clone(),
        ),
    );
    fixture.write_review(2, &review_verdict(1, &[], metrics));

    let with_args = read::attempt_metrics(&fixture.store, &fixture.task, 1).expect("metrics");
    assert_eq!(
        with_args.reviews[0].provider.as_deref(),
        Some("anthropic"),
        "the provider comes from the exec args"
    );
    assert_eq!(with_args.reviews[0].model.as_deref(), Some("m"));

    let without = read::attempt_metrics(&fixture.store, &fixture.task, 2).expect("metrics");
    assert_eq!(without.reviews[0].provider, None);
    assert_eq!(without.reviews[0].model, None);

    let text = cli::show_text(&fixture.store, &fixture.task).expect("show");
    assert!(
        text.contains(" review 1: model=anthropic/m "),
        "the review line prints the route: {text}"
    );
}

/// `task_metrics` sums each field over the runs that reported it, leaves a field no run
/// reported `None` (not 0), and counts the one run that reported no request and no token.
/// Mutation: treat a missing field as 0 in the sums, and `reasoning` becomes 0 here.
#[test]
fn the_totals_sum_only_reported_fields_and_count_unreported_runs() {
    let fixture = Fixture::new("totals");
    write_jsonl(
        &fixture.run_dir(1).join("result.jsonl"),
        &[json!({
            "type": "Result",
            "outcome": "completed",
            "cost": {"micros": 1000},
            "metrics": {
                "requests": 2,
                "input_uncached": 10,
                "cache_read": 4,
                "cache_write": null,
                "output": 3,
                "reasoning": null,
                "fee_micros": 1000,
                "models": [],
            },
            "text": "done",
        })],
    );
    fixture.write_run(
        1,
        "execution.json",
        &json!({
            "started_at": "2026-01-01T00:00:00.000000Z",
            "ended_at": "2026-01-01T00:00:10.000000Z",
            "exec_args": [],
        }),
    );
    write_jsonl(
        &fixture.run_dir(2).join("result.jsonl"),
        &[json!({
            "type": "Result",
            "outcome": "completed",
            "text": "no metrics at all",
        })],
    );

    let metrics = read::task_metrics(&fixture.store, &fixture.task).expect("metrics");
    assert_eq!(metrics.attempts.len(), 2);
    assert_eq!(metrics.total.requests, Some(2));
    assert_eq!(metrics.total.input_uncached, Some(10));
    assert_eq!(metrics.total.cache_read, Some(4));
    assert_eq!(metrics.total.output, Some(3));
    assert_eq!(
        metrics.total.reasoning, None,
        "a field no run reported is not 0"
    );
    assert_eq!(metrics.total.fee_micros, Some(1000));
    assert_eq!(metrics.total.unreported_runs, 1);
    assert_eq!(metrics.total.wall_secs, Some(10.0));
    let cost = metrics.total.total_cost_usd.expect("a reported cost");
    assert!((cost - 0.001).abs() < 1e-9, "the cost sum is 0.001: {cost}");
}

/// An attempt that still runs has no `Result` line: `attempt_metrics` returns the claim's
/// started time and not-reported fields, and no error. Mutation: require a `Result` to read an
/// attempt and this fails.
#[test]
fn a_running_attempt_reports_its_start_and_no_metrics() {
    let fixture = Fixture::new("running");
    let start = json!({
        "attempt_id": format!("{}-1", fixture.task),
        "token": "t",
        "started_at": "2026-01-01T00:00:00.000000Z",
        "session_id": "s",
        "worker": {"kind": "radian", "flow": "default"},
    });
    let dir = fixture.repo.attempt_dir(&fixture.task, 1);
    fs::create_dir_all(&dir).expect("the attempt directory");
    fs::write(dir.join("start.json"), start.to_string()).expect("start.json");
    fs::create_dir_all(fixture.run_dir(1)).expect("the run directory");

    let metrics = read::attempt_metrics(&fixture.store, &fixture.task, 1).expect("metrics");
    assert_eq!(
        metrics.worker.started_at.as_deref(),
        Some("2026-01-01T00:00:00.000000Z")
    );
    assert_eq!(metrics.worker.ended_at, None);
    assert_eq!(metrics.worker.wall_secs, None);
    assert_eq!(metrics.worker.metrics, None);
    assert_eq!(metrics.worker.provider, None);
    assert_eq!(metrics.cost_usd, None);
    assert!(metrics.reviews.is_empty());
    assert!(metrics.diff.is_none());
}

/// A torn or missing `diffstat.json` reads as not reported, and `show` still succeeds.
/// Mutation: propagate the parse error and `show` fails.
#[test]
fn a_torn_diffstat_reads_as_not_reported_and_show_succeeds() {
    let fixture = Fixture::new("torn-diffstat");
    write_jsonl(
        &fixture.run_dir(1).join("result.jsonl"),
        &[json!({
            "type": "Result",
            "outcome": "completed",
            "text": "done",
        })],
    );

    // A missing file first: the diff is not reported.
    let missing = read::attempt_metrics(&fixture.store, &fixture.task, 1).expect("metrics");
    assert!(missing.diff.is_none());

    // Then a torn one: it reads the same way.
    fs::write(fixture.run_dir(1).join("diffstat.json"), "{ not json").expect("the torn file");
    let torn = read::attempt_metrics(&fixture.store, &fixture.task, 1).expect("metrics");
    assert!(torn.diff.is_none());
    assert!(torn.increment.is_none());

    assert!(
        cli::show_text(&fixture.store, &fixture.task).is_ok(),
        "show exits 0 with a torn diffstat"
    );
    assert!(
        cli::show_json_text(&fixture.store, &fixture.task).is_ok(),
        "show --json exits 0 with a torn diffstat"
    );
}
