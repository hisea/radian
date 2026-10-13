//! `--header NAME=VALUE` parsing, shared by both hosts.
//!
//! The value is never echoed, not even in the error for a malformed argument: a user who
//! forgot the `=` may have pasted a credential as the whole argument. The name may be echoed
//! (the brief only forbids values).

/// The placeholder `--header` values may use for the process's session id.
pub const SESSION_PLACEHOLDER: &str = "{session_id}";

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
                 --api-key-env, so a header must not replace it"
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
}
