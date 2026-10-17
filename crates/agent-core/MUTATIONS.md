# Mutation checks — agent-core

## Rate-limit headers on `ModelResponseReceived` (ADR 0027 slice ④, 2026-10-17)

Base `c09b614` · branch `dogfood/footer-s4`. Each mutation was applied in place, only the catching
binary was run (`--no-fail-fast`), and the source was restored from a `cp` in `/tmp` and confirmed
with `cmp`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| LIM-A | the turn drops the headers (`limits: response.limits.clone()` → `limits: None` in the `ModelResponseReceived` emit) | ✅ 1 failed (rc=101) | `tests/usage.rs::limits_land_in_the_model_response_received_event` (the first call's `limits_of(0)` is `None`) |

Command: `timeout 300 cargo test -p agent-core --locked --no-fail-fast --test usage limits_land`.
The no-headers half of the same test (`limits_of(1)` is `None`) passes under the mutation: absence
is the default, so only the carrying half can catch a drop.
