//! Verdicts: the record of one gate stage against one commit (ADR 0048 decisions 3, 6, 10
//! and 11).
//!
//! A verdict is written once, to
//! `<repository>/.radian/arc/runs/<task>/attempts/<attempt>/verdicts/<stage>.json`, with
//! create-new semantics. A second write for the same path is an error. A verdict counts only
//! for the sha it names; when the branch moves, the old verdicts stop counting.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::de::Error as _;
use serde::ser::SerializeMap as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::config::{DEFAULT_FIX_LEVEL, Profile, Stage, StageSpec, effective_stages};
use super::error::GateError;
use super::review::REVIEW_BLOCKING;
use crate::store::config::IntegrationStyle;

/// The verdict schema version this crate writes and accepts.
pub const VERDICT_SCHEMA_VERSION: u64 = 1;

/// The outcome of one stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerdictResult {
    /// The stage passed for its sha.
    Pass,
    /// The stage failed for its sha.
    Fail,
}

/// One command a stage ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRecord {
    /// The command line exactly as the profile wrote it.
    pub line: String,
    /// The exit code, or `null` when the process was killed by a signal (a timeout).
    pub exit_code: Option<i32>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// The log file holding the command's standard output and standard error.
    pub log: String,
}

/// The levels a review finding may carry. One closed list, highest severity first: the review
/// prompt, the parser, the decision rule, the manual and the drift test that reads it all use
/// this list.
pub const REVIEW_LEVELS: &[&str] = &["critical", "major", "minor"];

/// The position of `level` in [`REVIEW_LEVELS`], where `0` is the highest severity.
#[must_use]
pub fn level_rank(level: &str) -> Option<usize> {
    REVIEW_LEVELS.iter().position(|known| *known == level)
}

/// The rank a `fix_level` compares at: an unknown level reads as [`DEFAULT_FIX_LEVEL`], so an
/// old or damaged setting never turns every finding into a failure.
fn fix_rank(fix_level: &str) -> usize {
    level_rank(fix_level)
        .or_else(|| level_rank(super::config::DEFAULT_FIX_LEVEL))
        .expect("the default fix level is in REVIEW_LEVELS")
}

/// `text` without a leading tag that repeats `level`: `[major]`, `[MAJOR]` and `[ major ]`
/// all count, and the spaces after the tag go too. A text that starts with another tag keeps
/// it.
fn strip_level_tag<'a>(text: &'a str, level: &str) -> &'a str {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix('[') else {
        return text;
    };
    let Some((tag, after)) = rest.split_once(']') else {
        return text;
    };
    if tag.trim().eq_ignore_ascii_case(level) {
        after.trim_start()
    } else {
        text
    }
}

/// One finding of a `review` session (ADR 0048 decision 10).
///
/// The object form is `{"severity", "file", "line", "summary", "trigger", "pre_existing"}`:
/// `severity` and `summary` are required, the other fields may be absent, an absent `trigger`
/// reads as empty, and an absent `pre_existing` reads as false.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// One of [`REVIEW_LEVELS`]; always present.
    pub severity: String,
    /// The path the finding is about.
    pub file: Option<String>,
    /// The line in `file` the finding is about.
    pub line: Option<u32>,
    /// What is wrong.
    pub summary: String,
    /// The input or state that shows the problem; empty when the session gave none.
    pub trigger: String,
    /// arc, not the session, set this: a rework round marked the finding pre-existing
    /// because it is not near a line the task's own change added or changed, nor near a
    /// failing finding of the previous review.
    pub pre_existing: bool,
}

impl Finding {
    /// A finding at `severity` with only its summary.
    #[must_use]
    pub fn new(severity: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            severity: severity.into(),
            file: None,
            line: None,
            summary: summary.into(),
            trigger: String::new(),
            pre_existing: false,
        }
    }

    /// Reads the new shape from a verdict arc wrote. `None` when the value is not an object,
    /// when `severity` or `summary` is missing or has the wrong type, when a present field has
    /// the wrong type, or when `severity` is not in [`REVIEW_LEVELS`].
    #[must_use]
    pub fn from_json(value: &Value) -> Option<Self> {
        Self::parse(value, true)
    }

    fn parse(value: &Value, trust_pre_existing: bool) -> Option<Self> {
        let object = value.as_object()?;
        let severity = REVIEW_LEVELS
            .iter()
            .copied()
            .find(|known| object.get("severity").and_then(Value::as_str) == Some(*known))?
            .to_owned();
        let summary = object.get("summary")?.as_str()?.to_owned();
        let file = optional_string(object.get("file"))?;
        let line = match object.get("line") {
            None | Some(Value::Null) => None,
            Some(line) => Some(u32::try_from(line.as_u64()?).ok()?),
        };
        let trigger = optional_string(object.get("trigger"))?.unwrap_or_default();
        let pre_existing = if trust_pre_existing {
            match object.get("pre_existing") {
                None | Some(Value::Null) => false,
                Some(flag) => flag.as_bool()?,
            }
        } else {
            false
        };
        Some(Self {
            severity,
            file,
            line,
            summary,
            trigger,
            pre_existing,
        })
    }

    /// Reads an item of the shape a verdict written before findings holds: a plain string, or
    /// an object with a `text` and at most a `level`. `default_severity` fills in a missing
    /// `level`. An explicit `level` outside [`REVIEW_LEVELS`] makes the whole report unusable.
    fn from_old_json(
        value: &Value,
        default_severity: &str,
        trust_pre_existing: bool,
    ) -> Option<Self> {
        if let Some(text) = value.as_str() {
            return Some(Self::new(default_severity, text));
        }
        let object = value.as_object()?;
        let text = object.get("text")?.as_str()?.to_owned();
        let level = match object.get("level") {
            None | Some(Value::Null) => None,
            Some(level) => Some(
                REVIEW_LEVELS
                    .iter()
                    .copied()
                    .find(|known| level.as_str() == Some(*known))?
                    .to_owned(),
            ),
        };
        let severity = level.clone().unwrap_or_else(|| default_severity.to_owned());
        // The tag is dropped only when the item carried the level itself, exactly as the
        // printed item did before findings existed.
        let summary = match &level {
            Some(level) => strip_level_tag(&text, level).to_owned(),
            None => text,
        };
        let file = optional_string(object.get("file"))?;
        let line = match object.get("line") {
            None | Some(Value::Null) => None,
            Some(line) => Some(u32::try_from(line.as_u64()?).ok()?),
        };
        let pre_existing = if trust_pre_existing {
            match object.get("pre_existing") {
                None | Some(Value::Null) => false,
                Some(flag) => flag.as_bool()?,
            }
        } else {
            false
        };
        Some(Self {
            severity,
            file,
            line,
            summary,
            trigger: String::new(),
            pre_existing,
        })
    }

    /// The finding's `summary` without a leading tag that repeats its own severity, so a
    /// hand-written `[minor]` prints once.
    #[must_use]
    pub fn summary_without_severity_tag(&self) -> &str {
        strip_level_tag(&self.summary, &self.severity)
    }

    /// Whether this finding is at or above `fix_level`: `critical` above `major`, `major`
    /// above `minor`.
    #[must_use]
    pub fn at_or_above(&self, fix_level: &str) -> bool {
        let severity = level_rank(&self.severity).unwrap_or(REVIEW_LEVELS.len());
        severity <= fix_rank(fix_level)
    }
}

/// `[severity] path:line summary — trigger`, leaving out the parts the finding does not have.
/// A line without a file names no place, so only a file carries a location. The trigger part
/// is left out when the trigger is empty. A `summary` that starts with a tag that repeats the
/// finding's own severity loses that tag, so the severity prints once.
impl std::fmt::Display for Finding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "[{}] ", self.severity)?;
        if let Some(file) = &self.file {
            write!(formatter, "{file}")?;
            if let Some(line) = self.line {
                write!(formatter, ":{line}")?;
            }
            formatter.write_str(" ")?;
        }
        formatter.write_str(self.summary_without_severity_tag())?;
        if !self.trigger.is_empty() {
            write!(formatter, " — {}", self.trigger)?;
        }
        Ok(())
    }
}

impl Serialize for Finding {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("severity", &self.severity)?;
        if let Some(file) = &self.file {
            map.serialize_entry("file", file)?;
        }
        if let Some(line) = &self.line {
            map.serialize_entry("line", line)?;
        }
        map.serialize_entry("summary", &self.summary)?;
        if !self.trigger.is_empty() {
            map.serialize_entry("trigger", &self.trigger)?;
        }
        if self.pre_existing {
            map.serialize_entry("pre_existing", &true)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Finding {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::from_json(&value).ok_or_else(|| {
            D::Error::custom(
                "a finding is an object with a `summary` and a `severity` of critical, major \
                 or minor, and at most a `file`, a `line`, a `trigger` and `pre_existing`",
            )
        })
    }
}

fn optional_string(value: Option<&Value>) -> Option<Option<String>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(value) => Some(Some(value.as_str()?.to_owned())),
    }
}

/// The findings a `review` session reported (ADR 0048 decision 10).
///
/// Field order is the serialized order. A review verdict records all three public fields,
/// empty lists included. A verdict written before findings existed holds `blocking` and
/// `suggestions` instead; [`ReviewReport::from_json`] maps them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewReport {
    /// 2 to 6 sentences on what the change does.
    pub summary: String,
    /// Every problem the session found, at every severity.
    pub findings: Vec<Finding>,
    /// What the session checked and found correct.
    pub verified: Vec<String>,
    /// `Some(n)` when the report was read from an old verdict's `blocking` / `suggestions`
    /// shape: the first `n` [`findings`](Self::findings) came from its `blocking` list, and
    /// the rest from its `suggestions`. `None` for a report that carries `findings`, which a
    /// verdict written now and a session's report always do. Never serialized. Only an old
    /// verdict that failed with `review_blocking` uses it, because every item of that list
    /// failed whatever its level, and the `fix_level` rule belongs to a report with
    /// `findings`.
    #[serde(skip)]
    pub old_blocking: Option<usize>,
}

impl ReviewReport {
    /// Reads the new shape from a verdict arc wrote, or maps the old `blocking` /
    /// `suggestions` shape to findings (ADR 0048 decision 10). A report with `findings`
    /// ignores `blocking` and `suggestions` if they are also present.
    ///
    /// An item of `blocking` becomes a finding at its own `level`, else `major`; an item of
    /// `suggestions` at its own `level`, else `minor`; its `text` becomes the summary without
    /// a leading tag that repeats that level, and its `trigger` is empty. `pre_existing` is
    /// kept. `None` on a missing or mistyped field, or a `severity` or `level` outside
    /// [`REVIEW_LEVELS`].
    #[must_use]
    pub fn from_json(value: &Value) -> Option<Self> {
        Self::parse(value, true, true)
    }

    /// Reads a report a review session produced. `pre_existing` is arc's to set, so a value
    /// the session supplies is ignored, and a report in the old shape is mapped for the
    /// `fix_level` rule like any other session report. The rest is [`ReviewReport::from_json`].
    #[must_use]
    pub fn from_session_json(value: &Value) -> Option<Self> {
        Self::parse(value, false, false)
    }

    fn parse(value: &Value, trust_pre_existing: bool, old_blocking: bool) -> Option<Self> {
        let summary = value.get("summary")?.as_str()?.to_owned();
        let verified = strings(value.get("verified")?)?;
        let (findings, old_blocking) = match value.get("findings") {
            Some(findings) => (
                items(findings, |item| Finding::parse(item, trust_pre_existing))?,
                None,
            ),
            None => {
                let blocking = items(value.get("blocking")?, |item| {
                    Finding::from_old_json(item, "major", trust_pre_existing)
                })?;
                let count = blocking.len();
                let mut findings = blocking;
                findings.extend(items(value.get("suggestions")?, |item| {
                    Finding::from_old_json(item, "minor", trust_pre_existing)
                })?);
                (findings, old_blocking.then_some(count))
            }
        };
        Some(Self {
            summary,
            findings,
            verified,
            old_blocking,
        })
    }

    /// The failing findings for `fix_level`: at or above that level and not `pre_existing`.
    /// These are the findings that send the task back.
    #[must_use]
    pub fn failing(&self, fix_level: &str) -> Vec<&Finding> {
        self.failing_with(fix_level, false)
    }

    /// The failing findings of a verdict that failed with `review_blocking` before findings
    /// existed: every item of its old `blocking` list is failing, whatever its level and
    /// whatever `fix_level` says, because that verdict already decided it. Its `suggestions`
    /// are never failing, and `fix_level` does not decide them either. A report that carries
    /// `findings` is decided by `fix_level` whatever its reason, because the `fix_level` rule
    /// belongs to a report with `findings`.
    #[must_use]
    pub fn failing_in_old_verdict(&self, fix_level: &str) -> Vec<&Finding> {
        self.failing_with(fix_level, true)
    }

    fn failing_with(&self, fix_level: &str, old_blocking: bool) -> Vec<&Finding> {
        self.findings
            .iter()
            .enumerate()
            .filter(|(index, finding)| match self.old_blocking {
                Some(count) if old_blocking => *index < count,
                _ => !finding.pre_existing && finding.at_or_above(fix_level),
            })
            .map(|(_, finding)| finding)
            .collect()
    }

    /// The rest: below `fix_level`, or `pre_existing`. They are recorded and collected but
    /// never send the task back.
    #[must_use]
    pub fn rest(&self, fix_level: &str) -> Vec<&Finding> {
        self.rest_with(fix_level, false)
    }

    /// The rest of a verdict that failed with `review_blocking`: its old `blocking` items are
    /// never here, and its `suggestions` always are, whatever their level.
    #[must_use]
    pub fn rest_in_old_verdict(&self, fix_level: &str) -> Vec<&Finding> {
        self.rest_with(fix_level, true)
    }

    fn rest_with(&self, fix_level: &str, old_blocking: bool) -> Vec<&Finding> {
        self.findings
            .iter()
            .enumerate()
            .filter(|(index, finding)| match self.old_blocking {
                Some(count) if old_blocking => *index >= count,
                _ => finding.pre_existing || !finding.at_or_above(fix_level),
            })
            .map(|(_, finding)| finding)
            .collect()
    }

    /// Every finding, highest severity first, keeping the order within one severity.
    #[must_use]
    pub fn sorted_by_severity(&self) -> Vec<&Finding> {
        let mut findings: Vec<&Finding> = self.findings.iter().collect();
        findings
            .sort_by_key(|finding| level_rank(&finding.severity).unwrap_or(REVIEW_LEVELS.len()));
        findings
    }
}

impl<'de> Deserialize<'de> for ReviewReport {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::from_json(&value).ok_or_else(|| {
            D::Error::custom(
                "a review report is an object with a `summary`, a list of `findings` (or the \
                 old `blocking` and `suggestions` lists) and a `verified` list",
            )
        })
    }
}

fn items(value: &Value, parse: impl Fn(&Value) -> Option<Finding>) -> Option<Vec<Finding>> {
    value.as_array()?.iter().map(parse).collect()
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect()
}

/// The settings one review session ran with, and where each came from (ADR 0048
/// decision 10), so `show` can tell a first review from a rework review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSettings {
    /// `first` for a task's first review, `rework` for a later one.
    pub round: String,
    /// The reviewer whose `[gate.reviewers.<name>]` section the session used (ADR 0048
    /// decision 15). Absent on a verdict written before the field existed.
    #[serde(default)]
    pub reviewer: String,
    /// The reviewer's `fix_level` (ADR 0048 decision 10): a finding at or above it and not
    /// `pre_existing` fails the round. Absent on a verdict written before the field existed.
    #[serde(default)]
    pub fix_level: String,
    /// The config key the session's `exec_args` came from: `gate.reviewers.<name>.first.exec_args`,
    /// `gate.reviewers.<name>.exec_args`, their `gate.review` legacy spellings, or
    /// `worker.exec_args`.
    pub exec_args: String,
    /// The wall-time limit of one session, in seconds.
    pub timeout_secs: u64,
    /// The config key `timeout_secs` came from: `gate.reviewers.<name>.first.timeout_secs`,
    /// `gate.reviewers.<name>.timeout_secs`, or their `gate.review` legacy spellings.
    pub timeout: String,
}

/// The verdict of one stage for one commit.
///
/// Field order is the serialized order and is part of the canonical form the digest hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    /// The verdict schema version.
    pub schema_version: u64,
    /// The stage that produced this verdict.
    pub stage: Stage,
    /// The commit the verdict is for: the task's `verified_sha` for a verification stage,
    /// `head_sha` for integration, the approved commit for a human verdict. A verification
    /// stage runs on `candidate_sha`, not necessarily on this commit.
    pub sha: String,
    /// A verification stage only: the remote main the candidate was built on (ADR 0048
    /// decision 10). `sha` stays `verified_sha`; `candidate_sha` is the commit the stage ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_sha: Option<String>,
    /// The outcome.
    pub result: VerdictResult,
    /// Why the stage failed: `command_failed`, `timeout`, `conflict`, the owner's note for a
    /// human verdict, or `null` on a pass.
    pub reason: Option<String>,
    /// The commands the stage ran, in order. Empty for a human verdict.
    pub commands: Vec<CommandRecord>,
    /// When the stage started, RFC 3339 in UTC.
    pub started_at: String,
    /// When the stage ended, RFC 3339 in UTC.
    pub ended_at: String,
    /// Integration only: the main commit the candidate was built on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// A verification or integration verdict: the commit the stage ran on. For a
    /// verification stage it is `sha` itself when the remote main is an ancestor, otherwise
    /// the merge commit; `None` when the merge conflicted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_sha: Option<String>,
    /// A verification or integration verdict: the conflicted paths that made a merge fail,
    /// sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_paths: Vec<String>,
    /// A `review` verdict only: who approved it. `auto` is the scheduling loop at auto level
    /// `merge`; absent means the owner approved it with `radian arc merge` (ADR 0048
    /// decision 12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approver: Option<String>,
    /// A `review` verdict only: the findings the session reported (ADR 0048 decision 10).
    /// Absent when the stage ran no session (a candidate conflict) or its output was unusable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewReport>,
    /// A `review` verdict only: the session id of every run the stage made, in order. Two
    /// entries mean the first run's output was unusable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_session_ids: Vec<String>,
    /// A `review` verdict only: the summed cost of the runs, in micros of a US dollar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_cost_micros: Option<u64>,
    /// A `review` verdict only: the failing session's `Result.model_error` object as read
    /// (ADR 0044 §2b), present only when the stage failed with reason `review_unavailable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_model_error: Option<Value>,
    /// A `review` verdict of a rework round only: how many blocking items arc moved to
    /// `suggestions` as `pre_existing` (ADR 0048 decision 10). Absent on a first review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_pre_existing: Option<u32>,
    /// A `review` verdict only: the settings the session ran with and the keys they came from
    /// (ADR 0048 decision 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_settings: Option<ReviewSettings>,
    /// The gate profile the stage ran under (ADR 0048 decisions 10 and 15). Absent on a
    /// verdict written before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Integration only: the `[integration] style` the candidate was built with. Absent on a
    /// verdict written before the field existed, which is `merge`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_style: Option<IntegrationStyle>,
}

impl Verdict {
    /// The owner's `human` verdict for `sha`.
    ///
    /// The owner action (no CLI in this slice) calls this constructor and then
    /// [`Verdict::write_new`]. A failed human verdict carries its note as the reason.
    #[must_use]
    pub fn human(sha: impl Into<String>, pass: bool, note: Option<String>) -> Self {
        let at = now_utc();
        Self {
            schema_version: VERDICT_SCHEMA_VERSION,
            stage: Stage::Human,
            sha: sha.into(),
            result: if pass {
                VerdictResult::Pass
            } else {
                VerdictResult::Fail
            },
            reason: note,
            commands: Vec::new(),
            started_at: at.clone(),
            ended_at: at,
            main_sha: None,
            base_sha: None,
            candidate_sha: None,
            conflict_paths: Vec::new(),
            approver: None,
            review: None,
            review_session_ids: Vec::new(),
            review_cost_micros: None,
            review_model_error: None,
            review_pre_existing: None,
            review_settings: None,
            profile: None,
            integration_style: None,
        }
    }

    /// A passing `human` verdict written by the scheduling loop at auto level `merge`.
    #[must_use]
    pub fn approved_by(sha: impl Into<String>, approver: &str) -> Self {
        let mut verdict = Self::human(sha, true, None);
        verdict.approver = Some(approver.to_owned());
        verdict
    }

    /// Whether this verdict is a pass.
    #[must_use]
    pub fn is_pass(&self) -> bool {
        self.result == VerdictResult::Pass
    }

    /// The failing findings of this review verdict (ADR 0048 decision 10): at or above the
    /// `fix_level` it recorded (the default when it recorded none, or a damaged one) and not
    /// `pre_existing`. A verdict that failed with `review_blocking` was decided before
    /// findings existed, so every item of its old `blocking` list is failing whatever its
    /// level, and its `suggestions` never are. Empty for a verdict with no report.
    #[must_use]
    pub fn review_failing(&self) -> Vec<&Finding> {
        let Some(report) = &self.review else {
            return Vec::new();
        };
        let fix_level = self.recorded_fix_level();
        if self.reason.as_deref() == Some(REVIEW_BLOCKING) {
            report.failing_in_old_verdict(fix_level)
        } else {
            report.failing(fix_level)
        }
    }

    /// The rest of this review verdict's findings: below the `fix_level`, or `pre_existing`;
    /// for a verdict that failed with `review_blocking`, its old `suggestions`. Empty for a
    /// verdict with no report.
    #[must_use]
    pub fn review_rest(&self) -> Vec<&Finding> {
        let Some(report) = &self.review else {
            return Vec::new();
        };
        let fix_level = self.recorded_fix_level();
        if self.reason.as_deref() == Some(REVIEW_BLOCKING) {
            report.rest_in_old_verdict(fix_level)
        } else {
            report.rest(fix_level)
        }
    }

    /// The `fix_level` this review verdict was decided with: `review_settings.fix_level`, else
    /// the default for a verdict written before the field existed. An empty or damaged value
    /// reads as the default through [`Finding::at_or_above`].
    fn recorded_fix_level(&self) -> &str {
        self.review_settings
            .as_ref()
            .map_or(DEFAULT_FIX_LEVEL, |settings| settings.fix_level.as_str())
    }

    /// Writes this verdict, refusing to replace an existing file.
    ///
    /// The parent directory is created. A verdict is never changed (ADR 0048 decision 3).
    ///
    /// # Errors
    ///
    /// [`GateError::VerdictExists`] when `path` already exists, or an I/O error.
    pub fn write_new(&self, path: &Path) -> Result<(), GateError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| GateError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::AlreadyExists {
                    GateError::VerdictExists(path.to_path_buf())
                } else {
                    GateError::Io {
                        path: path.to_path_buf(),
                        source,
                    }
                }
            })?;
        let mut body = serde_json::to_vec_pretty(self).map_err(|source| GateError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other(source),
        })?;
        body.push(b'\n');
        file.write_all(&body).map_err(|source| GateError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        file.flush().map_err(|source| GateError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Reads one verdict file.
    ///
    /// # Errors
    ///
    /// The file cannot be read or decoded.
    pub fn read(path: &Path) -> Result<Self, GateError> {
        let text = std::fs::read_to_string(path).map_err(|source| GateError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|source| GateError::MalformedVerdict {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// The attempt directory, `<repository>/.radian/arc/runs/<task>/attempts/<attempt>/`.
///
/// # Errors
///
/// A `task` or `attempt` that is not one safe path component.
pub fn attempt_dir(repository: &Path, task: &str, attempt: &str) -> Result<PathBuf, GateError> {
    check_component(task)?;
    check_component(attempt)?;
    Ok(repository
        .join(".radian")
        .join("arc")
        .join("runs")
        .join(task)
        .join("attempts")
        .join(attempt))
}

/// The verdict file of one stage in one attempt,
/// `<attempt_dir>/verdicts/<stage>.json`.
///
/// # Errors
///
/// A `task` or `attempt` that is not one safe path component.
pub fn verdict_path(
    repository: &Path,
    task: &str,
    attempt: &str,
    stage: Stage,
) -> Result<PathBuf, GateError> {
    Ok(attempt_dir(repository, task, attempt)?
        .join("verdicts")
        .join(format!("{}.json", stage.as_str())))
}

/// The verdicts that count for `head_sha`: only verdicts whose `sha` is that commit.
#[must_use]
pub fn valid_verdicts<'a>(verdicts: &'a [Verdict], head_sha: &str) -> Vec<&'a Verdict> {
    verdicts
        .iter()
        .filter(|verdict| verdict.sha == head_sha)
        .collect()
}

/// Whether every required stage of the effective profile passed for `sha`.
///
/// The check reads the same counted set as [`verdict_set_digest`]: the newest verdict per
/// stage for that sha. A newer attempt that failed a stage therefore outweighs the pass of
/// an older attempt, and a caller may pass every attempt's verdicts in any order.
///
/// `integration` is not part of this check: it always runs at merge, after the owner's
/// approval, not before `review.awaiting_human`.
#[must_use]
pub fn required_passed(
    profile: &Profile,
    extra: &[StageSpec],
    verdicts: &[Verdict],
    sha: &str,
) -> bool {
    let counted = counted_verdicts(verdicts, sha);
    effective_stages(profile, extra)
        .iter()
        .filter(|spec| spec.required)
        .all(|spec| {
            counted
                .iter()
                .any(|verdict| verdict.stage == spec.stage && verdict.is_pass())
        })
}

/// The digest of the verdict set that counts for `sha` (ADR 0048 decision 11).
///
/// The digest is the SHA-256 of the compact JSON array of the counted verdicts, sorted by
/// stage name, in the struct's field order. When the same stage appears more than once for
/// the sha (verdicts of several attempts), the newest by `ended_at` counts, with
/// `started_at` and then the verdict's own JSON as tie-breaks, so the digest does not depend
/// on the order the caller passes the verdicts in.
#[must_use]
pub fn verdict_set_digest(verdicts: &[Verdict], sha: &str) -> String {
    let counted = counted_verdicts(verdicts, sha);
    let canonical = serde_json::to_string(&counted).expect("verdicts serialize");
    hex(&Sha256::digest(canonical.as_bytes()))
}

fn counted_verdicts<'a>(verdicts: &'a [Verdict], sha: &str) -> Vec<&'a Verdict> {
    let mut by_stage: BTreeMap<&'static str, &'a Verdict> = BTreeMap::new();
    for verdict in verdicts.iter().filter(|verdict| verdict.sha == sha) {
        let key = verdict.stage.as_str();
        let replace = by_stage
            .get(key)
            .is_none_or(|current| precedence(verdict) > precedence(current));
        if replace {
            by_stage.insert(key, verdict);
        }
    }
    by_stage.into_values().collect()
}

fn precedence(verdict: &Verdict) -> (String, String, String) {
    (
        verdict.ended_at.clone(),
        verdict.started_at.clone(),
        serde_json::to_string(verdict).expect("verdicts serialize"),
    )
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn check_component(component: &str) -> Result<(), GateError> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains('/')
        || component.contains('\\')
        || component.contains('\0')
    {
        return Err(GateError::InvalidId(component.to_string()));
    }
    Ok(())
}

/// The current time in the verdict format: RFC 3339 UTC with microseconds, like the session
/// and memory logs.
#[must_use]
pub fn now_utc() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}
