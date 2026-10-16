//! The session projection (ADR 0023 U2/U5): the facts a UI reads for one session.
//!
//! The projection is the **only** owner of the changing session facts — the turn state, usage,
//! the budget, the tier, the MCP counts, the most recent tool call and the streaming buffer.
//! `App` holds UI state only and reads this. The producer rule is one order:
//!
//! - the producer writes the value, bumps the revision, then sets `dirty` with `Release` and
//!   reports "send a notification" **only when the flag was `false`** — so at most one `Dirty`
//!   per session is in flight;
//! - the UI clears the flag with `Acquire` **before** reading the values, so an update that
//!   happens after that read sets the flag again and produces another notification.
//!
//! `Relaxed` is never used for the flag. The values themselves are behind a `Mutex`, so their
//! contents are ordered by the lock; the flag's ordering is what makes the *decision* to read
//! them correct, and the `Acquire` read publishes the writes to the UI thread.
//!
//! `revision` is a plain counter: it is only ever read after [`SessionProjection::take_dirty`]
//! returned `true`, which is the acquire that orders it.

use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use agent_core::{BudgetReport, ModelUsage};
use radian_session::UsageFee;

use crate::ModelPrice;

/// The turn state the status bar shows (ADR 0023 U5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TurnState {
    #[default]
    Idle,
    Running,
    Cancelled,
}

impl TurnState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Which stream a chunk belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Assistant,
    Reasoning,
}

/// The accumulated streaming text, in the order the streams first appeared. The order matters:
/// the renderer shows the reasoning block above the answer when the provider streamed it first,
/// exactly as the deltas arrived.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamSnapshot {
    /// The streams that have appeared, first appearance first (at most two entries).
    pub order: Vec<StreamKind>,
    pub assistant: String,
    pub reasoning: String,
}

#[derive(Debug, Default)]
struct Stream {
    order: Vec<StreamKind>,
    assistant: String,
    reasoning: String,
}

impl Stream {
    fn clear(&mut self) {
        self.order.clear();
        self.assistant.clear();
        self.reasoning.clear();
    }
}

/// The most recent tool call the session started (ADR 0023 U5's "最近工具调用").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastToolCall {
    /// Position of the call in its batch.
    pub index: usize,
    pub call_id: String,
    pub name: String,
}

/// Every fact the projection holds. Private: readers go through [`ProjectionRead`] so they
/// cannot hold the lock while doing something else.
#[derive(Debug, Default)]
struct Facts {
    stream: Stream,
    turn: TurnState,
    usage_input: Option<u64>,
    usage_output: Option<u64>,
    /// The provider-reported cache **read** tokens (ADR 0027): the footer's
    /// `{cache}`. A provider that never reports it leaves it `None`, never 0.
    usage_cache_read: Option<u64>,
    /// The provider-reported cache **write** tokens, for the same reason.
    usage_cache_write: Option<u64>,
    /// The session's accumulated fee (ADR 0027/12). Each model call is priced at
    /// the rate the route had **when it arrived** (`cost::add_cost`), so a `/model` switch does
    /// not re-price earlier calls. `None` until a call had something to bill: the item hides
    /// rather than showing `$0.00`. An incomplete fee (a non-zero counter with no rate) makes
    /// the whole total incomplete.
    cost: Option<UsageFee>,
    /// `true` when at least one restored call was re-priced with today's catalog instead of
    /// taken from its record (ADR 0027): the footer shows `~$X`.
    cost_estimated: bool,
    /// The unit prices of the model in force (ADR 0027), set at startup and on each
    /// applied route change. `None` means the catalog has no price for it: calls arriving then
    /// contribute no fee.
    price: Option<ModelPrice>,
    budget: Option<BudgetReport>,
    /// The effective input budget `B` (ADR 0018), for the status bar's `T / B`.
    context_window: Option<u64>,
    tier: Option<String>,
    /// The working mode in force (`general` / `lead`, ADR 0038 decision 1). The footer and
    /// `/mode` with no argument read it.
    mode: Option<String>,
    mcp_configured: usize,
    mcp_open: Option<(usize, usize)>,
    last_tool: Option<LastToolCall>,
}

/// One session's projection.
#[derive(Debug, Default)]
pub struct SessionProjection {
    facts: Mutex<Facts>,
    revision: AtomicU64,
    dirty: AtomicBool,
}

impl SessionProjection {
    pub fn new() -> Self {
        Self::default()
    }

    /// The one write order: value first, revision, then the `Release` flag. Returns `true` when
    /// the caller must send a `Dirty` notification.
    fn write(&self, update: impl FnOnce(&mut Facts)) -> bool {
        {
            let mut facts = self.facts.lock().expect("the projection lock");
            update(&mut facts);
        }
        self.revision.fetch_add(1, Ordering::Relaxed);
        // `Release`: the value writes and the revision bump above are visible to a reader that
        // acquires this flag.
        !self.dirty.swap(true, Ordering::Release)
    }

    /// A read of every fact, holding the lock for a consistent set. The guard must be dropped
    /// before any write on this projection (a write while a read is alive would deadlock).
    pub fn read(&self) -> ProjectionRead<'_> {
        ProjectionRead {
            facts: self.facts.lock().expect("the projection lock"),
            revision: self.revision.load(Ordering::Relaxed),
        }
    }

    /// Appends one delta. Returns `true` when the caller must send a `Dirty` notification.
    pub fn push_delta(&self, kind: StreamKind, text: &str) -> bool {
        self.write(|facts| {
            if !facts.stream.order.contains(&kind) {
                facts.stream.order.push(kind);
            }
            match kind {
                StreamKind::Assistant => facts.stream.assistant.push_str(text),
                StreamKind::Reasoning => facts.stream.reasoning.push_str(text),
            }
        })
    }

    /// Drops the streaming buffer: a committed message replaced the live view. This does not
    /// wake the UI — the commit itself is a fact on the channel, and an empty snapshot is a
    /// no-op.
    pub fn clear_stream(&self) {
        let mut facts = self.facts.lock().expect("the projection lock");
        facts.stream.clear();
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    /// Discards **every** fact, as if the projection had just been constructed.
    ///
    /// The observer calls this when its journal file is replaced or truncated (ADR 0023 U6):
    /// the replaced file's facts must not survive into the new one. The revision stays
    /// monotonic (a reader that keys on it never sees it go back), and the dirty flag is left
    /// alone — a polled observer reads on its own schedule, not on a wake-up.
    pub fn reset(&self) {
        let mut facts = self.facts.lock().expect("the projection lock");
        *facts = Facts::default();
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    /// The turn state.
    pub fn set_turn(&self, turn: TurnState) -> bool {
        self.write(|facts| facts.turn = turn)
    }

    /// A request started: the turn is running, and the request carries its budget projection
    /// (ADR 0018). One write, so one wake-up for the pair.
    pub fn set_request_started(&self, budget: Option<BudgetReport>) -> bool {
        self.write(|facts| {
            facts.turn = TurnState::Running;
            facts.budget = budget;
        })
    }

    /// Adds one provider-reported usage to the session totals, pricing it at the route's
    /// **current** rate (ADR 0027). A field the provider did not report is left
    /// alone; it is never treated as zero.
    ///
    /// The fee is added here rather than derived at draw time from the totals: the totals mix
    /// models once `/model` switches, and one price cannot price a mixed total. A call that
    /// arrives with no known price contributes nothing to the fee (and nothing is invented).
    pub fn add_usage(&self, usage: &ModelUsage) -> bool {
        self.write(|facts| {
            let price = facts.price.clone();
            Self::add_usage_locked(facts, price.as_ref(), usage)
        })
    }

    /// Like [`Self::add_usage`], but at an explicit price and marking the total an estimate:
    /// the resume path folds a journal's **old** usage records (written before the fee field
    /// existed, ADR 0007), which have to be re-priced with today's catalog at the
    /// route that was in force **then**, not the route the process starts with (F3, ADR 0027
    /// revision 12 decision 3). A record that carries its own fee goes through
    /// [`Self::add_recorded_usage`] instead.
    pub fn add_usage_at(&self, price: Option<&ModelPrice>, usage: &ModelUsage) -> bool {
        self.write(|facts| {
            Self::add_usage_locked(facts, price, usage);
            facts.cost_estimated = true;
        })
    }

    /// Folds one **recorded** call back in (ADR 0027): its tokens join
    /// the totals and the fee it was billed at is added as it stands, so a later catalog change
    /// cannot move what the session already spent. Used only by the resume path.
    pub fn add_recorded_usage(&self, usage: &ModelUsage, fee: UsageFee) -> bool {
        self.write(|facts| {
            Self::add_tokens(facts, usage);
            facts.cost = Some(match facts.cost {
                Some(total) => total.merge(fee),
                None => fee,
            });
        })
    }

    fn add_usage_locked(facts: &mut Facts, price: Option<&ModelPrice>, usage: &ModelUsage) {
        Self::add_tokens(facts, usage);
        facts.cost = crate::cost::add_cost(facts.cost, price, usage);
    }

    /// Adds one provider-reported usage to the token totals. A field the provider did not
    /// report is left alone; it is never treated as zero.
    fn add_tokens(facts: &mut Facts, usage: &ModelUsage) {
        if let Some(input) = usage.input {
            facts.usage_input = Some(facts.usage_input.unwrap_or(0).saturating_add(input));
        }
        if let Some(output) = usage.output {
            facts.usage_output = Some(facts.usage_output.unwrap_or(0).saturating_add(output));
        }
        if let Some(cache_read) = usage.cache_read {
            facts.usage_cache_read = Some(
                facts
                    .usage_cache_read
                    .unwrap_or(0)
                    .saturating_add(cache_read),
            );
        }
        if let Some(cache_write) = usage.cache_write {
            facts.usage_cache_write = Some(
                facts
                    .usage_cache_write
                    .unwrap_or(0)
                    .saturating_add(cache_write),
            );
        }
    }

    /// Sets the unit prices of the route now in force (ADR 0027, ADR 0026 D5): the
    /// price each **later** call is billed at. An earlier call keeps the price it arrived
    /// under, which is why the fee is accumulated per call instead of recomputed here.
    pub fn set_price(&self, price: Option<ModelPrice>) -> bool {
        self.write(|facts| facts.price = price)
    }

    /// The most recent tool call.
    pub fn set_last_tool(&self, tool: LastToolCall) -> bool {
        self.write(|facts| facts.last_tool = Some(tool))
    }

    /// The effective input budget `B` (ADR 0018); `None` means no budget is configured.
    pub fn set_context_window(&self, budget: Option<u64>) -> bool {
        self.write(|facts| facts.context_window = budget)
    }

    /// The permission tier's display name (ADR 0012).
    pub fn set_tier(&self, tier: Option<String>) -> bool {
        self.write(|facts| facts.tier = tier)
    }

    /// The working mode's name (ADR 0038 decision 1). The session sets the startup value; a
    /// `/mode` switch sets the new one as it queues the change.
    pub fn set_mode(&self, mode: Option<String>) -> bool {
        self.write(|facts| facts.mode = mode)
    }

    /// How many MCP servers are configured, and the servers/tools the opened session reported.
    pub fn set_mcp(&self, configured: usize, open: Option<(usize, usize)>) -> bool {
        self.write(|facts| {
            facts.mcp_configured = configured;
            facts.mcp_open = open;
        })
    }

    /// Clears the dirty flag and reports whether it was set. The UI calls this **before**
    /// reading the projection; a `false` means another `Dirty` is already in flight (or the
    /// update was already consumed), so the read can be skipped.
    pub fn take_dirty(&self) -> bool {
        // `Acquire`: the value reads that follow see everything the producer wrote before it
        // set the flag.
        self.dirty.swap(false, Ordering::Acquire)
    }

    /// Clears the dirty flag and reads the snapshot, **in that order** (ADR 0023 U2's wake-up
    /// contract). `None` means another `Dirty` is already in flight, so the read can be
    /// skipped. `after_clear` runs between the two steps: the ordering test uses it to write
    /// from the producer exactly at the interleaving the order is about.
    pub fn take_snapshot_with(&self, after_clear: impl FnOnce()) -> Option<StreamSnapshot> {
        if !self.take_dirty() {
            return None;
        }
        after_clear();
        Some(self.snapshot())
    }

    /// [`Self::take_snapshot_with`] with no interleaving: what the event loop calls.
    pub fn take_snapshot(&self) -> Option<StreamSnapshot> {
        self.take_snapshot_with(|| {})
    }

    /// The accumulated stream. Read after [`Self::take_dirty`].
    pub fn snapshot(&self) -> StreamSnapshot {
        let facts = self.facts.lock().expect("the projection lock");
        StreamSnapshot {
            order: facts.stream.order.clone(),
            assistant: facts.stream.assistant.clone(),
            reasoning: facts.stream.reasoning.clone(),
        }
    }

    /// The number of changes since construction. Only meaningful after
    /// [`Self::take_dirty`] returned `true`.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }
}

/// A consistent read of every fact, with the revision it was read at (ADR 0023 U5's
/// `ProjectionRead`). The lock is held, so a caller that needs several values sees one instant.
pub struct ProjectionRead<'a> {
    facts: MutexGuard<'a, Facts>,
    revision: u64,
}

impl ProjectionRead<'_> {
    /// The revision of the values in this read.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn turn(&self) -> TurnState {
        self.facts.turn
    }

    pub fn usage_input(&self) -> Option<u64> {
        self.facts.usage_input
    }

    pub fn usage_output(&self) -> Option<u64> {
        self.facts.usage_output
    }

    pub fn usage_cache_read(&self) -> Option<u64> {
        self.facts.usage_cache_read
    }

    pub fn usage_cache_write(&self) -> Option<u64> {
        self.facts.usage_cache_write
    }

    /// The session's accumulated fee in micro-dollars (ADR 0027/12). `None` when
    /// no call has had anything to bill: the `cost` item hides rather than showing `$0.00`.
    pub fn cost_micros(&self) -> Option<u64> {
        self.facts.cost.map(|cost| cost.micros)
    }

    /// Whether the accumulated fee is a lower bound: a non-zero counter had no rate during at
    /// least one call (ADR 0027). The footer shows `≥`.
    pub fn cost_incomplete(&self) -> bool {
        self.facts.cost.is_some_and(|cost| cost.incomplete)
    }

    /// Whether the accumulated fee is an estimate: at least one restored call was re-priced
    /// with today's catalog rather than taken from its record (ADR 0027 decision
    /// 3). The footer shows `~`, which wins over the incomplete `≥`.
    pub fn cost_estimated(&self) -> bool {
        self.facts.cost_estimated
    }

    pub fn budget(&self) -> Option<&BudgetReport> {
        self.facts.budget.as_ref()
    }

    pub fn context_window(&self) -> Option<u64> {
        self.facts.context_window
    }

    pub fn tier(&self) -> Option<&str> {
        self.facts.tier.as_deref()
    }

    /// The working mode's name (ADR 0038 decision 1): `general` or `lead`.
    pub fn mode(&self) -> Option<&str> {
        self.facts.mode.as_deref()
    }

    pub fn mcp_configured(&self) -> usize {
        self.facts.mcp_configured
    }

    pub fn mcp_open(&self) -> Option<(usize, usize)> {
        self.facts.mcp_open
    }

    pub fn last_tool(&self) -> Option<&LastToolCall> {
        self.facts.last_tool.as_ref()
    }

    pub fn stream(&self) -> StreamSnapshot {
        StreamSnapshot {
            order: self.facts.stream.order.clone(),
            assistant: self.facts.stream.assistant.clone(),
            reasoning: self.facts.stream.reasoning.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wake-up contract, part one: a delta wakes the UI once, and a second delta before the
    /// UI looks does **not** enqueue a second notification.
    #[test]
    fn a_delta_wakes_the_ui_once_until_it_reads() {
        let projection = SessionProjection::new();
        assert!(
            projection.push_delta(StreamKind::Assistant, "a"),
            "the first delta wakes the UI"
        );
        assert!(
            !projection.push_delta(StreamKind::Assistant, "b"),
            "a second delta before the UI looks must not enqueue another notification"
        );
        assert_eq!(projection.snapshot().assistant, "ab");
        assert_eq!(projection.revision(), 2);

        assert!(projection.take_dirty(), "the flag was set");
        assert!(!projection.take_dirty(), "clearing it once is enough");
    }

    /// The wake-up contract, part two: an update **after** the UI read always produces another
    /// notification, so no wake-up is lost.
    #[test]
    fn an_update_after_the_read_wakes_the_ui_again() {
        let projection = SessionProjection::new();
        projection.push_delta(StreamKind::Assistant, "a");
        assert!(projection.take_dirty());
        let seen = projection.snapshot();
        assert_eq!(seen.assistant, "a");

        assert!(
            projection.push_delta(StreamKind::Assistant, "b"),
            "an update after the read must wake the UI again"
        );
        assert_eq!(projection.snapshot().assistant, "ab");
        assert!(projection.take_dirty());
    }

    /// The order of the two steps is the whole point: a write that lands **after** the flag was
    /// cleared must survive (the UI is woken again). Reversing them — read, then clear — loses
    /// exactly this update.
    #[test]
    fn an_update_between_the_clear_and_the_read_is_not_lost() {
        let projection = SessionProjection::new();
        projection.push_delta(StreamKind::Assistant, "a");
        let snapshot = projection.take_snapshot_with(|| {
            // The producer writes here: the flag was just cleared, the buffer not yet read.
            projection.push_delta(StreamKind::Assistant, "b");
        });
        assert_eq!(
            snapshot.expect("the flag was set").assistant,
            "ab",
            "the read sees the update that landed between the steps"
        );
        assert!(
            projection.take_dirty(),
            "and that update is not lost: the UI is woken again"
        );
    }

    /// The same order applies to every fact, not just the stream: a usage update after the UI
    /// looked must wake it, and the value must be visible in the read that follows.
    #[test]
    fn a_fact_update_after_the_read_wakes_the_ui_again() {
        let projection = SessionProjection::new();
        assert!(projection.set_turn(TurnState::Running));
        assert!(projection.take_dirty(), "the UI looks");
        assert_eq!(projection.read().turn(), TurnState::Running);

        // Every fact obeys the same contract. A field that never woke the UI would leave the
        // status bar stale after the flag was cleared.
        let usage = ModelUsage {
            input: Some(10),
            output: Some(2),
            cache_read: Some(7),
            ..ModelUsage::default()
        };
        assert!(
            projection.add_usage(&usage),
            "a usage update after the read must wake the UI again"
        );
        // A read holds the lock; the guard is scoped so a write cannot run while it is alive.
        {
            let read = projection.read();
            assert_eq!(read.usage_input(), Some(10));
            assert_eq!(read.usage_output(), Some(2));
            assert_eq!(read.usage_cache_read(), Some(7));
        }
        assert!(projection.take_dirty());

        // The first write after the read wakes the UI; the rest coalesce into it.
        assert!(projection.set_context_window(Some(1000)));
        assert!(!projection.set_tier(Some("workspace".into())));
        assert!(!projection.set_mcp(2, Some((1, 15))));
        assert!(!projection.set_last_tool(LastToolCall {
            index: 0,
            call_id: "c1".into(),
            name: "read".into(),
        }));
        assert!(
            projection.take_dirty(),
            "the writes coalesced into one wake-up"
        );
        {
            let read = projection.read();
            assert_eq!(read.context_window(), Some(1000));
            assert_eq!(read.tier(), Some("workspace"));
            assert_eq!(read.mcp_configured(), 2);
            assert_eq!(read.mcp_open(), Some((1, 15)));
            assert_eq!(read.last_tool().map(|t| t.name.as_str()), Some("read"));
        }
        assert!(!projection.take_dirty(), "one clear is enough");
    }

    /// `set_request_started` is one write: the turn and the budget wake the UI once, and the
    /// budget that arrived is readable.
    #[test]
    fn a_request_start_is_one_wake_up_for_the_turn_and_the_budget() {
        let projection = SessionProjection::new();
        assert!(!projection.take_dirty(), "nothing yet");
        assert!(projection.set_request_started(None));
        assert!(
            !projection.set_request_started(None),
            "the second request before the UI looks does not enqueue another notification"
        );
        assert!(projection.take_dirty());
        let read = projection.read();
        assert_eq!(read.turn(), TurnState::Running);
        assert!(read.budget().is_none());
    }

    #[test]
    fn the_stream_order_is_first_appearance() {
        let projection = SessionProjection::new();
        projection.push_delta(StreamKind::Reasoning, "think");
        projection.push_delta(StreamKind::Assistant, "answer");
        projection.push_delta(StreamKind::Reasoning, " more");
        let snapshot = projection.snapshot();
        assert_eq!(
            snapshot.order,
            vec![StreamKind::Reasoning, StreamKind::Assistant]
        );
        assert_eq!(snapshot.reasoning, "think more");
        assert_eq!(snapshot.assistant, "answer");
    }

    /// There is no loom here (adding a dependency is out of scope), so the memory order is
    /// pinned **structurally**: the flag's two swaps must be `Release`/`Acquire`. The program
    /// order is covered by the interleaving test above; the memory order is what makes that
    /// order meaningful across threads.
    #[test]
    fn the_dirty_flag_is_release_and_acquire_never_relaxed() {
        // Only the non-test part: the assertions below quote the very strings they forbid.
        let source = include_str!("projection.rs");
        let source = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
        let code: String = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            code.contains("self.dirty.swap(true, Ordering::Release)"),
            "the producer's flag write must be Release"
        );
        assert!(
            code.contains("self.dirty.swap(false, Ordering::Acquire)"),
            "the UI's flag clear must be Acquire"
        );
        assert!(
            !code.contains("dirty.swap(true, Ordering::Relaxed)")
                && !code.contains("dirty.swap(false, Ordering::Relaxed)"),
            "Relaxed is not allowed for the flag"
        );
    }

    /// The write order is pinned structurally too: the value write happens **before** the flag
    /// swap in `write`, and there is exactly one place that sets the flag.
    #[test]
    fn the_value_is_written_before_the_flag_is_set() {
        let source = include_str!("projection.rs");
        let source = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
        let code: String = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let body = code
            .split("fn write(")
            .nth(1)
            .expect("the write helper exists");
        let value = body.find("update(&mut facts)").expect("the value write");
        let flag = body
            .find("self.dirty.swap(true, Ordering::Release)")
            .expect("the flag write");
        assert!(
            value < flag,
            "the value must be written before the flag is set"
        );
    }

    #[test]
    fn clear_stream_drops_the_live_view() {
        let projection = SessionProjection::new();
        projection.push_delta(StreamKind::Assistant, "a");
        projection.take_dirty();
        projection.clear_stream();
        let snapshot = projection.snapshot();
        assert_eq!(snapshot, StreamSnapshot::default());
        // Clearing does not wake the UI: the committed message is the event that matters.
        assert!(!projection.take_dirty());
    }

    /// `reset` is the observer's "the file was replaced" (ADR 0023 U6): every fact goes back to
    /// the default, the revision stays monotonic, and nothing is woken.
    #[test]
    fn reset_discards_every_fact_but_keeps_the_revision_moving() {
        let projection = SessionProjection::new();
        projection.set_turn(TurnState::Running);
        projection.set_last_tool(LastToolCall {
            index: 3,
            call_id: "c1".into(),
            name: "write".into(),
        });
        projection.set_tier(Some("workspace".into()));
        projection.add_usage(&ModelUsage {
            input: Some(10),
            output: Some(2),
            ..ModelUsage::default()
        });
        projection.push_delta(StreamKind::Assistant, "hi");
        projection.take_dirty();
        let before = projection.revision();

        projection.reset();

        let read = projection.read();
        assert_eq!(read.turn(), TurnState::Idle);
        assert!(read.last_tool().is_none());
        assert!(read.tier().is_none());
        assert_eq!(read.usage_input(), None);
        assert_eq!(read.usage_output(), None);
        assert_eq!(read.stream(), StreamSnapshot::default());
        assert!(read.revision() > before, "the revision stays monotonic");
        drop(read);
        // A polled reader looks on its own schedule; reset does not enqueue a wake-up.
        assert!(!projection.take_dirty());
    }
}
