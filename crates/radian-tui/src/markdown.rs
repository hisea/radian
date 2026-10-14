//! Assistant-text Markdown (ADR 0028 R2): CommonMark plus tables and strikethrough, parsed
//! with `pulldown-cmark` into a small block tree the transcript lays out.
//!
//! No syntax highlighting (that is slice 5): the tree carries **inline roles** (bold, italic,
//! inline code, link text and link URL) that the renderer maps to theme roles.
//!
//! Every piece of displayed text that exists in the source keeps its **byte offset** in the
//! entry's display text, so the selection mapping can point at the real characters and a copy
//! yields the source the selection covers (the same rule the Mermaid block follows). Text the
//! renderer synthesizes — a list bullet, a quote bar, a rule, a table border, a link's
//! ` (url)` — has no offset: it selects the block's start.
//!
//! Pure: `parse` is a function of the source and its base offset. It never does I/O and never
//! looks at a width, so a width change re-lays out without re-parsing.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// An inline role (ADR 0028 R2, ADR 0029 decision 3: pi's same-named roles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inline {
    Plain,
    Bold,
    Italic,
    Code,
    LinkText,
    /// The synthesized ` (url)` after a link's text; `offset` is `None`.
    LinkUrl,
}

/// One run of displayed text with one inline role. `offset` is the source byte offset of the
/// run's first character, or `None` for synthesized text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    pub text: String,
    pub inline: Inline,
    pub offset: Option<usize>,
}

impl Piece {
    pub fn plain(text: impl Into<String>, offset: Option<usize>) -> Self {
        Self {
            text: text.into(),
            inline: Inline::Plain,
            offset,
        }
    }

    fn styled(text: impl Into<String>, inline: Inline, offset: Option<usize>) -> Self {
        Self {
            text: text.into(),
            inline,
            offset,
        }
    }
}

/// One parsed block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MdBlock {
    Paragraph(Vec<Piece>),
    Heading {
        level: u8,
        pieces: Vec<Piece>,
    },
    /// `ordered` is the first number for an ordered list (`None` = bullet list). An item is a
    /// list of blocks, so a nested list is an item's child.
    List {
        ordered: Option<u64>,
        items: Vec<Vec<MdBlock>>,
    },
    Quote(Vec<MdBlock>),
    /// A thematic break (`---`).
    Rule,
    Table {
        head: Vec<Vec<Piece>>,
        rows: Vec<Vec<Vec<Piece>>>,
    },
}

/// The parser's options: CommonMark plus the two extensions Radian renders.
fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH
}

/// Parses `source` (a block's source range, starting at `base` in the entry's display text)
/// into top-level blocks. `base` makes every piece's offset absolute.
pub fn parse(source: &str, base: usize) -> Vec<MdBlock> {
    let mut stack = vec![Container::Root { blocks: Vec::new() }];
    let mut inline: Vec<Inline> = Vec::new();
    let mut link_url: Option<String> = None;

    for (event, range) in Parser::new_ext(source, options()).into_offset_iter() {
        match event {
            Event::Start(tag) => match tag {
                // A block-level start closes any implicit paragraph a tight list item opened.
                Tag::Paragraph
                | Tag::Heading { .. }
                | Tag::List(_)
                | Tag::Item
                | Tag::BlockQuote(_)
                | Tag::Table(_)
                | Tag::TableHead
                | Tag::TableRow
                | Tag::TableCell
                | Tag::CodeBlock(_) => {
                    flush_open_pieces(&mut stack);
                    match tag {
                        Tag::Paragraph => {
                            stack.push(Container::Pieces(Vec::new(), PiecesKind::Paragraph))
                        }
                        Tag::Heading { level, .. } => stack.push(Container::Pieces(
                            Vec::new(),
                            PiecesKind::Heading(level_number(level)),
                        )),
                        Tag::List(start) => stack.push(Container::List {
                            ordered: start,
                            items: Vec::new(),
                        }),
                        Tag::Item => stack.push(Container::Item { blocks: Vec::new() }),
                        Tag::BlockQuote(_) => stack.push(Container::Quote { blocks: Vec::new() }),
                        Tag::Table(_) => stack.push(Container::Table {
                            head: None,
                            rows: Vec::new(),
                            in_head: false,
                        }),
                        Tag::TableHead => {
                            set_in_head(&mut stack, true);
                            stack.push(Container::Row { cells: Vec::new() })
                        }
                        Tag::TableRow => stack.push(Container::Row { cells: Vec::new() }),
                        Tag::TableCell => {
                            stack.push(Container::Pieces(Vec::new(), PiecesKind::Cell))
                        }
                        Tag::CodeBlock(_) => {
                            stack.push(Container::Pieces(Vec::new(), PiecesKind::Code))
                        }
                        _ => unreachable!(),
                    }
                }
                Tag::Emphasis => inline.push(Inline::Italic),
                Tag::Strong => inline.push(Inline::Bold),
                Tag::Strikethrough => inline.push(Inline::Plain),
                Tag::Link { dest_url, .. } => {
                    inline.push(Inline::LinkText);
                    link_url = Some(dest_url.into_string());
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => finish_pieces(&mut stack),
                TagEnd::Heading(_) => finish_pieces(&mut stack),
                TagEnd::List(_) => {
                    flush_open_pieces(&mut stack);
                    if let Some(Container::List { ordered, items }) = stack.pop() {
                        push_block(&mut stack, MdBlock::List { ordered, items });
                    }
                }
                TagEnd::Item => {
                    flush_open_pieces(&mut stack);
                    if let Some(Container::Item { blocks }) = stack.pop() {
                        push_item(&mut stack, blocks);
                    }
                }
                TagEnd::BlockQuote(_) => {
                    flush_open_pieces(&mut stack);
                    if let Some(Container::Quote { blocks }) = stack.pop() {
                        push_block(&mut stack, MdBlock::Quote(blocks));
                    }
                }
                TagEnd::Table => {
                    if let Some(Container::Table { head, rows, .. }) = stack.pop()
                        && let Some(head) = head
                    {
                        push_block(&mut stack, MdBlock::Table { head, rows });
                    }
                }
                TagEnd::TableHead => {
                    if let Some(Container::Row { cells }) = stack.pop() {
                        set_head(&mut stack, cells);
                    }
                    set_in_head(&mut stack, false);
                }
                TagEnd::TableRow => {
                    if let Some(Container::Row { cells }) = stack.pop() {
                        push_row(&mut stack, cells);
                    }
                }
                TagEnd::TableCell => finish_pieces(&mut stack),
                TagEnd::CodeBlock => finish_pieces(&mut stack),
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    inline.pop();
                }
                TagEnd::Link => {
                    inline.pop();
                    if let Some(url) = link_url.take() {
                        // The URL follows the text in parentheses (ADR 0028 R2: no OSC 8).
                        append(
                            &mut stack,
                            Piece::styled(format!(" ({url})"), Inline::LinkUrl, None),
                        );
                    }
                }
                _ => {}
            },
            Event::Text(text) => {
                let style = inline.last().copied().unwrap_or(Inline::Plain);
                append(
                    &mut stack,
                    Piece::styled(text.into_string(), style, Some(base + range.start)),
                );
            }
            Event::Code(text) => append(
                &mut stack,
                Piece::styled(text.into_string(), Inline::Code, Some(base + range.start)),
            ),
            Event::SoftBreak | Event::HardBreak => {
                // A table cell cannot hold a line break: it becomes a space there.
                let in_cell = matches!(stack.last(), Some(Container::Pieces(_, PiecesKind::Cell)));
                let text = if in_cell {
                    " ".to_owned()
                } else {
                    "\n".to_owned()
                };
                append(&mut stack, Piece::plain(text, Some(base + range.start)));
            }
            Event::Rule => push_block(&mut stack, MdBlock::Rule),
            Event::Html(text) | Event::InlineHtml(text) => append(
                &mut stack,
                Piece::plain(text.into_string(), Some(base + range.start)),
            ),
            _ => {}
        }
    }
    match stack.pop() {
        Some(Container::Root { blocks }) => blocks,
        _ => Vec::new(),
    }
}

/// The container stack: block containers hold finished blocks, leaf containers collect
/// pieces until their `End`.
enum Container {
    Root {
        blocks: Vec<MdBlock>,
    },
    List {
        ordered: Option<u64>,
        items: Vec<Vec<MdBlock>>,
    },
    Item {
        blocks: Vec<MdBlock>,
    },
    Quote {
        blocks: Vec<MdBlock>,
    },
    Table {
        head: Option<Vec<Vec<Piece>>>,
        rows: Vec<Vec<Vec<Piece>>>,
        in_head: bool,
    },
    Row {
        cells: Vec<Vec<Piece>>,
    },
    Pieces(Vec<Piece>, PiecesKind),
}

#[derive(Clone, Copy)]
enum PiecesKind {
    Paragraph,
    Heading(u8),
    Cell,
    Code,
}

/// Appends a piece to the innermost leaf (a paragraph, a heading, a cell or an indented code
/// block). A piece outside any leaf is dropped.
fn append(stack: &mut Vec<Container>, piece: Piece) {
    for container in stack.iter_mut().rev() {
        if let Container::Pieces(pieces, _) = container {
            pieces.push(piece);
            return;
        }
    }
    // A tight list item has no `Paragraph` tag: its text opens an implicit one, which the
    // enclosing `End(Item)` flushes.
    stack.push(Container::Pieces(vec![piece], PiecesKind::Paragraph));
}

/// Flushes any open leaf container into its enclosing block container (a tight list item's
/// implicit paragraph has no `End` of its own).
fn flush_open_pieces(stack: &mut Vec<Container>) {
    while matches!(
        stack.last(),
        Some(Container::Pieces(_, PiecesKind::Paragraph))
    ) {
        finish_pieces(stack);
    }
}

/// Ends a leaf container: its pieces become a block, pushed into the enclosing container.
fn finish_pieces(stack: &mut Vec<Container>) {
    if !matches!(stack.last(), Some(Container::Pieces(..))) {
        return;
    }
    let Some(Container::Pieces(pieces, actual)) = stack.pop() else {
        return;
    };
    match actual {
        PiecesKind::Cell => push_cell(stack, pieces),
        PiecesKind::Heading(level) => push_block(stack, MdBlock::Heading { level, pieces }),
        PiecesKind::Paragraph | PiecesKind::Code => {
            if !pieces.is_empty() {
                push_block(stack, MdBlock::Paragraph(pieces));
            }
        }
    }
}

/// Pushes a finished block into the innermost block container.
fn push_block(stack: &mut [Container], block: MdBlock) {
    for container in stack.iter_mut().rev() {
        match container {
            Container::Item { blocks }
            | Container::Quote { blocks }
            | Container::Root { blocks } => {
                blocks.push(block);
                return;
            }
            Container::List { items, .. } => {
                // A block directly in a list (pulldown always wraps items, so this is rare):
                // its own item.
                items.push(vec![block]);
                return;
            }
            _ => {}
        }
    }
}

/// A finished list item: its blocks become the enclosing list's next item.
fn push_item(stack: &mut [Container], blocks: Vec<MdBlock>) {
    for container in stack.iter_mut().rev() {
        if let Container::List { items, .. } = container {
            items.push(blocks);
            return;
        }
    }
}

/// A finished table row: a header row becomes the table's head, a body row its rows.
fn push_row(stack: &mut [Container], cells: Vec<Vec<Piece>>) {
    for container in stack.iter_mut().rev() {
        if let Container::Table {
            head,
            rows,
            in_head,
        } = container
        {
            if *in_head && head.is_none() {
                *head = Some(cells);
            } else {
                rows.push(cells);
            }
            return;
        }
    }
}

/// A finished table cell: appended to the innermost open row.
fn push_cell(stack: &mut [Container], pieces: Vec<Piece>) {
    for container in stack.iter_mut().rev() {
        if let Container::Row { cells } = container {
            cells.push(pieces);
            return;
        }
    }
}

/// The header row's cells become the table's head.
fn set_head(stack: &mut [Container], cells: Vec<Vec<Piece>>) {
    for container in stack.iter_mut().rev() {
        if let Container::Table { head, .. } = container {
            *head = Some(cells);
            return;
        }
    }
}

fn set_in_head(stack: &mut [Container], value: bool) {
    for container in stack.iter_mut().rev() {
        if let Container::Table { in_head, .. } = container {
            *in_head = value;
            return;
        }
    }
}

fn level_number(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(pieces: &[Piece]) -> String {
        pieces.iter().map(|piece| piece.text.as_str()).collect()
    }

    #[test]
    fn blocks_parse_with_offsets() {
        let source = "# Title\n\nA **bold** and `code` [link](https://x).\n\n- one\n- two\n\n> quoted\n\n---\n";
        let blocks = parse(source, 100);
        assert!(matches!(blocks[0], MdBlock::Heading { level: 1, .. }));
        let MdBlock::Heading { pieces, .. } = &blocks[0] else {
            unreachable!()
        };
        assert_eq!(text(pieces), "Title");
        assert_eq!(pieces[0].offset, Some(102), "`# ` is markup, `T` is at 102");
        let MdBlock::Paragraph(pieces) = &blocks[1] else {
            panic!("{blocks:?}")
        };
        assert_eq!(text(pieces), "A bold and code link (https://x).");
        assert_eq!(pieces[1].inline, Inline::Bold);
        assert_eq!(pieces[3].inline, Inline::Code);
        assert_eq!(pieces[5].inline, Inline::LinkText);
        assert_eq!(pieces[6].inline, Inline::LinkUrl);
        assert_eq!(pieces[6].offset, None, "the URL is synthesized");
        let MdBlock::List { ordered, items } = &blocks[2] else {
            panic!("{blocks:?}")
        };
        assert_eq!(*ordered, None);
        assert_eq!(items.len(), 2);
        assert!(matches!(blocks[3], MdBlock::Quote(_)));
        assert!(matches!(blocks[4], MdBlock::Rule));
    }

    #[test]
    fn nested_lists_and_tables_parse() {
        let source = "1. a\n   - b\n   - c\n2. d\n\n| h1 | h2 |\n| --- | --- |\n| a | b |\n";
        let blocks = parse(source, 0);
        let MdBlock::List { ordered, items } = &blocks[0] else {
            panic!("{blocks:?}")
        };
        assert_eq!(*ordered, Some(1));
        assert_eq!(items.len(), 2);
        assert!(
            matches!(items[0].last(), Some(MdBlock::List { .. })),
            "the nested list is the first item's child: {:?}",
            items[0]
        );
        let MdBlock::Table { head, rows } = &blocks[1] else {
            panic!("{blocks:?}")
        };
        assert_eq!(head.len(), 2);
        assert_eq!(rows.len(), 1);
        assert_eq!(text(&head[0]), "h1");
    }

    #[test]
    fn an_unclosed_list_still_parses() {
        // The streaming case: pulldown is happy with a partial document.
        let blocks = parse("- one\n- two", 0);
        let MdBlock::List { items, .. } = &blocks[0] else {
            panic!("{blocks:?}")
        };
        assert_eq!(items.len(), 2);
    }
}
