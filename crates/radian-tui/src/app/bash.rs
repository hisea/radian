//! The `!`/`!!` shell commands (ADR 0033): running one at a time, queueing it behind a
//! running turn, and the `RunBash` audit records the loop appends.

use super::*;

impl App {
    /// A `!`/`!!` command (ADR 0033): one at a time, and while a turn is running it waits for
    /// the turn to end rather than racing it.
    pub(super) fn begin_bash(
        &mut self,
        command: String,
        exclude_from_context: bool,
    ) -> UpdateResult {
        if !self.session.bash_available {
            self.push_notice("this session has no shell; `!` needs a shell-capable tier");
            return UpdateResult::dirty();
        }
        if self.session.bash_running.is_some() {
            self.push_notice("a shell command is already running; Esc cancels it first");
            return UpdateResult::dirty();
        }
        if self.turn() == TurnState::Running {
            self.session.queued_bash = Some((command, exclude_from_context));
            self.push_notice("a turn is running; the command runs when it ends");
            return UpdateResult::dirty();
        }
        self.session.bash_running = Some(command.clone());
        UpdateResult::effect(Effect::RunBash {
            command,
            exclude_from_context,
        })
    }

    /// A `!`/`!!` command finished: show its block, and for `!` enqueue the `Ran \`cmd\``
    /// message for the next turn (never starting one, ADR 0011). The audit record is emitted
    /// for both `!` and `!!`.
    pub(super) fn bash_done(
        &mut self,
        command: String,
        exclude_from_context: bool,
        result: Result<radian_coding::UserBash, String>,
    ) -> UpdateResult {
        self.session.bash_running = None;
        let mut effects = Vec::new();
        let (exit_code, cancelled) = match &result {
            Ok(run) => (run.exit_code, run.cancelled),
            Err(_) => (None, false),
        };
        match result {
            Ok(run) => {
                self.session
                    .entries
                    .push(bash_entry(&command, exclude_from_context, &run));
                if !exclude_from_context {
                    let text = bash_context_text(&command, &run);
                    // Shown live; the same message coming back through the transcript is
                    // skipped so the block is not drawn twice.
                    self.session.bash_context_pending = Some(text.clone());
                    effects.push(Effect::Inject(Message::user(text)));
                }
            }
            Err(error) => self.session.entries.push(Entry::new(
                EntryKind::Notice,
                format!("!{command}: {error}"),
            )),
        }
        effects.push(Effect::AuditUserBash {
            command,
            exclude_from_context,
            exit_code,
            cancelled,
        });
        UpdateResult {
            dirty: true,
            effects,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_run(exit_code: Option<i32>, stdout: &str) -> radian_coding::UserBash {
        radian_coding::UserBash {
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_code,
            signal: None,
            timed_out: false,
            cancelled: false,
        }
    }

    /// ADR 0033: a `!` line parses to a `RunBash` effect, and the command is remembered so a
    /// second one is refused while it runs.
    #[test]
    fn a_bang_routes_to_a_run_bash_effect() {
        let mut app = App::new("m", "s");
        let result = app.run_slash_command("!echo hi");
        assert_eq!(
            result.effects,
            vec![Effect::RunBash {
                command: "echo hi".to_owned(),
                exclude_from_context: false,
            }]
        );
        assert_eq!(app.session.bash_running.as_deref(), Some("echo hi"));
        // A second one is refused (one at a time).
        let second = app.run_slash_command("!echo again");
        assert!(second.effects.is_empty());
        assert!(
            app.session
                .entries
                .last()
                .unwrap()
                .text
                .contains("already running")
        );
    }

    /// Without a shell, `!` never runs.
    #[test]
    fn a_bang_without_a_shell_does_not_run() {
        let mut app = App::new("m", "s");
        app.set_bash_available(false);
        let result = app.run_slash_command("!echo hi");
        assert!(result.effects.is_empty());
        assert!(app.session.bash_running.is_none());
        assert!(
            app.session
                .entries
                .last()
                .unwrap()
                .text
                .contains("no shell"),
            "{}",
            app.session.entries.last().unwrap().text
        );
    }

    /// ADR 0033: while a turn runs, `!` waits for it to end rather than racing it.
    #[test]
    fn a_bang_queues_behind_a_running_turn() {
        let mut app = App::new("m", "s");
        app.session.projection.set_turn(TurnState::Running);
        let result = app.run_slash_command("!echo hi");
        assert!(result.effects.is_empty());
        assert_eq!(
            app.session.queued_bash,
            Some(("echo hi".to_owned(), false)),
            "queued, not run"
        );
        // The event sink writes the Idle fact before the app sees the event; the test does
        // the same.
        app.session.projection.set_turn(TurnState::Idle);
        let effects = app.apply_event(&AgentEvent::RunFinished {
            status: radian_core::RunStatus::Completed,
        });
        assert!(
            matches!(effects.as_slice(), [Effect::RunBash { command, .. }] if command == "echo hi"),
            "{effects:?}"
        );
        assert_eq!(app.session.bash_running.as_deref(), Some("echo hi"));
    }

    /// ADR 0016: `/access` with no argument says what the startup line
    /// says when the sandbox is one radian inherited from an outer sandbox, so `workspace` does
    /// not read as radian's own rules. Mutation: drop the note (this test's second line).
    #[test]
    fn access_repeats_an_inherited_sandbox_line() {
        let mut app = App::new("m", "s");
        app.set_sandbox_note(Some(
            "workspace · sandbox inherited (an outer sandbox confines this process; radian's own \
             rules are not applied)"
                .to_owned(),
        ));
        app.run_slash_command("/access");
        let lines: Vec<&str> = app
            .session
            .entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect();
        assert!(
            lines.iter().any(|line| line.contains("access: ")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("sandbox inherited")
                && line.contains("radian's own rules are not applied")),
            "the inherited sandbox line is missing: {lines:?}"
        );

        // A session whose sandbox is not inherited adds nothing.
        let mut app = App::new("m", "s");
        app.run_slash_command("/access");
        assert!(
            !app.session
                .entries
                .iter()
                .any(|entry| entry.text.contains("sandbox inherited")),
            "only an inherited sandbox gets the note"
        );
    }

    /// `!` shows a block, injects the `Ran \`cmd\`` message and audits; `!!` only shows and
    /// audits (ADR 0033).
    #[test]
    fn a_bang_done_injects_and_audits_and_a_bangbang_only_audits() {
        let mut app = App::new("m", "s");
        app.run_slash_command("!echo hi");
        let result = app.update(Input::BashDone {
            command: "echo hi".to_owned(),
            exclude_from_context: false,
            result: Ok(a_run(Some(0), "hi\n")),
        });
        assert!(
            matches!(result.effects[0], Effect::Inject(_)),
            "{:?}",
            result.effects
        );
        assert!(matches!(
            result.effects[1],
            Effect::AuditUserBash {
                exclude_from_context: false,
                exit_code: Some(0),
                ..
            }
        ));
        let entry = app.session.entries.last().unwrap();
        assert!(matches!(
            entry.kind,
            EntryKind::Bash {
                exclude_from_context: false
            }
        ));
        // The marker column carries the `$ `; the text is the command and output.
        assert!(entry.text.starts_with("echo hi"), "{:?}", entry.text);
        assert!(entry.text.contains("hi"), "{:?}", entry.text);

        let mut app = App::new("m", "s");
        app.run_slash_command("!!echo hi");
        let result = app.update(Input::BashDone {
            command: "echo hi".to_owned(),
            exclude_from_context: true,
            result: Ok(a_run(Some(0), "hi\n")),
        });
        assert!(
            !result
                .effects
                .iter()
                .any(|effect| matches!(effect, Effect::Inject(_))),
            "!! must not enter the context: {:?}",
            result.effects
        );
        assert!(matches!(
            app.session.entries.last().unwrap().kind,
            EntryKind::Bash {
                exclude_from_context: true
            }
        ));
    }

    /// The `!` message is shown live as a block; when it comes back through the transcript it
    /// is not drawn a second time (ADR 0033).
    #[test]
    fn the_injected_bang_message_is_not_drawn_twice() {
        let mut app = App::new("m", "s");
        app.run_slash_command("!echo hi");
        app.update(Input::BashDone {
            command: "echo hi".to_owned(),
            exclude_from_context: false,
            result: Ok(a_run(Some(0), "hi\n")),
        });
        let before = app.session.entries.len();
        let text = bash_context_text("echo hi", &a_run(Some(0), "hi\n"));
        app.push_message(&Message::user(text));
        assert_eq!(app.session.entries.len(), before, "no duplicate block");
    }

    /// A resumed `!` context message (no live block) renders as a Bash block, not user prose.
    #[test]
    fn a_resumed_bang_context_message_renders_as_a_block() {
        let mut app = App::new("m", "s");
        let text = bash_context_text("echo hi", &a_run(Some(0), "hi\n"));
        app.push_message(&Message::user(text));
        assert!(matches!(
            app.session.entries.last().unwrap().kind,
            EntryKind::Bash {
                exclude_from_context: false
            }
        ));
    }
}
