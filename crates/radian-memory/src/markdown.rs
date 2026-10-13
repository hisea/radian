//! The Markdown view (ADR 0021 D3): a record is a YAML front matter block plus its content.
//! Editing the file changes nothing until `import`; `import` matches on `id` + `revision`.
//!
//! The parser only supports this format (flat `key: value` lines, no nesting), so it needs no
//! YAML dependency. A malformed line is refused with its position.

use crate::error::MemoryError;
use crate::model::{Kind, MemoryId, MemoryRecord};

/// One record parsed from Markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRecord {
    pub id: MemoryId,
    pub revision: u64,
    pub kind: Kind,
    pub key: Option<String>,
    pub content: String,
}

/// Renders records as Markdown, one block each, in the given order.
pub fn export(records: &[MemoryRecord]) -> String {
    let mut out = String::new();
    for record in records {
        out.push_str("---\n");
        out.push_str(&format!("id: {}\n", record.id));
        out.push_str(&format!("revision: {}\n", record.revision));
        out.push_str(&format!("kind: {}\n", record.kind.as_str()));
        if let Some(key) = &record.key {
            out.push_str(&format!("key: {key}\n"));
        }
        out.push_str(&format!("source: {}\n", record.source.as_str()));
        out.push_str(&format!(
            "verification: {}\n",
            if record.verification.is_confirmed() {
                "confirmed"
            } else {
                "unverified"
            }
        ));
        out.push_str(&format!("status: {}\n", record.status.as_str()));
        out.push_str("---\n");
        out.push_str(&record.content);
        if !record.content.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// Parses a whole export. Any malformed block refuses the whole text (the caller writes
/// nothing).
pub fn parse(text: &str) -> Result<Vec<ParsedRecord>, MemoryError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut records = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim().is_empty() {
            i += 1;
            continue;
        }
        if lines[i].trim() != "---" {
            return Err(md(i + 1, 1, "expected `---` to start a record"));
        }
        let block = i + 1;
        i += 1;
        let mut fields: Vec<(String, String)> = Vec::new();
        while i < lines.len() && lines[i].trim() != "---" {
            let Some((key, value)) = lines[i].split_once(':') else {
                return Err(md(i + 1, 1, "expected `key: value`"));
            };
            fields.push((key.trim().to_owned(), value.trim().to_owned()));
            i += 1;
        }
        if i >= lines.len() {
            return Err(md(block, 1, "front matter is not closed with `---`"));
        }
        i += 1; // the closing `---`
        let body_start = i;
        while i < lines.len() && lines[i].trim() != "---" {
            i += 1;
        }
        let content = lines[body_start..i].join("\n");
        let content = content.trim_end_matches('\n').to_owned();

        let field = |name: &str| {
            fields
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let id = field("id").ok_or_else(|| md(block, 1, "missing `id`"))?;
        if id.is_empty() {
            return Err(md(block, 1, "`id` is empty"));
        }
        let revision: u64 = field("revision")
            .ok_or_else(|| md(block, 1, "missing `revision`"))?
            .parse()
            .map_err(|_| md(block, 1, "`revision` is not a number"))?;
        // A kind this version does not know is kept verbatim, not refused.
        let kind = field("kind")
            .map(|raw| Kind::from_name(&raw))
            .unwrap_or(Kind::Preference);
        let key = field("key").filter(|key| !key.is_empty());
        records.push(ParsedRecord {
            id: MemoryId(id),
            revision,
            kind,
            key,
            content,
        });
    }
    Ok(records)
}

fn md(line: usize, column: usize, message: &str) -> MemoryError {
    MemoryError::Markdown {
        line,
        column,
        message: message.to_owned(),
    }
}
