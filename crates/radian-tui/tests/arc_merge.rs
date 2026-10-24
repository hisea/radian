//! The arc TUI's merge page and the owner's actions (ADR 0048 §2, §6, §10 and §11).
//!
//! The stage rows, the review's groups, the commits and the diff come from fixture verdicts
//! and a real git repository; the actions run the library against a real repository with a
//! bare `origin`, the way `radian arc`'s own tests do. The confirmation float, the reject
//! note, the held `state.lock` and the auto cycle are driven through the app's keys.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use radian_arc::gate::{CommandRecord, ReviewItem, ReviewReport, Stage, Verdict, verdict_path};
use radian_arc::read;
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, config};
use radian_tui::arc::{ArcApp, ArcKey, actions, render};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;

/// A temp directory that removes itself.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-arc-merge-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the temp dir is creatable");
        Self { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "arc test")
        .env("GIT_AUTHOR_EMAIL", "arc@example.com")
        .env("GIT_COMMITTER_NAME", "arc test")
        .env("GIT_COMMITTER_EMAIL", "arc@example.com")
        .args(["-c", "core.hooksPath="])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn write(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn render(app: &mut ArcApp, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
}

fn screen(app: &mut ArcApp, width: u16, height: u16) -> String {
    let terminal = render(app, width, height);
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    (area.top()..area.bottom())
        .map(|y| {
            (area.left()..area.right())
                .map(|x| {
                    buffer
                        .cell((x, y))
                        .map(|cell| cell.symbol())
                        .unwrap_or(" ")
                        .to_string()
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The foreground of the first `mark` that sits before `needle` in its own row.
fn mark_fg(
    app: &mut ArcApp,
    needle: &str,
    mark: char,
    width: u16,
    height: u16,
) -> Option<ratatui::style::Color> {
    let terminal = render(app, width, height);
    let buffer = terminal.backend().buffer();
    for y in 0..height {
        let text: String = (0..width)
            .map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" "))
            .collect();
        let Some(byte) = text.find(needle) else {
            continue;
        };
        let column = text[..byte].chars().count();
        let chars: Vec<char> = text.chars().collect();
        if let Some(found) = chars[..column].iter().rposition(|ch| *ch == mark) {
            return buffer.cell((found as u16, y)).map(|cell| cell.fg);
        }
    }
    None
}

fn short(sha: &str) -> &str {
    &sha[..8]
}

/// The main sha the bare `origin` has, read over the wire.
fn remote_main(repo: &Path) -> String {
    git(repo, &["ls-remote", "origin", "refs/heads/main"])
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned()
}

// ---- the merge page's fixture --------------------------------------------------------

/// The config text with `stages` replaced by `stages`.
fn config_text(stages: &[&str]) -> String {
    let list = stages
        .iter()
        .map(|stage| format!("{{ stage = \"{stage}\", required = true }}"))
        .collect::<Vec<_>>()
        .join(", ");
    config::default_text("main").replace(
        "stages = [{ stage = \"commands\", required = true }, { stage = \"human\", required = true }]",
        &format!("stages = [{list}]"),
    )
}

/// A repository whose task branch holds two commits, and a store with one task in
/// `review.awaiting_human`.
struct Fixture {
    scratch: Scratch,
    repo: PathBuf,
    store: Store,
    plan: String,
    task: String,
    base: String,
    head: String,
}

impl Fixture {
    fn new(label: &str, stages: &[&str]) -> Self {
        let scratch = Scratch::new(label);
        let repo = scratch.path("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        write(&repo, "base.txt", "base\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);

        let root = repo.join(".radian/arc");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("config.toml"), config_text(stages)).unwrap();
        let store = Store::at(&repo);
        let plan = store
            .add_plan("view", "the view plan", "a request\n")
            .unwrap();
        let task = store
            .add_task(
                &plan,
                "view",
                "the view task",
                Vec::new(),
                false,
                Vec::new(),
                "the brief\n",
            )
            .unwrap();

        let branch = format!("work/{task}");
        git(&repo, &["checkout", "-q", "-B", &branch, &base]);
        write(&repo, "one.txt", "one\n");
        git(&repo, &["add", "one.txt"]);
        git(&repo, &["commit", "-q", "-m", "arc: add the first change"]);
        write(&repo, "two.txt", "two\n");
        git(&repo, &["add", "two.txt"]);
        git(
            &repo,
            &["commit", "-q", "-m", "adr: record the first change"],
        );
        let head = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["checkout", "-q", "main"]);
        // The remote-tracking ref the commit range is measured from.
        git(&repo, &["update-ref", "refs/remotes/origin/main", &base]);

        store
            .change_task(&task, |task| {
                task.state = TaskState::Review;
                task.phase = "awaiting_human".to_owned();
                task.attempt = 1;
                task.base_sha = Some(base.clone());
                task.head_sha = Some(head.clone());
                task.verified_sha = Some(head.clone());
                task.verdict_set_digest = Some("d".repeat(64));
                task.cost_usd = Some(0.4);
                Ok(())
            })
            .unwrap();
        Self {
            scratch,
            repo,
            store,
            plan,
            task,
            base,
            head,
        }
    }

    /// Sets `verified_sha` to `sha`: the default is the head, and `self.base` makes the branch
    /// look moved.
    fn verify_sha(&self, sha: &str) {
        let sha = sha.to_owned();
        self.store
            .change_task(&self.task, |task| {
                task.verified_sha = Some(sha.clone());
                Ok(())
            })
            .unwrap();
    }

    fn label(&self) -> String {
        format!("{}-1", self.task)
    }

    /// Writes one verdict for attempt 1.
    fn write(&self, verdict: &mut Verdict) {
        let path = verdict_path(&self.repo, &self.task, &self.label(), verdict.stage).unwrap();
        verdict.write_new(&path).expect("a verdict is written once");
    }

    /// A passing `commands` verdict whose three commands took the times the prototype shows.
    fn commands(&self, pass: bool, reason: Option<&str>) -> Verdict {
        let mut verdict = Verdict::human(self.verified_sha(), pass, reason.map(str::to_owned));
        verdict.stage = Stage::Commands;
        verdict.started_at = "2026-10-22T10:00:00Z".to_owned();
        verdict.ended_at = "2026-10-22T10:05:54Z".to_owned();
        verdict.commands = vec![
            CommandRecord {
                line: "cargo fmt --all -- --check".to_owned(),
                exit_code: Some(0),
                duration_ms: 1_000,
                log: self.repo.join("fmt.log").display().to_string(),
            },
            CommandRecord {
                line: "cargo clippy --workspace --all-targets -- -D warnings".to_owned(),
                exit_code: Some(0),
                duration_ms: 22_000,
                log: self.repo.join("clippy.log").display().to_string(),
            },
            CommandRecord {
                line: "cargo nextest run --workspace".to_owned(),
                exit_code: if pass { Some(0) } else { Some(1) },
                duration_ms: 185_000,
                log: self.repo.join("test.log").display().to_string(),
            },
        ];
        verdict
    }

    fn verified_sha(&self) -> String {
        self.store
            .load_task(&self.task)
            .unwrap()
            .item
            .verified_sha
            .unwrap()
    }

    /// A passing `review` verdict with the report the test gives it.
    fn review(&self, report: ReviewReport) -> Verdict {
        let mut verdict = Verdict::human(self.verified_sha(), true, None);
        verdict.stage = Stage::Review;
        verdict.started_at = "2026-10-22T10:06:00Z".to_owned();
        verdict.ended_at = "2026-10-22T10:08:10Z".to_owned();
        verdict.review = Some(report);
        verdict.review_session_ids = vec![format!("{}-review", self.task)];
        verdict
    }

    fn human(&self, approver: Option<&str>) -> Verdict {
        let mut verdict = Verdict::human(self.verified_sha(), true, None);
        verdict.stage = Stage::Human;
        verdict.approver = approver.map(str::to_owned);
        verdict
    }

    /// Opens the merge page on the task and applies the view the runner would read.
    fn app(&self) -> ArcApp {
        let mut app =
            ArcApp::with_session_dir(self.store.clone(), Some(self.scratch.path("sessions")));
        app.plan_sel = app
            .plans
            .iter()
            .position(|view| view.plan.item.id == self.plan)
            .unwrap();
        let view = app.selected_plan().unwrap();
        app.task_sel = app
            .task_rows(view)
            .iter()
            .position(|row| row.id == self.task)
            .unwrap();
        app.open_task();
        assert!(app.open_merge(), "the merge page opens for review");
        let view = read::merge_view(&self.store, &self.task, false).expect("the view reads");
        app.apply_merge(&self.task, Ok(view));
        app
    }

    /// Loads the task's diff into the merge page, as the runner's worker would.
    fn with_diff(&self, app: &mut ArcApp) {
        let diff = read::task_diff(&self.store, &self.task, Some(1)).expect("the diff reads");
        assert!(app.apply_diff(&self.task, 1, Ok(diff)));
    }
}

fn report(
    blocking: Vec<ReviewItem>,
    suggestions: Vec<ReviewItem>,
    verified: Vec<&str>,
) -> ReviewReport {
    ReviewReport {
        summary: "Adds the reverify command: the owner re-runs the gate without a worker."
            .to_owned(),
        blocking,
        suggestions,
        verified: verified.into_iter().map(str::to_owned).collect(),
    }
}

fn item(level: &str, file: &str, line: u32, text: &str) -> ReviewItem {
    ReviewItem {
        level: Some(level.to_owned()),
        file: Some(file.to_owned()),
        line: Some(line),
        text: text.to_owned(),
        pre_existing: false,
    }
}

#[test]
fn every_stage_passes_with_its_commands_and_the_review_groups() {
    let fixture = Fixture::new("pass", &["commands", "review", "human"]);
    let theme = radian_tui::theme::Theme::default();
    fixture.write(&mut fixture.commands(true, None));
    fixture.write(&mut fixture.review(report(
        Vec::new(),
        vec![item("minor", "src/x.rs", 3, "use a helper")],
        vec!["the CLI prints one line"],
    )));
    fixture.write(&mut fixture.human(Some("auto")));
    let mut app = fixture.app();
    fixture.with_diff(&mut app);
    let text = screen(&mut app, 120, 44);
    let head = short(&fixture.head);
    for needle in [
        format!("{} verified {head} · attempt 1", fixture.task),
        "the view task".to_owned(),
        format!("✓ commands    on {head}   3 commands · 5m54s"),
        "cargo fmt --all -- --check".to_owned(),
        "cargo nextest run --workspace".to_owned(),
        "185s".to_owned(),
        "◻ mutation    not in this profile".to_owned(),
        format!("✓ review      on {head}   1 session · 2m10s"),
        "◻ ci          not in this profile".to_owned(),
        "✓ human       approver auto".to_owned(),
        "◻ integration not run yet".to_owned(),
        "Adds the reverify command".to_owned(),
        "suggestions".to_owned(),
        "• [minor] src/x.rs:3 use a helper".to_owned(),
        "verified".to_owned(),
        "✓ the CLI prints one line".to_owned(),
        format!("• {} adr: record the first change", short(&fixture.head)),
        "arc: add the first change".to_owned(),
        "one.txt".to_owned(),
        "╭─ one.txt".to_owned(),
    ] {
        assert!(text.contains(&needle), "missing `{needle}`:\n{text}");
    }
    assert!(
        !text.contains("blocking"),
        "an empty group is omitted:\n{text}"
    );
    // The passing marks take the success role.
    assert_eq!(
        mark_fg(&mut app, "commands", '✓', 120, 44),
        theme.status_ok.fg,
        "a pass is green"
    );
    assert_eq!(
        mark_fg(&mut app, "integration", '◻', 120, 44),
        theme.hint.fg,
        "a stage that did not run is weak"
    );
}

#[test]
fn the_merge_pages_file_list_takes_at_most_half_of_the_diff_area() {
    use radian_arc::read::{DiffFile, DiffSource, TaskDiff};
    let fixture = Fixture::new("list", &["commands", "human"]);
    fixture.write(&mut fixture.commands(true, None));
    fixture.write(&mut fixture.human(Some("auto")));
    let mut app = fixture.app();
    let files = (1..=40)
        .map(|i| DiffFile {
            path: format!("file-{i:02}.txt"),
            added: 40,
            removed: 0,
            text: (1..=40)
                .map(|line| format!("+file-{i:02}.txt line {line}\n"))
                .collect(),
            uncommitted: false,
        })
        .collect();
    assert!(app.apply_diff(
        &fixture.task,
        1,
        Ok(TaskDiff {
            files,
            source: DiffSource::Range,
            fallback: false,
        }),
    ));
    for _ in 0..39 {
        app.on_key(&key(KeyCode::Char('n')));
    }
    assert_eq!(app.merge.as_ref().unwrap().file_sel, 39);

    let text = screen(&mut app, 120, 44);
    let lines: Vec<&str> = text.lines().collect();
    let first = lines
        .iter()
        .position(|line| line.contains("file-") || line.contains("files "))
        .expect("the list is drawn");
    let title = lines
        .iter()
        .position(|line| line.contains("╭─ file-40.txt"))
        .expect("the selected file's frame is drawn");
    let bottom = lines[title..]
        .iter()
        .position(|line| line.contains("╰──"))
        .map(|offset| title + offset)
        .expect("the diff frame ends");
    let diff_area = bottom - first + 1;
    let list_rows = lines[first..title]
        .iter()
        .filter(|line| line.contains("file-") || line.contains("files "))
        .count();
    assert_eq!(
        list_rows,
        diff_area / 2,
        "the list takes half the diff area, not all of it:\n{text}"
    );
    assert!(
        list_rows * 2 <= diff_area,
        "the list takes at most half: {list_rows} of {diff_area}\n{text}"
    );
    assert!(
        bottom > title + 1,
        "the diff frame keeps a body row:\n{text}"
    );
    // One of the list's rows is the label; the selected file is the last.
    let shown = list_rows - 1;
    assert!(
        text.contains(&format!("files {}–40 of 40", 40 - shown + 1)),
        "the list names the rows it shows:\n{text}"
    );
}

#[test]
fn a_failed_stage_shows_its_reason_and_the_commands_after_it_do_not_run() {
    let fixture = Fixture::new("failed", &["commands", "review", "human"]);
    let theme = radian_tui::theme::Theme::default();
    fixture.write(&mut fixture.commands(false, Some("command_failed")));
    let mut app = fixture.app();
    fixture.with_diff(&mut app);
    let text = screen(&mut app, 120, 44);
    for needle in [
        "× commands".to_owned(),
        "(command_failed)".to_owned(),
        "× cargo nextest run --workspace".to_owned(),
        "◻ review      not run yet".to_owned(),
        "◻ human       not run yet".to_owned(),
    ] {
        assert!(text.contains(&needle), "missing `{needle}`:\n{text}");
    }
    assert_eq!(
        mark_fg(&mut app, "commands", '×', 120, 44),
        theme.status_error.fg,
        "a failure is red"
    );
}

#[test]
fn a_moved_branch_turns_every_mark_weak_and_says_the_task_is_back_in_verification() {
    let fixture = Fixture::new("moved", &["commands", "review", "human"]);
    fixture.verify_sha(&fixture.base);
    fixture.write(&mut fixture.commands(true, None));
    fixture.write(&mut fixture.review(report(
        vec![item("critical", "src/a.rs", 7, "the lock is dropped early")],
        vec![item("minor", "src/b.rs", 2, "name it better")],
        vec!["the exit codes"],
    )));
    fixture.write(&mut fixture.human(Some("auto")));
    let mut app = fixture.app();
    let text = screen(&mut app, 120, 44);
    let head = short(&fixture.head);
    let verified = short(&fixture.base);
    assert!(
        text.contains(&format!(
            "the branch moved: head {head} is not verified {verified}; the task is back in verification"
        )),
        "{text}"
    );
    for stage in ["commands", "review", "human", "integration"] {
        assert!(
            text.contains(&format!("◻ {stage}")),
            "every mark is weak after a move: `{stage}`\n{text}"
        );
    }
    for passed in ["✓ commands", "✓ review", "✓ human", "✓ integration"] {
        assert!(
            !text.contains(passed),
            "`{passed}` survived the move:\n{text}"
        );
    }
    // No verdict detail survives either: no command rows, duration, approver, or groups.
    for detail in [
        "cargo fmt",
        "cargo nextest",
        "5m54s",
        "approver auto",
        "the lock is dropped early",
        "the exit codes",
    ] {
        assert!(
            !text.contains(detail),
            "`{detail}` survived the move:\n{text}"
        );
    }
    assert!(
        text.contains("no review for this commit: the branch moved"),
        "{text}"
    );
    assert!(text.contains("not run yet"), "{text}");
}

#[test]
fn a_task_whose_extra_stages_list_review_shows_its_review_and_summary() {
    let fixture = Fixture::new("extra-review", &["commands", "human"]);
    fixture
        .store
        .change_task(&fixture.task, |task| {
            task.extra_stages = vec!["review".to_owned()];
            Ok(())
        })
        .unwrap();
    fixture.write(&mut fixture.commands(true, None));
    fixture.write(&mut fixture.review(report(Vec::new(), Vec::new(), vec!["the extra stage"])));
    let mut app = fixture.app();
    let text = screen(&mut app, 120, 44);
    assert!(text.contains("✓ review"), "{text}");
    assert!(!text.contains("◻ review"), "{text}");
    assert!(
        text.contains("Adds the reverify command"),
        "the summary is drawn:\n{text}"
    );
    assert!(text.contains("✓ the extra stage"), "{text}");
    assert!(!text.contains("no review stage in this profile"), "{text}");
}

#[test]
fn the_review_draws_blocking_suggestions_and_verified_for_the_three_groups() {
    let fixture = Fixture::new("groups", &["commands", "review", "human"]);
    fixture.write(&mut fixture.commands(true, None));
    fixture.write(&mut fixture.review(report(
        vec![item("critical", "src/a.rs", 7, "the lock is dropped early")],
        vec![item("minor", "src/b.rs", 2, "name it better")],
        vec!["the exit codes", "the lock order"],
    )));
    let mut app = fixture.app();
    let text = screen(&mut app, 120, 44);
    for needle in [
        "blocking".to_owned(),
        "× [critical] src/a.rs:7 the lock is dropped early".to_owned(),
        "suggestions".to_owned(),
        "• [minor] src/b.rs:2 name it better".to_owned(),
        "verified".to_owned(),
        "✓ the exit codes".to_owned(),
        "✓ the lock order".to_owned(),
    ] {
        assert!(text.contains(&needle), "missing `{needle}`:\n{text}");
    }
}

#[test]
fn a_review_with_no_items_shows_only_its_summary() {
    let fixture = Fixture::new("no-groups", &["commands", "review", "human"]);
    fixture.write(&mut fixture.commands(true, None));
    fixture.write(&mut fixture.review(report(Vec::new(), Vec::new(), Vec::new())));
    let mut app = fixture.app();
    let text = screen(&mut app, 120, 44);
    assert!(text.contains("Adds the reverify command"), "{text}");
    for group in ["blocking", "suggestions"] {
        assert!(
            !text.contains(group),
            "`{group}` is omitted when empty:\n{text}"
        );
    }
    // `verified` is also omitted; the stage row is `✓ review`, not the group.
    assert!(!text.contains("│ verified"), "{text}");
}

#[test]
fn a_profile_without_a_review_stage_says_so() {
    let fixture = Fixture::new("no-review", &["commands", "human"]);
    fixture.write(&mut fixture.commands(true, None));
    let mut app = fixture.app();
    let text = screen(&mut app, 120, 44);
    assert!(text.contains("◻ review      not in this profile"), "{text}");
    assert!(
        text.contains("no review stage in this profile"),
        "the summary frame says the profile has no review:\n{text}"
    );
}

// ---- the owner's actions -------------------------------------------------------------

/// A repository with a bare `origin`, arc set up for it, a confirmed plan and a draft one.
struct ActionFixture {
    scratch: Scratch,
    repo: PathBuf,
    store: Store,
    plan: String,
    draft: String,
    draft_task: String,
    task: String,
    blocked: String,
}

impl ActionFixture {
    fn new(label: &str) -> Self {
        let scratch = Scratch::new(label);
        let origin = scratch.path("origin.git");
        git(
            scratch.dir.as_path(),
            &["init", "-q", "--bare", "origin.git"],
        );
        git(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        let repo = scratch.path("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        write(&repo, "README.md", "hello\n");
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-q", "-m", "first"]);
        git(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&repo, &["push", "-q", "-u", "origin", "main"]);
        git(&repo, &["remote", "set-head", "origin", "main"]);
        Store::init(&repo, false).expect("arc init");
        let store = Store::open(&repo).expect("the store opens");
        let plan = store.add_plan("p", "A plan", "the request\n").unwrap();
        let task = store
            .add_task(
                &plan,
                "work",
                "A task",
                Vec::new(),
                false,
                Vec::new(),
                "the brief\n",
            )
            .unwrap();
        let blocked = store
            .add_task(
                &plan,
                "blocked",
                "A blocked task",
                vec![task.clone()],
                false,
                Vec::new(),
                "the brief\n",
            )
            .unwrap();
        let draft = store.add_plan("d", "A draft", "the request\n").unwrap();
        let draft_task = store
            .add_task(
                &draft,
                "draft-task",
                "A draft task",
                Vec::new(),
                false,
                Vec::new(),
                "the brief\n",
            )
            .unwrap();
        radian_arc::store::plan_ops::confirm(&store, &plan).expect("the plan is confirmed");
        Self {
            scratch,
            repo,
            store,
            plan,
            draft,
            draft_task,
            task,
            blocked,
        }
    }

    /// Publishes the task's branch and verifies it, so the task is `review.awaiting_human`.
    fn verify_task(&self) -> String {
        let base = git(&self.repo, &["rev-parse", "refs/remotes/origin/main"]);
        let branch = format!("work/{}", self.task);
        git(&self.repo, &["checkout", "-q", "-B", &branch, &base]);
        write(&self.repo, "feature.txt", "feature\n");
        git(&self.repo, &["add", "feature.txt"]);
        git(&self.repo, &["commit", "-q", "-m", "feature"]);
        let head = git(&self.repo, &["rev-parse", "HEAD"]);
        git(&self.repo, &["push", "-q", "origin", &branch]);
        git(&self.repo, &["checkout", "-q", "main"]);
        self.store
            .change_task(&self.task, |task| {
                task.state = TaskState::Review;
                task.phase = "verifying".to_owned();
                task.attempt = 1;
                task.attempt_id = Some(format!("{}-1", task.id));
                task.base_sha = Some(base.clone());
                task.head_sha = Some(head.clone());
                Ok(())
            })
            .unwrap();
        radian_arc::review::verify(&self.store, &self.task).expect("verification runs");
        head
    }

    fn app(&self, plan: &str, task: &str) -> ArcApp {
        let mut app =
            ArcApp::with_session_dir(self.store.clone(), Some(self.scratch.path("sessions")));
        select(&mut app, plan, task, &self.store);
        app
    }

    fn phase(&self) -> String {
        let task = self.store.load_task(&self.task).unwrap().item;
        format!("{}.{}", task.state.as_str(), task.phase)
    }
}

fn select(app: &mut ArcApp, plan: &str, task: &str, store: &Store) {
    app.refresh();
    app.plan_sel = app
        .plans
        .iter()
        .position(|view| view.plan.item.id == plan)
        .unwrap();
    let view = store.plan_view(plan).unwrap();
    let rows = app.task_rows(&view);
    app.task_sel = rows
        .iter()
        .position(|row| row.id == task)
        .unwrap_or_else(|| panic!("no row `{task}` in {plan}: {rows:?}"));
    app.focus = radian_tui::arc::Focus::Tasks;
}

#[test]
fn c_confirms_only_a_draft_plan() {
    let fixture = ActionFixture::new("confirm");
    let mut app = fixture.app(&fixture.draft, &fixture.draft_task);
    // A draft plan is confirmed with the plan column focused (with the task column
    // focused `c` confirms a proposed task instead).
    app.focus = radian_tui::arc::Focus::Plans;
    assert_eq!(app.on_key(&key(KeyCode::Char('c'))), ArcKey::Redraw);
    let action = app.take_action().expect("the draft plan is confirmed");
    assert_eq!(action, actions::Action::Confirm(fixture.draft.clone()));
    assert!(app.pending.is_some(), "the spinner starts with the action");
    let outcome = actions::run(&action, &fixture.store, &actions::Library);
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.text, format!("{} confirmed", fixture.draft));
    app.finish_action(outcome);
    assert!(app.pending.is_none());
    assert!(
        app.result
            .as_ref()
            .is_some_and(|line| line.text.contains("confirmed")),
        "{:?}",
        app.result
    );

    // A confirmed plan is not confirmed again: the key explains why and queues nothing.
    let mut app = fixture.app(&fixture.plan, &fixture.task);
    app.focus = radian_tui::arc::Focus::Plans;
    app.on_key(&key(KeyCode::Char('c')));
    assert!(app.take_action().is_none());
    assert!(
        app.result.as_ref().is_some_and(|line| line
            .text
            .contains("only a draft plan or a proposed task is confirmed")),
        "{:?}",
        app.result
    );
}

#[test]
fn c_confirms_a_proposed_task_and_refuses_a_ready_one() {
    let fixture = ActionFixture::new("confirm-task");
    // A confirmed plan takes the lead's new task as `todo.proposed` (ADR 0048 decision 3).
    let proposed = fixture
        .store
        .add_task_proposed(
            &fixture.plan,
            "proposed",
            "A proposed task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .unwrap();
    let mut app = fixture.app(&fixture.plan, &proposed);
    assert_eq!(app.focus, radian_tui::arc::Focus::Tasks);
    assert_eq!(app.on_key(&key(KeyCode::Char('c'))), ArcKey::Redraw);
    assert_eq!(
        app.take_action(),
        Some(actions::Action::Confirm(proposed.clone())),
        "`c` on a proposed task confirms it"
    );
    let outcome = actions::run(
        &actions::Action::Confirm(proposed.clone()),
        &fixture.store,
        &actions::Library,
    );
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.text, format!("{proposed} todo.ready"));
    assert_eq!(
        fixture.store.load_task(&proposed).unwrap().item.phase,
        "ready"
    );

    // A task that is not proposed is refused with the one footer line, and nothing runs.
    let mut app = fixture.app(&fixture.plan, &fixture.task);
    app.on_key(&key(KeyCode::Char('c')));
    assert!(app.take_action().is_none());
    assert!(
        app.result.as_ref().is_some_and(|line| line
            .text
            .contains("only a draft plan or a proposed task is confirmed")),
        "{:?}",
        app.result
    );
}

#[test]
fn x_on_a_task_asks_and_n_changes_nothing_while_y_cancels_it() {
    let fixture = ActionFixture::new("cancel-task");
    let mut app = fixture.app(&fixture.plan, &fixture.task);
    assert_eq!(app.focus, radian_tui::arc::Focus::Tasks);
    app.on_key(&key(KeyCode::Char('x')));
    match &app.float {
        Some(radian_tui::arc::Float::Confirm { message, action }) => {
            assert_eq!(message, &format!("cancel {} ?", fixture.task));
            assert_eq!(action, &actions::Action::Cancel(fixture.task.clone()));
        }
        other => panic!("no cancel float: {other:?}"),
    }
    // `n` (and `Esc`) closes the float without queueing anything.
    app.on_key(&key(KeyCode::Char('n')));
    assert!(app.float.is_none());
    assert!(app.take_action().is_none());
    assert_eq!(fixture.phase(), "todo.ready");
    app.on_key(&key(KeyCode::Char('x')));
    app.on_key(&key(KeyCode::Esc));
    assert!(app.take_action().is_none());
    assert_eq!(fixture.phase(), "todo.ready");

    // `y` runs the CLI's cancel.
    app.on_key(&key(KeyCode::Char('x')));
    app.on_key(&key(KeyCode::Char('y')));
    let action = app.take_action().expect("the task cancel is queued");
    assert_eq!(action, actions::Action::Cancel(fixture.task.clone()));
    let outcome = actions::run(&action, &fixture.store, &actions::Library);
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.text, format!("{} failed.cancelled", fixture.task));
    app.finish_action(outcome);
    assert_eq!(fixture.phase(), "failed.cancelled");
    assert!(
        app.result
            .as_ref()
            .is_some_and(|line| line.text.contains("failed.cancelled")),
        "{:?}",
        app.result
    );
}

#[test]
fn x_on_a_plan_counts_its_open_tasks_and_y_cancels_the_plan() {
    let fixture = ActionFixture::new("cancel-plan");
    let mut app = fixture.app(&fixture.plan, &fixture.task);
    app.focus = radian_tui::arc::Focus::Plans;
    app.on_key(&key(KeyCode::Char('x')));
    match &app.float {
        Some(radian_tui::arc::Float::Confirm { message, action }) => {
            assert_eq!(
                message,
                &format!("cancel plan {} and its 2 open tasks?", fixture.plan),
                "both todo tasks are open"
            );
            assert_eq!(action, &actions::Action::Cancel(fixture.plan.clone()));
        }
        other => panic!("no cancel float: {other:?}"),
    }
    app.on_key(&key(KeyCode::Char('n')));
    assert!(app.take_action().is_none());
    assert_eq!(
        fixture.store.load_plan(&fixture.plan).unwrap().item.status,
        "confirmed",
        "`n` changed nothing"
    );

    app.on_key(&key(KeyCode::Char('x')));
    app.on_key(&key(KeyCode::Char('y')));
    let action = app.take_action().expect("the plan cancel is queued");
    assert_eq!(action, actions::Action::Cancel(fixture.plan.clone()));
    let outcome = actions::run(&action, &fixture.store, &actions::Library);
    assert!(outcome.ok, "{outcome:?}");
    assert!(
        outcome
            .text
            .starts_with(&format!("{} cancelled", fixture.plan)),
        "the plan line comes first: {outcome:?}"
    );
    assert!(
        outcome
            .text
            .contains(&format!("{} cancelled", fixture.plan)),
        "{outcome:?}"
    );
    app.finish_action(outcome);
    assert_eq!(
        fixture.store.load_plan(&fixture.plan).unwrap().item.status,
        "cancelled"
    );
    assert_eq!(fixture.phase(), "failed.cancelled");
    let blocked = fixture.store.load_task(&fixture.blocked).unwrap().item;
    assert_eq!(
        format!("{}.{}", blocked.state.as_str(), blocked.phase),
        "failed.cancelled",
        "the plan form cancels every open task"
    );
}

#[test]
fn plus_and_minus_move_the_priority_and_stop_at_the_ends() {
    let fixture = ActionFixture::new("priority");
    assert_eq!(
        fixture
            .store
            .load_plan(&fixture.plan)
            .unwrap()
            .item
            .priority(),
        "P3",
        "a plan without the field reads as P3"
    );
    let mut app = fixture.app(&fixture.plan, &fixture.task);
    app.focus = radian_tui::arc::Focus::Plans;
    // `+` is toward P0: P3 -> P2 -> P1 -> P0, then it stops.
    for expected in ["P2", "P1", "P0"] {
        app.on_key(&key(KeyCode::Char('+')));
        let action = app.take_action().expect("the step is queued");
        assert_eq!(
            action,
            actions::Action::Priority {
                plan: fixture.plan.clone(),
                level: expected.to_owned(),
            }
        );
        let outcome = actions::run(&action, &fixture.store, &actions::Library);
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(
            outcome.text,
            format!("{} priority {expected}", fixture.plan)
        );
        app.finish_action(outcome);
    }
    app.on_key(&key(KeyCode::Char('+')));
    assert!(app.take_action().is_none(), "P0 is the end");
    assert!(
        app.result
            .as_ref()
            .is_some_and(|line| line.text.contains("already P0")),
        "{:?}",
        app.result
    );
    assert_eq!(
        fixture
            .store
            .load_plan(&fixture.plan)
            .unwrap()
            .item
            .priority(),
        "P0"
    );

    // `-` is toward P5: P0 -> P1 -> ... -> P5, then it stops.
    for expected in ["P1", "P2", "P3", "P4", "P5"] {
        app.on_key(&key(KeyCode::Char('-')));
        let action = app.take_action().expect("the step is queued");
        assert_eq!(
            action,
            actions::Action::Priority {
                plan: fixture.plan.clone(),
                level: expected.to_owned(),
            }
        );
        let outcome = actions::run(&action, &fixture.store, &actions::Library);
        assert!(outcome.ok, "{outcome:?}");
        app.finish_action(outcome);
    }
    app.on_key(&key(KeyCode::Char('-')));
    assert!(app.take_action().is_none(), "P5 is the end");
    assert!(
        app.result
            .as_ref()
            .is_some_and(|line| line.text.contains("already P5")),
        "{:?}",
        app.result
    );
    assert_eq!(
        fixture
            .store
            .load_plan(&fixture.plan)
            .unwrap()
            .item
            .priority(),
        "P5"
    );
}

#[test]
fn a_cancelled_plan_refuses_plus_with_the_cli_message() {
    let fixture = ActionFixture::new("priority-cancelled");
    // Select the plan while it is still in the default view, then cancel it in the store.
    let mut app = fixture.app(&fixture.plan, &fixture.task);
    fixture
        .store
        .change_plan(&fixture.plan, |plan| {
            plan.status = "cancelled".to_owned();
            plan.auto = "off".to_owned();
            Ok(())
        })
        .unwrap();
    app.filter = radian_tui::arc::PlanFilter::All;
    app.refresh();
    app.focus = radian_tui::arc::Focus::Plans;
    app.on_key(&key(KeyCode::Char('+')));
    let action = app.take_action().expect("the step is queued");
    let outcome = actions::run(&action, &fixture.store, &actions::Library);
    assert!(!outcome.ok, "{outcome:?}");
    assert!(
        outcome.text.contains(&format!(
            "plan `{}` is `cancelled`; a cancelled plan is final",
            fixture.plan
        )),
        "{outcome:?}"
    );
    assert_eq!(
        fixture
            .store
            .load_plan(&fixture.plan)
            .unwrap()
            .item
            .priority(),
        "P3",
        "the refusal wrote nothing"
    );
    app.finish_action(outcome);

    // At an end the cancelled plan still reaches the CLI: the footer shows its refusal, not
    // "already P0" or "already P5".
    for (priority, key_code) in [("P0", '+'), ("P5", '-')] {
        fixture
            .store
            .change_plan(&fixture.plan, |plan| {
                plan.priority = Some(priority.to_owned());
                Ok(())
            })
            .unwrap();
        app.refresh();
        app.on_key(&key(KeyCode::Char(key_code)));
        let action = app
            .take_action()
            .unwrap_or_else(|| panic!("{priority} at the end is not queued"));
        assert_eq!(
            action,
            actions::Action::Priority {
                plan: fixture.plan.clone(),
                level: priority.to_owned(),
            }
        );
        let outcome = actions::run(&action, &fixture.store, &actions::Library);
        assert!(!outcome.ok, "{outcome:?}");
        assert!(
            outcome.text.contains(&format!(
                "plan `{}` is `cancelled`; a cancelled plan is final",
                fixture.plan
            )),
            "{priority}: {outcome:?}"
        );
        app.finish_action(outcome);
    }
}

#[test]
fn a_cycles_the_auto_level_and_the_merge_level_asks_first() {
    let fixture = ActionFixture::new("auto");
    // off -> run (no confirmation).
    let mut app = fixture.app(&fixture.plan, &fixture.task);
    app.on_key(&key(KeyCode::Char('a')));
    assert_eq!(
        app.take_action(),
        Some(actions::Action::Auto {
            plan: fixture.plan.clone(),
            level: "run".to_owned(),
        })
    );
    // run -> merge, which asks.
    set_auto(&fixture.store, &fixture.plan, "run");
    let mut app = fixture.app(&fixture.plan, &fixture.task);
    app.on_key(&key(KeyCode::Char('a')));
    assert!(app.take_action().is_none(), "the merge level asks first");
    match &app.float {
        Some(radian_tui::arc::Float::Confirm { message, action }) => {
            assert!(message.contains("auto merge"), "{message}");
            assert_eq!(
                action,
                &actions::Action::Auto {
                    plan: fixture.plan.clone(),
                    level: "merge".to_owned(),
                }
            );
        }
        other => panic!("no confirmation float: {other:?}"),
    }
    // `n` changes nothing.
    app.on_key(&key(KeyCode::Char('n')));
    assert!(app.float.is_none());
    assert!(app.take_action().is_none());
    assert_eq!(auto_level(&fixture.store, &fixture.plan), "run");
    // `y` runs it.
    app.on_key(&key(KeyCode::Char('a')));
    app.on_key(&key(KeyCode::Char('y')));
    assert_eq!(
        app.take_action(),
        Some(actions::Action::Auto {
            plan: fixture.plan.clone(),
            level: "merge".to_owned(),
        })
    );
    // merge -> off needs no confirmation.
    set_auto(&fixture.store, &fixture.plan, "merge");
    let mut app = fixture.app(&fixture.plan, &fixture.task);
    app.on_key(&key(KeyCode::Char('a')));
    assert_eq!(
        app.take_action(),
        Some(actions::Action::Auto {
            plan: fixture.plan.clone(),
            level: "off".to_owned(),
        })
    );
}

fn set_auto(store: &Store, plan: &str, level: &str) {
    radian_arc::store::plan_ops::set_auto(store, plan, level).expect("the level is set");
}

fn auto_level(store: &Store, plan: &str) -> String {
    store.load_plan(plan).unwrap().item.auto
}

#[test]
fn s_names_what_a_task_waits_for_instead_of_starting_it() {
    let fixture = ActionFixture::new("start-blocked");
    let mut app = fixture.app(&fixture.plan, &fixture.blocked);
    app.on_key(&key(KeyCode::Char('s')));
    assert!(
        app.take_action().is_none(),
        "a blocked task never reaches start"
    );
    let line = app.result.as_ref().expect("the footer names the wait");
    assert!(line.text.contains("waits"), "{:?}", line);
    assert!(line.text.contains(&fixture.task), "{:?}", line);
    assert!(!line.failed, "a wait is not an error: {:?}", line);

    // A draft plan's task waits on the confirmation.
    let mut app = fixture.app(&fixture.draft, &fixture.draft_task);
    app.on_key(&key(KeyCode::Char('s')));
    assert!(app.take_action().is_none());
    assert!(
        app.result
            .as_ref()
            .is_some_and(|line| line.text.contains("confirm it")),
        "{:?}",
        app.result
    );

    // A ready task in a confirmed plan starts.
    let mut app = fixture.app(&fixture.plan, &fixture.task);
    assert!(app.start_blocker().is_none(), "{:?}", app.start_blocker());
    app.on_key(&key(KeyCode::Char('s')));
    assert_eq!(
        app.take_action(),
        Some(actions::Action::Start(fixture.task.clone()))
    );
}

#[test]
fn the_merge_key_asks_and_n_changes_nothing_while_y_merges() {
    let fixture = ActionFixture::new("merge");
    let head = fixture.verify_task();
    assert_eq!(fixture.phase(), "review.awaiting_human");
    let main_before = remote_main(&fixture.repo);
    assert_ne!(main_before, head, "the task branch is not main yet");

    let mut app = fixture.app(&fixture.plan, &fixture.task);
    app.open_task();
    assert!(app.open_merge());
    app.on_key(&key(KeyCode::Char('M')));
    match &app.float {
        Some(radian_tui::arc::Float::Confirm { message, action }) => {
            assert_eq!(message, &format!("merge {} onto main?", fixture.task));
            assert_eq!(action, &actions::Action::Merge(fixture.task.clone()));
        }
        other => panic!("no confirmation float: {other:?}"),
    }
    // `n` changes nothing and runs nothing.
    app.on_key(&key(KeyCode::Char('n')));
    assert!(app.float.is_none());
    assert!(app.take_action().is_none());
    assert_eq!(fixture.phase(), "review.awaiting_human");
    assert_eq!(remote_main(&fixture.repo), main_before, "main did not move");

    // `y` runs the library's merge, in this process.
    app.on_key(&key(KeyCode::Char('M')));
    app.on_key(&key(KeyCode::Char('y')));
    let action = app.take_action().expect("the merge is queued");
    assert_eq!(action, actions::Action::Merge(fixture.task.clone()));
    let outcome = actions::run(&action, &fixture.store, &actions::Library);
    assert!(outcome.ok, "{outcome:?}");
    assert!(
        outcome
            .text
            .starts_with(&format!("{} done.merged", fixture.task)),
        "{outcome:?}"
    );
    app.finish_action(outcome);
    assert!(app.pending.is_none());
    let line = &app.result.as_ref().expect("the result line").text;
    assert!(line.contains("done.merged"), "{line}");
    assert_eq!(fixture.phase(), "done.merged");
    assert_eq!(remote_main(&fixture.repo), head, "the merge moved main");
}

#[test]
fn the_merge_action_calls_the_library_not_a_subprocess() {
    let fixture = ActionFixture::new("merge-seam");
    let recorder = Recorder::default();
    let outcome = actions::run(
        &actions::Action::Merge(fixture.task.clone()),
        &fixture.store,
        &recorder,
    );
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(
        outcome.text,
        format!("{} done.merged (abcdef12)", fixture.task),
        "the line is the library result's"
    );
    assert_eq!(
        recorder.calls.borrow().as_slice(),
        [format!("merge {}", fixture.task)],
        "the action reached the merge method and started nothing"
    );
}

/// A recording [`actions::ArcOps`]: a merge that shelled out would leave the record empty.
#[derive(Default)]
struct Recorder {
    calls: RefCell<Vec<String>>,
}

impl actions::ArcOps for Recorder {
    fn confirm(
        &self,
        _store: &Store,
        id: &str,
    ) -> Result<radian_arc::store::plan_ops::Confirmed, radian_arc::store::Error> {
        self.calls.borrow_mut().push(format!("confirm {id}"));
        Ok(radian_arc::store::plan_ops::Confirmed::Plan(id.to_owned()))
    }

    fn set_auto(
        &self,
        _store: &Store,
        _plan: &str,
        _level: &str,
    ) -> Result<(), radian_arc::store::Error> {
        unreachable!("the seam test only merges")
    }

    fn cancel(
        &self,
        _store: &Store,
        _id: &str,
    ) -> Result<radian_arc::attempt::Cancelled, radian_arc::store::Error> {
        unreachable!("the seam test only merges")
    }

    fn set_priority(
        &self,
        _store: &Store,
        _plan: &str,
        _level: &str,
    ) -> Result<(), radian_arc::store::Error> {
        unreachable!("the seam test only merges")
    }

    fn pause(&self, _store: &Store, _plan: &str) -> Result<(), radian_arc::store::Error> {
        unreachable!("the seam test only merges")
    }

    fn start(
        &self,
        _store: &Store,
        _task: &str,
    ) -> Result<radian_arc::attempt::Started, radian_arc::store::Error> {
        unreachable!("the seam test only merges")
    }

    fn merge(
        &self,
        _store: &Store,
        task: &str,
    ) -> Result<radian_arc::review::Merged, radian_arc::store::Error> {
        self.calls.borrow_mut().push(format!("merge {task}"));
        Ok(radian_arc::review::Merged {
            task: task.to_owned(),
            state: "done",
            phase: "merged".to_owned(),
            reason: None,
            candidate_sha: Some("abcdef12".to_owned()),
            rounds: 1,
            warning: None,
            conflict: None,
            followup: None,
        })
    }

    fn reject(
        &self,
        _store: &Store,
        _task: &str,
        _note: &str,
    ) -> Result<radian_arc::review::Rejected, radian_arc::store::Error> {
        unreachable!("the seam test only merges")
    }

    fn reverify(
        &self,
        _store: &Store,
        _task: &str,
    ) -> Result<radian_arc::review::Reverified, radian_arc::store::Error> {
        unreachable!("the seam test only merges")
    }
}

#[test]
fn reject_refuses_an_empty_note_and_takes_a_one_line_one() {
    let fixture = ActionFixture::new("reject");
    fixture.verify_task();
    let mut app = fixture.app(&fixture.plan, &fixture.task);
    app.open_task();
    assert!(app.open_merge());
    app.on_key(&key(KeyCode::Char('r')));
    assert!(matches!(
        app.float,
        Some(radian_tui::arc::Float::Reject { .. })
    ));
    // Enter with an empty note is the CLI's refusal; the float stays open.
    app.on_key(&key(KeyCode::Enter));
    assert!(app.take_action().is_none());
    assert!(
        app.result
            .as_ref()
            .is_some_and(|line| line.text == "the reject note is empty" && line.failed),
        "{:?}",
        app.result
    );
    assert!(matches!(
        app.float,
        Some(radian_tui::arc::Float::Reject { .. })
    ));

    for ch in "fix the lock order".chars() {
        app.on_key(&key(KeyCode::Char(ch)));
    }
    app.on_key(&key(KeyCode::Enter));
    let action = app.take_action().expect("the note is taken");
    assert_eq!(
        action,
        actions::Action::Reject {
            task: fixture.task.clone(),
            note: "fix the lock order".to_owned(),
        }
    );
    let outcome = actions::run(&action, &fixture.store, &actions::Library);
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(
        outcome.text,
        format!("{} review.rework_pending", fixture.task)
    );
    assert_eq!(fixture.phase(), "review.rework_pending");
    let note = fixture
        .store
        .root()
        .join("runs")
        .join(&fixture.task)
        .join("attempts")
        .join(format!("{}-1", fixture.task))
        .join("reject.md");
    assert_eq!(
        fs::read_to_string(note).unwrap().trim(),
        "fix the lock order",
        "the note is recorded"
    );
}

#[test]
fn a_held_state_lock_shows_the_cli_message_and_changes_nothing() {
    let fixture = ActionFixture::new("locked");
    let _held = fixture
        .store
        .lock_state()
        .expect("the test holds state.lock");
    let outcome = actions::run(
        &actions::Action::Confirm(fixture.draft.clone()),
        &fixture.store,
        &actions::Library,
    );
    assert!(!outcome.ok);
    assert!(
        outcome
            .text
            .contains("state.lock is locked by another process"),
        "{outcome:?}"
    );
    assert!(outcome.text.contains("(exit 4)"), "{outcome:?}");
    assert_eq!(
        fixture.store.load_plan(&fixture.draft).unwrap().item.status,
        "draft",
        "nothing changed"
    );

    // The same message and exit code for the task cancel and the priority step.
    let outcome = actions::run(
        &actions::Action::Cancel(fixture.task.clone()),
        &fixture.store,
        &actions::Library,
    );
    assert!(!outcome.ok, "{outcome:?}");
    assert!(
        outcome
            .text
            .contains("state.lock is locked by another process"),
        "{outcome:?}"
    );
    assert!(outcome.text.contains("(exit 4)"), "{outcome:?}");
    assert_eq!(fixture.phase(), "todo.ready", "the task did not change");

    let outcome = actions::run(
        &actions::Action::Priority {
            plan: fixture.plan.clone(),
            level: "P1".to_owned(),
        },
        &fixture.store,
        &actions::Library,
    );
    assert!(!outcome.ok, "{outcome:?}");
    assert!(
        outcome
            .text
            .contains("state.lock is locked by another process"),
        "{outcome:?}"
    );
    assert!(outcome.text.contains("(exit 4)"), "{outcome:?}");
    assert!(
        !fixture
            .store
            .load_plan(&fixture.plan)
            .unwrap()
            .item
            .priority
            .is_some(),
        "the priority did not change"
    );
}

#[test]
fn the_merge_page_opens_only_for_review_or_done() {
    let fixture = ActionFixture::new("open");
    let mut app = fixture.app(&fixture.plan, &fixture.task);
    app.open_task();
    assert!(!app.open_merge(), "a todo task has no merge page");
    assert!(
        app.result.as_ref().is_some_and(|line| line
            .text
            .contains("the merge page opens for a task in review or done")),
        "{:?}",
        app.result
    );

    fixture
        .store
        .change_task(&fixture.task, |task| {
            task.state = TaskState::Review;
            task.phase = "awaiting_human".to_owned();
            Ok(())
        })
        .unwrap();
    assert!(app.open_merge(), "a review task opens the merge page");
}

#[test]
fn enter_on_a_command_row_opens_its_log_and_esc_closes_it() {
    let fixture = Fixture::new("log", &["commands", "human"]);
    fs::write(fixture.repo.join("fmt.log"), "fmt output\nall clean\n").unwrap();
    fs::write(fixture.repo.join("clippy.log"), "clippy output\n").unwrap();
    fs::write(fixture.repo.join("test.log"), "test output\n").unwrap();
    fixture.write(&mut fixture.commands(true, None));
    let mut app = fixture.app();
    // The first command row is selected; Down moves to the second.
    app.on_key(&key(KeyCode::Down));
    assert_eq!(app.merge.as_ref().unwrap().command_sel, 1);
    // The read is queued for a worker thread: the frame never waits on the log.
    app.on_key(&key(KeyCode::Enter));
    let request = app
        .take_log_request()
        .expect("the log is read off the UI thread");
    assert_eq!(request.path, fixture.repo.join("clippy.log"));
    let text = radian_tui::arc::read_log_tail(&request.path).expect("the log reads");
    assert!(app.apply_log(&request, Ok(text)));
    let text = screen(&mut app, 120, 40);
    assert!(
        text.contains("clippy output"),
        "the selected command's log is not drawn:\n{text}"
    );
    assert!(
        text.contains("cargo clippy --workspace --all-targets -- -D warnings"),
        "the float is titled with the command line:\n{text}"
    );
    // Esc closes the log and stays on the merge page.
    app.on_key(&key(KeyCode::Esc));
    assert!(app.merge.as_ref().unwrap().log.is_none());
    assert_eq!(app.page, radian_tui::arc::Page::Merge);
}

#[test]
fn l_closes_the_command_log_and_opens_the_loop_log() {
    let fixture = Fixture::new("log-key", &["commands", "human"]);
    fs::write(fixture.repo.join("fmt.log"), "fmt output\n").unwrap();
    fs::write(fixture.repo.join("clippy.log"), "clippy output\n").unwrap();
    fs::write(fixture.repo.join("test.log"), "test output\n").unwrap();
    fixture.write(&mut fixture.commands(true, None));
    let mut app = fixture.app();
    app.on_key(&key(KeyCode::Enter));
    let request = app.take_log_request().expect("the log request");
    let text = radian_tui::arc::read_log_tail(&request.path).expect("the log reads");
    assert!(app.apply_log(&request, Ok(text)));
    assert!(app.merge.as_ref().unwrap().log.is_some());

    // `L` opens the loop's log above the page; the command log closes, because the command
    // log frame draws above the loop log and would hide the view `L` opened.
    assert_eq!(app.on_key(&key(KeyCode::Char('L'))), ArcKey::Redraw);
    assert!(app.log_open);
    assert!(app.merge.as_ref().unwrap().log.is_none());
    let text = screen(&mut app, 120, 40);
    assert!(text.contains("╭─ log"), "the loop log is drawn:\n{text}");
    assert!(text.contains("no loop log yet"), "{text}");
    assert!(
        !text.contains("fmt output"),
        "the command log still covers the loop log:\n{text}"
    );
    // A second `L` closes the loop log; the command log stays closed.
    assert_eq!(app.on_key(&key(KeyCode::Char('L'))), ArcKey::Redraw);
    assert!(!app.log_open);
    assert!(app.merge.as_ref().unwrap().log.is_none());

    // A command log still being read when `L` is pressed closes too: its late reply does not
    // reopen it above the loop log.
    app.on_key(&key(KeyCode::Enter));
    let late = app.take_log_request().expect("the log request");
    app.on_key(&key(KeyCode::Char('L')));
    assert!(!app.apply_log(&late, Ok("late output\n".to_owned())));
    assert!(app.merge.as_ref().unwrap().log.is_none());
    let text = screen(&mut app, 120, 40);
    assert!(text.contains("╭─ log"), "{text}");
    assert!(!text.contains("late output"), "{text}");
}

#[test]
fn enter_on_a_command_row_opens_its_log_in_a_scrollable_frame() {
    let fixture = Fixture::new("log-scroll", &["commands", "human"]);
    let long: String = (1..=40).map(|i| format!("log line {i}\n")).collect();
    fs::write(fixture.repo.join("fmt.log"), long).unwrap();
    fs::write(fixture.repo.join("clippy.log"), "clippy\n").unwrap();
    fs::write(fixture.repo.join("test.log"), "test\n").unwrap();
    fixture.write(&mut fixture.commands(true, None));
    let mut app = fixture.app();
    app.on_key(&key(KeyCode::Enter));
    let request = app.take_log_request().expect("the log request");
    let text = radian_tui::arc::read_log_tail(&request.path).expect("the log reads");
    assert!(app.apply_log(&request, Ok(text)));
    let log = app
        .merge
        .as_ref()
        .unwrap()
        .log
        .as_ref()
        .expect("the log opens");
    assert_eq!(log.lines.len(), 40);
    assert_eq!(log.scroll, 0);
    app.on_key(&key(KeyCode::Char('j')));
    app.on_key(&key(KeyCode::Char('j')));
    assert_eq!(app.merge.as_ref().unwrap().log.as_ref().unwrap().scroll, 2);
    app.on_key(&key(KeyCode::Char('f')));
    assert_eq!(
        app.merge.as_ref().unwrap().log.as_ref().unwrap().scroll,
        39,
        "f follows the end"
    );
    app.on_key(&key(KeyCode::Esc));
    assert!(app.merge.as_ref().unwrap().log.is_none());
}

#[test]
fn one_log_is_in_flight_and_a_stale_reply_is_dropped() {
    let fixture = Fixture::new("log-pending", &["commands", "human"]);
    fs::write(fixture.repo.join("fmt.log"), "fmt output\n").unwrap();
    fs::write(fixture.repo.join("clippy.log"), "clippy output\n").unwrap();
    fs::write(fixture.repo.join("test.log"), "test output\n").unwrap();
    fixture.write(&mut fixture.commands(true, None));
    let mut app = fixture.app();
    app.on_key(&key(KeyCode::Enter));
    let first = app.take_log_request().expect("the first log is queued");
    assert_eq!(first.path, fixture.repo.join("fmt.log"));
    // A second Enter while the read is in flight queues nothing.
    app.on_key(&key(KeyCode::Down));
    app.on_key(&key(KeyCode::Enter));
    assert!(
        app.take_log_request().is_none(),
        "a second read starts while one is in flight"
    );
    // A reply for a log nobody is waiting for opens nothing.
    let stale = radian_tui::arc::LogRequest {
        task: first.task.clone(),
        title: "cargo nextest run --workspace".to_owned(),
        path: fixture.repo.join("test.log"),
    };
    assert!(!app.apply_log(&stale, Ok("test output\n".to_owned())));
    assert!(
        app.merge.as_ref().unwrap().log.is_none(),
        "a stale reply opens nothing"
    );
    // The pending reply opens its float, titled with the command it belongs to.
    let text = radian_tui::arc::read_log_tail(&first.path).expect("the log reads");
    assert!(app.apply_log(&first, Ok(text)));
    let log = app
        .merge
        .as_ref()
        .unwrap()
        .log
        .as_ref()
        .expect("the pending log opens");
    assert_eq!(log.title, "cargo fmt --all -- --check");
    // The reply clears the guard, so the next Enter queues another read.
    app.on_key(&key(KeyCode::Esc));
    app.on_key(&key(KeyCode::Down));
    app.on_key(&key(KeyCode::Enter));
    assert!(
        app.take_log_request().is_some(),
        "the reply clears the in-flight guard"
    );
}

#[test]
fn a_log_is_read_from_its_end_so_a_giant_one_is_bounded() {
    let scratch = Scratch::new("log-tail");
    let path = scratch.path("big.log");
    let mut text = String::new();
    let mut n = 0;
    while (text.len() as u64) < radian_tui::arc::LOG_BYTES * 2 {
        n += 1;
        text.push_str(&format!("line {n}: the gate printed this\n"));
    }
    fs::write(&path, &text).unwrap();
    let tail = radian_tui::arc::read_log_tail(&path).expect("the tail reads");
    assert!(
        tail.len() as u64 <= radian_tui::arc::LOG_BYTES,
        "the tail is bounded: {} bytes",
        tail.len()
    );
    assert!(
        tail.starts_with("line "),
        "the cut line is dropped: {:?}",
        &tail[..20.min(tail.len())]
    );
    assert!(tail.ends_with(&format!("line {n}: the gate printed this\n")));
    assert_eq!(
        radian_tui::arc::LogView::new("big", &tail).lines.len(),
        radian_tui::arc::LOG_LINES,
        "the float keeps only its last lines"
    );
}

#[test]
fn a_log_cut_exactly_on_a_line_start_keeps_that_whole_line() {
    let scratch = Scratch::new("log-line-start");
    let path = scratch.path("lines.log");
    // Every line is exactly 64 bytes, so the file is `head line\n` plus exactly `LOG_BYTES`:
    // the cut (`len - LOG_BYTES`) lands on the newline and the first tail line is whole.
    let mut tail = String::new();
    let mut n = 0;
    while (tail.len() as u64) < radian_tui::arc::LOG_BYTES {
        n += 1;
        let label = format!("{n}:");
        tail.push_str(&label);
        tail.push_str(&"x".repeat(64 - label.len() - 1));
        tail.push('\n');
    }
    fs::write(&path, format!("head line\n{tail}")).unwrap();
    let text = radian_tui::arc::read_log_tail(&path).expect("the tail reads");
    assert_eq!(text, tail, "a cut on a line start keeps that line");
    assert!(
        text.starts_with("1:"),
        "the first whole line of the tail is kept: {:?}",
        &text[..8]
    );
}

#[test]
fn a_q_in_the_reject_note_is_typed_and_does_not_quit() {
    let fixture = Fixture::new("q-note", &["commands", "human"]);
    fixture.write(&mut fixture.commands(true, None));
    let mut app = fixture.app();
    app.on_key(&key(KeyCode::Char('r')));
    for ch in "fix quickly".chars() {
        assert_eq!(app.on_key(&key(KeyCode::Char(ch))), ArcKey::Redraw);
    }
    assert_eq!(
        app.float,
        Some(radian_tui::arc::Float::Reject {
            note: "fix quickly".to_owned()
        })
    );
    // Esc cancels the note without leaving the TUI.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert!(app.float.is_none());
}

#[test]
fn a_candidate_that_differs_is_named_over_main() {
    let fixture = Fixture::new("candidate", &["commands", "human"]);
    let mut verdict = fixture.commands(true, None);
    verdict.main_sha = Some(fixture.base.clone());
    verdict.candidate_sha = Some("f".repeat(40));
    fixture.write(&mut verdict);
    let mut app = fixture.app();
    let text = screen(&mut app, 140, 40);
    let candidate_sha = "f".repeat(40);
    let candidate = short(&candidate_sha);
    let main = short(&fixture.base);
    assert!(
        text.contains(&format!("→ candidate {candidate} over main {main}")),
        "the candidate and the main it merged are named:\n{text}"
    );
}

#[test]
fn the_running_gates_stage_is_a_running_mark() {
    let fixture = Fixture::new("running", &["commands", "human"]);
    let mut app = ArcApp::with_session_dir(fixture.store.clone(), None);
    app.plan_sel = app
        .plans
        .iter()
        .position(|view| view.plan.item.id == fixture.plan)
        .unwrap();
    let view = app.selected_plan().unwrap();
    app.task_sel = app
        .task_rows(view)
        .iter()
        .position(|row| row.id == fixture.task)
        .unwrap();
    app.open_task();
    assert!(app.open_merge());
    let view = read::merge_view(&fixture.store, &fixture.task, true).unwrap();
    app.apply_merge(&fixture.task, Ok(view));
    let text = screen(&mut app, 120, 40);
    assert!(text.contains("● commands"), "{text}");
    assert!(
        text.contains("◻ human       not run yet"),
        "the owner's stage never runs in a gate:\n{text}"
    );
}

#[test]
fn m_on_the_task_page_opens_the_merge_page() {
    let fixture = Fixture::new("m-key", &["commands", "human"]);
    let mut app = ArcApp::with_session_dir(fixture.store.clone(), None);
    app.plan_sel = app
        .plans
        .iter()
        .position(|view| view.plan.item.id == fixture.plan)
        .unwrap();
    let view = app.selected_plan().unwrap();
    app.task_sel = app
        .task_rows(view)
        .iter()
        .position(|row| row.id == fixture.task)
        .unwrap();
    app.open_task();
    assert_eq!(app.on_key(&key(KeyCode::Char('m'))), ArcKey::Redraw);
    assert_eq!(app.page, radian_tui::arc::Page::Merge);
    // The runner would now ask for the view; the page draws its loading line until it lands.
    assert_eq!(app.merge_request(), Some(fixture.task.clone()));
    app.mark_merge_loading();
    assert_eq!(app.merge_request(), None);
    let view = read::merge_view(&fixture.store, &fixture.task, false).unwrap();
    assert!(app.apply_merge(&fixture.task, Ok(view)));
    let text = screen(&mut app, 120, 30);
    assert!(text.contains("verified"), "{text}");
    // Esc closes the log first; with no log it goes back to the task page.
    app.on_key(&key(KeyCode::Esc));
    assert_eq!(app.page, radian_tui::arc::Page::Task);
}

// ---- the action's result line --------------------------------------------------------

/// The result line is the CLI's own text; a failure carries the exit reason.
#[test]
fn an_action_error_keeps_the_cli_message_and_the_exit_code() {
    let fixture = ActionFixture::new("error");
    // A confirmed plan is not confirmed again (exit 3).
    let outcome = actions::run(
        &actions::Action::Confirm(fixture.plan.clone()),
        &fixture.store,
        &actions::Library,
    );
    assert!(!outcome.ok);
    assert!(outcome.text.contains("already `confirmed`"), "{outcome:?}");
    assert!(outcome.text.contains("(exit 3)"), "{outcome:?}");
}

/// The fixture's JSON is the same shape the verdict writer uses (`json!` keeps the fields
/// explicit here so a change to the verdict's fields is caught).
#[test]
fn the_fixture_verdict_reads_back() {
    let fixture = Fixture::new("verdict", &["commands", "human"]);
    fixture.write(&mut fixture.commands(true, None));
    let path = verdict_path(
        &fixture.repo,
        &fixture.task,
        &fixture.label(),
        Stage::Commands,
    )
    .unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(value["stage"], json!("commands"));
    assert_eq!(
        value["commands"][0]["line"],
        json!("cargo fmt --all -- --check")
    );
}
