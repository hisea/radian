# ADR 0004: Hot Configuration Slots (`ConfigSlot` / `ConfigQueue`)

Status: accepted

## Background

`/model`, `/access`, `/thinking`, and `auto` change later behavior while a session runs. They must not change an in-flight model stream or a tool loop that already started in the turn. Core stays generic (`Agent<M, P, H>`) and uses no global trait object. The runtime configuration slots carry the change, and the change applies at the **turn boundary**.

## Decisions

### D1: Core carries no run marker; each session owns one slot

Each session owns its `Agent` and its own `ConfigSlot`. The host already knows the turn boundary (the driver itself calls `start_turn`/`step`), so the driver loop structure guarantees "effect from the next turn". The guarantee needs no Core field and no task-local value.

### D2: `ConfigSlot` and `ConfigQueue`

```rust
pub struct ConfigSlot { value: RwLock<Value>, revision: AtomicU64 }
pub struct ConfigQueue { pending: Mutex<Vec<Value>> }
```

- `ConfigSlot::new(initial)` returns `Arc<Self>`. `value()` returns the full configuration currently in effect. `revision()` returns the revision number of the current value (0 = initial value). Only the driver writes the slot, at the turn boundary.
- `ConfigQueue::push(change)` only enqueues the change. It takes **no immediate effect**. `len`/`is_empty` report the queue state. The driver drains the queue.
- A change value is a JSON object. Application **merges** it into the object in the slot, so `{"thinking": …}` never drops the model route, and vice versa. A non-object value replaces the whole value.

### D3: Apply at the boundary, and record the application

The driver calls `apply_pending_config()` before each `start_turn`. It drains the queue item by item and assigns monotonically increasing `revision` numbers (from 1). It **first** writes `RuntimeEvent::ConfigChanged { revision, change }` and the write must succeed, **then** it applies the change to the slot. After a successful application it passes the event to the runtime event listener. Application is the effect; enqueue is only the intent. The recorded `revision` therefore always names a configuration truly in effect. Each session applies at its own boundary.

A model change (`{"model": {"provider", "model"}}`) also makes the host recompute the next turn context budget and compaction settings (ADR 0026 D7). `ConfigChanged` holds only host labels (route/thinking/approval and similar). It holds **no secret or header value**.

### D4: In-flight work is not affected

- An **in-flight model request** already holds the old configuration. Later enqueues and later applications do not affect it.
- An in-flight **policy decision** is equally unaffected.
- Later model calls inside the same turn also do not see changes enqueued mid-turn. They take effect only at the next turn boundary.

### D5: Resume replays

On resume, the driver replays the committed `ConfigChanged` events (the recovery path in `driver.rs`). It restores the last effective state. A model, tier, or thinking level given explicitly on the command line overrides the replay, and the driver writes one more new `ConfigChanged` before the first turn.

### D6: Live state does not enter the slot

Two state classes exist:

| Dimension | Boundary-effective (slot) | Live state (the hook reads it directly) |
|---|---|---|
| Examples | model route, tier, thinking, auto | "always allow" granted mid-turn by the user, override switch |
| Effective point | Next `start_turn` | Next `before_tool_call`/`before_model_call` |
| Semantic guarantee | In-flight requests and the current tool loop stay unaffected | Reflected at once in the next hook decision |

Hook liveness guarantees "valid for this call" for approvals. Caching an approval in the slot and expecting "still valid next turn" violates ADR 0012 and the ADR 0006 rule "approvals do not replay across resume".

## Costs

- A global configuration change fans out to each session driver (one push per driver). The pushes do not wait for each other and do not overwrite each other.
- Two changes enqueued at the same boundary keep only the last one (latest wins). The earlier one leaves no independent record. A host that needs a complete intent audit records intents itself before enqueue.
- A change enqueued while the driver never runs another turn never takes effect and leaves no record. The host must decide at driver shutdown how to handle the queue.
- A slot swap does not affect an in-flight model request (the existing D4 semantics).

## Verification

Unit tests in `crates/radian-runtime/src/config.rs` cover slot values, revision numbers, merging, and draining. Driver tests cover "a change enqueued mid-turn is invisible in the current turn and visible in the next turn", budget recomputation on model change, and resume replay.
