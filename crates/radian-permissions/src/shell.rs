//! Shell command matching for the permission layer.
//!
//! **This is a deliberately conservative approximation, not a shell parser.** Two rules:
//!
//! - An `allow`/`ask` command rule only matches a **simple command**: a command whose
//!   characters are all in a small safe set (alphanumerics plus `_ - . / : = + , @` and
//!   spaces/tabs). Any shell metacharacter or structure — `;` `&` `|` `` ` `` `$` `>` `<`
//!   newline, quotes, backslash, globs, parentheses, braces, brackets, `~`, `!`, `#` — makes
//!   the command not simple, so `allow` rules do not apply and the call falls through to
//!   `ask`. Over-asking is the intended failure mode.
//! - A `deny` command rule is matched** conservatively over every segment**: the command is
//!   split on shell separators and the contents of quotes/backticks/subshells, then any token
//!   equal to (or, for the first token, whose file name equals) the rule's first token starts
//!   a match for the whole rule prefix. Any segment matching denies the whole command.
//!
//! What this **cannot** see, and never claims to: shell aliases, `eval`, a called program
//! that starts its own shell, `.`/`source`, or `sh -c "…"` where the nested command is a
//! quoted argument (the quotes make the outer command non-simple, so it asks; but the nested
//! text is not parsed). The real execution boundary is the sandbox (ADR 0012 P1); this layer
//! only decides allow/ask/deny.

/// Characters that may appear in a simple command's tokens. Anything else blocks `allow`.
fn simple_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '=' | '+' | ',' | '@')
}

/// True when `command` is a simple command: non-empty and made only of [`simple_char`]
/// characters, spaces and tabs.
pub fn is_simple(command: &str) -> bool {
    let trimmed = command.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| c == ' ' || c == '\t' || simple_char(c))
}

/// Whitespace-separated tokens of a simple command.
pub fn tokens(command: &str) -> Vec<String> {
    command.split_whitespace().map(str::to_owned).collect()
}

/// `CommandPrefix` match for `allow`/`ask`: the command must be simple and its tokens must
/// start with `prefix` (token-wise, so `git statusfoo` does not match `git status`).
pub fn simple_prefix_matches(command: &str, prefix: &[String]) -> bool {
    if prefix.is_empty() || !is_simple(command) {
        return false;
    }
    let tokens = tokens(command);
    tokens.len() >= prefix.len() && tokens[..prefix.len()] == *prefix
}

/// `CommandPrefix` match for `deny`, conservative over a composite command.
pub fn deny_matches(command: &str, prefix: &[String]) -> bool {
    if prefix.is_empty() {
        return false;
    }
    // Replace separators and quote/subshell characters with spaces so their contents become
    // separate tokens. This is a crude split, on purpose: it can only make `deny` match more,
    // never less.
    let mut normalized = String::with_capacity(command.len());
    for c in command.chars() {
        if is_separator(c) {
            normalized.push(' ');
        } else {
            normalized.push(c);
        }
    }
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    if tokens.len() < prefix.len() {
        // A single-token rule still matches a path-like token (`/bin/rm`).
        return prefix.len() == 1 && tokens.iter().any(|t| basename_matches(t, &prefix[0]));
    }
    tokens.windows(prefix.len()).any(|window| {
        window
            .iter()
            .zip(prefix.iter())
            .enumerate()
            .all(|(i, (token, want))| {
                if i == 0 {
                    basename_matches(token, want)
                } else {
                    *token == want.as_str()
                }
            })
    })
}

/// Separators and structure characters; their contents are examined as separate tokens.
fn is_separator(c: char) -> bool {
    matches!(
        c,
        ';' | '&' | '|' | '`' | '\'' | '"' | '(' | ')' | '{' | '}' | '[' | ']' | '\n' | '\r' | '$'
    )
}

/// The first token matches when it is exactly `want`, or its file name is `want` (so
/// `/bin/rm`, `./rm` and `../bin/rm` are the same command as `rm` for `deny`).
fn basename_matches(token: &str, want: &str) -> bool {
    token == want
        || std::path::Path::new(token)
            .file_name()
            .is_some_and(|name| name == want)
}

/// The list of metacharacters the bypass tests use. Kept here so the test and the docs agree.
pub const METACHARACTERS: &[&str] = &[
    ";", "&", "|", "`", "$(", "$VAR", ">", ">>", "<", "\n", "( )",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn simple_commands_are_recognised() {
        assert!(is_simple("git status"));
        assert!(is_simple("git status --short"));
        assert!(is_simple("cargo build -p foo"));
        assert!(!is_simple("git status; rm -rf ~"));
        assert!(!is_simple("git $(whoami)"));
        assert!(!is_simple(""));
    }

    #[test]
    fn prefixes_match_on_token_boundaries() {
        assert!(simple_prefix_matches(
            "git status --short",
            &rule(&["git", "status"])
        ));
        assert!(simple_prefix_matches(
            "git status",
            &rule(&["git", "status"])
        ));
        assert!(!simple_prefix_matches(
            "git statusfoo",
            &rule(&["git", "status"])
        ));
        assert!(!simple_prefix_matches("git", &rule(&["git", "status"])));
    }

    #[test]
    fn composite_deny_matches_any_segment() {
        assert!(deny_matches("git status; rm -rf /", &rule(&["rm"])));
        assert!(deny_matches("true && rm x", &rule(&["rm"])));
        assert!(deny_matches("echo $(rm x)", &rule(&["rm"])));
        assert!(deny_matches("sh -c 'rm x'", &rule(&["rm"])));
        assert!(deny_matches("/bin/rm x", &rule(&["rm"])));
        assert!(!deny_matches("git status", &rule(&["rm"])));
        assert!(!deny_matches("xrm x", &rule(&["rm"])));
    }
}
