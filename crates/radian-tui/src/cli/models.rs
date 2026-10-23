//! The `/model` and `/memory` completion candidates: the provider/model list built from the
//! descriptors, the config and `auth.json`, its sort order, and the memory record ids with
//! their one-line snippets. Local reads only; the background refresh owns the cache.

use super::*;

/// The provider names `/model` considers (ADR 0026 D5/D10), before any per-provider model list
/// is read: every usable `auth.json` entry, every config-declared provider with a usable key,
/// and the current route's provider. The background refresh covers the same set (D0), so a
/// provider the list would show is never left without a cache.
pub(super) fn model_provider_names(
    auth_entries: &[radian_host::AuthEntry],
    current_provider: Option<&str>,
    config_providers: &std::collections::BTreeMap<String, radian_host::ProviderDecl>,
) -> Vec<String> {
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

/// The providers the background refresh covers: the same set `/model` would offer (ADR 0026
/// D0), read fresh from `auth.json`. A provider without a usable key is included here and
/// skipped by the refresh itself, exactly like a switch would refuse it.
pub(super) fn model_refresh_targets(
    current_provider: Option<&str>,
    config_providers: &std::collections::BTreeMap<String, radian_host::ProviderDecl>,
) -> Vec<String> {
    let auth_entries = radian_host::auth::list().unwrap_or_default();
    model_provider_names(&auth_entries, current_provider, config_providers)
}

/// The base URL one provider's model-list refresh GETs from (ADR 0026 D0/D10): a named
/// provider's own endpoint, resolved the way a switch resolves it (`[providers.<name>]` first,
/// then the `auth.json` entry's `env`); a built-in descriptor's own default, or the entry's
/// `env.RADIAN_BASE_URL` when it carries one — the endpoint a route resolution picks. `None`
/// when the name resolves to no provider at all.
pub(super) fn provider_base_url(
    provider_id: &str,
    config_providers: &std::collections::BTreeMap<String, radian_host::ProviderDecl>,
    auth_env: Option<&std::collections::BTreeMap<String, String>>,
) -> Option<String> {
    let (provider, _warning) =
        radian_host::resolve_provider(provider_id, config_providers, auth_env).ok()?;
    Some(match provider {
        radian_host::ProviderRef::Named(named) => named.base_url,
        radian_host::ProviderRef::BuiltIn(descriptor) => auth_env
            .and_then(|env| env.get(radian_host::RADIAN_BASE_URL))
            .cloned()
            .unwrap_or_else(|| descriptor.default_base_url.to_owned()),
    })
}

/// One background refresh pass over `targets` (ADR 0026 D0): each provider by the TTL/retry
/// rule, silent on failure, never blocking the caller.
pub(super) async fn refresh_provider_models(
    targets: Vec<String>,
    config_providers: std::collections::BTreeMap<String, radian_host::ProviderDecl>,
) {
    for provider_id in &targets {
        refresh_one(provider_id, &config_providers).await;
    }
}

/// One provider's refresh: fetch from its own base URL when the cache is stale or absent, and
/// write the cache — an empty list on failure, the retry marker (D0). No key, no request.
async fn refresh_one(
    provider_id: &str,
    config_providers: &std::collections::BTreeMap<String, radian_host::ProviderDecl>,
) {
    let Some(resolved) = radian_host::auth::resolve_key(provider_id).ok().flatten() else {
        return;
    };
    // The retry rule (D0): skip when the cache is fresh, or when a fetch failed less
    // than the retry delay ago (the failure marker is an empty `models` write).
    let age = radian_host::provider_models_cache(provider_id).and_then(|path| {
        std::fs::metadata(path).ok().and_then(|meta| {
            meta.modified()
                .ok()
                .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
        })
    });
    let cached = radian_host::read_provider_models(provider_id, &resolved.key);
    let cached_models = cached.as_ref().map_or(0, |cache| cache.models.len());
    if !radian_host::provider_models_stale(age, cached_models) {
        return;
    }
    let Some(base_url) = provider_base_url(provider_id, config_providers, Some(&resolved.env))
    else {
        return;
    };
    match radian_host::fetch_provider_models(provider_id, &base_url, &resolved.key).await {
        Some(models) => {
            let _ = radian_host::write_provider_models(provider_id, &resolved.key, models);
        }
        None => {
            // The failure marker: an empty list, so the retry delay applies (D0).
            let _ = radian_host::write_provider_models(provider_id, &resolved.key, Vec::new());
        }
    }
}
/// The record ids a `/memory` command can act on, as completion candidates. A read failure is
/// an empty list, never a turn failure (ADR 0021 invariant 2).
///
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
    let names = model_provider_names(&auth_entries, current_provider, config_providers);
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

    /// One credential entry a test stores: provider name, key, and the entry's `env` pairs.
    type TestEntry<'a> = (&'a str, &'a str, Vec<(String, String)>);

    impl EnvHome {
        fn with_entries(entries: &[(&str, &str)]) -> Self {
            let owned: Vec<TestEntry<'_>> = entries
                .iter()
                .map(|&(provider, key)| (provider, key, Vec::new()))
                .collect();
            Self::with_env_entries(&owned)
        }

        /// The same home, with each entry's `env` object (a named provider's
        /// `RADIAN_BASE_URL`/`RADIAN_KIND`, ADR 0026 D10) written too.
        fn with_env_entries(entries: &[TestEntry<'_>]) -> Self {
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
            for (provider, key, env) in entries {
                let env: std::collections::BTreeMap<String, String> = env.iter().cloned().collect();
                store
                    .upsert_api_key(provider, key, &env)
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
            candidates
                .iter()
                .any(|c| c.insert.starts_with("opencode-go/")),
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
            candidates
                .iter()
                .any(|c| c.insert.starts_with("anthropic/")),
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
        let _home = EnvHome::with_entries(&[("opencode-go", "k-opencode-go"), ("corp", "k-corp")]);
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

    /// A one-shot HTTP server standing in for a provider's model-list endpoint (the
    /// `radian-host` catalog tests' pattern): it records the request head and answers once.
    fn capturing_server(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{addr}"), rx)
    }

    fn auth_env(base: &str) -> Vec<(String, String)> {
        vec![
            (
                radian_host::RADIAN_BASE_URL.to_owned(),
                format!("{base}/v1"),
            ),
            ("RADIAN_KIND".to_owned(), "openai-compatible".to_owned()),
        ]
    }

    fn corp_decl(base_url: &str) -> radian_host::ProviderDecl {
        radian_host::ProviderDecl {
            kind: "openai-compatible".to_owned(),
            base_url: base_url.to_owned(),
            reasoning_replay: None,
        }
    }

    /// ADR 0026 D0/D10: a named provider stored by `/login custom` (an `auth.json` entry whose
    /// `env` names the base URL) fetches its list from **its own** base URL, and `/model` then
    /// offers `<name>/<model>` rows. Taking the base URL from the descriptor again (empty for a
    /// named provider) makes the fetch miss `/models` and this test fail.
    #[tokio::test]
    async fn a_named_auth_provider_refreshes_from_its_own_base_url() {
        let (base, request) = capturing_server(
            "200 OK",
            r#"{"object":"list","data":[{"id":"glm-5.3"},{"id":"glm-5.3-flash"}]}"#,
        );
        let _home = EnvHome::with_env_entries(&[("z-ai", "sk-z-ai", auth_env(&base))]);
        let config = empty_config();
        assert!(
            model_refresh_targets(None, &config).contains(&"z-ai".to_owned()),
            "a usable auth entry is a refresh target"
        );
        refresh_provider_models(vec!["z-ai".to_owned()], config.clone()).await;
        let head = request
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the model-list request reached the provider's own base URL");
        assert!(
            head.starts_with("GET /v1/models "),
            "the entry's base URL decides the path"
        );
        assert!(
            head.to_lowercase()
                .contains("authorization: bearer sk-z-ai"),
            "the key rides as Bearer auth"
        );
        let key = radian_host::auth::resolve_key("z-ai")
            .unwrap()
            .expect("the key")
            .key;
        let cache = radian_host::read_provider_models("z-ai", &key)
            .expect("the named provider's cache is written");
        assert_eq!(cache.models, vec!["glm-5.3", "glm-5.3-flash"]);
        let candidates = model_route_candidates(None, &config);
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.insert == "z-ai/glm-5.3"),
            "the refreshed list reaches /model: {:?}",
            candidates.iter().map(|c| &c.insert).collect::<Vec<_>>()
        );
    }

    /// ADR 0026 D0/D10: a `[providers.<name>]` provider (its key in `auth.json` under the name)
    /// fetches from its declared `base_url`, never a built-in descriptor's.
    #[tokio::test]
    async fn a_config_declared_provider_refreshes_from_its_declared_base_url() {
        let (base, request) = capturing_server("200 OK", r#"{"data":[{"id":"m-corp"}]}"#);
        let _home = EnvHome::with_entries(&[("corp", "sk-corp")]);
        let mut config = empty_config();
        config.insert("corp".to_owned(), corp_decl(&format!("{base}/v1")));
        refresh_provider_models(vec!["corp".to_owned()], config.clone()).await;
        let head = request
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the model-list request reached the declared base URL");
        assert!(
            head.starts_with("GET /v1/models "),
            "the declaration's base URL decides the path"
        );
        let key = radian_host::auth::resolve_key("corp")
            .unwrap()
            .expect("the key")
            .key;
        let cache = radian_host::read_provider_models("corp", &key).expect("the cache is written");
        assert_eq!(cache.models, vec!["m-corp"]);
        let candidates = model_route_candidates(None, &config);
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.insert == "corp/m-corp"),
            "the refreshed list reaches /model: {:?}",
            candidates.iter().map(|c| &c.insert).collect::<Vec<_>>()
        );
    }

    /// ADR 0026 D0: the pass covers every listed provider, not only the route this run started
    /// with. Refreshing only the startup provider again fails the second cache assertion.
    #[tokio::test]
    async fn a_provider_that_is_not_the_startup_provider_is_refreshed_too() {
        let (auth_base, auth_request) =
            capturing_server("200 OK", r#"{"data":[{"id":"z-model"}]}"#);
        let (corp_base, corp_request) =
            capturing_server("200 OK", r#"{"data":[{"id":"c-model"}]}"#);
        let _home = EnvHome::with_env_entries(&[
            ("z-ai", "sk-z-ai", auth_env(&auth_base)),
            ("corp", "sk-corp", Vec::new()),
        ]);
        let mut config = empty_config();
        config.insert("corp".to_owned(), corp_decl(&format!("{corp_base}/v1")));
        let targets = model_refresh_targets(Some("opencode-go"), &config);
        assert!(targets.contains(&"z-ai".to_owned()), "{targets:?}");
        assert!(targets.contains(&"corp".to_owned()), "{targets:?}");
        refresh_provider_models(targets, config.clone()).await;
        auth_request
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the auth-declared provider refreshed");
        corp_request
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the config-declared provider refreshed");
        for (name, key) in [("z-ai", "sk-z-ai"), ("corp", "sk-corp")] {
            assert!(
                radian_host::read_provider_models(name, key).is_some(),
                "{name}'s cache is written"
            );
        }
    }

    /// ADR 0026 D0: a built-in descriptor keeps its own model-list URL rule (`opencode-go`
    /// lists on its discovery endpoint, never `<base>/models`), whatever base URL a route or
    /// an entry resolves.
    #[test]
    fn a_built_in_descriptor_keeps_its_own_model_list_url() {
        let env: std::collections::BTreeMap<String, String> =
            auth_env("https://corp.example").into_iter().collect();
        let base = provider_base_url("opencode-go", &empty_config(), Some(&env))
            .expect("a built-in resolves");
        assert_eq!(
            radian_host::provider_models_url("opencode-go", &base),
            "https://opencode.ai/zen/go/v1/models"
        );
        // Without an entry base URL, the descriptor's own default stands.
        let base =
            provider_base_url("opencode-go", &empty_config(), None).expect("a built-in resolves");
        assert_eq!(base, "https://opencode.ai/inference/go/openai/v1");
        // A named provider keeps `<its base>/models`.
        let mut config = empty_config();
        config.insert("corp".to_owned(), corp_decl("https://corp.example/v1/"));
        let base = provider_base_url("corp", &config, None).expect("a named provider resolves");
        assert_eq!(
            radian_host::provider_models_url("corp", &base),
            "https://corp.example/v1/models"
        );
    }

    /// ADR 0026 D0: a failing endpoint writes the failure marker (an empty list) and the next
    /// pass waits out the retry delay; past it the same pass fetches again and writes the list.
    #[tokio::test]
    async fn a_failing_endpoint_writes_the_marker_and_retries_only_after_the_delay() {
        let (failing, _failing_request) = capturing_server("404 Not Found", "{}");
        let _home = EnvHome::with_env_entries(&[("z-ai", "sk-z-ai", auth_env(&failing))]);
        let config = empty_config();
        refresh_provider_models(vec!["z-ai".to_owned()], config.clone()).await;
        let key = radian_host::auth::resolve_key("z-ai")
            .unwrap()
            .expect("the key")
            .key;
        let cache =
            radian_host::read_provider_models("z-ai", &key).expect("the failure marker is written");
        assert!(cache.models.is_empty(), "the marker is an empty list");

        // A working endpoint under the same name: the failure is younger than the retry delay,
        // so this pass does not fetch.
        let (good, request) = capturing_server("200 OK", r#"{"data":[{"id":"glm-5.3"}]}"#);
        let env: std::collections::BTreeMap<String, String> = auth_env(&good).into_iter().collect();
        radian_host::upsert_auth_api_key("z-ai", "sk-z-ai", &env).expect("the entry is updated");
        refresh_provider_models(vec!["z-ai".to_owned()], config.clone()).await;
        assert!(
            request
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "the retry delay holds the fetch off"
        );

        // Past the retry delay the same pass fetches and replaces the marker.
        let path = radian_host::provider_models_cache("z-ai").expect("a cache path");
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(
            std::time::SystemTime::now()
                - radian_host::PROVIDER_MODELS_RETRY
                - std::time::Duration::from_secs(1),
        )
        .unwrap();
        drop(file);
        refresh_provider_models(vec!["z-ai".to_owned()], config).await;
        request
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the retried request arrives");
        let cache = radian_host::read_provider_models("z-ai", &key).expect("the cache is written");
        assert_eq!(cache.models, vec!["glm-5.3"]);
    }
}
