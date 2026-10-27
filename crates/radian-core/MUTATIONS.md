# Mutation checks — radian-core

New rows use the id `<task id>-M<n>` (for example `TK0034-M1`); see "Mutation records" in `AGENTS.md`.

## Rate-limit headers on `ModelResponseReceived` (ADR 0027 slice ④, 2026-10-17)

Base `c09b614` · branch `dogfood/footer-s4`. Each mutation was applied in place, only the catching
binary was run (`--no-fail-fast`), and the source was restored from a `cp` in `/tmp` and confirmed
with `cmp`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| LIM-A | the turn drops the headers (`limits: response.limits.clone()` → `limits: None` in the `ModelResponseReceived` emit) | ✅ 1 failed (rc=101) | `tests/usage.rs::limits_land_in_the_model_response_received_event` (the first call's `limits_of(0)` is `None`) |

Command: `timeout 300 cargo test -p radian-core --locked --no-fail-fast --test usage limits_land`.
The no-headers half of the same test (`limits_of(1)` is `None`) passes under the mutation: absence
is the default, so only the carrying half can catch a drop.

## Exec `Result.model_error` and the `UsageLimit` variant (TK0184, 2026-10-26)

Branch `work/TK0184-exec-result-model-error`. Each mutation was applied in place, only the
catching test binary was run, and the source was restored from a `cp` under
`.radian/mut-backup`; the restored tree then passed the full crate run again.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0184-M1 | `UsageLimit` made transient (`is_transient` lists it in the `true` arm) | ✅ 1 failed | `tests/retry.rs::a_usage_limit_error_is_never_retried` (the turn sent four requests instead of one and announced a retry) |

## Rework 2: the kind list is declared once (TK0184, 2026-10-27)

Branch `work/TK0184-exec-result-model-error`. `model_error_kinds!` generates `ModelErrorKind`,
`as_str()` and `MODEL_ERROR_KINDS` together. The mutation renamed one declared wire name in that
one declaration; the source was restored from `.radian/mut-backup/model.rs` (`cmp` identical).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0184-M5 | rename one declared kind (`UsageLimit => "usageLimit"`) | ✅ 2 failed | `model::tests::every_model_error_kind_is_in_the_closed_list` (the variant's pinned name differs, and `usageLimit` is not snake_case) and `bash docs/schema/check-fixtures.sh` (`exec-result.jsonl` line 3: `model_error.kind 'usage_limit'` is not in the closed set parsed from the same declaration) |
