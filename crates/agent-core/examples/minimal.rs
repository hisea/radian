//! A two-step turn with no network: a stand-in model asks for a tool, then answers.
//!
//! Run with `cargo run -p agent-core --example minimal`.

use std::sync::Arc;

use agent_core::*;
use serde_json::json;

/// Requests `add` on the first call, then reports the tool's result.
struct FauxModel;

impl Model for FauxModel {
    async fn complete(
        &self,
        request: ModelRequest,
        _cx: ModelContext<'_>,
    ) -> Result<ModelResponse, ModelError> {
        let last_result = request.messages.iter().rev().find_map(|m| match m {
            Message::Tool { result, .. } => Some(result.text()),
            _ => None,
        });
        Ok(match last_result {
            None => ModelResponse {
                content: vec![ContentPart::text("Let me add those.")],
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "add".into(),
                    arguments: json!({"a": 2, "b": 3}),
                }],
                provider_payload: None,
                stop_reason: StopReason::ToolUse,
                usage: None,
            },
            Some(sum) => ModelResponse {
                content: vec![ContentPart::text(format!("2 + 3 = {sum}"))],
                tool_calls: vec![],
                provider_payload: None,
                stop_reason: StopReason::EndTurn,
                usage: None,
            },
        })
    }
}

struct Calculator {
    specs: Vec<ToolSpec>,
}

impl ToolSession for Calculator {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn execute(
        &self,
        call: &ToolCall,
        _cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        let a = call.arguments["a"].as_i64().unwrap_or_default();
        let b = call.arguments["b"].as_i64().unwrap_or_default();
        Ok(ToolOutput::text((a + b).to_string()))
    }
}

struct CalculatorProvider;

impl ToolProvider for CalculatorProvider {
    type Session = Calculator;

    async fn open_session(&self) -> Result<Calculator, ToolError> {
        Ok(Calculator {
            specs: vec![ToolSpec {
                name: "add".into(),
                description: "Adds two integers.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
                    "required": ["a", "b"]
                }),
                concurrency: ToolConcurrency::Shared,
            }],
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agent = Arc::new(Agent::new(FauxModel, CalculatorProvider));
    let mut chat = agent.open_session().await?;
    let print = |event: AgentEvent| println!("event: {event:?}");

    let outcome = chat
        .run_turn("What is 2 + 3?", TurnOptions::default(), &print)
        .await?;

    println!("status: {:?}", outcome.status);
    if let Some(answer) = outcome.final_message {
        println!("answer: {}", answer.text());
    }
    chat.close().await?;
    Ok(())
}
