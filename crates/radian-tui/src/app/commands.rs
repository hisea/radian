//! The `/…` commands: one line parsed and dispatched (`run_slash_command`), the effects' results
//! (the `/login` wizard's outcome, the model and thinking switches with their compaction
//! fallback), and the `/usage` lines. Parsing is pure; everything that touches the world is an
//! [`Effect`].

use super::*;

impl App {
    /// Applies the wizard's outcome: stay, or hand the loop an effect / close. The "what
    /// next" line on success is the loop's (it knows the save succeeded).
    pub(super) fn login_outcome(&mut self, outcome: LoginOutcome) -> UpdateResult {
        match outcome {
            LoginOutcome::Stay => {}
            LoginOutcome::Cancel => {
                self.remove_modal(|modal| matches!(modal, Modal::Login(_)));
                self.push_notice("login cancelled; nothing was saved");
            }
            LoginOutcome::Verify {
                name,
                protocol,
                base_url,
                key,
            } => {
                return UpdateResult::effect(Effect::LoginVerify {
                    name,
                    protocol,
                    base_url,
                    key,
                });
            }
            LoginOutcome::Save { name, key, env } => {
                return UpdateResult::effect(Effect::LoginSave { name, key, env });
            }
            LoginOutcome::BeginOAuthBrowser => {
                return UpdateResult::effect(Effect::LoginOAuthBrowser);
            }
            LoginOutcome::BeginOAuthDevice => {
                return UpdateResult::effect(Effect::LoginOAuthDevice);
            }
        }
        UpdateResult::dirty()
    }

    /// Runs one `/…` line (and a `!`/`!!` line, which shares the parse). Parsing is pure;
    /// everything that touches the world becomes an effect, and its result comes back as
    /// another [`Input`]. The line is not sent anywhere.
    pub(super) fn run_slash_command(&mut self, line: &str) -> UpdateResult {
        match command::parse(line, self.memory_enabled) {
            command::Outcome::Notices(notices) => {
                for notice in notices {
                    self.push_notice(notice);
                }
                UpdateResult::dirty()
            }
            // `/queue`: the text goes through the same `@` expansion as a send, but the
            // result is always a followup — never a steer, even while a turn runs.
            command::Outcome::Queue(text) => self.begin_queue(text),
            // `/retry`: resend the last user message, but only after a model-error turn.
            command::Outcome::Retry => self.retry(),
            // `/loop`: toggle the per-turn repeat, or arm/enable it with limits and an
            // optional prompt. The loop's own inline prompt goes through `@` expansion
            // like `/queue`.
            command::Outcome::Loop(command) => self.loop_command(command),
            // `/goal` (ADR 0047): the objective is literal text (no `@` expansion — the
            // stored objective is re-sent verbatim every turn). Changes journal through
            // the loop; `Show` only reads.
            command::Outcome::Goal(command) => self.goal_command(command),
            command::Outcome::Memory(command) => {
                // The interface has no stdin: a bare `-` writes nothing. Show the
                // one notice and keep the typed line so it can be edited in place.
                if let Some(notice) = command::stdin_dash_notice(&command) {
                    self.input = line.into();
                    self.push_notice(notice.to_owned());
                    return UpdateResult::dirty();
                }
                UpdateResult::effect(Effect::RunMemoryCommand {
                    command,
                    confirmed: false,
                })
            }
            // `/compact` is asynchronous: the loop runs it on the runtime between turns and the
            // result comes back as `Input::CompactDone`.
            command::Outcome::Compact(instructions) => {
                // ADR 0008 §2: compaction needs a model to summarize with.
                if !self.session.has_model {
                    self.push_notice("no model is set; choose one with /model before compacting");
                    return UpdateResult::dirty();
                }
                UpdateResult::effect(Effect::Compact { instructions })
            }
            command::Outcome::Sessions(query) => {
                // `/sessions` opens the picker overlay (ADR 0023 U4): listing reads the session
                // directory, so it is an effect the loop runs.
                UpdateResult::effect(Effect::OpenPicker { query })
            }
            command::Outcome::Export(request) => {
                UpdateResult::effect(Effect::Session(command::SessionOutcome::Export(request)))
            }
            command::Outcome::Fork => {
                // ADR 0022 §1: fork only when the session is idle. The decision is app state,
                // so it is made here rather than in the loop.
                if self.session.session_ended.is_some() {
                    self.push_notice(
                        "the session has ended; there is nothing to fork (`/fork` needs a live \
                         session)",
                    );
                    return UpdateResult::dirty();
                }
                if self.turn() == TurnState::Running {
                    self.push_notice(
                        "finish the current turn before forking (`/fork` needs an idle session)",
                    );
                    return UpdateResult::dirty();
                }
                // The turn ended (idle) but its boundary may not be on disk yet: the fork
                // reads the parent file, so it would copy an empty history. Wait for the
                // journaled `TurnFinished` (bounded by `FORK_COMMIT_WAIT`).
                if !self.session.turn_committed {
                    self.session.pending_fork = Some(self.now + FORK_COMMIT_WAIT);
                    return UpdateResult::dirty();
                }
                UpdateResult::effect(Effect::Session(command::SessionOutcome::Fork))
            }
            command::Outcome::New => {
                // `/new` ends this session and opens a fresh one in place. Like `/fork` it
                // needs an idle live session; a running background job would outlive the
                // session it belongs to, so it refuses with a pointer to `/jobs kill`.
                // Leaving the loop IS the execution, so this sets the flags, not an effect.
                if self.session.session_ended.is_some() {
                    self.push_notice(
                        "the session has ended; there is nothing to start over (`/new` needs a \
                         live session)",
                    );
                    return UpdateResult::dirty();
                }
                if self.turn() == TurnState::Running {
                    self.push_notice(
                        "finish the current turn before starting a new session (`/new` needs \
                         an idle session)",
                    );
                    return UpdateResult::dirty();
                }
                if !self.session.running_jobs.is_empty() {
                    self.push_notice(
                        "a background job is still running; stop it with `/jobs kill` first \
                         (`/new` needs an idle session)",
                    );
                    return UpdateResult::dirty();
                }
                self.new_session = true;
                self.quit = true;
                UpdateResult::dirty()
            }
            command::Outcome::Theme(name) => {
                // ADR 0008: the loop loads the theme (I/O) and writes `tui.theme`
                // back to the config file; the notice says which of those happened.
                UpdateResult::effect(Effect::Theme { name })
            }
            command::Outcome::Thinking(level) => {
                UpdateResult::effect(Effect::SetThinking { level })
            }
            // `/usage` (ADR 0027): the snapshot is already in the store —
            // the background source wrote it — so this reads it and prints; no effect, and no
            // network of its own.
            command::Outcome::Usage => {
                for line in self.usage_lines() {
                    self.push_notice(line);
                }
                UpdateResult::dirty()
            }
            command::Outcome::Model(switch) => UpdateResult::effect(Effect::ModelSwitch {
                provider: switch.provider,
                model: switch.model,
            }),
            // `/login`: the wizard is app state (pure); its network and file work are effects
            // the loop runs (ADR 0023 U3). An explicit `/login <id>` skips the choice step.
            command::Outcome::Login(provider) => {
                let step = match provider {
                    None => crate::login::LoginStep::ChooseProvider { cursor: 0 },
                    Some(id) if id.starts_with("no-such-provider:") => {
                        let name = id.trim_start_matches("no-such-provider:").to_owned();
                        self.push_notice(format!(
                            "unknown provider {name:?}: /login takes a built-in provider or \
                             `custom`"
                        ));
                        return UpdateResult::dirty();
                    }
                    // The two ChatGPT OAuth choices (ADR 0026) are not descriptors,
                    // so they do not go through `for_provider`: pushing the modal and kicking
                    // off the effect happens together, exactly as choosing them from the
                    // picker does in `LoginWizard::on_key`.
                    Some(id) if id == radian_host::CHATGPT_PROVIDER_ID => {
                        self.push_modal(Modal::Login(Box::new(LoginWizard::new(
                            crate::login::LoginStep::OAuthBrowserWaiting { url: None },
                        ))));
                        return UpdateResult::effect(Effect::LoginOAuthBrowser);
                    }
                    Some(id) if id == radian_host::CHATGPT_DEVICE_LOGIN => {
                        self.push_modal(Modal::Login(Box::new(LoginWizard::new(
                            crate::login::LoginStep::OAuthDeviceWaiting {
                                user_code: String::new(),
                                verification_url: String::new(),
                            },
                        ))));
                        return UpdateResult::effect(Effect::LoginOAuthDevice);
                    }
                    Some(id) => {
                        // for_provider builds the matching first step (D3).
                        self.push_modal(Modal::Login(Box::new(LoginWizard::for_provider(&id))));
                        return UpdateResult::dirty();
                    }
                };
                self.push_modal(Modal::Login(Box::new(LoginWizard::new(step))));
                UpdateResult::dirty()
            }
            // `/logout <provider>`: confirm first (`y`/`n`), then remove (D3).
            command::Outcome::Logout(provider) => {
                if provider.is_empty() {
                    self.push_notice("usage: /logout <provider>");
                    return UpdateResult::dirty();
                }
                if let Some(extra) = provider.strip_prefix("__extra__") {
                    self.push_notice(format!(
                        "unexpected extra name {extra:?}; usage: /logout <provider>"
                    ));
                    return UpdateResult::dirty();
                }
                self.push_modal(Modal::ConfirmLogout(provider));
                UpdateResult::dirty()
            }
            command::Outcome::AutoToggle(on) => {
                // ADR 0026 D9: the in-session switch. `/auto on` needs no confirmation (it is
                // typed, not one keypress), and a no-op switch is still recorded — the audit
                // line says who switched, so the journal keeps every command.
                self.session.auto = on;
                if on {
                    self.push_notice(
                        "auto: tool calls are approved without asking for the rest of this \
                         session; deny rules and the sandbox still apply",
                    );
                } else {
                    self.push_notice("auto off: tool asks prompt again");
                }
                UpdateResult::effect(Effect::SetApprovalPolicy { on })
            }
            // `/trust` (ADR 0031 T3): the query is a read, the writes go through the facade.
            // Every write changes disk state only (loaded resources never hot-load or
            // hot-unload) and prints one takes-effect line.
            command::Outcome::Trust(request) => match request.write() {
                None => UpdateResult::effect(Effect::TrustStatus),
                Some(write) => UpdateResult::effect(Effect::WriteTrust { write }),
            },
            // `/access` (ADR 0038 slice 2): a query with no argument, otherwise a switch. Only
            // `full` needs confirmation (it is "no sandbox"); the model has no tool for this.
            command::Outcome::Access(name) => match name {
                None => {
                    let current = self
                        .session
                        .projection
                        .read()
                        .tier()
                        .map(str::to_owned)
                        .unwrap_or_else(|| "unknown".to_owned());
                    self.push_notice(format!(
                        "access: {current} (from {})",
                        self.session.access_source.label()
                    ));
                    // ADR 0016: an inherited sandbox says so here too,
                    // rather than letting `workspace` read as radian's own rules.
                    if let Some(note) = self.session.sandbox_note.clone() {
                        self.push_notice(note);
                    }
                    UpdateResult::dirty()
                }
                Some(name) => {
                    let tier = match crate::profile::parse_tier(&name) {
                        Ok(tier) => tier,
                        Err(message) => {
                            self.push_notice(message);
                            return UpdateResult::dirty();
                        }
                    };
                    // The projection's tier may carry a source suffix (`full (project)`); the
                    // tier name is the first word.
                    let current = self
                        .session
                        .projection
                        .read()
                        .tier()
                        .map(str::to_owned)
                        .unwrap_or_default();
                    let current_tier = current.split_whitespace().next().unwrap_or("");
                    if current_tier == tier.as_str() {
                        self.push_notice(format!("access is already {}", tier.as_str()));
                        return UpdateResult::dirty();
                    }
                    if tier == crate::profile::Tier::FullAccess {
                        // `full` means no sandbox: confirm before opening it (ADR 0038
                        // decision 2). The panel's default focus is `cancel`.
                        self.push_modal(Modal::AccessFull(Box::default()));
                        return UpdateResult::dirty();
                    }
                    UpdateResult::effect(Effect::SetAccess {
                        tier,
                        remember: false,
                    })
                }
            },
            // `/skill:` (ADR 0025 S3): the host reads the body and the expanded text becomes a
            // user message. Whether it steers is decided here, like a typed send.
            command::Outcome::Skill { name, args } => {
                let steer = self.turn() == TurnState::Running;
                UpdateResult::effect(Effect::ExpandSkill { name, args, steer })
            }
            // `!`/`!!` (ADR 0033): executed by the loop in the sandbox. `!` also enqueues its
            // `Ran \`cmd\`` message for the model, but only once it has run.
            command::Outcome::Bash {
                command,
                exclude_from_context,
            } => self.begin_bash(command, exclude_from_context),
            // `/skills`: the host lists the discovered skills as notices.
            command::Outcome::Skills => UpdateResult::effect(Effect::ListSkills),
            // `/context [all]`: the loop reads the estimate from the facade; the
            // answer comes back as `Input::ContextReady`.
            command::Outcome::Context { all } => UpdateResult::effect(Effect::Context { all }),
            // `/btw [question]`: ask from the current context, or reopen the last
            // side answer. Pure: the loop sends the request and streams it back.
            command::Outcome::Btw(question) => self.run_btw(question),
            command::Outcome::Jobs(request) => UpdateResult::effect(Effect::Jobs(request)),
            // The inspector (`/jobs`, `/tools`, `/mcp`, `/hooks`, `/panel`): the loop
            // gathers the snapshot; `update` stays pure. A bare `/jobs` opens the jobs
            // page; `kill` forms stay text (the `Jobs` arm above).
            command::Outcome::Inspector(page) => {
                UpdateResult::effect(Effect::OpenInspector { page })
            }
            command::Outcome::Copy { code_only } => self.copy(code_only),
            command::Outcome::Message => UpdateResult::none(),
        }
    }

    /// `/usage`'s lines (ADR 0027): the current provider's snapshot,
    /// every window of it. With no provider, no usage source for the routed provider, or no
    /// snapshot yet, one line says so instead.
    fn usage_lines(&self) -> Vec<String> {
        let Some(provider) = self.session.provider.as_deref() else {
            return vec!["usage: no provider is routed yet".to_owned()];
        };
        if !radian_host::has_usage_source(provider) {
            return vec![format!("usage: {provider} has no usage source")];
        }
        let Some(contribution) = self.contributions.get(radian_host::USAGE_ITEM) else {
            return vec![format!("usage: no snapshot yet for {provider}")];
        };
        let Some(snapshot) = contribution.usage else {
            return vec![format!("usage: no snapshot yet for {provider}")];
        };
        if snapshot.provider != provider {
            return vec![format!("usage: no snapshot yet for {provider}")];
        }
        command::usage_notices(&snapshot, contribution.stale)
    }

    /// The `/model` decision (ADR 0026 D5/D7). The loop has already refused a provider without
    /// a credential; here the app decides between queueing directly and asking first.
    pub(super) fn model_checked(
        &mut self,
        provider: String,
        model: String,
        tokens: Option<u64>,
        budget: Option<u64>,
    ) -> UpdateResult {
        match (tokens, budget) {
            (Some(t), Some(b)) if t > b => {
                // D7: the switch does not fit, so compaction comes first — with the user's yes.
                let prompt = format!(
                    "switching to {provider}/{model} needs a compact first: about {t} tokens, \
                     over the new model's budget {b}. compact and switch? [y] compact and switch \
                     [n] cancel"
                );
                // The payload's `command` is unused on this path: `confirm_command` routes
                // on `switch` first, so `y` can never run a memory command. `Purge` is the
                // one variant a refusal can never reach.
                let command = radian_host::memory::commands::MemoryCommand::List {
                    global: false,
                    project: false,
                    all: false,
                    json: false,
                };
                self.push_modal(Modal::ConfirmCommand(PendingCommand {
                    command,
                    prompt,
                    switch: Some((provider, model, b)),
                }));
                UpdateResult::dirty()
            }
            _ => self.queue_model(provider, model),
        }
    }

    /// Queues the route (D5) and says what happens next. The cache warning is the accepted
    /// cost of a switch (ADR 0026 已接受的代价).
    fn queue_model(&mut self, provider: String, model: String) -> UpdateResult {
        // ADR 0008 §2: the first route in a session that started with no model unblocks send.
        self.session.has_model = true;
        self.session.model_name = model.clone();
        self.session.provider = Some(provider.clone());
        self.session.next_model = Some(format!("{provider}/{model}"));
        self.push_notice(format!(
            "next turn: {provider}/{model}; the first turn after a switch will not hit the cache"
        ));
        UpdateResult::effect(Effect::QueueModel { provider, model })
    }

    /// The clamped `/thinking` (D11): queue it, and say so when the clamp moved the level.
    pub(super) fn thinking_checked(
        &mut self,
        requested: String,
        applied: Option<String>,
        supported: Vec<String>,
    ) -> UpdateResult {
        let Some(applied) = applied else {
            self.push_notice("this model supports no thinking level; nothing changed");
            return UpdateResult::dirty();
        };
        if applied != requested {
            self.push_notice(format!(
                "{requested} → {applied} (this model supports {})",
                supported.join("/")
            ));
        }
        self.session.next_thinking = Some(applied.clone());
        self.push_notice(format!("next turn: thinking {applied}"));
        UpdateResult::effect(Effect::QueueThinking { level: applied })
    }

    /// The pre-switch compaction finished (D7): only success queues the switch.
    pub(super) fn switch_compacted(
        &mut self,
        provider: String,
        model: String,
        result: Result<CompactionOutcome, String>,
    ) -> UpdateResult {
        match result {
            Ok(_) => {
                self.push_notice(format!("compacted; switching to {provider}/{model}"));
                self.queue_model(provider, model)
            }
            Err(error) => {
                self.push_notice(format!(
                    "the switch was not queued: the compaction failed ({error})"
                ));
                UpdateResult::dirty()
            }
        }
    }

    /// Runs `/copy [code]`: copy the last committed assistant message — or its last fenced
    /// code block — through the configured clipboard backend (the same `Effect::Copy` a
    /// mouse selection uses, so OSC 52 and `pbcopy` report the same way). A streaming entry
    /// never counts; with no committed message, no block, an oversized message, or a
    /// disabled/unsupported backend the reason is a notice and nothing is copied.
    pub(super) fn copy(&mut self, code_only: bool) -> UpdateResult {
        let Some(source) = crate::copy::last_assistant_text(&self.session.entries) else {
            self.push_notice("nothing to copy");
            return UpdateResult::dirty();
        };
        let text = if code_only {
            match crate::copy::last_fenced_block(source) {
                Some(block) => block,
                None => {
                    self.push_notice("no code block in the last assistant message");
                    return UpdateResult::dirty();
                }
            }
        } else {
            source.to_owned()
        };
        // Refuse the absurd honestly, with the reason (the mouse selection's own rule).
        if text.len() > MAX_CLIPBOARD_BYTES {
            self.push_notice(format!(
                "message too large to copy: {} bytes (the limit is {})",
                text.len(),
                MAX_CLIPBOARD_BYTES
            ));
            return UpdateResult::dirty();
        }
        // `off` keeps the selection visual for the mouse path, but an explicit `/copy`
        // answers: nothing was copied, and why.
        if self.clipboard == ClipboardMode::Off {
            self.push_notice("copy is off (tui.clipboard = \"off\"); nothing was copied");
            return UpdateResult::dirty();
        }
        if !self.clipboard.supported_on_this_platform() {
            self.push_notice(format!(
                "tui.clipboard = {:?} is not supported on this platform; nothing was copied",
                self.clipboard
            ));
            return UpdateResult::dirty();
        }
        UpdateResult::effect(Effect::Copy { text })
    }
}
