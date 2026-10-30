# `benchmarks/terminal-bench` mutation record

New rows use the id `<task id>-M<n>` (for example `TK0252-M1`); see "Mutation records" in `AGENTS.md`.

Base: `work/TK0252-journal-metrics-extractor` · method: apply the mutation to
`benchmarks/journal_metrics.py`, run the named test with
`python3 -m unittest test_metrics.<Class>.<test>`, expect a non-zero exit,
restore the file from a backup under `.radian/`.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| TK0252-M1 | `_radian_inputs`: a record marked `input_includes_cache: true` keeps all of `input` as uncached instead of subtracting `cache_read` | `JournalMetricsTest.test_a_radian_journal_yields_every_column` (`input_uncached` 111,000 instead of 110,800) | ✅ failed |
| TK0252-M2 | latency anchor moved to the request boundary: `_usage` measures from `_anchor` and an assistant message resets it before its usage record | `JournalMetricsTest.test_latency_uses_a_bucket_per_100k_prompt_tokens` (bucket 0 p50 0 ms instead of 2,000 ms) | ✅ failed |
| TK0252-M3 | pi's `bash` keeps its own column (`PI_TOOL_NAMES` not applied) | `JournalMetricsTest.test_a_pi_session_uses_the_same_columns_and_bash_is_shell` (`{'bash': 1}` instead of `{'shell': 1}`) | ✅ failed |
