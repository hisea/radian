//! The model catalog against a local mock HTTP server (ADR 0018 §目录). These tests never
//! touch the real network: the URL points at the adapter's in-process mock.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_provider_openai_compatible::testing::{MockServer, Reply};
use radian_host::{Catalog, ModelLimit, WindowQuery, WindowSource, resolve};

/// A small `api.json` in the real shape: `opencode` with a model that has an `input` limit and
/// one whose `context` is `0`.
const API_JSON: &str = r#"{
  "opencode": {
    "id": "opencode",
    "env": ["OPENCODE_API_KEY"],
    "npm": "@opencode/ai",
    "models": {
      "deepseek-v4.1-flash": {
        "id": "deepseek-v4.1-flash",
        "limit": {"context": 1000000, "output": 384000, "input": 922000}
      },
      "context-only": {"id": "context-only", "limit": {"context": 128000}},
      "zero-context": {"id": "zero-context", "limit": {"context": 0, "output": 100}}
    }
  },
  "openai": {
    "id": "openai",
    "models": {
      "gpt-5.4": {"id": "gpt-5.4", "limit": {"context": 1050000, "input": 900000, "output": 128000}}
    }
  }
}"#;

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "radian-host-catalog-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn query(cache: &Path, server: &MockServer, model: &str) -> WindowQuery {
    WindowQuery {
        explicit: None,
        catalog_provider: Some("opencode".into()),
        base_url: "https://opencode.ai/zen/go/v1".into(),
        model: model.into(),
        offline: false,
        cache_path: cache.join("catalog/models.json"),
        url: format!("{}/api.json", server.base_url),
    }
}

fn age_file(path: &Path, age: Duration) {
    let when = std::time::SystemTime::now() - age;
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

fn cache_file(cache: &Path) -> PathBuf {
    cache.join("catalog/models.json")
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Acceptance 1: no cache → fetch → write cache → right window; the second run does not fetch.
#[tokio::test]
async fn a_missing_cache_fetches_once_and_then_uses_the_cache() {
    let cache = scratch("fetch");
    let server = MockServer::start(vec![Reply::status(200, API_JSON)]).await;

    let window = resolve(&query(&cache, &server, "deepseek-v4.1-flash")).await;
    assert_eq!(
        window.tokens,
        Some(922_000),
        "min(I, C) = min(922000, 1000000)"
    );
    assert!(matches!(window.source, WindowSource::Catalog { .. }));
    assert!(cache_file(&cache).exists(), "the cache must be written");
    assert_eq!(server.requests().len(), 1);

    let again = resolve(&query(&cache, &server, "deepseek-v4.1-flash")).await;
    assert_eq!(again.tokens, Some(922_000));
    assert_eq!(
        server.requests().len(),
        1,
        "a cache younger than 24 h must not fetch again"
    );

    #[cfg(unix)]
    {
        assert_eq!(
            mode(&cache_file(&cache)),
            0o600,
            "the cache file must be 0600"
        );
        assert_eq!(
            mode(&cache.join("catalog")),
            0o700,
            "the catalog directory must be 0700"
        );
    }
    let _ = std::fs::remove_dir_all(&cache);
}

/// Acceptance 2: a stale cache plus a failed fetch falls back to the cache.
#[tokio::test]
async fn a_stale_cache_survives_a_500() {
    let cache = scratch("stale-500");
    let server = MockServer::start(vec![Reply::status(500, "boom")]).await;
    std::fs::create_dir_all(cache.join("catalog")).unwrap();
    std::fs::write(cache_file(&cache), API_JSON).unwrap();
    age_file(&cache_file(&cache), Duration::from_secs(25 * 60 * 60));

    let window = resolve(&query(&cache, &server, "context-only")).await;
    assert_eq!(window.tokens, Some(128_000), "the stale cache is used");
    assert_eq!(server.requests().len(), 1, "it did try to refresh");
    let _ = std::fs::remove_dir_all(&cache);
}

/// Acceptance 2 (the timeout half): a hanging fetch times out and falls back to the cache.
#[tokio::test]
async fn a_hanging_fetch_times_out_and_the_stale_cache_is_used() {
    let cache = scratch("stale-timeout");
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: vec![] }]).await;
    std::fs::create_dir_all(cache.join("catalog")).unwrap();
    std::fs::write(cache_file(&cache), API_JSON).unwrap();
    age_file(&cache_file(&cache), Duration::from_secs(25 * 60 * 60));

    let started = std::time::Instant::now();
    let window = resolve(&query(&cache, &server, "context-only")).await;
    assert_eq!(window.tokens, Some(128_000));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the 3 s timeout must bound the fetch"
    );
    let _ = std::fs::remove_dir_all(&cache);
}

/// Acceptance 2: no cache and a failed fetch is "unknown", not an error.
#[tokio::test]
async fn no_cache_and_a_failed_fetch_is_unknown() {
    let cache = scratch("no-cache");
    let server = MockServer::start(vec![Reply::status(500, "boom")]).await;
    let window = resolve(&query(&cache, &server, "deepseek-v4.1-flash")).await;
    assert_eq!(window.tokens, None);
    assert_eq!(window.source, WindowSource::Unknown);
    assert_eq!(window.describe(), "unknown: context guard off");
    let _ = std::fs::remove_dir_all(&cache);
}

/// Acceptance 3: `--offline-catalog` never fetches, even with a stale cache.
#[tokio::test]
async fn offline_never_fetches() {
    let cache = scratch("offline");
    let server = MockServer::start(vec![Reply::status(200, API_JSON)]).await;
    std::fs::create_dir_all(cache.join("catalog")).unwrap();
    std::fs::write(cache_file(&cache), API_JSON).unwrap();
    age_file(&cache_file(&cache), Duration::from_secs(25 * 60 * 60));

    let mut q = query(&cache, &server, "deepseek-v4.1-flash");
    q.offline = true;
    let window = resolve(&q).await;
    assert_eq!(window.tokens, Some(922_000));
    assert_eq!(server.requests().len(), 0, "offline must not go out");

    // Offline with no cache is unknown, still without a request.
    let empty = scratch("offline-nocache");
    let mut q = query(&empty, &server, "deepseek-v4.1-flash");
    q.offline = true;
    let window = resolve(&q).await;
    assert_eq!(window.source, WindowSource::Unknown);
    assert_eq!(server.requests().len(), 0);
    let _ = std::fs::remove_dir_all(&cache);
    let _ = std::fs::remove_dir_all(&empty);
}

/// Acceptance 3: `--context-window` wins over the catalog and never fetches.
#[tokio::test]
async fn an_explicit_window_wins_and_does_not_fetch() {
    let cache = scratch("explicit");
    let server = MockServer::start(vec![Reply::status(200, API_JSON)]).await;
    let mut q = query(&cache, &server, "deepseek-v4.1-flash");
    q.explicit = Some(4096);
    let window = resolve(&q).await;
    assert_eq!(window.tokens, Some(4096));
    assert_eq!(window.source, WindowSource::Explicit);
    assert_eq!(window.describe(), "window 4096 from --context-window");
    assert_eq!(server.requests().len(), 0);
    let _ = std::fs::remove_dir_all(&cache);
}

/// Acceptance 4: `--catalog-provider` overrides the hostname mapping; a host that maps to
/// nothing still works when the provider is named.
#[tokio::test]
async fn catalog_provider_overrides_the_hostname() {
    let cache = scratch("provider");
    let server = MockServer::start(vec![Reply::status(200, API_JSON)]).await;
    let mut q = query(&cache, &server, "deepseek-v4.1-flash");
    q.base_url = "https://some.mirror.example/v1".into();
    q.catalog_provider = Some("opencode".into());
    let window = resolve(&q).await;
    assert_eq!(window.tokens, Some(922_000));

    // The mapping itself: api.openai.com resolves `openai` without a flag.
    let mut q = query(&cache, &server, "gpt-5.4");
    q.base_url = "https://api.openai.com/v1".into();
    q.catalog_provider = None;
    let window = resolve(&q).await;
    assert_eq!(window.tokens, Some(900_000));
    let _ = std::fs::remove_dir_all(&cache);
}

/// Acceptance 4: a `context` of `0` is "unknown"; a corrupt cache is treated as no cache.
#[tokio::test]
async fn zero_context_is_unknown_and_a_corrupt_cache_is_ignored() {
    let cache = scratch("zero-corrupt");
    let server = MockServer::start(vec![
        Reply::status(200, API_JSON),
        Reply::status(200, API_JSON),
    ])
    .await;

    let window = resolve(&query(&cache, &server, "zero-context")).await;
    assert_eq!(window.source, WindowSource::Unknown, "context 0 is unknown");

    // Corrupt the cache; the next resolve must fetch again and succeed.
    std::fs::write(cache_file(&cache), "{ not json").unwrap();
    let window = resolve(&query(&cache, &server, "deepseek-v4.1-flash")).await;
    assert_eq!(window.tokens, Some(922_000));
    assert_eq!(server.requests().len(), 2);
    let _ = std::fs::remove_dir_all(&cache);
}

/// Acceptance 5: the fetch is a plain GET with no key and no custom header.
#[tokio::test]
async fn the_catalog_request_carries_no_key_or_header() {
    let cache = scratch("headers");
    let server = MockServer::start(vec![Reply::status(200, API_JSON)]).await;
    resolve(&query(&cache, &server, "deepseek-v4.1-flash")).await;

    let request = &server.requests()[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/v1/api.json");
    for name in ["authorization", "x-api-key", "cookie", "anthropic-version"] {
        assert!(
            request.header(name).is_none(),
            "the catalog request must not send `{name}`"
        );
    }
    // No header (or the body) may contain anything that looks like the host's key.
    let joined = request
        .headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!joined.contains("sk-"), "{joined}");
    assert!(request.body.is_empty(), "a GET must not carry a body");
    let _ = std::fs::remove_dir_all(&cache);
}

/// The parser keeps only what the guard needs, and the accessor prefers `input`.
#[test]
fn the_parsed_catalog_reports_the_real_shape() {
    let catalog = Catalog::parse(API_JSON).unwrap();
    assert_eq!(catalog.provider_count(), 2);
    assert_eq!(
        catalog.limit("opencode", "deepseek-v4.1-flash"),
        Some(ModelLimit {
            context: Some(1_000_000),
            output: Some(384_000),
            input: Some(922_000)
        })
    );
    assert_eq!(
        catalog.limit("opencode", "context-only").unwrap().input,
        None
    );
    assert_eq!(
        catalog
            .limit("opencode", "context-only")
            .unwrap()
            .window_tokens(),
        Some(128_000)
    );
    assert_eq!(
        catalog
            .limit("opencode", "zero-context")
            .unwrap()
            .window_tokens(),
        None
    );
}

/// ADR 0018: an explicit `--context-window` never fetches the catalog. It reads a
/// cache that already exists for the `input`/`output` numbers, and with no cache it uses only
/// the explicit window — in both cases with zero requests.
#[tokio::test]
async fn an_explicit_window_never_fetches() {
    let cache = scratch("explicit-offline");
    let server = MockServer::start(vec![Reply::status(200, API_JSON)]).await;
    let mut q = query(&cache, &server, "deepseek-v4.1-flash");
    q.explicit = Some(5000);

    let window = resolve(&q).await;
    assert_eq!(window.tokens, Some(5000));
    assert_eq!(window.source, WindowSource::Explicit);
    assert_eq!(server.requests().len(), 0, "no cache: no request at all");
    assert_eq!(window.describe(), "window 5000 from --context-window");

    // With a cache present, the input/output come from it — still with no request.
    std::fs::create_dir_all(cache.join("catalog")).unwrap();
    std::fs::write(cache_file(&cache), API_JSON).unwrap();
    let window = resolve(&q).await;
    assert_eq!(
        server.requests().len(),
        0,
        "a cache present: still no request"
    );
    assert_eq!(window.limit.unwrap().input, Some(922_000));
    assert!(
        window
            .describe()
            .contains("(cached input 922000, output 384000)"),
        "{}",
        window.describe()
    );
    let _ = std::fs::remove_dir_all(&cache);
}
