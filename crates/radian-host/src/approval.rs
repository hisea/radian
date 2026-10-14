//! The approval policy at startup and across resume (ADR 0026 D9).
//!
//! Precedence: an explicit command-line choice wins; otherwise the journal's last
//! `ConfigChanged { change: {"approval": …} }` — what the in-session switch wrote; otherwise
//! the config file's `[permissions] approval`; otherwise `on-ask`. Under the `full` tier,
//! config `auto` is ignored with a warning and a restored `auto` is not applied (D9: `full`
//! can only enable auto explicitly, inside the session).

use std::path::Path;

use radian_permissions::{ApprovalPolicy, Preset};

/// What startup decided about the approval policy (ADR 0026 D9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupApproval {
    /// The policy the session starts with.
    pub policy: ApprovalPolicy,
    /// Warning lines the host shows once (a config value ignored under `full`).
    pub warnings: Vec<String>,
    /// The value to write back as a `ConfigChanged` before the first turn: set only when
    /// the command line chose explicitly (D9's backfill, the same as D8's model handling).
    /// A fresh session backfills too, so a `--approval auto` run survives a resume the
    /// session toggles cannot reach.
    pub backfill: Option<ApprovalPolicy>,
}

/// Resolves the startup policy. Pure, so both hosts and their tests share one precedence.
pub fn resolve_startup(
    cli: Option<ApprovalPolicy>,
    config: Option<ApprovalPolicy>,
    restored: Option<ApprovalPolicy>,
    tier: Preset,
    resume: bool,
) -> StartupApproval {
    let mut warnings = Vec::new();
    if let Some(cli) = cli {
        // The command line is explicit, fresh or resumed: it wins and is recorded.
        return StartupApproval {
            policy: cli,
            warnings,
            backfill: Some(cli),
        };
    }
    if resume
        && let Some(restored) = restored
    {
        if restored == ApprovalPolicy::Auto && tier == Preset::FullAccess {
            // D9: `full` never restores auto; the session must enable it explicitly.
            warnings.push(
                "auto was on in the previous run; run `/auto on` to re-enable".to_string(),
            );
            // Fall through to the config/default decision: under `full` the config's own
            // `auto` is ignored too, so this lands on `on-ask`.
        } else {
            return StartupApproval {
                policy: restored,
                warnings,
                backfill: None,
            };
        }
    }
    let policy = match config {
        // Config `auto` cannot enable auto under `full` (ADR 0026 D9): only the session's
        // own explicit choice (`/auto on`, `/yolo`, the approval box) can.
        Some(ApprovalPolicy::Auto) if tier == Preset::FullAccess => {
            warnings.push(
                "config permissions.approval = \"auto\" is ignored under the full tier; \
                 enable it in the session with `/auto on`"
                    .to_string(),
            );
            ApprovalPolicy::OnAsk
        }
        Some(config) => config,
        None => ApprovalPolicy::OnAsk,
    };
    StartupApproval {
        policy,
        warnings,
        backfill: None,
    }
}

/// The approval policy to restore from a journal: the `approval` value of the **last**
/// `ConfigChanged` record that carries one. `None` when the journal has none, or when it
/// cannot be read — a fresh session never consults this, and an unreadable one must not
/// fail startup over a policy default.
pub fn restored_approval(journal_path: &Path) -> Option<ApprovalPolicy> {
    let session = radian_session::Session::open(journal_path).ok()?;
    session.events().iter().rev().find_map(|event| {
        match &event.data {
            radian_session::EventData::ConfigChanged { change, .. } => change
                .get("approval")
                .and_then(|value| value.as_str())
                .and_then(ApprovalPolicy::parse),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CLI wins over everything, fresh or resumed, and backfills a record (D9, like D8).
    #[test]
    fn an_explicit_cli_choice_wins_and_backfills() {
        for tier in [Preset::Safe, Preset::WorkspaceWrite, Preset::FullAccess] {
            for resume in [false, true] {
                let startup = resolve_startup(
                    Some(ApprovalPolicy::Auto),
                    Some(ApprovalPolicy::Never),
                    Some(ApprovalPolicy::OnAsk),
                    tier,
                    resume,
                );
                assert_eq!(startup.policy, ApprovalPolicy::Auto, "tier {} resume {resume}", tier.as_str());
                assert_eq!(startup.backfill, Some(ApprovalPolicy::Auto));
                assert!(startup.warnings.is_empty());
            }
        }
    }

    /// `safe`/`workspace` restore the journal's last in-session state over the config.
    #[test]
    fn resume_restores_the_journaled_state_over_the_config() {
        let startup = resolve_startup(
            None,
            Some(ApprovalPolicy::Never),
            Some(ApprovalPolicy::Auto),
            Preset::WorkspaceWrite,
            true,
        );
        assert_eq!(startup.policy, ApprovalPolicy::Auto);
        assert_eq!(startup.backfill, None);
        assert!(startup.warnings.is_empty());
    }

    /// `full` never restores auto, and says how to get it back (D9).
    #[test]
    fn full_does_not_restore_auto_and_hints() {
        let startup = resolve_startup(
            None,
            None,
            Some(ApprovalPolicy::Auto),
            Preset::FullAccess,
            true,
        );
        assert_eq!(startup.policy, ApprovalPolicy::OnAsk);
        assert_eq!(startup.backfill, None);
        assert_eq!(
            startup.warnings,
            vec!["auto was on in the previous run; run `/auto on` to re-enable".to_string()]
        );
    }

    /// Config `auto` is ignored with a warning under `full` (D9); other config values hold.
    #[test]
    fn config_auto_is_ignored_under_full_with_a_warning() {
        let startup = resolve_startup(
            None,
            Some(ApprovalPolicy::Auto),
            None,
            Preset::FullAccess,
            false,
        );
        assert_eq!(startup.policy, ApprovalPolicy::OnAsk);
        assert_eq!(startup.warnings.len(), 1);
        assert!(startup.warnings[0].contains("ignored under the full tier"));

        let startup = resolve_startup(
            None,
            Some(ApprovalPolicy::Auto),
            None,
            Preset::WorkspaceWrite,
            false,
        );
        assert_eq!(startup.policy, ApprovalPolicy::Auto);
        assert!(startup.warnings.is_empty());
    }

    /// No CLI, no journal record, no config: `on-ask`.
    #[test]
    fn the_default_is_on_ask() {
        let startup = resolve_startup(None, None, None, Preset::WorkspaceWrite, false);
        assert_eq!(startup.policy, ApprovalPolicy::OnAsk);
        assert_eq!(startup.backfill, None);
        assert!(startup.warnings.is_empty());
    }

    /// A resumed `full` session with both a restored auto **and** a config auto warns for
    /// both facts: the restore was suppressed and the config value is ignored.
    #[test]
    fn full_resume_reports_both_the_suppressed_restore_and_the_ignored_config() {
        let startup = resolve_startup(
            None,
            Some(ApprovalPolicy::Auto),
            Some(ApprovalPolicy::Auto),
            Preset::FullAccess,
            true,
        );
        assert_eq!(startup.policy, ApprovalPolicy::OnAsk);
        assert_eq!(startup.warnings.len(), 2, "{:?}", startup.warnings);
    }
}
