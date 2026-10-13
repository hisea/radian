//! Context budget guard (ADR 0015): stop before an oversized model request; never compress.
//!
//! `ModelUsage::input` is the only trustworthy "how big was this request", and only the
//! provider can give it (ADR 0007: not reported is `None`, never 0). A local estimate is
//! available earlier but is an estimate. This module keeps the answers distinct:
//! **reported**, **reported + estimated** (a reported baseline plus estimated growth),
//! **estimated**, and **unavailable** (no window configured → disabled). It never invents a
//! number to fill a gap, and the estimator deliberately errs high (ADR 0015 C2).

use serde::{Deserialize, Serialize};

use crate::model::ModelRequest;

/// The host-supplied context window and policy. There is deliberately **no** model→window
/// table: it would age and would mostly be remembered numbers. Without a `ContextBudget`
/// the guard is disabled (`BudgetStatus::Disabled`), not given a guessed default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    pub window_tokens: u64,
    /// Block when the projection reaches this percent of the window.
    pub threshold_percent: u8,
    /// ASCII bytes per token for the local estimate.
    pub bytes_per_token: u32,
}

/// Block at 80% by default: leave ~20% of the window for the response and for provider-side
/// accounting differences (ADR 0015 C3).
pub const DEFAULT_THRESHOLD_PERCENT: u8 = 80;
/// A rough ASCII bytes-per-token for English and code. It is a warning ruler, not an audited
/// number, and the report says exactly how much of it is estimated.
pub const DEFAULT_BYTES_PER_TOKEN: u32 = 4;

impl ContextBudget {
    /// A window with the default threshold (80%) and estimate ratio (4 ASCII bytes/token).
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

/// Where the projection came from (ADR 0015 C1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    /// The projection is exactly a provider-reported `input` token count (no growth yet).
    Reported,
    /// A provider-reported `input` baseline **plus** estimated growth: the number is partly
    /// a fact and partly an estimate, and a host must not display it as a precise report.
    ReportedPlusEstimate,
    /// No request in this session has reported `input`; the whole projection is local.
    Estimated,
}

/// The request size split so the estimator can be conservative about non-ASCII text
/// (ADR 0015 C2). `total_bytes` is kept for the audit report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestSize {
    pub ascii_bytes: u64,
    pub non_ascii_bytes: u64,
    pub non_ascii_chars: u64,
}

impl RequestSize {
    /// The real byte count of the request, for the audit report.
    pub fn total_bytes(&self) -> u64 {
        self.ascii_bytes + self.non_ascii_bytes
    }

    /// The conservative token estimate. ASCII bytes are divided per token (rounded **up**,
    /// so an estimate never rounds down); every non-ASCII character counts as **at least
    /// one** token. Chinese is ~3 UTF-8 bytes per character and 1+ token per character, so
    /// a bare `bytes/4` would undercount it by 25–50% — and for a "stop before overflow"
    /// guard, undercounting means stopping too late. Overcounting only stops a little early.
    pub fn tokens(&self, bytes_per_token: u32) -> u64 {
        let bytes_per_token = u64::from(bytes_per_token.max(1));
        self.ascii_bytes.div_ceil(bytes_per_token) + self.non_ascii_chars
    }

    fn add_str(&mut self, text: &str) {
        for ch in text.chars() {
            if ch.is_ascii() {
                self.ascii_bytes += 1;
            } else {
                self.non_ascii_chars += 1;
                self.non_ascii_bytes += ch.len_utf8() as u64;
            }
        }
    }

    fn add_json<T: serde::Serialize>(&mut self, value: &T) {
        if let Ok(bytes) = serde_json::to_vec(value) {
            match std::str::from_utf8(&bytes) {
                Ok(text) => self.add_str(text),
                // serde_json is always UTF-8; this is a belt-and-braces fallback.
                Err(_) => self.ascii_bytes += bytes.len() as u64,
            }
        }
    }
}

/// The size of the final request (after `before_model_call`): system prompt, messages, tool
/// specs and settings, with JSON for the structured parts.
pub fn request_size(request: &ModelRequest) -> RequestSize {
    let mut size = RequestSize::default();
    if let Some(system) = &request.system_prompt {
        size.add_str(system);
    }
    size.add_json(&request.messages);
    size.add_json(&request.tools);
    size.add_json(&request.settings);
    size
}

/// The conservative token estimate for one request (ADR 0015 C2), exposed so a host can
/// show the status bar number and so the bound is testable on its own.
pub fn estimate_tokens(request: &ModelRequest, bytes_per_token: u32) -> u64 {
    request_size(request).tokens(bytes_per_token)
}

/// What the guard computed for one request. Host-visible, so it is honest about its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetReport {
    pub source: UsageSource,
    /// The projection at the moment of the check (`baseline + growth`, or pure estimate).
    pub projected_tokens: u64,
    pub window_tokens: u64,
    pub threshold_percent: u8,
    /// The real byte count of the request the projection was computed from.
    pub request_bytes: u64,
    /// The provider-reported `input` baseline, when there was one.
    pub baseline_input_tokens: Option<u64>,
    /// The estimated growth added on top of the baseline. `Some` iff there was a baseline;
    /// `0` only when the request had not grown. This is the part a host must not present as
    /// a provider report.
    pub estimated_growth_tokens: Option<u64>,
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

/// Computes the projection for one request (ADR 0015 C2). Pure, so the rule is testable on
/// its own.
///
/// `baseline` is `(reported_input_tokens, request_size_at_that_report)` for the session, if
/// any. With a baseline the baseline part is reported and the growth is estimated; without
/// one the whole projection is estimated.
pub fn project(
    budget: &ContextBudget,
    baseline: Option<(u64, RequestSize)>,
    size: RequestSize,
) -> BudgetReport {
    let tokens = size.tokens(budget.bytes_per_token);
    let (source, projected_tokens, baseline_input_tokens, estimated_growth_tokens) = match baseline
    {
        Some((input, base_size)) => {
            let growth = tokens.saturating_sub(base_size.tokens(budget.bytes_per_token));
            let source = if growth == 0 {
                UsageSource::Reported
            } else {
                UsageSource::ReportedPlusEstimate
            };
            (
                source,
                input.saturating_add(growth),
                Some(input),
                Some(growth),
            )
        }
        None => (UsageSource::Estimated, tokens, None, None),
    };
    BudgetReport {
        source,
        projected_tokens,
        window_tokens: budget.window_tokens,
        threshold_percent: budget.threshold_percent,
        request_bytes: size.total_bytes(),
        baseline_input_tokens,
        estimated_growth_tokens,
    }
}

/// Convenience: does this report reach the block line?
pub fn exceeds(budget: &ContextBudget, report: &BudgetReport) -> bool {
    report.projected_tokens >= budget.threshold_tokens()
}
