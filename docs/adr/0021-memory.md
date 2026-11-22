# ADR 0021: Memory

Status: removed

Removed: persistent memory is removed from radian, code and docs. Project conventions live in `AGENTS.md` (ADR 0036) and cross-project procedures in skills (ADR 0025). No part of the earlier design is in force. It is kept only as history. Do not build on it.

What stays true:

- Radian writes no memory, injects no memory index, and mounts no memory tool. The system prompt has no memory section and `/context` has no memory category.
- `radian memory` and `/memory` do not exist. The `radian-memory` crate does not exist.
- `~/.radian/memory/` is never read, written, created or deleted by radian. A user may delete it by hand.
- Old journals stay readable: a `MemoryInjected` record and a message with `origin.kind = "memory_addition"` open, replay and export unchanged (ADR 0006 decision 5.12). This version never writes either.
- The `[memory]` configuration table is a retired key: one warning line, then ignored; it never refuses the file (ADR 0008).
