//! Keyword search and the deterministic order (ADR 0021 D6).
//!
//! The order is total and free of timestamps of the moment: scope, then trust (user/confirmed
//! before agent-unverified), then the match score, then `updated_at` newest-first, then `id`.
//!
//! [`search`] keeps only records that contain every query term. [`ranked`] applies the same
//! structural filters, scoring and order but **keeps non-matching records at score 0**: the
//! memory injection ranks all Active records and lets the match score order them, rather than
//! dropping everything the current request happens not to mention (ADR 0021 D6). Both share
//! one scoring and one ordering, so they can never disagree.

use crate::model::MemoryRecord;

/// What to select. `text` is a case-insensitive keyword (all terms must match); `key` is an
/// exact key; archived records are excluded unless asked for.
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub text: Option<String>,
    pub key: Option<String>,
    pub include_archived: bool,
}

/// The terms of `query.text`, lower-cased. Empty when there is no text.
fn terms_of(query: &SearchQuery) -> Vec<String> {
    query
        .text
        .as_deref()
        .map(|text| {
            text.split_whitespace()
                .map(|term| term.to_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

/// The match score for one record: `Some(hits)` when every term occurs, `None` otherwise.
fn score(record: &MemoryRecord, terms: &[String]) -> Option<u32> {
    let haystack = format!(
        "{}\n{}",
        record.content.to_lowercase(),
        record.key.as_deref().unwrap_or_default().to_lowercase()
    );
    let mut score = 0u32;
    for term in terms {
        if !haystack.contains(term.as_str()) {
            return None;
        }
        score += haystack.matches(term.as_str()).count() as u32;
    }
    Some(score)
}

/// The structural filters both entry points apply: a record this version cannot interpret is
/// listed/exported but never selected (ADR 0021 D4), archived records are excluded unless
/// asked for, and an explicit `key` must match exactly.
fn keeps(record: &MemoryRecord, query: &SearchQuery) -> bool {
    record.unknown_field().is_none()
        && (query.include_archived || record.is_active())
        && match &query.key {
            Some(key) => record.key.as_deref() == Some(key.as_str()),
            None => true,
        }
}

/// The D6 total order. Deterministic: the same input yields the same order.
fn order(selected: &mut [(u32, MemoryRecord)]) {
    selected.sort_by(|(left_score, left), (right_score, right)| {
        scope_rank(left)
            .cmp(&scope_rank(right))
            .then_with(|| trust_rank(left).cmp(&trust_rank(right)))
            .then_with(|| right_score.cmp(left_score))
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| left.id.cmp(&right.id))
    });
}

/// Filters and orders `records`. Deterministic: the same input yields the same order.
pub fn search(records: &[MemoryRecord], query: &SearchQuery) -> Vec<MemoryRecord> {
    let terms = terms_of(query);
    let mut selected: Vec<(u32, MemoryRecord)> = records
        .iter()
        .filter(|record| keeps(record, query))
        .filter_map(|record| score(record, &terms).map(|score| (score, record.clone())))
        .collect();
    order(&mut selected);
    selected.into_iter().map(|(_, record)| record).collect()
}

/// Every record that passes the structural filters, in the D6 order, with a match score
/// against `query.text`. A record the text does not match scores 0 instead of being dropped.
/// The memory injection uses this (ADR 0021 D6); the score only orders, it does not select.
pub fn ranked(records: &[MemoryRecord], query: &SearchQuery) -> Vec<MemoryRecord> {
    let terms = terms_of(query);
    let mut selected: Vec<(u32, MemoryRecord)> = records
        .iter()
        .filter(|record| keeps(record, query))
        .map(|record| (score(record, &terms).unwrap_or(0), record.clone()))
        .collect();
    order(&mut selected);
    selected.into_iter().map(|(_, record)| record).collect()
}

/// Project before Global (ADR 0021 D1/D6).
fn scope_rank(record: &MemoryRecord) -> u8 {
    u8::from(record.scope.is_global())
}

/// User or confirmed before agent-unverified (ADR 0021 D5/D6).
fn trust_rank(record: &MemoryRecord) -> u8 {
    let trusted = record.source == crate::model::Source::User || record.verification.is_confirmed();
    u8::from(!trusted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, MemoryId, Provenance, Scope, Source};

    fn record(id: &str, scope: Scope, source: Source, content: &str) -> MemoryRecord {
        MemoryRecord::new(
            MemoryId(id.into()),
            scope,
            Kind::Fact,
            None,
            content.into(),
            source,
            Provenance::default(),
            "2026-10-13T00:00:00.000000Z".into(),
        )
    }

    /// `ranked` keeps a record the query does not match at score 0 and puts matching records
    /// first; `search` drops it. Same order for the matching subset.
    #[test]
    fn ranked_keeps_non_matching_records_and_search_drops_them() {
        let records = vec![
            record("a", Scope::Global, Source::User, "parser work"),
            record("b", Scope::Global, Source::User, "unrelated note"),
            record("c", Scope::Global, Source::User, "parser parser again"),
        ];
        let query = SearchQuery {
            text: Some("parser".into()),
            ..SearchQuery::default()
        };
        let matches: Vec<String> = search(&records, &query)
            .into_iter()
            .map(|r| r.id.0)
            .collect();
        assert_eq!(matches, ["c", "a"], "more hits first");
        let ranked: Vec<String> = ranked(&records, &query)
            .into_iter()
            .map(|r| r.id.0)
            .collect();
        assert_eq!(ranked, ["c", "a", "b"], "the non-match stays last");
    }
}
