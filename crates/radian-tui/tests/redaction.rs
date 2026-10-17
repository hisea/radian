//! A value registered **after** the session's redaction list was built must still be redacted
//! (ADR 0026).
//!
//! `resolved_keys()` is read once, at startup. A `chatgpt` access token that expires
//! mid-session is refreshed by `ensure_fresh` and written to `auth.json` well after that, so
//! the panic log, the model-error line and an export would print the token in use unless the
//! reader merges `radian_host::registered_secrets()` at the moment it redacts. This lives in
//! its own test binary on purpose: the registry is process-global, and a test that adds to it
//! would break `terminal.rs`'s own exact-equality assertions on the same lock in the `--lib`
//! binary.

use std::sync::{Arc, RwLock};

use radian_tui::terminal::{SharedSecrets, redact, shared_secrets};

#[test]
fn a_value_registered_after_the_list_was_built_is_still_redacted() {
    // Unique to this process, never asserted absent anywhere (the registry is process-global).
    let late_access = format!("late-access-token-{}", std::process::id());
    let late_refresh = format!("late-refresh-token-{}", std::process::id());
    let startup_key = format!("startup-api-key-{}", std::process::id());

    // The list as `cli.rs` builds it at startup: `resolved_keys()` and the header values.
    let secrets: SharedSecrets = Arc::new(RwLock::new(vec![startup_key.clone()]));
    assert!(
        !shared_secrets(&secrets).contains(&late_access),
        "the list was built before the token existed"
    );

    // The mid-session refresh: `store_token` writes the pair and registers both.
    radian_host::register_secret(&late_access);
    radian_host::register_secret(&late_refresh);

    let list = shared_secrets(&secrets);
    let text = format!(
        "model error: 401 for Bearer {late_access} (refresh {late_refresh}), key {startup_key}"
    );
    let redacted = redact(&text, &list);
    assert!(!redacted.contains(&late_access), "{redacted}");
    assert!(!redacted.contains(&late_refresh), "{redacted}");
    assert!(!redacted.contains(&startup_key), "{redacted}");
    assert!(redacted.contains("<redacted>"), "{redacted}");
}
