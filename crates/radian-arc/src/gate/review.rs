//! The `review` stage (ADR 0048 decision 10).
//!
//! `review` is a read-only radian session, not the worker session. One run is one
//! `radian exec` in `gates/review/`, on the verification candidate, mounted with exactly the
//! `read`, `grep` and `glob` tools ([`REVIEW_TOOLS`]): no shell, no write, and no MCP server
//! (the implicit sources are muted and no `--mcp-config` is passed). The session's model route
//! comes from `[gate.review] exec_args`.
//!
//! The prompt is built in ([`build_prompt`]): the task's brief, the commit list and the diff
//! `base..verified_sha`, and the instruction to check the change against the brief and the
//! repository's rules. A configured `prompt_file` is appended.
//!
//! The session ends with one fenced `json` block ([`ReviewReport`]). arc parses the last such
//! block. An empty `blocking` list passes the stage; any item fails it with reason
//! `review_blocking`. No parsable block, a missing field, a timeout, or a session error makes
//! the stage run the session once more; when the second run is unusable too the stage fails
//! with `review_output` (or `review_timeout` when the last run timed out). The verdict records
//! every run's session id and their summed cost.

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
use super::exclusive::DirLock;
use super::integration::candidate_for;
use super::verdict::{
    ReviewReport, VERDICT_SCHEMA_VERSION, Verdict, VerdictResult, attempt_dir, now_utc,
    verdict_path,
};

/// The lock file of the review stage under `gates/`, so two reviews never share the
/// `gates/review/` checkout.
pub(crate) const REVIEW_LOCK: &str = "review.lock";

/// The built-in tools a review session mounts. The one closed list the stage, the tests and
/// the manual read: a name that is not here is not mounted at all (`radian exec --tools`).
pub const REVIEW_TOOLS: &str = "read,grep,glob";

/// How many times one stage may run the session: once, and once more when the first run's
/// output is unusable (ADR 0048 decision 10).
pub const REVIEW_RUNS: u32 = 2;

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
    /// The commit the task's branch started from: the diff is `base_sha..sha`.
    pub base_sha: &'a str,
    /// The task's brief, as the worker got it.
    pub brief: &'a str,
    /// The ref that names the remote main after a fetch, typically
    /// `refs/remotes/<remote>/<main>`.
    pub main_ref: &'a str,
    /// The model route: the stage's own `radian exec` arguments before them.
    pub exec_args: &'a [String],
    /// A file appended to the built-in prompt, or `None`.
    pub prompt_file: Option<&'a Path>,
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
        let _lock = DirLock::acquire(self.repository, REVIEW_LOCK)?;
        let started_at = now_utc();
        let checkout = GateCheckout::new(self.repository, Stage::Review, self.remote);
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
        let mut reason = candidate.failure.clone();
        if candidate.failure.is_none() {
            let prompt = self.prompt(&checkout.path(), &candidate.sha)?;
            let mut unusable = Unusable::Output;
            for run in 1..=REVIEW_RUNS {
                let session_id = format!("{}-review-{run}", self.attempt);
                let ran =
                    self.run_session(&checkout.path(), &session_id, &prompt, &log_dir, run)?;
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
            if let Some(found) = &report {
                if !found.blocking.is_empty() {
                    reason = Some("review_blocking".to_owned());
                }
            } else {
                reason = Some(unusable.reason().to_owned());
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
        };
        verdict.write_new(&verdict_path(
            self.repository,
            self.task,
            self.attempt,
            Stage::Review,
        )?)?;
        Ok(verdict)
    }

    /// The built-in prompt, with the configured file appended.
    fn prompt(&self, checkout: &Path, sha: &str) -> Result<String, GateError> {
        let commits = super::git::ok(
            checkout,
            &[
                "log",
                "--no-decorate",
                "--format=%h %s",
                &format!("{}..{sha}", self.base_sha),
            ],
        )?;
        let diff = super::git::ok(
            checkout,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                &format!("{}..{sha}", self.base_sha),
            ],
        )?;
        let mut prompt = build_prompt(self.task, self.brief, self.base_sha, sha, &commits, &diff);
        if let Some(path) = self.prompt_file {
            let extra = fs::read_to_string(path).map_err(|source| GateError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            prompt.push('\n');
            prompt.push_str(extra.trim_end());
            prompt.push('\n');
        }
        Ok(prompt)
    }

    /// Runs one session and reads the last `Result` line of its jsonl stream.
    fn run_session(
        &self,
        checkout: &Path,
        session_id: &str,
        prompt: &str,
        log_dir: &Path,
        run: u32,
    ) -> Result<Ran, GateError> {
        let output_path = log_dir.join(format!("review-{run}.jsonl"));
        let stderr_path = log_dir.join(format!("review-{run}.stderr.log"));
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
        for path in deny_paths(self.repository) {
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
        // A stop from `--watch` reaches the session's group, not only the gate.
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
/// The object must hold all four fields, with `summary` a string and the other three lists of
/// strings. A missing field, a wrong type, an unclosed block, or no `json` block at all is
/// `None`.
#[must_use]
pub fn parse_report(text: &str) -> Option<ReviewReport> {
    let block = last_json_block(text)?;
    let value: Value = serde_json::from_str(&block).ok()?;
    Some(ReviewReport {
        summary: value.get("summary")?.as_str()?.to_owned(),
        blocking: strings(value.get("blocking")?)?,
        suggestions: strings(value.get("suggestions")?)?,
        verified: strings(value.get("verified")?)?,
    })
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
         Finish with exactly one fenced ```json block and nothing after it:\n\n\
         ```json\n\
         {{\"summary\": \"...\", \"blocking\": [\"...\"], \"suggestions\": [\"...\"], \
         \"verified\": [\"...\"]}}\n\
         ```\n\n\
         - `summary`: 2 to 6 sentences on what the change does.\n\
         - `blocking`: what must change before this can merge. An empty list passes the \
         review.\n\
         - `suggestions`: improvements that do not block.\n\
         - `verified`: what you checked and found correct.\n"
    )
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

/// The arc entries the review session must not read: everything except the stage checkouts,
/// and inside `gates/` every stage but `review` (the session's own workspace). The fixed
/// entries are denied before they exist; any other that exists now is denied too (ADR 0048
/// decision 5: a gate stage does not read the backlog).
fn deny_paths(repository: &Path) -> Vec<PathBuf> {
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
            if stage.file_name() != "review" {
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

    /// The last fenced `json` block is the report: a message with two blocks takes the
    /// second. Mutation: read the first block → this test sees the stale summary.
    #[test]
    fn the_last_json_block_wins() {
        let text = "first\n```json\n{\"summary\": \"old\", \"blocking\": [], \"suggestions\": [], \
                    \"verified\": []}\n```\nthen\n```json\n{\"summary\": \"new\", \"blocking\": \
                    [\"b\"], \"suggestions\": [\"s\"], \"verified\": [\"v\"]}\n```\ndone";
        let report = parse_report(text).expect("the last block parses");
        assert_eq!(report.summary, "new");
        assert_eq!(report.blocking, vec!["b".to_owned()]);
        assert_eq!(report.suggestions, vec!["s".to_owned()]);
        assert_eq!(report.verified, vec!["v".to_owned()]);
    }

    /// A block with a missing field, a wrong type, no fence, or an unclosed fence has no
    /// report; the empty lists of a passing review are a valid report.
    #[test]
    fn an_incomplete_or_unfenced_report_is_unusable() {
        for text in [
            "no block here",
            "```json\n{\"summary\": \"s\", \"blocking\": []}\n```",
            "```json\n{\"summary\": 1, \"blocking\": [], \"suggestions\": [], \"verified\": []}\n```",
            "```json\n{\"summary\": \"s\", \"blocking\": \"none\", \"suggestions\": [], \
             \"verified\": []}\n```",
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
    /// fenced block the parser reads.
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
        ] {
            assert!(
                prompt.contains(needle),
                "the prompt misses {needle:?}:\n{prompt}"
            );
        }
    }
}
