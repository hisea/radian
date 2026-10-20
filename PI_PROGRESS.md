# compact-adrs round 3

## Done
- commit 1: removed ADR revision tables; AGENTS.md now says ADRs are snapshots
- commit 2: ADR 0018 overflow fallback matches current single-stage code (Gain::Half, one retry)
- commit 3: ADR 0031 project config narrowing applies without trust; propagated to 0008/0020/0038/0043

## Verified
- `crates/radian-host/src/project.rs`: only `[permissions] profile` is read; `full` refused; other keys warned/ignored; no content is trust-gated
- `crates/radian-host/src/trust.rs`: config.toml is a trust trigger; project skills are the gated resource

## Next
- run acceptance checks, delete progress note
