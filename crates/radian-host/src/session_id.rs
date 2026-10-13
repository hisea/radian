//! Session-id generation and validation, shared by both hosts. A session id is the session
//! file's name and the value `{session_id}` expands to, so it is one concept with one rule.

/// A per-process identifier, opaque to the server. Time plus pid is enough for "stable within
/// one process, different across processes"; a UUID dependency would not earn its keep.
pub fn generate_session_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{:x}", std::process::id())
}

/// A session id becomes a file name, so it must not be able to escape the session directory
/// or name another file.
pub fn validate_session_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "session id {id:?} must be 1-128 characters of [A-Za-z0-9._-]"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_id_is_valid() {
        let id = generate_session_id();
        assert!(validate_session_id(&id).is_ok(), "{id}");
    }

    #[test]
    fn path_like_ids_are_refused() {
        for bad in ["", ".", "..", "a/b", "a b"] {
            assert!(validate_session_id(bad).is_err(), "{bad:?}");
        }
    }
}
