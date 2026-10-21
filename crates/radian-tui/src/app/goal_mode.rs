//! The `/goal` continuation (backlog `goal-mode.md`, step 3): the durable goal's live
//! half — the turn-end input beside `/loop`'s, the `/goal` commands, and the wait deadline.
//!
//! The durable state lives in the shared [`radian_host::SharedGoal`] (folded once from the
//! journal at open); this module owns the live half beside it: the current active stretch
//! (started clock, in-stretch tokens), the send guards (`inflight`/`outstanding`/
//! `awaiting_echo`, the same shape as the loop's), the armed wait deadline, and the herdr
//! block flag. Commands journal first and mutate on success (the loop reports back as
//! `Input::GoalDone`); turn-end stops mutate at once and journal as audit — the decision
//! gates the send, so it cannot wait a round trip.

use std::collections::HashSet;
use std::time::Duration;

use agent_core::{Message, OpaqueJson};
use radian_session::{
    GoalBudget, GoalState, GoalStatus, RuntimeEvent, TurnOutcome, goal_tokens,
    is_goal_continuation,
};

use super::*;
use crate::command::{GoalBudgetSpec, GoalCommand};

/// What a journaled `/goal` command applies when its append succeeds. The loop journals
/// the event; `Input::GoalDone` applies this. Every arm is idempotent and states its own
/// refusal when the goal moved on while the append was in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalApply {
    Set {
        objective: String,
        budget: GoalBudget,
    },
    Budget {
        budget: GoalBudget,
    },
    Pause,
    Resume,
    Drop,
    Exhaust {
        kind: String,
    },
}

/// The goal's live half. The shared durable state is behind `shared`; everything else is
/// process-local like the loop's (quitting the TUI ends the stretches and deadlines, the
/// journal keeps the goal).
pub struct GoalMode {
    /// The durable goal, shared with the `goal` tool and the `<goal>` prompt hook.
    pub shared: radian_host::SharedGoal,
    /// The current active stretch started here (the loop's clock). `None` while the goal
    /// is not actively running (paused, finished, or no goal).
    pub stretch_start: Option<Instant>,
    /// Tokens the running stretch consumed (live responses, same [`goal_tokens`] rule the
    /// fold uses; banked into the shared counters at every bank point).
    pub stretch_tokens: u64,
    /// When the latest admitted goal turn started (starts the `wait` clock).
    pub last_start: Option<Instant>,
    /// The `wait` deadline the loop waits for while idle (`LoopDeadline` fires it).
    pub next_due: Option<Instant>,
    /// The running turn is goal-attributed (set when its goal-tagged input is admitted).
    pub inflight: bool,
    /// Goal-tagged followups the runtime has enqueued but not admitted yet.
    pub outstanding: HashSet<MessageId>,
    /// A goal followup effect was produced but its `InboxEnqueued` echo has not arrived.
    pub awaiting_echo: bool,
    /// A budget stop waits while idle: herdr reports `blocked` until the next turn starts.
    pub blocked: bool,
}

impl GoalMode {
    pub fn new(shared: radian_host::SharedGoal) -> Self {
        Self {
            shared,
            stretch_start: None,
            stretch_tokens: 0,
            last_start: None,
            next_due: None,
            inflight: false,
            outstanding: HashSet::new(),
            awaiting_echo: false,
            blocked: false,
        }
    }

    /// The shared goal, locked briefly.
    fn goal(&self) -> std::sync::MutexGuard<'_, Option<GoalState>> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for GoalMode {
    fn default() -> Self {
        Self::new(radian_host::fresh_share())
    }
}

/// A goal continuation message: the fixed instruction plus the objective and the latest
/// progress. The `origin` audits it as the machine's re-send (ADR 0005 G1, the loop's
/// shape); `source = User` is attached later, at the inbox boundary (ADR 0011 L2).
pub fn goal_message(objective: &str, progress: Option<&str>, iteration: u64) -> Message {
    let origin = OpaqueJson::parse(
        serde_json::json!({ "continuation": "goal", "iteration": iteration }).to_string(),
    )
    .expect("the goal origin we build is valid JSON");
    let mut message = Message::user(format!(
        "Keep working toward the goal; when it is done, call the `goal` tool with \
         `op: \"done\"`.\nObjective: {objective}\nLatest progress: {}",
        progress.unwrap_or("none yet")
    ));
    match &mut message {
        Message::User { meta, .. } => meta.origin = Some(origin),
        _ => unreachable!("Message::user builds the User variant"),
    }
    message
}

/// The goal iteration a message carries, or `None` when it is not a goal re-send.
pub fn goal_iteration(message: &Message) -> Option<u64> {
    if !is_goal_continuation(message) {
        return None;
    }
    let meta = match message {
        Message::User { meta, .. }
        | Message::Assistant { meta, .. }
        | Message::Tool { meta, .. } => meta,
    };
    let origin = meta.origin.as_ref()?;
    let value: serde_json::Value = serde_json::from_str(origin.get()).ok()?;
    value.get("iteration")?.as_u64()
}

impl App {
    /// Replaces the read handle with the host's shared goal, so the TUI, the `goal` tool
    /// and the prompt hook advance the same state.
    pub fn set_goal(&mut self, goal: radian_host::SharedGoal) {
        self.session.goal_mode.shared = goal;
    }

    /// Whether a goal blocks `/loop`: an active or paused goal owns the turn-end decision.
    /// A finished goal (done, exhausted) or no goal leaves the loop alone.
    pub(super) fn goal_blocks_loop(&self) -> bool {
        matches!(
            self.session.goal_mode.goal().as_ref().map(|goal| goal.status),
            Some(GoalStatus::Active) | Some(GoalStatus::Paused)
        )
    }

    /// Runs one `/goal` line. `Show` reads the shared state; every change journals first
    /// through `Effect::GoalAppend` and applies when the loop reports `Input::GoalDone` —
    /// a failed append changes nothing.
    pub(super) fn goal_command(&mut self, command: GoalCommand) -> UpdateResult {
        match command {
            GoalCommand::Show => {
                for line in self.goal_status_lines() {
                    self.push_notice(line);
                }
                UpdateResult::dirty()
            }
            GoalCommand::Set { objective } => {
                if let Some(refused) = self.refuse_if_session_ended() {
                    return refused;
                }
                if !self.session.has_model {
                    self.push_notice(
                        "no model is set; choose one with /model (and /login if the provider \
                         needs a key) — the goal was not set",
                    );
                    return UpdateResult::dirty();
                }
                if self.session.loop_state.active {
                    self.push_notice("goal not set: /loop is on (stop it first with /loop)");
                    return UpdateResult::dirty();
                }
                let budget = GoalState::default_budget();
                UpdateResult::effect(Effect::GoalAppend {
                    event: RuntimeEvent::GoalSet {
                        objective: objective.clone(),
                        max_turns: budget.max_turns,
                        max_duration_secs: budget.max_duration_secs,
                        max_tokens: budget.max_tokens,
                    },
                    apply: GoalApply::Set { objective, budget },
                })
            }
            GoalCommand::Pause => {
                let status = self.session.goal_mode.goal().as_ref().map(|goal| goal.status);
                match status {
                    Some(GoalStatus::Active) => UpdateResult::effect(Effect::GoalAppend {
                        event: RuntimeEvent::GoalPaused {},
                        apply: GoalApply::Pause,
                    }),
                    Some(other) => {
                        self.push_notice(format!(
                            "goal is already {}; nothing to pause",
                            goal_status_word(other)
                        ));
                        UpdateResult::dirty()
                    }
                    None => {
                        self.push_notice("no goal to pause; /goal <objective> sets one");
                        UpdateResult::dirty()
                    }
                }
            }
            GoalCommand::Resume => {
                if self.session.loop_state.active {
                    self.push_notice("goal not resumed: /loop is on (stop it first with /loop)");
                    return UpdateResult::dirty();
                }
                let status = self.session.goal_mode.goal().as_ref().map(|goal| goal.status);
                match status {
                    Some(GoalStatus::Paused) => UpdateResult::effect(Effect::GoalAppend {
                        event: RuntimeEvent::GoalResumed {},
                        apply: GoalApply::Resume,
                    }),
                    Some(GoalStatus::Active) => {
                        self.push_notice("goal is already active");
                        UpdateResult::dirty()
                    }
                    Some(other) => {
                        self.push_notice(format!(
                            "goal is {}; /goal <objective> sets a new one",
                            goal_status_word(other)
                        ));
                        UpdateResult::dirty()
                    }
                    None => {
                        self.push_notice("no goal to resume; /goal <objective> sets one");
                        UpdateResult::dirty()
                    }
                }
            }
            GoalCommand::Drop => {
                if self.session.goal_mode.goal().is_none() {
                    self.push_notice("no goal to drop");
                    return UpdateResult::dirty();
                }
                UpdateResult::effect(Effect::GoalAppend {
                    event: RuntimeEvent::GoalDropped {},
                    apply: GoalApply::Drop,
                })
            }
            GoalCommand::Budget { specs } => self.goal_budget_command(specs),
        }
    }

    /// Runs `/goal budget …`: empty specs show the budget, otherwise the caps change. A
    /// raise on an exhausted goal reactivates it; a finished goal refuses (set a new one).
    fn goal_budget_command(&mut self, specs: Vec<GoalBudgetSpec>) -> UpdateResult {
        let current = self.session.goal_mode.goal().clone();
        let Some(goal) = current else {
            self.push_notice("no goal; /goal <objective> sets one first");
            return UpdateResult::dirty();
        };
        if specs.is_empty() {
            for line in self.goal_status_lines() {
                self.push_notice(line);
            }
            return UpdateResult::dirty();
        }
        if goal.status == GoalStatus::Done {
            self.push_notice("goal is done; /goal <objective> sets a new one");
            return UpdateResult::dirty();
        }
        let mut budget = goal.budget;
        for spec in &specs {
            match spec {
                GoalBudgetSpec::Turns(n) => budget.max_turns = Some(*n),
                GoalBudgetSpec::Duration(d) => budget.max_duration_secs = Some(d.as_secs()),
                GoalBudgetSpec::Tokens(n) => budget.max_tokens = Some(*n),
            }
        }
        UpdateResult::effect(Effect::GoalAppend {
            event: RuntimeEvent::GoalBudgetSet {
                max_turns: budget.max_turns,
                max_duration_secs: budget.max_duration_secs,
                max_tokens: budget.max_tokens,
            },
            apply: GoalApply::Budget { budget },
        })
    }

    /// Applies a journaled `/goal` command (`Input::GoalDone`). A failed append changes
    /// nothing; every arm re-checks the live state, so a goal that moved on while the
    /// append was in flight states its own refusal instead of mis-applying.
    pub(super) fn goal_applied(
        &mut self,
        apply: GoalApply,
        result: Result<(), String>,
    ) -> UpdateResult {
        let Ok(()) = result else {
            let error = result.unwrap_err();
            self.push_notice(format!("goal change was not journaled: {error}; nothing changed"));
            return UpdateResult::dirty();
        };
        match apply {
            GoalApply::Set { objective, budget } => {
                *self.session.goal_mode.goal() =
                    Some(GoalState::fresh(objective.clone(), budget));
                self.session.goal_mode.blocked = false;
                self.session.goal_mode.next_due = None;
                self.open_goal_stretch();
                self.push_notice(format!(
                    "goal set: {} ({}; /goal budget … to change it)",
                    truncate_notice(&objective),
                    budget_words(&budget)
                ));
                self.maybe_start_goal()
            }
            GoalApply::Budget { budget } => {
                let status = self
                    .session
                    .goal_mode
                    .goal()
                    .as_ref()
                    .map(|goal| goal.status);
                let Some(status) = status else {
                    self.push_notice("no goal; /goal <objective> sets one first");
                    return UpdateResult::dirty();
                };
                if let Some(goal) = self.session.goal_mode.goal().as_mut() {
                    goal.budget = budget;
                    if status == GoalStatus::BudgetExhausted {
                        goal.status = GoalStatus::Active;
                    }
                }
                if status == GoalStatus::BudgetExhausted {
                    self.session.goal_mode.blocked = false;
                    self.open_goal_stretch();
                    self.push_notice(format!(
                        "goal budget: {}; goal active again",
                        budget_words(&budget)
                    ));
                    return self.maybe_start_goal();
                }
                self.push_notice(format!("goal budget: {}", budget_words(&budget)));
                UpdateResult::dirty()
            }
            GoalApply::Pause => {
                let status = self
                    .session
                    .goal_mode
                    .goal()
                    .as_ref()
                    .map(|goal| goal.status);
                match status {
                    None => {
                        self.push_notice("no goal to pause; /goal <objective> sets one");
                        UpdateResult::dirty()
                    }
                    Some(GoalStatus::Active) => {
                        if let Some(goal) = self.session.goal_mode.goal().as_mut() {
                            goal.status = GoalStatus::Paused;
                        }
                        self.bank_goal_stretch();
                        self.push_notice("goal paused; /goal resume to continue");
                        UpdateResult::dirty()
                    }
                    Some(other) => {
                        self.push_notice(format!(
                            "goal is already {}; nothing to pause",
                            goal_status_word(other)
                        ));
                        UpdateResult::dirty()
                    }
                }
            }
            GoalApply::Resume => {
                let status = self
                    .session
                    .goal_mode
                    .goal()
                    .as_ref()
                    .map(|goal| goal.status);
                match status {
                    None => {
                        self.push_notice("no goal to resume; /goal <objective> sets one");
                        UpdateResult::dirty()
                    }
                    Some(GoalStatus::Paused) => {
                        if let Some(goal) = self.session.goal_mode.goal().as_mut() {
                            goal.status = GoalStatus::Active;
                        }
                        self.open_goal_stretch();
                        self.push_notice(
                            "goal resumed: continuing every turn until done or budgeted",
                        );
                        self.maybe_start_goal()
                    }
                    Some(other) => {
                        self.push_notice(format!(
                            "goal is {}; nothing to resume",
                            goal_status_word(other)
                        ));
                        UpdateResult::dirty()
                    }
                }
            }
            GoalApply::Drop => {
                *self.session.goal_mode.goal() = None;
                self.session.goal_mode.stretch_start = None;
                self.session.goal_mode.stretch_tokens = 0;
                self.session.goal_mode.next_due = None;
                self.session.goal_mode.blocked = false;
                self.push_notice("goal dropped");
                UpdateResult::dirty()
            }
            GoalApply::Exhaust { kind: _ } => {
                // Stops mutate at the turn end itself (the decision gates the send); the
                // completion only reports a lost audit write.
                UpdateResult::dirty()
            }
        }
    }

    /// The goal's status lines for `/goal` and `/goal budget` (bare).
    fn goal_status_lines(&self) -> Vec<String> {
        let guard = self.session.goal_mode.goal();
        let Some(goal) = guard.as_ref() else {
            return vec!["no goal; /goal <objective> sets one".to_owned()];
        };
        let mut lines = vec![
            format!("goal: {}", truncate_notice(&goal.objective)),
            format!(
                "status: {}",
                match goal.status {
                    GoalStatus::BudgetExhausted => format!(
                        "budget-exhausted ({})",
                        goal.exhausted_kind.as_deref().unwrap_or("?")
                    ),
                    other => goal_status_word(other).to_owned(),
                }
            ),
            format!(
                "progress: {}",
                goal.progress.as_deref().unwrap_or("none yet")
            ),
            format!(
                "budget: {} turns used{} · {} active{} · {} tokens used{}",
                goal.turns_used,
                match goal.budget.max_turns {
                    Some(max) => format!(" / {max}"),
                    None => " (no cap)".to_owned(),
                },
                super::loop_mode::format_loop_duration(Duration::from_secs(goal.active_secs)),
                match goal.budget.max_duration_secs {
                    Some(max) => format!(
                        " / {}",
                        super::loop_mode::format_loop_duration(Duration::from_secs(max))
                    ),
                    None => " (no cap)".to_owned(),
                },
                goal.tokens_used,
                match goal.budget.max_tokens {
                    Some(max) => format!(" / {max}"),
                    None => " (no cap)".to_owned(),
                },
            ),
        ];
        if goal.status == GoalStatus::Paused {
            lines.push("/goal resume to continue".to_owned());
        }
        lines
    }

    /// Opens the active stretch now (the loop's clock): time and tokens accrue to the goal
    /// from here until the next bank point.
    fn open_goal_stretch(&mut self) {
        self.session.goal_mode.stretch_start = Some(self.now);
        self.session.goal_mode.stretch_tokens = 0;
    }

    /// Banks the open stretch into the shared counters (time and tokens so far).
    fn bank_goal_stretch(&mut self) {
        let elapsed = self
            .session
            .goal_mode
            .stretch_start
            .map(|start| self.now.saturating_duration_since(start).as_secs())
            .unwrap_or(0);
        let tokens = self.session.goal_mode.stretch_tokens;
        self.session.goal_mode.stretch_start = None;
        self.session.goal_mode.stretch_tokens = 0;
        if let Some(goal) = self.session.goal_mode.goal().as_mut() {
            goal.active_secs += elapsed;
            goal.tokens_used += tokens;
        }
    }

    /// A live model response's usage accrues to the open stretch (the fold's
    /// [`goal_tokens`] rule, so the live count and the resume count agree).
    pub(super) fn note_goal_usage(&mut self, usage: &agent_core::ModelUsage) {
        if self.session.goal_mode.stretch_start.is_some() {
            self.session.goal_mode.stretch_tokens += goal_tokens(usage);
        }
    }

    /// The goal's current totals: the banked counters plus the open stretch.
    fn goal_totals(&self) -> (u64, u64, u64) {
        let guard = self.session.goal_mode.goal();
        let Some(goal) = guard.as_ref() else {
            return (0, 0, 0);
        };
        let stretch_secs = self
            .session
            .goal_mode
            .stretch_start
            .map(|start| self.now.saturating_duration_since(start).as_secs())
            .unwrap_or(0);
        (
            goal.turns_used,
            goal.active_secs + stretch_secs,
            goal.tokens_used + self.session.goal_mode.stretch_tokens,
        )
    }

    /// THE turn-end input beside `/loop`'s (step 3): one place where a finished goal turn
    /// either banks and re-sends, waits, or stops. Runs on the journaled `TurnFinished`,
    /// after `loop_turn_finished`. Returns the followup effect when the goal continues now,
    /// or nothing when it waits, pauses or stops. Stops mutate at once (the decision gates
    /// the send) and journal as audit through `Effect::GoalAppend`.
    pub(super) fn goal_turn_finished(&mut self, outcome: TurnOutcome) -> UpdateResult {
        // Mutual exclusion: the loop owns the turn end while it runs.
        if self.session.loop_state.active {
            return UpdateResult::none();
        }
        let status = self
            .session
            .goal_mode
            .goal()
            .as_ref()
            .map(|goal| goal.status);
        let was_inflight = std::mem::replace(&mut self.session.goal_mode.inflight, false);
        match status {
            None => UpdateResult::none(),
            Some(GoalStatus::Done) => {
                // The tool ended the goal mid-turn; the turn's end carries the notice.
                // Only the goal's own turn reports it — a later user turn stays quiet.
                if was_inflight {
                    let note = self
                        .session
                        .goal_mode
                        .goal()
                        .as_ref()
                        .and_then(|goal| goal.progress.clone());
                    self.bank_goal_stretch();
                    match note {
                        Some(note) => self.push_notice(format!("goal done: {note}")),
                        None => self.push_notice("goal done"),
                    }
                    return UpdateResult::dirty();
                }
                UpdateResult::none()
            }
            Some(GoalStatus::BudgetExhausted) | Some(GoalStatus::Paused) => UpdateResult::none(),
            Some(GoalStatus::Active) => match (was_inflight, outcome) {
                (true, TurnOutcome::Completed) => {
                    // A completed goal turn consumes the turn budget.
                    if let Some(goal) = self.session.goal_mode.goal().as_mut() {
                        goal.turns_used += 1;
                    }
                    self.bank_goal_stretch();
                    self.open_goal_stretch();
                    self.maybe_continue_goal()
                }
                (true, TurnOutcome::Cancelled) => {
                    self.bank_goal_stretch();
                    self.pause_goal("goal paused: the turn was cancelled; /goal resume to continue");
                    UpdateResult::dirty()
                }
                (true, TurnOutcome::Failed) => {
                    self.bank_goal_stretch();
                    self.pause_goal("goal paused: the turn failed; /goal resume to continue");
                    UpdateResult::dirty()
                }
                (true, _) => {
                    self.bank_goal_stretch();
                    let word = super::loop_mode::turn_outcome_word(outcome);
                    self.pause_goal(&format!("goal paused: the turn ended as {word}"));
                    UpdateResult::dirty()
                }
                // A finished turn that was not the goal's: a completed one continues the
                // goal (a steered one-off never consumes its budget); a cancelled or failed
                // one pauses it instead of blind-firing into whoever cancelled or broke it.
                (false, TurnOutcome::Completed) => self.maybe_continue_goal(),
                (false, TurnOutcome::Cancelled) => {
                    self.pause_goal("goal paused: the turn was cancelled; /goal resume to continue");
                    UpdateResult::dirty()
                }
                (false, TurnOutcome::Failed) => {
                    self.pause_goal("goal paused: the turn failed; /goal resume to continue");
                    UpdateResult::dirty()
                }
                (false, _) => {
                    let word = super::loop_mode::turn_outcome_word(outcome);
                    self.pause_goal(&format!("goal paused: the turn ended as {word}"));
                    UpdateResult::dirty()
                }
            },
        }
    }

    /// Pauses the live goal (the stretch is banked by the caller): the mode stays armed on
    /// the shared state, and `/goal resume` restarts it.
    fn pause_goal(&mut self, notice: &str) {
        if let Some(goal) = self.session.goal_mode.goal().as_mut()
            && goal.status == GoalStatus::Active
        {
            goal.status = GoalStatus::Paused;
        }
        self.session.goal_mode.stretch_start = None;
        self.session.goal_mode.stretch_tokens = 0;
        self.session.goal_mode.next_due = None;
        self.push_notice(notice.to_owned());
    }

    /// Sends the next goal continuation now, unless a cap stops the goal or a `wait`
    /// makes it wait. Stopping mutates at once (the decision gates the send) with one
    /// notice, and journals the stop as audit.
    fn maybe_continue_goal(&mut self) -> UpdateResult {
        let (objective, progress, budget, turns) = {
            let guard = self.session.goal_mode.goal();
            let Some(goal) = guard.as_ref() else {
                return UpdateResult::none();
            };
            if goal.status != GoalStatus::Active {
                return UpdateResult::none();
            }
            (
                goal.objective.clone(),
                goal.progress.clone(),
                goal.budget,
                goal.turns_used,
            )
        };
        // The stretch totals lock separately: holding the guard above across this call
        // would deadlock the same thread on the shared mutex.
        let (_, secs, tokens) = self.goal_totals();
        if self.session.goal_mode.awaiting_echo || !self.session.goal_mode.outstanding.is_empty()
        {
            // A send is in flight or queued: it runs next already. The caps re-check at
            // its end (at worst one extra turn, the loop's own rule).
            return UpdateResult::none();
        }
        if let Some(max) = budget.max_turns
            && turns >= max
        {
            return self.stop_goal_for_budget(
                "turns",
                format!("goal stopped: turn budget exhausted ({turns}/{max} turns); \
                         /goal budget … raises it or /goal <objective> sets a new goal"),
            );
        }
        if let Some(max) = budget.max_duration_secs
            && secs >= max
        {
            return self.stop_goal_for_budget(
                "duration",
                format!("goal stopped: time budget exhausted ({} active of {}); \
                         /goal budget … raises it or /goal <objective> sets a new goal",
                    super::loop_mode::format_loop_duration(Duration::from_secs(secs)),
                    super::loop_mode::format_loop_duration(Duration::from_secs(max))),
            );
        }
        if let Some(max) = budget.max_tokens
            && tokens >= max
        {
            return self.stop_goal_for_budget(
                "tokens",
                format!("goal stopped: token budget exhausted ({tokens}/{max} tokens); \
                         /goal budget … raises it or /goal <objective> sets a new goal"),
            );
        }
        // A fresh `wait` overrides an armed one (several waits merge — the last one wins,
        // like the loop's merged deadlines); an armed one holds until its deadline.
        let wait = self
            .session
            .goal_mode
            .goal()
            .as_mut()
            .and_then(|goal| goal.pending_wait_secs.take());
        if let Some(wait) = wait {
            // A `goal wait` defers the next continuation past this turn.
            let start = self.session.goal_mode.last_start.unwrap_or(self.now);
            self.session.goal_mode.next_due = Some(start + Duration::from_secs(wait));
            return UpdateResult::dirty();
        }
        if let Some(due) = self.session.goal_mode.next_due
            && self.now < due
        {
            // A `wait` armed earlier still holds: a turn that ends before its deadline does
            // not cut it short (the deadline fires it).
            return UpdateResult::dirty();
        }
        self.emit_goal_followup(&objective, progress.as_deref(), turns + 1)
    }

    /// A budget cap stopped the goal: terminal, visible like the ADR 0037 wake-gate stop
    /// (notice, herdr `blocked`, journal). Mutates at once — the decision gates the send —
    /// and the `GoalBudgetExhausted` record it journals is the audit.
    fn stop_goal_for_budget(&mut self, kind: &str, notice: String) -> UpdateResult {
        self.bank_goal_stretch();
        if let Some(goal) = self.session.goal_mode.goal().as_mut() {
            goal.status = GoalStatus::BudgetExhausted;
            goal.exhausted_kind = Some(kind.to_owned());
            goal.pending_wait_secs = None;
        }
        self.session.goal_mode.next_due = None;
        self.session.goal_mode.blocked = true;
        self.push_notice(notice);
        UpdateResult::effect(Effect::GoalAppend {
            event: RuntimeEvent::GoalBudgetExhausted {
                kind: kind.to_owned(),
            },
            apply: GoalApply::Exhaust {
                kind: kind.to_owned(),
            },
        })
    }

    /// A set or resumed goal starts now when idle (like the loop's armed prompt): a turn
    /// already running continues the goal at its end instead.
    fn maybe_start_goal(&mut self) -> UpdateResult {
        if self.session.goal_mode.awaiting_echo
            || !self.session.goal_mode.outstanding.is_empty()
            || self.turn() == TurnState::Running
        {
            return UpdateResult::none();
        }
        self.maybe_continue_goal()
    }

    /// Sends one goal continuation: tags it, records the in-flight send, and produces the
    /// followup the goal runs through the facade (so it carries `source = User` like every
    /// other send).
    fn emit_goal_followup(
        &mut self,
        objective: &str,
        progress: Option<&str>,
        iteration: u64,
    ) -> UpdateResult {
        self.session.goal_mode.awaiting_echo = true;
        self.session.goal_mode.next_due = None;
        UpdateResult::effect(Effect::SendFollowup(goal_message(
            objective, progress, iteration,
        )))
    }

    /// The `wait` deadline the event loop waits for: only while idle with an active goal
    /// and nothing in flight or queued. While a turn runs the turn end decides.
    pub fn goal_deadline(&self) -> Option<Instant> {
        if self.turn() != TurnState::Running
            && matches!(
                self.session.goal_mode.goal().as_ref().map(|goal| goal.status),
                Some(GoalStatus::Active)
            )
            && !self.session.goal_mode.awaiting_echo
            && self.session.goal_mode.outstanding.is_empty()
        {
            self.session.goal_mode.next_due
        } else {
            None
        }
    }

    /// The `wait` is over while the driver is idle: continue the goal. A running turn
    /// ignores the deadline (its end decides); several due ticks merge into this one send.
    pub(super) fn goal_deadline_fired(&mut self) -> UpdateResult {
        if self.turn() == TurnState::Running
            || !matches!(
                self.session.goal_mode.goal().as_ref().map(|goal| goal.status),
                Some(GoalStatus::Active)
            )
            || self.session.goal_mode.awaiting_echo
            || !self.session.goal_mode.outstanding.is_empty()
        {
            return UpdateResult::none();
        }
        let Some(due) = self.session.goal_mode.next_due else {
            return UpdateResult::none();
        };
        if self.now < due {
            return UpdateResult::none();
        }
        self.session.goal_mode.next_due = None;
        self.maybe_continue_goal()
    }
}

/// Feeds the goal the runtime events it tracks: the `InboxEnqueued`/`InputAdmitted` echo
/// pair marks which turns are goal-attributed, `TurnStarted` opens the wait. Called from
/// `apply_runtime_event` before its own arms, beside the loop's tracker.
pub(super) fn track_goal_runtime_event(
    mode: &mut GoalMode,
    event: &RuntimeEvent,
    now: Instant,
) {
    match event {
        RuntimeEvent::InboxEnqueued {
            lane,
            message_id,
            message,
            ..
        } => {
            if goal_iteration(message).is_some() {
                mode.awaiting_echo = false;
                if *lane == radian_session::InboxLane::NextTurn {
                    mode.outstanding.insert(message_id.clone());
                } else if matches!(
                    mode.goal().as_ref().map(|goal| goal.status),
                    Some(GoalStatus::Active)
                ) {
                    mode.inflight = true;
                }
            }
        }
        RuntimeEvent::InputAdmitted { message_id, .. } => {
            if mode.outstanding.remove(message_id)
                && matches!(
                    mode.goal().as_ref().map(|goal| goal.status),
                    Some(GoalStatus::Active) | Some(GoalStatus::Paused)
                )
            {
                // The running turn admitted a goal-tagged input: it is the goal's, even
                // when the goal paused mid-flight (its end then stays quiet).
                mode.inflight = true;
                mode.last_start = Some(now);
            }
        }
        RuntimeEvent::TurnStarted { .. } => {
            // A new turn ends the wait: a stale `wait` deadline from before it must not
            // fire into the running turn.
            mode.next_due = None;
        }
        _ => {}
    }
}

/// The one-word status for notices.
fn goal_status_word(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Done => "done",
        GoalStatus::BudgetExhausted => "budget-exhausted",
    }
}

/// The compact budget words for the set/raise notices: `30 turns · 2h`.
fn budget_words(budget: &GoalBudget) -> String {
    let mut parts = Vec::new();
    if let Some(max) = budget.max_turns {
        parts.push(format!("{max} turns"));
    }
    if let Some(max) = budget.max_duration_secs {
        parts.push(super::loop_mode::format_loop_duration(
            Duration::from_secs(max),
        ));
    }
    if let Some(max) = budget.max_tokens {
        parts.push(format!("{max} tokens"));
    }
    if parts.is_empty() {
        return "uncapped".to_owned();
    }
    parts.join(" · ")
}

/// A notice-safe objective: the first 200 characters, with the cut marked.
fn truncate_notice(text: &str) -> String {
    const LIMIT: usize = 200;
    if text.chars().count() <= LIMIT {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(LIMIT - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use agent_core::InputSource;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::mention::Expansion;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Submits a line and returns every effect (`/goal` needs no `@` expansion).
    fn submit(app: &mut App, line: &str) -> Vec<Effect> {
        app.input = line.into();
        let mut seen = Vec::new();
        let mut queue: VecDeque<Effect> = app.on_key(key(KeyCode::Enter)).effects.into();
        while let Some(effect) = queue.pop_front() {
            seen.push(effect.clone());
            if let Effect::ExpandMentions { text } = &effect {
                queue.extend(
                    app.update(Input::MentionsExpanded(Ok(Expansion {
                        text: text.clone(),
                        attachments: Vec::new(),
                        warnings: Vec::new(),
                    })))
                    .effects,
                );
            }
        }
        seen
    }

    fn notices(app: &App) -> Vec<String> {
        app.session
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Notice)
            .map(|entry| entry.text.clone())
            .collect()
    }

    fn enqueued(app: &mut App, id: &str, message: Message) {
        app.update(Input::Runtime(RuntimeEvent::InboxEnqueued {
            message,
            message_id: MessageId(id.to_owned()),
            lane: radian_session::InboxLane::NextTurn,
            source: InputSource::User,
            wake: true,
            degraded: false,
        }));
    }

    fn admitted(app: &mut App, id: &str) {
        app.update(Input::Runtime(RuntimeEvent::InputAdmitted {
            message_id: MessageId(id.to_owned()),
            transcript_index: 0,
        }));
    }

    fn started(app: &mut App, turn: &str) {
        app.update(Input::Runtime(RuntimeEvent::TurnStarted {
            turn_id: turn.to_owned(),
        }));
    }

    fn finished(app: &mut App, turn: &str, outcome: TurnOutcome) -> UpdateResult {
        app.update(Input::Runtime(RuntimeEvent::TurnFinished {
            turn_id: turn.to_owned(),
            outcome,
            reason: None,
            stop_reason: None,
        }))
    }

    /// One full goal iteration in production order: the echo, the start, the admission,
    /// then the end. Returns what the turn end produced.
    fn run_iteration(
        app: &mut App,
        id: &str,
        turn: &str,
        message: Message,
        outcome: TurnOutcome,
    ) -> UpdateResult {
        enqueued(app, id, message);
        started(app, turn);
        admitted(app, id);
        finished(app, turn, outcome)
    }

    /// Sets a goal through the command and its journal completion, like the loop does.
    fn set_goal(app: &mut App, objective: &str) -> Vec<Effect> {
        let effects = submit(app, &format!("/goal {objective}"));
        let (_event, apply) = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::GoalAppend { event, apply } => Some((event.clone(), apply.clone())),
                _ => None,
            })
            .expect("a set journals first");
        let completion = app.update(Input::GoalDone {
            apply,
            result: Ok(()),
        });
        [effects, completion.effects].concat()
    }

    fn shared_status(app: &App) -> Option<GoalStatus> {
        app.session
            .goal_mode
            .goal()
            .as_ref()
            .map(|goal| goal.status)
    }

    /// A goal-tagged message round-trips its iteration through `origin`; ordinary
    /// messages carry none.
    #[test]
    fn goal_messages_carry_the_iteration_in_origin() {
        assert_eq!(goal_iteration(&goal_message("o", None, 3)), Some(3));
        assert_eq!(goal_iteration(&Message::user("o")), None);
        let text = goal_message("ship it", Some("half"), 1).text();
        assert!(text.contains("ship it"), "{text}");
        assert!(text.contains("half"), "{text}");
        assert!(text.contains("goal"), "{text}");
    }

    /// `/goal <objective>` journals first (no state change yet); the loop's success
    /// applies it, notices, and sends the first continuation while idle.
    #[test]
    fn set_journals_first_and_applies_on_success() {
        let mut app = App::new("m", "s");
        let base = app.session.spinner_origin;
        app.set_now(base);
        app.input = "/goal ship it".into();
        let effects = app.on_key(key(KeyCode::Enter)).effects;
        assert_eq!(effects.len(), 1);
        let (event, apply) = match &effects[0] {
            Effect::GoalAppend { event, apply } => (event.clone(), apply.clone()),
            other => panic!("expected a journal append, got {other:?}"),
        };
        assert!(
            matches!(event, RuntimeEvent::GoalSet { .. }),
            "{event:?}"
        );
        assert!(
            app.session.goal_mode.goal().is_none(),
            "nothing applied before the append succeeds"
        );
        let completion = app.update(Input::GoalDone {
            apply,
            result: Ok(()),
        });
        assert_eq!(shared_status(&app), Some(GoalStatus::Active));
        assert!(
            notices(&app).iter().any(|n| n.contains("goal set: ship it")),
            "{:?}",
            notices(&app)
        );
        assert_eq!(
            completion.effects,
            vec![Effect::SendFollowup(goal_message("ship it", None, 1))],
            "the first continuation goes out while idle"
        );
    }

    /// A failed append changes nothing: the error names the journal, the goal stays absent.
    #[test]
    fn a_failed_append_changes_nothing() {
        let mut app = App::new("m", "s");
        submit(&mut app, "/goal ship it");
        let apply = GoalApply::Set {
            objective: "ship it".into(),
            budget: GoalState::default_budget(),
        };
        let result = app.update(Input::GoalDone {
            apply,
            result: Err("disk full".into()),
        });
        assert!(result.effects.is_empty());
        assert!(app.session.goal_mode.goal().is_none());
        assert!(
            notices(&app)
                .iter()
                .any(|n| n.contains("was not journaled")),
            "{:?}",
            notices(&app)
        );
    }

    /// A finished goal turn without `done` re-sends with the objective and the latest
    /// progress (the tool's note, written straight into the shared state).
    #[test]
    fn an_unfinished_turn_continues_with_the_latest_progress() {
        let mut app = App::new("m", "s");
        let base = app.session.spinner_origin;
        app.set_now(base);
        set_goal(&mut app, "ship it");
        // The model's `progress` call lands in the shared state mid-turn.
        app.session.goal_mode.goal().as_mut().expect("a goal").progress =
            Some("half there".into());

        let result = run_iteration(
            &mut app,
            "e-1",
            "t-1",
            goal_message("ship it", None, 1),
            TurnOutcome::Completed,
        );
        let [Effect::SendFollowup(sent)] = result.effects.as_slice() else {
            panic!("expected one continuation, got {:?}", result.effects);
        };
        // The text itself carries the objective and the latest progress — asserted here,
        // not just against `goal_message`, so a dropped progress fails this test.
        let text = sent.text();
        assert!(text.contains("ship it"), "{text}");
        assert!(text.contains("half there"), "{text}");
        assert_eq!(goal_iteration(sent), Some(2));
        assert_eq!(
            app.session
                .goal_mode
                .goal()
                .as_ref()
                .map(|goal| goal.turns_used),
            Some(1)
        );
    }

    /// The model called `goal done` mid-turn: the turn's end reports it once and sends
    /// nothing further.
    #[test]
    fn done_stops_the_continuation_with_a_notice() {
        let mut app = App::new("m", "s");
        let base = app.session.spinner_origin;
        app.set_now(base);
        set_goal(&mut app, "ship it");
        // The turn runs as the goal's (admitted while active), then the tool's write end
        // lands mid-turn: Done in the shared state (the host journals it).
        let message = goal_message("ship it", None, 1);
        enqueued(&mut app, "e-1", message);
        started(&mut app, "t-1");
        admitted(&mut app, "e-1");
        {
            let mut guard = app.session.goal_mode.goal();
            let goal = guard.as_mut().expect("a goal");
            goal.status = GoalStatus::Done;
            goal.progress = Some("shipped".into());
        }
        let result = finished(&mut app, "t-1", TurnOutcome::Completed);
        assert!(result.effects.is_empty(), "no continuation after done");
        assert!(
            notices(&app)
                .iter()
                .any(|n| n == "goal done: shipped"),
            "{:?}",
            notices(&app)
        );
        // A later user turn stays quiet: the notice fired exactly once.
        let result = run_iteration(
            &mut app,
            "e-2",
            "t-2",
            Message::user("thanks"),
            TurnOutcome::Completed,
        );
        assert!(result.effects.is_empty());
        assert_eq!(
            notices(&app).iter().filter(|n| n.contains("goal done")).count(),
            1
        );
    }

    /// `budget 2 turns` stops after exactly two turns: the second end notices, flags herdr,
    /// and journals the stop as audit.
    #[test]
    fn budget_two_turns_stops_after_exactly_two_turns() {
        let mut app = App::new("m", "s");
        let base = app.session.spinner_origin;
        app.set_now(base);
        set_goal(&mut app, "ship it");
        let effects = submit(&mut app, "/goal budget 2 turns");
        let apply = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::GoalAppend { apply, .. } => Some(apply.clone()),
                _ => None,
            })
            .expect("a budget journals");
        app.update(Input::GoalDone {
            apply,
            result: Ok(()),
        });

        let first = run_iteration(
            &mut app,
            "e-1",
            "t-1",
            goal_message("ship it", None, 1),
            TurnOutcome::Completed,
        );
        assert_eq!(first.effects.len(), 1, "the first turn continues");

        let second = run_iteration(
            &mut app,
            "e-2",
            "t-2",
            goal_message("ship it", None, 2),
            TurnOutcome::Completed,
        );
        assert_eq!(shared_status(&app), Some(GoalStatus::BudgetExhausted));
        assert!(
            notices(&app)
                .iter()
                .any(|n| n.contains("turn budget exhausted (2/2 turns)")),
            "{:?}",
            notices(&app)
        );
        // The stop journals as audit through the same effect shape.
        assert!(
            second.effects.iter().any(|effect| matches!(
                effect,
                Effect::GoalAppend {
                    apply: GoalApply::Exhaust { .. },
                    ..
                }
            )),
            "{:?}",
            second.effects
        );
        assert!(
            !second.effects.iter().any(|effect| matches!(
                effect,
                Effect::SendFollowup(_)
            )),
            "no third turn"
        );
        // herdr reports `blocked` with the goal label until the next turn starts.
        assert!(app.herdr_state_inputs().3);
        assert_eq!(
            app.herdr_blocked(),
            Some(radian_host::herdr::Blocked::GoalBudget)
        );
        started(&mut app, "t-3");
        assert!(!app.herdr_state_inputs().3);
        assert_eq!(app.herdr_blocked(), None);
    }

    /// `goal wait 600` defers the next continuation by 600 seconds on the injected clock;
    /// the deadline fires it.
    #[test]
    fn wait_delays_the_next_continuation() {
        let mut app = App::new("m", "s");
        let base = app.session.spinner_origin;
        app.set_now(base);
        set_goal(&mut app, "ship it");
        // The tool's write end: an armed wait in the shared state.
        app.session.goal_mode.goal().as_mut().expect("a goal").pending_wait_secs = Some(600);

        let result = run_iteration(
            &mut app,
            "e-1",
            "t-1",
            goal_message("ship it", None, 1),
            TurnOutcome::Completed,
        );
        assert!(result.effects.is_empty(), "the wait holds the send");
        assert_eq!(app.goal_deadline(), Some(base + Duration::from_secs(600)));
        // Early is a no-op; at 600s the deadline sends iteration 2.
        assert!(app.update(Input::LoopDeadline).effects.is_empty());
        app.set_now(base + Duration::from_secs(600));
        assert_eq!(
            app.update(Input::LoopDeadline).effects,
            vec![Effect::SendFollowup(goal_message("ship it", None, 2))]
        );
    }

    /// A cancelled goal turn pauses (the mode stays armed in the shared state); a failed
    /// one pauses the same way. `/goal resume` restarts them.
    #[test]
    fn a_cancelled_or_failed_turn_pauses_and_resume_restarts() {
        let mut app = App::new("m", "s");
        app.set_now(app.session.spinner_origin);
        set_goal(&mut app, "ship it");
        let result = run_iteration(
            &mut app,
            "e-1",
            "t-1",
            goal_message("ship it", None, 1),
            TurnOutcome::Cancelled,
        );
        assert!(result.effects.is_empty(), "a cancelled turn never re-sends");
        assert_eq!(shared_status(&app), Some(GoalStatus::Paused));
        assert!(
            notices(&app)
                .iter()
                .any(|n| n.contains("goal paused: the turn was cancelled")),
            "{:?}",
            notices(&app)
        );

        let effects = submit(&mut app, "/goal resume");
        let apply = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::GoalAppend { apply, .. } => Some(apply.clone()),
                _ => None,
            })
            .expect("a resume journals");
        let completion = app.update(Input::GoalDone {
            apply,
            result: Ok(()),
        });
        assert_eq!(shared_status(&app), Some(GoalStatus::Active));
        assert_eq!(
            completion.effects,
            vec![Effect::SendFollowup(goal_message("ship it", None, 1))],
            "resume continues while idle"
        );

        let result = run_iteration(
            &mut app,
            "e-2",
            "t-2",
            goal_message("ship it", None, 1),
            TurnOutcome::Failed,
        );
        assert!(result.effects.is_empty(), "never re-send into a broken provider");
        assert_eq!(shared_status(&app), Some(GoalStatus::Paused));
        assert!(
            notices(&app)
                .iter()
                .any(|n| n.contains("goal paused: the turn failed")),
            "{:?}",
            notices(&app)
        );
    }

    /// A resumed session's paused goal sends nothing at a turn end; `/goal resume`
    /// continues it.
    #[test]
    fn a_resumed_pause_sends_nothing_until_resumed() {
        let mut app = App::new("m", "s");
        app.set_now(app.session.spinner_origin);
        // The resume fold parks the goal (host side); the TUI shares that state.
        *app.session.goal_mode.goal() = Some(GoalState {
            status: GoalStatus::Paused,
            ..GoalState::fresh("ship it".into(), GoalState::default_budget())
        });
        let result = finished(&mut app, "t-0", TurnOutcome::Completed);
        assert!(result.effects.is_empty(), "no auto-continue after --resume");
        assert_eq!(shared_status(&app), Some(GoalStatus::Paused));
    }

    /// The loop and the goal refuse each other with a notice naming the way out.
    #[test]
    fn loop_and_goal_refuse_each_other() {
        let mut app = App::new("m", "s");
        app.set_now(app.session.spinner_origin);
        set_goal(&mut app, "ship it");
        assert!(submit(&mut app, "/loop go").is_empty());
        assert!(
            notices(&app)
                .iter()
                .any(|n| n.contains("loop not started: a goal is active")),
            "{:?}",
            notices(&app)
        );

        let mut app = App::new("m", "s");
        submit(&mut app, "/loop go");
        assert!(app.session.loop_state.active);
        assert!(submit(&mut app, "/goal ship it").is_empty());
        assert!(
            notices(&app)
                .iter()
                .any(|n| n.contains("goal not set: /loop is on")),
            "{:?}",
            notices(&app)
        );
    }

    /// `/goal` shows the status, progress and budget use; `/goal pause` banks and arms
    /// the resume hint.
    #[test]
    fn show_reports_status_progress_and_budget() {
        let mut app = App::new("m", "s");
        submit(&mut app, "/goal");
        assert!(
            notices(&app).iter().any(|n| n.contains("no goal")),
            "{:?}",
            notices(&app)
        );
        set_goal(&mut app, "ship it");
        submit(&mut app, "/goal pause");
        // The pause journals; apply it like the loop does.
        let result = app.update(Input::GoalDone {
            apply: GoalApply::Pause,
            result: Ok(()),
        });
        assert!(result.effects.is_empty());
        assert_eq!(shared_status(&app), Some(GoalStatus::Paused));
        submit(&mut app, "/goal");
        let shown = notices(&app).join("\n");
        assert!(shown.contains("status: paused"), "{shown}");
        assert!(shown.contains("/goal resume to continue"), "{shown}");
    }
}
