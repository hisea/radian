#!/usr/bin/env python3
"""Regenerate the bundled models.dev snapshot (ADR 0026).

The full models.dev catalog is ~5.7 MB; radian compiles in a **trimmed** copy so that
startup never depends on models.dev being reachable. This script fetches `CATALOG_URL`
and keeps only:

* the providers radian has a built-in descriptor for (``opencode-go``, ``anthropic``,
  ``openai``) plus the ones ``catalog::provider_for_host`` maps a base URL to
  (``opencode``, ``deepseek``, ``openrouter``, ``moonshotai-cn``);
* per model, only the fields radian reads: ``limit``, ``cost``, ``interleaved`` and the
  entry-level ``provider`` object (its ``npm`` decides the wire protocol);
* per provider, only the default ``npm`` (the entry-level fallback).

Nothing else is kept — in particular **not** ``provider.api``: radian never trusts the
catalog's base URL (ADR 0026).

Run by hand before a release (never from a test, never from the built binary):

    python3 crates/radian-host/scripts/update-catalog-snapshot.py \\
        crates/radian-host/src/catalog-snapshot.json

``CATALOG_URL`` overrides the source (default https://models.dev/api.json).
"""

import json
import os
import sys
import urllib.request

KNOWN_PROVIDERS = [
    "opencode-go",
    "opencode",
    "anthropic",
    "openai",
    "deepseek",
    "openrouter",
    "moonshotai-cn",
]

MODEL_FIELDS = ["limit", "cost", "interleaved"]


def trim(catalog):
    out = {}
    for provider_id in KNOWN_PROVIDERS:
        provider = catalog.get(provider_id)
        if not isinstance(provider, dict):
            continue
        models = provider.get("models")
        if not isinstance(models, dict):
            continue
        kept = {}
        for model_id, model in sorted(models.items()):
            if not isinstance(model, dict) or not isinstance(model.get("limit"), dict):
                # radian keeps only models with a `limit` (catalog.rs); a model without one is
                # invisible to it, so carrying it would only grow the binary.
                continue
            entry = {}
            for field in MODEL_FIELDS:
                if field in model:
                    entry[field] = model[field]
            provider_field = model.get("provider")
            if isinstance(provider_field, dict) and "npm" in provider_field:
                entry["provider"] = {"npm": provider_field["npm"]}
            kept[model_id] = entry
        if not kept:
            continue
        provider_out = {"models": kept}
        if isinstance(provider.get("npm"), str):
            provider_out["npm"] = provider["npm"]
        out[provider_id] = provider_out
    return out


def main():
    url = os.environ.get("CATALOG_URL", "https://models.dev/api.json")
    out_path = sys.argv[1] if len(sys.argv) > 1 else "catalog-snapshot.json"
    # models.dev answers a plain urllib request with 403; a named client is accepted.
    request = urllib.request.Request(url, headers={"User-Agent": "radian-catalog-snapshot"})
    with urllib.request.urlopen(request, timeout=30) as response:
        catalog = json.load(response)
    trimmed = {"_source": url, "_date": "2026-10-17"}
    trimmed.update(trim(catalog))
    text = json.dumps(trimmed, indent=1, sort_keys=True, separators=(",", ": "))
    with open(out_path, "w") as handle:
        handle.write(text)
        handle.write("\n")
    print(f"{out_path}: {len(text)} bytes, {len(trimmed) - 2} providers")


if __name__ == "__main__":
    main()
