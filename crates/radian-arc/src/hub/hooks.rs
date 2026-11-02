//! `[hub.hooks] after_pending` (ADR 0048 §16): the commands the hub runs when a live session's
//! pending queue gets a new head.
//!
//! The hook point belongs to the session's long-lived `subscribe` connection, not to the
//! scheduling loop: [`crate::hub::sessions`] decides that a head is new and calls [`fire`]. The
//! call returns at once and the commands run on a thread of their own, so a slow or failing
//! command never delays the subscription, another session's hook or an HTTP request.
//!
//! Each command is one `sh -c` line, the shape arc's `[hooks]` points use, and runs in the
//! repository root through the gate's shell runner. Its limit is that repository's
//! `[gate] command_timeout_secs`, the same rule arc's own hooks follow; a timeout kills the
//! command's process group. stdout, stderr and one line per failure or timeout are appended to
//! `$RADIAN_HOME/hub/hub.log`.
//!
//! The environment carries only the four values ADR 0034's privacy rule allows. Nothing else
//! from the request — no command text, path, arguments or question text — is passed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use radian_host::config::HubHooks;

use crate::gate::{SeriesOutcome, run_shell};
use crate::store::Store;

/// The hook point's name in `[hub.hooks]`.
pub const AFTER_PENDING: &str = "after_pending";

/// The log every hook's output and failure lines go to, inside the hub directory.
pub const HOOK_LOG: &str = "hub.log";

/// The environment every `after_pending` command receives. One list for the code and the
/// manual. The names are set with `Command::env` by name, so `radian-host/tests/env_docs.rs`
/// does not take them for inputs a person sets.
pub const HOOK_ENV: &[&str] = &[
    "RADIAN_HUB_REPO",
    "RADIAN_HUB_SESSION",
    "RADIAN_HUB_PENDING_KIND",
    "RADIAN_HUB_TOOL",
];

/// Serializes appends from the hook threads to one `hub.log`.
static LOG_LOCK: Mutex<()> = Mutex::new(());

/// Names each command's scratch output file; two hooks may run at the same time.
static TEMP: AtomicU64 = AtomicU64::new(0);

/// One new pending head, as the subscription reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingHead {
    /// `approval` or `question`.
    pub kind: String,
    /// The tool name; empty for a question.
    pub tool: String,
}

/// Starts the `after_pending` commands for one new head and returns at once. `repo_root` is the
/// registered repository's path, `repo` its registry name and `session` the session id.
pub fn fire(
    home: &Path,
    hooks: &HubHooks,
    repo_root: &Path,
    repo: &str,
    session: &str,
    head: &PendingHead,
) {
    if hooks.after_pending.is_empty() {
        return;
    }
    let commands = hooks.after_pending.clone();
    let home = home.to_path_buf();
    let repo_root = repo_root.to_path_buf();
    let repo = repo.to_owned();
    let session = session.to_owned();
    let head = head.clone();
    // A failed spawn loses one notification and nothing else; the subscription goes on.
    let _ = std::thread::Builder::new()
        .name(format!("hub-hook-{session}"))
        .spawn(move || run_all(&home, &commands, &repo_root, &repo, &session, &head));
}

/// Runs every command of the point in order. A command that fails or times out does not stop the
/// next one, the rule arc's hooks follow.
fn run_all(
    home: &Path,
    commands: &[String],
    repo_root: &Path,
    repo: &str,
    session: &str,
    head: &PendingHead,
) {
    let log = radian_host::hub_dir_for(home).join(HOOK_LOG);
    // The hub creates its directory before it starts; a direct call (a test, or a hub whose
    // directory was removed) still needs the scratch files' parent.
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // The limit is the repository's own arc configuration, exactly as arc's hooks read it. A
    // configuration that cannot be read leaves nothing to run against: one line, no commands.
    let timeout = match Store::at(repo_root).config() {
        Ok(config) => Duration::from_secs(config.gate.command_timeout_secs),
        Err(error) => {
            append(
                &log,
                &format!(
                    "{} hook `{AFTER_PENDING}` repo={repo} session={session} cannot read the \
                     repository configuration: {}\n",
                    stamp(),
                    error.message()
                ),
            );
            return;
        }
    };
    for (index, line) in commands.iter().enumerate() {
        run_one(
            &log,
            repo_root,
            line,
            index,
            commands.len(),
            &timeout,
            repo,
            session,
            head,
        );
    }
}

/// Runs one command and appends its output and outcome to `log`.
#[allow(clippy::too_many_arguments)]
fn run_one(
    log: &Path,
    repo_root: &Path,
    line: &str,
    index: usize,
    total: usize,
    timeout: &Duration,
    repo: &str,
    session: &str,
    head: &PendingHead,
) {
    let number = index + 1;
    let scratch = scratch_path(log);
    let env = [
        ("RADIAN_HUB_REPO", repo.to_owned()),
        ("RADIAN_HUB_SESSION", session.to_owned()),
        ("RADIAN_HUB_PENDING_KIND", head.kind.clone()),
        ("RADIAN_HUB_TOOL", head.tool.clone()),
    ];
    let outcome = run_shell(repo_root, line, *timeout, &scratch, &env, &[], None);
    let output = std::fs::read(&scratch)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let _ = std::fs::remove_file(&scratch);

    let mut text = format!(
        "{} hook `{AFTER_PENDING}` repo={repo} session={session} kind={} tool={} command \
         {number}/{total}\n",
        stamp(),
        head.kind,
        head.tool
    );
    text.push_str(&output);
    if !output.is_empty() && !output.ends_with('\n') {
        text.push('\n');
    }
    let failure = match outcome {
        Ok(outcome) if outcome.outcome == SeriesOutcome::Passed => None,
        Ok(outcome) => Some(if outcome.outcome == SeriesOutcome::TimedOut {
            format!("timed out after {} s", timeout.as_secs())
        } else {
            format!(
                "exited {}",
                outcome
                    .exit_code
                    .map_or_else(|| "on a signal".to_owned(), |code| code.to_string())
            )
        }),
        Err(error) => Some(format!("could not run: {error}")),
    };
    if let Some(reason) = failure {
        text.push_str(&format!(
            "{} hook `{AFTER_PENDING}` repo={repo} session={session} command {number} \
             {reason}: {line}\n",
            stamp()
        ));
    }
    append(log, &text);
}

/// The scratch file one command's output goes to before it joins `hub.log`. The gate's runner
/// truncates its log, so it cannot write the shared file directly.
fn scratch_path(log: &Path) -> PathBuf {
    let n = TEMP.fetch_add(1, Ordering::SeqCst);
    log.with_file_name(format!(".hub-hook-{}-{n}.log", std::process::id()))
}

/// Appends `text` to `log` in one write, under a lock so two sessions' hooks never interleave.
fn append(log: &Path, text: &str) {
    let _guard = LOG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(parent) = log.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let _ = file.write_all(text.as_bytes());
        let _ = file.flush();
    }
}

/// One RFC 3339 UTC timestamp with milliseconds, the shape `loop.log` uses.
fn stamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository whose arc configuration carries `timeout_secs` as its command limit.
    fn repo_with_timeout(root: &Path, timeout_secs: u64) {
        let arc = root.join(".radian/arc");
        std::fs::create_dir_all(&arc).expect("the arc directory is creatable");
        let text = crate::store::config::default_text("main").replace(
            "command_timeout_secs = 3600",
            &format!("command_timeout_secs = {timeout_secs}"),
        );
        std::fs::write(arc.join("config.toml"), text).expect("the configuration writes");
    }

    /// The hook runs in the repository root with exactly the four variables and logs its run.
    #[test]
    fn the_runner_sets_the_four_variables_and_appends_to_hub_log() {
        let dir = crate::test_support::TempDir::new("hub-hooks-runner");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("the repository is creatable");
        repo_with_timeout(&repo, 5);
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("the home is creatable");
        let env_file = repo.join("env.txt");
        run_all(
            &home,
            &[format!("env > '{}'", env_file.display())],
            &repo,
            "repo",
            "session",
            &PendingHead {
                kind: "approval".to_owned(),
                tool: "shell".to_owned(),
            },
        );
        let text = std::fs::read_to_string(&env_file).expect("the command wrote its environment");
        for line in [
            "RADIAN_HUB_REPO=repo",
            "RADIAN_HUB_SESSION=session",
            "RADIAN_HUB_PENDING_KIND=approval",
            "RADIAN_HUB_TOOL=shell",
        ] {
            assert!(
                text.lines().any(|candidate| candidate == line),
                "{line}: {text}"
            );
        }
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("RADIAN_HUB_"))
                .count(),
            HOOK_ENV.len()
        );
        let log = std::fs::read_to_string(home.join("hub/hub.log")).expect("the log writes");
        assert!(log.contains("hook `after_pending` repo=repo session=session"));
    }

    /// A command past the timeout is killed before its next step, and the outcome is logged.
    #[test]
    fn a_command_past_its_timeout_is_killed_and_logged() {
        let dir = crate::test_support::TempDir::new("hub-hooks-timeout");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("the repository is creatable");
        repo_with_timeout(&repo, 1);
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("the home is creatable");
        let marker = repo.join("after.txt");
        let pidfile = repo.join("child.pid");
        run_all(
            &home,
            &[format!(
                "sleep 30 & echo $! > '{}'; wait; printf after > '{}'",
                pidfile.display(),
                marker.display()
            )],
            &repo,
            "repo",
            "sleepy",
            &PendingHead {
                kind: "approval".to_owned(),
                tool: "shell".to_owned(),
            },
        );
        // The command wrote its background `sleep`'s pid before the timeout. A path that
        // returns `TimedOut` without killing the process **group** leaves that child alive
        // (`run_all` blocks through the timeout, so the kill has already happened). A killed
        // process also stays a zombie until its new parent reaps it: `sh` died with the group,
        // so the `sleep` is reparented and reaped asynchronously. Poll before deciding the kill
        // missed the group, as the route test does.
        let pid: u32 = std::fs::read_to_string(&pidfile)
            .expect("the command wrote its child pid")
            .trim()
            .parse()
            .expect("the pid is a number");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while crate::process::alive(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if crate::process::alive(pid) {
            // Clean up a child the broken path left behind, so a failed assertion does not
            // leak a sleeping process for its full 30 s.
            let _ = crate::process::signal(pid, crate::process::SIGKILL);
        }
        assert!(
            !crate::process::alive(pid),
            "the timed-out command's process group was killed"
        );
        assert!(
            !marker.exists(),
            "the timed-out command ran no further step"
        );
        let log = std::fs::read_to_string(home.join("hub/hub.log")).expect("the log writes");
        assert!(
            log.contains("session=sleepy") && log.contains("timed out after 1 s"),
            "{log}"
        );
    }
}
