//! The temp-directory leak check (`docs/backlog/test-temp-leak.md` acceptance 3): run one
//! test from each large temp-directory family in its own child process, then prove no new
//! entry named with that child's pid is left under the system temp dir.
//!
//! The entries are filtered by the child pid because nextest runs tests in parallel: another
//! test's live directory must not fail this one. A family whose test binary is not built (a
//! lone `cargo test --test temp_leak -p radian-coding` builds only this target) is skipped;
//! `cargo nextest run -p radian-coding` and the workspace run build every target.
//!
//! Mutation: restore the `radian-coding-tests-<pid>` shared root in `tests/common/mod.rs`
//! and the `read` child leaves one entry named with its pid, so this test fails.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;

/// `(test target, exact test name)` for each family; the target is the file stem of the test
/// binary in `<target>/<profile>/deps`.
const CASES: &[(&str, &str)] = &[
    // The largest families first.
    ("read", "read_prefixes_each_line_with_its_file_line_number"),
    (
        "full_reads",
        "a_skill_bundled_file_reads_through_read_in_both_tiers",
    ),
    (
        "read_roots",
        "a_registry_source_file_is_read_by_absolute_path",
    ),
    ("append", "a_torn_tail_is_truncated_before_appending"),
    (
        "budget",
        "an_over_budget_turn_completes_and_writes_no_context_budget_event",
    ),
    ("job_notify", "start_and_settle_reach_the_live_runtime_sink"),
    (
        "subagent_headers",
        "the_header_value_reaches_the_child_through_the_environment_not_its_argv",
    ),
    ("subagent", "a_general_child_has_no_subagent_tool"),
    ("cli", "one_shot_streams_text_to_stdout_only"),
    (
        "mcp",
        "tool_call_round_trips_through_core_on_both_protocol_versions",
    ),
];

fn deps_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary's path");
    exe.parent()
        .expect("the test binary lives in <target>/<profile>/deps")
        .to_path_buf()
}

/// The newest executable named `<target>-<hash>` in the deps directory (never the `.d`
/// dependency file beside it).
fn find_binary(target: &str) -> Option<PathBuf> {
    let prefix = format!("{target}-");
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(deps_dir()).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) || name.ends_with(".d") {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(newest, _)| modified > *newest) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

fn temp_entries() -> BTreeSet<String> {
    let dir = std::env::temp_dir();
    std::fs::read_dir(dir)
        .expect("the temp dir is readable")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

/// Whether an entry belongs to one of the child processes we started. The helpers name a
/// directory `{prefix}-{pid}-...`, so the pid is one whole `-`-separated field.
fn belongs_to(entry: &str, pids: &[u32]) -> bool {
    entry
        .split('-')
        .any(|field| pids.iter().any(|pid| field == pid.to_string()))
}

#[test]
fn every_test_temp_directory_is_removed_when_its_test_ends() {
    let before = temp_entries();
    let mut pids = Vec::new();
    let mut ran = Vec::new();
    // The exec suite's harness uses a short `/tmp` base (a control socket would exceed
    // `SUN_LEN` under the workspace sandbox's long TMPDIR), so it cannot run from inside the
    // sandbox; the gate's full Verify runs this test unconfined.
    let confined = radian_sandbox::confined_by_outer_sandbox();

    for (target, test) in CASES {
        if confined && *target == "cli" {
            eprintln!("skipped {target}: /tmp is not writable inside the sandbox");
            continue;
        }
        let Some(binary) = find_binary(target) else {
            eprintln!("skipped {target}: no test binary built (run the package via nextest)");
            continue;
        };
        let mut child = Command::new(&binary)
            .arg("--exact")
            .arg(test)
            .arg("--nocapture")
            .spawn()
            .unwrap_or_else(|error| panic!("spawn {}: {error}", binary.display()));
        pids.push(child.id());
        let status = child.wait().expect("wait for the child test");
        assert!(
            status.success(),
            "the child test {target}::{test} failed (see its output above)"
        );
        ran.push(*target);
    }

    assert!(
        ran.contains(&"read"),
        "the coding family did not run; build it with `cargo nextest run -p radian-coding`"
    );

    let leaked: Vec<String> = temp_entries()
        .into_iter()
        .filter(|entry| !before.contains(entry) && belongs_to(entry, &pids))
        .collect();
    assert!(
        leaked.is_empty(),
        "the child tests left temp entries behind: {leaked:?} (ran {ran:?})"
    );
}

/// The filter's own guard: an entry named with a pid matches only as a whole field.
#[test]
fn pid_matching_does_not_match_a_longer_number() {
    assert!(belongs_to("radian-coding-tests-42-0", &[42]));
    assert!(!belongs_to("radian-coding-tests-420-0", &[42]));
    assert!(!belongs_to("radian-coding-tests-420-0", &[42, 7]));
}
