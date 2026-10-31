//! `MemoryHooks` end to end: the injected block reaches the system prompt, no legacy
//! `MemoryInjected` record is written (this build records only index files, ADR 0021 D6), and a
//! broken memory log never fails the turn (ADR 0021 D6 / invariant 2).

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use radian_core::{CancellationToken, HookContext, Hooks, Message, ModelRequest};
use radian_host::memory::{
    InjectionConfig, MemoryHooks, MemoryInjection, MemoryService, ScopeSelection, WriteScope,
};
use radian_memory::Kind;
use radian_session::{EventData, Session, SessionCreatedData, SessionJournal};
use serde_json::json;

fn scratch(label: &str) -> common::TempDir {
    common::TempDir::named(&format!("radian-memory-hooks-{label}"))
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

/// How many `MemoryInjected` events a session carries. The in-process hook writes none: this
/// build's writer records injected index files only (ADR 0021 D6).
fn injected_events(session: &Session) -> usize {
    session
        .events()
        .iter()
        .filter(|event| matches!(event.data, EventData::MemoryInjected { .. }))
        .count()
}

#[tokio::test]
async fn the_block_reaches_the_system_prompt_and_writes_no_legacy_record() {
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

    let _journal = journal(&dir);
    let (warn, warnings) = warn_sink();
    let hooks = MemoryHooks::new(Some(service.clone()), InjectionConfig::default(), warn);

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

    // The second request is unchanged.
    let mut req = request("what style?");
    hooks.before_model_call(&context(), &mut req).await.unwrap();
    assert_eq!(req.system_prompt.as_deref(), Some(first_system.as_str()));
    let session = Session::open(dir.join("s.jsonl")).unwrap();
    assert_eq!(
        injected_events(&session),
        0,
        "this build writes only indexes (ADR 0021 D6)"
    );

    // The content must not be in the session file either.
    let raw = std::fs::read_to_string(dir.join("s.jsonl")).unwrap();
    assert!(!raw.contains("tabs over spaces"), "content leaked: {raw}");
    assert!(!raw.contains("MemoryInjected"), "no legacy record: {raw}");
    assert!(warnings.lock().unwrap().is_empty(), "{warnings:?}");

    // ADR 0021: a revision of the memory **does not** rewrite the block
    // in this process. The new revision reaches the model at the next session or resume.
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
    assert_eq!(
        injected_events(&session),
        0,
        "no legacy record: {session:?}"
    );

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
    // second turn's record is added **after** the block was computed, the way a record
    // written between two turns lands (ADR 0021, accepted cost (a)).
    service
        .add(
            WriteScope::Project,
            Kind::Fact,
            None,
            "the style is tabs".into(),
        )
        .unwrap();

    let (warn, warnings) = warn_sink();
    let injection = MemoryInjection::new(Some(service.clone()), InjectionConfig::default(), warn);
    let hooks = MemoryHooks::from_shared(Arc::clone(&injection));

    // The first turn fixes the block; the transcript is empty, so the ledger is too.
    let mut req = request("style");
    hooks.before_model_call(&context(), &mut req).await.unwrap();
    let fixed_system = req.system_prompt.clone().unwrap();
    assert!(fixed_system.contains("the style is tabs"), "{fixed_system}");

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
    assert_eq!(
        message.meta().source,
        Some(radian_core::InputSource::System)
    );
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
            content: vec![radian_core::ContentPart::text("This is a summary.")],
            meta: radian_core::MessageMeta {
                id: None,
                source: Some(radian_core::InputSource::System),
                origin: Some(
                    radian_core::OpaqueJson::parse(r#"{"kind":"compaction_summary"}"#).unwrap(),
                ),
            },
        },
        Message::user("second"),
    ];
    let again = injection
        .addition_for_turn("parser", &after_compaction)
        .expect("the id left the ledger with the compacted message");
    assert!(again.text().contains("src/parse.rs"), "{}", again.text());

    // The fixed block's bytes do not move when an addition joins the transcript.
    let mut req = request("parser");
    req.messages = with_addition.clone();
    hooks.before_model_call(&context(), &mut req).await.unwrap();
    assert_eq!(req.system_prompt.as_deref(), Some(fixed_system.as_str()));

    // A turn whose text hits nothing new appends nothing.
    assert!(
        injection
            .addition_for_turn("nothing to see here", &[])
            .is_none(),
        "a turn with no hit sends nothing"
    );
    // A record the fixed block already carries is never appended, even when the turn hits it.
    assert!(
        injection.addition_for_turn("style", &transcript).is_none(),
        "a record in the fixed block is not appended again"
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
    let (warn, _) = warn_sink();
    let config = InjectionConfig {
        addition_budget: 120,
        ..InjectionConfig::default()
    };
    let injection = MemoryInjection::new(Some(service.clone()), config, Arc::clone(&warn));
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
    let injection = MemoryInjection::new(Some(service), off, warn);
    assert!(
        injection.addition_for_turn("parser", &transcript).is_none(),
        "additions = false sends nothing"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_hook_orders_the_block_by_the_first_user_message() {
    let dir = scratch("hook-first");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let service = MemoryService::open(&ws, dir.join("memory"));
    service.create_project().unwrap();
    for content in ["about parsers", "about styles"] {
        service
            .add(WriteScope::Project, Kind::Fact, None, content.into())
            .unwrap();
    }
    let (warn, _) = warn_sink();
    let hooks = MemoryHooks::new(Some(service), InjectionConfig::default(), warn);

    // The request is a later turn of a session: the first user message is the session's first,
    // and the last is this turn's. Only the first may decide the order (decision 1). In a
    // normal turn the addition seam computes the block first, so this is the hook's own path —
    // the one a process with `additions = false` takes.
    let mut req = request("styles");
    req.messages = vec![
        Message::user("parsers"),
        Message::assistant("ok"),
        Message::user("styles"),
    ];
    hooks.before_model_call(&context(), &mut req).await.unwrap();
    let system = req.system_prompt.unwrap();
    let parsers = system.find("about parsers").expect("injected");
    let styles = system.find("about styles").expect("injected");
    assert!(
        parsers < styles,
        "the block is ranked by the first user message, not the last:\n{system}"
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
    let (warn, _) = warn_sink();
    let first = MemoryHooks::new(
        Some(service.clone()),
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
    let resumed = MemoryHooks::new(Some(service.clone()), InjectionConfig::default(), warn);
    let mut req = request("hello");
    resumed
        .before_model_call(&context(), &mut req)
        .await
        .unwrap();
    assert!(req.system_prompt.unwrap().contains("second"));

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
    let _journal = journal(&dir);
    let (warn, warnings) = warn_sink();
    let hooks = MemoryHooks::new(Some(service), InjectionConfig::default(), warn);

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

    // No record is written either.
    let session = Session::open(dir.join("s.jsonl")).unwrap();
    assert_eq!(injected_events(&session), 0, "no event on a failed read");
    let _ = std::fs::remove_dir_all(&dir);
}
