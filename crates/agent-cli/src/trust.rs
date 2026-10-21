//! The startup trust question and the management-command confirmation (ADR 0031 T3,
//! ADR 0021 D7): the one-line terminal shapes around trust and destructive commands.
//!
//! Split from `lib.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use std::io::{IsTerminal, Write};

use radian_host::memory::commands::CommandError;

use super::ui::LineHub;

/// The startup trust question (ADR 0031 T3), the same one-line terminal shape as the
/// approval: a numbered list, Enter takes the default (trust this directory and use the
/// sandbox for this project). A non-empty unrecognized answer is the fail-safe
/// `UntrustHere`, never a trust.
///
/// Slice B2: `preview` lists the project `.mcp.json` servers that trust would load — one
/// line each (the name, the `command` + `args` as written, the `env` key names only, never
/// values), with remote entries marked skipped. `None` means there is no project file and
/// nothing is printed.
pub(crate) async fn trust_prompt(
    lines: &LineHub,
    workspace: &std::path::Path,
    preview: Option<&radian_host::ProjectPreview>,
) -> Option<radian_host::trust::Choice> {
    use radian_host::trust::Choice;
    eprintln!(
        "project resources found in {}: trust them?",
        workspace.display()
    );
    if let Some(preview) = preview {
        for line in radian_host::preview_lines(preview) {
            eprintln!("  {line}");
        }
    }
    for (index, choice) in Choice::ALL.iter().enumerate() {
        let label = choice
            .label(workspace)
            .unwrap_or_else(|| "Trust the parent directory".to_owned());
        let marker = if *choice == Choice::TrustHereSandbox {
            " (default)"
        } else {
            ""
        };
        eprintln!("  [{}] {label}{marker}", index + 1);
    }
    let choice_default = Choice::ALL
        .iter()
        .position(|choice| *choice == Choice::TrustHereSandbox)
        .map(|index| index + 1)
        .unwrap_or(1);
    eprint!("choice [{choice_default}]: ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    // The same line hub the approval prompt uses: one reader owns stdin.
    let answer = lines.next().await.unwrap_or_default();
    Some(trust_choice_from_answer(answer.trim()))
}

/// The answer to the trust question, as a choice. An empty line takes the default (the
/// sandbox choice); any other unrecognized answer is the fail-safe `UntrustHere`, never a
/// trust. The numbers follow [`Choice::ALL`].
pub(crate) fn trust_choice_from_answer(answer: &str) -> radian_host::trust::Choice {
    use radian_host::trust::Choice;
    match answer.trim() {
        "" => Choice::TrustHereSandbox,
        "1" => Choice::TrustHereSandbox,
        "2" => Choice::TrustHere,
        "3" => Choice::TrustParent,
        "4" => Choice::TrustSession,
        "6" => Choice::UntrustSession,
        _ => Choice::UntrustHere,
    }
}

/// The confirmation for `forget`/`purge`/`split`/`merge` (ADR 0021 D7). A terminal asks;
/// without a terminal and without `--yes` the command refuses (exit 2) and does nothing.
pub(crate) fn confirm_or_refuse(message: &str) -> Result<(), CommandError> {
    if !std::io::stdin().is_terminal() {
        return Err(CommandError::usage(format!(
            "{message}: refusing without a terminal; pass --yes to confirm"
        )));
    }
    eprint!("{message} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| CommandError::failure(format!("cannot read the answer: {e}")))?;
    if answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes") {
        Ok(())
    } else {
        Err(CommandError::failure("cancelled"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0031 T3 / ADR 0043 decision 2 (revised 2026-10-20): the numbers follow
    /// [`Choice::ALL`] with the sandbox choice first; an empty line takes the default
    /// (the sandbox choice), and any other unrecognized answer is the fail-safe
    /// `UntrustHere`, never a trust.
    #[test]
    fn the_trust_answer_follows_the_new_order_and_fails_safe() {
        use radian_host::trust::Choice;
        assert_eq!(trust_choice_from_answer(""), Choice::TrustHereSandbox);
        assert_eq!(trust_choice_from_answer("1"), Choice::TrustHereSandbox);
        assert_eq!(trust_choice_from_answer("2"), Choice::TrustHere);
        assert_eq!(trust_choice_from_answer(" 3 "), Choice::TrustParent);
        assert_eq!(trust_choice_from_answer("4"), Choice::TrustSession);
        assert_eq!(trust_choice_from_answer("6"), Choice::UntrustSession);
        for other in ["5", "y", "yes", "99", "trust"] {
            assert_eq!(
                trust_choice_from_answer(other),
                Choice::UntrustHere,
                "{other:?}"
            );
        }
    }
}
