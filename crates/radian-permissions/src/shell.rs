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

/// Programs that run their arguments as a command (`xargs rm`, `sh -c …`, `sudo …`). A
/// segment that starts with one of them is never allowed by a prefix rule or by the
/// read-only table: the rule only sees the wrapper's own tokens, never what it will run.
/// (The pipe only carries data; this table stops the tail execution `a | xargs rm`.)
/// `deny` matching is unaffected and still runs first.
pub const WRAPPER_PROGRAMS: &[&str] = &[
    "xargs", "env", "sudo", "nohup", "time", "timeout", "watch", "sh", "bash", "zsh", "eval",
    "exec", "command", "builtin", "nice", "stdbuf",
];

/// True when the command's first whitespace-separated token names a
/// [`WRAPPER_PROGRAMS`] entry. Only an exact token matches (`xargsfoo` is not `xargs`).
pub fn starts_with_wrapper(command: &str) -> bool {
    tokens(command)
        .first()
        .is_some_and(|first| WRAPPER_PROGRAMS.contains(&first.as_str()))
}

/// When the segment is exactly `cd <target>` (two tokens, no options), the target.
/// Anything else (`cd` alone, `cd -P x`, `cd a b`, `ls x`) is not a plain `cd` segment.
pub fn cd_target(segment: &str) -> Option<&str> {
    let mut words = segment.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some("cd"), Some(target), None) => Some(target),
        _ => None,
    }
}

/// Split a command at every **unquoted** `&&`, `||`, `;` and `|`. Each segment must
/// itself be a [`is_simple`] simple command; any other metacharacter (redirection,
/// `$(…)`, backticks, a lone `&`, a newline, globs, quotes that survive, …) or an
/// empty segment makes the whole split `None`, which is today's behaviour (`ask`).
/// A simple command without separators is one segment.
///
/// Quotes only protect the separators: `echo 'a && b'` is one segment (which is still
/// not simple, so it still asks — it is just not split).
pub fn segments(command: &str) -> Option<Vec<String>> {
    split_compound(command).map(|(segments, _)| segments)
}

/// Like [`segments`], but also returns the exact text between the trimmed segments
/// (each separator with its surrounding whitespace), so a caller can rebuild the
/// command with one segment rewritten and every separator as it was.
pub fn split_compound(command: &str) -> Option<(Vec<String>, Vec<String>)> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    // Byte offsets of each separator token; quotes and separators are ASCII, so byte
    // indexing is safe as long as every cut is at one of them or at ASCII whitespace.
    let separators = separator_spans(command);
    // Raw pieces between the separators; each must trim to a non-empty simple command.
    let mut raws: Vec<&str> = Vec::with_capacity(separators.len() + 1);
    let mut prev = 0;
    for &(start, end) in &separators {
        raws.push(&command[prev..start]);
        prev = end;
    }
    raws.push(&command[prev..]);
    let mut trimmed: Vec<(usize, usize)> = Vec::with_capacity(raws.len());
    // `raw_start` is the raw piece's start in `command`: the raw pieces tile `command`
    // except for the separator tokens, so the next piece starts at the last token's end.
    let mut raw_start = 0;
    for (n, raw) in raws.iter().enumerate() {
        let leading = raw.len() - raw.trim_start().len();
        let trailing = raw.len() - raw.trim_end().len();
        let (start, end) = (raw_start + leading, raw_start + raw.len() - trailing);
        if start >= end || !is_simple(&command[start..end]) {
            return None;
        }
        trimmed.push((start, end));
        if n < separators.len() {
            raw_start = separators[n].1;
        }
    }
    let segments = trimmed
        .iter()
        .map(|&(start, end)| command[start..end].to_owned())
        .collect();
    let mut between: Vec<String> = Vec::with_capacity(separators.len());
    for n in 0..separators.len() {
        between.push(command[trimmed[n].1..trimmed[n + 1].0].to_owned());
    }
    Some((segments, between))
}

/// Byte spans of the separator tokens in `command`: `&&`, `||`, `;` and `|` outside
/// single and double quotes. A lone `&` is not a separator (it stays in the segment
/// text, which then fails [`is_simple`]). Quotes are only tracked so a separator
/// inside them does not split; unbalanced quotes simply protect to the end.
fn separator_spans(command: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut in_single = false;
    let mut in_double = false;
    let bytes = command.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
        } else if c == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
        } else if !in_single && !in_double {
            let two = i + 1 < bytes.len();
            let len = if two && c == b'&' && bytes[i + 1] == b'&' {
                2
            } else if two && c == b'|' && bytes[i + 1] == b'|' {
                2
            } else if c == b';' || c == b'|' {
                1
            } else {
                0
            };
            if len > 0 {
                spans.push((i, i + len));
                i += len;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    spans
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
    fn compound_commands_split_at_unquoted_separators() {
        assert_eq!(
            segments("cargo fmt && cargo test"),
            Some(vec!["cargo fmt".to_owned(), "cargo test".to_owned()])
        );
        assert_eq!(
            segments("cargo fmt||cargo test"),
            Some(vec!["cargo fmt".to_owned(), "cargo test".to_owned()])
        );
        assert_eq!(
            segments("cd sub; cargo test"),
            Some(vec!["cd sub".to_owned(), "cargo test".to_owned()])
        );
        assert_eq!(
            segments("rg foo | head"),
            Some(vec!["rg foo".to_owned(), "head".to_owned()])
        );
        assert_eq!(segments("git status"), Some(vec!["git status".to_owned()]));
    }

    #[test]
    fn quoted_separators_do_not_split() {
        // The scanner never reports a separator inside quotes: with the quotes
        // removed the `&&`/`;` would split, inside them they do not.
        assert_eq!(separator_spans("echo 'a && b'"), vec![]);
        assert_eq!(separator_spans("echo \"a; b\""), vec![]);
        assert_eq!(separator_spans("echo 'a' && b"), vec![(9, 11)]);
        assert_eq!(separator_spans("echo \"a\"; b"), vec![(8, 9)]);
        assert_eq!(
            separator_spans("a && b || c; d | e"),
            vec![(2, 4), (7, 9), (11, 12), (15, 16)]
        );
        // A lone `&` is not a separator; `&&&` is `&&` plus a lone `&`.
        assert_eq!(separator_spans("a & b"), vec![]);
        assert_eq!(separator_spans("a &&& b"), vec![(2, 4)]);
        // Quotes only protect the split: a surviving quote still fails the segment,
        // so the whole split is `None` and the caller falls back to `ask`.
        assert_eq!(segments("echo 'a && b'"), None);
    }

    #[test]
    fn anything_but_the_four_separators_refuses_the_split() {
        // Each of these leaves a non-simple segment (or an empty one), so the whole
        // split is `None` and the caller falls back to today's behaviour (`ask`).
        for command in [
            "git status > /tmp/x",      // redirection
            "git status >> /tmp/x",     // redirection
            "git status < /etc/passwd", // redirection
            "git status & rm -rf /",    // a lone `&` is not a separator
            "git status &&",            // empty tail segment
            "&& git status",            // empty head segment
            "git $(whoami)",            // subshell
            "git `whoami`",             // backticks
            "echo $HOME",               // variable
            "echo a*b",                 // glob
            "git status; rm -rf ~",     // `~` is not a simple char
            "",                         // empty
            "   ",                      // blank
        ] {
            assert_eq!(segments(command), None, "{command:?}");
        }
        // `;` splits, and `/` is a simple char, so both segments here are simple.
        assert_eq!(
            segments("git status; rm -rf /"),
            Some(vec!["git status".to_owned(), "rm -rf /".to_owned()])
        );
    }

    #[test]
    fn split_compound_keeps_every_separator_as_it_was() {
        let (segs, separators) = split_compound("cargo fmt&&git log -p ; ls").unwrap();
        assert_eq!(segs, vec!["cargo fmt", "git log -p", "ls"]);
        assert_eq!(separators, vec!["&&", " ; "]);
        // Rebuilding with one segment rewritten keeps the rest byte-for-byte.
        let rewritten = segs
            .iter()
            .enumerate()
            .map(|(n, s)| {
                if n == 1 {
                    "git log --no-ext-diff --no-textconv -p".to_owned()
                } else {
                    s.clone()
                }
            })
            .collect::<Vec<_>>();
        let mut out = rewritten[0].clone();
        for (n, sep) in separators.iter().enumerate() {
            out.push_str(sep);
            out.push_str(&rewritten[n + 1]);
        }
        assert_eq!(
            out,
            "cargo fmt&&git log --no-ext-diff --no-textconv -p ; ls"
        );
    }

    #[test]
    fn wrappers_match_on_the_exact_first_token() {
        for command in [
            "xargs rm",
            "sudo ls",
            "env FOO=1 cargo test",
            "time cargo test",
        ] {
            assert!(starts_with_wrapper(command), "{command:?}");
        }
        for command in ["ls", "cargo test", "xargsfoo bar", "mytime x"] {
            assert!(!starts_with_wrapper(command), "{command:?}");
        }
        assert_eq!(
            WRAPPER_PROGRAMS.len(),
            16,
            "the runs-its-arguments table: {WRAPPER_PROGRAMS:?}"
        );
    }

    #[test]
    fn cd_target_only_matches_plain_cd_with_one_target() {
        assert_eq!(cd_target("cd sub"), Some("sub"));
        assert_eq!(cd_target("cd .."), Some(".."));
        for segment in ["cd", "cd -P sub", "cd a b", "ls sub", "cdd sub", ""] {
            assert_eq!(cd_target(segment), None, "{segment:?}");
        }
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
