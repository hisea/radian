//! The named-agent registry (ADR 0030 O12, slice 7 part b): one long session per **name**,
//! commanded by the human, and usable by the lead through `send_agent` (ADR 0030 O4).
//!
//! It sits next to the [`crate::task::TaskRegistry`] in the same lead process and mirrors its
//! shape: the host supplies a [`NamedBuilder`] (the host-specific model, tool set, wording), and
//! the registry enforces the named-agent invariants on what the builder returns. What makes a
//! named agent a named agent:
//!
//! - **First use creates, later uses reuse** (O12): `send(name, text, source)` resolves the
//!   definition from the **lead's** definition dirs — the lead's trusted project
//!   `<workspace>/.radian/agents/<name>.md` when the workspace is trusted (ADR 0030 O7 rev 2 item
//!   4 / ADR 0031 T1), else `~/.radian/agents/<name>.md` (the built-in `default`/`task`/`lead` of
//!   that name is the fallback) — snapshots it, opens **one** session, and — after that — only
//!   enqueues. The definition is never re-read for a live name, and never from the named agent's
//!   own worktree.
//! - **One worktree, reused** (O12): the default isolation is a `git worktree` at
//!   `<radian-home>/worktrees/<repo-key>/agent/<name>` on `radian/agent/<name>`, created once.
//!   `isolation = "shared"` runs in the lead's own workspace. The worktree is **kept** when the
//!   lead exits (O12), so the branch is there to inspect and the session resumable.
//! - **No memory** (O8): two sessions writing one memory root would fight over its single-writer
//!   lock, so `memory_root = None`, exactly like a task.
//! - **Sources are the caller's** (O12): a human `/send` is `User`, the lead's `send_agent` is
//!   `Subagent`. The per-turn source is reset when the agent is idle and only lowered when it is
//!   busy (ADR 0012 decision 5), exactly as the CLI host does for its own inputs.
//! - **`spawn: true` is a capability** (O13): a named agent whose definition says `spawn: true`
//!   is a lead and mounts the lead tools (the host's builder passes its task builder back); one
//!   without it mounts none. The depth limit 2 stays **task-only**: a task ignores its own
//!   `spawn: true`, a named agent does not.
//! - **Commits stay on its own branch** (O3): a worktree named agent mounts `git_commit` limited
//!   to `radian/agent/<name>`; a `shared` one has no branch of its own, so it does not mount
//!   `git_commit` at all — it can never commit onto the human's checked-out branch, and a
//!   definition that lists `git_commit` is warned about in the send result.
//!
//! **`max_tasks` does not count named agents.** `max_tasks` is the process-wide cap on **task**
//! agents (O8/O13); a named agent is an O12 long session, not a task, and this registry never
//! touches the [`crate::task::TaskRegistry`] queue. The two registries are siblings, and only the
//! task one is capped.
//!
//! The lead's journal records the creation once, as a non-critical
//! [`RuntimeEvent::NamedAgentStarted`](radian_session::RuntimeEvent), so a resume can name the
//! agents that were created. Killing the lead closes every named session's driver; the worktrees
//! stay.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_core::{InputSource, Message};
use radian_runtime::RuntimeEvents;
use radian_session::{RuntimeEvent, SessionJournal, TurnOutcome};
use tokio::sync::{Mutex as AsyncMutex, watch};

use crate::agents::{NamedAgent, resolve_named};
use crate::home::radian_home;
use crate::observer::JournalObserver;
use crate::orchestration::{self, Orchestration};
use crate::session::{
    ApprovalRouting, LocalSession, SessionError, SessionQueue, SessionSetup, open_session,
};
use crate::session_id::validate_session_id;
use crate::task::{Isolation, TaskSession, WorktreeRoot, create_worktree, task_worktree_root};

/// Why a named agent could not be created or sent to.
#[derive(Debug, thiserror::Error)]
pub enum NamedError {
    #[error("{0}")]
    Message(String),
}

impl NamedError {
    fn new(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// What one `send` did (ADR 0030 O4/O12): the name, the session it went to, whether this call
/// **created** the session (first use) or reused it, whether the input was **queued** behind a
/// running turn (the `next_turn` lane), and a **token** (the turn generation to wait past) so a
/// caller can wait for exactly this send's turn to settle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendOutcome {
    pub name: String,
    pub session_id: String,
    /// `true` when this send created the session from its definition.
    pub created: bool,
    /// `true` when the agent was running and the input went to its `next_turn` lane.
    pub queued: bool,
    /// The token to hand to [`NamedRecord::await_turn_end`] (or `wait_agent`'s `since`): the
    /// send's own turn is the first turn whose end moves [`NamedRecord::turn_generation`] above
    /// this value, so a caller that waits on it never misses a turn that ended in between, and
    /// never settles on the reply of the turn it was queued behind.
    ///
    /// It is the generation at the enqueue **plus one when the send was queued** (ADR 0030 O12
    /// review fix 2): the input always goes to the `next_turn` lane, so it runs as the first turn
    /// after any turn that is running at the enqueue. While the agent is idle our turn is the
    /// next to end, and the generation now (`G`) is the token; while it is busy the running turn
    /// ends first (`G + 1`), so a token of `G` alone would let a wait return that turn's reply in
    /// the idle moment before ours starts — the common busy case.
    pub turn: u64,
    pub worktree: Option<PathBuf>,
    /// Warnings that did not fail the send (ADR 0030 O11/O3): a `skills` name discovery did not
    /// find, or `git_commit` in a `shared` agent's `tools` list (it is not mounted).
    pub warnings: Vec<String>,
}

/// What the [`NamedBuilder`] is told about the named agent it is building a session for. The
/// builder supplies the host-specific parts (model, tier, servers, events); the registry
/// overrides the named invariants (workspace, memory, the role, the queue).
pub struct NamedContext<'a> {
    /// The agent name, which is also its session id.
    pub name: &'a str,
    /// The session id (the name).
    pub session_id: &'a str,
    pub isolation: Isolation,
    /// The session's workspace root: the worktree, or the lead's workspace for `shared`.
    pub workspace: &'a Path,
    /// The `radian/agent/<name>` branch, or `None` for `shared`.
    pub branch: Option<&'a str>,
    /// The definition resolved and snapshotted at first use (ADR 0030 O11/O12).
    pub agent: &'a NamedAgent,
}

/// Builds the host-specific parts of a named session. Called once per first `send`.
pub type NamedBuilder =
    Arc<dyn Fn(&NamedContext<'_>) -> Result<TaskSession, SessionError> + Send + Sync>;

/// A test seam run at the start of a named session's creation, on the runtime thread.
pub type NamedCreateHook = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// The test seams a registry can carry. `None` in production.
#[derive(Clone, Default)]
pub struct NamedSeams {
    pub create: Option<NamedCreateHook>,
}

/// A live named agent: its one session and the facts the host and the lead need (ADR 0030 O12).
pub struct NamedRecord {
    pub name: String,
    pub session: Arc<LocalSession>,
    pub isolation: Isolation,
    /// The worktree path, or `None` for `shared`.
    pub worktree: Option<PathBuf>,
    /// The `radian/agent/<name>` branch, or `None` for `shared`.
    pub branch: Option<String>,
    /// The snapshotted definition (never re-read while the session lives).
    pub agent: NamedAgent,
    /// Skill names the definition named but discovery did not find, plus a `git_commit` a
    /// `shared` definition listed (never mounted). Never a failure (ADR 0030 O11/O3).
    pub warnings: Vec<String>,
    /// The turn tracking shared with the runtime-events wrapper.
    turns: Arc<NamedTurns>,
}

impl NamedRecord {
    /// Whether the named agent's turn is running right now (the driver's own flag).
    pub fn running(&self) -> bool {
        self.session.turn_running()
    }

    /// The outcome of the last turn that ended, or `None` before its first.
    pub fn last_outcome(&self) -> Option<TurnOutcome> {
        *self
            .turns
            .last_outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// The turn generation right now: it bumps on every turn start and turn end.
    pub fn turn_generation(&self) -> u64 {
        *self.turns.turns.borrow()
    }

    /// A receiver that changes on every turn boundary and turn start.
    pub fn subscribe_turns(&self) -> watch::Receiver<u64> {
        self.turns.turns.subscribe()
    }

    /// The last committed assistant text, read from the session's journal (bounded by the
    /// caller). `None` when the agent has not produced one.
    pub fn reply(&self) -> Option<String> {
        last_assistant_text(self.session.journal_path())
    }

    /// Waits until the turn generation has moved above `start` and the agent is idle again, then
    /// returns the last committed assistant text. Takes the token a [`SendOutcome`] carries, so
    /// a turn that ended before the wait started still satisfies it. Used for the TUI's
    /// completion notice and the lead's `wait_agent`.
    pub async fn await_turn_end(&self, start: u64) -> Option<String> {
        let mut turns = self.subscribe_turns();
        while !(self.turn_generation() > start && !self.running()) {
            if turns.changed().await.is_err() {
                return None;
            }
        }
        self.reply()
    }
}

/// One named agent's turn tracking: the last turn outcome and the `watch` a waiter uses. It has
/// no end-of-task commit (that is a task rule, ADR 0030 O2). "Running" comes from the session's
/// own driver flag, not from here: the driver does not deliver `TurnStarted` to a runtime-events
/// sink, only the turn boundaries.
struct NamedTurns {
    last_outcome: Mutex<Option<TurnOutcome>>,
    turns: watch::Sender<u64>,
}

impl NamedTurns {
    fn on_turn_event(&self, event: &RuntimeEvent) {
        match event {
            RuntimeEvent::TurnFinished { outcome, .. } => {
                *self.last_outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(*outcome);
                self.signal();
            }
            RuntimeEvent::TurnFailed { .. } => {
                *self.last_outcome.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(TurnOutcome::Failed);
                self.signal();
            }
            _ => {}
        }
    }

    fn signal(&self) {
        self.turns
            .send_modify(|version| *version = version.wrapping_add(1));
    }
}

/// Wraps the host's `runtime_events` callback with the named turn tracker.
struct NamedRuntimeEvents {
    inner: Arc<dyn RuntimeEvents>,
    turns: Arc<NamedTurns>,
}

impl RuntimeEvents for NamedRuntimeEvents {
    fn on_runtime_event(&self, event: &RuntimeEvent) {
        self.inner.on_runtime_event(event);
        self.turns.on_turn_event(event);
    }
}

/// The registry a lead holds: live named agents by name (ADR 0030 O12).
pub struct NamedRegistry {
    /// The lead's canonical workspace (where a `shared` named agent runs).
    workspace: PathBuf,
    /// The worktree root for named agents, or `None` when the workspace is not a git repository.
    worktree_root: Option<WorktreeRoot>,
    /// The lead's queue: a named agent's approvals and questions surface there, with the named
    /// session id as the origin (U7).
    lead_queue: Arc<SessionQueue>,
    /// The lead's journal: `NamedAgentStarted` lands here (ADR 0030 O12).
    lead_journal: Arc<SessionJournal>,
    /// The resolved orchestration snapshot (ADR 0030 slice 9): the lead's trusted project agent
    /// dir (ADR 0030 O7 rev 2 item 4), resolved once, process-wide
    /// ([`orchestration::process`]), overridable with [`NamedRegistry::with_orchestration`]. Only
    /// the **definition dir** is read here: the snapshot's `isolation`/`model`/`profile` defaults
    /// are `spawn_task`'s (`SpawnDefaults`, ADR 0030 O7), a task-creation rule — a named agent is
    /// the human's own long session, so its definition and the host's own route set its model and
    /// tier, not the task spawn defaults.
    orchestration: Orchestration,
    builder: NamedBuilder,
    seams: NamedSeams,
    /// Serializes first-use creation, so two concurrent sends cannot open two sessions for one
    /// name. Held across `open_session`; not held on the reuse path.
    creating: AsyncMutex<()>,
    agents: Mutex<BTreeMap<String, Arc<NamedRecord>>>,
}

impl NamedRegistry {
    /// Builds the registry over the lead's workspace, journal and queue.
    pub fn new(
        workspace: impl Into<PathBuf>,
        lead_journal: Arc<SessionJournal>,
        lead_queue: Arc<SessionQueue>,
        builder: NamedBuilder,
    ) -> Self {
        let workspace = workspace.into();
        let workspace = std::fs::canonicalize(&workspace).unwrap_or(workspace);
        let worktree_root = task_worktree_root(&workspace);
        Self {
            workspace,
            worktree_root,
            lead_queue,
            lead_journal,
            // ADR 0030 O7 rev 2 item 4 / slice 9: the lead's trusted project agent dir comes from
            // the process orchestration snapshot (resolved once, from the lead's workspace),
            // exactly as the task registry reads it.
            orchestration: orchestration::process(),
            builder,
            seams: NamedSeams::default(),
            creating: AsyncMutex::new(()),
            agents: Mutex::new(BTreeMap::new()),
        }
    }

    /// Installs the resolved orchestration snapshot (ADR 0030 slice 9): the named path reads the
    /// lead's trusted project agent dir from it (ADR 0030 O7 rev 2 item 4), so a named agent is
    /// defined by the **lead's** workspace, never by its own worktree. This is the test seam;
    /// production reads the snapshot from [`orchestration::process`].
    pub fn with_orchestration(mut self, orchestration: Orchestration) -> Self {
        self.orchestration = orchestration;
        self
    }

    /// Installs the test seams.
    pub fn with_seams(mut self, seams: NamedSeams) -> Self {
        self.seams = seams;
        self
    }

    /// The lead's workspace.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The live named agent, if one has that name.
    pub fn get(&self, name: &str) -> Option<Arc<NamedRecord>> {
        self.agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    /// The live named-agent names, in order.
    pub fn names(&self) -> Vec<String> {
        self.agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// Sends one input to the named agent `name`, creating it from its definition on first use
    /// (ADR 0030 O12). The input goes to the `next_turn` lane (`followup`): a new task for after
    /// the current turn (ADR 0011 L1). `source` is recorded and applied to the turn: reset when
    /// the agent is idle, lowered only when it is busy (ADR 0012 decision 5).
    ///
    /// The returned [`SendOutcome::turn`] is the token to wait past, read from the same "busy"
    /// the enqueue sees: `G` when idle, `G + 1` when the input was queued behind a running turn.
    ///
    /// The future is **boxed** (a type-erased `BoxFuture`): a first send calls `open_session`,
    /// and a lead tool that calls `open_session` would otherwise recurse in the async `Send`
    /// inference (open_session's driver runs the tools, one of which is `send_agent`). Exactly
    /// like [`crate::task::TaskRegistry::spawn`].
    pub fn send<'a>(
        &'a self,
        name: &'a str,
        text: impl Into<String>,
        source: InputSource,
    ) -> futures_util::future::BoxFuture<'a, Result<SendOutcome, NamedError>> {
        let text = text.into();
        Box::pin(async move {
            validate_session_id(name).map_err(|_| {
                NamedError::new(format!(
                    "invalid agent name {name:?}: [A-Za-z0-9._-], 1-128 chars"
                ))
            })?;
            let (record, created) = self.get_or_create(name).await?;
            // Read "running" and the turn generation together, immediately before the enqueue, so
            // the "busy" answer describes the turn boundary the input lands on: the input always
            // goes to the `next_turn` lane, so it runs as the first turn after any turn that is
            // running at this moment.
            let generation = record.turn_generation();
            let queued = record.running();
            // The token is the generation our input's turn must **end past** (ADR 0030 O12
            // review fix 2). Idle: our turn is the next to end, so the generation now (`G`)
            // suffices. Busy: the running turn ends first (`G + 1`), so a token of `G` alone
            // would let a wait settle on **that** turn's reply in the idle moment before ours
            // starts — the common busy case. Hence `G + 1` whenever the send is queued.
            let turn = generation + u64::from(queued);
            // The turn's source: the input that starts a turn resets it; a mid-turn one only
            // lowers (ADR 0012 decision 5). This mirrors the CLI host's `set_turn_source`.
            if queued {
                record.session.hooks().observe_input_source(source);
            } else {
                record.session.hooks().set_turn_source(Some(source));
            }
            record
                .session
                .inbox()
                .followup(Message::user(text), source)
                .await
                .map_err(|error| {
                    NamedError::new(format!("cannot send to named agent `{name}`: {error}"))
                })?;
            Ok(SendOutcome {
                name: record.name.clone(),
                session_id: record.session.id().as_str().to_owned(),
                created,
                queued,
                turn,
                worktree: record.worktree.clone(),
                warnings: record.warnings.clone(),
            })
        })
    }

    /// Closes every named session's driver (ADR 0030 O12: "lead 关闭时一起关，worktree 保留").
    /// The worktrees and branches stay on disk.
    pub fn close_all(&self) {
        let records: Vec<Arc<NamedRecord>> = self
            .agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for record in records {
            record.session.stop();
        }
    }

    /// Returns the live record for `name`, creating it on first use (ADR 0030 O12).
    async fn get_or_create(&self, name: &str) -> Result<(Arc<NamedRecord>, bool), NamedError> {
        if let Some(record) = self.get(name) {
            return Ok((record, false));
        }
        // Serialize creation: two concurrent first sends must not both open a session.
        let _guard = self.creating.lock().await;
        if let Some(record) = self.get(name) {
            return Ok((record, false));
        }
        let record = self.create(name).await?;
        self.agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_owned(), Arc::clone(&record));
        Ok((record, true))
    }

    /// Creates one named agent's session from its definition (ADR 0030 O11/O12). Called under
    /// `creating`, so exactly one session is opened per name.
    async fn create(&self, name: &str) -> Result<Arc<NamedRecord>, NamedError> {
        if let Some(hook) = &self.seams.create {
            hook(name).map_err(NamedError::new)?;
        }
        let agent = resolve_named(
            name,
            radian_home().as_deref(),
            self.orchestration.project_agents.as_deref(),
        )
        .map_err(|error| NamedError::new(format!("cannot resolve agent `{name}`: {error}")))?;
        let isolation = agent.isolation.unwrap_or_default();
        // A `spawn: true` named agent is a lead (O13); the builder mounts the lead tools when the
        // session's bindings carry a task builder. A named agent without it mounts none.
        let (workspace, worktree, branch) = match isolation {
            Isolation::Worktree => {
                let root = self.worktree_root.as_ref().ok_or_else(|| {
                    NamedError::new(format!(
                        "cannot create named agent `{name}` in a worktree: {} is not a git \
                         repository, or there is no usable ~/.radian",
                        self.workspace.display()
                    ))
                })?;
                let worktree = root.root.join("agent").join(name);
                let branch = format!("radian/agent/{name}");
                let lead = self.workspace.clone();
                let home = root.home.clone();
                let wt = worktree.clone();
                let br = branch.clone();
                tokio::task::spawn_blocking(move || create_worktree(&lead, &home, &wt, &br))
                    .await
                    .map_err(|error| NamedError::new(format!("worktree task failed: {error}")))?
                    .map_err(|error| NamedError::new(error.to_string()))?;
                (worktree.clone(), Some(worktree), Some(branch))
            }
            Isolation::Shared => (self.workspace.clone(), None, None),
        };

        let parts = (self.builder)(&NamedContext {
            name,
            session_id: name,
            isolation,
            workspace: &workspace,
            branch: branch.as_deref(),
            agent: &agent,
        })
        .map_err(|error| NamedError::new(error.to_string()))?;
        let TaskSession {
            mut setup,
            mut bindings,
        } = parts;
        enforce_named_setup(&mut setup, &workspace, branch.clone(), &agent);
        // A named agent's approvals and questions go to the lead's queue (one order, U7): its
        // asks are answered by the human, not by a task approver.
        bindings.shared_queue = Some(Arc::clone(&self.lead_queue));
        bindings.approvals = ApprovalRouting::Queued;
        // The `spawn: true` capability (O13): without it, drop whatever the builder passed, so a
        // plain named agent can never open a task. With it, keep the builder's task builder (and
        // named builder), which is how a named lead gets its tools.
        if !agent.spawn {
            bindings.task_builder = None;
            bindings.named_builder = None;
        }

        let turns = Arc::new(NamedTurns {
            last_outcome: Mutex::new(None),
            turns: watch::channel(0u64).0,
        });
        let inner = Arc::clone(&bindings.runtime_events);
        bindings.runtime_events = Arc::new(NamedRuntimeEvents {
            inner,
            turns: Arc::clone(&turns),
        });

        let session = match open_session(setup, bindings).await {
            Ok(session) => Arc::new(session),
            Err(error) => {
                // The session never opened; drop the worktree so a retry is clean.
                if let Some(worktree) = worktree.clone() {
                    let lead = self.workspace.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        crate::task::remove_worktree(&lead, &worktree, true)
                    })
                    .await;
                }
                return Err(NamedError::new(error.to_string()));
            }
        };

        // ADR 0030 O11: a `skills` name discovery did not find is a warning, never a failure.
        let mut warnings = match &agent.skills {
            Some(allow) => crate::skills::missing(session.skills(), allow),
            None => Vec::new(),
        };
        // ADR 0030 O3: a `shared` agent has no branch of its own, so `git_commit` is not
        // mounted. A definition that lists it is told so, rather than silently losing the tool.
        if isolation == Isolation::Shared
            && agent
                .tools
                .as_deref()
                .is_some_and(|tools| tools.iter().any(|name| name == "git_commit"))
        {
            warnings.push(
                "`git_commit` is not mounted: a `shared` agent has no branch of its own \
                 (ADR 0030 O3)"
                    .to_owned(),
            );
        }

        // ADR 0030 O12: record the creation in the lead's journal so a resume can name it. After
        // the session exists, so a recorded agent is one that really opened.
        let event = RuntimeEvent::NamedAgentStarted {
            name: name.to_owned(),
            agent_session_id: session.id().as_str().to_owned(),
            worktree: worktree
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        };
        let _ = self.lead_journal.append_runtime_event(event).await;

        Ok(Arc::new(NamedRecord {
            name: name.to_owned(),
            session,
            isolation,
            worktree,
            branch,
            agent,
            warnings,
            turns,
        }))
    }
}

/// Enforces the named-agent invariants on a builder's setup (ADR 0030 O8/O11/O12). Split out so a
/// test can pin each rule without a real session.
fn enforce_named_setup(
    setup: &mut SessionSetup,
    workspace: &Path,
    branch: Option<String>,
    agent: &NamedAgent,
) {
    setup.workspace = workspace.to_path_buf();
    setup.coding_root = Some(workspace.to_path_buf());
    setup.parent = None;
    setup.allowed_branch = branch;
    // ADR 0030 O3: an agent commits only to its own branch. A worktree named agent has
    // `radian/agent/<name>`; a `shared` one has none, so it does not mount `git_commit` (and the
    // tool refuses if it reached it), which is what keeps it off the human's checked-out branch.
    setup.git_commit_requires_branch = true;
    // A named agent is not a task agent: `/mode` is allowed and `git_commit` does not refuse
    // `main` by the task rule (ADR 0038 decision 4).
    setup.task_agent = false;
    // ADR 0030 O8: a named agent does not open memory (its single-writer lock).
    setup.memory_root = None;
    // ADR 0036 / ADR 0030 O11: layer 1 is the definition's body, snapshotted at
    // creation. The session never re-reads the file.
    setup.role = Some(agent.name.clone());
    setup.system_prompt = Some(agent.body.clone());
    // ADR 0030 problem 7 = A: the `tools` allow-list narrows (deny everything it does not name),
    // and the mounted set is restricted to it.
    if let Some(tools) = agent.tools.as_deref() {
        let denied: Vec<String> = crate::task::builtin_tool_names()
            .into_iter()
            .filter(|name| !tools.iter().any(|allowed| allowed == name))
            .map(str::to_owned)
            .collect();
        setup.profile = setup.profile.clone().with_denied_tools(denied);
        setup.tools_allow = Some(tools.to_vec());
    }
    // ADR 0030 O11: the definition's `skills` list is the session's skill allow-list.
    setup.skills_allow = agent.skills.clone();
    // ADR 0030 O8: a named agent mounts no MCP server unless its definition names one.
    setup
        .servers
        .retain(|server| agent.mcp.iter().any(|name| name == &server.id));
    // The definition's `approval` field is a task layer (ADR 0030 O2 layer 3); a named agent is
    // the human's own session, so its approval policy is the host's, not the file's.
}

/// The last committed assistant text in the journal at `path`, or `None`. Reads through the
/// committed-facts observer, so an uncommitted replacement is never shown (ADR 0023 U6).
fn last_assistant_text(path: &Path) -> Option<String> {
    let mut observer = JournalObserver::open(path).ok()?;
    let _ = observer.poll();
    observer
        .history()
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Assistant { content, .. } => {
                let text = content
                    .iter()
                    .filter_map(|part| match part {
                        agent_core::ContentPart::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        })
}

/// The known named-agent definitions: the `~/.radian/agents/*.md` file stems plus the built-in
/// `default`/`task`/`lead`, sorted and deduped. Used by `/send` with no arguments.
pub fn known_definitions() -> Vec<String> {
    let mut names: Vec<String> = vec!["default".into(), "task".into(), "lead".into()];
    if let Some(dir) = crate::home::agents_dir()
        && let Ok(entries) = std::fs::read_dir(&dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(std::ffi::OsStr::to_str) == Some("md")
                && let Some(stem) = path.file_stem().and_then(std::ffi::OsStr::to_str)
            {
                names.push(stem.to_owned());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_definitions_lists_the_builtins_at_least() {
        let names = known_definitions();
        for builtin in ["default", "task", "lead"] {
            assert!(names.iter().any(|name| name == builtin), "{names:?}");
        }
    }
}
