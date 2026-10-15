//! The background-job notifier (ADR 0037 slices 3-4): the durable start/settle records, the
//! busy/idle lane choice, the wake cap, and the crash-orphan scan.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use radian_coding::{JobEvent, JobState, Jobs};
use radian_host::job_notify::{JobNotifier, MAX_CONSECUTIVE_WAKES, orphan_jobs};
use radian_runtime::{Inbox, Lane};
use radian_session::{EventData, Session, SessionCreatedData, SessionJournal};
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

fn notifier(dir: &Path) -> (Arc<JobNotifier>, Arc<Inbox>, Arc<AtomicBool>) {
    let inbox = Inbox::new(64);
    let jobs = Arc::new(Jobs::new(dir));
    let turn_running = Arc::new(AtomicBool::new(false));
    let notifier = JobNotifier::new(
        jobs,
        Arc::clone(&inbox),
        journal(dir),
        Arc::clone(&turn_running),
    );
    (notifier, inbox, turn_running)
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

/// Busy chooses `inject` (`next_step`, no wake); idle chooses `followup` (`next_turn`, which
/// wakes a turn).
#[tokio::test]
async fn busy_injects_and_idle_wakes() {
    let dir = scratch("lanes");
    let (notifier, inbox, turn_running) = notifier(&dir);

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

/// After `MAX_CONSECUTIVE_WAKES` wake-started turns the next settle is injected only, and a
/// user message resets the budget.
#[tokio::test]
async fn the_wake_cap_degrades_to_inject_and_a_user_message_resets_it() {
    let dir = scratch("cap");
    let (notifier, inbox, _) = notifier(&dir);

    for i in 0..MAX_CONSECUTIVE_WAKES {
        notifier.deliver(settled(&format!("job-{i}"), true)).await;
    }
    assert_eq!(notifier.wakes(), MAX_CONSECUTIVE_WAKES);
    let wake_lane = lanes(&inbox)
        .await
        .iter()
        .filter(|l| **l == Lane::NextTurn)
        .count();
    let inject_lane = lanes(&inbox)
        .await
        .iter()
        .filter(|l| **l == Lane::NextStep)
        .count();
    assert_eq!(
        wake_lane, MAX_CONSECUTIVE_WAKES as usize,
        "the first {MAX_CONSECUTIVE_WAKES} wakes use the waking lane"
    );
    assert_eq!(inject_lane, 0);

    notifier.deliver(settled("job-cap", true)).await;
    assert_eq!(
        lanes(&inbox)
            .await
            .iter()
            .filter(|l| **l == Lane::NextStep)
            .count(),
        1,
        "the settle past the cap is injected only"
    );

    notifier.reset_wakes();
    assert_eq!(notifier.wakes(), 0);
    notifier.deliver(settled("job-after-user", true)).await;
    assert_eq!(
        lanes(&inbox)
            .await
            .iter()
            .filter(|l| **l == Lane::NextTurn)
            .count(),
        MAX_CONSECUTIVE_WAKES as usize + 1,
        "a user message restarts the wake budget"
    );
}

/// A settle the model already has (`notify: false`: a `job_output wait`, a `job_kill`, a
/// session-exit cleanup) sends nothing.
#[tokio::test]
async fn a_non_notifiable_settle_is_silent() {
    let dir = scratch("silent");
    let (notifier, inbox, _) = notifier(&dir);
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
    let (notifier, _, _) = notifier(&dir);
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
    let (notifier, _, _) = notifier(&dir);
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
