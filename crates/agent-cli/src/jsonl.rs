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

use agent_core::{CancelReason, IncompleteReason, RunStatus};
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
    /// family (ADR 0044). `unhandled` counts them.
    WakeBudget { unhandled: u64 },
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
        }
    }

    /// How many job notices never ran, for the `Result` record. `Some` only on the wake-
    /// budget path; every other ending serializes without the field, so their lines are
    /// unchanged.
    pub(crate) fn unhandled_job_notices(&self) -> Option<u64> {
        match self {
            ExecEnd::WakeBudget { unhandled } => Some(*unhandled),
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
    let mut summary = JournalSummary::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        return summary;
    };
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(event) = record.get("event").and_then(|e| e.as_str()) else {
            continue;
        };
        let data = record.get("data");
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
                    summary.fee_micros =
                        Some(summary.fee_micros.unwrap_or(0).saturating_add(micros));
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
    summary
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
/// restored call was re-priced with today's catalog); everything else is folded from the
/// journal, so it covers resumed history too. The wake-budget ending adds
/// `unhandled_job_notices`; every other ending omits it, so their field set is the
/// fixture's field set.
pub(crate) fn result_line(
    session_id: &str,
    end: &ExecEnd,
    summary: &JournalSummary,
    cost_estimated: bool,
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
    let cost = summary.fee_micros.map(|micros| {
        serde_json::json!({
            "micros": micros,
            "incomplete": summary.fee_incomplete,
            "estimated": cost_estimated,
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
        "text": summary.last_text,
    });
    if let Some(unhandled) = end.unhandled_job_notices() {
        result["unhandled_job_notices"] = unhandled.into();
    }
    let mut bytes = serde_json::to_vec(&result)
        .expect("a Result line is plain JSON and cannot fail to serialize");
    bytes.push(b'\n');
    bytes
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

    /// The `Result` line carries every field the ADR promises, with the folded totals.
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
        let line = result_line(
            "sid",
            &ExecEnd::Status(Some(RunStatus::BudgetExceeded)),
            &summary,
            true,
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
                "text": "done",
            })
        );
    }

    /// The wake-budget ending exits 3 with a budget outcome, its own stop reason, and
    /// the unhandled count; every other ending carries no such field. Mutation: exit 0
    /// instead of 3, or drop the field → this fails.
    #[test]
    fn the_wake_budget_ending_exits_three_with_its_unhandled_count() {
        let end = ExecEnd::WakeBudget { unhandled: 2 };
        let (outcome, code, reason) = end.result_fields();
        assert_eq!(outcome, "budget_exceeded");
        assert_eq!(code, 3);
        assert_eq!(reason, "wake_budget");
        assert_eq!(end.unhandled_job_notices(), Some(2));
        assert_eq!(ExecEnd::Status(None).unhandled_job_notices(), None);

        let summary = JournalSummary {
            turns: 4,
            last_text: "stuck".to_owned(),
            ..JournalSummary::default()
        };
        let line = result_line("sid", &end, &summary, false);
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
                "text": "stuck",
                "unhandled_job_notices": 2,
            })
        );
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
