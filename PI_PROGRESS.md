# compact-adrs round 3

## Done
- commit 1: removed ADR revision tables; AGENTS.md now says ADRs are snapshots

## Verified
- no revision-history references left in ADR bodies (only field names/`revision` labels)

## Done
- commit 2: ADR 0018 overflow fallback matches current single-stage code (Gain::Half, one retry)

## Verified
- `crates/agent-core/src/turn.rs`: overflow checked after `model_call_with_retries`; `ContextOverflow` is non-transient; one `overflow_retried`; retry not counted in `model_calls`
- `compaction_overflow.rs::a_fallback_that_cannot_reach_the_target_is_nothing_fits`

## Next
- commit 3: ADR 0031 project config narrowing applies without trust
