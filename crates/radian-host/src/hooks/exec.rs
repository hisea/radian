//! Running one hook command: the process, its timeout, and the async lifecycle.

use super::*;

impl CommandHooks {
    /// Runs one hook command with `stdin_json` on its stdin, inside the session sandbox
    /// when the slot holds one. A spawn failure or a timeout is a hook error (fail closed);
    /// exit codes are the caller's to interpret. Every run records one [`HookRunRecord`]
    /// (ADR 0045 §4): `tool` is the Radian tool name for the tool events, `None` for
    /// `Stop`.
    ///
    /// The child is a plain `std` process (`SandboxCommand` cannot become a Tokio command):
    /// stdin is written and the output waited on a blocking thread, and a timeout kills the
    /// whole session group (`setsid` in `spawn_piped`) before reaping it. `cancel` is the
    /// async run's handle: the pid is published there so a supersede or the session end can
    /// kill the group, and a run already cancelled when it starts is killed at once.
    pub(super) async fn execute(
        &self,
        hook: &HookCommand,
        stdin_json: &serde_json::Value,
        event: &str,
        tool: Option<&str>,
        cancel: Option<&AsyncRunHandle>,
    ) -> Result<ProcOutput, HookError> {
        self.execute_with_timeout(hook, stdin_json, event, tool, cancel, hook.timeout())
            .await
    }

    /// [`CommandHooks::execute`] with an explicit timeout: observe-only runs never inherit
    /// the async default, because `async` is inert on them (ADR 0045 §6).
    pub(super) async fn execute_with_timeout(
        &self,
        hook: &HookCommand,
        stdin_json: &serde_json::Value,
        event: &str,
        tool: Option<&str>,
        cancel: Option<&AsyncRunHandle>,
        timeout: std::time::Duration,
    ) -> Result<ProcOutput, HookError> {
        let started = Instant::now();
        let input = serde_json::to_string(stdin_json)
            .map_err(|error| HookError::new(format!("could not encode the hook input: {error}")))?;
        let mut std_child = match self.spawn_piped(hook) {
            Ok(child) => child,
            Err(error) => {
                self.record_run(
                    event,
                    tool,
                    &hook.command,
                    HookRunEnd::FailedToStart,
                    started,
                );
                return Err(HookError::new(format!(
                    "could not start the {event} hook: {error}"
                )));
            }
        };
        let pid = std_child.id();
        // The async path publishes the pid before anything can kill it, then re-checks the
        // cancel flag: a supersede that landed between registration and this spawn saw no pid
        // and only set the flag (ADR 0045 §5).
        if let Some(cancel) = cancel {
            cancel.pid.store(pid, Ordering::SeqCst);
            if cancel.cancelled.load(Ordering::SeqCst) {
                kill_tree(pid);
            }
        }
        // Dropping this future — a cancelled turn, a Core race — must kill the whole
        // process group, not leave the hook running after nobody waits for its answer
        // (ADR 0045 §4; edge case 9 of `hooks-permission-request`).
        let mut group = KillGroupOnDrop::new(pid);
        let stdin = std_child.stdin.take();
        // `select!` (not `tokio::time::timeout`) keeps the join handle, so a timeout can
        // still reap the child after killing it instead of detaching the waiter.
        let mut waiter = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            if let Some(mut stdin) = stdin {
                // A hook that never reads its stdin (e.g. `exit 2`) may already have
                // exited, closing the pipe: the write then fails with a broken pipe.
                // That is not a hook failure — the hook's own exit code still reports
                // its verdict — so a stdin write error is ignored, never fail-closed.
                // (Dropping `stdin` closes the pipe, so a reading hook sees EOF.)
                let _ = stdin.write_all(input.as_bytes());
            }
            std_child.wait_with_output()
        });
        let timeout_secs = timeout.as_secs();
        let finished = tokio::select! {
            result = &mut waiter => Some(result),
            () = tokio::time::sleep(timeout) => None,
        };
        match finished {
            Some(Ok(Ok(output))) => {
                // The child is reaped; never touch its (possibly reused) group again.
                group.disarm();
                // A process killed by a signal has no exit code; that run was cancelled
                // (the timeout path below is the timed-out end).
                let end = output
                    .status
                    .code()
                    .map_or(HookRunEnd::Cancelled, HookRunEnd::Status);
                self.record_run(event, tool, &hook.command, end, started);
                Ok(ProcOutput {
                    code: output.status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                })
            }
            Some(Ok(Err(error))) => {
                self.record_run(event, tool, &hook.command, HookRunEnd::Cancelled, started);
                Err(HookError::new(format!(
                    "the {event} hook could not finish: {error}"
                )))
            }
            Some(Err(join)) => {
                self.record_run(event, tool, &hook.command, HookRunEnd::Cancelled, started);
                Err(HookError::new(format!(
                    "the {event} hook task failed: {join}"
                )))
            }
            None => {
                kill_tree(pid);
                // The kill ends the child, so the waiter returns and reaps it; the
                // result no longer matters.
                let _ = waiter.await;
                group.disarm();
                self.record_run(event, tool, &hook.command, HookRunEnd::TimedOut, started);
                Err(HookError::new(format!(
                    "the {event} hook timed out after {timeout_secs}s"
                )))
            }
        }
    }

    /// Starts one async hook and returns at once: the process is registered as running and
    /// the run happens on the session runtime (ADR 0045 §5). The tool result or the `Stop`
    /// chain never waits for it.
    pub(super) fn spawn_async(
        &self,
        hook: &HookCommand,
        stdin_json: serde_json::Value,
        event: &str,
        tool: Option<&str>,
    ) {
        let Some((id, handle)) = self.begin_async_run(hook, event, tool) else {
            return;
        };
        let this = self.clone();
        let hook = hook.clone();
        let event = event.to_owned();
        let tool = tool.map(str::to_owned);
        self.runtime.spawn(async move {
            this.run_async(&hook, &stdin_json, &event, tool.as_deref(), &id, &handle)
                .await;
        });
    }

    /// Registers one async run and returns its id plus the handle its task shares with the
    /// supersede and session-end paths. `None` when the run does not start: the per-session
    /// cap (one warning and a `failed to start` record) or a session that has already ended.
    pub(super) fn begin_async_run(
        &self,
        hook: &HookCommand,
        event: &str,
        tool: Option<&str>,
    ) -> Option<(String, Arc<AsyncRunHandle>)> {
        if self.async_ended.load(Ordering::SeqCst) {
            // The session is over: no process starts. Not a cap failure (no warning, no
            // record) — the hook would have run for a session nobody is in.
            return None;
        }
        let key = AsyncHookKey::of(hook, event);
        let started = Instant::now();
        let mut running = self
            .async_running
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // Re-checked under the registry lock: an end that set the flag while this call waited
        // for the lock must not be overtaken by a run it would never drain.
        if self.async_ended.load(Ordering::SeqCst) {
            return None;
        }
        // THE SUPERSEDE RULE (ADR 0045 §5): a new run of the same hook kills the earlier
        // run's process group; that run delivers nothing and its record says `cancelled`. A
        // run whose process already exited (or whose group is already gone) is delivering its
        // result and is never superseded.
        running.retain(|run| {
            if run.key != key || run.handle.exited.load(Ordering::SeqCst) {
                return true;
            }
            let pid = run.handle.pid.load(Ordering::SeqCst);
            if pid != 0 && !kill_group(pid) {
                // `ESRCH`: the process was already reaped, so its result is in delivery.
                return true;
            }
            run.handle.cancelled.store(true, Ordering::SeqCst);
            false
        });
        // THE PER-SESSION CAP (ADR 0045 §5): the run that would be the ninth does not start.
        // It runs after the supersede above, so replacing the same hook never counts twice.
        // The count is of running **processes**, not of registered entries: a run whose
        // process already exited but whose result is still delivering is not a process and
        // holds no slot (its entry stays only so the supersede race can see it).
        let processes = running
            .iter()
            .filter(|run| !run.handle.exited.load(Ordering::SeqCst))
            .count();
        if processes >= MAX_ASYNC_HOOKS {
            drop(running);
            (self.warn)(&format!(
                "the {event} async hook did not start: {MAX_ASYNC_HOOKS} async hooks are \
                 already running"
            ));
            self.record_run(
                event,
                tool,
                &hook.command,
                HookRunEnd::FailedToStart,
                started,
            );
            return None;
        }
        let seq = self.async_seq.fetch_add(1, Ordering::SeqCst);
        let id = format!("hook-{}-{seq}", std::process::id());
        let handle = Arc::new(AsyncRunHandle::default());
        running.push(AsyncRun {
            running: RunningHook {
                id: id.clone(),
                event: event.to_owned(),
                tool: tool.map(str::to_owned),
                command: hook.command.clone(),
                started: Instant::now(),
            },
            key,
            handle: Arc::clone(&handle),
        });
        drop(running);
        Some((id, handle))
    }

    /// One async run on the session runtime: spawn, deliver, unregister. A run a supersede
    /// or the session end cancelled delivers nothing (ADR 0045 §5); [`Self::execute`] has
    /// already recorded its end.
    pub(super) async fn run_async(
        &self,
        hook: &HookCommand,
        stdin_json: &serde_json::Value,
        event: &str,
        tool: Option<&str>,
        id: &str,
        handle: &AsyncRunHandle,
    ) {
        let started = Instant::now();
        if handle.cancelled.load(Ordering::SeqCst) {
            // Cancelled before the process started: kill nothing, record the end.
            self.record_run(event, tool, &hook.command, HookRunEnd::Cancelled, started);
            handle.exited.store(true, Ordering::SeqCst);
            self.finish_async(id);
            return;
        }
        let output = self
            .execute(hook, stdin_json, event, tool, Some(handle))
            .await;
        // From here the result is in delivery: a supersede no longer applies.
        handle.exited.store(true, Ordering::SeqCst);
        if handle.cancelled.load(Ordering::SeqCst) || self.async_ended.load(Ordering::SeqCst) {
            self.finish_async(id);
            return;
        }
        self.deliver_async(hook, event, output).await;
        self.finish_async(id);
    }

    /// Removes one finished async hook from the running registry and wakes waiters.
    pub(super) fn finish_async(&self, id: &str) {
        self.async_running
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .retain(|run| run.running.id != id);
        self.async_signal.notify_waiters();
    }

    /// Delivers one finished async hook's result (ADR 0045 §5). Async hooks cannot change
    /// the call or the turn: their control fields are ignored with one warning. Exit 0
    /// `additionalContext` is an `inject`; exit 2 is an `inject` unless `asyncRewake` asks
    /// for a `followup` wake; every other end is one warning line (the run record is
    /// already in place). Nothing ever fails closed: the action the hook checked already
    /// happened.
    pub(super) async fn deliver_async(
        &self,
        hook: &HookCommand,
        event: &str,
        output: Result<ProcOutput, HookError>,
    ) {
        // The check and the inbox enqueue are one critical section (ADR 0045 §5):
        // `end_session` takes the same gate before it sets `async_ended`, so a result cannot
        // pass the check and then append itself to the old session after the end began.
        let _delivery = self.deliver_gate.lock().await;
        if self.async_ended.load(Ordering::SeqCst) {
            // The session is over: the result belongs to a session nobody is in.
            return;
        }
        let output = match output {
            Ok(output) => output,
            Err(error) => {
                (self.warn)(&format!("the {event} async hook failed: {error}"));
                return;
            }
        };
        match output.code {
            Some(0) => {
                let Some(parsed) = self.parsed_output(&output.stdout, event) else {
                    return;
                };
                let specific = parsed.hookSpecificOutput;
                let ignored = specific.as_ref().is_some_and(|specific| {
                    specific.permissionDecision.is_some()
                        || specific.updatedInput.is_some()
                        || specific.updatedToolOutput.is_some()
                        || specific.decision.is_some()
                }) || parsed.decision.is_some();
                if ignored {
                    (self.warn)(&format!(
                        "the {event} async hook returned permissionDecision, updatedInput, \
                         updatedToolOutput or decision; an async hook cannot change the call \
                         or the turn, so they are ignored"
                    ));
                }
                if let Some(context) = specific.and_then(|specific| specific.additionalContext) {
                    self.inject_async(format!("[{event} hook] {}", short(&context)))
                        .await;
                }
            }
            Some(2) => {
                let reason = short(&output.stderr);
                let reason = if reason.trim().is_empty() {
                    format!("the {event} hook exited with code 2")
                } else {
                    reason
                };
                if hook.async_rewake {
                    self.followup_async(reason).await;
                } else {
                    self.inject_async(reason).await;
                }
            }
            code => {
                (self.warn)(&format!(
                    "the {event} async hook exited with code {}: {}",
                    code.map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                    short(&output.stderr),
                ));
            }
        }
    }

    /// An async result waits for the next model call without opening a turn (ADR 0045 §5):
    /// the `inject` lane never wakes a driver, whether it is idle or busy when the hook
    /// finishes.
    pub(super) async fn inject_async(&self, text: String) {
        if let Err(error) = self
            .inbox
            .inject(Message::user(text), InputSource::System)
            .await
        {
            (self.warn)(&format!("could not deliver an async hook result: {error}"));
        }
    }

    /// An `asyncRewake` exit 2 takes the `followup` lane through the same wake gate as a
    /// `Stop` continuation (ADR 0045 §5): inside the gate it counts one wake; past it the
    /// result degrades to a marked `inject` with the wake-budget notice (the journal
    /// `degraded` mark, one TUI notice, herdr `wake budget`).
    pub(super) async fn followup_async(&self, text: String) {
        let message = Message::user(text);
        if self.gate.wake_allowed() {
            match self.inbox.followup(message, InputSource::System).await {
                Ok(_) => {
                    self.gate.note_wake();
                    self.degraded.store(0, Ordering::SeqCst);
                }
                Err(error) => {
                    (self.warn)(&format!("could not wake for an async hook result: {error}"));
                }
            }
        } else if self
            .inbox
            .inject_degraded(message, InputSource::System)
            .await
            .is_ok()
        {
            let degraded = self.degraded.fetch_add(1, Ordering::SeqCst) + 1;
            (self.wake)(&crate::job_notify::wake_budget_line(degraded));
        }
    }

    /// Spawns the hook with piped stdio: the sandboxed `/bin/sh -c` when the slot holds a
    /// sandbox (the same construction and environment rules the `shell` tool uses), a plain
    /// `/bin/sh -c` with the host environment otherwise. The child leads its own session
    /// (like `shell`), so a timeout can kill the whole tree and it never borrows the
    /// host's controlling terminal.
    pub(super) fn spawn_piped(&self, hook: &HookCommand) -> std::io::Result<std::process::Child> {
        // The child leads its own session (like `shell`'s `setsid`): no controlling
        // terminal is inherited, and a timeout can signal the whole tree at once.
        fn own_session(command: &mut std::process::Command) {
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                unsafe {
                    command.pre_exec(|| {
                        if libc::setsid() == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
            #[cfg(not(unix))]
            {
                let _ = command;
            }
        }
        match self.sandbox.get() {
            Some(sandbox) => {
                let mut command = match &hook.args {
                    // Exec form (ADR 0045 §4): the program, with its arguments, never
                    // through a shell. The sandbox builds it exactly like `shell`.
                    Some(args) => {
                        let args: Vec<&str> = args.iter().map(String::as_str).collect();
                        sandbox
                            .command_str(&hook.command, &args)
                            .map_err(std::io::Error::other)?
                    }
                    None => sandbox
                        .command_str("/bin/sh", &["-c", &hook.command])
                        .map_err(std::io::Error::other)?,
                };
                command.current_dir(&self.workspace);
                if !sandbox.mode().is_enforced() {
                    // The transparent backend does not filter the environment. A hook's
                    // stdin carries model-controlled tool input, so the host's API keys
                    // stay out the same way they do for `shell`.
                    command.env_clear();
                    for name in &sandbox.policy().allow_env {
                        if let Some(value) = std::env::var_os(name) {
                            command.env(name, value);
                        }
                    }
                }
                own_session(&mut command);
                command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            }
            None => {
                let mut command = match &hook.args {
                    // Exec form (ADR 0045 §4): the program starts directly, inheriting the
                    // host environment like the `/bin/sh -c` path does.
                    Some(args) => {
                        let mut command = std::process::Command::new(&hook.command);
                        command.args(args);
                        command
                    }
                    None => {
                        let mut command = std::process::Command::new("/bin/sh");
                        command.arg("-c").arg(&hook.command);
                        command
                    }
                };
                own_session(&mut command);
                command
                    .current_dir(&self.workspace)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            }
        }
    }
}
