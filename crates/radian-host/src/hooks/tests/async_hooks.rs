use super::*;

// ---------- async hooks (ADR 0045 §5) ----------

pub(super) fn async_hook(command: &str) -> HookCommand {
    HookCommand {
        command: command.to_owned(),
        args: None,
        timeout_secs: None,
        async_hook: true,
        async_rewake: false,
    }
}

pub(super) fn async_rewake_hook(command: &str) -> HookCommand {
    HookCommand {
        command: command.to_owned(),
        args: None,
        timeout_secs: None,
        async_hook: true,
        async_rewake: true,
    }
}

pub(super) fn post_hooks(hooks: Vec<HookCommand>) -> HooksConfig {
    HooksConfig {
        post_tool_use: vec![entry("patch", hooks)],
        ..HooksConfig::default()
    }
}

/// Waits until every spawned async hook has finished delivering its result.
pub(super) async fn drain_async(fixture: &Fixture) {
    let signal = fixture.hooks.async_signal();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while fixture.hooks.async_hooks_running() > 0 {
        let notified = signal.notified();
        if fixture.hooks.async_hooks_running() == 0 {
            return;
        }
        tokio::time::timeout_at(deadline, notified)
            .await
            .expect("the async hook never finished");
    }
}

pub(super) async fn after_patch(fixture: &Fixture, path: &str) -> ToolResult {
    let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
    fixture
        .hooks
        .after_tool_call(
            &cx(),
            &call("patch", serde_json::json!({"path": path})),
            &mut result,
        )
        .await
        .unwrap();
    result
}

/// Edge case 1: an async `PostToolUse` hook that sleeps 2 s does not hold the tool
/// result, and its `additionalContext` waits in `next_step` for the next model call
/// (`inject`, never a wake). Running it synchronously fails the timing assertion.
#[tokio::test]
async fn an_async_post_hook_returns_at_once_and_injects_before_the_next_model_call() {
    let script = r#"sleep 2; printf '%s' '{"hookSpecificOutput":{"additionalContext":"linted"}}'"#;
    let fixture = plain(&post_hooks(vec![async_hook(script)]));
    let started = std::time::Instant::now();
    let result = after_patch(&fixture, "src/a.rs").await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "the tool result must not wait for the hook: {:?}",
        started.elapsed()
    );
    assert_eq!(result.text(), "patched", "the result is untouched");
    assert_eq!(
        fixture.hooks.running_async_hooks().len(),
        1,
        "the hook is listed as running"
    );
    drain_async(&fixture).await;
    assert_eq!(fixture.hooks.running_async_hooks().len(), 0);
    let injected = fixture
        .inbox
        .claim_next_step()
        .await
        .expect("additionalContext injects");
    assert_eq!(injected.message.text(), "[PostToolUse hook] linted");
    assert_eq!(injected.source, InputSource::System);
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "inject must never wake a turn"
    );
    assert_eq!(fixture.hooks.stop_wakes(), 0);
}

/// Edge case 2: three edits to three files in one turn, each matched by its own async
/// command, produce three separate injected messages (one per result, never merged).
/// Each edit's run is drained before the next edit: the same three commands fire on every
/// edit, and the supersede rule (ADR 0045 §5) would kill a still-running predecessor's
/// result — a different rule from "one message per result", which this test is about.
#[tokio::test]
async fn three_async_edits_in_one_turn_inject_three_separate_messages() {
    let hook_for = |file: &str, text: &str| {
        async_hook(&format!(
            "input=$(cat); case \"$input\" in *{file}*) printf '%s' \
             '{{\"hookSpecificOutput\":{{\"additionalContext\":\"{text}\"}}}}';; esac"
        ))
    };
    let fixture = plain(&post_hooks(vec![
        hook_for("a.rs", "a-done"),
        hook_for("b.rs", "b-done"),
        hook_for("c.rs", "c-done"),
    ]));
    for path in ["a.rs", "b.rs", "c.rs"] {
        after_patch(&fixture, path).await;
        drain_async(&fixture).await;
    }
    drain_async(&fixture).await;
    let mut texts = Vec::new();
    while let Some(entry) = fixture.inbox.claim_next_step().await {
        texts.push(entry.message.text());
    }
    texts.sort();
    assert_eq!(
        texts,
        vec![
            "[PostToolUse hook] a-done",
            "[PostToolUse hook] b-done",
            "[PostToolUse hook] c-done"
        ],
        "one message per async command"
    );
}

/// Edge case 3: `asyncRewake` exit 2 wakes an idle session through the `followup`
/// lane and counts one wake; past the gate it degrades to `inject_degraded` with the
/// degraded counter and the wake-budget notice (the three visible signals).
#[tokio::test]
async fn async_rewake_wakes_once_or_degrades_with_the_three_signals() {
    let fixture = plain(&post_hooks(vec![async_rewake_hook(
        "echo rewake-reason >&2; exit 2",
    )]));
    after_patch(&fixture, "a.rs").await;
    drain_async(&fixture).await;
    let entry = fixture
        .inbox
        .claim_next_turn()
        .await
        .expect("an idle asyncRewake wakes a turn");
    assert_eq!(entry.message.text(), "rewake-reason\n");
    assert_eq!(fixture.hooks.stop_wakes(), 1);
    assert_eq!(fixture.hooks.degraded(), 0);
    assert!(fixture.inbox.claim_next_step().await.is_none());

    let fixture = plain(&post_hooks(vec![async_rewake_hook(
        "echo rewake-reason >&2; exit 2",
    )]));
    for _ in 0..MAX_WAKES_PER_WINDOW {
        fixture.hooks.gate.note_wake();
    }
    after_patch(&fixture, "a.rs").await;
    drain_async(&fixture).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "past the gate no turn opens"
    );
    let step = fixture
        .inbox
        .claim_next_step()
        .await
        .expect("the gated result injects");
    assert_eq!(step.message.text(), "rewake-reason\n");
    assert_eq!(
        fixture.hooks.degraded(),
        1,
        "the degraded signal is the journal-visible count"
    );
    let notices = warned(&fixture.wake_notices);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("wake budget"), "{notices:?}");
}

/// An async exit 2 without `asyncRewake` injects its stderr like `additionalContext`:
/// it never opens a continuation and never wakes.
#[tokio::test]
async fn async_post_exit_2_injects_stderr_without_waking() {
    let fixture = plain(&post_hooks(vec![async_hook(
        "echo lint-failed >&2; exit 2",
    )]));
    let result = after_patch(&fixture, "a.rs").await;
    assert_eq!(result.text(), "patched", "the result is untouched");
    drain_async(&fixture).await;
    let injected = fixture
        .inbox
        .claim_next_step()
        .await
        .expect("exit 2 injects its stderr");
    assert_eq!(injected.message.text(), "lint-failed\n");
    assert_eq!(injected.source, InputSource::System);
    assert!(fixture.inbox.claim_next_turn().await.is_none());
    assert_eq!(fixture.hooks.stop_wakes(), 0);
}

/// Edge case 4: an async hook's `permissionDecision` (and the other control fields) is
/// ignored with one warning; the tool call and its result are unaffected and nothing
/// is delivered.
#[tokio::test]
async fn async_control_fields_are_ignored_with_one_warning() {
    for field in [
        r#""permissionDecision":"deny""#,
        r#""updatedInput":{"path":"elsewhere"}"#,
        r#""updatedToolOutput":"changed""#,
        r#""decision":{"behavior":"deny"}"#,
    ] {
        let script = format!(r#"printf '%s' '{{"hookSpecificOutput":{{{field}}}}}'"#);
        let fixture = plain(&post_hooks(vec![async_hook(&script)]));
        let result = after_patch(&fixture, "a.rs").await;
        assert_eq!(result.text(), "patched", "{field}: the result is untouched");
        assert!(!result.is_error, "{field}");
        drain_async(&fixture).await;
        let warnings = warned(&fixture.warnings);
        assert_eq!(warnings.len(), 1, "{field}: {warnings:?}");
        assert!(
            warnings[0].contains("ignored") && warnings[0].contains("async hook"),
            "{field}: {warnings:?}"
        );
        assert!(fixture.inbox.claim_next_step().await.is_none());
        assert!(fixture.inbox.claim_next_turn().await.is_none());
    }
}

/// Edge case 6: a `workspace`-tier async hook runs inside the same sandbox, so a write
/// outside the workspace fails and lands nothing; the failure is one warning line and
/// never fails the session closed.
#[cfg(unix)]
#[tokio::test]
async fn an_async_hook_runs_inside_the_workspace_sandbox() {
    let root = TempRoot::new("sandbox-async");
    let outside = root.outside("escaped.txt");
    let config = post_hooks(vec![async_hook(&format!(
        "echo escaped > {}; exit 1",
        outside.display()
    ))]);
    let sandbox =
        radian_sandbox::Sandbox::new(radian_sandbox::SandboxMode::WorkspaceWrite, &root.ws)
            .expect("the workspace sandbox probes clean on this machine");
    let fixture = layered_with_root(
        root,
        &config,
        &HooksConfig::default(),
        SandboxSlot::with(sandbox),
    );
    let result = after_patch(&fixture, "a.rs").await;
    assert_eq!(
        result.text(),
        "patched",
        "the denial never changes the result"
    );
    drain_async(&fixture).await;
    assert!(
        !outside.exists(),
        "the write outside the workspace must not land"
    );
    let warnings = warned(&fixture.warnings);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("async hook exited with code"),
        "{warnings:?}"
    );
    let runs = fixture.hooks.hook_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].end, HookRunEnd::Status(1));
}

/// An async `Stop` hook is *not* part of the continuation chain when it exits 2: it is
/// spawned and the turn still ends with no new turn, and the idle-continuation counter
/// never sees it.
#[tokio::test]
async fn an_async_stop_hook_never_joins_the_continuation_chain() {
    let config = HooksConfig {
        stop: vec![HookMatcher {
            matcher: None,
            hooks: vec![async_hook("echo more >&2; exit 2")],
        }],
        ..HooksConfig::default()
    };
    let fixture = plain(&config);
    fixture.hooks.run_stop_once(&completed_quiet()).await;
    drain_async(&fixture).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "an async Stop hook never opens the turn"
    );
    let injected = fixture
        .inbox
        .claim_next_step()
        .await
        .expect("its exit 2 injects instead");
    assert_eq!(injected.message.text(), "more\n");
    assert_eq!(fixture.hooks.hook_idle_continuations(), 0);
    assert_eq!(fixture.hooks.stop_wakes(), 0);
}

/// Edge case 7: an async `Stop` hook is running when the validator decides, so the
/// validator does not trigger, and the sync hook's `background_tasks` lists the async
/// one as `type: "hook"`.
#[tokio::test]
async fn a_running_async_stop_hook_stands_the_validator_down_and_is_in_background_tasks() {
    let root = TempRoot::new("async-stop-background");
    let stdin_path = root.dir.join("stop-stdin.json");
    let config = HooksConfig {
        stop: vec![HookMatcher {
            matcher: None,
            hooks: vec![
                async_hook("sleep 2; exit 0"),
                hook(&format!("cat > {}", stdin_path.display())),
            ],
        }],
        ..HooksConfig::default()
    };
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    fixture
        .hooks
        .run_stop_once(&completed_text("Now the docs-only ADR updates."))
        .await;
    assert_eq!(
        fixture.hooks.async_hooks_running(),
        1,
        "the async Stop hook is still running"
    );
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "a running async hook stands the validator down"
    );
    let stdin: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&stdin_path).unwrap()).unwrap();
    let tasks = stdin["background_tasks"].as_array().expect("an array");
    assert_eq!(tasks.len(), 1, "{stdin}");
    assert_eq!(tasks[0]["type"], "hook");
    assert_eq!(tasks[0]["status"], "running");
    assert_eq!(tasks[0]["command"], "sleep 2; exit 0");
    drain_async(&fixture).await;
}

/// Edge case 8: an async `additionalContext` result never wakes, whether the session
/// is idle or a turn is in flight when the hook finishes; it waits in `next_step`.
#[tokio::test]
async fn async_results_never_wake_idle_or_busy() {
    let script = r#"printf '%s' '{"hookSpecificOutput":{"additionalContext":"done"}}'"#;
    let fixture = plain(&post_hooks(vec![async_hook(script)]));

    // A turn is in flight when the hook finishes (busy).
    fixture
        .inbox
        .followup(Message::user("a turn"), InputSource::User)
        .await
        .unwrap();
    let turn = fixture.inbox.claim_next_turn().await.expect("a turn");
    after_patch(&fixture, "busy.rs").await;
    drain_async(&fixture).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "busy: the result waits for the turn's next model call"
    );
    assert_eq!(fixture.hooks.stop_wakes(), 0);
    let injected = fixture.inbox.claim_next_step().await.expect("busy inject");
    assert_eq!(injected.message.text(), "[PostToolUse hook] done");
    fixture.inbox.turn_ended();
    drop(turn);

    // No turn in flight when the hook finishes (idle).
    after_patch(&fixture, "idle.rs").await;
    drain_async(&fixture).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "idle: no wake either"
    );
    assert_eq!(fixture.hooks.stop_wakes(), 0);
    assert!(fixture.inbox.claim_next_step().await.is_some());
}

// ---------- async hook lifecycle (ADR 0045 §5) ----------

/// Whether a pid is still alive (`kill 0` only probes).
#[cfg(unix)]
pub(super) fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 checks existence and sends nothing.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Waits until `path` holds at least `count` pids, and returns them in file order.
#[cfg(unix)]
pub(super) async fn wait_for_pids(path: &Path, count: usize) -> Vec<u32> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            let pids: Vec<u32> = text
                .lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect();
            if pids.len() >= count {
                return pids;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the async hook never recorded its pid in {}",
            path.display()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Waits for one pid to disappear, failing the test when it outlives the deadline.
#[cfg(unix)]
pub(super) async fn wait_gone(pid: u32, what: &str) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while alive(pid) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what} (pid {pid}) is still alive"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Waits until a run record matching `end` is in the session's history.
pub(super) async fn wait_for_run_end(fixture: &Fixture, end: HookRunEnd) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if fixture.hooks.hook_runs().iter().any(|run| run.end == end) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no run record for {end:?}: {:?}",
            fixture.hooks.hook_runs()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// An async hook that records its own pid, names the edited file in its result, and
/// sleeps `sleep_secs` before exiting: two runs of the one command tell themselves apart
/// by their stdin.
#[cfg(unix)]
pub(super) fn tagged_async_hook(pids: &Path, sleep_secs: u64) -> HookCommand {
    async_hook(&format!(
        "echo $$ >> {}; input=$(cat); case \"$input\" in *one.rs*) tag=ONE;; *) tag=TWO;; esac; \
         sleep {sleep_secs}; printf '{{\"hookSpecificOutput\":{{\"additionalContext\":\"%s\"}}}}' \
         \"$tag\"",
        pids.display()
    ))
}

/// Edge case 1: the same async hook fired twice 100 ms apart. The first run's process
/// group is killed (its pid is gone), only the second run's text is injected, and the
/// killed run's record says `cancelled`. Without the supersede rule both texts arrive.
#[cfg(unix)]
#[tokio::test]
async fn a_superseded_async_hook_is_killed_and_only_the_latest_result_is_delivered() {
    let root = TempRoot::new("async-supersede");
    let pids = root.dir.join("pids");
    let fixture = layered_with_root(
        root,
        &post_hooks(vec![tagged_async_hook(&pids, 2)]),
        &HooksConfig::default(),
        SandboxSlot::empty(),
    );
    after_patch(&fixture, "one.rs").await;
    let first = wait_for_pids(&pids, 1).await[0];
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    after_patch(&fixture, "two.rs").await;
    let all = wait_for_pids(&pids, 2).await;
    assert_eq!(all.len(), 2, "one pid per run: {all:?}");
    wait_gone(first, "the superseded run's process group").await;
    drain_async(&fixture).await;
    let mut texts = Vec::new();
    while let Some(entry) = fixture.inbox.claim_next_step().await {
        texts.push(entry.message.text());
    }
    assert_eq!(
        texts,
        vec!["[PostToolUse hook] TWO"],
        "only the latest run delivers"
    );
    let runs = fixture.hooks.hook_runs();
    assert!(
        runs.iter().any(|run| run.end == HookRunEnd::Cancelled),
        "the killed run is recorded as cancelled: {runs:?}"
    );
    assert!(
        runs.iter().any(|run| run.end == HookRunEnd::Status(0)),
        "the second run completed: {runs:?}"
    );
}

/// Edge case 2: two different async commands fired together both run. The supersede key
/// is the command (plus its args), so it never reaches across commands.
#[cfg(unix)]
#[tokio::test]
async fn two_different_async_commands_run_without_superseding_each_other() {
    let root = TempRoot::new("async-two-commands");
    let pids = root.dir.join("pids");
    let emit = |text: &str| {
        async_hook(&format!(
            "echo $$ >> {}; sleep 1; printf '%s' \
             '{{\"hookSpecificOutput\":{{\"additionalContext\":\"{text}\"}}}}'",
            pids.display()
        ))
    };
    let fixture = layered_with_root(
        root,
        &post_hooks(vec![emit("a-done"), emit("b-done")]),
        &HooksConfig::default(),
        SandboxSlot::empty(),
    );
    after_patch(&fixture, "a.rs").await;
    assert_eq!(
        fixture.hooks.async_hooks_running(),
        2,
        "different commands never supersede each other"
    );
    drain_async(&fixture).await;
    let mut texts = Vec::new();
    while let Some(entry) = fixture.inbox.claim_next_step().await {
        texts.push(entry.message.text());
    }
    texts.sort();
    assert_eq!(
        texts,
        vec!["[PostToolUse hook] a-done", "[PostToolUse hook] b-done"]
    );
}

/// Edge case 3: nine runs of nine different commands at once. Eight start; the ninth does
/// not, and it leaves exactly one warning and one `failed to start` record. Starting all
/// nine leaves no warning and nine running hooks.
#[tokio::test]
async fn the_ninth_concurrent_async_hook_does_not_start() {
    let hooks: Vec<HookCommand> = (0..9)
        .map(|n| {
            async_hook(&format!(
                "sleep 1; printf '%s' \
                 '{{\"hookSpecificOutput\":{{\"additionalContext\":\"n{n}\"}}}}'"
            ))
        })
        .collect();
    let fixture = plain(&post_hooks(hooks));
    after_patch(&fixture, "a.rs").await;
    assert_eq!(
        fixture.hooks.async_hooks_running(),
        MAX_ASYNC_HOOKS,
        "the cap holds at {MAX_ASYNC_HOOKS}"
    );
    let warnings = warned(&fixture.warnings);
    assert_eq!(warnings.len(), 1, "exactly one warning: {warnings:?}");
    assert!(
        warnings[0].contains("did not start") && warnings[0].contains("already running"),
        "{warnings:?}"
    );
    let runs = fixture.hooks.hook_runs();
    assert_eq!(
        runs.len(),
        1,
        "one record for the run that never started: {runs:?}"
    );
    assert_eq!(runs[0].end, HookRunEnd::FailedToStart);
    assert!(runs[0].command.contains("n8"), "{:?}", runs[0]);
    drain_async(&fixture).await;
    assert_eq!(fixture.hooks.async_hooks_running(), 0);
}

/// Edge case 4: a run whose process already exited is delivering its result; the same
/// hook firing again must not supersede it. The entry stays registered until its delivery
/// finishes, so the supersede reads "the process exited" (and the group kill finds
/// nothing), leaves it alone, and both texts stand.
#[tokio::test]
async fn a_finished_async_run_is_not_superseded_while_its_result_is_delivered() {
    let fixture = plain(&post_hooks(Vec::new()));
    let hook = async_hook("true");
    let (first_id, first) = fixture
        .hooks
        .begin_async_run(&hook, "PostToolUse", Some("patch"))
        .expect("the first run registers");
    // The process exited before delivery starts: the task sets this first.
    first.exited.store(true, Ordering::SeqCst);
    let (_, second) = fixture
        .hooks
        .begin_async_run(&hook, "PostToolUse", Some("patch"))
        .expect("the second run registers");
    assert!(
        !first.cancelled.load(Ordering::SeqCst),
        "a run whose process already exited is never superseded"
    );
    assert!(!second.cancelled.load(Ordering::SeqCst));
    let ids: Vec<String> = fixture
        .hooks
        .running_async_hooks()
        .into_iter()
        .map(|run| run.id)
        .collect();
    assert_eq!(
        ids.len(),
        2,
        "both runs are listed while one delivers: {ids:?}"
    );
    assert!(ids.contains(&first_id));
    fixture.hooks.end_session().await;
}

/// Edge case 4, end to end: with the first run's result already in the inbox (its
/// delivery is blocked inside the sink), the same hook fires again. The run is not
/// superseded and the enqueued text stays: no later run can retract a result that
/// already reached the inbox.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivery_in_progress_keeps_its_text_when_the_hook_fires_again() {
    /// Blocks the first inbox enqueue until the test releases it.
    struct DeliveryGate {
        first: AtomicBool,
        reached: std::sync::mpsc::Sender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl radian_runtime::RuntimeEvents for DeliveryGate {
        fn on_runtime_event(&self, event: &radian_session::RuntimeEvent) {
            if !matches!(event, radian_session::RuntimeEvent::InboxEnqueued { .. }) {
                return;
            }
            if self.first.swap(false, Ordering::SeqCst) {
                let _ = self.reached.send(());
                let _ = self
                    .release
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .recv();
            }
        }
    }

    let root = TempRoot::new("async-delivering");
    let pids = root.dir.join("pids");
    let fixture = layered_with_root(
        root,
        &post_hooks(vec![tagged_async_hook(&pids, 0)]),
        &HooksConfig::default(),
        SandboxSlot::empty(),
    );
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    fixture.inbox.set_runtime_events(Arc::new(DeliveryGate {
        first: AtomicBool::new(true),
        reached: reached_tx,
        release: std::sync::Mutex::new(release_rx),
    }));
    after_patch(&fixture, "one.rs").await;
    // The hook exits at once; its delivery is now held inside the inbox.
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio::task::spawn_blocking(move || reached_rx.recv()),
    )
    .await
    .expect("the first result never reached the inbox")
    .expect("the gate task did not panic")
    .expect("the gate was dropped");
    after_patch(&fixture, "two.rs").await;
    release_tx.send(()).unwrap();
    drain_async(&fixture).await;
    let mut texts = Vec::new();
    while let Some(entry) = fixture.inbox.claim_next_step().await {
        texts.push(entry.message.text());
    }
    assert_eq!(
        texts,
        vec!["[PostToolUse hook] ONE", "[PostToolUse hook] TWO"],
        "the delivering run is not superseded"
    );
}

/// Edge case 7: a superseded hook's grandchild is in the same process group (the hook
/// leads its own session), so the group kill stops it too. Killing only the leader
/// leaves the grandchild and fails this.
#[cfg(unix)]
#[tokio::test]
async fn a_superseded_hook_kills_its_grandchild_too() {
    let root = TempRoot::new("async-grandchild");
    let pids = root.dir.join("pids");
    let grandchild = root.dir.join("grandchild");
    let script = format!(
        "sleep 30 & echo $! > {}; echo $$ >> {}; sleep 30",
        grandchild.display(),
        pids.display()
    );
    let fixture = layered_with_root(
        root,
        &post_hooks(vec![async_hook(&script)]),
        &HooksConfig::default(),
        SandboxSlot::empty(),
    );
    after_patch(&fixture, "a.rs").await;
    let first = wait_for_pids(&pids, 1).await[0];
    let child = wait_for_pids(&grandchild, 1).await[0];
    // The same command fires again: the first process group (the shell and the
    // `sleep 30` it forked) is killed.
    after_patch(&fixture, "b.rs").await;
    wait_gone(first, "the superseded shell").await;
    wait_gone(child, "the superseded hook's grandchild").await;
    // The second run sleeps 30 s: end the session so the cleanup is exercised, not leaked.
    fixture.hooks.end_session().await;
    drain_async(&fixture).await;
}

/// Edge case 5/6 at the hook layer: ending the session kills every running async hook's
/// process group, delivers nothing, records `cancelled`, and starts no further run.
/// Without the session-end kill the pid outlives the session and its text would inject.
#[cfg(unix)]
#[tokio::test]
async fn ending_the_session_kills_a_running_async_hook_and_delivers_nothing() {
    let root = TempRoot::new("async-session-end");
    let pids = root.dir.join("pids");
    let fixture = layered_with_root(
        root,
        &post_hooks(vec![tagged_async_hook(&pids, 30)]),
        &HooksConfig::default(),
        SandboxSlot::empty(),
    );
    after_patch(&fixture, "one.rs").await;
    let pid = wait_for_pids(&pids, 1).await[0];
    assert_eq!(fixture.hooks.async_hooks_running(), 1);
    fixture.hooks.end_session().await;
    assert_eq!(
        fixture.hooks.async_hooks_running(),
        0,
        "the ended session lists no running hook"
    );
    wait_gone(pid, "the session-end kill").await;
    wait_for_run_end(&fixture, HookRunEnd::Cancelled).await;
    assert!(
        fixture.inbox.claim_next_step().await.is_none(),
        "an ended session's hook delivers nothing"
    );
    assert!(fixture.inbox.claim_next_turn().await.is_none());
    // No new run starts after the session ended, and nothing is recorded for one.
    let runs = fixture.hooks.hook_runs().len();
    after_patch(&fixture, "two.rs").await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(fixture.hooks.async_hooks_running(), 0);
    assert_eq!(
        fixture.hooks.hook_runs().len(),
        runs,
        "no run after the end"
    );
    assert_eq!(std::fs::read_to_string(&pids).unwrap().lines().count(), 1);
}

/// Rework (cap): the cap counts running hook processes, not registered entries. Eight
/// runs whose processes have already exited stay listed while their results are
/// delivered, but they hold no process, so a ninth distinct hook still starts. Counting
/// registered entries rejects it as the ninth.
#[tokio::test]
async fn the_cap_counts_running_processes_not_delivering_results() {
    let fixture = plain(&post_hooks(Vec::new()));
    for n in 0..MAX_ASYNC_HOOKS {
        let hook = async_hook(&format!("true # {n}"));
        let (_, handle) = fixture
            .hooks
            .begin_async_run(&hook, "PostToolUse", Some("patch"))
            .unwrap_or_else(|| panic!("run {n} starts"));
        // The process has exited; the result is still waiting to be delivered.
        handle.exited.store(true, Ordering::SeqCst);
    }
    assert_eq!(
        fixture.hooks.async_hooks_running(),
        MAX_ASYNC_HOOKS,
        "eight delivering entries are still listed"
    );
    let ninth = async_hook("true # ninth");
    assert!(
        fixture
            .hooks
            .begin_async_run(&ninth, "PostToolUse", Some("patch"))
            .is_some(),
        "no hook process is running, so the ninth hook starts"
    );
    assert!(
        warned(&fixture.warnings).is_empty(),
        "no cap warning: {:?}",
        warned(&fixture.warnings)
    );
    fixture.hooks.end_session().await;
}

/// Rework (session end): the end is an atomic guard around the inbox enqueue, not a check
/// before it. A delivery that is inside the enqueue when the end begins finishes before
/// `end_session` returns, so nothing can append to the old session afterwards. With the
/// check outside the guard, `end_session` returns while the delivery is still blocked in
/// the journal, and its text lands after the end.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_session_end_waits_for_a_delivery_already_enqueueing() {
    use radian_core::{JournalRecord, TranscriptJournal as _};
    use radian_session::{SessionCreatedData, SessionJournal};

    let root = TempRoot::new("async-end-guard");
    let journal = Arc::new(
        SessionJournal::create(
            root.dir.join("s.jsonl"),
            "sid",
            SessionCreatedData::new(
                "/work/example",
                serde_json::json!({"provider": "example", "model": "m-1"}),
            ),
        )
        .unwrap(),
    );
    let inbox = Inbox::durable(64, Arc::clone(&journal));
    let (warn, warnings) = warnings_collector();
    let (wake, wake_notices) = warnings_collector();
    let (intent, intent_notices) = warnings_collector();
    let (hook_stop, hook_stop_notices) = warnings_collector();
    let config = post_hooks(vec![async_hook(
        r#"printf '%s' '{"hookSpecificOutput":{"additionalContext":"late"}}'"#,
    )]);
    let hooks = CommandHooks::new(
        &config,
        &HooksConfig::default(),
        &root.ws,
        "test-session",
        SandboxSlot::empty(),
        &inbox,
        &tokio::runtime::Handle::current(),
        warn,
        wake,
        intent,
        hook_stop,
    );
    let fixture = Fixture {
        _root: root,
        hooks,
        warnings,
        wake_notices,
        intent_notices,
        hook_stop_notices,
        inbox,
    };

    // An open replacement segment holds the delivery inside the inbox's journal append,
    // so it is provably mid-enqueue when the session end begins.
    journal
        .append(JournalRecord::TranscriptReplaced {
            revision: 1,
            len: 0,
        })
        .await
        .unwrap();
    after_patch(&fixture, "one.rs").await;
    journal.wait_for_gate_waiter().await;

    let end = {
        let hooks = fixture.hooks.clone();
        tokio::spawn(async move { hooks.end_session().await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !end.is_finished(),
        "the session end must wait for the delivery that is already enqueueing"
    );

    // Close the segment: the delivery's enqueue finishes, then the end sets its flag
    // and returns. Nothing can append after it.
    journal
        .append(JournalRecord::TranscriptReplacementCommitted {
            revision: 1,
            len: 0,
        })
        .await
        .unwrap();
    end.await.expect("the session-end task did not panic");
    let entry = fixture
        .inbox
        .claim_next_step()
        .await
        .expect("the delivery completed before the end returned");
    assert_eq!(entry.message.text(), "[PostToolUse hook] late");
    assert!(
        fixture.inbox.claim_next_step().await.is_none(),
        "nothing is delivered after the end"
    );
}

/// Edge case 8: a cancelled turn leaves a running async hook alone — it belongs to an
/// action that already happened — and its result still injects. A cancel path that also
/// ends the hooks kills the pid and loses the text.
#[cfg(unix)]
#[tokio::test]
async fn a_cancelled_turn_leaves_a_running_async_hook_alone() {
    let root = TempRoot::new("async-cancel");
    let pids = root.dir.join("pids");
    let fixture = layered_with_root(
        root,
        &post_hooks(vec![tagged_async_hook(&pids, 1)]),
        &HooksConfig::default(),
        SandboxSlot::empty(),
    );
    after_patch(&fixture, "one.rs").await;
    let pid = wait_for_pids(&pids, 1).await[0];
    fixture.inbox.cancel(radian_runtime::CancelCause::Requested);
    fixture
        .hooks
        .run_stop_once(&completed_outcome(
            RunStatus::Cancelled(CancelReason::Requested),
            Some("stopping"),
        ))
        .await;
    assert_eq!(
        fixture.hooks.async_hooks_running(),
        1,
        "a cancelled turn never touches a running async hook"
    );
    drain_async(&fixture).await;
    assert!(!alive(pid), "the hook ran to its own end");
    let injected = fixture
        .inbox
        .claim_next_step()
        .await
        .expect("the cancelled turn left the result alone");
    assert_eq!(injected.message.text(), "[PostToolUse hook] ONE");
}
