//! `--header NAME=VALUE` parsing, shared by both hosts.
//!
//! The value is never echoed, not even in the error for a malformed argument: a user who
//! forgot the `=` may have pasted a credential as the whole argument. The name may be echoed
//! (the brief only forbids values).

/// The placeholder `--header` values may use for the process's session id.
pub const SESSION_PLACEHOLDER: &str = "{session_id}";

/// The environment variable a `subagent` child reads for its parent's `--header` values (ADR
/// 0043 decision 1a). The values are secrets, so they are **never** in the child's argv (argv
/// is visible to every user through `ps`); this variable is the channel instead.
pub const CHILD_HEADERS_ENV: &str = "RADIAN_CHILD_HEADERS";

/// Encodes the parent's raw `--header` arguments for [`CHILD_HEADERS_ENV`]: a JSON array of the
/// `NAME=VALUE` strings, so a value with a `=` or a newline survives the trip.
pub fn encode_child_headers(headers: &[String]) -> String {
    serde_json::to_string(headers).unwrap_or_else(|_| "[]".to_owned())
}

/// Takes the parent's raw `--header` arguments out of [`CHILD_HEADERS_ENV`] — the variable is
/// **removed from this process's environment** in the same call — or `[]` when it is unset.
///
/// The values stay unresolved here: the child expands `{session_id}` itself, so its headers name
/// its own session, exactly as a config file's headers do. A malformed value is refused without
/// echoing it (a bug in the parent, or a stray variable).
///
/// Taking the value out of the environment is the point (ADR 0043 decision 2, fix round 1 A):
/// they are secrets, and a variable left behind would be inherited by everything this child
/// starts. The child reads it here and keeps only the parsed headers, which its own startup adds
/// to the redaction list.
///
/// # Thread safety
///
/// `std::env::remove_var` is `unsafe` (edition 2024): the process environment is not thread-safe
/// against a concurrent `getenv`/`setenv`. The contract is on the caller — call this while the
/// process still has one thread. The CLI calls it at the top of `run_blocking`, before it builds
/// the Tokio runtime, which is what starts this process's other threads; nothing before that
/// point in the `radian exec` path spawns a thread.
pub fn take_child_headers_from_env() -> Result<Vec<String>, String> {
    let Some(raw) = std::env::var_os(CHILD_HEADERS_ENV) else {
        return Ok(Vec::new());
    };
    // SAFETY: the caller guarantees the process is still single-threaded (see above), so no
    // other thread can be reading or writing the environment while this runs.
    unsafe { std::env::remove_var(CHILD_HEADERS_ENV) };
    serde_json::from_str(&raw.to_string_lossy()).map_err(|_| {
        format!(
            "{CHILD_HEADERS_ENV} is not the JSON array of `NAME=VALUE` strings a subagent parent \
             sets (the value is never printed); unset it and pass --header instead"
        )
    })
}

/// Parses repeated `--header NAME=VALUE` arguments, expanding `{session_id}` in values.
///
/// A malformed argument, an empty or non-token name, and `authorization` all fail. The value
/// of a failing argument is never part of the error message.
pub fn parse_headers(raw: &[String], session_id: &str) -> Result<Vec<(String, String)>, String> {
    let mut headers = Vec::with_capacity(raw.len());
    for (index, item) in raw.iter().enumerate() {
        let Some((name, value)) = item.split_once('=') else {
            return Err(format!(
                "--header (number {}) must be NAME=VALUE",
                index + 1
            ));
        };
        if name.is_empty() {
            return Err(format!("--header (number {}) has an empty name", index + 1));
        }
        if !is_header_name(name) {
            return Err(format!("--header {name}: not a valid HTTP header name"));
        }
        if name.eq_ignore_ascii_case("authorization") {
            return Err(
                "--header authorization=... is refused: the API key comes only from \
                 ~/.radian/credentials/auth.json, so a header must not replace it"
                    .to_string(),
            );
        }
        let value = value.replace(SESSION_PLACEHOLDER, session_id);
        headers.push((name.to_owned(), value));
    }
    Ok(headers)
}

/// RFC 7230 token characters, which is what a header name may contain.
fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            matches!(b,
                b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^'
                    | b'_' | b'`' | b'|' | b'~'
                    | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn a_value_can_reference_the_session_id() {
        let headers = parse_headers(&raw(&["x-s={session_id}"]), "abc").unwrap();
        assert_eq!(headers, vec![("x-s".to_string(), "abc".to_string())]);
    }

    #[test]
    fn a_malformed_header_never_echoes_its_value() {
        let error = parse_headers(&raw(&["sk-secret-value"]), "abc").unwrap_err();
        assert!(error.contains("must be NAME=VALUE"), "{error}");
        assert!(!error.contains("sk-secret-value"), "{error}");
    }

    #[test]
    fn an_authorization_header_is_refused_without_its_value() {
        let error =
            parse_headers(&raw(&["Authorization=Bearer sk-from-a-header"]), "abc").unwrap_err();
        assert!(error.contains("authorization"), "{error}");
        assert!(!error.contains("sk-from-a-header"), "{error}");
    }

    #[test]
    fn an_invalid_name_is_refused() {
        let error = parse_headers(&raw(&["bad name=v"]), "abc").unwrap_err();
        assert!(error.contains("not a valid HTTP header name"), "{error}");
    }

    /// The child channel: a value with a `=`, a space and a newline survives the JSON encoding
    /// byte for byte, and the child's own session id expands the placeholder, not the parent's.
    #[test]
    fn the_child_headers_environment_round_trips() {
        let raw = raw(&["x-test=a=b c", "x-multi=line1\nline2", "x-s={session_id}"]);
        let encoded = encode_child_headers(&raw);
        let decoded: Vec<String> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, raw);
        let headers = parse_headers(&decoded, "child-id").unwrap();
        assert_eq!(headers[0], ("x-test".to_string(), "a=b c".to_string()));
        assert_eq!(headers[2], ("x-s".to_string(), "child-id".to_string()));
    }
}
