//! The transcript: the startup block, the width the renderer used, the committed messages
//! (`push_message`, with the live block it replaces), the live events that feed it
//! (`apply_event`, `set_live`, `drop_live`), and the runtime facts that are not agent events
//! (`apply_runtime_event`). The two scan counters are test-only and live here with the tests.

use super::*;

impl App {
    // ---------- in-transcript search (Phase 5) ----------

    /// Records the transcript width the renderer used. A width change re-wraps the transcript,
    /// so an open find box recomputes its matches — once per resize, never per frame.
    pub fn set_transcript_width(&mut self, width: usize) {
        if self.session.transcript_width == width {
            return;
        }
        self.session.transcript_width = width;
        // The wordmark drops to its one-line fallback below a width; rebuilding is cheap and
        // only happens when the width actually changed.
        if self.session.startup_screen.is_some() {
            self.rebuild_startup_entries();
        }
        if self.find_state().is_some() {
            self.find_recompute();
        }
    }

    /// Sets the startup screen and builds its leading entry block.
    pub fn set_startup(&mut self, startup: StartupScreen) {
        self.session.startup_screen = Some(startup);
        self.rebuild_startup_entries();
    }

    /// `Ctrl-P` (the `toggle_startup` action): fold or unfold the startup information.
    pub fn toggle_startup(&mut self) -> UpdateResult {
        let Some(startup) = self.session.startup_screen.as_mut() else {
            return UpdateResult::none();
        };
        startup.expanded = !startup.expanded;
        self.rebuild_startup_entries();
        UpdateResult::dirty()
    }

    /// The leading startup block, rebuilt from [`StartupScreen`]: the wordmark (the wide form
    /// when it fits the current width), the collapsed line or the full lines, the safety
    /// lines (always shown), and the Tip.
    fn rebuild_startup_entries(&mut self) {
        let Some(startup) = self.session.startup_screen.clone() else {
            self.session
                .entries
                .drain(0..self.session.startup_len.min(self.session.entries.len()));
            self.session.startup_len = 0;
            return;
        };
        let mut block: Vec<Entry> = startup
            .banner_for_width(self.session.transcript_width)
            .into_iter()
            .map(|line| Entry::new(EntryKind::Banner, line))
            .collect();
        if startup.expanded {
            block.push(Entry::new(
                EntryKind::Startup,
                "- startup information (Ctrl-P to fold)",
            ));
            for line in &startup.lines {
                block.push(Entry::new(EntryKind::Startup, line.clone()));
            }
        } else {
            block.push(Entry::new(
                EntryKind::Startup,
                format!("+ {}   (Ctrl-P to expand)", startup.summary),
            ));
        }
        for line in &startup.safety {
            block.push(Entry::new(EntryKind::Warning, line.clone()));
        }
        if let Some(tip) = &startup.tip {
            block.push(Entry::new(EntryKind::Tip, tip.clone()));
        }
        let block_len = block.len();
        let replace = self.session.startup_len.min(self.session.entries.len());
        self.session.entries.splice(0..replace, block);
        self.session.startup_len = block_len;
    }

    /// ADR 0017: a failed turn is shown in the transcript; the session stays open.
    pub(super) fn apply_runtime_event(&mut self, event: &RuntimeEvent) -> UpdateResult {
        if let RuntimeEvent::TurnFinished { .. } = event {
            // The turn boundary is on disk: a `/fork` that was waiting for it can run now.
            self.session.turn_committed = true;
            if self.session.pending_fork.take().is_some() {
                return UpdateResult::effect(Effect::Session(command::SessionOutcome::Fork));
            }
        }
        if let RuntimeEvent::ConfigChanged { change, .. } = event {
            // The driver applied the queued change: the status bar's `next:` promise is done.
            self.session.next_model = None;
            self.session.next_thinking = None;
            // Owner addendum (ADR 0027 slice ①): an applied model switch must follow
            // through to the footer's `model` item, or it keeps showing the startup model. The
            // session's model facts follow the applied route the way startup leaves them: the
            // model id, the provider from the route, and `model_display = None` so
            // `{display_name}` falls back to the id. Startup passes `None` for the display name
            // too (`cli.rs`'s `set_footer_facts`); no catalog name is resolved today.
            if let Some(route) = radian_host::ModelRoute::from_value(change) {
                self.session.model_name = route.model;
                self.session.provider = Some(route.provider);
                self.session.model_display = None;
            }
        }
        if let RuntimeEvent::BackgroundJobStarted { job_id, .. } = event {
            // ADR 0027 slice ③ item 5: the footer's `jobs` item reads this
            // session's running jobs. Stamp the loop's clock; the age ticks with the existing
            // footer refresh, so no new timer exists for it.
            self.session.running_jobs.retain(|job| job.id != *job_id);
            self.session.running_jobs.push(crate::app::RunningJob {
                id: job_id.clone(),
                started: self.now,
            });
            return UpdateResult::dirty();
        }
        if let RuntimeEvent::BackgroundJobSettled { job_id, .. } = event {
            self.session.running_jobs.retain(|job| job.id != *job_id);
            return UpdateResult::dirty();
        }
        if let RuntimeEvent::TurnFailed { message, .. } = event {
            self.session.entries.push(Entry::new(
                EntryKind::Notice,
                format!("turn failed: {message}"),
            ));
            // ADR 0018's fallback: a recognizable context overflow gets one extra line, since
            // automatic compaction did not keep the request inside the window. The host only
            // has the text, so the prefix is what identifies it.
            if radian_host::is_context_overflow(message) {
                self.session.entries.push(Entry::new(
                    EntryKind::Notice,
                    radian_host::CONTEXT_OVERFLOW_HELP.to_string(),
                ));
            }
            return UpdateResult::dirty();
        }
        UpdateResult::none()
    }

    /// Pushes a notice entry. Public so the event loop can show a `/compact` result.
    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.session
            .entries
            .push(Entry::new(EntryKind::Notice, text));
    }

    /// Applies a live event. Deltas build `live` entries; the committed message
    /// (`TranscriptAppended`) replaces them, so a dropped delta can never leave a duplicate.
    /// Returns the effects the event implies (clearing an approval prompt abandons its reply
    /// channel, which the loop owns).
    pub fn apply_event(&mut self, event: &AgentEvent) -> Vec<Effect> {
        let mut effects = Vec::new();
        match event {
            // ADR 0023 U5: the turn, budget, usage and tool facts are written to the session
            // projection by the event sink (`ProjectionSink`), never here. `App` handles only
            // the transcript, the note and the effects.
            AgentEvent::TurnStarted => {
                // The turn is being written now; a `/fork` must wait for its `TurnFinished`.
                self.session.turn_committed = false;
            }
            // ADR 0027: the local turn timer starts with the model request and
            // stops with its response. The token count is the model's reported output.
            AgentEvent::ModelRequestStarted { .. } => {
                // A model request starts only after the batch before it ended (the turn loop is
                // request -> batch -> request), so nothing in the running set can still be
                // running here. A call left in it is a `ToolStarted` whose `ToolFinished` never
                // came — the core discards the results of the calls already running when a
                // journal append fails (`agent-core/src/turn.rs`, the `journal_failed` path) —
                // and the note must not outlive the batch it belongs to.
                self.session.forget_running_tools();
                self.session.timer.request_started(self.now);
            }
            AgentEvent::ModelResponseReceived { usage, .. } => {
                self.session
                    .timer
                    .response_finished(self.now, usage.as_ref().and_then(|usage| usage.output));
            }
            // ADR 0017: a transient failure is being retried. The failed attempt's
            // streamed text is discarded (it was live-only), and the status line names the retry.
            AgentEvent::ModelRetrying {
                attempt,
                max,
                delay,
                reason,
            } => {
                self.drop_live();
                self.session.note = Some(format!(
                    "retrying {attempt}/{max} in {}s: {reason}",
                    delay.as_secs()
                ));
            }
            AgentEvent::ToolStarted { call_id, name, .. } => {
                self.session.start_tool(call_id, name);
            }
            AgentEvent::ToolFinished { call_id, .. } => {
                // The tool is done. The note follows the calls that are **still** running: it
                // moves off the settled one when that was the call it named, and empties when
                // the last tool of the batch finishes, so the footer never names a settled tool
                // (the next model request finds it clear).
                self.session.finish_tool(call_id);
            }
            AgentEvent::TranscriptAppended { message, .. } => self.push_message(message),
            // ADR 0018: compaction is visible to the user as it happens.
            AgentEvent::CompactionStarted { tokens_before, .. } => {
                self.session.note = Some("compacting context".to_string());
                self.session.compacting = true;
                self.session.entries.push(Entry::new(
                    EntryKind::Notice,
                    format!("context over the budget ({tokens_before} tokens); compacting…"),
                ));
            }
            AgentEvent::CompactionFinished {
                revision,
                tokens_before,
                tokens_after,
                deterministic,
                dropped,
                ..
            } => {
                self.session.note = None;
                self.session.compacting = false;
                // ADR 0018, decision 3: a deterministic fallback says so and names
                // how many messages it dropped; a model summary keeps the token line.
                let text = if *deterministic {
                    format!("compacted without a summary ({dropped} dropped)")
                } else {
                    format!("context compacted ({tokens_before} → {tokens_after} tokens)")
                };
                self.session
                    .entries
                    .push(Entry::new(EntryKind::Notice, text));
                // The summary message arrives right after this event (ADR 0018): the numbers
                // are handed to it, since the journaled message does not carry them.
                self.session.pending_compaction = Some((
                    *revision,
                    *tokens_before,
                    *tokens_after,
                    *deterministic,
                    *dropped,
                ));
            }
            AgentEvent::CompactionFailed { reason } => {
                self.session.note = None;
                self.session.compacting = false;
                self.session.entries.push(Entry::new(
                    EntryKind::Notice,
                    format!("compaction failed: {reason}"),
                ));
            }
            // A skip means nothing was summarized (ADR 0018): the flag must not stick.
            AgentEvent::CompactionSkipped { .. } => {
                self.session.compacting = false;
            }
            AgentEvent::RunFinished { status: _ } => {
                // The turn is over. A pending approval is not cleared here: the permission
                // layer raced the wait against the turn's cancellation, so its waiter is gone,
                // the queue drops the element and wakes the UI (ADR 0023 U7). Driving the modal
                // from the queue alone is what makes "the waiter gave up" and "the turn ended"
                // the same event.
                self.session.note = None;
                self.session.running_tools.clear();
                self.session.compacting = false;
                // ADR 0033: a `!` submitted behind the turn starts now.
                if let Some((command, exclude_from_context)) = self.session.queued_bash.take() {
                    effects.extend(self.begin_bash(command, exclude_from_context).effects);
                }
            }
            _ => {}
        }
        effects
    }

    /// Sets the live entry of `kind` to `text` (creating it when it does not exist yet). The
    /// stream arrives as a snapshot, so this replaces rather than appends. The reasoning
    /// channel's clock starts at its **first** delta (ADR 0028), from
    /// the loop's injected instant (`self.now`): a redraw never reads the wall clock.
    pub(super) fn set_live(&mut self, kind: EntryKind, text: &str) {
        match self
            .session
            .entries
            .iter_mut()
            .rev()
            .find(|entry| entry.live && entry.kind == kind)
        {
            Some(entry) => {
                if kind == EntryKind::Reasoning && entry.reasoning_started.is_none() {
                    entry.reasoning_started = Some(self.now);
                }
                entry.text = text.to_owned();
            }
            None => {
                let mut entry = Entry::new(kind, text);
                entry.live = true;
                if kind == EntryKind::Reasoning {
                    entry.reasoning_started = Some(self.now);
                }
                self.session.entries.push(entry);
                self.session.live_count += 1;
            }
        }
    }

    /// The live reasoning channel's clock at the commit (ADR 0028): its
    /// first delta's instant and this one, the entry's end. `None` when this message does not
    /// replace a live reasoning entry — a resume has no live view, so a resumed thinking block
    /// has no duration. Must run **before** [`App::drop_live`], which is the entry's end.
    fn take_live_reasoning_clock(&mut self) -> Option<(Instant, Instant)> {
        // The same guard as `drop_live`: a resume pushes every message with no live entry, and
        // an unconditional reverse scan per message was the O(n^2) family `drop_live`'s count
        // fixed (resume-perf A).
        if self.session.live_count == 0 {
            return None;
        }
        let now = self.now;
        let entry = self
            .session
            .entries
            .iter_mut()
            .rev()
            .find(|entry| entry.live && entry.kind == EntryKind::Reasoning)?;
        let started = entry.reasoning_started?;
        entry.reasoning_ended = Some(now);
        Some((started, now))
    }

    /// Discards every live (streaming) entry. A committed message replaces them
    /// (`push_message`), and so does a retry: the failed attempt's deltas are live-only
    /// (ADR 0010/0017 revision 1). The scan runs only when one exists (resume-perf A).
    fn drop_live(&mut self) {
        if self.session.live_count > 0 {
            let mut scanned = 0u64;
            self.session.entries.retain(|entry| {
                scanned += 1;
                !entry.live
            });
            self.session.live_count = 0;
            #[cfg(test)]
            LIVE_SCANS.with(|count| count.set(count.get() + scanned));
        }
    }

    /// Appends a committed message as transcript entries. Used for live commits and for the
    /// history of a resumed session.
    pub fn push_message(&mut self, message: &Message) {
        // The committed message is the truth; discard the partial live view of it. The scan
        // runs **only when a live entry exists**: a resume pushes every message before any
        // stream has started, and an unconditional `retain` over a 66k-entry transcript per
        // message was O(n^2) (resume-perf A). The count is the number of live entries, so a
        // live entry is dropped wherever it sits - the stream can leave one behind a
        // compaction notice (they are not always a suffix of `entries`).
        // The live reasoning channel's clock, taken before the drop (ADR 0028
        // decision 4): the committed entry below carries its duration, so `◻ thinking · 3.2s`
        // survives the live-to-committed handoff. A resume has no live entry: `None`.
        let reasoning_clock = self.take_live_reasoning_clock();
        self.drop_live();
        // ADR 0033: a `!` command's `Ran \`cmd\`` context message is a Bash block, not user
        // prose. The one we already drew live is skipped so it is not shown twice; a resumed
        // one (no pending text) is drawn as a block.
        if let Message::User { .. } = message {
            let text = message.text();
            if self.session.bash_context_pending.as_deref() == Some(text.as_str()) {
                self.session.bash_context_pending = None;
                return;
            }
            if let Some(entry) = bash_entry_from_context(&text) {
                self.session.entries.push(entry);
                return;
            }
        }
        match message {
            Message::User { content, .. } => {
                // A compaction summary (ADR 0028 R5) is a `User` message with a
                // `compaction_summary` origin (ADR 0018); it shows as one line, not as a wall
                // of user text. Core's readers supply the summary and the verbatim user
                // words - nothing is parsed out of the body here.
                if agent_core::compaction::is_compaction_summary(message) {
                    let mut info = CompactionInfo {
                        summary: agent_core::compaction::compaction_summary_text(message)
                            .unwrap_or_default(),
                        user_messages: agent_core::compaction::compaction_user_messages(message)
                            .into_iter()
                            .map(|message| message.text)
                            .collect(),
                        ..CompactionInfo::default()
                    };
                    // The live path knows the revision and the token counts from the
                    // `CompactionFinished` event that preceded this message; a resume replay
                    // does not, and omits them.
                    if let Some((revision, before, after, deterministic, dropped)) =
                        self.session.pending_compaction.take()
                    {
                        info.revision = Some(revision);
                        info.tokens_before = Some(before);
                        info.tokens_after = Some(after);
                        info.deterministic = deterministic;
                        // `dropped` is 0 for a model summary; only a deterministic compaction
                        // names it (ADR 0018, decision 3).
                        info.dropped = deterministic.then_some(dropped);
                    }
                    let mut entry = Entry::new(EntryKind::Compaction, info.display_text());
                    entry.compaction = Some(Box::new(info));
                    self.session.entries.push(entry);
                    return;
                }
                // ADR 0018: an attachment is its own part, so the display renders
                // it as `[file: P, N lines]` rather than joining its body into the message.
                self.session.entries.push(Entry::new(
                    EntryKind::User,
                    crate::mention::display_parts(content),
                ));
            }
            Message::Assistant {
                content,
                tool_calls,
                reasoning,
                interrupted,
                ..
            } => {
                let reasoning = text_of(reasoning);
                if !reasoning.is_empty() {
                    let mut entry = Entry::new(EntryKind::Reasoning, reasoning);
                    // The duration measured live, carried onto the committed entry (ADR 0028
                    // revision 5 decision 4). It rides only here: a resumed message has no
                    // clock, so its thinking block shows no duration.
                    if let Some((started, ended)) = reasoning_clock {
                        entry.reasoning_started = Some(started);
                        entry.reasoning_ended = Some(ended);
                    }
                    self.session.entries.push(entry);
                }
                let mut entry = Entry::new(EntryKind::Assistant, text_of(content));
                entry.interrupted = *interrupted;
                self.session.entries.push(entry);
                // One card per call (ADR 0028 R4): each carries its own name and arguments,
                // and its text is the same header the renderer will show, so the head stays
                // selectable and copyable character for character.
                for call in tool_calls {
                    let card = crate::tools::ToolCard::new(
                        call.id.clone(),
                        call.name.clone(),
                        call.arguments.clone(),
                    );
                    // The call-time head is the tool name and the renderer's title (ADR 0028
                    // revision 5 decision 1); the meta and the status glyph are the block
                    // layer's, drawn from the card and the entry's state. The pairing refreshes
                    // the title once the result is in (a `git_diff` title is terse until then).
                    let mut entry =
                        Entry::new(EntryKind::ToolCall, crate::tools::call_text(&card, ""));
                    entry.tool = Some(Box::new(card));
                    // A call with no result yet is a whole card of its own (ADR 0028 revision
                    // 2); the pairing below turns it into the head of the merged card.
                    entry.card = crate::blocks::Card::Solo;
                    self.session.entries.push(entry);
                }
            }
            Message::Tool { result, .. } => {
                // The result pairs with its call **by id** (the same key the live and the
                // resume paths carry), so a batch of calls pairs correctly whatever order
                // their results arrive in.
                //
                // The pairing is a **single** scan from the back: the call is the most recent
                // matching card, and the index it found is reused for the insertion below.
                // (A second front `position` for the index walked the whole transcript per
                // result — the same O(n^2) family as the `retain` resume-perf A fixed.)
                let mut paired: Option<(String, serde_json::Value)> = None;
                let mut call_index: Option<usize> = None;
                let len = self.session.entries.len();
                for (rev, entry) in self.session.entries.iter_mut().rev().enumerate() {
                    #[cfg(test)]
                    TOOL_PAIR_SCANS.with(|count| count.set(count.get() + 1));
                    if entry.kind == EntryKind::ToolCall
                        && entry
                            .tool
                            .as_ref()
                            .is_some_and(|card| card.call_id == result.call_id)
                    {
                        entry.result = Some(result.is_error);
                        if let Some(card) = entry.tool.as_mut() {
                            card.structured = result.structured.clone();
                            // The head's title can only be final now (a `git_diff`'s counts
                            // come from the result text), so the entry's text - what renders
                            // and copies - is regenerated from the same function the layout
                            // uses. The result's own row count is part of the head's collapsed
                            // meta (`· N lines`), and only this entry can see the result text.
                            let display = crate::tools::result_display_text(card, &result.text());
                            card.result_lines = Some(crate::tools::body_line_count(&display));
                            // ADR 0024: the call entry draws the whole ask block
                            // (its result entry is blank), so it keeps the result's own text
                            // for the declined/cancelled status line inside the box.
                            entry.text = if card.name == "ask_user" {
                                result.text()
                            } else {
                                crate::tools::call_text(card, &result.text())
                            };
                            paired = Some((card.name.clone(), card.arguments.clone()));
                        }
                        call_index = Some(len - 1 - rev);
                        break;
                    }
                }
                let (name, arguments) = paired.unwrap_or_default();
                let mut card = crate::tools::ToolCard::new(result.call_id.clone(), name, arguments);
                card.structured = result.structured.clone();
                // The body's text **is** the entry's text (the mapping/search invariant): a
                // `patch` shows the synthesized diff, a `write` the content written, every
                // other tool its own result text.
                let display = crate::tools::result_display_text(&card, &result.text());
                let mut entry = Entry::new(EntryKind::ToolResult, display);
                entry.is_error = result.is_error;
                entry.error_code = result.error_code.clone();
                entry.tool = Some(Box::new(card));
                entry.card = crate::blocks::Card::Solo;
                // Rule 4: the result sits right after **its own** call, so a batch reads
                // call/result, call/result instead of every call then every result. The
                // entries are the UI's own order; the journal keeps the message order, so a
                // resume replay runs this same code and lands on the same order. A result
                // whose call is gone (a torn transcript) is appended.
                match call_index {
                    Some(index) => {
                        // The call heads the merged card and this result closes it (ADR 0028
                        // revision 2): the drawing merges them, the data stays two entries.
                        self.session.entries[index].card = crate::blocks::Card::Head;
                        entry.card = crate::blocks::Card::Tail;
                        self.session.entries.insert(index + 1, entry);
                        // The insertion shifts every later entry: a selection anchored to one
                        // of them moves with it.
                        self.shift_anchors(index + 1);
                    }
                    None => self.session.entries.push(entry),
                }
            }
        }
    }
}

// Test-only: how many entries `push_message` has walked to drop live ones. A unit test
// asserts it does not grow when no live entry exists, which is the property the O(n^2) fix
// rests on (it is not wall-clock based). Thread-local so parallel tests do not interfere.
#[cfg(test)]
thread_local! {
    pub(crate) static LIVE_SCANS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

// Test-only: how many entries the tool-result pairing has walked. A unit test asserts it stays
// linear in the transcript when every result pairs with the most recent call, which is the
// property the O(n^2) fix rests on (no wall clock). Thread-local for the same reason as above.
#[cfg(test)]
thread_local! {
    pub(crate) static TOOL_PAIR_SCANS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filler(count: usize) -> Vec<agent_core::Message> {
        (0..count)
            .map(|i| agent_core::Message::user(format!("message {i}")))
            .collect()
    }

    fn scans() -> u64 {
        LIVE_SCANS.with(std::cell::Cell::get)
    }

    /// resume-perf A: a resume pushes every message with no live entry, so the live-entry scan
    /// must not run. The number of entries walked is the property; no wall clock.
    #[test]
    fn push_message_does_not_walk_the_transcript_without_a_live_entry() {
        let mut app = App::new("m", "s");
        for message in filler(2_000) {
            app.push_message(&message);
        }
        assert_eq!(app.session.entries.len(), 2_000);
        let before = scans();
        for message in filler(1_000) {
            app.push_message(&message);
        }
        assert_eq!(
            scans(),
            before,
            "no live entry exists: push_message must not walk the transcript"
        );
    }

    /// The counter is live: with a live entry the walk does happen, and the entry is dropped.
    #[test]
    fn push_message_walks_the_transcript_to_drop_a_live_entry() {
        let mut app = App::new("m", "s");
        app.set_live(EntryKind::Assistant, "partial");
        let before = scans();
        app.push_message(&agent_core::Message::user("committed"));
        assert!(scans() > before, "a live entry makes the walk run");
        assert!(app.session.entries.iter().all(|entry| !entry.live));
        assert_eq!(app.session.live_count, 0);
    }

    /// A live entry is not always a suffix: a compaction notice can sit after it. The drop
    /// must still find it (a count, not a trailing-only scan).
    #[test]
    fn push_message_drops_a_live_entry_that_is_not_at_the_tail() {
        let mut app = App::new("m", "s");
        app.set_live(EntryKind::Assistant, "partial");
        app.session.entries.push(Entry::new(
            EntryKind::Notice,
            "context over the budget; compacting\u{2026}",
        ));
        app.push_message(&agent_core::Message::user("committed"));
        assert!(
            app.session.entries.iter().all(|entry| !entry.live),
            "the live entry is dropped wherever it sits"
        );
        assert!(
            app.session
                .entries
                .iter()
                .any(|entry| entry.kind == EntryKind::Notice),
            "the non-live entry after it survives"
        );
        assert_eq!(app.session.live_count, 0);
    }

    fn pair_scans() -> u64 {
        TOOL_PAIR_SCANS.with(std::cell::Cell::get)
    }

    /// A resume replays assistant tool calls and their results in order, so every result pairs
    /// with the most recent call. The pairing must not walk the transcript from the front: that
    /// was a second O(n^2) over the whole resume (resume-perf2). The number of entries walked
    /// is the property; no wall clock.
    #[test]
    fn tool_result_pairing_does_not_walk_the_transcript() {
        use agent_core::{ContentPart, ToolCall, ToolResult};

        let mut app = App::new("m", "s");
        let pairs = 400u64;
        let before = pair_scans();
        for i in 0..pairs {
            app.push_message(&agent_core::Message::Assistant {
                content: Vec::new(),
                reasoning: Vec::new(),
                tool_calls: vec![ToolCall {
                    id: format!("call-{i}"),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "file.txt"}),
                }],
                provider_payload: None,
                interrupted: false,
                meta: Default::default(),
            });
            app.push_message(&agent_core::Message::Tool {
                result: ToolResult::success(format!("call-{i}"), vec![ContentPart::text("ok")]),
                meta: Default::default(),
            });
        }
        let scanned = pair_scans() - before;
        assert!(
            scanned <= pairs * 4,
            "pairing {pairs} results walked {scanned} entries; it must not scan the transcript"
        );
        // The results still pair: one Assistant entry, one call and one result per pair.
        assert_eq!(app.session.entries.len() as u64, pairs * 3);
    }

    /// The pairing search is a scan from the back, so it finds the most recent matching call.
    /// A batch of calls (one assistant message, several results) pairs each result with its own
    /// call, not the first one in the batch.
    #[test]
    fn a_batch_of_tool_results_pairs_each_with_its_own_call() {
        use agent_core::{ContentPart, ToolCall, ToolResult};

        let mut app = App::new("m", "s");
        app.push_message(&agent_core::Message::Assistant {
            content: Vec::new(),
            reasoning: Vec::new(),
            tool_calls: vec![
                ToolCall {
                    id: "a".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "a"}),
                },
                ToolCall {
                    id: "b".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "b"}),
                },
            ],
            provider_payload: None,
            interrupted: false,
            meta: Default::default(),
        });
        // The results arrive in the reverse order (allowed): each must pair with its own call.
        for id in ["b", "a"] {
            app.push_message(&agent_core::Message::Tool {
                result: ToolResult::success(id.to_owned(), vec![ContentPart::text("ok")]),
                meta: Default::default(),
            });
        }
        let order: Vec<&str> = app
            .session
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::ToolCall)
            .filter_map(|entry| entry.tool.as_ref().map(|card| card.call_id.as_str()))
            .collect();
        assert_eq!(order, vec!["a", "b"], "the calls keep their order");
        // Each result sits right after **its own** call (rule 4): call a, result a, call b,
        // result b - not every call then every result.
        let layout: Vec<(EntryKind, String)> = app
            .session
            .entries
            .iter()
            .map(|entry| {
                let id = entry
                    .tool
                    .as_ref()
                    .map(|card| card.call_id.clone())
                    .unwrap_or_default();
                (entry.kind, id)
            })
            .collect();
        assert_eq!(
            layout,
            vec![
                (EntryKind::Assistant, String::new()),
                (EntryKind::ToolCall, "a".to_owned()),
                (EntryKind::ToolResult, "a".to_owned()),
                (EntryKind::ToolCall, "b".to_owned()),
                (EntryKind::ToolResult, "b".to_owned()),
            ]
        );
        // Both calls paired with their own result.
        let calls: Vec<Option<bool>> = app
            .session
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::ToolCall)
            .map(|entry| entry.result)
            .collect();
        assert_eq!(calls, vec![Some(false), Some(false)]);
    }
}
