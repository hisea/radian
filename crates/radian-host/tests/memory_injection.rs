//! `MemoryHooks` end to end: the index section reaches the system prompt, `MemoryInjected` names
//! the injected index files (ADR 0021 D6), and a broken index never fails the turn (invariant 2).

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use radian_core::{CancellationToken, HookContext, Hooks, Message, ModelRequest};
use radian_host::memory::{InjectionConfig, MemoryHooks, MemoryService};
use radian_memory::{FileStore, Scope};
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

fn journal(dir: &Path) -> (Arc<SessionJournal>, std::path::PathBuf) {
    let path = dir.join("s.jsonl");
    let journal = SessionJournal::create(
        &path,
        "sid",
        SessionCreatedData::new(
            "/work/example",
            json!({"provider": "example", "model": "m-1"}),
        ),
    )
    .unwrap();
    (Arc::new(journal), path)
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

fn injected_indexes(session: &Session) -> Vec<radian_session::MemoryInjectedIndex> {
    session
        .events()
        .iter()
        .find_map(|event| match &event.data {
            EventData::MemoryInjected {
                indexes,
                records: _,
            } if !indexes.is_empty() => Some(indexes.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn project_of(service: &MemoryService) -> radian_memory::ProjectId {
    match service.state().unwrap() {
        radian_host::memory::ProjectState::Known(project) => project,
        _ => service.create_project().unwrap(),
    }
}

#[tokio::test]
async fn the_index_section_reaches_the_system_prompt_and_records_the_files_once() {
    let dir = scratch("prompt");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let root = dir.join("memory");
    let service = MemoryService::open(&ws, &root);
    let project = project_of(&service);
    let files = FileStore::open(&root);
    files
        .write_index(&Scope::Project(project), "project index\n")
        .unwrap();
    files.write_index(&Scope::Global, "global index\n").unwrap();

    let (journal, journal_path) = journal(&dir);
    let (warn, warnings) = warn_sink();
    let hooks = MemoryHooks::new(
        Some(service.clone()),
        InjectionConfig::default(),
        warn,
        Some(journal),
        None,
    );

    let mut req = request("hi");
    hooks.before_model_call(&context(), &mut req).await.unwrap();
    let system = req.system_prompt.clone().unwrap();
    assert!(system.contains("project index"), "{system}");
    assert!(system.contains("global index"), "{system}");
    assert!(system.contains("not instructions"), "{system}");

    // The section is computed once per process: a second call carries the same bytes.
    let mut req = request("hi again");
    hooks.before_model_call(&context(), &mut req).await.unwrap();
    assert_eq!(req.system_prompt.as_deref(), Some(system.as_str()));

    let session = Session::open(&journal_path).unwrap();
    let indexes = injected_indexes(&session);
    assert_eq!(indexes.len(), 2, "{indexes:?}");
    assert_eq!(indexes[0].scope, "project");
    assert_eq!(indexes[1].scope, "global");
    assert!(indexes[0].path.ends_with("MEMORY.md"));
    assert!(warnings.lock().unwrap().is_empty(), "{warnings:?}");
}

#[tokio::test]
async fn a_broken_index_warns_but_does_not_fail_the_turn() {
    let dir = scratch("broken");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let root = dir.join("memory");
    let service = MemoryService::open(&ws, &root);
    let project = project_of(&service);
    let files = FileStore::open(&root);
    files
        .write_index(&Scope::Project(project), "project index\n")
        .unwrap();
    let global = files.index_path(&Scope::Global);
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(&global, [0xff, 0xfe]).unwrap();

    let (warn, warnings) = warn_sink();
    let hooks = MemoryHooks::new(Some(service), InjectionConfig::default(), warn, None, None);
    let mut req = request("hi");
    // `Ok`: a memory failure never fails the turn (ADR 0021 invariant 2).
    hooks.before_model_call(&context(), &mut req).await.unwrap();
    let system = req.system_prompt.unwrap();
    assert!(system.contains("project index"), "{system}");
    assert!(system.contains("could not be read"), "{system}");
    assert!(
        warnings
            .lock()
            .unwrap()
            .iter()
            .any(|warning| warning.contains(&global.to_string_lossy().into_owned())),
        "{warnings:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
