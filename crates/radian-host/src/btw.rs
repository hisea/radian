//! `/btw`: a side question answered from the current context (backlog
//! `tui-btw-side-question`).
//!
//! The side request reuses the next formal request's shape byte for byte
//! ([`LocalSession::next_request_shape`]) plus one trailing user message (the
//! question plus [`BTW_SUFFIX`]), and sends it through the session's current
//! model. It never touches the transcript or the journal, never runs a tool,
//! never records usage, and never fails a turn: every outcome lands in the
//! overlay through a [`SideSink`].

use std::sync::Arc;

use agent_core::{
    AgentEvent, CancellationToken, ContentPart, EventSink, Message, Model, ModelContext,
    ModelRequest,
};

/// The fixed sentence the trailing side message carries after the question: the
/// model answers from context and calls no tool.
pub const BTW_SUFFIX: &str = "Answer only from the existing context above. Do not call any tool.";

pub const NO_TOOL_LINE: &str = "no tools were run";

/// The trailing user message a side question appends to the formal shape: the
/// question plus [`BTW_SUFFIX`].
pub fn side_user_message(question: &str) -> Message {
    Message::user(format!("{question}\n\n{BTW_SUFFIX}"))
}

/// Where one side request reports. The host calls `delta` for streamed text and
/// exactly one `done` at the end, from its own spawned task.
pub trait SideSink: Send + Sync {
    fn delta(&self, text: &str);
    fn done(&self, outcome: SideOutcome);
}

/// One side request's outcome. `Failed` and `Cancelled` are overlay-only: they
/// never end a turn (no ADR 0017 path).
#[derive(Debug, Clone)]
pub enum SideOutcome {
    /// The answer, streamed already through `delta`.
    Done {
        text: String,
        /// The model returned tool calls; none was executed.
        tool_calls_dropped: bool,
        usage: Option<agent_core::ModelUsage>,
        /// The call's fee at the route in force, if a price was known.
        fee_micros: Option<u64>,
    },
    /// The provider (or transport) failed. Shown in the overlay only.
    Failed(String),
    /// Esc cancelled the request. `partial` is the text streamed so far.
    Cancelled { partial: String },
}

/// Cancels an in-flight side request. `cancel` is idempotent.
#[derive(Debug, Clone)]
pub struct SideCancel {
    token: CancellationToken,
}

impl SideCancel {
    pub fn new(token: CancellationToken) -> Self {
        Self { token }
    }

    pub fn cancel(&self) {
        self.token.cancel();
    }
}

/// Forwards streamed answer text to the sink while accumulating it.
struct SideTap<'a> {
    sink: &'a Arc<dyn SideSink>,
    text: std::sync::Mutex<String>,
}

impl EventSink for SideTap<'_> {
    fn emit(&self, event: AgentEvent) {
        if let AgentEvent::ModelTextDelta { text } = event {
            self.sink.delta(&text);
            self.text
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_str(&text);
        }
        // Reasoning deltas stay out of the overlay: the side answer is text only.
    }
}

/// Sends one side request: `request` already carries the trailing side message.
/// Never touches the transcript, the journal, the inbox or the driver; never
/// executes a tool; never records usage. Cancellation through `cancel` ends as
/// [`SideOutcome::Cancelled`] with the text streamed so far.
pub async fn complete_side<M: Model>(
    model: &M,
    request: ModelRequest,
    cancel: &CancellationToken,
    sink: &Arc<dyn SideSink>,
) -> SideOutcome {
    let tap = SideTap {
        sink,
        text: std::sync::Mutex::new(String::new()),
    };
    let cx = ModelContext {
        cancellation: cancel.child_token(),
        events: &tap,
    };
    match model.complete(request, cx).await {
        Ok(response) => {
            let mut text = tap.text.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if text.is_empty() {
                text = text_of(&response.content);
            }
            SideOutcome::Done {
                text,
                tool_calls_dropped: !response.tool_calls.is_empty(),
                usage: response.usage,
                fee_micros: None,
            }
        }
        Err(error) => {
            let partial = tap.text.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if cancel.is_cancelled() {
                SideOutcome::Cancelled { partial }
            } else {
                SideOutcome::Failed(error.to_string())
            }
        }
    }
}

/// The visible text of response content (the streaming tap normally already
/// accumulated it; this covers an adapter that never emitted a delta).
fn text_of(parts: &[ContentPart]) -> String {
    let mut out = String::new();
    for part in parts {
        if let ContentPart::Text { text, .. } = part {
            out.push_str(text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted `Model`: answers with `texts`, tool calls and usage on demand,
    /// or blocks until cancelled.
    struct Script {
        texts: Vec<String>,
        tool_calls: Vec<agent_core::ToolCall>,
        usage: Option<agent_core::ModelUsage>,
        block: bool,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl Model for Script {
        async fn complete(
            &self,
            _request: ModelRequest,
            cx: ModelContext<'_>,
        ) -> Result<agent_core::ModelResponse, agent_core::ModelError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.block {
                cx.cancellation.cancelled().await;
                return Err(agent_core::ModelError::Transport(
                    "the request was cancelled".into(),
                ));
            }
            for text in &self.texts {
                cx.events
                    .emit(AgentEvent::ModelTextDelta { text: text.clone() });
            }
            Ok(agent_core::ModelResponse {
                content: vec![agent_core::ContentPart::text(self.texts.concat())],
                reasoning: Vec::new(),
                tool_calls: self.tool_calls.clone(),
                provider_payload: None,
                stop_reason: agent_core::StopReason::EndTurn,
                usage: self.usage.clone(),
                limits: None,
            })
        }
    }

    struct Collect {
        deltas: std::sync::Mutex<Vec<String>>,
        outcomes: std::sync::Mutex<Vec<SideOutcome>>,
    }

    impl Collect {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                deltas: std::sync::Mutex::new(Vec::new()),
                outcomes: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    impl SideSink for Collect {
        fn delta(&self, text: &str) {
            self.deltas.lock().unwrap().push(text.to_owned());
        }

        fn done(&self, outcome: SideOutcome) {
            self.outcomes.lock().unwrap().push(outcome);
        }
    }

    fn request() -> ModelRequest {
        ModelRequest {
            system_prompt: Some("base".into()),
            messages: vec![Message::user("history")],
            tools: Vec::new(),
            settings: serde_json::Value::Null,
        }
    }

    /// The streamed deltas and the final text agree, and usage rides along.
    #[tokio::test]
    async fn the_answer_streams_and_carries_its_usage() {
        let model = Script {
            texts: vec!["hel".into(), "lo".into()],
            tool_calls: Vec::new(),
            usage: Some(agent_core::ModelUsage {
                input: Some(10),
                output: Some(3),
                ..Default::default()
            }),
            block: false,
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let sink = Collect::new();
        let sink_dyn: Arc<dyn SideSink> = sink.clone();
        let outcome = complete_side(&model, request(), &CancellationToken::new(), &sink_dyn).await;
        let SideOutcome::Done {
            text,
            tool_calls_dropped,
            usage,
            ..
        } = outcome
        else {
            panic!("a good answer is Done");
        };
        assert_eq!(text, "hello");
        assert!(!tool_calls_dropped);
        assert_eq!(usage.unwrap().input, Some(10));
        assert_eq!(sink.deltas.lock().unwrap().concat(), "hello");
    }

    /// A model error is a `Failed` outcome, never a turn failure: the runner
    /// owns no journal, inbox or driver handle that could fail one.
    #[tokio::test]
    async fn a_model_error_is_an_overlay_failure_not_a_turn_failure() {
        struct Broken;
        impl Model for Broken {
            async fn complete(
                &self,
                _request: ModelRequest,
                _cx: ModelContext<'_>,
            ) -> Result<agent_core::ModelResponse, agent_core::ModelError> {
                Err(agent_core::ModelError::Transport("boom".into()))
            }
        }
        let sink = Collect::new();
        let sink_dyn: Arc<dyn SideSink> = sink.clone();
        let outcome = complete_side(&Broken, request(), &CancellationToken::new(), &sink_dyn).await;
        assert!(
            matches!(outcome, SideOutcome::Failed(ref message) if message.contains("boom")),
            "{outcome:?}"
        );
    }

    /// Tool calls in the answer are dropped, never executed: the runner has no
    /// tool session to execute them with.
    #[tokio::test]
    async fn tool_calls_in_the_answer_are_dropped_never_executed() {
        let model = Script {
            texts: vec!["done".into()],
            tool_calls: vec![agent_core::ToolCall {
                id: "c1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "rm -rf /"}),
            }],
            usage: None,
            block: false,
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let sink = Collect::new();
        let sink_dyn: Arc<dyn SideSink> = sink.clone();
        let outcome = complete_side(&model, request(), &CancellationToken::new(), &sink_dyn).await;
        let SideOutcome::Done {
            tool_calls_dropped,
            text,
            ..
        } = outcome
        else {
            panic!("the answer is Done");
        };
        assert!(tool_calls_dropped, "the overlay must say no tool ran");
        assert_eq!(text, "done");
        assert_eq!(
            model.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one model call, no tool call"
        );
    }

    /// Cancelling mid-request ends as `Cancelled` with the partial text.
    #[tokio::test]
    async fn cancelling_mid_request_ends_as_cancelled() {
        let model = Script {
            texts: Vec::new(),
            tool_calls: Vec::new(),
            usage: None,
            block: true,
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let cancel = CancellationToken::new();
        let sink = Collect::new();
        let sink_dyn: Arc<dyn SideSink> = sink.clone();
        let cancelled = cancel.clone();
        tokio::spawn(async move {
            cancelled.cancel();
        });
        let outcome = complete_side(&model, request(), &cancel, &sink_dyn).await;
        assert!(
            matches!(outcome, SideOutcome::Cancelled { .. }),
            "{outcome:?}"
        );
    }

    /// The trailing side message is the question plus the fixed sentence.
    #[test]
    fn the_trailing_message_is_the_question_plus_the_fixed_sentence() {
        let message = side_user_message("what was that file");
        let Message::User { content, .. } = &message else {
            panic!("the side message is a user message");
        };
        let text: String = content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(text.starts_with("what was that file"), "{text}");
        assert!(text.contains(BTW_SUFFIX), "{text}");
    }
}
