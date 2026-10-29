# ADR 0044: `radian exec --output jsonl` and Outcome Exit Codes

Status: accepted

## Background

Headless mode owns no machine-readable output. `-p` stdout carries only model text (ADR 0032 A3). Structured info needs journal reads or control-socket connects. "Unfinished, over-budget, cancelled, failed" all exit 1 in `radian exec`, and benchmarks cannot tell them apart.

## Decisions

### 1. `--output jsonl`: one JSON object per stdout row

Default output (no `--output`) stays byte-identical: `-p` still carries only model text, line mode still streams text. `--output jsonl` makes stdout carry only JSON rows:

- **Durable rows**: 0006 journal records as-is (same serialization, byte-identical), all from `SessionCreated` on. Consumers filtering by `event` get the journal stream.
- **Live rows**: `{"live": true, "event": "ModelTextDelta" | "ModelReasoningDelta", "data": {"text": ...}}`. `live` marks never-on-disk. `event`/`data` reuse journal envelope shapes. Live rows with durable rows take two channels, **with no order promised between them**.
- **The last row is a `Result` record**: `{"type": "Result", ...}` (§2). It is no journal event. It uses `type`, never `event`. `event`-only consumers find no place for it in replayable streams. It prints after driver close, after the last durable row.

State, hints, errors still use stderr (all modes). Default-mode reasoning uses the stderr `[reasoning]` channel per ADR 0014. Jsonl-mode reasoning deltas are live rows into stdout.

Implementation: `SessionJournal::set_tee` installs the sender under the write lock and returns the then-`written_len`. The exec side first prints the file first `written_len` bytes (rows written before tee install), then prints tee-delivered rows. Install and each write take the same lock, so no row drops or prints twice. A solo task writes stdout, blocked stdout pipes never back-pressure journal writes. Failed writes (injected crashes) send no tee.

### 2. `Result` records

| Field | Meaning |
|---|---|
| `session_id` | Session identity |
| `outcome` | Core `RunStatus` snake_case (0006 §5.6 wire words): `completed`, `refused`, `budget_exceeded`, `failed`, `poisoned`, `incomplete`, `cancelled`. Future states read `unknown` |
| `exit_code` | §3 exit code |
| `stop_reason` | Outcome detail: `completed`, `failed`, `budget`, `deadline`, `max_tokens`, `content_filtered`, `tool_use_without_calls`, `unknown:<raw>`, `refused`, `cancelled`, `terminated`, `wake_budget` (wake budget spent, see §2a) |
| `turns` | `TurnFinished` rows in file (turns run in this session, pre-`--resume` included) |
| `usage` | ADR 0007 shaped session totals: only reported counters appear (`input`, `output`, `cache_read`, `cache_write`). "Unreported" means absent, never 0. No records means `{}` |
| `cost` | fee `micros` totals: `{"micros", "incomplete", "estimated"}`. No fee records means `null`. True `estimated` means resume recalculated old records against the current price table |
| `run_turns` | `TurnFinished` rows written by this process: suffix fold after tee install (after `set_tee` returned `written_len`). No resume means equal to `turns` |
| `run_usage` | usage totals written by this process: same shape as `usage`, counts suffix only. No resume means byte-equal to `usage` |
| `run_cost` | fee totals written by this process: same shape as `cost`, counts suffix only. `estimated` is always false. Resume recalculation runs only in memory (before tee install) and writes no new journal rows, so the suffix holds no recalculated old records, and its fees all compute live. No fee records means `null`, no resume means byte-equal to `cost` |
| `text` | Last assistant message text joined. No assistant messages means `""` |
| `model_error` | Present only when the run ended because a turn failed on a `ModelError`, absent on every other ending. Shape: `{"kind", "status", "retry_after_secs"}` (§2b) |
| `unhandled_job_notices` | Job completion notices with no turn to meet: appears only on spent wake budget, all other outcomes own no such field |
| `unhandled_stop_notices` | Stop continuation verdicts with no turn to meet (`exit 2` degraded by gates): appears only on spent wake budget **with** such notices, all other outcomes and pure job degrades own no such field |

Two `stop_reason` values are fixed decisions, never judged from model text:

- `terminated` covers only two abnormal ends: `Poisoned` (in-flight futures dropped, session uncontinuable) and the SIGTERM path (128 + 15). The second owns `cancelled` as `outcome`.
- "Model quits after tool refusal" lists nothing separately: Core records `Completed`, model-text judging is unreliable, and it exits 0. Exhausted empty-response retries are model errors (ADR 0017), still exit 1, no `radian-core` change.

`max_tokens` means the turn ended on a truncated response with no tool calls after two truncated responses in a row had already continued it (ADR 0017 §2). A truncated response with tool calls continues the turn and never makes this outcome by itself. A budget or the call cap that forbids the continuation wins instead (`budget`, ADR 0017 §2).

Other off-path ends: `NotStarted` (input refused before turns) with stateless stops read `failed`/`failed`/1. Clean interactive leave with no turns reads `completed`/`completed`/0. Driver self-stop reads `completed` when clean, else `failed`.

### 2a. `-p` waits for background jobs before exit

`-p` exits at "one turn plus the chain it wakes". Finished turns with still-running background jobs keep exec waiting. Job ends wake next turns through ADR 0037 `followup`. Waiting ends with no running jobs and no pending wakes. Exit code with `Result` takes the last turn. No background jobs means behavior unchanged.

Topped wake budgets (ADR 0037 decision 5 gates) make waiting pointless: degraded-to-inject-only notices never meet turns. Exec then ends the session. `outcome` reads `budget_exceeded`. `stop_reason` reads `wake_budget`. `unhandled_job_notices` counts those notices. Exit code is 3 (§3 budget class). Degraded `Stop` hook continuation verdicts work the same, whatever degraded them: the wake budget, the built-in validator's two-in-a-row cap, or the consecutive idle hook-continuation cap (ADR 0045 §3). No running jobs, no pending wakes, never busy. exec still ends the session. Exit code is 3. `unhandled_job_notices` counts job notices (can be 0), `unhandled_stop_notices` counts Stop verdicts. Timeout with SIGTERM behavior stays unchanged, no extra timers added.

### 2b. `model_error`: the structured failure behind a failed turn

When the run ended because a turn failed on a `ModelError` (ADR 0017), the `Result` object carries `model_error`:

```json
{"kind": "usage_limit", "status": null, "retry_after_secs": 6240}
```

- `kind` is one closed set, one value per `ModelError` variant, snake_case: `transport`, `protocol`, `unavailable`, `usage_limit`, `context_overflow`, `empty_response`. The list is declared once in code, by `model_error_kinds!` in `radian-core/src/model.rs`, which generates both `radian_core::ModelErrorKind` and `radian_core::MODEL_ERROR_KINDS`. The serializer writes `ModelErrorKind::as_str()`, and the docs drift test and `check-fixtures.sh` read that same declaration; there is no second literal set.
- `status` is the HTTP status the variant carries (`Unavailable.status`), else `null`.
- `retry_after_secs` is `Unavailable.retry_after` or `UsageLimit.resets_in` in whole seconds, rounded up, else `null`.
- The field is absent on every other run end: completed, cancelled, budget, a hook error, a tool failure. It is never `null`.
- It carries no message text: the message stays on stderr as before. Exit codes do not change.

The field carries the `ModelError` itself from the failing turn to the `Result` writer; no host parses the message text.

### 3. Exit codes split by outcome (all exec modes, never jsonl-only)

Core `RunStatus` (`#[non_exhaustive]`, unnamed new states take the wildcard arm, exit 1):

| Code | Outcome |
|---|---|
| 0 | `Completed` |
| 1 | `Failed`, `Poisoned`, states unnamed in this table |
| 2 | Usage/setup errors (unchanged) |
| 3 | `BudgetExceeded`; `Cancelled(DeadlineExceeded)`; spent wake budget (`-p` waiting on background jobs meets closed gates, `stop_reason` reads `wake_budget`) |
| 4 | `Incomplete(_)` (max tokens, content filtered, tool use with no calls, unknown stop reasons); `Refused` |
| 130 | `Cancelled(CancelReason::Requested)`; double Ctrl-C (unchanged) |
| 143 | SIGTERM (128 + 15, unchanged) |

### 4. `-p -` reads prompts from stdin

`-p -` reads stdin to EOF as the whole prompt, and starts only after reads finish. A literal `-` prompt is impossible. Combined with `--output jsonl`, scripts drive it. `--help` states the rule.

### 5. Not built

`--output json` (one whole object), new events past streaming, control-port clients (separate builds).

## Verification

- Golden tests: one mock-session jsonl stdout passes row-by-row checks. Durable rows match journal files byte for byte, live rows carry deltas only, `Result` rows match their own fixture (`docs/schema/fixtures/exec-result.jsonl`, `check-fixtures.sh` Result sections check). The fixture's `model_error` line is checked against the list `model_error_kinds!` declares (the script parses that declaration), and `docs/user/cli.md` names every kind value (the drift test reads the same list).
- Five outcomes each own one exit-code test (0/1/3/4, plus two existing 130 tests with 2 usage tests staying pinned): any outcome falling back to 1 fails.
- `-p -` reads stdin and starts after EOF. `-p` stdout matches pre-change bytes (jsonl rows leaking to default output fail).
- Terminal-Bench adapters run `--output jsonl`, and read `exit_code`/`stop_reason` from `Result` rows.
