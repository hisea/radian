//! The background-job notifier (ADR 0037 slices 3-4): the durable start/settle records, the
//! busy/idle lane choice, the wake cap, and the crash-orphan scan.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use radian_coding::{JobEvent, JobState, Jobs, OUTPUT_HINT};
use radian_host::job_notify::{JobNotifier, orphan_jobs};
use radian_host::wake_gate::{MAX_WAKES_PER_WINDOW, WAKE_WINDOW};
use radian_runtime::{Inbox, Lane};
use radian_sandbox::{Sandbox, SandboxMode, SandboxPolicy};
use radian_session::{EventData, RuntimeEvent, Session, SessionCreatedData, SessionJournal};
use serde_json::json;

fn scratch(label: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "radian-job-notify-{}-{label}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

fn notifier(dir: &Path) -> (Arc<JobNotifier>, Arc<Jobs>, Arc<Inbox>, Arc<AtomicBool>) {
    let inbox = Inbox::new(64);
    let jobs = Arc::new(Jobs::new(dir.join("jobs")));
    let turn_running = Arc::new(AtomicBool::new(false));
    let notifier = JobNotifier::new(
        Arc::clone(&jobs),
        Arc::clone(&inbox),
        journal(dir),
        Arc::clone(&turn_running),
        // The live runtime-event sink: these tests do not read it.
        Arc::new(|_: &RuntimeEvent| {}),
        // The wake-budget notice: most tests below do not read it.
        Arc::new(|_: &str| {}),
    );
    (notifier, jobs, inbox, turn_running)
}

fn settled(id: &str, notify: bool) -> JobEvent {
    JobEvent::Settled {
        id: id.into(),
        state: JobState::Exited(0),
        duration_ms: 1200,
        notify,
    }
}

async fn lanes(inbox: &Inbox) -> Vec<Lane> {
    inbox
        .snapshot()
        .await
        .iter()
        .map(|entry| entry.lane)
        .collect()
}

/// The start/settle pair reaches the live runtime-event sink (ADR 0027 slice ③
/// item 5): the TUI's footer `jobs` item reads them there, never by polling the journal file.
#[tokio::test]
async fn start_and_settle_reach_the_live_runtime_sink() {
    use std::sync::Mutex;
    let dir = scratch("sink");
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_in = Arc::clone(&seen);
    let notifier = JobNotifier::new(
        Arc::new(Jobs::new(dir.join("jobs"))),
        Inbox::new(64),
        journal(&dir),
        Arc::new(AtomicBool::new(false)),
        Arc::new(move |event: &RuntimeEvent| {
            seen_in
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((event.name().to_string(), job_id_of(event)));
        }),
        Arc::new(|_: &str| {}),
    );
    notifier
        .deliver(JobEvent::Started {
            id: "job-1".into(),
            command: "make test".into(),
            output: OUTPUT_HINT.into(),
        })
        .await;
    notifier.deliver(settled("job-1", false)).await;
    assert_eq!(
        *seen.lock().unwrap_or_else(|e| e.into_inner()),
        vec![
            ("BackgroundJobStarted".to_string(), "job-1".to_string()),
            ("BackgroundJobSettled".to_string(), "job-1".to_string()),
        ]
    );
}

/// The job id a start/settle record carries.
fn job_id_of(event: &RuntimeEvent) -> String {
    match event {
        RuntimeEvent::BackgroundJobStarted { job_id, .. } => job_id.clone(),
        RuntimeEvent::BackgroundJobSettled { job_id, .. } => job_id.clone(),
        _ => String::new(),
    }
}

/// The completion message names `job` as the read channel (ADR 0037 decision 4): a settle
/// the model has not seen tells it where to read.
#[tokio::test]
async fn the_completion_message_names_job_as_the_read_channel() {
    let dir = scratch("notice-text");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (notifier, jobs, inbox, _turn_running) = notifier(&dir);
    let mut events = jobs.subscribe();

    // A real finished job, so the message carries the tail and the read hint.
    let policy = SandboxPolicy::new(SandboxMode::FullAccess, &ws).expect("full-access policy");
    let sandbox = Sandbox::from_policy(policy).expect("full-access needs no backend");
    let started = jobs.start(&sandbox, &ws, "echo hi").expect("start the job");
    let id = loop {
        match tokio::time::timeout(Duration::from_secs(15), events.recv()).await {
            Ok(Ok(JobEvent::Settled { id, notify, .. })) if notify => break id,
            Ok(Ok(_)) => continue,
            other => panic!("no notifiable settle: {other:?}"),
        }
    };
    assert_eq!(id, started.id);

    notifier
        .deliver(JobEvent::Settled {
            id: id.clone(),
            state: radian_coding::JobState::Exited(0),
            duration_ms: 1,
            notify: true,
        })
        .await;
    let texts: Vec<String> = inbox
        .snapshot()
        .await
        .iter()
        .map(|entry| entry.message.text())
        .collect();
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].contains("hi"), "the tail is inline: {}", texts[0]);
    assert!(
        texts[0].contains("Read its output with job."),
        "the notice names the one tool: {}",
        texts[0]
    );
}

/// Busy chooses `inject` (`next_step`, no wake); idle chooses `followup` (`next_turn`, which
/// wakes a turn).
#[tokio::test]
async fn busy_injects_and_idle_wakes() {
    let dir = scratch("lanes");
    let (notifier, _jobs, inbox, turn_running) = notifier(&dir);

    turn_running.store(true, Ordering::SeqCst);
    notifier.deliver(settled("job-1", true)).await;
    assert_eq!(lanes(&inbox).await, vec![Lane::NextStep], "busy: inject");

    turn_running.store(false, Ordering::SeqCst);
    notifier.deliver(settled("job-2", true)).await;
    // `snapshot` lists the `next_turn` lane first, then `next_step`.
    assert_eq!(
        lanes(&inbox).await,
        vec![Lane::NextTurn, Lane::NextStep],
        "idle: followup"
    );
}

/// A worker doing round after round of "background job -> wake -> handle" with realistic
/// spacing is never degraded: six wakes minutes apart all open a turn. (The old per-message
/// count of 3 would stop this chain at the fourth round.)
#[tokio::test]
async fn a_worker_chain_of_six_wakes_is_never_degraded() {
    tokio::time::pause();
    let dir = scratch("rate-chain");
    let (notifier, _jobs, inbox, _) = notifier(&dir);

    for i in 0..6 {
        notifier.deliver(settled(&format!("job-{i}"), true)).await;
        assert!(
            inbox.claim_next_turn().await.is_some(),
            "round {i}: the wake opens a turn"
        );
        // Realistic spacing: the job ran minutes before its notice woke a turn.
        tokio::time::advance(Duration::from_secs(180)).await;
    }
    assert!(
        notifier.wakes() <= MAX_WAKES_PER_WINDOW,
        "a minutes-apart chain never fills the window: {}",
        notifier.wakes()
    );
}

/// A self-exciting loop (a job that ends at once whose turn only starts the next job,
/// wake gaps ~1 s) is degraded after N wakes: the next settle wakes nothing and is
/// injected only.
#[tokio::test]
async fn a_self_exciting_loop_is_degraded_after_n_wakes() {
    tokio::time::pause();
    let dir = scratch("rate-loop");
    let (notifier, _jobs, inbox, _) = notifier(&dir);

    for i in 0..MAX_WAKES_PER_WINDOW {
        notifier.deliver(settled(&format!("job-{i}"), true)).await;
        assert!(
            inbox.claim_next_turn().await.is_some(),
            "wake {i}: the loop still wakes while the budget lasts"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    assert_eq!(notifier.wakes(), MAX_WAKES_PER_WINDOW);

    notifier.deliver(settled("job-past-the-gate", true)).await;
    assert!(
        inbox.claim_next_turn().await.is_none(),
        "past the gate: the settle wakes no turn"
    );
    assert!(
        inbox.claim_next_step().await.is_some(),
        "past the gate: the settle is injected only (the degrade branch)"
    );
    assert_eq!(notifier.wakes(), MAX_WAKES_PER_WINDOW);
}

/// The window recovers with no user message: once the oldest wakes age out, settles wake
/// turns again.
#[tokio::test]
async fn the_window_recovers_with_no_user_message() {
    tokio::time::pause();
    let dir = scratch("rate-recover");
    let (notifier, _jobs, inbox, _) = notifier(&dir);

    for i in 0..MAX_WAKES_PER_WINDOW {
        notifier.deliver(settled(&format!("job-{i}"), true)).await;
        assert!(inbox.claim_next_turn().await.is_some());
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    notifier.deliver(settled("job-capped", true)).await;
    assert!(
        inbox.claim_next_turn().await.is_none(),
        "the gate is full before the window passes"
    );
    assert!(inbox.claim_next_step().await.is_some());

    // No user message: the window passing alone reopens the gate.
    tokio::time::advance(WAKE_WINDOW + Duration::from_secs(1)).await;
    notifier.deliver(settled("job-after-window", true)).await;
    assert!(
        inbox.claim_next_turn().await.is_some(),
        "after the window: the settle wakes a turn again"
    );
    assert_eq!(notifier.wakes(), 1);
}

/// A user message still resets the budget at once (ADR 0037 decision 3's reset survives
/// the rate gate).
#[tokio::test]
async fn a_user_message_still_resets_the_budget() {
    tokio::time::pause();
    let dir = scratch("rate-reset");
    let (notifier, _jobs, inbox, _) = notifier(&dir);

    for i in 0..3 {
        notifier.deliver(settled(&format!("job-{i}"), true)).await;
        assert!(inbox.claim_next_turn().await.is_some());
    }
    assert_eq!(notifier.wakes(), 3);

    notifier.reset_wakes();
    assert_eq!(notifier.wakes(), 0);
    notifier.deliver(settled("job-after-user", true)).await;
    assert!(
        inbox.claim_next_turn().await.is_some(),
        "a user message restarts the wake budget"
    );
    assert_eq!(notifier.wakes(), 1);
}

/// A settle the model already has (`notify: false`: a `job` wait, a `job` kill, a
/// session-exit cleanup) sends nothing.
#[tokio::test]
async fn a_non_notifiable_settle_is_silent() {
    let dir = scratch("silent");
    let (notifier, _jobs, inbox, _) = notifier(&dir);
    notifier.deliver(settled("job-1", false)).await;
    assert!(
        inbox.is_empty().await,
        "no message for a non-notifiable settle"
    );
    assert_eq!(notifier.wakes(), 0);
}

/// The start and settle events are written to the journal as the durable records.
#[tokio::test]
async fn the_start_and_settle_records_are_written() {
    let dir = scratch("journal");
    let (notifier, _jobs, _, _) = notifier(&dir);
    notifier
        .deliver(JobEvent::Started {
            id: "job-1".into(),
            command: "cargo test".into(),
            output: ".radian/jobs/job-1.out".into(),
        })
        .await;
    notifier.deliver(settled("job-1", true)).await;

    let session = Session::open(dir.join("s.jsonl")).unwrap();
    let started: Vec<(&str, &str, &str)> = session
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            EventData::BackgroundJobStarted {
                job_id,
                command,
                output,
            } => Some((job_id.as_str(), command.as_str(), output.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        vec![("job-1", "cargo test", ".radian/jobs/job-1.out")]
    );
    let settles: Vec<(&str, &str, u64)> = session
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            EventData::BackgroundJobSettled {
                job_id,
                state,
                duration_ms,
            } => Some((job_id.as_str(), state.as_str(), *duration_ms)),
            _ => None,
        })
        .collect();
    assert_eq!(settles, vec![("job-1", "exited 0", 1200)]);
}

/// The orphan scan reports a start with no settle (a crash) and never re-runs it.
#[tokio::test]
async fn orphan_jobs_reports_a_start_without_a_settle() {
    // A session with one settled and one orphaned job: only the orphan is reported.
    let dir = scratch("orphan");
    let (notifier, _jobs, _, _) = notifier(&dir);
    notifier
        .deliver(JobEvent::Started {
            id: "job-1".into(),
            command: "cargo test".into(),
            output: ".radian/jobs/job-1.out".into(),
        })
        .await;
    notifier
        .deliver(JobEvent::Started {
            id: "job-2".into(),
            command: "sleep 600".into(),
            output: ".radian/jobs/job-2.out".into(),
        })
        .await;
    notifier.deliver(settled("job-1", false)).await;

    let session = Session::open(dir.join("s.jsonl")).unwrap();
    assert_eq!(
        orphan_jobs(&session),
        vec![("job-2".to_string(), "sleep 600".to_string())]
    );
}

// ---------- ADR 0039 decision 6: no lost first event, no Settled before its Started ----------

fn full_access(ws: &Path) -> Sandbox {
    let policy = SandboxPolicy::new(SandboxMode::FullAccess, ws).expect("full-access policy");
    Sandbox::from_policy(policy).expect("full-access needs no backend")
}

/// Waits for the notifier to deliver the settle's System message. Because the notifier handles
/// a job's `Started` before its `Settled`, a delivery proves both durable records are written,
/// without a sleep: the idle settle sends a `followup`, which wakes the inbox.
async fn await_delivery(inbox: &Inbox) {
    loop {
        if inbox.claim_next_turn().await.is_some() {
            return;
        }
        tokio::time::timeout(Duration::from_secs(10), inbox.wake_signal())
            .await
            .expect("a settle must reach the notifier");
    }
}

/// The first job's `Started` and `Settled` both reach the journal. Subscribing **inside** the
/// spawned task would lose them: the job starts between `spawn` returning and the task running
/// (ADR 0039 decision 6).
#[tokio::test]
async fn the_first_job_events_are_not_lost() {
    let dir = scratch("first-events");
    let (notifier, jobs, inbox, _turn) = notifier(&dir);
    notifier.spawn(&tokio::runtime::Handle::current());
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let sandbox = full_access(&ws);

    let info = jobs
        .start(&sandbox, &ws, "true")
        .expect("start the first job");
    await_delivery(&inbox).await;

    let session = Session::open(dir.join("s.jsonl")).unwrap();
    let (mut started, mut settled) = (Vec::new(), Vec::new());
    for event in session.events() {
        match &event.data {
            EventData::BackgroundJobStarted { job_id, output, .. } => {
                started.push((job_id.clone(), output.clone()))
            }
            EventData::BackgroundJobSettled { job_id, .. } => settled.push(job_id.clone()),
            _ => {}
        }
    }
    assert_eq!(
        started,
        vec![(info.id.clone(), OUTPUT_HINT.to_owned())],
        "the first job's Started must not be lost"
    );
    assert_eq!(
        settled,
        vec![info.id.clone()],
        "the first job's Settled must not be lost"
    );
}

/// A burst of instant jobs: every `Settled` in the journal is preceded by its `Started`. The
/// supervisor is started only after the job is registered and `Started` is published, so an
/// instant leader cannot settle first (ADR 0039 decision 6).
#[tokio::test]
async fn a_burst_of_instant_jobs_keeps_every_settle_behind_its_start() {
    let dir = scratch("burst-order");
    let (notifier, jobs, inbox, _turn) = notifier(&dir);
    notifier.spawn(&tokio::runtime::Handle::current());
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let sandbox = full_access(&ws);

    let mut ids = Vec::new();
    for _ in 0..1000 {
        // Keep every settle on the idle lane, so `await_delivery` is the message the notifier
        // sends after writing both records.
        notifier.reset_wakes();
        let info = jobs.start(&sandbox, &ws, "true").expect("an instant job");
        ids.push(info.id);
        await_delivery(&inbox).await;
    }

    let session = Session::open(dir.join("s.jsonl")).unwrap();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut settles = 0usize;
    for event in session.events() {
        match &event.data {
            EventData::BackgroundJobStarted { job_id, .. } => {
                seen.insert(job_id.as_str());
            }
            EventData::BackgroundJobSettled { job_id, .. } => {
                assert!(
                    seen.contains(job_id.as_str()),
                    "{job_id} settled before it started"
                );
                settles += 1;
            }
            _ => {}
        }
    }
    assert_eq!(settles, 1000, "every job must have its settle");
    assert_eq!(seen.len(), 1000, "every job must have its start");
    assert!(
        seen.contains(ids[0].as_str()),
        "the first job's Started must not be lost"
    );
}

/// An ended session releases its journal: once the session drops its job registry, the
/// subscriber task sees the channel close and ends, so nothing keeps the journal (and its
/// lock) alive. Until 2026-10-17 the notifier held the registry strongly, the channel never
/// closed, and a removed task's id could not be spawned again in the same process.
#[tokio::test]
async fn an_ended_session_releases_its_journal() {
    let dir = scratch("release");
    let journal = journal(&dir);
    let jobs = Arc::new(Jobs::new(dir.join("jobs")));
    let notifier = JobNotifier::new(
        Arc::clone(&jobs),
        Inbox::new(8),
        Arc::clone(&journal),
        Arc::new(AtomicBool::new(false)),
        // The live runtime-event sink: this test only watches the registry drop.
        Arc::new(|_: &RuntimeEvent| {}),
        Arc::new(|_: &str| {}),
    );
    notifier.spawn(&tokio::runtime::Handle::current());
    // The session ends: the host drops its notifier and its registry.
    drop(notifier);
    drop(jobs);
    let released = tokio::time::timeout(Duration::from_secs(10), async {
        while Arc::strong_count(&journal) > 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        released.is_ok(),
        "the subscriber task still holds the journal: {} references",
        Arc::strong_count(&journal)
    );
}

/// A later wake ends the degrade burst: the woken turn absorbs the waiting injects, so
/// the degraded count clears with the turn, not only on a user message.
#[tokio::test]
async fn a_later_wake_ends_the_degrade_burst() {
    tokio::time::pause();
    let dir = scratch("rate-burst-end");
    let (notifier, _jobs, inbox, _) = notifier(&dir);

    for i in 0..MAX_WAKES_PER_WINDOW {
        notifier.deliver(settled(&format!("job-{i}"), true)).await;
        assert!(inbox.claim_next_turn().await.is_some());
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    notifier.deliver(settled("job-d1", true)).await;
    notifier.deliver(settled("job-d2", true)).await;
    assert_eq!(notifier.degraded(), 2);

    // The window passes and a later wake lands: the burst is over.
    tokio::time::advance(WAKE_WINDOW + Duration::from_secs(1)).await;
    notifier.deliver(settled("job-after-window", true)).await;
    assert!(inbox.claim_next_turn().await.is_some());
    assert_eq!(notifier.degraded(), 0);
}

/// A settle past the gate is injected with the degrade mark, counted, and announced:
/// the durable `InboxEnqueued` says `degraded`, and the wake notice carries the count
/// since the last wake.
#[tokio::test]
async fn a_degraded_notice_writes_the_marked_record_and_announces_it() {
    use std::sync::Mutex;
    let dir = scratch("degraded");
    let journal = journal(&dir);
    let inbox = Inbox::durable(64, Arc::clone(&journal));
    let notices: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notices_in = Arc::clone(&notices);
    let notifier = JobNotifier::new(
        Arc::new(Jobs::new(dir.join("jobs"))),
        Arc::clone(&inbox),
        journal,
        Arc::new(AtomicBool::new(false)),
        Arc::new(|_: &RuntimeEvent| {}),
        Arc::new(move |line: &str| {
            notices_in
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(line.to_owned());
        }),
    );

    tokio::time::pause();
    for _ in 0..MAX_WAKES_PER_WINDOW {
        notifier.deliver(settled("job-fill", true)).await;
        // Each woken turn absorbs the queue; the test drains it like the driver would.
        assert!(inbox.claim_next_turn().await.is_some());
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    notifier.deliver(settled("job-d1", true)).await;
    notifier.deliver(settled("job-d2", true)).await;
    assert_eq!(notifier.degraded(), 2);

    // The waking followups record no degrade; the two degraded injects do.
    let session = Session::open(dir.join("s.jsonl")).unwrap();
    let degraded: Vec<bool> = session
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            EventData::InboxEnqueued { degraded, .. } => Some(*degraded),
            _ => None,
        })
        .collect();
    assert_eq!(
        degraded,
        [vec![false; MAX_WAKES_PER_WINDOW as usize], vec![true, true]].concat()
    );

    // Every degrade fires the wake notice with the count since the last wake.
    assert_eq!(
        *notices.lock().unwrap_or_else(|e| e.into_inner()),
        vec![
            "wake budget used up: 1 job notices are waiting — send any message to continue"
                .to_owned(),
            "wake budget used up: 2 job notices are waiting — send any message to continue"
                .to_owned(),
        ]
    );

    // A user message restarts the count with the budget.
    notifier.reset_wakes();
    assert_eq!(notifier.degraded(), 0);
}
