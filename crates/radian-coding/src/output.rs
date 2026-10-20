//! Reading and creating **session-owned command output files**, shared by background jobs
//! and a foreground command's retained output (both read back through the `job` tool).
//!
//! The file is always created by [`open_output_file`] — `O_CREAT|O_EXCL|O_NOFOLLOW`, mode
//! 0600, in a caller-owned 0700 directory (ADR 0039 decision 1) — and read only through
//! [`open_output_read`] (`O_NOFOLLOW`). The functions here never reveal the path: the model
//! always names an opaque handle/id, and the caller resolves it to a registry entry, never a
//! model-supplied string.
//!
//! The line readers are the same shape `read`/`grep` use: a numbered line range, a `tail_lines`
//! view, or the lines matching a regex, each bounded by a byte budget and a match cap.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
#[cfg(not(unix))]
use std::path::PathBuf;

use agent_core::{ContentPart, ToolFailure, ToolOutput};
use serde::Deserialize;
use serde_json::json;

use crate::args;
use crate::fail;

/// The most a single line contributes before it is marked `[truncated]` (grep's cap).
pub(crate) const LINE_BYTES: usize = crate::grep::MAX_LINE_BYTES;

/// Creates `dir` 0700 if it is missing. The host normally creates the session directories with
/// `radian_session::ensure_private_dir`; this mirrors that for a registry used without a host
/// (tests, an embedded tool set) so the output is owner-only either way.
pub(crate) fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    let created = {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    };
    #[cfg(not(unix))]
    let created = std::fs::create_dir_all(dir);
    match created {
        // Another thread or process won the race; the directory is there.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

/// Opens an output file for appending, creating it **exclusively** and refusing to follow a
/// symlink at the destination (ADR 0039 decision 1): `O_CREAT|O_EXCL|O_NOFOLLOW`, mode 0600.
///
/// `create_new` already refuses an existing path, and a symlink is an existing path, so
/// `O_NOFOLLOW` is defence in depth here: when the destination is a symlink the two are
/// equivalent, which `job_output_security` pins. The pair is what the ADR asks for, and both
/// a background job's file and a retained foreground file go through this one creator.
pub(crate) fn open_output_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.append(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
        options.mode(0o600);
    }
    options.open(path)
}

/// Opens an output file for reading. The path comes from a registry entry, never a
/// model-supplied string; `O_NOFOLLOW` refuses a symlink at the final component.
pub(crate) fn open_output_read(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

/// A **private** output directory held as a directory fd. Every file inside is opened relative
/// to this fd (`openat`), so a parent directory swapped for a symlink after the directory was
/// opened cannot redirect a create or a read. The directory is created 0700 (one level) when
/// missing and a symlink at the directory path is refused, so a pre-planted parent link cannot
/// move the retained output outside the root the host chose.
///
/// The root is always a caller-supplied directory outside the model's workspace (the session's
/// jobs directory); see `provider.rs`.
pub(crate) struct PrivateDir {
    #[cfg(unix)]
    dir: File,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl PrivateDir {
    #[cfg(unix)]
    pub(crate) fn open(path: &Path) -> std::io::Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::DirBuilderExt;
        use std::os::unix::io::FromRawFd;

        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "the private output root is a symlink; refusing",
                ));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    "the private output root is not a directory",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // One level only: the parent is the caller's, not the model's, to create.
                match std::fs::DirBuilder::new().mode(0o700).create(path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
            }
            Err(error) => return Err(error),
        }
        let c = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
        // SAFETY: `c` is a NUL-terminated path; the flags are plain integers.
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor we own.
        Ok(Self {
            dir: unsafe { File::from_raw_fd(fd) },
        })
    }

    #[cfg(not(unix))]
    pub(crate) fn open(path: &Path) -> std::io::Result<Self> {
        if !path.exists() {
            std::fs::create_dir(path)?;
        }
        let meta = std::fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the private output root is a symlink; refusing",
            ));
        }
        if !meta.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "the private output root is not a directory",
            ));
        }
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    /// Creates `name` inside the directory: `O_CREAT|O_EXCL|O_NOFOLLOW`, mode 0600.
    #[cfg(unix)]
    pub(crate) fn create(&self, name: &OsStr) -> std::io::Result<File> {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::io::{AsRawFd, FromRawFd};
        let c = std::ffi::CString::new(name.as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in name"))?;
        // SAFETY: `dir` is an open directory fd and `c` a NUL-terminated name.
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor we own.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    #[cfg(not(unix))]
    pub(crate) fn create(&self, name: &OsStr) -> std::io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.path.join(name))
    }

    /// Clears the directory on resume (ADR 0037 §6): every regular file and every symlink
    /// is unlinked (the link itself, never its target); anything else (a subdirectory, a
    /// socket, …) is left in place and counted as skipped. The listing goes through the
    /// directory fd (`fdopendir` on a duplicate), so a swapped parent cannot redirect it.
    /// The first I/O failure is kept in [`SweepSummary::first_error`]; the sweep continues
    /// past it. Nothing is logged here: the host warns once.
    #[cfg(unix)]
    pub(crate) fn sweep(&self) -> SweepSummary {
        let entries = match crate::safe_fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) => {
                return SweepSummary {
                    removed: 0,
                    skipped: 0,
                    first_error: Some(error.to_string()),
                };
            }
        };
        let mut summary = SweepSummary::default();
        for entry in entries {
            match entry.kind {
                crate::safe_fs::EntryKind::File | crate::safe_fs::EntryKind::Symlink => {
                    match unlink(&self.dir, &entry.name) {
                        Ok(()) => summary.removed += 1,
                        Err(error) => {
                            if summary.first_error.is_none() {
                                summary.first_error =
                                    Some(format!("{}: {error}", entry.name.to_string_lossy()));
                            }
                        }
                    }
                }
                _ => summary.skipped += 1,
            }
        }
        summary
    }

    /// The non-Unix sweep: the same rule (regular files and symlinks are removed as links,
    /// anything else is left and counted as skipped) over a path-based listing. The coding
    /// tools run on Unix hosts; like [`PrivateDir::open`]/`create` above this keeps the
    /// crate compiling elsewhere with no symlink defence.
    #[cfg(not(unix))]
    pub(crate) fn sweep(&self) -> SweepSummary {
        let listing = match std::fs::read_dir(&self.path) {
            Ok(listing) => listing,
            Err(error) => {
                return SweepSummary {
                    removed: 0,
                    skipped: 0,
                    first_error: Some(error.to_string()),
                };
            }
        };
        let mut summary = SweepSummary::default();
        for entry in listing {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    if summary.first_error.is_none() {
                        summary.first_error = Some(error.to_string());
                    }
                    continue;
                }
            };
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) => {
                    if summary.first_error.is_none() {
                        summary.first_error =
                            Some(format!("{}: {error}", entry.file_name().to_string_lossy()));
                    }
                    continue;
                }
            };
            // `DirEntry::file_type` never follows a symlink, and `remove_file` removes the
            // link itself, so a link's target is never touched here either.
            if kind.is_file() || kind.is_symlink() {
                match std::fs::remove_file(entry.path()) {
                    Ok(()) => summary.removed += 1,
                    Err(error) => {
                        if summary.first_error.is_none() {
                            summary.first_error =
                                Some(format!("{}: {error}", entry.file_name().to_string_lossy()));
                        }
                    }
                }
            } else {
                summary.skipped += 1;
            }
        }
        summary
    }
}

/// What a resume sweep removed, skipped and failed on (ADR 0037 §6). `first_error` is the
/// first I/O failure; the sweep continues past it. The host warns once on `Some`, never
/// failing the open.
#[derive(Debug, Default)]
pub struct SweepSummary {
    /// Regular files and symlinks unlinked.
    pub removed: usize,
    /// Subdirectories and anything else left in place.
    pub skipped: usize,
    /// The first I/O failure, if any.
    pub first_error: Option<String>,
}

/// `unlinkat(dirfd, name, 0)`: removes a regular file or a symlink itself, never a link's
/// target. Directories are never passed here (the caller counts them as skipped).
#[cfg(unix)]
fn unlink(dir: &File, name: &OsStr) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    let c = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in name"))?;
    // SAFETY: `dir` is an open directory fd and `c` a NUL-terminated name; flags 0 means
    // unlink the entry itself (no `AT_REMOVEDIR`), so a symlink's target is untouched.
    let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), 0) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The shared file-read arguments of the `job` tool.
#[derive(Deserialize)]
pub(crate) struct FileArgs {
    /// 1-based line of the output file to start at.
    pub from_line: Option<u64>,
    /// Maximum lines to return from `from_line`.
    pub max_lines: Option<u64>,
    /// The last N lines of the output file.
    pub tail_lines: Option<u64>,
    /// Only lines matching this Rust regex.
    pub pattern: Option<String>,
    /// The read the model names: with it, the other reads' fields are ignored.
    pub mode: Option<String>,
}

/// The `properties` object both tools' schemas embed for the file reads.
pub(crate) fn file_properties() -> serde_json::Value {
    json!({
        "from_line": {
            "type": "integer",
            "minimum": 1,
            "description": "First output-file line to return (1-based). Use it with \
                            `max_lines` to page."
        },
        "max_lines": {
            "type": "integer",
            "minimum": 1,
            "description": "Maximum full-file lines to return from `from_line`."
        },
        "tail_lines": {
            "type": "integer",
            "minimum": 1,
            "description": "Return the last N lines of the output file."
        },
        "pattern": {
            "type": "string",
            "description": "Return only output-file lines that match this Rust regex. \
                            Lines are numbered and capped."
        }
    })
}

/// The number of lines in `file` as the read tool numbers them: a trailing newline does not add
/// an empty final line, and a non-empty file with no final newline has one more line.
pub(crate) fn count_lines(file: &File) -> std::io::Result<u64> {
    let mut handle = file;
    handle.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(handle);
    let mut count = 0u64;
    let mut any = false;
    let mut last_was_newline = true;
    let mut buf = [0u8; 64 << 10];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        any = true;
        for &byte in &buf[..n] {
            if byte == b'\n' {
                count += 1;
                last_was_newline = true;
            } else {
                last_was_newline = false;
            }
        }
    }
    if any && !last_was_newline {
        count += 1;
    }
    Ok(count)
}

/// A line range of the output file, already numbered `N: `.
pub(crate) struct RangeRead {
    pub content: String,
    pub start_line: u64,
    pub end_line: u64,
    pub returned: u64,
    pub next_line: Option<u64>,
}

/// Reads at most `max_lines` numbered lines from `from_line`, bounded by `budget` bytes. The
/// skip is constant memory; each line is capped at [`LINE_BYTES`].
pub(crate) fn read_range(
    file: &File,
    from_line: u64,
    max_lines: Option<u64>,
    budget: usize,
) -> std::io::Result<RangeRead> {
    let mut handle = file;
    handle.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(handle);
    crate::read::skip_lines(&mut reader, (from_line - 1) as i64)?;
    let mut content = String::new();
    let mut line: Vec<u8> = Vec::new();
    let mut lineno = from_line;
    let mut returned = 0u64;
    let mut next_line = None;
    loop {
        if let Some(max) = max_lines
            && returned >= max
        {
            next_line = Some(lineno);
            break;
        }
        let truncated = match crate::grep::read_line_capped(&mut reader, &mut line, LINE_BYTES)? {
            crate::grep::LineRead::Eof => break,
            crate::grep::LineRead::Complete => false,
            crate::grep::LineRead::TooLong => true,
        };
        let text = crate::grep::strip_line_end(&line);
        let entry = if truncated {
            format!("{lineno}: {text}[truncated]\n")
        } else {
            format!("{lineno}: {text}\n")
        };
        if content.len() + entry.len() > budget {
            next_line = Some(lineno);
            break;
        }
        content.push_str(&entry);
        returned += 1;
        lineno += 1;
    }
    Ok(RangeRead {
        content,
        start_line: from_line,
        end_line: if returned == 0 {
            from_line.saturating_sub(1)
        } else {
            lineno - 1
        },
        returned,
        next_line,
    })
}

/// Matching lines of the output file, already numbered `N: `.
pub(crate) struct PatternRead {
    pub content: String,
    pub total_lines: u64,
    pub matches: u64,
    pub cut: bool,
}

/// Scans the whole file for `regex`, numbering the matching lines, capped at `cap` matches and
/// `budget` bytes. The scan continues to end of file so the result can name the total line count.
pub(crate) fn read_pattern(
    file: &File,
    regex: &regex::Regex,
    budget: usize,
    cap: usize,
) -> std::io::Result<PatternRead> {
    let mut handle = file;
    handle.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(handle);
    let mut content = String::new();
    let mut line: Vec<u8> = Vec::new();
    let mut lineno = 1u64;
    let mut matches = 0u64;
    let mut cut = false;
    loop {
        let truncated = match crate::grep::read_line_capped(&mut reader, &mut line, LINE_BYTES)? {
            crate::grep::LineRead::Eof => break,
            crate::grep::LineRead::Complete => false,
            crate::grep::LineRead::TooLong => true,
        };
        let text = crate::grep::strip_line_end(&line);
        if regex.is_match(&text) && !cut {
            if matches >= cap as u64 {
                cut = true;
            } else {
                let entry = if truncated {
                    format!("{lineno}: {text}[truncated]\n")
                } else {
                    format!("{lineno}: {text}\n")
                };
                if content.len() + entry.len() > budget {
                    cut = true;
                } else {
                    content.push_str(&entry);
                    matches += 1;
                }
            }
        }
        lineno += 1;
    }
    Ok(PatternRead {
        content,
        total_lines: lineno - 1,
        matches,
        cut,
    })
}

/// The read of the output file failed. The path is not shown to the model.
pub(crate) fn read_failure(code: &str, error: &std::io::Error) -> ToolFailure {
    fail(code, format!("could not read the retained output: {error}"))
}

/// Which part of the output file a call asks for. `None` is the incremental ring read, which
/// only `job` with `op: read` and no range has.
pub(crate) enum FileMode {
    Range {
        from_line: u64,
        max_lines: Option<u64>,
    },
    Tail {
        lines: u64,
    },
    Pattern {
        source: String,
        regex: regex::Regex,
    },
}

impl FileMode {
    /// The file mode the arguments request, or `None` for the incremental read. `tool` names
    /// the calling tool in an argument error.
    pub(crate) fn from_args(
        tool: &str,
        args: &FileArgs,
        raw: &serde_json::Value,
    ) -> Result<Option<Self>, ToolFailure> {
        // `args::parse` already dropped an optional `""` (so `pattern: ""` is no pattern).
        let pattern = args.pattern.as_deref();
        match args.mode.as_deref() {
            Some("new") => return Ok(None),
            Some("tail") => {
                let Some(lines) = args.tail_lines else {
                    return Err(args::invalid(
                        tool,
                        raw,
                        ["`mode: tail` needs `tail_lines`"],
                    ));
                };
                return Self::tail(lines, tool, raw).map(Some);
            }
            Some("range") => {
                return Self::range(args.from_line, args.max_lines, tool, raw).map(Some);
            }
            Some("pattern") => {
                let Some(source) = pattern else {
                    return Err(args::invalid(
                        tool,
                        raw,
                        ["`mode: pattern` needs a non-empty `pattern`"],
                    ));
                };
                return Self::pattern(source).map(Some);
            }
            // The schema's enum refuses any other value before this point.
            _ => {}
        }
        let requested = [
            pattern.is_some(),
            args.tail_lines.is_some(),
            args.from_line.is_some() || args.max_lines.is_some(),
        ]
        .into_iter()
        .filter(|wanted| *wanted)
        .count();
        if requested == 0 {
            return Ok(None);
        }
        if requested > 1 {
            return Err(args::invalid(
                tool,
                raw,
                ["use only one of `pattern`, `tail_lines`, or `from_line`/`max_lines`"],
            ));
        }
        if let Some(lines) = args.tail_lines {
            return Self::tail(lines, tool, raw).map(Some);
        }
        if let Some(source) = pattern {
            return Self::pattern(source).map(Some);
        }
        Self::range(args.from_line, args.max_lines, tool, raw).map(Some)
    }

    fn tail(lines: u64, tool: &str, raw: &serde_json::Value) -> Result<Self, ToolFailure> {
        if lines < 1 {
            return Err(args::invalid(
                tool,
                raw,
                ["`tail_lines` must be at least 1"],
            ));
        }
        Ok(FileMode::Tail { lines })
    }

    fn pattern(source: &str) -> Result<Self, ToolFailure> {
        let regex = regex::Regex::new(source)
            .map_err(|error| fail("invalid_regex", format!("`{source}`: {error}")))?;
        Ok(FileMode::Pattern {
            source: source.to_owned(),
            regex,
        })
    }

    fn range(
        from_line: Option<u64>,
        max_lines: Option<u64>,
        tool: &str,
        raw: &serde_json::Value,
    ) -> Result<Self, ToolFailure> {
        let from_line = from_line.unwrap_or(1);
        if from_line < 1 {
            return Err(args::invalid(tool, raw, ["`from_line` must be at least 1"]));
        }
        if let Some(lines) = max_lines
            && lines < 1
        {
            return Err(args::invalid(tool, raw, ["`max_lines` must be at least 1"]));
        }
        Ok(FileMode::Range {
            from_line,
            max_lines,
        })
    }
}

/// Reads a job's or a foreground command's output in the requested [`FileMode`], from an
/// already-open fd. The `id` is the opaque handle/job id the model gave; it is never a path.
pub(crate) fn read_file(
    file: &File,
    id_field: &str,
    id: &str,
    mode: FileMode,
    budget: usize,
    pattern_cap: usize,
    failure_code: &str,
) -> Result<ToolOutput, ToolFailure> {
    match mode {
        FileMode::Range {
            from_line,
            max_lines,
        } => {
            let total = count_lines(file).map_err(|e| read_failure(failure_code, &e))?;
            let range = read_range(file, from_line, max_lines, budget)
                .map_err(|e| read_failure(failure_code, &e))?;
            Ok(render_range(id_field, id, "range", range, total))
        }
        FileMode::Tail { lines } => {
            let total = count_lines(file).map_err(|e| read_failure(failure_code, &e))?;
            // 1-based: the last `lines` lines are `total - lines + 1 ..= total`.
            let start = total.saturating_sub(lines) + 1;
            let range = read_range(file, start, Some(lines), budget)
                .map_err(|e| read_failure(failure_code, &e))?;
            Ok(render_range(id_field, id, "tail", range, total))
        }
        FileMode::Pattern { source, regex } => {
            let scan = read_pattern(file, &regex, budget, pattern_cap)
                .map_err(|e| read_failure(failure_code, &e))?;
            Ok(render_pattern(id_field, id, &source, scan))
        }
    }
}

fn render_range(id_field: &str, id: &str, mode: &str, range: RangeRead, total: u64) -> ToolOutput {
    let mut text = range.content.clone();
    if range.returned == 0 {
        text.push_str(&format!(
            "[no lines in that range of the output ({total} lines total)]\n"
        ));
    } else {
        text.push_str(&format!(
            "[output: {total} lines; showing lines {}-{}]\n",
            range.start_line, range.end_line
        ));
        match range.next_line {
            Some(next) => text.push_str(&format!("[more remains: call with from_line={next}]\n")),
            None => text.push_str("[end of output]\n"),
        }
    }
    let mut structured = json!({
        "source": "file",
        "mode": mode,
        "total_lines": total,
        "start_line": range.start_line,
        "end_line": range.end_line,
        "returned": range.returned,
        "next_line": range.next_line,
        "truncated": range.next_line.is_some(),
    });
    structured[id_field] = json!(id);
    ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(structured),
    }
}

fn render_pattern(id_field: &str, id: &str, source: &str, scan: PatternRead) -> ToolOutput {
    let mut text = scan.content.clone();
    if scan.matches == 0 {
        text.push_str(&format!("[no matches for `{source}`]\n"));
    }
    text.push_str(&format!(
        "[output: {} lines; {} matching line(s) for `{source}`{}]\n",
        scan.total_lines,
        scan.matches,
        if scan.cut { "; more were cut" } else { "" }
    ));
    let mut structured = json!({
        "source": "file",
        "mode": "pattern",
        "pattern": source,
        "total_lines": scan.total_lines,
        "matches": scan.matches,
        "truncated": scan.cut,
    });
    structured[id_field] = json!(id);
    ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(structured),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one creator is `O_CREAT|O_EXCL|O_NOFOLLOW`: a pre-existing file and a planted symlink
    /// are both refused, so a planted target never receives the host's output. This is the same
    /// creator `Jobs::start` uses, which `job_output_security` exercises end to end.
    #[cfg(unix)]
    #[test]
    fn the_creator_refuses_an_existing_file_and_a_symlink() {
        let dir = std::env::temp_dir().join(format!(
            "radian-output-create-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let existing = dir.join("existing.out");
        std::fs::write(&existing, "PREEXISTING\n").unwrap();
        let error = open_output_file(&existing).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "PREEXISTING\n");

        let outside = dir.join("outside.txt");
        std::fs::write(&outside, "UNCHANGED\n").unwrap();
        let link = dir.join("link.out");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(
            open_output_file(&link).is_err(),
            "a symlink must be refused"
        );
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "UNCHANGED\n");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
