use super::*;

/// The built-in validator fires with no hook configured at all: one `System`
/// followup carrying the fixed text and the `intent-stop` origin, counted as one
/// wake. Keeping the old early return (no Stop hook means no run) leaves the inbox
/// empty and fails this.
#[tokio::test]
async fn intent_stop_fires_with_no_hook_configured() {
    let fixture = plain(&HooksConfig::default());
    fixture
        .hooks
        .run_stop_once(&completed_text("Now the docs-only ADR updates."))
        .await;
    let entry = fixture
        .inbox
        .claim_next_turn()
        .await
        .expect("the validator opens a turn");
    assert_eq!(entry.source, InputSource::System);
    assert_eq!(entry.message.text(), intent_stop::INTENT_STOP_TEXT);
    assert!(
        intent_stop::is_intent_stop_continuation(&entry.message),
        "validator-driven followups carry the origin"
    );
    assert_eq!(fixture.hooks.stop_wakes(), 1, "one wake");
    assert_eq!(fixture.hooks.consecutive_hits(), 1);
}

/// Ordinary ends hold the validator: a done report, a question, a long report whose
/// last sentence matches, and a hit text with the switch off. Removing the rule
/// (always firing) opens a turn for each and fails this.
#[tokio::test]
async fn ordinary_text_does_not_fire() {
    for text in [
        "Done.",
        "Is it done?",
        &format!("{}\n\nNext: the reviewer decides.", "x".repeat(300)),
    ] {
        let fixture = plain(&HooksConfig::default());
        fixture.hooks.run_stop_once(&completed_text(text)).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "no continuation for {text:?}"
        );
        assert_eq!(fixture.hooks.stop_wakes(), 0);
        assert_eq!(fixture.hooks.consecutive_hits(), 0);
    }
    // The kill switch: a hit text with `[hooks] intent_stop = false` holds too.
    let user = HooksConfig {
        intent_stop: Some(false),
        ..HooksConfig::default()
    };
    let fixture = layered(&user, &HooksConfig::default());
    fixture
        .hooks
        .run_stop_once(&completed_text("Now the docs-only ADR updates."))
        .await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "the switch turns the validator off"
    );
}

/// A waiting text holds while a background job runs (the model may be legitimately
/// waiting on it), and fires once the registry is gone.
#[tokio::test]
async fn waiting_with_a_running_job_does_not_fire() {
    let fixture = plain(&HooksConfig::default());
    let jobs = Arc::new(radian_coding::Jobs::new(fixture._root.dir.join("jobs")));
    let sandbox = radian_sandbox::Sandbox::new(
        radian_sandbox::SandboxMode::WorkspaceWrite,
        &fixture._root.ws,
    )
    .expect("the workspace sandbox probes clean on this machine");
    let _started = jobs
        .start(&sandbox, &fixture._root.ws, "sleep 60")
        .expect("the job starts");
    fixture.hooks.set_jobs(Some(&jobs));
    assert_eq!(fixture.hooks.jobs_running(), 1);
    fixture
        .hooks
        .run_stop_once(&completed_text("Waiting for the completion notice."))
        .await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "a running job suppresses the validator"
    );
    assert_eq!(fixture.hooks.stop_wakes(), 0);
    assert_eq!(fixture.hooks.consecutive_hits(), 0);
    jobs.kill_all();
    drop(jobs);
    fixture
        .hooks
        .run_stop_once(&completed_text("Waiting for the completion notice."))
        .await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_some(),
        "with no job the same text fires"
    );
}

/// A hook exit-2 and a validator hit join one continuation: one `followup`, one
/// wake, the hook reasons first and the fixed text last, marked with the origin.
/// Splitting the validator into its own followup opens two turns and fails this.
#[tokio::test]
async fn hook_and_validator_verdicts_join_one_continuation() {
    let config = stop_config("echo hook-reason >&2; exit 2");
    let fixture = plain(&config);
    fixture
        .hooks
        .run_stop_once(&completed_text("Now the docs-only ADR updates."))
        .await;
    let entry = fixture
        .inbox
        .claim_next_turn()
        .await
        .expect("the one continuation opens a turn");
    assert_eq!(entry.source, InputSource::System);
    let text = entry.message.text();
    assert!(
        text.contains("hook-reason"),
        "the hook reason rides: {text}"
    );
    assert!(
        text.contains(intent_stop::INTENT_STOP_TEXT),
        "the validator text joins it: {text}"
    );
    assert!(
        intent_stop::is_intent_stop_continuation(&entry.message),
        "the joint message carries the origin"
    );
    assert_eq!(fixture.hooks.stop_wakes(), 1, "one wake, not two");
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "at most one continuation per turn"
    );
    assert_eq!(fixture.hooks.consecutive_hits(), 1);
}

/// Two continuations in a row, then the third consecutive hit reports `blocked`
/// instead of continuing: no turn opens, one cap line fires, the verdict injects
/// degraded (journal-visible, no wake), and the count stays saturated. A user
/// message (`reset_wakes`) clears the chain. Dropping the cap opens the third turn
/// and fails this.
#[tokio::test]
async fn third_consecutive_hit_reports_blocked_instead_of_continuing() {
    let fixture = plain(&HooksConfig::default());
    let hit = completed_text("Now the docs-only ADR updates.");
    for round in 0..2 {
        fixture.hooks.run_stop_once(&hit).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_some(),
            "round {round}: the continuation opens a turn"
        );
    }
    assert_eq!(fixture.hooks.consecutive_hits(), 2);
    fixture.hooks.run_stop_once(&hit).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "the third hit opens no turn"
    );
    assert_eq!(
        warned(&fixture.intent_notices),
        vec![intent_stop::intent_stop_cap_line()],
        "exactly one cap notice"
    );
    let step = fixture
        .inbox
        .claim_next_step()
        .await
        .expect("the cap verdict injects degraded");
    assert!(
        intent_stop::is_intent_stop_continuation(&step.message),
        "the cap inject is journal-visible"
    );
    assert_eq!(fixture.hooks.degraded(), 1);
    assert_eq!(
        fixture.hooks.consecutive_hits(),
        2,
        "saturated, not cleared"
    );
    fixture.hooks.reset_wakes();
    fixture.hooks.run_stop_once(&hit).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_some(),
        "a user message clears the chain"
    );
}

/// A rule miss between two hits breaks the chain: the hit after the miss counts
/// from zero, so the run below caps only at its own third consecutive hit.
#[tokio::test]
async fn a_rule_miss_breaks_the_chain() {
    let fixture = plain(&HooksConfig::default());
    let hit = completed_text("Now the docs-only ADR updates.");
    fixture.hooks.run_stop_once(&hit).await;
    assert!(fixture.inbox.claim_next_turn().await.is_some());
    fixture.hooks.run_stop_once(&completed_text("Done.")).await;
    assert!(fixture.inbox.claim_next_turn().await.is_none());
    assert_eq!(fixture.hooks.consecutive_hits(), 0);
    fixture.hooks.run_stop_once(&hit).await;
    assert!(fixture.inbox.claim_next_turn().await.is_some());
    fixture.hooks.run_stop_once(&hit).await;
    assert!(fixture.inbox.claim_next_turn().await.is_some());
    fixture.hooks.run_stop_once(&hit).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "the third consecutive hit after the miss caps"
    );
    assert_eq!(warned(&fixture.intent_notices).len(), 1);
}

/// While `/loop` is armed the turn end belongs to the loop: a hit text opens
/// nothing and counts nothing; disarmed, the same hit fires. Removing the loop
/// check fires the armed case and fails this.
#[tokio::test]
async fn intent_stop_stays_off_while_loop_is_armed() {
    let fixture = plain(&HooksConfig::default());
    let hit = completed_text("Now the docs-only ADR updates.");
    fixture.hooks.set_loop_active(true);
    fixture.hooks.run_stop_once(&hit).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "an armed loop owns the turn end"
    );
    assert_eq!(fixture.hooks.consecutive_hits(), 0);
    fixture.hooks.set_loop_active(false);
    fixture.hooks.run_stop_once(&hit).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_some(),
        "disarmed, the same hit fires"
    );
}

/// An `Active` or `Paused` goal owns the turn end (it re-posts itself): a hit text
/// opens nothing; a `Done` goal leaves the validator alone. Removing the goal
/// check fires the active case and fails this.
#[tokio::test]
async fn intent_stop_stays_off_while_a_goal_owns_the_turn_end() {
    use radian_session::{GoalState, GoalStatus};

    let fixture = plain(&HooksConfig::default());
    let hit = completed_text("Now the docs-only ADR updates.");
    let share: crate::goal::SharedGoal = Arc::new(std::sync::Mutex::new(Some(GoalState::fresh(
        "o".into(),
        GoalState::default_budget(),
    ))));
    fixture.hooks.set_goal(Arc::clone(&share));
    fixture.hooks.run_stop_once(&hit).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "an active goal owns the turn end"
    );
    assert_eq!(fixture.hooks.consecutive_hits(), 0);
    share.lock().unwrap().as_mut().expect("a goal").status = GoalStatus::Paused;
    fixture.hooks.run_stop_once(&hit).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "a paused goal still owns the turn end"
    );
    share.lock().unwrap().as_mut().expect("a goal").status = GoalStatus::Done;
    fixture.hooks.run_stop_once(&hit).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_some(),
        "a done goal leaves the validator alone"
    );
}

/// A non-completed turn breaks the validator's chain through `on_turn_end` (like a
/// rule miss), and no Stop run comes for it: the pending count drops synchronously.
#[tokio::test]
async fn a_cancelled_turn_breaks_the_chain_with_no_stop_run() {
    let fixture = plain(&HooksConfig::default());
    let hit = completed_text("Now the docs-only ADR updates.");
    fixture.hooks.note_turn_started();
    fixture.hooks.run_stop_once(&hit).await;
    assert!(fixture.inbox.claim_next_turn().await.is_some());
    assert_eq!(fixture.hooks.consecutive_hits(), 1);
    fixture.hooks.on_turn_end(
        &cx(),
        &completed_outcome(
            RunStatus::Cancelled(CancelReason::Requested),
            Some("Now x."),
        ),
    );
    assert_eq!(fixture.hooks.consecutive_hits(), 0);
    assert_eq!(fixture.hooks.pending_stops(), 0);
}

/// The validator's continuation counts against the wake gate: with the window full
/// of hook-driven wakes, a hit degrades to a marked inject with the wake-budget
/// notice instead of opening a turn. Not counting the gate opens the turn and fails
/// this.
#[tokio::test]
async fn the_wake_gate_counts_validator_continuations() {
    tokio::time::pause();
    let config = stop_config("echo keep-going >&2; exit 2");
    let fixture = plain(&config);
    for _ in 0..MAX_WAKES_PER_WINDOW {
        fixture.hooks.run_stop_once(&completed_quiet()).await;
        assert!(fixture.inbox.claim_next_turn().await.is_some());
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
    }
    assert_eq!(fixture.hooks.stop_wakes(), MAX_WAKES_PER_WINDOW);
    fixture
        .hooks
        .run_stop_once(&completed_text("Now the docs-only ADR updates."))
        .await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "past the gate the validator degrades like a hook"
    );
    let step = fixture
        .inbox
        .claim_next_step()
        .await
        .expect("the gated verdict injects");
    assert!(step.message.text().contains(intent_stop::INTENT_STOP_TEXT));
    assert!(
        intent_stop::is_intent_stop_continuation(&step.message),
        "the degraded message keeps the origin"
    );
    assert_eq!(fixture.hooks.degraded(), 1);
    assert_eq!(
        warned(&fixture.wake_notices).len(),
        1,
        "the gate fires its own notice, not the cap line"
    );
    assert!(warned(&fixture.intent_notices).is_empty());
    assert_eq!(fixture.hooks.consecutive_hits(), 1, "the hit still counts");
}

/// `Stop` stdin carries `last_assistant_message` (Claude's field name): the joined
/// text parts, or empty when the turn has none.
#[tokio::test]
async fn stop_stdin_carries_last_assistant_message() {
    let root = TempRoot::new("stop-stdin");
    let stdin_path = root.dir.join("stdin.json");
    let config = stop_config(&format!("cat > {}", stdin_path.display()));
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    fixture
        .hooks
        .run_stop_once(&completed_text("Now the docs-only ADR updates."))
        .await;
    let stdin: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&stdin_path).unwrap()).unwrap();
    assert_eq!(stdin["hook_event_name"], "Stop");
    assert_eq!(
        stdin["last_assistant_message"],
        "Now the docs-only ADR updates."
    );
    fixture.hooks.run_stop_once(&completed_quiet()).await;
    let stdin: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&stdin_path).unwrap()).unwrap();
    assert_eq!(stdin["last_assistant_message"], "");
}

/// Runs one `Stop` cycle whose hook writes its stdin to `path`, and returns that JSON.
/// Removes an earlier copy first, so a hook that never ran cannot pass a stale read
/// off as this run's.
pub(super) async fn stop_stdin_at(
    fixture: &Fixture,
    path: &Path,
    outcome: &RunOutcome,
) -> serde_json::Value {
    let _ = std::fs::remove_file(path);
    fixture.hooks.run_stop_once(outcome).await;
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Releases a blocked `Stop` hook even when an assertion fails first, so a broken
/// test fails instead of waiting for the running child. `disarm` is called once the
/// hook is known to have exited; until then, a failure writes the release file and
/// waits long enough for the blocked child to observe it before the fixture's temp
/// directory (and the file) goes away.
struct Release {
    path: PathBuf,
    armed: bool,
}

impl Release {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Release {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::write(&self.path, b"go");
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }
}

/// Waits for a file a hook creates (polled: the hook is a child process).
pub(super) async fn wait_for_file(path: &Path) {
    for _ in 0..600 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{} never appeared", path.display());
}

pub(super) async fn wait_for_json(path: &Path) -> serde_json::Value {
    // The file is created before its bytes land, so existence alone is not enough.
    for _ in 0..600 {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(value) = serde_json::from_str(&text)
        {
            return value;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{} never held valid JSON", path.display());
}

/// Waits until a `Stop` continuation was actually queued (the gate records the wake
/// after the enqueue lands).
pub(super) async fn wait_until_continuation(fixture: &Fixture) {
    for _ in 0..600 {
        if fixture.hooks.stop_wakes() > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the continuation was never queued");
}

/// A `Stop` run for an earlier turn finishes late (exit 2) while a user's turn B is
/// open, and B ends after that verdict. B's own `Stop` must report `false`; only the
/// continuation the late verdict queued may report `true` (ADR 0045 §2). A
/// session-global pending flag is consumed by whichever turn ends first, so B reports
/// `true` and the real continuation reports `false` there.
#[tokio::test]
async fn a_late_stop_verdict_does_not_leak_into_a_users_turn() {
    let root = TempRoot::new("stop-late-verdict");
    let a_stdin = root.dir.join("a.json");
    let b_stdin = root.dir.join("b.json");
    let c_stdin = root.dir.join("c.json");
    let started = root.dir.join("a-started");
    let release = root.dir.join("release");
    let script = format!(
        "stdin=$(cat); case \"$stdin\" in \
         *'\"last_assistant_message\":\"A\"'*) printf '%s' \"$stdin\" > '{a}'; \
         touch '{started}'; while [ ! -f '{release}' ]; do sleep 0.01; done; \
         echo keep-going >&2; exit 2 ;; \
         *'\"last_assistant_message\":\"B\"'*) printf '%s' \"$stdin\" > '{b}' ;; \
         *) printf '%s' \"$stdin\" > '{c}' ;; esac",
        a = a_stdin.display(),
        b = b_stdin.display(),
        c = c_stdin.display(),
        started = started.display(),
        release = release.display(),
    );
    let config = stop_config(&script);
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    let mut release_guard = Release::new(release.clone());

    // Turn A ends; its Stop hook blocks before exiting 2.
    fixture.hooks.on_turn_end(&cx(), &completed_text("A"));
    wait_for_file(&started).await;

    // The user speaks while A's hook still runs: turn B opens and is not a
    // continuation.
    fixture.hooks.reset_wakes();
    fixture
        .inbox
        .followup("B", InputSource::User)
        .await
        .unwrap();
    let b = fixture.inbox.claim_next_turn().await.expect("B opens");
    assert_eq!(b.message.text(), "B");
    assert!(
        !fixture.hooks.stop_hook_active(),
        "a user message opens a turn that is not a continuation"
    );

    // A's hook now exits 2 and queues its continuation; B ends after that verdict.
    std::fs::write(&release, b"go").unwrap();
    wait_until_continuation(&fixture).await;
    release_guard.disarm();
    fixture.hooks.on_turn_end(&cx(), &completed_text("B"));
    let b_stdin = wait_for_json(&b_stdin).await;
    assert_eq!(
        b_stdin["stop_hook_active"],
        serde_json::json!(false),
        "the user's turn must not consume the other turn's continuation: {b_stdin}"
    );

    // Only the continuation the late verdict queued is one.
    fixture
        .inbox
        .claim_next_turn()
        .await
        .expect("the late verdict queued a continuation");
    assert!(
        fixture.hooks.stop_hook_active(),
        "claiming the continuation marks its turn"
    );
    fixture.hooks.on_turn_end(&cx(), &completed_text("C"));
    let c_stdin = wait_for_json(&c_stdin).await;
    assert_eq!(
        c_stdin["stop_hook_active"],
        serde_json::json!(true),
        "the continuation itself says so: {c_stdin}"
    );
}

/// Edge case 1: with no job, no `/loop` and no goal the three fields are present — not
/// omitted — as `stop_hook_active: false`, `background_tasks: []`, `session_crons: []`.
/// Inserting an array only when it is non-empty fails this.
#[tokio::test]
async fn stop_stdin_always_carries_the_three_new_fields() {
    let root = TempRoot::new("stop-extra-empty");
    let stdin_path = root.dir.join("stdin.json");
    let config = stop_config(&format!("cat > {}", stdin_path.display()));
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    let stdin = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
    assert_eq!(
        stdin["stop_hook_active"],
        serde_json::json!(false),
        "{stdin}"
    );
    assert_eq!(stdin["background_tasks"], serde_json::json!([]), "{stdin}");
    assert_eq!(stdin["session_crons"], serde_json::json!([]), "{stdin}");
}

/// Edge case 2: a background job running at turn end is one `background_tasks` entry
/// with its id, `type: "shell"`, `status: "running"`, the first line as `description`
/// and the full multi-line command as `command`. Reporting the first-line summary as
/// the command fails this.
#[tokio::test]
async fn stop_stdin_lists_a_running_background_job() {
    let root = TempRoot::new("stop-extra-job");
    let stdin_path = root.dir.join("stdin.json");
    let config = stop_config(&format!("cat > {}", stdin_path.display()));
    let ws = root.ws.clone();
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    let jobs = Arc::new(radian_coding::Jobs::new(fixture._root.dir.join("jobs")));
    let sandbox = radian_sandbox::Sandbox::new(radian_sandbox::SandboxMode::FullAccess, &ws)
        .expect("full access needs no backend");
    let started = jobs
        .start(&sandbox, &ws, "sleep 60\necho done")
        .expect("the job starts");
    fixture.hooks.set_jobs(Some(&jobs));
    let stdin = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
    let tasks = stdin["background_tasks"]
        .as_array()
        .unwrap_or_else(|| panic!("an array: {stdin}"));
    assert_eq!(tasks.len(), 1, "{stdin}");
    assert_eq!(tasks[0]["id"], started.id.as_str(), "{stdin}");
    assert_eq!(tasks[0]["type"], "shell", "{stdin}");
    assert_eq!(tasks[0]["status"], "running", "{stdin}");
    assert_eq!(tasks[0]["description"], "sleep 60", "{stdin}");
    assert_eq!(tasks[0]["command"], "sleep 60\necho done", "{stdin}");
    jobs.kill_all();
}

/// Edge case 3: the turn a `Stop` exit-2 continuation opens reports
/// `stop_hook_active: true`; the turn after it, which opens nothing, reports `false`.
/// The provenance travels with the queued message: the claim is what makes the turn a
/// continuation. Never setting it fails the middle assertion; never clearing it fails
/// the last.
#[tokio::test]
async fn the_continued_turn_reports_stop_hook_active() {
    let root = TempRoot::new("stop-active");
    let stdin_path = root.dir.join("stdin.json");
    let marker = root.dir.join("continued");
    let config = stop_config(&format!(
        "cat > {stdin}; if [ -f {marker} ]; then exit 0; else touch {marker}; \
         echo keep-going >&2; exit 2; fi",
        stdin = stdin_path.display(),
        marker = marker.display(),
    ));
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());

    let first = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
    assert_eq!(
        first["stop_hook_active"],
        serde_json::json!(false),
        "{first}"
    );
    assert!(
        !fixture.hooks.stop_hook_active(),
        "nothing is claimed yet, so no running turn is a continuation"
    );
    fixture
        .inbox
        .claim_next_turn()
        .await
        .expect("the exit 2 opens the continued turn");
    assert!(
        fixture.hooks.stop_hook_active(),
        "claiming the continuation message marks the turn it opens"
    );

    let second = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
    assert_eq!(
        second["stop_hook_active"],
        serde_json::json!(true),
        "the continued turn says so: {second}"
    );
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "the exit 0 opens nothing"
    );

    let after = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
    assert_eq!(
        after["stop_hook_active"],
        serde_json::json!(false),
        "the turn after the continuation is not one: {after}"
    );
}

/// Edge case 4: a user message while a continuation is queued drops its provenance,
/// so the turn that later claims it reports `false`. Dropping the pending-set clear
/// from `reset_wakes` (the followup/steer/inject reset) fails this.
#[tokio::test]
async fn a_user_message_clears_stop_hook_active() {
    let root = TempRoot::new("stop-active-user");
    let stdin_path = root.dir.join("stdin.json");
    let config = stop_config(&format!(
        "cat > {}; echo keep-going >&2; exit 2",
        stdin_path.display()
    ));
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
    // The same reset the three user-message lanes (followup, steer, inject) call,
    // before the queued continuation is claimed.
    fixture.hooks.reset_wakes();
    fixture
        .inbox
        .claim_next_turn()
        .await
        .expect("the exit 2 queued a continuation");
    assert!(
        !fixture.hooks.stop_hook_active(),
        "a user message ends the queued continuation"
    );
    let next = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
    assert_eq!(
        next["stop_hook_active"],
        serde_json::json!(false),
        "the continuation turn is no longer one: {next}"
    );
}

/// Edge case 5: a non-completed (cancelled) turn after a continuation clears the
/// provenance; the next completed turn's `Stop` reports `false`. Keeping the claimed
/// continuation's flag through a non-completed turn fails this.
#[tokio::test]
async fn a_cancelled_turn_clears_stop_hook_active() {
    let root = TempRoot::new("stop-active-cancel");
    let stdin_path = root.dir.join("stdin.json");
    let config = stop_config(&format!(
        "cat > {}; echo keep-going >&2; exit 2",
        stdin_path.display()
    ));
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
    fixture
        .inbox
        .claim_next_turn()
        .await
        .expect("the exit 2 queued a continuation");
    assert!(
        fixture.hooks.stop_hook_active(),
        "the claimed continuation marked this turn"
    );
    fixture.hooks.on_turn_end(
        &cx(),
        &completed_outcome(
            RunStatus::Cancelled(CancelReason::Requested),
            Some("Now x."),
        ),
    );
    assert!(
        !fixture.hooks.stop_hook_active(),
        "a non-completed turn ends the continuation"
    );
    let next = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
    assert_eq!(next["stop_hook_active"], serde_json::json!(false), "{next}");
}

/// Edge case 6: `session_crons` lists an armed `/loop` and an `Active` or `Paused` goal
/// with its status, and nothing for a `Done` or cleared goal. Reporting a paused goal as
/// active (or keeping a done one) fails this.
#[tokio::test]
async fn session_crons_lists_the_loop_and_the_goal() {
    use radian_session::{GoalState, GoalStatus};

    let root = TempRoot::new("stop-crons");
    let stdin_path = root.dir.join("stdin.json");
    let config = stop_config(&format!("cat > {}", stdin_path.display()));
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    let share: crate::goal::SharedGoal = Arc::new(std::sync::Mutex::new(Some(GoalState::fresh(
        "ship it".into(),
        GoalState::default_budget(),
    ))));
    fixture.hooks.set_goal(Arc::clone(&share));
    fixture.hooks.set_loop_active(true);

    let both = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
    assert_eq!(
        both["session_crons"],
        serde_json::json!([
            {"id": "loop", "type": "loop"},
            {"id": "goal", "type": "goal", "status": "active"},
        ]),
        "the loop and the active goal: {both}"
    );

    share.lock().unwrap().as_mut().expect("a goal").status = GoalStatus::Paused;
    fixture.hooks.set_loop_active(false);
    let paused = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
    assert_eq!(
        paused["session_crons"],
        serde_json::json!([{"id": "goal", "type": "goal", "status": "paused"}]),
        "a paused goal is still a scheduler: {paused}"
    );

    share.lock().unwrap().as_mut().expect("a goal").status = GoalStatus::Done;
    let done = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
    assert_eq!(
        done["session_crons"],
        serde_json::json!([]),
        "a done goal adds nothing: {done}"
    );

    *share.lock().unwrap() = None;
    let cleared = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
    assert_eq!(
        cleared["session_crons"],
        serde_json::json!([]),
        "a cleared goal adds nothing: {cleared}"
    );
}

/// Edge case 7: a job registry that was dropped (the `Weak` no longer upgrades) reads
/// as `background_tasks: []` and never panics. An `unwrap` on the upgrade panics here.
#[tokio::test]
async fn a_dropped_job_registry_reads_as_no_background_tasks() {
    let root = TempRoot::new("stop-extra-dropped");
    let stdin_path = root.dir.join("stdin.json");
    let config = stop_config(&format!("cat > {}", stdin_path.display()));
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    let jobs = Arc::new(radian_coding::Jobs::new(fixture._root.dir.join("jobs")));
    fixture.hooks.set_jobs(Some(&jobs));
    drop(jobs);
    let stdin = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
    assert_eq!(
        stdin["background_tasks"],
        serde_json::json!([]),
        "a dropped registry reads as no jobs: {stdin}"
    );
}

/// Edge case 1: eight hook continuations in a row with text-only turns. The 7th still
/// opens a turn; the 8th does not — it injects marked degraded, fires one notice line,
/// and counts one degraded item. Dropping the cap opens the 8th and fails the inbox
/// assertions; dropping the notice fails the notice assertion.
#[tokio::test]
async fn eight_idle_hook_continuations_degrade_the_eighth_continuation() {
    let fixture = plain(&stop_config("echo keep-going >&2; exit 2"));
    for round in 0..7 {
        assert!(
            hook_round(&fixture, &completed_calls(None, 1)).await,
            "continuation {round} still opens a turn"
        );
    }
    assert_eq!(
        fixture.hooks.hook_idle_continuations(),
        6,
        "the first turn end is not a continuation; six idle continued turns preceded"
    );
    // The eighth continuation request (the seventh continued turn's Stop) takes the cap.
    fixture.hooks.run_stop_once(&completed_calls(None, 1)).await;
    assert_eq!(fixture.hooks.hook_idle_continuations(), 7);
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "the eighth continuation opens no turn"
    );
    let step = fixture
        .inbox
        .claim_next_step()
        .await
        .expect("the capped continuation injects the verdict");
    assert!(
        step.message.text().contains("keep-going"),
        "the reasons ride the inject: {}",
        step.message.text()
    );
    assert_eq!(fixture.hooks.degraded(), 1, "the inject is marked degraded");
    assert_eq!(
        warned(&fixture.hook_stop_notices),
        vec![hook_stop_cap_line()],
        "exactly one notice line"
    );
    assert!(
        warned(&fixture.wake_notices).is_empty(),
        "the idle cap is not the wake budget: {:?}",
        warned(&fixture.wake_notices)
    );
}

/// Edge case 2: seven idle hook continuations, then a continued turn that calls a tool,
/// then seven more idle continuations — no degrade, because the tool call reset the
/// counter. Keeping the count across the tool turn caps the later streak and fails the
/// empty-notice assertion.
#[tokio::test]
async fn a_tool_calling_continued_turn_resets_the_idle_streak() {
    tokio::time::pause();
    let fixture = plain(&stop_config("echo keep-going >&2; exit 2"));
    for round in 0..7 {
        assert!(
            hook_round(&fixture, &completed_calls(None, 1)).await,
            "continuation {round} opens a turn"
        );
    }
    assert_eq!(fixture.hooks.hook_idle_continuations(), 6);
    // The next continued turn did real work: its Stop sees model_calls > 1, resets the
    // streak, and opens the next continuation.
    fixture
        .hooks
        .run_stop_once_with(&completed_calls(None, 2), Continuation::Hook)
        .await;
    assert_eq!(
        fixture.hooks.hook_idle_continuations(),
        0,
        "a tool call resets the streak"
    );
    assert!(
        fixture.inbox.claim_next_turn().await.is_some(),
        "the tool turn's Stop still opens its continuation"
    );
    // The wake window is a hard cap independent of the idle streak; this test is about
    // the reset, so let the first streak's wakes age out before the second.
    tokio::time::advance(WAKE_WINDOW + std::time::Duration::from_secs(1)).await;
    // Seven more continuation requests open; none takes the cap.
    for round in 0..6 {
        assert!(
            hook_round(&fixture, &completed_calls(None, 1)).await,
            "post-reset continuation {round} opens a turn"
        );
    }
    assert_eq!(fixture.hooks.hook_idle_continuations(), 6);
    assert!(
        warned(&fixture.hook_stop_notices).is_empty(),
        "the reset streak does not cap: {:?}",
        warned(&fixture.hook_stop_notices)
    );
}

/// Edge case 3: a cancelled turn in the middle of a streak resets the count, at the same
/// place the validator's count resets. Keeping the count through the cancel caps the
/// rebuilt streak early.
#[tokio::test]
async fn a_cancelled_turn_resets_the_idle_streak() {
    let fixture = plain(&stop_config("echo keep-going >&2; exit 2"));
    assert!(hook_round(&fixture, &completed_calls(None, 1)).await);
    assert!(hook_round(&fixture, &completed_calls(None, 1)).await);
    assert_eq!(fixture.hooks.hook_idle_continuations(), 1);
    fixture.hooks.on_turn_end(
        &cx(),
        &completed_outcome(RunStatus::Cancelled(CancelReason::Requested), None),
    );
    assert_eq!(
        fixture.hooks.hook_idle_continuations(),
        0,
        "a non-completed turn resets the streak"
    );
    assert!(
        hook_round(&fixture, &completed_calls(None, 1)).await,
        "the next completed continuation opens a turn"
    );
    assert_eq!(
        fixture.hooks.hook_idle_continuations(),
        0,
        "the first plain turn end starts the rebuilt streak from zero"
    );
    assert!(hook_round(&fixture, &completed_calls(None, 1)).await);
    assert_eq!(fixture.hooks.hook_idle_continuations(), 1);
}

/// Edge case 4: a user message while the cap is degraded clears it, and the next hook
/// exit 2 opens a turn again. Keeping the streak after `reset_wakes` injects instead
/// and fails the second turn assertion.
#[tokio::test]
async fn a_user_message_clears_the_idle_cap() {
    let fixture = plain(&stop_config("echo keep-going >&2; exit 2"));
    for round in 0..7 {
        assert!(
            hook_round(&fixture, &completed_calls(None, 1)).await,
            "{round}"
        );
    }
    fixture.hooks.run_stop_once(&completed_calls(None, 1)).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "the cap is degraded"
    );
    assert_eq!(warned(&fixture.hook_stop_notices).len(), 1);
    // The same reset the user-message lanes call.
    fixture.hooks.reset_wakes();
    assert_eq!(fixture.hooks.hook_idle_continuations(), 0);
    assert_eq!(fixture.hooks.degraded(), 0);
    assert!(
        hook_round(&fixture, &completed_calls(None, 1)).await,
        "after a user message the next hook exit 2 opens a turn"
    );
    assert_eq!(
        warned(&fixture.hook_stop_notices).len(),
        1,
        "the user message fires no new notice"
    );
}

/// Edge case 5: a merged validator and hook continuation counts once in each counter:
/// the continued turn's end advances the hook idle counter once and the validator's
/// streak once, and still opens a single turn. Incrementing either twice fails.
#[tokio::test]
async fn a_merged_continuation_counts_once_in_each_counter() {
    let fixture = plain(&stop_config("echo hook-reason >&2; exit 2"));
    let hit = "Now the docs-only ADR updates.";
    fixture.hooks.run_stop_once(&completed_text(hit)).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_some(),
        "the merged continuation opens one turn"
    );
    assert_eq!(fixture.hooks.hook_idle_continuations(), 0);
    assert_eq!(fixture.hooks.consecutive_hits(), 1);
    fixture
        .hooks
        .run_stop_once(&completed_calls(Some(hit), 1))
        .await;
    assert_eq!(
        fixture.hooks.hook_idle_continuations(),
        1,
        "the hook counter advances once"
    );
    assert_eq!(
        fixture.hooks.consecutive_hits(),
        2,
        "the validator counter advances once"
    );
    assert!(
        fixture.inbox.claim_next_turn().await.is_some(),
        "one merged continuation, one turn"
    );
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "not two turns for the two counters"
    );
}

/// Edge case 6: the 10-per-10-minute window still caps first when every continued turn
/// calls a tool (each resets the idle counter). The 11th wake degrades through the wake
/// budget, not the idle cap. Counting tool turns as idle would fire the hook-stop
/// notice instead.
#[tokio::test]
async fn the_wake_window_cap_triggers_first_when_continued_turns_call_tools() {
    tokio::time::pause();
    let fixture = plain(&stop_config("echo keep-going >&2; exit 2"));
    for round in 0..MAX_WAKES_PER_WINDOW {
        // The first turn end is plain; every later one is a continued turn that called
        // a tool (model_calls > 1), which resets the idle streak each time.
        let outcome = completed_calls(None, if round == 0 { 0 } else { 2 });
        assert!(
            hook_round(&fixture, &outcome).await,
            "wake {round} still opens a turn"
        );
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
    }
    assert_eq!(fixture.hooks.stop_wakes(), MAX_WAKES_PER_WINDOW);
    fixture.hooks.run_stop_once(&completed_calls(None, 2)).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "the window gates the 11th continuation"
    );
    let step = fixture
        .inbox
        .claim_next_step()
        .await
        .expect("the gated continuation injects");
    assert!(step.message.text().contains("keep-going"));
    assert_eq!(fixture.hooks.degraded(), 1);
    assert_eq!(
        warned(&fixture.wake_notices).len(),
        1,
        "the wake notice fires, not the idle cap: {:?}",
        warned(&fixture.hook_stop_notices)
    );
    assert!(
        warned(&fixture.hook_stop_notices).is_empty(),
        "tool turns never fill the idle cap"
    );
    assert_eq!(fixture.hooks.hook_idle_continuations(), 0);
}
