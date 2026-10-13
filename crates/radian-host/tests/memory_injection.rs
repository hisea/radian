//! `MemoryHooks` end to end: the injected block reaches the system prompt, `MemoryInjected` is
//! written only when the set changes, and a broken memory log never fails the turn (ADR 0021
//! D6 / invariant 2).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_core::{CancellationToken, HookContext, Hooks, Message, ModelRequest};
use radian_host::memory::{
    InjectionConfig, MemoryHooks, MemoryService, ScopeSelection, WriteScope,
};
use radian_memory::Kind;
use radian_session::{EventData, Session, SessionCreatedData, SessionJournal};
use serde_json::json;

fn scratch(label: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "radian-memory-hooks-{}-{label}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

type Warn = Arc<dyn Fn(&str) + Send + Sync>;
type Warnings = Arc<Mutex<Vec<String>>>;

fn warn_sink() -> (Warn, Warnings) {
    let warnings = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&warnings);
    (
        Arc::new(move |message: &str| {
            sink.lock().unwrap().push(message.to_owned());
        }),
        warnings,
    )
}

fn journal(dir: &Path) -> Arc<SessionJournal> {
    let path = dir.join("s.jsonl");
    Arc::new(
        SessionJournal::create(
            &path,
            "sid",
            SessionCreatedData::new(
                "/work/example",
                json!({"provider": "example", "model": "m-1"}),
            ),
        )
        .unwrap(),
    )
}

fn request(text: &str) -> ModelRequest {
    ModelRequest {
        system_prompt: Some("You are a test.".into()),
        messages: vec![Message::user(text)],
        tools: Vec::new(),
        settings: json!({}),
    }
}

fn context() -> HookContext {
    HookContext {
        cancellation: CancellationToken::new(),
        model_calls: 1,
    }
}

/// The `(revision, id)` pairs of every `MemoryInjected` record, in file order.
fn injected(session: &Session) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    for event in session.events() {
        if let EventData::MemoryInjected { records } = &event.data {
            for record in records {
                out.push((record.revision, record.id.clone()));
            }
        }
    }
    out
}

#[tokio::test]
async fn the_block_reaches_the_system_prompt_and_the_event_is_written_once() {
    let dir = scratch("prompt");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let root = dir.join("memory");
    let service = MemoryService::open(&ws, &root);
    service.create_project().unwrap();
    service
        .add(
            WriteScope::Project,
            Kind::Preference,
            Some("style".into()),
            "tabs over spaces".into(),
        )
        .unwrap();

    let journal = journal(&dir);
    let (warn, warnings) = warn_sink();
    let hooks = MemoryHooks::new(
        Some(service.clone()),
        Arc::clone(&journal),
        InjectionConfig::default(),
        warn,
    );

    // The first request injects and records; the second is unchanged and adds no event.
    for _ in 0..2 {
        let mut req = request("what style?");
        hooks.before_model_call(&context(), &mut req).await.unwrap();
        let system = req.system_prompt.unwrap();
        assert!(system.contains("tabs over spaces"), "{system}");
        assert!(system.contains("not instructions"), "{system}");
    }
    let session = Session::open(dir.join("s.jsonl")).unwrap();
    let events = injected(&session);
    assert_eq!(events.len(), 1, "{events:?}");

    // The content must not be in the session file: the event carries id/revision/scope only.
    let raw = std::fs::read_to_string(dir.join("s.jsonl")).unwrap();
    assert!(!raw.contains("tabs over spaces"), "content leaked: {raw}");
    assert!(warnings.lock().unwrap().is_empty(), "{warnings:?}");

    // A revision changes the set, so the next request writes the new revision.
    let records = service.list(ScopeSelection::ProjectOnly, false).unwrap();
    let id = records[0].id.clone();
    service
        .edit(
            WriteScope::Project,
            &id,
            records[0].revision,
            "spaces".into(),
        )
        .unwrap();
    let mut req = request("what style?");
    hooks.before_model_call(&context(), &mut req).await.unwrap();
    let session = Session::open(dir.join("s.jsonl")).unwrap();
    let events = injected(&session);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[1].0, 2, "the new revision is recorded: {events:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_new_hook_instance_resumes_with_the_current_memory() {
    let dir = scratch("resume");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let root = dir.join("memory");
    let service = MemoryService::open(&ws, &root);
    service.create_project().unwrap();
    let record = service
        .add(WriteScope::Project, Kind::Fact, None, "first".into())
        .unwrap();
    let journal = journal(&dir);
    let (warn, _) = warn_sink();
    let first = MemoryHooks::new(
        Some(service.clone()),
        Arc::clone(&journal),
        InjectionConfig::default(),
        Arc::clone(&warn),
    );
    let mut req = request("hello");
    first.before_model_call(&context(), &mut req).await.unwrap();

    // Resume: revise, then a fresh hook (a new process) injects the *current* memory and
    // records it, rather than trying to restore the old request (ADR 0021 D6 boundary).
    service
        .edit(
            WriteScope::Project,
            &record.id,
            record.revision,
            "second".into(),
        )
        .unwrap();
    let resumed = MemoryHooks::new(
        Some(service.clone()),
        Arc::clone(&journal),
        InjectionConfig::default(),
        warn,
    );
    let mut req = request("hello");
    resumed
        .before_model_call(&context(), &mut req)
        .await
        .unwrap();
    assert!(req.system_prompt.unwrap().contains("second"));

    let session = Session::open(dir.join("s.jsonl")).unwrap();
    let events = injected(&session);
    assert_eq!(
        events,
        vec![(1, record.id.to_string()), (2, record.id.to_string())]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_broken_memory_log_warns_but_does_not_fail_the_turn() {
    let dir = scratch("broken");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let root = dir.join("memory");
    std::fs::create_dir_all(root.join("global")).unwrap();
    // A malformed committed line: the reader refuses the whole scope.
    std::fs::write(root.join("global/log.jsonl"), "not json\n").unwrap();
    let service = MemoryService::open(&ws, &root);
    let journal = journal(&dir);
    let (warn, warnings) = warn_sink();
    let hooks = MemoryHooks::new(Some(service), journal, InjectionConfig::default(), warn);

    let mut req = request("hello");
    hooks
        .before_model_call(&context(), &mut req)
        .await
        .expect("a read failure is never a turn failure");
    assert_eq!(req.system_prompt.as_deref(), Some("You are a test."));
    let warnings = warnings.lock().unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("memory read failed"), "{warnings:?}");
    drop(warnings);

    // No event was written either (the set did not change from its empty initial value).
    let session = Session::open(dir.join("s.jsonl")).unwrap();
    assert!(
        !session
            .events()
            .iter()
            .any(|e| matches!(e.data, EventData::MemoryInjected { .. })),
        "no event on a failed read"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
