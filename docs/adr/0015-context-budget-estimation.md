# ADR 0015: Context Budget Estimation

Status: accepted

## Background

A read-heavy turn can fill the provider context window within a few turns. Auto compaction (ADR 0018) needs to know "how large the next request is". But provider usage reports can be missing or stale. This ADR defines only **estimation and projection**: reported and estimated values stay strictly distinguishable, estimates round up, and resume rebuilds the baseline. It produces no user-visible stop. It never changes the transcript.

## Decisions

### C1: Usage sources never mix

"This request is how large" has no single honest answer. Types separate three cases:

1. **`Reported`**: the projection is exactly one provider-reported `input`, with no growth.
2. **`ReportedPlusEstimate`**: the baseline is a provider-reported `input`, plus an estimate of content added after that request. `BudgetReport::baseline_input_tokens` (reported part) and `estimated_growth_tokens` (estimated part) split the two numbers. `projected_tokens = baseline + growth`.
3. **`Estimated`**: no reported `input` exists. The whole projection is a local estimate.
4. **Unavailable (`Disabled`)**: no context window is configured. The code guesses no window and reports no number.

A `ModelUsage::input` of `None` means "not reported" (ADR 0007). It never reads as 0.

Host display must mark the source: `Reported` can print an exact value. `ReportedPlusEstimate` must print both parts (`reported 12,000 + estimated 340`). `Estimated` must print an estimate mark. `Disabled` prints no number or "context not configured". **A mixed reported-plus-estimated value must never carry a reported label as a whole.**

### C2: Projection formula and request size

- `request_size`: `system_prompt`, `messages`, `tools`, and `settings`, each as serialized characters, split into `ascii_bytes` / `non_ascii_bytes` / `non_ascii_chars`. It takes the **final request shape** (after the `before_model_call` hook runs, the exact bytes sent). The estimate counts only content sent to the provider. It never counts host meta (ADR 0005). `budget::request_size` subtracts `meta` from each message serialization.
- Estimate: `tokens = ceil(ascii_bytes / bytes_per_token) + non_ascii_chars`.
  - ASCII bytes **round up** (`div_ceil`). Rounding never undercounts.
  - **Each non-ASCII character counts at least 1 token**. An underestimate means late compaction, which can overflow. An overestimate only compacts earlier. This is a documented conservative lower bound.
- `Reported` (no growth) and `ReportedPlusEstimate`: `growth = saturating_sub(tokens(now), tokens(baseline_size))`, `projected = baseline_input + growth`. `saturating_sub` covers the floor.
- `Estimated`: `projected = tokens(now)`.
- `bytes_per_token` defaults to 4 (**ASCII only**). The host can change it. It is a coarse early-warning ruler, not an audit number.
- With `count_reasoning` on (default off), the estimate counts the assistant `reasoning` field. The host turns it on when the route opens `reasoning_replay` (ADR 0014 R6).
- **An image counts by pixels, never by its base64 bytes.** A `ContentPart::Image` part does not contribute to `ascii_bytes`; instead it counts `ceil(w′ × h′ ÷ 750)` tokens, where `(w′, h′)` is `(w, h)` scaled down so that the long side is at most **1568** px with the aspect ratio kept (an image already within 1568 px is not scaled). When the dimensions cannot be read from the image data, the part counts a fixed **1600**. The constants are 750, 1568 and 1600. One rule serves every provider (OpenAI's tile rule stays within 2×). The code reads only the image header (`imagesize`, no decode) and decodes only the base64 prefix that header parse needs, never the whole image. The image part's JSON (the base64 string) never reaches `ascii_bytes`; the message's other fields count as before. `RequestSize::request_bytes` stays the real byte count.

**Not a real tokenizer**: the code introduces no tokenizer and writes no exact ratio for any model. The rules above are only conservative rounding-up assumptions.

### C3: Budget `B`

`B = min(I, C − R)`: `I` is the input limit (skipped from min when absent), `C` is total context, `R` is the output reserve.

- `R = max(min(16384, ⌊C/4⌋), largest output actually sent in this request)`. The largest output actually sent in the request (the user `--max-tokens`, or the default the adapter sends in the request) can raise `R`. A provider that sends no such field never raises it. The catalog `limit.output` never joins `R`. It only checks that the request value never exceeds it.
- `--context-window` overrides total window `C`, never `B`.
- With unknown `C`, `R` drops out (`B = I` or unknown).
- `C ≤ R` is a startup error. `B = 0` reads as unset (protection and compaction triggers stay off).
- `[model] context_window = 0` equals unset. `--context-window 0` on the command line is a usage error (exit code 2). Both hosts reject it before they create the session file.

### C4: `BudgetStatus`

`ContextBudget { input_budget_tokens, bytes_per_token, count_reasoning }`. `T > B` records `BudgetStatus::Over(report)`. **The estimate itself is never a user-visible stop**: it never rejects a request, never changes the transcript, and never produces a `RunStatus` or exit code. The driver and auto compaction (ADR 0018) consume this projection. The default compacts before send. Only when active compaction is off or skipped does an over-budget request send as usual, and the turn ends as usual. The status bar shows `T / B`.

`AgentEvent::ModelRequestStarted` carries `budget: Option<BudgetReport>`. The host status bar shows `T / B` from it.

### C5: Constants and resume baseline

- **Restore**: Core provides `AgentSession::restore_budget_baseline(&mut self, prefix_len: usize, input_tokens: u64) -> bool`. At `Driver::open`, runtime finds the last `ModelUsageRecorded` with `input` inside the current transcript revision of the session file. It takes the index of the assistant `MessageAppended` before it as `prefix_len`. It hands both to Core. Core recomputes `RequestSize` over `transcript[..prefix_len]` with the current system prompt, tools, and settings.
- The baseline uses `ModelUsage::prompt_total()` (the full request input, including cache reads and writes). The OpenAI shape is `input` itself. The Anthropic shape adds `cache_read` and `cache_write`. Old records (no mark) trust `input`.
- Reports before a `TranscriptReplaced` never join (indices already reset). `CompactionUsage` never serves as baseline. With no input-carrying report, the code stays purely estimated and reports no error. A `prefix_len` past the transcript returns `false` and stays estimated (it never invents 0). After restore, the source label is still `ReportedPlusEstimate`.
- **Known skew**: the file holds only the provider-reported `input`, never the actual bytes of that request. Recomputation uses the restore-time configuration and tool list. Current configuration larger than then leads to a small projection (later compaction). Current configuration smaller than then leads to a large projection (earlier compaction). When a hook rewrote the request then, restore never reruns hooks. Protection prefers "compact early", so the main record keeps the conservative side.

### C6: `/context` shows the estimate by category

- `/context` splits the next formal request shape by category: base system prompt, context files (ADR 0036), memory injection (ADR 0021), tool definitions, user messages in history, assistant text, reasoning (listed only with `count_reasoning` on), tool results, and images. The images line is the C2 pixel estimate of every image part, so the user sees how much of the estimate is images. Each line shows estimated tokens and share. The last line is total estimate `T / B` (with window `C` when known, as `T / B (window C)`).
- Same estimate rule (C2), no tokenizer. Category sizes add first, then one `div_ceil` runs. The remainder goes to the category with the largest fraction: **category estimates sum exactly to the total estimate**.
- Stack identity: category sum (used `T`) plus free space before auto-compact plus reserve buffer equals the window. Free space is threshold `B` minus used (0 when over budget). Buffer is window minus `B` (the gap from output reserve and input limit). `B` is the auto-compaction threshold (ADR 0018).
- Report lines always carry separate labels (C1): the latest provider-reported input (`prompt_total()` of the last `ModelUsageRecorded` inside the current transcript revision; reset after `TranscriptReplaced`). With no report, the line reads "none yet". It never shares a label with an estimate.
- The view also carries message counts and session compaction counts (committed `TranscriptReplacementCommitted` count). Read-only command: it reads the committed view of the session file and the system layers stored at session start. It never touches inbox or driver. It works mid-turn and never interrupts a turn.
- The split result is a pure data structure, independent of rendering. View code only renders it. Presentation changes touch only the view, never the split.

## Constraints and costs

- The estimate is a conservative lower bound, never an exact value. `bytes_per_token` and "each non-ASCII character counts at least 1 token" only guarantee no underestimate.
- The resume baseline skews when configuration changes or a hook rewrote the request. The direction is recorded above. Only "same configuration and no hook rewrite" restores a projection field-equal to uninterrupted projection.

## Verification

`crates/radian-core/tests/budget.rs` covers: with no active compaction or skipped compaction, a report value past `B` still sends the request and the transcript stays unchanged; default auto compaction compacts first past the budget (tests live in ADR 0018); no report marks `Estimated`; unconfigured window marks `Disabled`; CJK text counts at least 1 token per character; ASCII bytes round up per `bytes_per_token`; reported-plus-estimated splits its labels; large host meta never changes the estimate; the resume baseline equals uninterrupted projection; out-of-range `prefix_len` stays estimated. `radian-runtime` covers resume baseline restore, staying estimated with no reports, the Anthropic shape through `prompt_total()`, and old records through `input`. `radian-host` `context_estimate` covers category sum equals total estimate, one count per message and per tool definition, reasoning listed only when counted, categories plus free space plus buffer equals window, and reported and estimated lines labeled separately.
