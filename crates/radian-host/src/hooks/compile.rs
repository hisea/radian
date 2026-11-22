//! Compiling the configured matchers, project layer first.

use super::*;

/// Compiles one event's two layers in run order: the project entries first, then the
/// user's (ADR 0045 slice B). An invalid pattern warns once here (not per call) and the
/// entry never runs.
pub(super) fn compile_layered(
    project: &[HookMatcher],
    user: &[HookMatcher],
    event: &str,
    warn: &HooksWarning,
) -> Vec<CompiledEntry> {
    project
        .iter()
        .chain(user.iter())
        .enumerate()
        .map(|(index, entry)| compile_entry(entry, event, index, warn))
        .collect()
}

/// Compiles one observe-only event's two layers in run order (project first, then the user's).
/// A matcher outside `allowed` warns once here (never per run) and that entry never runs
/// (ADR 0045 §6).
pub(super) fn compile_observe_layered(
    project: &[HookMatcher],
    user: &[HookMatcher],
    event: &str,
    allowed: &[&str],
    warn: &HooksWarning,
) -> Vec<CompiledObserveEntry> {
    project
        .iter()
        .chain(user.iter())
        .enumerate()
        .map(|(index, entry)| {
            let matcher = match entry.matcher.as_deref() {
                None | Some("") | Some("*") => ObserveMatcher::All,
                Some(value) if allowed.contains(&value) => ObserveMatcher::Value(value.to_owned()),
                Some(value) => {
                    (warn)(&format!(
                        "{event}[{index}] matcher {value:?} is not one of {}; the entry will \
                         never run",
                        allowed.join(", ")
                    ));
                    ObserveMatcher::Never
                }
            };
            CompiledObserveEntry {
                hooks: entry.hooks.clone(),
                matcher,
            }
        })
        .collect()
}

/// Compiles one `{ matcher, hooks }` entry: the regex is built once at load time, so an
/// invalid pattern warns once there (not per call) and the entry never runs.
pub(super) fn compile_entry(
    entry: &HookMatcher,
    event: &str,
    index: usize,
    warn: &HooksWarning,
) -> CompiledEntry {
    let matcher = match &entry.matcher {
        None => CompiledMatcher::All,
        Some(pattern) if pattern.trim().is_empty() || pattern.trim() == "*" => CompiledMatcher::All,
        Some(pattern) => match regex::Regex::new(&format!("^(?:{pattern})$")) {
            Ok(regex) => CompiledMatcher::Regex(regex),
            Err(error) => {
                (warn)(&format!(
                    "{event}[{index}] matcher {pattern:?} is not a valid regex ({error}); \
                     the entry will never run"
                ));
                CompiledMatcher::Never
            }
        },
    };
    CompiledEntry {
        hooks: entry.hooks.clone(),
        matcher,
    }
}
