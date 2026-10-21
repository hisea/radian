//! Recovery (ADR 0011 L3): the three states, a genuine half-written line, and a traversal
//! over every prefix of a real journal.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{MemTools, Recorder, ScriptedModel, TempDir, call, calls, created, text};
use radian_core::{Agent, InputSource, Message, TurnOptions};
use radian_runtime::{Driver, Inbox};
use radian_session::{EventData, Session, SessionJournal};
use serde_json::json;

fn message_id_of(message: &Message) -> Option<String> {
    match message {
        Message::User { meta, .. }
        | Message::Assistant { meta, .. }
        | Message::Tool { meta, .. } => meta.id.as_ref().map(|id| id.0.clone()),
    }
}

/// The lines of a session file.
fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Truncates the file to its first `k` committed lines: a crash that lost everything after
/// the k-th durable write. The remaining bytes are the real journal's own bytes.
fn truncate_to(path: &Path, k: usize) {
    let kept: Vec<String> = lines(path).into_iter().take(k).collect();
    let mut body = kept.join("\n");
    body.push('\n');
    std::fs::write(path, body).unwrap();
}

/// Runs one full turn with a real driver and returns the journal path (and the tools, so a
/// test can watch for replays).
async fn reference_run(dir: &TempDir, script: Vec<radian_core::ModelResponse>) -> PathBuf {
    let path = dir.file("s.jsonl");
    let model = ScriptedModel::new(script);
    let tools = MemTools::new();
    let journal = Arc::new(SessionJournal::create(&path, "sid", created()).unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let recorder = Arc::new(Recorder::default());
    let agent = Arc::new(Agent::new(model, tools));
    let mut driver = Driver::open(
        &agent,
        journal,
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn radian_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    inbox.followup("go", InputSource::User).await.unwrap();
    let handle = tokio::spawn(async move {
        let _ = driver.run().await;
    });
    recorder.wait_finished().await;
    handle.abort();
    // Wait for the aborted task to be dropped so the journal's file lock is released.
    let _ = handle.await;
    path
}

/// Opens a driver over an existing file, returning it with its inbox and tools.
async fn reopen(
    path: &Path,
    tools: MemTools,
    script: Vec<radian_core::ModelResponse>,
) -> (
    Driver<Arc<ScriptedModel>, MemTools, ()>,
    Arc<Inbox>,
    Arc<Recorder>,
    Arc<ScriptedModel>,
) {
    let model = ScriptedModel::new(script);
    let journal = Arc::new(SessionJournal::resume(path, "sid").unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let recorder = Arc::new(Recorder::default());
    let agent = Arc::new(Agent::new(Arc::clone(&model), tools));
    let driver = Driver::open(
        &agent,
        journal,
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn radian_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    (driver, inbox, recorder, model)
}

// ---------- 6. the three recovery states ----------

#[tokio::test]
async fn an_admitted_input_is_not_replayed() {
    let dir = TempDir::new("recover-admitted");
    let path = reference_run(&dir, vec![text("done")]).await;

    let tools = MemTools::new();
    let (driver, inbox, _recorder, model) = reopen(&path, tools.clone(), vec![]).await;
    // Nothing was claimed again, and the transcript still holds the input.
    assert!(inbox.is_empty().await, "an admitted input is not requeued");
    let history = Session::open(&path).unwrap().history().unwrap();
    assert_eq!(
        history.iter().map(Message::text).collect::<Vec<_>>(),
        ["go", "done"]
    );
    // Nothing to run: an admitted input is not requeued, so the driver has no work at all.
    assert!(inbox.snapshot().await.is_empty());
    assert!(model.requests().is_empty(), "no turn was replayed");
    drop(driver);
}

#[tokio::test]
async fn a_claimed_input_without_admission_returns_to_the_front_of_its_lane() {
    let dir = TempDir::new("recover-claimed");
    let path = reference_run(&dir, vec![text("done")]).await;

    // Crash right after `InputClaimed`: the input was claimed, never admitted.
    let claimed_line = lines(&path)
        .iter()
        .position(|l| l.contains("\"InputClaimed\""))
        .expect("the run wrote InputClaimed")
        + 1;
    truncate_to(&path, claimed_line);

    let tools = MemTools::new();
    let (_driver, inbox, _recorder, _model) = reopen(&path, tools, vec![]).await;
    let queue = inbox.snapshot().await;
    assert_eq!(queue.len(), 1, "the claimed input came back");
    assert_eq!(queue[0].message.text(), "go");
    assert_eq!(queue[0].lane, radian_runtime::Lane::NextTurn);
    // And the transcript does not contain it: it was never admitted.
    let history = Session::open(&path).unwrap().history().unwrap();
    assert!(history.is_empty(), "{history:?}");
}

/// A crash between "the input was committed at the start of the step" and "the
/// `InputAdmitted` record was written" must not replay the input: the transcript already
/// holds it, and `meta.id` links the two.
#[tokio::test]
async fn an_input_in_the_transcript_without_an_admission_record_is_not_replayed() {
    let dir = TempDir::new("recover-unrecorded");
    let path = reference_run(&dir, vec![text("done")]).await;

    // Keep everything before the InputAdmitted line: the message is committed, the record
    // is not.
    let admitted_line = lines(&path)
        .iter()
        .position(|l| l.contains("\"InputAdmitted\""))
        .expect("the run wrote InputAdmitted");
    truncate_to(&path, admitted_line);

    let tools = MemTools::new();
    let (_driver, inbox, _recorder, _model) = reopen(&path, tools, vec![]).await;
    assert!(
        inbox.is_empty().await,
        "an input already in the transcript must not be requeued: {:?}",
        inbox
            .snapshot()
            .await
            .iter()
            .map(|e| e.message.text())
            .collect::<Vec<_>>()
    );
    // Recovery wrote the missing admission record, so the journal now agrees.
    let session = Session::open(&path).unwrap();
    assert!(
        session
            .events()
            .iter()
            .any(|e| matches!(&e.data, EventData::InputAdmitted { .. })),
        "the admission record was not caught up"
    );
    assert_eq!(
        session
            .history()
            .unwrap()
            .iter()
            .map(Message::text)
            .collect::<Vec<_>>(),
        ["go", "done"]
    );
}

#[tokio::test]
async fn an_unpaired_tool_call_is_repaired_and_the_tool_is_not_replayed() {
    let dir = TempDir::new("recover-tool");
    let path = reference_run(
        &dir,
        vec![calls(vec![call("c1", "echo", json!({}))]), text("done")],
    )
    .await;

    // Crash after the tool started but before its result was written.
    let started_line = lines(&path)
        .iter()
        .position(|l| l.contains("\"ToolExecutionStarted\""))
        .expect("the run dispatched a tool")
        + 1;
    truncate_to(&path, started_line);

    let tools = MemTools::new();
    let (mut driver, _inbox, recorder, _model) = reopen(
        &path,
        tools.clone(),
        vec![calls(vec![call("c2", "echo", json!({}))]), text("done")],
    )
    .await;

    // Recovery repaired the call and wrote the synthetic result back (ADR 0002 E9).
    let history = Session::open(&path).unwrap().history().unwrap();
    let repaired = history.iter().find_map(|m| match m {
        Message::Tool { result, .. } => Some(result),
        _ => None,
    });
    assert_eq!(
        repaired.and_then(|r| r.error_code.as_deref()),
        Some("outcome_unknown_after_crash"),
        "the unpaired call is classified, not replayed: {history:?}"
    );
    assert!(
        tools.started_ids().is_empty(),
        "the tool was not executed again: {:?}",
        tools.started_ids()
    );

    // A new turn runs only its own tool call.
    let inbox = Arc::clone(driver.inbox());
    let handle = tokio::spawn(async move {
        let _ = driver.run().await;
    });
    inbox.followup("next", InputSource::User).await.unwrap();
    recorder.wait_finished().await;
    handle.abort();
    let _ = handle.await;
    assert_eq!(
        tools.started_ids(),
        ["c2"],
        "only the new turn's call ran (the repaired c1 did not)"
    );
}

// ---------- 7. traversal over every prefix of a real journal ----------

#[tokio::test]
async fn crash_traversal_recovers_every_prefix() {
    let dir = TempDir::new("crash-traversal");
    let path = reference_run(
        &dir,
        vec![calls(vec![call("c1", "echo", json!({}))]), text("done")],
    )
    .await;
    // The real bytes, captured once; each case is a prefix of exactly these.
    let reference = lines(&path);
    assert!(
        reference.len() > 6,
        "the reference run should have many durable writes"
    );

    for k in 1..=reference.len() {
        let case = TempDir::new("crash-prefix");
        let case_path = case.file("s.jsonl");
        let mut body = reference[..k].join("\n");
        body.push('\n');
        std::fs::write(&case_path, body).unwrap();

        let tools = MemTools::new();
        let (mut driver, inbox, recorder, _model) = reopen(
            &case_path,
            tools.clone(),
            vec![text("a"), text("b"), text("c")],
        )
        .await;

        let session = Session::open(&case_path).unwrap();
        let history = session.history().unwrap();
        let admitted: Vec<String> = session
            .events()
            .iter()
            .filter_map(|e| match &e.data {
                EventData::InputAdmitted { message_id, .. } => Some(message_id.0.clone()),
                _ => None,
            })
            .collect();
        let queued: Vec<String> = inbox
            .snapshot()
            .await
            .iter()
            .map(|e| e.id.0.clone())
            .collect();

        // Every input that was enqueued is either in the transcript (admitted) or back in
        // the queue; never both, never neither.
        for event in session.events() {
            if let EventData::InboxEnqueued { message_id, .. } = &event.data {
                let is_admitted = admitted.contains(&message_id.0);
                let is_queued = queued.contains(&message_id.0);
                assert_ne!(
                    is_admitted, is_queued,
                    "k={k}: input {} must be exactly one of admitted/queued (admitted={admitted:?} queued={queued:?})",
                    message_id.0
                );
                if is_admitted {
                    assert!(
                        history
                            .iter()
                            .any(|m| message_id_of(m).as_deref() == Some(message_id.0.as_str())),
                        "k={k}: admitted input must be in the transcript"
                    );
                }
            }
        }
        // No tool was replayed while recovering.
        assert!(tools.started_ids().is_empty(), "k={k}: a tool was replayed");

        // The recovered session is usable: a new turn completes.
        let handle = tokio::spawn(async move {
            let _ = driver.run().await;
        });
        inbox.followup("next", InputSource::User).await.unwrap();
        recorder.wait_finished().await;
        handle.abort();
        let _ = handle.await;
    }
}

// ---------- a genuinely half-written line ----------

#[test]
fn a_genuinely_half_written_line_is_treated_as_a_torn_tail() {
    let Ok(path) = std::env::var("RADIAN_RUNTIME_CRASH_PATH") else {
        return; // only runs when spawned as the child
    };
    let path = PathBuf::from(path);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let journal = SessionJournal::create(&path, "sid", created()).unwrap();
        journal
            .append_runtime_event(radian_session::RuntimeEvent::InboxEnqueued {
                message: Message::user("half"),
                message_id: radian_core::MessageId("inbox-1".into()),
                lane: radian_session::InboxLane::NextTurn,
                source: InputSource::User,
                wake: true,
                degraded: false,
                pending_wake: false,
            })
            .await
            .unwrap();
        journal
            .append_runtime_event(radian_session::RuntimeEvent::TurnStarted {
                turn_id: "t-1".into(),
            })
            .await
            .unwrap();
    });
    // A real partial line, then die.
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(br#"{"schema_version":1,"session_id":"sid","seq":3,"ts":"2026"#)
        .unwrap();
    file.flush().unwrap();
    println!("READY");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}

#[tokio::test]
async fn a_killed_writer_leaves_only_a_torn_tail() {
    let dir = TempDir::new("kill-driver");
    let path = dir.file("s.jsonl");
    let exe = std::env::current_exe().unwrap();
    let mut child = std::process::Command::new(exe)
        .env("RADIAN_RUNTIME_CRASH_PATH", &path)
        .args([
            "--exact",
            "a_genuinely_half_written_line_is_treated_as_a_torn_tail",
            "--nocapture",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut ready = false;
    for line in std::io::BufRead::lines(std::io::BufReader::new(stdout)) {
        if line.unwrap().trim() == "READY" {
            ready = true;
            break;
        }
    }
    assert!(ready, "the child never reported its partial write");
    child.kill().unwrap();
    child.wait().unwrap();

    assert!(
        !std::fs::read(&path).unwrap().ends_with(b"\n"),
        "torn tail expected"
    );

    // Recovery ignores the torn tail, and the enqueued-but-unclaimed input comes back.
    let tools = MemTools::new();
    let (_driver, inbox, _recorder, _model) = reopen(&path, tools, vec![]).await;
    let queue = inbox.snapshot().await;
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].message.text(), "half");
    let session = Session::open(&path).unwrap();
    let seqs: Vec<u64> = session.events().iter().map(|e| e.seq).collect();
    assert_eq!(seqs, [0, 1, 2], "the torn line was dropped, not counted");
}
