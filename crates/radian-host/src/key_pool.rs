//! The per-provider key pool (ADR 0026 D2): several keys for one provider, one request at a
//! time, a quota `429` resting the key that hit it.
//!
//! The pool is **process state only**. Nothing about keys enters the journal, `ConfigChanged`,
//! the export or the startup line; a resumed session starts at the first key again (ADR 0010:
//! live, not durable). The keys themselves are already in the redaction list (ADR 0013).
//!
//! The pick rule is stickiness, not rotation: a request uses the first key that is not resting,
//! and the session keeps using that key until it rests. The vendor's prompt cache for this
//! session therefore stays on one key. A `ModelError::Unavailable` with `status == Some(429)`
//! rests the key the request used (its `Retry-After`, capped at one hour, else five minutes);
//! no other status rests anything. With every key resting the request uses the key whose rest
//! ends soonest, and the adapter's own error reaches core as usual (ADR 0017).
//!
//! One notice line `provider: key "first" rests until 14:32; using "team"` is queued when a
//! request uses a different key than the previous request. The key value never appears; the
//! name or the masked form does. The hosts drain the queue after a turn.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use radian_core::ModelError;

use crate::auth::{self, NamedKey};

/// The rest a `429` without `Retry-After` puts on a key.
pub const DEFAULT_REST: Duration = Duration::from_secs(5 * 60);

/// The longest rest a `Retry-After` can ask for; a larger value is capped here.
pub const MAX_REST: Duration = Duration::from_secs(60 * 60);

/// One key the pool may use: the display name (`first`, a `keys[]` name, or the masked form)
/// and the resolved value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolKey {
    pub name: String,
    pub key: String,
}

impl PoolKey {
    /// The cache fingerprint: a hash of the key, never the key itself. A switch to another
    /// key changes the model cache key, so the second key gets its own `HostModel`.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest as _, Sha256};
        let digest = Sha256::digest(self.key.as_bytes());
        digest
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// The key one request uses, plus the one notice a switch produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickedKey {
    /// The index in the pool, for [`KeyPool::rest_for_error`].
    pub index: usize,
    pub name: String,
    pub key: String,
    pub fingerprint: String,
    /// The switch line, when this request's key differs from the previous one.
    pub notice: Option<String>,
}

struct State {
    /// `Some(until)` while the key rests.
    rest_until: Vec<Option<SystemTime>>,
    /// The key the previous request used.
    last: Option<usize>,
}

/// One provider's keys for the life of the process (ADR 0026 D2).
pub struct KeyPool {
    provider: String,
    keys: Vec<PoolKey>,
    state: Mutex<State>,
}

impl std::fmt::Debug for KeyPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyPool")
            .field("provider", &self.provider)
            .field("keys", &self.keys)
            .finish_non_exhaustive()
    }
}

impl KeyPool {
    pub fn new(provider: impl Into<String>, keys: Vec<PoolKey>) -> Self {
        let rest_until = vec![None; keys.len()];
        Self {
            provider: provider.into(),
            keys,
            state: Mutex::new(State {
                rest_until,
                last: None,
            }),
        }
    }

    /// Builds a pool from every resolved key of one entry, in file order.
    pub fn from_named(provider: impl Into<String>, keys: &[NamedKey]) -> Self {
        Self::new(
            provider,
            keys.iter()
                .map(|key| PoolKey {
                    name: key.display_name(),
                    key: key.key.clone(),
                })
                .collect(),
        )
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn keys(&self) -> &[PoolKey] {
        &self.keys
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// The key this request uses, and the notice when it differs from the previous request.
    /// `None` for an empty pool.
    pub fn pick(&self) -> Option<PickedKey> {
        self.pick_at(SystemTime::now())
    }

    fn pick_at(&self, now: SystemTime) -> Option<PickedKey> {
        if self.keys.is_empty() {
            return None;
        }
        let mut state = self.lock();
        // Stickiness: the session keeps the previous key while it is not resting.
        let chosen = match state.last {
            Some(last) if !is_resting(&state.rest_until, last, now) => last,
            _ => match (0..self.keys.len())
                .find(|&index| !is_resting(&state.rest_until, index, now))
            {
                Some(index) => index,
                // Every key rests: the one whose rest ends soonest, and the adapter's error
                // reaches core as usual.
                None => (0..self.keys.len())
                    .min_by_key(|&index| state.rest_until[index])
                    .expect("a non-empty pool has an index"),
            },
        };
        let previous = state.last;
        // Captured before `last` moves on, so the notice names the key that rested.
        let previous_rest = previous.and_then(|index| state.rest_until[index]);
        state.last = Some(chosen);
        let notice = match (previous, previous_rest) {
            (Some(previous), Some(until)) if previous != chosen => Some(switch_notice(
                &self.provider,
                &self.keys[previous].name,
                until,
                &self.keys[chosen].name,
            )),
            _ => None,
        };
        let key = &self.keys[chosen];
        Some(PickedKey {
            index: chosen,
            name: key.name.clone(),
            key: key.key.clone(),
            fingerprint: key.fingerprint(),
            notice,
        })
    }

    /// The key the pool currently speaks for: the previous request's, or the first key before
    /// any request. The usage fetch reads this at each fetch (ADR 0027 C4).
    pub fn current(&self) -> Option<PoolKey> {
        let index = self.lock().last.unwrap_or(0);
        self.keys.get(index).cloned()
    }

    /// Rests one key for `duration`, capped at [`MAX_REST`].
    pub fn rest(&self, index: usize, duration: Duration) {
        let duration = duration.min(MAX_REST);
        let mut state = self.lock();
        if let Some(slot) = state.rest_until.get_mut(index) {
            *slot = Some(SystemTime::now() + duration);
        }
    }

    /// Rests the key the request used only for a quota `429` (ADR 0026 D2): the adapter's
    /// `Retry-After` when it parsed one, else [`DEFAULT_REST`]. A `5xx` is the vendor's, and a
    /// `400`/`401`/`403` is the request's or a dead key, so neither rests anything.
    pub fn rest_for_error(&self, index: usize, error: &ModelError) {
        let ModelError::Unavailable {
            status: Some(429),
            retry_after,
            ..
        } = error
        else {
            return;
        };
        self.rest(index, retry_after.unwrap_or(DEFAULT_REST));
    }

    /// When the key rests until, for a test or a diagnostic. `None` when it is not resting.
    pub fn resting_until(&self, index: usize) -> Option<SystemTime> {
        self.lock().rest_until.get(index).copied().flatten()
    }
}

fn is_resting(rest_until: &[Option<SystemTime>], index: usize, now: SystemTime) -> bool {
    rest_until
        .get(index)
        .copied()
        .flatten()
        .is_some_and(|until| until > now)
}

/// `provider: key "first" rests until 14:32; using "team"` — the key value never appears.
fn switch_notice(provider: &str, previous: &str, until: SystemTime, next: &str) -> String {
    let local: chrono::DateTime<chrono::Local> = until.into();
    format!(
        "{provider}: key {previous:?} rests until {}; using {next:?}",
        local.format("%H:%M")
    )
}

/// Every process pool plus the notices waiting for a host to drain them.
#[derive(Default)]
struct Registry {
    pools: HashMap<String, Arc<KeyPool>>,
    notices: Vec<String>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

/// The process's pool for `provider`, built from `auth.json` on first use. `None` when the
/// entry has no usable key. A refused `keys[]` item's notice is queued here too.
pub fn pool(provider: &str) -> Option<Arc<KeyPool>> {
    if let Some(existing) = registry()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .pools
        .get(provider)
        .cloned()
    {
        return Some(existing);
    }
    let resolved = auth::resolve_keys(provider).ok().flatten()?;
    if resolved.keys.is_empty() {
        return None;
    }
    let pool = Arc::new(KeyPool::from_named(provider, &resolved.keys));
    let mut registry = registry()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if !resolved.notices.is_empty() {
        registry.notices.extend(resolved.notices);
    }
    Some(
        registry
            .pools
            .entry(provider.to_owned())
            .or_insert(pool)
            .clone(),
    )
}

/// Rebuilds the pool after `/login` or `/logout` changed `auth.json`. The new pool starts at
/// the first key; a `/logout` that leaves no key removes the pool.
pub fn reload(provider: &str) -> Option<Arc<KeyPool>> {
    registry()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .pools
        .remove(provider);
    pool(provider)
}

/// Queues one switch notice for the host to show after the turn.
pub(crate) fn push_notice(notice: impl Into<String>) {
    registry()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .notices
        .push(notice.into());
}

/// Takes the notices queued since the last call. The hosts drain this after a turn.
pub fn take_notices() -> Vec<String> {
    std::mem::take(
        &mut registry()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .notices,
    )
}

/// Drops every process pool and notice. Tests use it between cases; a running process never
/// needs it (the pool is process state).
pub fn reset() {
    *registry()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Registry::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(values: &[&str]) -> Vec<PoolKey> {
        values
            .iter()
            .enumerate()
            .map(|(index, key)| PoolKey {
                name: if index == 0 {
                    "first".to_owned()
                } else {
                    format!("key{index}")
                },
                key: (*key).to_owned(),
            })
            .collect()
    }

    fn quota(seconds: Option<u64>) -> ModelError {
        ModelError::Unavailable {
            message: "HTTP 429: quota".into(),
            retry_after: seconds.map(Duration::from_secs),
            status: Some(429),
        }
    }

    /// The pick rule: the first key, kept across requests; a rested key is skipped for the
    /// next one, and that switch queues exactly one notice naming both keys.
    #[test]
    fn the_pool_picks_the_first_key_and_keeps_it() {
        let pool = KeyPool::new("opencode-go", keys(&["sk-first", "sk-second"]));
        let first = pool.pick().expect("a key");
        assert_eq!(first.index, 0);
        assert_eq!(first.name, "first");
        assert_eq!(first.notice, None);
        let again = pool.pick().expect("a key");
        assert_eq!(again.index, 0, "no rotation between requests");
        assert_eq!(again.notice, None);

        // A 429 with Retry-After rests the key for that long.
        pool.rest_for_error(0, &quota(Some(120)));
        let until = pool.resting_until(0).expect("the key rests");
        let expected = SystemTime::now() + Duration::from_secs(120);
        let delta = until
            .duration_since(expected)
            .or_else(|_| expected.duration_since(until))
            .unwrap();
        assert!(delta < Duration::from_secs(5), "{delta:?}");

        let second = pool.pick().expect("a key");
        assert_eq!(second.index, 1);
        assert_eq!(second.name, "key1");
        let notice = second.notice.expect("one switch notice");
        assert!(notice.contains("opencode-go"), "{notice}");
        assert!(notice.contains("\"first\""), "{notice}");
        assert!(notice.contains("\"key1\""), "{notice}");
        assert!(notice.contains("rests until"), "{notice}");
        assert!(notice.contains("using"), "{notice}");
        assert!(!notice.contains("sk-first"), "{notice}");
        assert!(!notice.contains("sk-second"), "{notice}");
        // Stickiness on the new key too.
        assert_eq!(pool.pick().expect("a key").index, 1);
    }

    /// The rest rule: a 429 uses its Retry-After (capped at one hour), a 429 without one rests
    /// five minutes, and a 503 or a 401 rests nothing.
    #[test]
    fn only_a_quota_429_rests_its_key() {
        let pool = KeyPool::new("p", keys(&["a", "b"]));
        pool.rest_for_error(0, &quota(None));
        let until = pool.resting_until(0).expect("a rest");
        let delta = until.duration_since(SystemTime::now()).unwrap_or_default();
        assert!(
            delta > Duration::from_secs(4 * 60) && delta <= DEFAULT_REST,
            "{delta:?}"
        );

        let capped = KeyPool::new("p", keys(&["a"]));
        capped.rest_for_error(0, &quota(Some(2 * 60 * 60)));
        let delta = capped
            .resting_until(0)
            .expect("a rest")
            .duration_since(SystemTime::now())
            .unwrap_or_default();
        assert!(delta <= MAX_REST, "{delta:?}");

        for error in [
            ModelError::Unavailable {
                message: "HTTP 503: busy".into(),
                retry_after: Some(Duration::from_secs(30)),
                status: Some(503),
            },
            ModelError::Protocol("HTTP 401: bad key".into()),
            ModelError::Transport("request failed".into()),
        ] {
            let pool = KeyPool::new("p", keys(&["a"]));
            pool.rest_for_error(0, &error);
            assert_eq!(pool.resting_until(0), None, "{error} must not rest the key");
        }
    }

    /// With every key resting the soonest-free key is used, and the error reaches core.
    #[test]
    fn every_key_resting_uses_the_soonest_free() {
        let pool = KeyPool::new("p", keys(&["a", "b"]));
        pool.rest_for_error(0, &quota(Some(600)));
        pool.rest_for_error(1, &quota(Some(60)));
        assert_eq!(pool.pick().expect("a key").index, 1);
    }

    /// The first key's name is the masked value when a `keys[]` item has no name.
    #[test]
    fn an_unnamed_key_is_shown_masked() {
        let pool = KeyPool::from_named(
            "p",
            &[
                NamedKey {
                    name: Some("first".to_owned()),
                    key: "sk-first".to_owned(),
                    item: None,
                },
                NamedKey {
                    name: None,
                    key: "sk-second".to_owned(),
                    item: Some(0),
                },
            ],
        );
        assert_eq!(pool.keys()[1].name, crate::auth::mask_key("sk-second"));
    }
}
