//! Structural test for the `opencode-go` built-in list: the thinking maps and names must match
//! the vendored upstream `models.json` (`tests/data/opencode-go-models.json`; source, commit and
//! licence in `tests/data/README.md`), and the ids/protocols must match the models.dev snapshot
//! of 2026-10-17 that the list was resynced to (ADR 0026). Updating the
//! list means replacing the data and regenerating the const — a hand edit that drifts is caught
//! here.

use radian_host::{Protocol, descriptor_base_url, descriptors, supported_thinking_levels};

/// The seven levels in `THINKING_LEVELS` order.
const LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The ids the 2026-10-17 models.dev snapshot no longer carries: dropped from the built-in list
/// even though upstream `cdd8a70` still has them (ADR 0026).
const DROPPED_PER_MODELS_DEV: &[&str] = &["deepseek-v4-flash", "grok-4.6"];

/// The ids models.dev marks `@ai-sdk/openai` on `opencode-go` (Responses), which upstream
/// `cdd8a70` still calls `openai-completions`. The catalog wins (ADR 0026
/// decision 1), so the table says Responses.
const RESPONSES_PER_MODELS_DEV: &[&str] = &[
    "grok-4.7",
    "gpt-5.6-luna",
    "gpt-6-luna",
    "muse-spark-1.2-contributor",
    "muse-spark-1.3-contributor",
];

const OPENCODE_GO_ANTHROPIC_BASE: &str = "https://opencode.ai/inference/go/anthropic";
const OPENCODE_GO_OPENAI_BASE: &str = "https://opencode.ai/inference/go/v1";

fn upstream() -> Vec<serde_json::Value> {
    serde_json::from_str(include_str!("data/opencode-go-models.json")).expect("vendored JSON")
}

/// The upstream rows the built-in list still carries (the dropped ids are asserted separately).
fn kept_upstream() -> Vec<serde_json::Value> {
    upstream()
        .into_iter()
        .filter(|model| !DROPPED_PER_MODELS_DEV.contains(&model["id"].as_str().expect("id")))
        .collect()
}

/// Every kept upstream model, id for id: name, protocol, base URL and the thinking map (the
/// non-null levels and their wire values, exactly as upstream writes them).
#[test]
fn the_opencode_go_builtins_match_upstream() {
    let descriptor = descriptors()
        .iter()
        .find(|descriptor| descriptor.id == "opencode-go")
        .expect("the opencode-go descriptor");
    for model in kept_upstream() {
        let id = model["id"].as_str().expect("id");
        let built = descriptor
            .model(id)
            .unwrap_or_else(|| panic!("{id}: missing from the built-in list"));
        assert_eq!(built.name, model["name"].as_str().unwrap(), "{id}: name");

        // The protocol is models.dev's, not upstream's `api`: the five `@ai-sdk/openai` models
        // are Responses.
        let protocol = if RESPONSES_PER_MODELS_DEV.contains(&id) {
            Protocol::Responses
        } else if model["api"] == "anthropic-messages" {
            Protocol::Anthropic
        } else {
            Protocol::OpenAiCompatible
        };
        assert_eq!(built.protocol, protocol, "{id}: protocol");
        let base = if protocol == Protocol::Anthropic {
            OPENCODE_GO_ANTHROPIC_BASE
        } else {
            OPENCODE_GO_OPENAI_BASE
        };
        assert_eq!(
            descriptor_base_url("opencode-go", protocol),
            Some(base),
            "{id}: base_url"
        );

        match model.get("thinkingLevelMap") {
            None => assert!(built.thinking.is_empty(), "{id}: must carry no map"),
            Some(map) => {
                let expected: Vec<(&str, Option<&str>)> = LEVELS
                    .iter()
                    .map(|level| (*level, map[*level].as_str()))
                    .collect();
                assert_eq!(built.thinking, &expected[..], "{id}: thinking map");
            }
        }
    }
}

/// The ids the 2026-10-17 snapshot dropped are absent from the table, and every other upstream
/// id is present: a row silently dropped or re-added fails here.
#[test]
fn the_models_dev_drops_and_additions_are_exact() {
    let descriptor = descriptors()
        .iter()
        .find(|descriptor| descriptor.id == "opencode-go")
        .expect("the opencode-go descriptor");
    for id in DROPPED_PER_MODELS_DEV {
        assert!(
            descriptor.model(id).is_none(),
            "{id} is not in the 2026-10-17 models.dev snapshot and must not be listed"
        );
    }
    for id in ["beam-free", "step-5-preview"] {
        assert!(
            descriptor.model(id).is_some(),
            "{id} is in the snapshot and must be listed"
        );
    }
    assert!(
        descriptor.model("gemini-4-argon-contributor").is_none(),
        "the unsupported `@ai-sdk/google` model must not be listed"
    );
}

/// `supported_thinking_levels` is the non-null levels of the upstream map, in level order.
#[test]
fn every_thinking_map_matches_supported_thinking_levels() {
    for model in kept_upstream() {
        let id = model["id"].as_str().expect("id");
        let expected: Vec<&str> = match model.get("thinkingLevelMap") {
            None => Vec::new(),
            Some(map) => LEVELS
                .iter()
                .copied()
                .filter(|level| !map[*level].is_null())
                .collect(),
        };
        assert_eq!(
            supported_thinking_levels("opencode-go", id),
            expected,
            "{id}: supported levels"
        );
    }
}

/// A model upstream carries no map for sends no `thinking` parameter: the provider-level map
/// is empty, so `supported_thinking_levels` is empty too.
#[test]
fn models_without_a_map_support_no_thinking_level() {
    for model in kept_upstream() {
        if model.get("thinkingLevelMap").is_some() {
            continue;
        }
        let id = model["id"].as_str().expect("id");
        assert_eq!(
            supported_thinking_levels("opencode-go", id),
            Vec::<&str>::new(),
            "{id}"
        );
    }
}
