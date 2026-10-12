//! `ModelUsage`: serialization shape (AC-A5-6).

use agent_core::{ModelUsage, OpaqueJson};

#[test]
fn default_usage_serializes_as_an_empty_object() {
    assert_eq!(serde_json::to_string(&ModelUsage::default()).unwrap(), "{}");
}

#[test]
fn partial_usage_serializes_only_its_fields() {
    let usage = ModelUsage {
        input: Some(5),
        total: Some(12),
        ..ModelUsage::default()
    };
    assert_eq!(
        serde_json::to_string(&usage).unwrap(),
        r#"{"input":5,"total":12}"#
    );
}

#[test]
fn details_round_trip_preserves_the_nested_object_verbatim() {
    let raw = r#"{"cached_tokens":7,"audio_tokens":{"prefill":0,"written":0}}"#;
    let usage = ModelUsage {
        input: Some(3),
        details: Some(OpaqueJson::parse(raw).unwrap()),
        ..ModelUsage::default()
    };
    let json = serde_json::to_string(&usage).unwrap();
    assert_eq!(
        json,
        r#"{"input":3,"details":{"cached_tokens":7,"audio_tokens":{"prefill":0,"written":0}}}"#
    );
    let back: ModelUsage = serde_json::from_str(&json).unwrap();
    assert_eq!(back, usage);
    assert_eq!(back.details.unwrap().get(), raw);
}

#[test]
fn old_json_with_missing_fields_reads_as_none() {
    let usage: ModelUsage = serde_json::from_str(r#"{"total":3}"#).unwrap();
    assert_eq!(usage.total, Some(3));
    assert_eq!(usage.input, None);
    assert_eq!(usage.output, None);
    assert_eq!(usage.cache_read, None);
    assert_eq!(usage.cache_write, None);
    assert_eq!(usage.details, None);
}
