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
Every other entry stays untouched.

Choose the provider with `--provider`.
Use `openai-compatible` for the default wire format.
Use `anthropic` for the Anthropic message format.
Choose the endpoint with `--base-url`.
Omit `--base-url` to use the provider public endpoint.
Choose the model with `--model <name>`.
Use a bare model name.
Never use `<provider>/<model>` on the command line.
Use `/model <provider>/<model>` inside the full screen interface.
Set `model.provider`, `model.base_url`, and `model.model` in the config file for defaults.

Pass `--max-tokens <n>` for Anthropic requests.
The default is 16384.
This flag has no effect on `openai-compatible` requests.
Pass `--thinking <mode>` for Anthropic extended thinking.
Use `off` to turn it off.
Use `adaptive` to let the provider decide.
Use `enabled:<budget>` to set a numeric budget.
This flag has no effect on `openai-compatible` requests.
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
