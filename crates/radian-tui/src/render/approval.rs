//! The approval prompt: its key line, the body above it, and the box height and cap.

use radian_permissions::ApprovalRequest;

use super::wrap::wrap;

/// The approval prompt's key line. It is drawn on its **own** row under the body, never part
/// of the wrapped text, so a long command can never push the keys out of the box. The auto key
/// is a constant like the line itself: if the key changes, this string and [`crate::app::Input::ApprovalAuto`]'s `KeyCode` are the
/// only two places (ADR 0026 D9).
pub const APPROVAL_KEYS: &str =
    "[y] allow once   [a] always this session   [n] deny   [!] auto for this session";

/// The approval prompt's body: everything above the key line. The **resolved** resource is what
/// is shown: `a/../secret` must not read as if it stayed under `a/` (the permission layer
/// resolves it before the prompt, and that resolved form is also the session-grant key).
///
/// The resource's own argument is **not** repeated in `args`: the command or path already has
/// its line, so the rest (`cwd`, `timeout`, …) is what `args` shows, and it is omitted entirely
/// when nothing is left. The user must be able to read the whole command, so it is never
/// truncated; the 400-byte cap applies to the remaining `args` only.
pub fn approval_body(request: &ApprovalRequest) -> String {
    let label = if request.arguments.get("path").is_some() {
        "path"
    } else if request.arguments.get("command").is_some() {
        "command"
    } else {
        "resource"
    };
    let mut text = String::new();
    text.push_str(&format!("tool: {}\n", request.tool));
    text.push_str(&format!("{label}: {}\n", request.resolved_resource));
    let rest: serde_json::Map<String, serde_json::Value> = request
        .arguments
        .as_object()
        .map(|object| {
            object
                .iter()
                .filter(|(key, _)| key.as_str() != label)
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    if !rest.is_empty() {
        let args = serde_json::to_string_pretty(&rest)
            .unwrap_or_else(|_| serde_json::Value::Object(rest.clone()).to_string());
        let (shown, discarded) = truncate_for_display(&args, 400);
        text.push_str(&format!("args: {shown}"));
        if discarded > 0 {
            text.push_str(&format!("\n      … {discarded} more bytes"));
        }
    }
    if let Some(reason) = &request.reason {
        text.push_str(&format!("\nwhy: {reason}"));
    }
    text
}

/// The approval prompt's full text: body, then the key line. Kept for callers that want one
/// string (tests, a future non-TUI rendering); the box itself draws the two separately.
pub fn approval_text(request: &ApprovalRequest) -> String {
    format!("{}\n{APPROVAL_KEYS}", approval_body(request))
}

/// Truncates a display string on a character boundary, returning the kept part and how many
/// bytes were dropped.
fn truncate_for_display(text: &str, cap: usize) -> (String, usize) {
    if text.len() <= cap {
        return (text.to_owned(), 0);
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), text.len() - end)
}

/// The most of the terminal the approval box may take (~60%, the transcript
/// keeps at least three rows). `completion` is the rows the `@` list above the box needs.
pub fn approval_cap(screen_height: usize, completion: usize) -> usize {
    let cap = (screen_height * 60 / 100).max(MIN_APPROVAL_HEIGHT);
    cap.min(
        screen_height
            .saturating_sub(completion + 4)
            .max(MIN_APPROVAL_HEIGHT),
    )
}

/// The approval box's floor, border and key line included.
pub const MIN_APPROVAL_HEIGHT: usize = 5;

/// The height the approval box needs: the wrapped body rows (bounded by `cap`) and the
/// panel's two border rows. The body scrolls rather than growing past `cap`.
pub(super) fn approval_height(request: &ApprovalRequest, width: usize, cap: usize) -> u16 {
    let rows = wrap(&approval_body(request), width.saturating_sub(4).max(1)).len();
    let visible = rows.min(cap.saturating_sub(2).max(1));
    (visible as u16 + 2).clamp(
        MIN_APPROVAL_HEIGHT as u16,
        cap.max(MIN_APPROVAL_HEIGHT) as u16,
    )
}
