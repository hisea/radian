//! The two files arc keeps per repository: one plan, and one task.
//!
//! A plan is one product request and the tasks that deliver it; a task is one unit of work
//! with a brief. Each file is YAML front matter (the fields) and a body (the request text
//! or the brief). The front matter field names, their order and their closed value sets
//! are fixed: later slices fill the optional sha and attempt fields without renaming
//! anything here.

use std::collections::BTreeMap;

use super::config::STAGE_NAMES;
use super::frontmatter::{self, Value};

/// The plan and task file schema version this build writes and accepts.
pub const SCHEMA_VERSION: u32 = 1;

/// A plan's `status`: `done` is derived from the tasks, never stored.
pub const PLAN_STATUSES: &[&str] = &["draft", "confirmed"];

/// A plan's `auto` level. Only a `confirmed` plan may be other than `off`.
pub const AUTO_LEVELS: &[&str] = &["off", "run", "merge"];

/// A task's `state`, in the order the manual's table lists it.
pub const TASK_STATES: &[&str] = &["todo", "doing", "review", "done", "failed"];

/// Each state's phases. A task's `phase` is one of its state's entries, and nothing else.
pub const TASK_PHASES: &[(&str, &[&str])] = &[
    ("todo", &["ready", "blocked", "retry_wait", "paused_budget"]),
    ("doing", &["starting", "running", "reconciling"]),
    (
        "review",
        &[
            "verifying",
            "rework_pending",
            "awaiting_human",
            "integrating",
            "ready_to_merge",
        ],
    ),
    ("done", &["merged"]),
    ("failed", &["exhausted", "cancelled", "irrecoverable"]),
];

/// The longest slug, in characters.
pub const SLUG_MAX_CHARS: usize = 48;

/// A slug: 1 to 48 characters from `[a-z0-9-]`, with no leading or trailing `-`.
pub fn valid_slug(slug: &str) -> bool {
    let count = slug.chars().count();
    (1..=SLUG_MAX_CHARS).contains(&count)
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-')
        && !slug.ends_with('-')
}

/// A stored id: a four-digit number, `-`, and a valid slug. Also the check that refuses an
/// id that would leave `plans/` or `tasks/`.
pub fn valid_id(id: &str) -> bool {
    match id.split_once('-') {
        Some((number, slug)) => {
            number.len() == 4
                && number.chars().all(|c| c.is_ascii_digit())
                && number != "0000"
                && valid_slug(slug)
        }
        None => false,
    }
}

/// The id for `number` and `slug`. Numbers are never padded past four digits: a fifth
/// digit is refused before this is called.
pub fn format_id(number: u32, slug: &str) -> String {
    format!("{number:04}-{slug}")
}

/// The number an id starts with.
pub fn id_number(id: &str) -> Option<u32> {
    id.split_once('-')
        .and_then(|(number, _)| number.parse().ok())
}

/// A task's state, as a value rather than a string (the file keeps the string).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Todo,
    Doing,
    Review,
    Done,
    Failed,
}

impl TaskState {
    /// Every state, in [`TASK_STATES`] order.
    pub const ALL: &'static [TaskState] = &[
        TaskState::Todo,
        TaskState::Doing,
        TaskState::Review,
        TaskState::Done,
        TaskState::Failed,
    ];

    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Todo => "todo",
            TaskState::Doing => "doing",
            TaskState::Review => "review",
            TaskState::Done => "done",
            TaskState::Failed => "failed",
        }
    }

    /// The phases this state allows, from [`TASK_PHASES`].
    pub fn phases(self) -> &'static [&'static str] {
        TASK_PHASES
            .iter()
            .find(|(state, _)| *state == self.as_str())
            .map(|(_, phases)| *phases)
            .unwrap_or(&[])
    }

    /// The state a stored spelling names.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|state| state.as_str() == text)
    }
}

/// Whether `phase` belongs to `state`.
pub fn valid_phase(state: TaskState, phase: &str) -> bool {
    state.phases().contains(&phase)
}

/// One plan file.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanFile {
    pub schema_version: u32,
    pub id: String,
    pub title: String,
    pub status: String,
    pub auto: String,
    pub max_cost_usd: Option<f64>,
    pub max_wall_time_secs: Option<u64>,
    /// The plan's tasks, in display order.
    pub tasks: Vec<String>,
    /// The request text, then the `## Change records` section.
    pub body: String,
}

impl PlanFile {
    /// A new `draft` plan with no tasks, holding `request` as its text.
    pub fn new(id: String, title: String, request: &str) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            id,
            title,
            status: "draft".to_owned(),
            auto: "off".to_owned(),
            max_cost_usd: None,
            max_wall_time_secs: None,
            tasks: Vec::new(),
            body: plan_body(request),
        }
    }

    /// The file's bytes.
    pub fn to_text(&self) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("schema_version: {}\n", self.schema_version));
        out.push_str(&format!("id: {}\n", frontmatter::scalar(&self.id)));
        out.push_str(&format!("title: {}\n", frontmatter::scalar(&self.title)));
        out.push_str(&format!("status: {}\n", self.status));
        out.push_str(&format!("auto: {}\n", self.auto));
        if let Some(limit) = self.max_cost_usd {
            out.push_str(&format!("max_cost_usd: {limit}\n"));
        }
        if let Some(limit) = self.max_wall_time_secs {
            out.push_str(&format!("max_wall_time_secs: {limit}\n"));
        }
        out.push_str(&frontmatter::list_field("tasks", &self.tasks));
        out.push_str("---\n");
        out.push_str(&self.body);
        out
    }

    /// Reads one file. An unknown key, a wrong type, an unknown `status` or `auto`, and a
    /// missing required field are errors that name the field.
    pub fn from_text(text: &str) -> Result<Self, String> {
        let document = frontmatter::parse(text)?;
        let mut fields = Fields::new(document.fields);
        let schema_version = fields.required_u32("schema_version")?;
        if schema_version != SCHEMA_VERSION {
            return Err(format!(
                "unsupported schema_version {schema_version} (this build reads {SCHEMA_VERSION})"
            ));
        }
        let id = fields.required_string("id")?;
        if !valid_id(&id) {
            return Err(format!("`id` `{id}` is not a four-digit number and a slug"));
        }
        let title = fields.required_string("title")?;
        let status = fields.required_string("status")?;
        if !PLAN_STATUSES.contains(&status.as_str()) {
            return Err(format!(
                "`status` `{status}` is not one of {}",
                PLAN_STATUSES.join(", ")
            ));
        }
        let auto = fields.required_string("auto")?;
        if !AUTO_LEVELS.contains(&auto.as_str()) {
            return Err(format!(
                "`auto` `{auto}` is not one of {}",
                AUTO_LEVELS.join(", ")
            ));
        }
        let max_cost_usd = fields.optional_f64("max_cost_usd")?;
        let max_wall_time_secs = fields.optional_u64("max_wall_time_secs")?;
        let tasks = fields.optional_list("tasks")?;
        fields.finish()?;
        Ok(Self {
            schema_version,
            id,
            title,
            status,
            auto,
            max_cost_usd,
            max_wall_time_secs,
            tasks,
            body: document.body,
        })
    }
}

/// One task file.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskFile {
    pub schema_version: u32,
    pub id: String,
    pub plan: String,
    pub title: String,
    pub state: TaskState,
    pub phase: String,
    /// Tasks that must be merged before this one starts.
    pub after: Vec<String>,
    pub supersedes: Option<String>,
    pub attempt: u32,
    pub allow_noop: bool,
    /// Gate stages this brief adds to the profile's.
    pub extra_stages: Vec<String>,
    pub max_attempts: Option<u32>,
    pub max_cost_usd: Option<f64>,
    pub max_wall_time_secs: Option<u64>,
    pub stall_timeout_secs: Option<u64>,
    /// Frozen v0 fields: later slices fill these, the names do not change.
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    pub verified_sha: Option<String>,
    /// An absolute UTC time, `YYYY-MM-DDTHH:MM:SSZ`.
    pub retry_at: Option<String>,
    /// The brief.
    pub body: String,
}

impl TaskFile {
    /// A new `todo`/`ready` task with no attempt yet.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        plan: String,
        title: String,
        after: Vec<String>,
        allow_noop: bool,
        extra_stages: Vec<String>,
        brief: &str,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            id,
            plan,
            title,
            state: TaskState::Todo,
            phase: "ready".to_owned(),
            after,
            supersedes: None,
            attempt: 0,
            allow_noop,
            extra_stages,
            max_attempts: None,
            max_cost_usd: None,
            max_wall_time_secs: None,
            stall_timeout_secs: None,
            base_sha: None,
            head_sha: None,
            verified_sha: None,
            retry_at: None,
            body: brief_body(brief),
        }
    }

    /// The file's bytes.
    pub fn to_text(&self) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("schema_version: {}\n", self.schema_version));
        out.push_str(&format!("id: {}\n", frontmatter::scalar(&self.id)));
        out.push_str(&format!("plan: {}\n", frontmatter::scalar(&self.plan)));
        out.push_str(&format!("title: {}\n", frontmatter::scalar(&self.title)));
        out.push_str(&format!("state: {}\n", self.state.as_str()));
        out.push_str(&format!("phase: {}\n", self.phase));
        out.push_str(&frontmatter::list_field("after", &self.after));
        if let Some(id) = &self.supersedes {
            out.push_str(&format!("supersedes: {}\n", frontmatter::scalar(id)));
        }
        out.push_str(&format!("attempt: {}\n", self.attempt));
        out.push_str(&format!("allow_noop: {}\n", self.allow_noop));
        out.push_str(&frontmatter::list_field("extra_stages", &self.extra_stages));
        if let Some(limit) = self.max_attempts {
            out.push_str(&format!("max_attempts: {limit}\n"));
        }
        if let Some(limit) = self.max_cost_usd {
            out.push_str(&format!("max_cost_usd: {limit}\n"));
        }
        if let Some(limit) = self.max_wall_time_secs {
            out.push_str(&format!("max_wall_time_secs: {limit}\n"));
        }
        if let Some(limit) = self.stall_timeout_secs {
            out.push_str(&format!("stall_timeout_secs: {limit}\n"));
        }
        if let Some(sha) = &self.base_sha {
            out.push_str(&format!("base_sha: {}\n", frontmatter::scalar(sha)));
        }
        if let Some(sha) = &self.head_sha {
            out.push_str(&format!("head_sha: {}\n", frontmatter::scalar(sha)));
        }
        if let Some(sha) = &self.verified_sha {
            out.push_str(&format!("verified_sha: {}\n", frontmatter::scalar(sha)));
        }
        if let Some(retry_at) = &self.retry_at {
            out.push_str(&format!("retry_at: {}\n", frontmatter::scalar(retry_at)));
        }
        out.push_str("---\n");
        out.push_str(&self.body);
        out
    }

    /// Reads one file. An unknown key, a wrong type, an unknown `state`, a `phase` its
    /// state does not allow, or an unknown `extra_stages` name are errors that name the
    /// field.
    pub fn from_text(text: &str) -> Result<Self, String> {
        let document = frontmatter::parse(text)?;
        let mut fields = Fields::new(document.fields);
        let schema_version = fields.required_u32("schema_version")?;
        if schema_version != SCHEMA_VERSION {
            return Err(format!(
                "unsupported schema_version {schema_version} (this build reads {SCHEMA_VERSION})"
            ));
        }
        let id = fields.required_string("id")?;
        if !valid_id(&id) {
            return Err(format!("`id` `{id}` is not a four-digit number and a slug"));
        }
        let plan = fields.required_string("plan")?;
        if !valid_id(&plan) {
            return Err(format!(
                "`plan` `{plan}` is not a four-digit number and a slug"
            ));
        }
        let title = fields.required_string("title")?;
        let state_text = fields.required_string("state")?;
        let Some(state) = TaskState::parse(&state_text) else {
            return Err(format!(
                "`state` `{state_text}` is not one of {}",
                TASK_STATES.join(", ")
            ));
        };
        let phase = fields.required_string("phase")?;
        if !valid_phase(state, &phase) {
            return Err(format!(
                "`phase` `{phase}` is not one of the {} phases: {}",
                state.as_str(),
                state.phases().join(", ")
            ));
        }
        let after = fields.optional_list("after")?;
        let supersedes = fields.optional_string("supersedes")?;
        let attempt = fields.optional_u32("attempt")?.unwrap_or(0);
        let allow_noop = fields.optional_bool("allow_noop")?.unwrap_or(false);
        let extra_stages = fields.optional_list("extra_stages")?;
        for stage in &extra_stages {
            if !STAGE_NAMES.contains(&stage.as_str()) {
                return Err(format!(
                    "`extra_stages` names `{stage}`; stages are {}",
                    STAGE_NAMES.join(", ")
                ));
            }
        }
        let max_attempts = fields.optional_u32("max_attempts")?;
        let max_cost_usd = fields.optional_f64("max_cost_usd")?;
        let max_wall_time_secs = fields.optional_u64("max_wall_time_secs")?;
        let stall_timeout_secs = fields.optional_u64("stall_timeout_secs")?;
        let base_sha = fields.optional_string("base_sha")?;
        let head_sha = fields.optional_string("head_sha")?;
        let verified_sha = fields.optional_string("verified_sha")?;
        let retry_at = fields.optional_string("retry_at")?;
        if let Some(retry_at) = &retry_at
            && !valid_utc(retry_at)
        {
            return Err(format!(
                "`retry_at` `{retry_at}` is not an absolute UTC time (YYYY-MM-DDTHH:MM:SSZ)"
            ));
        }
        fields.finish()?;
        Ok(Self {
            schema_version,
            id,
            plan,
            title,
            state,
            phase,
            after,
            supersedes,
            attempt,
            allow_noop,
            extra_stages,
            max_attempts,
            max_cost_usd,
            max_wall_time_secs,
            stall_timeout_secs,
            base_sha,
            head_sha,
            verified_sha,
            retry_at,
            body: document.body,
        })
    }
}

/// The body a new plan file gets: the request text, then the section change records are
/// appended to.
pub fn plan_body(request: &str) -> String {
    format!("{}\n\n## Change records\n", request.trim_end())
}

/// The body a new task file gets: the brief.
pub fn brief_body(brief: &str) -> String {
    let brief = brief.trim_end();
    if brief.is_empty() {
        return String::new();
    }
    format!("{brief}\n")
}

/// Whether `text` is `YYYY-MM-DDTHH:MM:SSZ` (the stored shape of an absolute UTC time).
pub fn valid_utc(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 20 {
        return false;
    }
    let digits_at = |range: std::ops::Range<usize>| {
        bytes[range.clone()]
            .iter()
            .all(|byte| byte.is_ascii_digit())
    };
    bytes[10] == b'T'
        && bytes[19] == b'Z'
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && digits_at(0..4)
        && digits_at(5..7)
        && digits_at(8..10)
        && digits_at(11..13)
        && digits_at(14..16)
        && digits_at(17..19)
}

/// The typed reader over one front matter map: every accessor names the field in its
/// error, and [`Fields::finish`] rejects what is left over.
struct Fields {
    fields: BTreeMap<String, Value>,
}

impl Fields {
    fn new(fields: BTreeMap<String, Value>) -> Self {
        Self { fields }
    }

    fn take(&mut self, key: &str) -> Result<Option<Value>, String> {
        Ok(self.fields.remove(key))
    }

    fn required_string(&mut self, key: &str) -> Result<String, String> {
        match self.take(key)? {
            None => Err(format!("missing `{key}`")),
            Some(Value::Scalar(value)) => Ok(value),
            Some(Value::List(_)) => Err(format!("`{key}` must be a scalar, not a list")),
        }
    }

    fn optional_string(&mut self, key: &str) -> Result<Option<String>, String> {
        match self.take(key)? {
            None => Ok(None),
            Some(Value::Scalar(value)) => Ok(Some(value)),
            Some(Value::List(_)) => Err(format!("`{key}` must be a scalar, not a list")),
        }
    }

    fn required_u32(&mut self, key: &str) -> Result<u32, String> {
        let text = self.required_string(key)?;
        parse_number(key, &text)
    }

    fn optional_u32(&mut self, key: &str) -> Result<Option<u32>, String> {
        match self.optional_string(key)? {
            None => Ok(None),
            Some(text) => parse_number(key, &text).map(Some),
        }
    }

    fn optional_u64(&mut self, key: &str) -> Result<Option<u64>, String> {
        match self.optional_string(key)? {
            None => Ok(None),
            Some(text) => Ok(Some(text.parse::<u64>().map_err(|_| {
                format!("`{key}` must be a non-negative integer, not `{text}`")
            })?)),
        }
    }

    fn optional_f64(&mut self, key: &str) -> Result<Option<f64>, String> {
        match self.optional_string(key)? {
            None => Ok(None),
            Some(text) => {
                Ok(Some(text.parse::<f64>().map_err(|_| {
                    format!("`{key}` must be a number, not `{text}`")
                })?))
            }
        }
    }

    fn optional_bool(&mut self, key: &str) -> Result<Option<bool>, String> {
        match self.optional_string(key)? {
            None => Ok(None),
            Some(text) => match text.as_str() {
                "true" => Ok(Some(true)),
                "false" => Ok(Some(false)),
                other => Err(format!("`{key}` must be `true` or `false`, not `{other}`")),
            },
        }
    }

    fn optional_list(&mut self, key: &str) -> Result<Vec<String>, String> {
        match self.take(key)? {
            None => Ok(Vec::new()),
            Some(Value::List(values)) => Ok(values),
            Some(Value::Scalar(_)) => Err(format!("`{key}` must be a list")),
        }
    }

    /// Rejects every field the reader did not consume, naming them all.
    fn finish(self) -> Result<(), String> {
        if self.fields.is_empty() {
            return Ok(());
        }
        let keys: Vec<&str> = self.fields.keys().map(String::as_str).collect();
        Err(format!("unknown field(s): {}", keys.join(", ")))
    }
}

fn parse_number(key: &str, text: &str) -> Result<u32, String> {
    text.parse::<u32>()
        .map_err(|_| format!("`{key}` must be a non-negative integer, not `{text}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan() -> PlanFile {
        PlanFile {
            schema_version: SCHEMA_VERSION,
            id: "0001-first".to_owned(),
            title: "First: a plan".to_owned(),
            status: "draft".to_owned(),
            auto: "off".to_owned(),
            max_cost_usd: Some(5.0),
            max_wall_time_secs: Some(7200),
            tasks: vec!["0001-one".to_owned(), "0002-two".to_owned()],
            body: plan_body("do the thing"),
        }
    }

    fn sample_task() -> TaskFile {
        TaskFile {
            schema_version: SCHEMA_VERSION,
            id: "0002-two".to_owned(),
            plan: "0001-first".to_owned(),
            title: "Second task".to_owned(),
            state: TaskState::Review,
            phase: "awaiting_human".to_owned(),
            after: vec!["0001-one".to_owned()],
            supersedes: Some("0009-old".to_owned()),
            attempt: 2,
            allow_noop: true,
            extra_stages: vec!["mutation".to_owned(), "review".to_owned()],
            max_attempts: Some(2),
            max_cost_usd: Some(1.5),
            max_wall_time_secs: Some(600),
            stall_timeout_secs: Some(300),
            base_sha: Some("a".repeat(40)),
            head_sha: Some("b".repeat(40)),
            verified_sha: Some("c".repeat(40)),
            retry_at: Some("2026-10-22T10:00:00Z".to_owned()),
            body: brief_body("do it"),
        }
    }

    #[test]
    fn a_plan_file_round_trips() {
        let plan = sample_plan();
        let text = plan.to_text();
        assert_eq!(PlanFile::from_text(&text).unwrap(), plan);
    }

    #[test]
    fn a_task_file_round_trips() {
        let task = sample_task();
        let text = task.to_text();
        assert_eq!(TaskFile::from_text(&text).unwrap(), task);
    }

    #[test]
    fn a_new_plan_and_task_are_draft_and_ready() {
        let plan = PlanFile::new("0001-a".into(), "A".into(), "the request\n");
        assert!(plan.to_text().contains("status: draft\nauto: off\n"));
        assert!(plan.body.ends_with("## Change records\n"));
        let task = TaskFile::new(
            "0001-a".into(),
            "0001-a".into(),
            "A".into(),
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        );
        let text = task.to_text();
        assert!(text.contains("state: todo\nphase: ready\n"), "{text}");
        assert!(text.contains("attempt: 0\n"), "{text}");
        assert_eq!(TaskFile::from_text(&text).unwrap(), task);
    }

    #[test]
    fn an_unknown_field_is_named() {
        let text = sample_task()
            .to_text()
            .replace("attempt: 2", "attempt: 2\nverdicts: []");
        let error = TaskFile::from_text(&text).expect_err("an unknown field must fail");
        assert!(error.contains("verdicts"), "{error}");
    }

    #[test]
    fn a_phase_the_state_does_not_allow_is_refused() {
        let text = sample_task()
            .to_text()
            .replace("state: review", "state: todo");
        let error = TaskFile::from_text(&text).expect_err("awaiting_human is not a todo phase");
        assert!(error.contains("awaiting_human"), "{error}");
    }

    #[test]
    fn an_unknown_stage_in_extra_stages_is_refused() {
        let text = sample_task().to_text().replace(
            "extra_stages:\n  - mutation\n  - review",
            "extra_stages:\n  - integration",
        );
        let error = TaskFile::from_text(&text).expect_err("integration is not a stage");
        assert!(error.contains("integration"), "{error}");
    }

    #[test]
    fn slugs_and_ids_follow_the_rules() {
        assert!(valid_slug("a"));
        assert!(valid_slug("a-1"));
        assert!(valid_slug(&"a".repeat(48)));
        assert!(!valid_slug(""));
        assert!(!valid_slug(&"a".repeat(49)));
        assert!(!valid_slug("-a"));
        assert!(!valid_slug("a-"));
        assert!(!valid_slug("A"));
        assert!(!valid_slug("a_b"));
        assert!(valid_id("0001-a"));
        assert!(!valid_id("1-a"));
        assert!(!valid_id("0000-a"), "numbers start at 1");
        assert!(!valid_id("0001-"));
        assert!(!valid_id("../0001-a"));
        assert_eq!(format_id(1, "a"), "0001-a");
        assert_eq!(id_number("0042-a-b"), Some(42));
    }

    #[test]
    fn a_retry_time_must_be_absolute_utc() {
        assert!(valid_utc("2026-10-22T10:00:00Z"));
        // The shape is what is checked: digits where digits belong, and the two letters.
        for bad in [
            "2026-10-22",
            "2026-10-22T10:00:00",
            "2026-10-22T10:00:00+02:00",
            "2026-10-22 10:00:00Z",
            "2026-10-2aT10:00:00Z",
        ] {
            assert!(!valid_utc(bad), "{bad}");
        }
    }
}
