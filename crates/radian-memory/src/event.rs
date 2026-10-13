//! The memory event schema and its replay (ADR 0021 D3/D4). The events are the only source of
//! truth; the current state is `replay`'s result.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Confirmer, MemoryId, MemoryRecord, Status, Verification};

/// One durable memory event. The disk name is the exact string in [`MemoryEvent::NAMES`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryEvent {
    Created(MemoryRecord),
    Revised {
        id: MemoryId,
        revision: u64,
        content: String,
        at: String,
    },
    Archived {
        id: MemoryId,
        revision: u64,
        at: String,
    },
    Restored {
        id: MemoryId,
        revision: u64,
        at: String,
    },
    Confirmed {
        id: MemoryId,
        revision: u64,
        by: Confirmer,
        at: String,
    },
    Forgotten {
        id: MemoryId,
        at: String,
    },
    Purged {
        removed: Vec<MemoryId>,
        at: String,
    },
}

impl MemoryEvent {
    /// Every event name a reader knows. Anything else is handled by the `critical` policy.
    pub const NAMES: &'static [&'static str] = &[
        "MemoryCreated",
        "MemoryRevised",
        "MemoryArchived",
        "MemoryRestored",
        "MemoryConfirmed",
        "MemoryForgotten",
        "StorePurged",
    ];

    /// The v1 disk event name.
    pub fn name(&self) -> &'static str {
        match self {
            MemoryEvent::Created(_) => "MemoryCreated",
            MemoryEvent::Revised { .. } => "MemoryRevised",
            MemoryEvent::Archived { .. } => "MemoryArchived",
            MemoryEvent::Restored { .. } => "MemoryRestored",
            MemoryEvent::Confirmed { .. } => "MemoryConfirmed",
            MemoryEvent::Forgotten { .. } => "MemoryForgotten",
            MemoryEvent::Purged { .. } => "StorePurged",
        }
    }

    /// Whether an older reader that does not know this event must refuse the whole log.
    /// `StorePurged` is audit only, so skipping it loses nothing.
    pub fn critical(&self) -> bool {
        !matches!(self, MemoryEvent::Purged { .. })
    }

    /// The `data` object for the envelope.
    pub fn to_data(&self) -> Value {
        match self {
            MemoryEvent::Created(record) => serde_json::json!({ "record": record }),
            MemoryEvent::Revised {
                id,
                revision,
                content,
                at,
            } => {
                serde_json::json!({ "id": id, "revision": revision, "content": content, "at": at })
            }
            MemoryEvent::Archived { id, revision, at } => {
                serde_json::json!({ "id": id, "revision": revision, "at": at })
            }
            MemoryEvent::Restored { id, revision, at } => {
                serde_json::json!({ "id": id, "revision": revision, "at": at })
            }
            MemoryEvent::Confirmed {
                id,
                revision,
                by,
                at,
            } => serde_json::json!({ "id": id, "revision": revision, "by": by, "at": at }),
            MemoryEvent::Forgotten { id, at } => serde_json::json!({ "id": id, "at": at }),
            MemoryEvent::Purged { removed, at } => {
                serde_json::json!({ "removed": removed, "at": at })
            }
        }
    }

    /// Parses one known event's `data`. Unknown fields are ignored (serde's default), so a
    /// newer writer's extra field does not break an older reader.
    pub fn from_data(name: &str, data: &Value) -> Result<Self, serde_json::Error> {
        let parsed = match name {
            "MemoryCreated" => MemoryEvent::Created(from_value::<CreatedData>(data)?.record),
            "MemoryRevised" => {
                let d = from_value::<RevisedData>(data)?;
                MemoryEvent::Revised {
                    id: d.id,
                    revision: d.revision,
                    content: d.content,
                    at: d.at,
                }
            }
            "MemoryArchived" => {
                let d = from_value::<StatusData>(data)?;
                MemoryEvent::Archived {
                    id: d.id,
                    revision: d.revision,
                    at: d.at,
                }
            }
            "MemoryRestored" => {
                let d = from_value::<StatusData>(data)?;
                MemoryEvent::Restored {
                    id: d.id,
                    revision: d.revision,
                    at: d.at,
                }
            }
            "MemoryConfirmed" => {
                let d = from_value::<ConfirmedData>(data)?;
                MemoryEvent::Confirmed {
                    id: d.id,
                    revision: d.revision,
                    by: d.by,
                    at: d.at,
                }
            }
            "MemoryForgotten" => {
                let d = from_value::<ForgottenData>(data)?;
                MemoryEvent::Forgotten { id: d.id, at: d.at }
            }
            "StorePurged" => {
                let d = from_value::<PurgedData>(data)?;
                MemoryEvent::Purged {
                    removed: d.removed,
                    at: d.at,
                }
            }
            other => {
                return Err(serde::de::Error::custom(format!(
                    "unknown event {other:?} reached from_data"
                )));
            }
        };
        Ok(parsed)
    }

    /// Applies the event to the replayed state. Returns a message when the log contradicts
    /// itself (for example a `Revised` for a record that is not present); the caller turns
    /// that into a `Corrupt` error naming the line.
    pub fn apply(
        &self,
        state: &mut BTreeMap<MemoryId, MemoryRecord>,
        forgotten: &mut BTreeSet<MemoryId>,
    ) -> Result<(), String> {
        match self {
            MemoryEvent::Created(record) => {
                if state.insert(record.id.clone(), record.clone()).is_some() {
                    return Err(format!("record {} was created twice", record.id));
                }
                forgotten.remove(&record.id);
            }
            MemoryEvent::Revised {
                id,
                revision,
                content,
                at,
            } => {
                let record = state
                    .get_mut(id)
                    .ok_or_else(|| format!("record {id} was revised before it existed"))?;
                record.content = content.clone();
                record.revision = *revision;
                record.updated_at = at.clone();
            }
            MemoryEvent::Archived { id, revision, at } => {
                let record = state
                    .get_mut(id)
                    .ok_or_else(|| format!("record {id} was archived before it existed"))?;
                record.status = Status::Archived;
                record.revision = *revision;
                record.updated_at = at.clone();
            }
            MemoryEvent::Restored { id, revision, at } => {
                let record = state
                    .get_mut(id)
                    .ok_or_else(|| format!("record {id} was restored before it existed"))?;
                record.status = Status::Active;
                record.revision = *revision;
                record.updated_at = at.clone();
            }
            MemoryEvent::Confirmed {
                id,
                revision,
                by,
                at,
            } => {
                let record = state
                    .get_mut(id)
                    .ok_or_else(|| format!("record {id} was confirmed before it existed"))?;
                // Confirming never changes `source` (ADR 0021 D4); it only records who and
                // when.
                record.verification = Verification::Confirmed {
                    by: *by,
                    at: at.clone(),
                };
                record.revision = *revision;
                record.updated_at = at.clone();
            }
            MemoryEvent::Forgotten { id, .. } => {
                state.remove(id);
                forgotten.insert(id.clone());
            }
            // Audit only: the rewritten log's live records are the state.
            MemoryEvent::Purged { removed, .. } => {
                for id in removed {
                    forgotten.insert(id.clone());
                }
            }
        }
        Ok(())
    }
}

fn from_value<T: for<'de> Deserialize<'de>>(data: &Value) -> Result<T, serde_json::Error> {
    serde_json::from_value(data.clone())
}

#[derive(Deserialize, Serialize)]
struct CreatedData {
    record: MemoryRecord,
}

#[derive(Deserialize, Serialize)]
struct RevisedData {
    id: MemoryId,
    revision: u64,
    content: String,
    at: String,
}

#[derive(Deserialize, Serialize)]
struct StatusData {
    id: MemoryId,
    revision: u64,
    at: String,
}

#[derive(Deserialize, Serialize)]
struct ConfirmedData {
    id: MemoryId,
    revision: u64,
    by: Confirmer,
    at: String,
}

#[derive(Deserialize, Serialize)]
struct ForgottenData {
    id: MemoryId,
    at: String,
}

#[derive(Deserialize, Serialize)]
struct PurgedData {
    #[serde(default)]
    removed: Vec<MemoryId>,
    at: String,
}
