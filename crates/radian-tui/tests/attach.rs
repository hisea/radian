//! ADR 0043 slice ① (`radian attach <session> [--follow]`): the view's own tests.
//!
//! The read-only view is exercised through its public surface (`AttachView`) rather than the
//! internals, so this binary is also the place the "no live event can reach the frame"
//! guarantee is pinned: a behavioral test (a `ModelTextDelta` line in the journal is skipped,
//! never rendered) plus a structural one (the view's source has no live-event plumbing at all).
//! A4 (the first-frame budget) and A5 (an unknown id) need the real binary and live here too;
//! the pty-driven cases that need the full-screen program are in `tests/pty.rs`.
//!
//! Each test names the mutation it catches where the slice's acceptance list requires one.

use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use radian_tui::attach::{AttachKey, AttachView};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const BIN: &str = env!("CARGO_BIN_EXE_radian");

/// A scratch directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("radian-attach-{}-{label}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One envelope line (ADR 0006 §1). `critical` is what the reader's unknown-event policy keys
/// on (§7).
fn envelope(seq: u64, event: &str, critical: bool, data: serde_json::Value) -> String {
    format!(
        r#"{{"schema_version":1,"session_id":"attach-sid","seq":{seq},"ts":"2026-10-17T00:00:00.000000Z","event":"{event}","critical":{critical},"data":{data}}}"#
    )
}

/// A `MessageAppended` body for the message at transcript `index`.
fn appended(index: u64, message: agent_core::Message) -> serde_json::Value {
    serde_json::json!({"index": index, "message": message})
}

/// A journal with one committed user turn and one committed assistant reply, plus a **live**
/// event line that must never be rendered.
fn committed_journal() -> Vec<String> {
    vec![
        envelope(
            0,
            "SessionCreated",
            true,
            serde_json::json!({"workspace": "/w", "config_snapshot": {}}),
        ),
        envelope(
            1,
            "TurnStarted",
            true,
            serde_json::json!({"turn_id": "t-1"}),
        ),
        envelope(
            2,
            "MessageAppended",
            true,
            appended(0, agent_core::Message::user("ATTACH-COMMITTED-USER")),
        ),
        // The live delta: `ModelTextDelta` is never durable (ADR 0010 K2), so it can only
        // reach the frame through a reader that renders live events.
        envelope(
            3,
            "ModelTextDelta",
            false,
            serde_json::json!({"text": "ATTACH-LIVE-DELTA"}),
        ),
        envelope(
            4,
            "MessageAppended",
            true,
            appended(1, agent_core::Message::assistant("ATTACH-COMMITTED-REPLY")),
        ),
        envelope(
            5,
            "TurnFinished",
            true,
            serde_json::json!({"turn_id": "t-1", "outcome": "completed"}),
        ),
    ]
}

/// Writes `lines` as `<id>.jsonl` and opens the view over it.
fn open_view(label: &str, id: &str, lines: &[String]) -> (Scratch, AttachView) {
    let scratch = Scratch::new(label);
    let path = scratch.file(&format!("{id}.jsonl"));
    std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
    let view = AttachView::open(&path, id).expect("the view opens");
    (scratch, view)
}

/// Appends `lines` to the file a view is watching.
fn append(path: &Path, lines: &[String]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(format!("{}\n", lines.join("\n")).as_bytes())
        .unwrap();
    file.flush().unwrap();
}

/// The screen a frame draws at `width` x `height`.
fn screen(view: &mut AttachView, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| view.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" "))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

// ---------- A2: no live event reaches the frame ----------

/// The committed transcript is what a frame shows, and a live delta is not part of it.
///
/// Mutation: give the view a path from a live event (an `AgentEvent` sink, or an
/// unknown-event policy that keeps the line) and render `ATTACH-LIVE-DELTA` → FAILED.
#[test]
fn a_frame_shows_the_committed_transcript_and_never_a_live_delta() {
    let (_scratch, mut view) = open_view("committed", "commit-me", &committed_journal());
    let shown = screen(&mut view, 80, 24);
    assert!(
        shown.contains("ATTACH-COMMITTED-USER"),
        "the committed user message is not on screen:\n{shown}"
    );
    assert!(
        shown.contains("ATTACH-COMMITTED-REPLY"),
        "the committed assistant message is not on screen:\n{shown}"
    );
    assert!(
        !shown.contains("ATTACH-LIVE-DELTA"),
        "a live delta reached the frame:\n{shown}"
    );
    assert_eq!(
        view.history().len(),
        2,
        "the committed history is the two messages: the skipped live event is not one"
    );
}

/// The view's own source has no live-event plumbing: no `AgentEvent`, no streaming buffer
/// write, no delta name. The frame is built from committed messages and from the observer's
/// two durable projections (the turn state and the two timestamps) only.
///
/// Mutation: wire any live event into the view (an `AgentEvent` match, a `push_delta`) →
/// FAILED, whether or not the behavioral test above happens to notice.
#[test]
fn the_attach_view_has_no_live_event_sink() {
    // Only the code: the module docs and the tests below name the live events on purpose.
    let source = include_str!("../src/attach.rs");
    let source = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in [
        "AgentEvent",
        "push_delta",
        "StreamKind",
        "ModelTextDelta",
        "ModelReasoningDelta",
        "ToolProgress",
    ] {
        assert!(
            !code.contains(forbidden),
            "the attach view must have no live-event path, found `{forbidden}`"
        );
    }
}

// ---------- the durable status line ----------

/// `working…` is the last `TurnStarted` with no `TurnFinished` — a durable fact — and it
/// leaves when the turn finishes.
///
/// Mutation: show the line from anything else (a live event, a timer) → the second half (the
/// line must be gone after `TurnFinished`) FAILED.
#[test]
fn the_working_line_follows_the_open_turn() {
    let mut lines = committed_journal();
    lines.push(envelope(
        6,
        "TurnStarted",
        true,
        serde_json::json!({"turn_id": "t-2"}),
    ));
    let (_scratch, mut view) = open_view("working", "run-me", &lines);
    let shown = screen(&mut view, 80, 24);
    assert!(
        shown.contains("working…"),
        "an open turn does not show `working…`:\n{shown}"
    );
    assert!(view.running(), "the last TurnStarted has no TurnFinished");

    // The turn finishes: the line goes away on the next read.
    let path = view.path().to_owned();
    append(
        &path,
        &[envelope(
            7,
            "TurnFinished",
            true,
            serde_json::json!({"turn_id": "t-2", "outcome": "completed"}),
        )],
    );
    assert!(
        view.poll().expect("the new line folds"),
        "the read changed the frame"
    );
    assert!(!view.running(), "the turn is finished");
    let shown = screen(&mut view, 80, 24);
    assert!(
        !shown.contains("working…"),
        "a finished turn still shows `working…`:\n{shown}"
    );
}

/// A message committed after the first read appears on the next read (the `--follow` half of
/// the view).
///
/// Mutation: read the file once and drop the observer's poll → the new message never shows.
#[test]
fn a_follow_read_shows_a_new_committed_message() {
    let (_scratch, mut view) = open_view("follow", "follow-me", &committed_journal());
    assert!(!screen(&mut view, 80, 24).contains("ATTACH-NEW-MESSAGE"));

    let path = view.path().to_owned();
    append(
        &path,
        &[envelope(
            6,
            "MessageAppended",
            true,
            appended(2, agent_core::Message::assistant("ATTACH-NEW-MESSAGE")),
        )],
    );

    assert!(view.poll().expect("the new line folds"));
    let shown = screen(&mut view, 80, 24);
    assert!(
        shown.contains("ATTACH-NEW-MESSAGE"),
        "the appended message is not on screen:\n{shown}"
    );
    assert_eq!(view.history().len(), 3, "the folded history grew by one");
}

/// One assistant message that requests `n` `read` calls (`a`, `b`, …), the shape the fold's
/// repair works from (a call with no committed result message).
fn assistant_with_calls(n: usize) -> agent_core::Message {
    agent_core::Message::Assistant {
        content: Vec::new(),
        reasoning: Vec::new(),
        tool_calls: (0..n)
            .map(|i| {
                let id = ((b'a' + i as u8) as char).to_string();
                agent_core::ToolCall {
                    id: id.clone(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": format!("{id}.txt")}),
                }
            })
            .collect(),
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
    }
}

/// A `ToolExecutionSettled` body and the committed `Message::Tool` for the same result.
fn settled(call_id: &str, text: &str) -> (serde_json::Value, agent_core::Message) {
    let result =
        agent_core::ToolResult::success(call_id, vec![agent_core::ContentPart::text(text)]);
    (
        serde_json::json!({"call_id": call_id, "result": result}),
        agent_core::Message::Tool {
            result,
            meta: Default::default(),
        },
    )
}

/// A journal whose open turn has requested three calls, one started, none settled. The repaired
/// suffix (ADR 0002) is therefore one synthesised result per call.
fn open_call_journal() -> Vec<String> {
    vec![
        envelope(
            0,
            "SessionCreated",
            true,
            serde_json::json!({"workspace": "/w", "config_snapshot": {}}),
        ),
        envelope(
            1,
            "TurnStarted",
            true,
            serde_json::json!({"turn_id": "t-1"}),
        ),
        envelope(
            2,
            "MessageAppended",
            true,
            appended(0, assistant_with_calls(3)),
        ),
        envelope(
            3,
            "ToolExecutionStarted",
            true,
            serde_json::json!({"call_id": "a", "name": "read", "arguments": {"path": "a.txt"}}),
        ),
    ]
}

/// On a **live** session (a writer holds the file's lock) the open calls are `running`/`queued`,
/// never the fold's crash wording; the settles then commit the real results and replace them.
///
/// Mutation: keep using the recovery fold for a live session (push the synthesised results) →
/// the first half fails (`had not started executing` on screen).
#[test]
fn a_live_sessions_open_calls_show_as_running_and_queued_then_their_results() {
    let scratch = Scratch::new("live-calls");
    // The file's `session_id` (the envelope helper's) must match, so `SessionJournal::resume`
    // takes the lock over the same session.
    let id = "attach-sid";
    let path = scratch.file(&format!("{id}.jsonl"));
    std::fs::write(&path, format!("{}\n", open_call_journal().join("\n"))).unwrap();
    // The writer's lock, held for the rest of the test: this is what makes the session live.
    let _journal = radian_session::SessionJournal::resume(&path, id).expect("take the writer lock");

    let mut view = AttachView::open(&path, id).expect("the view opens");
    let shown = screen(&mut view, 80, 24);
    assert_eq!(
        shown.matches("running:").count(),
        1,
        "one started call is running:\n{shown}"
    );
    assert_eq!(
        shown.matches("queued:").count(),
        2,
        "two calls have not started:\n{shown}"
    );
    assert!(
        shown.contains("running: read a.txt")
            && shown.contains("queued: read b.txt")
            && shown.contains("queued: read c.txt"),
        "the lines name the calls:\n{shown}"
    );
    assert!(
        !shown.contains("interrupted") && !shown.contains("had not started"),
        "a live session's open calls are not a crash:\n{shown}"
    );

    // The runtime settles all three: the marker, then the committed result message (ADR 0002).
    let mut lines = open_call_journal();
    let mut seq = lines.len() as u64;
    let results = [("a", "RESULT-A"), ("b", "RESULT-B"), ("c", "RESULT-C")];
    for (index, (call_id, text)) in results.into_iter().enumerate() {
        let (settle, result) = settled(call_id, text);
        lines.push(envelope(seq, "ToolExecutionSettled", true, settle));
        lines.push(envelope(
            seq + 1,
            "MessageAppended",
            true,
            appended(index as u64 + 1, result),
        ));
        seq += 2;
    }
    append(&path, &lines[4..]);
    assert!(
        view.poll().expect("the new lines fold"),
        "the frame changed"
    );
    let shown = screen(&mut view, 80, 24);
    assert!(
        !shown.contains("queued:") && !shown.contains("running:"),
        "no pending line is left:\n{shown}"
    );
    assert_eq!(
        shown.matches('✓').count(),
        3,
        "the three calls are settled with their real results:\n{shown}"
    );
    let history = view
        .history()
        .iter()
        .map(agent_core::Message::text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        history.contains("RESULT-A") && history.contains("RESULT-C"),
        "the committed results are in the fold:\n{history}"
    );
    assert!(
        !history.contains("interrupted") && !history.contains("had not started"),
        "the fold carries no crash wording for the settled calls:\n{history}"
    );
}

/// A session that is **not** live keeps the fold's recovery wording: nothing holds the lock, so
/// those calls really did not finish. This is the other half of the liveness gate.
///
/// Mutation: treat every session as live (skip the probe) → this fails (`queued:` on screen
/// instead of the crash wording).
#[test]
fn a_dead_sessions_open_calls_keep_the_recovery_wording() {
    let (_scratch, mut view) = open_view("dead-calls", "dead-calls", &open_call_journal());
    let shown = screen(&mut view, 80, 24);
    assert!(
        shown.contains("had not started executing"),
        "a session with no live writer shows the recovery wording:\n{shown}"
    );
    assert!(
        !shown.contains("queued:"),
        "no pending line for a dead session:\n{shown}"
    );
}

// ---------- the view's chrome and input ----------

/// The footer carries the attach facts and nothing that belongs to the session's process.
#[test]
fn the_footer_names_only_the_attach_facts() {
    let (_scratch, mut view) = open_view("footer", "footer-me", &committed_journal());
    screen(&mut view, 80, 24);
    let footer = view.footer_text();
    assert!(
        footer.contains("attach · footer-me · read-only"),
        "{footer}"
    );
    assert!(footer.contains("scroll"), "{footer}");
    for absent in ["tokens", "cost", "model", "workspace", "idle", "running"] {
        assert!(
            !footer.contains(absent),
            "the footer must not carry `{absent}`: {footer}"
        );
    }
}

/// Only scrolling and quitting do anything; every other key is swallowed.
#[test]
fn a_key_that_is_not_a_scroll_or_quit_key_is_ignored() {
    let (_scratch, mut view) = open_view("keys", "keys-me", &committed_journal());
    screen(&mut view, 80, 8);
    for ignored in [
        key(KeyCode::Char('x'), KeyModifiers::NONE),
        key(KeyCode::Char('a'), KeyModifiers::CONTROL),
        key(KeyCode::Enter, KeyModifiers::NONE),
        key(KeyCode::Up, KeyModifiers::NONE),
        key(KeyCode::Tab, KeyModifiers::NONE),
        key(KeyCode::Char('/'), KeyModifiers::NONE),
        key(KeyCode::Home, KeyModifiers::NONE),
    ] {
        assert_eq!(
            view.on_key(&ignored),
            AttachKey::Ignored,
            "{ignored:?} is not a scroll or quit key"
        );
    }
    for quit in [
        key(KeyCode::Char('q'), KeyModifiers::NONE),
        key(KeyCode::Esc, KeyModifiers::NONE),
        key(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        assert_eq!(view.on_key(&quit), AttachKey::Quit, "{quit:?} quits");
    }
}

/// The scroll keys move the window and the footer follows them; `ctrl-end` follows again.
#[test]
fn the_scroll_keys_move_the_window_and_bottom_follows_again() {
    let mut lines = committed_journal();
    for index in 2..40u64 {
        lines.push(envelope(
            index + 4,
            "MessageAppended",
            true,
            appended(
                index,
                agent_core::Message::assistant(format!("line {index}")),
            ),
        ));
    }
    let (_scratch, mut view) = open_view("scroll", "scroll-me", &lines);
    screen(&mut view, 80, 10);
    assert!(
        view.footer_text().contains("follow"),
        "a fresh view follows the tail: {}",
        view.footer_text()
    );
    let at_bottom = view.footer_text();

    assert_eq!(
        view.on_key(&key(KeyCode::PageUp, KeyModifiers::NONE)),
        AttachKey::Redraw
    );
    screen(&mut view, 80, 10);
    let paused = view.footer_text();
    assert!(
        paused.contains("paused"),
        "page up pauses the follow: {paused}"
    );
    assert_ne!(paused, at_bottom, "the position moved: {paused}");

    assert_eq!(
        view.on_key(&key(KeyCode::End, KeyModifiers::CONTROL)),
        AttachKey::Redraw
    );
    screen(&mut view, 80, 10);
    assert_eq!(
        view.footer_text(),
        at_bottom,
        "ctrl-end is back at the tail and follows"
    );
}

/// The view's history is the same fold a resume produces for the same file.
#[test]
fn the_history_is_the_resume_fold_of_the_same_file() {
    let (scratch, view) = open_view("fold", "fold-me", &committed_journal());
    let file = scratch.file("fold-me.jsonl");
    let session = radian_session::Session::open(&file).expect("a valid session file");
    assert_eq!(
        view.history(),
        session.history().expect("the fold").as_slice(),
        "the attach view and Session::open must fold the same history"
    );
}

// ---------- A5: an unknown id ----------

/// An unknown id refuses with one line on stderr and a setup exit code, creates nothing, and
/// never enters the terminal.
#[test]
fn an_unknown_session_id_refuses_and_creates_nothing() {
    let scratch = Scratch::new("unknown-id");
    let sessions = scratch.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let output = attach_without_a_terminal(&scratch, &sessions, "no-such-session");
    assert_eq!(output.status.code(), Some(2), "a setup refusal is exit 2");
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The whole line, not a substring: the refusal names the id and the directory it looked in,
    // and it is the refusal **before** opening anything (a later I/O error would word it as a
    // read failure instead).
    assert_eq!(
        stderr,
        format!(
            "radian: no session \"no-such-session\" in {}\n",
            sessions.display()
        ),
        "exactly one line on stderr, the refusal"
    );
    assert!(
        output.stdout.is_empty(),
        "nothing on stdout: {:?}",
        output.stdout
    );
    assert_empty_dir(&sessions, "a refused attach");
}

/// A session id becomes a file name, so a path-like id is refused by the same rule `--resume`
/// uses (`validate_session_id`) — before the session directory is even resolved, so nothing is
/// looked up, created or read. The `../decoy` case is the reason: one level up sits a real,
/// readable session file, and it must stay unread.
///
/// Mutation: drop the validation (build the path from the raw id) → `../decoy` is opened (and
/// the line stops being the refusal) → FAILED.
#[test]
fn a_path_like_session_id_is_refused_before_anything_is_read() {
    let scratch = Scratch::new("bad-id");
    let sessions = scratch.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    // The decoy: where `../decoy` resolves to, and a perfectly readable session file.
    let decoy = scratch.file("decoy.jsonl");
    std::fs::write(&decoy, format!("{}\n", committed_journal().join("\n"))).unwrap();
    let decoy_before = std::fs::read(&decoy).unwrap();

    for bad in ["../decoy", "a/b", "..", ".", ""] {
        let output = attach_without_a_terminal(&scratch, &sessions, bad);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{bad:?} must be refused with a setup exit code"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            format!("radian: session id {bad:?} must be 1-128 characters of [A-Za-z0-9._-]\n"),
            "{bad:?}: exactly one line, the id rule"
        );
        assert!(output.stdout.is_empty(), "{bad:?}: nothing on stdout");
    }
    assert_empty_dir(&sessions, "a refused id");
    assert_eq!(
        std::fs::read(&decoy).unwrap(),
        decoy_before,
        "the escaped file must not be touched"
    );
}

/// Runs `radian attach <id>` with `sessions` as the session directory and **no** terminal
/// (`stdin` is `/dev/null`): every refusal path is taken before the terminal is entered, so the
/// process exits on its own without a pty.
fn attach_without_a_terminal(scratch: &Scratch, sessions: &Path, id: &str) -> std::process::Output {
    Command::new(BIN)
        .args(["attach", id])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", scratch.path())
        .env("RADIAN_SESSION_DIR", sessions)
        .env("TERM", "xterm-256color")
        .stdin(Stdio::null())
        .output()
        .expect("the binary runs")
}

/// `dir` holds nothing (no journal, no draft, no job directory).
fn assert_empty_dir(dir: &Path, what: &str) {
    let leftovers: Vec<String> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        leftovers.is_empty(),
        "{what} created {leftovers:?} in {}",
        dir.display()
    );
}

// ---------- a subagent child's journal ----------

/// ADR 0043 decision 1a: the session a `subagent` call starts is an ordinary journal file with
/// `SessionCreated.parent { relation = "subagent" }`, so `radian attach <child id>` shows it —
/// there is no live session to hold and no socket involved, only the committed facts. This
/// drives the real full-screen `attach` under a pty and looks for the child's assistant text.
#[test]
fn attach_shows_a_subagent_child_session() {
    let scratch = Scratch::new("subagent-child");
    let sessions = scratch.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let child_id = "attach-sid";
    let lines = [
        envelope(
            0,
            "SessionCreated",
            true,
            serde_json::json!({
                "workspace": "/w",
                "config_snapshot": {"access": "workspace"},
                "parent": {"session_id": "parent-sid", "seq": 0, "relation": "subagent"},
            }),
        ),
        envelope(
            1,
            "TurnStarted",
            true,
            serde_json::json!({"turn_id": "t-1"}),
        ),
        envelope(
            2,
            "MessageAppended",
            true,
            appended(0, agent_core::Message::user("CHILD-INPUT")),
        ),
        envelope(
            3,
            "MessageAppended",
            true,
            appended(1, agent_core::Message::assistant("CHILD-CONCLUSION-OK")),
        ),
        envelope(
            4,
            "TurnFinished",
            true,
            serde_json::json!({"turn_id": "t-1", "outcome": "completed"}),
        ),
    ];
    std::fs::write(
        sessions.join(format!("{child_id}.jsonl")),
        format!("{}\n", lines.join("\n")),
    )
    .unwrap();

    let mut pty = Pty::spawn(
        &[
            "attach",
            child_id,
            "--session-dir",
            sessions.to_str().unwrap(),
        ],
        scratch.path(),
    );
    assert!(
        pty.first_frame("CHILD-CONCLUSION-OK", 60).is_some(),
        "`radian attach <child>` must show the child's assistant text"
    );
    pty.quit();
}

// ---------- A4: the first-frame budget ----------

/// A minimal pty for the first-frame measurement: `libc::openpty` + a direct spawn (no shell),
/// a reader thread that drains the output. The full-screen program never runs in the test
/// runner's terminal.
struct Pty {
    master: std::fs::File,
    bytes: Arc<Mutex<Vec<u8>>>,
    start: Instant,
    child: Child,
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Pty {
    fn spawn(args: &[&str], home: &Path) -> Self {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        size.ws_row = 24;
        size.ws_col = 100;
        // SAFETY: `openpty` writes the two fds and reads the null termios/name pointers.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(size),
            )
        };
        assert_eq!(rc, 0, "openpty failed");
        // SAFETY: the fds are fresh from `openpty`; the `OwnedFd`s close them.
        let (master, slave) = unsafe {
            (
                std::os::fd::OwnedFd::from_raw_fd(master),
                std::os::fd::OwnedFd::from_raw_fd(slave),
            )
        };
        let start = Instant::now();
        let mut cmd = Command::new(BIN);
        cmd.args(args);
        cmd.env_clear();
        for name in ["PATH", "TMPDIR", "LANG"] {
            if let Ok(value) = std::env::var(name) {
                cmd.env(name, value);
            }
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("HOME", home);
        cmd.env("RADIAN_CATALOG_URL", "http://127.0.0.1:1/api.json");
        cmd.env("RADIAN_KEYBOARD_ENHANCEMENT", "0");
        cmd.stdin(Stdio::from(slave.try_clone().unwrap()));
        cmd.stdout(Stdio::from(slave.try_clone().unwrap()));
        cmd.stderr(Stdio::from(slave));
        let child = cmd.spawn().unwrap();
        drop(cmd);
        let master = std::fs::File::from(master);
        let mut reader = master.try_clone().unwrap();
        let bytes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&bytes);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                log.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Self {
            master,
            bytes,
            start,
            child,
        }
    }

    /// The elapsed time from the spawn until `needle` is in the raw output; `None` on timeout.
    fn first_frame(&self, needle: &str, secs: u64) -> Option<Duration> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if String::from_utf8_lossy(&self.bytes.lock().unwrap()).contains(needle) {
                return Some(self.start.elapsed());
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Quits the app (`q` is attach's quit key; the TUI quits on the Ctrl-C pair).
    fn quit(&mut self) {
        for key in [b"q".as_slice(), &[0x03], b"q".as_slice()] {
            let _ = self.master.write_all(key);
            let _ = self.master.flush();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A valid v1 session file with `n` messages; the last one is a visible marker, so "the first
/// frame is up" is the moment it appears.
fn write_session(path: &Path, session_id: &str, n: usize) {
    use agent_core::Message;
    use std::io::BufWriter;
    let mut out = BufWriter::new(std::fs::File::create(path).unwrap());
    let ts = "2026-10-12T00:00:00.000000Z";
    writeln!(
        out,
        "{}",
        serde_json::json!({"schema_version":1,"session_id":session_id,"seq":0,"ts":ts,
            "event":"SessionCreated","critical":true,
            "data":{"workspace":"/work/example","config_snapshot":{}}})
    )
    .unwrap();
    for i in 0..n {
        let message = if i + 1 == n {
            Message::assistant("ATTACH-PERF-END")
        } else if i % 2 == 0 {
            Message::user(format!("resumed user message {i}"))
        } else {
            Message::assistant(format!("resumed assistant reply {i}"))
        };
        writeln!(
            out,
            "{}",
            serde_json::json!({"schema_version":1,"session_id":session_id,"seq":i + 1,"ts":ts,
                "event":"MessageAppended","critical":true,
                "data":{"index":i,"message":serde_json::to_value(&message).unwrap()}})
        )
        .unwrap();
    }
    out.flush().unwrap();
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

/// **A4**: `attach`'s first frame of a 10 000-entry session is within twice a `--resume` first
/// frame, measured in the same run on the same machine. Both are measured the same way: from
/// the spawn to the appearance of the last message's text, which both paths put in their first
/// frame (a resume renders the recovered history before it draws; attach renders the folded
/// journal before it draws).
///
/// Mutation: load the journal a second time, or re-fold per entry, so the attach first frame
/// outgrows twice the resume one → FAILED.
#[test]
fn a_ten_thousand_entry_first_frame_is_within_twice_a_resume_first_frame() {
    let scratch = Scratch::new("perf");
    let sessions = scratch.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    write_session(&sessions.join("perf.jsonl"), "perf", 10_000);
    let dir = sessions.to_str().unwrap();

    let mut attach = Vec::new();
    let mut resume = Vec::new();
    for run in 0..3 {
        let mut pty = Pty::spawn(&["attach", "perf", "--session-dir", dir], scratch.path());
        let time = pty.first_frame("ATTACH-PERF-END", 60).unwrap_or_else(|| {
            panic!("attach run {run}: the first frame never showed the last message")
        });
        attach.push(time);
        pty.quit();

        let mut pty = Pty::spawn(
            &[
                "--model",
                "mock",
                "--base-url",
                "http://127.0.0.1:1",
                "--session-dir",
                dir,
                "--resume",
                "perf",
            ],
            scratch.path(),
        );
        let time = pty.first_frame("ATTACH-PERF-END", 60).unwrap_or_else(|| {
            panic!("resume run {run}: the first frame never showed the last message")
        });
        resume.push(time);
        pty.quit();
    }
    let attach = median(attach);
    let resume = median(resume);
    println!(
        "10k first frame: attach {:.0} ms, resume {:.0} ms",
        attach.as_secs_f64() * 1000.0,
        resume.as_secs_f64() * 1000.0
    );
    assert!(
        attach <= resume * 2,
        "attach's first frame ({attach:?}) must be within twice the resume first frame \
         ({resume:?})"
    );
}
