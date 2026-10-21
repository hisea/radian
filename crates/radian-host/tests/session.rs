//! The Session Facade (ADR 0023 U5): the capability list is the host's, and the facade
//! re-validates a command before it runs — the host does not trust the UI's copy.
//!
//! `open_session` is the real in-process implementation, so this test opens a real session
//! (with a mock model that is never called) and checks the refusal.

use std::sync::Arc;

use agent_core::{NoopSink, TurnOptions};
use radian_host::{
    HostBindings, SessionCapability, SessionCommand, SessionFacade, SessionId, SessionProjection,
    SessionSetup, build_model, open_session,
};
use radian_permissions::{
    ApprovalError, ApprovalOutcome, ApprovalRequest, Approver, NoopAudit, Preset, Profile,
};
use serde_json::json;

struct AllowAll;

impl Approver for AllowAll {
    fn decide(
        &self,
        _request: ApprovalRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<ApprovalOutcome, ApprovalError>> {
        Box::pin(async { Ok(ApprovalOutcome::AllowOnce) })
    }
}

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "radian-host-session-{label}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Opens a session that declares only `Observe`.
async fn observe_only(label: &str) -> (Scratch, Arc<radian_host::LocalSession>) {
    open_with(label, vec![SessionCapability::Observe]).await
}

/// Opens a session with `capabilities`.
async fn open_with(
    label: &str,
    capabilities: Vec<SessionCapability>,
) -> (Scratch, Arc<radian_host::LocalSession>) {
    open_with_resume(label, capabilities, false).await
}

/// [`open_with`] with an explicit `resume`: `resume` writes a small journal first.
async fn open_with_resume(
    label: &str,
    capabilities: Vec<SessionCapability>,
    resume: bool,
) -> (Scratch, Arc<radian_host::LocalSession>) {
    let records = resume.then(|| {
        vec![
            agent_core::JournalRecord::Message {
                index: 0,
                message: agent_core::Message::user("go"),
            },
            agent_core::JournalRecord::Message {
                index: 1,
                message: agent_core::Message::assistant("ok"),
            },
        ]
    });
    open_with_records(label, capabilities, records).await
}

/// Opens a session whose file is the given records (`None` = a new session). A `Some` list is
/// written first, so the session resumes it.
async fn open_with_records(
    label: &str,
    capabilities: Vec<SessionCapability>,
    records: Option<Vec<agent_core::JournalRecord>>,
) -> (Scratch, Arc<radian_host::LocalSession>) {
    let scratch = Scratch::new(label);
    let workspace = scratch.0.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let resume = records.is_some();
    if let Some(records) = records {
        let path = scratch.0.join(format!("{label}.jsonl"));
        let journal = radian_session::SessionJournal::create(
            &path,
            label,
            radian_session::SessionCreatedData::new(workspace.to_string_lossy(), json!({})),
        )
        .unwrap();
        for record in records {
            agent_core::TranscriptJournal::append(&journal, record)
                .await
                .unwrap();
        }
        drop(journal);
    }
    let config_slot = radian_runtime::ConfigSlot::new(serde_json::Value::Null);
    let model = build_model(radian_host::ModelArgs {
        provider: radian_host::Provider::OpenAiCompatible,
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "mock".into(),
        api_key: None,
        headers: Vec::new(),
        include_usage: None,
        max_tokens: 1024,
        thinking: radian_host::Thinking::Off,
        reasoning_effort: None,
        reasoning_replay: None,
        chatgpt_oauth: false,
    })
    .unwrap();
    let model = radian_host::SwitchableModel::new(
        model,
        radian_host::ModelRoute::new("openai-compatible", "mock"),
        config_slot.clone(),
        radian_host::RouteContext {
            providers: std::collections::BTreeMap::new(),
            session_id: label.to_owned(),
            user_headers: Vec::new(),
            include_usage: None,
            max_tokens: 1024,
            thinking: "medium".to_owned(),
            thinking_budget: None,
        },
    );
    let profile = Profile::full_access(&workspace).unwrap();
    let session = open_session(
        SessionSetup {
            session_id: SessionId::new(label),
            session_dir: scratch.0.clone(),
            workspace: workspace.clone(),
            coding_root: None,
            model,
            config_slot: config_slot.clone(),
            config_queue: radian_runtime::ConfigQueue::new(),
            budget_for: None,
            price_for: None,
            shell_auto_background: None,
            servers: Vec::new(),
            mcp_config_path: None,
            system_prompt: None,
            context_budget: None,
            window_context: None,
            auto_compact: false,
            compaction_settings: serde_json::Value::Null,
            compaction_settings_for: None,
            retries: 0,
            sandbox: None,
            tier: Preset::FullAccess,
            profile,
            access_builder: None,
            parent: None,
            subagent: None,
            memory_root: None,
            skills_extra_paths: Vec::new(),
            skills_builtin: true,
            skills_disabled: Vec::new(),
            tools_allow: None,
            skills_allow: None,
            memory_injection: radian_host::memory::InjectionConfig::default(),
            hooks: Default::default(),
            trust: radian_host::trust::Setup::untrusted(&workspace, false),
            resume,
            config_snapshot: json!({"provider": "openai-compatible", "model": "mock"}),
            turn_options: TurnOptions::default(),
            turn_timeout: None,
            wording: radian_host::CLI_WORDING,
            capabilities,
        },
        HostBindings {
            approvals: radian_host::ApprovalRouting::Direct(Arc::new(AllowAll)),
            shared_queue: None,
            audit: Arc::new(|_journal| {
                Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
            }),
            events: Arc::new(NoopSink),
            projection: Arc::new(SessionProjection::new()),
            runtime_events: Arc::new(|_event: &radian_session::RuntimeEvent| {}),
            driver_stopped: Arc::new(|_| {}),
            ask_user: false,
            before_driver: None,
            memory_warning: Arc::new(|_message: &str| {}),
            hooks_warning: Arc::new(|_message: &str| {}),
            wake_warning: Arc::new(|_message: &str| {}),
        },
    )
    .await
    .unwrap();
    (scratch, Arc::new(session))
}

/// The facade declares `Observe` only, so a send is refused **by the facade**, not only by the
/// UI's key handling.
#[tokio::test]
async fn a_local_session_refuses_a_command_it_does_not_declare() {
    let (scratch, session) = observe_only("refuse").await;
    assert_eq!(session.capabilities(), [SessionCapability::Observe]);

    let refused = session
        .command(SessionCommand::SendFollowup("hi".into()))
        .expect_err("a session that cannot send must refuse the command");
    assert_eq!(refused.command, "send");
    assert_eq!(refused.capability.as_deref(), Some("send"));
    assert!(
        refused.reason.contains("does not declare"),
        "{}",
        refused.reason
    );

    // Every command that needs an undeclared capability is refused, and the projection is
    // still readable (the facade serves `Observe`).
    assert!(session.command(SessionCommand::Cancel).is_err());
    assert_eq!(session.id().as_str(), "refuse");
    assert_eq!(
        session.projection().read().turn(),
        radian_host::TurnState::Idle
    );

    session.stop();
    assert!(session.join().await.is_ok());
    drop(scratch);
}

/// A session that declares the capability accepts the command (the check is not "refuse
/// everything").
#[tokio::test]
async fn a_local_session_accepts_a_declared_command() {
    let scratch = Scratch::new("accept");
    let workspace = scratch.0.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let config_slot = radian_runtime::ConfigSlot::new(serde_json::Value::Null);
    let model = build_model(radian_host::ModelArgs {
        provider: radian_host::Provider::OpenAiCompatible,
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "mock".into(),
        api_key: None,
        headers: Vec::new(),
        include_usage: None,
        max_tokens: 1024,
        thinking: radian_host::Thinking::Off,
        reasoning_effort: None,
        reasoning_replay: None,
        chatgpt_oauth: false,
    })
    .unwrap();
    let model = radian_host::SwitchableModel::new(
        model,
        radian_host::ModelRoute::new("openai-compatible", "mock"),
        config_slot.clone(),
        radian_host::RouteContext {
            providers: std::collections::BTreeMap::new(),
            session_id: "can-send".to_owned(),
            user_headers: Vec::new(),
            include_usage: None,
            max_tokens: 1024,
            thinking: "medium".to_owned(),
            thinking_budget: None,
        },
    );
    let profile = Profile::full_access(&workspace).unwrap();
    let session = open_session(
        SessionSetup {
            session_id: SessionId::new("can-send"),
            session_dir: scratch.0.clone(),
            workspace: workspace.clone(),
            coding_root: None,
            model,
            config_slot: config_slot.clone(),
            config_queue: radian_runtime::ConfigQueue::new(),
            budget_for: None,
            price_for: None,
            shell_auto_background: None,
            servers: Vec::new(),
            mcp_config_path: None,
            system_prompt: None,
            context_budget: None,
            window_context: None,
            auto_compact: false,
            compaction_settings: serde_json::Value::Null,
            compaction_settings_for: None,
            retries: 0,
            sandbox: None,
            tier: Preset::FullAccess,
            profile,
            access_builder: None,
            parent: None,
            subagent: None,
            memory_root: None,
            skills_extra_paths: Vec::new(),
            skills_builtin: true,
            skills_disabled: Vec::new(),
            tools_allow: None,
            skills_allow: None,
            memory_injection: radian_host::memory::InjectionConfig::default(),
            hooks: Default::default(),
            trust: radian_host::trust::Setup::untrusted(&workspace, false),
            resume: false,
            config_snapshot: json!({}),
            turn_options: TurnOptions::default(),
            turn_timeout: None,
            wording: radian_host::CLI_WORDING,
            capabilities: SessionCapability::ALL.to_vec(),
        },
        HostBindings {
            approvals: radian_host::ApprovalRouting::Direct(Arc::new(AllowAll)),
            shared_queue: None,
            audit: Arc::new(|_journal| {
                Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
            }),
            events: Arc::new(NoopSink),
            projection: Arc::new(SessionProjection::new()),
            runtime_events: Arc::new(|_event: &radian_session::RuntimeEvent| {}),
            driver_stopped: Arc::new(|_| {}),
            ask_user: false,
            before_driver: None,
            memory_warning: Arc::new(|_message: &str| {}),
            hooks_warning: Arc::new(|_message: &str| {}),
            wake_warning: Arc::new(|_message: &str| {}),
        },
    )
    .await
    .unwrap();
    let session = Arc::new(session);
    // `command` blocks on the inbox, so it must not run on a runtime thread: a real host calls
    // it from its synchronous UI thread, and this test uses a blocking thread.
    let command = Arc::clone(&session);
    tokio::task::spawn_blocking(move || command.command(SessionCommand::SendFollowup("go".into())))
        .await
        .unwrap()
        .expect("a declared capability accepts the command");
    // The command returned `Ok`, which is the whole assertion: a declared capability accepts
    // it. (The driver may already have claimed the turn, so the inbox need not still hold it.)
    session.stop();
    let _ = session.join().await;
}

/// The compact path checks the capability too (PR ④'s review found this unchecked). A session
/// that declares only `Observe` must refuse it **immediately**, without touching the inbox:
/// a real request would be async (it is a control request the driver answers).
#[tokio::test]
async fn a_local_session_without_the_capability_refuses_compact() {
    let (scratch, session) = observe_only("compact").await;
    assert_eq!(session.capabilities(), [SessionCapability::Observe]);

    let (tx, mut rx) = tokio::sync::oneshot::channel();
    session.compact(
        None,
        Box::new(move |result| {
            let _ = tx.send(result);
        }),
    );
    let result = rx
        .try_recv()
        .expect("a refused compact must report immediately, without an inbox request");
    assert!(
        result.unwrap_err().contains("does not declare"),
        "the refusal names the missing capability"
    );

    session.stop();
    let _ = session.join().await;
    drop(scratch);
}

/// The answer path checks the capability too (ADR 0023 U5's `AnswerRequest`): a facade that
/// declares only `Observe` cannot answer an approval, so a UI that ignored the capability
/// list still cannot resolve one.
#[tokio::test]
async fn a_local_session_without_the_capability_refuses_an_answer() {
    let (scratch, session) = observe_only("answer").await;
    assert_eq!(session.capabilities(), [SessionCapability::Observe]);

    let refused = session
        .answer(
            1,
            radian_host::PendingResponse::Approval(ApprovalOutcome::AllowOnce),
        )
        .expect_err("a session that cannot answer must refuse");
    assert_eq!(refused.command, "answer");
    assert_eq!(refused.capability.as_deref(), Some("answer"));

    session.stop();
    let _ = session.join().await;
    drop(scratch);
}

/// A late answer (the request is already gone) is silently ignored, not an error: ADR 0024 Q4
/// says a request that was already completed or abandoned must not be completed twice. The
/// facade hides `Answered::Late`/`Unknown` from the UI, which has nothing useful to do with it.
#[tokio::test]
async fn a_late_answer_is_silently_ignored() {
    let (scratch, session) = open_with("late", SessionCapability::ALL.to_vec()).await;
    assert!(session.queue().is_empty());
    session
        .answer(
            7,
            radian_host::PendingResponse::Approval(ApprovalOutcome::AllowOnce),
        )
        .expect("a late answer is ignored, not refused");
    assert!(session.queue().is_empty());
    session.stop();
    let _ = session.join().await;
    drop(scratch);
}

/// ADR 0023 U7 / ADR 0024 Q4: closing a session completes its pending requests — an approval
/// is denied fail-closed and a question gets `PromptError::Gone` — so nothing is left waiting.
#[tokio::test]
async fn stopping_a_session_aborts_its_pending_requests() {
    use std::time::Duration;

    let (scratch, session) = open_with("stop", SessionCapability::ALL.to_vec()).await;
    let origin = session.id().as_str().to_string();
    let (_approval_id, approval) = session.queue().push(
        origin.clone(),
        radian_host::PendingRequest::Approval(radian_permissions::ApprovalRequest::new(
            "perm-1",
            "call-1",
            "write",
            "/ws/out.txt",
            "/ws/out.txt",
            json!({}),
            None,
        )),
    );
    let (_question_id, question) = session.queue().push(
        origin,
        radian_host::PendingRequest::Question(radian_coding::QuestionRequest::new(Vec::new())),
    );
    assert_eq!(session.queue().len(), 2);

    session.stop();

    let approval = tokio::time::timeout(Duration::from_secs(2), approval.wait())
        .await
        .expect("the approval must not hang")
        .expect("the queue must complete it");
    assert_eq!(
        approval,
        radian_host::PendingResponse::Approval(ApprovalOutcome::Deny {
            reason: Some("session closed".into())
        }),
        "an approval is denied fail-closed when the session closes"
    );
    let question = tokio::time::timeout(Duration::from_secs(2), question.wait())
        .await
        .expect("the question must not hang")
        .expect("the queue must complete it");
    assert_eq!(
        question,
        radian_host::PendingResponse::Question(Err(radian_coding::PromptError::Gone)),
        "a question is aborted when the session closes"
    );
    assert!(session.queue().is_empty());
    let _ = session.join().await;
    drop(scratch);
}

/// resume-perf B: a resumed journal is parsed **once**. The host parses it, the journal open
/// reuses that parse, and the driver's recovery reuses it too; only a written replacement
/// abort makes the driver re-parse. The count, not a wall clock, is the assertion.
#[tokio::test]
async fn a_resume_parses_the_journal_once() {
    radian_session::testing::reset_open_count();
    let (scratch, session) =
        open_with_resume("parse-once", vec![SessionCapability::Observe], true).await;
    assert_eq!(
        radian_session::testing::open_count(),
        1,
        "a resumed journal must be parsed exactly once"
    );
    session.stop();
    let _ = session.join().await;
    drop(scratch);
}

/// A session file that ends inside an uncommitted replacement segment is aborted when it is
/// resumed (`SessionJournal::into_journal` reports `wrote_abort`). The driver must recover the
/// transcript the **disk** fold has - it may not reuse the parse taken before the abort line
/// was written. The pending segment's messages are dropped either way, so this also pins that
/// the abort closes the segment the way the fold on disk does.
#[tokio::test]
async fn a_resume_aborts_a_pending_segment_and_the_driver_matches_the_disk_fold() {
    use agent_core::{JournalRecord, Message};
    let records = vec![
        JournalRecord::Message {
            index: 0,
            message: Message::user("old zero"),
        },
        JournalRecord::Message {
            index: 1,
            message: Message::user("old one"),
        },
        JournalRecord::TranscriptReplaced {
            revision: 7,
            len: 4,
        },
        JournalRecord::Message {
            index: 0,
            message: Message::user("pending zero"),
        },
        JournalRecord::Message {
            index: 1,
            message: Message::user("pending one"),
        },
    ];
    let (scratch, session) = open_with_records(
        "resume-abort",
        vec![SessionCapability::Observe],
        Some(records),
    )
    .await;
    // The resume wrote the abort line; the fold on disk is the authority.
    let path = scratch.0.join("resume-abort.jsonl");
    let disk = radian_session::Session::open(&path).unwrap();
    assert_eq!(
        disk.uncommitted_replacements().len(),
        1,
        "the pending segment is closed as aborted"
    );
    let disk = disk.history().unwrap();
    assert_eq!(
        disk,
        vec![Message::user("old zero"), Message::user("old one")],
        "the pending segment's messages are dropped"
    );
    assert_eq!(
        session.history, disk,
        "the driver's recovered transcript must match the disk fold"
    );
    session.stop();
    let _ = session.join().await;
    drop(scratch);
}

/// resume-perf B/resume-perf2: the host's initial transcript is the one the driver recovered
/// from the **single** parse, so it must equal opening and folding the file. This is the
/// promise step 2 makes, checked where a second fold could drift: a committed compaction
/// replacement (ADR 0018/0019) plus a pending one at end of file (which the resume aborts).
/// The comparison is the assertion; no wall clock.
#[tokio::test]
async fn a_resumed_history_matches_the_disk_fold_through_compaction_and_replacement() {
    use agent_core::{JournalRecord, Message};

    let records = vec![
        JournalRecord::Message {
            index: 0,
            message: Message::user("old zero"),
        },
        JournalRecord::Message {
            index: 1,
            message: Message::assistant("old one"),
        },
        // A committed compaction: the summary at 0, two kept messages after it.
        JournalRecord::ContextCompacted {
            revision: 1,
            source_revision: 0,
            source_len: 2,
            summary: "summary of the above".to_owned(),
            first_kept: 1,
            tokens_before: 1_000,
            tokens_after: 100,
            below_target: false,
            model: "mock".to_owned(),
            deterministic: false,
        },
        JournalRecord::TranscriptReplaced {
            revision: 1,
            len: 3,
        },
        JournalRecord::Message {
            index: 0,
            message: Message::user("summary of the above"),
        },
        JournalRecord::Message {
            index: 1,
            message: Message::user("kept zero"),
        },
        JournalRecord::Message {
            index: 2,
            message: Message::assistant("kept one"),
        },
        JournalRecord::TranscriptReplacementCommitted {
            revision: 1,
            len: 3,
        },
        // A second replacement left pending at end of file: the resume aborts it, so its
        // messages are dropped and the history stays the committed one.
        JournalRecord::TranscriptReplaced {
            revision: 2,
            len: 2,
        },
        JournalRecord::Message {
            index: 0,
            message: Message::user("pending zero"),
        },
        JournalRecord::Message {
            index: 1,
            message: Message::user("pending one"),
        },
    ];
    let (scratch, session) = open_with_records(
        "resume-history",
        vec![SessionCapability::Observe],
        Some(records),
    )
    .await;
    let path = scratch.0.join("resume-history.jsonl");
    let disk = radian_session::Session::open(&path)
        .unwrap()
        .history()
        .unwrap();
    assert_eq!(
        session.history, disk,
        "the host's initial view must be exactly the disk fold"
    );
    assert!(
        !session.history.iter().any(|m| m.text() == "pending zero"),
        "the pending replacement's messages are dropped"
    );
    assert!(
        session.history.iter().any(|m| m.text() == "kept zero"),
        "the committed replacement's messages stand"
    );
    session.stop();
    let _ = session.join().await;
    drop(scratch);
}
