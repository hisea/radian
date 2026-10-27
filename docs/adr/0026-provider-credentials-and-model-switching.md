# ADR 0026: Provider Credentials and Model Switching

Status: accepted

## Decisions

### D0 Provider descriptor

- `ProviderDescriptor` (`radian-host`) holds: `id`, display name, default base URL, login method, per-model online protocol (mapped to `openai-compatible`, `anthropic`, or `openai-responses`) plus that model base URL, `request_headers(session_id)` hook, optional usage source (Endpoint or Headers), and descriptor window table. Adapters never change; the descriptor only chooses which adapter and fills what parameters.
- Built-in descriptors register at compile time: `opencode-go` (auto-adds `x-opencode-session`), `anthropic`, `openai`, `chatgpt` (subscription OAuth), generic `openai-compatible`. No dynamic third-party plugin loading exists.
- **Model list**: descriptor built-in list first, then `~/.radian/cache/providers/<id>.json`, then background revalidation with a key (default `GET <base>/models`; a descriptor can name its own model-list URL; `opencode-go` uses `https://opencode.ai/zen/go/v1/models` because its chat base has no `/models`). **Every provider `/model` would offer** — each usable `api_key` entry and each `[providers.<name>]` with a usable key — refreshes after the session opens, each from the **base URL a switch to it resolves** (the config declaration first, then the entry's `env`, then the built-in descriptor's own), and a built-in descriptor keeps its own model-list URL rule. Opening `/model` also refreshes a listed provider whose cache is missing or stale. Background validation never blocks startup and never blocks `/model`. TTL is 1 hour. Failure retries in 5 minutes. Cache stores only the key fingerprint, never the key. Window caps still come from the models.dev catalog (ADR 0018); both merge by model id.
- **Interface method comes from models.dev `provider.npm`** (entry level first, provider level as fallback): `@ai-sdk/openai-compatible` means OpenAI chat, `@ai-sdk/anthropic` means Anthropic, `@ai-sdk/openai` means Responses. Other values explicitly reject ("radian does not support this model interface (<npm>)"). **Addresses never read from models.dev**: each built-in descriptor hardcodes its own addresses (opencode-go chat and Responses use `https://opencode.ai/inference/go/v1`; Anthropic uses `…/inference/go/anthropic/v1`). Data changes never move hosts.
- **One catalog snapshot ships with the program**: fetch order is fresh cache, then network, then stale cache, then built-in snapshot, then none. The snapshot keeps only radian-used providers and fields. It records the fetch date. A script regenerates it. Parse failure is a build error. Models missing from the snapshot fall back to the descriptor hardcoded table. When catalog and table conflict, the catalog wins, and the startup line says so once.
- **Streamed usage**: the `opencode-go` descriptor asks for streamed usage by default (`stream_options.include_usage`). The choice is made by descriptor, never by a host-name substring. `--include-usage` overrides it in either direction. Every other descriptor keeps the adapter's host default (on for `api.openai.com` only). The adapter still reads a stream whose usage arrives without the option, and the final usage chunk is counted once.
- CLI `--provider` and `--model` stay. `--base-url` and `--header` only work for generic `openai-compatible`. `--api-key-env` is removed.

### D1 Credential file

- Location is `~/.radian/credentials/auth.json`, directory 0700, file 0600. No system keychain exists: credentials live in the directory (Linux bwrap can only hide existing paths).
- **Format exactly matches pi `auth.json`**: flat object, provider id as key, entry types `api_key` or `oauth`. A generic `openai-compatible` base URL lives in the entry `env` under key `RADIAN_BASE_URL`. `RADIAN_KIND` defaults to `openai-compatible`. Pi ignores these two keys. Files radian writes stay pi-readable.
- `key` supports literal values and `$ENV` or `${ENV}`. **`!command` is unsupported** (it would run arbitrary commands outside the sandbox). A key starting with `!` means that provider has no credentials, plus one notice.
- **Several keys per provider**: an `api_key` entry keeps `key` and may add `keys`, a list of further keys in order. Each `keys[]` item has `key` (required; the same `$ENV` / `${ENV}` resolution as `key`; a value starting with `!` is refused exactly like `key`, with one notice naming the item, and the other keys stay usable) and `name` (optional; a missing name shows as the key's masked form `sk-…cond`). The first key's name is `first` when it has none. pi reads `key` alone, so the file stays pi-readable; radian writes keep unknown fields as today. A `keys` list on an `oauth` entry is ignored with one notice.
- **Unknown or unsupported entries stay as-is**: radian writes keep them and only add or change its own entry. Writes are atomic (temp file plus rename), 0600. Reading with permissions wider than 0600 refuses and prints one fix command.
- `config import-pi` never imports pi `auth.json`. Users who want reuse copy the file themselves.

### D2 Keys come only from `auth.json`

- The runtime key **sole** source is `auth.json`. `--api-key-env`, configuration `model.api_key`, and per-provider default environment-variable fallback logic are removed.
- **The key pool**: the host keeps one pool per provider for the process: the keys in file order, and for each a rest-until instant (none at start). A request uses **the first key that is not resting**, and the session keeps using that key until it rests: no rotation and no per-turn change, so the vendor's prompt cache for this session stays on one key. When every key rests, the request uses the key whose rest ends soonest and the error reaches core as usual. The pool is process state only: nothing about keys enters the journal, `ConfigChanged`, the export or the startup line, and a resumed session starts at the first key again (ADR 0010: live, not durable). Failover rides core's existing retry: the adapter's error is inspected on the way back, and the next attempt asks the host for the model again and gets the next key. With `[model] retries = 0` no failover happens inside a turn; the next turn uses the next key.
- **Resting a key**: a model error that is `ModelError::Unavailable` with `status: Some(429)` rests the key the request used: for the `Retry-After` duration when the adapter parsed one (capped at one hour), else five minutes. No other error rests a key: a `5xx` is the vendor's, and a `400`/`401`/`403` is the request's or a dead key, which the user fixes with `/logout`.
- **One notice per switch**: when a request uses a different key than the session's previous request, the host emits one notice line (`opencode-go: key "first" rests until 14:32; using "team"`, local clock time). The key value never appears; the name or the masked form does.
- Scenarios needing environment variables (CI, no key on disk) write `"key": "$MY_KEY"` in the `auth.json` entry.
- `model.api_key` in configuration errors and points at `/login`. It never silently ignores.
- `auth.json` is sandbox-unreadable (ADR 0020). Keys never enter journal, export, panic log, or startup line. Every resolved key enters the redaction list, every `keys[]` item included.

### D3 `/login` varies by provider

- The descriptor declares its supported login methods: API key (input never echoes; generic `openai-compatible` also asks name, base URL, and key, all into that `auth.json` entry); browser OAuth (PKCE plus loopback; remote or headless pastes the final redirect URL); device code (RFC 8628); environment credentials (Bedrock and Vertex) unsupported.
- **It sends one minimal validation request before saving** (`GET <base>/models`; a descriptor can name its own validation request; `opencode-go` uses its usage endpoint `GET https://opencode.ai/zen/go/v1/usage` with the same `Authorization: Bearer` because its chat base has no `/models`). Failure shows the reason, and the user can still save. This is an explicit network item (ADR 0013).
- It writes only `auth.json`, never configuration. `/logout <provider>` deletes that entry and keeps other entries as-is. `radian login` and `radian logout` follow the same steps.
- `/login <provider>` on an entry that already has a key asks `replace the current key, or add another? [r/a]`; `a` then asks a name (empty allowed). The validation request runs with the new key. `radian exec` is non-interactive and does not log in. The `custom` flow is unchanged except that an existing name offers the same choice.
- `/logout <provider>` on an entry with several keys lists them (`1 first (sk-…irst)  2 team (sk-…cond)`) and asks which one, or `all`. Removing the first key promotes the next one to `key` (the file keeps a `key`, so pi keeps reading it). One key behaves as before. `radian logout <provider> <name>` removes that one without asking; an unknown name is refused naming the known ones.

### D4 Sandbox

`~/.radian/credentials/` joins the deny list, and the `workspace` tier denies it. The host creates this directory (0700) before building the sandbox. The escape matrix covers `credentials_unreadable`.

### D5 `/model`

- Selection list: all **logged-in** provider descriptor model lists, grouped by provider. A hand-typed model name is also allowed. The current-route provider still lists even with no usable credentials, so the `✓` row never misses (switch still refuses per D6). `/model <provider>/<model>` completes both levels. The current model marks `✓`. The description column shows the provider. Typing matches both model and provider names.
- On switch one user line states: "next round uses <model>; the first round after switch misses cache".
- After select it pushes into `ConfigQueue` only a route reference `{"model":{"provider":"…","model":"…"}}`, **with no key and no header values**. Per ADR 0004 it takes effect **before the next turn**. The in-progress turn stays unaffected. The status bar shows "next: <model>".
- The host holds one switchable model and takes out (or builds and caches) the matching `HostModel` per request by current route. The driver only swaps the slot between turns.

### D6 Cross-provider switch

- The target provider needs usable credentials in `auth.json`, or switch refuses and points at `/login <provider>`.
- Cross-provider switch is safe under current request construction: both adapters building assistant messages never read `reasoning` (unless the route requires replay). The OpenAI-compatible adapter never reads `provider_payload` and always writes `None`. The Anthropic adapter only replays `thinking` and `redacted_thinking`. Tool-call ids emit as-is; Anthropic only requires a match with `^[a-zA-Z0-9_-]+$`.
- Each adapter request-construction test covers "last round came from another home". The contract harness never extends.

### D7 Switched context budget

- `TurnOptions.context_budget` overrides `AgentConfig.context_budget`. The driver applies `ConfigChanged` and recomputes `B` from the catalog by new model.
- Before switch it first judges `T` (ADR 0015 estimate):
  - `T ≤ B'`: queues the switch directly, with one line.
  - `T > B'`: opens a confirm box "switch to <model> needs compact first: current ≈ T tokens, over new model budget B'. Compact then switch? [y] compact and switch [n] cancel". `y` first compacts **with the current model** (same path as `/compact`), targeting `B'`. On compact success it queues the switch. On compact failure or cancel it never switches. `n` never switches. This is a Modal; `y` and `n` never rebind.
- With auto-compaction off the same applies: this is a user-initiated switch.

### D8 Which model resume uses

- Default is the journal last `ConfigChanged` model.
- An explicit CLI `--model` or `--provider` differing from the record wins: the CLI writes one new `ConfigChanged` before the first turn.
- The configuration-file default model has lowest priority.
- A restored model with no credentials in `auth.json` errors and points at `/login <provider>`. It never silently swaps to another model.

### D9 Auto (automatic approval)

- **The name is `auto`** (automatic approval), not `yolo`. Behavior matches `--yes`: it only auto-answers `ask` as allow. `deny` rules still reject. The sandbox still applies. The tier never changes.
- Four entries: configuration `[permissions] approval = "auto"`; `--yes` (`--approval auto` shorthand); approval-screen **`!`** (approves this once and auto-approves after; `Y` stays equal to `y`); commands `/auto on` and `/auto off` (`/yolo` is an alias).
- On and off each write one `ApprovalPolicyChanged` (non-critical) plus one `ConfigChanged { change: {"approval": "auto" | "on-ask"} }`.
- **`full` tier**: configuration `approval = "auto"` is ignored with a warning. `full` only enables auto in-session (`/auto on`, `/yolo`, or `!`). `--yes` under `full` counts as an explicit CLI choice and works as usual.
- **Resume**: `workspace` restores the last state. Under the `full` tier **resume never restores auto**, and the startup line says "auto was on in the previous run; run `/auto on` to re-enable". An explicit CLI `--approval` or `--yes` wins and backfills `ConfigChanged`.

### D10 Named providers

```toml
[providers.<name>]
kind = "openai-compatible"      # only openai-compatible and anthropic allowed
base_url = "…"
reasoning_replay = "reasoning_content"   # optional, overrides catalog
```

- `<name>` never clashes with built-in descriptor ids and is never `openai-compatible`. Rules: letters, digits, `-`, `_`.
- `auth.json` stores the key under `<name>`. Multiple custom endpoints each hold their own key. Key lookup uses the **named provider name**.
- Selection: `--provider <name>`, configuration `model.provider = "<name>"`, `/model <name>/<model>`. The model list comes from `GET <its base URL>/models` with its key — the declaration's or the entry's own `base_url`, never a built-in descriptor's — refreshed in the background and cached for 1 hour like every other provider (D0), plus hand-typed names.
- `/login` "custom" writes name, protocol, base URL, and key all into that `auth.json` entry. Hand-written configuration also works. With a same-name entry in both places, **configuration wins** with one warning.
- Named and custom providers **never read the catalog `npm`**. They follow the user-declared `kind`. A custom provider never chooses Responses.

### D11 Unified thinking levels

- **Seven levels**: `off | minimal | low | medium | high | xhigh | max`. Entries: `--thinking <level>`, configuration `model.thinking`, runtime `/thinking <level>`. The footer `reasoning` entry shows the **actual effective** level.
- **Default is `medium`**.
- **Mapping has two layers** (per the pi `thinkingLevelMap`, where `null` means unsupported): model layer (per-model table in the descriptor) then provider-level default table. With neither, it sends no parameter.
- Online parameters: OpenAI-compatible sends `reasoning_effort`. Anthropic maps to `adaptive` or `enabled:<budget>`. It never invents a value a provider never knows.
- **Unsupported levels never show**: `/thinking` completion and selection lists show only current-model supported levels. A hand-typed unsupported level **clamps up to the nearest supported level**. With none above, it clamps down to the nearest, with one line (for example `medium → high (this model supports low/high/max)`). Default `medium` clamps the same way. The startup line and footer show the clamped actual level.
- **Anthropic per-level budgets**: `off` sends none. `minimal` 1024, `low` 2048, `medium` (default) `adaptive`, `high` 4096, `xhigh` 8192, `max` 12288 (all `enabled:<budget>`), under the `1024 <= budget < max_tokens` bound. Old `enabled:<budget>` spelling stays accepted for one version and sends the user budget as-is.
- Timing matches `/model`: next-turn effect, one `ConfigChanged { change: {"thinking": "<level>"} }`, restore at resume. An explicit CLI `--thinking` wins.

### D12 ChatGPT subscription login and Responses adapter

- **Provider `chatgpt`** (display name "ChatGPT (subscription)"): credentials are the `auth.json` `oauth` entry. Requests use the Responses adapter to `https://chatgpt.com/backend-api/codex/responses`, with `chatgpt-account-id`, `OpenAI-Beta: responses=experimental`, and `originator: radian` (never impersonates Codex CLI).
- **Keeps `openai` (API key)**: it still uses Chat Completions.
- **Login**: `/login` adds "Sign in with ChatGPT": browser plus PKCE plus fixed callback `http://localhost:1455/auth/callback` (an occupied port errors explicitly and points at device code). Device code is the second entry. The token writes the `auth.json` `oauth` entry (0600). It **never enters logs, journal, or export** and enters the redaction list. Before expiry it auto-refreshes with the refresh token. Refresh failure prompts re-login. `account_id` decodes from the access token each time and never stores separately.
- **Responses adapter** (`agent-provider-openai-responses`): streaming text, tool calls, reasoning (with `encrypted_content` replay, per ADR 0014), usage, error classes (`is_transient`, `retry_after`, `ContextOverflow`); `store: false`. It must pass the provider contract. Concepts a provider lacks declare absence per `crates/agent-provider-contract/README.md`.
- **Responses works with an API key**: built-in-descriptor models through Responses use that provider key in `auth.json` as `Authorization: Bearer`, without the three ChatGPT and Codex headers and with no token refresh. The `chatgpt` descriptor OAuth path never changes. Custom providers still never choose Responses.
- **Usage**: `GET https://chatgpt.com/backend-api/wham/usage` (same OAuth headers) returns `rate_limit.primary_window` and `secondary_window` plus `plan_type`. Response headers `x-codex-{primary,secondary}-*` also stay as a Headers source.

### D13 `RADIAN_EXTRA_BODY`

- `RADIAN_EXTRA_BODY` is an input of both hosts (`radian`, `radian exec`): a JSON object read once at startup. Its top-level fields merge into every model request body through the adapter's existing `extra_body` slot — OpenAI-compatible chat, Responses, and Anthropic — on the startup route and on every later `/model` route. A `subagent` child inherits the process environment, so it reads the same value itself; there is no separate plumbing.
- The value must parse as a JSON object. Invalid JSON or a non-object stops startup with one error line naming the variable, before any session file exists; the process exits as it does for another startup configuration error (2). Every adapter keeps its own reserved list — chat `model`, `messages`, `tools`, `stream`, `stream_options`; Responses `model`, `input`, `instructions`, `tools`, `stream`, `store`, `include`; Anthropic `model`, `messages`, `max_tokens`, `stream`, `system`, `tools`, `tool_choice` — and reserved keys are checked **per route**, never as a union. Startup refuses only a key the startup route's adapter reserves, with one error line naming the variable and the key. A `/model` switch to a route whose adapter reserves a key the value sets is refused with the same line, and the switch does not happen: the session stays on its current model and route. The key is never dropped silently. A `subagent` child inherits the process environment and checks the value against its own route at its own startup; no separate plumbing exists.
- The merge order stays the adapter's existing one: `extra_body` first, then the request's own settings, so a per-request setting keeps its value. Startup prints one warning line naming every extra-body key that the startup route's compaction settings also set.
- Startup prints one line naming the keys applied, never their values. Unset, an empty value, and `{}` change nothing: no line and no field.

## Constraints and Costs

- **Switching models misses prompt cache**: prompt caches split by model (and provider). The first round after switch bills full input and runs slower. No mechanism offsets this; only one line in the `/model` hint states it. Cross-provider switch fully misses. Compaction rewrites the context prefix, so the first request after compaction never cache-hits.
- ChatGPT subscription login is not officially open third-party access (it uses the public Codex OAuth client).
- Background `/v1/models` refresh and `/login` validation are explicit network items (ADR 0013). Failures stay silent and produce no URL-bearing error text.

## Verification

`radian-host` auth, provider, and model tests cover `auth.json` shape and unknown-entry preservation, the `keys` list (order, names, masking, `$ENV`, a refused `!command` item, an ignored oauth `keys`), the key pool (first key not resting, stickiness, the 429 rest durations, the soonest-free key when all rest, no rest for a `5xx`/`401`, the adapter's own error reaching core, one switch notice, the usage fetch's current key and title), `!command` rejection, over-wide-permission rejection, keys in no output, descriptor parsing, catalog `npm` interface choice, hardcoded addresses, built-in snapshot fallback, Responses plus API-key headers, OAuth refresh, ChatGPT usage, `/model` switch and window recompute, resume rules, `auto` four entries and `full` rules, and thinking-level clamp and mapping. `RADIAN_EXTRA_BODY` tests cover the field reaching each of the three request bodies, the request settings winning with one warning, the refusal of a non-object, the refusal of a key the startup route reserves, the acceptance of a key only another route reserves (chat `max_tokens`), and the refusal of a `/model` switch to a route that reserves a key the value sets. All three provider contracts (openai-compatible, anthropic, openai-responses) stay green. The sandbox matrix covers `credentials_unreadable`.
