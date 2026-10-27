//! The `chatgpt` route's window (ADR 0018): models.dev has no `chatgpt` provider, so the
//! numbers come from the descriptor table, which cites them from
//! `@oh-my-pi/pi-catalog/src/models.json`'s `openai-codex` block (see `provider::CODEX_MODELS`).
//! Without this a chatgpt session ran with no window at all — the status bar said
//! "context budget off (no window configured)", and neither auto compaction nor the budget
//! guard could work (ADR 0015, ADR 0018).
//!
//! No test here touches the network: the catalog URL points at a closed local port, which is
//! also the real-world shape of a chatgpt route (no provider mapping *and* no catalog entry).

mod common;

use radian_host::{RouteOutput, WindowQuery, WindowSource, resolve, resolve_offline};

/// The descriptor's endpoint (`provider::CHATGPT_BASE`).
const CHATGPT_BASE: &str = "https://chatgpt.com/backend-api/codex";

fn scratch(label: &str) -> common::TempDir {
    common::TempDir::named(&format!("radian-host-chatgpt-{label}"))
}

/// A chatgpt query with a cache path that does not exist and a URL nothing listens on.
fn query(cache: &std::path::Path, model: &str) -> WindowQuery {
    WindowQuery {
        explicit: None,
        catalog_provider: Some("chatgpt".into()),
        base_url: CHATGPT_BASE.into(),
        model: model.into(),
        // The Responses adapter sends no max-output field by default, so `R` is the safety
        // reservation alone.
        request_output: RouteOutput::None,
        offline: false,
        cache_path: cache.join("catalog/models.json"),
        url: "http://127.0.0.1:1/api.json".into(),
    }
}

/// Every entry of the built-in list resolves the numbers the descriptor cites: the window `C`,
/// the output ceiling, and a budget that is not `None` (so the host's startup line is not the
/// "no window configured" one).
#[tokio::test]
async fn every_chatgpt_model_resolves_the_window_its_table_cites() {
    let cache = scratch("window");
    // id → (context window, maxTokens), from pi-catalog's `openai-codex` block.
    let expected: [(&str, u64, u64); 8] = [
        ("gpt-5.3-codex-spark", 128_000, 128_000),
        ("gpt-5.4", 272_000, 128_000),
        ("gpt-5.4-mini", 272_000, 128_000),
        ("gpt-5.5", 272_000, 128_000),
        ("gpt-5.6-luna", 1_000_000, 128_000),
        ("gpt-5.6-sol", 1_000_000, 128_000),
        ("gpt-5.6-terra", 1_000_000, 128_000),
        ("gpt-daybreak-blue-latest", 272_000, 128_000),
    ];
    for (model, context, output) in expected {
        let window = resolve(&query(&cache, model)).await;
        assert_eq!(
            window.source,
            WindowSource::Descriptor {
                provider: "chatgpt".into(),
                model: model.into()
            },
            "{model}: the window must come from the descriptor, not the catalog"
        );
        assert_eq!(window.context, Some(context), "{model}: context window");
        assert_eq!(window.output, Some(output), "{model}: output ceiling");
        assert_eq!(window.input, None, "{model}: no input ceiling upstream");
        assert!(
            window.budget.is_some(),
            "{model}: a chatgpt route must not run with the budget off"
        );
        assert_eq!(
            window.startup_check(),
            radian_host::WindowCheck::Ok,
            "{model}"
        );
        // No price: models.dev is the price source and has no `chatgpt` provider.
        assert!(window.price.is_none(), "{model}");
    }
    // The end-to-end arithmetic for one model (R = min(16384, C/4) = 16384, B = C − R).
    let window = resolve(&query(&cache, "gpt-5.6-sol")).await;
    assert_eq!(window.reservation, 16_384);
    assert_eq!(window.budget, Some(983_616));
    assert!(
        window.describe().contains("from the chatgpt descriptor"),
        "{}",
        window.describe()
    );
    let _ = std::fs::remove_dir_all(&cache);
}

/// An id the table does not list has no window: nothing is guessed from the name.
#[tokio::test]
async fn a_model_outside_the_table_has_no_window() {
    let cache = scratch("unknown");
    let window = resolve(&query(&cache, "gpt-9-not-listed")).await;
    assert_eq!(window.source, WindowSource::Unknown);
    assert_eq!(window.budget, None);
    assert_eq!(window.context, None);
    assert!(
        window.describe().contains("window unknown"),
        "{}",
        window.describe()
    );
    let _ = std::fs::remove_dir_all(&cache);
}

/// A `/model` switch resolves offline (ADR 0026 D7) and reaches the same table: a chatgpt
/// route has no cache entry to miss.
#[tokio::test]
async fn an_offline_switch_to_a_chatgpt_model_resolves_the_table() {
    let cache = scratch("switch");
    let window = resolve_offline(&query(&cache, "gpt-5.6-terra"));
    assert_eq!(window.context, Some(1_000_000));
    assert_eq!(window.budget, Some(983_616));
    assert_eq!(
        window.source,
        WindowSource::Descriptor {
            provider: "chatgpt".into(),
            model: "gpt-5.6-terra".into()
        }
    );
    // And a model the table does not list stays unknown offline too.
    let unknown = resolve_offline(&query(&cache, "gpt-9-not-listed"));
    assert_eq!(unknown.budget, None);
    let _ = std::fs::remove_dir_all(&cache);
}

/// The table answers only for the provider it has numbers for (`chatgpt`): a descriptor whose
/// window comes from models.dev (`anthropic`) is answered by the catalog — with no cache and no
/// network, the bundled snapshot (ADR 0026) — never by the table.
#[tokio::test]
async fn a_descriptor_without_numbers_is_answered_by_the_catalog_not_the_table() {
    let cache = scratch("no-numbers");
    let mut query = query(&cache, "claude-opus-5-5");
    query.catalog_provider = Some("anthropic".into());
    query.base_url = "https://api.anthropic.com".into();
    let window = resolve(&query).await;
    assert!(
        matches!(window.source, WindowSource::Bundled { .. }),
        "{:?}",
        window.source
    );
    assert!(
        window.budget.is_some(),
        "the bundled snapshot carries the anthropic window"
    );
    let _ = std::fs::remove_dir_all(&cache);
}
