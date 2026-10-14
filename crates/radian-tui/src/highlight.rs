//! Code-block syntax highlighting (ADR 0028 R2, decision 2): `syntect`, with the
//! **pure-Rust** regex backend (`regex-fancy`; no oniguruma) and syntect's packed default
//! syntax set.
//!
//! Two rules shape the module:
//!
//! - **Colors come from the Radian theme, never from syntect.** syntect's themes are not
//!   compiled in; the parser only produces a [`SyntaxRole`] per piece — one of the nine
//!   `syntax*` roles ADR 0029 defines, or `Plain` for anything the map does not know — and
//!   `render.rs` resolves the role through the active [`crate::theme::Theme`].
//! - **Only common languages are highlighted.** Whether a fence's info string highlights is
//!   decided by [`syntax_for`]'s explicit table, so a grammar the set happens to ship but we
//!   do not list (or a language the set does not ship at all) falls back to the code block's
//!   uniform style.
//!
//! The syntax set is loaded once ([`syntax_set`]) and cached for the process.

use std::sync::OnceLock;

use syntect::parsing::{ParseState, ScopeStack, ScopeStackOp, SyntaxReference, SyntaxSet};
use unicode_width::UnicodeWidthChar;

/// One piece of a highlighted line and the Radian role it takes. `Plain` is the fallback: the
/// renderer styles it with the code block's own style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxRole {
    Comment,
    Keyword,
    Function,
    Variable,
    String,
    Number,
    Type,
    Operator,
    Punctuation,
    Plain,
}

/// One highlighted line: pieces in order, covering the whole source line.
pub type HighlightedLine = Vec<(String, SyntaxRole)>;

/// The process's syntax set, loaded once. syntect's default set (75 grammars) is a compressed
/// 368 KB dump embedded by the `default-syntaxes` feature; loading it deserializes it on first
/// use.
fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// The languages a fenced block may name, mapped to syntect's own syntax name. This table **is**
/// the "common languages" decision (ADR 0028 R2): a language not listed here does not
/// highlight, even when the grammar is present in the set. Aliases are the file extensions and
/// spellings people actually write.
const LANGUAGES: &[(&str, &str)] = &[
    ("bash", "Bourne Again Shell (bash)"),
    ("c", "C"),
    ("c#", "C#"),
    ("c++", "C++"),
    ("cc", "C++"),
    ("clj", "Clojure"),
    ("clojure", "Clojure"),
    ("cpp", "C++"),
    ("cs", "C#"),
    ("csharp", "C#"),
    ("css", "CSS"),
    ("cxx", "C++"),
    ("diff", "Diff"),
    ("dot", "Graphviz (DOT)"),
    ("el", "Lisp"),
    ("erl", "Erlang"),
    ("erlang", "Erlang"),
    ("go", "Go"),
    ("golang", "Go"),
    ("graphviz", "Graphviz (DOT)"),
    ("haskell", "Haskell"),
    ("hs", "Haskell"),
    ("htm", "HTML"),
    ("html", "HTML"),
    ("java", "Java"),
    ("javascript", "JavaScript"),
    ("js", "JavaScript"),
    ("json", "JSON"),
    ("jsx", "JavaScript"),
    ("latex", "LaTeX"),
    ("lisp", "Lisp"),
    ("lua", "Lua"),
    ("make", "Makefile"),
    ("makefile", "Makefile"),
    ("markdown", "Markdown"),
    ("matlab", "MATLAB"),
    ("md", "Markdown"),
    ("ml", "OCaml"),
    ("mm", "Objective-C"),
    ("objc", "Objective-C"),
    ("objective-c", "Objective-C"),
    ("ocaml", "OCaml"),
    ("patch", "Diff"),
    ("perl", "Perl"),
    ("php", "PHP"),
    ("pl", "Perl"),
    ("py", "Python"),
    ("python", "Python"),
    ("r", "R"),
    ("rb", "Ruby"),
    ("rs", "Rust"),
    ("ruby", "Ruby"),
    ("rust", "Rust"),
    ("scala", "Scala"),
    ("sh", "Bourne Again Shell (bash)"),
    ("shell", "Bourne Again Shell (bash)"),
    ("sql", "SQL"),
    ("tex", "LaTeX"),
    ("xml", "XML"),
    ("yaml", "YAML"),
    ("yml", "YAML"),
    ("zsh", "Bourne Again Shell (bash)"),
];

/// The syntax for a fence's info string, or `None` when the language is not in
/// [`LANGUAGES`] (or the grammar is missing from this syntect build).
pub fn syntax_for(lang: &str) -> Option<&'static SyntaxReference> {
    let lang = lang.trim().to_ascii_lowercase();
    let name = LANGUAGES
        .iter()
        .find(|(alias, _)| *alias == lang)
        .map(|(_, name)| *name)?;
    syntax_set().find_syntax_by_name(name)
}

/// Highlights `lines` as one block: multi-line state (a comment or string spanning lines) is
/// carried across lines, which is why the whole block is highlighted at once, not line by
/// line. `None` when the language is unknown.
pub fn highlight_lines(lines: &[&str], lang: &str) -> Option<Vec<HighlightedLine>> {
    let syntax = syntax_for(lang)?;
    let set = syntax_set();
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let ops = state.parse_line(line, set).unwrap_or_default();
        out.push(spans(line, &ops, &mut stack));
    }
    Some(out)
}

/// The pieces of one line from its scope operations, applying them to `stack` as it goes (the
/// stack is the caller's, so it survives to the next line).
fn spans(line: &str, ops: &[(usize, ScopeStackOp)], stack: &mut ScopeStack) -> HighlightedLine {
    let mut out: HighlightedLine = Vec::new();
    let mut cursor = 0usize;
    let push = |text: &str, stack: &ScopeStack, out: &mut HighlightedLine| {
        if !text.is_empty() {
            out.push((text.to_owned(), role_for(stack)));
        }
    };
    for (index, op) in ops {
        let index = (*index).min(line.len());
        if index > cursor {
            push(&line[cursor..index], stack, &mut out);
            cursor = index;
        }
        // A malformed op is not fatal: keep the text and the stack as they are.
        let _ = stack.apply(op);
    }
    if cursor < line.len() {
        push(&line[cursor..], stack, &mut out);
    }
    out
}

/// The role for the current scope stack. The **highest-priority** scope wins, not simply the
/// innermost: a comment's `//` is `punctuation.definition.comment` inside a `comment` scope,
/// and a string's quotes are `punctuation.definition.string` inside a `string` scope, so a
/// plain innermost-wins rule would lose the comment/string color on the markers. Equal ranks
/// keep the innermost scope (the later one in the outer→inner stack).
fn role_for(stack: &ScopeStack) -> SyntaxRole {
    let mut best = SyntaxRole::Plain;
    let mut best_rank = 0u8;
    for scope in &stack.scopes {
        let name = scope.build_string();
        for (prefix, role) in SCOPE_ROLES {
            if matches_prefix(&name, prefix) {
                let rank = role_rank(*role);
                if rank >= best_rank {
                    best = *role;
                    best_rank = rank;
                }
                break;
            }
        }
    }
    best
}

fn matches_prefix(name: &str, prefix: &str) -> bool {
    name == prefix || (name.starts_with(prefix) && name.as_bytes().get(prefix.len()) == Some(&b'.'))
}

/// A role's priority when several scopes in one stack match. Comment and string outrank the
/// punctuation inside them; an operator outranks plain punctuation. `Plain` is the floor.
fn role_rank(role: SyntaxRole) -> u8 {
    match role {
        SyntaxRole::Comment => 9,
        SyntaxRole::String => 8,
        SyntaxRole::Number => 7,
        SyntaxRole::Type => 6,
        SyntaxRole::Keyword => 5,
        SyntaxRole::Function => 4,
        SyntaxRole::Variable => 3,
        SyntaxRole::Operator => 2,
        SyntaxRole::Punctuation => 1,
        SyntaxRole::Plain => 0,
    }
}

/// The scope-prefix → role table, in match order (an operator before its `keyword` parent, a
/// `storage.type` before `storage`). pi's nine roles, plus the `Plain` fallback.
const SCOPE_ROLES: &[(&str, SyntaxRole)] = &[
    ("comment", SyntaxRole::Comment),
    ("string", SyntaxRole::String),
    ("constant.numeric", SyntaxRole::Number),
    ("keyword.operator", SyntaxRole::Operator),
    // `storage.type` is Sublime's scope for `fn`/`let`/`struct` — keywords, not type names;
    // the type names are `entity.name.type` / `support.type` below.
    ("storage.type", SyntaxRole::Keyword),
    ("keyword", SyntaxRole::Keyword),
    ("storage", SyntaxRole::Keyword),
    ("entity.name.function", SyntaxRole::Function),
    ("support.function", SyntaxRole::Function),
    ("entity.name.tag", SyntaxRole::Function),
    ("entity.name.type", SyntaxRole::Type),
    ("entity.name.class", SyntaxRole::Type),
    ("entity.name.struct", SyntaxRole::Type),
    ("entity.name.enum", SyntaxRole::Type),
    ("entity.name.namespace", SyntaxRole::Type),
    ("support.type", SyntaxRole::Type),
    ("support.class", SyntaxRole::Type),
    ("variable", SyntaxRole::Variable),
    ("meta.function.parameters", SyntaxRole::Variable),
    ("punctuation", SyntaxRole::Punctuation),
];

/// Truncates one highlighted line to `width`, exactly as [`crate::blocks::code_line_layout`]
/// truncates the plain text: tabs expand to four spaces, a line that fits is kept whole, a
/// wider one keeps `width - 1` columns and ends in `…`. The ellipsis takes the role of the
/// character before it, so a cut string stays string-colored. The joined text equals
/// `code_line_layout(line, width).0`.
pub fn styled_line(line: &HighlightedLine, width: usize) -> HighlightedLine {
    let total: usize = line
        .iter()
        .flat_map(|(text, _)| text.chars())
        .map(char_width)
        .sum();
    let cut = total > width;
    let limit = if cut {
        width.saturating_sub(1)
    } else {
        usize::MAX
    };
    let mut out: HighlightedLine = Vec::new();
    let mut used = 0usize;
    let mut last_role = SyntaxRole::Plain;
    'outer: for (text, role) in line {
        let mut chunk = String::new();
        for ch in text.chars() {
            let w = char_width(ch);
            if used + w > limit {
                if !chunk.is_empty() {
                    out.push((chunk, *role));
                }
                last_role = *role;
                break 'outer;
            }
            if ch == '\t' {
                chunk.push_str("    ");
            } else {
                chunk.push(ch);
            }
            used += w;
        }
        if !chunk.is_empty() {
            out.push((chunk, *role));
        }
        last_role = *role;
    }
    if cut {
        out.push(("…".to_owned(), last_role));
    }
    out
}

/// The display width of one char, tab counted as four columns (ADR 0028 R9).
fn char_width(ch: char) -> usize {
    if ch == '\t' {
        4
    } else {
        UnicodeWidthChar::width(ch).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(line: &HighlightedLine) -> Vec<SyntaxRole> {
        line.iter().map(|(_, role)| *role).collect()
    }

    /// The Rust grammar is in syntect's default set, and the three roles the acceptance names
    /// (keyword, string, comment) come out of the scope map.
    #[test]
    fn rust_maps_keyword_string_and_comment() {
        let lines = ["fn main() {", "    let s = \"hi\"; // note", "}"];
        let out = highlight_lines(&lines, "rust").expect("rust is in the set");
        let all: Vec<SyntaxRole> = out.iter().flat_map(roles).collect();
        assert!(all.contains(&SyntaxRole::Keyword), "{out:?}");
        assert!(all.contains(&SyntaxRole::String), "{out:?}");
        assert!(all.contains(&SyntaxRole::Comment), "{out:?}");
    }

    /// An unknown language (and a language outside the table) does not highlight.
    #[test]
    fn unknown_languages_fall_back() {
        assert!(syntax_for("nonesuch").is_none());
        assert!(highlight_lines(&["x"], "nonesuch").is_none());
        // The set ships `Plain Text`, but it is not in the table.
        assert!(syntax_for("plain text").is_none());
    }

    /// The truncation mirrors the plain-text rule, and the ellipsis keeps the last role.
    #[test]
    fn styled_line_truncates_like_the_plain_rule() {
        let line: HighlightedLine = vec![
            ("let ".to_owned(), SyntaxRole::Keyword),
            ("value".to_owned(), SyntaxRole::Variable),
            (" = 1".to_owned(), SyntaxRole::Plain),
        ];
        let full = styled_line(&line, 40);
        let joined: String = full.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(joined, "let value = 1");
        let cut = styled_line(&line, 6);
        let joined: String = cut.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(joined, "let v…");
        assert_eq!(cut.last().unwrap().1, SyntaxRole::Variable);
    }

    /// A tab expands to four cells, in the highlighted path too.
    #[test]
    fn styled_line_expands_tabs() {
        let line: HighlightedLine = vec![("\tfn".to_owned(), SyntaxRole::Keyword)];
        let out = styled_line(&line, 40);
        let joined: String = out.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(joined, "    fn");
    }
}
