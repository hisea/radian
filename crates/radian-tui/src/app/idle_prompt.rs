//! The `idle_prompt` timer (ADR 0045 §6): a TUI turn ended, no input arrived for
//! [`IDLE_PROMPT_DELAY`], and the session's `Notification` hooks run once.
//!
//! The timer is pure app state on the loop's injected clock (`App::now`), like the `/loop`
//! and goal deadlines: `update` only asks whether the deadline passed, and the effect runs
//! the hooks in the loop. Input restarts the timer; a running turn, `/loop`, or an
//! active/paused goal suppresses it (the session is not waiting for a human).

use super::*;

impl App {
    /// The instant the `idle_prompt` fires, or `None` while it is not armed: no turn has
    /// ended yet, a turn is running, the observer already fired for this stretch, `/loop` is
    /// on, or a goal owns the next turn.
    pub fn idle_deadline(&self) -> Option<Instant> {
        if self.idle_notified
            || self.turn() == TurnState::Running
            || self.loop_active()
            || self.session.goal_mode.active_or_paused()
        {
            return None;
        }
        self.idle_since.map(|since| since + IDLE_PROMPT_DELAY)
    }

    /// The loop's deadline input: fire once when the idle period is over. A re-check keeps a
    /// stale wake from firing into a running turn or a loop-owned end.
    pub(super) fn idle_deadline_fired(&mut self) -> UpdateResult {
        let Some(deadline) = self.idle_deadline() else {
            return UpdateResult::none();
        };
        if self.now < deadline {
            return UpdateResult::none();
        }
        self.idle_notified = true;
        UpdateResult::effect(Effect::NotifyIdle)
    }

    /// A turn ended (any outcome): start the idle clock. The deadline itself stays unarmed
    /// while `/loop` or a goal will open the next turn (see [`App::idle_deadline`]).
    pub(super) fn idle_turn_finished(&mut self) {
        self.idle_since = Some(self.now);
        self.idle_notified = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radian_session::{RuntimeEvent, TurnOutcome};

    fn finished(app: &mut App, outcome: TurnOutcome) -> UpdateResult {
        app.update(Input::Runtime(RuntimeEvent::TurnFinished {
            turn_id: "t-1".to_owned(),
            outcome,
            reason: None,
            stop_reason: None,
        }))
    }

    fn notified(result: &UpdateResult) -> bool {
        result
            .effects
            .iter()
            .any(|effect| matches!(effect, Effect::NotifyIdle))
    }

    /// Edge case: after a turn ends, 60 s without input fires `idle_prompt` once; 60 s more
    /// fires nothing; an input in between restarts the timer. Uses the injected clock, so
    /// the test never sleeps.
    #[test]
    fn idle_prompt_fires_once_and_input_restarts_the_timer() {
        let mut app = App::new("m", "s");
        let base = app.session.spinner_origin;
        app.set_now(base);
        finished(&mut app, TurnOutcome::Completed);
        assert_eq!(app.idle_deadline(), Some(base + IDLE_PROMPT_DELAY));

        // Not yet due: no effect.
        app.set_now(base + Duration::from_secs(59));
        assert!(!notified(&app.update(Input::IdleDeadline)));

        // At 60 s: exactly one `idle_prompt`.
        app.set_now(base + IDLE_PROMPT_DELAY);
        let result = app.update(Input::IdleDeadline);
        assert!(notified(&result), "{:?}", result.effects);
        assert_eq!(app.idle_deadline(), None, "never fires twice");

        // 60 s more: nothing.
        app.set_now(base + IDLE_PROMPT_DELAY + Duration::from_secs(60));
        assert!(!notified(&app.update(Input::IdleDeadline)));

        // Input restarts the timer: a fresh 60 s from the key press.
        app.set_now(base + Duration::from_secs(200));
        app.update(Input::Char('x'));
        assert!(!app.idle_notified);
        assert_eq!(
            app.idle_deadline(),
            Some(base + Duration::from_secs(200) + IDLE_PROMPT_DELAY)
        );
        app.set_now(base + Duration::from_secs(259));
        assert!(!notified(&app.update(Input::IdleDeadline)));
        app.set_now(base + Duration::from_secs(260));
        assert!(notified(&app.update(Input::IdleDeadline)));
    }

    /// `/loop` owns the next turn, so a turn end never arms the idle timer; turning the loop
    /// off does not arm a stale one (the next turn end or input starts a fresh clock).
    #[test]
    fn loop_active_suppresses_idle_prompt() {
        let mut app = App::new("m", "s");
        let base = app.session.spinner_origin;
        app.set_now(base);
        app.session.loop_state.active = true;
        finished(&mut app, TurnOutcome::Completed);
        assert_eq!(app.idle_deadline(), None);
        app.set_now(base + IDLE_PROMPT_DELAY);
        assert!(!notified(&app.update(Input::IdleDeadline)));

        // A goal that is active or paused also owns the next turn (covered by
        // `goal_mode::active_or_paused`); once the loop is off the ordinary clock is back.
        app.session.loop_state.active = false;
        assert_eq!(
            app.idle_deadline(),
            Some(base + IDLE_PROMPT_DELAY),
            "with no scheduler left, the turn end waits for the human"
        );
    }

    /// A running turn suppresses the timer even with a stale `idle_since`, and a user
    /// keystroke (not just a character) restarts it.
    #[test]
    fn a_running_turn_suppresses_the_timer_and_a_key_restarts_it() {
        let mut app = App::new("m", "s");
        let base = app.session.spinner_origin;
        app.set_now(base);
        finished(&mut app, TurnOutcome::Completed);
        app.session
            .projection
            .set_turn(crate::app::TurnState::Running);
        assert_eq!(app.idle_deadline(), None);
        app.session.projection.set_turn(crate::app::TurnState::Idle);

        app.set_now(base + Duration::from_secs(30));
        app.update(Input::Char('x'));
        assert_eq!(
            app.idle_deadline(),
            Some(base + Duration::from_secs(30) + IDLE_PROMPT_DELAY)
        );
    }

    /// A wake with no turn ended (the app just started) is a no-op.
    #[test]
    fn an_idle_deadline_with_no_turn_end_does_nothing() {
        let mut app = App::new("m", "s");
        app.set_now(app.session.spinner_origin + IDLE_PROMPT_DELAY);
        assert!(!notified(&app.update(Input::IdleDeadline)));
    }

    /// The finish path is the Runtime `TurnFinished`; a `Message` import keeps the event
    /// shape honest if the enum changes.
    #[test]
    fn every_outcome_arms_the_clock() {
        for outcome in [
            TurnOutcome::Completed,
            TurnOutcome::Failed,
            TurnOutcome::Cancelled,
        ] {
            let mut app = App::new("m", "s");
            let base = app.session.spinner_origin;
            app.set_now(base);
            finished(&mut app, outcome);
            assert_eq!(
                app.idle_deadline(),
                Some(base + IDLE_PROMPT_DELAY),
                "{outcome:?}"
            );
        }
    }
}
