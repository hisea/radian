//! The YAML front matter of plan and task files.
//!
//! The subset is the one `radian-host`'s skill reader parses (`crates/radian-host/src/skills`):
//! top-level `key: value` scalars, quoted scalars, `>`/`|` block scalars, blank lines and
//! `#` comments. arc adds the two list spellings its own files use: a flow list
//! (`tasks: [0001-a, 0002-b]`) and one `- id` line per entry under the key.
//!
//! Unknown keys are not this layer's business: the typed reader in [`super::model`] rejects
//! them by name, so a hand-edited file names its bad key instead of loading silently.

use std::collections::BTreeMap;

/// One field's value: a scalar or a list of scalars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Scalar(String),
    List(Vec<String>),
}

/// A parsed file: its front matter fields and the body after the closing `---`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Document {
    pub(crate) fields: BTreeMap<String, Value>,
    pub(crate) body: String,
}

/// How an indented line continues the previous key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Plain,
    Fold,
    Literal,
}

impl Style {
    fn is_block(self) -> bool {
        matches!(self, Style::Fold | Style::Literal)
    }
}

/// Parses a whole file (front matter and body).
pub(crate) fn parse(text: &str) -> Result<Document, String> {
    let first_end = text.find('\n').map(|index| index + 1).unwrap_or(text.len());
    if text[..first_end].trim_end() != "---" {
        return Err("the file does not start with `---`".into());
    }
    let mut fields: BTreeMap<String, Value> = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut style = Style::Plain;
    let mut pos = first_end;
    while pos < text.len() {
        let end = text[pos..]
            .find('\n')
            .map(|index| pos + index + 1)
            .unwrap_or(text.len());
        let line = text[pos..end].trim_end_matches('\n').trim_end_matches('\r');
        if line.trim_end() == "---" {
            let mut body = &text[end..];
            if let Some(rest) = body.strip_prefix("\r\n") {
                body = rest;
            } else if let Some(rest) = body.strip_prefix('\n') {
                body = rest;
            }
            return Ok(Document {
                fields,
                body: body.to_owned(),
            });
        }
        pos = end;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            current = None;
            style = Style::Plain;
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if trimmed.starts_with('#') && !(indented && style.is_block()) {
            continue;
        }
        if indented {
            let Some(key) = current.clone() else {
                continue;
            };
            if let Some(item) = trimmed.strip_prefix("- ") {
                let value = fields.entry(key).or_insert_with(|| Value::List(Vec::new()));
                match value {
                    Value::List(items) => items.push(unquote(item.trim())),
                    // The key already held a scalar: a list here is a malformed file, and
                    // the typed reader reports the type mismatch with the key's name.
                    Value::Scalar(_) => {}
                }
                continue;
            }
            if style.is_block() {
                let separator = if style == Style::Fold { ' ' } else { '\n' };
                if let Some(Value::Scalar(value)) = fields.get_mut(&key) {
                    if !value.is_empty() {
                        value.push(separator);
                    }
                    value.push_str(trimmed);
                }
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!("`{line}` is not `key: value`"));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("`{line}` has an empty key"));
        }
        let value = value.trim();
        current = Some(key.to_owned());
        style = match value {
            ">" | ">-" | ">+" => Style::Fold,
            "|" | "|-" | "|+" => Style::Literal,
            _ => Style::Plain,
        };
        if style.is_block() {
            fields.insert(key.to_owned(), Value::Scalar(String::new()));
        } else if value.is_empty() {
            // An empty value is written only for a list (`tasks:` then `- id` lines); a
            // scalar field that needs to be empty is written as an empty string `""`.
            fields.insert(key.to_owned(), Value::List(Vec::new()));
        } else if let Some(inner) = value
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            let items = inner
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(unquote)
                .collect();
            fields.insert(key.to_owned(), Value::List(items));
        } else {
            fields.insert(key.to_owned(), Value::Scalar(unquote(value)));
        }
    }
    Err("the frontmatter is not closed with `---`".into())
}

/// Strips the YAML quoting this writer emits: matching single or double quotes, with the
/// double-quoted escapes `\\` and `\"` restored.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        let mut out = String::new();
        let mut chars = value[1..value.len() - 1].chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some(other) => out.push(other),
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        return out;
    }
    if bytes.len() >= 2 && bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'' {
        return value[1..value.len() - 1].to_owned();
    }
    value.to_owned()
}

/// One scalar as the writer puts it on the line: plain when the text is safe, quoted
/// otherwise (an empty value, surrounding space, a leading character YAML reads as
/// structure, `: `, ` #`, a newline, a quote or a backslash).
pub(crate) fn scalar(value: &str) -> String {
    const LEADING: &str = "[]{}#&*!|>'\"%@`-?,";
    let needs_quote = value.is_empty()
        || value.trim() != value
        || value.contains(": ")
        || value.contains(" #")
        || value.contains('\n')
        || value.contains('"')
        || value.contains('\\')
        || value.starts_with(|c: char| LEADING.contains(c));
    if needs_quote {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for c in value.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                other => out.push(other),
            }
        }
        out.push('"');
        out
    } else {
        value.to_owned()
    }
}

/// A list field as one or more whole lines: `key: []` when empty, otherwise the key line
/// and one indented `- item` per entry.
pub(crate) fn list_field(key: &str, values: &[String]) -> String {
    let mut out = String::new();
    if values.is_empty() {
        out.push_str(&format!("{key}: []\n"));
        return out;
    }
    out.push_str(&format!("{key}:\n"));
    for value in values {
        out.push_str(&format!("  - {}\n", scalar(value)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_lists_and_the_body_round_trip() {
        let text = "---\nid: 0001-a\ntitle: \"a: b\"\ntasks:\n  - 0002-b\n  - 0003-c\nempty: []\n---\nthe body\nsecond line\n";
        let document = parse(text).unwrap();
        assert_eq!(document.fields["id"], Value::Scalar("0001-a".into()));
        assert_eq!(document.fields["title"], Value::Scalar("a: b".into()));
        assert_eq!(
            document.fields["tasks"],
            Value::List(vec!["0002-b".into(), "0003-c".into()])
        );
        assert_eq!(document.fields["empty"], Value::List(Vec::new()));
        assert_eq!(document.body, "the body\nsecond line\n");
    }

    #[test]
    fn a_flow_list_parses() {
        let document = parse("---\ntasks: [0001-a, 0002-b]\n---\n").unwrap();
        assert_eq!(
            document.fields["tasks"],
            Value::List(vec!["0001-a".into(), "0002-b".into()])
        );
        let document = parse("---\ntasks: []\n---\n").unwrap();
        assert_eq!(document.fields["tasks"], Value::List(Vec::new()));
    }

    #[test]
    fn the_writer_quotes_what_needs_quoting() {
        for (text, written) in [
            ("plain", "plain"),
            ("a: b", "\"a: b\""),
            ("- lead", "\"- lead\""),
            ("trail ", "\"trail \""),
            ("", "\"\""),
            ("say \"hi\"", "\"say \\\"hi\\\"\""),
        ] {
            assert_eq!(scalar(text), written);
            let document = parse(&format!("---\nk: {}\n---\n", scalar(text))).unwrap();
            assert_eq!(document.fields["k"], Value::Scalar(text.to_owned()));
        }
    }

    #[test]
    fn a_missing_or_unclosed_front_matter_is_an_error() {
        assert!(parse("no front matter\n").is_err());
        assert!(parse("---\nid: 1\n").is_err());
    }

    #[test]
    fn list_fields_are_written_in_block_form() {
        assert_eq!(list_field("tasks", &[]), "tasks: []\n");
        assert_eq!(
            list_field("tasks", &["0001-a".into(), "0002-b".into()]),
            "tasks:\n  - 0001-a\n  - 0002-b\n"
        );
    }
}
