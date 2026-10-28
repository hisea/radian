//! ADR 0026 D10 end to end: a named provider's request that is refused with a
//! protocol-status HTTP error probes the other protocols, remembers the one that works, and
//! writes the rule. The tests drive `SwitchableModel` — the exact seam both hosts use — so the
//! probe, the notice and the write-back are the ones production runs.
//!
//! `RADIAN_HOME` is process-wide, so `home` sets it once and every test holds [`LOCK`]: the key
//! pool and the notice queue are process state too.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;

use agent_provider_anthropic::testing::{
    Reply as AnthropicReply, block_stop, message_delta, message_start, message_stop, text_delta,
    text_start,
};
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use agent_provider_openai_responses::testing::{
    Reply as ResponsesReply, completed, message_item, output_item_done,
};
use radian_core::{
    Agent, AgentConfig, CancellationToken, Model, ModelContext, ModelRequest, NoTools, NoopSink,
    RunStatus, TurnOptions,
};
use radian_host::{
    AdvertisedEndpoint, FetchedProviderModels, ModelRoute, ProviderDecl, RouteContext,
    SwitchableModel, build_model,
};
use serde_json::json;

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A private `RADIAN_HOME` with one usable `magpie` credential.
fn home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = common::process_dir("radian-named-probe");
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
        // SAFETY: `get_or_init` runs this once, before any credential read in this binary;
        // every test takes `LOCK` first.
        unsafe { std::env::set_var("RADIAN_HOME", &dir) };
        dir
    })
    .clone()
}

fn prepare() -> PathBuf {
    home();
    radian_host::key_pool::reset();
    // The cache is shared by the process; each test writes its own list.
    let root = std::env::var_os("RADIAN_HOME").unwrap();
    let path = PathBuf::from(root).join("cache").join("providers");
    let _ = std::fs::remove_dir_all(path);
    std::env::var_os("RADIAN_HOME")
        .map(PathBuf::from)
        .expect("a test home")
}

fn advertised(kind: &str, path: &str) -> AdvertisedEndpoint {
    AdvertisedEndpoint {
        kind: kind.to_owned(),
        path: path.to_owned(),
    }
}

fn write_list(models: &[(&str, Vec<AdvertisedEndpoint>)]) {
    let list = FetchedProviderModels {
        models: models.iter().map(|(id, _)| (*id).to_owned()).collect(),
        endpoints: models
            .iter()
            .map(|(id, endpoints)| ((*id).to_owned(), endpoints.clone()))
            .collect(),
    };
    radian_host::write_provider_model_list("magpie", "sk-magpie", &list).unwrap();
}

fn config_text(base_url: &str, prefer: Option<&[&str]>, rules: &str) -> String {
    let prefer = match prefer {
        Some(prefer) => format!(
            "prefer = [{}]\n",
            prefer
                .iter()
                .map(|kind| format!("\"{kind}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None => String::new(),
    };
    format!(
        "schema_version = 1\n\
         [providers.magpie]\n\
         kind = \"openai-compatible\"\n\
         base_url = \"{base_url}\"\n\
         {prefer}{rules}"
    )
}

/// Writes the config and returns the guard (kept alive by the caller), its path and the parsed
/// declarations.
fn config(
    label: &str,
    base_url: &str,
    prefer: Option<&[&str]>,
    rules: &str,
) -> (common::TempDir, PathBuf, BTreeMap<String, ProviderDecl>) {
    let dir = common::TempDir::named(label);
    let path = dir.join("config.toml");
    let text = config_text(base_url, prefer, rules);
    std::fs::write(&path, text).unwrap();
    let providers = radian_host::parse_config(&path, &config_text(base_url, prefer, rules))
        .unwrap()
        .providers;
    (dir, path, providers)
}

fn context(providers: BTreeMap<String, ProviderDecl>, model: &str) -> (RouteContext, ModelRoute) {
    let ctx = RouteContext {
        providers,
        session_id: "named-probe".to_owned(),
        user_headers: Vec::new(),
        extra_body: Default::default(),
        include_usage: None,
        max_tokens: 4096,
        thinking: "medium".to_owned(),
        thinking_budget: None,
    };
    (ctx, ModelRoute::new("magpie", model))
}

fn switchable(
    ctx: &RouteContext,
    route: &ModelRoute,
    config_path: Option<&Path>,
    read_only: bool,
) -> SwitchableModel {
    let args = ctx
        .args_for(&route.provider, &route.model, None)
        .expect("the route resolves");
    let initial = build_model(args).expect("the startup model builds");
    SwitchableModel::new(
        initial,
        route.clone(),
        radian_runtime::ConfigSlot::new(serde_json::Value::Null),
        ctx.clone(),
    )
    .with_config(config_path.map(Path::to_path_buf), read_only)
}

fn request() -> ModelRequest {
    ModelRequest {
        system_prompt: None,
        messages: vec![radian_core::Message::user("hi")],
        tools: Vec::new(),
        settings: serde_json::Value::Null,
    }
}

fn call_context() -> ModelContext<'static> {
    ModelContext {
        cancellation: CancellationToken::new(),
        events: &NoopSink,
    }
}

fn anthropic_reply(text: &str) -> AnthropicReply {
    AnthropicReply::sse(&[
        message_start(None),
        text_start(0),
        text_delta(0, text),
        block_stop(0),
        message_delta("end_turn", None),
        message_stop(),
    ])
}

fn responses_reply(text: &str) -> Reply {
    let ResponsesReply::Sse {
        pieces,
        pause,
        headers,
    } = ResponsesReply::sse(&[
        output_item_done(0, message_item("msg_1", text)),
        completed(None),
    ])
    else {
        unreachable!("an sse reply")
    };
    Reply::Sse {
        pieces,
        pause,
        headers,
    }
}

/// One Anthropic stream event as the mock sends it, so an `SseThenCut` can carry a partial
/// answer.
fn anthropic_piece(event: serde_json::Value) -> Vec<u8> {
    format!("data: {event}\n\n").into_bytes()
}

/// A switchable model whose slot already carries a `/thinking` change. The driver writes the
/// slot at a turn boundary, so a test that does not stand up a driver writes it here.
fn switchable_at_thinking(
    ctx: &RouteContext,
    route: &ModelRoute,
    path: &Path,
    level: &str,
) -> (SwitchableModel, Arc<radian_runtime::ConfigSlot>) {
    let slot = radian_runtime::ConfigSlot::new(json!({ "thinking": level }));
    let args = ctx
        .args_for(&route.provider, &route.model, None)
        .expect("the route resolves");
    let initial = build_model(args).expect("the startup model builds");
    let model = SwitchableModel::new(initial, route.clone(), Arc::clone(&slot), ctx.clone())
        .with_config(Some(path.to_path_buf()), false);
    (model, slot)
}

/// The main scenario: the model list puts the model under chat, the gateway refuses chat with
/// 400, and `/messages` answers. The turn completes on anthropic, one notice names both
/// protocols, the rule is appended once, and a second, fresh session resolves by the rule
/// without sending a chat request.
#[tokio::test]
async fn a_refused_protocol_probes_another_and_saves_the_rule() {
    let _guard = LOCK.lock().await;
    let _home = prepare();
    let server = MockServer::start(vec![
        Reply::status(400, "chat is not served for this model"),
        anthropic_reply("hello").into_compatible(),
        anthropic_reply("again").into_compatible(),
    ])
    .await;
    write_list(&[(
        "claude/opus-5-5",
        vec![advertised("openai-compatible", "/v1/chat/completions")],
    )]);
    let (_dir, path, providers) = config("named-probe-main", &server.base_url, None, "");
    let (ctx, route) = context(providers, "claude/opus-5-5");

    let mut model = switchable(&ctx, &route, Some(&path), false);
    let queue = radian_runtime::ConfigQueue::new();
    model.set_config_queue(std::sync::Arc::clone(&queue));
    let agent = std::sync::Arc::new(Agent::new(model, NoTools).with_config(AgentConfig::default()));
    let mut chat = agent.open_session().await.expect("a session");
    let outcome = chat
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .expect("the turn finishes");
    assert_eq!(outcome.status, RunStatus::Completed, "{:?}", outcome.status);
    assert!(
        chat.transcript()
            .last()
            .is_some_and(|message| message.text().contains("hello")),
        "the anthropic answer is in the transcript"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    assert_eq!(requests[0].path, "/v1/chat/completions");
    assert_eq!(requests[1].path, "/v1/messages");
    assert_eq!(requests[1].header("x-api-key"), Some("sk-magpie"));

    let notices = radian_host::key_pool::take_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    let notice = &notices[0];
    assert!(notice.contains("magpie/claude/opus-5-5"), "{notice}");
    assert!(
        notice.contains("openai-compatible refused (HTTP 400)"),
        "{notice}"
    );
    assert!(notice.contains("anthropic works"), "{notice}");
    assert!(
        notice.contains(&format!("saved to {}", path.display())),
        "{notice}"
    );

    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(after.matches("kind = \"anthropic\"").count(), 1, "{after}");
    assert!(after.contains("match = \"claude/opus-5-5\""), "{after}");
    // The same change a `/model` switch queues: the driver journals it at the next boundary.
    assert_eq!(queue.len(), 1, "one ConfigChanged route");

    // A second session parses the file afresh: the rule picks anthropic at startup, so the
    // next request is the only one (the mock's third reply) and no chat request follows.
    let parsed = radian_host::parse_config(&path, &after).unwrap();
    let (ctx, route) = context(parsed.providers, "claude/opus-5-5");
    let args = ctx.args_for(&route.provider, &route.model, None).unwrap();
    assert_eq!(args.provider, radian_host::Provider::Anthropic);
    let second = switchable(&ctx, &route, Some(&path), false);
    second
        .complete(request(), call_context())
        .await
        .expect("the rule's route answers");
    let requests = server.requests();
    assert_eq!(requests.len(), 3, "{requests:#?}");
    assert_eq!(requests[2].path, "/v1/messages");
    assert!(
        radian_host::key_pool::take_notices().is_empty(),
        "a rule route never probes"
    );
}

/// 401, 403, 429 and 5xx are not protocol refusals: they take the ordinary path and send no
/// probe request.
#[tokio::test]
async fn a_non_probe_status_never_probes() {
    let _guard = LOCK.lock().await;
    let _home = prepare();
    for status in [401, 403, 429, 500, 503] {
        let server = MockServer::start(vec![Reply::status(status, "no")]).await;
        write_list(&[(
            "m/refused",
            vec![advertised("openai-compatible", "/v1/chat/completions")],
        )]);
        let (_dir, path, providers) = config("named-probe-status", &server.base_url, None, "");
        let (ctx, route) = context(providers, "m/refused");
        let model = switchable(&ctx, &route, Some(&path), false);
        let error = model
            .complete(request(), call_context())
            .await
            .expect_err("the request fails");
        assert!(!error.to_string().contains("tried"), "{status}: {error}");
        assert_eq!(server.requests().len(), 1, "{status}: no probe request");
        assert!(
            radian_host::key_pool::take_notices().is_empty(),
            "{status}: no probe notice"
        );
    }
}

/// A stream that breaks after it started is not an HTTP refusal, even when the adapter reports
/// it as a `Protocol` error.
#[tokio::test]
async fn a_mid_stream_failure_never_probes() {
    let _guard = LOCK.lock().await;
    let _home = prepare();
    let server = MockServer::start(vec![Reply::SseThenCut {
        pieces: vec![
            format!(
                "data: {}\n\n",
                chunk(json!({"role": "assistant", "content": "partial"}), None)
            )
            .into_bytes(),
        ],
    }])
    .await;
    write_list(&[(
        "m/cut",
        vec![advertised("openai-compatible", "/v1/chat/completions")],
    )]);
    let (_dir, path, providers) = config("named-probe-cut", &server.base_url, None, "");
    let (ctx, route) = context(providers, "m/cut");
    let model = switchable(&ctx, &route, Some(&path), false);
    let error = model
        .complete(request(), call_context())
        .await
        .expect_err("the stream ends early");
    assert!(!error.to_string().contains("tried"), "{error}");
    assert_eq!(server.requests().len(), 1, "no probe request");
    assert!(radian_host::key_pool::take_notices().is_empty());
}

/// A transport failure never probes either: there is no HTTP status at all.
#[tokio::test]
async fn a_transport_failure_never_probes() {
    let _guard = LOCK.lock().await;
    let _home = prepare();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    write_list(&[(
        "m/down",
        vec![advertised("openai-compatible", "/v1/chat/completions")],
    )]);
    let base = format!("http://{address}/v1");
    let (_dir, path, providers) = config("named-probe-down", &base, None, "");
    let (ctx, route) = context(providers, "m/down");
    let model = switchable(&ctx, &route, Some(&path), false);
    let error = model
        .complete(request(), call_context())
        .await
        .expect_err("nothing listens");
    assert!(!error.to_string().contains("tried"), "{error}");
    assert!(radian_host::key_pool::take_notices().is_empty());
}

/// A rule is a decision: a 400 from it names the rule and the file and never probes.
#[tokio::test]
async fn a_rule_sourced_failure_names_the_rule_and_does_not_probe() {
    let _guard = LOCK.lock().await;
    let _home = prepare();
    let server = MockServer::start(vec![Reply::status(404, "no such model")]).await;
    write_list(&[(
        "claude/opus-5-5",
        vec![advertised("openai-compatible", "/v1/chat/completions")],
    )]);
    let rules = "[[providers.magpie.models]]\n\
                 match = \"claude/*\"\n\
                 kind = \"anthropic\"\n";
    let (_dir, path, providers) = config("named-probe-rule", &server.base_url, None, rules);
    let (ctx, route) = context(providers, "claude/opus-5-5");
    let args = ctx.args_for(&route.provider, &route.model, None).unwrap();
    assert_eq!(args.provider, radian_host::Provider::Anthropic);
    let model = switchable(&ctx, &route, Some(&path), false);
    let error = model
        .complete(request(), call_context())
        .await
        .expect_err("the rule's protocol refuses");
    let text = error.to_string();
    assert!(text.contains("rule claude/*"), "{text}");
    assert!(text.contains(&path.display().to_string()), "{text}");
    assert!(!text.contains("tried"), "{text}");
    assert_eq!(server.requests().len(), 1, "no probe request");
    assert!(radian_host::key_pool::take_notices().is_empty());
}

/// Every protocol refusing ends the turn with the first error plus the tried list and the rule
/// hint; no rule is written.
#[tokio::test]
async fn every_attempt_failing_keeps_the_first_error_and_writes_nothing() {
    let _guard = LOCK.lock().await;
    let _home = prepare();
    let server = MockServer::start(vec![
        Reply::status(400, "not chat"),
        Reply::status(404, "not messages"),
        Reply::status(400, "not responses"),
        // The second half is the same failure through a real turn.
        Reply::status(400, "not chat"),
        Reply::status(404, "not messages"),
        Reply::status(400, "not responses"),
    ])
    .await;
    write_list(&[(
        "claude/opus-5-5",
        vec![advertised("openai-compatible", "/v1/chat/completions")],
    )]);
    let (_dir, path, providers) = config("named-probe-all", &server.base_url, None, "");
    let before = std::fs::read_to_string(&path).unwrap();
    let (ctx, route) = context(providers, "claude/opus-5-5");
    let model = switchable(&ctx, &route, Some(&path), false);
    let error = model
        .complete(request(), call_context())
        .await
        .expect_err("every protocol refuses");
    let text = error.to_string();
    assert!(text.starts_with("model protocol error: HTTP 400"), "{text}");
    assert!(
        text.contains(
            "tried openai-compatible (HTTP 400), anthropic (HTTP 404), \
             openai-responses (HTTP 400)"
        ),
        "{text}"
    );
    assert!(
        text.contains("Set the protocol: [[providers.magpie.models]]"),
        "{text}"
    );
    assert!(
        text.contains("kind = \"<openai-compatible|anthropic|openai-responses>\""),
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "nothing written"
    );
    assert!(radian_host::key_pool::take_notices().is_empty());

    // The same failure through a real turn: the turn ends failed and the transcript holds no
    // assistant message from any failed attempt.
    let (ctx, route) = context(
        radian_host::parse_config(&path, &before).unwrap().providers,
        "claude/opus-5-5",
    );
    let model = switchable(&ctx, &route, None, false);
    let agent = std::sync::Arc::new(Agent::new(model, NoTools).with_config(AgentConfig::default()));
    let mut chat = agent.open_session().await.expect("a session");
    let error = chat
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .expect_err("the model error ends the turn's model call");
    assert!(
        matches!(error, radian_core::RunError::Model(_)),
        "{error:?}"
    );
    assert!(
        error
            .to_string()
            .contains("tried openai-compatible (HTTP 400)"),
        "{error}"
    );
    assert!(
        chat.transcript()
            .iter()
            .all(|message| !matches!(message, radian_core::Message::Assistant { .. })),
        "no failed attempt left an assistant message"
    );
}

/// A read-only config keeps the route for this session and says why the rule was not saved.
#[tokio::test]
async fn a_read_only_config_is_not_saved() {
    let _guard = LOCK.lock().await;
    let _home = prepare();
    let server = MockServer::start(vec![
        Reply::status(400, "not chat"),
        anthropic_reply("hello").into_compatible(),
    ])
    .await;
    write_list(&[(
        "claude/opus-5-5",
        vec![advertised("openai-compatible", "/v1/chat/completions")],
    )]);
    let (_dir, path, providers) = config("named-probe-readonly", &server.base_url, None, "");
    let read_only_text = format!(
        "schema_version = 99\n\
         [providers.magpie]\n\
         kind = \"openai-compatible\"\n\
         base_url = \"{}\"\n",
        server.base_url
    );
    std::fs::write(&path, &read_only_text).unwrap();
    let (ctx, route) = context(providers, "claude/opus-5-5");
    let model = switchable(&ctx, &route, Some(&path), true);
    let _response = model
        .complete(request(), call_context())
        .await
        .expect("the probed route is used");
    let notices = radian_host::key_pool::take_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("not saved:"), "{}", notices[0]);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), read_only_text);
}

/// The attempt cap and the prefer order: chat refuses, anthropic refuses, responses answers —
/// three requests, in that order.
#[tokio::test]
async fn the_probe_tries_the_prefer_order_and_stops_at_two_extra_attempts() {
    let _guard = LOCK.lock().await;
    let _home = prepare();
    let server = MockServer::start(vec![
        Reply::status(400, "not chat"),
        Reply::status(404, "not messages"),
        responses_reply("solved"),
    ])
    .await;
    write_list(&[(
        "m/model",
        vec![advertised("openai-compatible", "/v1/chat/completions")],
    )]);
    let (_dir, path, providers) = config("named-probe-order", &server.base_url, None, "");
    let (ctx, route) = context(providers, "m/model");
    let model = switchable(&ctx, &route, Some(&path), false);
    model
        .complete(request(), call_context())
        .await
        .expect("the third protocol answers");
    let requests = server.requests();
    assert_eq!(requests.len(), 3, "{requests:#?}");
    assert_eq!(requests[0].path, "/v1/chat/completions");
    assert_eq!(requests[1].path, "/v1/messages", "anthropic comes first");
    assert_eq!(requests[2].path, "/v1/responses", "then responses");
    let notice = radian_host::key_pool::take_notices()
        .pop()
        .expect("one notice");
    assert!(
        notice.contains("openai-compatible refused (HTTP 400)"),
        "{notice}"
    );
    assert!(notice.contains("openai-responses works"), "{notice}");
}

/// A protocol the model list names an endpoint for is probed on that derived endpoint, not on
/// `base_url`. `prefer` puts chat first so the startup route is chat and the probe is reachable.
#[tokio::test]
async fn a_model_list_endpoint_is_probed_before_the_base_url() {
    let _guard = LOCK.lock().await;
    let _home = prepare();
    let server = MockServer::start(vec![
        Reply::status(400, "not chat"),
        anthropic_reply("hello").into_compatible(),
    ])
    .await;
    write_list(&[(
        "m/model",
        vec![
            advertised("openai-compatible", "/v1/chat/completions"),
            advertised("anthropic", "/apis/anthropic/v1/messages"),
        ],
    )]);
    let (_dir, path, providers) = config(
        "named-probe-endpoint",
        &server.base_url,
        Some(&["openai-compatible", "openai-responses", "anthropic"]),
        "",
    );
    let (ctx, route) = context(providers, "m/model");
    let args = ctx.args_for(&route.provider, &route.model, None).unwrap();
    assert_eq!(args.provider, radian_host::Provider::OpenAiCompatible);
    let model = switchable(&ctx, &route, Some(&path), false);
    model
        .complete(request(), call_context())
        .await
        .expect("the advertised anthropic endpoint answers");
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    assert_eq!(requests[0].path, "/v1/chat/completions");
    assert_eq!(requests[1].path, "/apis/anthropic/v1/messages");
}

/// A `/model` switch queued mid-turn is the user's decision: a probe on the old route must not
/// queue the old route after it and undo the switch.
#[tokio::test]
async fn a_queued_model_switch_is_not_undone_by_a_probe() {
    let _guard = LOCK.lock().await;
    let _home = prepare();
    let server = MockServer::start(vec![
        Reply::status(400, "not chat"),
        anthropic_reply("hello").into_compatible(),
    ])
    .await;
    write_list(&[(
        "old/model",
        vec![advertised("openai-compatible", "/v1/chat/completions")],
    )]);
    let (_dir, path, providers) = config("named-probe-queued", &server.base_url, None, "");
    let (ctx, route) = context(providers, "old/model");
    let mut model = switchable(&ctx, &route, Some(&path), false);
    let queue = radian_runtime::ConfigQueue::new();
    queue.push(ModelRoute::new("openai", "gpt-new").to_value());
    model.set_config_queue(std::sync::Arc::clone(&queue));
    model
        .complete(request(), call_context())
        .await
        .expect("the probe answers");
    assert_eq!(
        queue.pending_model_route(),
        Some(ModelRoute::new("openai", "gpt-new")),
        "the user's switch is still the last queued route"
    );
    assert_eq!(queue.len(), 1, "the probe queued nothing");
}

/// Edge 1 (review): a session started at `medium`, then `/thinking high`. The candidate the
/// probe sends carries `high`'s wire mapping, and so does the next request, which reuses the
/// model the probe remembered.
#[tokio::test]
async fn a_probe_candidate_carries_the_thinking_level_in_force() {
    let _guard = LOCK.lock().await;
    let _home = prepare();
    let server = MockServer::start(vec![
        Reply::status(404, "chat is not served"),
        anthropic_reply("hello").into_compatible(),
        anthropic_reply("again").into_compatible(),
    ])
    .await;
    write_list(&[(
        "m/thinking",
        vec![advertised("openai-compatible", "/v1/chat/completions")],
    )]);
    let (_dir, path, providers) = config("named-probe-thinking", &server.base_url, None, "");
    // The startup context is `medium`; the applied level is `high`.
    let (ctx, route) = context(providers, "m/thinking");
    assert_eq!(ctx.thinking, "medium");
    let (model, _slot) = switchable_at_thinking(&ctx, &route, &path, "high");

    model
        .complete(request(), call_context())
        .await
        .expect("the probe answers");
    model
        .complete(request(), call_context())
        .await
        .expect("the remembered route answers");

    let requests = server.requests();
    assert_eq!(requests.len(), 3, "{requests:#?}");
    assert_eq!(requests[0].path, "/v1/chat/completions");
    assert_eq!(requests[1].path, "/v1/messages");
    // ADR 0026 D11: `high` on the Anthropic map is `enabled:4096`, clamped to
    // `max_tokens - 1 = 4095`; the stale startup `medium` would be `adaptive`.
    let high = json!({"type": "enabled", "budget_tokens": 4095});
    assert_eq!(
        requests[1].json()["thinking"],
        high,
        "the candidate carries the level in force, not the startup level"
    );
    assert_eq!(
        requests[2].json()["thinking"],
        high,
        "the remembered route keeps the level it was probed at"
    );
}

/// Edge 2 (review): after a probe succeeded at `high`, `/thinking low` sends `low`. The
/// remembered route is keyed per thinking level, so the level is not frozen at the probe.
#[tokio::test]
async fn a_remembered_probe_follows_a_later_thinking_change() {
    let _guard = LOCK.lock().await;
    let _home = prepare();
    let server = MockServer::start(vec![
        Reply::status(404, "chat is not served"),
        anthropic_reply("hello").into_compatible(),
        anthropic_reply("again").into_compatible(),
        anthropic_reply("low").into_compatible(),
    ])
    .await;
    write_list(&[(
        "m/thinking-change",
        vec![advertised("openai-compatible", "/v1/chat/completions")],
    )]);
    let (_dir, path, providers) = config("named-probe-thinking-change", &server.base_url, None, "");
    let (ctx, route) = context(providers, "m/thinking-change");
    let (model, slot) = switchable_at_thinking(&ctx, &route, &path, "high");

    model
        .complete(request(), call_context())
        .await
        .expect("the probe answers");
    model
        .complete(request(), call_context())
        .await
        .expect("the remembered route answers");

    slot.set_for_test(json!({ "thinking": "low" }));
    model
        .complete(request(), call_context())
        .await
        .expect("the new level answers");

    let requests = server.requests();
    assert_eq!(requests.len(), 4, "{requests:#?}");
    assert_eq!(
        requests[2].json()["thinking"],
        json!({"type": "enabled", "budget_tokens": 4095}),
        "still on the level the probe was made at"
    );
    assert_eq!(
        requests[3].json()["thinking"],
        json!({"type": "enabled", "budget_tokens": 2048}),
        "the next request follows the new level"
    );
}

/// Edge 3 (review): a candidate that returns 200, streams one delta and then drops the
/// connection ends the probe there. No third protocol is tried, the turn reports that
/// stream error plus the tried line, and no rule is written.
#[tokio::test]
async fn a_candidate_that_streams_then_fails_ends_the_probe() {
    let _guard = LOCK.lock().await;
    let _home = prepare();
    let cut = AnthropicReply::SseThenCut {
        pieces: vec![
            anthropic_piece(message_start(None)),
            anthropic_piece(text_start(0)),
            anthropic_piece(text_delta(0, "partial")),
        ],
    };
    let server = MockServer::start(vec![
        Reply::status(400, "chat is not served"),
        cut.into_compatible(),
    ])
    .await;
    write_list(&[(
        "m/cut-candidate",
        vec![advertised("openai-compatible", "/v1/chat/completions")],
    )]);
    let (_dir, path, providers) = config("named-probe-cut-candidate", &server.base_url, None, "");
    let before = std::fs::read_to_string(&path).unwrap();
    let (ctx, route) = context(providers, "m/cut-candidate");
    let model = switchable(&ctx, &route, Some(&path), false);

    let error = model
        .complete(request(), call_context())
        .await
        .expect_err("the stream is cut");
    let text = error.to_string();
    assert!(
        text.contains("tried openai-compatible (HTTP 400)"),
        "{text}"
    );
    assert!(text.contains("anthropic (transport)"), "{text}");
    assert!(text.contains("Set the protocol:"), "{text}");
    assert_eq!(
        server.requests().len(),
        2,
        "no Responses request after the stream started: {:#?}",
        server.requests()
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "a failed probe writes no rule"
    );
    assert!(radian_host::key_pool::take_notices().is_empty());
}

/// Edge 4 (review): a refused candidate still probes the next protocol, unchanged. Chat is
/// refused with 400, Anthropic with 404, and Responses answers.
#[tokio::test]
async fn a_refused_candidate_still_probes_the_next_protocol() {
    let _guard = LOCK.lock().await;
    let _home = prepare();
    let server = MockServer::start(vec![
        Reply::status(400, "not chat"),
        Reply::status(404, "not messages"),
        responses_reply("solved"),
    ])
    .await;
    write_list(&[(
        "m/refusals",
        vec![advertised("openai-compatible", "/v1/chat/completions")],
    )]);
    let (_dir, path, providers) = config("named-probe-refusals", &server.base_url, None, "");
    let (ctx, route) = context(providers, "m/refusals");
    let model = switchable(&ctx, &route, Some(&path), false);

    model
        .complete(request(), call_context())
        .await
        .expect("the last protocol answers");
    let requests = server.requests();
    assert_eq!(requests.len(), 3, "{requests:#?}");
    assert_eq!(requests[1].path, "/v1/messages");
    assert_eq!(requests[2].path, "/v1/responses");
    let notice = radian_host::key_pool::take_notices()
        .pop()
        .expect("one notice");
    assert!(
        notice.contains("openai-compatible refused (HTTP 400)"),
        "{notice}"
    );
    assert!(notice.contains("openai-responses works"), "{notice}");
}

/// Edges 5 and 6 (review): a candidate that answers 429 or 500 is not a refusal, so the probe
/// stops there: no Responses request, this candidate's error (with everything tried) and no
/// rule written.
#[tokio::test]
async fn a_candidate_that_is_not_a_refusal_ends_the_probe() {
    let _guard = LOCK.lock().await;
    let _home = prepare();
    for status in [429, 500] {
        let server = MockServer::start(vec![
            Reply::status(400, "not chat"),
            Reply::status(status, "not a refusal"),
        ])
        .await;
        let label = format!("named-probe-candidate-{status}");
        write_list(&[(
            "m/candidate",
            vec![advertised("openai-compatible", "/v1/chat/completions")],
        )]);
        let (_dir, path, providers) = config(&label, &server.base_url, None, "");
        let before = std::fs::read_to_string(&path).unwrap();
        let (ctx, route) = context(providers, "m/candidate");
        let model = switchable(&ctx, &route, Some(&path), false);

        let error = model
            .complete(request(), call_context())
            .await
            .expect_err("a 429/500 ends the probe");
        let text = error.to_string();
        assert!(
            text.contains("tried openai-compatible (HTTP 400)"),
            "{status}: {text}"
        );
        assert!(
            text.contains(&format!("anthropic (HTTP {status})")),
            "{status}: {text}"
        );
        assert_eq!(
            server.requests().len(),
            2,
            "{status}: no Responses request: {:#?}",
            server.requests()
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "{status}: no rule written"
        );
        assert!(
            radian_host::key_pool::take_notices().is_empty(),
            "{status}: no probe notice"
        );
    }
}

/// The `Provider::Anthropic` reply must be converted into the chat mock's `Reply` for the main
/// scenario's single server; a small extension keeps the two mock shapes out of the assertions.
trait IntoCompatible {
    fn into_compatible(self) -> Reply;
}

impl IntoCompatible for AnthropicReply {
    fn into_compatible(self) -> Reply {
        match self {
            AnthropicReply::Sse {
                pieces,
                pause,
                headers,
            } => Reply::Sse {
                pieces,
                pause,
                headers,
            },
            AnthropicReply::SseThenHang { pieces } => Reply::SseThenHang { pieces },
            AnthropicReply::SseThenCut { pieces } => Reply::SseThenCut { pieces },
            AnthropicReply::Status {
                code,
                headers,
                body,
            } => Reply::Status {
                code,
                headers,
                body,
            },
        }
    }
}
