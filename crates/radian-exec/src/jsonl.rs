//! `radian exec --output jsonl` (ADR 0044): the machine-readable stream, the exit-code
//! map, and the closing `Result` record.
//!
//! In this mode stdout carries only JSON lines: every durable journal line byte-identical to
//! the session file, live-only delta lines marked `"live": true`, and one closing `Result`
//! line after the driver has shut down. Default output is unchanged byte for byte: without
//! `--output` the model text still streams raw and no JSON line is printed. Status, hints,
//! reasoning (in default mode) and errors stay on stderr in every mode.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use radian_core::{CancelReason, IncompleteReason, ModelError, RunStatus};
use tokio::sync::mpsc::UnboundedReceiver;

/// The machine-readable output mode. Only `jsonl` exists: one JSON object per stdout line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum OutputMode {
    /// Journal lines verbatim, live-only delta lines, and a closing `Result` record.
    Jsonl,
}

/// Reads the `-p -` prompt: stdin to EOF. A literal `-` prompt is not possible.
pub(crate) fn read_prompt(mut input: impl Read) -> std::io::Result<String> {
    let mut text = String::new();
    input.read_to_string(&mut text)?;
    Ok(text)
}

/// How an `exec` run ended, for the `Result` record and the exit code. The code is decided
/// where the run stops (the signal paths keep their historical codes); this only names the
/// outcome and the stop reason.
#[derive(Debug, Clone)]
pub(crate) enum ExecEnd {
    /// A turn ran to a Core status. `None` means the run stopped before any status was
    /// recorded (a shutdown failure, or a cancellation while idle): a failed run.
    Status(Option<RunStatus>),
    /// No turn ever ran (the input was refused before it started): a failed run.
    NotStarted,
    /// A clean interactive leave with no turn: the session simply ended.
    NoTurns,
    /// SIGTERM: the process was asked to stop (128 + 15, unchanged).
    Terminated,
    /// The second Ctrl-C during a wind-down (130, unchanged).
    Forced,
    /// The driver stopped on its own. `ok` tells whether it ended cleanly.
    DriverEnded { code: i32, ok: bool },
    /// The wake gate closed with notices no turn will ever run: exit 3, the budget
    /// family (ADR 0044). `unhandled` counts degraded job notices, `unhandled_stop`
    /// counts degraded Stop verdicts.
    WakeBudget { unhandled: u64, unhandled_stop: u64 },
    /// A job completion notice's enqueue failed every attempt (ADR 0037 decision 5's
    /// failure rule): exit 3, the budget family (ADR 0044). `lost` counts the notices
    /// no turn will ever run for. Never 0: `lost_end` only builds it when one was lost.
    NoticeLost { lost: u64 },
}

/// The exit code for a Core turn status (all `exec` modes, not only jsonl). `RunStatus` is
/// `#[non_exhaustive]`, so the wildcard arm maps any future status to 1.
pub(crate) fn exit_code_for_status(status: &RunStatus) -> i32 {
    match status {
        RunStatus::Completed => 0,
        RunStatus::Failed | RunStatus::Poisoned => 1,
        RunStatus::BudgetExceeded => 3,
        RunStatus::Cancelled(CancelReason::DeadlineExceeded) => 3,
        RunStatus::Cancelled(CancelReason::Requested) => 130,
        RunStatus::Incomplete(_) | RunStatus::Refused => 4,
        _ => 1,
    }
}

/// The `Result` outcome name for a Core turn status: the ADR 0006 §5.6 wire vocabulary.
pub(crate) fn outcome_name(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Completed => "completed",
        RunStatus::Refused => "refused",
        RunStatus::BudgetExceeded => "budget_exceeded",
        RunStatus::Failed => "failed",
        RunStatus::Poisoned => "poisoned",
        RunStatus::Incomplete(_) => "incomplete",
        RunStatus::Cancelled(_) => "cancelled",
        _ => "unknown",
    }
}

/// The `Result` stop reason: the detail behind the outcome.
pub(crate) fn stop_reason(status: &RunStatus) -> String {
    match status {
        RunStatus::Completed => "completed".to_owned(),
        RunStatus::Failed => "failed".to_owned(),
        RunStatus::BudgetExceeded => "budget".to_owned(),
        RunStatus::Cancelled(CancelReason::Requested) => "cancelled".to_owned(),
        RunStatus::Cancelled(CancelReason::DeadlineExceeded) => "deadline".to_owned(),
        RunStatus::Incomplete(IncompleteReason::MaxTokens) => "max_tokens".to_owned(),
        RunStatus::Incomplete(IncompleteReason::ContentFiltered) => "content_filtered".to_owned(),
        RunStatus::Incomplete(IncompleteReason::ToolUseWithoutCalls) => {
            "tool_use_without_calls".to_owned()
        }
        RunStatus::Incomplete(IncompleteReason::UnknownStopReason(raw)) => {
            format!("unknown:{raw}")
        }
        RunStatus::Refused => "refused".to_owned(),
        // The session died abnormally (an in-flight turn future was dropped).
        RunStatus::Poisoned => "terminated".to_owned(),
        // A future status this build does not name: exit 1, reported as a failure.
        _ => "failed".to_owned(),
    }
}

impl ExecEnd {
    /// The `Result` outcome, exit code and stop reason for how the run ended.
    pub(crate) fn result_fields(&self) -> (&'static str, i32, String) {
        match self {
            ExecEnd::Status(Some(status)) => (
                outcome_name(status),
                exit_code_for_status(status),
                stop_reason(status),
            ),
            // The run stopped before any status was recorded, or never started a turn.
            ExecEnd::Status(None) | ExecEnd::NotStarted => ("failed", 1, "failed".to_owned()),
            ExecEnd::NoTurns => ("completed", 0, "completed".to_owned()),
            ExecEnd::Terminated => ("cancelled", 128 + 15, "terminated".to_owned()),
            ExecEnd::Forced => ("cancelled", 130, "cancelled".to_owned()),
            ExecEnd::DriverEnded { code, ok } => {
                let (outcome, reason) = if *ok {
                    ("completed", "completed")
                } else {
                    ("failed", "failed")
                };
                (outcome, *code, reason.to_owned())
            }
            // The wake budget is used up: budget outcome, budget exit, its own reason.
            ExecEnd::WakeBudget { .. } => ("budget_exceeded", 3, "wake_budget".to_owned()),
            // A lost completion notice: budget outcome, budget exit, its own reason.
            ExecEnd::NoticeLost { .. } => ("budget_exceeded", 3, "job_notice_lost".to_owned()),
        }
    }

    /// How many job notices never ran, for the `Result` record. `Some` only on the wake-
    /// budget path; every other ending serializes without the field, so their lines are
    /// unchanged.
    pub(crate) fn unhandled_job_notices(&self) -> Option<u64> {
        match self {
            ExecEnd::WakeBudget { unhandled, .. } => Some(*unhandled),
            _ => None,
        }
    }

    /// How many completion notices were lost to failed enqueues, for the `Result`
    /// record. `Some` only on the lost-notice path; every other ending serializes
    /// without the field, so their lines are unchanged.
    pub(crate) fn lost_notices(&self) -> Option<u64> {
        match self {
            ExecEnd::NoticeLost { lost } => Some(*lost),
            _ => None,
        }
    }

    /// How many degraded Stop verdicts never ran, for the `Result` record. `Some` only
    /// on the wake-budget path with a nonzero count; every other ending — and a
    /// job-only wake-budget ending — serializes without the field, so their lines are
    /// unchanged.
    pub(crate) fn unhandled_stop_notices(&self) -> Option<u64> {
        match self {
            ExecEnd::WakeBudget { unhandled_stop, .. } if *unhandled_stop > 0 => {
                Some(*unhandled_stop)
            }
            _ => None,
        }
    }
}

/// One live-only delta line: `{"live": true, "event": ..., "data": {"text": ...}}`. The
/// `event`/`data` shape matches the journal envelope (ADR 0006); `live` marks a line that
/// was never journaled. Live lines are not ordered against durable lines.
pub(crate) fn live_line(event: &str, text: &str) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&serde_json::json!({
        "live": true,
        "event": event,
        "data": { "text": text },
    }))
    .expect("a live line is plain JSON and cannot fail to serialize");
    bytes.push(b'\n');
    bytes
}

/// The session totals folded from the journal file after the driver has shut down. Counters
/// no record reported stay `None` ("not reported", never 0, per ADR 0007 J1).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct JournalSummary {
    /// How many `TurnFinished` lines the file holds (every turn this session ran, including
    /// turns from before a `--resume`).
    pub(crate) turns: u64,
    pub(crate) input: Option<u64>,
    pub(crate) output: Option<u64>,
    pub(crate) cache_read: Option<u64>,
    pub(crate) cache_write: Option<u64>,
    /// Summed `fee.micros`, when at least one usage record carried a fee.
    pub(crate) fee_micros: Option<u64>,
    /// Whether any recorded fee was incomplete (a billable counter had no rate).
    pub(crate) fee_incomplete: bool,
    /// The last assistant message's text parts concatenated, or empty when the session has
    /// no assistant message yet.
    pub(crate) last_text: String,
}

/// Folds one journal file into its [`JournalSummary`]. Unparseable lines are skipped: a torn
/// tail must not stop the `Result` record.
pub(crate) fn summarize_journal(path: &Path) -> JournalSummary {
    let Ok(bytes) = std::fs::read(path) else {
        return JournalSummary::default();
    };
    fold_bytes(&bytes)
}

/// Folds only the bytes this process wrote: the journal suffix after `prefix_len`, the
/// `set_tee` return that separates resumed history from this run. The prefix is whole
/// lines (install and every write take the same lock), so the suffix starts on a line
/// boundary; a short slice still folds by skipping what does not parse, exactly like the
/// full fold.
pub(crate) fn summarize_journal_from(path: &Path, prefix_len: u64) -> JournalSummary {
    let Ok(bytes) = std::fs::read(path) else {
        return JournalSummary::default();
    };
    let start = (prefix_len as usize).min(bytes.len());
    fold_bytes(&bytes[start..])
}

/// Folds the records of **one turn** into a [`JournalSummary`]: the usage records and
/// assistant messages between the `TurnStarted` and `TurnFinished` with `turn_id`. A prompt's
/// `_meta` reports the turn that answered it (ADR 0049), so a later turn's fees and text must
/// not leak into it, and a turn that ran before it must not stand in for it.
pub(crate) fn summarize_turn(path: &Path, turn_id: &str) -> JournalSummary {
    let Ok(bytes) = std::fs::read(path) else {
        return JournalSummary::default();
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut summary = JournalSummary::default();
    let mut inside = false;
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(event) = record.get("event").and_then(|e| e.as_str()) else {
            continue;
        };
        let data = record.get("data");
        match event {
            "TurnStarted" => {
                if turn_of(data) == Some(turn_id) {
                    inside = true;
                }
            }
            "TurnFinished" => {
                if turn_of(data) == Some(turn_id) {
                    summary.turns += 1;
                    inside = false;
                }
            }
            _ if inside => fold_record(&mut summary, event, data),
            _ => {}
        }
    }
    summary
}

/// The `turn_id` of a `TurnStarted`/`TurnFinished` body.
fn turn_of(data: Option<&serde_json::Value>) -> Option<&str> {
    data.and_then(|d| d.get("turn_id")).and_then(|t| t.as_str())
}

/// Folds journal bytes into a [`JournalSummary`]. Unparseable lines are skipped.
fn fold_bytes(bytes: &[u8]) -> JournalSummary {
    let mut summary = JournalSummary::default();
    let text = String::from_utf8_lossy(bytes);
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(event) = record.get("event").and_then(|e| e.as_str()) else {
            continue;
        };
        fold_record(&mut summary, event, record.get("data"));
    }
    summary
}

/// Folds one record into a summary. Unparseable lines are skipped by the callers.
fn fold_record(summary: &mut JournalSummary, event: &str, data: Option<&serde_json::Value>) {
    match event {
        "TurnFinished" => summary.turns += 1,
        "ModelUsageRecorded" | "CompactionUsageRecorded" => {
            if let Some(usage) = data.and_then(|d| d.get("usage")) {
                add_counter(&mut summary.input, usage.get("input"));
                add_counter(&mut summary.output, usage.get("output"));
                add_counter(&mut summary.cache_read, usage.get("cache_read"));
                add_counter(&mut summary.cache_write, usage.get("cache_write"));
            }
            if let Some(fee) = data.and_then(|d| d.get("fee"))
                && let Some(micros) = fee.get("micros").and_then(|m| m.as_u64())
            {
                summary.fee_micros = Some(summary.fee_micros.unwrap_or(0).saturating_add(micros));
                if fee.get("incomplete").and_then(|i| i.as_bool()) == Some(true) {
                    summary.fee_incomplete = true;
                }
            }
        }
        "MessageAppended" => {
            if let Some(text) = data
                .and_then(|d| d.get("message"))
                .and_then(|m| m.get("assistant"))
                .and_then(|a| a.get("content"))
                .and_then(|c| c.as_array())
                .map(|content| assistant_text(content))
                && !text.is_empty()
            {
                summary.last_text = text;
            }
        }
        _ => {}
    }
}

/// Adds a reported usage counter to a session total. A missing or misshapen field is "not
/// reported" and leaves the total alone.
fn add_counter(total: &mut Option<u64>, value: Option<&serde_json::Value>) {
    if let Some(count) = value.and_then(|v| v.as_u64()) {
        *total = Some(total.unwrap_or(0).saturating_add(count));
    }
}

/// The text parts of one assistant message concatenated. Non-text parts (images, opaque
/// provider blocks) are not text and are skipped.
fn assistant_text(content: &[serde_json::Value]) -> String {
    let mut text = String::new();
    for part in content {
        if let Some(part_text) = part
            .get("text")
            .and_then(|t| t.get("text"))
            .and_then(|t| t.as_str())
        {
            text.push_str(part_text);
        }
    }
    text
}

/// Builds the closing `Result` line. `cost_estimated` comes from the live projection (a
/// restored call was re-priced with today's catalog); everything else session-wide is folded
/// from the journal, so it covers resumed history too. The `run_*` triple folds only the
/// suffix after the tee prefix: this process's own lines. Its `estimated` is always false:
/// the resume re-pricing writes no journal record (it only folds in memory before the tee
/// is installed), so the suffix never holds a re-priced old record — its fees are
/// contemporary. The wake-budget ending adds `unhandled_job_notices`; every other ending
/// omits it, so their field set is the fixture's field set. `model_error` is present only
/// when the run ended because a turn failed on a `ModelError` (ADR 0044 §2b).
pub(crate) fn result_line(
    session_id: &str,
    end: &ExecEnd,
    summary: &JournalSummary,
    run_summary: &JournalSummary,
    cost_estimated: bool,
    model_error: Option<&ModelError>,
) -> Vec<u8> {
    let (outcome, exit_code, reason) = end.result_fields();
    let mut usage = serde_json::Map::new();
    for (name, value) in [
        ("input", summary.input),
        ("output", summary.output),
        ("cache_read", summary.cache_read),
        ("cache_write", summary.cache_write),
    ] {
        if let Some(count) = value {
            usage.insert(name.to_owned(), count.into());
        }
    }
    let mut run_usage = serde_json::Map::new();
    for (name, value) in [
        ("input", run_summary.input),
        ("output", run_summary.output),
        ("cache_read", run_summary.cache_read),
        ("cache_write", run_summary.cache_write),
    ] {
        if let Some(count) = value {
            run_usage.insert(name.to_owned(), count.into());
        }
    }
    let cost = summary.fee_micros.map(|micros| {
        serde_json::json!({
            "micros": micros,
            "incomplete": summary.fee_incomplete,
            "estimated": cost_estimated,
        })
    });
    let run_cost = run_summary.fee_micros.map(|micros| {
        serde_json::json!({
            "micros": micros,
            "incomplete": run_summary.fee_incomplete,
            "estimated": false,
        })
    });
    let mut result = serde_json::json!({
        "type": "Result",
        "session_id": session_id,
        "outcome": outcome,
        "exit_code": exit_code,
        "stop_reason": reason,
        "turns": summary.turns,
        "usage": usage,
        "cost": cost,
        "run_turns": run_summary.turns,
        "run_usage": run_usage,
        "run_cost": run_cost,
        "text": summary.last_text,
    });
    if let Some(error) = model_error
        && outcome == "failed"
    {
        result["model_error"] = model_error_value(error);
    }
    if let Some(unhandled) = end.unhandled_job_notices() {
        result["unhandled_job_notices"] = unhandled.into();
    }
    if let Some(lost) = end.lost_notices() {
        result["lost_notices"] = lost.into();
    }
    if let Some(unhandled_stop) = end.unhandled_stop_notices() {
        result["unhandled_stop_notices"] = unhandled_stop.into();
    }
    let mut bytes = serde_json::to_vec(&result)
        .expect("a Result line is plain JSON and cannot fail to serialize");
    bytes.push(b'\n');
    bytes
}

/// The `model_error` object of a `Result` record (ADR 0044 §2b). `kind` is the variant's own
/// closed-set name; `status` and `retry_after_secs` are `null` when the variant carries none.
/// No message text is copied.
pub(crate) fn model_error_value(error: &ModelError) -> serde_json::Value {
    let status = match error {
        ModelError::Unavailable { status, .. } => *status,
        _ => None,
    };
    let retry_after = match error {
        ModelError::Unavailable { retry_after, .. } => *retry_after,
        // A used-up plan window is not retried, but its reset wait is what a caller needs to
        // wait out before sending the same request again.
        ModelError::UsageLimit { resets_in, .. } => *resets_in,
        _ => None,
    };
    serde_json::json!({
        "kind": error.kind().as_str(),
        "status": status,
        "retry_after_secs": retry_after.map(ceil_seconds),
    })
}

/// Whole seconds, rounded up: a 500 ms wait is one second, not zero.
fn ceil_seconds(duration: Duration) -> u64 {
    duration.as_secs() + u64::from(duration.subsec_nanos() > 0)
}

/// Prints the journal prefix lines, then every teed line, on one task, so a blocked stdout
/// pipe never stalls a journal write (the tee's send is unbounded and its failure is
/// dropped). Stdout write failures are ignored: a closed pipe is a silent stop, never a
/// failure. Returns when every sender is dropped and the queue is drained.
pub(crate) fn spawn_printer(
    prefix: Vec<Vec<u8>>,
    mut rx: UnboundedReceiver<Vec<u8>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut out = tokio::io::stdout();
        for line in prefix {
            if out.write_all(&line).await.is_err() {
                return;
            }
        }
        while let Some(bytes) = rx.recv().await {
            if out.write_all(&bytes).await.is_err() {
                return;
            }
        }
        let _ = out.flush().await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exit 0 is only a completed turn. Mutation: map `Completed` to 1 → this fails.
    #[test]
    fn a_completed_turn_exits_zero() {
        assert_eq!(exit_code_for_status(&RunStatus::Completed), 0);
        assert_eq!(outcome_name(&RunStatus::Completed), "completed");
        assert_eq!(stop_reason(&RunStatus::Completed), "completed");
    }

    /// Exit 1 is a failed turn or a dead session. Mutation: map any of these back to 1's
    /// neighbors (0, 3, 4) → this fails.
    #[test]
    fn a_failed_turn_or_a_dead_session_exits_one() {
        for status in [RunStatus::Failed, RunStatus::Poisoned] {
            assert_eq!(exit_code_for_status(&status), 1, "{status:?}");
        }
        assert_eq!(outcome_name(&RunStatus::Failed), "failed");
        assert_eq!(stop_reason(&RunStatus::Failed), "failed");
        // A poisoned session died abnormally: it is reported as terminated.
        assert_eq!(outcome_name(&RunStatus::Poisoned), "poisoned");
        assert_eq!(stop_reason(&RunStatus::Poisoned), "terminated");
    }

    /// Exit 3 is the budget family. Mutation: map either back to 1 → this fails.
    #[test]
    fn an_exhausted_budget_exits_three() {
        for status in [
            RunStatus::BudgetExceeded,
            RunStatus::Cancelled(CancelReason::DeadlineExceeded),
        ] {
            assert_eq!(exit_code_for_status(&status), 3, "{status:?}");
        }
        assert_eq!(outcome_name(&RunStatus::BudgetExceeded), "budget_exceeded");
        assert_eq!(stop_reason(&RunStatus::BudgetExceeded), "budget");
        assert_eq!(
            stop_reason(&RunStatus::Cancelled(CancelReason::DeadlineExceeded)),
            "deadline"
        );
    }

    /// Exit 4 is an unfinished answer. Mutation: map any of these back to 1 → this fails.
    #[test]
    fn an_unfinished_answer_exits_four() {
        for status in [
            RunStatus::Incomplete(IncompleteReason::MaxTokens),
            RunStatus::Incomplete(IncompleteReason::ContentFiltered),
            RunStatus::Incomplete(IncompleteReason::ToolUseWithoutCalls),
            RunStatus::Incomplete(IncompleteReason::UnknownStopReason("mystery".into())),
            RunStatus::Refused,
        ] {
            assert_eq!(exit_code_for_status(&status), 4, "{status:?}");
        }
        assert_eq!(outcome_name(&RunStatus::Refused), "refused");
        assert_eq!(stop_reason(&RunStatus::Refused), "refused");
        assert_eq!(
            outcome_name(&RunStatus::Incomplete(IncompleteReason::MaxTokens)),
            "incomplete"
        );
        assert_eq!(
            stop_reason(&RunStatus::Incomplete(IncompleteReason::MaxTokens)),
            "max_tokens"
        );
        assert_eq!(
            stop_reason(&RunStatus::Incomplete(IncompleteReason::ContentFiltered)),
            "content_filtered"
        );
        assert_eq!(
            stop_reason(&RunStatus::Incomplete(
                IncompleteReason::ToolUseWithoutCalls
            )),
            "tool_use_without_calls"
        );
        assert_eq!(
            stop_reason(&RunStatus::Incomplete(IncompleteReason::UnknownStopReason(
                "mystery".into()
            ))),
            "unknown:mystery"
        );
    }

    /// A requested cancellation keeps the historical 130. Mutation: map it to 1 → this fails.
    #[test]
    fn a_requested_cancellation_exits_130() {
        let status = RunStatus::Cancelled(CancelReason::Requested);
        assert_eq!(exit_code_for_status(&status), 130);
        assert_eq!(outcome_name(&status), "cancelled");
        assert_eq!(stop_reason(&status), "cancelled");
    }

    /// Usage and setup errors stay 2 (ADR 0032 A3). Mutation: change `USAGE_ERROR` → this fails.
    #[test]
    fn usage_errors_stay_pinned_at_two() {
        assert_eq!(crate::USAGE_ERROR, 2);
        assert_eq!(ExecEnd::Terminated.result_fields().1, 143);
        assert_eq!(ExecEnd::Forced.result_fields().1, 130);
    }

    /// `-p -` reads everything to EOF, including newlines, and an empty stdin is an empty prompt.
    #[test]
    fn dash_p_reads_to_eof() {
        let prompt = "summarize\nthese two lines\n";
        assert_eq!(
            read_prompt(std::io::Cursor::new(prompt)).unwrap(),
            prompt,
            "the prompt keeps its newlines"
        );
        assert_eq!(read_prompt(std::io::Cursor::new("")).unwrap(), "");
    }

    /// The read only returns after the writer closes: what was written in pieces arrives whole.
    #[test]
    fn dash_p_starts_only_after_eof() {
        use std::os::unix::net::UnixStream;
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let done = std::thread::spawn(move || read_prompt(reader).unwrap());
        use std::io::Write;
        writer.write_all(b"first ").unwrap();
        writer.write_all(b"second").unwrap();
        drop(writer);
        assert_eq!(done.join().unwrap(), "first second");
    }

    /// The fold counts turns, sums reported counters, keeps unreported ones absent, and takes
    /// the last assistant message's text.
    #[test]
    fn the_journal_fold_counts_turns_and_sums_reported_counters() {
        let dir = std::env::temp_dir().join(format!("radian-jsonl-fold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fold.jsonl");
        let mut file = String::new();
        for event in [
            serde_json::json!({"schema_version":1,"session_id":"s","seq":0,"ts":"2026-10-20T00:00:00Z","event":"SessionCreated","critical":true,"data":{}}),
            serde_json::json!({"schema_version":1,"session_id":"s","seq":1,"ts":"2026-10-20T00:00:01Z","event":"MessageAppended","critical":true,"data":{"index":0,"message":{"assistant":{"content":[{"text":{"text":"first"}}]}}}}),
            serde_json::json!({"schema_version":1,"session_id":"s","seq":2,"ts":"2026-10-20T00:00:02Z","event":"ModelUsageRecorded","critical":false,"data":{"usage":{"input":10,"output":4},"fee":{"micros":30}}}),
            serde_json::json!({"schema_version":1,"session_id":"s","seq":3,"ts":"2026-10-20T00:00:03Z","event":"MessageAppended","critical":true,"data":{"index":1,"message":{"assistant":{"content":[{"text":{"text":"second"}},{"image":{"mime_type":"image/png","data":"eA=="}}]}}}}),
            serde_json::json!({"schema_version":1,"session_id":"s","seq":4,"ts":"2026-10-20T00:00:04Z","event":"ModelUsageRecorded","critical":false,"data":{"usage":{"input":5,"output":2,"cache_read":3},"fee":{"micros":7,"incomplete":true}}}),
            serde_json::json!({"schema_version":1,"session_id":"s","seq":5,"ts":"2026-10-20T00:00:05Z","event":"TurnFinished","critical":true,"data":{"turn_id":"t","outcome":"completed"}}),
        ] {
            file.push_str(&serde_json::to_string(&event).unwrap());
            file.push('\n');
        }
        // A torn tail: no trailing newline, skipped by the fold.
        file.push_str("this torn tail has no newline and is skipped");
        std::fs::write(&path, &file).unwrap();

        let summary = summarize_journal(&path);
        assert_eq!(summary.turns, 1);
        assert_eq!(summary.input, Some(15));
        assert_eq!(summary.output, Some(6));
        assert_eq!(summary.cache_read, Some(3));
        assert_eq!(
            summary.cache_write, None,
            "unreported stays absent, never 0"
        );
        assert_eq!(summary.fee_micros, Some(37));
        assert!(summary.fee_incomplete);
        assert_eq!(
            summary.last_text, "second",
            "only the last assistant message counts"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A turn's own fold: only the records between its `TurnStarted` and `TurnFinished`, so a
    /// prompt answer reports the fee and text of the turn that answered it (ADR 0049), never a
    /// later or earlier turn's.
    #[test]
    fn a_turn_fold_takes_only_that_turns_records() {
        let dir = std::env::temp_dir().join(format!("radian-jsonl-turn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("turn.jsonl");
        let mut file = String::new();
        let line = |seq: u64, event: &str, data: serde_json::Value| {
            serde_json::json!({
                "schema_version": 1,
                "session_id": "s",
                "seq": seq,
                "ts": "2026-10-28T00:00:00Z",
                "event": event,
                "critical": true,
                "data": data,
            })
        };
        let assistant = |index: u64, text: &str| {
            serde_json::json!({
                "index": index,
                "message": {"assistant": {"content": [{"text": {"text": text}}]}},
            })
        };
        for event in [
            line(0, "TurnStarted", serde_json::json!({"turn_id": "t-1"})),
            line(1, "MessageAppended", assistant(0, "first")),
            line(
                2,
                "ModelUsageRecorded",
                serde_json::json!({"usage": {"input": 10, "output": 4}, "fee": {"micros": 30}}),
            ),
            line(3, "TurnFinished", serde_json::json!({"turn_id": "t-1"})),
            line(4, "TurnStarted", serde_json::json!({"turn_id": "t-2"})),
            line(5, "MessageAppended", assistant(1, "second")),
            line(
                6,
                "ModelUsageRecorded",
                serde_json::json!({
                    "usage": {"input": 5, "output": 2},
                    "fee": {"micros": 7, "incomplete": true},
                }),
            ),
            line(7, "TurnFinished", serde_json::json!({"turn_id": "t-2"})),
        ] {
            file.push_str(&serde_json::to_string(&event).unwrap());
            file.push('\n');
        }
        std::fs::write(&path, &file).unwrap();

        let first = summarize_turn(&path, "t-1");
        assert_eq!(first.turns, 1);
        assert_eq!(first.last_text, "first");
        assert_eq!(first.fee_micros, Some(30));
        assert!(!first.fee_incomplete);
        assert_eq!(first.input, Some(10), "only this turn's counters");

        let second = summarize_turn(&path, "t-2");
        assert_eq!(second.turns, 1);
        assert_eq!(second.last_text, "second");
        assert_eq!(second.fee_micros, Some(7));
        assert!(second.fee_incomplete);
        assert_eq!(second.input, Some(5));

        let missing = summarize_turn(&path, "t-3");
        assert_eq!(
            missing,
            JournalSummary::default(),
            "an unknown turn is empty"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `Result` line carries every field the ADR promises, with the folded totals.
    /// Mutation: copy the session totals into `run_*` → the run half differs here and this fails.
    #[test]
    fn the_result_line_carries_the_run_and_the_session_totals() {
        let summary = JournalSummary {
            turns: 2,
            input: Some(15),
            output: Some(6),
            cache_read: None,
            cache_write: None,
            fee_micros: Some(37),
            fee_incomplete: false,
            last_text: "done".to_owned(),
        };
        let run_summary = JournalSummary {
            turns: 1,
            input: Some(5),
            output: Some(2),
            cache_read: None,
            cache_write: None,
            fee_micros: Some(7),
            fee_incomplete: true,
            last_text: "ignored".to_owned(),
        };
        let line = result_line(
            "sid",
            &ExecEnd::Status(Some(RunStatus::BudgetExceeded)),
            &summary,
            &run_summary,
            true,
            None,
        );
        let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "Result",
                "session_id": "sid",
                "outcome": "budget_exceeded",
                "exit_code": 3,
                "stop_reason": "budget",
                "turns": 2,
                "usage": {"input": 15, "output": 6},
                "cost": {"micros": 37, "incomplete": false, "estimated": true},
                "run_turns": 1,
                "run_usage": {"input": 5, "output": 2},
                "run_cost": {"micros": 7, "incomplete": true, "estimated": false},
                "text": "done",
            })
        );
    }

    /// The suffix fold counts only the bytes after the tee prefix. Mutation: ignore
    /// `prefix_len` and fold the whole file → the run half equals the session half and
    /// this fails; an off-by-one-line boundary (one history line too many or one run
    /// line too few) also fails here.
    #[test]
    fn the_run_fold_counts_only_the_suffix_after_the_prefix() {
        let dir = std::env::temp_dir().join(format!("radian-jsonl-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("run.jsonl");
        let events = [
            serde_json::json!({"schema_version":1,"session_id":"s","seq":0,"ts":"2026-10-20T00:00:00Z","event":"SessionCreated","critical":true,"data":{}}),
            serde_json::json!({"schema_version":1,"session_id":"s","seq":1,"ts":"2026-10-20T00:00:01Z","event":"ModelUsageRecorded","critical":false,"data":{"usage":{"input":10,"output":4},"fee":{"micros":30}}}),
            serde_json::json!({"schema_version":1,"session_id":"s","seq":2,"ts":"2026-10-20T00:00:02Z","event":"TurnFinished","critical":true,"data":{"turn_id":"t1","outcome":"completed"}}),
            serde_json::json!({"schema_version":1,"session_id":"s","seq":3,"ts":"2026-10-20T00:00:03Z","event":"ModelUsageRecorded","critical":false,"data":{"usage":{"input":5,"output":2},"fee":{"micros":7}}}),
            serde_json::json!({"schema_version":1,"session_id":"s","seq":4,"ts":"2026-10-20T00:00:04Z","event":"TurnFinished","critical":true,"data":{"turn_id":"t2","outcome":"completed"}}),
        ];
        let mut file = String::new();
        let mut prefix_len = 0u64;
        for (i, event) in events.iter().enumerate() {
            let mut line = serde_json::to_string(event).unwrap();
            line.push('\n');
            if i < 3 {
                prefix_len += line.len() as u64;
            }
            file.push_str(&line);
        }
        std::fs::write(&path, &file).unwrap();

        let session = summarize_journal(&path);
        assert_eq!(session.turns, 2);
        assert_eq!(session.input, Some(15));

        let run = summarize_journal_from(&path, prefix_len);
        assert_eq!(run.turns, 1, "only the second turn ran after the tee");
        assert_eq!(run.input, Some(5));
        assert_eq!(run.output, Some(2));
        assert_eq!(run.fee_micros, Some(7));
        assert!(!run.fee_incomplete);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The wake-budget ending exits 3 with a budget outcome, its own stop reason, and
    /// the unhandled counts; every other ending carries no such fields. A nonzero Stop
    /// count adds `unhandled_stop_notices`; a zero one serializes without it. Mutation:
    /// exit 0 instead of 3, or drop a field → this fails.
    #[test]
    fn the_wake_budget_ending_exits_three_with_its_unhandled_count() {
        let end = ExecEnd::WakeBudget {
            unhandled: 2,
            unhandled_stop: 0,
        };
        let (outcome, code, reason) = end.result_fields();
        assert_eq!(outcome, "budget_exceeded");
        assert_eq!(code, 3);
        assert_eq!(reason, "wake_budget");
        assert_eq!(end.unhandled_job_notices(), Some(2));
        assert_eq!(end.unhandled_stop_notices(), None);
        assert_eq!(ExecEnd::Status(None).unhandled_job_notices(), None);
        assert_eq!(ExecEnd::Status(None).unhandled_stop_notices(), None);

        let summary = JournalSummary {
            turns: 4,
            last_text: "stuck".to_owned(),
            ..JournalSummary::default()
        };
        let line = result_line(
            "sid",
            &end,
            &summary,
            &JournalSummary::default(),
            false,
            None,
        );
        let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "Result",
                "session_id": "sid",
                "outcome": "budget_exceeded",
                "exit_code": 3,
                "stop_reason": "wake_budget",
                "turns": 4,
                "usage": {},
                "cost": serde_json::Value::Null,
                "run_turns": 0,
                "run_usage": {},
                "run_cost": serde_json::Value::Null,
                "text": "stuck",
                "unhandled_job_notices": 2,
            })
        );

        // A lost notice ends with exit 3, the budget outcome, its own reason and
        // count — and neither of the wake-budget fields.
        let end = ExecEnd::NoticeLost { lost: 1 };
        let (outcome, code, reason) = end.result_fields();
        assert_eq!(outcome, "budget_exceeded");
        assert_eq!(code, 3);
        assert_eq!(reason, "job_notice_lost");
        assert_eq!(end.lost_notices(), Some(1));
        assert_eq!(end.unhandled_job_notices(), None);
        assert_eq!(end.unhandled_stop_notices(), None);
        let line = result_line(
            "sid",
            &end,
            &summary,
            &JournalSummary::default(),
            false,
            None,
        );
        let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            value.get("lost_notices"),
            Some(&serde_json::json!(1)),
            "{value}"
        );
        assert_eq!(
            value.get("stop_reason"),
            Some(&serde_json::json!("job_notice_lost")),
            "{value}"
        );
        assert!(
            value.get("unhandled_job_notices").is_none(),
            "the wake-budget field stays out: {value}"
        );

        // A nonzero Stop count adds its own optional field.
        let end = ExecEnd::WakeBudget {
            unhandled: 0,
            unhandled_stop: 1,
        };
        assert_eq!(end.unhandled_stop_notices(), Some(1));
        let line = result_line(
            "sid",
            &end,
            &summary,
            &JournalSummary::default(),
            false,
            None,
        );
        let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            value.get("unhandled_job_notices"),
            Some(&serde_json::json!(0)),
            "{value}"
        );
        assert_eq!(
            value.get("unhandled_stop_notices"),
            Some(&serde_json::json!(1)),
            "{value}"
        );
    }

    /// The `model_error` object (ADR 0044 §2b): one kind per variant, the HTTP status only with
    /// `Unavailable`, and the wait from `Unavailable.retry_after` or `UsageLimit.resets_in`,
    /// rounded up to whole seconds. Mutation: read `retry_after` only (ignoring `resets_in`) →
    /// the usage-limit expectation fails; use `as_secs` (truncating) → the 500 ms case fails.
    #[test]
    fn the_model_error_field_carries_kind_status_and_wait() {
        let cases = [
            (
                ModelError::Transport("network down".into()),
                serde_json::json!({"kind": "transport", "status": null, "retry_after_secs": null}),
            ),
            (
                ModelError::Protocol("HTTP 400: bad request".into()),
                serde_json::json!({"kind": "protocol", "status": null, "retry_after_secs": null}),
            ),
            (
                ModelError::Unavailable {
                    message: "HTTP 429".into(),
                    retry_after: Some(Duration::from_secs(30)),
                    status: Some(429),
                },
                serde_json::json!({
                    "kind": "unavailable",
                    "status": 429,
                    "retry_after_secs": 30,
                }),
            ),
            (
                ModelError::Unavailable {
                    message: "the provider is overloaded".into(),
                    retry_after: None,
                    status: None,
                },
                serde_json::json!({
                    "kind": "unavailable",
                    "status": null,
                    "retry_after_secs": null,
                }),
            ),
            (
                ModelError::UsageLimit {
                    message: "HTTP 429: the plan window is used up".into(),
                    resets_in: Some(Duration::from_secs(6240)),
                },
                serde_json::json!({
                    "kind": "usage_limit",
                    "status": null,
                    "retry_after_secs": 6240,
                }),
            ),
            (
                ModelError::UsageLimit {
                    message: "HTTP 429: the plan window is used up".into(),
                    resets_in: Some(Duration::from_millis(1_500)),
                },
                serde_json::json!({
                    "kind": "usage_limit",
                    "status": null,
                    "retry_after_secs": 2,
                }),
            ),
            (
                ModelError::ContextOverflow("HTTP 400: prompt is too long".into()),
                serde_json::json!({
                    "kind": "context_overflow",
                    "status": null,
                    "retry_after_secs": null,
                }),
            ),
            (
                ModelError::EmptyResponse,
                serde_json::json!({
                    "kind": "empty_response",
                    "status": null,
                    "retry_after_secs": null,
                }),
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(model_error_value(&error), expected, "kind for {error}");
        }
        // A sub-second wait still reports one second, never zero.
        assert_eq!(
            model_error_value(&ModelError::Unavailable {
                message: "HTTP 503".into(),
                retry_after: Some(Duration::from_millis(500)),
                status: Some(503),
            }),
            serde_json::json!({
                "kind": "unavailable",
                "status": 503,
                "retry_after_secs": 1,
            })
        );
    }

    /// `model_error` appears only on a failed run whose turn carried a `ModelError.` Mutation:
    /// drop the field from the writer → the failed case fails; emit it on every ending → the
    /// completed, cancelled and no-error expectations fail.
    #[test]
    fn the_result_line_carries_model_error_only_on_a_failed_model_error() {
        let error = ModelError::UsageLimit {
            message: "HTTP 429: the plan window is used up".into(),
            resets_in: Some(Duration::from_secs(6240)),
        };
        let failed = result_line(
            "sid",
            &ExecEnd::Status(Some(RunStatus::Failed)),
            &JournalSummary::default(),
            &JournalSummary::default(),
            false,
            Some(&error),
        );
        let value: serde_json::Value = serde_json::from_slice(&failed).unwrap();
        assert_eq!(
            value.get("model_error"),
            Some(&serde_json::json!({
                "kind": "usage_limit",
                "status": null,
                "retry_after_secs": 6240,
            })),
            "{value}"
        );

        for (end, name) in [
            (ExecEnd::Status(Some(RunStatus::Completed)), "completed"),
            (
                ExecEnd::Status(Some(RunStatus::Cancelled(CancelReason::Requested))),
                "cancelled",
            ),
            (ExecEnd::Status(Some(RunStatus::Failed)), "no error"),
        ] {
            let error = (name != "no error").then_some(&error);
            let line = result_line(
                "sid",
                &end,
                &JournalSummary::default(),
                &JournalSummary::default(),
                false,
                error,
            );
            let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
            assert!(
                value.get("model_error").is_none(),
                "{name} carries no model_error: {value}"
            );
        }
    }

    /// A `UsageLimit` error travels the real path: a scripted model fails the turn, the driver
    /// hands the structured error to the exec runtime sink (`on_model_error`), and the `Result`
    /// writer serializes what the sink kept. The direct mapper test above cannot prove the
    /// callback carries this variant. Mutation: the sink stops storing the error (or the driver
    /// stops calling it) → there is no `model_error` here.
    #[tokio::test]
    async fn a_usage_limit_error_reaches_the_result_through_the_driver() {
        use std::sync::Arc;

        use radian_core::{
            Agent, AgentConfig, AgentEvent, EventSink, InputSource, Model, ModelContext,
            ModelRequest, ModelResponse, NoTools, TurnOptions,
        };
        use radian_runtime::{Driver, Inbox};
        use radian_session::{SessionCreatedData, SessionJournal};

        struct UsageLimitModel;

        impl Model for UsageLimitModel {
            async fn complete(
                &self,
                _request: ModelRequest,
                _cx: ModelContext<'_>,
            ) -> Result<ModelResponse, ModelError> {
                Err(ModelError::UsageLimit {
                    message: "HTTP 429: the ChatGPT plus plan's 300 min usage window is used \
                              up; it resets in 104 min; /model switches to another provider"
                        .into(),
                    resets_in: Some(Duration::from_secs(6240)),
                })
            }
        }

        /// Waits for the turn boundary, so the driver is idle before it is closed.
        #[derive(Default)]
        struct Finished(tokio::sync::Notify);

        impl EventSink for Finished {
            fn emit(&self, event: AgentEvent) {
                if matches!(event, AgentEvent::RunFinished { .. }) {
                    self.0.notify_one();
                }
            }
        }

        let dir =
            std::env::temp_dir().join(format!("radian-jsonl-model-error-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let journal = Arc::new(
            SessionJournal::create(
                &path,
                "sid-model-error",
                SessionCreatedData::new(
                    "/work/example",
                    serde_json::json!({"model": {"provider": "example", "model": "m-1"}}),
                ),
            )
            .unwrap(),
        );
        let inbox = Inbox::durable(8, Arc::clone(&journal));
        let agent = Arc::new(
            Agent::new(UsageLimitModel, NoTools).with_config(AgentConfig {
                retries: 0,
                ..Default::default()
            }),
        );
        let finished = Arc::new(Finished::default());
        let mut driver = Driver::open(
            &agent,
            Arc::clone(&journal),
            Arc::clone(&inbox),
            Arc::clone(&finished) as Arc<dyn EventSink>,
            TurnOptions::default(),
        )
        .await
        .expect("open the driver");
        // The exec host's own sink, not a stand-in: it stores the structured error the runtime
        // callback receives, and the `Result` writer below reads that store.
        let (sink, last_model_error) = crate::chat::ExecRuntimeEvents::new(Arc::new(Vec::new()));
        driver.set_runtime_events(Arc::new(sink));
        let handle = tokio::spawn(async move { driver.run().await });

        inbox.followup("hi", InputSource::User).await.unwrap();
        finished.0.notified().await;
        inbox.close();
        handle
            .await
            .expect("the driver task does not panic")
            .expect("the driver ends cleanly");
        assert_eq!(
            last_model_error
                .lock()
                .unwrap()
                .as_ref()
                .map(ModelError::kind),
            Some(radian_core::ModelErrorKind::UsageLimit),
            "the runtime callback carried the structured variant"
        );

        let summary = summarize_journal(&path);
        let stored = last_model_error.lock().unwrap();
        let line = result_line(
            "sid-model-error",
            &ExecEnd::Status(Some(RunStatus::Failed)),
            &summary,
            &summary,
            false,
            stored.as_ref(),
        );
        let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            value.get("model_error"),
            Some(&serde_json::json!({
                "kind": "usage_limit",
                "status": serde_json::Value::Null,
                "retry_after_secs": 6240,
            })),
            "{value}"
        );
        drop(stored);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every `model_error.kind` value the serializer can write is named in the manual. The
    /// source is `radian_core::MODEL_ERROR_KINDS`, generated from the same `model_error_kinds!`
    /// declaration the serializer reads, so a new variant fails here until `docs/user/cli.md`
    /// documents its value.
    #[test]
    fn every_model_error_kind_appears_in_the_manual() {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let cli_md = std::fs::read_to_string(manifest.join("../../docs/user/cli.md"))
            .expect("docs/user/cli.md exists");
        assert!(
            cli_md.contains("`model_error`"),
            "cli.md must document the Result field by name"
        );
        for kind in radian_core::MODEL_ERROR_KINDS {
            assert!(
                cli_md.contains(&format!("`{kind}`")),
                "kind `{kind}` is missing from docs/user/cli.md"
            );
        }
    }

    /// A live line is exactly `live` + the journal's `event`/`data` shape.
    #[test]
    fn a_live_line_marks_itself_and_keeps_the_journal_shape() {
        let line = live_line("ModelTextDelta", "hi");
        let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "live": true,
                "event": "ModelTextDelta",
                "data": { "text": "hi" },
            })
        );
        assert!(line.ends_with(b"\n"));
    }
}
