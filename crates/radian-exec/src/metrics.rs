//! One run's metrics, folded from its journal records (ADR 0044 `Result.run_metrics`,
//! ADR 0049 §6 `_meta.radian.metrics`).
//!
//! The field definitions are those of `benchmarks/journal_metrics.py`'s `COLUMNS` for every
//! field the two share:
//!
//! - `requests`: `ModelUsageRecorded` plus `CompactionUsageRecorded` records;
//!   `compaction_requests` is the second kind alone.
//! - `input_uncached`, `cache_read`, `cache_write`, `output`, `reasoning`, `fee_micros`: the
//!   sums of those columns, with ADR 0007 J11's split (`input_includes_cache: true` subtracts
//!   both cache counters from `input`; `false` and an unmarked old record use `input` as is).
//! - `peak_context`: the largest per-request prompt size (input including cache, ADR 0007
//!   `prompt_total()`), and `context_window`: the window this process resolved (ADR 0018).
//! - `compactions`: `ContextCompacted` records; `compaction_ms`: `compaction_seconds` in
//!   milliseconds, from the `ts` of the record immediately before a compaction's first
//!   `CompactionUsageRecorded` to the `ts` of its `ContextCompacted`, summed.
//! - `max_tokens_responses`: usage records whose `stop_reason` is `max_tokens`.
//! - `model_errors`: `TurnFailed` records; a run that ended on a model error wrote one, so it
//!   is counted there once.
//! - `tools`: `calls`/`result_bytes` as `journal_metrics.py` counts `calls.<tool>` and
//!   `bytes.<tool>` (a call is counted at `ToolExecutionSettled`, attributed through its
//!   `ToolExecutionStarted`); `denied` counts a `PermissionResolved` with `decision: deny`,
//!   and a denied call is not also counted in `failed`.
//! - `models`: the route each request ran under, in first-use order. The route is the
//!   configured provider name, never the adapter kind: `SessionCreated.config_snapshot`
//!   carries it in `provider_id`, or nested as `model.provider`/`model.model`, and the TUI
//!   writes it directly in the top-level `provider`/`model`; every later
//!   `ConfigChanged.change.model.provider`/`.model` switches it. An older exec snapshot that
//!   predates `provider_id` carries only the adapter kind in `provider`, which is then the best
//!   available answer.
//! - `request_log`: one entry per request, with `latency_ms` from the last journal record
//!   written before the request to the usage record's `ts`, cut at 500 entries
//!   (`request_log_truncated` then says so). The anchor is the turn's `TurnStarted` for its
//!   first request and, for a later one, the last record written between the previous response
//!   and this request: a `ToolExecutionStarted`/`ToolExecutionSettled`, a tool-result or steer
//!   `MessageAppended`, a compaction's records, the max-tokens notice, or the previous turn's
//!   `TurnFinished` (a `/compact` served between turns). A response's own records (its assistant
//!   `MessageAppended` and its usage record) never move it; the usage record closes the request
//!   and is itself the anchor of a request that follows with no record in between (a
//!   `PauseTurn` continuation). Records the host writes while a request is in flight
//!   (`InboxEnqueued` for a steer or a background-job notice, `BackgroundJobSettled`, a
//!   `ConfigChanged`) do not move it. So inside a turn the value is the model's whole response
//!   time, not the gap between two writes of one response; a between-turns `/compact` also spans
//!   the idle time from the turn's end, because its request writes no record. A range with no
//!   `TurnStarted`, usage record or `TurnFinished` before its first request has no anchor, so
//!   that first `latency_ms` is null.
//!
//! A counter no record reported is `None` ("not reported", never 0, ADR 0007 J1). `retries`
//! counts the live `ModelRetrying` events of the folded range (ADR 0010: they are not journal
//! records): an exec run counts its own process's, a resumed run only the resumed part, and an
//! ACP turn only its own turn's.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde_json::{Map, Value, json};

/// How many request-log entries a run keeps. A longer run keeps the first ones and says so.
const REQUEST_LOG_LIMIT: u64 = 500;

/// The keys `journal_metrics.py` reads a reasoning count from, in its order.
const REASONING_KEYS: [&str; 5] = [
    "reasoning_tokens",
    "reasoning_output_tokens",
    "thinking_tokens",
    "reasoningTokens",
    "reasoning",
];

/// One tool's counts inside `tools`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ToolMetrics {
    pub(crate) calls: u64,
    pub(crate) result_bytes: u64,
    pub(crate) failed: u64,
    pub(crate) denied: u64,
}

/// One model route's request count inside `models`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelCount {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) requests: u64,
}

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
    pub(crate) compaction_ms: Option<u64>,
    pub(crate) max_tokens_responses: Option<u64>,
    pub(crate) model_errors: u64,
    pub(crate) retries: u64,
    pub(crate) tools: BTreeMap<String, ToolMetrics>,
    pub(crate) models: Vec<ModelCount>,
    pub(crate) request_log: Vec<Value>,
    pub(crate) request_log_truncated: bool,
}

impl RunMetrics {
    /// The JSON object the two outputs carry. `request_log_truncated` is present only when the
    /// log was cut, so an ordinary run's key set is fixed.
    pub(crate) fn to_value(&self) -> Value {
        let tools: Map<String, Value> = self
            .tools
            .iter()
            .map(|(name, tool)| {
                (
                    name.clone(),
                    json!({
                        "calls": tool.calls,
                        "result_bytes": tool.result_bytes,
                        "failed": tool.failed,
                        "denied": tool.denied,
                    }),
                )
            })
            .collect();
        let models: Vec<Value> = self
            .models
            .iter()
            .map(|model| {
                json!({
                    "provider": model.provider,
                    "model": model.model,
                    "requests": model.requests,
                })
            })
            .collect();
        let mut value = json!({
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
            "compaction_ms": self.compaction_ms,
            "max_tokens_responses": self.max_tokens_responses,
            "model_errors": self.model_errors,
            "retries": self.retries,
            "tools": Value::Object(tools),
            "models": models,
            "request_log": self.request_log,
        });
        if self.request_log_truncated {
            value["request_log_truncated"] = Value::Bool(true);
        }
        value
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
    let mut folder = Folder::new(retries, context_window);
    // The tee boundary sits after `SessionCreated`, and a config switch can precede the run's
    // first request; the route in force at the start of the suffix is read (but not counted)
    // from the lines before it.
    for line in String::from_utf8_lossy(&bytes[..start]).lines() {
        if let Ok(record) = serde_json::from_str::<Value>(line) {
            folder.learn_route(&record);
        }
    }
    for line in String::from_utf8_lossy(&bytes[start..]).lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if record.get("event").and_then(Value::as_str).is_none() {
            continue;
        }
        folder.observe(&record);
    }
    folder.finish()
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
    let mut folder = Folder::new(retries, context_window);
    let mut inside = false;
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(event) = record.get("event").and_then(Value::as_str) else {
            continue;
        };
        let data = record.get("data");
        // The route that answers this turn was established before its `TurnStarted` (and may
        // switch during it); learning it here does not count a record.
        if !inside {
            folder.learn_route(&record);
        }
        match event {
            "TurnStarted" => {
                if turn_of(data) == Some(turn_id) {
                    inside = true;
                    // The turn start is the first record of the range: it is the latency
                    // anchor for the turn's first request.
                    folder.observe(&record);
                }
            }
            "TurnFinished" => {
                if turn_of(data) == Some(turn_id) {
                    break;
                }
            }
            _ if inside => folder.observe(&record),
            _ => {}
        }
    }
    folder.finish()
}

/// A fold over raw journal bytes: unparseable lines (a torn tail included) are skipped, the
/// same rule `summarize_journal` uses. The two public entry points do their own range scan;
/// this whole-buffer form backs the unit tests.
#[cfg(test)]
pub(crate) fn fold_bytes(bytes: &[u8], retries: u64, context_window: Option<u64>) -> RunMetrics {
    let text = String::from_utf8_lossy(bytes);
    let mut folder = Folder::new(retries, context_window);
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if record.get("event").and_then(Value::as_str).is_none() {
            continue;
        }
        folder.observe(&record);
    }
    folder.finish()
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

/// Whether a `MessageAppended` body carries the assistant message a request produced
/// (`{"assistant": ...}`), as opposed to a user input, a steer or a tool result.
fn is_assistant_message(data: Option<&Value>) -> bool {
    data.and_then(|d| d.get("message"))
        .and_then(Value::as_object)
        .is_some_and(|message| message.contains_key("assistant"))
}

struct Folder {
    metrics: RunMetrics,
    /// The `ts` of the record immediately before the current one.
    previous_ts: Option<String>,
    /// Whether this folded range has a source for the next request's anchor: its `TurnStarted`
    /// or a usage record. Until then the records before the first request do not anchor it, so
    /// a range that began mid-turn reports a null first latency.
    anchor_ready: bool,
    /// Whether the turn's first response has not begun yet. The records written between
    /// `TurnStarted` and that response (the input's `MessageAppended`, `InputAdmitted`) belong
    /// to the turn's opening, so they do not move the anchor: the first request is timed from
    /// its `TurnStarted` (docs/user/cli.md).
    opening: bool,
    /// The `ts` of the last record written before the pending request, and not part of the
    /// response being timed. Its event set is named in [`Folder::observe`].
    request_anchor: Option<String>,
    /// `call_id -> tool name` from `ToolExecutionStarted`, so a settle is attributed.
    pending: HashMap<String, String>,
    /// Every call a `PermissionResolved` denied.
    denied_calls: HashSet<String>,
    /// The denied calls already counted in `denied`, so a settle does not count them twice.
    denied_counted: HashSet<String>,
    /// The route in force.
    route: Option<(String, String)>,
    /// Routes in first-request order, with their request counts.
    routes: Vec<ModelCount>,
    /// `revision -> the ts (micros) of the record before its first CompactionUsageRecorded`.
    compaction_start: HashMap<u64, Option<i64>>,
    /// A compaction whose timestamps could not all be read: the column is `null`.
    compaction_missing: bool,
}

impl Folder {
    fn new(retries: u64, context_window: Option<u64>) -> Self {
        Self {
            metrics: empty(retries, context_window),
            previous_ts: None,
            anchor_ready: false,
            opening: false,
            request_anchor: None,
            pending: HashMap::new(),
            denied_calls: HashSet::new(),
            denied_counted: HashSet::new(),
            route: None,
            routes: Vec::new(),
            compaction_start: HashMap::new(),
            compaction_missing: false,
        }
    }

    /// The route a `SessionCreated` or `ConfigChanged` record puts in force. Other records do
    /// nothing, so a caller can read the route from a record it does not count.
    fn learn_route(&mut self, record: &Value) {
        let event = record.get("event").and_then(Value::as_str).unwrap_or("");
        let data = record.get("data");
        match event {
            "SessionCreated" => {
                if let Some(route) = data
                    .and_then(|d| d.get("config_snapshot"))
                    .and_then(route_from_snapshot)
                {
                    self.route = Some(route);
                }
            }
            "ConfigChanged" => {
                if let Some(route) = data
                    .and_then(|d| d.get("change"))
                    .and_then(route_from_change)
                {
                    self.route = Some(route);
                }
            }
            _ => {}
        }
    }

    fn observe(&mut self, record: &Value) {
        let event = record.get("event").and_then(Value::as_str).unwrap_or("");
        let data = record.get("data");
        let ts = record.get("ts").and_then(Value::as_str);
        self.learn_route(record);
        // The records a response itself writes: the assistant message the provider returned
        // and the usage record that follows it (written only when the provider reported
        // usage). Neither moves the anchor of the request that produced it.
        let response_message = event == "MessageAppended" && is_assistant_message(data);
        match event {
            "ToolExecutionStarted" => {
                let call_id = data.and_then(|d| d.get("call_id")).and_then(Value::as_str);
                let name = data.and_then(|d| d.get("name")).and_then(Value::as_str);
                if let (Some(call_id), Some(name)) = (call_id, name) {
                    self.pending.insert(call_id.to_owned(), name.to_owned());
                }
            }
            "PermissionResolved" => {
                if data.and_then(|d| d.get("decision")).and_then(Value::as_str) == Some("deny")
                    && let Some(call_id) =
                        data.and_then(|d| d.get("call_id")).and_then(Value::as_str)
                {
                    self.denied_calls.insert(call_id.to_owned());
                    if let Some(name) = self.pending.get(call_id).cloned() {
                        self.metrics.tools.entry(name).or_default().denied += 1;
                        self.denied_counted.insert(call_id.to_owned());
                    }
                }
            }
            "ToolExecutionSettled" => {
                let call_id = data.and_then(|d| d.get("call_id")).and_then(Value::as_str);
                if let Some(call_id) = call_id
                    && let Some(name) = self.pending.remove(call_id)
                {
                    let denied = self.denied_calls.contains(call_id);
                    let new_denial = denied && self.denied_counted.insert(call_id.to_owned());
                    let tool = self.metrics.tools.entry(name).or_default();
                    let result = data.and_then(|d| d.get("result"));
                    tool.calls += 1;
                    if let Some(result) = result {
                        tool.result_bytes += result_text(result).len() as u64;
                        if result.get("is_error").and_then(Value::as_bool) == Some(true) {
                            if denied {
                                if new_denial {
                                    tool.denied += 1;
                                }
                            } else {
                                tool.failed += 1;
                            }
                        }
                    }
                }
            }
            "ModelUsageRecorded" | "CompactionUsageRecorded" => {}
            "ContextCompacted" => {
                self.metrics.compactions += 1;
                let revision = data.and_then(|d| d.get("revision")).and_then(Value::as_u64);
                let end = ts.and_then(parse_ts);
                let start = revision.and_then(|revision| self.compaction_start.get(&revision));
                // No usage record is the deterministic fallback: 0 seconds, like
                // `journal_metrics.py`'s `compaction_seconds`.
                if let Some(start) = start {
                    match (*start, end) {
                        (Some(start), Some(end)) => {
                            let elapsed = (end - start).max(0) / 1_000;
                            self.metrics.compaction_ms =
                                Some(self.metrics.compaction_ms.unwrap_or(0) + elapsed as u64);
                        }
                        _ => self.compaction_missing = true,
                    }
                }
            }
            "TurnFailed" => self.metrics.model_errors += 1,
            _ => {}
        }
        // The anchor rule and its event set. The anchor is the `ts` of the last journal record
        // written before the request was sent, so only the records that are written between a
        // response and the next request may move it; a record the host writes while a request
        // is already streaming is not before that request and must not move it.
        match event {
            "TurnStarted" => {
                // The turn's start is the anchor of its first request.
                self.anchor_ready = true;
                self.opening = true;
                self.request_anchor = ts.map(str::to_owned);
            }
            "ModelUsageRecorded" | "CompactionUsageRecorded" => {
                self.usage(event, data, ts);
                // The usage record is the response's last write. It closes the request being
                // timed and, after `usage` read the anchor, becomes the anchor itself for a
                // request that follows with no record in between (a `PauseTurn` continuation).
                self.anchor_ready = true;
                self.opening = false;
                self.request_anchor = ts.map(str::to_owned);
            }
            // The turn's end. A control request the driver serves between turns (a manual
            // `/compact`, ADR 0018) writes no record of its own, so the `TurnFinished` is the
            // last record before that request and must anchor it. It also opens the anchor for
            // a folded range whose first record is a `TurnFinished`.
            "TurnFinished" => {
                self.anchor_ready = true;
                self.opening = false;
                self.request_anchor = ts.map(str::to_owned);
            }
            // The response has begun. Its assistant message is the response's own first write
            // and never moves the anchor; the records after it belong to the tool work the
            // next request follows.
            "MessageAppended" if response_message => self.opening = false,
            // A transcript write that is not a response's assistant message: the turn's input,
            // a steer once applied as a message, a tool result, the max-tokens notice. Each is
            // written before a request, so a later request follows it. The turn's own opening
            // input is exempt: the first request is timed from its `TurnStarted`.
            "MessageAppended" => self.move_anchor(ts),
            // The previous response's tool work and a compaction's write sequence.
            // Deliberately absent are the records the host writes while a request is in
            // flight or outside the turn's request cycle: `InboxEnqueued` (a steer's enqueue
            // or a background-job notice, ADR 0037) and `BackgroundJobSettled`,
            // `ConfigChanged`, `PermissionResolved`, `Goal*`, `UserBash` and the turn's
            // opening (`InputClaimed`, `InputAdmitted`). None of them is the last record
            // before a request.
            "ToolExecutionStarted"
            | "ToolExecutionSettled"
            | "ContextCompacted"
            | "TranscriptReplaced"
            | "TranscriptReplacementCommitted"
            | "TranscriptReplacementAborted" => self.move_anchor(ts),
            _ => {}
        }
        self.previous_ts = ts.map(str::to_owned);
    }

    /// Moves the pending request's anchor to `ts`, unless this range has no anchor source yet
    /// (a range that began mid-turn: its first latency stays null) or the record belongs to the
    /// turn's opening, which does not move the `TurnStarted` anchor.
    fn move_anchor(&mut self, ts: Option<&str>) {
        if self.anchor_ready && !self.opening {
            self.request_anchor = ts.map(str::to_owned);
        }
    }

    fn usage(&mut self, event: &str, data: Option<&Value>, ts: Option<&str>) {
        let Some(data) = data else { return };
        let Some(usage) = data.get("usage") else {
            return;
        };
        self.metrics.requests += 1;
        let compaction = event == "CompactionUsageRecorded";
        if compaction {
            self.metrics.compaction_requests += 1;
        }
        let revision = data.get("revision").and_then(Value::as_u64);
        if let Some(revision) = revision
            && !self.compaction_start.contains_key(&revision)
        {
            // The summary call starts at the record before its first usage record.
            self.compaction_start
                .insert(revision, self.previous_ts.as_deref().and_then(parse_ts));
        }
        let (input_uncached, cache_read, prompt_total) = inputs(usage);
        add(&mut self.metrics.input_uncached, input_uncached);
        add(&mut self.metrics.cache_read, cache_read);
        let cache_write = usage.get("cache_write").and_then(Value::as_u64);
        add(&mut self.metrics.cache_write, cache_write);
        let output = usage.get("output").and_then(Value::as_u64);
        add(&mut self.metrics.output, output);
        let reasoning = reasoning_of(usage);
        add(&mut self.metrics.reasoning, reasoning);
        if let Some(total) = prompt_total {
            self.metrics.peak_context = Some(self.metrics.peak_context.unwrap_or(0).max(total));
        }
        let fee = data.get("fee");
        let fee_micros = fee.and_then(|f| f.get("micros")).and_then(Value::as_u64);
        add(&mut self.metrics.fee_micros, fee_micros);
        if fee
            .and_then(|f| f.get("incomplete"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            self.metrics.fee_incomplete = true;
        }
        let stop_reason = data.get("stop_reason").and_then(Value::as_str);
        if let Some(stop_reason) = stop_reason {
            let count = self.metrics.max_tokens_responses.unwrap_or(0)
                + u64::from(stop_reason == "max_tokens");
            self.metrics.max_tokens_responses = Some(count);
        }
        if let Some((provider, model)) = self.route.clone() {
            match self
                .routes
                .iter_mut()
                .find(|entry| entry.provider == provider && entry.model == model)
            {
                Some(entry) => entry.requests += 1,
                None => self.routes.push(ModelCount {
                    provider,
                    model,
                    requests: 1,
                }),
            }
        }
        let latency_ms = match (
            self.request_anchor.as_deref().and_then(parse_ts),
            ts.and_then(parse_ts),
        ) {
            (Some(start), Some(end)) => Some(((end - start).max(0) / 1_000) as u64),
            _ => None,
        };
        if self.metrics.request_log.len() < REQUEST_LOG_LIMIT as usize {
            self.metrics.request_log.push(json!({
                "ts": ts,
                "latency_ms": latency_ms,
                "kind": if compaction { "compaction" } else { "chat" },
                "input_uncached": input_uncached,
                "cache_read": cache_read,
                "cache_write": cache_write,
                "output": output,
                "reasoning": reasoning,
                "fee_micros": fee_micros,
                "stop_reason": stop_reason,
                "model": self.route.as_ref().map(|(_, model)| model.clone()),
            }));
        }
    }

    fn finish(mut self) -> RunMetrics {
        // A deterministic compaction has no summary call and takes 0 ms; no compaction at all
        // leaves the column `null`, as does a compaction whose timestamps could not be read.
        if self.metrics.compactions > 0 && !self.compaction_missing {
            self.metrics.compaction_ms = Some(self.metrics.compaction_ms.unwrap_or(0));
        } else {
            self.metrics.compaction_ms = None;
        }
        self.metrics.request_log_truncated =
            self.metrics.requests > self.metrics.request_log.len() as u64;
        self.metrics.models = std::mem::take(&mut self.routes);
        self.metrics
    }
}

/// The route a `SessionCreated.config_snapshot` names, as the configured provider name (never
/// the adapter kind). The exec host writes `provider_id` (the configured route) beside the
/// adapter kind in `provider`; a nested `model.provider`/`model.model` and the TUI's direct
/// top-level `provider`/`model` name the route too. An older exec snapshot carries only the
/// adapter kind, which is then the best available answer.
fn route_from_snapshot(snapshot: &Value) -> Option<(String, String)> {
    if let Some(model) = snapshot.get("model")
        && let (Some(provider), Some(name)) = (
            model.get("provider").and_then(Value::as_str),
            model.get("model").and_then(Value::as_str),
        )
    {
        return Some((provider.to_owned(), name.to_owned()));
    }
    let model = snapshot.get("model").and_then(Value::as_str)?;
    let provider = snapshot
        .get("provider_id")
        .and_then(Value::as_str)
        .or_else(|| snapshot.get("provider").and_then(Value::as_str))?;
    Some((provider.to_owned(), model.to_owned()))
}

/// The route a `ConfigChanged.change` names (`{"model": {"provider", "model"}}`), if it is a
/// model change at all. The value is `ModelRoute::to_value`: the configured route name.
fn route_from_change(change: &Value) -> Option<(String, String)> {
    let model = change.get("model")?;
    let provider = model.get("provider").and_then(Value::as_str)?;
    let name = model.get("model").and_then(Value::as_str)?;
    Some((provider.to_owned(), name.to_owned()))
}

/// `journal_metrics.py`'s `_radian_inputs`: (uncached, cache_read, prompt total).
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

/// The reasoning count inside a provider usage object, read like `journal_metrics.py`'s
/// `_reasoning`: `details` first, then the usage object itself, recursively.
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

/// The text parts of a `ToolExecutionSettled.result` concatenated, as `journal_metrics.py`
/// reads them (`{"text": {"text": ...}}` for radian content).
fn result_text(result: &Value) -> String {
    let Some(content) = result.get("content") else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    let Some(parts) = content.as_array() else {
        return String::new();
    };
    let mut text = String::new();
    for part in parts {
        if let Some(inner) = part
            .get("text")
            .and_then(|t| t.get("text"))
            .and_then(Value::as_str)
        {
            text.push_str(inner);
        } else if part.get("type").and_then(Value::as_str) == Some("text")
            && let Some(inner) = part.get("text").and_then(Value::as_str)
        {
            text.push_str(inner);
        }
    }
    text
}

fn add(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0).saturating_add(value));
    }
}

/// An RFC 3339 UTC stamp (`now_utc`'s `%Y-%m-%dT%H:%M:%S%.6fZ`) as microseconds since the
/// epoch. The journal's format is fixed, so this is a small parser rather than a date crate.
fn parse_ts(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes.get(10) != Some(&b'T') || bytes.last() != Some(&b'Z') {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> { text.get(range)?.parse().ok() };
    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    let fraction = &text[19..text.len() - 1];
    let micros = if fraction.is_empty() {
        0
    } else {
        let digits = fraction.strip_prefix('.')?;
        let mut micros = 0i64;
        let mut scale = 100_000i64;
        for digit in digits.chars().take(6) {
            micros += i64::from(digit.to_digit(10)?) * scale;
            scale /= 10;
        }
        micros
    };
    let days = days_from_civil(year, month, day);
    Some(((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000_000) + micros)
}

/// Days from 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric(requests: u64) -> RunMetrics {
        RunMetrics {
            requests,
            ..RunMetrics::default()
        }
    }

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

    fn route_record(seq: u64, ts: &str, route: &str) -> Value {
        json!({
            "schema_version": 1,
            "session_id": "s",
            "seq": seq,
            "ts": ts,
            "event": "SessionCreated",
            "critical": true,
            "data": {
                "workspace": "/w",
                "config_snapshot": {"model": {"provider": route, "model": "m-1"}},
            },
        })
    }

    /// Edge 1: two chat requests and one compaction request.
    #[test]
    fn two_chat_requests_and_one_compaction_request_are_counted_in_order() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:01Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10, "output": 1}}),
            ),
            record(
                2,
                "2026-10-31T00:00:02Z",
                "CompactionUsageRecorded",
                json!({"revision": 1, "usage": {"input": 20, "output": 2}}),
            ),
            record(
                3,
                "2026-10-31T00:00:03Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 30, "output": 3}}),
            ),
        ]);
        assert_eq!(metrics.requests, 3);
        assert_eq!(metrics.compaction_requests, 1);
        assert_eq!(metrics.request_log.len(), 3);
        assert_eq!(metrics.request_log[0]["kind"], "chat");
        assert_eq!(metrics.request_log[1]["kind"], "compaction");
        assert_eq!(metrics.request_log[2]["kind"], "chat");
        assert_eq!(metrics.input_uncached, Some(60));
    }

    /// Edge 2: `input_includes_cache: true` subtracts cache read and write; `false` uses input.
    #[test]
    fn a_marked_input_subtracts_the_cache_and_an_unmarked_one_does_not() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:01Z",
                "ModelUsageRecorded",
                json!({"usage": {
                    "input": 11_686, "cache_read": 4_000, "input_includes_cache": true,
                }}),
            ),
            record(
                2,
                "2026-10-31T00:00:02Z",
                "ModelUsageRecorded",
                json!({"usage": {
                    "input": 100, "cache_read": 40, "input_includes_cache": false,
                }}),
            ),
        ]);
        assert_eq!(metrics.input_uncached, Some(7_686 + 100));
        assert_eq!(metrics.cache_read, Some(4_040));
    }

    /// Edge 3: no usage record at all.
    #[test]
    fn a_run_with_no_usage_record_reports_zero_requests_and_no_counters() {
        let metrics = fold(&[route_record(0, "2026-10-31T00:00:00Z", "example")]);
        assert_eq!(metrics.requests, 0);
        assert_eq!(metrics.compaction_requests, 0);
        assert!(metrics.input_uncached.is_none());
        assert!(metrics.cache_read.is_none());
        assert!(metrics.cache_write.is_none());
        assert!(metrics.output.is_none());
        assert!(metrics.reasoning.is_none());
        assert!(metrics.fee_micros.is_none());
        assert!(metrics.peak_context.is_none());
        assert!(metrics.compaction_ms.is_none());
        assert!(metrics.request_log.is_empty());
    }

    /// Edge 4: the run range is the suffix after the tee prefix, not the whole journal.
    #[test]
    fn the_run_range_is_only_the_suffix_after_the_prefix() {
        let dir = std::env::temp_dir().join(format!("radian-metrics-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("run.jsonl");
        let events = [
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:01Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10, "output": 1}}),
            ),
            record(
                2,
                "2026-10-31T00:00:02Z",
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
        assert_eq!(
            run.models.len(),
            1,
            "the route from the prefix is still in force"
        );
        assert_eq!(run.models[0].model, "m-1");
        assert_eq!(run.request_log[0]["model"], "m-1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Edge 6: a `max_tokens` response appears in the count and its log entry.
    #[test]
    fn a_max_tokens_response_is_counted_and_logged() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:01Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10, "output": 1}, "stop_reason": "max_tokens"}),
            ),
            record(
                2,
                "2026-10-31T00:00:02Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 20, "output": 2}, "stop_reason": "end_turn"}),
            ),
        ]);
        assert_eq!(metrics.max_tokens_responses, Some(1));
        assert_eq!(metrics.request_log[0]["stop_reason"], "max_tokens");
        assert_eq!(metrics.request_log[1]["stop_reason"], "end_turn");
    }

    /// Edge 7: a denied call and a failed call of the same tool.
    #[test]
    fn a_denied_call_and_a_failed_call_are_counted_separately() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:01Z",
                "ToolExecutionStarted",
                json!({"call_id": "c1", "name": "shell", "arguments": {}}),
            ),
            record(
                2,
                "2026-10-31T00:00:02Z",
                "PermissionResolved",
                json!({"call_id": "c1", "decision": "deny"}),
            ),
            record(
                3,
                "2026-10-31T00:00:03Z",
                "ToolExecutionSettled",
                json!({"call_id": "c1", "result": {
                    "call_id": "c1", "is_error": true,
                    "content": [{"text": {"text": "denied"}}],
                }}),
            ),
            record(
                4,
                "2026-10-31T00:00:04Z",
                "ToolExecutionStarted",
                json!({"call_id": "c2", "name": "shell", "arguments": {}}),
            ),
            record(
                5,
                "2026-10-31T00:00:05Z",
                "ToolExecutionSettled",
                json!({"call_id": "c2", "result": {
                    "call_id": "c2", "is_error": true,
                    "content": [{"text": {"text": "boom"}}],
                }}),
            ),
        ]);
        let tool = metrics.tools.get("shell").expect("shell is counted");
        assert_eq!(tool.calls, 2);
        assert_eq!(tool.failed, 1);
        assert_eq!(tool.denied, 1);
        assert_eq!(
            tool.result_bytes,
            "denied".len() as u64 + "boom".len() as u64
        );
    }

    /// Edge 8: a `ConfigChanged` model switch; each request names its model.
    #[test]
    fn a_model_switch_splits_the_request_counts_and_names_each_models_log_entry() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "first"),
            record(
                1,
                "2026-10-31T00:00:01Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10}}),
            ),
            record(
                2,
                "2026-10-31T00:00:02Z",
                "ConfigChanged",
                json!({"revision": 1, "change": {"model": {"provider": "second", "model": "m-2"}}}),
            ),
            record(
                3,
                "2026-10-31T00:00:03Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 20}}),
            ),
            record(
                4,
                "2026-10-31T00:00:04Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 30}}),
            ),
        ]);
        assert_eq!(metrics.models.len(), 2);
        assert_eq!(metrics.models[0].provider, "first");
        assert_eq!(metrics.models[0].requests, 1);
        assert_eq!(metrics.models[1].provider, "second");
        assert_eq!(metrics.models[1].requests, 2);
        assert_eq!(metrics.request_log[0]["model"], "m-1");
        assert_eq!(metrics.request_log[1]["model"], "m-2");
    }

    /// Edge 9: 501 requests keep the first 500 and mark the log truncated.
    #[test]
    fn more_than_five_hundred_requests_truncate_the_log() {
        let mut events = vec![route_record(0, "2026-10-31T00:00:00Z", "example")];
        for index in 0..501u64 {
            events.push(record(
                index + 1,
                &format!("2026-10-31T00:{:02}:{:02}Z", index / 60, index % 60),
                "ModelUsageRecorded",
                json!({"usage": {"input": 1}}),
            ));
        }
        let metrics = fold(&events);
        assert_eq!(metrics.requests, 501);
        assert_eq!(metrics.request_log.len(), 500);
        assert!(metrics.request_log_truncated);
        assert_eq!(metrics.input_uncached, Some(501));
    }

    /// Edge 10: a torn final line is skipped and the rest folds.
    #[test]
    fn a_torn_final_line_is_skipped() {
        let mut text = String::new();
        text.push_str(
            &serde_json::to_string(&route_record(0, "2026-10-31T00:00:00Z", "example")).unwrap(),
        );
        text.push('\n');
        text.push_str(
            &serde_json::to_string(&record(
                1,
                "2026-10-31T00:00:01Z",
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

    /// Edge 11: two `ModelRetrying` events before a successful request.
    #[test]
    fn two_retries_are_reported() {
        let mut text = String::new();
        for line in [
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:01Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10}}),
            ),
        ] {
            text.push_str(&serde_json::to_string(&line).unwrap());
            text.push('\n');
        }
        let metrics = fold_bytes(text.as_bytes(), 2, None);
        assert_eq!(metrics.retries, 2);
    }

    /// Edge 13: one compaction of 15 s, two of 15 s and 5 s, and none.
    #[test]
    fn compaction_ms_sums_every_compactions_summary_call() {
        let one = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                2,
                "2026-10-31T00:00:01Z",
                "CompactionUsageRecorded",
                json!({"revision": 1, "usage": {"input": 10}}),
            ),
            record(
                3,
                "2026-10-31T00:00:15Z",
                "ContextCompacted",
                json!({"revision": 1}),
            ),
        ]);
        assert_eq!(one.compactions, 1);
        assert_eq!(one.compaction_ms, Some(15_000));

        let two = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:01Z",
                "CompactionUsageRecorded",
                json!({"revision": 1, "usage": {"input": 10}}),
            ),
            record(
                2,
                "2026-10-31T00:00:15Z",
                "ContextCompacted",
                json!({"revision": 1}),
            ),
            record(
                3,
                "2026-10-31T00:00:20Z",
                "TurnStarted",
                json!({"turn_id": "t-2"}),
            ),
            record(
                4,
                "2026-10-31T00:00:20Z",
                "CompactionUsageRecorded",
                json!({"revision": 2, "usage": {"input": 10}}),
            ),
            record(
                5,
                "2026-10-31T00:00:25Z",
                "ContextCompacted",
                json!({"revision": 2}),
            ),
        ]);
        assert_eq!(two.compactions, 2);
        assert_eq!(two.compaction_ms, Some(20_000));

        let none = fold(&[route_record(0, "2026-10-31T00:00:00Z", "example")]);
        assert!(none.compaction_ms.is_none());
    }

    /// A turn fold stops at its own boundaries (edge 5's unit-level twin).
    #[test]
    fn a_turn_fold_takes_only_that_turns_usage() {
        let dir = std::env::temp_dir().join(format!("radian-metrics-turn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("turn.jsonl");
        let events = [
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:01Z",
                "TurnStarted",
                json!({"turn_id": "t-1"}),
            ),
            record(
                2,
                "2026-10-31T00:00:02Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10}}),
            ),
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
            record(
                5,
                "2026-10-31T00:00:05Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 20}}),
            ),
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
        assert_eq!(first.models.len(), 1, "the route predates the turn");
        assert_eq!(first.request_log[0]["model"], "m-1");
        let second = for_turn(&path, "t-2", 0, None);
        assert_eq!(second.requests, 1);
        assert_eq!(second.input_uncached, Some(20));
        assert_eq!(second.models.len(), 1, "the route predates the turn");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The timestamp reader accepts the writer's format and rejects text that is not UTC.
    #[test]
    fn timestamps_read_as_microseconds_since_the_epoch() {
        assert_eq!(parse_ts("1970-01-01T00:00:00.000000Z"), Some(0));
        assert_eq!(parse_ts("1970-01-01T00:00:01Z"), Some(1_000_000));
        assert_eq!(
            parse_ts("2026-10-31T00:00:16.500000Z"),
            Some(((days_from_civil(2026, 10, 31) * 86_400 + 16) * 1_000_000) + 500_000)
        );
        assert_eq!(parse_ts("2026-10-31T00:00:00+00:00"), None);
        assert_eq!(parse_ts("not a time"), None);
    }

    /// A default value's key set is the fixture's field set (the optional truncation marker
    /// is absent).
    #[test]
    fn the_default_object_has_every_field_but_the_truncation_marker() {
        let value = metric(0).to_value();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert!(!keys.contains(&"request_log_truncated"));
        for key in [
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
            "compaction_ms",
            "max_tokens_responses",
            "model_errors",
            "retries",
            "tools",
            "models",
            "request_log",
        ] {
            assert!(keys.contains(&key), "{key} is missing");
        }
    }

    /// An assistant `MessageAppended` body; the response's own records.
    fn assistant(index: u64, ts: &str) -> Value {
        record(
            100 + index,
            ts,
            "MessageAppended",
            json!({"index": index, "message": {"assistant": {"content": []}}}),
        )
    }

    /// A tool-result `MessageAppended` body, written after its `ToolExecutionSettled`.
    fn tool_message(seq: u64, ts: &str, call_id: &str) -> Value {
        record(
            seq,
            ts,
            "MessageAppended",
            json!({"index": 9, "message": {"tool": {
                "result": {"call_id": call_id, "content": []},
            }}}),
        )
    }

    /// Edge 1: the first request of a turn anchors on its `TurnStarted`, not on the
    /// assistant message the same response wrote just before its usage record.
    #[test]
    fn a_first_request_latency_runs_from_the_turn_start() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:00Z",
                "TurnStarted",
                json!({"turn_id": "t-1"}),
            ),
            assistant(0, "2026-10-31T00:00:03Z"),
            record(
                3,
                "2026-10-31T00:00:03.050000Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10, "output": 1}}),
            ),
        ]);
        assert_eq!(metrics.request_log[0]["latency_ms"], 3050);
    }

    /// The input append (and any other record the turn's opening writes) comes after
    /// `TurnStarted` but is still that start: the first request is timed from the turn start,
    /// not from its own input message.
    #[test]
    fn the_turns_opening_input_keeps_the_turn_start_anchor() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:00Z",
                "TurnStarted",
                json!({"turn_id": "t-1"}),
            ),
            record(
                2,
                "2026-10-31T00:00:01Z",
                "MessageAppended",
                json!({"index": 0, "message": {"user": {"content": []}}}),
            ),
            record(
                3,
                "2026-10-31T00:00:01.100000Z",
                "InputAdmitted",
                json!({"message_id": "m-1"}),
            ),
            assistant(1, "2026-10-31T00:00:03Z"),
            record(
                5,
                "2026-10-31T00:00:03.050000Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10}}),
            ),
        ]);
        assert_eq!(
            metrics.request_log[0]["latency_ms"], 3050,
            "the anchor is the turn start, not the 1 s input message"
        );
    }

    /// Edge 2: a later request anchors on the record that settled the previous response's
    /// tool call, so the latency is the model's whole response time and not the 100 ms gap
    /// between the assistant message and its usage record.
    #[test]
    fn a_later_request_latency_runs_from_the_last_tool_settle() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:00Z",
                "TurnStarted",
                json!({"turn_id": "t-1"}),
            ),
            record(
                2,
                "2026-10-31T00:00:00Z",
                "MessageAppended",
                json!({"index": 0, "message": {"user": {"content": []}}}),
            ),
            record(
                3,
                "2026-10-31T00:00:01Z",
                "MessageAppended",
                json!({"index": 1, "message": {"assistant": {
                    "content": [],
                    "tool_calls": [{"id": "c1", "name": "shell", "arguments": {}}],
                }}}),
            ),
            record(
                4,
                "2026-10-31T00:00:01Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10}}),
            ),
            record(
                5,
                "2026-10-31T00:00:02Z",
                "ToolExecutionStarted",
                json!({"call_id": "c1", "name": "shell", "arguments": {}}),
            ),
            record(
                6,
                "2026-10-31T00:00:10Z",
                "ToolExecutionSettled",
                json!({"call_id": "c1", "result": {"call_id": "c1", "content": []}}),
            ),
            tool_message(7, "2026-10-31T00:00:10Z", "c1"),
            assistant(2, "2026-10-31T00:00:14.900000Z"),
            record(
                9,
                "2026-10-31T00:00:15Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 20}}),
            ),
        ]);
        assert_eq!(metrics.request_log.len(), 2);
        assert_eq!(metrics.request_log[1]["latency_ms"], 5000);
    }

    /// Edge 3: a response with several tool calls anchors on the last settle of the batch.
    #[test]
    fn a_batchs_anchor_is_its_last_tool_settle() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:00Z",
                "TurnStarted",
                json!({"turn_id": "t-1"}),
            ),
            record(
                2,
                "2026-10-31T00:00:00Z",
                "MessageAppended",
                json!({"index": 0, "message": {"user": {"content": []}}}),
            ),
            assistant(1, "2026-10-31T00:00:01Z"),
            record(
                4,
                "2026-10-31T00:00:01Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10}}),
            ),
            record(
                5,
                "2026-10-31T00:00:02Z",
                "ToolExecutionStarted",
                json!({"call_id": "c1", "name": "shell", "arguments": {}}),
            ),
            record(
                6,
                "2026-10-31T00:00:02Z",
                "ToolExecutionStarted",
                json!({"call_id": "c2", "name": "shell", "arguments": {}}),
            ),
            record(
                7,
                "2026-10-31T00:00:02Z",
                "ToolExecutionStarted",
                json!({"call_id": "c3", "name": "shell", "arguments": {}}),
            ),
            record(
                8,
                "2026-10-31T00:00:10Z",
                "ToolExecutionSettled",
                json!({"call_id": "c1", "result": {"call_id": "c1", "content": []}}),
            ),
            tool_message(9, "2026-10-31T00:00:10Z", "c1"),
            record(
                10,
                "2026-10-31T00:00:11Z",
                "ToolExecutionSettled",
                json!({"call_id": "c2", "result": {"call_id": "c2", "content": []}}),
            ),
            tool_message(11, "2026-10-31T00:00:11Z", "c2"),
            record(
                12,
                "2026-10-31T00:00:12Z",
                "ToolExecutionSettled",
                json!({"call_id": "c3", "result": {"call_id": "c3", "content": []}}),
            ),
            tool_message(13, "2026-10-31T00:00:12Z", "c3"),
            assistant(2, "2026-10-31T00:00:19.900000Z"),
            record(
                15,
                "2026-10-31T00:00:20Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 20}}),
            ),
        ]);
        assert_eq!(
            metrics.request_log[1]["latency_ms"], 8000,
            "the anchor is the 12 s settle, not the 10 s one"
        );
    }

    /// Edge 4: a compaction request between two chat requests anchors on the last record
    /// written before it, and the chat request after the compaction anchors on the
    /// compaction's own records rather than on the assistant message it produced.
    #[test]
    fn a_compaction_request_anchors_on_the_record_before_it() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:00Z",
                "TurnStarted",
                json!({"turn_id": "t-1"}),
            ),
            record(
                2,
                "2026-10-31T00:00:00Z",
                "MessageAppended",
                json!({"index": 0, "message": {"user": {"content": []}}}),
            ),
            assistant(1, "2026-10-31T00:00:01Z"),
            record(
                4,
                "2026-10-31T00:00:01Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10}}),
            ),
            record(
                5,
                "2026-10-31T00:00:01.100000Z",
                "ToolExecutionStarted",
                json!({"call_id": "c1", "name": "shell", "arguments": {}}),
            ),
            record(
                6,
                "2026-10-31T00:00:02Z",
                "ToolExecutionSettled",
                json!({"call_id": "c1", "result": {"call_id": "c1", "content": []}}),
            ),
            tool_message(7, "2026-10-31T00:00:02Z", "c1"),
            record(
                8,
                "2026-10-31T00:00:05Z",
                "CompactionUsageRecorded",
                json!({"revision": 1, "usage": {"input": 50}}),
            ),
            record(
                9,
                "2026-10-31T00:00:06Z",
                "ContextCompacted",
                json!({"revision": 1}),
            ),
            assistant(2, "2026-10-31T00:00:10Z"),
            record(
                11,
                "2026-10-31T00:00:10.200000Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 60}}),
            ),
        ]);
        assert_eq!(metrics.request_log[1]["kind"], "compaction");
        assert_eq!(
            metrics.request_log[1]["latency_ms"], 3000,
            "the compaction request anchors on the 2 s tool settle"
        );
        assert_eq!(metrics.request_log[2]["kind"], "chat");
        assert_eq!(
            metrics.request_log[2]["latency_ms"], 4200,
            "the chat request after the compaction anchors on its 6 s commit"
        );
    }

    /// A `/compact` served between turns follows the previous turn's `TurnFinished`, which is
    /// then the last record written before its request; the anchor must not stay on the
    /// previous turn's final usage record.
    #[test]
    fn a_compaction_after_a_turn_anchors_on_the_turns_end() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:00Z",
                "TurnStarted",
                json!({"turn_id": "t-1"}),
            ),
            assistant(2, "2026-10-31T00:00:01Z"),
            record(
                3,
                "2026-10-31T00:00:01Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10}}),
            ),
            record(
                4,
                "2026-10-31T00:00:05Z",
                "TurnFinished",
                json!({"turn_id": "t-1", "outcome": "completed"}),
            ),
            // The user waited a minute and then ran `/compact`; the compaction request has
            // no journal record of its own, so the turn's end is the last write before it.
            record(
                5,
                "2026-10-31T00:01:05Z",
                "CompactionUsageRecorded",
                json!({"revision": 1, "usage": {"input": 50}}),
            ),
            record(
                6,
                "2026-10-31T00:01:06Z",
                "ContextCompacted",
                json!({"revision": 1}),
            ),
        ]);
        assert_eq!(metrics.request_log.len(), 2);
        assert_eq!(metrics.request_log[0]["latency_ms"], 1000);
        assert_eq!(metrics.request_log[1]["kind"], "compaction");
        assert_eq!(
            metrics.request_log[1]["latency_ms"], 60000,
            "the anchor is the 5 s TurnFinished, not the 1 s usage record"
        );
    }

    /// Edge 5: a range with no `TurnStarted` has no anchor for its first request, so its
    /// latency is null; later requests are measured from the records after it.
    #[test]
    fn a_journal_without_a_turn_start_measures_only_later_requests() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:01Z",
                "MessageAppended",
                json!({"index": 0, "message": {"user": {"content": []}}}),
            ),
            assistant(1, "2026-10-31T00:00:03Z"),
            record(
                3,
                "2026-10-31T00:00:03.050000Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10}}),
            ),
            record(
                4,
                "2026-10-31T00:00:03.100000Z",
                "ToolExecutionStarted",
                json!({"call_id": "c1", "name": "shell", "arguments": {}}),
            ),
            record(
                5,
                "2026-10-31T00:00:10Z",
                "ToolExecutionSettled",
                json!({"call_id": "c1", "result": {"call_id": "c1", "content": []}}),
            ),
            tool_message(6, "2026-10-31T00:00:10Z", "c1"),
            assistant(2, "2026-10-31T00:00:14.900000Z"),
            record(
                8,
                "2026-10-31T00:00:15Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 20}}),
            ),
        ]);
        assert_eq!(metrics.request_log[0]["latency_ms"], Value::Null);
        assert_eq!(metrics.request_log[1]["latency_ms"], 5000);
    }

    /// Edge 6: a retried request writes nothing for its failed attempts (ADR 0010: the
    /// `ModelRetrying` notices are live events), so its latency runs from the record before
    /// the first attempt through the backoff and the failed attempts to the successful
    /// response's usage record. The wrong (old) anchor would report 0 ms here.
    #[test]
    fn a_retried_request_spans_the_retries_and_the_failed_attempts() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:00Z",
                "TurnStarted",
                json!({"turn_id": "t-1"}),
            ),
            record(
                2,
                "2026-10-31T00:00:00Z",
                "MessageAppended",
                json!({"index": 0, "message": {"user": {"content": []}}}),
            ),
            assistant(1, "2026-10-31T00:00:01Z"),
            record(
                4,
                "2026-10-31T00:00:01Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10}}),
            ),
            record(
                5,
                "2026-10-31T00:00:01.100000Z",
                "ToolExecutionStarted",
                json!({"call_id": "c1", "name": "shell", "arguments": {}}),
            ),
            record(
                6,
                "2026-10-31T00:00:10Z",
                "ToolExecutionSettled",
                json!({"call_id": "c1", "result": {"call_id": "c1", "content": []}}),
            ),
            tool_message(7, "2026-10-31T00:00:10Z", "c1"),
            // The next request fails twice before it succeeds; only its success is written.
            assistant(2, "2026-10-31T00:00:25Z"),
            record(
                9,
                "2026-10-31T00:00:25Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 20}}),
            ),
        ]);
        assert_eq!(metrics.request_log[1]["latency_ms"], 15000);
    }

    /// A response whose provider reported no usage writes no usage record, so it never
    /// closes the turn's first request by itself. The records after its assistant message
    /// (its tools' starts, settles and result messages) still anchor the next request.
    #[test]
    fn a_response_without_usage_ends_at_the_record_after_its_assistant_message() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:00Z",
                "TurnStarted",
                json!({"turn_id": "t-1"}),
            ),
            record(
                2,
                "2026-10-31T00:00:00Z",
                "MessageAppended",
                json!({"index": 0, "message": {"user": {"content": []}}}),
            ),
            assistant(1, "2026-10-31T00:00:03Z"),
            record(
                4,
                "2026-10-31T00:00:03.100000Z",
                "ToolExecutionStarted",
                json!({"call_id": "c1", "name": "shell", "arguments": {}}),
            ),
            record(
                5,
                "2026-10-31T00:00:10Z",
                "ToolExecutionSettled",
                json!({"call_id": "c1", "result": {"call_id": "c1", "content": []}}),
            ),
            tool_message(6, "2026-10-31T00:00:10Z", "c1"),
            assistant(2, "2026-10-31T00:00:14.900000Z"),
            record(
                8,
                "2026-10-31T00:00:15Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 20}}),
            ),
        ]);
        assert_eq!(metrics.request_log.len(), 1);
        assert_eq!(
            metrics.request_log[0]["latency_ms"], 5000,
            "the anchor is the 10 s settle, not the turn start"
        );
    }

    /// A `PauseTurn` response is followed by another request with no record in between: the
    /// last record before that request is the first response's own usage record, so it is
    /// the anchor even though it belongs to the response.
    #[test]
    fn a_request_right_after_a_usage_record_anchors_on_it() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:00Z",
                "TurnStarted",
                json!({"turn_id": "t-1"}),
            ),
            record(
                2,
                "2026-10-31T00:00:00Z",
                "MessageAppended",
                json!({"index": 0, "message": {"user": {"content": []}}}),
            ),
            assistant(1, "2026-10-31T00:00:01Z"),
            record(
                4,
                "2026-10-31T00:00:01Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10}}),
            ),
            assistant(2, "2026-10-31T00:00:05Z"),
            record(
                6,
                "2026-10-31T00:00:05Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 20}}),
            ),
        ]);
        assert_eq!(metrics.request_log.len(), 2);
        assert_eq!(metrics.request_log[0]["latency_ms"], 1000);
        assert_eq!(
            metrics.request_log[1]["latency_ms"], 4000,
            "the anchor is the first usage record, not the turn start"
        );
    }

    /// An inbox record the host writes while a request is already streaming (a steer's
    /// enqueue, a background job's completion notice or settle) is not the last record before
    /// that request, so it must not become the response's anchor.
    #[test]
    fn an_inbox_enqueue_during_a_response_does_not_move_the_anchor() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:00Z",
                "TurnStarted",
                json!({"turn_id": "t-1"}),
            ),
            record(
                2,
                "2026-10-31T00:00:00Z",
                "MessageAppended",
                json!({"index": 0, "message": {"user": {"content": []}}}),
            ),
            assistant(1, "2026-10-31T00:00:01Z"),
            record(
                4,
                "2026-10-31T00:00:01Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10}}),
            ),
            record(
                5,
                "2026-10-31T00:00:01.100000Z",
                "ToolExecutionStarted",
                json!({"call_id": "c1", "name": "shell", "arguments": {}}),
            ),
            record(
                6,
                "2026-10-31T00:00:03Z",
                "ToolExecutionSettled",
                json!({"call_id": "c1", "result": {"call_id": "c1", "content": []}}),
            ),
            tool_message(7, "2026-10-31T00:00:03Z", "c1"),
            // Both land while the next request streams, after it was sent at 3 s.
            record(
                8,
                "2026-10-31T00:00:05.800000Z",
                "InboxEnqueued",
                json!({"message_id": "m-1", "source": "System", "lane": "next_step"}),
            ),
            record(
                9,
                "2026-10-31T00:00:05.850000Z",
                "BackgroundJobSettled",
                json!({"job_id": "job-1", "state": "exited 0"}),
            ),
            assistant(2, "2026-10-31T00:00:05.900000Z"),
            record(
                11,
                "2026-10-31T00:00:06Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 20}}),
            ),
        ]);
        assert_eq!(metrics.request_log.len(), 2);
        assert_eq!(
            metrics.request_log[1]["latency_ms"], 3000,
            "the anchor is the 3 s tool result, not the 5.8 s notice"
        );
    }

    /// A steer is an anchor only once it is applied as a user `MessageAppended` before the
    /// request; that message moves the anchor as any other pre-request record does.
    #[test]
    fn a_steer_applied_between_requests_moves_the_anchor() {
        let metrics = fold(&[
            route_record(0, "2026-10-31T00:00:00Z", "example"),
            record(
                1,
                "2026-10-31T00:00:00Z",
                "TurnStarted",
                json!({"turn_id": "t-1"}),
            ),
            assistant(1, "2026-10-31T00:00:01Z"),
            record(
                3,
                "2026-10-31T00:00:01Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 10}}),
            ),
            record(
                4,
                "2026-10-31T00:00:01.100000Z",
                "ToolExecutionStarted",
                json!({"call_id": "c1", "name": "shell", "arguments": {}}),
            ),
            record(
                5,
                "2026-10-31T00:00:03Z",
                "ToolExecutionSettled",
                json!({"call_id": "c1", "result": {"call_id": "c1", "content": []}}),
            ),
            tool_message(6, "2026-10-31T00:00:03Z", "c1"),
            // The steer applied at the start of the next step, before its request.
            record(
                7,
                "2026-10-31T00:00:04Z",
                "MessageAppended",
                json!({"index": 3, "message": {"user": {"content": []}}}),
            ),
            assistant(2, "2026-10-31T00:00:07.900000Z"),
            record(
                9,
                "2026-10-31T00:00:08Z",
                "ModelUsageRecorded",
                json!({"usage": {"input": 20}}),
            ),
        ]);
        assert_eq!(metrics.request_log.len(), 2);
        assert_eq!(
            metrics.request_log[1]["latency_ms"], 4000,
            "the applied steer at 4 s is the anchor"
        );
    }
}
