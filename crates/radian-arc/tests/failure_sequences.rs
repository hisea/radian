//! The model test of arc's automatic path (ADR 0048 decisions 9, 10, 12 and 13).
//!
//! One task is driven through the real store transitions with a scripted, repeating sequence
//! of outcomes: a worker success or one of the failure classes, a gate that passes, fails its
//! commands, fails its review (blocking or unusable), conflicts with main, or dies after
//! `running`; plus a bounded number of main moves and lead reworks. Every scripted sequence
//! must, with no further lead or owner action, reach one of the states the automatic path
//! leaves on its own:
//!
//! - `done.merged` (not produced here: the model runs at `auto` `run`, which stops at
//!   `review.awaiting_human`, but the check accepts it),
//! - `failed.*`,
//! - `todo.paused_budget`,
//! - `review.awaiting_human`, or a `review.rework_pending` that neither the lead nor the
//!   automatic path leaves or moves again,
//! - a paused plan (its `auto` is `off` and the task waits at `todo.ready` or
//!   `review.rework_pending`),
//! - `review.verifying` whose gate is stuck: it left no record or died once too often for the
//!   same inputs, and no gate process runs ([`background::stuck`]).
//!
//! The calls are the ones the pass makes: `attempt_ops::claim` and `attempt_ops::apply` for a
//! worker outcome, `review::finish_verification` for the verdicts a gate left, the run loop's
//! `record_auto_rework` hook after a failed verification, `review::auto_reverify` for a moved
//! main, and `background::count_death` for a gate that died. No rule is re-implemented; the
//! harness only chooses which real outcome happens next and writes the verdict a gate would
//! have written.
//!
//! The bound: a settling sequence consumes each counter at most once per round, and each
//! round is bounded by the others. With the limits set here (`max_attempts` 3,
//! `max_auto_rework` 2, `max_conflict_rework` 2, `max_auto_reverify` 2, `max_gate_deaths` 1)
//! the bound is `(max_auto_rework + 1) * (max_conflict_rework + 1) * (max_auto_reverify + 1)
//! * (max_gate_deaths + 2) + (max_attempts + 1) + lead reworks + main moves + 16` = 3 * 3 * 3
//! * 3 + 4 + 2 + 2 + 16 = 105 steps. A sequence that reaches it is a loop: the automatic
//!   path is making transitions without reaching a state it leaves.

mod common;

use std::collections::HashSet;
use std::fs;

use common::*;
use radian_arc::gate::background;
use radian_arc::gate::{
    ReviewItem, ReviewReport, Stage, VERDICT_SCHEMA_VERSION, Verdict, VerdictResult, now_utc,
    verdict_path,
};
use radian_arc::review::{self, AutoReverify};
use radian_arc::store::attempt_ops::{self, Class, Finish};
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, plan_ops};
use serde_json::{Value, json};

/// A retry time in the far future: `release_retries` treats every past-due retry as due, so
/// the harness does not wait out the backoff. The scheduling loop's wait is not what this test
/// models.
const ALL_RETRIES_DUE: &str = "9999-12-31T23:59:59Z";
/// The fixed seed: a failure reproduces byte for byte.
const SEED: u64 = 0x5EED_0052;
/// How many generated sequences run after the hand-written coverage ones.
const GENERATED: usize = 32;

/// One outcome the worker or the gate can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Event {
    // Worker outcomes. Each maps to one failure class of decision 9.
    Success,
    DeliveryFail,
    ModelFail,
    InfrastructureFail,
    LockedFail,
    Poisoned,
    Cancel,
    Stall,
    WallTime,
    // Gate outcomes.
    GatePass,
    CommandsFail,
    CommandsTimeout,
    ReviewBlocking,
    ReviewOutput,
    Conflict,
    GateDeath,
}

impl Event {
    fn is_worker(self) -> bool {
        matches!(
            self,
            Event::Success
                | Event::DeliveryFail
                | Event::ModelFail
                | Event::InfrastructureFail
                | Event::LockedFail
                | Event::Poisoned
                | Event::Cancel
                | Event::Stall
                | Event::WallTime
        )
    }

    fn is_gate(self) -> bool {
        !self.is_worker()
    }

    const ALL: [Event; 16] = [
        Event::Success,
        Event::DeliveryFail,
        Event::ModelFail,
        Event::InfrastructureFail,
        Event::LockedFail,
        Event::Poisoned,
        Event::Cancel,
        Event::Stall,
        Event::WallTime,
        Event::GatePass,
        Event::CommandsFail,
        Event::CommandsTimeout,
        Event::ReviewBlocking,
        Event::ReviewOutput,
        Event::Conflict,
        Event::GateDeath,
    ];
}

/// A tiny deterministic generator (xorshift64*), so the seed is the whole record and no
/// dependency is added for it.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// One scripted sequence: a repeating cycle of worker and gate outcomes, and a bounded number
/// of main moves and lead reworks.
#[derive(Debug, Clone)]
struct Script {
    events: Vec<Event>,
    cursor: usize,
    lead_reworks: u32,
    main_moves: u32,
}

impl Script {
    fn new(events: Vec<Event>, lead_reworks: u32, main_moves: u32) -> Self {
        assert!(
            events.iter().any(|event| event.is_worker()),
            "a cycle needs a worker outcome: {events:?}"
        );
        assert!(
            events.iter().any(|event| event.is_gate()),
            "a cycle needs a gate outcome: {events:?}"
        );
        Self {
            events,
            cursor: 0,
            lead_reworks,
            main_moves,
        }
    }

    /// The next event the predicate accepts, from the cursor and wrapping. The cursor moves
    /// past it, so the cycle repeats in order.
    fn next_event(&mut self, accept: impl Fn(Event) -> bool) -> Option<Event> {
        let length = self.events.len();
        for offset in 0..length {
            let index = (self.cursor + offset) % length;
            let event = self.events[index];
            if accept(event) {
                self.cursor = (index + 1) % length;
                return Some(event);
            }
        }
        None
    }

    fn describe(&self) -> String {
        format!(
            "events {:?}, {} lead rework(s), {} main move(s)",
            self.events, self.lead_reworks, self.main_moves
        )
    }
}

/// The limits the sequences run under, read from the config the harness wrote.
struct Limits {
    max_attempts: u32,
    max_auto_rework: u32,
    max_conflict_rework: u32,
    max_auto_reverify: u32,
    max_gate_deaths: u32,
}

impl Limits {
    fn read(store: &Store) -> Self {
        let limits = store.config().expect("the config reads").limits;
        Self {
            max_attempts: limits.max_attempts,
            max_auto_rework: limits.max_auto_rework,
            max_conflict_rework: limits.max_conflict_rework,
            max_auto_reverify: limits.max_auto_reverify,
            max_gate_deaths: limits.max_gate_deaths,
        }
    }

    /// The step bound the module comment derives from these limits.
    fn bound(&self, script: &Script) -> usize {
        let rounds = (self.max_auto_rework as usize + 1)
            * (self.max_conflict_rework as usize + 1)
            * (self.max_auto_reverify as usize + 1)
            * (self.max_gate_deaths as usize + 2);
        rounds
            + self.max_attempts as usize
            + 1
            + script.lead_reworks as usize
            + script.main_moves as usize
            + 16
    }
}

/// The failure classes the harness produces, so coverage is checked rather than assumed.
fn classes_are_covered(seen: &HashSet<Event>) {
    for event in Event::ALL {
        assert!(
            seen.contains(&event),
            "the sequences never produced {event:?}"
        );
    }
}

/// One stage's verdict, as the harness writes it.
#[derive(Clone, Copy)]
struct StageDetail<'a> {
    pass: bool,
    reason: Option<&'a str>,
    blocking: &'a [&'a str],
    conflict_paths: &'a [&'a str],
}

const PASS: StageDetail<'static> = StageDetail {
    pass: true,
    reason: None,
    blocking: &[],
    conflict_paths: &[],
};

fn fail(reason: &'static str) -> StageDetail<'static> {
    StageDetail {
        pass: false,
        reason: Some(reason),
        ..PASS
    }
}

/// Writes the verdict a gate would have written for one stage. `write_new` refuses a second
/// write, so a harness bug that reruns an attempt's stage surfaces here.
fn write_verdict(
    store: &Store,
    task: &str,
    label: &str,
    stage: Stage,
    head: &str,
    main: &str,
    detail: StageDetail<'_>,
) {
    let at = now_utc();
    let mut verdict = Verdict {
        schema_version: VERDICT_SCHEMA_VERSION,
        stage,
        sha: head.to_owned(),
        main_sha: Some(main.to_owned()),
        result: if detail.pass {
            VerdictResult::Pass
        } else {
            VerdictResult::Fail
        },
        reason: detail.reason.map(str::to_owned),
        commands: Vec::new(),
        started_at: at.clone(),
        ended_at: at,
        base_sha: None,
        candidate_sha: Some(head.to_owned()),
        conflict_paths: detail
            .conflict_paths
            .iter()
            .map(|path| (*path).to_owned())
            .collect(),
        approver: None,
        review: (stage == Stage::Review).then(|| ReviewReport {
            summary: "the model test's review".to_owned(),
            blocking: detail
                .blocking
                .iter()
                .map(|text| ReviewItem {
                    level: Some("major".to_owned()),
                    file: Some("file.txt".to_owned()),
                    line: Some(1),
                    text: (*text).to_owned(),
                    pre_existing: false,
                })
                .collect(),
            suggestions: Vec::new(),
            verified: Vec::new(),
        }),
        review_session_ids: Vec::new(),
        review_cost_micros: None,
        review_pre_existing: None,
        review_settings: None,
    };
    if stage == Stage::Review && !detail.pass && detail.reason == Some("review_output") {
        // An unusable report left no parsed findings.
        verdict.review = None;
    }
    let path = verdict_path(store.repo(), task, label, stage).expect("a verdict path");
    verdict
        .write_new(&path)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
}

fn conflict_reworks(store: &Store, task: &str) -> u32 {
    let path = store
        .root()
        .join("runs")
        .join(task)
        .join(review::CONFLICT_REWORK_FILE);
    let Ok(text) = fs::read_to_string(path) else {
        return 0;
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| value.get("reworks").and_then(Value::as_u64))
        .map_or(0, |count| u32::try_from(count).unwrap_or(u32::MAX))
}

fn auto_reverifies(store: &Store, task: &str) -> usize {
    let path = store
        .root()
        .join("runs")
        .join(task)
        .join(review::AUTO_REVERIFY_FILE);
    let Ok(text) = fs::read_to_string(path) else {
        return 0;
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .get("reverified")
                .and_then(Value::as_array)
                .map(Vec::len)
        })
        .unwrap_or(0)
}

/// Writes a gate record as the gate process would leave it. A `running` record names a pid the
/// harness owns no process for: `background::running` reads the lock, so a free lock is what
/// makes the pass call it a death.
fn write_gate_record(store: &Store, task: &str, value: Value) {
    let dir = store.root().join("runs").join(task);
    fs::create_dir_all(&dir).expect("the run directory");
    fs::write(dir.join(background::GATE_FILE), value.to_string()).expect("the gate record");
}

fn clear_gate_record(store: &Store, task: &str) {
    let _ = fs::remove_file(
        store
            .root()
            .join("runs")
            .join(task)
            .join(background::GATE_FILE),
    );
}

/// The checks every sequence asserts after each step: no counter passes its limit, and a
/// counter that has tripped it has stopped the automatic path.
fn check_counters(store: &Store, task: &str, limits: &Limits) {
    let item = store.load_task(task).expect("the task reads").item;
    let plan = store.load_plan(&item.plan).expect("the plan reads").item;
    assert!(
        item.retry_count <= item.max_attempts.unwrap_or(limits.max_attempts),
        "retry_count {} passed max_attempts: {item:?}",
        item.retry_count
    );
    assert!(
        conflict_reworks(store, task) <= limits.max_conflict_rework,
        "{} conflict reworks passed max_conflict_rework {}",
        conflict_reworks(store, task),
        limits.max_conflict_rework
    );
    assert!(
        auto_reverifies(store, task) <= limits.max_auto_reverify as usize,
        "{} automatic reverifies passed max_auto_reverify {}",
        auto_reverifies(store, task),
        limits.max_auto_reverify
    );
    assert!(
        item.auto_rework <= limits.max_auto_rework + 1,
        "auto_rework {} is more than one step past max_auto_rework {}",
        item.auto_rework,
        limits.max_auto_rework
    );
    if item.auto_rework > limits.max_auto_rework {
        assert_eq!(
            plan.auto, "off",
            "auto_rework {} tripped max_auto_rework but the plan did not pause",
            item.auto_rework
        );
    }
    if let Some(record) = background::read(store, task) {
        assert!(
            record.deaths <= limits.max_gate_deaths + 1,
            "gate deaths {} passed max_gate_deaths {} + 1: {record:?}",
            record.deaths,
            limits.max_gate_deaths
        );
        if record.status == background::DIED {
            assert!(
                record.deaths > limits.max_gate_deaths,
                "a died record without the count that stopped it: {record:?}"
            );
        }
    }
}

/// Commits one file on `branch` at `base` in the fixture clone and returns the commit id.
fn commit_on(repo: &Repo, branch: &str, base: &str, file: &str, content: &str) -> String {
    repo.git(&["checkout", "-B", branch, base]);
    write(&repo.clone.join(file), content);
    repo.git(&["add", file]);
    repo.git(&["commit", "-m", &format!("{branch}: {file}")]);
    repo.git(&["rev-parse", "HEAD"]).trim().to_owned()
}

/// Publishes a local ref on the fixture's remote.
fn push_ref(repo: &Repo, refspec: &str) {
    repo.git(&["push", "origin", refspec]);
}

/// Moves the remote main to the other fixture commit and returns the new sha.
fn move_main(repo: &Repo, current: &str, one: &str, other: &str) -> String {
    let next = if current == one { other } else { one };
    repo.git(&[
        "push",
        "--force",
        "origin",
        &format!("{next}:refs/heads/main"),
    ]);
    next.to_owned()
}

/// Applies one gate outcome (except a death) and returns the verification's state.
fn apply_gate_outcome(
    store: &Store,
    task: &str,
    event: Event,
    main: &str,
) -> Result<(), radian_arc::store::Error> {
    let plan = review::plan_verification(store, task)?;
    let head = plan.head.clone();
    let label = plan.label.clone();
    match event {
        Event::GatePass => {
            write_verdict(store, task, &label, Stage::Commands, &head, main, PASS);
            write_verdict(store, task, &label, Stage::Review, &head, main, PASS);
        }
        Event::CommandsFail | Event::CommandsTimeout => {
            let reason = if event == Event::CommandsFail {
                "command_failed"
            } else {
                "timeout"
            };
            write_verdict(
                store,
                task,
                &label,
                Stage::Commands,
                &head,
                main,
                fail(reason),
            );
            write_verdict(store, task, &label, Stage::Review, &head, main, PASS);
        }
        Event::ReviewBlocking => {
            write_verdict(store, task, &label, Stage::Commands, &head, main, PASS);
            write_verdict(
                store,
                task,
                &label,
                Stage::Review,
                &head,
                main,
                StageDetail {
                    blocking: &["the brief is not done"],
                    ..fail("review_blocking")
                },
            );
        }
        Event::ReviewOutput => {
            write_verdict(store, task, &label, Stage::Commands, &head, main, PASS);
            write_verdict(
                store,
                task,
                &label,
                Stage::Review,
                &head,
                main,
                fail("review_output"),
            );
        }
        Event::Conflict => {
            write_verdict(
                store,
                task,
                &label,
                Stage::Commands,
                &head,
                main,
                StageDetail {
                    conflict_paths: &["feature.txt"],
                    ..fail("conflict")
                },
            );
            write_verdict(store, task, &label, Stage::Review, &head, main, PASS);
        }
        other => panic!("{other:?} is not a gate outcome"),
    }
    // The gate process exits after its verdicts: the pass reads a `done` record, clears it and
    // applies the verdicts. The plan is the one read above, so the harness does not fetch the
    // remote a second time; `finish_verification` is the pass's own state move.
    clear_gate_record(store, task);
    let verified = review::finish_verification(store, &plan)?;
    // `run.rs`'s hook: a verification that ended in `review.rework_pending` without a conflict
    // counts one automatic rework (`review_auto_rework` already counted its own pause).
    if verified.phase == "rework_pending"
        && verified.conflict.is_none()
        && verified.paused.is_none()
    {
        attempt_ops::record_auto_rework(store, task)?;
    }
    Ok(())
}

/// Applies one gate death: writes the `running` record a gate leaves, then asks the real
/// death counter what the pass would do. `true` when the gate is `died` and nothing restarts.
fn apply_gate_death(store: &Store, task: &str) -> Result<bool, radian_arc::store::Error> {
    let plan = review::plan_verification(store, task)?;
    let label = plan.label.clone();
    let inputs = plan.inputs.clone();
    let previous = background::read(store, task)
        .filter(|record| record.attempt == label && record.inputs.as_deref() == Some(&inputs));
    let deaths = previous.as_ref().map_or(0, |record| record.deaths);
    write_gate_record(
        store,
        task,
        json!({
            "kind": "verify",
            "attempt": label,
            "status": background::RUNNING,
            "pid": 999_999_999u32,
            "process_started_at": "",
            "inputs": inputs,
            "deaths": deaths,
            "started_at": now_utc(),
        }),
    );
    let record = background::read(store, task).expect("the recorded death");
    match background::count_death(store, task, "verify", &label, &record, Some(&inputs))? {
        background::Death::Stop => Ok(true),
        // The restarted gate keeps the count in its `running` record, so the next death adds
        // to it.
        background::Death::Count(count) => {
            write_gate_record(
                store,
                task,
                json!({
                    "kind": "verify",
                    "attempt": label,
                    "status": background::RUNNING,
                    "pid": 999_999_999u32,
                    "process_started_at": "",
                    "inputs": inputs,
                    "deaths": count,
                    "started_at": now_utc(),
                }),
            );
            Ok(false)
        }
        background::Death::Restart => Ok(false),
    }
}

/// Claims one attempt and applies the scripted worker outcome.
fn apply_worker_outcome(
    store: &Store,
    task: &str,
    event: Event,
    base: &str,
    head: &str,
) -> Result<(), radian_arc::store::Error> {
    let claim = attempt_ops::claim(store, task)?;
    let token = store
        .load_task(task)
        .expect("the task reads")
        .item
        .token
        .expect("the claim wrote a token");
    let (outcome, pushed, failure, exit, signal) = match event {
        Event::Success => ("completed", true, None, 0, None),
        Event::DeliveryFail => (
            "completed",
            false,
            Some("uncommitted or untracked files: dirty.txt"),
            1,
            None,
        ),
        Event::ModelFail => ("failed", false, None, 1, None),
        Event::InfrastructureFail => ("terminated", false, None, 1, None),
        Event::LockedFail => ("failed", false, None, 4, None),
        Event::Poisoned => ("poisoned", false, None, 1, None),
        Event::Cancel => ("cancelled", false, None, 1, Some(Class::Cancel)),
        Event::Stall => ("terminated", false, None, 1, Some(Class::Stall)),
        Event::WallTime => ("terminated", false, None, 1, Some(Class::WallTime)),
        other => panic!("{other:?} is not a worker outcome"),
    };
    let finish = Finish {
        attempt_id: claim.attempt_id.clone(),
        token,
        exit_code: exit,
        outcome: outcome.to_owned(),
        base_sha: Some(base.to_owned()),
        head_sha: Some(head.to_owned()),
        pushed,
        failure: failure.map(str::to_owned),
    };
    let applied = attempt_ops::apply(
        store,
        task,
        claim.attempt,
        Some(&finish),
        signal,
        Some(exit),
    )?;
    assert!(
        applied.ignored.is_none(),
        "{}",
        applied.ignored.unwrap_or_default()
    );
    // An attempt that never reached the gate did not conflict with main, so its run of
    // conflict reworks is over (decision 9). This is the rule mutation TK0052-M1 breaks.
    if !(applied.state == "review" && applied.phase == "verifying") {
        assert_eq!(
            conflict_reworks(store, task),
            0,
            "an attempt that never reached the gate left the conflict run at {}",
            conflict_reworks(store, task)
        );
    }
    Ok(())
}

/// The outcome of one scripted sequence.
struct Outcome {
    task: String,
    terminal: String,
}

/// The fixture one sequence runs against: the shared store and remote, the limits, and the
/// commits a main move goes between.
struct Fixture<'a> {
    repo: &'a Repo,
    store: &'a Store,
    limits: &'a Limits,
    base: &'a str,
    main_one: &'a str,
    main_two: &'a str,
}

/// Drives one task through `script` until a terminal state. Panics with the sequence and the
/// step log when the bound is reached first.
fn run_sequence(
    fixture: &Fixture<'_>,
    sequence: usize,
    head: &str,
    script: &mut Script,
    seen: &mut HashSet<Event>,
) -> Outcome {
    let repo = fixture.repo;
    let store = fixture.store;
    let limits = fixture.limits;
    let base = fixture.base;
    let main_one = fixture.main_one;
    let main_two = fixture.main_two;
    let plan = store
        .add_plan(&format!("seq{sequence}"), "A plan", "the request\n")
        .expect("add a plan");
    let task = store
        .add_task(
            &plan,
            "work",
            "A task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .expect("add a task");
    plan_ops::confirm(store, &plan).expect("confirm");
    plan_ops::set_auto(store, &plan, "run").expect("auto run");
    push_ref(repo, &format!("seq-{sequence}:refs/heads/work/{task}"));
    let mut main = base.to_owned();
    let bound = limits.bound(script);
    let mut log = Vec::new();

    for step in 0..bound {
        let item = store.load_task(&task).expect("the task reads").item;
        let task_plan = store.load_plan(&item.plan).expect("the plan reads").item;
        let (state, phase) = (item.state, item.phase.clone());

        // The pass's step 2: a due retry is ready again. The harness's clock is "now".
        if state == TaskState::Todo && phase == "retry_wait" {
            attempt_ops::release_retries(store, ALL_RETRIES_DUE).expect("release the retry");
            log.push(format!("{step}: retry released"));
            check_counters(store, &task, limits);
            continue;
        }

        // A plan the automatic path paused starts nothing more.
        if state == TaskState::Todo && phase == "ready" && task_plan.auto == "off" {
            return Outcome {
                task: task.clone(),
                terminal: "a paused plan".to_owned(),
            };
        }

        match (state, phase.as_str()) {
            (TaskState::Todo, "ready") => {
                let event = script
                    .next_event(Event::is_worker)
                    .expect("the cycle has a worker outcome");
                seen.insert(event);
                apply_worker_outcome(store, &task, event, base, head).expect("the worker outcome");
                log.push(format!("{step}: {event:?} -> {}", describe(store, &task)));
            }
            (TaskState::Review, "verifying") => {
                let event = script
                    .next_event(Event::is_gate)
                    .expect("the cycle has a gate outcome");
                seen.insert(event);
                if event == Event::GateDeath {
                    let died = apply_gate_death(store, &task).expect("the gate death");
                    log.push(format!("{step}: GateDeath -> {}", describe(store, &task)));
                    if died {
                        return Outcome {
                            task: task.clone(),
                            terminal: "review.verifying with a stuck gate".to_owned(),
                        };
                    }
                } else {
                    apply_gate_outcome(store, &task, event, &main).expect("the gate outcome");
                    log.push(format!("{step}: {event:?} -> {}", describe(store, &task)));
                }
            }
            (TaskState::Review, "rework_pending") => {
                // The pass's step 3: a moved main re-verifies a command failure by itself.
                match review::auto_reverify(store, &task).expect("auto reverify") {
                    AutoReverify::Started { .. } | AutoReverify::Limit { .. } => {
                        log.push(format!(
                            "{step}: auto reverify -> {}",
                            describe(store, &task)
                        ));
                        check_counters(store, &task, limits);
                        continue;
                    }
                    AutoReverify::Not => {}
                }
                if script.main_moves > 0 {
                    main = move_main(repo, &main, main_one, main_two);
                    script.main_moves -= 1;
                    log.push(format!("{step}: main moved to {}", &main[..7]));
                    continue;
                }
                if script.lead_reworks > 0 {
                    plan_ops::add_rework(store, &task, "one more round").expect("the lead rework");
                    script.lead_reworks -= 1;
                    log.push(format!("{step}: lead rework -> {}", describe(store, &task)));
                    check_counters(store, &task, limits);
                    continue;
                }
                return Outcome {
                    task: task.clone(),
                    terminal: "review.rework_pending".to_owned(),
                };
            }
            (TaskState::Review, "awaiting_human") => {
                return Outcome {
                    task: task.clone(),
                    terminal: "review.awaiting_human".to_owned(),
                };
            }
            (TaskState::Todo, "paused_budget") => {
                return Outcome {
                    task: task.clone(),
                    terminal: "todo.paused_budget".to_owned(),
                };
            }
            (TaskState::Done, "merged") => {
                return Outcome {
                    task: task.clone(),
                    terminal: "done.merged".to_owned(),
                };
            }
            (TaskState::Failed, failed) => {
                return Outcome {
                    task: task.clone(),
                    terminal: format!("failed.{failed}"),
                };
            }
            (state, phase) => panic!(
                "sequence {sequence} ({}) stopped at an unexpected {}.{} after {step} steps:\n{}",
                script.describe(),
                state.as_str(),
                phase,
                log.join("\n")
            ),
        }
        check_counters(store, &task, limits);
    }

    panic!(
        "sequence {sequence} ({}) did not settle in {bound} steps (the bound derived from the \
         limits); the automatic path is looping:\n{}",
        script.describe(),
        log.join("\n")
    )
}

fn describe(store: &Store, task: &str) -> String {
    let item = store.load_task(task).expect("the task reads").item;
    let plan = store.load_plan(&item.plan).expect("the plan reads").item;
    format!(
        "{}.{} (attempt {}, retry {}, auto rework {}, conflicts {}, reverifies {}, plan auto {})",
        item.state.as_str(),
        item.phase,
        item.attempt,
        item.retry_count,
        item.auto_rework,
        conflict_reworks(store, task),
        auto_reverifies(store, task),
        plan.auto
    )
}

/// The config the sequences run under: the `commands` and `review` stages, short limits, and
/// `auto` `run` (the model never merges).
fn configure(repo: &Repo) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    let text = text
        .replace(
            "stages = [{ stage = \"commands\", required = true }, { stage = \"human\", \
             required = true }]",
            "stages = [{ stage = \"commands\", required = true }, { stage = \"review\", \
             required = true }, { stage = \"human\", required = true }]",
        )
        .replace("max_conflict_rework = 3", "max_conflict_rework = 2")
        .replace("max_gate_deaths = 2", "max_gate_deaths = 1");
    fs::write(&path, text).expect("write config.toml");
}

/// The hand-written cycles: every event and every counter is reached at least once before the
/// generated ones run. They also carry the two scripted side events.
fn targeted_scripts() -> Vec<Script> {
    let mut scripts = Vec::new();
    let no_side = |events: Vec<Event>| Script::new(events, 0, 0);
    scripts.push(no_side(vec![Event::Success, Event::GatePass]));
    scripts.push(no_side(vec![Event::Success, Event::CommandsFail]));
    scripts.push(no_side(vec![Event::Success, Event::CommandsTimeout]));
    scripts.push(no_side(vec![Event::Success, Event::ReviewBlocking]));
    scripts.push(no_side(vec![Event::Success, Event::ReviewOutput]));
    scripts.push(no_side(vec![Event::Success, Event::Conflict]));
    scripts.push(no_side(vec![Event::Success, Event::GateDeath]));
    for worker in [
        Event::DeliveryFail,
        Event::ModelFail,
        Event::InfrastructureFail,
        Event::LockedFail,
        Event::Poisoned,
        Event::Cancel,
        Event::Stall,
        Event::WallTime,
    ] {
        scripts.push(no_side(vec![worker, Event::GatePass]));
    }
    // A command failure that main moving fixes, until `max_auto_reverify` stops the step.
    scripts.push(Script::new(vec![Event::Success, Event::CommandsFail], 0, 2));
    // A review rework until `max_auto_rework` pauses the plan, then the lead's one rework.
    scripts.push(Script::new(
        vec![Event::Success, Event::ReviewBlocking],
        1,
        0,
    ));
    // A conflict and a failed retry: before a conflict rework kept the retry count, the two
    // limits reset each other and this cycle never ended.
    scripts.push(Script::new(
        vec![Event::Success, Event::Conflict, Event::ModelFail],
        1,
        0,
    ));
    scripts
}

/// A generated sequence: a cycle with at least one worker and one gate outcome, and up to two
/// main moves and lead reworks.
fn generated_script(rng: &mut Rng) -> Script {
    let length = 1 + rng.below(6);
    let mut events: Vec<Event> = (0..length)
        .map(|_| Event::ALL[rng.below(Event::ALL.len())])
        .collect();
    if !events.iter().any(|event| event.is_worker()) {
        events.push(Event::ALL[rng.below(9)]);
    }
    if !events.iter().any(|event| event.is_gate()) {
        events.push(Event::ALL[9 + rng.below(Event::ALL.len() - 9)]);
    }
    let lead_reworks = rng.below(3) as u32;
    let main_moves = rng.below(3) as u32;
    Script::new(events, lead_reworks, main_moves)
}

/// The model test. Mutation TK0052-M1 (do not reset the conflict run on an attempt that never
/// reached the gate) fails the per-attempt assertion; TK0052-M2 (`max_auto_rework` never
/// pauses the plan) fails `check_counters` or the terminal assertion.
#[test]
fn every_automatic_failure_sequence_ends_within_the_limits() {
    let repo = Repo::new("failure-sequences");
    Store::init(&repo.clone, false).expect("arc init");
    configure(&repo);
    let store = Store::open(&repo.clone).expect("the store opens");
    let limits = Limits::read(&store);
    assert_eq!(
        (
            limits.max_attempts,
            limits.max_auto_rework,
            limits.max_conflict_rework
        ),
        (3, 2, 2),
        "the bound in the module comment is written for these limits"
    );
    assert_eq!(
        (limits.max_auto_reverify, limits.max_gate_deaths),
        (2, 1),
        "the bound in the module comment is written for these limits"
    );

    // Two commits of the fixture: a main move goes from one to the other and back, so the
    // wire main always changes and every move is a new `(head_sha, main)` input.
    let base = repo.remote_main_sha();
    let main_one = commit_on(&repo, "main-one", &base, "main-one.txt", "one\n");
    let main_two = commit_on(&repo, "main-two", &base, "main-two.txt", "two\n");

    let mut rng = Rng(SEED);
    let mut scripts = targeted_scripts();
    for _ in 0..GENERATED {
        scripts.push(generated_script(&mut rng));
    }

    let mut seen = HashSet::new();
    let mut outcomes = Vec::new();
    let fixture = Fixture {
        repo: &repo,
        store: &store,
        limits: &limits,
        base: &base,
        main_one: &main_one,
        main_two: &main_two,
    };
    for (index, mut script) in scripts.into_iter().enumerate() {
        let head = commit_on(
            &repo,
            &format!("seq-{index}"),
            &base,
            &format!("seq-{index}.txt"),
            &format!("change {index}\n"),
        );
        let outcome = run_sequence(&fixture, index, &head, &mut script, &mut seen);
        assert!(
            !outcome.terminal.is_empty(),
            "sequence {index} reported an empty terminal state"
        );
        let task = outcome.task.clone();
        outcomes.push((index, outcome));
        // Delete the remote branch, not the local one: the next sequence checks out a new
        // branch from the base anyway, and a short remote keeps each gate's fetch cheap.
        let _ = repo.git(&["push", "origin", &format!(":refs/heads/work/{task}")]);
    }
    classes_are_covered(&seen);
    // The suite is not vacuous: some sequences settle at every kind of terminal state the
    // automatic path reaches here.
    let terminals: HashSet<&str> = outcomes
        .iter()
        .map(|(_, outcome)| outcome.terminal.as_str())
        .collect();
    for expected in [
        "review.awaiting_human",
        "review.rework_pending",
        "failed.exhausted",
        "failed.irrecoverable",
        "failed.cancelled",
        "a paused plan",
        "review.verifying with a stuck gate",
        "todo.paused_budget",
    ] {
        if expected == "todo.paused_budget" {
            // No sequence sets a cost budget, so this terminal is accepted but not produced.
            continue;
        }
        assert!(
            terminals.contains(expected),
            "no sequence reached {expected}: {terminals:?}"
        );
    }
}
