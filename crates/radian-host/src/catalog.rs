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

/// The **cap** on the output reservation `R` (ADR 0018): the reservation is a
/// quarter of the window, never more than this. A fixed 16384 made any window at or below 16K
/// have no budget at all.
pub const OUTPUT_RESERVATION_CAP: u64 = 16_384;

/// `R = max(min(16384, ⌊C/4⌋), request_max_output)` (ADR 0018).
///
/// The reservation scales with the window — a quarter, capped at [`OUTPUT_RESERVATION_CAP`] —
/// so a small-window model still has a budget. The request's **actual** output setting raises
/// it further, because that output really does occupy the window; a provider that sends no
/// such field does not raise it. `C` unknown: `R` does not take part in `B`, and the value is
/// the request's own output (0 when there is none). The catalog's `limit.output` is a
/// capability ceiling and does **not** participate.
pub fn output_reservation(context: Option<u64>, request_max_output: Option<u32>) -> u64 {
    let request = u64::from(request_max_output.unwrap_or(0));
    match context.filter(|tokens| *tokens > 0) {
        Some(context) => OUTPUT_RESERVATION_CAP.min(context / 4).max(request),
        None => request,
    }
}

/// ADR 0018's effective input budget `B = min(I, C − R)`.
///
/// `context` is `C` (`--context-window` when given, else the catalog), `input` is `I` (catalog
/// only), and `request_max_output` is the output the request itself sets. A missing or zero
/// value is unknown, never a fabricated number. `C ≤ R` is an error: the window cannot hold
/// the reservation.
pub fn input_budget(
    context: Option<u64>,
    input: Option<u64>,
    request_max_output: Option<u32>,
) -> Result<Option<u64>, String> {
    let context = context.filter(|tokens| *tokens > 0);
    let input = input.filter(|tokens| *tokens > 0);
    match (context, input) {
        (Some(context), input) => {
            let reservation = output_reservation(Some(context), request_max_output);
            if context <= reservation {
                return Err(format!(
                    "the context window ({context} tokens) is not larger than the output \
                     reservation ({reservation} tokens: a quarter of the window, capped at \
                     {OUTPUT_RESERVATION_CAP}, raised by the request's max output); raise \
                     --context-window or lower the request's max output"
                ));
            }
            let after_reservation = context - reservation;
            Ok(Some(input.map_or(after_reservation, |input| {
                input.min(after_reservation)
            })))
        }
        (None, Some(input)) => Ok(Some(input)),
        (None, None) => Ok(None),
    }
}

/// A one-line warning when the request's output setting is above the catalog's output ceiling
/// (ADR 0018: the ceiling is checked, not used in `R`).
pub fn output_ceiling_warning(
    request_max_output: Option<u32>,
    output_ceiling: Option<u64>,
) -> Option<String> {
    let request = u64::from(request_max_output?);
    let ceiling = output_ceiling.filter(|value| *value > 0)?;
    (request > ceiling).then(|| {
        format!(
            "[budget] warning: the request's max output ({request}) is above the catalog's \
             output ceiling for this model ({ceiling})"
        )
    })
}

/// One provider's catalog metadata (ADR 0026 D0). `api`/`env`/`name` are read so a descriptor
/// can fall back to the catalog; only models that carry a `limit` are kept, so the existing
/// window behavior is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfo {
    pub id: String,
    /// The provider's display name (`"OpenCode Go"`).
    pub name: Option<String>,
    /// The provider's default base URL. Absent for `anthropic`/`openai` in the 2026-10-13
    /// catalog (they rely on the SDK default).
    pub api: Option<String>,
    /// The environment variables the provider reads its key from. Informational only: ADR 0026
    /// D2 makes `auth.json` the only runtime source.
    pub env: Vec<String>,
}

/// One model in the catalog, for `/model`'s list (ADR 0026 D0). `limit` is the window ADR 0018
/// uses; a model without a `limit` is not in the catalog at all (existing behavior).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    pub id: String,
    /// The model's display name (`"Claude Opus 5.5"`), when the catalog has one.
    pub name: Option<String>,
    pub limit: ModelLimit,
}

/// One catalog model as parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CatalogModel {
    name: Option<String>,
    limit: ModelLimit,
}

/// One catalog provider as parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CatalogProvider {
    name: Option<String>,
    api: Option<String>,
    env: Vec<String>,
    models: BTreeMap<String, CatalogModel>,
}

/// The parsed catalog: provider id → provider metadata and its models.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    providers: BTreeMap<String, CatalogProvider>,
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
                    CatalogModel {
                        name: model.get("name").and_then(Value::as_str).map(str::to_owned),
                        limit: ModelLimit {
                            context: limit.get("context").and_then(Value::as_u64),
                            output: limit.get("output").and_then(Value::as_u64),
                            input: limit.get("input").and_then(Value::as_u64),
                        },
                    },
                );
            }
            if !models_out.is_empty() {
                parsed.insert(
                    provider_id.clone(),
                    CatalogProvider {
                        name: provider
                            .get("name")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        api: provider
                            .get("api")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        env: provider
                            .get("env")
                            .and_then(Value::as_array)
                            .map(|values| {
                                values
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        models: models_out,
                    },
                );
            }
        }
        Some(Self { providers: parsed })
    }

    pub fn limit(&self, provider: &str, model: &str) -> Option<ModelLimit> {
        self.providers
            .get(provider)?
            .models
            .get(model)
            .map(|entry| entry.limit)
    }

    /// The provider's catalog metadata (display name, `api`, `env`), if present.
    pub fn provider_info(&self, provider: &str) -> Option<ProviderInfo> {
        let entry = self.providers.get(provider)?;
        Some(ProviderInfo {
            id: provider.to_owned(),
            name: entry.name.clone(),
            api: entry.api.clone(),
            env: entry.env.clone(),
        })
    }

    /// The provider's models, in id order, for `/model`'s list.
    pub fn models_for(&self, provider: &str) -> Vec<ModelEntry> {
        self.providers
            .get(provider)
            .map(|entry| {
                entry
                    .models
                    .iter()
                    .map(|(id, model)| ModelEntry {
                        id: id.clone(),
                        name: model.name.clone(),
                        limit: model.limit,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The model's display name (`"Claude Opus 5.5"`), or `None` when the catalog has no such
    /// model or no name for it.
    pub fn display_name(&self, provider: &str, model: &str) -> Option<&str> {
        self.providers
            .get(provider)?
            .models
            .get(model)?
            .name
            .as_deref()
    }

    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    pub fn model_count(&self, provider: &str) -> usize {
        self.providers
            .get(provider)
            .map_or(0, |entry| entry.models.len())
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
    /// Nothing matched; the budget is off.
    Unknown,
}

/// The resolved window for a run: the numbers ADR 0018 distinguishes, plus the effective input
/// budget `B` computed from them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// The effective input budget `B`, or `None` when it cannot be computed (window unknown).
    pub budget: Option<u64>,
    /// `Some` when the window cannot hold the output reservation. See [`Window::startup_check`]
    /// for what a host does with it.
    pub error: Option<String>,
    /// `C`: `--context-window` when given, else the catalog's `limit.context`.
    pub context: Option<u64>,
    /// `I`: the catalog's `limit.input` (never from `--context-window`).
    pub input: Option<u64>,
    /// The catalog's output ceiling (a capability; only used for a warning).
    pub output: Option<u64>,
    /// `R`: the output reservation used to compute `B`.
    pub reservation: u64,
    pub source: WindowSource,
    pub limit: Option<ModelLimit>,
}

/// What a host does at startup with a [`Window`] whose `C ≤ R`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowCheck {
    /// Nothing to report.
    Ok,
    /// The user's own `--context-window` cannot hold the reservation: refuse to start.
    Refuse(String),
    /// The catalog's window cannot hold the reservation (a small model): warn, and run with
    /// the budget off. Refusing would make a small-window model unusable for a number the user
    /// never typed; the provider's own overflow error still ends a turn (ADR 0017).
    Warn(String),
}

impl Window {
    /// The startup decision for `C ≤ R`: refuse for an explicit `--context-window`, warn for
    /// a catalog window (the budget is already `None` then).
    pub fn startup_check(&self) -> WindowCheck {
        match (&self.error, &self.source) {
            (None, _) => WindowCheck::Ok,
            (Some(error), WindowSource::Explicit) => {
                WindowCheck::Refuse(format!("--context-window: {error}"))
            }
            (Some(error), _) => WindowCheck::Warn(format!(
                "[budget] warning: the catalog's window for this model {error}; the context \
                 budget is off"
            )),
        }
    }

    fn unknown() -> Self {
        Self {
            budget: None,
            error: None,
            context: None,
            input: None,
            output: None,
            // `C` is unknown, so `R` does not enter `B`; `describe` shows it as `?`.
            reservation: 0,
            source: WindowSource::Unknown,
            limit: None,
        }
    }

    /// The one startup line (ADR 0018): the numbers, the effective budget `B`, and exactly
    /// where they came from. It never contains a key, a header value or a URL.
    pub fn describe(&self) -> String {
        let numbers = format!(
            "C {}, I {}, output {}, R {}, B {}",
            number(self.context),
            number(self.input),
            number(self.output),
            number(Some(self.reservation)),
            number(self.budget),
        );
        let origin = match &self.source {
            WindowSource::Explicit => "from --context-window".to_string(),
            WindowSource::Catalog {
                provider,
                model,
                age,
            } => format!(
                "from models.dev ({provider}/{model}, {})",
                match age {
                    Some(age) if *age < Duration::from_secs(60) => "just fetched".to_string(),
                    Some(age) => format!("cached {} ago", human_age(*age)),
                    None => "age unknown".to_string(),
                }
            ),
            WindowSource::Unknown => "unknown".to_string(),
        };
        match (&self.error, self.budget) {
            (Some(error), _) => format!("no input budget: {error}"),
            (None, Some(_)) => format!("{numbers} {origin}"),
            (None, None) => format!(
                "window unknown ({numbers}); no budget, only the provider's over-length error \
                 remains"
            ),
        }
    }
}

fn number(value: Option<u64>) -> String {
    value
        .filter(|value| *value > 0)
        .map_or_else(|| "?".to_string(), |value| value.to_string())
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

/// Everything the resolver needs. `cache_path`/`url` are explicit so tests never touch the
/// real cache or the network; [`WindowQuery::production`] fills them from the environment.
pub struct WindowQuery {
    pub explicit: Option<u64>,
    pub catalog_provider: Option<String>,
    pub base_url: String,
    pub model: String,
    /// The output the request itself sets (`max_tokens`), for `R`; `None` for providers that
    /// do not send one.
    pub request_max_output: Option<u32>,
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
        request_max_output: Option<u32>,
        offline: bool,
    ) -> Self {
        Self {
            explicit,
            catalog_provider,
            base_url: base_url.into(),
            model: model.into(),
            request_max_output,
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
        // its `input`/`output` are read from it; otherwise only the explicit number is used.
        let limit = query.provider().and_then(|provider| {
            read_cache(&query.cache_path)
                .and_then(|(catalog, _)| catalog.limit(&provider, &query.model))
        });
        let input = limit.and_then(|limit| limit.input);
        let output = limit.and_then(|limit| limit.output);
        return build_window(
            Some(explicit),
            input,
            output,
            query.request_max_output,
            WindowSource::Explicit,
            limit,
        );
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
    build_window(
        limit.context,
        limit.input,
        limit.output,
        query.request_max_output,
        WindowSource::Catalog {
            provider,
            model: query.model.clone(),
            age,
        },
        Some(limit),
    )
}

/// Computes `B` and assembles a [`Window`], keeping a `C ≤ R` error instead of failing here
/// (the host decides how to report it).
fn build_window(
    context: Option<u64>,
    input: Option<u64>,
    output: Option<u64>,
    request_max_output: Option<u32>,
    source: WindowSource,
    limit: Option<ModelLimit>,
) -> Window {
    let reservation = output_reservation(context, request_max_output);
    match input_budget(context, input, request_max_output) {
        Ok(budget) => Window {
            budget,
            error: None,
            context,
            input,
            output,
            reservation,
            source,
            limit,
        },
        Err(error) => Window {
            budget: None,
            error: Some(error),
            context,
            input,
            output,
            reservation,
            source,
            limit,
        },
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
    fn the_output_reservation_scales_with_the_window_and_the_request() {
        // ADR 0018: R = max(min(16384, C/4), request).
        assert_eq!(output_reservation(Some(8_192), None), 2_048);
        assert_eq!(output_reservation(Some(40_000), None), 10_000);
        assert_eq!(output_reservation(Some(200_000), None), 16_384);
        // The request's own output raises R above the quarter, up to the request's value.
        assert_eq!(output_reservation(Some(8_192), Some(16_384)), 16_384);
        assert_eq!(output_reservation(Some(1_000_000), Some(32_000)), 32_000);
        // A request below the quarter does not lower it.
        assert_eq!(output_reservation(Some(200_000), Some(1_000)), 16_384);
        // C unknown: R does not take part in B; the value is the request's own output.
        assert_eq!(output_reservation(None, None), 0);
        assert_eq!(output_reservation(None, Some(32_000)), 32_000);
        assert_eq!(output_reservation(Some(0), None), 0);
    }

    #[test]
    fn the_input_budget_is_min_of_input_and_context_minus_the_reservation() {
        // The catalog's output ceiling (384000) does NOT raise R: R comes from the window and
        // the request.
        assert_eq!(
            input_budget(Some(1_000_000), Some(922_000), None),
            Ok(Some(922_000))
        );
        // C = 8192, no request max output: R = 2048, B = 6144.
        assert_eq!(input_budget(Some(8_192), None, None), Ok(Some(6_144)));
        // Anthropic with max_tokens = 32000: R = 32000, C - R = 968000.
        assert_eq!(
            input_budget(Some(1_000_000), Some(922_000), Some(32_000)),
            Ok(Some(922_000))
        );
        assert_eq!(
            input_budget(Some(1_000_000), None, Some(32_000)),
            Ok(Some(968_000))
        );
        // I < C - R: the input ceiling wins.
        assert_eq!(
            input_budget(Some(100_000), Some(50_000), Some(16_384)),
            Ok(Some(50_000))
        );
        // `--context-window` changes C (here 50000): R = 12500, B = 37500.
        assert_eq!(input_budget(Some(50_000), None, None), Ok(Some(37_500)));
        // C <= R only happens when the request's own output reaches C: an Anthropic request
        // with max_tokens = 16384 against a 8192 window.
        assert!(input_budget(Some(8_192), None, Some(16_384)).is_err());
        // Without a request output, the quarter is always below C, so there is no error.
        assert!(input_budget(Some(10_000), None, None).is_ok());
        // Both unknown: no budget.
        assert_eq!(input_budget(None, None, None), Ok(None));
        // I alone is a budget, even with a request output.
        assert_eq!(
            input_budget(None, Some(70_000), Some(32_000)),
            Ok(Some(70_000))
        );
        // The catalog output ceiling only warns.
        assert!(output_ceiling_warning(Some(32_000), Some(4_000)).is_some());
        assert!(output_ceiling_warning(Some(4_000), Some(32_000)).is_none());
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
    fn provider_and_model_names_are_read() {
        let text = r#"{
            "opencode-go": {
                "id": "opencode-go", "name": "OpenCode Go",
                "api": "https://opencode.ai/inference/go/v1",
                "env": ["OPENCODE_API_KEY"],
                "models": {
                    "deepseek-v4.1-flash": {"name": "DeepSeek V4.1 Flash", "limit": {"context": 1000000, "output": 384000}},
                    "qwen3.7-max": {"name": "Qwen3.7 Max", "limit": {"context": 1000000, "output": 65536}}
                }
            },
            "anthropic": {
                "name": "Anthropic", "env": ["ANTHROPIC_API_KEY"],
                "models": {
                    "claude-opus-5-5": {"name": "Claude Opus 5.5", "limit": {"context": 1000000}},
                    "no-name": {"limit": {"context": 5}}
                }
            }
        }"#;
        let catalog = Catalog::parse(text).unwrap();
        let info = catalog.provider_info("opencode-go").unwrap();
        assert_eq!(info.name.as_deref(), Some("OpenCode Go"));
        assert_eq!(
            info.api.as_deref(),
            Some("https://opencode.ai/inference/go/v1")
        );
        assert_eq!(info.env, ["OPENCODE_API_KEY"]);
        // `anthropic` has no `api` in the real catalog.
        assert_eq!(catalog.provider_info("anthropic").unwrap().api, None);
        assert_eq!(catalog.provider_info("nope"), None);

        assert_eq!(
            catalog.display_name("opencode-go", "deepseek-v4.1-flash"),
            Some("DeepSeek V4.1 Flash")
        );
        assert_eq!(
            catalog.display_name("anthropic", "claude-opus-5-5"),
            Some("Claude Opus 5.5")
        );
        // A model without a `name`, and an unknown model, are both `None`.
        assert_eq!(catalog.display_name("anthropic", "no-name"), None);
        assert_eq!(catalog.display_name("opencode-go", "nope"), None);

        let models = catalog.models_for("opencode-go");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "deepseek-v4.1-flash");
        assert_eq!(models[0].name.as_deref(), Some("DeepSeek V4.1 Flash"));
        assert_eq!(models[0].limit.context, Some(1_000_000));
        assert!(catalog.models_for("nope").is_empty());
    }

    #[test]
    fn a_non_object_top_level_is_corrupt() {
        assert!(Catalog::parse("[]").is_none());
        assert!(Catalog::parse("not json").is_none());
        assert!(Catalog::parse("").is_none());
    }

    #[test]
    fn a_small_catalog_window_warns_and_an_explicit_one_refuses() {
        // C <= R needs the request's own output to reach C (revision 4): an Anthropic request
        // with max_tokens = 16384 against an 8192 window.
        let explicit = build_window(
            Some(8_192),
            None,
            None,
            Some(16_384),
            WindowSource::Explicit,
            None,
        );
        assert_eq!(explicit.budget, None);
        assert!(
            matches!(explicit.startup_check(), WindowCheck::Refuse(ref m) if m.starts_with("--context-window: ")),
            "{:?}",
            explicit.startup_check()
        );
        let catalog = build_window(
            Some(8_192),
            None,
            None,
            Some(16_384),
            WindowSource::Catalog {
                provider: "p".into(),
                model: "m".into(),
                age: None,
            },
            None,
        );
        assert_eq!(catalog.budget, None);
        assert!(
            matches!(catalog.startup_check(), WindowCheck::Warn(ref m) if !m.contains("--context-window:")),
            "{:?}",
            catalog.startup_check()
        );
        let fine = build_window(
            Some(100_000),
            None,
            None,
            Some(16_384),
            WindowSource::Explicit,
            None,
        );
        assert_eq!(fine.startup_check(), WindowCheck::Ok);
    }

    #[test]
    fn describe_names_the_source() {
        assert_eq!(
            Window {
                budget: Some(1000),
                error: None,
                context: Some(1000),
                input: None,
                output: None,
                reservation: 250,
                source: WindowSource::Explicit,
                limit: None
            }
            .describe(),
            "C 1000, I ?, output ?, R 250, B 1000 from --context-window"
        );
        let catalog = Window {
            budget: Some(922_000),
            error: None,
            context: Some(1_000_000),
            input: Some(922_000),
            output: Some(384_000),
            reservation: 16_384,
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
            "C 1000000, I 922000, output 384000, R 16384, B 922000 from models.dev \
             (opencode/deepseek-v4.1-flash, cached 3 h ago)"
        );
        assert_eq!(
            Window::unknown().describe(),
            "window unknown (C ?, I ?, output ?, R ?, B ?); no budget, only the provider's \
             over-length error remains"
        );
    }
}
