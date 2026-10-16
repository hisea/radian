//! Background-job output hardening (ADR 0039 decision 1): the output file is created
//! `O_CREAT|O_EXCL|O_NOFOLLOW` 0600 in a caller-owned 0700 directory that is never derived
//! from the workspace.
//!
//! These tests drive [`Jobs`] directly with a `FullAccess` sandbox, so the file's location and
//! permissions are pinned without a host. The id-predicting tests serialize on `LOCK`, because
//! they start a probe job to learn the next id: with `NEXT_JOB` process-global, a concurrent
//! start would consume the predicted id.

mod common;

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::TempDir;
use radian_coding::Jobs;
use radian_sandbox::{Sandbox, SandboxMode, SandboxPolicy};

/// Serializes the tests that predict a job id.
static LOCK: Mutex<()> = Mutex::new(());

fn full_access(ws: &Path) -> Sandbox {
    let policy = SandboxPolicy::new(SandboxMode::FullAccess, ws).expect("full-access policy");
    Sandbox::from_policy(policy).expect("full-access needs no backend")
}

/// Bounded poll for a file's existence; the sleep is the poll interval, not the
/// synchronization.
async fn wait_for(path: &Path, what: &str) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out waiting for {what} at {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The id `next` after `id`, for a `job-<pid>-<n>` id.
fn next_id(id: &str) -> String {
    let (prefix, n) = id.rsplit_once('-').expect("a job id");
    let n: u64 = n.parse().expect("a numeric suffix");
    format!("{prefix}-{}", n + 1)
}

#[cfg(unix)]
fn plant_symlink(link: &Path, target: &Path) {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// The job's output file lives under the registry's own (0700) directory, is 0600, and the
/// workspace never grows a `.radian/jobs` directory.
#[cfg(unix)]
#[tokio::test]
async fn the_output_file_is_owner_only_and_under_the_jobs_directory() {
    let _guard = LOCK.lock().unwrap();
    let dir = TempDir::new("job-sec-private");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let jobs_dir = dir.file("sessions/sid.jobs"); // outside the workspace, not yet created
    let sandbox = full_access(&ws);
    let jobs = Jobs::new(&jobs_dir);

    let info = jobs
        .start(&sandbox, &ws, "echo hi", None)
        .expect("start the job");
    let output = jobs.output_path(&info.id);
    wait_for(&output, "the output file").await;

    assert_eq!(mode(&jobs_dir), 0o700, "the jobs directory is 0700");
    assert_eq!(mode(&output), 0o600, "the output file is 0600");
    assert!(
        output.starts_with(&jobs_dir),
        "the output file is under the jobs directory"
    );
    assert!(
        !ws.join(".radian").join("jobs").exists(),
        "the workspace must not grow a jobs directory"
    );

    jobs.kill_all();
}

/// A symlink pre-planted at the next job's output path (in the new location **and** in the old
/// workspace location) never receives the job's output. The outside file is unchanged, and the
/// start either fails cleanly or uses a fresh regular file.
#[cfg(unix)]
#[tokio::test]
async fn a_planted_symlink_at_the_next_output_file_is_not_followed() {
    let _guard = LOCK.lock().unwrap();
    let dir = TempDir::new("job-sec-symlink");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let jobs_dir = dir.file("sessions/sid.jobs");
    let sandbox = full_access(&ws);
    let jobs = Jobs::new(&jobs_dir);

    // Learn the next id: the probe consumes `n`, so the job below takes `n + 1`.
    let probe = jobs
        .start(&sandbox, &ws, "true", None)
        .expect("a probe job");
    let predicted = next_id(&probe.id);

    let outside = dir.file("outside.txt");
    std::fs::write(&outside, "UNCHANGED\n").unwrap();
    plant_symlink(&jobs_dir.join(format!("{predicted}.out")), &outside);
    plant_symlink(
        &ws.join(".radian/jobs").join(format!("{predicted}.out")),
        &outside,
    );

    let result = jobs.start(&sandbox, &ws, "echo LEAK", None);

    assert_eq!(
        std::fs::read_to_string(&outside).unwrap(),
        "UNCHANGED\n",
        "a pre-planted symlink must never receive the job's output"
    );
    match result {
        // The exclusive open refused the planted link.
        Err(error) => assert_eq!(error.code, "jobs_output", "{error:?}"),
        // Or the job took a fresh id: its file is a regular file, not the planted link.
        Ok(info) => {
            let path = jobs.output_path(&info.id);
            assert!(
                std::fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_file(),
                "a fresh output must be a regular file"
            );
        }
    }

    jobs.kill_all();
}

/// A pre-existing regular file at the next id is never appended to: the exclusive create fails
/// cleanly. Dropping `create_new` (keeping the append open) would append to it.
#[cfg(unix)]
#[tokio::test]
async fn an_existing_output_file_at_the_next_id_is_not_appended_to() {
    let _guard = LOCK.lock().unwrap();
    let dir = TempDir::new("job-sec-existing");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let jobs_dir = dir.file("sessions/sid.jobs");
    let sandbox = full_access(&ws);
    let jobs = Jobs::new(&jobs_dir);

    let probe = jobs
        .start(&sandbox, &ws, "true", None)
        .expect("a probe job");
    let planted = jobs.output_path(&next_id(&probe.id));
    std::fs::write(&planted, "PREEXISTING\n").unwrap();

    let result = jobs.start(&sandbox, &ws, "echo LEAK", None);

    assert_eq!(
        std::fs::read_to_string(&planted).unwrap(),
        "PREEXISTING\n",
        "an existing output file must not be appended to"
    );
    let error = result.expect_err("the exclusive create must refuse an existing file");
    assert_eq!(error.code, "jobs_output", "{error:?}");

    jobs.kill_all();
}
