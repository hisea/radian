//! The `skill` tool (ADR 0025 S3, slice 2): read one discovered skill's body.
//!
//! The body is the `SKILL.md` text without its frontmatter, followed by one line naming the
//! skill's directory as the base for relative paths (the same base the `bash` tool's working
//! directory rules point at; ADR 0025 S3's "相对路径以 skill 目录为基准"). The name is looked
//! up in the discovered table — **a path is never built from the name** (ADR 0025 S3, owner
//! ruling: a `../` in the argument must not escape). Unknown names, read failures and bodies
//! over the output cap are tool-result errors, never turn failures.

use std::sync::Arc;

use agent_core::ToolConcurrency;
use agent_core::{
    ToolCall, ToolContext, ToolError, ToolFailure, ToolOutput, ToolProvider, ToolSession, ToolSpec,
};
use serde_json::Value;

use super::{SKILL_FILE_NAME, Skill};

/// The largest body the tool returns. A skill bigger than this is a tool-result error rather
/// than a truncation: half a skill is worse than a clear failure (ADR 0025 S3). The repo has no
/// shared tool-output cap; 64 KiB matches the `read` tool's default `max_bytes`.
pub const SKILL_BODY_LIMIT_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct SkillTools {
    skills: Arc<Vec<Skill>>,
}

impl SkillTools {
    pub fn new(skills: Arc<Vec<Skill>>) -> Self {
        Self { skills }
    }
}

impl ToolProvider for SkillTools {
    type Session = SkillSession;

    async fn open_session(&self) -> Result<Self::Session, ToolError> {
        Ok(SkillSession {
            skills: Arc::clone(&self.skills),
            specs: specs(),
        })
    }
}

pub struct SkillSession {
    skills: Arc<Vec<Skill>>,
    specs: Vec<ToolSpec>,
}

impl ToolSession for SkillSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn execute(
        &self,
        call: &ToolCall,
        _cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        if call.name != "skill" {
            return Err(ToolFailure::new(
                "unknown_tool",
                format!("`{}` is not the skill tool", call.name),
            ));
        }
        let name = call
            .arguments
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolFailure::new(
                    "invalid_arguments",
                    "`name` is required and must be a string",
                )
            })?;
        let args = match call.arguments.get("args") {
            None | Some(Value::Null) => None,
            Some(Value::String(args)) => Some(args.as_str()),
            Some(_) => {
                return Err(ToolFailure::new(
                    "invalid_arguments",
                    "`args`, when given, must be a string",
                ));
            }
        };
        // The lookup is the whole safety story: only a discovered skill can be read, and its
        // path was fixed at discovery time. A skill with `disable-model-invocation: true` is
        // not readable through this tool at all (ADR 0025 S4): it is meant for the user's
        // explicit `/skill:<name>`, so the tool answers with the same `unknown_skill` error as
        // a name that does not exist and does not reveal that it does.
        let skill = self
            .skills
            .iter()
            .find(|skill| skill.name == name && !skill.disable_model_invocation)
            .ok_or_else(|| {
                ToolFailure::new("unknown_skill", format!("there is no skill named `{name}`"))
            })?;
        let text = std::fs::read_to_string(&skill.path).map_err(|error| {
            ToolFailure::new(
                "skill_unreadable",
                format!(
                    "skill `{name}` cannot be read ({}): {error}",
                    skill.path.display()
                ),
            )
        })?;
        let body = strip_frontmatter(&text).unwrap_or(&text);
        if body.len() > SKILL_BODY_LIMIT_BYTES {
            return Err(ToolFailure::new(
                "skill_too_large",
                format!(
                    "skill `{name}` is {} bytes, over the {SKILL_BODY_LIMIT_BYTES}-byte limit",
                    body.len()
                ),
            ));
        }
        let mut out = String::with_capacity(body.len() + 128);
        out.push_str(body.trim_start_matches('\n'));
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!(
            "\nRelative paths in this skill are resolved from: {}\n",
            skill.dir.display()
        ));
        if let Some(args) = args {
            // The arguments are appended verbatim (ADR 0025 S3): they are data the model wrote,
            // not a command line.
            out.push_str("\nArguments: ");
            out.push_str(args);
            out.push('\n');
        }
        Ok(ToolOutput::text(out))
    }
}

/// Cuts the leading YAML frontmatter block from a `SKILL.md` body. `None` when the file has no
/// frontmatter at all (the body is then returned whole).
pub fn strip_frontmatter(text: &str) -> Option<&str> {
    let mut lines = text.split_inclusive('\n');
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let mut offset = text.find('\n')? + 1;
    for line in lines {
        offset += line.len();
        if line.trim_end() == "---" {
            return Some(&text[offset..]);
        }
    }
    None
}

fn specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "skill".into(),
        description: format!(
            "Read the full body of one of the skills listed in the system prompt. Pass the \
             skill's `name`; its body is returned with its directory noted as the base for \
             relative paths. Optional `args` are appended to the returned text (for skills that \
             template their arguments). The body is limited to {SKILL_BODY_LIMIT_BYTES} bytes."
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": format!("The skill's name, exactly as listed (its `{SKILL_FILE_NAME}` frontmatter `name`).")
                },
                "args": {
                    "type": "string",
                    "description": "Optional arguments appended verbatim to the returned body."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
        concurrency: ToolConcurrency::Shared,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{SKILL_FILE_NAME, discover};
    use agent_core::NoopSink;
    use serde_json::json;

    struct Scratch {
        root: std::path::PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!(
                "radian-skill-tool-{label}-{}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn write(&self, dir: &str, body: &str) {
            let path = self.root.join("skills").join(dir);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join(SKILL_FILE_NAME), body).unwrap();
        }

        async fn session(&self) -> SkillSession {
            let skills = Arc::new(discover(&self.root.join("skills")).skills);
            SkillTools::new(skills).open_session().await.unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn call(name: &str, args: Option<&str>) -> ToolCall {
        let mut arguments = json!({ "name": name });
        if let Some(args) = args {
            arguments["args"] = json!(args);
        }
        ToolCall {
            id: "call-1".into(),
            name: "skill".into(),
            arguments,
        }
    }

    fn text_of(output: &ToolOutput) -> String {
        output
            .content
            .iter()
            .map(|part| match part {
                agent_core::ContentPart::Text { text, .. } => text.clone(),
                _ => String::new(),
            })
            .collect()
    }

    async fn run(session: &SkillSession, call: &ToolCall) -> Result<ToolOutput, ToolFailure> {
        let sink = NoopSink;
        session
            .execute(
                call,
                ToolContext {
                    cancellation: agent_core::CancellationToken::new(),
                    events: &sink,
                    index: 0,
                },
            )
            .await
    }

    #[tokio::test]
    async fn the_body_has_no_frontmatter_and_names_the_base_directory() {
        let scratch = Scratch::new("body");
        scratch.write(
            "greet",
            "---\nname: greet\ndescription: Say hello.\n---\nSay hello to the user.\n",
        );
        let session = scratch.session().await;
        let out = run(&session, &call("greet", None)).await.unwrap();
        let text = text_of(&out);
        assert!(!text.contains("description: Say hello."), "{text}");
        assert!(text.contains("Say hello to the user."), "{text}");
        assert!(
            text.contains(&scratch.root.join("skills/greet").display().to_string()),
            "{text}"
        );
    }

    #[tokio::test]
    async fn args_are_appended_verbatim() {
        let scratch = Scratch::new("args");
        scratch.write("a", "---\nname: a\ndescription: d\n---\nBody.\n");
        let session = scratch.session().await;
        let out = run(&session, &call("a", Some("--x \"引号\"")))
            .await
            .unwrap();
        let text = text_of(&out);
        assert!(text.contains("--x \"引号\""), "{text}");
    }

    #[tokio::test]
    async fn an_unknown_name_is_a_tool_result_error() {
        let scratch = Scratch::new("unknown");
        scratch.write("a", "---\nname: a\ndescription: d\n---\nBody.\n");
        let session = scratch.session().await;
        let error = run(&session, &call("nope", None)).await.unwrap_err();
        assert_eq!(error.code, "unknown_skill");
        // A traversal-looking name is just an unknown name: the path is never built from it.
        let error = run(&session, &call("../a", None)).await.unwrap_err();
        assert_eq!(error.code, "unknown_skill");
    }

    /// S4: `disable-model-invocation: true` hides a skill from the model. The tool must answer
    /// exactly like an unknown name (same code, same text) rather than reading the body.
    #[tokio::test]
    async fn a_model_invocation_disabled_skill_is_unknown_to_the_tool() {
        let scratch = Scratch::new("disabled");
        scratch.write(
            "secret",
            "---\nname: secret\ndescription: d\ndisable-model-invocation: true\n---\nSecret body.\n",
        );
        let session = scratch.session().await;
        let error = run(&session, &call("secret", None)).await.unwrap_err();
        assert_eq!(error.code, "unknown_skill");
        assert_eq!(error.message, "there is no skill named `secret`");
    }

    #[tokio::test]
    async fn a_file_deleted_after_discovery_is_a_tool_result_error() {
        let scratch = Scratch::new("deleted");
        scratch.write("a", "---\nname: a\ndescription: d\n---\nBody.\n");
        let session = scratch.session().await;
        std::fs::remove_file(scratch.root.join("skills/a/SKILL.md")).unwrap();
        let error = run(&session, &call("a", None)).await.unwrap_err();
        assert_eq!(error.code, "skill_unreadable");
    }

    #[tokio::test]
    async fn a_body_over_the_cap_is_a_tool_result_error() {
        let scratch = Scratch::new("large");
        let body = format!(
            "---\nname: big\ndescription: d\n---\n{}",
            "x".repeat(SKILL_BODY_LIMIT_BYTES + 1)
        );
        scratch.write("big", &body);
        let session = scratch.session().await;
        let error = run(&session, &call("big", None)).await.unwrap_err();
        assert_eq!(error.code, "skill_too_large");
    }

    #[tokio::test]
    async fn a_missing_name_is_invalid_arguments() {
        let scratch = Scratch::new("missing-name");
        scratch.write("a", "---\nname: a\ndescription: d\n---\nBody.\n");
        let session = scratch.session().await;
        let bad = ToolCall {
            id: "call-1".into(),
            name: "skill".into(),
            arguments: json!({}),
        };
        let error = run(&session, &bad).await.unwrap_err();
        assert_eq!(error.code, "invalid_arguments");
    }

    #[test]
    fn strip_frontmatter_handles_absent_and_unterminated_blocks() {
        assert_eq!(
            strip_frontmatter("---\nname: a\n---\nBody.\n"),
            Some("Body.\n")
        );
        assert_eq!(strip_frontmatter("Body.\n"), None);
        assert_eq!(strip_frontmatter("---\nname: a\nBody.\n"), None);
    }
}
