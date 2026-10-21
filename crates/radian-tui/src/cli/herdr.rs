//! Reporting the session's state, identity and display metadata to herdr (ADR 0034 and its
//! revision 1): [`HerdrReporter`] and its sender thread, the monotonic `seq`, the session
//! identity and the resume command, and the diffs that queue a report only when something
//! changed.

use std::collections::BTreeMap;

use super::*;

/// The TUI's herdr reporter (ADR 0034): the sender thread, the monotonic `seq`, the session
/// identity, the resume command, the last reported state, and the display metadata herdr was last
/// told. `maybe_report` recomputes the state and the metadata and queues a report only when each
/// changed; the UI thread never waits on I/O.
pub(super) struct HerdrReporter {
    reporter: radian_host::herdr::Reporter,
    seq: radian_host::herdr::Seq,
    session: radian_host::herdr::SessionIdentity,
    resume: Option<Vec<String>>,
    last: Option<(radian_host::herdr::State, Option<String>)>,
    /// The token values herdr holds, as this process put them there: the diff base for
    /// "only the changed keys" (revision 1 decision 1).
    last_tokens: BTreeMap<String, String>,
    /// The last `blocked` label sent (revision 1 decision 2).
    last_label: Option<String>,
    /// The last `working` label sent (the jobs item): `Some("jobs")` while background jobs
    /// keep an idle session working. Leaving that state clears herdr's labels on the wire, so
    /// re-entering it reports again.
    last_working: Option<String>,
    /// `display_agent` is constant, so it rides the first metadata report only.
    display_agent_sent: bool,
}

impl HerdrReporter {
    pub(super) fn maybe_report(&mut self, app: &App) {
        let (turn, pending, trust, idle_block, jobs) = app.herdr_state_inputs();
        let state = radian_host::herdr::state(turn, pending, trust, idle_block, jobs);
        let message = (state == radian_host::herdr::State::Blocked)
            .then(|| radian_host::herdr::blocked_message(app.herdr_blocked().as_ref(), trust));
        if self.last.as_ref() != Some(&(state, message.clone())) {
            self.last = Some((state, message.clone()));
            let mut report = radian_host::herdr::Report::new(state);
            if let Some(message) = message {
                report = report.message(message);
            }
            report = report.session(self.session.id.clone(), self.session.path.clone());
            if let Some(argv) = &self.resume {
                report = report.resume(Some(argv.clone()));
            }
            self.reporter.report(&report, self.seq.next());
        }
        self.report_metadata(app, trust, state, turn, jobs);
    }

    /// Queues a `pane.report_metadata` when a token or a state label changed (revision 1
    /// decisions 1 and 2, plus the jobs item's `working: jobs`). An empty diff queues nothing,
    /// so a live session sends nothing at all.
    fn report_metadata(
        &mut self,
        app: &App,
        trust: bool,
        state: radian_host::herdr::State,
        turn: radian_host::TurnState,
        jobs: usize,
    ) {
        let changed = radian_host::herdr::token_diff(&self.last_tokens, &metadata_tokens(app));
        for (name, value) in &changed {
            match value {
                Some(value) => {
                    self.last_tokens.insert(name.clone(), value.clone());
                }
                None => {
                    self.last_tokens.remove(name);
                }
            }
        }
        let label = radian_host::herdr::blocked_label(app.herdr_blocked().as_ref(), trust);
        let mut state_labels = BTreeMap::new();
        if let Some(label) = &label
            && self.last_label.as_deref() != Some(label.as_str())
        {
            self.last_label = Some(label.clone());
            state_labels.insert("blocked".to_owned(), label.clone());
        }
        let working = radian_host::herdr::working_label(state, turn, jobs);
        if let Some(label) = &working
            && self.last_working.as_deref() != Some(label.as_str())
        {
            self.last_working = Some(label.clone());
            state_labels.insert("working".to_owned(), label.clone());
        }
        // Herdr keeps labels until cleared: leaving idle-with-jobs (a turn starts, or the
        // last job settles) clears them, so a later turn is not shown as `working · jobs`.
        let clearing = working.is_none() && self.last_working.take().is_some();
        let metadata = radian_host::herdr::Metadata {
            tokens: changed,
            state_labels,
            display_agent: (!self.display_agent_sent).then(|| radian_host::herdr::AGENT.to_owned()),
            clear_state_labels: clearing,
        };
        if metadata.is_empty() {
            return;
        }
        self.display_agent_sent = true;
        self.reporter.metadata(&metadata, self.seq.next());
    }

    pub(super) fn release(&self) {
        self.reporter.release(self.seq.next());
    }

    /// Reports the session identity as its own `pane.report_agent_session` (revision 1 decision
    /// 6): herdr marks the pane as an active named agent by it, which is what `herdr agent prompt`
    /// needs. The startup order matters - the state report must already hold the pane, or herdr
    /// refuses the `resume_argv` - so the caller runs this after the first `maybe_report`.
    pub(super) fn report_session(&self) {
        self.reporter
            .session(&self.session, self.resume.as_deref(), self.seq.next());
    }
}

/// The metadata tokens radian reports (revision 1 decision 1): `model` (the id, the short name),
/// `context` (the used percentage, rounded to the 1% notch), `tier`, and `pending` (how many
/// decisions wait). `None` is "unknown right now": the key is omitted, or cleared if herdr was
/// told a value before. No `tasks` key (ADR 0043 removed in-process tasks), no cost and no token
/// totals (H7).
fn metadata_tokens(app: &App) -> BTreeMap<String, Option<String>> {
    let context = crate::footer::context_fraction(&app.session.projection.read())
        .map(|fraction| format!("{:.0}%", fraction * 100.0));
    // The status bar's tier string carries where the choice came from (`workspace (project)`,
    // ADR 0038 decision 3); the token wants the tier name only (decision 1's
    // `workspace | full`), so the source marker is dropped here.
    let tier = app
        .session
        .projection
        .read()
        .tier()
        .map(|tier| tier.split(' ').next().unwrap_or(tier).to_owned());
    let mut tokens = BTreeMap::new();
    tokens.insert("model".to_owned(), Some(app.session.model_name.clone()));
    tokens.insert("context".to_owned(), context);
    tokens.insert("tier".to_owned(), tier);
    tokens.insert("pending".to_owned(), Some(app.pending.to_string()));
    tokens
}

/// Builds the reporter when the environment says we are in a herdr pane (ADR 0034 H1). The
/// returned `Option<String>` is the reason the resume command was left out, for the startup
/// line.
pub(super) fn herdr_reporter(
    config: &radian_host::config::Config,
    session_id: &str,
    journal: &Path,
    extra: &[String],
) -> Option<(HerdrReporter, Option<String>)> {
    let env = radian_host::herdr::detect(config.integrations.herdr)?;
    let (resume, note) = match radian_host::herdr::resume_argv(session_id, extra) {
        Ok(argv) => (Some(argv), None),
        Err(reason) => (None, Some(reason)),
    };
    Some((
        HerdrReporter {
            reporter: radian_host::herdr::Reporter::spawn(env),
            seq: radian_host::herdr::Seq::new(),
            session: radian_host::herdr::SessionIdentity {
                id: session_id.to_owned(),
                path: journal.to_path_buf(),
            },
            resume,
            last: None,
            last_tokens: BTreeMap::new(),
            last_label: None,
            last_working: None,
            display_agent_sent: false,
        },
        note,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The diff base: what herdr holds, updated the way `report_metadata` updates it.
    fn step(app: &App, last: &mut BTreeMap<String, String>) -> BTreeMap<String, Option<String>> {
        let diff = radian_host::herdr::token_diff(last, &metadata_tokens(app));
        for (name, value) in &diff {
            match value {
                Some(value) => {
                    last.insert(name.clone(), value.clone());
                }
                None => {
                    last.remove(name);
                }
            }
        }
        diff
    }

    /// Every trigger the ADR's acceptance names reports once, and the report carries only the key
    /// that changed: the model, the tier, the pending count and the context percentage's 1% notch.
    /// There is no `tasks` key (ADR 0043 removed in-process tasks) and no cost or token totals.
    #[test]
    fn each_metadata_trigger_reports_only_the_changed_key() {
        let mut app = App::new("mock", "sid");
        let mut last: BTreeMap<String, String> = BTreeMap::new();
        let first = step(&app, &mut last);
        assert_eq!(first["model"], Some("mock".to_owned()));
        assert_eq!(first["pending"], Some("0".to_owned()));
        assert!(
            !first.contains_key("context") && !first.contains_key("tier"),
            "unknown keys are omitted: {first:?}"
        );
        assert!(
            !first.contains_key("tasks") && !first.contains_key("cost"),
            "no tasks (ADR 0043), no cost: {first:?}"
        );
        assert!(step(&app, &mut last).is_empty(), "nothing moved");

        app.set_tier("workspace (project)");
        let tier = step(&app, &mut last);
        assert_eq!(tier.len(), 1, "{tier:?}");
        assert_eq!(
            tier["tier"],
            Some("workspace".to_owned()),
            "the status bar's source marker is dropped"
        );

        app.session.model_name = "other".to_owned();
        let model = step(&app, &mut last);
        assert_eq!(model.len(), 1, "{model:?}");
        assert_eq!(model["model"], Some("other".to_owned()));

        app.pending = 2;
        let pending = step(&app, &mut last);
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert_eq!(pending["pending"], Some("2".to_owned()));

        app.set_context_window(Some(1000));
        app.session
            .projection
            .set_request_started(Some(budget(421)));
        let context = step(&app, &mut last);
        assert_eq!(context.len(), 1, "{context:?}");
        assert_eq!(context["context"], Some("42%".to_owned()));
        // Under the 1% notch nothing moves.
        app.session
            .projection
            .set_request_started(Some(budget(421)));
        assert!(step(&app, &mut last).is_empty(), "same notch");
        // Crossing it does.
        app.session
            .projection
            .set_request_started(Some(budget(431)));
        let context = step(&app, &mut last);
        assert_eq!(context.len(), 1, "{context:?}");
        assert_eq!(context["context"], Some("43%".to_owned()));
    }

    /// A tier that goes unknown again is cleared, not silently kept.
    #[test]
    fn a_key_that_goes_unknown_is_cleared() {
        let mut app = App::new("mock", "sid");
        let mut last: BTreeMap<String, String> = BTreeMap::new();
        app.set_context_window(Some(1000));
        app.session
            .projection
            .set_request_started(Some(budget(421)));
        step(&app, &mut last);
        assert!(last.contains_key("context"));
        app.session.projection.set_context_window(None);
        let cleared = step(&app, &mut last);
        assert_eq!(cleared.len(), 1, "{cleared:?}");
        assert_eq!(cleared["context"], None);
        assert!(!last.contains_key("context"));
    }

    /// A `BudgetReport` with `projected_tokens` out of a 1000-token budget.
    fn budget(projected_tokens: u64) -> agent_core::BudgetReport {
        agent_core::BudgetReport {
            source: agent_core::UsageSource::Estimated,
            projected_tokens,
            input_budget_tokens: 1000,
            request_bytes: 0,
            baseline_input_tokens: None,
            estimated_growth_tokens: None,
        }
    }
}
