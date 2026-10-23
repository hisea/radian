//! `@` file references and path completion (Phase 4 composer).
//!
//! A user message may reference workspace files with `@relative/path`. The composer (a
//! separate slice) will offer completion; this module is the pure part: which words are
//! references, how they expand into the message the model sees, how a sent message is folded
//! back for display, and what completions exist for a word at the cursor.
//!
//! The rules (decided in the brief):
//!
//! * A reference is an `@` at the start of the line or after whitespace, followed by a
//!   non-empty run of non-whitespace characters. `a@b.com` is an email, not a reference.
//!   Trailing punctuation (`,`, `.`, `;`, `:`, `!`, `?`, `)`, `]`, `}`, `>`, quotes) is not
//!   part of the path, so `see @a.rs,` references `a.rs`.
//! * A reference expands only when it resolves to an existing **regular file** inside the
//!   workspace. Anything else (`@someone`, a directory) is left as written and is not an
//!   error. A candidate that resolves *outside* the workspace (an absolute path, `..`, or a
//!   symlink that leaves the workspace) is not expanded and produces a warning, because the
//!   user probably meant a file and the path is wrong.
//! * An existing file that is binary (invalid UTF-8 or a NUL byte) or over the size limits
//!   makes the whole message **unsendable**: the caller keeps the input and shows the
//!   reasons. Limits are per file [`MAX_FILE_BYTES`] and in total [`MAX_TOTAL_BYTES`].
//! * The expanded message keeps the user's text unchanged and appends one
//!   `<file path="…">…</file>` block per file, in first-reference order, once per file.
//!
//! Everything here is a pure function of its arguments plus the filesystem reads `expand`
//! performs; nothing touches the terminal.

use std::borrow::Cow;
use std::io::Read;

use radian_coding::image::{self, ImageError, LoadedImage};
use radian_core::{ContentPart, Message, MessageMeta};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

/// The largest single file that may be attached (100 KiB).
pub const MAX_FILE_BYTES: usize = 100 * 1024;

/// The largest total of attached file contents (400 KiB).
pub const MAX_TOTAL_BYTES: usize = 400 * 1024;

/// The bounded head read for an absolute (dropped or typed) path: enough for every magic
/// number and for `imagesize` to read an ordinary header. The rest is read only after the
/// metadata size check passes, so a huge video or log is never allocated just to be refused.
const DROPPED_HEAD_BYTES: usize = 64 * 1024;

/// How many completions are offered at most.
pub const MAX_CANDIDATES: usize = 50;

/// Characters stripped from the end of a candidate: `see @a.rs,` references `a.rs`, not
/// `a.rs,`.
const TRAILING_PUNCTUATION: &[char] = &[
    ',', '.', ';', ':', '!', '?', ')', ']', '}', '>', '"', '\'', '`',
];

/// Entries never offered by completion (nor, in practice, attached: they are directories).
/// `.radian-worktrees/` is the workspace-local worktree root; arc's own checkouts are the
/// same thing for its workers and gate stages, so only those directories are hidden and the
/// rest of `.radian/` stays offerable.
const ALWAYS_HIDDEN: &[&str] = &[".git", ".radian-sandbox", ".radian-worktrees"];
/// arc's checkout roots, workspace-relative (ADR 0048 decision 15): one per worker task and
/// one per gate stage.
const ARC_CHECKOUT_ROOTS: &[&str] = &[".radian/arc/worktrees", ".radian/arc/gates"];

/// One attached file: its workspace-relative path and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub path: String,
    pub content: String,
}

/// A message after expansion: the user's text (attachments are **not** spliced into it), the
/// files attached in first-reference order, the images attached by `@` or by a dropped path
/// (the dropped path itself is removed from `text`), and non-fatal warnings to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    pub text: String,
    pub attachments: Vec<Attachment>,
    pub images: Vec<LoadedImage>,
    pub warnings: Vec<String>,
}

impl Expansion {
    /// The attached paths and image names, for the notice.
    pub fn attached(&self) -> Vec<&str> {
        self.attachments
            .iter()
            .map(|attachment| attachment.path.as_str())
            .chain(self.images.iter().map(|image| image.name.as_str()))
            .collect()
    }
}

/// The message one send carries (ADR 0018): the user's text as one part, one
/// [`ContentPart::Image`] per image, then one [`ContentPart::attachment`] part per file. The
/// provider sees the same text it always did (joined), but the parts are separate, so Core can
/// replace an attachment with a placeholder when it keeps the user's words verbatim.
pub fn user_message(text: &str, attachments: &[Attachment], images: &[LoadedImage]) -> Message {
    // An image-only message (a dropped path is removed from the text) has no text part: an
    // empty `Text` part would be sent to the adapters and draw a blank transcript line.
    let mut content = Vec::new();
    if !text.trim().is_empty() {
        content.push(ContentPart::text(text));
    }
    content.extend(images.iter().map(LoadedImage::part));
    for attachment in attachments {
        content.push(ContentPart::attachment(
            attachment.content.clone(),
            attachment.path.clone(),
        ));
    }
    Message::User {
        content,
        meta: MessageMeta::default(),
    }
}

/// The display text of a user message's parts: an attachment part becomes
/// `[file: P, N lines]`, an image part `[image: <name>, W×H, N KB]` (ADR 0028 R5); neither
/// ever shows its content.
pub fn display_parts(parts: &[ContentPart]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text, .. } => match part.attachment_path() {
                Some(path) => lines.push(format!("[file: {path}, {} lines]", text.lines().count())),
                None => lines.push(text.clone()),
            },
            ContentPart::Image { .. } => {
                if let Some(line) = image::part_placeholder(part) {
                    lines.push(line);
                }
            }
            _ => {}
        }
    }
    lines.join("\n\n")
}

/// One referenced file that exists but cannot be attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub path: String,
    pub reason: String,
}

/// Why a message was not expanded. The message must not be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpandError {
    Unusable(Vec<Rejection>),
}

impl std::fmt::Display for ExpandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExpandError::Unusable(rejections) => {
                write!(f, "unusable attachment(s): ")?;
                for (index, rejection) in rejections.iter().enumerate() {
                    if index > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{}: {}", rejection.path, rejection.reason)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ExpandError {}

/// The completion for the word at the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The char range to replace: the text after `@` up to the cursor.
    pub range: Range<usize>,
    /// Relative paths (directories end in `/`), sorted, at most [`MAX_CANDIDATES`].
    pub candidates: Vec<String>,
    /// The longest common prefix of `candidates` (empty when there are none).
    pub common_prefix: String,
}

/// Expands the `@` references in `text` against `workspace`.
///
/// `Ok` when nothing was referenced, or every referenced existing file was attached (the
/// warnings, if any, are escapes that were left as written). `Err` when a referenced file
/// exists but cannot be attached; the caller must keep the input and not send.
pub fn expand(text: &str, workspace: &Path) -> Result<Expansion, ExpandError> {
    let root = std::fs::canonicalize(workspace).ok();
    let root_dir = root
        .as_deref()
        .and_then(|root| radian_coding::safe_fs::RootDir::open(root).ok());
    let mut attachments: Vec<Attachment> = Vec::new();
    let mut images: Vec<LoadedImage> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut rejections: Vec<Rejection> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut total = 0usize;

    // Q7: an absolute path to an existing image, dropped or typed as a whole token, becomes an
    // image attachment and the path leaves the text: the host reads the bytes, the model gets
    // the image and not the path. This does not go through the sandbox or the read roots.
    let mut text = text.to_string();
    let mut removals: Vec<Range<usize>> = Vec::new();
    for dropped in dropped_paths(&text) {
        match load_dropped(&dropped.path) {
            DroppedImage::Loaded(image) => {
                images.push(image);
                removals.push(dropped.range);
            }
            DroppedImage::Unusable(reason) => rejections.push(Rejection {
                path: dropped.path.display().to_string(),
                reason,
            }),
            DroppedImage::NotImage => {
                // A missing path, or an existing non-image: it stays as written.
            }
        }
    }
    for range in removals.into_iter().rev() {
        remove_range(&mut text, range);
    }

    for candidate in candidates(text.as_str()) {
        match resolve(&candidate, workspace, root.as_deref()) {
            Resolve::File(path) => {
                // The same file reached by two spellings is attached once.
                if seen.contains(&path) {
                    continue;
                }
                // Check the text size before reading it: a huge text file must not be read
                // into memory just to be rejected. An image may be larger (up to its own 5
                // MiB limit), so the magic number is read first below.
                let metadata = std::fs::metadata(&path).ok();
                let total_bytes = metadata.map(|meta| meta.len()).unwrap_or(0);
                // ADR 0039 decision 2: the file is opened through the workspace root's
                // directory fd (`openat(O_NOFOLLOW)` per component), never by the path
                // string the resolve step checked; a symlink anywhere in it is a rejection.
                let file = match root_dir
                    .as_ref()
                    .ok_or_else(|| "the workspace root cannot be opened".to_string())
                    .and_then(|root_dir| {
                        let rel: PathBuf =
                            radian_coding::safe_fs::components(Path::new(&candidate))
                                .map(|parts| parts.iter().collect())
                                .map_err(|error| error.to_string())?;
                        root_dir
                            .file(&rel, radian_coding::safe_fs::OpenMode::Read)
                            .map_err(|error| error.to_string())
                    }) {
                    Ok(file) => file,
                    Err(reason) => {
                        rejections.push(Rejection {
                            path: candidate,
                            reason,
                        });
                        continue;
                    }
                };
                let mut reader = &file;
                let mut head = [0u8; 12];
                let mut filled = 0usize;
                let mut read_failed = false;
                while filled < head.len() {
                    match std::io::Read::read(&mut reader, &mut head[filled..]) {
                        Ok(0) => break,
                        Ok(n) => filled += n,
                        Err(error) => {
                            rejections.push(Rejection {
                                path: candidate.clone(),
                                reason: error.to_string(),
                            });
                            read_failed = true;
                            break;
                        }
                    }
                }
                if read_failed {
                    continue;
                }
                // The kind comes from the magic number, never the extension (Q3).
                if image::ImageKind::from_magic(&head[..filled]).is_some() {
                    if total_bytes > image::MAX_IMAGE_BYTES {
                        rejections.push(Rejection {
                            path: candidate,
                            reason: format!(
                                "{total_bytes} bytes, over the {} MiB image limit",
                                image::MAX_IMAGE_BYTES / (1024 * 1024)
                            ),
                        });
                        continue;
                    }
                    let mut bytes = head[..filled].to_vec();
                    if let Err(error) = std::io::Read::read_to_end(&mut reader, &mut bytes) {
                        rejections.push(Rejection {
                            path: candidate,
                            reason: error.to_string(),
                        });
                        continue;
                    }
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| candidate.clone());
                    match image::load(&name, &bytes) {
                        Ok(image) => {
                            seen.push(path);
                            images.push(image);
                        }
                        // The metadata said image, the bytes say otherwise (a short read or a
                        // swapped file): keep the existing binary rejection.
                        Err(ImageError::NotImage) => rejections.push(Rejection {
                            path: candidate,
                            reason: "binary (not valid UTF-8)".into(),
                        }),
                        Err(error) => rejections.push(Rejection {
                            path: candidate,
                            reason: error.to_string(),
                        }),
                    }
                    continue;
                }
                if total_bytes > MAX_FILE_BYTES as u64 {
                    rejections.push(Rejection {
                        path: candidate,
                        reason: format!(
                            "{total_bytes} bytes, over the {} KiB per-file limit",
                            MAX_FILE_BYTES / 1024
                        ),
                    });
                    continue;
                }
                let mut bytes = head[..filled].to_vec();
                if let Err(error) = std::io::Read::read_to_end(&mut reader, &mut bytes) {
                    rejections.push(Rejection {
                        path: candidate,
                        reason: error.to_string(),
                    });
                    continue;
                }
                let content = match String::from_utf8(bytes) {
                    Ok(content) if !content.contains('\0') => content,
                    Ok(_) => {
                        rejections.push(Rejection {
                            path: candidate,
                            reason: "binary (contains a NUL byte)".into(),
                        });
                        continue;
                    }
                    Err(_) => {
                        rejections.push(Rejection {
                            path: candidate,
                            reason: "binary (not valid UTF-8)".into(),
                        });
                        continue;
                    }
                };
                if content.len() > MAX_FILE_BYTES {
                    rejections.push(Rejection {
                        path: candidate,
                        reason: format!(
                            "{} bytes, over the {} KiB per-file limit",
                            content.len(),
                            MAX_FILE_BYTES / 1024
                        ),
                    });
                    continue;
                }
                if total + content.len() > MAX_TOTAL_BYTES {
                    rejections.push(Rejection {
                        path: candidate,
                        reason: format!(
                            "the attachments would exceed the {} KiB total limit",
                            MAX_TOTAL_BYTES / 1024
                        ),
                    });
                    continue;
                }
                seen.push(path);
                total += content.len();
                attachments.push(Attachment {
                    path: candidate,
                    content,
                });
            }
            Resolve::Escape => warnings.push(format!(
                "@{candidate} resolves outside the workspace, so it was not attached"
            )),
            // Not found, a directory, or a workspace that cannot be resolved: not a
            // reference, left as written, not an error.
            Resolve::NotAFile => {}
        }
    }

    if !rejections.is_empty() {
        return Err(ExpandError::Unusable(rejections));
    }
    Ok(Expansion {
        text,
        attachments,
        images,
        warnings,
    })
}

/// The candidate references in `text`, in order, with trailing punctuation removed.
fn candidates(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let at_start = i == 0 || chars[i - 1].is_whitespace();
        if chars[i] == '@' && at_start {
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && !chars[end].is_whitespace() {
                end += 1;
            }
            let mut word: String = chars[start..end].iter().collect();
            while word
                .chars()
                .last()
                .is_some_and(|c| TRAILING_PUNCTUATION.contains(&c))
            {
                word.pop();
            }
            if !word.is_empty() {
                out.push(word);
            }
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

enum Resolve {
    File(PathBuf),
    Escape,
    NotAFile,
}

/// Resolves one candidate the way the `read` tool resolves a path: reject an absolute path
/// or `..` before touching the filesystem, then canonicalize and require the result to stay
/// under the canonical workspace root.
fn resolve(candidate: &str, workspace: &Path, root: Option<&Path>) -> Resolve {
    let relative = Path::new(candidate);
    if relative.is_absolute()
        || relative.components().any(|c| {
            matches!(
                c,
                Component::RootDir | Component::Prefix(_) | Component::ParentDir
            )
        })
    {
        return Resolve::Escape;
    }
    let Some(root) = root else {
        return Resolve::NotAFile;
    };
    let Ok(real) = std::fs::canonicalize(workspace.join(relative)) else {
        return Resolve::NotAFile;
    };
    if !real.starts_with(root) {
        return Resolve::Escape;
    }
    match std::fs::metadata(&real) {
        Ok(metadata) if metadata.is_file() => Resolve::File(real),
        _ => Resolve::NotAFile,
    }
}

/// One absolute path token found in the composer text: the char range it occupies (quotes
/// included) and the path after unescaping and `~` expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedPath {
    pub range: Range<usize>,
    pub path: PathBuf,
}

/// What reading a dropped path produced (Q7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DroppedImage {
    Loaded(LoadedImage),
    /// The path exists and has an image magic number, but a limit (or the header) refused it.
    Unusable(String),
    /// Missing, unreadable, or not a PNG/JPEG/GIF/WebP: the text stays as written.
    NotImage,
}

/// The absolute path token that ends at `cursor` (a trailing run of whitespace is allowed),
/// if any. Pure: the shape only; [`load_dropped`] reads the bytes.
pub fn dropped_path(text: &str, cursor: usize) -> Option<DroppedPath> {
    let last = dropped_paths(text).into_iter().last()?;
    if cursor < last.range.end {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    if chars[last.range.end.min(chars.len())..cursor.min(chars.len())]
        .iter()
        .any(|c| !c.is_whitespace())
    {
        return None;
    }
    Some(last)
}

/// Every absolute path token in `text`. Terminals insert a dropped path in different shapes:
/// backslash-escaped spaces, a single- or double-quoted path, and often a trailing space. All
/// are recognised; relative paths are not (a dropped path is absolute). Quotes and escapes are
/// removed from the path and included in the range.
pub fn dropped_paths(text: &str) -> Vec<DroppedPath> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let start = i;
        let (value, end) = if chars[i] == '\'' || chars[i] == '"' {
            let quote = chars[i];
            let mut j = i + 1;
            let mut raw = String::new();
            while j < chars.len() && chars[j] != quote {
                raw.push(chars[j]);
                j += 1;
            }
            let end = if j < chars.len() { j + 1 } else { j };
            (unescape(&raw), end)
        } else {
            let mut j = i;
            let mut raw = String::new();
            while j < chars.len() && !chars[j].is_whitespace() {
                if chars[j] == '\\' && j + 1 < chars.len() {
                    raw.push(chars[j]);
                    raw.push(chars[j + 1]);
                    j += 2;
                } else {
                    raw.push(chars[j]);
                    j += 1;
                }
            }
            (unescape(&raw), j)
        };
        // A dropped path is absolute. A single leading slash names a root-level path but is
        // also how every slash command starts, so it is not taken as a drop; a real dropped
        // path has another separator or the `~/` prefix.
        let absolute =
            value.starts_with("~/") || (value.starts_with('/') && value[1..].contains('/'));
        if absolute {
            out.push(DroppedPath {
                range: start..end,
                path: expand_tilde(&value),
            });
        }
        i = end.max(i + 1);
    }
    out
}

/// Reads one dropped path with the host's own bytes: outside the workspace is accepted (Q7),
/// the model gets the image and not the path, and the sandbox and read roots do not change.
/// Only a bounded head is read first: a non-image ends there, and an over-limit image is
/// refused from its metadata before its body is touched.
pub fn load_dropped(path: &Path) -> DroppedImage {
    let Ok(mut file) = std::fs::File::open(path) else {
        return DroppedImage::NotImage;
    };
    let Ok(meta) = file.metadata() else {
        return DroppedImage::NotImage;
    };
    if !meta.is_file() {
        return DroppedImage::NotImage;
    }
    let total = meta.len();
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let mut head = Vec::with_capacity(DROPPED_HEAD_BYTES.min(total as usize));
    let mut buf = [0u8; 8192];
    while head.len() < DROPPED_HEAD_BYTES {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                note_read(n);
                head.extend_from_slice(&buf[..n]);
            }
            Err(_) => return DroppedImage::NotImage,
        }
    }
    if image::ImageKind::from_magic(&head).is_none() {
        return DroppedImage::NotImage;
    }
    // A candidate image: the byte limit comes from metadata, before any body read.
    if total > image::MAX_IMAGE_BYTES {
        return DroppedImage::Unusable(ImageError::TooLarge { bytes: total }.to_string());
    }
    // When the head already holds the dimensions, a per-side over-limit is refused here too.
    if let Some((width, height)) = image::header_dimensions(&head)
        && (width > image::MAX_IMAGE_SIDE || height > image::MAX_IMAGE_SIDE)
    {
        return DroppedImage::Unusable(ImageError::TooManyPixels { width, height }.to_string());
    }
    // The accepted image (at most 5 MiB by the check above): read the rest. The cap also holds
    // if the file grew after the metadata read.
    let want = total.min(image::MAX_IMAGE_BYTES) as usize;
    let mut bytes = head;
    bytes.reserve(want.saturating_sub(bytes.len()));
    while bytes.len() < want {
        let room = (want - bytes.len()).min(buf.len());
        match file.read(&mut buf[..room]) {
            Ok(0) => break,
            Ok(n) => {
                note_read(n);
                bytes.extend_from_slice(&buf[..n]);
            }
            Err(_) => return DroppedImage::NotImage,
        }
    }
    if bytes.len() as u64 > image::MAX_IMAGE_BYTES {
        return DroppedImage::Unusable(
            ImageError::TooLarge {
                bytes: bytes.len() as u64,
            }
            .to_string(),
        );
    }
    match image::load(&name, &bytes) {
        Ok(image) => DroppedImage::Loaded(image),
        Err(ImageError::NotImage) => DroppedImage::NotImage,
        Err(error) => DroppedImage::Unusable(error.to_string()),
    }
}

// Test-only count of the bytes `load_dropped` reads, so a test can prove a huge non-image is
// not read whole. Production builds compile the hook out.
#[cfg(test)]
std::thread_local! {
    static DROPPED_BYTES_READ: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_read_bytes() {
    DROPPED_BYTES_READ.with(|bytes| bytes.set(0));
}

#[cfg(test)]
fn read_bytes() -> usize {
    DROPPED_BYTES_READ.with(std::cell::Cell::get)
}

fn note_read(n: usize) {
    #[cfg(test)]
    DROPPED_BYTES_READ.with(|bytes| bytes.set(bytes.get() + n));
    #[cfg(not(test))]
    let _ = n;
}

fn expand_tilde(value: &str) -> PathBuf {
    if (value == "~" || value.starts_with("~/"))
        && let Some(home) = std::env::var_os("HOME")
    {
        let rest = value.trim_start_matches('~');
        return PathBuf::from(home).join(rest.trim_start_matches('/'));
    }
    PathBuf::from(value)
}

fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(next) if matches!(next, ' ' | '\'' | '"' | '\\') => out.push(next),
                Some(next) => {
                    out.push('\\');
                    out.push(next);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn remove_range(text: &mut String, range: Range<usize>) {
    let mut chars: Vec<char> = text.chars().collect();
    let start = range.start.min(chars.len());
    let end = range.end.min(chars.len());
    chars.drain(start..end);
    *text = chars.into_iter().collect();
}

/// Folds every `<file path="P">…</file>` block in a message into one `[file: P, N lines]`
/// line. Display only: the message itself is unchanged.
pub fn fold_files(text: &str) -> Cow<'_, str> {
    const OPEN: &str = "<file path=\"";
    const CLOSE: &str = "</file>";
    if !text.contains(OPEN) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        let Some(quote) = after.find('"') else {
            out.push_str(&rest[start..]);
            return Cow::Owned(out);
        };
        let path = &after[..quote];
        let Some(gt) = after[quote + 1..].strip_prefix('>') else {
            out.push_str(&rest[start..]);
            return Cow::Owned(out);
        };
        let Some(end) = gt.find(CLOSE) else {
            out.push_str(&rest[start..]);
            return Cow::Owned(out);
        };
        let lines = gt[..end].trim_matches('\n').lines().count();
        out.push_str(&format!("[file: {path}, {lines} lines]"));
        rest = &gt[end + CLOSE.len()..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The completion for the word ending at `cursor` (a char index), or `None` when that word
/// does not start with `@`.
pub fn complete(text: &str, cursor: usize, workspace: &Path) -> Option<Completion> {
    let chars: Vec<char> = text.chars().collect();
    if cursor > chars.len() {
        return None;
    }
    let mut start = cursor;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    if start >= cursor || chars[start] != '@' {
        return None;
    }
    let prefix: String = chars[start + 1..cursor].iter().collect();
    let (dir, file_prefix) = split_prefix(&prefix);
    let candidates = list_candidates(workspace, dir, file_prefix);
    Some(Completion {
        range: (start + 1)..cursor,
        common_prefix: common_prefix(&candidates),
        candidates,
    })
}

/// Splits a completion prefix into its directory part (including the trailing `/`) and the
/// part of the name already typed.
fn split_prefix(prefix: &str) -> (&str, &str) {
    match prefix.rfind('/') {
        Some(index) => (&prefix[..=index], &prefix[index + 1..]),
        None => ("", prefix),
    }
}

fn list_candidates(workspace: &Path, dir: &str, file_prefix: &str) -> Vec<String> {
    let Ok(root) = std::fs::canonicalize(workspace) else {
        return Vec::new();
    };
    let relative = Path::new(dir);
    if relative.is_absolute()
        || relative.components().any(|c| {
            matches!(
                c,
                Component::RootDir | Component::Prefix(_) | Component::ParentDir
            )
        })
    {
        return Vec::new();
    }
    let Ok(real) = std::fs::canonicalize(workspace.join(relative)) else {
        return Vec::new();
    };
    if !real.starts_with(&root) || !real.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&real) else {
        return Vec::new();
    };
    // A hidden entry is offered only once the user has typed the leading `.`.
    let show_hidden = file_prefix.starts_with('.');
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if ALWAYS_HIDDEN.contains(&name.as_str()) {
            continue;
        }
        if name.starts_with('.') && !show_hidden {
            continue;
        }
        if !name.starts_with(file_prefix) {
            continue;
        }
        let mut candidate = format!("{dir}{name}");
        if ARC_CHECKOUT_ROOTS.contains(&candidate.as_str()) {
            continue;
        }
        if entry.path().is_dir() {
            candidate.push('/');
        }
        out.push(candidate);
    }
    out.sort();
    out.truncate(MAX_CANDIDATES);
    out
}

fn common_prefix(candidates: &[String]) -> String {
    let Some(first) = candidates.first() else {
        return String::new();
    };
    let mut prefix: Vec<char> = first.chars().collect();
    for candidate in &candidates[1..] {
        let chars: Vec<char> = candidate.chars().collect();
        let mut n = 0;
        while n < prefix.len() && n < chars.len() && prefix[n] == chars[n] {
            n += 1;
        }
        prefix.truncate(n);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A temp directory with a unique name (no dev-dependency on a tempdir crate).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir()
                .join(format!("radian-mention-{}-{label}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, relative: &str, contents: impl AsRef<[u8]>) {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ---------- recognition ----------

    #[test]
    fn only_an_at_at_a_line_start_or_after_whitespace_is_a_candidate() {
        assert_eq!(candidates("@a.rs"), ["a.rs"]);
        assert_eq!(candidates("see @a.rs now"), ["a.rs"]);
        assert_eq!(candidates("line one\n@a.rs"), ["a.rs"]);
        // An email is not a reference.
        assert!(candidates("mail a@b.com").is_empty());
        assert!(candidates("a@b.com").is_empty());
        // Two references in one line.
        assert_eq!(candidates("@a.rs and @b.rs"), ["a.rs", "b.rs"]);
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_the_path() {
        assert_eq!(candidates("see @a.rs,"), ["a.rs"]);
        assert_eq!(candidates("see @a.rs."), ["a.rs"]);
        assert_eq!(candidates("@a.rs)"), ["a.rs"]);
        assert_eq!(candidates("@a.rs;"), ["a.rs"]);
        // A `(` before `@` is not whitespace, so the `@` is not at a word start.
        assert!(candidates("(@a.rs)").is_empty());
        // A bare `@` is not a candidate.
        assert!(candidates("hello @ world").is_empty());
    }

    // ---------- expansion ----------

    #[test]
    fn a_reference_to_an_existing_file_is_attached_once() {
        let dir = TempDir::new("expand");
        dir.write("notes.txt", "hello\nworld\n");
        let expansion = expand("look at @notes.txt and @notes.txt", dir.path()).unwrap();
        assert_eq!(expansion.attached(), ["notes.txt"]);
        assert!(expansion.warnings.is_empty());
        // ADR 0018: the text is the user's own words, the file is its own part.
        assert_eq!(expansion.text, "look at @notes.txt and @notes.txt");
        assert_eq!(expansion.attachments.len(), 1);
        assert_eq!(expansion.attachments[0].path, "notes.txt");
        assert_eq!(expansion.attachments[0].content, "hello\nworld\n");
    }

    #[test]
    fn a_missing_file_or_a_directory_is_left_as_written_without_a_warning() {
        let dir = TempDir::new("not-a-file");
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let expansion = expand("hi @someone and @sub", dir.path()).unwrap();
        assert!(expansion.attached().is_empty());
        assert!(expansion.warnings.is_empty());
        assert_eq!(expansion.text, "hi @someone and @sub");
    }

    #[test]
    fn a_symlink_that_leaves_the_workspace_is_not_attached_and_warns() {
        let dir = TempDir::new("escape");
        let outside = TempDir::new("escape-outside");
        outside.write("secret.txt", "secret");
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("link.txt"),
        )
        .unwrap();
        let expansion = expand("read @link.txt", dir.path()).unwrap();
        assert!(expansion.attached().is_empty());
        assert_eq!(expansion.warnings.len(), 1, "{:?}", expansion.warnings);
        assert!(expansion.warnings[0].contains("outside the workspace"));
        assert!(expansion.attachments.is_empty());
    }

    #[test]
    fn an_absolute_path_or_a_parent_escape_warns_and_is_not_attached() {
        let dir = TempDir::new("parent-escape");
        let expansion = expand("read @../secret @/etc/hosts", dir.path()).unwrap();
        assert!(expansion.attached().is_empty());
        assert_eq!(expansion.warnings.len(), 2, "{:?}", expansion.warnings);
    }

    #[test]
    fn a_binary_file_makes_the_message_unsendable() {
        let dir = TempDir::new("binary");
        dir.write("blob.bin", [0x00, 0x01, 0xff, 0xfe]);
        let error = expand("see @blob.bin", dir.path()).unwrap_err();
        let ExpandError::Unusable(rejections) = error;
        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].path, "blob.bin");
        assert!(rejections[0].reason.contains("binary"), "{:?}", rejections);
    }

    #[test]
    fn a_file_over_the_per_file_limit_makes_the_message_unsendable() {
        let dir = TempDir::new("too-big");
        dir.write("big.txt", "x".repeat(MAX_FILE_BYTES + 1));
        let error = expand("see @big.txt", dir.path()).unwrap_err();
        let ExpandError::Unusable(rejections) = error;
        assert_eq!(rejections[0].path, "big.txt");
        assert!(
            rejections[0].reason.contains("per-file limit"),
            "{:?}",
            rejections
        );
    }

    #[test]
    fn the_total_limit_makes_the_message_unsendable() {
        let dir = TempDir::new("total");
        // Each file is under the per-file limit; together they are over the total.
        for i in 0..5 {
            dir.write(&format!("part{i}.txt"), "x".repeat(MAX_FILE_BYTES));
        }
        let error = expand(
            "@part0.txt @part1.txt @part2.txt @part3.txt @part4.txt",
            dir.path(),
        )
        .unwrap_err();
        let ExpandError::Unusable(rejections) = error;
        assert_eq!(rejections.len(), 1, "{rejections:?}");
        assert!(
            rejections[0].reason.contains("total limit"),
            "{:?}",
            rejections
        );
    }

    #[test]
    fn a_utf8_file_with_a_nul_byte_is_binary() {
        let dir = TempDir::new("nul");
        dir.write("nul.txt", b"hello\0world");
        let error = expand("@nul.txt", dir.path()).unwrap_err();
        let ExpandError::Unusable(rejections) = error;
        assert!(rejections[0].reason.contains("NUL"), "{:?}", rejections);
    }

    #[test]
    fn no_reference_means_the_text_is_unchanged() {
        let dir = TempDir::new("none");
        let expansion = expand("just words", dir.path()).unwrap();
        assert_eq!(expansion.text, "just words");
        assert!(expansion.attached().is_empty());
    }

    // ---------- images ----------

    /// A minimal PNG header with these dimensions (24 bytes).
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes
    }

    #[test]
    fn an_at_mention_of_a_png_attaches_the_image_and_shows_the_placeholder() {
        let dir = TempDir::new("mention-image");
        dir.write("shot.png", png(1200, 800));
        let expansion = expand("look at @shot.png", dir.path()).unwrap();
        assert!(expansion.attachments.is_empty());
        assert_eq!(expansion.images.len(), 1);
        assert_eq!(expansion.images[0].name, "shot.png");
        assert_eq!(expansion.text, "look at @shot.png");

        let message = user_message(&expansion.text, &expansion.attachments, &expansion.images);
        let Message::User { content, .. } = &message else {
            panic!("a user message");
        };
        assert!(
            matches!(content[1], ContentPart::Image { .. }),
            "{content:?}"
        );
        assert_eq!(
            display_parts(content),
            "look at @shot.png\n\n[image: shot.png, 1200×800, 1 KB]"
        );

        // A journal round trip keeps the same line (the annotations travel with the part).
        let stored = serde_json::to_string(&message).unwrap();
        let read_back: Message = serde_json::from_str(&stored).unwrap();
        let Message::User { content, .. } = read_back else {
            panic!("a user message");
        };
        assert_eq!(
            display_parts(&content),
            "look at @shot.png\n\n[image: shot.png, 1200×800, 1 KB]"
        );
    }

    /// An image-only message (a dropped path removed, no other text) omits the empty text part:
    /// the adapters get no empty `Text` and the transcript draws no blank line.
    #[test]
    fn an_image_only_message_has_no_empty_text_part() {
        let image = radian_coding::image::load("shot.png", &png(1200, 800)).unwrap();
        let message = user_message("   ", &[], std::slice::from_ref(&image));
        let Message::User { content, .. } = &message else {
            panic!("a user message");
        };
        assert_eq!(content.len(), 1, "{content:?}");
        assert!(matches!(content[0], ContentPart::Image { .. }));
        assert_eq!(display_parts(content), "[image: shot.png, 1200×800, 1 KB]");
    }

    #[test]
    fn the_extension_does_not_decide_and_an_image_over_the_limit_is_unusable() {
        let dir = TempDir::new("mention-kind");
        // Text named `.png` stays a text attachment.
        dir.write("notes.png", "hello\n");
        let expansion = expand("@notes.png", dir.path()).unwrap();
        assert!(expansion.images.is_empty());
        assert_eq!(expansion.attachments.len(), 1);
        // A PNG named `.txt` is an image.
        dir.write("shot.txt", png(10, 10));
        let expansion = expand("@shot.txt", dir.path()).unwrap();
        assert!(expansion.attachments.is_empty());
        assert_eq!(expansion.images.len(), 1);

        // Over 5 MiB: the message is not sendable and the reason names the limit.
        let mut huge = png(10, 10);
        huge.resize(radian_coding::MAX_IMAGE_BYTES as usize + 1, 0);
        dir.write("huge.png", &huge);
        let error = expand("@huge.png", dir.path()).unwrap_err();
        let ExpandError::Unusable(rejections) = error;
        assert!(
            rejections[0].reason.contains("5 MiB image limit"),
            "{rejections:?}"
        );
    }

    #[test]
    fn a_dropped_absolute_path_accepts_escaped_spaces_quotes_and_a_trailing_space() {
        let workspace = TempDir::new("drop-ws");
        let outside = TempDir::new("drop-outside");
        outside.write("shot one.png", png(10, 10));
        let path = outside.path().join("shot one.png");
        let escaped = path.display().to_string().replace(' ', "\\ ");
        let shapes = [
            format!("see {escaped}"),
            format!("see {escaped} "),
            format!("see '{}' ", path.display()),
            format!("see \"{}\"", path.display()),
        ];
        for text in shapes {
            let cursor = text.chars().count();
            let dropped =
                dropped_path(&text, cursor).unwrap_or_else(|| panic!("a dropped path in {text:?}"));
            assert_eq!(dropped.path, path, "{text:?}");
            // The workspace is a different directory: a dropped path is outside it and still
            // accepted (the host reads the bytes itself).
            let expansion = expand(&text, workspace.path()).unwrap();
            assert_eq!(expansion.images.len(), 1, "{text:?}");
            assert_eq!(expansion.images[0].name, "shot one.png");
            assert!(!expansion.text.contains("shot one.png"), "{text:?}");
        }
    }

    /// A huge sparse non-image must not be read whole: the head is enough to say `NotImage`.
    #[test]
    fn a_large_non_image_is_refused_from_its_head() {
        let dir = TempDir::new("drop-huge-non-image");
        let path = dir.path().join("huge.bin");
        let mut file = std::fs::File::create(&path).unwrap();
        std::io::Write::write_all(&mut file, b"not an image").unwrap();
        file.set_len(2 * 1024 * 1024 * 1024).unwrap();
        drop(file);
        reset_read_bytes();
        assert_eq!(load_dropped(&path), DroppedImage::NotImage);
        let read = read_bytes();
        assert!(
            read > 0 && read <= DROPPED_HEAD_BYTES,
            "read {read} bytes of a 2 GiB non-image"
        );
    }

    /// An over-limit image is refused from the header/metadata, never by reading its body.
    #[test]
    fn an_over_limit_image_is_refused_before_its_body_is_read() {
        let dir = TempDir::new("drop-huge-image");
        let path = dir.path().join("huge.png");
        let mut file = std::fs::File::create(&path).unwrap();
        std::io::Write::write_all(&mut file, &png(10, 10)).unwrap();
        file.set_len(radian_coding::MAX_IMAGE_BYTES + 1).unwrap();
        drop(file);
        reset_read_bytes();
        let outcome = load_dropped(&path);
        assert!(
            matches!(&outcome, DroppedImage::Unusable(reason) if reason.contains("5 MiB")),
            "{outcome:?}"
        );
        let read = read_bytes();
        assert!(read > 0, "the head must be read");
        assert!(read <= DROPPED_HEAD_BYTES, "read {read} bytes");
        assert!(
            (read as u64) < radian_coding::MAX_IMAGE_BYTES,
            "the body was read"
        );
    }

    /// A header whose dimensions are already over 8000 px is refused from the head too.
    #[test]
    fn an_over_limit_side_is_refused_from_its_header() {
        let dir = TempDir::new("drop-wide-image");
        let path = dir.path().join("wide.png");
        std::fs::write(&path, png(9000, 10)).unwrap();
        reset_read_bytes();
        let outcome = load_dropped(&path);
        assert!(
            matches!(&outcome, DroppedImage::Unusable(reason) if reason.contains("8000")),
            "{outcome:?}"
        );
        assert!(read_bytes() > 0, "the head must be read");
        assert!(read_bytes() <= DROPPED_HEAD_BYTES);
    }

    #[test]
    fn a_missing_dropped_path_stays_text_and_a_partial_one_is_not_a_drop() {
        let workspace = TempDir::new("drop-missing");
        let text = "see /no/such/dir/shot.png";
        let dropped = dropped_path(text, text.chars().count()).expect("the shape is a path");
        assert_eq!(load_dropped(&dropped.path), DroppedImage::NotImage);
        let expansion = expand(text, workspace.path()).unwrap();
        assert!(expansion.images.is_empty());
        assert_eq!(expansion.text, text);

        // The cursor before the path's end is not a completed drop.
        assert!(dropped_path(text, 8).is_none());
        // A slash command is not an absolute path drop.
        assert!(dropped_paths("/model").is_empty());
    }

    #[test]
    fn a_bmp_is_still_binary_not_an_image() {
        let dir = TempDir::new("mention-bmp");
        dir.write("old.bmp", b"BM\xFF\xFE");
        let error = expand("@old.bmp", dir.path()).unwrap_err();
        let ExpandError::Unusable(rejections) = error;
        assert!(rejections[0].reason.contains("binary"), "{rejections:?}");
    }

    // ---------- folding ----------

    #[test]
    fn a_file_block_folds_to_one_line() {
        let text = "look\n\n<file path=\"notes.txt\">\nhello\nworld\n</file>\n\nafter";
        let folded = fold_files(text);
        assert!(folded.contains("[file: notes.txt, 2 lines]"), "{folded}");
        assert!(folded.contains("look"), "{folded}");
        assert!(folded.contains("after"), "{folded}");
        assert!(!folded.contains("hello"), "{folded}");
    }

    #[test]
    fn text_without_a_block_is_borrowed_unchanged() {
        assert!(matches!(
            fold_files("plain text"),
            Cow::Borrowed("plain text")
        ));
    }

    #[test]
    fn an_unclosed_block_is_left_as_written() {
        let text = "<file path=\"notes.txt\">\nhello";
        assert_eq!(fold_files(text), text);
    }

    // ---------- completion ----------

    #[test]
    fn completion_lists_a_directory_and_filters_by_prefix() {
        let dir = TempDir::new("complete");
        dir.write("notes.txt", "n");
        dir.write("notes.md", "n");
        dir.write("other.txt", "o");
        dir.write("sub/inner.txt", "i");

        let completion = complete("see @not", 8, dir.path()).unwrap();
        assert_eq!(completion.range, 5..8);
        assert_eq!(completion.candidates, ["notes.md", "notes.txt"]);
        assert_eq!(completion.common_prefix, "notes.");
    }

    #[test]
    fn completion_descends_into_a_subdirectory_and_marks_directories() {
        let dir = TempDir::new("complete-sub");
        dir.write("sub/inner.txt", "i");
        dir.write("notes.txt", "n");
        // A bare `@` lists the top level; `sub` gets a trailing slash.
        let completion = complete("@", 1, dir.path()).unwrap();
        assert_eq!(completion.candidates, ["notes.txt", "sub/"]);
        // `@sub/` lists the subdirectory.
        let completion = complete("@sub/", 5, dir.path()).unwrap();
        assert_eq!(completion.range, 1..5);
        assert_eq!(completion.candidates, ["sub/inner.txt"]);
    }

    #[test]
    fn hidden_entries_need_a_leading_dot_and_git_is_always_skipped() {
        let dir = TempDir::new("complete-hidden");
        dir.write(".gitignore", "x");
        dir.write(".hidden.txt", "x");
        dir.write("visible.txt", "x");
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join(".radian-sandbox")).unwrap();

        let plain = complete("@", 1, dir.path()).unwrap();
        assert_eq!(plain.candidates, ["visible.txt"]);
        let dotted = complete("@.", 2, dir.path()).unwrap();
        assert_eq!(dotted.candidates, [".gitignore", ".hidden.txt"]);
    }

    #[test]
    fn completion_is_capped_at_fifty() {
        let dir = TempDir::new("complete-cap");
        for i in 0..60 {
            dir.write(&format!("file{i:02}.txt"), "x");
        }
        let completion = complete("@file", 5, dir.path()).unwrap();
        assert_eq!(completion.candidates.len(), MAX_CANDIDATES);
        assert_eq!(completion.candidates[0], "file00.txt");
        assert_eq!(completion.candidates[MAX_CANDIDATES - 1], "file49.txt");
    }

    #[test]
    fn completion_handles_non_ascii_names_and_a_cursor_in_the_middle() {
        let dir = TempDir::new("complete-cjk");
        dir.write("笔记.txt", "x");
        dir.write("笔记.md", "x");
        let completion = complete("@笔记", 3, dir.path()).unwrap();
        assert_eq!(completion.candidates, ["笔记.md", "笔记.txt"]);
        assert_eq!(completion.common_prefix, "笔记.");

        // The cursor in the middle of the word completes only what is typed.
        let completion = complete("@笔记.txt", 2, dir.path()).unwrap();
        assert_eq!(completion.range, 1..2);
        assert_eq!(completion.candidates, ["笔记.md", "笔记.txt"]);
    }

    #[test]
    fn completion_is_none_when_the_word_does_not_start_with_at() {
        let dir = TempDir::new("complete-none");
        assert!(complete("plain", 5, dir.path()).is_none());
        assert!(complete("mail a@b", 8, dir.path()).is_none());
        // A cursor at the start of the line has no word.
        assert!(complete("", 0, dir.path()).is_none());
    }
}
