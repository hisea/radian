//! The hub's scheduling loop (ADR 0048 decision 16).
//!
//! `radian hub` with no subcommand takes `$RADIAN_HOME/hub/hub.lock` for its life, then keeps
//! one loop per registered repository. Every repository's loop runs on its own OS thread,
//! because a pass is synchronous and blocking: it spawns and waits for git and gate processes,
//! and it waits the interval in a sleep. The hub's own thread re-reads `repos.toml` every
//! `[hub] interval_secs`, composes the status the HTTP API and the status lines share, and
//! stops every loop on SIGINT or SIGTERM (releasing `hub.lock` last) or after a rebuilt
//! binary (then `exec` of the recorded path).
//!
//! A repository whose `runner.lock` another process holds is not scheduled: its status line
//! names the holder, and the loop probes the lock again every interval. The hub never writes
//! the registry.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use radian_host::config::Config;

use crate::run::{self, LoopState, Passed};
use crate::store::{Error, RunnerLock, Store};

use super::lock::HubLock;
use super::registry::{Registry, RepoEntry};
use super::status::{HubStatus, RepoState, RepoStatus};

/// How often the hub's own thread wakes to look at the registry, the status and the signals.
const TICK: Duration = Duration::from_millis(100);

/// Runs the scheduling loop until SIGINT or SIGTERM (or until the hub replaces itself with a
/// rebuilt binary). `lock` is the `hub.lock` the caller already took: the hub holds it for its
/// whole life and releases it last. `emit` receives every line the hub prints: each
/// repository's loop lines and the status block when it changes.
///
/// # Errors
///
/// The hub configuration cannot be read, or `hub.lock` cannot be opened again after a failed
/// restart.
pub fn run(
    registry: &Registry,
    emit: Arc<dyn Fn(&str) + Send + Sync>,
    lock: HubLock,
    config: Option<&std::path::Path>,
) -> Result<(), Error> {
    let mut lock = Some(lock);
    let config = load_config(config)?;
    let interval = interval_secs(&config);
    let cap = config.hub.concurrency.max(1);
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    // [`http::serve`] already installed it before the hub's first line; installing it again
    // without clearing keeps a SIGTERM that arrived while the hub was starting.
    run::install_stop_signals();

    'hub: loop {
        let stop_all = Arc::new(AtomicBool::new(false));
        let restart = Arc::new(AtomicBool::new(false));
        let restart_path: Arc<Mutex<Option<Restart>>> = Arc::new(Mutex::new(None));
        let dirty = Arc::new(AtomicBool::new(true));
        let mut repos: BTreeMap<String, RepoHandle> = BTreeMap::new();
        let mut retired: Vec<JoinHandle<()>> = Vec::new();
        let mut last_status: Option<String> = None;
        let mut next_registry = Instant::now();
        let mut next_status = Instant::now();

        loop {
            let now = Instant::now();
            if run::stop_requested() || restart.load(Ordering::SeqCst) {
                stop_all.store(true, Ordering::SeqCst);
            }
            if now >= next_registry {
                reconcile(
                    registry,
                    &mut repos,
                    &mut retired,
                    &emit,
                    &stop_all,
                    &restart,
                    &restart_path,
                    &dirty,
                    interval,
                    cap,
                );
                next_registry = now + Duration::from_secs(interval);
            }
            if dirty.swap(false, Ordering::SeqCst) || now >= next_status {
                let status = compose_status(registry, &repos, cap);
                let text = status.render();
                if last_status.as_deref() != Some(text.as_str()) {
                    (emit)(&text);
                    last_status = Some(text);
                }
                next_status = now + Duration::from_secs(1);
            }
            retired.retain(|handle| !handle.is_finished());
            if stop_all.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(TICK);
        }

        // Stop every loop and wait for it, so every repository's `runner.lock` is released
        // before the hub exits or execs.
        for handle in repos.values_mut() {
            handle.removed.store(true, Ordering::SeqCst);
        }
        for handle in repos.values_mut() {
            if let Some(handle) = handle.handle.take() {
                let _ = handle.join();
            }
        }
        for handle in retired {
            let _ = handle.join();
        }
        // `hub.lock` last (ADR 0048 decision 16).
        drop(lock.take());

        if restart.load(Ordering::SeqCst) {
            let target = restart_path.lock().expect("the restart lock").take();
            if let Some(target) = target {
                let error = run::replace_process(&target.program, &arguments);
                // The exec failed: say so in the repository whose pass claimed the restart, so
                // the failure is in a `loop.log` (ADR 0048 decision 13) and not only on
                // stdout, take `hub.lock` again, and go on with the old code. New loops record
                // the new identity, so they do not retry every pass.
                report_restart_failure(&*emit, &target, &error);
                lock = Some(take_hub_lock(registry)?);
                continue 'hub;
            }
        }
        return Ok(());
    }
}

/// The binary a rebuilt-binary restart will `exec`, and the repository whose pass claimed it
/// (so a failed `exec` is written to that repository's `loop.log`).
struct Restart {
    program: PathBuf,
    repo: PathBuf,
}

/// One line for a failed `exec`, in the claiming repository's `loop.log` and on stdout.
fn report_restart_failure(emit: &dyn Fn(&str), target: &Restart, error: &std::io::Error) {
    let line = format!("radian arc: cannot restart with the new binary: {error}\n");
    run::LoopLog::new(&Store::at(&target.repo)).write(&line);
    emit(&line);
}

/// One repository's loop handle in the hub's own thread.
struct RepoHandle {
    name: String,
    path: PathBuf,
    state: Arc<Mutex<RepoState>>,
    removed: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

/// Takes `hub.lock` or returns the one-line refusal naming the holder.
fn take_hub_lock(registry: &Registry) -> Result<HubLock, Error> {
    HubLock::take(registry.dir())
}

/// Re-reads the registry and starts or retires the loops that changed.
#[allow(clippy::too_many_arguments)]
fn reconcile(
    registry: &Registry,
    repos: &mut BTreeMap<String, RepoHandle>,
    retired: &mut Vec<JoinHandle<()>>,
    emit: &Arc<dyn Fn(&str) + Send + Sync>,
    stop_all: &Arc<AtomicBool>,
    restart: &Arc<AtomicBool>,
    restart_path: &Arc<Mutex<Option<Restart>>>,
    dirty: &Arc<AtomicBool>,
    interval: u64,
    cap: u64,
) {
    let entries = match registry.repos() {
        Ok(entries) => entries,
        Err(error) => {
            // A registry that cannot be read keeps the set the hub already schedules; one
            // line says why, and the next interval tries again.
            (emit)(&format!("radian hub: {}\n", error.message()));
            return;
        }
    };

    let mut keep: Vec<String> = Vec::with_capacity(entries.len());
    for entry in entries {
        match repos.get(&entry.name) {
            Some(handle) if handle.path == entry.path => {
                keep.push(entry.name);
                continue;
            }
            Some(_) => {}
            None => {}
        }
        // A name registered with another path is a different repository: retire the old loop
        // and start the new one.
        if let Some(old) = repos.remove(&entry.name) {
            old.removed.store(true, Ordering::SeqCst);
            if let Some(handle) = old.handle {
                retired.push(handle);
            }
        }
        let state = Arc::new(Mutex::new(RepoState::Starting));
        let removed = Arc::new(AtomicBool::new(false));
        let name = entry.name.clone();
        let path = entry.path.clone();
        let handle = {
            let entry = entry.clone();
            let registry = registry.clone();
            let emit = Arc::clone(emit);
            let state = Arc::clone(&state);
            let removed = Arc::clone(&removed);
            let stop_all = Arc::clone(stop_all);
            let restart = Arc::clone(restart);
            let restart_path = Arc::clone(restart_path);
            let dirty = Arc::clone(dirty);
            std::thread::spawn(move || {
                repo_loop(
                    &entry,
                    &registry,
                    interval,
                    cap,
                    emit,
                    state,
                    removed,
                    stop_all,
                    restart,
                    restart_path,
                    dirty,
                );
            })
        };
        keep.push(name.clone());
        repos.insert(
            name,
            RepoHandle {
                name: entry.name,
                path,
                state,
                removed,
                handle: Some(handle),
            },
        );
        (dirty).store(true, Ordering::SeqCst);
    }

    // A repository the file no longer lists releases its lock after its pass in flight.
    let gone: Vec<String> = repos
        .keys()
        .filter(|name| !keep.contains(name))
        .cloned()
        .collect();
    for name in gone {
        if let Some(handle) = repos.remove(&name) {
            handle.removed.store(true, Ordering::SeqCst);
            if let Some(join) = handle.handle {
                retired.push(join);
            }
            (dirty).store(true, Ordering::SeqCst);
        }
    }
}

/// One repository's loop, on its own thread. It holds `runner.lock` for the loop's life and
/// runs the pass every interval; a lock another process holds makes it read-only until that
/// process lets go.
#[allow(clippy::too_many_arguments)]
fn repo_loop(
    entry: &RepoEntry,
    registry: &Registry,
    interval: u64,
    cap: u64,
    emit: Arc<dyn Fn(&str) + Send + Sync>,
    state: Arc<Mutex<RepoState>>,
    removed: Arc<AtomicBool>,
    stop_all: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    restart_path: Arc<Mutex<Option<Restart>>>,
    dirty: Arc<AtomicBool>,
) {
    // `Store::at` skips `Store::open`'s config check: a broken `config.toml` must surface as a
    // pass error in this repository's `loop.log`, not keep the loop from ever starting.
    let store = Store::at(&entry.path);
    let mut loop_state = LoopState::new(&store);
    let mut lock: Option<RunnerLock> = None;
    let machine = {
        let registry = registry.clone();
        move || machine_room(&registry, cap)
    };

    loop {
        if stop_all.load(Ordering::SeqCst) || removed.load(Ordering::SeqCst) {
            break;
        }
        if lock.is_none() {
            match store.try_runner_lock() {
                Ok(Some(taken)) => {
                    lock = Some(taken);
                    set_state(&state, RepoState::Scheduled);
                    dirty.store(true, Ordering::SeqCst);
                }
                Ok(None) => {
                    set_state(&state, RepoState::ReadOnly(store.runner_lock_pid()));
                    dirty.store(true, Ordering::SeqCst);
                }
                Err(error) => {
                    let mut log = run::LoopLog::new(&store);
                    log.write(&format!("radian arc: {}\n", error.message()));
                    (emit)(&format!("radian arc: {}\n", error.message()));
                    set_state(&state, RepoState::Stopped);
                    dirty.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }
        if lock.is_some() {
            let mut say = |text: &str| (emit)(text);
            match loop_state.pass(&store, &mut say, Some(&machine)) {
                Ok(Passed::Ran) => {
                    set_state(&state, RepoState::Scheduled);
                    dirty.store(true, Ordering::SeqCst);
                }
                Ok(Passed::Stopped) => break,
                Ok(Passed::Restart(path)) => {
                    // One replacement is one line for the whole hub: every loop may see the
                    // same change, but only the first one claims the restart and announces
                    // it, in its own `loop.log` and on stdout. A sibling that sees it too
                    // stops without printing a second line.
                    if !restart.swap(true, Ordering::SeqCst) {
                        *restart_path.lock().expect("the restart lock") = Some(Restart {
                            program: path,
                            repo: store.repo().to_path_buf(),
                        });
                        let line = format!("radian arc: {}\n", run::RESTART_TEXT);
                        run::LoopLog::new(&store).write(&line);
                        (emit)(&line);
                    }
                    break;
                }
                Err(_) => {
                    // `pass` already wrote the stop line to `loop.log`; the hub stops
                    // scheduling this repository and keeps every other loop running.
                    set_state(&state, RepoState::Stopped);
                    dirty.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }
        if !run::wait_interval(interval, &|| {
            stop_all.load(Ordering::SeqCst) || removed.load(Ordering::SeqCst)
        }) {
            break;
        }
    }

    if run::stop_requested() {
        run::stop_gates(&store);
    }
    drop(lock);
}

fn set_state(state: &Arc<Mutex<RepoState>>, next: RepoState) {
    if let Ok(mut state) = state.lock() {
        *state = next;
    }
}

/// The machine-wide count against `[hub] concurrency`: `true` when one more task may start.
fn machine_room(registry: &Registry, cap: u64) -> Result<bool, Error> {
    Ok(machine_doing(registry) < cap)
}

/// Tasks in `doing` across every registered repository, read now, never cached: two passes in
/// different repositories may each see room and both start one task, so the cap bounds the
/// steady state rather than acting as a lock.
///
/// A repository whose store cannot be read contributes nothing instead of failing the count.
/// That is the fault-isolation rule: a broken task file in one repository must not make
/// another repository's pass fail and, after [`run::REPEATED_ERROR_LIMIT`], stop its loop. The
/// repository that owns the file reports the error in its own pass; the cap can only be
/// undercounted while it is broken, which keeps a healthy repository scheduling.
fn machine_doing(registry: &Registry) -> u64 {
    let Ok(entries) = registry.repos() else {
        // The hub's own thread reports a registry it cannot read; a pass must not fail on it.
        return 0;
    };
    let mut total = 0u64;
    for entry in entries {
        let store = Store::at(&entry.path);
        if let Ok(counts) = crate::store::attempt_ops::doing_counts(&store) {
            total = total.saturating_add(counts.total);
        }
    }
    total
}

/// The status structure the HTTP task will serve.
fn compose_status(
    registry: &Registry,
    repos: &BTreeMap<String, RepoHandle>,
    cap: u64,
) -> HubStatus {
    HubStatus {
        cap,
        doing: machine_doing(registry),
        repos: repos
            .values()
            .map(|handle| RepoStatus {
                name: handle.name.clone(),
                path: handle.path.clone(),
                state: *handle.state.lock().expect("a repository's state"),
            })
            .collect(),
    }
}

fn load_config(explicit: Option<&std::path::Path>) -> Result<Config, Error> {
    let path = radian_host::config::resolved_path(explicit);
    radian_host::config::load_path(path.as_deref())
        .map_err(|error| Error::Failed(format!("cannot read the configuration: {error}")))
}

/// The interval `[hub] interval_secs` names. `RADIAN_HUB_INTERVAL_SECS` is a test-only seam: a
/// hub test needs an interval shorter than the documented five-second minimum so one
/// repository's slow pass can be shown not to gate another's cadence. radian never sets it and
/// the manual does not document it; `crates/radian-host/tests/env_docs.rs` lists it in
/// `TEST_ONLY`.
fn interval_secs(config: &Config) -> u64 {
    std::env::var("RADIAN_HUB_INTERVAL_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(config.hub.interval_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loop reads the same file the port came from: a top-level `--config` supplies
    /// `[hub] interval_secs` and `[hub] concurrency`, never the default file under
    /// `RADIAN_HOME`.
    #[test]
    fn the_loop_reads_the_explicit_config_path() {
        let dir = crate::test_support::TempDir::new("hub-loop-config");
        let path = dir.path().join("explicit.toml");
        std::fs::write(
            &path,
            "schema_version = 1\n[hub]\ninterval_secs = 5\nconcurrency = 7\n",
        )
        .expect("the test writes its configuration");
        let config = load_config(Some(&path)).expect("the explicit file reads");
        assert_eq!(interval_secs(&config), 5);
        assert_eq!(config.hub.concurrency, 7);
    }

    /// A failed `exec` is written to the claiming repository's `loop.log`, not only printed.
    #[test]
    fn a_failed_exec_is_written_to_the_claiming_repositorys_loop_log() {
        let dir = crate::test_support::TempDir::new("hub-exec-fail");
        let arc = dir.path().join(".radian").join("arc");
        std::fs::create_dir_all(&arc).expect("the arc directory is creatable");
        let said = std::sync::Mutex::new(Vec::new());
        let emit = |text: &str| said.lock().expect("the test lock").push(text.to_owned());
        let target = Restart {
            program: PathBuf::from("/nonexistent/radian"),
            repo: dir.path().to_path_buf(),
        };
        report_restart_failure(&emit, &target, &std::io::Error::other("exec failed"));
        let log = std::fs::read_to_string(arc.join(crate::run::LOOP_LOG))
            .expect("the failure reached loop.log");
        assert!(
            log.contains("cannot restart with the new binary: exec failed"),
            "{log}"
        );
        let printed = said.lock().expect("the test lock");
        assert_eq!(printed.len(), 1, "one line on stdout");
        assert!(printed[0].contains("cannot restart with the new binary"));
    }
}
