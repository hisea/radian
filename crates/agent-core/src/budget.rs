//! Context budget guard (ADR 0015): stop before an oversized model request; never compress.
//!
//! `ModelUsage::input` is the only trustworthy "how big was this request", and only the
//! provider can give it (ADR 0007: not reported is `None`, never 0). A local estimate is
//! available earlier but is an estimate. This module keeps the three answers distinct:
//! **reported**, **estimated**, and **unavailable** (no window configured → disabled). It
//! never invents a number to fill a gap.

use crate::model::ModelRequest;

/// The host-supplied context window and policy. There is deliberately **no** model→window
/// table: it would age and would mostly be remembered numbers. Without a `ContextBudget`
/// the guard is disabled (`BudgetStatus::Disabled`), not given a guessed default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    pub window_tokens: u64,
    /// Block when the projection reaches this percent of the window.
    pub threshold_percent: u8,
    /// Bytes per token for the local estimate.
    pub bytes_per_token: u32,
}

/// Block at 80% by default: leave ~20% of the window for the response and for provider-side
/// accounting differences (ADR 0015 C3).
pub const DEFAULT_THRESHOLD_PERCENT: u8 = 80;
/// A rough bytes-per-token for English and code. It is a warning ruler, not an audited
/// number, and the report is always labelled `Estimated` when it is the only source.
pub const DEFAULT_BYTES_PER_TOKEN: u32 = 4;

impl ContextBudget {
    /// A window with the default threshold (80%) and estimate ratio (4 bytes/token).
    pub fn new(window_tokens: u64) -> Self {
        Self {
            window_tokens,
            threshold_percent: DEFAULT_THRESHOLD_PERCENT,
            bytes_per_token: DEFAULT_BYTES_PER_TOKEN,
        }
    }

    pub fn with_threshold_percent(mut self, percent: u8) -> Self {
        self.threshold_percent = percent.min(100);
        self
    }

    pub fn with_bytes_per_token(mut self, bytes: u32) -> Self {
        self.bytes_per_token = bytes.max(1);
        self
    }

    /// The block line in tokens: `window * threshold_percent / 100`.
    pub fn threshold_tokens(&self) -> u64 {
        self.window_tokens
            .saturating_mul(u64::from(self.threshold_percent))
            / 100
    }
}

/// Where the projection's baseline came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSource {
    /// The baseline is an earlier request's provider-reported `input` token count; the
    /// growth on top of it is estimated.
    Reported,
    /// No request in this session has reported `input`; the whole projection is local.
    Estimated,
}

/// What the guard computed for one request. Host-visible, so it is honest about its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetReport {
    pub source: UsageSource,
    /// The projection at the moment of the check (`input + growth`, or pure estimate).
    pub projected_tokens: u64,
    pub window_tokens: u64,
    pub threshold_percent: u8,
    /// The request size the projection was computed from.
    pub request_bytes: u64,
    /// The reported `input` baseline, when there was one. Kept so a host can see the fact
    /// behind a `Reported` projection.
    pub baseline_input_tokens: Option<u64>,
}

/// The guard's state for a session (ADR 0015 C1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetStatus {
    /// No window configured: the guard is off and no number is claimed.
    Disabled,
    /// A projection is available and below the threshold.
    Ok(BudgetReport),
    /// The next request was not sent.
    Blocked(BudgetReport),
}

impl BudgetStatus {
    /// The report, if any (both `Ok` and `Blocked` carry one).
    pub fn report(&self) -> Option<&BudgetReport> {
        match self {
            BudgetStatus::Disabled => None,
            BudgetStatus::Ok(report) | BudgetStatus::Blocked(report) => Some(report),
        }
    }
}

/// The bytes the request will send: system prompt, messages, tool specs and settings, as
/// JSON for the structured parts. Called on the final request (after `before_model_call`).
pub fn request_bytes(request: &ModelRequest) -> u64 {
    let mut total = request.system_prompt.as_ref().map_or(0, |s| s.len() as u64);
    total += json_len(&request.messages);
    total += json_len(&request.tools);
    total += json_len(&request.settings);
    total
}

fn json_len<T: serde::Serialize>(value: &T) -> u64 {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len() as u64)
}

/// Computes the projection for one request (ADR 0015 C2). Pure, so the rule is testable on
/// its own.
///
/// `baseline` is `(reported_input_tokens, request_bytes_at_that_report)` for the session, if
/// any. With a baseline the result is `Reported`; otherwise `Estimated`.
pub fn project(
    budget: &ContextBudget,
    baseline: Option<(u64, u64)>,
    request_bytes: u64,
) -> BudgetReport {
    let bytes_per_token = u64::from(budget.bytes_per_token.max(1));
    let (source, projected_tokens, baseline_input_tokens) = match baseline {
        Some((input, base_bytes)) => {
            let growth = request_bytes.saturating_sub(base_bytes) / bytes_per_token;
            (
                UsageSource::Reported,
                input.saturating_add(growth),
                Some(input),
            )
        }
        None => (
            UsageSource::Estimated,
            request_bytes / bytes_per_token,
            None,
        ),
    };
    BudgetReport {
        source,
        projected_tokens,
        window_tokens: budget.window_tokens,
        threshold_percent: budget.threshold_percent,
        request_bytes,
        baseline_input_tokens,
    }
}

/// Convenience: does this report reach the block line?
pub fn exceeds(budget: &ContextBudget, report: &BudgetReport) -> bool {
    report.projected_tokens >= budget.threshold_tokens()
}
