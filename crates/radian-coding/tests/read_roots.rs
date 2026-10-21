//! ADR 0039: `read`, `grep`, `glob` and `list` may also read the cargo registry
//! sources and every toolchain's `lib/rustlib/src`, read-only, through the same fd walk
//! (`O_NOFOLLOW` at every level). `write`, `patch` and `shell`'s working directory do not.
//!
//! `CARGO_HOME`, `RUSTUP_HOME` and `HOME` are process-wide, so this binary sets them **once**
//! to a private tree under the system temp dir and every test shares it; the developer's real
//! `~/.cargo` and `~/.rustup` are never read. The tools are built with `CodingTools::new`,
//! which resolves the roots from that environment — the production path.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use common::{call, text};
use radian_coding::CodingTools;
use serde_json::json;

struct Fixture {
    /// An empty workspace (the tools still need one).
    workspace: PathBuf,
    /// `<base>/.cargo/registry/src`.
    registry: PathBuf,
    /// `<base>/.cargo/registry/src/crate-1.0.0/src/lib.rs`.
    crate_file: PathBuf,
    /// `<base>/.cargo/credentials.toml` (in `$CARGO_HOME`, **not** under `registry/src`).
    credentials: PathBuf,
    /// `<base>/.rustup/toolchains/1.88.0-host/lib/rustlib/src/rust/library/std/src/lib.rs`.
    std_file: PathBuf,
    /// `<base>/outside`, the target of `registry/link-out`.
    outside: PathBuf,
    /// `<base>/.cargo/registry/src/link-out`, a symlink to `outside`.
    link: PathBuf,
}

static FIXTURE: OnceLock<Fixture> = OnceLock::new();

fn fixture() -> &'static Fixture {
    FIXTURE.get_or_init(|| {
        let base = std::env::temp_dir().join(format!("radian-read-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        let cargo_home = base.join(".cargo");
        let registry = cargo_home.join("registry/src");
        let crate_file = registry.join("crate-1.0.0/src/lib.rs");
        std::fs::create_dir_all(crate_file.parent().unwrap()).unwrap();
        std::fs::write(&crate_file, "pub fn dep() {}\n").unwrap();
        // A file in `$CARGO_HOME` but outside `registry/src`: readable by the sandbox, **not**
        // by the file tools (revision 1 decision 3).
        let credentials = cargo_home.join("credentials.toml");
        std::fs::write(&credentials, "token = \"secret\"\n").unwrap();

        let rustup_home = base.join(".rustup");
        let std_file =
            rustup_home.join("toolchains/1.88.0-host/lib/rustlib/src/rust/library/std/src/lib.rs");
        std::fs::create_dir_all(std_file.parent().unwrap()).unwrap();
        std::fs::write(&std_file, "// std\n").unwrap();

        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "SENTINEL-CONTENT\n").unwrap();
        let link = registry.join("link-out");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let workspace = base.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        // SAFETY: this binary sets these three variables exactly once, before any tool reads
        // them. The `OnceLock` serialises the write and every reader runs after it returns, so
        // no thread observes a concurrent `setenv`/`getenv`. They are never changed again.
        unsafe {
            std::env::set_var("CARGO_HOME", &cargo_home);
            std::env::set_var("RUSTUP_HOME", &rustup_home);
            // `HOME` = base, so `~/.cargo/...` expands to `base/.cargo/...` and the `~`
            // spelling of both roots is exercised (the `~` rule uses `HOME`).
            std::env::set_var("HOME", &base);
        }

        Fixture {
            workspace,
            registry,
            crate_file,
            credentials,
            std_file,
            outside,
            link,
        }
    })
}

fn tools() -> CodingTools {
    CodingTools::new(&fixture().workspace).unwrap()
}

fn abs(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---------- read: absolute, `~`, and a toolchain root ----------

#[tokio::test]
async fn a_registry_source_file_is_read_by_absolute_path() {
    let fx = fixture();
    let tools = tools();
    let out = call(&tools, "read", json!({"path": abs(&fx.crate_file)}))
        .await
        .expect("a registry source file is readable");
    assert_eq!(text(&out), "1: pub fn dep() {}\n");
}

#[tokio::test]
async fn a_registry_source_file_is_read_by_home_path() {
    fixture();
    let tools = tools();
    let out = call(
        &tools,
        "read",
        json!({"path": "~/.cargo/registry/src/crate-1.0.0/src/lib.rs"}),
    )
    .await
    .expect("a registry source file is readable with `~`");
    assert_eq!(text(&out), "1: pub fn dep() {}\n");
}

#[tokio::test]
async fn a_toolchain_source_file_is_read() {
    let fx = fixture();
    let tools = tools();
    let out = call(&tools, "read", json!({"path": abs(&fx.std_file)}))
        .await
        .expect("a toolchain lib/rustlib/src file is readable");
    assert_eq!(text(&out), "1: // std\n");

    let out = call(
        &tools,
        "read",
        json!({"path": "~/.rustup/toolchains/1.88.0-host/lib/rustlib/src/rust/library/std/src/lib.rs"}),
    )
    .await
    .expect("the toolchain root is readable with `~` too");
    assert_eq!(text(&out), "1: // std\n");
}

// ---------- grep / glob / list find it ----------

#[tokio::test]
async fn grep_finds_a_registry_source_file_and_names_an_absolute_path() {
    let fx = fixture();
    let tools = tools();
    let out = call(
        &tools,
        "grep",
        json!({"pattern": "pub fn dep", "path": abs(&fx.registry)}),
    )
    .await
    .expect("grep searches the registry root");
    let body = text(&out);
    assert!(body.contains("pub fn dep"), "{body}");
    // The result path is absolute, so the model can pass it straight back to `read`.
    assert!(body.contains(&abs(&fx.crate_file)), "{body}");
}

#[tokio::test]
async fn glob_finds_a_registry_source_file() {
    let fx = fixture();
    let tools = tools();
    let out = call(
        &tools,
        "glob",
        json!({"pattern": "**/*.rs", "path": "~/.cargo/registry/src"}),
    )
    .await
    .expect("glob searches the registry root");
    let body = text(&out);
    assert!(body.contains(&abs(&fx.crate_file)), "{body}");
}

#[tokio::test]
async fn read_lists_a_registry_directory() {
    let fx = fixture();
    let tools = tools();
    let out = call(&tools, "read", json!({"path": abs(&fx.registry)}))
        .await
        .expect("read lists the registry root");
    let body = text(&out);
    assert!(body.contains("crate-1.0.0"), "{body}");
    assert!(
        body.contains("link-out") && body.contains("symlink"),
        "a link is shown, not followed: {body}"
    );
}

// ---------- write / patch never accept the roots ----------

#[tokio::test]
async fn write_to_a_registry_source_file_is_refused_and_leaves_it_alone() {
    let fx = fixture();
    let tools = tools();
    let failure = call(
        &tools,
        "write",
        json!({"path": abs(&fx.crate_file), "content": "PWNED"}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");
    assert_eq!(
        std::fs::read_to_string(&fx.crate_file).unwrap(),
        "pub fn dep() {}\n"
    );
}

#[tokio::test]
async fn patch_to_a_registry_source_file_is_refused_and_leaves_it_alone() {
    let fx = fixture();
    let tools = tools();
    let failure = call(
        &tools,
        "patch",
        json!({"path": abs(&fx.crate_file), "old": "dep", "new": "pwn"}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");
    assert_eq!(
        std::fs::read_to_string(&fx.crate_file).unwrap(),
        "pub fn dep() {}\n"
    );
}

// ---------- outside `registry/src` stays refused ----------

#[tokio::test]
async fn a_file_in_cargo_home_outside_registry_src_is_refused() {
    let fx = fixture();
    let tools = tools();
    for path in [
        abs(&fx.credentials),
        "~/.cargo/credentials.toml".to_string(),
    ] {
        let failure = call(&tools, "read", json!({"path": path}))
            .await
            .unwrap_err();
        assert_eq!(
            failure.code, "path_outside_workspace",
            "{path}: {failure:?}"
        );
        assert!(
            !failure.message.contains("token"),
            "{path}: the message leaked content"
        );
    }
}

#[tokio::test]
async fn an_unrelated_absolute_path_is_refused_and_the_message_names_both_roots() {
    let tools = tools();
    let failure = call(&tools, "read", json!({"path": "/etc/passwd"}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");
    assert!(
        failure.message.contains("absolute path") && failure.message.contains("/etc/passwd"),
        "the refusal still names the path kind and the path: {}",
        failure.message
    );
    assert!(
        failure.message.contains("$CARGO_HOME/registry/src"),
        "{}",
        failure.message
    );
    assert!(
        failure
            .message
            .contains("$RUSTUP_HOME/toolchains/*/lib/rustlib/src"),
        "{}",
        failure.message
    );
}

// ---------- a symlink under the root is never followed ----------

#[tokio::test]
async fn a_symlink_inside_the_registry_root_pointing_outside_is_refused() {
    let fx = fixture();
    let tools = tools();
    let outside_file = fx.outside.join("secret.txt");
    // Through the link.
    let failure = call(
        &tools,
        "read",
        json!({"path": abs(&fx.link.join("secret.txt"))}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");
    assert!(
        failure
            .message
            .contains("symlink leaving the readable root"),
        "{}",
        failure.message
    );
    assert!(
        !failure.message.contains("SENTINEL-CONTENT")
            && !failure.message.contains(outside_file.to_str().unwrap()),
        "the outside path/content leaked: {}",
        failure.message
    );
    // The link itself.
    let failure = call(&tools, "read", json!({"path": abs(&fx.link)}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");

    // grep and glob never descend into it, so its contents are not found.
    let out = call(
        &tools,
        "grep",
        json!({"pattern": "SENTINEL", "path": abs(&fx.registry)}),
    )
    .await
    .unwrap();
    assert!(
        !text(&out).contains("SENTINEL-CONTENT"),
        "grep followed the link: {}",
        text(&out)
    );
    let out = call(
        &tools,
        "glob",
        json!({"pattern": "**/*", "path": abs(&fx.registry)}),
    )
    .await
    .unwrap();
    assert!(
        !text(&out).contains("link-out"),
        "glob listed a symlink: {}",
        text(&out)
    );
    // The outside file is untouched.
    assert_eq!(
        std::fs::read_to_string(&outside_file).unwrap(),
        "SENTINEL-CONTENT\n"
    );
}
