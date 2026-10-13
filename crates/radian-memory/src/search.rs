//! Keyword search and the deterministic order (ADR 0021 D6).
//!
//! The order is total and free of timestamps of the moment: scope, then trust (user/confirmed
//! before agent-unverified), then the match score, then `updated_at` newest-first, then `id`.

use crate::model::MemoryRecord;

/// What to select. `text` is a case-insensitive keyword (all terms must match); `key` is an
/// exact key; archived records are excluded unless asked for.
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub text: Option<String>,
    pub key: Option<String>,
    pub include_archived: bool,
}

/// Filters and orders `records`. Deterministic: the same input yields the same order.
pub fn search(records: &[MemoryRecord], query: &SearchQuery) -> Vec<MemoryRecord> {
    let terms: Vec<String> = query
        .text
        .as_deref()
        .map(|text| {
            text.split_whitespace()
                .map(|term| term.to_lowercase())
                .collect()
        })
        .unwrap_or_default();
    let mut selected: Vec<(u32, MemoryRecord)> = records
        .iter()
        // A record with an unknown enum value is listed/exported but not searched (ADR 0021
        // D4): this version cannot interpret it.
        .filter(|record| record.unknown_field().is_none())
        .filter(|record| query.include_archived || record.is_active())
        .filter(|record| match &query.key {
            Some(key) => record.key.as_deref() == Some(key.as_str()),
            None => true,
        })
        .filter_map(|record| {
            let haystack = format!(
                "{}\n{}",
                record.content.to_lowercase(),
                record.key.as_deref().unwrap_or_default().to_lowercase()
            );
            let mut score = 0u32;
            for term in &terms {
                if !haystack.contains(term.as_str()) {
                    return None;
                }
                score += haystack.matches(term.as_str()).count() as u32;
            }
            Some((score, record.clone()))
        })
        .collect();
    selected.sort_by(|(left_score, left), (right_score, right)| {
        scope_rank(left)
            .cmp(&scope_rank(right))
            .then_with(|| trust_rank(left).cmp(&trust_rank(right)))
            .then_with(|| right_score.cmp(left_score))
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| left.id.cmp(&right.id))
    });
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
