//! One run's metrics, folded from its journal records (ADR 0044 `Result.run_metrics`,
//! ADR 0049 §6 `_meta.radian.metrics`). Totals only; ADR 0044 §2 defines every field.
//!
//! - `requests`: `ModelUsageRecorded` plus `CompactionUsageRecorded` records;
//!   `compaction_requests` is the second kind alone.
//! - `input_uncached`, `cache_read`, `cache_write`, `output`, `reasoning`, `fee_micros`: the
//!   sums of those columns, with ADR 0007 J11's split (`input_includes_cache: true` subtracts
//!   both cache counters from `input`; `false` and an unmarked old record use `input` as is).
//! - `peak_context`: the largest per-request prompt size (input including cache, ADR 0007
//!   `prompt_total()`), and `context_window`: the window this process resolved (ADR 0018).
//! - `compactions`: `ContextCompacted` records.
//! - `model_errors`: `TurnFailed` records; a run that ended on a model error wrote one, so it
//!   is counted there once.
//!
//! A counter no record reported is `None` ("not reported", never 0, ADR 0007 J1). `retries`
//! counts the live `ModelRetrying` events of the folded range (ADR 0010: they are not journal
//! records): an exec run counts its own process's, a resumed run only the resumed part, and an
//! ACP turn only its own turn's.

use std::path::Path;

use serde_json::{Value, json};

/// The keys a reasoning count is read from, in order.
const REASONING_KEYS: [&str; 5] = [
    "reasoning_tokens",
    "reasoning_output_tokens",
    "thinking_tokens",
    "reasoningTokens",
    "reasoning",
];

/// One run's metrics. Every `Option` is "not reported", never 0 (ADR 0007 J1).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RunMetrics {
    pub(crate) requests: u64,
    pub(crate) compaction_requests: u64,
    pub(crate) input_uncached: Option<u64>,
    pub(crate) cache_read: Option<u64>,
    pub(crate) cache_write: Option<u64>,
    pub(crate) output: Option<u64>,
    pub(crate) reasoning: Option<u64>,
    pub(crate) fee_micros: Option<u64>,
    pub(crate) fee_incomplete: bool,
    pub(crate) peak_context: Option<u64>,
    pub(crate) context_window: Option<u64>,
    pub(crate) compactions: u64,
    pub(crate) model_errors: u64,
    pub(crate) retries: u64,
}

impl RunMetrics {
    /// The JSON object the two outputs carry: a fixed key set.
    pub(crate) fn to_value(&self) -> Value {
        json!({
            "requests": self.requests,
            "compaction_requests": self.compaction_requests,
            "input_uncached": self.input_uncached,
            "cache_read": self.cache_read,
            "cache_write": self.cache_write,
            "output": self.output,
            "reasoning": self.reasoning,
            "fee_micros": self.fee_micros,
            "fee_incomplete": self.fee_incomplete,
            "peak_context": self.peak_context,
            "context_window": self.context_window,
            "compactions": self.compactions,
            "model_errors": self.model_errors,
            "retries": self.retries,
        })
    }
}

/// The metrics of the bytes this process wrote: the journal suffix after the tee prefix, the
/// same range `run_turns`/`run_usage`/`run_cost` fold. A missing or torn file folds to its
/// parseable lines only.
pub(crate) fn for_run(
    path: &Path,
    prefix_len: u64,
    retries: u64,
    context_window: Option<u64>,
) -> RunMetrics {
    let Ok(bytes) = std::fs::read(path) else {
        return empty(retries, context_window);
    };
    let start = (prefix_len as usize).min(bytes.len());
    let mut metrics = empty(retries, context_window);
    for line in String::from_utf8_lossy(&bytes[start..]).lines() {
        if let Ok(record) = serde_json::from_str::<Value>(line) {
            observe(&mut metrics, &record);
        }
    }
    metrics
}

/// The metrics of one turn: the records between its `TurnStarted` and `TurnFinished`, the same
/// range `summarize_turn` folds for the `session/prompt` answer.
pub(crate) fn for_turn(
    path: &Path,
    turn_id: &str,
    retries: u64,
    context_window: Option<u64>,
) -> RunMetrics {
    let Ok(bytes) = std::fs::read(path) else {
        return empty(retries, context_window);
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut metrics = empty(retries, context_window);
    let mut inside = false;
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(event) = record.get("event").and_then(Value::as_str) else {
            continue;
        };
        match event {
            "TurnStarted" => {
                if turn_of(record.get("data")) == Some(turn_id) {
                    inside = true;
                }
            }
            "TurnFinished" => {
                if turn_of(record.get("data")) == Some(turn_id) {
                    break;
                }
            }
            _ if inside => observe(&mut metrics, &record),
            _ => {}
        }
    }
    metrics
}

/// A fold over raw journal bytes: unparseable lines (a torn tail included) are skipped, the
/// same rule `summarize_journal` uses. Backs the unit tests.
#[cfg(test)]
pub(crate) fn fold_bytes(bytes: &[u8], retries: u64, context_window: Option<u64>) -> RunMetrics {
    let mut metrics = empty(retries, context_window);
    for line in String::from_utf8_lossy(bytes).lines() {
        if let Ok(record) = serde_json::from_str::<Value>(line) {
            observe(&mut metrics, &record);
        }
    }
    metrics
}

fn empty(retries: u64, context_window: Option<u64>) -> RunMetrics {
    RunMetrics {
        retries,
        context_window,
        ..RunMetrics::default()
    }
}

/// The `turn_id` of a `TurnStarted`/`TurnFinished` body.
fn turn_of(data: Option<&Value>) -> Option<&str> {
    data.and_then(|d| d.get("turn_id")).and_then(|t| t.as_str())
}

fn observe(metrics: &mut RunMetrics, record: &Value) {
    let Some(event) = record.get("event").and_then(Value::as_str) else {
        return;
    };
    match event {
        "ModelUsageRecorded" | "CompactionUsageRecorded" => {
            usage(
                metrics,
                event == "CompactionUsageRecorded",
                record.get("data"),
            );
        }
        "ContextCompacted" => metrics.compactions += 1,
        "TurnFailed" => metrics.model_errors += 1,
        _ => {}
    }
}

fn usage(metrics: &mut RunMetrics, compaction: bool, data: Option<&Value>) {
    let Some(data) = data else { return };
    let Some(usage) = data.get("usage") else {
        return;
    };
    metrics.requests += 1;
    if compaction {
        metrics.compaction_requests += 1;
    }
    let (input_uncached, cache_read, prompt_total) = inputs(usage);
    add(&mut metrics.input_uncached, input_uncached);
    add(&mut metrics.cache_read, cache_read);
    add(
        &mut metrics.cache_write,
        usage.get("cache_write").and_then(Value::as_u64),
    );
    add(
        &mut metrics.output,
        usage.get("output").and_then(Value::as_u64),
    );
    add(&mut metrics.reasoning, reasoning_of(usage));
    if let Some(total) = prompt_total {
        metrics.peak_context = Some(metrics.peak_context.unwrap_or(0).max(total));
    }
    let fee = data.get("fee");
    add(
        &mut metrics.fee_micros,
        fee.and_then(|f| f.get("micros")).and_then(Value::as_u64),
    );
    if fee
        .and_then(|f| f.get("incomplete"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        metrics.fee_incomplete = true;
    }
}

/// (uncached, cache_read, prompt total) of one usage object, by ADR 0007 J11's split.
fn inputs(usage: &Value) -> (Option<u64>, Option<u64>, Option<u64>) {
    let input = usage.get("input").and_then(Value::as_u64);
    let cache_read = usage.get("cache_read").and_then(Value::as_u64);
    let cache_write = usage.get("cache_write").and_then(Value::as_u64);
    let Some(input) = input else {
        return (None, cache_read, None);
    };
    let cache = cache_read.unwrap_or(0) + cache_write.unwrap_or(0);
    match usage.get("input_includes_cache").and_then(Value::as_bool) {
        Some(true) => (Some(input.saturating_sub(cache)), cache_read, Some(input)),
        Some(false) => (Some(input), cache_read, Some(input + cache)),
        // An unmarked old record: `input` stands alone both as the full-price part and as the
        // prompt total; its cache read may already sit inside it.
        None => (Some(input), cache_read, Some(input)),
    }
}

/// The reasoning count inside a provider usage object, read from `details` first, then the usage object itself, recursively.
fn reasoning_of(usage: &Value) -> Option<u64> {
    usage
        .get("details")
        .and_then(reasoning_value)
        .or_else(|| reasoning_value(usage))
}

fn reasoning_value(value: &Value) -> Option<u64> {
    let object = value.as_object()?;
    for key in REASONING_KEYS {
        if let Some(count) = object.get(key).and_then(Value::as_u64) {
            return Some(count);
        }
    }
    for value in object.values() {
        if value.is_object()
            && let Some(count) = reasoning_value(value)
        {
            return Some(count);
        }
    }
    None
}

fn add(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0).saturating_add(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold(lines: &[Value]) -> RunMetrics {
        let mut text = String::new();
        for line in lines {
            text.push_str(&serde_json::to_string(line).unwrap());
            text.push('\n');
        }
        fold_bytes(text.as_bytes(), 0, None)
    }

    fn record(seq: u64, ts: &str, event: &str, data: Value) -> Value {
        json!({
            "schema_version": 1,
            "session_id": "s",
            "seq": seq,
            "ts": ts,
            "event": event,
            "critical": false,
            "data": data,
        })
    }

    fn session(seq: u64, ts: &str) -> Value {
        json!({
            "schema_version": 1, "session_id": "s", "seq": seq, "ts": ts,
            "event": "SessionCreated", "critical": true,
            "data": {"workspace": "/w", "config_snapshot": {"model": {"provider": "p", "model": "m-1"}}},
        })
    }

    fn usage_record(seq: u64, event: &str, data: Value) -> Value {
        record(seq, "2026-10-31T00:00:01Z", event, data)
    }

    #[test]
    fn two_chat_requests_and_one_compaction_request_are_counted() {
        let metrics = fold(&[
            session(0, "2026-10-31T00:00:00Z"),
            usage_record(
                1,
                "ModelUsageRecorded",
                json!({"usage": {"input": 10, "output": 1}}),
            ),
            usage_record(
                2,
                "CompactionUsageRecorded",
                json!({"revision": 1, "usage": {"input": 20, "output": 2}}),
            ),
            usage_record(
                3,
                "ModelUsageRecorded",
                json!({"usage": {"input": 30, "output": 3}}),
            ),
        ]);
        assert_eq!(metrics.requests, 3);
        assert_eq!(metrics.compaction_requests, 1);
        assert_eq!(metrics.input_uncached, Some(60));
        assert_eq!(metrics.output, Some(6));
    }

    #[test]
    fn a_marked_input_subtracts_the_cache_and_an_unmarked_one_does_not() {
        let metrics = fold(&[
            usage_record(
                1,
                "ModelUsageRecorded",
                json!({"usage": {"input": 11_686, "cache_read": 4_000, "input_includes_cache": true}}),
            ),
            usage_record(
                2,
                "ModelUsageRecorded",
                json!({"usage": {"input": 100, "cache_read": 40, "input_includes_cache": false}}),
            ),
        ]);
        assert_eq!(metrics.input_uncached, Some(7_686 + 100));
        assert_eq!(metrics.cache_read, Some(4_040));
        assert_eq!(metrics.peak_context, Some(11_686));
    }

    #[test]
    fn a_run_with_no_usage_record_reports_zero_requests_and_no_counters() {
        let metrics = fold(&[session(0, "2026-10-31T00:00:00Z")]);
        assert_eq!(metrics.requests, 0);
        assert_eq!(metrics.compaction_requests, 0);
        assert!(metrics.input_uncached.is_none());
        assert!(metrics.cache_read.is_none());
        assert!(metrics.cache_write.is_none());
        assert!(metrics.output.is_none());
        assert!(metrics.reasoning.is_none());
        assert!(metrics.fee_micros.is_none());
        assert!(metrics.peak_context.is_none());
    }

    #[test]
    fn fees_compactions_and_model_errors_are_totalled() {
        let metrics = fold(&[
            usage_record(
                1,
                "ModelUsageRecorded",
                json!({"usage": {"input": 1}, "fee": {"micros": 5, "incomplete": true}}),
            ),
            usage_record(
                2,
                "ModelUsageRecorded",
                json!({"usage": {"input": 1}, "fee": {"micros": 7}}),
            ),
            record(
                3,
                "2026-10-31T00:00:02Z",
                "ContextCompacted",
                json!({"revision": 1}),
            ),
            record(
                4,
                "2026-10-31T00:00:03Z",
                "TurnFailed",
                json!({"turn_id": "t"}),
            ),
        ]);
        assert_eq!(metrics.fee_micros, Some(12));
        assert!(metrics.fee_incomplete);
        assert_eq!(metrics.compactions, 1);
        assert_eq!(metrics.model_errors, 1);
    }

    #[test]
    fn the_run_range_is_only_the_suffix_after_the_prefix() {
        let dir = std::env::temp_dir().join(format!("radian-metrics-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("run.jsonl");
        let events = [
            session(0, "2026-10-31T00:00:00Z"),
            usage_record(
                1,
                "ModelUsageRecorded",
                json!({"usage": {"input": 10, "output": 1}}),
            ),
            usage_record(
                2,
                "ModelUsageRecorded",
                json!({"usage": {"input": 20, "output": 2}}),
            ),
        ];
        let mut file = String::new();
        let mut prefix_len = 0u64;
        for (index, event) in events.iter().enumerate() {
            let mut line = serde_json::to_string(event).unwrap();
            line.push('\n');
            if index < 2 {
                prefix_len += line.len() as u64;
            }
            file.push_str(&line);
        }
        std::fs::write(&path, &file).unwrap();
        let whole = for_run(&path, 0, 0, None);
        assert_eq!(whole.requests, 2);
        assert_eq!(whole.input_uncached, Some(30));
        let run = for_run(&path, prefix_len, 0, None);
        assert_eq!(run.requests, 1, "only the request after the tee prefix");
        assert_eq!(run.input_uncached, Some(20));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_final_line_is_skipped() {
        let mut text = String::new();
        text.push_str(
            &serde_json::to_string(&usage_record(
                1,
                "ModelUsageRecorded",
                json!({"usage": {"input": 10}}),
            ))
            .unwrap(),
        );
        text.push('\n');
        text.push_str("{\"event\":\"ModelUsageRecorded\",\"data\":{\"usage\":{\"inp");
        let metrics = fold_bytes(text.as_bytes(), 0, None);
        assert_eq!(metrics.requests, 1);
        assert_eq!(metrics.input_uncached, Some(10));
    }

    #[test]
    fn retries_and_the_window_pass_through() {
        let metrics = fold_bytes(b"", 2, Some(200_000));
        assert_eq!(metrics.retries, 2);
        assert_eq!(metrics.context_window, Some(200_000));
    }

    #[test]
    fn a_turn_fold_takes_only_that_turns_usage() {
        let dir = std::env::temp_dir().join(format!("radian-metrics-turn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("turn.jsonl");
        let events = [
            session(0, "2026-10-31T00:00:00Z"),
            record(
                1,
                "2026-10-31T00:00:01Z",
                "TurnStarted",
                json!({"turn_id": "t-1"}),
            ),
            usage_record(2, "ModelUsageRecorded", json!({"usage": {"input": 10}})),
            record(
                3,
                "2026-10-31T00:00:03Z",
                "TurnFinished",
                json!({"turn_id": "t-1", "outcome": "completed"}),
            ),
            record(
                4,
                "2026-10-31T00:00:04Z",
                "TurnStarted",
                json!({"turn_id": "t-2"}),
            ),
            usage_record(5, "ModelUsageRecorded", json!({"usage": {"input": 20}})),
            record(
                6,
                "2026-10-31T00:00:06Z",
                "TurnFinished",
                json!({"turn_id": "t-2", "outcome": "completed"}),
            ),
        ];
        let mut text = String::new();
        for event in &events {
            text.push_str(&serde_json::to_string(event).unwrap());
            text.push('\n');
        }
        std::fs::write(&path, text).unwrap();
        let first = for_turn(&path, "t-1", 0, None);
        assert_eq!(first.requests, 1);
        assert_eq!(first.input_uncached, Some(10));
        let second = for_turn(&path, "t-2", 0, None);
        assert_eq!(second.requests, 1);
        assert_eq!(second.input_uncached, Some(20));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The object's key set is exactly the totals; nothing of the removed detail remains.
    #[test]
    fn the_object_has_only_the_totals() {
        let value = RunMetrics::default().to_value();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        let mut want = vec![
            "requests",
            "compaction_requests",
            "input_uncached",
            "cache_read",
            "cache_write",
            "output",
            "reasoning",
            "fee_micros",
            "fee_incomplete",
            "peak_context",
            "context_window",
            "compactions",
            "model_errors",
            "retries",
        ];
        want.sort_unstable();
        assert_eq!(keys, want);
    }
}
