# ADR 0047: Continuation Engine: `/loop` and `/goal`

Status: accepted

## Background

`/loop` resubmits one prompt unchanged each turn. Its turn-end decision point, resubmit path, and cap semantics live in ADR 0011 L7. This ADR only cites L7. `/goal` is the goal half of the continuation engine: Codex with omp `/goal` all mean "goals persist, turn ends auto-continue, done stops". radian dogfood often showed "read-only runs" with resume waiting for humans to say "continue". Goal mode reuses `/loop` turn-end decision points, changing only what to post and when to stop. Done judgments take tools, never text matches.

## Decisions

### 1. `/loop` stays under ADR 0011 L7

Loop durable inbox semantics stay at 0011 L7. They are `User` source, `origin` audit, journaled `TurnFinished` decision points, `--every` rulings, no wake-gate counting, default 20 turns or 1 hour, Esc/model-error pauses, in-process state. This ADR rules only the goal half. Both read `TurnFinished` at one spot.

### 2. `/goal` commands

`/goal <objective>` sets or replaces (4 000 char cap, literal text, no `@` expansion). Bare `/goal` shows state, progress, and budget use. `/goal pause`, `/goal resume`, `/goal drop` control. `/goal budget <N turns|turn> [<dur>] [<N tokens|token>]` sets caps (many groups take effect at once, bare `budget` shows). No `/guided-goal` exists. `radian exec -p --goal` stays undecided, unbuilt.

`/goal` with `/loop` exclude each other: one running refuses the other start, naming which side stops first (hints spell commands). Ended goals (done, budget-exhausted) or no goals never block `/loop`.

### 3. Persistence: `Goal*` events with fold

Goals, states (active/paused/done/budget-exhausted), latest progress, with budgets are durable journal records (ADR 0006): `GoalSet{objective,max_turns,max_duration_secs,max_tokens}`, `GoalBudgetSet{…same triple…}`, `GoalProgress{note}`, `GoalDone{note?}`, `GoalPaused{}`, `GoalResumed{}`, `GoalDropped{}`, `GoalWait{secs}`, `GoalBudgetExhausted{kind}` (kind reads `turns`, `duration`, `tokens`). All own `critical = false`: audit only, never rebuild transcripts. Readers skip them.

`goal::fold` rebuilds states from events (a second fold besides transcript fold): `Set` creates (same-name replaces reset wholly). `BudgetSet` swaps triples. `Progress` replaces latest progress. `Done`/`Exhausted` close segments. `Paused` closes segments only when active. `Resumed` opens segments only when paused. `Drop` forgets. Budget counts stay durable too. Turns count only completed turns with `{"continuation":"goal"}` origin inputs. Attribution runs `InboxEnqueued` → `InputClaimed`. Failures with cancels never count. Caliber matches loop iterations. Durations add open-segment `ts` wall clocks. Tokens add open-segment `ModelUsageRecorded` (`total` wins when present, else prompt (ADR 0007 `input_includes_cache` caliber) plus output. Summary calls never count).

Default budgets are 30 turns with 2 hours, tokens default unbounded. `/goal budget` extra funds on exhausted re-activate. Extra funds on done refuse (set new ones). No goals refuse.

### 4. What turn ends post

Active goals with no model-reported `done` this turn make the host post fixed-shape continuation messages. Messages hold "keep pushing the goal; call goal done when done" plus goal plus latest progress. `source = User` (only `User` turns take session approval with memory, ADR 0011 L2). `origin = {"continuation":"goal","iteration":n}` (ADR 0005 G1 audit). Path matches `/loop` (`Effect::SendFollowup` → `Inbox::followup(_, User)`). Same echo/outstanding/inflight dedup applies. Same Esc with model-error semantics applies. Cancel/failure/no-usable-end-state all pause, leaving resume paths. Non-goal-turn completions continue goals (never counting budgets). Non-goal-turn cancels/failures pause goals (never blind-posts to just-cancelled or just-broken providers).

Commands journal before taking effect (failures change nothing). Turn-end stops take effect at once, with journals recording audit (decisions never wait one journal round trip).

### 5. `goal` tools

`goal { op: "progress" | "done" | "wait", note?, secs? }` (same style as `job {id,op}`). One line each applies. Schema with description never repeats facts. `progress` needs non-empty `note`, replacing latest progress. `done` sets done (this turn continues, never continues turns, turn ends show TUI notices). `wait` needs positive-integer `secs` (clamped to 1 hour), delaying next continuations. Non-active goals (no goal, paused, done, exhausted) all answer `no_active_goal` tool-result errors (paused/done hints each name their way out). Tools journal before changing shared state. Tools mount in every session (workspaceless with child sessions included; goalless answers only err).

Rights place `goal` with `ask_user`/`skill`: they only advance session-goal state with journals, never write workspace files and never run commands. Both tiers `allow`, never prompting.

### 6. When to stop, with topped visibility

`done`. Topped budgets pick one of three (judged in turns, duration, tokens order, first arrival stops first). `pause`/`drop`. Model errors. Topped budgets show in ADR 0037 decision 5 shapes. TUI notices name topped budgets with two ways out. herdr reports `blocked` (new `goal budget` category, holds when idle, vanishes at new-turn starts). Journals record `GoalBudgetExhausted`.

### 7. `--resume` with `/fork`

`--resume` folds active goals as paused (never auto-continues), and startup blocks hint `/goal resume`. `/fork` children carry goal records but paused (goals, budgets, latest progress; counters restart. Child sessions are new sessions with own budgets). Done/exhausted/dropped goals never carry over.

### 8. Prompt blocks, compaction, with ADR 0036 exception

Goal blocks are one fixed `<goal>` block in context zones (ADR 0036 layer 3, after skills, before `<session>`): goals only. Blocks recompute per request, appear only with goals, vanish with no goals. Unchanged goals keep bytes. Blocks live only in system-prompt layers, never transcript messages. Compaction consumes transcripts only (ADR 0018). Compaction neither summarizes nor drops them. Continuation messages enter verbatim as usual as plain `User` messages, keeping source words, never summaries, still injecting per request.

Exception to ADR 0036: layer 3 "computes once in process, bytes freeze" rules yield to `<goal>` blocks. Yielding means presence follows goals, never content following progress: unchanged goals keep bytes, system prefix caches stay untouched. Latest progress (with model-visible budget use, when present) appears only in turn-end continuation messages (§4, at message tails, changes never touch caches). It regenerates each turn from durable records. Compaction never drops it. No goals mean hooks write nothing, chains keep bytes fixed, and this exception never takes effect.

### 9. `wait` delay caliber

`goal wait secs` delays next continuations by `secs` (relative to last iteration start, clamps at 1 hour). `/goal` owns no `--every` flag, and backlog "takes larger of `--every` with `secs`" owns no attach point: `secs` rules. Many waits merge (last wins), running turns never interrupt.

## Verification

TUI (`goal_mode`): set goals with no reported done continue with goals and latest progress in continuation messages (asserting text itself). `done` never continues and notifies exactly once. `wait 600` delays 600 seconds under injected clocks. `budget 2 turns` stops after exactly two turns with herdr pure functions reading `blocked`. Failure/cancel pauses, `resume` re-continues, `/loop` mutual exclusion. Host: `goal` three-operation journal-first with `no_active_goal`, `GoalHooks` blocks with silent goalless hooks, hook-chain order (memory, skills, goal, facts). Session: fold state/count triples, resume parking, fork carrying paused, compaction records never disturbing goals. Schema: `goal.jsonl` passes checkers. One mutation each (off-by-one budgets, unarmed waits, lost progress, renamed done, counted failure turns, released tool approvals, renamed goalless blocks, reversed forks, misplaced parses, mislabeled herdr, removed exclusion, self-continuing resume, dropped rights) caught by named tests each.

## Constraints and Costs

- Goal blocks recompute per request, but unchanged goals keep bytes, never touching prefix caches. Continuation-message new content sits at message tails (§4).
- Fork child sessions restart budgets (§7): forks are user actions, and budget workarounds are user choices.
- Tokens with durations are session caliber (open segments before pauses included, all requests during goal-active periods. Cancel-unjournaled responses count by journal after resume, live counts slightly more).
- Goal text inside 4 000 char caps enters journals each turn with continuation messages. Long goals on long runs mean long journals. Accepted as durable costs.
