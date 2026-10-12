//! `ConfigChanged`: serializability, unknown-field tolerance, monotonic revisions.

use radian_slots::*;

#[test]
fn config_changed_round_trips_through_json() {
    let model = ConfigChanged {
        revision: 1,
        change: ConfigChange::ModelRoute {
            route: "openai:gpt-4o".into(),
        },
    };
    let json = serde_json::to_string(&model).unwrap();
    assert_eq!(
        json,
        r#"{"revision":1,"change":{"kind":"model_route","route":"openai:gpt-4o"}}"#
    );
    let back: ConfigChanged = serde_json::from_str(&json).unwrap();
    assert_eq!(back, model);

    let policy = ConfigChanged {
        revision: 2,
        change: ConfigChange::Policy {
            name: "approve-first".into(),
        },
    };
    let json = serde_json::to_string(&policy).unwrap();
    assert_eq!(
        json,
        r#"{"revision":2,"change":{"kind":"policy","name":"approve-first"}}"#
    );
    let back: ConfigChanged = serde_json::from_str(&json).unwrap();
    assert_eq!(back, policy);
}

#[test]
fn old_reader_ignores_unknown_optional_fields() {
    // A future writer added `future_top` next to the known ones and `future_field` inside
    // the change payload; an old reader must ignore both, keeping the known fields.
    let json = r#"{
        "revision": 3,
        "change": {"kind": "policy", "name": "p", "future_field": 7},
        "future_top": true
    }"#;
    let parsed: ConfigChanged = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.revision, 3);
    assert_eq!(parsed.change, ConfigChange::Policy { name: "p".into() });
}
