//! Escaping for prompt frames (ADR 0036 C2 supplement).
//!
//! A file from the workspace is untrusted input. When it is placed inside a tag
//! (`<project_instructions path="…">…</project_instructions>`, a skill's `<skill …>…</skill>`),
//! a literal closing tag in the content would end the frame early and let the rest of the file
//! pretend to be a higher-priority layer. The same goes for the attribute: a directory name can
//! contain `"` and `>`. So the content's closing tags and the attribute values are escaped.

/// Escapes every case-insensitive `</tag` occurrence's `<` as `&lt;`, so the content cannot
/// close `tag`'s frame. `&lt;/tag>` reads as the literal text, so the model can still see what
/// the file said.
pub(crate) fn close_tag(content: &str, tag: &str) -> String {
    let needle = format!("</{}", tag.to_ascii_lowercase());
    // `to_ascii_lowercase` keeps the byte length and every non-ASCII byte, so an offset found
    // in the lowercased copy is valid in the original.
    let lower = content.to_ascii_lowercase();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    while let Some(position) = lower[i..].find(&needle) {
        let at = i + position;
        out.push_str(&content[i..at]);
        out.push_str("&lt;");
        i = at + 1;
    }
    out.push_str(&content[i..]);
    out
}

/// Escapes an attribute value: `&`, `"`, `<` and `>` (in that order, so an `&` introduced by a
/// later replacement is not escaped twice).
pub(crate) fn attr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A literal closing tag (any case) is neutralized; only the original `<` is replaced.
    #[test]
    fn a_closing_tag_cannot_close_the_frame() {
        let out = close_tag("a</project_instructions>b", "project_instructions");
        assert_eq!(out, "a&lt;/project_instructions>b");
        let out = close_tag("a</PROJECT_INSTRUCTIONS>b", "project_instructions");
        assert_eq!(out, "a&lt;/PROJECT_INSTRUCTIONS>b");
        // A different tag, or an opening tag, is untouched.
        assert_eq!(
            close_tag("<project_instructions>", "project_instructions"),
            "<project_instructions>"
        );
        assert_eq!(close_tag("</skill>", "project_instructions"), "</skill>");
        // Every occurrence, and non-ASCII content around them, survives.
        let out = close_tag("你</skill>好</SKILL>", "skill");
        assert_eq!(out, "你&lt;/skill>好&lt;/SKILL>");
    }

    /// Attribute values escape the characters that would end the attribute or the tag.
    #[test]
    fn an_attribute_value_cannot_end_the_attribute() {
        assert_eq!(attr("a\"b"), "a&quot;b");
        assert_eq!(attr("a<b>c&d"), "a&lt;b&gt;c&amp;d");
        assert_eq!(attr("/tmp/plain"), "/tmp/plain");
    }
}
