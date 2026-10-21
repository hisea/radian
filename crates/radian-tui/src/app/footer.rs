//! The footer and the status line: the startup facts the footer draws from (cwd, model,
//! provider, thinking level), the spinner's frame and status, the contributor store and the
//! `[tui.footer]` fold, and the four tests that pin the spinner and the compaction status to
//! the injected clock.

use super::*;

impl App {
    /// The footer's startup facts (ADR 0027 C2): the working directory, the model's display
    /// name and provider, and the applied thinking level.
    pub fn set_footer_facts(
        &mut self,
        cwd: Option<PathBuf>,
        model_display: Option<String>,
        provider: Option<String>,
        thinking_level: Option<String>,
    ) {
        self.session.cwd = cwd;
        self.session.model_display = model_display;
        self.session.provider = provider;
        self.session.thinking_level = thinking_level;
    }

    /// The background-refreshed git branch (ADR 0027 C2: the draw path never runs git). The
    /// host's `GitContributor` is the normal writer now; this setter remains for a host that
    /// has a first value without the contributor.
    pub fn set_git_branch(&mut self, branch: Option<String>) {
        self.session.git_branch = branch;
    }

    /// The machine's hostname (ADR 0027), read once at startup.
    pub fn set_hostname(&mut self, hostname: Option<String>) {
        self.hostname = hostname;
    }

    /// The model's catalog unit prices (ADR 0027). Written through to the
    /// projection, which is what prices each arriving call: the footer reads the accumulated
    /// fee from there, because a call keeps the price it arrived under even after a switch.
    pub fn set_model_price(&mut self, price: Option<radian_host::ModelPrice>) {
        self.session.model_price = price.clone();
        self.session.projection.set_price(price);
    }

    /// Whether automatic compaction is on (ADR 0027).
    pub fn set_auto_compact(&mut self, on: bool) {
        self.auto_compact = on;
    }

    /// The loop's current instant (ADR 0027): the timer never reads the clock.
    pub fn set_now(&mut self, now: Instant) {
        self.now = now;
    }

    /// The spinner's current frame (ADR 0027): a function of the injected clock,
    /// so a test advances it with `set_now`. Whether the pulse interpolates comes from the theme
    /// (and the glyph set); a frame is a pure function of its inputs, which is what makes
    /// "redraw only when it changed" a comparison of frames.
    pub fn spinner_frame(&self) -> crate::spinner::Frame {
        let tick = self
            .now
            .saturating_duration_since(self.session.spinner_origin);
        crate::spinner::frame(
            self.spinner_kind,
            self.footer_glyphs(),
            self.theme.pulse_can_breathe(),
            tick,
        )
    }

    /// How long the spinner's current frame stays (ADR 0027): the breath's step
    /// while the pulse interpolates, its 500 ms two-colour switch when it cannot, and the
    /// rotating kinds' 250 ms. The loop wakes at this interval while a spinner is visible.
    pub fn spinner_interval(&self) -> Duration {
        crate::spinner::frame_interval(
            self.spinner_kind,
            self.footer_glyphs(),
            self.theme.pulse_can_breathe(),
        )
    }

    /// The style for a spinner frame: the pulse breathes between the accent and the dim level
    /// when both ends are RGB (ADR 0027); otherwise it is two-colour
    /// switch, and the rotating frames stay on the accent.
    pub fn spinner_style(&self, frame: crate::spinner::Frame) -> ratatui::style::Style {
        // One definition, shared with the transcript's running tool head (`crate::render`).
        crate::render::spinner_style(&self.theme, frame)
    }

    /// The input rule's status text (ADR 0027): the most specific running state, or
    /// `None` when the app is idle (the rule is then plain). The states are the ones the UI
    /// already has: a modal waiting for the user, a running turn, a running `!`.
    pub fn spinner_status(&self) -> Option<&'static str> {
        if matches!(self.top_modal(), Some(Modal::Approval(_))) {
            return Some("Waiting for approval");
        }
        if matches!(self.top_modal(), Some(Modal::Question(_))) {
            return Some("Waiting for your answer");
        }
        // A running compaction is the most specific state (ADR 0018).
        if self.session.compacting {
            return Some("Compacting");
        }
        if self.turn_running() {
            return Some("Working");
        }
        if self.session.bash_running.is_some() {
            return Some("Running a command");
        }
        None
    }

    /// Whether a turn is running (the projection's own state, ADR 0023 U5). The `bar` status
    /// row shows `esc to interrupt` exactly then: Esc cancels a running turn (ADR 0009).
    pub fn turn_running(&self) -> bool {
        self.turn() == TurnState::Running
    }

    /// Whether any spinner is on screen (ADR 0027): the input rule's status line,
    /// a live reasoning line, or a running tool card. Only then does the loop animate.
    pub fn spinner_visible(&self) -> bool {
        self.spinner_status().is_some()
            || self.session.entries.iter().any(|entry| entry.live)
            || self
                .session
                .entries
                .iter()
                .any(|entry| entry.kind == EntryKind::ToolCall && entry.result.is_none())
    }

    /// The contributor store the renderer reads (ADR 0027 C3).
    pub fn set_contributions(
        &mut self,
        contributions: std::sync::Arc<radian_host::ContributionStore>,
    ) {
        self.contributions = contributions;
    }

    /// Installs the shared footer-facts handoff the loop publishes into and a background
    /// `command` item reads (ADR 0027).
    pub fn set_footer_context(&mut self, context: std::sync::Arc<radian_host::FooterContext>) {
        self.footer_context = context;
    }

    /// The footer facts a `command` item may read (ADR 0027). Only facts
    /// the footer already displays: the model, the cwd, the branch, the context proportion, the
    /// token totals and the fee. A missing fact is `None`, never a fabricated
    /// number (v5 §5). The git branch prefers the background contributor's value and falls back
    /// to the host's one-shot read.
    pub fn footer_snapshot(&self) -> radian_host::FooterSnapshot {
        let session = self.session.projection.read();
        let branch = self
            .contributions
            .get("git_branch")
            .map(|contribution| {
                contribution
                    .segments
                    .iter()
                    .map(|segment| segment.text.as_str())
                    .collect::<String>()
            })
            .filter(|branch| !branch.is_empty())
            .or_else(|| self.session.git_branch.clone());
        radian_host::FooterSnapshot {
            model: Some(self.session.model_name.clone()),
            cwd: self
                .session
                .cwd
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            branch,
            context_percent: crate::footer::context_fraction(&session)
                .map(|fraction| fraction * 100.0),
            tokens_input: session.usage_input(),
            tokens_output: session.usage_output(),
            tokens_cache_read: session.usage_cache_read(),
            cost_usd: session
                .cost_micros()
                .map(|micros| micros as f64 / 1_000_000.0),
        }
    }

    /// Publishes the current footer facts for the background `command` items (ADR 0027).
    /// A pure state read plus one lock write; the command reads the latest
    /// snapshot when it runs, so the UI never waits for it.
    pub fn publish_footer_context(&self) {
        self.footer_context.set(self.footer_snapshot());
    }

    /// The applied thinking level, once the driver reports it (ADR 0026 D11).
    pub fn set_thinking_level(&mut self, level: Option<String>) {
        self.session.thinking_level = level;
    }

    /// The footer's glyph set, for the items that draw an icon.
    pub fn footer_glyphs(&self) -> crate::footer::Glyphs {
        self.footer.glyphs()
    }

    /// The brand mark (ADR 0027): `tui.brand` when set, else the glyph set's
    /// default. The footer's `brand` item and the startup banner's one-line fallback share it,
    /// so the two can never disagree.
    pub fn brand_mark(&self) -> &str {
        self.brand
            .as_deref()
            .unwrap_or_else(|| crate::footer::brand_mark(self.footer_glyphs()))
    }

    /// Applies `[tui.footer]` (ADR 0027 C2). Returns the footer's warnings (unknown item,
    /// placeholder, style), shown once at startup.
    pub fn apply_footer(&mut self, config: &radian_host::config::TuiFooterConfig) -> Vec<String> {
        let (footer, warnings) = crate::footer::Footer::from_config(config);
        self.footer = footer;
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0027: the spinner frame follows the injected clock, not the text, and
    /// the configured kind decides the glyphs.
    #[test]
    fn the_spinner_frame_follows_the_injected_clock() {
        let mut app = App::new("m", "s");
        app.spinner_kind = crate::spinner::Kind::Arc;
        let base = app.session.spinner_origin;
        app.set_now(base);
        assert_eq!(app.spinner_frame().glyph, "◜");
        app.set_now(base + std::time::Duration::from_millis(250));
        assert_eq!(app.spinner_frame().glyph, "◝");
        app.set_now(base + std::time::Duration::from_millis(500));
        assert_eq!(app.spinner_frame().glyph, "◞");

        // The default pulse keeps its glyph and only changes its colour: with a non-RGB theme
        // (revision 13 decision 3) it is revision 10's two-colour switch, every 500 ms.
        app.spinner_kind = crate::spinner::Kind::Pulse;
        app.theme = crate::theme::Theme::mono();
        app.set_now(base);
        assert_eq!(app.spinner_frame().step, crate::spinner::PULSE_ACCENT_STEP);
        app.set_now(base + std::time::Duration::from_millis(500));
        assert_eq!(app.spinner_frame().step, crate::spinner::PULSE_DIM_STEP);
        // An RGB theme breathes instead: the step advances every 125 ms.
        app.theme = crate::theme::Theme::dark();
        app.set_now(base);
        assert_eq!(app.spinner_frame().step, crate::spinner::PULSE_DIM_STEP);
        app.set_now(base + app.spinner_interval());
        assert!(
            app.spinner_frame().step > crate::spinner::PULSE_DIM_STEP,
            "the second sample is already brighter"
        );
        assert_eq!(app.spinner_interval(), crate::spinner::pulse_step());
    }

    /// ADR 0018 + ADR 0027: the compaction events drive the `Compacting` status.
    #[test]
    fn the_compacting_status_follows_the_compaction_events() {
        let mut app = App::new("m", "s");
        app.session.projection.set_turn(TurnState::Running);
        assert_eq!(app.spinner_status(), Some("Working"));

        app.apply_event(&AgentEvent::CompactionStarted {
            revision: 1,
            tokens_before: 100,
            keep_target: 50,
            trigger: agent_core::CompactionTrigger::Budget,
        });
        assert_eq!(app.spinner_status(), Some("Compacting"));

        app.apply_event(&AgentEvent::CompactionFinished {
            revision: 1,
            tokens_before: 100,
            tokens_after: 40,
            below_target: false,
            deterministic: false,
            dropped: 0,
        });
        assert_eq!(app.spinner_status(), Some("Working"));

        // A failed or skipped compaction clears it too.
        app.apply_event(&AgentEvent::CompactionStarted {
            revision: 2,
            tokens_before: 100,
            keep_target: 50,
            trigger: agent_core::CompactionTrigger::Manual,
        });
        app.apply_event(&AgentEvent::CompactionFailed {
            reason: "boom".to_string(),
        });
        assert_eq!(app.spinner_status(), Some("Working"));
        app.apply_event(&AgentEvent::CompactionStarted {
            revision: 3,
            tokens_before: 100,
            keep_target: 50,
            trigger: agent_core::CompactionTrigger::Manual,
        });
        app.apply_event(&AgentEvent::CompactionSkipped {
            reason: "already fits".to_string(),
        });
        assert_eq!(app.spinner_status(), Some("Working"));
    }

    /// ADR 0018: a deterministic compaction shows "compacted without a
    /// summary (N dropped)", and the summary entry's folded line says "without a summary".
    #[test]
    fn a_deterministic_compaction_is_shown_without_a_summary() {
        let mut app = App::new("m", "s");
        app.apply_event(&AgentEvent::CompactionFinished {
            revision: 2,
            tokens_before: 900,
            tokens_after: 120,
            below_target: false,
            deterministic: true,
            dropped: 7,
        });
        assert!(
            app.session
                .entries
                .iter()
                .any(|entry| entry.text == "compacted without a summary (7 dropped)"),
            "the notice names the count: {:?}",
            app.session
                .entries
                .iter()
                .map(|e| &e.text)
                .collect::<Vec<_>>()
        );

        let message = agent_core::compaction::compacted_history(
            "[context compacted without a summary: 7 messages dropped, 0 tool results truncated]",
            &[],
            &[],
        )
        .unwrap();
        app.push_message(&message[0]);
        let info = app
            .session
            .entries
            .iter()
            .find_map(|entry| entry.compaction.as_deref())
            .expect("the summary entry");
        assert!(info.deterministic);
        assert_eq!(info.dropped, Some(7));
        assert!(
            info.line().contains("without a summary") && info.line().contains("7 dropped"),
            "{}",
            info.line()
        );
    }

    /// ADR 0017: a retry names the attempt on the status line and discards the
    /// failed attempt's partial stream.
    #[test]
    fn a_retrying_event_drops_the_live_block_and_names_the_attempt() {
        let mut app = App::new("m", "s");
        app.set_live(EntryKind::Assistant, "half an answer");
        assert_eq!(app.session.live_count, 1);

        app.apply_event(&AgentEvent::ModelRetrying {
            attempt: 1,
            max: 3,
            delay: std::time::Duration::from_secs(4),
            reason: "the stream ended before the response was complete".into(),
        });

        assert_eq!(
            app.session.live_count, 0,
            "the failed attempt's partial text must be gone"
        );
        assert!(app.session.entries.iter().all(|entry| !entry.live));
        assert_eq!(
            app.session.note.as_deref(),
            Some("retrying 1/3 in 4s: the stream ended before the response was complete")
        );
    }
}
