---
description: Store keys, choose a provider and model, and control request options.
---

# Providers and login

Run `radian login` to store a key.
Pick a built in provider from the numbered list.
Pass `custom` to name a new endpoint.
The key is never echoed.
Radian verifies the key with a read only request to `<base>/models` first.
Keys live in `~/.radian/credentials/auth.json`.
Run `radian logout <provider>` to remove one key.
Removal asks first.
With several keys it asks which one, or `all`; `radian logout <provider> <name>` removes that one without asking.
Every other entry stays untouched.

An entry may hold several keys.
Keep the first as `key` and list the rest in `keys`:

```json
"opencode-go": {"type": "api_key", "key": "sk-first",
                "keys": [{"name": "team", "key": "sk-second"}]}
```

Each `keys` item has `key` and an optional `name`.
`key` and every `keys[].key` resolve `$NAME` or `${NAME}` the same way.
A value starting with `!` is refused and never run; the other keys stay usable.
The first key is named `first` when it has no name; an unnamed `keys` item shows as its masked form `sk-…cond`.
pi reads `key` alone, so the file stays pi-readable.
A `keys` list on an `oauth` entry is ignored, with one notice.
A request uses the first key that is not resting.
The session keeps that key until it rests: no rotation, so the provider's prompt cache stays on one key for this session.
An HTTP 429 rests the key the request used: for the `Retry-After` duration when the provider sent one (at most one hour), else five minutes.
No other error rests a key: a 5xx is the vendor's, and a 400, 401 or 403 is the request's or a dead key.
When every key rests, the request uses the key whose rest ends soonest.
One notice names a switch: `opencode-go: key "first" rests until 14:32; using "team"`.
The notice goes to stderr, under `radian exec --output jsonl` too, so the machine-readable stdout stream stays JSON lines.
With `[model] retries = 0` no failover happens inside a turn; the next turn uses the next key.
The pool is process state only: a resumed session starts at the first key again.
Every key is redacted from the journal, the export and the panic log.
The usage row's title names the key in use (`Usage · team`) when the entry has several keys.

Choose the provider with `--provider`: a built-in id (`opencode-go`, `chatgpt`, `anthropic`, `openai`, `openai-compatible`) or a `[providers.<name>]` name.
An unknown provider is refused.
Omit it to use `openai-compatible`.
Choose the endpoint with `--base-url`.
Omit `--base-url` to use the provider public endpoint.
A custom `--base-url` or `--header` is refused for every non-generic provider.
Choose the model with `--model <name>`.
Use a bare model name.
Never use `<provider>/<model>` on the command line.
Use `/model <provider>/<model>` inside the full screen interface.
`/model` lists a provider's models from `GET <its base URL>/models` with its key.
A named provider uses its own `base_url`.
The answer is cached under `~/.radian/cache/providers/<name>.json` for 1 hour.
Radian refreshes the list in the background after a session starts, and when `/model` opens with a missing or stale list.
A failed fetch stays silent and retries after 5 minutes.
The model-list fetch never sends the key to a plain `http` address that is not this machine; such a list is not fetched.
Set `model.provider`, `model.base_url`, and `model.model` in the config file for defaults.

Pass `--max-tokens <n>` for Anthropic requests.
The default is 16384.
This flag is refused for `openai-compatible` requests.

An OpenAI-compatible chat request sets its own output cap when the models catalog knows both numbers for the model: `max_tokens = min(O, ⌊C/4⌋)`, where `C` is the context window and `O` is the output limit.
When either number is unknown, Radian sends no `max_tokens`.
An explicit `max_tokens` in an extra request body or a per-request setting wins.
Set `RADIAN_EXTRA_BODY` to merge top-level fields into every request body; see `environment` for the reserved keys, the startup line and the refusal.
A route to `api.openai.com` sends none, because its newer chat models reject `max_tokens`.
The same cap is the output reservation that the context budget subtracts from `C`.

`opencode-go` asks the server to include usage in the stream by default.
This is a property of that provider descriptor, not of its host name.
Pass `--include-usage false` to turn it off.
A route to `api.openai.com` also asks by default; every other route does not.
Pass `--include-usage` to turn it on.

Pass `--thinking <level>` to set the thinking level.
Use one of `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`.
Omit it to use `medium`.
Radian clamps the level to what the route supports.
The older `adaptive` and `enabled:<budget>` spellings still work with a warning.
On `openai-compatible` routes the level becomes the `reasoning_effort` request value.
Use `/thinking <level>` inside the full screen interface to set the level.

Pass `--header NAME=VALUE` to add one request header.
Repeat the flag to add more headers.
Radian refuses `Authorization` here.
The key comes only from the credential store.
A value may hold `{session_id}`.
Radian expands it to the current session id.

Radian looks up the context window from the models catalog.
The catalog URL is read only.
The cache lasts 24 hours.
The fetch times out after 3 seconds.
Pass `--context-window <TOKENS>` to set an explicit value.
An explicit value always wins.
Pass `--offline-catalog` to use only the local cache.
Pass `--catalog-provider <id>` to look the model up under another catalog name.
Omit it to derive the name from the base URL host.

Define named endpoints under `[providers.<name>]`.
Set `kind` and `base_url` for each name.
Set `model.provider` to the name to use it.

Read `cli` for every flag and exit code.
Read `configuration` for every file key.
Read `environment` for the key variables and lookup order.
