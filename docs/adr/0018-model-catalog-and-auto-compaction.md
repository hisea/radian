# ADR 0018: Model Catalog and Auto Compaction

Status: accepted

## Background

Windows need human filling, and the fill is wrong. Near the limit the code could only stop. No compaction existed. This ADR defines the window source, the effective input budget, auto compaction, the over-long fallback, and failure paths.

## Decisions

### 1. Window sources

In this order. The first present source wins:

1. `--context-window` (explicit, always first);
2. models.dev catalog by `(provider, model)`;
3. **Descriptor-built-in window table** (currently only `chatgpt`; numbers quoted from the `@oh-my-pi/pi-catalog` `openai-codex` block; with a catalog entry the code **never** overrides the catalog);
4. Unknown (guards off, no guessed numbers).

The catalog keeps three numbers separately, **never merged**: total context `C = limit.context`, input limit `I = limit.input` (absent means no separate limit), output limit `limit.output`. 0 or missing reads as unknown. `--context-window` overrides `C`, never `B`.

**Fetch order**: fresh cache (within 24 hours), then network (3-second timeout), then stale cache, then **built-in snapshot** (compiled into the binary, keeps only providers and fields radian uses, records fetch date), then none. With explicit `--context-window` the code never goes to network. It reads only an existing cache. `--offline-catalog` reads cache only. Cache writes use 0700 and 0600 with atomic write. Failures fall back silently. The startup line states the source: explicit, catalog (with cache time), descriptor, built-in snapshot, or unknown.

**Match safety**: the models.dev provider derives from the `--base-url` hostname (mapping table in code, with tests). `--catalog-provider` overrides it. Matching uses hostnames only, never paths. **Interface mode** (ADR 0026 D0) follows the catalog entry `provider.npm`: `@ai-sdk/openai-compatible` means chat, `@ai-sdk/anthropic` means Anthropic, `@ai-sdk/openai` means Responses. Any other value is an explicit rejection. **Addresses never come from the catalog**. Each built-in descriptor hard-codes its own address in code.

### 2. Effective input budget

`B = min(I, C − R)`.

- `R = max(min(16384, ⌊C/4⌋), largest output actually sent in this request)`: one quarter of the window, capped at 16K. The largest output actually sent in the request can raise it. The catalog `limit.output` never joins `R`. It only checks that the request value never exceeds it.
- With unknown `C`, `R` drops out. `C ≤ R` is a startup error. `B = 0` reads as unset.

### 3. Auto compaction

- **On by default**. `--no-auto-compact` or configuration turns it off.
- **Trigger**: the ADR 0015 projection (reported value plus conservative estimate) serves as current input `T`. When `T > B`, the code compacts before the next model request. Checkpoints: after each tool-result batch write, and before a new user message opens a new turn.
- **Keep target**: `K = min(20000, max(2048, ⌊0.25·B⌋))`. It is a target, not a hard split point: the real keep must also satisfy atomicity, summary space needs, and request limits. The compacted request (system prompt plus summary plus kept part) must stay `≤ B`. When it cannot (the latest legal fragment alone exceeds `B`), the code fails explicitly.
- **Splits**: split only at user or assistant messages. **One assistant message plus all tool calls it starts plus all their results form one atomic unit**. The code never keeps only part of one unit. With a long tool chain, splits can land on assistant messages inside the chain. Split points must still fall between atomic units. The compacted transcript must pass `validate_history`. Interrupted, cancelled, or result-less calls follow ADR 0002 and ADR 0003 rules. They stay with their calls or enter the summary together.
- **Gain**: `T_after < T_before` and `T_after ≤ B` are required. Otherwise the error is `InsufficientCompaction`. The code never commits. The turn ends as failed with a stated reason. The goal is `T_after ≤ 0.75·B`. With only the goal missed, the code commits as usual and marks `below_target`. The rest of this turn never auto-compacts again. Later requests in this turn that exceed `B` again end with `InsufficientCompaction`. Each model request triggers at most one auto compaction. Manual `/compact` requires only `T_after < T_before`.
- **Summary model**: `[compaction] model = "current"` (default, uses the current model; builds no second provider route layer).
- **Summary reasoning tier**: `[compaction] thinking = "<tier>" | "current"`, default **`low`**. Tier names match the seven ADR 0026 D11 tiers. The host hands converted live values to Core as `AgentConfig.compaction_settings` (a JSON object). Core places them verbatim into summary requests and shortened retry requests in `settings`. **It never places them into dialogue requests**. `"current"` means no override. Adapter rules unify as "request `settings` beat configuration". The host clamps unsupported tiers to the nearest one with one notice.
- **Summary structure** (fixed order): `Goal` (quote user words where possible), `Constraints`, `Progress` (`Done` / `In progress` / `Pending`), `Verified facts (do not re-check)` (only facts truly verified in the session, each stating how it verified; plans and hypotheses excluded; time order, new facts appended at end; when too long, drop unimportant details in other sections first; drop a fact only when unrelated to goals and current work, oldest first; cap at one third of the whole summary), `Current work`, `Next step` (one step only, directly after `Current work`), `Key decisions`, `Critical context`, `Files read and modified`. Serialization truncates each tool result to 2000 characters. Repeated compactions roll the last summary forward as input.
- **Summary cap**: `summary_max_tokens(B) = min(K, max(8192, ⌊B/8⌋))`. Without a budget the cap is fixed 8192. `summary_system_prompt(max_tokens)` writes the cap into the system prompt. An over-long retry switches to `shorten_request`. It sends only the over-long summary, targets 80% of the cap, and keeps the same section structure. An empty summary resends the original request. A valid summary is non-empty and within the length cap.
- **User words stay verbatim**: the summary message (`User`, `source = System`, `origin.kind = "compaction_summary"`) has two parts: the model summary and user verbatim quotes. `check_summary` and the summary length cap govern only the model summary. Only `InputSource::User` messages enter (a missing `source` counts as User at the summary layer; steered user messages count). `System`, `SlashCommand`, `Scheduler`, `Mcp`, `Subagent`, and `PermissionDecision` never enter. Attachments (standalone text fragments with `annotations = {"kind":"attachment","path":"P"}`) become one line `[file: P, N lines]`; an `Image` part becomes one line `[image: W×H]`, or `[image]` when its dimensions cannot be read (the ADR 0015 C2 header parse). An image in a tool result stays in that result and enters the summary with it.
  - Total `U = max(512, min(8192, ⌊B/16⌋))` governs user messages after the first: newest first until full, keeps a continuous run of latest messages. Earlier messages collapse into one line `[N earlier user messages omitted]` (N accumulates across turns). The latest message always stays. Past `U`, it truncates to `U`.
  - Single-message cap `U₁ = min(2000, U)` governs the first session user message: always kept. Past the cap, it keeps about 3/4 from the start and 1/4 from the end, with `[… N tokens omitted …]` in the middle.
  - Without a budget, `U = 8192` and `U₁ = 2000`. Token counting uses the same conservative ruler as `message_tokens`. Output follows time order. Verbatim quotes never join the model-summary length cap, but they join `T_after ≤ B`. The keep plan subtracts verbatim actual size first.
  - Rolling: the last verbatim list never goes to the model. A summary request carries only the last model summary and newly summarized dialogue. The new verbatim list equals the last list plus newly summarized user messages, then both budget layers apply again. The first message stays first.
  - Read-back reads only `origin.user_messages`. It never parses body text. An old session without the field reads back as an empty list. Body verbatim quotes render from the same list.
  - The keep plan subtracts verbatim first. `request_size` never counts host meta, so verbatim counts body text only once.

### 4. Persistence and billing

Write order per compaction:

```text
CompactionUsageRecorded { revision, model, usage, fee? }          (real call, always billed)
ContextCompacted { revision, source_revision, source_len, summary, first_kept, tokens_before, tokens_after, below_target, model, deterministic? }   (candidate)
TranscriptReplaced { revision, len }
MessageAppended × len
TranscriptReplacementCommitted { revision, len }                  (commit point)
```

- `ContextCompacted` is only a candidate record. The compaction takes effect only when the same-revision commit reaches disk. With candidate but no commit, recovery ignores this compaction. History stays pre-compaction (ADR 0019). Original records stay. Export and search still see them.
- `CompactionUsageRecorded` and the `CompactionUsage` event: the code writes and sends them once the summary call returns, whether the compaction later commits or aborts. It is not `ModelUsageRecorded`. Resume never uses it as a budget baseline. Each summary attempt records once (a controlled retry is a second real call). With no provider-reported usage, the code neither sends nor writes.
- Compaction runs only at driver step boundaries. `ContextCompacted` records the source view (`source_revision`, `source_len`). Before commit, Core checks that the memory transcript is still this view. Otherwise it drops this compaction. A compaction revision must strictly exceed the last committed revision. On the read side, a committed revision that does not increase reports a diagnostic. It never refuses.

### 5. Failures

| Error | Behavior |
|---|---|
| Provider temporarily unavailable, rate-limited | Retries per ADR 0017 strategy; turn `failed` on exhaustion |
| User cancels | Stops compaction, never commits |
| Invalid summary (empty, over-long, `validate_history` rejects) | Auto path enters deterministic fallback (§6); manual `/compact` reports an error |
| Journal write fails | `RecoveryRequired` (ADR 0001, ADR 0019; file recovery result is pre-compaction) |
| Still over `B` after compaction | Sends no request; turn `failed` with stated reason |
| Insufficient gain (`InsufficientCompaction`) | Auto path tries deterministic fallback first; then `failed`, no second compaction before the same request |

The code never sends a certainly over-long request after failed compaction. It never retries without bound.

### 6. Overflow fallback and deterministic fallback

- **Active compaction first reads the budget**. With unknown window (`B` uncomputable), no active compaction runs. Only the overflow fallback remains.
- On a recognizable provider "context over-long" error (each adapter `classify_overflow`; HTTP only at 400 and 413 and only when recognizable; 429 and 5xx excluded; in-stream error without status code follows the same code and message recognition), the turn compacts **once** and resends the request **once**:
  - This check runs after `model_call_with_retries` returns `ContextOverflow`. `ContextOverflow` is not transient (`is_transient()` false), so ADR 0017 retries never consume it first. Other model errors end the turn as failed directly, with no compaction.
  - `reference = min(T_before, B)` (window known). With unknown window, `reference = T_before`. `T_before` is the rejected-request projection.
  - Gain rule `Gain::Half { reference }`: commit requires `T_after * 2 <= reference`. The error-text goal writes `reference / 2`.
  - Compaction uses `CompactionTrigger::Overflow`, keep target `compaction::MAX_KEEP_TARGET`, summary budget from the input budget (when configured). The auto path allows deterministic fallback, so the overflow fallback allows it too.
  - The resend never counts into `model_calls` (`model_calls` already incremented on entry to this model call).
  - A second overflow after resend ends the turn as failed. The adapter error text appends `(compacted once and retried; still over the window)`. `NothingToCompact` ends as failed with `(nothing to compact)` appended.
  - `--no-auto-compact` closes only the active path. The overflow fallback stays.
- **Deterministic fallback** (no model call): when the auto path (active and overflow fallback) ends a summary path with `InvalidSummary` (still invalid after shorten retries), `Summary(ModelError)` (after retries exhaust), `Insufficient`, or `NothingFits`:
  - Summary message equals the last model summary verbatim (when present, not one word changed) plus one fixed line `[context compacted without a summary: N messages dropped, M tool results truncated]` plus user verbatim quotes (same rules and budgets).
  - From the normal split plan: ① truncate each tool result over 2000 characters in the kept part to that length, ending with `[truncated N characters]`; ② when still over target, drop the oldest atomic unit in the kept part (one assistant message with all calls it starts and all results). Dropped units' user messages enter the verbatim list. ③ repeat ② until the target holds. **The newest atomic unit always stays**. When even it plus the summary message cannot fit, the result is `NothingFits` and the turn ends as failed.
  - Targets follow the triggering path (active `≤ B`, 0.75 rule and `below_target` unchanged; overflow fallback `≤ reference / 2`).
  - `ContextCompacted.deterministic = true`. **No** `CompactionUsageRecorded` (no call happened). `CompactionFinished.deterministic = true` with `dropped` (transcript message count absent from the new view).
  - **Manual `/compact` never takes this path**: it reports the error to the user, and the user decides.
- An error unrecognized as overflow is always an ordinary model error (ADR 0017: failed, no compaction, no retry). The code never guesses.

### 7. Manual and off

- TUI and CLI provide `/compact [note]`: requires only `T_after < T_before`, needs no budget configuration. `instructions` go to the summary request. At runtime it is a control request. The driver runs it **between two turns**. A request received mid-turn runs after the turn ends. Cancellation (Ctrl-C or Esc) never commits.
- `--no-auto-compact` closes only the active path. The overflow fallback stays. `[compaction] auto` turns it off.

### 8. Boundary with Memory

The compaction summary only serves continuation of the current session. **It never serves long-term memory**. Core and compaction logic never depend on any Memory extension. With Memory off, compaction works as usual. Memory reads only **committed** session journals. It never reads uncommitted replacements or candidate-only compactions.

### 9. Known limits

- With unknown window only the overflow fallback exists: `reference = T_before`, goal `≤ T_before / 2`. Keep target, summary cap, and verbatim cap are fixed values (verbatim at most about 10k, summary 8192, keep 20000, combined about 38k, plus system prompt and tool definitions). So when `T_before / 2` falls below this fixed total, the auto path first tries deterministic fallback. When even the fallback misses the goal, it fails with `NothingFits` (`compaction_overflow.rs::a_fallback_that_cannot_reach_the_target_is_nothing_fits`).
- DeepSeek and opencode real over-long error bodies fall under the OpenAI-compatible generic rules. When real wording is absent from the table, add one entry per observed wording.

## Verification

- `radian-core/tests/compaction_flow.rs` covers journal order, rolling summaries, atomic tool units, cancellation, insufficient gain, `below_target` stop, invalid-summary retry, replacement abort, baseline clearing, source-view change, candidate without commit, deterministic fallback, and `NothingFits`.
- `compaction_overflow.rs` covers the current single-tier overflow fallback: unknown window compacts to `T_before/2` and retries once (`an_overflow_with_no_window_compacts_to_half_and_retries_once`), a retry that still overflows fails after one retry (`a_retry_that_still_overflows_fails_after_one_retry`), non-overflow model errors never compact (`a_non_overflow_model_error_does_not_compact`), known window uses `min(T_before, B)` (`a_known_window_uses_min_of_projection_and_budget`), fallback below target means `NothingFits` (`a_fallback_that_cannot_reach_the_target_is_nothing_fits`), active compaction still allows one fallback after it (`the_fallback_is_allowed_once_after_a_proactive_compaction`), plus `compact_now` cases.
- `compaction.rs` covers summary caps scaling with window, system prompt with cap, shorten requests with only the over-long summary, user verbatim two-layer budgets and `origin` read-back, attachment and image placeholder lines, old-summary empty-list read-back.
- `radian-runtime/tests/compact_control.rs` covers run between two turns, mid-turn wait until turn end, cancel without commit, `NothingToCompact`.
- `radian-exec` and `radian-tui` pty and cli tests cover `/compact`, the startup line, `--no-auto-compact`, and overflow notices.
- Fixtures: `compaction-committed.jsonl`, `compaction-candidate-only.jsonl`, `compaction-verbatim.jsonl`, `compaction-deterministic.jsonl`, with the reference checker in sync.
