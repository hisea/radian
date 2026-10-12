//! `ConfigChanged`: serializability, unknown-field tolerance, monotonic revisions.

mod common;

use std::sync::Arc;

use common::ConstantModel;
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

#[tokio::test]
async fn shared_log_keeps_one_monotonic_revision_sequence() {
    let log = Arc::new(ConfigLog::new());
    let mslot = ModelSlot::with_config_log(ConstantModel::new("a"), Arc::clone(&log));
    let pslot = PolicySlot::with_config_log(AllowAll, Arc::clone(&log));

    let r1 = mslot.swap("r1", ConstantModel::new("b"));
    let r2 = pslot.swap("p1", DenyAll::new("no"));
    let r3 = pslot.swap("p2", AllowAll);
    assert_eq!([r1.revision, r2.revision, r3.revision], [1, 2, 3]);
    let records = log.records();
    assert_eq!(records, vec![r1, r2, r3]);
    // Each record names only the changed route/policy: no keys or secrets.
    for record in &records {
        assert!(serde_json::to_string(record).unwrap().len() < 256);
    }
}
