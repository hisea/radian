//! The `review` stage (ADR 0048 decision 10).
//!
//! `review` is a read-only radian session, not the worker session. One run is one
//! `radian exec` in `gates/review/`, on the verification candidate, mounted with exactly the
//! `read`, `grep` and `glob` tools ([`REVIEW_TOOLS`]): no shell, no write, and no MCP server
//! (the implicit sources are muted and no `--mcp-config` is passed). The session's model route
//! comes from `[gate.review] exec_args`.
//!
//! The prompt is built in ([`build_prompt`]): the task's brief, the commit list and the diff
//! from the merge base of the remote main and `verified_sha` to `verified_sha` (the task's own
//! change, not main's), and the instruction to check the change against the brief and the
//! repository's rules. A configured `prompt_file` is appended.
//!
//! A review that follows an earlier review of the same task is a rework round (ADR 0048
//! decision 10): its prompt holds the brief, the previous review's blocking items, the commit
//! list `<previous sha>..<sha>` and the increment, the lines the rework itself added or changed
//! ([`build_rework_prompt`]). After its report is parsed, arc moves a blocking item to
//! `suggestions` and marks it `pre_existing` ([`ReworkRound`]) unless it is within
//! [`PRE_EXISTING_WINDOW`] lines of a line the task's own change added or changed (not the
//! rework's increment), within [`PREVIOUS_ITEM_WINDOW`] lines of a blocking item of the previous
//! review in the same file, a `critical` item, or an item without a `file` or a `line`; the
//! stage then passes when no blocking item is left.
//!
//! The session ends with one fenced `json` block ([`ReviewReport`]). arc parses the last such
//! block. An empty `blocking` list passes the stage; any item fails it with reason
//! `review_blocking`. No parsable block, a missing field, a timeout, or a session error makes
//! the stage run the session once more; when the second run is unusable too the stage fails
//! with `review_output` (or `review_timeout` when the last run timed out). The verdict records
//! every run's session id and their summed cost.
//!
//! A task whose own change is empty starts no session: the stage passes and its report says
//! that there is nothing to review (ADR 0048 decision 10).

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::checkout::GateCheckout;
use super::commands::kill_group;
use super::config::Stage;
use super::error::GateError;
use super::exclusive::SlotLock;
use super::integration::candidate_for;
use super::verdict::{
    REVIEW_LEVELS, ReviewItem, ReviewReport, ReviewSettings, VERDICT_SCHEMA_VERSION, Verdict,
    VerdictResult, attempt_dir, now_utc, verdict_path,
};

/// The prefix of the review stage's slot locks under `gates/`: `review-0.lock` …
/// `review-<concurrency-1>.lock` (ADR 0048 decision 10). A slot number also names that
/// slot's checkout, `gates/review-<n>/`.
pub(crate) const REVIEW_SLOT: &str = "review";

/// The built-in tools a review session mounts. The one closed list the stage, the tests and
/// the manual read: a name that is not here is not mounted at all (`radian exec --tools`).
pub const REVIEW_TOOLS: &str = "read,grep,glob";

/// How many times one stage may run the session: once, and once more when the first run's
/// output is unusable (ADR 0048 decision 10).
pub const REVIEW_RUNS: u32 = 2;

/// The item level whose item always blocks a rework round, wherever it is (ADR 0048 decision
/// 10): the window rules never move it.
const CRITICAL: &str = "critical";

/// The report of a review of a task whose own change is empty (ADR 0048 decision 10): no
/// session starts, and the verdict's summary says why the stage passed.
const EMPTY_CHANGE_SUMMARY: &str = "The task's own change is empty: its tree is the remote \
     main's tree. There is nothing for the review to check, so the stage passes without \
     starting a session.";

/// A rework round: how far a blocking item may sit from a line the task's own change added or
/// changed and still block (ADR 0048 decision 10, rule (a)).
pub const PRE_EXISTING_WINDOW: u32 = 3;

/// A rework round: how far a blocking item may sit from a blocking item of the previous review
/// and still block (rule (b)), so an unfixed earlier item stays blocking.
pub const PREVIOUS_ITEM_WINDOW: u32 = 10;

/// The review a rework round follows (ADR 0048 decision 10): the previous review's commit and
/// the blocking items it reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousReview {
    /// The commit the previous review verified.
    pub sha: String,
    /// The blocking items it reported; empty when its output was unusable.
    pub blocking: Vec<ReviewItem>,
}

/// One rework round's input: the previous review and the lines the task's own change added or
/// changed.
#[derive(Debug, Clone)]
struct ReworkRound {
    previous: PreviousReview,
    /// The new-side lines of the task's own diff, `merge-base(main, sha)..sha`: the lines the
    /// task itself added or changed. `(path, line)`.
    changed: BTreeSet<(String, u32)>,
}

impl ReworkRound {
    /// Whether a blocking item stays blocking (ADR 0048 decision 10, the rework round's
    /// exceptions): a `critical` item always does, wherever it is; an item without a `file`
    /// or a `line` does too, at its level; any other item does when its `file:line` is within
    /// [`PRE_EXISTING_WINDOW`] lines of a line the task's own change added or changed, or it is
    /// in the same file as and within [`PREVIOUS_ITEM_WINDOW`] lines of a blocking item of the
    /// previous review.
    fn keeps(&self, item: &ReviewItem) -> bool {
        // A critical item blocks in every round, wherever it is: the position of a line does
        // not prove that a problem is harmless or old.
        if item.level.as_deref() == Some(CRITICAL) {
            return true;
        }
        // An item without a place is not demoted: the review prompt asks for `file` and
        // `line`, so a missing one is the session's omission, not evidence that the problem
        // is old.
        let (Some(file), Some(line)) = (&item.file, item.line) else {
            return true;
        };
        let near_change = self
            .changed
            .iter()
            .any(|(path, at)| path == file && at.abs_diff(line) <= PRE_EXISTING_WINDOW);
        let near_previous = self.previous.blocking.iter().any(|earlier| {
            earlier.file.as_deref() == Some(file.as_str())
                && earlier
                    .line
                    .is_some_and(|at| at.abs_diff(line) <= PREVIOUS_ITEM_WINDOW)
        });
        near_change || near_previous
    }

    /// Moves every blocking item [`ReworkRound::keeps`] does not keep to `suggestions`, marks
    /// it `pre_existing`, and returns how many it moved. A moved item keeps its `level`, so
    /// the follow-up task carries it at that level.
    fn mark_pre_existing(&self, report: &mut ReviewReport) -> u32 {
        let mut kept = Vec::new();
        let mut moved = 0;
        for item in std::mem::take(&mut report.blocking) {
            if self.keeps(&item) {
                kept.push(item);
            } else {
                moved += 1;
                report.suggestions.push(ReviewItem {
                    pre_existing: true,
                    ..item
                });
            }
        }
        report.blocking = kept;
        moved
    }
}

/// The `radian exec` arguments the stage sets itself; `[gate.review] exec_args` may not
/// repeat one, because the stage's value would be overridden (or the flag given twice). One
/// closed list: the check and the manual's drift test read it.
pub const OWNED_EXEC_ARGS: &[&str] = &[
    "--tools",
    "--session-id",
    "--workspace",
    "--profile",
    "--resume",
    "--deny-read",
    "--mcp-config",
    "--yes",
    "--output",
    "--no-config-mcp",
    "-p",
];

/// The failure reason of one unusable run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unusable {
    Output,
    Timeout,
}

impl Unusable {
    fn reason(self) -> &'static str {
        match self {
            Unusable::Output => "review_output",
            Unusable::Timeout => "review_timeout",
        }
    }
}

/// The `review` stage for one attempt and one sha.
#[derive(Debug, Clone)]
pub struct ReviewRun<'a> {
    /// The repository root.
    pub repository: &'a Path,
    /// The repository's remote name, for the fetches before the checkout.
    pub remote: &'a str,
    /// The task id (one path component).
    pub task: &'a str,
    /// The attempt id (one path component).
    pub attempt: &'a str,
    /// The task's `verified_sha`: the verdict's `sha` and the change the session reviews.
    pub sha: &'a str,
    /// The task's brief, as the worker got it.
    pub brief: &'a str,
    /// The ref that names the remote main after a fetch, typically
    /// `refs/remotes/<remote>/<main>`.
    pub main_ref: &'a str,
    /// The model route: the stage's own `radian exec` arguments before them.
    pub exec_args: &'a [String],
    /// The gate profile the stage runs under, for the verdict (ADR 0048 decisions 10 and 15).
    pub profile: &'a str,
    /// How many review sessions may run at once in the repository (ADR 0048 decision 10).
    pub concurrency: u32,
    /// The settings this session runs with, for the verdict (ADR 0048 decision 10).
    pub settings: ReviewSettings,
    /// A file appended to the built-in prompt, or `None`.
    pub prompt_file: Option<&'a Path>,
    /// The previous review, when this review is a rework round (ADR 0048 decision 10).
    pub previous: Option<&'a PreviousReview>,
    /// The wall-time limit of one session. Zero waits forever.
    pub timeout: Duration,
    /// Test-only: run this program instead of the current executable, with the same
    /// arguments. The integration tests point it at a script that records its argv and prints
    /// a chosen `Result`, so the stage is driven without a model. Never set by arc; an input
    /// that exists only for tests is not documented (AGENTS.md).
    pub program: Option<&'a Path>,
}

impl ReviewRun<'_> {
    /// Runs the stage and returns the verdict it wrote.
    ///
    /// # Errors
    ///
    /// The checkout, a log file, the prompt file, a git command, or the verdict write failed.
    /// A conflict, unusable output and a blocking finding are `fail` verdicts, not errors.
    pub fn run(&self) -> Result<Verdict, GateError> {
        // One review slot for the whole stage: its lock keeps at most `concurrency` sessions
        // running, and its number names the checkout this session alone reads (ADR 0048
        // decision 10).
        let slot = SlotLock::acquire(self.repository, REVIEW_SLOT, self.concurrency)?;
        let started_at = now_utc();
        let checkout = GateCheckout::review_slot(self.repository, slot.slot(), self.remote);
        let candidate = candidate_for(
            self.repository,
            self.remote,
            self.main_ref,
            self.sha,
            self.task,
            &checkout,
        )?;
        let log_dir = attempt_dir(self.repository, self.task, self.attempt)?.join("logs");
        fs::create_dir_all(&log_dir).map_err(|source| GateError::Io {
            path: log_dir.clone(),
            source,
        })?;

        let mut sessions = Vec::new();
        let mut cost: Option<u64> = None;
        let mut report = None;
        let mut pre_existing = None;
        let mut reason = candidate.failure.clone();
        if candidate.failure.is_none() {
            let (base, own_diff) =
                self.own_change(&checkout.path(), &candidate.sha, &candidate.main_sha)?;
            if own_diff.is_empty() {
                // Nothing to review (ADR 0048 decision 10): the stage passes without a session
                // and its report says why.
                report = Some(ReviewReport {
                    summary: EMPTY_CHANGE_SUMMARY.to_owned(),
                    blocking: Vec::new(),
                    suggestions: Vec::new(),
                    verified: Vec::new(),
                });
            } else {
                let (prompt, round) =
                    self.prompt(&checkout.path(), &candidate.sha, &base, &own_diff)?;
                let mut unusable = Unusable::Output;
                for run in 1..=REVIEW_RUNS {
                    let (session_id, log_name) = self.session_for(run);
                    let ran = self.run_session(
                        &checkout.path(),
                        &session_id,
                        &log_name,
                        &prompt,
                        &log_dir,
                    )?;
                    sessions.push(
                        ran.result
                            .as_ref()
                            .and_then(|result| result.get("session_id"))
                            .and_then(Value::as_str)
                            .unwrap_or(&session_id)
                            .to_owned(),
                    );
                    if let Some(micros) = ran.result.as_ref().and_then(cost_micros) {
                        cost = Some(cost.unwrap_or(0).saturating_add(micros));
                    }
                    unusable = if ran.timed_out {
                        Unusable::Timeout
                    } else {
                        Unusable::Output
                    };
                    let Some(found) = usable_report(&ran) else {
                        continue;
                    };
                    report = Some(found);
                    break;
                }
                if let Some(found) = report.as_mut() {
                    // A rework round's report is judged by arc, not by the session: an item
                    // blocks only when it is near a line the task's own change added or
                    // changed (the whole task, not just the rework's increment), near a
                    // blocking item of the previous review, critical, or without a `file` or
                    // a `line` (ADR 0048 decision 10).
                    if let Some(round) = &round {
                        pre_existing = Some(round.mark_pre_existing(found));
                    }
                    if !found.blocking.is_empty() {
                        reason = Some("review_blocking".to_owned());
                    }
                } else {
                    reason = Some(unusable.reason().to_owned());
                }
            }
        }

        let result = if reason.is_none() && report.is_some() {
            VerdictResult::Pass
        } else {
            VerdictResult::Fail
        };
        let verdict = Verdict {
            schema_version: VERDICT_SCHEMA_VERSION,
            stage: Stage::Review,
            sha: candidate.sha,
            result,
            reason,
            commands: Vec::new(),
            started_at,
            ended_at: now_utc(),
            main_sha: Some(candidate.main_sha),
            base_sha: None,
            candidate_sha: candidate.candidate_sha,
            conflict_paths: candidate.conflict_paths,
            approver: None,
            review: report,
            review_session_ids: sessions,
            review_cost_micros: cost,
            review_pre_existing: pre_existing,
            review_settings: Some(self.settings.clone()),
            profile: Some(self.profile.to_owned()),
        };
        verdict.write_new(&verdict_path(
            self.repository,
            self.task,
            self.attempt,
            Stage::Review,
        )?)?;
        Ok(verdict)
    }

    /// The task's own change and the commit it is measured from: the diff from the merge base
    /// of the remote main and the task's commit to that commit (ADR 0048 decision 10). An empty
    /// diff means there is nothing to review. The range starts at the merge base because main's
    /// commits reach the branch through the merge the step before an attempt makes, and those
    /// are not the task's change (decision 7).
    fn own_change(
        &self,
        checkout: &Path,
        sha: &str,
        main_sha: &str,
    ) -> Result<(String, String), GateError> {
        let base = super::git::ok(checkout, &["merge-base", main_sha, sha])?;
        let own_diff = super::git::ok(
            checkout,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                &format!("{base}..{sha}"),
            ],
        )?;
        Ok((base, own_diff))
    }

    /// The built-in prompt, with the configured file appended, and the rework round it
    /// reviews (`None` for a first review). `base` and `own_diff` are [`ReviewRun::own_change`]'s
    /// answer for this commit.
    fn prompt(
        &self,
        checkout: &Path,
        sha: &str,
        base: &str,
        own_diff: &str,
    ) -> Result<(String, Option<ReworkRound>), GateError> {
        let mut round = None;
        let mut prompt = match self.previous {
            // A previous sha this checkout does not have cannot be diffed: the review is a
            // first review, so nothing is treated as pre-existing (ADR 0048 decision 10).
            // Neither is a review of the commit the previous review already reviewed (a
            // `reverify` of an unchanged head): the rework changed no line, so nothing could
            // be pre-existing, and the same items must still block.
            Some(previous) if previous.sha != self.sha && has_commit(checkout, &previous.sha)? => {
                let range = format!("{}..{sha}", previous.sha);
                let commits = super::git::ok(
                    checkout,
                    &["log", "--no-decorate", "--format=%h %s", &range],
                )?;
                let rework = super::git::ok(
                    checkout,
                    &["diff", "--no-ext-diff", "--no-textconv", &range],
                )?;
                // The increment is the rework's own new lines that the task's own diff also
                // has: a line that came in with a merge of main is not in the task's own diff
                // and drops out (ADR 0048 decision 10).
                let own_lines = new_side_lines(own_diff);
                let increment: BTreeSet<(String, u32)> = new_side_lines(&rework)
                    .intersection(&own_lines)
                    .cloned()
                    .collect();
                let diff = increment_diff(own_diff, &increment);
                let prompt =
                    build_rework_prompt(self.task, self.brief, previous, sha, &commits, &diff);
                // Whether an item blocks is decided by the whole task's change, not by the
                // increment: a problem in a line the task wrote in an earlier attempt is the
                // task's, not pre-existing (ADR 0048 decision 10).
                round = Some(ReworkRound {
                    previous: previous.clone(),
                    changed: own_lines,
                });
                prompt
            }
            _ => {
                let commits = super::git::ok(
                    checkout,
                    &[
                        "log",
                        "--no-decorate",
                        "--format=%h %s",
                        &format!("{base}..{sha}"),
                    ],
                )?;
                build_prompt(self.task, self.brief, base, sha, &commits, own_diff)
            }
        };
        if let Some(path) = self.prompt_file {
            let extra = fs::read_to_string(path).map_err(|source| GateError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            prompt.push('\n');
            prompt.push_str(extra.trim_end());
            prompt.push('\n');
        }
        Ok((prompt, round))
    }

    /// The session id and log name of `run` (ADR 0048 decision 10). A gate is started again
    /// for the same attempt after it died, so a journal this attempt's earlier review left is
    /// never reused: the stage takes `<attempt>-review-<run>-<k>` and the log
    /// `review-<run>-<k>` for the smallest free `k`.
    fn session_for(&self, run: u32) -> (String, String) {
        let base_session = format!("{}-review-{run}", self.attempt);
        let base_log = format!("review-{run}");
        let Some(dir) = review_session_dir(self.exec_args) else {
            return (base_session, base_log);
        };
        if !dir.join(format!("{base_session}.jsonl")).exists() {
            return (base_session, base_log);
        }
        let mut k = 1u32;
        loop {
            let session = format!("{base_session}-{k}");
            if !dir.join(format!("{session}.jsonl")).exists() {
                return (session, format!("{base_log}-{k}"));
            }
            k = k.saturating_add(1);
        }
    }

    /// Runs one session and reads the last `Result` line of its jsonl stream.
    fn run_session(
        &self,
        checkout: &Path,
        session_id: &str,
        log_name: &str,
        prompt: &str,
        log_dir: &Path,
    ) -> Result<Ran, GateError> {
        let output_path = log_dir.join(format!("{log_name}.jsonl"));
        let stderr_path = log_dir.join(format!("{log_name}.stderr.log"));
        let output = File::create(&output_path).map_err(|source| GateError::Io {
            path: output_path.clone(),
            source,
        })?;
        let stderr = File::create(&stderr_path).map_err(|source| GateError::Io {
            path: stderr_path.clone(),
            source,
        })?;
        let program = match self.program {
            Some(program) => program.to_path_buf(),
            None => std::env::current_exe().map_err(|source| GateError::Spawn {
                program: "radian".to_string(),
                source,
            })?,
        };
        let mut command = Command::new(&program);
        command.arg("exec");
        // A gate stage does not read the backlog (ADR 0048 decision 5). The stage's own
        // checkout is under `gates/review/`, so that entry stays readable.
        for path in deny_paths(self.repository, checkout) {
            command.arg("--deny-read").arg(path);
        }
        command
            .arg("--session-id")
            .arg(session_id)
            .arg("--workspace")
            .arg(checkout)
            .arg("--tools")
            .arg(REVIEW_TOOLS)
            // No MCP tool is mounted: `--tools` filters built-ins only, and the implicit
            // sources (the config's `[mcp]`, the workspace's `.mcp.json`) are muted.
            .arg("--no-config-mcp")
            .arg("--profile")
            .arg("workspace")
            .arg("--yes")
            .arg("--output")
            .arg("jsonl")
            .args(self.exec_args)
            .arg("-p")
            .arg("-");
        command
            .current_dir(checkout)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(stderr));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            // Its own process group, so a timeout reaches every child the session started.
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|source| GateError::Spawn {
            program: program.display().to_string(),
            source,
        })?;
        if let Some(mut stdin) = child.stdin.take() {
            // A session that exits before reading closes the pipe; the outcome is unusable
            // and the next run may still work.
            let _ = stdin.write_all(prompt.as_bytes());
        }
        let deadline = (!self.timeout.is_zero()).then(|| Instant::now() + self.timeout);
        // A stop from `serve` reaches the session's group, not only the gate.
        #[cfg(unix)]
        let mut running = super::stage::Running::track(child.id());
        let (status, timed_out) = loop {
            if let Some(status) = child.try_wait().map_err(|source| GateError::Io {
                path: checkout.to_path_buf(),
                source,
            })? {
                break (status, false);
            }
            #[cfg(unix)]
            running.stopped();
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                #[cfg(unix)]
                kill_group(child.id());
                #[cfg(not(unix))]
                let _ = child.kill();
                let status = child.wait().map_err(|source| GateError::Io {
                    path: checkout.to_path_buf(),
                    source,
                })?;
                break (status, true);
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        // A stop killed the session: no verdict may be written for it.
        if super::stage::stop_requested() {
            return Err(GateError::Stopped);
        }
        Ok(Ran {
            timed_out,
            exit_code: status.code(),
            result: last_result(&output_path),
        })
    }
}

/// The directory the review sessions' journals live in: a `--session-dir` in
/// `[gate.review] exec_args`, else `RADIAN_SESSION_DIR`, else the user's data directory. The
/// session id of a review run is chosen against this directory, so a re-run never reuses a
/// journal (ADR 0048 decision 10).
fn review_session_dir(exec_args: &[String]) -> Option<PathBuf> {
    let mut flag = None;
    let mut args = exec_args.iter();
    while let Some(arg) = args.next() {
        if arg == "--session-dir" {
            flag = args.next().map(PathBuf::from);
        } else if let Some(dir) = arg.strip_prefix("--session-dir=") {
            flag = Some(PathBuf::from(dir));
        }
    }
    radian_host::session_dir(flag.as_deref()).ok()
}

/// What one session produced.
struct Ran {
    /// The stage killed the session at its wall-time limit.
    timed_out: bool,
    /// The session's exit code, or `None` when a signal ended it.
    exit_code: Option<i32>,
    /// The last `Result` line of its jsonl stream.
    result: Option<Value>,
}

/// The report of a run that ended `completed` with exit `0` and a parsable block. Every
/// other run is unusable and the stage runs the session once more.
fn usable_report(ran: &Ran) -> Option<ReviewReport> {
    if ran.timed_out || ran.exit_code != Some(0) {
        return None;
    }
    let result = ran.result.as_ref()?;
    if result.get("outcome").and_then(Value::as_str) != Some("completed") {
        return None;
    }
    parse_report(result.get("text").and_then(Value::as_str)?)
}

/// The cost a `Result` line reports, in micros of a US dollar.
fn cost_micros(result: &Value) -> Option<u64> {
    result.get("cost")?.get("micros")?.as_u64()
}

/// The last `Result` record of a jsonl stream, if the session wrote one.
fn last_result(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    text.lines().rev().find_map(|line| {
        let value: Value = serde_json::from_str(line.trim()).ok()?;
        (value.get("type").and_then(Value::as_str) == Some("Result")).then_some(value)
    })
}

/// Parses the last fenced `json` block of a session's final message (ADR 0048 decision 10).
///
/// The object must hold all four fields, with `summary` a string, `blocking` and `suggestions`
/// lists of items, and `verified` a list of strings. An item is an object, or a plain string
/// for an item with only its `text`. A missing field, a wrong type, a level outside
/// [`REVIEW_LEVELS`], an unclosed block, or no `json` block at all is `None`.
#[must_use]
pub fn parse_report(text: &str) -> Option<ReviewReport> {
    let block = last_json_block(text)?;
    let value: Value = serde_json::from_str(&block).ok()?;
    Some(ReviewReport {
        summary: value.get("summary")?.as_str()?.to_owned(),
        blocking: items(value.get("blocking")?)?,
        suggestions: items(value.get("suggestions")?)?,
        verified: strings(value.get("verified")?)?,
    })
}

fn items(value: &Value) -> Option<Vec<ReviewItem>> {
    value
        .as_array()?
        .iter()
        .map(ReviewItem::from_json)
        .collect()
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect()
}

/// The body of the last closed fence whose info string is `json`.
fn last_json_block(text: &str) -> Option<String> {
    let mut blocks: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, String)> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        match current.as_mut() {
            Some((info, body)) => {
                if trimmed == "```" {
                    blocks.push((info.clone(), std::mem::take(body)));
                    current = None;
                } else {
                    body.push_str(line);
                    body.push('\n');
                }
            }
            None => {
                if let Some(info) = trimmed.strip_prefix("```") {
                    current = Some((info.trim().to_owned(), String::new()));
                }
            }
        }
    }
    blocks
        .into_iter()
        .rev()
        .find(|(info, _)| info.eq_ignore_ascii_case("json") || info.eq_ignore_ascii_case("jsonc"))
        .map(|(_, body)| body)
}

/// The built-in prompt (ADR 0048 decision 10).
fn build_prompt(
    task: &str,
    brief: &str,
    base: &str,
    sha: &str,
    commits: &str,
    diff: &str,
) -> String {
    format!(
        "You are reviewing one change for the task `{task}`.\n\n\
         ## The brief\n\
         {brief}\n\n\
         ## The commits (`{base}..{sha}`)\n\
         {commits}\n\n\
         ## The diff (`{base}..{sha}`)\n\
         ```diff\n\
         {diff}\n\
         ```\n\n\
         Check the change against the brief and the rules of this repository: read AGENTS.md \
         and every ADR the brief names. Look for ways the change fails the brief, breaks a \
         rule, is incomplete, or introduces a bug. Report only findings about this change.\n\n\
         {format}",
        format = report_format()
    )
}

/// The prompt of a rework round (ADR 0048 decision 10): the brief, the previous review's
/// blocking items, the commits since it, and the increment instead of the full diff.
fn build_rework_prompt(
    task: &str,
    brief: &str,
    previous: &PreviousReview,
    sha: &str,
    commits: &str,
    increment: &str,
) -> String {
    let previous_sha = &previous.sha;
    let items = if previous.blocking.is_empty() {
        "- none".to_owned()
    } else {
        previous
            .blocking
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "You are reviewing a rework for the task `{task}`.\n\n\
         ## The brief\n\
         {brief}\n\n\
         ## The previous review (`{previous_sha}`)\n\
         It reported these blocking items:\n\
         {items}\n\n\
         ## The commits (`{previous_sha}..{sha}`)\n\
         {commits}\n\n\
         ## The increment (`{previous_sha}..{sha}`)\n\
         Only the hunks of the task's own change that this rework changed:\n\
         ```diff\n\
         {increment}\n\
         ```\n\n\
         This is a rework round. Check that each blocking item of the previous review is \
         fixed. Review the increment for problems it introduces: ways it fails the brief, \
         breaks a rule of this repository, is incomplete, or is a bug. List every problem you \
         find, not only the first, and give each one its `file` and `line`. The files are in \
         the workspace; read them to confirm a finding.\n\n\
         {format}",
        format = report_format()
    )
}

/// The report format both prompts end with (ADR 0048 decision 10). The level list comes from
/// [`REVIEW_LEVELS`], so the prompt and the parser cannot drift.
fn report_format() -> String {
    let levels = REVIEW_LEVELS
        .iter()
        .map(|level| format!("`{level}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Finish with exactly one fenced ```json block and nothing after it:\n\n\
         ```json\n\
         {{\"summary\": \"...\", \"blocking\": [{{\"level\": \"major\", \"file\": \"path\", \
         \"line\": 1, \"text\": \"...\"}}], \"suggestions\": [], \"verified\": []}}\n\
         ```\n\n\
         - `summary`: 2 to 6 sentences on what the change does.\n\
         - `blocking`: what must change before this can merge. An empty list passes the \
         review.\n\
         - `suggestions`: improvements that do not block.\n\
         - `verified`: what you checked and found correct.\n\
         - Each item of `blocking` and `suggestions` is an object: `text` is required, and \
         `level` (one of {levels}), `file` and `line` are given when they apply.\n"
    )
}

/// Whether `sha` is a commit this checkout has.
fn has_commit(checkout: &Path, sha: &str) -> Result<bool, GateError> {
    let object = format!("{sha}^{{commit}}");
    let output = super::git::run(checkout, &["rev-parse", "--verify", "--quiet", &object])?;
    Ok(output.success() && !output.stdout.trim().is_empty())
}

/// One hunk of a unified diff: its `@@` line, its body lines and the line numbers of the new
/// file it names, context and added lines alike.
struct DiffHunk<'a> {
    header: &'a str,
    body: Vec<&'a str>,
    new_lines: Vec<u32>,
}

/// One file of a unified diff: the header lines before its first hunk and its hunks.
struct DiffFile<'a> {
    header: Vec<&'a str>,
    hunks: Vec<DiffHunk<'a>>,
}

impl DiffFile<'_> {
    /// The new path of the file, from its `+++ ` line.
    fn path(&self) -> Option<&str> {
        self.header
            .iter()
            .find_map(|line| line.strip_prefix("+++ "))
            .map(diff_path)
    }
}

/// The path of a `+++ ` line: git's `b/` prefix and the tab a path with spaces carries are
/// dropped.
fn diff_path(rest: &str) -> &str {
    let rest = rest.split('\t').next().unwrap_or(rest);
    rest.strip_prefix("b/").unwrap_or(rest)
}

/// Splits a unified diff into files and hunks. A hunk's body is read by the line counts of its
/// `@@` header, so a content line that looks like a header (`+++ b/x`) stays a content line.
fn parse_diff(diff: &str) -> Vec<DiffFile<'_>> {
    let mut files: Vec<DiffFile<'_>> = Vec::new();
    let mut file: Option<DiffFile<'_>> = None;
    let mut hunk: Option<DiffHunk<'_>> = None;
    let mut left_old = 0i64;
    let mut left_new = 0i64;
    let mut new_line = 0u32;
    for line in diff.lines() {
        if left_old > 0 || left_new > 0 {
            let Some(current) = hunk.as_mut() else {
                continue;
            };
            current.body.push(line);
            if line.starts_with('+') {
                current.new_lines.push(new_line);
                new_line += 1;
                left_new -= 1;
            } else if line.starts_with('-') {
                left_old -= 1;
            } else if !line.starts_with('\\') {
                current.new_lines.push(new_line);
                new_line += 1;
                left_new -= 1;
                left_old -= 1;
            }
            continue;
        }
        if line.starts_with("@@ ") {
            if let Some(current) = hunk.take()
                && let Some(open) = file.as_mut()
            {
                open.hunks.push(current);
            }
            let Some((old_count, new_start, new_count)) = hunk_range(line) else {
                continue;
            };
            if file.is_none() {
                file = Some(DiffFile {
                    header: Vec::new(),
                    hunks: Vec::new(),
                });
            }
            left_old = old_count;
            left_new = new_count;
            new_line = new_start;
            hunk = Some(DiffHunk {
                header: line,
                body: Vec::new(),
                new_lines: Vec::new(),
            });
            continue;
        }
        if line.starts_with("diff --git ") {
            if let Some(current) = hunk.take()
                && let Some(open) = file.as_mut()
            {
                open.hunks.push(current);
            }
            if let Some(done) = file.take() {
                files.push(done);
            }
            file = Some(DiffFile {
                header: vec![line],
                hunks: Vec::new(),
            });
            continue;
        }
        if let Some(open) = file.as_mut() {
            open.header.push(line);
        }
    }
    if let Some(current) = hunk.take()
        && let Some(open) = file.as_mut()
    {
        open.hunks.push(current);
    }
    if let Some(done) = file.take() {
        files.push(done);
    }
    files
}

/// The `(old count, new start, new count)` of an `@@ -a,b +c,d @@` header.
fn hunk_range(header: &str) -> Option<(i64, u32, i64)> {
    let mut tokens = header.split_whitespace();
    tokens.next()?;
    let old = tokens.next()?.strip_prefix('-')?;
    let new = tokens.next()?.strip_prefix('+')?;
    let (_, old_count) = range(old)?;
    let (new_start, new_count) = range(new)?;
    Some((old_count, new_start, new_count))
}

/// One `a,b` range of a hunk header; a missing count is one line.
fn range(text: &str) -> Option<(u32, i64)> {
    match text.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((text.parse().ok()?, 1)),
    }
}

/// The new-side lines of a diff: `(path, line)` for every context and added line.
fn new_side_lines(diff: &str) -> BTreeSet<(String, u32)> {
    let mut lines = BTreeSet::new();
    for file in parse_diff(diff) {
        let Some(path) = file.path() else {
            continue;
        };
        for hunk in &file.hunks {
            for line in &hunk.new_lines {
                lines.insert((path.to_owned(), *line));
            }
        }
    }
    lines
}

/// The task's own diff restricted to the hunks that hold an increment line (ADR 0048 decision
/// 10). A file with no such hunk is left out.
fn increment_diff(own_diff: &str, increment: &BTreeSet<(String, u32)>) -> String {
    let mut out = String::new();
    for file in parse_diff(own_diff) {
        let Some(path) = file.path().map(str::to_owned) else {
            continue;
        };
        let hunks: Vec<&DiffHunk<'_>> = file
            .hunks
            .iter()
            .filter(|hunk| {
                hunk.new_lines
                    .iter()
                    .any(|line| increment.contains(&(path.clone(), *line)))
            })
            .collect();
        if hunks.is_empty() {
            continue;
        }
        for line in &file.header {
            out.push_str(line);
            out.push('\n');
        }
        for hunk in hunks {
            out.push_str(hunk.header);
            out.push('\n');
            for line in &hunk.body {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    if out.is_empty() {
        "the rework changed no line of the task's own diff\n".to_owned()
    } else {
        out
    }
}

/// The path-like argument `arg` is or sets, if it is one the stage owns. A long flag matches
/// its `=value` spelling too; the single short flag the stage owns (`-p`) matches any cluster
/// that starts with it, because clap reads `-pfoo` as `-p foo`.
#[must_use]
pub fn owned_name(arg: &str) -> Option<&'static str> {
    OWNED_EXEC_ARGS.iter().copied().find(|owned| {
        arg == *owned
            || (owned.starts_with("--") && arg.starts_with(&format!("{owned}=")))
            || (!owned.starts_with("--") && arg.starts_with(owned))
    })
}

/// The arc entries the review session must not read: everything except `workspace` (the
/// session's own slot checkout), and inside `gates/` every stage and every other review
/// slot. The fixed entries are denied before they exist; any other that exists now is denied
/// too (ADR 0048 decision 5: a gate stage does not read the backlog).
fn deny_paths(repository: &Path, workspace: &Path) -> Vec<PathBuf> {
    const FIXED: &[&str] = &[
        "config.toml",
        "plans",
        "tasks",
        "ledger",
        "runs",
        "attempts",
        "worktrees",
        "state.lock",
        "runner.lock",
    ];
    let arc = repository.join(".radian").join("arc");
    let mut paths: Vec<PathBuf> = FIXED.iter().map(|name| arc.join(name)).collect();
    if let Ok(stages) = fs::read_dir(arc.join("gates")) {
        for stage in stages.flatten() {
            if stage.path() != workspace {
                paths.push(stage.path());
            }
        }
    }
    if let Ok(entries) = fs::read_dir(&arc) {
        for entry in entries.flatten() {
            if entry.file_name() == "gates" {
                continue;
            }
            let path = entry.path();
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(level: Option<&str>, file: Option<&str>, line: Option<u32>, text: &str) -> ReviewItem {
        ReviewItem {
            level: level.map(str::to_owned),
            file: file.map(str::to_owned),
            line,
            text: text.to_owned(),
            pre_existing: false,
        }
    }

    /// The last fenced `json` block is the report: a message with two blocks takes the
    /// second. Mutation: read the first block → this test sees the stale summary.
    #[test]
    fn the_last_json_block_wins() {
        let text = "first\n```json\n{\"summary\": \"old\", \"blocking\": [], \"suggestions\": [], \
                    \"verified\": []}\n```\nthen\n```json\n{\"summary\": \"new\", \"blocking\": \
                    [{\"level\": \"major\", \"file\": \"a.rs\", \"line\": 3, \"text\": \"b\"}], \
                    \"suggestions\": [\"s\"], \"verified\": [\"v\"]}\n```\ndone";
        let report = parse_report(text).expect("the last block parses");
        assert_eq!(report.summary, "new");
        assert_eq!(
            report.blocking,
            vec![item(Some("major"), Some("a.rs"), Some(3), "b")]
        );
        assert_eq!(report.suggestions, vec![ReviewItem::text("s")]);
        assert_eq!(report.verified, vec!["v".to_owned()]);
    }

    /// A block with a missing field, a wrong type, a level outside the closed list, no fence,
    /// or an unclosed fence has no report; the empty lists of a passing review are a valid
    /// report.
    #[test]
    fn an_incomplete_or_unfenced_report_is_unusable() {
        for text in [
            "no block here",
            "```json\n{\"summary\": \"s\", \"blocking\": []}\n```",
            "```json\n{\"summary\": 1, \"blocking\": [], \"suggestions\": [], \"verified\": []}\n```",
            "```json\n{\"summary\": \"s\", \"blocking\": \"none\", \"suggestions\": [], \
             \"verified\": []}\n```",
            // An item needs its text.
            "```json\n{\"summary\": \"s\", \"blocking\": [{\"file\": \"a.rs\"}], \
             \"suggestions\": [], \"verified\": []}\n```",
            // The level is the closed list.
            "```json\n{\"summary\": \"s\", \"blocking\": [{\"level\": \"high\", \"text\": \"b\"}], \
             \"suggestions\": [], \"verified\": []}\n```",
            // A line is a non-negative integer.
            "```json\n{\"summary\": \"s\", \"blocking\": [{\"text\": \"b\", \"line\": -1}], \
             \"suggestions\": [], \"verified\": []}\n```",
            "```json\n{\"summary\": \"s\", \"blocking\": [], \"suggestions\": [], \
             \"verified\": []}\n",
            "```diff\n{\"summary\": \"s\", \"blocking\": [], \"suggestions\": [], \
             \"verified\": []}\n```",
        ] {
            assert!(parse_report(text).is_none(), "{text}");
        }
        let passing = parse_report(
            "```json\n{\"summary\": \"s\", \"blocking\": [], \"suggestions\": [], \
             \"verified\": []}\n```",
        )
        .expect("empty lists are a report");
        assert!(passing.blocking.is_empty());
    }

    /// A plain string is an item with only its text, and it serializes back to that string, so
    /// a verdict written before the object form keeps its bytes (and its digest).
    #[test]
    fn a_plain_string_item_is_an_item_with_only_its_text() {
        let text = "```json\n{\"summary\": \"s\", \"blocking\": [\"rename the helper\"], \
                    \"suggestions\": [], \"verified\": []}\n```";
        let report = parse_report(text).expect("a string item parses");
        assert_eq!(report.blocking, vec![ReviewItem::text("rename the helper")]);
        assert_eq!(
            serde_json::to_string(&report.blocking[0]).expect("it serializes"),
            "\"rename the helper\""
        );
        // The object form round-trips as an object, and `pre_existing` appears only when set.
        let moved = ReviewItem {
            pre_existing: true,
            ..ReviewItem::text("old")
        };
        assert_eq!(
            serde_json::to_string(&moved).expect("it serializes"),
            "{\"text\":\"old\",\"pre_existing\":true}"
        );
        let graded = item(Some("minor"), Some("src/x.rs"), Some(9), "x");
        let json = serde_json::to_string(&graded).expect("it serializes");
        assert_eq!(
            json,
            "{\"level\":\"minor\",\"file\":\"src/x.rs\",\"line\":9,\"text\":\"x\"}"
        );
        let read: ReviewItem = serde_json::from_str(&json).expect("it reads back");
        assert_eq!(read, graded);
    }

    /// The printed item is `[level] path:line text`, and the parts the item does not have are
    /// left out.
    #[test]
    fn an_item_prints_its_level_its_place_and_its_text() {
        assert_eq!(
            item(Some("major"), Some("src/x.rs"), Some(12), "the bug").to_string(),
            "[major] src/x.rs:12 the bug"
        );
        assert_eq!(
            item(None, Some("src/x.rs"), None, "the bug").to_string(),
            "src/x.rs the bug"
        );
        assert_eq!(item(None, None, None, "the bug").to_string(), "the bug");
        assert_eq!(
            item(Some("critical"), None, Some(4), "the bug").to_string(),
            "[critical] the bug"
        );
    }

    /// The new-side lines of a diff are the context and added lines of the new file, with the
    /// path git's `+++` line names.
    #[test]
    fn the_new_side_lines_of_a_diff_are_the_lines_of_the_new_file() {
        let diff = "diff --git a/a.txt b/a.txt\n\
                    index 111..222 100644\n\
                    --- a/a.txt\n\
                    +++ b/a.txt\n\
                    @@ -1,3 +1,4 @@\n\
                     one\n\
                    -two\n\
                    +TWO\n\
                     three\n\
                    +four\n";
        let lines: Vec<(String, u32)> = new_side_lines(diff).into_iter().collect();
        assert_eq!(
            lines,
            vec![
                ("a.txt".to_owned(), 1),
                ("a.txt".to_owned(), 2),
                ("a.txt".to_owned(), 3),
                ("a.txt".to_owned(), 4),
            ]
        );
    }

    /// A content line that looks like a header stays inside its hunk, because the hunk's line
    /// counts decide where it ends.
    #[test]
    fn a_content_line_that_looks_like_a_header_stays_in_its_hunk() {
        let diff = "diff --git a/a.txt b/a.txt\n\
                    --- a/a.txt\n\
                    +++ b/a.txt\n\
                    @@ -1,1 +1,2 @@\n\
                     one\n\
                    +++ b/not-a-header\n";
        let files = parse_diff(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path(), Some("a.txt"));
        assert_eq!(files[0].hunks.len(), 1, "the body line opened a hunk");
        assert_eq!(files[0].hunks[0].body.len(), 2);
    }

    /// The increment diff keeps the hunks that hold an increment line and leaves the other
    /// files out.
    #[test]
    fn only_the_hunks_with_an_increment_line_are_kept() {
        let own = "diff --git a/a.txt b/a.txt\n\
                   --- a/a.txt\n\
                   +++ b/a.txt\n\
                   @@ -1,1 +1,2 @@\n\
                    one\n\
                   +two\n\
                   diff --git a/b.txt b/b.txt\n\
                   --- a/b.txt\n\
                   +++ b/b.txt\n\
                   @@ -1,1 +1,2 @@\n\
                    one\n\
                   +two\n";
        let increment: BTreeSet<(String, u32)> = [("a.txt".to_owned(), 2)].into_iter().collect();
        let kept = increment_diff(own, &increment);
        assert!(kept.contains("a.txt"), "{kept}");
        assert!(kept.contains("+two"), "{kept}");
        assert!(
            !kept.contains("b.txt"),
            "the untouched file stayed:\n{kept}"
        );
        assert_eq!(
            increment_diff(own, &BTreeSet::new()),
            "the rework changed no line of the task's own diff\n"
        );
    }

    /// Rules (a) and (b): a blocking item near a line the task's own change touched, or near
    /// an earlier blocking item, stays blocking; an item far from both is moved to `suggestions`
    /// with `pre_existing`; an item without a place stays blocking at its level. Mutation:
    /// demote an unplaced item again → the `vague` item moves.
    #[test]
    fn a_pre_existing_item_is_moved_and_marked() {
        let round = ReworkRound {
            previous: PreviousReview {
                sha: "prev".to_owned(),
                blocking: vec![item(Some("major"), Some("src/old.rs"), Some(40), "unfixed")],
            },
            changed: [(String::from("src/new.rs"), 10)].into_iter().collect(),
        };
        let mut report = ReviewReport {
            summary: "s".to_owned(),
            blocking: vec![
                // (a) two lines from a line the task's own change touched.
                item(
                    Some("major"),
                    Some("src/new.rs"),
                    Some(12),
                    "near the change",
                ),
                // (b) seven lines from the previous item, in its file.
                item(
                    Some("minor"),
                    Some("src/old.rs"),
                    Some(47),
                    "the same problem",
                ),
                // Neither: another file, another line.
                item(Some("minor"), Some("src/old.rs"), Some(80), "untouched"),
                // No place at all: the session's omission, not evidence that it is old.
                ReviewItem::text("vague"),
            ],
            suggestions: Vec::new(),
            verified: Vec::new(),
        };
        assert_eq!(round.mark_pre_existing(&mut report), 1);
        let texts: Vec<&str> = report
            .blocking
            .iter()
            .map(|item| item.text.as_str())
            .collect();
        assert_eq!(texts, vec!["near the change", "the same problem", "vague"]);
        assert_eq!(report.suggestions.len(), 1);
        assert!(
            report.suggestions.iter().all(|item| item.pre_existing),
            "{:?}",
            report.suggestions
        );
        assert_eq!(report.suggestions[0].level.as_deref(), Some("minor"));
    }

    /// A `critical` item blocks in every round, wherever it is: far from the task's changed
    /// lines and far from every previous item. Mutation: let the window rule move it → this test
    /// sees it in `suggestions`.
    #[test]
    fn a_critical_item_blocks_wherever_it_is() {
        let round = ReworkRound {
            previous: PreviousReview {
                sha: "prev".to_owned(),
                blocking: vec![item(Some("major"), Some("src/old.rs"), Some(40), "unfixed")],
            },
            changed: [(String::from("src/new.rs"), 10)].into_iter().collect(),
        };
        let mut report = ReviewReport {
            summary: "s".to_owned(),
            blocking: vec![
                item(
                    Some("critical"),
                    Some("src/far.rs"),
                    Some(900),
                    "a permission bypass",
                ),
                item(
                    Some("critical"),
                    Some("src/old.rs"),
                    Some(400),
                    "a critical problem far from the previous one",
                ),
            ],
            suggestions: Vec::new(),
            verified: Vec::new(),
        };
        assert_eq!(round.mark_pre_existing(&mut report), 0);
        let texts: Vec<&str> = report
            .blocking
            .iter()
            .map(|item| item.text.as_str())
            .collect();
        assert_eq!(
            texts,
            vec![
                "a permission bypass",
                "a critical problem far from the previous one"
            ]
        );
        assert!(report.suggestions.is_empty(), "{:?}", report.suggestions);
    }

    /// An item without a place is not demoted: it stays in `blocking` at its level. Mutation:
    /// demote it again → this test sees all three in `suggestions`.
    #[test]
    fn an_item_without_a_place_stays_blocking_at_its_level() {
        let round = ReworkRound {
            previous: PreviousReview {
                sha: "prev".to_owned(),
                blocking: vec![item(Some("major"), Some("src/old.rs"), Some(40), "unfixed")],
            },
            changed: [(String::from("src/new.rs"), 10)].into_iter().collect(),
        };
        let mut report = ReviewReport {
            summary: "s".to_owned(),
            blocking: vec![
                item(Some("major"), None, None, "a major item with no place"),
                item(
                    Some("critical"),
                    Some("src/far.rs"),
                    None,
                    "a critical item with no line",
                ),
                item(None, None, Some(4), "an ungraded item with only a line"),
            ],
            suggestions: Vec::new(),
            verified: Vec::new(),
        };
        assert_eq!(round.mark_pre_existing(&mut report), 0);
        assert_eq!(report.blocking.len(), 3, "{:?}", report.blocking);
        assert_eq!(report.blocking[0].level.as_deref(), Some("major"));
        assert_eq!(report.blocking[1].level.as_deref(), Some("critical"));
        assert_eq!(report.blocking[2].level, None);
        assert!(report.suggestions.is_empty(), "{:?}", report.suggestions);
    }

    /// A `major` item outside both windows is still moved to `suggestions` with `pre_existing`,
    /// and keeps its `level` so the follow-up carries it; `minor` items keep the same rule.
    /// Mutation: drop the level of a moved item → this test loses `major`.
    #[test]
    fn a_major_item_outside_the_windows_is_moved_and_keeps_its_level() {
        let round = ReworkRound {
            previous: PreviousReview {
                sha: "prev".to_owned(),
                blocking: vec![item(Some("major"), Some("src/old.rs"), Some(40), "unfixed")],
            },
            changed: [(String::from("src/new.rs"), 10)].into_iter().collect(),
        };
        let mut report = ReviewReport {
            summary: "s".to_owned(),
            blocking: vec![
                // Another file, and 40 lines from the previous item.
                item(Some("major"), Some("src/old.rs"), Some(80), "untouched"),
                item(Some("minor"), Some("src/other.rs"), Some(5), "a nit"),
            ],
            suggestions: Vec::new(),
            verified: Vec::new(),
        };
        assert_eq!(round.mark_pre_existing(&mut report), 2);
        assert!(report.blocking.is_empty(), "{:?}", report.blocking);
        assert_eq!(report.suggestions[0].level.as_deref(), Some("major"));
        assert!(report.suggestions[0].pre_existing);
        assert_eq!(report.suggestions[1].level.as_deref(), Some("minor"));
    }

    /// Rule (a) follows the task's own change, not the rework: an item on a line the task
    /// added or changed in an earlier attempt keeps blocking even when the rework touched
    /// nothing there. Mutation: measure against the rework's increment again → the item moves.
    #[test]
    fn an_item_on_a_line_the_task_changed_keeps_blocking() {
        let round = ReworkRound {
            previous: PreviousReview {
                sha: "prev".to_owned(),
                blocking: Vec::new(),
            },
            changed: [(String::from("src/kept.rs"), 20)].into_iter().collect(),
        };
        let mut report = ReviewReport {
            summary: "s".to_owned(),
            blocking: vec![
                // Two lines from a line the task changed in an earlier attempt.
                item(
                    Some("major"),
                    Some("src/kept.rs"),
                    Some(22),
                    "the task's own bug",
                ),
                // Exactly at the window's edge.
                item(
                    Some("minor"),
                    Some("src/kept.rs"),
                    Some(17),
                    "the edge of the window",
                ),
            ],
            suggestions: Vec::new(),
            verified: Vec::new(),
        };
        assert_eq!(round.mark_pre_existing(&mut report), 0);
        assert_eq!(report.blocking.len(), 2, "{:?}", report.blocking);
        assert!(report.suggestions.is_empty(), "{:?}", report.suggestions);
    }

    /// Rule (a) is per file: an item in a file the task's own change does not touch is demoted,
    /// even at a line number a changed file has. Mutation: compare only the line, not the
    /// path → the item in the other file stays blocking.
    #[test]
    fn an_item_in_a_file_the_task_did_not_touch_is_demoted() {
        let round = ReworkRound {
            previous: PreviousReview {
                sha: "prev".to_owned(),
                blocking: Vec::new(),
            },
            changed: [(String::from("src/kept.rs"), 20)].into_iter().collect(),
        };
        let mut report = ReviewReport {
            summary: "s".to_owned(),
            blocking: vec![item(
                Some("major"),
                Some("src/other.rs"),
                Some(20),
                "another file",
            )],
            suggestions: Vec::new(),
            verified: Vec::new(),
        };
        assert_eq!(round.mark_pre_existing(&mut report), 1);
        assert!(report.blocking.is_empty(), "{:?}", report.blocking);
        assert!(report.suggestions[0].pre_existing);
        assert_eq!(report.suggestions[0].level.as_deref(), Some("major"));
    }

    /// Rule (a) has a window per file: a line more than [`PRE_EXISTING_WINDOW`] from every
    /// changed line of its file is demoted, though one at the edge stays. Mutation: keep any
    /// item in a file the task changed, whatever its distance → the far item stays blocking.
    #[test]
    fn an_item_more_than_the_window_from_a_changed_line_is_demoted() {
        let round = ReworkRound {
            previous: PreviousReview {
                sha: "prev".to_owned(),
                blocking: Vec::new(),
            },
            changed: [(String::from("src/kept.rs"), 20)].into_iter().collect(),
        };
        let mut report = ReviewReport {
            summary: "s".to_owned(),
            blocking: vec![
                item(
                    Some("minor"),
                    Some("src/kept.rs"),
                    Some(24),
                    "four lines away",
                ),
                item(
                    Some("minor"),
                    Some("src/kept.rs"),
                    Some(23),
                    "three lines away",
                ),
            ],
            suggestions: Vec::new(),
            verified: Vec::new(),
        };
        assert_eq!(round.mark_pre_existing(&mut report), 1);
        let texts: Vec<&str> = report
            .blocking
            .iter()
            .map(|item| item.text.as_str())
            .collect();
        assert_eq!(texts, vec!["three lines away"]);
        assert_eq!(report.suggestions.len(), 1);
        assert_eq!(report.suggestions[0].text, "four lines away");
        assert!(report.suggestions[0].pre_existing);
    }

    /// The stage's own arguments are refused by name: an `exec_args` entry may not unmount
    /// the read-only tools or change the session the stage sets.
    #[test]
    fn an_argument_the_stage_owns_is_named() {
        assert_eq!(owned_name("--tools=shell"), Some("--tools"));
        assert_eq!(owned_name("--session-id"), Some("--session-id"));
        assert_eq!(owned_name("-p"), Some("-p"));
        assert_eq!(owned_name("-px"), Some("-p"));
        assert_eq!(owned_name("--model"), None);
        assert_eq!(owned_name("--base-url"), None);
    }

    #[test]
    fn the_review_tools_are_the_three_read_only_tools() {
        assert_eq!(
            REVIEW_TOOLS.split(',').collect::<Vec<_>>(),
            vec!["read", "grep", "glob"]
        );
    }

    /// The prompt carries the brief, the commit list and the diff, and asks for the one
    /// fenced block the parser reads in the object form.
    #[test]
    fn the_prompt_holds_the_brief_the_commits_and_the_diff() {
        let prompt = build_prompt(
            "TK0001-a",
            "the brief",
            "base1",
            "head1",
            "abc one",
            "+new\n",
        );
        for needle in [
            "TK0001-a",
            "the brief",
            "base1..head1",
            "abc one",
            "+new",
            "\"summary\"",
            "\"blocking\"",
            "\"level\"",
            "\"file\"",
            "\"line\"",
            "`critical`, `major`, `minor`",
        ] {
            assert!(
                prompt.contains(needle),
                "the prompt misses {needle:?}:\n{prompt}"
            );
        }
    }

    /// A rework round's prompt holds the previous items, the commits since them and the
    /// increment, and it asks for the two checks of a rework round.
    #[test]
    fn the_rework_prompt_holds_the_previous_items_and_the_increment() {
        let previous = PreviousReview {
            sha: "prev1".to_owned(),
            blocking: vec![item(Some("major"), Some("src/x.rs"), Some(3), "the bug")],
        };
        let prompt = build_rework_prompt(
            "TK0001-a",
            "the brief",
            &previous,
            "head2",
            "def two",
            "+new\n",
        );
        for needle in [
            "TK0001-a",
            "the brief",
            "prev1..head2",
            "- [major] src/x.rs:3 the bug",
            "+new",
            "Check that each blocking item of the previous review is fixed",
            "List every problem you find",
            "\"level\"",
        ] {
            assert!(
                prompt.contains(needle),
                "the prompt misses {needle:?}:\n{prompt}"
            );
        }
        assert!(!prompt.contains("## The diff"), "{prompt}");
    }

    fn run_for<'a>(attempt: &'a str, exec_args: &'a [String]) -> ReviewRun<'a> {
        ReviewRun {
            repository: Path::new("."),
            remote: "origin",
            task: "TK0001-a",
            attempt,
            sha: "sha",
            brief: "the brief",
            main_ref: "refs/remotes/origin/main",
            exec_args,
            profile: "default",
            concurrency: 1,
            settings: ReviewSettings {
                round: "first".to_owned(),
                exec_args: "gate.review.exec_args".to_owned(),
                timeout_secs: 0,
                timeout: "gate.review.timeout_secs".to_owned(),
            },
            prompt_file: None,
            previous: None,
            timeout: Duration::ZERO,
            program: None,
        }
    }

    /// A gate started again for the same attempt does not reuse a review session id whose
    /// journal exists: it takes the smallest free `-<k>`, and the log name follows it.
    /// Mutation: return the base id unconditionally and the second run cannot start.
    #[test]
    fn a_review_rerun_takes_the_next_free_session_and_log_name() {
        let dir =
            std::env::temp_dir().join(format!("radian-arc-review-sessions-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let args = vec![
            "--session-dir".to_owned(),
            dir.to_str().expect("a utf-8 path").to_owned(),
        ];
        let run = run_for("TK0001-1", &args);
        assert_eq!(
            run.session_for(1),
            ("TK0001-1-review-1".to_owned(), "review-1".to_owned())
        );
        fs::write(dir.join("TK0001-1-review-1.jsonl"), "").unwrap();
        assert_eq!(
            run.session_for(1),
            ("TK0001-1-review-1-1".to_owned(), "review-1-1".to_owned())
        );
        fs::write(dir.join("TK0001-1-review-1-1.jsonl"), "").unwrap();
        assert_eq!(
            run.session_for(1),
            ("TK0001-1-review-1-2".to_owned(), "review-1-2".to_owned())
        );
        assert_eq!(
            run.session_for(2),
            ("TK0001-1-review-2".to_owned(), "review-2".to_owned()),
            "the second run of the stage keeps its own base"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
