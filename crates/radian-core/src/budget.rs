//! Context budget **projection** (ADR 0015, kept by ADR 0018): how big is the request, and is
//! it over the effective input budget `B`?
//!
//! `ModelUsage::input` is the only trustworthy "how big was this request", and only the
//! provider can give it (ADR 0007: not reported is `None`, never 0). A local estimate is
//! available earlier but is an estimate. This module keeps the answers distinct:
//! **reported**, **reported + estimated** (a reported baseline plus estimated growth),
//! **estimated**, and **unavailable** (no budget configured → disabled). It never invents a
//! number to fill a gap, and the estimator deliberately errs high (ADR 0015 C2).
//!
//! Crossing `B` does **not** stop the request any more (ADR 0018): the status becomes
//! [`BudgetStatus::Over`] and the request is still sent. Automatic compaction, the next
//! slice, will act on that status.

use serde::{Deserialize, Serialize};

use crate::message::{ContentPart, Message};
use crate::model::ModelRequest;

/// The host-supplied **effective input budget `B`** (ADR 0018) and estimate ratio. The host
/// computes `B = min(I, C - R)` from the model catalog and the request's output setting; Core
/// knows none of `C`, `I` or `R`. There is deliberately **no** model→budget table: it would
/// age and would mostly be remembered numbers. Without a `ContextBudget` there is no budget
/// (`BudgetStatus::Disabled`), never a guessed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// The effective input budget `B`: the request may use this many tokens before compaction
    /// is due (ADR 0018). Crossing it does **not** stop the request.
    pub input_budget_tokens: u64,
    /// ASCII bytes per token for the local estimate.
    pub bytes_per_token: u32,
    /// Whether the local estimate counts the assistant `reasoning` field (ADR 0014
    /// Q4). The provider only receives that field when the model requires replay, so the
    /// default is `false`: the estimate matches the request actually sent. The host turns it
    /// on when the route's `reasoning_replay` is set.
    pub count_reasoning: bool,
}

/// A rough ASCII bytes-per-token for English and code. It is a warning ruler, not an audited
/// number, and the report says exactly how much of it is estimated.
pub const DEFAULT_BYTES_PER_TOKEN: u32 = 4;

/// ADR 0015 C2: an image counts `ceil(w′ × h′ ÷ 750)` tokens. Providers bill an
/// image by pixels; Anthropic's own rule divides the pixel count by 750.
pub const IMAGE_TOKEN_DIVISOR: u64 = 750;

/// ADR 0015 C2: the long side is scaled down to this many pixels, aspect ratio
/// kept. An image already within it is not scaled.
pub const IMAGE_MAX_SIDE: u64 = 1568;

/// ADR 0015 C2: an image whose dimensions cannot be read from its header counts
/// this many tokens.
pub const IMAGE_FALLBACK_TOKENS: u64 = 1600;

/// The most base64 bytes decoded while probing an image header. A JPEG's frame
/// marker can sit behind its EXIF and thumbnail segments; 64 KiB covers those and
/// stays tiny beside a multi-megabyte image.
const IMAGE_HEADER_MAX_BYTES: usize = 64 * 1024;

impl ContextBudget {
    /// An input budget with the default estimate ratio (4 ASCII bytes/token).
    pub fn new(input_budget_tokens: u64) -> Self {
        Self {
            input_budget_tokens,
            bytes_per_token: DEFAULT_BYTES_PER_TOKEN,
            count_reasoning: false,
        }
    }

    pub fn with_bytes_per_token(mut self, bytes: u32) -> Self {
        self.bytes_per_token = bytes.max(1);
        self
    }

    /// Count the assistant `reasoning` field in the estimate (ADR 0014 R6): the host
    /// sets this when the route's `reasoning_replay` is on, because the field is then really
    /// sent and really occupies input tokens.
    pub fn with_reasoning(mut self, count: bool) -> Self {
        self.count_reasoning = count;
        self
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
    /// Serialized image parts excluded from the text counts (ADR 0015 C2). They
    /// still count toward [`Self::total_bytes`] (the audit number), never toward
    /// [`Self::tokens`].
    pub image_bytes: u64,
    /// The pixel-based estimate of every image part (ADR 0015 C2).
    pub image_tokens: u64,
}

impl RequestSize {
    /// The real byte count of the request, for the audit report.
    pub fn total_bytes(&self) -> u64 {
        self.ascii_bytes + self.non_ascii_bytes + self.image_bytes
    }

    /// The conservative token estimate. ASCII bytes are divided per token (rounded **up**,
    /// so an estimate never rounds down); every non-ASCII character counts as **at least
    /// one** token. Chinese is ~3 UTF-8 bytes per character and 1+ token per character, so
    /// a bare `bytes/4` would undercount it by 25–50% — and for a "stop before overflow"
    /// guard, undercounting means stopping too late. Overcounting only stops a little early.
    /// Image parts count by pixels instead of bytes (ADR 0015 C2).
    pub fn tokens(&self, bytes_per_token: u32) -> u64 {
        let bytes_per_token = u64::from(bytes_per_token.max(1));
        self.ascii_bytes.div_ceil(bytes_per_token) + self.non_ascii_chars + self.image_tokens
    }

    /// Removes one image part's serialized JSON and its separator commas from the
    /// text counts, and records its pixel estimate (ADR 0015 C2).
    fn add_image(&mut self, contribution: &ImageContribution) {
        self.remove_str(&contribution.json);
        for _ in 0..contribution.commas {
            self.remove_str(",");
        }
        self.image_bytes += contribution.json.len() as u64;
        self.image_tokens += contribution.tokens;
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

    /// The inverse of [`Self::add_str`], for a substring that was added and must not count.
    /// Saturating: the caller computes it from a substring of what was added.
    fn remove_str(&mut self, text: &str) {
        for ch in text.chars() {
            if ch.is_ascii() {
                self.ascii_bytes = self.ascii_bytes.saturating_sub(1);
            } else {
                self.non_ascii_chars = self.non_ascii_chars.saturating_sub(1);
                self.non_ascii_bytes = self.non_ascii_bytes.saturating_sub(ch.len_utf8() as u64);
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
///
/// **Revision **: only what a provider actually receives counts. A message's
/// `meta` is host-owned and never sent (ADR 0005), so each non-empty `meta` is subtracted here.
/// Before this, a host that put a large `origin` on a message (ADR 0018's verbatim
/// user messages) inflated the estimate for content the provider never saw.
///
/// **Revision (ADR 0014 R6)**: an assistant message's `reasoning` is likewise
/// subtracted unless the caller passes `count_reasoning` — the provider only receives it when
/// the model requires replay. `request_size` is the default (do not count); the host uses
/// [`request_size_with`] with the route's setting.
pub fn request_size(request: &ModelRequest) -> RequestSize {
    request_size_with(request, false)
}

/// [`request_size`] with the reasoning-count switch (ADR 0014 R6).
pub fn request_size_with(request: &ModelRequest, count_reasoning: bool) -> RequestSize {
    let mut size = RequestSize::default();
    if let Some(system) = &request.system_prompt {
        size.add_str(system);
    }
    size.add_json(&request.messages);
    for message in &request.messages {
        for contribution in image_contributions(message) {
            size.add_image(&contribution);
        }
        let meta = message.meta();
        if !meta.is_empty() {
            // `serde` emits `"meta":<json>` last among the message's fields, so its
            // contribution to the serialized message is `,"meta":<json>`.
            let field = serde_json::json!({ "meta": meta }).to_string();
            if let Some(inner) = field.strip_prefix('{').and_then(|f| f.strip_suffix('}')) {
                size.remove_str(",");
                size.remove_str(inner);
            }
        }
        // ADR 0014 R6: the provider only sees `reasoning` when the model requires it
        // replayed, so subtract the field unless the caller says the model does.
        if !count_reasoning
            && let crate::message::Message::Assistant { reasoning, .. } = message
            && !reasoning.is_empty()
        {
            let field = serde_json::json!({ "reasoning": reasoning }).to_string();
            if let Some(inner) = field.strip_prefix('{').and_then(|f| f.strip_suffix('}')) {
                size.remove_str(",");
                size.remove_str(inner);
            }
        }
    }
    size.add_json(&request.tools);
    size.add_json(&request.settings);
    size
}

/// The header dimensions of an image part, read without decoding the image (ADR 0015
/// C2). `None` when the part is not an image or its header cannot be read.
pub fn image_dimensions(part: &ContentPart) -> Option<(u64, u64)> {
    let ContentPart::Image { data, .. } = part else {
        return None;
    };
    image_dimensions_from_base64(data)
}

/// The pixel estimate of one image part (ADR 0015 C2). An image and its base64
/// bytes never enter the byte-based counts; this is the whole contribution.
pub fn image_tokens(part: &ContentPart) -> u64 {
    image_dimensions(part).map_or(IMAGE_FALLBACK_TOKENS, |(width, height)| {
        image_tokens_for(width, height)
    })
}

/// `ceil(w′ × h′ ÷ 750)` after scaling the long side down to 1568 px (ADR 0015 C2).
fn image_tokens_for(width: u64, height: u64) -> u64 {
    let (width, height) = scaled_image_size(width, height);
    (u128::from(width) * u128::from(height)).div_ceil(u128::from(IMAGE_TOKEN_DIVISOR)) as u64
}

/// Scales `(width, height)` so the long side is at most [`IMAGE_MAX_SIDE`] px,
/// keeping the aspect ratio. An image already within the limit is unchanged.
fn scaled_image_size(width: u64, height: u64) -> (u64, u64) {
    let long = width.max(height);
    if long <= IMAGE_MAX_SIDE {
        return (width, height);
    }
    let scale = |side: u64| -> u64 {
        (u128::from(side) * u128::from(IMAGE_MAX_SIDE) / u128::from(long)) as u64
    };
    (scale(width).max(1), scale(height).max(1))
}

/// The header dimensions from the front of a base64 image, decoding only the
/// prefix the parser needs (ADR 0015 C2). `None` when the header is unreadable.
fn image_dimensions_from_base64(data: &str) -> Option<(u64, u64)> {
    let bytes = data.as_bytes();
    let mut tried = 0usize;
    for target in [48usize, 256, 1024, 4096, IMAGE_HEADER_MAX_BYTES] {
        // A base64 char carries 6 bits: `ceil(bytes / 3) * 4` chars decode them.
        let take = (target.div_ceil(3) * 4).min(bytes.len());
        if take == tried {
            continue;
        }
        tried = take;
        let Some(decoded) = decode_base64_prefix(&bytes[..take]) else {
            continue;
        };
        if let Ok(size) = imagesize::blob_size(&decoded)
            && size.width > 0
            && size.height > 0
        {
            return Some((size.width as u64, size.height as u64));
        }
        if take == bytes.len() {
            break;
        }
    }
    None
}

/// Decodes a base64 prefix. The prefix is cut at common lengths, so it may end in
/// a partial group or padding; both are handled. `None` on a byte outside the
/// standard/URL-safe alphabets.
fn decode_base64_prefix(input: &[u8]) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }

    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    let mut at = 0;
    while at + 4 <= input.len() {
        let a = value(input[at])?;
        let b = value(input[at + 1])?;
        let c = input[at + 2];
        let d = input[at + 3];
        if c == b'=' {
            if d != b'=' {
                return None;
            }
            out.push((a << 2) | (b >> 4));
        } else {
            let c = value(c)?;
            out.push((a << 2) | (b >> 4));
            out.push((b << 4) | (c >> 2));
            if d != b'=' {
                let d = value(d)?;
                out.push((c << 6) | d);
            }
        }
        at += 4;
    }
    match input.len() - at {
        0 => {}
        1 => return None,
        2 => {
            let a = value(input[at])?;
            let b = value(input[at + 1])?;
            out.push((a << 2) | (b >> 4));
        }
        3 => {
            let a = value(input[at])?;
            let b = value(input[at + 1])?;
            let c = value(input[at + 2])?;
            out.push((a << 2) | (b >> 4));
            out.push((b << 4) | (c >> 2));
        }
        _ => unreachable!(),
    }
    Some(out)
}

/// One image part's contribution to a serialized message (ADR 0015 C2): the JSON
/// text that must not count as text, its pixel token estimate, and how many
/// separator commas the enclosing content array loses when every image is
/// dropped. A caller subtracts `json` and `commas` commas from its size, and adds
/// `tokens`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageContribution {
    pub json: String,
    pub tokens: u64,
    pub commas: usize,
}

/// The image parts in a message's **content** (user/assistant `content` or a tool
/// result's `content`). `reasoning` is not content: it is only sent when the
/// model requires replay, and the existing `reasoning` subtraction governs it.
pub fn image_contributions(message: &Message) -> Vec<ImageContribution> {
    let content = match message {
        Message::User { content, .. } | Message::Assistant { content, .. } => content.as_slice(),
        Message::Tool { result, .. } => result.content.as_slice(),
    };
    let images = content
        .iter()
        .filter(|part| matches!(part, ContentPart::Image { .. }))
        .count();
    if images == 0 {
        return Vec::new();
    }
    // A content array of `n` parts has `n - 1` commas. Removing `k` image parts
    // leaves `max(0, n - k - 1)` commas, so `min(k, n - 1)` go away.
    let commas = images.min(content.len().saturating_sub(1));
    let mut out = Vec::with_capacity(images);
    let mut first = true;
    for part in content {
        if matches!(part, ContentPart::Image { .. }) {
            out.push(ImageContribution {
                json: serde_json::to_string(part).unwrap_or_default(),
                tokens: image_tokens(part),
                commas: if first { commas } else { 0 },
            });
            first = false;
        }
    }
    out
}

/// The conservative token estimate for one request (ADR 0015 C2), exposed so a host can
/// show the status bar number and so the bound is testable on its own.
pub fn estimate_tokens(request: &ModelRequest, bytes_per_token: u32) -> u64 {
    request_size(request).tokens(bytes_per_token)
}

/// [`estimate_tokens`] with the reasoning-count switch (ADR 0014 R6).
pub fn estimate_tokens_with(
    request: &ModelRequest,
    bytes_per_token: u32,
    count_reasoning: bool,
) -> u64 {
    request_size_with(request, count_reasoning).tokens(bytes_per_token)
}

/// What the guard computed for one request. Host-visible, so it is honest about its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetReport {
    pub source: UsageSource,
    /// The projection at the moment of the check (`baseline + growth`, or pure estimate).
    pub projected_tokens: u64,
    /// The input budget `B` the projection is compared against (ADR 0018).
    pub input_budget_tokens: u64,
    /// The real byte count of the request the projection was computed from.
    pub request_bytes: u64,
    /// The provider-reported `input` baseline, when there was one.
    pub baseline_input_tokens: Option<u64>,
    /// The estimated growth added on top of the baseline. `Some` iff there was a baseline;
    /// `0` only when the request had not grown. This is the part a host must not present as
    /// a provider report.
    pub estimated_growth_tokens: Option<u64>,
}

/// The projection's state for a session (ADR 0015 C1, ADR 0018).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetStatus {
    /// No input budget configured: no number is claimed.
    Disabled,
    /// The projection is at or below the input budget `B`.
    Ok(BudgetReport),
    /// The projection is over `B`. The request is **still sent** (ADR 0018); automatic
    /// compaction triggers on this state in the next slice.
    Over(BudgetReport),
}

impl BudgetStatus {
    /// The report, if any (both `Ok` and `Over` carry one).
    pub fn report(&self) -> Option<&BudgetReport> {
        match self {
            BudgetStatus::Disabled => None,
            BudgetStatus::Ok(report) | BudgetStatus::Over(report) => Some(report),
        }
    }
}

/// Whether the projection is **over** the input budget `B` (ADR 0018). `T == B` is still
/// within the budget: compaction is due only strictly above `B`. Pure, so the boundary is
/// testable on its own.
pub fn over_budget(report: &BudgetReport) -> bool {
    report.projected_tokens > report.input_budget_tokens
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
        input_budget_tokens: budget.input_budget_tokens,
        request_bytes: size.total_bytes(),
        baseline_input_tokens,
        estimated_growth_tokens,
    }
}
