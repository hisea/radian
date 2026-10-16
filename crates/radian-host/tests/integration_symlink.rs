//! ADR 0039 decision 4 / ADR 0030: a symlink at **any** level of the
//! integration worktree's root refuses creation before git runs.
//!
//! `RADIAN_HOME` is process-wide, so this binary deliberately holds exactly one test (like
//! `task_root.rs`): it owns its whole home, including the `worktrees/` level every other
//! integration test shares.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use radian_host::{SessionQueue, TaskBuilder, TaskRegistry};
use radian_session::{SessionCreatedData, SessionJournal};
use serde_json::json;

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("radian-integ-sym-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

fn git(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git runs")
        .success()
}

fn registry_for(lead: &Path, label: &str) -> TaskRegistry {
    let session_dir = lead.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let journal = Arc::new(
        SessionJournal::create(
            session_dir.join(format!("{label}.jsonl")),
            "lead",
            SessionCreatedData::new(lead.to_string_lossy(), json!({})),
        )
        .expect("the lead journal"),
    );
    let queue: Arc<SessionQueue> = Arc::new(SessionQueue::new());
    let builder: TaskBuilder =
        Arc::new(|_ctx| Err(radian_host::SessionError::Message("unused".into())));
    TaskRegistry::new(lead.to_path_buf(), "lead", journal, queue, builder)
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_root_level_refuses_the_integration_worktree() {
    let scratch = scratch("home");
    let home = scratch.join("radian-home");
    let outside = scratch.join("outside");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    // SAFETY: this test binary holds exactly one test.
    unsafe { std::env::set_var("RADIAN_HOME", &home) };

    let lead = scratch.join("lead");
    std::fs::create_dir_all(&lead).unwrap();
    assert!(git(&lead, &["init", "-q", "-b", "main"]));
    assert!(git(
        &lead,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]
    ));

    let registry = registry_for(&lead, "lead");

    let refused = |result: Result<radian_host::Integration, radian_host::TaskError>| {
        let error = match result {
            Ok(_) => panic!("a symlink at a root level must be refused"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("symlink"),
            "the error must name the symlink: {error}"
        );
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            0,
            "the symlink target stays empty"
        );
    };

    // (a) the `worktrees/` level is a symlink.
    std::os::unix::fs::symlink(&outside, home.join("worktrees")).unwrap();
    refused(registry.ensure_integration().await);
    std::fs::remove_file(home.join("worktrees")).unwrap();

    // (b) the `<repo-key>` level is a symlink.
    std::fs::create_dir_all(home.join("worktrees")).unwrap();
    let root = registry
        .ensure_integration()
        .await
        .expect("create under a clean worktrees level")
        .worktree;
    let key = root.parent().unwrap().to_path_buf();
    // A second registry over the same repo now sees the key level as a symlink.
    std::fs::remove_dir_all(&key).unwrap();
    std::os::unix::fs::symlink(&outside, &key).unwrap();
    let other = registry_for(&lead, "other");
    refused(other.ensure_integration().await);
    std::fs::remove_file(&key).unwrap();
}
