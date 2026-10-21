# ADR 0022: Session Fork

Status: accepted

## Decisions

1. **Fork only at turn boundaries.** An allowed fork point is a position in the parent session with **no turn in progress**: after some `TurnFinished` in the parent log (or at session start, with no turns yet). Never fork from mid-turn, from mid-tool-call, or from an uncommitted replacement segment. The host forks the current tail only when the session is idle. Fork from a history position uses only these boundaries.
2. **Materialize the effective view.** The child initial transcript is the parent effective history at the fork point, replayed per ADR 0019: committed replacements apply, uncommitted segments drop, and `repair` completes unpaired calls (ADR 0002). The child writes this history as its own `MessageAppended` records (index from 0) into its own file. It never rebuilds from the parent file. It never copies raw parent events, tool markers, or usage. Parent raw facts stay unchanged (only one `SessionForked` appends).
3. **Parent event plus child parent, both recorded.**
   - Child: `SessionCreated.parent = { session_id: <parent>, seq: <fork point seq in parent log>, relation: "fork" }`.
   - Parent: `SessionForked { child_session_id, parent_seq }`. `parent_seq` equals the child `parent.seq`.
4. **Child lands first.** Order: create and fsync the child file (`SessionCreated` plus all materialized `MessageAppended` records), **then** append `SessionForked` to the parent log. Crash outcomes: child written but parent unrecorded means the child session is complete and usable, and the parent log misses one lineage record (lineage authority is the child `parent`; the parent `SessionForked` is an audit index). Half-written child means the child file exists as a torn tail or incomplete prefix, and reads follow normal session rules (the host reports "fork incomplete", and the user can delete it). The parent never records a child that does not exist.
5. A child is an ordinary session: it has its own session id, single-writer lock, resume, and re-fork. It shares the workspace with the parent (fork creates no worktree).
6. **Model and permission change at fork**: the host decides child startup parameters as usual. This ADR sets no rule. A `subagent` child uses the same `parent` and `relation` shape (`relation = "subagent"`), but it is an ADR 0043 one-shot blocking child session, not a fork.

## Out of Scope

- Rewind or `Rewound` views in the same file: fork already covers "branch from a past boundary", so this ADR defines no rewind.
- Cross-workspace fork and fork into an isolated worktree.

## Verification

`crates/radian-session/tests/fork.rs` covers the materialized view equal to the parent `recover` result at that point (including committed compaction), rejection of mid-turn, uncommitted, and out-of-range points, child re-fork, parent with only one extra `SessionForked` and matching seq, complete child when the parent record fails, and unchanged parent when the child write halves. `radian-exec` covers fork and exit 1 when the parent is locked. `radian-tui` pty tests cover idle fork and running rejection. Fixtures `fork-parent.jsonl` and `fork-child.jsonl` pass the reference checker.
