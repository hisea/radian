//! The `/model` and `/memory` completion candidates: the provider/model list built from the
//! descriptors, the config and `auth.json`, its sort order, and the memory record ids with
//! their one-line snippets. Local reads only; the background refresh owns the cache.

use super::*;
/// The record ids a `/memory` command can act on, as completion candidates. A read failure is
/// an empty list, never a turn failure (ADR 0021 invariant 2).
/// The `/model` completion candidates (ADR 0026 D5): the logged-in providers before the
/// slash, or one provider's models after it — the descriptor's built-in list plus the cached
/// `/v1/models` list. Reads local files only; the background refresh (D0) writes that cache.
pub(super) fn model_route_candidates(
    current_provider: Option<&str>,
    config_providers: &std::collections::BTreeMap<String, radian_host::ProviderDecl>,
) -> Vec<radian_tui::command::Candidate> {
    // Only providers a switch could accept (ADR 0026 D5/D6): one with a usable `auth.json`
    // entry — a built-in descriptor, a config `[providers.<name>]` declaration, or a custom
    // name stored by `/login custom` (D10) — plus the current route's provider, whose `✓` row
    // is never missing even without a credential. Each is resolved with `resolve_provider` so
    // the list matches what a switch would accept, then its models are flattened into one
    // list (ADR 0035): the label is the model, the description the provider.
    let auth_entries = radian_host::auth::list().unwrap_or_default();
    let logged_in = |name: &str| {
        auth_entries
            .iter()
            .any(|entry| entry.provider_id == name && entry.usable)
    };
    let mut names: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for descriptor in radian_host::descriptors() {
        if logged_in(descriptor.id) && seen.insert(descriptor.id.to_owned()) {
            names.push(descriptor.id.to_owned());
        }
    }
    for name in config_providers.keys() {
        if logged_in(name) && seen.insert(name.clone()) {
            names.push(name.clone());
        }
    }
    for entry in &auth_entries {
        if entry.usable && seen.insert(entry.provider_id.clone()) {
            names.push(entry.provider_id.clone());
        }
    }
    // The current route's provider stays listed even without a credential, so its `✓` row is
    // never missing (D5); switching to it still refuses without one (D6).
    if let Some(current) = current_provider
        && seen.insert(current.to_owned())
    {
        names.push(current.to_owned());
    }
    let mut candidates = Vec::new();
    for name in &names {
        let auth_env = auth_entries
            .iter()
            .find(|entry| &entry.provider_id == name)
            .and_then(|_| radian_host::AuthStore::open().ok())
            .and_then(|store| store.entry_env(name));
        if radian_host::resolve_provider(name, config_providers, auth_env.as_ref()).is_err() {
            continue;
        }
        for model in provider_models(name) {
            candidates.push(radian_tui::command::Candidate {
                insert: format!("{name}/{model}"),
                description: name.clone(),
                current: false,
            });
        }
    }
    sort_model_candidates(&mut candidates, current_provider);
    candidates
}

/// Marks the row in effect in a `/model` candidate list: the `provider/model` row (or the
/// `provider/` row). The pure part of the `Fetch::ModelRoute` answer; the loop owns the I/O.
pub(super) fn mark_current_route(
    candidates: Vec<radian_tui::command::Candidate>,
    provider: &str,
    model: &str,
) -> Vec<radian_tui::command::Candidate> {
    candidates
        .into_iter()
        .map(|mut candidate| {
            if candidate.insert == format!("{provider}/{model}")
                || candidate.insert == format!("{provider}/")
            {
                candidate.current = true;
            }
            candidate
        })
        .collect()
}

/// The flat `/model` list's order (ADR 0035): the current provider's models first,
/// then by provider and model name.
fn sort_model_candidates(
    candidates: &mut [radian_tui::command::Candidate],
    current_provider: Option<&str>,
) {
    let is_current = |candidate: &radian_tui::command::Candidate| {
        current_provider.is_some_and(|provider| {
            candidate
                .insert
                .strip_prefix(provider)
                .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    candidates.sort_by(|left, right| {
        is_current(right)
            .cmp(&is_current(left))
            .then_with(|| left.description.cmp(&right.description))
            .then_with(|| left.insert.cmp(&right.insert))
    });
}

/// One provider's model ids: the built-in descriptor's list, then the cached `/v1/models` ids
/// (D0), merged by id.
fn provider_models(provider: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    if let Some(descriptor) = radian_host::descriptor(provider) {
        for model in descriptor.models {
            if seen.insert(model.id.to_owned()) {
                out.push(model.id.to_owned());
            }
        }
    }
    if let Some(key) = radian_host::auth::resolve_key(provider)
        .ok()
        .flatten()
        .map(|resolved| resolved.key)
        && let Some(cache) = radian_host::read_provider_models(provider, &key)
    {
        for id in cache.models {
            if seen.insert(id.clone()) {
                out.push(id);
            }
        }
    }
    out
}

/// The [`ProviderRef`] a `/thinking` clamp/completion resolves against: the route's provider
/// name through the config's declarations and the auth entry's env (ADR 0026 D10). A name that
/// does not resolve falls back to the generic descriptor, whose map sends nothing.
pub(super) fn provider_ref_for(
    name: &str,
    config_providers: &std::collections::BTreeMap<String, radian_host::ProviderDecl>,
) -> radian_host::ProviderRef {
    let auth_env = radian_host::AuthStore::open()
        .ok()
        .and_then(|store| store.entry_env(name));
    radian_host::resolve_provider(name, config_providers, auth_env.as_ref())
        .map(|(provider, _warning)| provider)
        .unwrap_or_else(|_| {
            radian_host::ProviderRef::BuiltIn(
                radian_host::descriptor(radian_host::GENERIC_ID).expect("the generic descriptor"),
            )
        })
}

pub(super) fn memory_record_candidates(
    memory: &Option<MemoryService>,
    _command: &str,
) -> Vec<radian_tui::command::Candidate> {
    let Some(service) = memory else {
        return Vec::new();
    };
    let Ok(records) = service.list(radian_host::memory::ScopeSelection::Default, false) else {
        return Vec::new();
    };
    records
        .into_iter()
        .map(|record| radian_tui::command::Candidate {
            insert: record.id.as_str().to_owned(),
            description: format!(
                "{} {}",
                record.kind.as_str(),
                record_snippet(&record.content)
            ),
            current: false,
        })
        .collect()
}

/// One line of a record's content, cut for a single completion row.
fn record_snippet(content: &str) -> String {
    let first = content.lines().next().unwrap_or_default();
    let mut text: String = first.chars().take(40).collect();
    if first.chars().count() > 40 {
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp `RADIAN_HOME` for one test (the redact.rs pattern): `auth.json` holds `entries`,
    /// the previous value is restored on drop, and a process-wide lock serializes these tests
    /// against each other — the variable is process-global, and the real `~/.radian` is never
    /// touched.
    struct EnvHome {
        previous: Option<std::ffi::OsString>,
        root: std::path::PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    impl EnvHome {
        fn with_entries(entries: &[(&str, &str)]) -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
            let root = std::env::temp_dir().join(format!(
                "radian-model-filter-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |time| time.as_nanos())
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let previous = std::env::var_os("RADIAN_HOME");
            // SAFETY: serialized by `ENV_LOCK`, restored on drop.
            unsafe { std::env::set_var("RADIAN_HOME", &root) };
            let mut store = radian_host::AuthStore::open().expect("a fresh store");
            for &(provider, key) in entries {
                store
                    .upsert_api_key(provider, key, &std::collections::BTreeMap::new())
                    .expect("the entry is stored");
            }
            Self {
                previous,
                root,
                _guard: guard,
            }
        }
    }

    impl Drop for EnvHome {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe { std::env::set_var("RADIAN_HOME", value) },
                None => unsafe { std::env::remove_var("RADIAN_HOME") },
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn empty_config() -> std::collections::BTreeMap<String, radian_host::ProviderDecl> {
        std::collections::BTreeMap::new()
    }

    /// ADR 0026 D5: with only `opencode-go` logged in, the list holds its models and no
    /// `anthropic/` or `chatgpt/` row. Removing the login filter fails this test.
    #[test]
    fn the_model_list_hides_providers_without_a_usable_credential() {
        let _home = EnvHome::with_entries(&[("opencode-go", "k-opencode-go")]);
        let candidates = model_route_candidates(None, &empty_config());
        assert!(
            candidates.iter().any(|c| c.insert.starts_with("opencode-go/")),
            "the logged-in provider is listed: {:?}",
            candidates.iter().map(|c| &c.insert).collect::<Vec<_>>()
        );
        assert!(
            candidates
                .iter()
                .all(|c| !c.insert.starts_with("anthropic/") && !c.insert.starts_with("chatgpt/")),
            "no logged-out provider is listed: {:?}",
            candidates.iter().map(|c| &c.insert).collect::<Vec<_>>()
        );
    }

    /// Adding an `anthropic` entry adds its models to the list.
    #[test]
    fn adding_a_login_adds_that_providers_models() {
        let _home = EnvHome::with_entries(&[
            ("opencode-go", "k-opencode-go"),
            ("anthropic", "k-anthropic"),
        ]);
        let candidates = model_route_candidates(None, &empty_config());
        assert!(
            candidates.iter().any(|c| c.insert.starts_with("anthropic/")),
            "the newly logged-in provider is listed: {:?}",
            candidates.iter().map(|c| &c.insert).collect::<Vec<_>>()
        );
        assert!(
            !candidates.iter().any(|c| c.insert.starts_with("chatgpt/")),
            "chatgpt stays out without a credential: {:?}",
            candidates.iter().map(|c| &c.insert).collect::<Vec<_>>()
        );
    }

    /// A custom provider declared in config, with its key in `auth.json` under its name (D10),
    /// is listed too — the filter must not drop what the switch would accept.
    #[test]
    fn a_logged_in_custom_provider_is_listed() {
        let _home = EnvHome::with_entries(&[
            ("opencode-go", "k-opencode-go"),
            ("corp", "k-corp"),
        ]);
        radian_host::write_provider_models("corp", "k-corp", vec!["m-corp".to_owned()])
            .expect("the cache is writable");
        let mut config = empty_config();
        config.insert(
            "corp".to_owned(),
            radian_host::ProviderDecl {
                kind: "openai-compatible".to_owned(),
                base_url: "http://127.0.0.1:9/v1".to_owned(),
                reasoning_replay: None,
            },
        );
        let candidates = model_route_candidates(None, &config);
        assert!(
            candidates.iter().any(|c| c.insert == "corp/m-corp"),
            "the custom provider is listed: {:?}",
            candidates.iter().map(|c| &c.insert).collect::<Vec<_>>()
        );
    }

    /// The current route's provider stays listed even without a credential, so its `✓` row is
    /// never missing — and `mark_current_route` marks exactly that row.
    #[test]
    fn the_current_provider_is_listed_and_marked_without_a_credential() {
        let _home = EnvHome::with_entries(&[("opencode-go", "k-opencode-go")]);
        let candidates = model_route_candidates(Some("anthropic"), &empty_config());
        let current_model = candidates
            .iter()
            .find(|c| c.description == "anthropic")
            .map(|c| c.insert.clone())
            .expect("the current provider is listed without a credential");
        let (provider, model) = current_model.split_once('/').expect("a route");
        assert_eq!(provider, "anthropic");
        let marked = mark_current_route(candidates, provider, model);
        for candidate in &marked {
            assert_eq!(
                candidate.current,
                candidate.insert == current_model,
                "only the route in effect is marked: {:?}",
                marked.iter().map(|c| &c.insert).collect::<Vec<_>>()
            );
        }
    }
    /// ADR 0035: the current provider's models come first in the flat `/model`
    /// list, then the rest by provider and model name.
    #[test]
    fn the_flat_model_list_puts_the_current_provider_first() {
        let candidate = |insert: &str, provider: &str| radian_tui::command::Candidate {
            insert: insert.to_owned(),
            description: provider.to_owned(),
            current: false,
        };
        let mut candidates = vec![
            candidate("openai/gpt-5", "openai"),
            candidate("opencode-go/zeta", "opencode-go"),
            candidate("anthropic/claude-4", "anthropic"),
            candidate("opencode-go/alpha", "opencode-go"),
        ];
        sort_model_candidates(&mut candidates, Some("opencode-go"));
        let order: Vec<&str> = candidates.iter().map(|c| c.insert.as_str()).collect();
        assert_eq!(
            order,
            [
                "opencode-go/alpha",
                "opencode-go/zeta",
                "anthropic/claude-4",
                "openai/gpt-5"
            ],
            "the current provider's models first, then by provider and model"
        );
        // Without a current provider, only the name order applies.
        sort_model_candidates(&mut candidates, None);
        let order: Vec<&str> = candidates.iter().map(|c| c.insert.as_str()).collect();
        assert_eq!(
            order,
            [
                "anthropic/claude-4",
                "openai/gpt-5",
                "opencode-go/alpha",
                "opencode-go/zeta"
            ]
        );
    }
}
