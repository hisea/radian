//! Structural test for the `opencode-go` built-in list (ADR 0026 D11): it must match the
//! vendored upstream `models.json` (`tests/data/opencode-go-models.json`; source, commit and
//! licence in `tests/data/README.md`). Updating the list means replacing the data file and
//! regenerating the const — a hand edit that drifts is caught here.

use radian_host::{Protocol, descriptors, supported_thinking_levels};

/// The seven levels in `THINKING_LEVELS` order.
const LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

const OPENCODE_GO_ANTHROPIC_BASE: &str = "https://opencode.ai/zen/go";
const OPENCODE_GO_OPENAI_BASE: &str = "https://opencode.ai/zen/go/v1";

fn upstream() -> Vec<serde_json::Value> {
    serde_json::from_str(include_str!("data/opencode-go-models.json")).expect("vendored JSON")
}

/// Every upstream model, id for id: name, protocol, base URL and the thinking map (the
/// non-null levels and their wire values, exactly as upstream writes them).
#[test]
fn the_opencode_go_builtins_match_upstream() {
    let upstream = upstream();
    let descriptor = descriptors()
        .iter()
        .find(|descriptor| descriptor.id == "opencode-go")
        .expect("the opencode-go descriptor");
    assert!(
        descriptor.models.len() >= upstream.len(),
        "the built-in list is shorter than upstream ({} < {})",
        descriptor.models.len(),
        upstream.len()
    );
    for model in &upstream {
        let id = model["id"].as_str().expect("id");
        let built = descriptor
            .model(id)
            .unwrap_or_else(|| panic!("{id}: missing from the built-in list"));
        assert_eq!(built.name, model["name"].as_str().unwrap(), "{id}: name");

        let anthropic = model["api"] == "anthropic-messages";
        let protocol = if anthropic {
            Protocol::Anthropic
        } else {
            Protocol::OpenAiCompatible
        };
        assert_eq!(built.protocol, protocol, "{id}: protocol");
        let base = if anthropic {
            OPENCODE_GO_ANTHROPIC_BASE
        } else {
            OPENCODE_GO_OPENAI_BASE
        };
        assert_eq!(built.base_url, base, "{id}: base_url");

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

/// `supported_thinking_levels` is the non-null levels of the upstream map, in level order.
#[test]
fn every_thinking_map_matches_supported_thinking_levels() {
    for model in upstream() {
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
    for model in upstream() {
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
