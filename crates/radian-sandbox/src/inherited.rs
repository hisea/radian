//! The `inherited` backend (ADR 0016): a process the kernel says is already confined
//! by an outer sandbox gets **no second layer**. radian's own rules are not applied here — the
//! outer sandbox owns what the confined commands may do — so this backend only spawns the
//! command, with the host-side rules that are not sandbox rules: the environment whitelist
//! ([`apply_env`]) and the workspace as the child's cwd. The process-group `setsid` lives in the
//! shell tool (`radian-coding`), not here, and is unchanged.
//!
//! It is only ever selected when [`crate::confined_by_outer_sandbox`] asks the kernel and gets a
//! yes (ADR 0016): no environment variable, config key or flag can reach
//! it. For the same reason `probe` is a fixed report and never runs a child: the process is
//! already confined, and the nested `sandbox-exec` a real probe starts cannot run at all
//! (`sandbox_apply: Operation not permitted`, measured on macOS 27, 2026-10-16).

use std::ffi::{OsStr, OsString};
use std::process::Command;

use crate::error::{ProbeReport, SandboxError};
use crate::policy::SandboxPolicy;
use crate::{SandboxBackend, SandboxCommand, apply_env, workspace_tmp};

pub(crate) static INHERITED: Inherited = Inherited;

/// What [`Inherited::probe`] reports, and what [`crate::Sandbox::inherited`] carries. One
/// string in one place, so the startup line's `(probe: …)` text cannot drift.
pub(crate) const PROBE_DETAIL: &str = "confined by an outer sandbox; no probe";

pub struct Inherited;

impl SandboxBackend for Inherited {
    fn name(&self) -> &'static str {
        "inherited"
    }

    fn probe(&self) -> Result<ProbeReport, SandboxError> {
        Ok(ProbeReport {
            backend: self.name(),
            detail: PROBE_DETAIL.into(),
        })
    }

    fn command(
        &self,
        policy: &SandboxPolicy,
        program: &OsStr,
        args: &[OsString],
    ) -> Result<SandboxCommand, SandboxError> {
        let mut command = Command::new(program);
        command.args(args).current_dir(&policy.workspace);
        // Host-side, not sandbox-side: a model-controlled shell never inherits the host's API
        // keys, whatever the outer sandbox allows.
        apply_env(&mut command, policy);
        // The same rule the macOS backend applies in the `workspace` tier: tools write temporary
        // files, and the writable tree the outer sandbox grants is the workspace, so keep
        // `TMPDIR` inside it rather than at whatever the host's is.
        command.env("TMPDIR", workspace_tmp(policy));
        Ok(SandboxCommand::new(command))
    }
}
