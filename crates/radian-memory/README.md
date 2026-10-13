# radian-memory

The ADR 0021 M1 memory storage layer. It is **host-free**: the caller passes the root
directory (the host passes `~/.radian/memory/`); the crate never reads `HOME`, and Core does
not know it exists.

## Layout on disk

```
<root>/projects.jsonl                    the project registry (identity key → ProjectId)
<root>/global/log.jsonl                  the Global scope log
<root>/projects/<ProjectId>/log.jsonl    a Project scope log
```

The directory is `0700` and the files `0600`. Each log is append-only JSONL, the only source
of truth. The mechanics mirror `radian-session` (ADR 0006 §3.2/§7) without depending on it:
one writer at a time (`flock`, blocking so writers serialize), `write`+`flush`+`fsync` per
append, a torn tail (bytes after the last LF) is ignored and truncated before the next
append, a malformed line refuses the log with its 1-based line number, and an unknown
`critical` event refuses the log while an unknown non-critical one is skipped — but the events
and the replay are the memory's own (ADR 0021 D3).

## Events and an envelope sample

`MemoryCreated`, `MemoryRevised`, `MemoryArchived`, `MemoryRestored`, `MemoryConfirmed`,
`MemoryForgotten` (all `critical: true`) and `StorePurged` (`critical: false`, audit only).

```json
{"schema_version":1,"seq":0,"ts":"2026-10-13T12:00:00.000000Z","event":"MemoryCreated","critical":true,"data":{"record":{"id":"mem-…","scope":"global","kind":"preference","key":"style","content":"tabs","source":"user","verification":{"confirmed":{"by":"user","at":"2026-10-13T12:00:00.000000Z"}},"status":"active","created_at":"…","updated_at":"…","revision":1}}}
```

Replay: `MemoryCreated` inserts, `Revised`/`Archived`/`Restored`/`Confirmed` update and bump
the revision, `MemoryForgotten` removes (and is never resurrected by a rebuild), `StorePurged`
is a no-op after a rewrite.

## Public API

- `MemoryStore::{create, create_with_id, revise, archive, restore, confirm, forget, purge,
  purge_with_fault, export, import, records, view, split, merge, log_path}` — CRUD with
  revision checks, same-scope Active-key uniqueness, the source rules (an `Agent` writer may
  not revise/archive/forget a `User` record), and the cross-scope view (a Project Active record
  overrides a Global one with the same key).
- `ProjectRegistry::{register, resolve, relink, projects, identity_keys}` — `register` is
  stable per identity key; `relink` attaches a key to an existing project.
- `search(records, &SearchQuery)` — the deterministic order (scope → trust → match score →
  `updated_at` → `id`).
- `export(records)` / `parse(text)` — the Markdown view (flat front matter, no YAML
  dependency).

The API is synchronous: an operation takes the writer lock, replays, mutates, fsyncs and
releases, so a second process serializes instead of seeing a lock error. `purge_with_fault`
takes a `PurgeStep` to stop after one step for the crash-atomicity test; a real crash leaves
the same state (the path holds the old log or the new one, never a partial file).

## Not here

The host wiring (D5–D8): tool registration, permission rules, configuration, injection, the
session `MemoryInjected` event, and sandbox read denial. This slice is storage only.
