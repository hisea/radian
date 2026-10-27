//! ADR 0026 D10 end to end through the host's route resolution (`RouteContext::args_for`,
//! the exact function a `/model` switch and the startup path call): a **named** provider's
//! model list decides the protocol and the endpoint, and the request goes to the API the
//! gateway advertised — Responses `<base>/responses` with a Bearer key and no ChatGPT/Codex
//! headers, or the Anthropic `<derived endpoint>/messages`.
//!
//! `RADIAN_HOME` is process-wide, so this binary deliberately holds exactly one test (the same
//! rule `tests/responses_api_key_route.rs` follows).

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use agent_provider_anthropic::testing::{
    MockServer as AnthropicServer, Reply as AnthropicReply, block_stop, message_delta,
    message_start, message_stop, text_delta, text_start,
};
use agent_provider_openai_responses::testing::{
    MockServer as ResponsesServer, Reply as ResponsesReply, completed, message_item,
    output_item_done,
};
use radian_core::{CancellationToken, Message, Model, ModelContext, ModelRequest, NoopSink};
use radian_host::{
    AdvertisedEndpoint, FetchedProviderModels, ModelRuleDecl, Provider, ProviderDecl, RouteContext,
    build_model,
};

/// A private `RADIAN_HOME` with one usable `magpie` credential.
fn isolated_home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = common::process_dir("radian-named-model-routing");
        let credentials = dir.join("credentials");
        std::fs::create_dir_all(&credentials).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let entry = serde_json::json!({
            "magpie": {"type": "api_key", "key": "sk-magpie"}
        });
        let auth = credentials.join("auth.json");
        std::fs::write(&auth, serde_json::to_string(&entry).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let dir = std::fs::canonicalize(&dir).unwrap();
        // SAFETY: `get_or_init` runs this once, before any catalog or credential read in this
        // binary (this is its only test).
        unsafe { std::env::set_var("RADIAN_HOME", &dir) };
        dir
    })
    .clone()
}

fn advertised(kind: &str, path: &str) -> AdvertisedEndpoint {
    AdvertisedEndpoint {
        kind: kind.to_owned(),
        path: path.to_owned(),
    }
}

fn providers(base_url: &str) -> BTreeMap<String, ProviderDecl> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "magpie".to_owned(),
        ProviderDecl {
            kind: "openai-compatible".to_owned(),
            base_url: base_url.to_owned(),
            models: vec![ModelRuleDecl {
                pattern: "claude/*".to_owned(),
                kind: "anthropic".to_owned(),
            }],
            ..Default::default()
        },
    );
    providers
}

fn context(base_url: &str) -> RouteContext {
    RouteContext {
        extra_body: Default::default(),
        providers: providers(base_url),
        session_id: "named-model-routing".to_owned(),
        user_headers: Vec::new(),
        include_usage: None,
        max_tokens: 4096,
        thinking: "medium".to_owned(),
        thinking_budget: None,
    }
}

fn request() -> ModelRequest {
    ModelRequest {
        system_prompt: None,
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
        settings: serde_json::Value::Null,
    }
}

#[tokio::test]
async fn a_named_provider_sends_to_the_api_its_model_list_advertised() {
    let _home = isolated_home();
    // The provider's own model list, as `GET <base>/models` advertised it: the Responses model
    // and the Anthropic model that also lists chat (the gateway refuses chat).
    let list = FetchedProviderModels {
        models: vec![
            "anthropic:claude-haiku-4-5".to_owned(),
            "codex/gpt-5.6-sol".to_owned(),
            "plain-chat".to_owned(),
        ],
        endpoints: BTreeMap::from([
            (
                "anthropic:claude-haiku-4-5".to_owned(),
                vec![
                    advertised("openai-compatible", "/v1/chat/completions"),
                    advertised("anthropic", "/apis/anthropic/v1/messages"),
                ],
            ),
            (
                "codex/gpt-5.6-sol".to_owned(),
                vec![advertised("openai-responses", "/v1/responses")],
            ),
            (
                "plain-chat".to_owned(),
                vec![advertised("openai-compatible", "/v1/chat/completions")],
            ),
        ]),
    };
    radian_host::write_provider_model_list("magpie", "sk-magpie", &list).unwrap();

    // Responses: the default `prefer` picks it over chat, and the request goes to
    // `<base>/responses` with the provider key as a Bearer token and no ChatGPT/Codex headers.
    let responses = ResponsesServer::start(vec![ResponsesReply::sse(&[
        output_item_done(0, message_item("msg_1", "solved")),
        completed(None),
    ])])
    .await;
    let ctx = context(&format!("{}/v1", responses.base_url));
    let args = ctx
        .args_for("magpie", "codex/gpt-5.6-sol", None)
        .expect("the Responses route resolves");
    assert_eq!(args.provider, Provider::ChatGpt);
    assert_eq!(args.base_url, format!("{}/v1", responses.base_url));
    assert!(!args.chatgpt_oauth, "an API key, not the OAuth route");
    let model = build_model(args).expect("the Responses model builds");
    model
        .complete(
            request(),
            ModelContext {
                cancellation: CancellationToken::new(),
                events: &NoopSink,
            },
        )
        .await
        .expect("the mock answers");
    let recorded = &responses.requests()[0];
    assert_eq!(recorded.path, "/v1/responses");
    assert_eq!(recorded.header("authorization"), Some("Bearer sk-magpie"));
    for name in ["chatgpt-account-id", "openai-beta", "originator"] {
        assert!(recorded.header(name).is_none(), "{name} must not be sent");
    }

    // Anthropic: the list advertises chat too, but `anthropic` comes first in `prefer`. The
    // endpoint is the advertised path's origin (`/apis/anthropic/v1`), and the Anthropic
    // adapter re-adds `/v1/messages`, so the request lands on `/apis/anthropic/v1/messages`.
    let anthropic = AnthropicServer::start(vec![
        AnthropicReply::sse(&[
            message_start(None),
            text_start(0),
            text_delta(0, "hello"),
            block_stop(0),
            message_delta("end_turn", None),
            message_stop(),
        ]),
        AnthropicReply::sse(&[
            message_start(None),
            text_start(0),
            text_delta(0, "bye"),
            block_stop(0),
            message_delta("end_turn", None),
            message_stop(),
        ]),
    ])
    .await;
    let ctx = context(&format!("{}/v1", anthropic.base_url));
    let args = ctx
        .args_for("magpie", "anthropic:claude-haiku-4-5", None)
        .expect("the Anthropic route resolves");
    assert_eq!(args.provider, Provider::Anthropic);
    assert_eq!(
        args.base_url,
        format!("{}/apis/anthropic", anthropic.base_url)
    );
    let model = build_model(args).expect("the Anthropic model builds");
    model
        .complete(
            request(),
            ModelContext {
                cancellation: CancellationToken::new(),
                events: &NoopSink,
            },
        )
        .await
        .expect("the mock answers");
    let recorded = &anthropic.requests()[0];
    assert_eq!(recorded.path, "/apis/anthropic/v1/messages");
    assert_eq!(recorded.header("x-api-key"), Some("sk-magpie"));

    // A rule beats the list and names only a protocol: the provider's `/v1` base is a chat
    // base, so the Anthropic adapter is handed the origin and sends `/v1/messages`.
    let ctx = context(&format!("{}/v1", anthropic.base_url));
    let args = ctx
        .args_for("magpie", "claude/opus-5-5", None)
        .expect("the rule route resolves");
    assert_eq!(args.provider, Provider::Anthropic);
    assert_eq!(args.base_url, anthropic.base_url);
    build_model(args)
        .expect("the rule's Anthropic model builds")
        .complete(
            request(),
            ModelContext {
                cancellation: CancellationToken::new(),
                events: &NoopSink,
            },
        )
        .await
        .expect("the mock answers");
    assert_eq!(anthropic.requests()[1].path, "/v1/messages");

    // A `/model` switch inside one named provider: the chat route first, then the Responses
    // route, builds a different adapter from the same declaration and list.
    let ctx = context(&format!("{}/v1", responses.base_url));
    let chat = ctx
        .args_for("magpie", "plain-chat", None)
        .expect("the chat route resolves");
    assert_eq!(chat.provider, Provider::OpenAiCompatible);
    let responses_route = ctx
        .args_for("magpie", "codex/gpt-5.6-sol", None)
        .expect("the switched route resolves");
    assert_eq!(responses_route.provider, Provider::ChatGpt);
    assert_eq!(
        responses_route.base_url,
        format!("{}/v1", responses.base_url)
    );

    // A model neither a rule nor the list names uses the declared `kind` and `base_url`.
    let chat = context(&format!("{}/v1", anthropic.base_url))
        .args_for("magpie", "hand-typed-model", None)
        .expect("the kind route resolves");
    assert_eq!(chat.provider, Provider::OpenAiCompatible);
    assert_eq!(chat.base_url, format!("{}/v1", anthropic.base_url));
}
