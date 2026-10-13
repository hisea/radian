//! The models.dev catalog: where a model's context window comes from (ADR 0018 §目录).
//!
//! The window is **not** guessed from the model name. It is either the host's explicit
//! `--context-window`, or a lookup in the public models.dev catalog matched by
//! `(provider, model)`. The catalog is fetched over a read-only GET with no key, no header and
//! no user data, cached owner-only under the same cache root as the diagnostics (ADR 0013
//! decision 4), and refreshed at most once every [`CACHE_TTL`].
//!
//! The fetch is deliberately allowed to fail: a missing or stale catalog leaves the window
//! unknown and the guard off (ADR 0015), never a guessed number.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

/// The public catalog. Overridable with `RADIAN_CATALOG_URL` (a test seam; not in `--help`).
pub const CATALOG_URL: &str = "https://models.dev/api.json";
/// Re-fetch at most this often; inside it, the cache is used without any network call.
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// The whole fetch is bounded by this; a slow catalog must not hold up startup.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(3);

/// The hostname→provider map for deriving the catalog provider from `--base-url`. Matched by
/// hostname only (ADR 0018): a provider's `api` field and the owner's base URL differ in path.
pub fn provider_for_host(host: &str) -> Option<&'static str> {
    match host.trim_end_matches('.').to_ascii_lowercase().as_str() {
        "opencode.ai" => Some("opencode"),
        "api.anthropic.com" => Some("anthropic"),
        "api.openai.com" => Some("openai"),
        "api.deepseek.com" => Some("deepseek"),
        // Verified against the 2026-10-13 catalog (their `api` hostnames).
        "openrouter.ai" => Some("openrouter"),
        "api.moonshot.cn" => Some("moonshotai-cn"),
        _ => None,
    }
}

/// The hostname of a base URL, lower-cased, without userinfo or port. `None` when the URL has
/// no authority or does not parse.
pub fn host_of(base_url: &str) -> Option<String> {
    let rest = base_url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if authority.starts_with('[') {
        authority.split(']').next()?.trim_start_matches('[')
    } else {
        authority.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// One model's limits as the catalog reports them. The three numbers are kept **separately**
/// (ADR 0018): `context` is the total window `C`, `input` the input ceiling `I`
/// when the provider publishes one, and `output` the output ceiling. A missing or zero value
/// means "unknown", never a fabricated number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModelLimit {
    pub context: Option<u64>,
    pub output: Option<u64>,
    pub input: Option<u64>,
}

impl ModelLimit {
    /// The window handed to the existing [`agent_core::ContextBudget`] in this slice:
    /// `min(I, C)` when both are known, `C` when `I` is missing, `I` when `C` is missing, and
    /// `None` when neither is positive.
    ///
    /// **Temporary:** the compaction slice will replace this with ADR 0018's effective input
    /// budget `B = min(I, C − R)` (`R` the output reservation). The three numbers are stored
    /// separately so that change needs no re-parsing.
    pub fn window_tokens(&self) -> Option<u64> {
        let context = self.context.filter(|tokens| *tokens > 0);
        let input = self.input.filter(|tokens| *tokens > 0);
        match (input, context) {
            (Some(input), Some(context)) => Some(input.min(context)),
            (Some(input), None) => Some(input),
            (None, Some(context)) => Some(context),
            (None, None) => None,
        }
    }
}

/// The parsed catalog: provider id → model id → limits.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    providers: BTreeMap<String, BTreeMap<String, ModelLimit>>,
}

impl Catalog {
    /// Parses `api.json`. Unknown fields and missing `limit` fields are ignored; a top-level
    /// that is not an object is "corrupt" and returns `None` (treated as no cache).
    pub fn parse(text: &str) -> Option<Self> {
        let root: Value = serde_json::from_str(text).ok()?;
        let Value::Object(providers) = root else {
            return None;
        };
        let mut parsed = BTreeMap::new();
        for (provider_id, provider) in providers {
            let Some(models) = provider.get("models").and_then(Value::as_object) else {
                continue;
            };
            let mut models_out = BTreeMap::new();
            for (model_id, model) in models {
                let Some(limit) = model.get("limit").and_then(Value::as_object) else {
                    continue;
                };
                models_out.insert(
                    model_id.clone(),
                    ModelLimit {
                        context: limit.get("context").and_then(Value::as_u64),
                        output: limit.get("output").and_then(Value::as_u64),
                        input: limit.get("input").and_then(Value::as_u64),
                    },
                );
            }
            if !models_out.is_empty() {
                parsed.insert(provider_id.clone(), models_out);
            }
        }
        Some(Self { providers: parsed })
    }

    pub fn limit(&self, provider: &str, model: &str) -> Option<ModelLimit> {
        self.providers.get(provider)?.get(model).copied()
    }

    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    pub fn model_count(&self, provider: &str) -> usize {
        self.providers.get(provider).map_or(0, BTreeMap::len)
    }
}

/// Where a resolved window came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowSource {
    /// An explicit `--context-window`.
    Explicit,
    /// The catalog, with the matched provider/model and how old the data is.
    Catalog {
        provider: String,
        model: String,
        age: Option<Duration>,
    },
    /// Nothing matched; the guard is off.
    Unknown,
}

/// The resolved window for a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub tokens: Option<u64>,
    pub source: WindowSource,
    pub limit: Option<ModelLimit>,
}

impl Window {
    fn unknown() -> Self {
        Self {
            tokens: None,
            source: WindowSource::Unknown,
            limit: None,
        }
    }

    /// The one startup line (ADR 0018): the window and exactly where it came from. It never
    /// contains a key, a header value or a URL.
    pub fn describe(&self) -> String {
        match (&self.source, self.tokens) {
            (WindowSource::Explicit, Some(tokens)) => {
                format!(
                    "window {tokens} from --context-window{}",
                    input_output_suffix(self.limit.as_ref())
                )
            }
            (
                WindowSource::Catalog {
                    provider,
                    model,
                    age,
                },
                Some(tokens),
            ) => {
                format!(
                    "window {tokens} from models.dev ({provider}/{model}{}, {})",
                    limits_suffix(self.limit.as_ref()),
                    match age {
                        Some(age) if *age < Duration::from_secs(60) => "just fetched".to_string(),
                        Some(age) => format!("cached {} ago", human_age(*age)),
                        None => "age unknown".to_string(),
                    }
                )
            }
            _ => "unknown: context guard off".to_string(),
        }
    }
}

fn human_age(age: Duration) -> String {
    let secs = age.as_secs();
    if secs < 60 {
        format!("{secs} s")
    } else if secs < 60 * 60 {
        format!("{} min", secs / 60)
    } else {
        format!("{} h", secs / (60 * 60))
    }
}

/// `, context 1000000, input 922000, output 384000` for whichever of the three the catalog
/// published (ADR 0018: they are shown separately, not merged).
fn limits_suffix(limit: Option<&ModelLimit>) -> String {
    let Some(limit) = limit else {
        return String::new();
    };
    let mut parts = Vec::new();
    for (name, value) in [
        ("context", limit.context),
        ("input", limit.input),
        ("output", limit.output),
    ] {
        if let Some(value) = value.filter(|value| *value > 0) {
            parts.push(format!("{name} {value}"));
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(", {}", parts.join(", "))
    }
}

/// ` (cached input 922000, output 384000)`: the part of a cached entry an explicit
/// `--context-window` still reads (ADR 0018). The explicit number is `C`, so the
/// catalog's `context` is not shown.
fn input_output_suffix(limit: Option<&ModelLimit>) -> String {
    let Some(limit) = limit else {
        return String::new();
    };
    let mut parts = Vec::new();
    for (name, value) in [("input", limit.input), ("output", limit.output)] {
        if let Some(value) = value.filter(|value| *value > 0) {
            parts.push(format!("{name} {value}"));
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" (cached {})", parts.join(", "))
    }
}

/// Everything the resolver needs. `cache_path`/`url` are explicit so tests never touch the
/// real cache or the network; [`WindowQuery::production`] fills them from the environment.
pub struct WindowQuery {
    pub explicit: Option<u64>,
    pub catalog_provider: Option<String>,
    pub base_url: String,
    pub model: String,
    pub offline: bool,
    pub cache_path: PathBuf,
    pub url: String,
}

impl WindowQuery {
    /// The production query: the real cache path and [`CATALOG_URL`] (or the
    /// `RADIAN_CATALOG_URL` test seam when set).
    pub fn production(
        explicit: Option<u64>,
        catalog_provider: Option<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        offline: bool,
    ) -> Self {
        Self {
            explicit,
            catalog_provider,
            base_url: base_url.into(),
            model: model.into(),
            offline,
            cache_path: cache_path(),
            url: std::env::var("RADIAN_CATALOG_URL").unwrap_or_else(|_| CATALOG_URL.to_string()),
        }
    }

    fn provider(&self) -> Option<String> {
        self.catalog_provider.clone().or_else(|| {
            host_of(&self.base_url).and_then(|host| provider_for_host(&host).map(str::to_owned))
        })
    }
}

/// The catalog cache file: `~/.radian/cache/catalog/models.json` (ADR 0020's layout).
pub fn cache_path() -> PathBuf {
    match crate::home::catalog_cache() {
        Some(path) => path,
        // No HOME: a path the write will fail on, so the window stays unknown.
        None => PathBuf::from("radian-catalog-models.json"),
    }
}

/// Resolves the window: `--context-window`, then the catalog, then unknown.
pub async fn resolve(query: &WindowQuery) -> Window {
    if let Some(explicit) = query.explicit {
        // ADR 0018: an explicit window never fetches. If a cache already exists,
        // its `input`/`output` are read from it (for the input ceiling and the output
        // capability checks); otherwise only the explicit number is used.
        let limit = query.provider().and_then(|provider| {
            read_cache(&query.cache_path)
                .and_then(|(catalog, _)| catalog.limit(&provider, &query.model))
        });
        return Window {
            tokens: Some(explicit),
            source: WindowSource::Explicit,
            limit,
        };
    }
    let Some(provider) = query.provider() else {
        return Window::unknown();
    };
    let (catalog, age) = load(&query.cache_path, &query.url, query.offline).await;
    let Some(catalog) = catalog else {
        return Window::unknown();
    };
    let Some(limit) = catalog.limit(&provider, &query.model) else {
        return Window::unknown();
    };
    match limit.window_tokens() {
        Some(tokens) => Window {
            tokens: Some(tokens),
            source: WindowSource::Catalog {
                provider,
                model: query.model.clone(),
                age,
            },
            limit: Some(limit),
        },
        None => Window::unknown(),
    }
}

/// Loads the catalog from the cache or the network. Returns the catalog and the age of the
/// data used (`Some(0)` when it was just fetched). Never fails: a fetch failure falls back to
/// a stale cache, and no cache means `None`.
async fn load(cache_path: &Path, url: &str, offline: bool) -> (Option<Catalog>, Option<Duration>) {
    let cached = read_cache(cache_path);
    if offline {
        return cached
            .map(|(catalog, age)| (Some(catalog), Some(age)))
            .unwrap_or((None, None));
    }
    if let Some((catalog, age)) = &cached
        && *age < CACHE_TTL
    {
        return (Some(catalog.clone()), Some(*age));
    }
    match fetch(url).await.and_then(|text| {
        let catalog = Catalog::parse(&text)?;
        Some((catalog, text))
    }) {
        Some((catalog, text)) => {
            let _ = write_cache(cache_path, &text);
            (Some(catalog), Some(Duration::ZERO))
        }
        None => cached
            .map(|(catalog, age)| (Some(catalog), Some(age)))
            .unwrap_or((None, None)),
    }
}

/// Reads and parses the cache. A missing file, an unreadable one, or corrupt JSON is "no
/// cache". The age comes from the file's mtime.
fn read_cache(path: &Path) -> Option<(Catalog, Duration)> {
    let text = std::fs::read_to_string(path).ok()?;
    let catalog = Catalog::parse(&text)?;
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);
    Some((catalog, age))
}

/// Writes the cache atomically (temp file + rename) with owner-only permissions.
fn write_cache(path: &Path, text: &str) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "cache path has no directory")
    })?;
    radian_session::ensure_private_dir(dir)?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(radian_session::PRIVATE_FILE_MODE);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// One read-only GET with a short timeout, no key and no custom headers.
async fn fetch(url: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .ok()?;
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.text().await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_mapping_is_by_hostname_only() {
        assert_eq!(provider_for_host("opencode.ai"), Some("opencode"));
        assert_eq!(provider_for_host("OPENCODE.AI"), Some("opencode"));
        assert_eq!(provider_for_host("api.anthropic.com"), Some("anthropic"));
        assert_eq!(provider_for_host("api.openai.com"), Some("openai"));
        assert_eq!(provider_for_host("api.deepseek.com"), Some("deepseek"));
        assert_eq!(provider_for_host("openrouter.ai"), Some("openrouter"));
        assert_eq!(provider_for_host("api.moonshot.cn"), Some("moonshotai-cn"));
        assert_eq!(provider_for_host("evil.example.com"), None);
    }

    #[test]
    fn the_catalog_host_ignores_the_path_the_owner_uses() {
        // The owner's base URL is `/zen/go/v1` while the catalog's `api` is `/inference/v1`;
        // the hostname is what matches.
        assert_eq!(
            host_of("https://opencode.ai/zen/go/v1").as_deref(),
            Some("opencode.ai")
        );
        assert_eq!(
            host_of("https://api.deepseek.com/v1").as_deref(),
            Some("api.deepseek.com")
        );
        assert_eq!(
            host_of("http://127.0.0.1:8080/x").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(host_of("not a url"), None);
    }

    #[test]
    fn window_tokens_is_min_of_input_and_context_and_zero_is_unknown() {
        let both = ModelLimit {
            context: Some(1_000_000),
            input: Some(922_000),
            output: Some(128_000),
        };
        assert_eq!(both.window_tokens(), Some(922_000));
        let no_input = ModelLimit {
            context: Some(100_000),
            input: None,
            output: None,
        };
        assert_eq!(no_input.window_tokens(), Some(100_000));
        let zero_input = ModelLimit {
            context: Some(100_000),
            input: Some(0),
            output: None,
        };
        assert_eq!(zero_input.window_tokens(), Some(100_000));
        let zero_context = ModelLimit {
            context: Some(0),
            input: None,
            output: None,
        };
        assert_eq!(zero_context.window_tokens(), None);
        // The three numbers are stored separately, not merged away.
        assert_eq!(both.context, Some(1_000_000));
        assert_eq!(both.input, Some(922_000));
        assert_eq!(both.output, Some(128_000));
    }

    #[test]
    fn parsing_tolerates_unknown_fields_and_missing_limits() {
        let text = r#"{
            "opencode": {
                "id": "opencode", "env": ["X"], "npm": "@x",
                "models": {
                    "deepseek-v4.1-flash": {"id": "x", "limit": {"context": 1000000, "output": 384000}},
                    "no-limit": {"id": "y"},
                    "future": {"limit": {"context": 5, "unknown_field": 1}}
                }
            },
            "empty": {"models": {}},
            "not-an-object": 7
        }"#;
        let catalog = Catalog::parse(text).expect("parse");
        assert_eq!(catalog.provider_count(), 1);
        assert_eq!(catalog.model_count("opencode"), 2);
        assert_eq!(
            catalog.limit("opencode", "deepseek-v4.1-flash"),
            Some(ModelLimit {
                context: Some(1_000_000),
                output: Some(384_000),
                input: None
            })
        );
        assert_eq!(catalog.limit("opencode", "no-limit"), None);
        assert_eq!(
            catalog.limit("opencode", "future").unwrap().context,
            Some(5)
        );
    }

    #[test]
    fn a_non_object_top_level_is_corrupt() {
        assert!(Catalog::parse("[]").is_none());
        assert!(Catalog::parse("not json").is_none());
        assert!(Catalog::parse("").is_none());
    }

    #[test]
    fn describe_names_the_source() {
        assert_eq!(
            Window {
                tokens: Some(1000),
                source: WindowSource::Explicit,
                limit: None
            }
            .describe(),
            "window 1000 from --context-window"
        );
        let catalog = Window {
            tokens: Some(922_000),
            source: WindowSource::Catalog {
                provider: "opencode".into(),
                model: "deepseek-v4.1-flash".into(),
                age: Some(Duration::from_secs(3 * 60 * 60)),
            },
            limit: Some(ModelLimit {
                context: Some(1_000_000),
                input: Some(922_000),
                output: Some(384_000),
            }),
        };
        assert_eq!(
            catalog.describe(),
            "window 922000 from models.dev (opencode/deepseek-v4.1-flash, context 1000000, \
             input 922000, output 384000, cached 3 h ago)"
        );
        assert_eq!(Window::unknown().describe(), "unknown: context guard off");
    }
}
