//! Reporting the session's state to herdr (ADR 0034): [`HerdrReporter`] and its sender thread,
//! the monotonic `seq`, the session identity and the resume command, and the state diff that
//! queues a report only when something changed.

use super::*;

/// The TUI's herdr reporter (ADR 0034): the sender thread, the monotonic `seq`, the session
/// identity, the resume command and the last reported state. `maybe_report` recomputes the
/// state and queues a report only when it changed; the UI thread never waits on I/O.
pub(super) struct HerdrReporter {
    reporter: radian_host::herdr::Reporter,
    seq: radian_host::herdr::Seq,
    session: radian_host::herdr::SessionIdentity,
    resume: Option<Vec<String>>,
    last: Option<(radian_host::herdr::State, Option<String>)>,
}

impl HerdrReporter {
    pub(super) fn maybe_report(&mut self, app: &App) {
        let (turn, pending, trust) = app.herdr_state_inputs();
        let state = radian_host::herdr::state(turn, pending, trust);
        let message = (state == radian_host::herdr::State::Blocked)
            .then(|| radian_host::herdr::blocked_message(app.herdr_blocked().as_ref(), trust));
        if self.last.as_ref() == Some(&(state, message.clone())) {
            return;
        }
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

    pub(super) fn release(&self) {
        self.reporter.release(self.seq.next());
    }
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
        },
        note,
    ))
}
