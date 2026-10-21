//! The persistent goal (ADR 0047): the durable state folded from the `Goal*` journal
//! events, plus the goal-attributed budget counters.
//!
//! [`GoalState`] is **not** a transcript record: like every other runtime event it is audit
//! only (`critical = false`), and this fold is the one place that reads it back. The host
//! folds the session's events once at open (a resume maps a terminal `Active` to `Paused`);
//! the TUI and the `goal` tool share the result behind a mutex and write every change back
//! through the journal, so the fold always rebuilds the same state.

use std::collections::HashSet;

use agent_core::{Message, ModelUsage};

use crate::reader::{EventData, SessionEvent};
use crate::runtime::TurnOutcome;

/// The goal's budget limits. `None` is "no cap" for that dimension. Fresh goals default to
/// [`DEFAULT_MAX_TURNS`] turns and [`DEFAULT_MAX_DURATION_SECS`] seconds (ADR 0047); tokens
/// are uncapped unless `/goal budget` sets them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GoalBudget {
    pub max_turns: Option<u64>,
    pub max_duration_secs: Option<u64>,
    pub max_tokens: Option<u64>,
}

/// The default cap: at most this many goal turns when no `/goal budget` set one (ADR 0047).
pub const DEFAULT_MAX_TURNS: u64 = 30;

/// The default cap: stop this long after the goal was set when no `/goal budget` set one
/// (ADR 0047).
pub const DEFAULT_MAX_DURATION_SECS: u64 = 2 * 3600;

/// Whether the goal still runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalStatus {
    Active,
    Paused,
    Done,
    BudgetExhausted,
}

/// One session's persistent goal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalState {
    pub objective: String,
    pub status: GoalStatus,
    /// The latest `GoalProgress` note (or the `done` note), shown in the prompt block.
    pub progress: Option<String>,
    pub budget: GoalBudget,
    /// Which cap stopped the goal (`turns`, `duration` or `tokens`). `None` unless
    /// `status` is [`GoalStatus::BudgetExhausted`].
    pub exhausted_kind: Option<String>,
    /// Completed goal-attributed turns (durable: recomputed from the journal, so a resume
    /// keeps the count).
    pub turns_used: u64,
    /// Accumulated active wall-clock seconds (durable part; the live stretch is added by
    /// the TUI, which owns the clock).
    pub active_secs: u64,
    /// Conversation tokens spent while active (durable part; same live-stretch rule).
    pub tokens_used: u64,
    /// A `goal wait` the turn end has not consumed yet (transient: never folded, the
    /// `GoalWait` event is the audit). The tool sets it, the TUI's turn-end arms the
    /// deadline from it and clears it.
    pub pending_wait_secs: Option<u64>,
}

impl GoalState {
    /// A fresh goal from `/goal <objective>`: active, default budget, no counters.
    pub fn fresh(objective: String, budget: GoalBudget) -> Self {
        Self {
            objective,
            status: GoalStatus::Active,
            progress: None,
            budget,
            exhausted_kind: None,
            turns_used: 0,
            active_secs: 0,
            tokens_used: 0,
            pending_wait_secs: None,
        }
    }

    /// The default budget (ADR 0047): 30 turns and 2 hours, tokens uncapped.
    pub fn default_budget() -> GoalBudget {
        GoalBudget {
            max_turns: Some(DEFAULT_MAX_TURNS),
            max_duration_secs: Some(DEFAULT_MAX_DURATION_SECS),
            max_tokens: None,
        }
    }
}

/// The tokens one usage record bills to the goal's token budget: the provider's own
/// `total` when it reported one, else its whole prompt ([`ModelUsage::prompt_total`], which
/// already settles the `input_includes_cache` question per ADR 0007) plus its
/// output. Missing counters bill as zero: an unmarked old record is then a lower bound,
/// the same convention the context budget uses — never an invented number.
pub fn goal_tokens(usage: &ModelUsage) -> u64 {
    if let Some(total) = usage.total {
        return total;
    }
    usage.prompt_total().unwrap_or(0) + usage.output.unwrap_or(0)
}

/// Folds the session's events into the current goal, or `None` when no goal exists (never
/// set, or dropped). Tolerant: an event that names no goal (progress/done/pause/resume on
/// nothing) is ignored rather than an error — the journal stays the authority and the
/// commands validate before writing.
pub fn fold(events: &[SessionEvent]) -> Option<GoalState> {
    let mut goal: Option<GoalState> = None;
    // Message ids the machine sent as goal continuations, and the turns that claimed them.
    let mut goal_messages: HashSet<String> = HashSet::new();
    let mut goal_turns: HashSet<String> = HashSet::new();
    // The open active span's start (epoch seconds), for the duration counter.
    let mut span_start: Option<i64> = None;

    // Closes the open span at `ts_secs`, accumulating its wall time.
    fn close_span(goal: &mut Option<GoalState>, span_start: &mut Option<i64>, ts_secs: i64) {
        if let Some(start) = span_start.take()
            && let Some(goal) = goal.as_mut()
        {
            goal.active_secs += ts_secs.saturating_sub(start).max(0) as u64;
        }
    }

    for event in events {
        let ts_secs = parse_ts_secs(&event.ts);
        match &event.data {
            EventData::GoalSet {
                objective,
                max_turns,
                max_duration_secs,
                max_tokens,
            } => {
                // A set while a span is open ends it first (a replace never inherits time).
                close_span(&mut goal, &mut span_start, ts_secs);
                let mut fresh = GoalState::fresh(
                    objective.clone(),
                    GoalBudget {
                        max_turns: *max_turns,
                        max_duration_secs: *max_duration_secs,
                        max_tokens: *max_tokens,
                    },
                );
                // An unset budget on the wire is today's default, not "no cap": old records
                // predate per-field optionality the same way `wake` did (ADR 0006 §5.5).
                if fresh.budget.max_turns.is_none()
                    && fresh.budget.max_duration_secs.is_none()
                {
                    fresh.budget = GoalState::default_budget();
                }
                goal = Some(fresh);
                span_start = Some(ts_secs);
            }
            EventData::GoalBudgetSet {
                max_turns,
                max_duration_secs,
                max_tokens,
            } => {
                if let Some(goal) = goal.as_mut() {
                    goal.budget = GoalBudget {
                        max_turns: *max_turns,
                        max_duration_secs: *max_duration_secs,
                        max_tokens: *max_tokens,
                    };
                }
            }
            EventData::GoalProgress { note } => {
                if let Some(goal) = goal.as_mut() {
                    goal.progress = Some(note.clone());
                }
            }
            EventData::GoalDone { note } => {
                if let Some(goal) = goal.as_mut() {
                    if let Some(note) = note {
                        goal.progress = Some(note.clone());
                    }
                    goal.status = GoalStatus::Done;
                }
                close_span(&mut goal, &mut span_start, ts_secs);
            }
            EventData::GoalPaused {} => {
                let was_active =
                    goal.as_ref().is_some_and(|goal| goal.status == GoalStatus::Active);
                if was_active {
                    if let Some(goal) = goal.as_mut() {
                        goal.status = GoalStatus::Paused;
                    }
                    close_span(&mut goal, &mut span_start, ts_secs);
                }
            }
            EventData::GoalResumed {} => {
                if let Some(goal) = goal.as_mut()
                    && goal.status == GoalStatus::Paused
                {
                    goal.status = GoalStatus::Active;
                    span_start = Some(ts_secs);
                }
            }
            EventData::GoalDropped {} => {
                close_span(&mut goal, &mut span_start, ts_secs);
                goal = None;
            }
            EventData::GoalWait { .. } => {}
            EventData::GoalBudgetExhausted { kind } => {
                if let Some(goal) = goal.as_mut() {
                    goal.status = GoalStatus::BudgetExhausted;
                    goal.exhausted_kind = Some(kind.clone());
                }
                close_span(&mut goal, &mut span_start, ts_secs);
            }
            EventData::InboxEnqueued {
                message, message_id, ..
            } => {
                if is_goal_continuation(message) {
                    goal_messages.insert(message_id.0.clone());
                }
            }
            EventData::InputClaimed {
                message_ids,
                turn_id,
            } => {
                if message_ids
                    .iter()
                    .any(|id| goal_messages.contains(id.0.as_str()))
                {
                    goal_turns.insert(turn_id.clone());
                }
            }
            EventData::TurnFinished { turn_id, .. } => {
                // Only a completed goal turn consumes the turn budget (the loop counts the
                // same way: a failed or cancelled turn never consumed an iteration).
                if goal_turns.contains(turn_id.as_str())
                    && matches!(
                        &event.data,
                        EventData::TurnFinished {
                            outcome: TurnOutcome::Completed,
                            ..
                        }
                    )
                    && let Some(goal) = goal.as_mut()
                {
                    goal.turns_used += 1;
                }
            }
            EventData::ModelUsageRecorded { usage, .. } => {
                // Conversation usage inside an active span bills to the goal. The
                // summarizer's own calls (`CompactionUsageRecorded`) never do.
                if span_start.is_some()
                    && let Some(goal) = goal.as_mut()
                {
                    goal.tokens_used += goal_tokens(usage);
                }
            }
            _ => {}
        }
    }
    goal
}

/// Folds for a `--resume` open: an `Active` goal comes back `Paused` (ADR 0047 — a resume
/// never auto-continues; `/goal resume` restarts it). Every other state folds through.
pub fn fold_for_resume(events: &[SessionEvent]) -> Option<GoalState> {
    fold(events).map(|mut goal| {
        if goal.status == GoalStatus::Active {
            goal.status = GoalStatus::Paused;
        }
        goal
    })
}

/// Whether this inbox message is a goal continuation: its `origin` carries
/// `{"continuation":"goal"}` (ADR 0005 G1, the same audit shape as the loop's).
pub fn is_goal_continuation(message: &Message) -> bool {
    let meta = match message {
        Message::User { meta, .. }
        | Message::Assistant { meta, .. }
        | Message::Tool { meta, .. } => meta,
    };
    let Some(origin) = meta.origin.as_ref() else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(origin.get()) else {
        return false;
    };
    value.get("continuation").and_then(|v| v.as_str()) == Some("goal")
}

/// Parses a journal `ts` (`YYYY-MM-DDTHH:MM:SS[.frac]Z`, ADR 0006) to epoch seconds.
/// Unparseable input maps to 0 rather than failing the fold — a bad clock must not lose
/// the goal, only its duration math.
fn parse_ts_secs(ts: &str) -> i64 {
    let date = ts.strip_suffix('Z').unwrap_or(ts);
    let (ymd, hms) = match date.split_once('T') {
        Some(pair) => pair,
        None => return 0,
    };
    let date: Vec<&str> = ymd.split('-').collect();
    let time: Vec<&str> = hms.split(':').collect();
    if date.len() != 3 || time.len() != 3 {
        return 0;
    }
    let Ok(year) = date[0].parse::<i64>() else {
        return 0;
    };
    let Ok(month) = date[1].parse::<i64>() else {
        return 0;
    };
    let Ok(day) = date[2].parse::<i64>() else {
        return 0;
    };
    let secs: Vec<&str> = time[2].split('.').collect();
    let Ok(hour) = time[0].parse::<i64>() else {
        return 0;
    };
    let Ok(min) = time[1].parse::<i64>() else {
        return 0;
    };
    let Ok(sec) = secs[0].parse::<i64>() else {
        return 0;
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60
    {
        return 0;
    }
    days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec
}

/// Days since 1970-01-01 (Howard Hinnant's civil-from-days inverse).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::MessageId;

    /// One event with a fixed `ts` and no envelope meaning beyond the fold.
    fn event(ts: &str, data: EventData) -> SessionEvent {
        SessionEvent {
            seq: 0,
            ts: ts.to_owned(),
            event: String::new(),
            critical: false,
            byte_offset: 0,
            line_len: 0,
            data,
        }
    }

    fn set(ts: &str, objective: &str) -> SessionEvent {
        event(
            ts,
            EventData::GoalSet {
                objective: objective.to_owned(),
                max_turns: Some(2),
                max_duration_secs: None,
                max_tokens: None,
            },
        )
    }

    /// A `GoalSet` opens an active span: pausing 90 seconds later banks 90 seconds.
    #[test]
    fn set_then_pause_banks_the_active_span() {
        let goal = fold(&[
            set("2026-10-20T10:00:00Z", "ship it"),
            event("2026-10-20T10:01:30Z", EventData::GoalPaused {}),
        ])
        .expect("a goal");
        assert_eq!(goal.objective, "ship it");
        assert_eq!(goal.status, GoalStatus::Paused);
        assert_eq!(goal.active_secs, 90);
        assert_eq!(goal.turns_used, 0);
    }

    /// Progress replaces the latest note; done keeps its note and closes the span.
    #[test]
    fn progress_replaces_the_note_and_done_closes_the_span() {
        let goal = fold(&[
            set("2026-10-20T10:00:00Z", "ship it"),
            event(
                "2026-10-20T10:00:10Z",
                EventData::GoalProgress {
                    note: "half there".into(),
                },
            ),
            event(
                "2026-10-20T10:00:20Z",
                EventData::GoalDone {
                    note: Some("shipped".into()),
                },
            ),
        ])
        .expect("a goal");
        assert_eq!(goal.status, GoalStatus::Done);
        assert_eq!(goal.progress.as_deref(), Some("shipped"));
        assert_eq!(goal.active_secs, 20);
    }

    /// A drop forgets the goal: the fold returns `None`.
    #[test]
    fn drop_forgets_the_goal() {
        assert!(
            fold(&[
                set("2026-10-20T10:00:00Z", "ship it"),
                event("2026-10-20T10:00:01Z", EventData::GoalDropped {}),
            ])
            .is_none()
        );
    }

    /// A set replaces the old goal wholesale: new objective, reset counters.
    #[test]
    fn set_replaces_the_old_goal_and_resets_its_counters() {
        let goal = fold(&[
            set("2026-10-20T10:00:00Z", "one"),
            event(
                "2026-10-20T10:00:10Z",
                EventData::GoalProgress {
                    note: "note".into(),
                },
            ),
            set("2026-10-20T10:01:00Z", "two"),
        ])
        .expect("a goal");
        assert_eq!(goal.objective, "two");
        assert_eq!(goal.progress, None);
        assert_eq!(goal.active_secs, 0);
        assert_eq!(goal.status, GoalStatus::Active);
    }

    /// Only completed goal-attributed turns consume the turn budget: attribution runs
    /// `InboxEnqueued` (origin `continuation=goal`) → `InputClaimed` → `TurnFinished`, and
    /// a failed goal turn plus an ordinary completed turn count for nothing.
    #[test]
    fn only_completed_goal_turns_consume_the_turn_budget() {
        fn origin_goal() -> agent_core::OpaqueJson {
            agent_core::OpaqueJson::parse(r#"{"continuation":"goal","iteration":1}"#.to_owned())
                .expect("test origin")
        }
        let mut goal_message = Message::user("continue");
        if let Message::User { meta, .. } = &mut goal_message {
            meta.origin = Some(origin_goal());
        }
        let plain = Message::user("a user note");
        let usage = agent_core::ModelUsage {
            input: Some(100),
            output: Some(50),
            total: None,
            cache_read: None,
            cache_write: None,
            input_includes_cache: None,
            details: None,
        };
        let goal = fold(&[
            set("2026-10-20T10:00:00Z", "ship it"),
            event(
                "2026-10-20T10:00:01Z",
                EventData::InboxEnqueued {
                    message: goal_message,
                    message_id: MessageId("m-1".into()),
                    lane: crate::runtime::InboxLane::NextTurn,
                    source: agent_core::InputSource::User,
                    wake: true,
                    degraded: false,
                },
            ),
            event(
                "2026-10-20T10:00:02Z",
                EventData::InboxEnqueued {
                    message: plain,
                    message_id: MessageId("m-2".into()),
                    lane: crate::runtime::InboxLane::NextTurn,
                    source: agent_core::InputSource::User,
                    wake: true,
                    degraded: false,
                },
            ),
            event(
                "2026-10-20T10:00:03Z",
                EventData::InputClaimed {
                    message_ids: vec![MessageId("m-1".into())],
                    turn_id: "t-goal-1".into(),
                },
            ),
            event(
                "2026-10-20T10:00:04Z",
                EventData::TurnFinished {
                    turn_id: "t-goal-1".into(),
                    outcome: TurnOutcome::Completed,
                    reason: None,
                    stop_reason: None,
                },
            ),
            // The failed goal turn and the ordinary completed turn that follow.
            event(
                "2026-10-20T10:00:05Z",
                EventData::InputClaimed {
                    message_ids: vec![MessageId("m-1".into())],
                    turn_id: "t-goal-2".into(),
                },
            ),
            event(
                "2026-10-20T10:00:06Z",
                EventData::TurnFinished {
                    turn_id: "t-goal-2".into(),
                    outcome: TurnOutcome::Failed,
                    reason: None,
                    stop_reason: None,
                },
            ),
            event(
                "2026-10-20T10:00:07Z",
                EventData::InputClaimed {
                    message_ids: vec![MessageId("m-2".into())],
                    turn_id: "t-plain".into(),
                },
            ),
            event(
                "2026-10-20T10:00:08Z",
                EventData::TurnFinished {
                    turn_id: "t-plain".into(),
                    outcome: TurnOutcome::Completed,
                    reason: None,
                    stop_reason: None,
                },
            ),
            // Usage inside the still-open span bills to the goal.
            event(
                "2026-10-20T10:00:09Z",
                EventData::ModelUsageRecorded { usage, fee: None },
            ),
            event("2026-10-20T10:00:10Z", EventData::GoalPaused {}),
            // Usage after the pause does not.
            event(
                "2026-10-20T10:00:11Z",
                EventData::ModelUsageRecorded {
                    usage: agent_core::ModelUsage {
                        input: Some(1000),
                        output: Some(1000),
                        total: None,
                        cache_read: None,
                        cache_write: None,
                        input_includes_cache: None,
                        details: None,
                    },
                    fee: None,
                },
            ),
        ])
        .expect("a goal");
        assert_eq!(goal.turns_used, 1, "only the completed goal turn counts");
        assert_eq!(goal.tokens_used, 150, "only in-span conversation usage bills");
        assert_eq!(goal.active_secs, 10);
    }

    /// A compaction in the journal (candidate + replacement) never disturbs the goal: the
    /// fold reads only the `Goal*` events, and the block lives outside the transcript, so
    /// a summary can neither contain it nor drop it.
    #[test]
    fn a_compaction_in_the_journal_leaves_the_goal_alone() {
        let usage = agent_core::ModelUsage {
            input: Some(10),
            output: Some(5),
            total: None,
            cache_read: None,
            cache_write: None,
            input_includes_cache: None,
            details: None,
        };
        let goal = fold(&[
            set("2026-10-20T10:00:00Z", "ship it"),
            event(
                "2026-10-20T10:00:01Z",
                EventData::ModelUsageRecorded { usage, fee: None },
            ),
            event(
                "2026-10-20T10:00:02Z",
                EventData::ContextCompacted {
                    revision: 1,
                    source_revision: 0,
                    source_len: 4,
                    summary: "did stuff".into(),
                    first_kept: 2,
                    tokens_before: 100,
                    tokens_after: 50,
                    below_target: true,
                    model: "m".into(),
                    deterministic: false,
                },
            ),
        ])
        .expect("a goal");
        assert_eq!(goal.objective, "ship it");
        assert_eq!(goal.status, GoalStatus::Active);
        assert_eq!(goal.tokens_used, 15);
    }

    /// A resume never auto-continues: `Active` folds back as `Paused`.
    #[test]
    fn fold_for_resume_parks_an_active_goal() {
        let goal = fold_for_resume(&[set("2026-10-20T10:00:00Z", "ship it")]).expect("a goal");
        assert_eq!(goal.status, GoalStatus::Paused);
        assert_eq!(goal.turns_used, 0, "the counters survive the park");
    }

    /// The token rule: the provider's `total` wins; otherwise prompt (which already settles
    /// the cache question) plus output. An unmarked old record bills its input at face
    /// value — the context budget's own lower-bound convention.
    #[test]
    fn token_accounting_prefers_total_and_never_double_counts_cache() {
        let base = agent_core::ModelUsage {
            input: Some(100),
            output: Some(50),
            total: None,
            cache_read: Some(30),
            cache_write: Some(10),
            input_includes_cache: None,
            details: None,
        };
        assert_eq!(goal_tokens(&base), 150, "unmarked: input at face value plus output");
        assert_eq!(
            goal_tokens(&agent_core::ModelUsage {
                total: Some(1000),
                ..base.clone()
            }),
            1000
        );
        assert_eq!(
            goal_tokens(&agent_core::ModelUsage {
                input_includes_cache: Some(false),
                ..base.clone()
            }),
            190,
            "Anthropic-shaped: uncached input plus its cache siblings plus output"
        );
    }

    /// A bad clock never loses the goal: unparseable `ts` folds as zero-length spans.
    #[test]
    fn an_unparseable_clock_costs_duration_math_not_the_goal() {
        assert_eq!(parse_ts_secs("not-a-ts"), 0);
        assert_eq!(parse_ts_secs("2026-10-20T10:00:00Z"), parse_ts_secs("2026-10-20T10:00:00.5Z"));
        let goal = fold(&[
            set("garbage", "ship it"),
            event("garbage", EventData::GoalPaused {}),
        ])
        .expect("a goal");
        assert_eq!(goal.active_secs, 0);
    }
}
