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

use super::config::{Profile, Stage, StageSpec, effective_stages};
use super::error::GateError;

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

/// The levels a review item may carry. One closed list: the review prompt, the parser, the
/// manual and the drift test that reads it all use this list.
pub const REVIEW_LEVELS: &[&str] = &["critical", "major", "minor"];

/// One finding of a `review` session (ADR 0048 decision 10).
///
/// The object form is `{"level", "file", "line", "text", "pre_existing"}`: `text` is required
/// and the other fields may be absent. A plain string is the same item with only its `text`,
/// which is what a verdict written before the object form holds, so it still reads and still
/// serializes as that string (the verdict set digest is computed from the re-serialized
/// verdict).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewItem {
    /// `critical`, `major` or `minor`; absent when the session did not grade the item.
    pub level: Option<String>,
    /// The path the finding is about.
    pub file: Option<String>,
    /// The line in `file` the finding is about.
    pub line: Option<u32>,
    /// What is wrong.
    pub text: String,
    /// arc, not the session, set this: a rework round moved the item out of `blocking`
    /// because the rework did not touch it.
    pub pre_existing: bool,
}

impl ReviewItem {
    /// An item with only its text.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            level: None,
            file: None,
            line: None,
            text: text.into(),
            pre_existing: false,
        }
    }

    /// Reads the object form, or a plain string as an item with only its `text`.
    ///
    /// `None` when the value is neither, when `text` is missing, when a present field has the
    /// wrong type, or when `level` is not in [`REVIEW_LEVELS`].
    #[must_use]
    pub fn from_json(value: &Value) -> Option<Self> {
        if let Some(text) = value.as_str() {
            return Some(Self::text(text));
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
        let file = match object.get("file") {
            None | Some(Value::Null) => None,
            Some(file) => Some(file.as_str()?.to_owned()),
        };
        let line = match object.get("line") {
            None | Some(Value::Null) => None,
            Some(line) => Some(u32::try_from(line.as_u64()?).ok()?),
        };
        let pre_existing = match object.get("pre_existing") {
            None | Some(Value::Null) => false,
            Some(flag) => flag.as_bool()?,
        };
        Some(Self {
            level,
            file,
            line,
            text,
            pre_existing,
        })
    }
}

/// `[level] path:line text`, leaving out the parts the item does not have. A line without a
/// file names no place, so only a file carries a location.
impl std::fmt::Display for ReviewItem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(level) = &self.level {
            write!(formatter, "[{level}] ")?;
        }
        if let Some(file) = &self.file {
            write!(formatter, "{file}")?;
            if let Some(line) = self.line {
                write!(formatter, ":{line}")?;
            }
            formatter.write_str(" ")?;
        }
        formatter.write_str(&self.text)
    }
}

impl Serialize for ReviewItem {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // A bare item is the plain string, so an old verdict keeps its bytes and its digest.
        if self.level.is_none() && self.file.is_none() && self.line.is_none() && !self.pre_existing
        {
            return serializer.serialize_str(&self.text);
        }
        let mut map = serializer.serialize_map(None)?;
        if let Some(level) = &self.level {
            map.serialize_entry("level", level)?;
        }
        if let Some(file) = &self.file {
            map.serialize_entry("file", file)?;
        }
        if let Some(line) = &self.line {
            map.serialize_entry("line", line)?;
        }
        map.serialize_entry("text", &self.text)?;
        if self.pre_existing {
            map.serialize_entry("pre_existing", &true)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for ReviewItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::from_json(&value).ok_or_else(|| {
            D::Error::custom(
                "a review item is a string, or an object with a `text` and at most a `level` \
                 of critical, major or minor, a `file`, a `line` and `pre_existing`",
            )
        })
    }
}

/// The findings a `review` session reported (ADR 0048 decision 10).
///
/// Field order is the serialized order. A review verdict records all four fields, empty
/// lists included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewReport {
    /// 2 to 6 sentences on what the change does.
    pub summary: String,
    /// What must change before the change can merge. Empty: the stage passes.
    pub blocking: Vec<ReviewItem>,
    /// Improvements that do not block.
    pub suggestions: Vec<ReviewItem>,
    /// What the session checked and found correct.
    pub verified: Vec<String>,
}

/// The settings one review session ran with, and where each came from (ADR 0048
/// decision 10), so `show` can tell a first review from a rework review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSettings {
    /// `first` for a task's first review, `rework` for a later one.
    pub round: String,
    /// The config key the session's `exec_args` came from: `gate.review.first.exec_args`,
    /// `gate.review.exec_args` or `worker.exec_args`.
    pub exec_args: String,
    /// The wall-time limit of one session, in seconds.
    pub timeout_secs: u64,
    /// The config key `timeout_secs` came from: `gate.review.first.timeout_secs` or
    /// `gate.review.timeout_secs`.
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
    /// A `review` verdict of a rework round only: how many blocking items arc moved to
    /// `suggestions` because the rework did not touch them (ADR 0048 decision 10). Absent on a
    /// first review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_pre_existing: Option<u32>,
    /// A `review` verdict only: the settings the session ran with and the keys they came from
    /// (ADR 0048 decision 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_settings: Option<ReviewSettings>,
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
            review_pre_existing: None,
            review_settings: None,
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
