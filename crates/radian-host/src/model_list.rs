//! The `/model` list (ADR 0026 D5): the logged-in providers' models, in the order the TUI
//! shows them.
//!
//! One implementation for the TUI and the control port's `models` verb, so the list a client
//! can see and the route a `config` change can name cannot drift. Local reads only: `auth.json`
//! and the per-provider model cache (the background refresh writes that cache).

use std::collections::BTreeMap;

use crate::auth::AuthEntry;
use crate::provider::ProviderDecl;

/// One model a `/model` list offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub provider: String,
    pub model: String,
}

/// The provider names `/model` considers (ADR 0026 D5/D10), before any per-provider model list
/// is read: every usable `auth.json` entry, every config-declared provider with a usable key,
/// and the current route's provider. The background refresh covers the same set (D0), so a
/// provider the list would show is never left without a cache.
pub fn model_provider_names(
    auth_entries: &[AuthEntry],
    current_provider: Option<&str>,
    config_providers: &BTreeMap<String, ProviderDecl>,
) -> Vec<String> {
    let logged_in = |name: &str| {
        auth_entries
            .iter()
            .any(|entry| entry.provider_id == name && entry.usable)
    };
    let mut names: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for descriptor in crate::provider::descriptors() {
        if logged_in(descriptor.id) && seen.insert(descriptor.id.to_owned()) {
            names.push(descriptor.id.to_owned());
        }
    }
    for name in config_providers.keys() {
        if logged_in(name) && seen.insert(name.clone()) {
            names.push(name.clone());
        }
    }
    for entry in auth_entries {
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
    names
}

/// The [`ProviderRef`](crate::ProviderRef) a `/thinking` clamp/completion resolves against: the
/// route's provider name through the config's declarations and the auth entry's env (ADR 0026
/// D10), with its model list attached so the thinking map follows the **resolved** protocol
/// (D10). A name that does not resolve falls back to the generic descriptor, whose map sends
/// nothing.
pub fn provider_ref_for(
    name: &str,
    config_providers: &BTreeMap<String, ProviderDecl>,
) -> crate::ProviderRef {
    let auth_env = crate::auth::AuthStore::open()
        .ok()
        .and_then(|store| store.entry_env(name));
    let model_list = crate::auth::resolve_key(name)
        .ok()
        .flatten()
        .and_then(|resolved| crate::read_provider_models(name, &resolved.key));
    crate::provider::resolve_provider_with_models(
        name,
        config_providers,
        auth_env.as_ref(),
        model_list,
    )
    .map(|(provider, _warning)| provider)
    .unwrap_or_else(|_| {
        crate::ProviderRef::BuiltIn(
            crate::provider::descriptor(crate::GENERIC_ID).expect("the generic descriptor"),
        )
    })
}

/// One provider's model ids: the built-in descriptor's list, then the cached `/v1/models` ids
/// (D0), merged by id.
pub fn provider_models(provider: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    if let Some(descriptor) = crate::provider::descriptor(provider) {
        for model in descriptor.models {
            if seen.insert(model.id.to_owned()) {
                out.push(model.id.to_owned());
            }
        }
    }
    if let Some(key) = crate::auth::resolve_key(provider)
        .ok()
        .flatten()
        .map(|resolved| resolved.key)
        && let Some(cache) = crate::read_provider_models(provider, &key)
    {
        for id in cache.models {
            if seen.insert(id.clone()) {
                out.push(id);
            }
        }
    }
    out
}

/// The flat `/model` list (ADR 0026 D5), in the order the TUI shows it: every resolvable
/// provider's models, the current provider's first, then by provider and model name.
pub fn model_choices(
    current_provider: Option<&str>,
    config_providers: &BTreeMap<String, ProviderDecl>,
) -> Vec<ModelChoice> {
    let auth_entries = crate::auth::list().unwrap_or_default();
    let names = model_provider_names(&auth_entries, current_provider, config_providers);
    let mut choices = Vec::new();
    for name in &names {
        let auth_env = auth_entries
            .iter()
            .find(|entry| &entry.provider_id == name)
            .and_then(|_| crate::auth::AuthStore::open().ok())
            .and_then(|store| store.entry_env(name));
        if crate::provider::resolve_provider(name, config_providers, auth_env.as_ref()).is_err() {
            continue;
        }
        for model in provider_models(name) {
            choices.push(ModelChoice {
                provider: name.clone(),
                model,
            });
        }
    }
    sort_choices(&mut choices, current_provider);
    choices
}

/// The flat `/model` list's order (ADR 0035): the current provider's models first, then by
/// provider and model name.
fn sort_choices(choices: &mut [ModelChoice], current_provider: Option<&str>) {
    let is_current = |choice: &ModelChoice| current_provider.is_some_and(|p| choice.provider == p);
    choices.sort_by(|left, right| {
        is_current(right)
            .cmp(&is_current(left))
            .then_with(|| left.provider.cmp(&right.provider))
            .then_with(|| left.model.cmp(&right.model))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(provider: &str, model: &str) -> ModelChoice {
        ModelChoice {
            provider: provider.to_owned(),
            model: model.to_owned(),
        }
    }

    /// ADR 0035: the current provider's models come first in the flat `/model` list, then the
    /// rest by provider and model name. Without a current provider, only the name order
    /// applies.
    #[test]
    fn the_flat_model_list_puts_the_current_provider_first() {
        let mut choices = vec![
            choice("openai", "gpt-5"),
            choice("opencode-go", "zeta"),
            choice("anthropic", "claude-4"),
            choice("opencode-go", "alpha"),
        ];
        sort_choices(&mut choices, Some("opencode-go"));
        assert_eq!(
            choices,
            vec![
                choice("opencode-go", "alpha"),
                choice("opencode-go", "zeta"),
                choice("anthropic", "claude-4"),
                choice("openai", "gpt-5"),
            ],
            "the current provider's models first, then by provider and model"
        );
        sort_choices(&mut choices, None);
        assert_eq!(
            choices,
            vec![
                choice("anthropic", "claude-4"),
                choice("openai", "gpt-5"),
                choice("opencode-go", "alpha"),
                choice("opencode-go", "zeta"),
            ]
        );
    }
}
