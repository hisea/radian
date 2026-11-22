# `agent-provider-common` mutation record

New rows use the id `<task id>-M<n>`; see "Mutation records" in `AGENTS.md`. The mutation was
applied to the working tree, the crate suite was run, and the source was restored by a reverse
edit.

| # | Mutation | Result | Test(s) that caught it |
|---|---|---|---|
| T-0017-M1 | `parse_retry_after` no longer trims the header value (`value.trim()` → `value`, `retry_after.rs`) | caught (FAILED) | `retry_after::tests::retry_after_seconds_are_taken_as_is` |
