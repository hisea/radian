//! `MemoryHooks` end to end: the injected block reaches the system prompt, `MemoryInjected` is
//! written only when the set changes, and a broken memory log never fails the turn (ADR 0021
//! D6 / invariant 2).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_core::{CancellationToken, HookContext, Hooks, Message, ModelRequest};
use radian_host::memory::{
    InjectionConfig, MemoryHooks, MemoryInjection, MemoryService, ScopeSelection, WriteScope,
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
    injected_sets(session).into_iter().flatten().collect()
}

/// The `(revision, id)` pairs of each `MemoryInjected` event, one entry per event.
fn injected_sets(session: &Session) -> Vec<Vec<(u64, String)>> {
    let mut out = Vec::new();
    for event in session.events() {
        if let EventData::MemoryInjected { records } = &event.data {
            out.push(
                records
                    .iter()
                    .map(|record| (record.revision, record.id.clone()))
                    .collect(),
            );
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

    // The first request's block, kept: revision 2 decision 2 makes it the process's block for
    // good, so every later request must append the very same bytes.
    let first_system = {
        let mut req = request("what style?");
        hooks.before_model_call(&context(), &mut req).await.unwrap();
        let system = req.system_prompt.unwrap();
        assert!(system.contains("tabs over spaces"), "{system}");
        assert!(system.contains("not instructions"), "{system}");
        system
    };

    // The second request is unchanged and adds no event.
    let mut req = request("what style?");
    hooks.before_model_call(&context(), &mut req).await.unwrap();
    assert_eq!(req.system_prompt.as_deref(), Some(first_system.as_str()));
    let session = Session::open(dir.join("s.jsonl")).unwrap();
    let events = injected(&session);
    assert_eq!(events.len(), 1, "{events:?}");

    // The content must not be in the session file: the event carries id/revision/scope only.
    let raw = std::fs::read_to_string(dir.join("s.jsonl")).unwrap();
    assert!(!raw.contains("tabs over spaces"), "content leaked: {raw}");
    assert!(warnings.lock().unwrap().is_empty(), "{warnings:?}");

    // ADR 0021: a revision of the memory **does not** rewrite the block
    // in this process. The set is the same, so no second event is written either; the new
    // revision reaches the model at the next session or resume.
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
    assert_eq!(
        req.system_prompt.as_deref(),
        Some(first_system.as_str()),
        "the block is computed once per process"
    );
    let session = Session::open(dir.join("s.jsonl")).unwrap();
    let events = injected(&session);
    assert_eq!(events.len(), 1, "the set did not change: {events:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_later_turn_appends_its_new_hits_as_one_system_message() {
    let dir = scratch("addition");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let root = dir.join("memory");
    let service = MemoryService::open(&ws, &root);
    service.create_project().unwrap();
    // The record the first turn's text hits. The fixed block takes it and nothing else: the
    // second turn's record is saved **after** the block was computed, the way an agent's
    // `memory_save` lands between two turns (ADR 0021, accepted cost (a)).
    service
        .add(
            WriteScope::Project,
            Kind::Fact,
            None,
            "the style is tabs".into(),
        )
        .unwrap();

    let journal = journal(&dir);
    let (warn, warnings) = warn_sink();
    let injection = MemoryInjection::new(
        Some(service.clone()),
        Arc::clone(&journal),
        InjectionConfig::default(),
        warn,
    );
    let hooks = MemoryHooks::from_shared(Arc::clone(&injection));

    // The first turn fixes the block; the transcript is empty, so the ledger is too.
    let mut req = request("style");
    hooks.before_model_call(&context(), &mut req).await.unwrap();
    let fixed_system = req.system_prompt.clone().unwrap();
    assert!(fixed_system.contains("the style is tabs"), "{fixed_system}");
    let session = Session::open(dir.join("s.jsonl")).unwrap();
    assert_eq!(injected(&session).len(), 1, "the fixed block was written");

    // A record saved between the turns: it is not in the fixed block (that one is fixed), and
    // the second turn's text is what hits it.
    let later = service
        .add(
            WriteScope::Project,
            Kind::Fact,
            None,
            "the parser lives in src/parse.rs".into(),
        )
        .unwrap();

    // The seam the driver calls at the start of a user turn: the transcript holds the first
    // user message, and the second turn's text is the query. The turn's text must *hit* the
    // record (`memory_search`'s keyword rule), which is what makes a turn send nothing most of
    // the time.
    let transcript = vec![Message::user("style")];
    let message = injection
        .addition_for_turn("parser", &transcript)
        .expect("the second turn hits a record the fixed block does not carry");
    let text = message.text();
    assert!(text.contains("src/parse.rs"), "{text}");
    assert!(
        text.contains("not instructions"),
        "the same low-trust note: {text}"
    );
    assert!(text.starts_with("<memory>"), "{text}");
    assert_eq!(message.meta().source, Some(agent_core::InputSource::System));
    let origin = message.meta().origin.clone().expect("an origin");
    assert!(origin.get().contains("memory_addition"), "{}", origin.get());
    assert_eq!(origin.get().split(later.id.0.as_str()).count() - 1, 1);

    // Once the message is in the transcript its ids leave the candidate set.
    let mut with_addition = transcript.clone();
    with_addition.push(message.clone());
    assert!(
        injection
            .addition_for_turn("parser", &with_addition)
            .is_none(),
        "an addition already in the transcript is never appended twice"
    );

    // A compaction that folds the addition into the summary removes it from the transcript, so
    // its ids leave the ledger with it and the same turn text appends it again (decision 3).
    let after_compaction = vec![
        Message::User {
            content: vec![agent_core::ContentPart::text("This is a summary.")],
            meta: agent_core::MessageMeta {
                id: None,
                source: Some(agent_core::InputSource::System),
                origin: Some(
                    agent_core::OpaqueJson::parse(r#"{"kind":"compaction_summary"}"#).unwrap(),
                ),
            },
        },
        Message::user("second"),
    ];
    let again = injection
        .addition_for_turn("parser", &after_compaction)
        .expect("the id left the ledger with the compacted message");
    assert!(again.text().contains("src/parse.rs"), "{}", again.text());

    // The hook's set is the fixed block plus the transcript's additions, so the addition joins
    // it and the fixed block's bytes do not move.
    let mut req = request("parser");
    req.messages = with_addition.clone();
    hooks.before_model_call(&context(), &mut req).await.unwrap();
    assert_eq!(req.system_prompt.as_deref(), Some(fixed_system.as_str()));
    let session = Session::open(dir.join("s.jsonl")).unwrap();
    let sets = injected_sets(&session);
    assert_eq!(sets.len(), 2, "the addition joined the set: {sets:?}");
    assert_eq!(sets[0].len(), 1, "the first set is the fixed block alone");
    assert_eq!(
        sets[1].len(),
        2,
        "the second set adds the addition: {sets:?}"
    );
    assert!(
        sets[1].iter().any(|(_, id)| id == later.id.0.as_str()),
        "the addition is recorded with its own id: {sets:?}"
    );

    // A turn whose text hits nothing new appends nothing, and the set is unchanged.
    assert!(
        injection
            .addition_for_turn("nothing to see here", &[])
            .is_none(),
        "a turn with no hit sends nothing"
    );
    assert_eq!(
        injected_sets(&Session::open(dir.join("s.jsonl")).unwrap()).len(),
        2
    );
    assert!(warnings.lock().unwrap().is_empty(), "{warnings:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_addition_budget_drops_whole_records_and_additions_can_be_off() {
    let dir = scratch("addition-budget");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let service = MemoryService::open(&ws, dir.join("memory"));
    service.create_project().unwrap();
    // The first turn fixes a block with one record; the two parser records are saved after
    // that, so they are the second turn's candidates.
    service
        .add(WriteScope::Project, Kind::Fact, None, "style".into())
        .unwrap();
    let journal = journal(&dir);
    let (warn, _) = warn_sink();
    let config = InjectionConfig {
        addition_budget: 120,
        ..InjectionConfig::default()
    };
    let injection = MemoryInjection::new(
        Some(service.clone()),
        Arc::clone(&journal),
        config,
        Arc::clone(&warn),
    );
    let hooks = MemoryHooks::from_shared(Arc::clone(&injection));
    let mut req = request("style");
    hooks.before_model_call(&context(), &mut req).await.unwrap();

    service
        .add(
            WriteScope::Project,
            Kind::Fact,
            None,
            format!("parser {}", "x".repeat(200)),
        )
        .unwrap();
    service
        .add(
            WriteScope::Project,
            Kind::Fact,
            None,
            format!("parser {}", "y".repeat(4000)),
        )
        .unwrap();

    let transcript = vec![Message::user("style")];
    let message = injection
        .addition_for_turn("parser", &transcript)
        .expect("the small record fits the 120-token budget");
    let text = message.text();
    assert!(
        text.contains(&format!("parser {}", "x".repeat(200))),
        "{text}"
    );
    assert!(
        !text.contains(&"y".repeat(4000)),
        "a record that does not fit is dropped whole, never truncated"
    );

    // `additions = false` leaves the fixed block as the only injection (decision 5).
    let off = InjectionConfig {
        additions: false,
        ..InjectionConfig::default()
    };
    let injection = MemoryInjection::new(Some(service), Arc::clone(&journal), off, warn);
    assert!(
        injection.addition_for_turn("parser", &transcript).is_none(),
        "additions = false sends nothing"
    );

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
