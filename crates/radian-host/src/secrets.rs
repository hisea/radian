//! The process-wide, append-only registry of secret values (ADR 0026).
//!
//! `resolved_keys` reads `auth.json` once, at startup; a `chatgpt` access token that expires
//! mid-session is refreshed and written back *after* that read, so a redaction list built
//! from `resolved_keys` alone stops redacting the token actually in use (about an hour into a
//! session, when `ensure_fresh` first fires). [`register`] is where such a value is added:
//! [`crate::oauth_chatgpt::store_token`] registers the access and refresh tokens it writes, so
//! both a `/login` and every later refresh land here, and every redaction reader merges
//! [`registered`] at the moment it redacts.
//!
//! Nothing ever removes a value: a token that has appeared stays redacted (only the values a
//! redactor must replace live here, never a key that must be kept).

use std::sync::{OnceLock, RwLock};

/// Every value registered so far, in the order it was first registered. `OnceLock` because a
/// process that never signs in never allocates it.
static REGISTERED: OnceLock<RwLock<Vec<String>>> = OnceLock::new();

/// **Poison-safe**, like `radian_tui::terminal::shared_secrets`: a panic while another thread
/// held the lock must not turn redaction off (failing open is the wrong direction for a
/// redaction list), so the guard is taken through `into_inner` instead of being dropped.
fn registry() -> &'static RwLock<Vec<String>> {
    REGISTERED.get_or_init(|| RwLock::new(Vec::new()))
}

/// Adds `value` to the process's redaction registry. An empty value is ignored (there is
/// nothing to redact, and an empty needle would match everywhere); a value already registered
/// is not added twice (the list is scanned on every redaction).
pub fn register(value: &str) {
    if value.is_empty() {
        return;
    }
    let mut list = registry()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !list.iter().any(|existing| existing == value) {
        list.push(value.to_owned());
    }
}

/// A snapshot of every value [`register`]ed in this process, oldest first. A redactor merges
/// this with its own list **at the time it redacts**, never once at startup: the values it
/// carries are the ones `resolved_keys` could not have seen yet.
pub fn registered() -> Vec<String> {
    registry()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}
