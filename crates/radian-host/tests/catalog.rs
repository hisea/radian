//! The model catalog against a local mock HTTP server (ADR 0018 §目录). These tests never
//! touch the real network: the URL points at the adapter's in-process mock, or — for the one
//! test that must see the client hang up — at a local listener that stalls.

use std::path::{Path, PathBuf};
mod common;

use std::time::{Duration, Instant};

use agent_provider_openai_compatible::testing::{MockServer, Reply};
use radian_host::{
    Catalog, FETCH_TIMEOUT, ModelLimit, RouteOutput, WindowQuery, WindowSource, resolve,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

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

fn scratch(label: &str) -> common::TempDir {
    common::TempDir::named(&format!("radian-host-catalog-{label}"))
}

fn query(cache: &Path, server: &MockServer, model: &str) -> WindowQuery {
    query_at(cache, &server.base_url, model)
}

fn query_at(cache: &Path, base_url: &str, model: &str) -> WindowQuery {
    WindowQuery {
        explicit: None,
        catalog_provider: Some("opencode".into()),
        base_url: "https://opencode.ai/zen/go/v1".into(),
        model: model.into(),
        // The route carries no output field in these resolver tests (ADR 0018 §2); a chat
        // route's cap is exercised by `a_chat_route_reserves_its_catalog_max_tokens`.
        request_output: RouteOutput::None,
        offline: false,
        cache_path: cache.join("catalog/models.json"),
        url: format!("{base_url}/api.json"),
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

/// ADR 0018 §2: a chat route sends `max_tokens = min(limit.output, ⌊C/4⌋)`, and that sent
/// value is what `R` and `B` count.
#[tokio::test]
async fn a_chat_route_reserves_its_catalog_max_tokens() {
    let cache = scratch("chat-cap");
    let server = MockServer::start(vec![Reply::status(200, API_JSON)]).await;
    let mut q = query(&cache, &server, "deepseek-v4.1-flash");
    q.request_output = RouteOutput::CatalogChat;
    let window = resolve(&q).await;
    assert_eq!(
        window.request_output,
        Some(250_000),
        "min(384000, 1000000 / 4)"
    );
    assert_eq!(window.reservation, 250_000);
    assert_eq!(
        window.budget,
        Some(750_000),
        "min(922000, 1000000 - 250000)"
    );
    // The output ceiling did not raise R on its own: an unknown output or a request that
    // sends nothing keeps the safety quarter.
    let mut plain = query(&cache, &server, "deepseek-v4.1-flash");
    plain.request_output = RouteOutput::None;
    assert_eq!(resolve(&plain).await.budget, Some(922_000));
    let _ = std::fs::remove_dir_all(&cache);
}

/// Acceptance 1: no cache → fetch → write cache → right window; the second run does not fetch.
#[tokio::test]
async fn a_missing_cache_fetches_once_and_then_uses_the_cache() {
    let cache = scratch("fetch");
    let server = MockServer::start(vec![Reply::status(200, API_JSON)]).await;

    let window = resolve(&query(&cache, &server, "deepseek-v4.1-flash")).await;
    // B = min(I, C - R) = min(922000, 1000000 - 16384) = 922000.
    assert_eq!(window.budget, Some(922_000));
    assert_eq!(window.context, Some(1_000_000));
    assert_eq!(window.input, Some(922_000));
    assert_eq!(window.reservation, 16_384);
    assert!(matches!(window.source, WindowSource::Catalog { .. }));
    assert!(cache_file(&cache).exists(), "the cache must be written");
    assert_eq!(server.requests().len(), 1);

    let again = resolve(&query(&cache, &server, "deepseek-v4.1-flash")).await;
    assert_eq!(again.budget, Some(922_000));
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
    // context-only: B = C - R = 128000 - 16384.
    assert_eq!(window.budget, Some(111_616), "the stale cache is used");
    assert_eq!(server.requests().len(), 1, "it did try to refresh");
    let _ = std::fs::remove_dir_all(&cache);
}

/// A local server whose catalog response never ends: it reads the GET, writes the status line
/// and the SSE headers, and then sends nothing, like a stalled catalog. It records when the
/// request arrived and when the client hung up, so the test can time the **fetch** alone — the
/// arc between those two moments — instead of `resolve` plus everything around it (the reqwest
/// client build, the cache read, the test process's own start-up and teardown).
///
/// The adapter's `MockServer` cannot report the hang-up: `Reply::SseThenHang` writes its head
/// and parks in `std::future::pending()` without looking at the socket again. Teaching the
/// shared fixture to watch for EOF would change every provider suite that uses it, so this one
/// test keeps its own tiny server.
struct HangingCatalog {
    /// `http://127.0.0.1:<port>/v1`, the same shape as `MockServer::base_url`.
    base_url: String,
    /// Fires when the client closes the connection, with `(arrived, closed)`.
    closed: oneshot::Receiver<(Instant, Instant)>,
}

async fn hanging_catalog() -> HangingCatalog {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("address").port();
    let (tx, closed) = oneshot::channel();
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        // The GET has arrived once its head is complete.
        let mut buffer = Vec::new();
        let mut scratch = [0u8; 4096];
        loop {
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            match stream.read(&mut scratch).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buffer.extend_from_slice(&scratch[..n]),
            }
        }
        let arrived = Instant::now();
        // A complete head; the chunked body never ends and no chunk is ever sent.
        let _ = stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                  transfer-encoding: chunked\r\nconnection: close\r\n\r\n",
            )
            .await;
        // Silence. The client gives up at its own timeout and closes the socket; that EOF is
        // when the fetch ended.
        let mut sink = [0u8; 1024];
        loop {
            match stream.read(&mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let _ = tx.send((arrived, Instant::now()));
    });
    HangingCatalog {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        closed,
    }
}

/// Acceptance 2 (the timeout half): a hanging fetch times out and falls back to the cache.
///
/// The assertion is on the fetch's own duration, not on `resolve`'s: the timeout it must prove
/// is the 3 s one in `fetch`, and a slow machine can spend seconds before the request or after
/// it without the timeout ever being late.
#[tokio::test]
async fn a_hanging_fetch_times_out_and_the_stale_cache_is_used() {
    let cache = scratch("stale-timeout");
    let server = hanging_catalog().await;
    std::fs::create_dir_all(cache.join("catalog")).unwrap();
    std::fs::write(cache_file(&cache), API_JSON).unwrap();
    age_file(&cache_file(&cache), Duration::from_secs(25 * 60 * 60));

    // A backstop only: without it a fetch that is never cut off would hang the test instead of
    // failing it. It is far above the 3 s timeout this test is about.
    let window = tokio::time::timeout(
        Duration::from_secs(30),
        resolve(&query_at(&cache, &server.base_url, "context-only")),
    )
    .await
    .expect("resolve must return: the fetch was not cut off by its timeout");
    assert_eq!(window.budget, Some(111_616), "the stale cache is used");

    let (arrived, closed) = tokio::time::timeout(Duration::from_secs(30), server.closed)
        .await
        .expect("the client must hang up on the hanging server")
        .expect("the server records its two timestamps");
    let fetch = closed.duration_since(arrived);
    // It really did hang for about the timeout — an immediate disconnect would prove nothing.
    let least = FETCH_TIMEOUT.saturating_sub(Duration::from_secs(1));
    assert!(
        fetch >= least,
        "the fetch gave up before its {FETCH_TIMEOUT:?} timeout: {fetch:?}"
    );
    // Absolute, not relative to `FETCH_TIMEOUT`: raising the timeout well past it must still
    // fail this test. 3 s is observed; the rest is headroom for a busy machine.
    assert!(
        fetch < Duration::from_secs(8),
        "the {FETCH_TIMEOUT:?} timeout must cut the fetch: {fetch:?}"
    );
    let _ = std::fs::remove_dir_all(&cache);
}

/// Acceptance 2 (ADR 0026): no cache and a failed fetch falls back to the
/// **bundled snapshot**, not to "unknown". The request was still attempted (and failed).
#[tokio::test]
async fn no_cache_and_a_failed_fetch_uses_the_bundled_snapshot() {
    let cache = scratch("no-cache");
    let server = MockServer::start(vec![Reply::status(500, "boom")]).await;
    let window = resolve(&query(&cache, &server, "deepseek-v4.1-flash")).await;
    assert_eq!(
        window.source,
        WindowSource::Bundled {
            provider: "opencode".into(),
            model: "deepseek-v4.1-flash".into(),
            date: radian_host::SNAPSHOT_DATE,
        }
    );
    // The snapshot's `opencode/deepseek-v4.1-flash`: C = 1000000, no input ceiling, R = 16384.
    assert_eq!(window.context, Some(1_000_000));
    assert_eq!(window.budget, Some(983_616));
    assert_eq!(server.requests().len(), 1, "it did try to fetch");
    assert!(
        window
            .describe()
            .contains("from bundled snapshot (2026-10-17)"),
        "{}",
        window.describe()
    );
    let _ = std::fs::remove_dir_all(&cache);
}

/// A provider or model the bundled snapshot does not carry is still unknown: nothing is guessed.
#[tokio::test]
async fn a_model_the_snapshot_does_not_carry_is_still_unknown() {
    let cache = scratch("absent");
    let server = MockServer::start(vec![Reply::status(500, "boom")]).await;
    let mut q = query(&cache, &server, "no-such-model");
    q.catalog_provider = Some("no-such-provider".into());
    let window = resolve(&q).await;
    assert_eq!(window.source, WindowSource::Unknown);
    assert_eq!(window.budget, None);
    assert!(window.error.is_none(), "unknown is not an error");
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
    assert_eq!(window.budget, Some(922_000));
    assert_eq!(server.requests().len(), 0, "offline must not go out");

    // Offline with no cache uses the bundled snapshot, still without a request (ADR 0026
    // ).
    let empty = scratch("offline-nocache");
    let mut q = query(&empty, &server, "deepseek-v4.1-flash");
    q.offline = true;
    let window = resolve(&q).await;
    assert!(
        matches!(window.source, WindowSource::Bundled { .. }),
        "{:?}",
        window.source
    );
    assert_eq!(window.budget, Some(983_616));
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
    q.explicit = Some(100_000);
    let window = resolve(&q).await;
    // --context-window overrides C; B = C - R = 100000 - 16384.
    assert_eq!(window.budget, Some(83_616));
    assert_eq!(window.context, Some(100_000));
    assert_eq!(window.source, WindowSource::Explicit);
    assert!(
        window.describe().contains("from --context-window"),
        "{}",
        window.describe()
    );
    assert_eq!(server.requests().len(), 0);

    // C <= R needs the request's own output to reach C (ADR 0018): an Anthropic
    // request with max_tokens = 16384 against an 8192 window.
    q.explicit = Some(8_192);
    q.request_output = RouteOutput::Fixed(16_384);
    let window = resolve(&q).await;
    assert_eq!(window.budget, None);
    assert!(window.error.is_some(), "C <= R must be an error");
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
    assert_eq!(window.budget, Some(922_000));

    // The mapping itself: api.openai.com resolves `openai` without a flag.
    let mut q = query(&cache, &server, "gpt-5.4");
    q.base_url = "https://api.openai.com/v1".into();
    q.catalog_provider = None;
    let window = resolve(&q).await;
    assert_eq!(window.budget, Some(900_000));
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
    assert_eq!(window.budget, None, "context 0 is unknown");
    assert!(
        window.describe().contains("window unknown"),
        "{}",
        window.describe()
    );

    // Corrupt the cache; the next resolve must fetch again and succeed.
    std::fs::write(cache_file(&cache), "{ not json").unwrap();
    let window = resolve(&query(&cache, &server, "deepseek-v4.1-flash")).await;
    assert_eq!(window.budget, Some(922_000));
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
    // ADR 0018: B = min(I, C - R). `context-only` has no I, so B = 128000 - 16384.
    assert_eq!(
        radian_host::input_budget(Some(128_000), None, None),
        Ok(Some(111_616))
    );
    assert_eq!(
        radian_host::input_budget(Some(0), None, None),
        Ok(None),
        "a zero context is unknown"
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
    q.explicit = Some(100_000);

    let window = resolve(&q).await;
    assert_eq!(window.budget, Some(83_616));
    assert_eq!(window.source, WindowSource::Explicit);
    assert_eq!(server.requests().len(), 0, "no cache: no request at all");

    // With a cache present, the input/output come from it — still with no request.
    std::fs::create_dir_all(cache.join("catalog")).unwrap();
    std::fs::write(cache_file(&cache), API_JSON).unwrap();
    let window = resolve(&q).await;
    assert_eq!(
        server.requests().len(),
        0,
        "a cache present: still no request"
    );
    // The cached I is read; B = min(922000, 100000 - 16384) = 83616.
    assert_eq!(window.input, Some(922_000));
    assert_eq!(window.budget, Some(83_616));
    assert_eq!(window.output, Some(384_000));
    let _ = std::fs::remove_dir_all(&cache);
}
