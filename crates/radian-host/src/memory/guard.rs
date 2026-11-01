//! The memory write guards (ADR 0021 invariants 4 and 6): the input guard, the secret guard
//! and the user-record guard.
//!
//! All three are **host decisions**, not permission rules, and all three run before the file
//! tool is called, so a refused write touches no byte:
//!
//! - **Input guard**: once this session has taken in a result from an MCP tool, every `write`
//!   or `patch` whose canonical target is a memory scope directory is refused. The flag is
//!   per process; a resumed session starts with it set when its committed journal holds a
//!   settled call to a name the opened MCP session routes ([`journal_has_mcp_result`]). A name
//!   no server provides is never counted, and neither is a result the runtime produced without
//!   the tool returning anything (`cancelled`, `skipped_by_steering`, `hook_error`,
//!   `denied`, `interrupted`, or an error from a journal older than `error_code`).
//! - **Secret guard**: the content (for `patch`, the content the edit would produce and each
//!   inserted replacement) runs through the ADR 0008 §3.2 rule-2 heuristic in
//!   [`crate::config`]. A hit refuses the write and names the matched prefix, never the value.
//! - **User-record guard** (ADR 0021 invariants 3 and 6): a `write` or `patch` of a file whose
//!   frontmatter already declares `source: user` is refused, and so is one that would give a
//!   record that source; a call that declares a different source than the file already has is
//!   refused too. The model writes `model`; `user` belongs to the user commands, so a claimed
//!   user source can neither impersonate nor overwrite a user record.
//!
//! The scope directories are the same ones the file tools may reach (ADR 0039 decision 4);
//! the whole `memory/` tree is not covered, so this guard can never refuse a write the tools
//! could not make anyway.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use radian_core::{ToolCall, ToolFailure, ToolResult};
use radian_memory::declared_source;
use radian_session::{EventData, SessionEvent};
use serde_json::Value;

use super::service::MemoryService;

/// The input guard's refusal, word for word (ADR 0021 invariant 4).
pub const INPUT_GUARD: &str =
    "memory write refused: this session has taken in MCP tool results (input guard)";

/// The secret guard's refusal prefix; the matched prefix follows in parentheses.
pub const SECRET_GUARD: &str = "memory write refused: the content looks like a secret";

/// The refusal for a file-tool `write` or `patch` that would declare `source: user`
/// (ADR 0021 invariant 6): `user` belongs to the user commands, not to the model.
pub const USER_SOURCE: &str =
    "memory write refused: a model write may not claim source: user (invariant 6)";

/// The refusal for a file-tool `write` or `patch` of a file that already declares
/// `source: user` (ADR 0021 invariant 6).
pub const USER_RECORD: &str = "memory write refused: the record is user-created (invariant 6)";

/// The refusal for a file-tool `write` or `patch` that would declare a source other than the
/// one the file already has (ADR 0021 invariant 3): the write that created the file fixed it.
pub const SOURCE_CHANGE: &str =
    "memory write refused: a record's source is fixed by the write that created it (invariant 3)";

/// What the secret guard names for the 40-character half of the heuristic, which has no
/// prefix of its own.
pub const LONG_TOKEN: &str = "40+ characters";

/// The shared state of one session's memory write guards.
#[derive(Clone)]
pub struct MemoryWriteGuard {
    workspace: PathBuf,
    /// The canonical scope directories: `global`, then this workspace's project when linked.
    roots: Vec<PathBuf>,
    /// Set once this process has seen an MCP tool result (or a resume read one from the
    /// journal).
    polluted: Arc<AtomicBool>,
}

impl MemoryWriteGuard {
    /// Builds the guard from the same service that owns the file tools' memory roots.
    /// `polluted` is the resumed session's journal answer; a fresh session passes `false`.
    pub fn new(service: &MemoryService, polluted: bool) -> Self {
        let roots = service
            .file_scope_roots()
            .into_iter()
            .map(|(_, dir)| canonical_existing_prefix(&dir))
            .collect();
        Self {
            workspace: service.workspace().to_path_buf(),
            roots,
            polluted: Arc::new(AtomicBool::new(polluted)),
        }
    }

    /// Records that a call to a tool an MCP server provided settled. The input guard refuses
    /// memory writes from this point on; an error result counts too, because the session has
    /// still heard from an outside tool.
    pub fn note_mcp_result(&self) {
        self.polluted.store(true, Ordering::SeqCst);
    }

    /// The resume half of the input guard: arms when the committed journal holds a settled
    /// call to a name the opened MCP session routes and a result the tool returned
    /// ([`journal_has_mcp_result`]).
    pub fn adopt_resumed(&self, calls: &[SettledCall], routed: &HashSet<String>) {
        if journal_has_mcp_result(calls, routed) {
            self.note_mcp_result();
        }
    }

    /// Checks one call before the built-in tools see it. A write or patch outside a memory
    /// scope, and every other tool, pass through untouched.
    pub fn check(&self, call: &ToolCall) -> Result<(), ToolFailure> {
        if !matches!(call.name.as_str(), "write" | "patch") {
            return Ok(());
        }
        let Some(path) = call.arguments.get("path").and_then(Value::as_str) else {
            // Malformed arguments belong to the tool's own parser.
            return Ok(());
        };
        let target = self.resolve(path);
        if !self.is_memory(&target) {
            return Ok(());
        }
        if self.polluted.load(Ordering::SeqCst) {
            return Err(ToolFailure::new("memory_write_refused", INPUT_GUARD));
        }

        // ADR 0021 invariant 6: `source: user` is a user command's alone. A file that declares
        // it is a user record, and the model may neither overwrite nor patch it; a call that
        // would give a record that source is a claim the host refuses, so the model can never
        // make its own record look like a user record and inherit the protection.
        let existing_source = record_prefix(&target).and_then(|text| declared_source(&text));
        if existing_source.as_deref() == Some("user") {
            return Err(ToolFailure::new("memory_write_refused", USER_RECORD));
        }
        // ADR 0021 invariant 3: the write that creates the file fixes its source, so a model
        // `write` or `patch` must declare the source the file already has.
        let same_source = |declared: Option<&str>| match (&existing_source, declared) {
            (Some(existing), Some(declared)) if existing != declared => {
                Err(ToolFailure::new("memory_write_refused", SOURCE_CHANGE))
            }
            _ => Ok(()),
        };

        let refuse = |prefix: &'static str| {
            ToolFailure::new("memory_write_refused", format!("{SECRET_GUARD} ({prefix})"))
        };
        if call.name == "write" {
            if let Some(content) = call.arguments.get("content").and_then(Value::as_str) {
                let declared = declared_source(content);
                if declared.as_deref() == Some("user") {
                    return Err(ToolFailure::new("memory_write_refused", USER_SOURCE));
                }
                same_source(declared.as_deref())?;
                if let Some(prefix) = secret_hit(content) {
                    return Err(refuse(prefix));
                }
            }
            return Ok(());
        }
        // The result content is the rule (ADR 0021 invariant 4). When the tool's own locators
        // can model the result, that content is what gets checked; a patch that puts a
        // secret-shaped string inside an existing word is judged by the result, as the tool
        // would write it. Only when the result cannot be computed (the file does not exist,
        // the tool would refuse the edit, or the arguments are malformed) is each inserted
        // text checked on its own, so a secret cannot be written either way.
        match patch_result_content(&target, &call.arguments) {
            Some(result) => {
                let declared = declared_source(&result);
                if declared.as_deref() == Some("user") {
                    return Err(ToolFailure::new("memory_write_refused", USER_SOURCE));
                }
                same_source(declared.as_deref())?;
                if let Some(prefix) = secret_hit(&result) {
                    return Err(refuse(prefix));
                }
            }
            None => {
                for content in patch_new_texts(&call.arguments) {
                    if let Some(prefix) = secret_hit(&content) {
                        return Err(refuse(prefix));
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether a canonical target lies in one of the scope directories.
    fn is_memory(&self, target: &Path) -> bool {
        self.roots.iter().any(|root| target.starts_with(root))
    }

    /// The path as the file tool resolves it: `~` expanded, a relative path against the
    /// workspace, then canonicalized as far as it exists (a `write` target need not exist).
    fn resolve(&self, path: &str) -> PathBuf {
        let expanded = if path == "~" {
            crate::home::user_home().unwrap_or_default()
        } else if let Some(rest) = path.strip_prefix("~/") {
            crate::home::user_home()
                .map(|home| home.join(rest))
                .unwrap_or_else(|| PathBuf::from(path))
        } else {
            PathBuf::from(path)
        };
        let joined = if expanded.is_absolute() {
            expanded
        } else {
            self.workspace.join(expanded)
        };
        canonical_existing_prefix(&joined)
    }
}

/// One tool call a committed journal records as started and settled: the name its
/// `ToolExecutionStarted` marker carried and the result its `ToolExecutionSettled` carried.
#[derive(Debug, Clone)]
pub struct SettledCall {
    pub name: String,
    pub result: ToolResult,
}

/// The settled calls of a committed journal, in settle order. A result whose start marker is
/// missing (a recovered call) cannot name a tool, so it is skipped.
pub fn settled_calls(events: &[SessionEvent]) -> Vec<SettledCall> {
    let mut names: HashMap<&str, &str> = HashMap::new();
    let mut settled = Vec::new();
    for event in events {
        match &event.data {
            EventData::ToolExecutionStarted { call_id, name, .. } => {
                names.insert(call_id.as_str(), name.as_str());
            }
            EventData::ToolExecutionSettled { call_id, result } => {
                if let Some(name) = names.get(call_id.as_str()) {
                    settled.push(SettledCall {
                        name: (*name).to_owned(),
                        result: result.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    settled
}

/// Whether one of `calls` reached an MCP server (the resume half of the input guard).
///
/// `routed` is the opened MCP session's tool list — the same names the live guard checks — so
/// a name no server provides never counts as outside content, double underscore or not. The
/// result must also be one the call could only produce after it ran: `cancelled`,
/// `skipped_by_steering`, `hook_error`, `denied`, `interrupted` and an error with no code (a
/// journal written before `error_code` existed) are all generated without the tool returning
/// anything, so they do not count either.
pub fn journal_has_mcp_result(calls: &[SettledCall], routed: &HashSet<String>) -> bool {
    calls
        .iter()
        .any(|call| routed.contains(&call.name) && delivered(&call.result))
}

/// Whether a settled result carries content the tool returned, as opposed to a result the
/// runtime or the permission layer produced without the tool returning one.
fn delivered(result: &ToolResult) -> bool {
    if !result.is_error {
        return true;
    }
    !matches!(
        result.error_code.as_deref(),
        None | Some(
            "unknown_tool"
                | "cancelled"
                | "skipped_by_steering"
                | "hook_error"
                | "denied"
                | "interrupted"
        )
    )
}

/// The heuristic over a memory body: the whole text and every whitespace-separated token. A
/// token is the memory-content analog of a configuration value; scanning the whole text as
/// well keeps a bare secret at the start of the file caught exactly as before.
fn secret_hit(text: &str) -> Option<&'static str> {
    for candidate in std::iter::once(text).chain(text.split_whitespace()) {
        if let Some(prefix) = crate::config::secret_prefix(candidate) {
            return Some(prefix);
        }
    }
    if std::iter::once(text)
        .chain(text.split_whitespace())
        .any(crate::config::is_long_random)
    {
        return Some(LONG_TOKEN);
    }
    None
}

/// A bounded prefix of a possible record file: enough for its frontmatter. A non-regular
/// file is not read at all (a fifo would block), and a file too large to be a record is read
/// only up to this cap. `None` when the path is missing or cannot be read.
fn record_prefix(path: &Path) -> Option<String> {
    use std::io::Read;

    const FRONTMATTER_PREFIX: u64 = 8192;
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let mut buffer = Vec::with_capacity(FRONTMATTER_PREFIX as usize);
    std::fs::File::open(path)
        .ok()?
        .take(FRONTMATTER_PREFIX)
        .read_to_end(&mut buffer)
        .ok()?;
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

/// Every replacement string a `patch` call inserts, from the legacy `new` or the `edits`
/// array. Malformed arguments are left to the tool's parser.
fn patch_new_texts(arguments: &Value) -> Vec<String> {
    let mut texts = Vec::new();
    if let Some(new) = arguments.get("new").and_then(Value::as_str) {
        texts.push(new.to_owned());
    }
    if let Some(edits) = arguments.get("edits").and_then(Value::as_array) {
        for edit in edits {
            if let Some(new) = edit.get("new").and_then(Value::as_str) {
                texts.push(new.to_owned());
            }
        }
    }
    texts
}

/// The content a `patch` call would produce, computed with the tool's own locators (the LF
/// view, typographic normalisation, trailing whitespace) and its own rejections. `None` means
/// the tool itself would write nothing or the file cannot be read here (it does not exist, is
/// not a regular file, or is over the tool's byte cap), or the arguments do not form a patch;
/// the inserted text is checked on its own only in that case.
fn patch_result_content(path: &Path, arguments: &Value) -> Option<String> {
    // The patch tool refuses a non-regular file and a file over its byte cap; do not read
    // either here (a fifo would block, a huge file would allocate before the tool's own
    // check). The inserted text is still checked on its own in that case.
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file()
        || metadata.len() > radian_coding::Limits::default().max_patch_bytes as u64
    {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    // `edits` and the legacy pair are mutually exclusive, as in the tool's own parser.
    if arguments.get("edits").is_some()
        && (arguments.get("old").is_some()
            || arguments.get("new").is_some()
            || arguments.get("replace_all").is_some())
    {
        return None;
    }
    let mut edits: Vec<(String, String, bool)> = Vec::new();
    if let (Some(old), Some(new)) = (
        arguments.get("old").and_then(Value::as_str),
        arguments.get("new").and_then(Value::as_str),
    ) {
        edits.push((
            old.to_owned(),
            new.to_owned(),
            arguments
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ));
    }
    if let Some(array) = arguments.get("edits").and_then(Value::as_array) {
        if array.is_empty() {
            return None;
        }
        for edit in array {
            let (Some(old), Some(new)) = (
                edit.get("old").and_then(Value::as_str),
                edit.get("new").and_then(Value::as_str),
            ) else {
                return None;
            };
            edits.push((
                old.to_owned(),
                new.to_owned(),
                edit.get("replace_all")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ));
        }
    }
    if edits.is_empty() {
        return None;
    }
    // The real tool's locators: LF view, typographic normalisation, trailing whitespace. A
    // plain `contains` would miss the result a CRLF or normalised needle produces (ADR 0021
    // invariant 4's guard must judge the content the edit would actually write).
    radian_coding::patched_content(&text, &edits)
}

/// `std::fs::canonicalize` as far as the path exists, with the missing tail normalized
/// lexically. A symlink is followed when the component exists; a `..` in the tail pops.
fn canonical_existing_prefix(path: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        if let Ok(real) = std::fs::canonicalize(&current) {
            let mut out = real;
            for part in tail.iter().rev() {
                match part.to_str() {
                    Some("..") => {
                        out.pop();
                    }
                    Some(".") => {}
                    _ => out.push(part.clone()),
                }
            }
            return out;
        }
        let Some(name) = current.file_name().map(|name| name.to_os_string()) else {
            return path.to_path_buf();
        };
        tail.push(name);
        let Some(parent) = current.parent() else {
            return path.to_path_buf();
        };
        current = parent.to_path_buf();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radian_core::{ContentPart, ToolResult};
    use serde_json::json;

    use crate::test_support::TempDir;

    fn guard(label: &str) -> (TempDir, MemoryWriteGuard, PathBuf) {
        let dir = TempDir::with_label("radian-memory-guard", label);
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let root = dir.join("memory");
        let service = MemoryService::open(&ws, &root);
        let guard = MemoryWriteGuard::new(&service, false);
        (dir, guard, root)
    }

    fn call(name: &str, path: &Path, content: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: json!({"path": path.to_str().unwrap(), "content": content}),
        }
    }

    #[test]
    fn an_mcp_result_refuses_the_next_memory_write_and_nothing_else() {
        let (dir, guard, root) = guard("input");
        let memory = root.join("global/x.md");
        let workspace = dir.join("ws/out.md");
        assert!(guard.check(&call("write", &memory, "note\n")).is_ok());
        guard.note_mcp_result();
        let refusal = guard
            .check(&call("write", &memory, "note\n"))
            .expect_err("the input guard refuses");
        assert_eq!(refusal.message, INPUT_GUARD);
        assert!(
            guard.check(&call("write", &workspace, "note\n")).is_ok(),
            "a workspace write is not a memory write"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Edge case 4's message half: the matched prefix is named, the value never is.
    #[test]
    fn a_secret_refusal_names_the_prefix_and_not_the_value() {
        let (dir, guard, root) = guard("secret");
        let memory = root.join("global/x.md");
        let refusal = guard
            .check(&call("write", &memory, "sk-abcdefghijklmnopqrst\n"))
            .expect_err("the secret guard refuses");
        assert!(refusal.message.contains("(sk-)"), "{}", refusal.message);
        assert!(!refusal.message.contains("abcdefghijklmnopqrst"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_prefix_inside_a_word_is_not_a_secret() {
        let (dir, guard, root) = guard("word");
        let memory = root.join("global/x.md");
        assert!(
            guard
                .check(&call(
                    "write",
                    &memory,
                    "the musk-abcdefghijklmnopqrst note\n"
                ))
                .is_ok(),
            "the heuristic matches at the start of a token only"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Edge case 4's patch half: with a computed result, the result is what is judged. Burying
    /// a secret-shaped string inside an existing word stays allowed, because the written text
    /// has no token that starts with the prefix.
    #[test]
    fn a_patch_that_buries_a_secret_shape_inside_a_word_is_judged_by_the_result() {
        let (dir, guard, root) = guard("patch-in-word");
        let memory = root.join("global/x.md");
        std::fs::create_dir_all(memory.parent().unwrap()).unwrap();
        std::fs::write(&memory, "musk-abc\n").unwrap();
        let patch = ToolCall {
            id: "c1".into(),
            name: "patch".into(),
            arguments: json!({
                "path": memory.to_str().unwrap(),
                "old": "abc",
                "new": "sk-abcdefghijklmnopqrst",
            }),
        };
        assert!(
            guard.check(&patch).is_ok(),
            "the resulting `musk-sk-...` carries no token starting with `sk-`"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A well-formed record text whose frontmatter names `source`.
    fn record(source: &str) -> String {
        format!(
            "---\nname: x\ndescription: d\ntype: user\nsource: {source}\n\
             created: 2026-10-31T12:00:00.000000Z\nmodified: 2026-10-31T12:00:00.000000Z\n---\nbody\n"
        )
    }

    /// ADR 0021 invariant 6: `source: user` belongs to a user command, so a model `write` that
    /// declares it is refused. The value is read as the parser reads it, so a quoted or an
    /// escaped spelling is the same claim.
    #[test]
    fn a_write_that_declares_source_user_is_refused() {
        for (label, source) in [
            ("plain", "user"),
            ("single-quoted", "'user'"),
            ("double-quoted", "\"user\""),
            ("escaped", "\"us\\u0065r\""),
        ] {
            let (dir, guard, root) = guard(label);
            let memory = root.join("global/x.md");
            let refusal = guard
                .check(&call("write", &memory, &record(source)))
                .expect_err("a claimed user source is refused");
            assert_eq!(refusal.code, "memory_write_refused", "{label}");
            assert_eq!(refusal.message, USER_SOURCE, "{label}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The claim through a `patch`: the target is a model record, so only the result can say
    /// `user`, and the guard judges the result.
    #[test]
    fn a_patch_that_would_declare_source_user_is_refused() {
        let (dir, guard, root) = guard("claim-source-patch");
        let memory = root.join("global/x.md");
        std::fs::create_dir_all(memory.parent().unwrap()).unwrap();
        std::fs::write(&memory, record("model")).unwrap();
        let patch = ToolCall {
            id: "c1".into(),
            name: "patch".into(),
            arguments: json!({
                "path": memory.to_str().unwrap(),
                "old": "source: model",
                "new": "source: user",
            }),
        };
        let refusal = guard
            .check(&patch)
            .expect_err("the result would claim a user source");
        assert_eq!(refusal.message, USER_SOURCE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file a user command created declares `source: user`; neither file tool may touch it,
    /// whatever content the model supplies.
    #[test]
    fn a_write_and_a_patch_of_a_user_record_are_refused() {
        let (dir, guard, root) = guard("user-record");
        let memory = root.join("global/x.md");
        std::fs::create_dir_all(memory.parent().unwrap()).unwrap();
        std::fs::write(&memory, record("user")).unwrap();
        let write = guard
            .check(&call("write", &memory, &record("model")))
            .expect_err("a user record is not overwritten");
        assert_eq!(write.message, USER_RECORD);
        let patch = ToolCall {
            id: "c1".into(),
            name: "patch".into(),
            arguments: json!({
                "path": memory.to_str().unwrap(),
                "old": "body",
                "new": "changed",
            }),
        };
        let refusal = guard
            .check(&patch)
            .expect_err("a user record is not patched");
        assert_eq!(refusal.message, USER_RECORD);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR 0021 invariant 3: the write that creates a file fixes its source, so a later model
    /// `write` or `patch` that declares a different one is refused; the same source, or a body
    /// patch that leaves the source alone, still passes.
    #[test]
    fn a_write_or_patch_that_would_change_the_source_is_refused() {
        let (dir, guard, root) = guard("source-change");
        let memory = root.join("global/x.md");
        std::fs::create_dir_all(memory.parent().unwrap()).unwrap();
        std::fs::write(&memory, record("import")).unwrap();
        let write = guard
            .check(&call("write", &memory, &record("model")))
            .expect_err("a write may not change the source");
        assert_eq!(write.message, SOURCE_CHANGE);
        let changed = ToolCall {
            id: "c1".into(),
            name: "patch".into(),
            arguments: json!({
                "path": memory.to_str().unwrap(),
                "old": "source: import",
                "new": "source: model",
            }),
        };
        let refusal = guard
            .check(&changed)
            .expect_err("a patch may not change the source");
        assert_eq!(refusal.message, SOURCE_CHANGE);
        assert!(
            guard
                .check(&call("write", &memory, &record("import")))
                .is_ok(),
            "rewriting with the same source is allowed"
        );
        let body = ToolCall {
            id: "c2".into(),
            name: "patch".into(),
            arguments: json!({
                "path": memory.to_str().unwrap(),
                "old": "body",
                "new": "changed",
            }),
        };
        assert!(
            guard.check(&body).is_ok(),
            "a body patch leaves the source alone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn routed(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// A double underscore alone is not proof: a name no MCP server provides settles
    /// `unknown_tool` without any server seeing it, so a resume must not count it — nor a
    /// result a `before_tool_call` hook forged for it.
    #[test]
    fn an_invented_double_underscore_call_is_not_an_mcp_result() {
        let routed = routed(&["fx__echo"]);
        let journal = [
            started("c1", "foo__bar"),
            settled(
                "c1",
                ToolResult::error("c1", "unknown_tool", "There is no tool named `foo__bar`."),
            ),
        ];
        assert!(!journal_has_mcp_result(&settled_calls(&journal), &routed));
        let forged = [
            started("c2", "fx__nope"),
            settled(
                "c2",
                ToolResult::success("c2", vec![ContentPart::text("{}")]),
            ),
        ];
        assert!(
            !journal_has_mcp_result(&settled_calls(&forged), &routed),
            "an unrouted name is not outside content whatever its result"
        );
    }

    /// The positive control: a settled call to a routed MCP alias counts, and so does an
    /// error the server itself produced.
    #[test]
    fn a_settled_call_to_an_mcp_alias_is_an_mcp_result() {
        let routed = routed(&["fx__echo"]);
        let success = [
            started("c1", "fx__echo"),
            settled(
                "c1",
                ToolResult::success("c1", vec![ContentPart::text("{}")]),
            ),
        ];
        assert!(journal_has_mcp_result(&settled_calls(&success), &routed));
        let failed = [
            started("c2", "fx__echo"),
            settled(
                "c2",
                ToolResult::error("c2", "extension_error", "the server answered with an error"),
            ),
        ];
        assert!(journal_has_mcp_result(&settled_calls(&failed), &routed));
        let tool_error = [
            started("c3", "fx__echo"),
            settled(
                "c3",
                ToolResult::error("c3", "tool_error", "the tool failed"),
            ),
        ];
        assert!(journal_has_mcp_result(&settled_calls(&tool_error), &routed));
    }

    /// A fabricated `__` call can be journaled as started and settled without a server ever
    /// seeing it: an already-cancelled turn, steering, a failing `after_tool_call` hook, a
    /// permission denial, and a journal from before `error_code` existed all produce a settled
    /// result the tool did not return. None of them arms the resume guard.
    #[test]
    fn a_call_settled_by_the_runtime_without_reaching_a_server_is_not_an_mcp_result() {
        let routed = routed(&["fx__echo"]);
        let codes = [
            (Some("unknown_tool"), "fx__nope"),
            (Some("cancelled"), "fx__nope"),
            (Some("cancelled"), "fx__echo"),
            (Some("skipped_by_steering"), "fx__echo"),
            (Some("hook_error"), "fx__echo"),
            (Some("denied"), "fx__echo"),
            (None, "fx__echo"),
        ];
        for (code, name) in codes {
            let result = match code {
                Some(code) => ToolResult::error("c1", code, "the runtime settled it"),
                None => ToolResult {
                    call_id: "c1".into(),
                    content: Vec::new(),
                    structured: None,
                    is_error: true,
                    error_code: None,
                },
            };
            let journal = [started("c1", name), settled("c1", result)];
            assert!(
                !journal_has_mcp_result(&settled_calls(&journal), &routed),
                "{code:?} {name} did not reach a server"
            );
        }
    }

    fn event(seq: u64, data: EventData) -> SessionEvent {
        SessionEvent {
            seq,
            ts: String::new(),
            event: String::new(),
            critical: true,
            byte_offset: 0,
            line_len: 0,
            data,
        }
    }

    fn started(call_id: &str, name: &str) -> SessionEvent {
        event(
            1,
            EventData::ToolExecutionStarted {
                call_id: call_id.into(),
                name: name.into(),
                arguments: json!({}),
            },
        )
    }

    fn settled(call_id: &str, result: ToolResult) -> SessionEvent {
        event(
            2,
            EventData::ToolExecutionSettled {
                call_id: call_id.into(),
                result,
            },
        )
    }
}
