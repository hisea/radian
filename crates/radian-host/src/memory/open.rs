//! Opening a memory file or directory in the user's editor or the platform opener.
//!
//! `/memory add`, `/memory edit` and `/memory open` reach this through the host: `$EDITOR` when
//! set, else the platform's own opener (`open` on macOS, `xdg-open` elsewhere). The headless
//! command waits so an editor keeps the terminal; the TUI spawns it detached and keeps drawing.

use std::path::Path;
use std::process::{Command, Stdio};

/// Opens `path` with `$EDITOR` when set, else the platform opener. `wait` waits for an editor
/// to exit; the platform opener is never waited for.
pub fn open_path(path: &Path, wait: bool) -> Result<(), String> {
    if let Some(editor) = std::env::var_os("EDITOR").filter(|value| !value.is_empty()) {
        let editor = editor.to_string_lossy().into_owned();
        let mut parts = editor.split_whitespace();
        let Some(program) = parts.next() else {
            return Err("$EDITOR is empty".to_owned());
        };
        let mut command = Command::new(program);
        command.args(parts).arg(path);
        return spawn(&mut command, wait);
    }
    platform_open(path, wait)
}

#[cfg(target_os = "macos")]
fn platform_open(path: &Path, _wait: bool) -> Result<(), String> {
    let mut command = Command::new("open");
    command.arg(path);
    spawn(&mut command, false)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_open(path: &Path, _wait: bool) -> Result<(), String> {
    let mut command = Command::new("xdg-open");
    command.arg(path);
    spawn(&mut command, false)
}

#[cfg(windows)]
fn platform_open(path: &Path, _wait: bool) -> Result<(), String> {
    let mut command = Command::new("cmd");
    command.args(["/C", "start", ""]).arg(path);
    spawn(&mut command, false)
}

fn spawn(command: &mut Command, wait: bool) -> Result<(), String> {
    if wait {
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let status = command.status().map_err(|error| error.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("the editor exited with {status}"))
        }
    } else {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
            .spawn()
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}
