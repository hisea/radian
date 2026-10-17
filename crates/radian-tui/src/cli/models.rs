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
    // Every provider a switch could name (ADR 0026 D5/D10): the built-in descriptors, the
    // config's `[providers.<name>]` declarations, and the usable `auth.json` entries. Each is
    // resolved with `resolve_provider` so the list matches what a switch would accept, then its
    // models are flattened into one list (ADR 0035): the label is the model, the
    // description the provider.
    let auth_entries = radian_host::auth::list().unwrap_or_default();
    let mut names: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for descriptor in radian_host::descriptors() {
        if seen.insert(descriptor.id.to_owned()) {
            names.push(descriptor.id.to_owned());
        }
    }
    for name in config_providers.keys() {
        if seen.insert(name.clone()) {
            names.push(name.clone());
        }
    }
    for entry in &auth_entries {
        if entry.usable && seen.insert(entry.provider_id.clone()) {
            names.push(entry.provider_id.clone());
        }
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
