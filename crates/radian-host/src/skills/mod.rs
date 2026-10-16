//! Skill discovery and the system-prompt section (ADR 0025 S1/S2/S6).
//!
//! A skill is a directory containing a `SKILL.md`; the body is **not** loaded at startup, only
//! each skill's name, description and path go into one section of the system prompt. The body
//! is read on demand by the `skill` tool ([`crate::skills::tools`]). Discovery is strict about
//! determinism (ADR 0021 D6's reason): the same files always produce a byte-identical section,
//! so the prompt cache is not invalidated by a reshuffle.
//!
//! Failures never block startup (the `tui.*` / memory rule): a bad frontmatter file, a missing
//! description, an invalid name or a duplicate name is one warning, and the skill is skipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod tools;

use agent_core::{HookContext, HookError, Hooks, ModelRequest, RequestSize};

/// The one file name discovery recognizes (ADR 0025 S1, like pi).
pub const SKILL_FILE_NAME: &str = "SKILL.md";

/// The section's soft token budget (ADR 0025 S2 / owner ruling 6).
pub const SECTION_BUDGET_TOKENS: u64 = 1000;

/// Directories discovery never descends into (the same names `mention` ignores; ADR 0025 S1).
/// `.radian-worktrees/` holds one full worktree per task agent (ADR 0030 O3), so descending
/// into it would discover every skill once per task.
pub const IGNORED_DIRS: &[&str] = &[".git", ".radian-sandbox", ".radian-worktrees"];

/// The longest `name` (Agent Skills spec, via pi; ADR 0025 S1).
pub const MAX_NAME_CHARS: usize = 64;
/// The longest `description` (Agent Skills spec, via pi; ADR 0025 S1).
pub const MAX_DESCRIPTION_CHARS: usize = 1024;

/// Where a skill came from (ADR 0025). The order is the discovery order: a
/// user-level or `extra_paths` skill of the same name wins over a built-in one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    /// `<RADIAN_HOME>/skills/`.
    User,
    /// A `[skills] extra_paths` root.
    Extra,
    /// Compiled into the binary (`include_str!`); not on the filesystem.
    Builtin,
}

impl SkillSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillSource::User => "user",
            SkillSource::Extra => "extra_paths",
            SkillSource::Builtin => "builtin",
        }
    }
}

/// The built-in skills, compiled in (ADR 0025). `(name, SKILL.md text)`.
pub const BUILTIN: &[(&str, &str)] = &[
    ("diagrams", include_str!("../../skills/diagrams/SKILL.md")),
    (
        "theme-import",
        include_str!("../../skills/theme-import/SKILL.md"),
    ),
];

/// One discovered skill. `path` is the `SKILL.md` (absolute); `dir` is its directory, the base
/// for relative paths in the body (ADR 0025 S3). For a built-in skill `body` is the embedded
/// text and `path`/`dir` are synthetic (`<builtin>/<name>/…`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub dir: PathBuf,
    /// `disable-model-invocation: true`: the skill is loaded but never listed in the prompt.
    pub disable_model_invocation: bool,
    pub source: SkillSource,
    /// The body of a built-in skill, so the `skill` tool and `/skill:` do not read a file.
    pub body: Option<&'static str>,
}

/// The result of one scan: the skills in discovery (path) order, and the one-line warnings to
/// show at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    pub skills: Vec<Skill>,
    pub warnings: Vec<String>,
}

/// Scans one skills directory (`<RADIAN_HOME>/skills`). A missing directory is an empty
/// discovery, not an error (ADR 0025 S1: nothing blocks startup).
pub fn discover(root: &Path) -> Discovery {
    // One root, no built-ins: this is the "scan this directory" helper (tests and callers that
    // do not want the compiled-in skills). `open_session` uses `discover_with_extras`.
    discover_with_extras(Some(root), &[], false, &[])
}

/// Scans the user-level skills directory (`primary`, usually `<RADIAN_HOME>/skills`) **and** the
/// configured `[skills] extra_paths` roots, in that order (ADR 0025 slice 4 / ruling 2). The
/// user level is discovered first, so its skill wins a name conflict. An extra path that is not
/// a directory is one warning, never an error; an extra root that holds a `SKILL.md` directly is
/// itself one skill (the same rule as a subdirectory of the primary root).
/// The built-in skills are discovered **last** (ADR 0025): a user-level or
/// `extra_paths` skill of the same name wins. `builtin` is `[skills] builtin` (default true),
/// `disabled` is `[skills] disabled`.
pub fn discover_with_extras(
    primary: Option<&Path>,
    extras: &[PathBuf],
    builtin: bool,
    disabled: &[String],
) -> Discovery {
    let mut discovery = Discovery::default();
    // One name map across every root: the first discovery of a name wins, whichever root it came
    // from, and a later one is warned about.
    let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();
    if let Some(primary) = primary {
        collect_from(primary, SkillSource::User, &mut discovery, &mut by_name);
    }
    for extra in extras {
        if !extra.is_dir() {
            discovery.warnings.push(format!(
                "skills.extra_paths: {} is not a directory; skipped",
                extra.display()
            ));
            continue;
        }
        collect_from(extra, SkillSource::Extra, &mut discovery, &mut by_name);
    }
    if builtin {
        collect_builtin(&mut discovery, &mut by_name, disabled);
    }
    discovery
}

/// Collects the skills of one root into `discovery`, keeping the first name seen in `by_name`.
fn collect_from(
    root: &Path,
    source: SkillSource,
    discovery: &mut Discovery,
    by_name: &mut BTreeMap<String, PathBuf>,
) {
    if !root.is_dir() {
        return;
    }
    // Every directory that holds a SKILL.md, in path order (the deterministic order ADR 0025
    // S2 requires); a directory with SKILL.md is a skill and is not descended into.
    //
    // `walk` already yields `Path` order — its per-directory sort is the first component of
    // `Path`'s component-wise comparison, so the order is a depth-first walk with each
    // directory's entries in name order, which is exactly `Path` order. The explicit sort is
    // kept as the statement of the contract (and so a future `walk` change cannot quietly
    // break it); removing it is an equivalent mutation.
    let mut candidates: Vec<PathBuf> = Vec::new();
    walk(root, &mut candidates);
    candidates.sort();

    for path in candidates {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                discovery
                    .warnings
                    .push(format!("skill {} cannot be read: {error}", path.display()));
                continue;
            }
        };
        match load_text(&path, &text, source, None) {
            Ok((skill, extra)) => {
                discovery.warnings.extend(extra);
                if let Some(first) = by_name.get(&skill.name) {
                    // ADR 0025 S1: keep the first discovered (the earlier root, then the earlier
                    // path) and warn once.
                    discovery.warnings.push(format!(
                        "skill `{}` in {} shadows {} (kept the first)",
                        skill.name,
                        skill.dir.display(),
                        first.display()
                    ));
                    continue;
                }
                by_name.insert(skill.name.clone(), skill.dir.clone());
                discovery.skills.push(skill);
            }
            Err(reason) => discovery.warnings.push(reason),
        }
    }
}

/// Whether `root` holds at least one `SKILL.md` under the discovery rules, **without parsing
/// it**. Project trust uses this as its trigger (ADR 0031 T1): "the workspace provides
/// something" is a file question, and a malformed skill is still something the loader reads.
pub fn has_skill_file(root: &Path) -> bool {
    let mut candidates = Vec::new();
    walk(root, &mut candidates);
    !candidates.is_empty()
}

/// Collects every `SKILL.md` under `dir`, pruning at a directory that has one.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    children.sort();
    let skill_file = dir.join(SKILL_FILE_NAME);
    if skill_file.is_file() {
        out.push(skill_file);
        return;
    }
    for child in children {
        let Some(name) = child.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if IGNORED_DIRS.contains(&name) || !child.is_dir() {
            continue;
        }
        walk(&child, out);
    }
}

/// Collects the built-in skills (ADR 0025), last in the discovery order. A name in
/// `disabled` is skipped; a name already discovered is shadowed with the usual warning.
fn collect_builtin(
    discovery: &mut Discovery,
    by_name: &mut BTreeMap<String, PathBuf>,
    disabled: &[String],
) {
    for (name, text) in BUILTIN {
        if disabled.iter().any(|disabled| disabled == name) {
            continue;
        }
        let path = PathBuf::from(format!("<builtin>/{name}/{SKILL_FILE_NAME}"));
        match load_text(&path, text, SkillSource::Builtin, Some(text)) {
            Ok((skill, extra)) => {
                discovery.warnings.extend(extra);
                if let Some(first) = by_name.get(&skill.name) {
                    discovery.warnings.push(format!(
                        "skill `{}` in {} shadows {} (kept the first)",
                        skill.name,
                        skill.dir.display(),
                        first.display()
                    ));
                    continue;
                }
                by_name.insert(skill.name.clone(), skill.dir.clone());
                discovery.skills.push(skill);
            }
            Err(reason) => discovery.warnings.push(reason),
        }
    }
}

/// Validates one `SKILL.md`'s text. `Err` is the one-line warning; the skill is skipped.
fn load_text(
    path: &Path,
    text: &str,
    source: SkillSource,
    body: Option<&'static str>,
) -> Result<(Skill, Vec<String>), String> {
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let front = parse_frontmatter(text)
        .map_err(|reason| format!("skill {} has bad frontmatter: {reason}", path.display()))?;
    let name = front
        .get("name")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if !valid_name(&name) {
        return Err(format!(
            "skill {} has an invalid name {name:?} (1–{MAX_NAME_CHARS} of letters, digits, `-`, `_`)",
            path.display()
        ));
    }
    let description = front
        .get("description")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if description.is_empty() {
        // ADR 0025 S1: a skill without a description is not loaded.
        return Err(format!(
            "skill `{name}` in {} has no description; not loaded",
            dir.display()
        ));
    }
    if !valid_description(&description) {
        return Err(format!(
            "skill `{name}` in {} has an invalid description (1–{MAX_DESCRIPTION_CHARS} characters, \
             no control characters)",
            dir.display()
        ));
    }
    let mut warnings = Vec::new();
    if front.contains_key("allowed-tools") {
        // Owner ruling 4: `allowed-tools` is never wired to permissions (that would let a skill
        // grant itself tools); it is read as text and ignored with one warning.
        warnings.push(format!(
            "skill `{name}` declares `allowed-tools`, which Radian does not implement; it has \
             no effect"
        ));
    }
    let disable_model_invocation = front
        .get("disable-model-invocation")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    Ok((
        Skill {
            name,
            description,
            path: path.to_path_buf(),
            dir,
            disable_model_invocation,
            source,
            body,
        },
        warnings,
    ))
}

/// The subset of YAML frontmatter Radian needs: top-level `key: value` scalars, with quoted
/// scalars, empty values, `>`/`|` block scalars and indented continuation lines; a nested map
/// or a list (such as `metadata:` or `allowed-tools:`) is not a value, so its keys never become
/// top-level keys. Blank lines and `#` comments are not keys (S1: before this, one blank line
/// made the whole skill fail to load). Unknown keys are ignored (a Claude Code skill's private
/// fields must not spam warnings). No YAML dependency: the keys we read are a few scalars, and
/// a real parser would be a large dependency for them (report the choice).
fn parse_frontmatter(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut lines = text.lines();
    let Some(first) = lines.next() else {
        return Err("the file is empty".into());
    };
    if first.trim_end() != "---" {
        return Err("the file does not start with `---`".into());
    }
    let mut front: BTreeMap<String, String> = BTreeMap::new();
    // The key the current indented block belongs to, and how its value continues (S2).
    let mut current: Option<String> = None;
    let mut style = BlockStyle::Plain;
    let mut closed = false;
    for line in lines {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        let trimmed = line.trim();
        // S1: a blank line is not a `key: value` pair (and ends the current block).
        if trimmed.is_empty() {
            current = None;
            style = BlockStyle::Plain;
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        // S1: a `#` comment is not a key. Inside a block scalar (`>`/`|`) a leading `#` is
        // content, not a comment.
        if trimmed.starts_with('#') && !(indented && style.is_block()) {
            continue;
        }
        if indented {
            // S2: an indented line belongs to the previous key. A list item (`- x`) or a nested
            // `key: value` is a nested structure, not the value of that key, so it is ignored;
            // otherwise the line continues a block scalar or a plain multi-line scalar.
            let list_item = trimmed == "-" || trimmed.starts_with("- ");
            let nested_key = trimmed.contains(':');
            let Some(key) = current.clone() else { continue };
            let value = front.entry(key).or_default();
            match style {
                BlockStyle::Literal => {
                    if !value.is_empty() {
                        value.push('\n');
                    }
                    value.push_str(trimmed);
                }
                BlockStyle::Fold => {
                    if !value.is_empty() {
                        value.push(' ');
                    }
                    value.push_str(trimmed);
                }
                // A plain scalar continues only when the key already has a value; a bare
                // `key:` followed by indented `key: value` lines is a nested map, not a value.
                BlockStyle::Plain if !value.is_empty() && !list_item && !nested_key => {
                    value.push(' ');
                    value.push_str(trimmed);
                }
                BlockStyle::Plain => {}
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!("`{line}` is not `key: value`"));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("`{line}` has an empty key"));
        }
        let value = value.trim();
        current = Some(key.to_owned());
        // A `>`/`|` marker (with an optional chomping indicator) starts a block scalar; its
        // indented lines are the value. Anything else is a scalar on this line.
        style = match value {
            ">" | ">-" | ">+" => BlockStyle::Fold,
            "|" | "|-" | "|+" => BlockStyle::Literal,
            _ => BlockStyle::Plain,
        };
        if style.is_block() {
            front.insert(key.to_owned(), String::new());
        } else {
            front.insert(key.to_owned(), unquote(value));
        }
    }
    if !closed {
        return Err("the frontmatter is not closed with `---`".into());
    }
    Ok(front)
}

/// How an indented line continues the previous key (the subset of YAML block styles Radian
/// reads; S2). A nested map or a list is not a value, so it is `Plain` with nothing to append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockStyle {
    /// A scalar on the key's own line, possibly continued by indented lines (folded).
    Plain,
    /// `>`, folded block scalar.
    Fold,
    /// `|`, literal block scalar.
    Literal,
}

impl BlockStyle {
    fn is_block(self) -> bool {
        matches!(self, BlockStyle::Fold | BlockStyle::Literal)
    }
}

/// Strips the YAML quoting Radian supports: matching single or double quotes.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            // A double-quoted YAML scalar could carry escapes; the skills we read do not, and
            // an unescaped inner quote would be a malformed file. Keep the inner text as-is.
            return value[1..value.len() - 1].to_owned();
        }
    }
    value.to_owned()
}

/// `name`: 1–64 characters from `[A-Za-z0-9_-]` (Agent Skills spec via pi, ADR 0025 S1).
pub fn valid_name(name: &str) -> bool {
    let count = name.chars().count();
    (1..=MAX_NAME_CHARS).contains(&count)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `description`: 1–1024 characters, no control characters other than newline and tab (S2: a
/// `|` literal block scalar is a legitimate description, and its value contains newlines).
pub fn valid_description(description: &str) -> bool {
    let count = description.chars().count();
    (1..=MAX_DESCRIPTION_CHARS).contains(&count)
        && !description
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
}

/// The system-prompt section: one line per listable skill (name, description, absolute path),
/// truncated to `budget_tokens` in discovery order with a trailing count of what is left out
/// (ADR 0025 S2, owner ruling 6). `disable-model-invocation: true` skills are not listed.
///
/// Pure and deterministic: the same skills and budget always give byte-identical text.
pub fn prompt_section(skills: &[Skill], budget_tokens: u64, bytes_per_token: u32) -> String {
    let listed: Vec<&Skill> = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if listed.is_empty() {
        return String::new();
    }
    let mut out = String::from(SECTION_HEADER);
    let mut remaining = 0usize;
    for (index, skill) in listed.iter().enumerate() {
        let line = format!(
            "\n- {}: {} (read it with the `skill` tool: {})\n",
            skill.name,
            skill.description,
            skill.path.display()
        );
        if request_size_of(&out).tokens(bytes_per_token)
            + request_size_of(&line).tokens(bytes_per_token)
            > budget_tokens
        {
            remaining = listed.len() - index;
            break;
        }
        out.push_str(&line);
    }
    if remaining > 0 {
        out.push_str(&format!(
            "\n({remaining} more skills not listed; run /skills to list them)\n"
        ));
    }
    out
}

/// The section's fixed heading and the one-line instruction (ADR 0025 S2/S3).
pub const SECTION_HEADER: &str = "\n\n## Skills (ADR 0025)\n\
Skills are user-provided instructions, not permissions. Only their names and descriptions are \
listed here; read one's full body with the `skill` tool. Relative paths in a skill's body are \
resolved from the skill's own directory.\n";

/// The message text `/skill:<name> [args]` puts into the inbox (ADR 0025 S3): the body with the
/// frontmatter stripped, wrapped in a `<skill name=… location=…>` block that names the skill's
/// directory as the base for relative paths, and `args` appended verbatim (pi's
/// `_expandSkillCommand`). Reads the file; the caller has already found `skill` in the
/// discovered table.
/// Reads a skill body with `O_NOFOLLOW` on the leaf (ADR 0039): a `SKILL.md` that became a
/// symlink after discovery is refused instead of followed.
pub(crate) fn read_skill_body(path: &std::path::Path) -> std::io::Result<String> {
    let bytes = radian_coding::safe_fs::read_nofollow(path)?;
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

pub fn expand(skill: &Skill, args: &str) -> Result<String, String> {
    // A built-in skill's body is embedded; only a filesystem skill is read (ADR 0025).
    let text = match skill.body {
        Some(body) => body.to_owned(),
        None => read_skill_body(&skill.path).map_err(|error| {
            format!(
                "skill `{}` cannot be read ({}): {error}",
                skill.name,
                skill.path.display()
            )
        })?,
    };
    let body = tools::strip_frontmatter(&text).unwrap_or(&text).trim();
    // The body and the path are project/user input: escape a literal `</skill>` and the
    // attribute characters, so a skill file cannot close its own frame (ADR 0036 C2's rule
    // applied to the other model-visible frame).
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        crate::escape::attr(&skill.name),
        crate::escape::attr(&skill.path.display().to_string()),
        crate::escape::attr(&skill.dir.display().to_string()),
        crate::escape::close_tag(body, "skill")
    );
    Ok(if args.is_empty() {
        block
    } else {
        format!("{block}\n\n{args}")
    })
}

/// Counts a string the way `agent_core` does, without building a `ModelRequest` (the same
/// helper as `memory/injection.rs`; ADR 0015 C2's conservative estimate).
fn request_size_of(text: &str) -> RequestSize {
    let mut size = RequestSize::default();
    for ch in text.chars() {
        if ch.is_ascii() {
            size.ascii_bytes += 1;
        } else {
            size.non_ascii_chars += 1;
        }
    }
    size
}

/// The `Hooks` implementation (ADR 0025 S2/S6): appends the precomputed section to the system
/// prompt on every request, after the memory section (the tuple order in `session.rs`). It never
/// fails a turn.
pub struct SkillsHooks {
    section: String,
}

impl SkillsHooks {
    pub fn new(skills: &[Skill]) -> Self {
        Self {
            section: prompt_section(
                skills,
                SECTION_BUDGET_TOKENS,
                agent_core::DEFAULT_BYTES_PER_TOKEN,
            ),
        }
    }

    /// The section this hook appends (empty when there is nothing to list).
    pub fn section(&self) -> &str {
        &self.section
    }
}

impl Hooks for SkillsHooks {
    async fn before_model_call(
        &self,
        _cx: &HookContext,
        request: &mut ModelRequest,
    ) -> Result<(), HookError> {
        if self.section.is_empty() {
            return Ok(());
        }
        match &mut request.system_prompt {
            Some(system) => system.push_str(&self.section),
            None => request.system_prompt = Some(self.section.clone()),
        }
        Ok(())
    }
}

/// The `/skill:` completion candidates (ADR 0025 S3, ruling 7): every discovered skill as
/// `(name, description)`. `disable-model-invocation: true` skills **are** included — the flag
/// hides a skill from the *model* (the prompt and the `skill` tool), not from the user, who can
/// still invoke it with `/skill:<name>` (pi registers every loaded skill as a `skill:<name>`
/// command). A description with newlines (a `|` literal block) is folded to one line.
pub fn completion_candidates(skills: &[Skill]) -> Vec<(String, String)> {
    skills
        .iter()
        .map(|skill| (skill.name.clone(), fold(&skill.description)))
        .collect()
}

/// The `/skills` notice lines: `name: description`, with a marker for the skills the model
/// cannot auto-invoke (they are still user-invocable via `/skill:`).
pub fn notices(skills: &[Skill]) -> Vec<String> {
    if skills.is_empty() {
        return vec!["no skills found under ~/.radian/skills".to_owned()];
    }
    skills
        .iter()
        .map(|skill| {
            let disabled = if skill.disable_model_invocation {
                " (model-invocation off)"
            } else {
                ""
            };
            // ADR 0025: a built-in skill's source is shown.
            let source = if skill.source == SkillSource::Builtin {
                " [builtin]"
            } else {
                ""
            };
            format!(
                "{}: {}{source}{disabled}",
                skill.name,
                fold(&skill.description)
            )
        })
        .collect()
}

/// One line: a `|` literal-block description can contain newlines.
fn fold(description: &str) -> String {
    description.replace('\n', " ")
}

/// A shared, immutable skill list (`Arc` so the hook and the tool share one discovery).
pub type SharedSkills = Arc<Vec<Skill>>;

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::CancellationToken;

    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let root = std::env::temp_dir()
                .join(format!("radian-skills-{label}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        /// Writes `<root>/skills/<dir>/SKILL.md` with `body`.
        fn skill(&self, dir: &str, body: &str) {
            let path = self.root.join("skills").join(dir);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join(SKILL_FILE_NAME), body).unwrap();
        }

        fn skills_dir(&self) -> PathBuf {
            self.root.join("skills")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn frontmatter(name: &str, description: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\nBody line.\n")
    }

    #[test]
    fn a_normal_skill_is_discovered_with_its_paths() {
        let scratch = Scratch::new("normal");
        scratch.skill("greet", &frontmatter("greet", "Say hello."));
        let discovery = discover(&scratch.skills_dir());
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        assert_eq!(discovery.skills.len(), 1);
        let skill = &discovery.skills[0];
        assert_eq!(skill.name, "greet");
        assert_eq!(skill.description, "Say hello.");
        assert!(skill.path.ends_with("skills/greet/SKILL.md"));
        assert!(skill.dir.ends_with("skills/greet"));
        assert!(!skill.disable_model_invocation);
    }

    #[test]
    fn a_missing_description_is_a_warning_and_the_skill_is_skipped() {
        let scratch = Scratch::new("no-description");
        scratch.skill("a", "---\nname: a\n---\nBody.\n");
        let discovery = discover(&scratch.skills_dir());
        assert!(discovery.skills.is_empty());
        assert_eq!(discovery.warnings.len(), 1);
        assert!(
            discovery.warnings[0].contains("no description"),
            "{:?}",
            discovery.warnings
        );
    }

    #[test]
    fn bad_frontmatter_and_invalid_names_are_warnings() {
        let scratch = Scratch::new("bad");
        scratch.skill("unterminated", "---\nname: x\ndescription: y\nBody.\n");
        scratch.skill("not-a-pair", "---\nname x\ndescription: y\n---\n");
        scratch.skill("bad-name", &frontmatter("has space", "Bad name."));
        let discovery = discover(&scratch.skills_dir());
        assert!(discovery.skills.is_empty());
        assert_eq!(discovery.warnings.len(), 3, "{:?}", discovery.warnings);
        assert!(
            discovery
                .warnings
                .iter()
                .any(|w| w.contains("bad frontmatter"))
        );
        assert!(
            discovery
                .warnings
                .iter()
                .any(|w| w.contains("invalid name"))
        );
    }

    #[test]
    fn a_name_collision_keeps_the_first_and_warns() {
        let scratch = Scratch::new("collision");
        // `aaa` sorts before `zzz`, so the first found (sorted by path) wins.
        scratch.skill("aaa", &frontmatter("same", "First."));
        scratch.skill("zzz", &frontmatter("same", "Second."));
        let discovery = discover(&scratch.skills_dir());
        assert_eq!(discovery.skills.len(), 1);
        assert_eq!(discovery.skills[0].description, "First.");
        assert_eq!(discovery.warnings.len(), 1);
        assert!(
            discovery.warnings[0].contains("shadows"),
            "{:?}",
            discovery.warnings
        );
    }

    #[test]
    fn allowed_tools_is_read_and_warned_once_per_skill() {
        let scratch = Scratch::new("allowed-tools");
        scratch.skill(
            "a",
            "---\nname: a\ndescription: d\nallowed-tools:\n  - Bash\n  - Read\n---\nBody.\n",
        );
        let discovery = discover(&scratch.skills_dir());
        assert_eq!(discovery.skills.len(), 1);
        assert_eq!(discovery.warnings.len(), 1);
        assert!(
            discovery.warnings[0].contains("allowed-tools"),
            "{:?}",
            discovery.warnings
        );
    }

    #[test]
    fn discovery_stops_at_a_directory_that_has_a_skill_file() {
        let scratch = Scratch::new("prune");
        scratch.skill("outer", &frontmatter("outer", "Outer."));
        // A nested skill below `outer/` must not be discovered (the walk stops at `outer`).
        let nested = scratch.skills_dir().join("outer/nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(SKILL_FILE_NAME), frontmatter("inner", "Inner.")).unwrap();
        // `.git` is ignored.
        let ignored = scratch.skills_dir().join(".git/hidden");
        std::fs::create_dir_all(&ignored).unwrap();
        std::fs::write(
            ignored.join(SKILL_FILE_NAME),
            frontmatter("hidden", "Hidden."),
        )
        .unwrap();

        let discovery = discover(&scratch.skills_dir());
        let names: Vec<&str> = discovery.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["outer"], "{:?}", discovery.warnings);
    }

    /// ADR 0025 slice 4: the user level is discovered first, so its skill wins a name conflict
    /// with an extra root; the extra root's own skills are still added.
    #[test]
    fn the_user_level_wins_a_conflict_with_an_extra_path() {
        let scratch = Scratch::new("extras-conflict");
        scratch.skill("same", &frontmatter("same", "From the user level."));
        let extra = scratch.root.join("team-skills");
        std::fs::create_dir_all(extra.join("same")).unwrap();
        std::fs::write(
            extra.join("same/SKILL.md"),
            frontmatter("same", "From the team."),
        )
        .unwrap();
        std::fs::create_dir_all(extra.join("team-only")).unwrap();
        std::fs::write(
            extra.join("team-only/SKILL.md"),
            frontmatter("team-only", "Only in the team root."),
        )
        .unwrap();

        let discovery = discover_with_extras(Some(&scratch.skills_dir()), &[extra], false, &[]);
        let names: Vec<&str> = discovery.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["same", "team-only"], "{:?}", discovery.warnings);
        assert_eq!(discovery.skills[0].description, "From the user level.");
        assert_eq!(discovery.warnings.len(), 1);
        assert!(
            discovery.warnings[0].contains("shadows"),
            "{:?}",
            discovery.warnings
        );
    }

    /// A configured extra path that is not a directory is one warning, never an error, and the
    /// other roots are still discovered.
    #[test]
    fn a_missing_extra_path_is_one_warning() {
        let scratch = Scratch::new("extras-missing");
        scratch.skill("a", &frontmatter("a", "A."));
        let missing = scratch.root.join("does-not-exist");
        let discovery = discover_with_extras(
            Some(&scratch.skills_dir()),
            std::slice::from_ref(&missing),
            false,
            &[],
        );
        assert_eq!(discovery.skills.len(), 1);
        assert_eq!(discovery.warnings.len(), 1);
        assert!(
            discovery.warnings[0].contains("extra_paths")
                && discovery.warnings[0].contains("skipped"),
            "{:?}",
            discovery.warnings
        );
        assert!(discovery.warnings[0].contains(missing.to_str().unwrap()));
    }

    /// Ruling 2: a root that is not listed is never read. `~/.claude/skills` is just another
    /// directory here — the point is that discovery does not reach it until it is an extra path.
    #[test]
    fn an_unlisted_root_is_not_read_until_it_is_an_extra_path() {
        let scratch = Scratch::new("extras-unlisted");
        scratch.skill("own", &frontmatter("own", "Own."));
        let claude = scratch.root.join("claude-skills");
        std::fs::create_dir_all(claude.join("borrowed")).unwrap();
        std::fs::write(
            claude.join("borrowed/SKILL.md"),
            frontmatter("borrowed", "From the other tool."),
        )
        .unwrap();

        // Not listed: not read.
        let discovery = discover_with_extras(Some(&scratch.skills_dir()), &[], false, &[]);
        let names: Vec<&str> = discovery.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["own"], "{:?}", discovery.warnings);

        // Listed: read, with the same parse and validation rules.
        let discovery = discover_with_extras(Some(&scratch.skills_dir()), &[claude], false, &[]);
        let names: Vec<&str> = discovery.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["own", "borrowed"], "{:?}", discovery.warnings);
    }

    /// An extra path that is itself a skill directory (it holds a `SKILL.md`) loads as one skill,
    /// the same rule the primary root's subdirectories follow.
    #[test]
    fn an_extra_path_may_be_a_single_skill_directory() {
        let scratch = Scratch::new("extras-single");
        let single = scratch.root.join("one-skill");
        std::fs::create_dir_all(&single).unwrap();
        std::fs::write(single.join(SKILL_FILE_NAME), frontmatter("solo", "Solo.")).unwrap();
        let discovery = discover_with_extras(Some(&scratch.skills_dir()), &[single], false, &[]);
        let names: Vec<&str> = discovery.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["solo"], "{:?}", discovery.warnings);
    }

    /// ADR 0025: the compiled-in `diagrams` skill is discovered, marked `builtin`,
    /// and listed by `/skills`.
    #[test]
    fn the_builtin_diagrams_skill_is_discovered_and_listed() {
        let discovery = discover_with_extras(None, &[], true, &[]);
        let skill = discovery
            .skills
            .iter()
            .find(|skill| skill.name == "diagrams")
            .expect("the built-in diagrams skill");
        assert_eq!(skill.source, SkillSource::Builtin);
        assert!(skill.body.is_some(), "the body is embedded");
        assert!(!skill.description.is_empty());
        let notices = notices(&discovery.skills);
        assert!(
            notices
                .iter()
                .any(|line| line.contains("diagrams") && line.contains("[builtin]")),
            "{notices:?}"
        );
        // It is a completion candidate too.
        assert!(
            completion_candidates(&discovery.skills)
                .iter()
                .any(|(name, _)| name == "diagrams")
        );
    }

    /// A user-level skill of the same name wins; the built-in is shadowed with the usual
    /// warning.
    #[test]
    fn a_user_skill_of_the_same_name_overrides_the_builtin() {
        let scratch = Scratch::new("builtin-override");
        scratch.skill("diagrams", &frontmatter("diagrams", "My own."));
        let discovery = discover_with_extras(Some(&scratch.skills_dir()), &[], true, &[]);
        let diagrams: Vec<&Skill> = discovery
            .skills
            .iter()
            .filter(|skill| skill.name == "diagrams")
            .collect();
        assert_eq!(diagrams.len(), 1, "only one wins");
        assert_eq!(diagrams[0].source, SkillSource::User);
        assert_eq!(diagrams[0].description, "My own.");
        assert!(
            discovery.warnings.iter().any(|w| w.contains("shadows")),
            "{:?}",
            discovery.warnings
        );
    }

    /// `[skills] builtin = false` turns every built-in off; `disabled` turns one off by name.
    #[test]
    fn builtin_false_and_disabled_turn_the_builtin_off() {
        assert!(
            discover_with_extras(None, &[], false, &[])
                .skills
                .is_empty()
        );
        assert!(
            discover_with_extras(
                None,
                &[],
                true,
                &["diagrams".to_owned(), "theme-import".to_owned()]
            )
            .skills
            .is_empty()
        );
    }

    /// ADR 0029: the compiled-in `theme-import` skill is discovered,
    /// marked `builtin`, and listed by `/skills`.
    #[test]
    fn the_builtin_theme_import_skill_is_discovered_and_listed() {
        let discovery = discover_with_extras(None, &[], true, &[]);
        let skill = discovery
            .skills
            .iter()
            .find(|skill| skill.name == "theme-import")
            .expect("the built-in theme-import skill");
        assert_eq!(skill.source, SkillSource::Builtin);
        assert!(skill.body.is_some(), "the body is embedded");
        assert!(
            skill.description.contains("pi theme"),
            "{}",
            skill.description
        );
        assert!(
            notices(&discovery.skills)
                .iter()
                .any(|line| line.contains("theme-import") && line.contains("[builtin]")),
            "{:?}",
            notices(&discovery.skills)
        );
    }

    #[test]
    fn a_missing_skills_directory_is_empty_and_silent() {
        let scratch = Scratch::new("missing");
        let discovery = discover(&scratch.skills_dir());
        assert_eq!(discovery, Discovery::default());
        assert_eq!(prompt_section(&[], SECTION_BUDGET_TOKENS, 4), "");
    }

    #[test]
    fn disable_model_invocation_skills_are_not_listed() {
        let scratch = Scratch::new("disabled");
        scratch.skill("a", &frontmatter("a", "Listed."));
        scratch.skill(
            "b",
            "---\nname: b\ndescription: Hidden.\ndisable-model-invocation: true\n---\nBody.\n",
        );
        let discovery = discover(&scratch.skills_dir());
        assert_eq!(discovery.skills.len(), 2);
        assert!(discovery.skills[1].disable_model_invocation);
        let section = prompt_section(&discovery.skills, SECTION_BUDGET_TOKENS, 4);
        assert!(section.contains("a: Listed."));
        assert!(!section.contains("Hidden."), "{section}");
    }

    #[test]
    fn the_section_is_byte_identical_for_the_same_input() {
        let scratch = Scratch::new("deterministic");
        scratch.skill("a", &frontmatter("a", "One."));
        scratch.skill("b", &frontmatter("b", "Two."));
        scratch.skill("c", &frontmatter("c", "Three."));
        let first = discover(&scratch.skills_dir());
        let second = discover(&scratch.skills_dir());
        assert_eq!(first.skills, second.skills);
        let left = prompt_section(&first.skills, SECTION_BUDGET_TOKENS, 4);
        let right = prompt_section(&second.skills, SECTION_BUDGET_TOKENS, 4);
        assert_eq!(left.as_bytes(), right.as_bytes());
        // The order is the discovery (path) order.
        let a = left.find("- a:").unwrap();
        let b = left.find("- b:").unwrap();
        let c = left.find("- c:").unwrap();
        assert!(a < b && b < c, "{left}");
    }

    #[test]
    fn over_budget_entries_are_dropped_in_order_with_a_remaining_count() {
        let skills = vec![
            Skill {
                name: "one".into(),
                description: "d1".into(),
                path: PathBuf::from("/s/one/SKILL.md"),
                dir: PathBuf::from("/s/one"),
                disable_model_invocation: false,
                source: SkillSource::User,
                body: None,
            },
            Skill {
                name: "two".into(),
                description: "d2".into(),
                path: PathBuf::from("/s/two/SKILL.md"),
                dir: PathBuf::from("/s/two"),
                disable_model_invocation: false,
                source: SkillSource::User,
                body: None,
            },
            Skill {
                name: "three".into(),
                description: "d3".into(),
                path: PathBuf::from("/s/three/SKILL.md"),
                dir: PathBuf::from("/s/three"),
                disable_model_invocation: false,
                source: SkillSource::User,
                body: None,
            },
        ];
        // The header is long, so the budget is derived from the rendered text rather than
        // guessed: the tokens of the prefix that ends exactly before the second entry fit one
        // entry, not two.
        let full = prompt_section(&skills, u64::MAX, 4);
        let before_second = full.find("- two:").unwrap();
        let budget = request_size_of(&full[..before_second]).tokens(4);
        let section = prompt_section(&skills, budget, 4);
        assert!(section.contains("- one:"), "{section}");
        assert!(!section.contains("- two:"), "{section}");
        assert!(section.contains("2 more skills not listed"), "{section}");

        // The header alone fills the budget: nothing is listed, the remainder is counted.
        let header_only = request_size_of(SECTION_HEADER).tokens(4);
        let section = prompt_section(&skills, header_only, 4);
        assert!(!section.contains("- one:"), "{section}");
        assert!(section.contains("3 more skills not listed"), "{section}");
    }

    /// S1: a blank line or a `#` comment in the frontmatter is not a `key: value` pair. Before
    /// this, one blank line made the whole skill fail to load (real Claude Code skills have
    /// both).
    /// S2: an indented key under a nested map (`metadata:`) must not become a top-level key. The
    /// nested line here comes *after* the real `name`, which is the order the old parser got
    /// wrong (it overwrote the real name with the nested one).
    #[test]
    fn an_indented_nested_name_does_not_override_the_top_level_name() {
        let scratch = Scratch::new("nested-name");
        scratch.skill(
            "a",
            "---\nname: outer\ndescription: d\nmetadata:\n  name: inner\n  version: 1\n---\nBody.\n",
        );
        let discovery = discover(&scratch.skills_dir());
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        assert_eq!(discovery.skills.len(), 1);
        assert_eq!(discovery.skills[0].name, "outer");
    }

    /// S2: `>` folds its indented lines into one space-joined scalar.
    #[test]
    fn a_folded_block_scalar_is_joined_with_spaces() {
        let scratches = Scratch::new("folded");
        scratches.skill(
            "a",
            "---\nname: a\ndescription: >\n  Folded line one.\n  Folded line two.\n---\nBody.\n",
        );
        let discovery = discover(&scratches.skills_dir());
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        assert_eq!(
            discovery.skills[0].description,
            "Folded line one. Folded line two."
        );
    }

    /// S2: `|` keeps its indented lines' newlines.
    #[test]
    fn a_literal_block_scalar_keeps_newlines() {
        let front =
            parse_frontmatter("---\nname: a\ndescription: |\n  line one\n  line two\n---\n")
                .unwrap();
        assert_eq!(front.get("description").unwrap(), "line one\nline two");
        // And such a skill still loads: a literal-block description is valid.
        let scratch = Scratch::new("literal");
        scratch.skill(
            "a",
            "---\nname: a\ndescription: |\n  line one\n  line two\n---\nBody.\n",
        );
        let discovery = discover(&scratch.skills_dir());
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        assert_eq!(discovery.skills[0].description, "line one\nline two");
    }

    /// S2: a plain scalar continued on indented lines folds with spaces.
    #[test]
    fn a_plain_scalar_continued_on_an_indented_line_is_folded() {
        let front = parse_frontmatter("---\nname: a\ndescription: first\n  second\n---\n").unwrap();
        assert_eq!(front.get("description").unwrap(), "first second");
    }

    #[test]
    fn blank_lines_and_comments_do_not_reject_a_skill() {
        let scratch = Scratch::new("blank-lines");
        scratch.skill(
            "a",
            "---\nname: a\n# a comment\n\ndescription: Has a blank line and a comment.\n\n---\nBody.\n",
        );
        let discovery = discover(&scratch.skills_dir());
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        assert_eq!(discovery.skills.len(), 1);
        assert_eq!(
            discovery.skills[0].description,
            "Has a blank line and a comment."
        );
    }

    #[test]
    fn quotes_and_extra_fields_are_tolerated() {
        let scratch = Scratch::new("quotes");
        scratch.skill(
            "a",
            "---\nname: \"a\"\ndescription: 'Quoted description.'\npreamble-tier: full\n---\n",
        );
        let discovery = discover(&scratch.skills_dir());
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        assert_eq!(discovery.skills[0].name, "a");
        assert_eq!(discovery.skills[0].description, "Quoted description.");
    }

    /// ADR 0025 S3 / ruling 7: the `/skill:` completion candidates and the `/skills` notices
    /// include `disable-model-invocation: true` skills — the flag hides them from the model, not
    /// from the user (pi registers every loaded skill as a `skill:<name>` command).
    #[test]
    fn completion_and_notices_include_model_invocation_disabled_skills() {
        let skills = vec![
            Skill {
                name: "greet".into(),
                description: "Say hello.".into(),
                path: PathBuf::from("/s/greet/SKILL.md"),
                dir: PathBuf::from("/s/greet"),
                disable_model_invocation: false,
                source: SkillSource::User,
                body: None,
            },
            Skill {
                name: "secret".into(),
                description: "Only the user.".into(),
                path: PathBuf::from("/s/secret/SKILL.md"),
                dir: PathBuf::from("/s/secret"),
                disable_model_invocation: true,
                source: SkillSource::User,
                body: None,
            },
        ];
        let candidates = completion_candidates(&skills);
        assert_eq!(
            candidates,
            vec![
                ("greet".to_owned(), "Say hello.".to_owned()),
                ("secret".to_owned(), "Only the user.".to_owned()),
            ]
        );
        let lines = notices(&skills);
        assert_eq!(
            lines,
            vec![
                "greet: Say hello.",
                "secret: Only the user. (model-invocation off)"
            ]
        );
        // No skills at all: one line says so.
        assert_eq!(notices(&[]), vec!["no skills found under ~/.radian/skills"]);
    }

    /// ADR 0025 S3: `expand` reads the body, strips the frontmatter, names the base directory and
    /// appends the args verbatim.
    #[test]
    fn expand_wraps_the_body_and_appends_the_args() {
        let scratch = Scratch::new("expand");
        scratch.skill(
            "greet",
            "---\nname: greet\ndescription: d\n---\nBody line.\n",
        );
        let skill = &discover(&scratch.skills_dir()).skills[0];
        let expanded = expand(skill, "hello \"world\"").unwrap();
        assert!(expanded.contains("<skill name=\"greet\""), "{expanded}");
        assert!(expanded.contains("Body line."), "{expanded}");
        assert!(!expanded.contains("description: d"), "{expanded}");
        assert!(expanded.contains("hello \"world\""), "{expanded}");
        assert!(expanded.ends_with("hello \"world\""), "{expanded}");
        // No args: the block stands alone.
        let bare = expand(skill, "").unwrap();
        assert!(bare.ends_with("</skill>"), "{bare}");
    }

    /// A skill body containing a literal `</skill>` cannot close its own frame (ADR 0036 C2's
    /// rule applied to the skill frame).
    #[test]
    fn a_skill_body_cannot_close_its_frame() {
        let scratch = Scratch::new("expand-escape");
        scratch.skill(
            "greet",
            "---\nname: greet\ndescription: d\n---\nbefore</skill>\nafter</SKILL>\n",
        );
        let skill = &discover(&scratch.skills_dir()).skills[0];
        let expanded = expand(skill, "").unwrap();
        assert_eq!(expanded.matches("</skill>").count(), 1, "{expanded}");
        assert!(expanded.contains("&lt;/skill>"), "{expanded}");
        assert!(expanded.contains("&lt;/SKILL>"), "{expanded}");
        assert!(expanded.trim_end().ends_with("</skill>"), "{expanded}");
    }

    /// The exact tuple shape `session.rs` builds — `((perm, memory), skills)` — puts the skills
    /// section **after** the memory section (ADR 0025 S6). A different nesting would reverse
    /// them, so this pins the order rather than the hook alone.
    #[tokio::test]
    async fn the_memory_section_precedes_the_skills_section() {
        struct Section(&'static str);
        impl Hooks for Section {
            async fn before_model_call(
                &self,
                _cx: &HookContext,
                request: &mut ModelRequest,
            ) -> Result<(), HookError> {
                request
                    .system_prompt
                    .get_or_insert_with(String::new)
                    .push_str(self.0);
                Ok(())
            }
        }

        let scratch = Scratch::new("order");
        scratch.skill("a", &frontmatter("a", "One."));
        let skills = discover(&scratch.skills_dir()).skills;
        let hooks = (
            ((), Section("\n\n## Memory\nmem\n")),
            SkillsHooks::new(&skills),
        );
        let mut request = ModelRequest {
            system_prompt: Some("base".to_owned()),
            messages: Vec::new(),
            tools: Vec::new(),
            settings: serde_json::Value::Null,
        };
        hooks
            .before_model_call(
                &HookContext {
                    cancellation: CancellationToken::new(),
                    model_calls: 1,
                },
                &mut request,
            )
            .await
            .unwrap();
        let prompt = request.system_prompt.unwrap();
        let memory_at = prompt.find("## Memory").unwrap();
        let skills_at = prompt.find("## Skills").unwrap();
        assert!(memory_at < skills_at, "{prompt}");
        assert!(prompt.starts_with("base"), "{prompt}");
    }

    #[tokio::test]
    async fn the_hook_appends_the_section_to_the_system_prompt() {
        use agent_core::HookContext;
        let scratch = Scratch::new("hook");
        scratch.skill("a", &frontmatter("a", "One."));
        let skills = discover(&scratch.skills_dir()).skills;
        let hooks = SkillsHooks::new(&skills);
        assert!(hooks.section().contains("- a: One."));

        let mut request = ModelRequest {
            system_prompt: Some("base".to_owned()),
            messages: Vec::new(),
            tools: Vec::new(),
            settings: serde_json::Value::Null,
        };
        hooks
            .before_model_call(
                &HookContext {
                    cancellation: CancellationToken::new(),
                    model_calls: 1,
                },
                &mut request,
            )
            .await
            .unwrap();
        assert_eq!(
            request.system_prompt.as_deref(),
            Some(format!("base{}", hooks.section()).as_str())
        );

        // With no skills the hook touches nothing.
        let empty = SkillsHooks::new(&[]);
        let mut untouched = ModelRequest {
            system_prompt: None,
            messages: Vec::new(),
            tools: Vec::new(),
            settings: serde_json::Value::Null,
        };
        empty
            .before_model_call(
                &HookContext {
                    cancellation: CancellationToken::new(),
                    model_calls: 1,
                },
                &mut untouched,
            )
            .await
            .unwrap();
        assert_eq!(untouched.system_prompt, None);
    }
}
