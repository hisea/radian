//! Building an [`App`] and setting it up: [`App::new`], the `set_*` seams the host and the
//! tests write through (context window, tier, memory, auto, access source, sandbox note, MCP
//! counts, projection), the `apply_config` fold of `[tui]`, the display modes a frame lays the
//! transcript out with, and the herdr state the loop reports.

use super::*;

impl App {
    pub fn new(model_name: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            hostname: None,
            auto_compact: true,
            now: Instant::now(),
            footer: crate::footer::Footer::default(),
            contributions: radian_host::ContributionStore::new(),
            footer_context: radian_host::FooterContext::new(),
            input: Composer::new(),
            quit: false,
            new_session: false,
            pending: 0,
            quit_pending: None,
            last_answered: None,
            layers: vec![Layer::Surface],
            memory_enabled: false,
            pending_expand: VecDeque::new(),
            slash_completion: None,
            mention_completion: None,
            completion_cursor: 0,
            pending_fetch: None,
            theme: Theme::default(),
            brand: None,
            spinner_kind: crate::spinner::Kind::default(),
            keymap: Keymap::default(),
            input_style: InputStyle::default(),
            startup: Startup::Session,
            picker_outcome: None,
            mouse_enabled: true,
            clipboard: ClipboardMode::Osc52,
            bell: BellMode::Off,
            // ADR 0028 R3's default: thinking collapsed to one line.
            reasoning_mode: ReasoningMode::Collapsed,
            tool_display: crate::tools::ToolDisplay::default(),
            mermaid_enabled: true,
            highlight_enabled: true,
            inspector_last: None,
            session: SessionView {
                session_id: session_id.into(),
                model_name: model_name.into(),
                model_display: None,
                provider: None,
                has_model: true,
                max_model_calls: radian_host::DEFAULT_MAX_MODEL_CALLS,
                cwd: None,
                git_branch: None,
                model_price: None,
                spinner_origin: Instant::now(),
                timer: crate::timing::TurnTimer::default(),
                thinking_level: None,
                entries: Vec::new(),
                live_count: 0,
                history: Vec::new(),
                history_cursor: None,
                history_stash: String::new(),
                scroll: 0,
                follow: true,
                projection: Arc::new(SessionProjection::new()),
                note: None,
                running_tools: Vec::new(),
                running_jobs: Vec::new(),
                queued: HashSet::new(),
                last_turn_failed: false,
                loop_state: loop_mode::LoopState::default(),
                goal_mode: GoalMode::default(),
                session_ended: None,
                turn_committed: true,
                pending_fork: None,
                compacting: false,
                startup_screen: None,
                startup_len: 0,
                auto: false,
                access_source: crate::profile::AccessSource::Default,
                sandbox_note: None,
                next_model: None,
                next_thinking: None,
                bash_running: None,
                queued_bash: None,
                bash_available: true,
                bash_context_pending: None,
                total_lines: 0,
                view_height: 0,
                max_scroll: 0,
                workspace: None,
                transcript_width: 80,
                selection: None,
                selecting: None,
                last_click: None,
                pending_compaction: None,
                wake_notice: None,
                wake_waiting: false,
            },
        }
    }

    /// The display modes a frame lays the transcript out with (ADR 0028 R1/R3/R4): the
    /// reasoning fold and the tool fold together are the layout cache key.
    pub fn display_modes(&self) -> crate::tools::DisplayModes {
        crate::tools::DisplayModes {
            reasoning: self.reasoning_mode,
            tools: self.tool_display,
            mermaid: self.mermaid_enabled,
            highlight: self.highlight_enabled,
            glyphs: self.footer_glyphs(),
        }
    }

    /// Records the effective input budget `B` (no budget is configured when this is `None`).
    ///
    /// The host normally writes this through the projection when it opens the session; the
    /// method is the seam the tests use.
    pub fn set_context_window(&mut self, input_budget: Option<u64>) {
        self.session.projection.set_context_window(input_budget);
    }

    /// Records the permission tier's display name, for the status bar.
    pub fn set_tier(&mut self, tier: impl Into<String>) {
        self.session.projection.set_tier(Some(tier.into()));
    }

    /// Whether this process has a memory root, for `/memory`. Set once at startup; the service
    /// itself stays on the loop's side (ADR 0023 U3).
    pub fn set_memory_enabled(&mut self, enabled: bool) {
        self.memory_enabled = enabled;
    }

    /// The session's in-session auto state (ADR 0026 D9), set at startup from the resolved
    /// policy and afterwards only by the switch itself.
    pub fn set_auto(&mut self, auto: bool) {
        self.session.auto = auto;
    }

    /// Where the tier in force came from (ADR 0038 decision 3): the startup resolution, or the
    /// run-time switch's own record. `/access` with no argument prints it.
    pub fn set_access_source(&mut self, source: crate::profile::AccessSource) {
        self.session.access_source = source;
    }

    /// The line `/access` adds when the sandbox is inherited from an outer one (ADR 0016
    /// revision 2 decision 4). The host passes the startup profile line; nothing else sets it.
    pub fn set_sandbox_note(&mut self, note: Option<String>) {
        self.session.sandbox_note = note;
    }

    /// Whether this session has a shell for `!` (ADR 0033): false when the session mounts
    /// no `shell`.
    pub fn set_bash_available(&mut self, available: bool) {
        self.session.bash_available = available;
    }

    /// Whether a model is routed (ADR 0008 §2). False when the session started with no model;
    /// `/model` sets it when the first route is queued.
    pub fn set_has_model(&mut self, has_model: bool) {
        self.session.has_model = has_model;
    }

    /// Applies the wired `tui.*` configuration. A bad keymap (a key bound to two actions) is
    /// an error the host turns into a startup refusal; unknown actions/keys are warnings.
    pub fn apply_config(
        &mut self,
        config: &radian_host::config::Config,
        no_color: bool,
    ) -> Result<Vec<String>, String> {
        let keymap = Keymap::from_config(&config.tui.keybindings).map_err(|e| e.to_string())?;
        let mut warnings = keymap.warnings.clone();
        self.keymap = keymap;
        // The theme: built-in names resolve directly, anything else is a file in
        // `~/.radian/themes/` (ADR 0029). NO_COLOR wins over both the file and the
        // `[tui.colors]` overrides (ADR 0029 decision 7); the theme module owns the rules.
        let (theme, theme_warnings) = crate::theme::load(
            config.tui.theme.as_deref(),
            no_color,
            crate::theme::truecolor_env(),
            radian_host::themes_dir().as_deref(),
            &config.tui.colors,
        );
        self.theme = theme;
        warnings.extend(theme_warnings);
        // `tui.brand` (ADR 0027): a configured short text, else `None` for the
        // glyph set's default. The reader already refused an over-wide value.
        self.brand = config.tui.brand.clone();
        // `tui.spinner` (ADR 0027): an unknown name warns and uses `pulse`.
        self.spinner_kind = match config.tui.spinner.as_deref() {
            None => crate::spinner::Kind::default(),
            Some(name) => crate::spinner::Kind::parse(name).unwrap_or_else(|| {
                warnings.push(format!(
                    "tui.spinner = {name:?} is not one of pulse|arc|quadrant|braille; using pulse"
                ));
                crate::spinner::Kind::default()
            }),
        };
        // `tui.input_style` (ADR 0035; `bar` removed by the 2026-10-16
        // note): `lines` (the default) or `box`. The removed `bar`, and any other unknown
        // spelling, warns once and uses `lines` — a display preference, never a refusal
        // (ADR 0008).
        self.input_style = match config.tui.input_style.as_deref() {
            None => InputStyle::default(),
            Some("bar") => {
                warnings.push("tui.input_style = \"bar\" was removed; using lines".into());
                InputStyle::default()
            }
            Some(name) => InputStyle::parse(name).unwrap_or_else(|| {
                warnings.push(format!(
                    "tui.input_style = {name:?} is not one of lines|box; using lines"
                ));
                InputStyle::default()
            }),
        };
        // `tui.display.reasoning` (ADR 0028 R3, default collapsed) and
        // `tui.display.tool_output` + `tool_preview_lines` (R4, default errors / 5). An
        // unknown spelling never reaches here (the host reader skips it with a warning).
        self.reasoning_mode = config
            .tui
            .display_reasoning
            .as_deref()
            .and_then(ReasoningMode::parse)
            .unwrap_or_default();
        self.tool_display = crate::tools::ToolDisplay {
            fold: config
                .tui
                .display_tool_output
                .as_deref()
                .and_then(crate::tools::ToolFold::parse)
                .unwrap_or_default(),
            preview_lines: config
                .tui
                .display_tool_preview_lines
                .map(|lines| lines as usize)
                .unwrap_or(5),
        };
        // `tui.display.mermaid` (ADR 0028 R8, default true).
        self.mermaid_enabled = config.tui.display_mermaid.unwrap_or(true);
        self.highlight_enabled = config.tui.display_highlight.unwrap_or(true);
        // `tui.clipboard` (default OSC 52). An unknown spelling never reaches here (the host
        // config reader skips it with a warning); `pbcopy` off macOS does: warn once and fall
        // back, because the copy effect would have no program to run.
        self.mouse_enabled = config.tui.mouse.unwrap_or(true);
        self.clipboard = config
            .tui
            .clipboard
            .as_deref()
            .and_then(ClipboardMode::parse)
            .unwrap_or_default();
        if self.clipboard == ClipboardMode::PbCopy && !self.clipboard.supported_on_this_platform() {
            self.clipboard = ClipboardMode::Osc52;
            warnings.push("tui.clipboard = \"pbcopy\" is macOS-only; using osc52".into());
        }
        // `tui.bell` (default off). An unknown spelling never reaches here (the host
        // config reader skips it with a warning, the way `tui.clipboard` does); a direct
        // construction falls back to off all the same. `NO_COLOR` does not affect it: the
        // bell is a control character, not a color.
        self.bell = config
            .tui
            .bell
            .as_deref()
            .and_then(BellMode::parse)
            .unwrap_or_default();
        // `[tui.footer]` (ADR 0027 C2); the legacy `[tui.widgets]` (C3) folds into rows
        // positioned above/below the input (ADR 0027), with one warning.
        let mut footer = config.tui.footer.clone();
        if let Some(warning) = crate::footer::fold_legacy_widgets(&mut footer, &config.tui.widgets)
        {
            warnings.push(warning);
        }
        warnings.extend(self.apply_footer(&footer));
        warnings.extend(
            config
                .skipped
                .iter()
                .map(|key| format!("ignored key `{key}`")),
        );
        Ok(warnings)
    }

    /// Records the MCP state: how many servers are configured, and what the opened session
    /// reported (if it has opened).
    pub fn set_mcp(&mut self, configured: usize, open: Option<(usize, usize)>) {
        self.session.projection.set_mcp(configured, open);
    }

    /// Replaces the read handle with the host's projection, so the renderer reads the same
    /// facts the facade owns.
    /// The inputs to the herdr state function (ADR 0034 H2): the turn state, the pending
    /// queue's length, whether the startup trust prompt is waiting, and whether a block
    /// waits while idle (a used-up wake budget, or a `/goal` budget stop, ADR 0047).
    pub fn herdr_state_inputs(&self) -> (TurnState, usize, bool, bool) {
        let trust = matches!(self.top_modal(), Some(Modal::Trust(_)));
        (
            self.session.projection.read().turn(),
            self.pending,
            trust,
            self.session.wake_waiting || self.session.goal_mode.blocked,
        )
    }

    /// What is blocking, for herdr's category-only message (ADR 0034 H2). The approval's tool
    /// name only - never the command, path or question body. A used-up wake budget reports
    /// only while no modal claims the block; a `/goal` budget stop (ADR 0047) reports with
    /// the `goal budget` label until the next turn starts.
    pub fn herdr_blocked(&self) -> Option<radian_host::herdr::Blocked> {
        match self.top_modal() {
            Some(Modal::Approval(prompt)) => Some(radian_host::herdr::Blocked::Approval(
                prompt.request.tool.clone(),
            )),
            Some(Modal::Question(_)) => Some(radian_host::herdr::Blocked::Question),
            _ if self.session.goal_mode.blocked => Some(radian_host::herdr::Blocked::GoalBudget),
            _ if self.session.wake_waiting => Some(radian_host::herdr::Blocked::WakeBudget),
            _ => None,
        }
    }

    pub fn set_projection(&mut self, projection: Arc<SessionProjection>) {
        self.session.projection = projection;
    }
}
