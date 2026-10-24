//! ADR 0026 D1: a `keys` list on an `oauth` entry is ignored with **one** notice, and the
//! notice reaches the host's notice path even though the entry has no key pool at all. A
//! normal ChatGPT login must not hide it: `resolve_keys` returns `None` for the entry, so the
//! pool queues the notice and the next host drain shows it once.
//!
//! `RADIAN_HOME` is process-wide, so this binary deliberately holds exactly one test (the rule
//! `tests/key_pool_failover.rs` follows).

mod common;

#[test]
fn an_oauth_entries_ignored_keys_notice_reaches_the_host_once() {
    let dir = common::process_dir("radian-oauth-keys-notice");
    let credentials = dir.join("credentials");
    std::fs::create_dir_all(&credentials).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let auth = credentials.join("auth.json");
    std::fs::write(
        &auth,
        r#"{"chatgpt": {"type": "oauth", "access": "a", "refresh": "r", "expires": 1,
                        "keys": [{"name": "team", "key": "sk-ignored"}]}}"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let dir = std::fs::canonicalize(&dir).unwrap();
    // SAFETY: this binary holds one test, and the variable is set before any credential read.
    unsafe { std::env::set_var("RADIAN_HOME", &dir) };
    radian_host::key_pool::reset();

    // An `oauth` entry has no pool, and asking again (every request does) repeats nothing.
    assert!(radian_host::key_pool::pool("chatgpt").is_none());
    let notices = radian_host::key_pool::take_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    let notice = &notices[0];
    assert!(notice.contains("chatgpt"), "{notice}");
    assert!(notice.contains("`keys`"), "{notice}");
    assert!(notice.contains("ignored"), "{notice}");
    assert!(!notice.contains("sk-ignored"), "{notice}");

    assert!(radian_host::key_pool::pool("chatgpt").is_none());
    assert!(
        radian_host::key_pool::take_notices().is_empty(),
        "the ignored-`keys` notice is queued once, not once per request"
    );
}
