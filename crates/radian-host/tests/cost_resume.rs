//! F3 (ADR 0027): a resumed session folds its journal's usage records back into
//! the projection, so the footer's tokens and fee show what the session already spent instead
//! of starting from zero.
//!
//! The projection's only other writer is the live event sink, so before this a `--resume`
//! showed `-` for the tokens and hid `cost` entirely until the next response arrived.
//!
//! The fold prices **each call at the route in force when it ran** (`/model` switches included,
//! ADR 0026 D5), and counts the summarizer's calls too (ADR 0018), matching what a
//! live run would have accumulated.

use std::sync::Arc;

use radian_core::{JournalRecord, Message, TranscriptJournal};
use radian_host::{SessionProjection, cost};
use radian_session::{RuntimeEvent, Session, SessionCreatedData, SessionJournal};
use serde_json::json;

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("radian-host-cost-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The catalog prices the tests use, as dollars per million tokens (`ModelPrice` is
/// micro-dollars per million tokens).
fn cheap() -> radian_host::ModelPrice {
    radian_host::ModelPrice {
        input: 150_000,  // $0.15
        output: 600_000, // $0.60
        cache_read: None,
        cache_write: None,
        tiers: Vec::new(),
    }
}

fn expensive() -> radian_host::ModelPrice {
    radian_host::ModelPrice {
        input: 3_000_000, // $3
        output: 15_000_000,
        cache_read: None,
        cache_write: None,
        tiers: Vec::new(),
    }
}

/// A price resolver over the two routes the tests use; anything else has no price.
fn prices() -> cost::RoutePrice {
    Arc::new(|provider: &str, model: &str| match (provider, model) {
        ("opencode-go", "cheap") => Some(cheap()),
        ("anthropic", "expensive") => Some(expensive()),
        _ => None,
    })
}

/// A session file: two conversation calls (the second after a `/model` switch) and one
/// summarizer call.
async fn usage_session(label: &str) -> (Scratch, std::path::PathBuf) {
    let scratch = Scratch::new(label);
    let path = scratch.file("s.jsonl");
    let journal = SessionJournal::create(
        &path,
        label,
        SessionCreatedData::new(
            "/work/example",
            // The session was created on the **cheap** route.
            json!({"model": {"provider": "opencode-go", "model": "cheap"}}),
        ),
    )
    .unwrap();
    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("hi"),
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::Message {
            index: 1,
            message: Message::assistant("hello"),
        })
        .await
        .unwrap();
    // The first call: 1M input on the cheap route = $0.15.
    journal
        .append(JournalRecord::ModelUsage {
            usage: radian_core::ModelUsage {
                input: Some(1_000_000),
                input_includes_cache: Some(false),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    // The switch (ADR 0026 D5/D8): everything after it is on the expensive route.
    journal
        .append_runtime_event(RuntimeEvent::ConfigChanged {
            revision: 1,
            change: radian_runtime::ModelRoute::new("anthropic", "expensive").to_value(),
        })
        .await
        .unwrap();
    // The second call: 1M input on the expensive route = $3.00.
    journal
        .append(JournalRecord::ModelUsage {
            usage: radian_core::ModelUsage {
                input: Some(1_000_000),
                input_includes_cache: Some(false),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    // A summarizer's call (ADR 0018): 100K input on the expensive route = $0.30.
    journal
        .append(JournalRecord::CompactionUsage {
            revision: 2,
            model: "current".into(),
            usage: radian_core::ModelUsage {
                input: Some(100_000),
                output: Some(0),
                input_includes_cache: Some(false),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    drop(journal);
    (scratch, path)
}

/// The fold restores the totals and the fee, pricing each call at its own route.
#[test]
fn a_resume_restores_the_totals_and_the_fee_at_each_calls_own_price() {
    let (_scratch, path) = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(usage_session("fold"));
    let parsed = Session::open(&path).unwrap();
    let projection = SessionProjection::new();

    let folded = cost::restore_usage(&parsed, &projection, &prices());
    assert_eq!(folded, 3, "two conversation calls and one summarizer call");

    let read = projection.read();
    // The counters: 1M + 1M + 100K in.
    assert_eq!(read.usage_input(), Some(2_100_000));
    assert_eq!(read.usage_output(), Some(0), "reported 0, and 0 is a value");
    // The fee: $0.15 (cheap) + $3.00 + $0.30 (expensive). Pricing every call at the *final*
    // route would give 2.1M x $3 = $6.30 instead.
    assert_eq!(read.cost_micros(), Some(3_450_000));
}

/// A route the catalog cannot price contributes no fee, but its tokens still count — and an
/// earlier priced call keeps the fee it earned.
#[test]
fn an_unpriced_route_restores_the_tokens_without_inventing_a_fee() {
    let (_scratch, path) = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(usage_session("unpriced"));
    let parsed = Session::open(&path).unwrap();
    let projection = SessionProjection::new();

    // A resolver that knows only the cheap route: the expensive calls are unpriced.
    let only_cheap: cost::RoutePrice =
        Arc::new(|provider: &str, model: &str| match (provider, model) {
            ("opencode-go", "cheap") => Some(cheap()),
            _ => None,
        });
    assert_eq!(cost::restore_usage(&parsed, &projection, &only_cheap), 3);

    let read = projection.read();
    assert_eq!(read.usage_input(), Some(2_100_000), "tokens still count");
    assert_eq!(read.cost_micros(), Some(150_000), "only the priced call");
}

/// A journal with no usage records folds nothing: the projection stays empty, so the items
/// keep hiding rather than showing a fabricated zero.
#[test]
fn a_session_without_usage_records_restores_nothing() {
    let scratch = Scratch::new("empty");
    let path = scratch.file("s.jsonl");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let journal = SessionJournal::create(
            &path,
            "empty",
            SessionCreatedData::new("/work/example", json!({})),
        )
        .unwrap();
        journal
            .append(JournalRecord::Message {
                index: 0,
                message: Message::user("hi"),
            })
            .await
            .unwrap();
        drop(journal);
    });
    let parsed = Session::open(&path).unwrap();
    let projection = SessionProjection::new();

    assert_eq!(cost::restore_usage(&parsed, &projection, &prices()), 0);
    let read = projection.read();
    assert_eq!(read.usage_input(), None, "not 0: never reported");
    assert_eq!(read.cost_micros(), None, "no fee: the item hides");
}

/// The config snapshot's route prices the calls **before** the first `ConfigChanged`; a
/// snapshot naming an unknown route prices nothing, and the switch still re-prices what
/// follows it.
#[test]
fn the_snapshot_route_prices_the_calls_before_the_first_switch() {
    let scratch = Scratch::new("snapshot");
    let path = scratch.file("s.jsonl");
    let parsed = tokio::runtime::Runtime::new().unwrap().block_on(async {
        let journal = SessionJournal::create(
            &path,
            "snapshot",
            // A route the resolver does not know: no price for the call before the switch.
            SessionCreatedData::new(
                "/work/example",
                json!({"model": {"provider": "mystery", "model": "unknown"}}),
            ),
        )
        .unwrap();
        journal
            .append(JournalRecord::Message {
                index: 0,
                message: Message::user("hi"),
            })
            .await
            .unwrap();
        journal
            .append(JournalRecord::Message {
                index: 1,
                message: Message::assistant("hello"),
            })
            .await
            .unwrap();
        journal
            .append(JournalRecord::ModelUsage {
                usage: radian_core::ModelUsage {
                    input: Some(1_000_000),
                    input_includes_cache: Some(false),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        journal
            .append_runtime_event(RuntimeEvent::ConfigChanged {
                revision: 1,
                change: radian_runtime::ModelRoute::new("opencode-go", "cheap").to_value(),
            })
            .await
            .unwrap();
        journal
            .append(JournalRecord::ModelUsage {
                usage: radian_core::ModelUsage {
                    input: Some(1_000_000),
                    input_includes_cache: Some(false),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        drop(journal);
        Session::open(&path).unwrap()
    });

    let projection = SessionProjection::new();
    cost::restore_usage(&parsed, &projection, &prices());
    let read = projection.read();
    assert_eq!(read.usage_input(), Some(2_000_000));
    // Only the call after the switch is priced: $0.15, not $0.15 + a guess.
    assert_eq!(read.cost_micros(), Some(150_000));
}

/// Research F5: a resumed session folds each historical call at the price in force **then**,
/// and a tiered price picks its tier by that call's own prompt length, not the session total.
/// ADR 0027: a record that carries its fee is added as it stands, so a
/// catalog update does not change what an old session spent. Without this the resume re-priced
/// the call with the **current** catalog and the total moved when the price did.
#[tokio::test]
async fn a_recorded_fee_is_restored_as_it_stands_when_the_catalog_moves() {
    let scratch = Scratch::new("recorded-fee");
    let path = scratch.file("s.jsonl");
    let journal = SessionJournal::create(
        &path,
        "recorded-fee",
        SessionCreatedData::new(
            "/work/example",
            json!({"model": {"provider": "opencode-go", "model": "cheap"}}),
        ),
    )
    .unwrap();
    // What the writer records: a fixed $0.10 for a 1M-input call (micro-dollars).
    journal.set_usage_fee(Arc::new(|usage: &radian_core::ModelUsage| {
        usage
            .input
            .map(|input| radian_session::UsageFee::new(input / 10, false))
    }));
    journal
        .append(JournalRecord::ModelUsage {
            usage: radian_core::ModelUsage {
                input: Some(1_000_000),
                input_includes_cache: Some(false),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    drop(journal);

    // The catalog has moved since: the same route now bills 20x more. The recorded fee wins.
    let moved: cost::RoutePrice = Arc::new(|provider: &str, model: &str| {
        (provider == "opencode-go" && model == "cheap").then(|| radian_host::ModelPrice {
            input: 3_000_000,
            output: 0,
            cache_read: None,
            cache_write: None,
            tiers: Vec::new(),
        })
    });
    let parsed = Session::open(&path).unwrap();
    let projection = SessionProjection::new();
    assert_eq!(cost::restore_usage(&parsed, &projection, &moved), 1);
    let read = projection.read();
    assert_eq!(
        read.cost_micros(),
        Some(100_000),
        "the recorded fee, not the moved catalog price"
    );
    assert!(!read.cost_estimated(), "a recorded fee is not an estimate");
    assert!(!read.cost_incomplete());
}

/// ADR 0027: an old record (no fee field) is re-priced with the current
/// catalog **and** the total is marked an estimate, so the footer shows `~`.
#[test]
fn an_old_record_without_a_fee_is_repriced_and_marks_the_total_an_estimate() {
    let (_scratch, path) = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(usage_session("old-estimate"));
    let parsed = Session::open(&path).unwrap();
    let projection = SessionProjection::new();

    assert_eq!(cost::restore_usage(&parsed, &projection, &prices()), 3);
    let read = projection.read();
    assert_eq!(
        read.cost_micros(),
        Some(3_450_000),
        "re-priced with today's catalog"
    );
    assert!(
        read.cost_estimated(),
        "an old record has no fee: the total is an estimate"
    );
}

/// ADR 0027: a recorded fee that was incomplete keeps the total
/// incomplete after a resume — and is still not an estimate.
#[tokio::test]
async fn a_recorded_incomplete_fee_keeps_the_total_incomplete() {
    let scratch = Scratch::new("recorded-incomplete");
    let path = scratch.file("s.jsonl");
    let journal = SessionJournal::create(
        &path,
        "recorded-incomplete",
        SessionCreatedData::new(
            "/work/example",
            json!({"model": {"provider": "opencode-go", "model": "cheap"}}),
        ),
    )
    .unwrap();
    journal.set_usage_fee(Arc::new(|_usage: &radian_core::ModelUsage| {
        Some(radian_session::UsageFee::new(50_000, true))
    }));
    journal
        .append(JournalRecord::ModelUsage {
            usage: radian_core::ModelUsage {
                input: Some(1_000_000),
                input_includes_cache: Some(false),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    drop(journal);

    let parsed = Session::open(&path).unwrap();
    let projection = SessionProjection::new();
    assert_eq!(cost::restore_usage(&parsed, &projection, &prices()), 1);
    let read = projection.read();
    assert_eq!(read.cost_micros(), Some(50_000));
    assert!(read.cost_incomplete(), "the recorded flag survives");
    assert!(!read.cost_estimated());
}

#[tokio::test]
async fn a_resumed_tiered_price_folds_each_call_by_its_own_length() {
    let scratch = Scratch::new("tiered-resume");
    let path = scratch.file("s.jsonl");
    let journal = SessionJournal::create(
        &path,
        "tiered-resume",
        SessionCreatedData::new(
            "/work/example",
            json!({"model": {"provider": "t", "model": "tiered"}}),
        ),
    )
    .unwrap();
    // Two calls on the same tiered route: one below the 100K threshold, one above.
    for input in [50_000u64, 150_000] {
        journal
            .append(JournalRecord::ModelUsage {
                usage: radian_core::ModelUsage {
                    input: Some(input),
                    input_includes_cache: Some(false),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
    }
    drop(journal);

    let tiered = radian_host::ModelPrice {
        input: 100_000,
        output: 0,
        cache_read: None,
        cache_write: None,
        tiers: vec![radian_host::PriceTier {
            size: 100_000,
            input: 500_000,
            output: 0,
            cache_read: None,
            cache_write: None,
        }],
    };
    let prices: cost::RoutePrice = Arc::new(move |provider: &str, model: &str| {
        (provider == "t" && model == "tiered").then(|| tiered.clone())
    });
    let parsed = Session::open(&path).unwrap();
    let projection = SessionProjection::new();
    assert_eq!(cost::restore_usage(&parsed, &projection, &prices), 2);
    // 50K at the base rate = 5_000 micro-dollars; 150K at the tier = 75_000; total 80_000.
    assert_eq!(
        projection.read().cost_micros(),
        Some(80_000),
        "each call keeps its own tier"
    );
}
