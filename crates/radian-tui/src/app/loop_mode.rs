//! The `/loop` continuation (backlog `tui-loop-continuation.md`, steps 1-2): the per-turn
//! repeat state, the one turn-end decision point (step 3, `goal-mode.md`, adds an input
//! here), and the `/loop` command itself.
//!
//! The loop re-sends one fixed prompt as `Inbox::followup` with `source = User` (ADR 0011:
//! only a `User` turn carries this session's approvals and memory) and a message `origin`
//! of `{"continuation":"loop","iteration":n}` (ADR 0005: the audit trail — the journal shows
//! which turns the machine submitted). It never touches the ADR 0037 wake gate; its own
//! caps (count and duration) are the bound.

use std::time::Duration;

use agent_core::{Message, OpaqueJson};
use radian_session::{RuntimeEvent, TurnOutcome};

use super::*;
use crate::command::{LoopCommand, LoopOptions};

/// The default cap: at most this many loop turns when `--times` is absent (written into
/// ADR 0011 by the docs-only commit; tuned from dogfood afterwards).
pub(crate) const DEFAULT_MAX_ITERATIONS: u64 = 20;

/// The default cap: stop this long after the first loop turn when `--for` is absent
/// (written into ADR 0011 by the docs-only commit).
pub(crate) const DEFAULT_MAX_DURATION: Duration = Duration::from_secs(3600);

/// The `/loop` state. Process-local only: quitting the TUI ends the loop, nothing is
/// journaled, and `radian exec` has none of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoopState {
    /// The mode is on (armed or iterating). A bare `/loop` flips this.
    pub(crate) active: bool,
    /// The prompt every iteration re-sends verbatim. `None` = armed: the next user message
    /// becomes it.
    pub(crate) prompt: Option<String>,
    /// Wait at least this long between iteration starts (`--every <dur>`).
    pub(crate) every: Option<Duration>,
    /// Stop after this many loop turns (`--times N`, default [`DEFAULT_MAX_ITERATIONS`]).
    pub(crate) max_iterations: u64,
    /// Whether `--times` was given (the stop notice names it; otherwise the default cap).
    pub(crate) times_explicit: bool,
    /// Stop this long after the first loop turn (`--for <dur>`, default
    /// [`DEFAULT_MAX_DURATION`]).
    pub(crate) max_duration: Duration,
    /// Whether `--for` was given (the stop notice names it; otherwise the default cap).
    pub(crate) for_explicit: bool,
    /// When the first loop turn was admitted (starts the `--for` clock).
    pub(crate) started_at: Option<Instant>,
    /// Completed loop turns. The next send is tagged `iterations + 1`.
    pub(crate) iterations: u64,
    /// When the latest loop turn was admitted (starts the `--every` clock).
    pub(crate) last_start: Option<Instant>,
    /// The `--every` deadline the loop waits for while idle (`LoopDeadline` fires it).
    pub(crate) next_due: Option<Instant>,
    /// Paused by Esc/a cancel (prompt dropped) or by a model error (prompt kept): the next
    /// user message becomes the new prompt and resumes. The mode stays on.
    pub(crate) paused: bool,
    /// The running turn is loop-attributed (set when its loop-tagged input is admitted).
    pub(crate) inflight: bool,
    /// Loop-tagged followups the runtime has enqueued but not admitted yet. The turn-end
    /// decision never sends while one is outstanding — the queued one runs next already.
    pub(crate) outstanding: std::collections::HashSet<MessageId>,
    /// A loop followup effect was produced but its `InboxEnqueued` echo has not arrived.
    /// The turn-end decision never sends while one is in flight: a turn that ends between
    /// the emission and the echo must not produce a second send. Cleared by the echo, or by
    /// a `send` refusal (the inbox rejected around now; the decision re-evaluates — a
    /// refusal the loop did not cause can at worst cost one extra iteration, still capped).
    pub(crate) awaiting_echo: bool,
}

impl Default for LoopState {
    fn default() -> Self {
        Self {
            active: false,
            prompt: None,
            every: None,
            max_iterations: DEFAULT_MAX_ITERATIONS,
            times_explicit: false,
            max_duration: DEFAULT_MAX_DURATION,
            for_explicit: false,
            started_at: None,
            iterations: 0,
            last_start: None,
            next_due: None,
            paused: false,
            inflight: false,
            outstanding: std::collections::HashSet::new(),
            awaiting_echo: false,
        }
    }
}

/// A user message tagged as loop iteration `n`: the `origin` the journal audits (ADR 0005).
/// `source = User` is attached later, at the inbox boundary (ADR 0011 L2), like every
/// other send.
pub fn loop_message(text: &str, iteration: u64) -> Message {
    let origin = OpaqueJson::parse(
        serde_json::json!({ "continuation": "loop", "iteration": iteration }).to_string(),
    )
    .expect("the loop origin we build is valid JSON");
    let mut message = Message::user(text);
    match &mut message {
        Message::User { meta, .. } => meta.origin = Some(origin),
        _ => unreachable!("Message::user builds the User variant"),
    }
    message
}

/// The loop iteration a message carries, or `None` when it is not a loop re-send.
pub fn loop_iteration(message: &Message) -> Option<u64> {
    let origin = match message {
        Message::User { meta, .. } => meta.origin.as_ref()?,
        Message::Assistant { meta, .. } => meta.origin.as_ref()?,
        Message::Tool { meta, .. } => meta.origin.as_ref()?,
    };
    let value: serde_json::Value = serde_json::from_str(origin.get()).ok()?;
    if value.get("continuation")?.as_str()? != "loop" {
        return None;
    }
    value.get("iteration")?.as_u64()
}

/// A compact duration: `45s` under a minute, `3m` under an hour, `2h` above (the footer's
/// `loop every 5m` and `loop · next in 3m` wording).
pub(crate) fn format_loop_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

impl App {
    /// Runs one `/loop` line. A bare `/loop` toggles; flags and/or a prompt enable fresh
    /// (the last `/loop` wins: counters reset, limits replaced). An inline prompt is sent
    /// through the same `@` expansion as `/queue` — immediately when idle, queued behind
    /// the running turn otherwise — and tagged as iteration 1.
    pub(super) fn loop_command(&mut self, command: LoopCommand) -> UpdateResult {
        match command {
            LoopCommand::Toggle => {
                if self.session.loop_state.active {
                    self.stop_loop("loop off");
                } else {
                    self.session.loop_state = LoopState {
                        active: true,
                        ..LoopState::default()
                    };
                    self.push_notice(format!(
                        "loop on: your next message becomes the loop prompt (repeats every \
                         turn, up to {} turns or {})",
                        DEFAULT_MAX_ITERATIONS,
                        format_loop_duration(DEFAULT_MAX_DURATION)
                    ));
                }
                UpdateResult::dirty()
            }
            LoopCommand::Enable(options) => {
                if let Some(refused) = self.refuse_if_session_ended() {
                    return refused;
                }
                if !self.session.has_model {
                    self.push_notice(
                        "no model is set; choose one with /model (and /login if the provider \
                         needs a key) — the loop was not started",
                    );
                    return UpdateResult::dirty();
                }
                // Enabling is fresh: the last `/loop` wins — counters reset, limits replaced
                // (`active: true` re-arms a paused loop; the struct literal cleared it).
                self.session.loop_state = LoopState {
                    active: true,
                    prompt: options.prompt.clone(),
                    every: options.every,
                    max_iterations: options.times.unwrap_or(DEFAULT_MAX_ITERATIONS),
                    times_explicit: options.times.is_some(),
                    max_duration: options.for_duration.unwrap_or(DEFAULT_MAX_DURATION),
                    for_explicit: options.for_duration.is_some(),
                    ..LoopState::default()
                };
                if let Some(prompt) = options.prompt.clone() {
                    self.push_notice(format!(
                        "loop on: repeating every turn{}",
                        describe_limits(&options)
                    ));
                    let mut result = self.begin_loop_send(prompt);
                    result.dirty = true;
                    result
                } else {
                    self.push_notice(format!(
                        "loop on: your next message becomes the loop prompt{}",
                        describe_limits(&options)
                    ));
                    UpdateResult::dirty()
                }
            }
        }
    }

    /// Sends the loop's prompt through `@` expansion (like `/queue`): always a followup,
    /// even while a turn runs, and always captured as the loop prompt when it comes back.
    fn begin_loop_send(&mut self, text: String) -> UpdateResult {
        self.pending_expand.push_back(PendingExpand {
            kind: PendingSend::Followup,
            text: text.clone(),
            loop_capture: true,
        });
        UpdateResult::effect(Effect::ExpandMentions { text })
    }

    /// Whether a freshly expanded message becomes the loop prompt: the loop is on and it is
    /// armed (no prompt yet) or paused (the next message resumes it) — or this send is the
    /// `/loop` command's own inline prompt.
    pub(super) fn loop_captures(&self, loop_send: bool) -> bool {
        self.session.loop_state.active
            && (loop_send
                || self.session.loop_state.prompt.is_none()
                || self.session.loop_state.paused)
    }

    /// Tags an expanded loop message, records the prompt, and marks the turn it will run
    /// as loop-attributed once the runtime admits it (the `InputAdmitted` echo carries the
    /// id; `inflight` is set there, not here). Also marks the send in flight so a turn
    /// that ends before its echo arrives cannot produce a second send.
    pub(super) fn capture_loop_message(&mut self, message: &mut Message) {
        let iteration = self.session.loop_state.iterations + 1;
        if let Some(tagged) = loop_iteration_message(message, iteration) {
            *message = tagged;
        }
        self.session.loop_state.prompt = Some(message.text());
        self.session.loop_state.paused = false;
        self.session.loop_state.next_due = None;
        self.session.loop_state.awaiting_echo = true;
    }

    /// A paused loop resumes on the resent message: it is tagged as the next loop iteration
    /// (a failed or cancelled turn consumed no count) and becomes the loop prompt again
    /// (after an Esc dropped it). The admission echo marks the turn loop-attributed.
    pub(super) fn resume_loop_on_retry(&mut self, text: String) -> UpdateResult {
        let iteration = self.session.loop_state.iterations + 1;
        self.session.loop_state.prompt = Some(text.clone());
        self.session.loop_state.paused = false;
        self.session.loop_state.next_due = None;
        self.emit_loop_followup(&text, iteration)
    }

    /// Stops the loop with one notice. The mode, the prompt and every deadline go; the
    /// transcript keeps whatever ran.
    fn stop_loop(&mut self, notice: &str) {
        self.session.loop_state = LoopState::default();
        self.push_notice(notice.to_owned());
    }

    /// Pauses the loop after a user cancel: the mode stays on, the prompt is dropped (omp's
    /// shape), and the next user message becomes the new prompt and resumes.
    fn pause_loop_cancelled(&mut self) {
        self.session.loop_state.paused = true;
        self.session.loop_state.prompt = None;
        self.session.loop_state.next_due = None;
        self.push_notice(
            "loop paused: the turn was cancelled; your next message becomes the new loop \
             prompt",
        );
    }

    /// Pauses the loop after a model or hook error (ADR 0017): no retry, the prompt is kept
    /// so `/retry` resends it; a new message replaces it.
    pub(super) fn pause_loop_failed(&mut self) {
        // `TurnFailed` always precedes `TurnFinished { failed }` (ADR 0017 §3), so the
        // notice is pushed once, here; the turn-end arm only ensures the pause stands.
        if !self.session.loop_state.paused {
            self.session.loop_state.paused = true;
            self.session.loop_state.next_due = None;
            self.push_notice("loop paused: the turn failed; /retry to resend");
        }
    }

    /// The `--every` deadline the event loop waits for: only while idle with a prompt to
    /// send and nothing in flight or queued. While a turn runs the turn end decides (an
    /// `--every` tick never interrupts a turn; several due ticks merge into one send).
    pub fn loop_deadline(&self) -> Option<Instant> {
        if self.turn() != TurnState::Running
            && self.session.loop_state.active
            && !self.session.loop_state.paused
            && self.session.loop_state.prompt.is_some()
            && !self.session.loop_state.awaiting_echo
            && self.session.loop_state.outstanding.is_empty()
        {
            self.session.loop_state.next_due
        } else {
            None
        }
    }

    /// THE turn-end decision point (step 3 adds the goal input here): one place where a
    /// finished loop turn either re-sends the prompt, waits for `--every`, or stops. Runs on
    /// the journaled `TurnFinished` (the boundary `/fork` waits for), after `TurnFailed`
    /// already paused a failed turn. Returns the followup effect when the loop continues
    /// now, or nothing when it waits, pauses or stops.
    pub(super) fn loop_turn_finished(&mut self, outcome: TurnOutcome) -> UpdateResult {
        if !self.session.loop_state.active {
            return UpdateResult::none();
        }
        let was_inflight = std::mem::replace(&mut self.session.loop_state.inflight, false);
        if !was_inflight {
            // The finished turn was not the loop's (a one-off steered in, or a turn from
            // before the loop armed). It still ends a wait: with a prompt and no `--every`
            // debt, start iterating now.
            return self.maybe_start_loop();
        }
        match outcome {
            TurnOutcome::Completed => {
                self.session.loop_state.iterations += 1;
                self.maybe_continue_loop()
            }
            TurnOutcome::Cancelled => {
                // Esc or Ctrl-C on a loop turn pauses (the mode stays on, the prompt is
                // dropped); anything else resumes with its next message.
                self.pause_loop_cancelled();
                UpdateResult::dirty()
            }
            TurnOutcome::Failed => {
                // `TurnFailed` paused already; keep the pause standing without a second
                // notice, and never re-send into a broken provider.
                self.pause_loop_failed();
                UpdateResult::dirty()
            }
            _ => {
                // Refused, budget-exceeded and the rest end the turn without a usable
                // result: pausing is the safe default (no blind re-send into a limit).
                self.session.loop_state.paused = true;
                self.session.loop_state.next_due = None;
                self.push_notice(format!(
                    "loop paused: the turn ended as {}",
                    turn_outcome_word(outcome)
                ));
                UpdateResult::dirty()
            }
        }
    }

    /// Sends one loop iteration: tags it, records the in-flight send (the `InboxEnqueued`
    /// echo clears it), and produces the followup the loop runs through the facade (so it
    /// carries `source = User` like every other send).
    fn emit_loop_followup(&mut self, prompt: &str, iteration: u64) -> UpdateResult {
        self.session.loop_state.awaiting_echo = true;
        self.session.loop_state.next_due = None;
        UpdateResult::effect(Effect::SendFollowup(loop_message(prompt, iteration)))
    }

    /// A turn that was not the loop's just ended: with a prompt, no pause and nothing
    /// queued, the loop starts now (the armed prompt's first turn, or the turn after a
    /// steered one-off).
    fn maybe_start_loop(&mut self) -> UpdateResult {
        if self.session.loop_state.paused
            || self.session.loop_state.prompt.is_none()
            || !self.session.loop_state.outstanding.is_empty()
            || self.session.loop_state.awaiting_echo
            || self.turn() == TurnState::Running
        {
            return UpdateResult::none();
        }
        self.maybe_continue_loop()
    }

    /// Sends the next iteration now, unless a cap stops the loop or `--every` makes it
    /// wait. Stopping deactivates with one notice.
    fn maybe_continue_loop(&mut self) -> UpdateResult {
        if self.session.loop_state.paused
            || self.session.loop_state.prompt.is_none()
            || self.session.loop_state.awaiting_echo
            || !self.session.loop_state.outstanding.is_empty()
        {
            return UpdateResult::none();
        }
        if self.session.loop_state.iterations >= self.session.loop_state.max_iterations {
            let max = self.session.loop_state.max_iterations;
            let notice = if self.session.loop_state.times_explicit {
                format!("loop finished after {max} turns (--times {max})")
            } else {
                format!("loop finished after {max} turns (the default cap of 20 turns or 1h)")
            };
            self.stop_loop(&notice);
            return UpdateResult::dirty();
        }
        if let Some(started) = self.session.loop_state.started_at
            && self.now.saturating_duration_since(started) >= self.session.loop_state.max_duration
        {
            let max = format_loop_duration(self.session.loop_state.max_duration);
            let notice = if self.session.loop_state.for_explicit {
                format!("loop finished after {max} (--for {max})")
            } else {
                format!("loop finished after {max} (the default cap of 20 turns or 1h)")
            };
            self.stop_loop(&notice);
            return UpdateResult::dirty();
        }
        if let Some(every) = self.session.loop_state.every {
            let due = self
                .session
                .loop_state
                .last_start
                .is_none_or(|start| self.now.saturating_duration_since(start) >= every);
            if !due {
                // Not due yet: wait for `LoopDeadline` (the footer counts down). Several
                // due ticks merge — one deadline, one send.
                let start = self.session.loop_state.last_start.unwrap_or(self.now);
                self.session.loop_state.next_due = Some(start + every);
                return UpdateResult::dirty();
            }
        }
        let prompt = self.session.loop_state.prompt.clone().unwrap_or_default();
        self.emit_loop_followup(&prompt, self.session.loop_state.iterations + 1)
    }

    /// The `--every` wait is over while the driver is idle: send the next iteration. A
    /// running turn ignores the deadline (its end decides); several due ticks merge into
    /// this one send.
    pub(super) fn loop_deadline_fired(&mut self) -> UpdateResult {
        if self.turn() == TurnState::Running
            || !self.session.loop_state.active
            || self.session.loop_state.paused
            || self.session.loop_state.prompt.is_none()
            || self.session.loop_state.awaiting_echo
            || !self.session.loop_state.outstanding.is_empty()
        {
            return UpdateResult::none();
        }
        let Some(due) = self.session.loop_state.next_due else {
            return UpdateResult::none();
        };
        if self.now < due {
            return UpdateResult::none();
        }
        self.session.loop_state.next_due = None;
        self.maybe_continue_loop()
    }
}

/// Retags an expanded message as loop iteration `n`, preserving its parts. `Message::user`
/// carries no attachments here: the expanded form (with attachment parts, ADR 0018
/// revision 6) keeps them — only the `origin` is added.
fn loop_iteration_message(message: &Message, iteration: u64) -> Option<Message> {
    let origin = OpaqueJson::parse(
        serde_json::json!({ "continuation": "loop", "iteration": iteration }).to_string(),
    )
    .expect("the loop origin we build is valid JSON");
    let mut tagged = message.clone();
    match &mut tagged {
        Message::User { meta, .. }
        | Message::Assistant { meta, .. }
        | Message::Tool { meta, .. } => meta.origin = Some(origin),
    }
    Some(tagged)
}

/// The limits suffix for the enable notices: ` --times 2 · --every 5m`, or empty.
fn describe_limits(options: &LoopOptions) -> String {
    let mut parts = Vec::new();
    if let Some(every) = options.every {
        parts.push(format!("--every {}", format_loop_duration(every)));
    }
    if let Some(times) = options.times {
        parts.push(format!("--times {times}"));
    }
    if let Some(for_duration) = options.for_duration {
        parts.push(format!("--for {}", format_loop_duration(for_duration)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(" · "))
    }
}

/// The one-word outcome for the pause notice on an unusual turn end.
fn turn_outcome_word(outcome: TurnOutcome) -> &'static str {
    match outcome {
        TurnOutcome::Completed => "completed",
        TurnOutcome::Refused => "refused",
        TurnOutcome::BudgetExceeded => "budget_exceeded",
        TurnOutcome::ContextBudget => "context_budget",
        TurnOutcome::Failed => "failed",
        TurnOutcome::Poisoned => "poisoned",
        TurnOutcome::Incomplete => "incomplete",
        TurnOutcome::Cancelled => "cancelled",
        TurnOutcome::Unknown => "unknown",
        // `TurnOutcome` is non-exhaustive: a future outcome pauses rather than loops.
        _ => "unknown",
    }
}

/// Feeds the loop the runtime events it tracks: the `InboxEnqueued`/`InputAdmitted` echo
/// pair marks which turns are loop-attributed, `TurnStarted` opens the wait, and
/// `TurnFinished` is the decision. Called from `apply_runtime_event` before its own arms.
pub(super) fn track_loop_runtime_event(state: &mut LoopState, event: &RuntimeEvent, now: Instant) {
    match event {
        RuntimeEvent::InboxEnqueued {
            lane,
            message_id,
            message,
            ..
        } => {
            if loop_iteration(message).is_some() {
                // A loop send landed: it is no longer in flight. A `NextTurn` one runs
                // next (the decision skips while it is outstanding); a `NextStep` one
                // (a mid-turn capture steering) joins the running turn, which is the
                // loop's from here on.
                state.awaiting_echo = false;
                if *lane == radian_session::InboxLane::NextTurn {
                    state.outstanding.insert(message_id.clone());
                } else if state.active {
                    state.inflight = true;
                }
            }
        }
        RuntimeEvent::InputAdmitted { message_id, .. } => {
            if state.outstanding.remove(message_id) && state.active {
                // The running turn admitted a loop-tagged input: it is the loop's.
                state.inflight = true;
                state.last_start = Some(now);
                if state.started_at.is_none() {
                    state.started_at = Some(now);
                }
            }
        }
        RuntimeEvent::TurnStarted { .. } => {
            // A new turn ends the wait: a stale `--every` deadline from before it must not
            // fire into the running turn.
            state.next_due = None;
        }
        _ => {}
    }
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

    /// Submits a line, running its `@` expansion like the loop, and returns every effect.
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
            pending_wake: false,
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

    fn failed(app: &mut App, turn: &str, message: &str) {
        app.update(Input::Runtime(RuntimeEvent::TurnFailed {
            turn_id: turn.to_owned(),
            message: message.to_owned(),
        }));
    }

    /// One full loop iteration in production order: the echo, the start, the admission,
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

    /// A loop-tagged message round-trips its iteration through `origin`; ordinary
    /// messages carry none.
    #[test]
    fn loop_messages_carry_the_iteration_in_origin() {
        assert_eq!(loop_iteration(&loop_message("go", 3)), Some(3));
        assert_eq!(loop_iteration(&Message::user("go")), None);
        assert_eq!(format_loop_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_loop_duration(Duration::from_secs(300)), "5m");
        assert_eq!(format_loop_duration(Duration::from_secs(7200)), "2h");
    }

    /// A bare `/loop` toggles: on (armed, the next message becomes the prompt), then off.
    #[test]
    fn a_bare_loop_toggles_the_mode() {
        let mut app = App::new("m", "s");
        assert!(submit(&mut app, "/loop").is_empty());
        assert!(app.session.loop_state.active);
        assert!(app.session.loop_state.prompt.is_none());
        assert!(
            notices(&app)
                .iter()
                .any(|n| n.contains("loop on: your next message")),
            "{:?}",
            notices(&app)
        );
        assert!(submit(&mut app, "/loop").is_empty());
        assert!(!app.session.loop_state.active);
        assert!(
            notices(&app).iter().any(|n| n == "loop off"),
            "{:?}",
            notices(&app)
        );
    }

    /// `/loop --times 2 go` sends the prompt as iteration 1, then re-sends it once and
    /// stops with a notice: exactly two turns.
    #[test]
    fn times_two_runs_exactly_two_turns() {
        let mut app = App::new("m", "s");
        let base = app.session.spinner_origin;
        app.set_now(base);
        let effects = submit(&mut app, "/loop --times 2 go");
        assert_eq!(
            effects,
            vec![
                Effect::ExpandMentions { text: "go".into() },
                Effect::SendFollowup(loop_message("go", 1)),
            ]
        );
        assert_eq!(app.session.loop_state.prompt.as_deref(), Some("go"));

        let result = run_iteration(
            &mut app,
            "e-1",
            "t-1",
            loop_message("go", 1),
            TurnOutcome::Completed,
        );
        assert_eq!(
            result.effects,
            vec![Effect::SendFollowup(loop_message("go", 2))],
            "the finished turn re-sends the prompt"
        );
        assert_eq!(app.session.loop_state.iterations, 1);

        let result = run_iteration(
            &mut app,
            "e-2",
            "t-2",
            loop_message("go", 2),
            TurnOutcome::Completed,
        );
        assert!(result.effects.is_empty(), "--times 2 stops after two turns");
        assert!(!app.session.loop_state.active);
        assert!(
            notices(&app)
                .iter()
                .any(|n| n == "loop finished after 2 turns (--times 2)"),
            "{:?}",
            notices(&app)
        );
    }

    /// `/loop go` queued behind a running turn does not double-send: the turn's end sees
    /// the queued followup outstanding and stays quiet; the queued turn then iterates.
    #[test]
    fn a_prompt_queued_behind_a_running_turn_runs_once() {
        let mut app = App::new("m", "s");
        app.session.projection.set_turn(TurnState::Running);
        let effects = submit(&mut app, "/loop go");
        assert_eq!(
            effects,
            vec![
                Effect::ExpandMentions { text: "go".into() },
                Effect::SendFollowup(loop_message("go", 1)),
            ]
        );
        // The running turn ends before the echo arrives: the in-flight send still
        // suppresses a second one.
        let result = finished(&mut app, "t-1", TurnOutcome::Completed);
        assert!(result.effects.is_empty(), "one queued send, not two");
        // The echo lands late; the queued turn then counts as iteration 1.
        enqueued(&mut app, "e-1", loop_message("go", 1));
        app.session.projection.set_turn(TurnState::Idle);
        started(&mut app, "t-2");
        admitted(&mut app, "e-1");
        let result = finished(&mut app, "t-2", TurnOutcome::Completed);
        assert_eq!(
            result.effects,
            vec![Effect::SendFollowup(loop_message("go", 2))]
        );
        assert_eq!(app.session.loop_state.iterations, 1);
    }

    /// With `--every 5m` a turn that ends after one minute waits; the deadline fires at
    /// five minutes. A turn that runs past the interval sends at its end.
    #[test]
    fn every_waits_for_the_interval_and_a_long_turn_sends_at_its_end() {
        let mut app = App::new("m", "s");
        let base = app.session.spinner_origin;
        app.set_now(base);
        submit(&mut app, "/loop --every 5m go");
        enqueued(&mut app, "e-1", loop_message("go", 1));
        started(&mut app, "t-1");
        admitted(&mut app, "e-1");

        // One minute in: not due — no send, one deadline armed.
        app.set_now(base + Duration::from_secs(60));
        let result = finished(&mut app, "t-1", TurnOutcome::Completed);
        assert!(result.effects.is_empty(), "the interval is not over");
        assert_eq!(
            app.loop_deadline(),
            Some(base + Duration::from_secs(300)),
            "due five minutes after the iteration started"
        );
        // The deadline early is a no-op (several due ticks merge into one send).
        assert!(app.update(Input::LoopDeadline).effects.is_empty());
        // At five minutes the deadline sends iteration 2.
        app.set_now(base + Duration::from_secs(300));
        assert_eq!(
            app.update(Input::LoopDeadline).effects,
            vec![Effect::SendFollowup(loop_message("go", 2))]
        );

        // A turn that runs seven minutes is already due at its end: send at once.
        let mut app = App::new("m", "s");
        let base = app.session.spinner_origin;
        app.set_now(base);
        submit(&mut app, "/loop --every 5m go");
        enqueued(&mut app, "e-1", loop_message("go", 1));
        started(&mut app, "t-1");
        admitted(&mut app, "e-1");
        app.set_now(base + Duration::from_secs(7 * 60));
        let result = finished(&mut app, "t-1", TurnOutcome::Completed);
        assert_eq!(
            result.effects,
            vec![Effect::SendFollowup(loop_message("go", 2))],
            "a long turn sends at its end, never interrupting it"
        );
    }

    /// A cancelled loop turn pauses (the mode stays on, the prompt is dropped); the next
    /// message becomes the new prompt and resumes.
    #[test]
    fn a_cancelled_turn_pauses_and_the_next_message_resumes() {
        let mut app = App::new("m", "s");
        submit(&mut app, "/loop go");
        enqueued(&mut app, "e-1", loop_message("go", 1));
        started(&mut app, "t-1");
        admitted(&mut app, "e-1");
        let result = finished(&mut app, "t-1", TurnOutcome::Cancelled);
        assert!(result.effects.is_empty(), "a cancelled turn never re-sends");
        assert!(app.session.loop_state.active);
        assert!(app.session.loop_state.paused);
        assert_eq!(app.session.loop_state.prompt, None);
        assert!(
            notices(&app)
                .iter()
                .any(|n| n.contains("loop paused: the turn was cancelled")),
            "{:?}",
            notices(&app)
        );

        // The next message becomes the new prompt (iteration counting continues).
        let effects = submit(&mut app, "try this instead");
        assert_eq!(
            effects,
            vec![
                Effect::ExpandMentions {
                    text: "try this instead".into()
                },
                Effect::SendFollowup(loop_message("try this instead", 1)),
            ]
        );
        assert!(!app.session.loop_state.paused);
        assert_eq!(
            app.session.loop_state.prompt.as_deref(),
            Some("try this instead")
        );
    }

    /// A failed turn pauses without re-sending; `/retry` resumes on the same prompt.
    #[test]
    fn a_failed_turn_pauses_and_retry_resumes() {
        let mut app = App::new("m", "s");
        app.session.entries.push(Entry::new(EntryKind::User, "go"));
        submit(&mut app, "/loop go");
        enqueued(&mut app, "e-1", loop_message("go", 1));
        started(&mut app, "t-1");
        admitted(&mut app, "e-1");
        failed(&mut app, "t-1", "boom");
        assert!(app.session.loop_state.paused);
        assert_eq!(app.session.loop_state.prompt.as_deref(), Some("go"));
        let result = finished(&mut app, "t-1", TurnOutcome::Failed);
        assert!(
            result.effects.is_empty(),
            "never re-send into a broken provider"
        );
        assert!(
            notices(&app)
                .iter()
                .any(|n| n.contains("loop paused: the turn failed")),
            "{:?}",
            notices(&app)
        );

        app.input = "/retry".into();
        let effects = app.on_key(key(KeyCode::Enter)).effects;
        assert_eq!(effects, vec![Effect::SendFollowup(loop_message("go", 1))]);
        assert!(!app.session.loop_state.paused, "the retry resumes the loop");
    }

    /// A turn that ends without a usable result (budget, refusal, …) pauses rather than
    /// looping into the same wall.
    #[test]
    fn an_unusable_turn_end_pauses() {
        let mut app = App::new("m", "s");
        submit(&mut app, "/loop go");
        enqueued(&mut app, "e-1", loop_message("go", 1));
        started(&mut app, "t-1");
        admitted(&mut app, "e-1");
        let result = finished(&mut app, "t-1", TurnOutcome::BudgetExceeded);
        assert!(result.effects.is_empty());
        assert!(app.session.loop_state.paused);
        assert!(
            notices(&app)
                .iter()
                .any(|n| n.contains("loop paused: the turn ended as budget_exceeded")),
            "{:?}",
            notices(&app)
        );
    }
}
