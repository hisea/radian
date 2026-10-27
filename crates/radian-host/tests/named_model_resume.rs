//! ADR 0026 D10: a resumed session rebuilds the **same** route. The journal stores only the
//! provider and model (D8); the protocol and endpoint resolve again from the named provider's
//! cached model list and its rules, exactly as at startup. A regression here would resume on the
//! declared `kind` instead of the model's own protocol or endpoint.
//!
//! `RADIAN_HOME` is process-wide, so this binary deliberately holds exactly one test (the same
//! rule `tests/named_model_routing.rs` follows).

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use radian_host::{
    AdvertisedEndpoint, FetchedProviderModels, ModelRoute, ModelRuleDecl, Provider, ProviderDecl,
    RouteContext, describe_model_route, read_provider_models, resolve_provider_with_models,
    resumed_model_route, write_provider_model_list,
};
use radian_session::{RuntimeEvent, SessionCreatedData, SessionJournal};
use serde_json::json;

/// A private `RADIAN_HOME` with one usable `magpie` credential.
fn isolated_home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = common::process_dir("radian-named-model-resume");
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

fn providers() -> BTreeMap<String, ProviderDecl> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "magpie".to_owned(),
        ProviderDecl {
            kind: "openai-compatible".to_owned(),
            base_url: "http://127.0.0.1:3425/v1".to_owned(),
            models: vec![ModelRuleDecl {
                pattern: "claude/*".to_owned(),
                kind: "anthropic".to_owned(),
            }],
            ..Default::default()
        },
    );
    providers
}

fn context() -> RouteContext {
    RouteContext {
        extra_body: Default::default(),
        providers: providers(),
        session_id: "named-model-resume".to_owned(),
        user_headers: Vec::new(),
        include_usage: None,
        max_tokens: 4096,
        thinking: "medium".to_owned(),
        thinking_budget: None,
    }
}

/// Writes a one-route session journal: a resume reads exactly this (ADR 0026 D8).
async fn session_with_route(dir: &std::path::Path, id: &str, model: &str) {
    let journal = SessionJournal::create(
        dir.join(format!("{id}.jsonl")),
        id,
        SessionCreatedData::new("/work/example", json!({})),
    )
    .unwrap();
    journal
        .append_runtime_event(RuntimeEvent::ConfigChanged {
            revision: 1,
            change: ModelRoute::new("magpie", model).to_value(),
        })
        .await
        .unwrap();
    drop(journal);
}

#[tokio::test]
async fn a_resume_rebuilds_the_same_named_route() {
    let _home = isolated_home();
    // The provider's model list, as its own `GET <base>/models` advertised it. The cache lives
    // under the isolated home, so a resume reads it exactly like a startup does.
    let list = FetchedProviderModels {
        models: vec![
            "codex/gpt-5.6-sol".to_owned(),
            "anthropic:claude-haiku-4-5".to_owned(),
        ],
        endpoints: BTreeMap::from([
            (
                "codex/gpt-5.6-sol".to_owned(),
                vec![advertised("openai-responses", "/v1/responses")],
            ),
            (
                "anthropic:claude-haiku-4-5".to_owned(),
                vec![advertised("anthropic", "/apis/anthropic/v1/messages")],
            ),
        ]),
    };
    write_provider_model_list("magpie", "sk-magpie", &list).unwrap();

    // The line a resume prints resolves through the same function startup uses: the rule beats
    // the model list and the declared `kind`.
    let cached = read_provider_models("magpie", "sk-magpie").expect("the cached model list");
    let (provider_ref, _warning) =
        resolve_provider_with_models("magpie", &providers(), None, Some(cached))
            .expect("the named provider resolves");
    assert_eq!(
        describe_model_route(&provider_ref, "claude/opus-5-5").as_deref(),
        Some("magpie/claude/opus-5-5 (anthropic, rule claude/*)")
    );
    assert_eq!(
        describe_model_route(&provider_ref, "codex/gpt-5.6-sol").as_deref(),
        Some("magpie/codex/gpt-5.6-sol (openai-responses, model list)")
    );

    // The journal keeps only the provider and the model (D8); the protocol, endpoint and source
    // resolve again. One session per resolution step a route can come from.
    let dir = common::TempDir::canonical("radian-named-model-resume-session");
    for (id, model, provider, base_url) in [
        (
            "sid-list",
            "codex/gpt-5.6-sol",
            Provider::ChatGpt,
            "http://127.0.0.1:3425/v1",
        ),
        (
            "sid-rule",
            "claude/opus-5-5",
            Provider::Anthropic,
            // The rule names only a protocol; the `/v1` chat base is stripped and the adapter
            // adds `/v1/messages`.
            "http://127.0.0.1:3425",
        ),
    ] {
        session_with_route(dir.path(), id, model).await;
        let resumed = resumed_model_route(dir.path(), id).expect("the journal's route");
        assert_eq!(resumed.provider, "magpie");
        assert_eq!(resumed.model, model);
        let args = context()
            .args_for(&resumed.provider, &resumed.model, None)
            .expect("the resumed route resolves");
        assert_eq!(args.provider, provider, "{id}: the rebuilt adapter");
        assert_eq!(args.base_url, base_url, "{id}: the rebuilt endpoint");
        if id == "sid-list" {
            assert!(!args.chatgpt_oauth, "an API key, not the OAuth route");
        }
    }
}
