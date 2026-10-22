//! The Markdown table reader the user-manual drift tests share.
//!
//! A check names the heading its table sits under. The heading is the anchor: from it to the
//! next heading at the same or a higher level, every table (a run of `|` lines, header and
//! separator dropped) is returned, and fenced code blocks are skipped. A table elsewhere on
//! the page is never read.
//!
//! This is test support: the `testing` feature arms the module, so a production build does
//! not compile it. The four crates that check `docs/user/` already depend on
//! `radian-coding`, so one reader serves all of them instead of a copy per crate.

/// One Markdown table's data rows, each split into trimmed cells.
pub type Table = Vec<Vec<String>>;

/// Whether `line` opens or closes a fenced code block.
fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// Split a table row on unescaped `|` only: value cells escape theirs as `\|`. The cells are
/// trimmed, and the empty cells around the outer `|`s are dropped.
pub fn split_cells(row: &str) -> Vec<String> {
    let mut cells = vec![String::new()];
    let mut chars = row.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'|') {
            cells.last_mut().expect("a cell is open").push('|');
            chars.next();
        } else if c == '|' {
            cells.push(String::new());
        } else {
            cells.last_mut().expect("a cell is open").push(c);
        }
    }
    if cells.first().is_some_and(|cell| cell.trim().is_empty()) {
        cells.remove(0);
    }
    if cells.last().is_some_and(|cell| cell.trim().is_empty()) {
        cells.pop();
    }
    cells
        .into_iter()
        .map(|cell| cell.trim().to_owned())
        .collect()
}

/// The line index and level of the first heading that starts with `prefix`, outside a fenced
/// code block.
fn heading_at(doc: &str, prefix: &str) -> Option<(usize, usize)> {
    let mut fenced = false;
    for (index, line) in doc.lines().enumerate() {
        if is_fence(line) {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let level = line.chars().take_while(|c| *c == '#').count();
        if level > 0 && line.starts_with(prefix) {
            return Some((index, level));
        }
    }
    None
}

/// Every data table under the heading that starts with `prefix`, from that heading to the
/// next heading at the same or a higher level. The heading is the table's anchor: a table
/// outside it is never read by a check. A missing heading, or a heading with no table, fails
/// with `page` and the heading.
pub fn tables_under(doc: &str, page: &str, prefix: &str) -> Vec<Table> {
    let (start, level) = heading_at(doc, prefix).unwrap_or_else(|| {
        panic!("{page} has no heading `{prefix}`; the check's table anchor is missing")
    });
    let mut tables = Vec::new();
    let mut rows: Option<Table> = None;
    let mut fenced = false;
    for line in doc.lines().skip(start + 1) {
        if is_fence(line) {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let line_level = line.chars().take_while(|c| *c == '#').count();
        if line_level > 0 && line_level <= level {
            break;
        }
        if line.starts_with('|') {
            rows.get_or_insert_with(Vec::new).push(split_cells(line));
        } else if let Some(mut table) = rows.take() {
            // The first row is the header and the second the separator.
            table.drain(..2.min(table.len()));
            if !table.is_empty() {
                tables.push(table);
            }
        }
    }
    if let Some(mut table) = rows {
        table.drain(..2.min(table.len()));
        if !table.is_empty() {
            tables.push(table);
        }
    }
    assert!(
        !tables.is_empty(),
        "{page}'s `{prefix}` heading has no table"
    );
    tables
}

/// The first table under `prefix`.
pub fn table_under(doc: &str, page: &str, prefix: &str) -> Table {
    tables_under(doc, page, prefix).swap_remove(0)
}
