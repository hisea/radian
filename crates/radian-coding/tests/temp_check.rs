//! The temp file system check of `scripts/temp-check.sh`, which `verify.sh` and
//! `arc-gate.sh` run before their first step.
//!
//! A stub `df` first on `PATH` returns the two outputs a test names, so the
//! check never reads the real file system. Each expectation from the brief has
//! a test: plenty free passes; low inodes or low space fail with the line; the
//! macOS and GNU `df -i` shapes both parse, including the very large `ifree`
//! APFS reports; an inode count of `0`, `-` or one too large for the shell skips
//! the inode check; a negative free space fails the space check; each limit can
//! be overridden and `0` turns a check off; a limit past the documented
//! 18-digit bound is refused.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// The checkout's `scripts/temp-check.sh`.
fn helper() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/temp-check.sh")
}

/// The checkout's `scripts/`.
fn scripts() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts")
}

/// A temp directory with a `bin/df` stub, removed when the test ends.
struct FakeDf {
    dir: PathBuf,
}

impl FakeDf {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-temp-check-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).expect("the stub dir is creatable");
        let stub = dir.join("bin/df");
        std::fs::write(
            &stub,
            "#!/bin/sh\ncase \" $* \" in\n  *\" -i \"*) printf '%s\\n' \"$FAKE_DF_INODES\" ;;\n  *) printf '%s\\n' \"$FAKE_DF_SPACE\" ;;\nesac\n",
        )
        .expect("the df stub is writable");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
                .expect("the df stub is executable");
        }
        // A no-op cargo records its calls: if the check ever fails to stop
        // the script, the test must not start a real build (or a recursive
        // Verify), and the empty record proves no step started.
        let cargo = dir.join("bin/cargo");
        std::fs::write(
            &cargo,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CARGO_CALLS\"\nexit 0\n",
        )
        .expect("the cargo stub is writable");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755))
                .expect("the cargo stub is executable");
        }
        Self { dir }
    }

    fn root(&self) -> &Path {
        &self.dir
    }

    /// `PATH` with this stub's `bin` first.
    fn path_env(&self) -> std::ffi::OsString {
        let mut path = self.dir.join("bin").into_os_string();
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        path
    }
}

impl Drop for FakeDf {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[derive(Debug)]
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Run the check with the stub's outputs and the named limit overrides. Both
/// limit variables always start unset, so the caller's environment never leaks
/// into a test.
fn check(df: &FakeDf, inodes: &str, space: &str, vars: &[(&str, &str)]) -> Run {
    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg("source \"$RADIAN_TEMP_CHECK_HELPER\"\nradian_check_temp_fs verify")
        .env("RADIAN_TEMP_CHECK_HELPER", helper())
        .env("TMPDIR", df.root())
        .env("PATH", df.path_env())
        .env("FAKE_DF_INODES", inodes)
        .env("FAKE_DF_SPACE", space)
        .env_remove("RADIAN_VERIFY_MIN_FREE_INODES")
        .env_remove("RADIAN_VERIFY_MIN_FREE_GIB");
    for (name, value) in vars {
        command.env(name, value);
    }
    let output = command.output().expect("bash runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

const GNU_INODES: &str =
    "Filesystem Inodes IUsed IFree IUse% Mounted on\n/dev/sda1 1000000 1000 999000 1% /tmp";
const MACOS_INODES: &str = "Filesystem 1024-blocks Used Available Capacity iused ifree %iused Mounted on\n/dev/disk3s5 971350180 740000000 196400000 80% 990 10 1% /System/Volumes/Data";
/// APFS reports `ifree` as 2^63 - 1; BSD awk prints that sum as
/// 9223372036854775808, one past bash's intmax, so it is no usable count.
const MACOS_HUGE_INODES: &str = "Filesystem 1024-blocks Used Available Capacity iused ifree %iused Mounted on\n/dev/disk3s5 971350180 740000000 196400000 80% 471663 9223372036854775807 0% /System/Volumes/Data";
/// Some macOS volumes report `-` instead of a free inode count.
const MACOS_DASH_INODES: &str = "Filesystem 1024-blocks Used Available Capacity iused ifree %iused Mounted on\n/dev/disk3s5 971350180 740000000 196400000 80% 471663 - 0% /System/Volumes/Data";
/// 4 GiB free.
const PLENTY_SPACE: &str = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100000000 1000 4194304 1% /tmp";
/// 1 MiB free.
const LOW_SPACE: &str = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100000000 1000 1024 1% /tmp";

fn assert_failed(run: &Run, needle: &str) {
    assert_eq!(run.code, Some(1), "stderr: {}", run.stderr);
    assert_eq!(run.stdout, "", "the check writes nothing to stdout");
    assert!(run.stderr.starts_with("verify: "), "{}", run.stderr);
    assert!(
        run.stderr.contains(needle),
        "wanted {needle:?} in {:?}",
        run.stderr
    );
    assert!(
        run.stderr.contains("remove leaked test directories first"),
        "the line says what to do: {}",
        run.stderr
    );
}

#[test]
fn plenty_free_passes() {
    let df = FakeDf::new("plenty");
    let run = check(&df, GNU_INODES, PLENTY_SPACE, &[]);
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert_eq!(run.stderr, "", "a passing check is silent");
}

/// The line the brief asks for: path, free and total inodes, free space, both
/// limits, and what to do.
#[test]
fn low_inodes_fail_with_the_line() {
    let df = FakeDf::new("low-inodes");
    let inodes = "Filesystem Inodes IUsed IFree IUse% Mounted on\n/dev/sda1 1000 990 10 99% /tmp";
    let run = check(&df, inodes, PLENTY_SPACE, &[]);
    assert_failed(
        &run,
        &format!(
            "{} has 10 of 1000 inodes free (limit 50000) and 4 GiB free (limit 2 GiB); remove leaked test directories first",
            df.root().display()
        ),
    );
    assert_eq!(run.stderr.lines().count(), 1, "one line: {}", run.stderr);
}

#[test]
fn low_space_fails_with_the_line() {
    let df = FakeDf::new("low-space");
    let run = check(&df, GNU_INODES, LOW_SPACE, &[]);
    assert_failed(
        &run,
        &format!(
            "{} has 999000 of 1000000 inodes free (limit 50000) and 0 GiB free (limit 2 GiB)",
            df.root().display()
        ),
    );
}

/// macOS `df -P -k -i` has no `Inodes` total column; its total is `iused +
/// ifree`. GNU has `Inodes`. The same numbers through both shapes must fail
/// with the same parsed values. A volume that reports no usable count (`-`, or
/// the intmax-sized `ifree` APFS reports) skips the inode check quietly.
#[test]
fn macos_and_gnu_inode_output_both_parse() {
    let df = FakeDf::new("shapes");
    let macos = check(&df, MACOS_INODES, PLENTY_SPACE, &[]);
    let gnu = check(&df, GNU_INODES, PLENTY_SPACE, &[]);
    assert_failed(&macos, "has 10 of 1000 inodes free (limit 50000)");
    assert_eq!(
        gnu.code,
        Some(0),
        "GNU's 999000 free passes: {}",
        gnu.stderr
    );

    for (label, inodes) in [("huge", MACOS_HUGE_INODES), ("dash", MACOS_DASH_INODES)] {
        let run = check(&df, inodes, PLENTY_SPACE, &[]);
        assert_eq!(
            run.code,
            Some(0),
            "{label}: no usable inode count skips the check: {}",
            run.stderr
        );
        assert_eq!(run.stderr, "", "{label}: the skip is quiet");
    }
}

/// A file system that reports no inode count keeps the space check and skips
/// the inode check.
#[test]
fn a_zero_inode_count_skips_the_inode_check() {
    let df = FakeDf::new("zero-inodes");
    let zero = "Filesystem Inodes IUsed IFree IUse% Mounted on\n/dev/sda1 0 0 0 - /tmp";
    let pass = check(&df, zero, PLENTY_SPACE, &[]);
    assert_eq!(pass.code, Some(0), "space is fine: {}", pass.stderr);

    let run = check(&df, zero, LOW_SPACE, &[]);
    assert_failed(&run, "has no inode count and 0 GiB free (limit 2 GiB)");
    assert!(
        !run.stderr.contains("inodes free"),
        "the inode check is skipped: {}",
        run.stderr
    );
}

/// `RADIAN_VERIFY_MIN_FREE_INODES` overrides the default, and `0` turns the
/// inode check off.
#[test]
fn the_inode_limit_can_be_overridden_and_zero_turns_it_off() {
    let df = FakeDf::new("inode-override");
    let inodes = "Filesystem Inodes IUsed IFree IUse% Mounted on\n/dev/sda1 1000 990 10 99% /tmp";
    let default = check(&df, inodes, PLENTY_SPACE, &[]);
    assert_eq!(default.code, Some(1), "10 is below the default 50000");

    for limit in ["5", "10"] {
        let run = check(
            &df,
            inodes,
            PLENTY_SPACE,
            &[("RADIAN_VERIFY_MIN_FREE_INODES", limit)],
        );
        assert_eq!(
            run.code,
            Some(0),
            "10 free is not fewer than {limit}: {}",
            run.stderr
        );
    }

    let raised = check(
        &df,
        inodes,
        PLENTY_SPACE,
        &[("RADIAN_VERIFY_MIN_FREE_INODES", "11")],
    );
    assert_failed(&raised, "has 10 of 1000 inodes free (limit 11)");

    let off = check(
        &df,
        inodes,
        PLENTY_SPACE,
        &[("RADIAN_VERIFY_MIN_FREE_INODES", "0")],
    );
    assert_eq!(off.code, Some(0), "0 turns the inode check off");
}

/// `RADIAN_VERIFY_MIN_FREE_GIB` overrides the default, and `0` turns the space
/// check off.
#[test]
fn the_space_limit_can_be_overridden_and_zero_turns_it_off() {
    let df = FakeDf::new("space-override");
    let one_gib = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100000000 1000 1048576 1% /tmp";
    let default = check(&df, GNU_INODES, one_gib, &[]);
    assert_eq!(default.code, Some(1), "1 GiB is below the default 2 GiB");

    let overridden = check(
        &df,
        GNU_INODES,
        one_gib,
        &[("RADIAN_VERIFY_MIN_FREE_GIB", "1")],
    );
    assert_eq!(
        overridden.code,
        Some(0),
        "1 GiB is not fewer than 1 GiB: {}",
        overridden.stderr
    );

    let raised = check(
        &df,
        GNU_INODES,
        PLENTY_SPACE,
        &[("RADIAN_VERIFY_MIN_FREE_GIB", "5")],
    );
    assert_failed(
        &raised,
        "has 999000 of 1000000 inodes free (limit 50000) and 4 GiB free (limit 5 GiB)",
    );

    let off = check(
        &df,
        GNU_INODES,
        LOW_SPACE,
        &[("RADIAN_VERIFY_MIN_FREE_GIB", "0")],
    );
    assert_eq!(off.code, Some(0), "0 turns the space check off");
}

/// A value that is not a whole number is a machine-configuration mistake, not
/// a silent fallback to the default.
#[test]
fn a_non_numeric_limit_fails_the_check() {
    let df = FakeDf::new("invalid");
    let run = check(
        &df,
        GNU_INODES,
        PLENTY_SPACE,
        &[("RADIAN_VERIFY_MIN_FREE_INODES", "lots")],
    );
    assert_eq!(run.code, Some(1), "stderr: {}", run.stderr);
    assert!(
        run.stderr.contains("RADIAN_VERIFY_MIN_FREE_INODES") && run.stderr.contains("whole number"),
        "{}",
        run.stderr
    );
}

/// GNU `df` prints a negative Available on ext4 when fewer bytes are free than
/// the reserved-block count. That is a real value: the space check must fail on
/// it, not skip with a "df failed" warning.
#[test]
fn a_negative_free_space_fails_the_space_check() {
    let df = FakeDf::new("negative-space");
    let negative = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100000000 100000001 -5000000 101% /tmp";
    let run = check(&df, GNU_INODES, negative, &[]);
    assert_failed(
        &run,
        "has 999000 of 1000000 inodes free (limit 50000) and -4 GiB free (limit 2 GiB)",
    );
    assert!(
        !run.stderr.contains("cannot read"),
        "a negative Available is a real value: {}",
        run.stderr
    );
}

/// A limit above the documented 18-digit bound is refused: bash arithmetic
/// would wrap it, and `gib_limit * 1024 * 1024` would overflow to a negative
/// threshold that sails past the space check.
#[test]
fn a_limit_past_the_bound_fails_the_check() {
    let df = FakeDf::new("limit-bound");
    for name in [
        "RADIAN_VERIFY_MIN_FREE_INODES",
        "RADIAN_VERIFY_MIN_FREE_GIB",
    ] {
        let run = check(
            &df,
            GNU_INODES,
            PLENTY_SPACE,
            &[(name, "9999999999999999999")],
        );
        assert_eq!(run.code, Some(1), "{name}: stderr: {}", run.stderr);
        assert!(
            run.stderr.contains(name) && run.stderr.contains("999999999999999999"),
            "{name} names the variable and the bound: {}",
            run.stderr
        );
        assert!(
            !run.stderr.contains("integer expression expected"),
            "{name}: the check must not overflow: {}",
            run.stderr
        );
    }
}

/// Both scripts run the check before any step: with low inodes the real
/// `verify.sh` and `arc-gate.sh quick` exit `1` without starting a build.
#[test]
fn both_scripts_check_before_their_first_step() {
    let df = FakeDf::new("wiring");
    let inodes = "Filesystem Inodes IUsed IFree IUse% Mounted on\n/dev/sda1 1000 990 10 99% /tmp";
    let calls = df.root().join("cargo-calls.txt");
    for (script, args, prefix) in [
        ("verify.sh", Vec::<&str>::new(), "verify: "),
        ("arc-gate.sh", vec!["quick"], "arc-gate.sh: "),
    ] {
        let output = Command::new("bash")
            .arg(scripts().join(script))
            .args(&args)
            .current_dir(scripts().join(".."))
            .env("PATH", df.path_env())
            .env("TMPDIR", df.root())
            .env("FAKE_DF_INODES", inodes)
            .env("FAKE_DF_SPACE", PLENTY_SPACE)
            .env("CARGO_CALLS", &calls)
            .env_remove("RADIAN_VERIFY_MIN_FREE_INODES")
            .env_remove("RADIAN_VERIFY_MIN_FREE_GIB")
            .output()
            .expect("bash runs");
        assert_eq!(
            output.status.code(),
            Some(1),
            "{script} exits 1: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!(
                "{prefix}{} has 10 of 1000 inodes free (limit 50000)",
                df.root().display()
            )),
            "{script} prints the check's line: {stderr}"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout, "", "{script} builds nothing after a failed check");
    }
    assert!(
        !calls.exists() || std::fs::read_to_string(&calls).unwrap().is_empty(),
        "no cargo step ran after a failed check: {calls:?}"
    );
}
