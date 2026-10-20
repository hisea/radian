//! The session-phase event context ([`LoopContext`]): it feeds one UI event to the app
//! and runs one effect the app produced through the session facade (ADR 0023 U2/U5).
//! Split from `cli.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use super::*;

pub(super) struct LoopContext<'a> {
    /// The startup route (ADR 0026 D11): the fallback when the session has no route yet.
    pub(super) startup_route: (String, String),
    /// The catalog prices of a route, from the **warm local cache only** (ADR 0026 D7: a
    /// switch never fetches). Resolved on startup and again whenever a route change is
    /// applied, so every later call is billed at its own model's price (ADR 0027).
    pub(super) route_price: RoutePrice,
    /// The config's `[providers.<name>]` declarations (ADR 0026 D10), for `/model`'s provider
    /// completion.
    pub(super) config_providers: std::collections::BTreeMap<String, radian_host::ProviderDecl>,
    /// The applied thinking level (ADR 0026 D11), kept in step with the app, for `/thinking`'s
    /// current-value marker.
    pub(super) current_thinking: String,
    /// The configured theme name, for `/theme`'s current-value marker.
    pub(super) current_theme: Option<String>,
    /// The request's `--max-tokens`: a level whose budget cannot fit is not offered and is
    /// clamped past (review B1).
    pub(super) thinking_max_tokens: u32,
    pub(super) workspace: &'a Path,
    pub(super) runtime: &'a tokio::runtime::Runtime,
    /// The UI's one handle on the session (ADR 0023 U5): commands go through it, and it
    /// re-validates the capability the UI declared.
    pub(super) session: &'a Arc<dyn radian_host::SessionFacade>,
    pub(super) memory: &'a Option<MemoryService>,
    pub(super) host: &'a SessionHost,
    pub(super) tx: &'a Sender<UiEvent>,
    /// The pending queue (ADR 0023 U7): the loop reads the head and the count when it is told
    /// something changed, and the facade answers it.
    pub(super) queue: &'a radian_host::SessionQueue,
    /// The signal that stopped the loop, if any: it decides the reason line (and, in `main`,
    /// the exit code).
    pub(super) signal: Option<i32>,
    /// How a selection copy reaches the clipboard (`tui.clipboard`), read once from the app
    /// when the context is built: the mode cannot change mid-run.
    pub(super) clipboard: ClipboardMode,
    /// Whether a `Bell` already rang in this frame: bell-class events that share a frame
    /// merge into one ring, and the loop re-arms the flag for the next frame.
    pub(super) bell_rang: bool,
    /// The herdr reporter (ADR 0034), when running inside a herdr pane.
    pub(super) herdr: Option<HerdrReporter>,
    /// The sandbox the user's `!`/`!!` commands run in (ADR 0033): the coding tools' own,
    /// cloned. `None` when the session mounts no `shell`.
    pub(super) bash_sandbox: Option<radian_sandbox::Sandbox>,
    /// The running `!` command's cancellation token (Esc, ADR 0033).
    pub(super) bash_cancel: Option<agent_core::CancellationToken>,
    /// The opencode-go usage gate (ADR 0027): the UI flips it when `/model`
    /// switches the current provider. `None` when there is no key or the period is off.
    pub(super) usage_gate: Option<tokio::sync::watch::Sender<bool>>,
    /// The chatgpt usage gate (ADR 0026), same mechanism, flipped to
    /// `route.provider == "chatgpt"` instead.
    pub(super) chatgpt_usage_gate: Option<tokio::sync::watch::Sender<bool>>,
    /// Whether the first-launch model write-back already ran (ADR 0008 §2): set after the
    /// first `/model` in a session that started without one, so later switches never write.
    pub(super) model_writeback_done: bool,
}

impl LoopContext<'_> {
    /// Feeds one event to the app. Returns whether the frame is dirty; the effects it produced
    /// are appended to `effects`.
    pub(super) fn handle(
        &mut self,
        event: UiEvent,
        app: &mut App,
        effects: &mut Vec<Effect>,
    ) -> bool {
        // ADR 0027: the loop is the only place that reads the wall clock for the
        // footer's `speed`/`elapsed`; the timer itself is pure.
        app.set_now(std::time::Instant::now());
        // ADR 0027: a `/model` switch arrives as a model `ConfigChanged` at the turn
        // boundary. Flip the usage gate then: away from opencode-go the contributor stops
        // refreshing (no network) and its row hides; back to it, one immediate refresh.
        if let UiEvent::Runtime(radian_session::RuntimeEvent::ConfigChanged { change, .. }) = &event
            && let Some(route) = radian_host::ModelRoute::from_value(change)
        {
            if let Some(gate) = &self.usage_gate {
                let _ = gate.send(route.provider == "opencode-go");
            }
            if let Some(gate) = &self.chatgpt_usage_gate {
                let _ = gate.send(chatgpt_usage_active(&route.provider));
            }
        }
        let result = match event {
            UiEvent::Agent(event) => app.update(AppInput::Event(event)),
            UiEvent::Runtime(event) => {
                // ADR 0027 + ADR 0026 D5: the driver applies a queued route at a
                // turn boundary and tells us through `ConfigChanged`. From now on every call
                // is the new model's, so the price that prices it changes here — earlier
                // calls keep the fee they already accumulated.
                if let Some(price) = price_for_applied_change(&event, &self.route_price) {
                    app.set_model_price(price);
                }
                app.update(AppInput::Runtime(event))
            }
            UiEvent::Pending => {
                // ADR 0023 U7: the UI shows the queue's head. Reading it here is what makes a
                // waiter that gave up (a cancelled turn) disappear immediately.
                let head = self.queue.head();
                app.update(AppInput::Pending(head.map(Box::new), self.queue.len()))
            }
            UiEvent::Command(input) => app.update(input),
            UiEvent::Input(InputEvent::Key(key)) => app.on_key(key),
            UiEvent::Input(InputEvent::Paste(text)) => app.update(AppInput::Paste(text)),
            UiEvent::Input(InputEvent::Resize { rows, cols }) => {
                app.update(AppInput::Resize { rows, cols })
            }
            UiEvent::Input(InputEvent::MouseScroll(direction)) => app.mouse_scroll(direction),
            UiEvent::Input(InputEvent::MouseDown { row, col }) => {
                app.update(AppInput::MouseDown { row, col })
            }
            UiEvent::Input(InputEvent::MouseDrag { row, col }) => {
                app.update(AppInput::MouseDrag { row, col })
            }
            UiEvent::Input(InputEvent::MouseUp { row, col }) => {
                app.update(AppInput::MouseUp { row, col })
            }
            UiEvent::Dirty => return apply_dirty(app),
            UiEvent::Signal(signal) => {
                // Leave through the ordinary clean-exit path: the terminal is restored, the
                // driver is stopped, and `main` prints the reason (a signal exits
                // 128+signal). No last frame: after a SIGHUP the terminal may already be gone.
                self.signal = Some(signal);
                app.quit = true;
                return false;
            }
            UiEvent::TerminalGone => {
                // The terminal went away (the pty master closed, the ssh session dropped):
                // leave through the ordinary clean-exit path. No signal was received, so the
                // exit code is the ordinary 0.
                exit_trace("terminal-gone");
                app.quit = true;
                return false;
            }
            UiEvent::DriverStopped(end) => {
                // The driver stopped: a fatal error (ADR 0001: the journal may be broken, so
                // no record is written), a panic, or a graceful close. The session is over.
                // The TUI leaves `Running`, names the reason in the status bar and refuses
                // `/fork` (ADR 0022 §1's "idle session" no longer exists). It stays so the
                // reason is readable; the user quits with the ordinary keys.
                let reason = match &end {
                    radian_host::DriverEnd::Ok => "the driver stopped".to_string(),
                    radian_host::DriverEnd::Failed(error) => format!("driver failed: {error}"),
                    radian_host::DriverEnd::Panicked(message) => {
                        format!("driver panicked: {message}")
                    }
                };
                app.session.session_ended = Some(reason.clone());
                app.session.pending_fork = None;
                app.session.projection.set_turn(radian_tui::TurnState::Idle);
                app.session.note = Some(reason.clone());
                app.session.entries.push(radian_tui::Entry::new(
                    radian_tui::EntryKind::Notice,
                    format!("the session ended: {reason}"),
                ));
                return true;
            }
        };
        // The completion menu's current-value markers read these; keep them in step with the
        // app (the footer's `reasoning` item is the same value).
        if let Some(level) = app.session.thinking_level.clone() {
            self.current_thinking = level;
        }
        effects.extend(result.effects);
        result.dirty
    }

    /// Runs one effect. Its result is an event on the channel (ADR 0023 U2), so the caller
    /// picks it up on the next round.
    pub(super) fn execute(&mut self, effect: Effect) -> Result<(), String> {
        match effect {
            // ADR 0023 U5: the UI asks the session through its facade; the facade declares
            // what it can do and re-checks when the command runs.
            Effect::SendFollowup(text) => {
                self.command(radian_host::SessionCommand::SendFollowup(text))
            }
            Effect::Steer(text) => self.command(radian_host::SessionCommand::Steer(text)),
            Effect::Cancel => {
                // Ctrl-C cancels the turn; it also stops a running `!` (ADR 0033) so there is
                // no orphan process group.
                if let Some(cancel) = self.bash_cancel.take() {
                    cancel.cancel();
                }
                self.command(radian_host::SessionCommand::Cancel)
            }
            Effect::Quit => Ok(()),
            // ADR 0033: run the `!`/`!!` command through radian-coding's sandboxed executor
            // (`run_user_bash`, the shell tool's own path), off the loop so Esc can cancel it.
            Effect::RunBash {
                command,
                exclude_from_context,
            } => {
                let tx = self.tx.clone();
                let sandbox = self.bash_sandbox.clone();
                let workspace = self.workspace.to_path_buf();
                let cancel = agent_core::CancellationToken::new();
                self.bash_cancel = Some(cancel.clone());
                self.runtime.spawn(async move {
                    let result = match sandbox {
                        Some(sandbox) => {
                            radian_coding::run_user_bash(&workspace, &sandbox, &command, &cancel)
                                .await
                        }
                        None => Err("this session has no shell; `!` needs a \
                                     shell-capable tier"
                            .to_owned()),
                    };
                    let _ = tx.send(UiEvent::Command(AppInput::BashDone {
                        command,
                        exclude_from_context,
                        result,
                    }));
                });
                Ok(())
            }
            // ADR 0033 + ADR 0011: the `!` message is injected without waking an idle driver.
            Effect::Inject(message) => self.command(radian_host::SessionCommand::Inject(message)),
            // ADR 0033: the durable audit record, for `!` and `!!` alike.
            Effect::AuditUserBash {
                command,
                exclude_from_context,
                exit_code,
                cancelled,
            } => {
                self.session.record_user_bash(&radian_host::UserBashRecord {
                    command,
                    exclude_from_context,
                    exit_code,
                    cancelled,
                });
                Ok(())
            }
            Effect::CancelBash => {
                if let Some(cancel) = self.bash_cancel.take() {
                    cancel.cancel();
                }
                Ok(())
            }
            // ADR 0008 revision: `/theme` loads the theme and writes `tui.theme` back. Both
            // are I/O, so they run here and the result comes back as `Input::ThemeReady`.
            Effect::Theme { name } => {
                let outcome = radian_tui::theme::switch(
                    &name,
                    radian_tui::theme::Theme::no_color_env(),
                    radian_host::themes_dir().as_deref(),
                    &self.host.theme_colors,
                    self.host.theme_write_path.as_deref(),
                    self.host.config_read_only,
                );
                if outcome.theme.is_some() {
                    self.current_theme = Some(name);
                }
                self.send(AppInput::ThemeReady {
                    theme: outcome.theme.map(Box::new),
                    notices: outcome.notices,
                });
                Ok(())
            }
            // ADR 0026 D9: the in-session switch. The facade applies it to the hooks (which
            // audit it) and queues the durable `ConfigChanged` for the next turn boundary.
            Effect::SetApprovalPolicy { on } => self
                .session
                .set_approval(if on {
                    radian_permissions::ApprovalPolicy::Auto
                } else {
                    radian_permissions::ApprovalPolicy::OnAsk
                })
                .map_err(|refused| {
                    format!(
                        "cannot switch auto: {} ({})",
                        refused.reason,
                        refused.capability.as_deref().unwrap_or("capability")
                    )
                }),
            // ADR 0038 slice 2: the run-time tier switch. The facade rebuilds the profile and
            // the sandbox (a failed probe is a refusal that changes nothing) and queues the
            // durable `ConfigChanged`; when the user chose "remember", the loop also writes the
            // per-project record. The answer comes back as `Input::AccessApplied`.
            Effect::SetAccess { tier, remember } => {
                match self.session.set_access(tier.preset()) {
                    Ok(()) => {
                        if remember
                            && let Err(error) =
                                radian_host::access::remember_for(self.workspace, tier.preset())
                        {
                            self.send(AppInput::SessionDone(vec![format!(
                                "access: switched, but could not remember it for this project: \
                                 {error}"
                            )]));
                        }
                        self.send(AppInput::AccessApplied {
                            tier,
                            remember,
                            result: Ok(()),
                        });
                    }
                    Err(refused) => {
                        self.send(AppInput::AccessApplied {
                            tier,
                            remember,
                            result: Err(refused.reason),
                        });
                    }
                }
                Ok(())
            }
            Effect::AnswerRequest {
                request_id,
                response,
            } => {
                // ADR 0023 U5: the answer goes through the facade, which re-validates the
                // `AnswerRequest` capability. A late answer is silently ignored there.
                match self.session.answer(request_id, response) {
                    Ok(()) => Ok(()),
                    Err(refused) if refused.capability.is_some() => {
                        self.send(AppInput::CommandRefused(refused));
                        Ok(())
                    }
                    Err(refused) => Err(refused.reason),
                }
            }
            // `@` expansion and completion read files, so they are effects; their results go
            // back through the channel as `Command` events.
            Effect::ExpandMentions { text } => {
                let result = radian_tui::mention::expand(&text, self.workspace);
                self.send(AppInput::MentionsExpanded(result));
                Ok(())
            }
            Effect::CompleteMention { text, cursor } => {
                let result = radian_tui::mention::complete(&text, cursor, self.workspace);
                self.send(AppInput::CompletionReady(result));
                Ok(())
            }
            Effect::FetchCompletions(fetch) => {
                // A level that needs the host: list the candidates here (I/O) and send them back as
                // `Input::CompletionsReady` (ADR 0023 U3: `update` never does I/O).
                let candidates = match &fetch {
                    radian_tui::command::Fetch::MemoryRecords { command, .. } => {
                        memory_record_candidates(self.memory, command)
                    }
                    radian_tui::command::Fetch::Themes { .. } => {
                        // The current theme is marked, so the menu says what a pick changes.
                        let current = self.current_theme.clone();
                        radian_tui::command::theme_candidates(radian_host::themes_dir().as_deref())
                            .into_iter()
                            .map(|mut candidate| {
                                if Some(&candidate.insert) == current.as_ref() {
                                    candidate.current = true;
                                }
                                candidate
                            })
                            .collect()
                    }
                    radian_tui::command::Fetch::ModelRoute { .. } => {
                        // Mark the route in effect (the slot, or the startup route before it).
                        let (current_provider, current_model) = self
                            .session
                            .model_route()
                            .unwrap_or_else(|| self.startup_route.clone());
                        model_route_candidates(
                            Some(current_provider.as_str()),
                            &self.config_providers,
                        )
                        .into_iter()
                        .map(|mut candidate| {
                            let is_current = candidate.insert
                                == format!("{current_provider}/{current_model}")
                                || candidate.insert == format!("{current_provider}/");
                            if is_current {
                                candidate.current = true;
                            }
                            candidate
                        })
                        .collect()
                    }
                    radian_tui::command::Fetch::ThinkingLevels { .. } => {
                        // D11: only the levels the current model supports. Each row names the
                        // wire value it sends (`enabled:4096` for Anthropic, `high` for
                        // openai-compatible), and the applied level is marked.
                        let (route_provider, route_model) = self
                            .session
                            .model_route()
                            .unwrap_or_else(|| self.startup_route.clone());
                        let provider_ref =
                            provider_ref_for(&route_provider, &self.config_providers);
                        radian_tui::command::thinking_candidates(
                            &provider_ref,
                            &route_model,
                            self.thinking_max_tokens,
                            &self.current_thinking,
                        )
                    }
                    // `/skill:`'s candidates: the discovered skill names, including the ones
                    // with `disable-model-invocation: true` — that flag hides a skill from the
                    // *model*, not from the user (ADR 0025 S3; pi registers every loaded skill
                    // as a `skill:<name>` command).
                    radian_tui::command::Fetch::SkillNames { .. } => {
                        radian_host::completion_candidates(self.session.skills())
                            .into_iter()
                            .map(|(name, description)| radian_tui::command::Candidate {
                                insert: name,
                                description,
                                current: false,
                            })
                            .collect()
                    }
                    // `/logout`'s candidates: every stored credential (D3).
                    radian_tui::command::Fetch::LoggedInProviders { .. } => {
                        match radian_host::list_auth() {
                            Ok(entries) => entries
                                .into_iter()
                                .map(|entry| radian_tui::command::Candidate {
                                    insert: entry.provider_id,
                                    description: match entry.kind.as_str() {
                                        "api_key" if entry.usable => "stored api key".to_owned(),
                                        "api_key" => "stored (unusable here)".to_owned(),
                                        other => format!("stored {other} entry"),
                                    },
                                    current: false,
                                })
                                .collect(),
                            Err(error) => {
                                vec![radian_tui::command::Candidate {
                                    insert: String::new(),
                                    description: format!("cannot read auth.json: {error}"),
                                    current: false,
                                }]
                            }
                        }
                    }
                };
                self.send(AppInput::CompletionsReady { fetch, candidates });
                Ok(())
            }
            Effect::RunMemoryCommand { command, confirmed } => {
                let result = match self.memory {
                    Some(service) => {
                        let json = command.json();
                        match radian_host::memory::commands::execute(
                            service,
                            command.clone(),
                            confirmed,
                            json,
                        ) {
                            Ok(output) => match output.needs_confirmation {
                                Some(prompt) => MemoryResult::NeedsConfirmation { command, prompt },
                                None => MemoryResult::Output(radian_tui::command::output_notices(
                                    &output,
                                )),
                            },
                            Err(error) => MemoryResult::Failed(error.message),
                        }
                    }
                    None => {
                        MemoryResult::Failed("memory is disabled (no RADIAN_HOME or HOME)".into())
                    }
                };
                self.send(AppInput::MemoryCommandDone(result));
                Ok(())
            }
            // `/skill:<name> [args]` (ADR 0025 S3): the host reads the body and folds it into
            // the message; the app decides follow-up vs steer.
            Effect::ExpandSkill { name, args, steer } => {
                let result = self.session.expand_skill(&name, &args);
                self.send(AppInput::SkillExpanded { result, steer });
                Ok(())
            }
            // `/skills` (ADR 0025 S3): the discovered skills as notices.
            Effect::ListSkills => {
                let notices = radian_host::notices(self.session.skills());
                self.send(AppInput::SessionDone(notices));
                Ok(())
            }
            // ADR 0037 decision 4: `/jobs` lists the running background jobs, or stops
            // one/all. The registry lives in the host, so the loop reads it here and shows
            // notices.
            Effect::Jobs(request) => {
                let notices = match request {
                    radian_tui::command::JobsRequest::List => {
                        let jobs = self.session.jobs();
                        radian_tui::command::jobs_list_notices(&jobs)
                    }
                    radian_tui::command::JobsRequest::Kill(id) => {
                        match self.session.kill_job(&id) {
                            Ok(()) => {
                                vec![format!("stopping {id} (SIGTERM, then SIGKILL after 2s)")]
                            }
                            Err(error) => vec![error],
                        }
                    }
                    radian_tui::command::JobsRequest::KillAll => {
                        let running = self
                            .session
                            .jobs()
                            .iter()
                            .filter(|job| job.state.is_running())
                            .count();
                        self.session.kill_all_jobs();
                        vec![format!("stopping {running} background job(s)")]
                    }
                };
                self.send(AppInput::SessionDone(notices));
                Ok(())
            }
            // ADR 0031 T3: apply the startup trust choice through the facade (it writes
            // `trust.json` and journals `ProjectTrustChanged`), then show the new state.
            Effect::SetTrust { choice } => {
                match self.session.record_trust(choice) {
                    Ok(setup) => self.send(AppInput::TrustResolved(setup)),
                    Err(error) => {
                        self.send(AppInput::CommandRefused(radian_host::CommandRefused {
                            command: "trust".to_owned(),
                            reason: error,
                            capability: None,
                        }))
                    }
                }
                Ok(())
            }
            Effect::Compact { instructions } => {
                // The facade runs the request between turns and reports the outcome here; the
                // result becomes an event like every other effect result.
                let tx = self.tx.clone();
                self.session.compact(
                    instructions,
                    Box::new(move |result| {
                        let _ = tx.send(UiEvent::Command(AppInput::CompactDone(result)));
                    }),
                );
                Ok(())
            }
            Effect::ModelSwitch { provider, model } => {
                // D6: the target needs a credential. `resolve_key` reads `auth.json` (file I/O,
                // fine in the loop): without one the switch dies here with the `/login` hint.
                // `resolve_key` only resolves an `api_key` entry; `chatgpt`'s login is an `oauth`
                // entry (ADR 0026), so `has_login` - the presence check both shapes
                // share, as at startup and in `RouteContext::args_for` - also counts here.
                let checked = match radian_host::auth::resolve_key(&provider) {
                    Ok(found) if found.is_some() || radian_host::has_login(&provider) => {
                        // D7: `B'` from the warm catalog, `T` from the projection's last report.
                        let budget = self.session.budget_for_route(&provider, &model);
                        let tokens = self
                            .session
                            .projection()
                            .budget()
                            .map(|report| report.projected_tokens);
                        AppInput::ModelChecked {
                            provider,
                            model,
                            tokens,
                            budget,
                        }
                    }
                    Ok(_) => {
                        self.send(AppInput::CommandRefused(radian_host::CommandRefused {
                            command: format!("/model {provider}/{model}"),
                            reason: format!(
                                "no credential for `{provider}`: run /login {provider}"
                            ),
                            capability: None,
                        }));
                        return Ok(());
                    }
                    Err(error) => {
                        self.send(AppInput::CommandRefused(radian_host::CommandRefused {
                            command: format!("/model {provider}/{model}"),
                            reason: error.to_string(),
                            capability: None,
                        }));
                        return Ok(());
                    }
                };
                self.send(checked);
                Ok(())
            }
            Effect::CompactForSwitch {
                provider,
                model,
                budget,
            } => {
                // D7: the same compaction path as `/compact`, targeting the new model's B'.
                let tx = self.tx.clone();
                self.session.compact_with_budget(
                    None,
                    Some(budget),
                    Box::new(move |result| {
                        let _ = tx.send(UiEvent::Command(AppInput::SwitchCompacted {
                            provider,
                            model,
                            result,
                        }));
                    }),
                );
                Ok(())
            }
            Effect::SetThinking { level } => {
                // D11: clamp the requested level against the **current** route's map; the
                // route comes from the slot (a resumed change or a queued switch included).
                let (route_provider, route_model) = self
                    .session
                    .model_route()
                    .unwrap_or_else(|| self.startup_route.clone());
                let provider_ref = provider_ref_for(&route_provider, &self.config_providers);
                let supported: Vec<String> = radian_host::supported_thinking_levels_ref_with(
                    &provider_ref,
                    &route_model,
                    Some(self.thinking_max_tokens),
                )
                .into_iter()
                .map(str::to_owned)
                .collect();
                let map = radian_host::thinking_map_for_ref(&provider_ref, &route_model);
                let applied =
                    radian_host::clamp_thinking_with(map, &level, Some(self.thinking_max_tokens))
                        .map(|(level, _)| level.to_string());
                self.send(AppInput::ThinkingChecked {
                    requested: level,
                    applied,
                    supported,
                });
                Ok(())
            }
            Effect::QueueThinking { level } => {
                // D11: the change is the level name only — no key, no header values.
                self.session
                    .queue_config(serde_json::json!({ "thinking": level }));
                Ok(())
            }
            Effect::QueueModel { provider, model } => {
                // D5: the route is the whole change — no key, no header values.
                self.session
                    .queue_config(radian_host::ModelRoute::new(&provider, &model).to_value());
                // ADR 0008 §2: the first `/model` in a session that started without one
                // writes `[model] provider`/`model` back to the config file (same rules as
                // `/theme`, including the read-only refusal). A later switch never writes.
                // A failed write still uses the chosen model: the notice says so.
                if self.host.started_without_model && !self.model_writeback_done {
                    self.model_writeback_done = true;
                    match self.host.theme_write_path.as_deref() {
                        Some(path) => {
                            match radian_host::write_model_route(
                                path,
                                self.host.config_read_only,
                                &provider,
                                &model,
                            ) {
                                Ok(saved) => self.send(AppInput::SessionDone(vec![format!(
                                    "model saved to {}",
                                    saved.display()
                                )])),
                                Err(error) => self.send(AppInput::SessionDone(vec![format!(
                                    "model applies for this session; could not save: {error}"
                                )])),
                            }
                        }
                        None => self.send(AppInput::SessionDone(vec![
                            "model applies for this session; there is no configuration file \
                             location to save it"
                                .to_owned(),
                        ])),
                    }
                }
                Ok(())
            }
            Effect::Session(request) => {
                let notices = handle_session_request(self.host, self.runtime, request);
                self.send(AppInput::SessionDone(notices));
                Ok(())
            }
            Effect::OpenPicker { query } => {
                self.send(load_picker(self.host, query));
                Ok(())
            }
            // `/login`'s verification request (ADR 0026 D3): one GET, off the UI thread; the
            // answer comes back as `Input::LoginVerified`. The key rides in the closure only —
            // it is never logged here.
            Effect::LoginVerify {
                name,
                protocol,
                base_url,
                key,
            } => {
                let protocol = radian_host::Protocol::from_kind(protocol)
                    .unwrap_or(radian_host::Protocol::OpenAiCompatible);
                let tx = self.tx.clone();
                self.runtime.spawn(async move {
                    let result = radian_host::login::verify(protocol, &base_url, &key).await;
                    let _ = tx.send(UiEvent::Command(AppInput::LoginVerified { name, result }));
                });
                Ok(())
            }
            // The save: the only place `/login` writes, and it is `auth.json` (D3 — the
            // config file stays read-only).
            Effect::LoginSave { name, key, env } => {
                let env: std::collections::BTreeMap<String, String> = env.into_iter().collect();
                let result = radian_host::AuthStore::open()
                    .and_then(|mut store| store.upsert_api_key(&name, &key, &env))
                    .map_err(|error| error.to_string());
                if result.is_ok() {
                    // The session's redaction list must know the new key from now on
                    //: the panic log and an export read it live.
                    refresh_secrets(&self.host.secrets);
                }
                self.send(AppInput::LoginSaved { name, result });
                Ok(())
            }
            // Sign in with ChatGPT, browser flow (ADR 0026): bind the listener,
            // report the URL right away (`LoginOAuthUrlReady`) so a remote/headless session —
            // or a browser the `open`/`xdg-open` guess failed to launch — has something to
            // copy, then wait for the redirect and exchange. The whole thing is one spawned
            // task so a slow or abandoned browser step cannot block the UI loop; it ends on
            // the same `LoginSaved` event the API-key path does.
            Effect::LoginOAuthBrowser => {
                let tx = self.tx.clone();
                let secrets = Arc::clone(&self.host.secrets);
                self.runtime.spawn(async move {
                    let endpoints = radian_host::OAuthEndpoints::default();
                    let (login, listener) = match radian_host::BrowserLogin::start(&endpoints).await
                    {
                        Ok(pair) => pair,
                        Err(error) => {
                            let _ = tx.send(UiEvent::Command(AppInput::LoginSaved {
                                name: radian_host::CHATGPT_PROVIDER_ID.to_owned(),
                                result: Err(error.to_string()),
                            }));
                            return;
                        }
                    };
                    let _ = tx.send(UiEvent::Command(AppInput::LoginOAuthUrlReady {
                        url: login.authorize_url.clone(),
                    }));
                    radian_host::oauth_chatgpt::try_open_browser(&login.authorize_url);
                    let outcome = login
                        .finish(
                            &endpoints,
                            listener,
                            radian_host::oauth_chatgpt::CALLBACK_TIMEOUT,
                        )
                        .await;
                    let result = store_oauth_outcome(outcome, &secrets);
                    let _ = tx.send(UiEvent::Command(AppInput::LoginSaved {
                        name: radian_host::CHATGPT_PROVIDER_ID.to_owned(),
                        result,
                    }));
                });
                Ok(())
            }
            // Sign in with ChatGPT, device-code flow: ask for a code, report it
            // (`LoginOAuthDeviceReady`), then poll in the same spawned task.
            Effect::LoginOAuthDevice => {
                let tx = self.tx.clone();
                let secrets = Arc::clone(&self.host.secrets);
                self.runtime.spawn(async move {
                    let endpoints = radian_host::OAuthEndpoints::default();
                    let start = match radian_host::oauth_device_start(&endpoints).await {
                        Ok(start) => start,
                        Err(error) => {
                            let _ = tx.send(UiEvent::Command(AppInput::LoginSaved {
                                name: radian_host::CHATGPT_PROVIDER_ID.to_owned(),
                                result: Err(error.to_string()),
                            }));
                            return;
                        }
                    };
                    let _ = tx.send(UiEvent::Command(AppInput::LoginOAuthDeviceReady {
                        user_code: start.user_code.clone(),
                        verification_url: radian_host::oauth_chatgpt::DEVICE_VERIFICATION_URL
                            .to_owned(),
                    }));
                    // 5 minutes at the server's own interval (floored at
                    // `DEVICE_POLL_MIN_INTERVAL`), matching the browser flow's timeout.
                    let max_polls = (radian_host::oauth_chatgpt::CALLBACK_TIMEOUT.as_secs()
                        / start.interval.as_secs().max(1))
                    .max(1) as u32;
                    let outcome = radian_host::oauth_poll_device_until_done(
                        &endpoints,
                        &start,
                        start.interval,
                        max_polls,
                    )
                    .await;
                    let result = store_oauth_outcome(outcome, &secrets);
                    let _ = tx.send(UiEvent::Command(AppInput::LoginSaved {
                        name: radian_host::CHATGPT_PROVIDER_ID.to_owned(),
                        result,
                    }));
                });
                Ok(())
            }
            // `/logout` after the `y`: remove the entry, everything else byte-for-byte (D3).
            Effect::Logout { provider } => {
                let result = radian_host::AuthStore::open()
                    .and_then(|mut store| store.remove(&provider))
                    .map_err(|error| error.to_string());
                self.send(AppInput::LogoutDone { provider, result });
                Ok(())
            }
            // The clipboard write is I/O, so it lives here (ADR 0023 U3); its result is the
            // honest status-bar notice. OSC 52 has no receipt, so success says only what
            // Radian did ("via OSC 52"); `pbcopy` can fail, and says so.
            Effect::Copy { text } => {
                let notice = match self.clipboard {
                    ClipboardMode::Off => return Ok(()),
                    ClipboardMode::Osc52 => match write_osc52(&text) {
                        Ok(()) => format!("copied {} chars (via OSC 52)", text.chars().count()),
                        Err(error) => format!("clipboard write failed: {error}"),
                    },
                    ClipboardMode::PbCopy => match run_pbcopy(&text) {
                        Ok(()) => format!("copied {} chars (via pbcopy)", text.chars().count()),
                        Err(error) => format!("pbcopy failed: {error}"),
                    },
                };
                self.send(AppInput::ClipboardDone(notice));
                Ok(())
            }
            // One `\x07` per frame: bell-class events that share a frame (an approval
            // arriving as a turn ends) ring once — the first `Bell` of the frame wins and
            // the loop re-arms the flag for the next one.
            Effect::Bell => {
                if !self.bell_rang {
                    self.bell_rang = true;
                    write_bel();
                }
                Ok(())
            }
        }
    }

    /// Runs one session command. A refusal the facade reports (an undeclared capability) is a
    /// notice, not a fatal loop error; an inbox rejection keeps its old path.
    pub(super) fn command(&mut self, command: radian_host::SessionCommand) -> Result<(), String> {
        match self.session.command(command) {
            Ok(()) => Ok(()),
            Err(refused) if refused.capability.is_some() => {
                self.send(AppInput::CommandRefused(refused));
                Ok(())
            }
            Err(refused) => Err(refused.reason),
        }
    }

    /// Sends an effect's result back as an event, so it is handled like every other input.
    pub(super) fn send(&self, input: AppInput) {
        let _ = self.tx.send(UiEvent::Command(input));
    }
}
