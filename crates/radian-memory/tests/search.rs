//! ADR 0021 M1 #9 (search): the order is total and deterministic — scope, then trust, then
//! match score, then `updated_at`, then `id`.

mod common;

use radian_memory::{
    Confirmer, Kind, MemoryId, MemoryRecord, ProjectId, Provenance, Scope, SearchQuery, Source,
    Status, Verification, search,
};

const AT: &str = "2026-10-13T00:00:00.000000Z";

#[allow(clippy::too_many_arguments)]
fn record(
    id: &str,
    scope: Scope,
    source: Source,
    confirmed: bool,
    key: Option<&str>,
    content: &str,
    updated_at: &str,
    status: Status,
) -> MemoryRecord {
    MemoryRecord {
        id: MemoryId(id.into()),
        scope,
        kind: Kind::Fact,
        key: key.map(str::to_owned),
        content: content.into(),
        source,
        provenance: Provenance::default(),
        verification: if confirmed {
            Verification::Confirmed {
                by: Confirmer::User,
                at: AT.into(),
            }
        } else {
            Verification::Unverified
        },
        status,
        created_at: AT.into(),
        updated_at: updated_at.into(),
        revision: 1,
    }
}

fn ids(records: &[MemoryRecord]) -> Vec<&str> {
    records.iter().map(|record| record.id.as_str()).collect()
}

#[test]
fn search_orders_by_scope_then_trust_then_score_then_time_then_id() {
    let global = Scope::Global;
    let project = Scope::Project(ProjectId("p1".into()));
    let records = vec![
        record(
            "a",
            global.clone(),
            Source::Agent,
            false,
            None,
            "rust",
            "2026-10-13T03:00:00Z",
            Status::Active,
        ),
        record(
            "b",
            project.clone(),
            Source::Agent,
            false,
            None,
            "rust",
            "2026-10-13T04:00:00Z",
            Status::Active,
        ),
        record(
            "c",
            global.clone(),
            Source::User,
            true,
            None,
            "rust rust rust",
            "2026-10-13T01:00:00Z",
            Status::Active,
        ),
        record(
            "d",
            project.clone(),
            Source::User,
            true,
            None,
            "rust",
            "2026-10-13T02:00:00Z",
            Status::Active,
        ),
    ];
    let query = SearchQuery {
        text: Some("rust".into()),
        ..Default::default()
    };
    // Project before Global; within a scope, the user record before the agent record.
    assert_eq!(ids(&search(&records, &query)), ["d", "b", "c", "a"]);

    // The same input twice yields the same order.
    assert_eq!(
        ids(&search(&records, &query)),
        ids(&search(&records, &query))
    );
}

#[test]
fn a_higher_match_score_wins_within_a_group() {
    let records = vec![
        record(
            "low",
            Scope::Global,
            Source::User,
            true,
            None,
            "rust",
            "2026-10-13T01:00:00Z",
            Status::Active,
        ),
        record(
            "high",
            Scope::Global,
            Source::User,
            true,
            None,
            "rust rust rust",
            "2026-10-13T05:00:00Z",
            Status::Active,
        ),
    ];
    let query = SearchQuery {
        text: Some("rust".into()),
        ..Default::default()
    };
    assert_eq!(ids(&search(&records, &query)), ["high", "low"]);
}

#[test]
fn ties_break_on_updated_at_then_id() {
    let records = vec![
        record(
            "b",
            Scope::Global,
            Source::User,
            true,
            None,
            "x",
            "2026-10-13T02:00:00Z",
            Status::Active,
        ),
        record(
            "a",
            Scope::Global,
            Source::User,
            true,
            None,
            "x",
            "2026-10-13T02:00:00Z",
            Status::Active,
        ),
        record(
            "c",
            Scope::Global,
            Source::User,
            true,
            None,
            "x",
            "2026-10-13T03:00:00Z",
            Status::Active,
        ),
    ];
    let query = SearchQuery {
        text: Some("x".into()),
        ..Default::default()
    };
    // Newest updated_at first; equal timestamps fall back to id order.
    assert_eq!(ids(&search(&records, &query)), ["c", "a", "b"]);
}

#[test]
fn search_filters_by_status_and_key() {
    let records = vec![
        record(
            "a",
            Scope::Global,
            Source::User,
            true,
            Some("one"),
            "x",
            "2026-10-13T01:00:00Z",
            Status::Active,
        ),
        record(
            "b",
            Scope::Global,
            Source::User,
            true,
            Some("two"),
            "x",
            "2026-10-13T01:00:00Z",
            Status::Archived,
        ),
    ];
    let all = SearchQuery::default();
    assert_eq!(ids(&search(&records, &all)), ["a"], "archived is excluded");

    let archived = SearchQuery {
        include_archived: true,
        ..Default::default()
    };
    assert_eq!(ids(&search(&records, &archived)).len(), 2);

    let by_key = SearchQuery {
        key: Some("two".into()),
        include_archived: true,
        ..Default::default()
    };
    assert_eq!(ids(&search(&records, &by_key)), ["b"]);
}

#[test]
fn a_missing_keyword_matches_nothing() {
    let records = vec![record(
        "a",
        Scope::Global,
        Source::User,
        true,
        None,
        "rust",
        "2026-10-13T01:00:00Z",
        Status::Active,
    )];
    let query = SearchQuery {
        text: Some("python".into()),
        ..Default::default()
    };
    assert!(search(&records, &query).is_empty());
}
