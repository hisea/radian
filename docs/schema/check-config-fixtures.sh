#!/usr/bin/env bash
# 校验 docs/schema/config/ 下每个 fixture 是否符合 README 与 ADR 0008 声明的期望。
# 只用 python3 -I 与标准库（TOML 用 tomllib）。任何一个 fixture 不符合期望即退出非 0。
set -euo pipefail
cd "$(dirname "$0")"
python3 -I - <<'PY'
import re
import sys
import tomllib

SUPPORTED = 1
SENSITIVE_KEYS = {"api_key", "apikey", "token", "secret", "password", "passwd", "authorization"}
PLAINTEXT_PREFIXES = ("sk-", "sk_", "xoxb-", "ghp_", "AKIA", "Bearer ")
SECRET_FROM = {"env", "keychain", "file"}

# 每个 fixture 的期望（与 config/README.md 一一对应，手工同步）。
#   outcome: accept | readonly_or_refuse | migration_required | reject
EXPECT = {
    "minimal.toml": dict(outcome="accept"),
    "v1-unknown-fields.toml": dict(
        outcome="accept",
        unknown_top=("future_top_level",),
        unknown_nested=(("model", "future_option"),),
    ),
    "schema-version-newer.toml": dict(outcome="readonly_or_refuse", version=99),
    "schema-version-lower.toml": dict(outcome="migration_required", version=0),
    "tui-theme-colors.toml": dict(outcome="accept"),
    "tui-display.toml": dict(outcome="accept"),
    "tui-tips-banner.toml": dict(outcome="accept"),
    "tui-brand.toml": dict(outcome="accept"),
    "tui-spinner.toml": dict(outcome="accept"),
    "tui-input-style.toml": dict(outcome="accept"),
    "tui-footer.toml": dict(outcome="accept"),
    "tui-widgets.toml": dict(outcome="accept"),
    "tui-footer-items.toml": dict(outcome="accept"),
    "tui-footer-command.toml": dict(outcome="accept"),
    "providers.toml": dict(outcome="accept"),
    "compaction-thinking.toml": dict(outcome="accept"),
    "model-retries.toml": dict(outcome="accept"),
    "shell-auto-background.toml": dict(outcome="accept"),
    "permissions-approval.toml": dict(outcome="accept"),
    "skills-extra-paths.toml": dict(outcome="accept"),
    "skills-builtin.toml": dict(outcome="accept"),
    "memory-injection.toml": dict(outcome="accept"),
    "sandbox-unix-sockets.toml": dict(outcome="accept"),
    "integrations-herdr.toml": dict(outcome="accept"),
    "control.toml": dict(outcome="accept"),
    "api-key-removed.toml": dict(outcome="reject"),
    "plaintext-secret.toml": dict(outcome="reject"),
    "heuristic-secret.toml": dict(outcome="reject"),
    "plaintext-plain.toml": dict(outcome="reject"),
}


class ConfigError(Exception):
    pass


def check_no_plaintext(doc):
    """§4.2：敏感键被赋裸字符串、或任何位置出现明显密钥样字符串 → 拒绝。"""
    def walk(key, value, path):
        if isinstance(value, dict):
            for k, v in value.items():
                walk(k, v, f"{path}.{k}" if path else k)
            return
        if isinstance(value, list):
            for i, item in enumerate(value):
                walk("", item, f"{path}[{i}]")
            return
        if key.lower() in SENSITIVE_KEYS and isinstance(value, str):
            raise ConfigError(f"{path}: 敏感键 {key} 的值必须是 SecretRef 表对象，不能是裸字符串")
        if isinstance(value, str) and (
            value.startswith(PLAINTEXT_PREFIXES)
            or (len(value) >= 40 and re.fullmatch(r"[A-Za-z0-9_\-]+", value))
        ):
            raise ConfigError(f"{path}: 疑似明文密钥串（{value[:12]!r}…）")

    for k, v in doc.items():
        walk(k, v, k)


def check_secret_refs(doc):
    """§4.1：敏感键位必须是 from/name 的 SecretRef 表对象。"""
    def walk(d, path):
        for k, v in d.items():
            here = f"{path}.{k}" if path else k
            if k.lower() in SENSITIVE_KEYS:
                if not isinstance(v, dict):
                    raise ConfigError(f"{here}: 必须是 SecretRef 表对象")
                if v.get("from") not in SECRET_FROM:
                    raise ConfigError(f"{here}: SecretRef.from 必须是 env|keychain|file")
                if not isinstance(v.get("name"), str) or not v["name"]:
                    raise ConfigError(f"{here}: SecretRef.name 必须是非空字符串")
                extra = set(v) - {"from", "name"}
                if extra:
                    raise ConfigError(f"{here}: SecretRef 未知字段 {sorted(extra)}")
            elif isinstance(v, dict):
                walk(v, here)
            elif isinstance(v, list):
                for i, item in enumerate(v):
                    if isinstance(item, dict):
                        walk(item, f"{here}[{i}]")

    walk(doc, "")


def check_api_key_removed(doc):
    """ADR 0026 D2：`model.api_key` 一律拒绝，提示改用 auth.json（不静默忽略）。"""
    if "api_key" in doc.get("model", {}):
        raise ConfigError("model.api_key 不再读取；key 只从 ~/.radian/credentials/auth.json 来")


def check_fixture(name, exp):
    with open(f"config/{name}", "rb") as fh:
        try:
            doc = tomllib.load(fh)
        except tomllib.TOMLDecodeError as e:
            raise ConfigError(f"{name}: TOML 解析失败: {e}") from None
    version = doc.get("schema_version")
    if not isinstance(version, int) or isinstance(version, bool):
        raise ConfigError(f"{name}: schema_version 必须是非布尔整数")

    if exp["outcome"] == "readonly_or_refuse":
        want = exp["version"]
        if version != want or version <= SUPPORTED:
            raise ConfigError(f"{name}: schema_version {version} != 期望 {want}（须高于 v{SUPPORTED}）")
        return f"PASS {name}: schema v{version} > v{SUPPORTED}，只读/拒绝，禁止写入"
    if exp["outcome"] == "migration_required":
        want = exp["version"]
        if version != want or version >= SUPPORTED:
            raise ConfigError(f"{name}: schema_version {version} != 期望 {want}（须低于 v{SUPPORTED}）")
        return f"PASS {name}: schema v{version} < v{SUPPORTED}，拒绝（无迁移路径），绝不静默改写"
    if exp["outcome"] == "reject":
        try:
            check_no_plaintext(doc)
            check_secret_refs(doc)
            check_api_key_removed(doc)
        except ConfigError as e:
            return f"PASS {name}: 拒绝（{e}）"
        raise ConfigError(f"{name}: 期望拒绝打开，但没有检测到明文密钥/非法 SecretRef")

    # accept
    if version != SUPPORTED:
        raise ConfigError(f"{name}: schema_version {version} != v{SUPPORTED}")
    check_no_plaintext(doc)
    check_secret_refs(doc)
    for top in exp.get("unknown_top", ()):
        if top not in doc:
            raise ConfigError(f"{name}: 期望含未知顶层字段 {top}")
    for table, key in exp.get("unknown_nested", ()):
        if key not in doc.get(table, {}):
            raise ConfigError(f"{name}: 期望含未知嵌套字段 {table}.{key}")
    check_api_key_removed(doc)
    return f"PASS {name}: 接受（schema v{SUPPORTED}，0 拒绝，仅说明“解释时忽略 + 写回必须保真”）"


failures = 0
for name in sorted(EXPECT):
    try:
        print(check_fixture(name, EXPECT[name]))
    except ConfigError as e:
        print(f"FAIL {e}")
        failures += 1
if failures:
    print(f"\n{failures} 个 config fixture 不符合期望")
    sys.exit(1)
print("\n全部 config fixture 符合 README/ADR 0008 期望")
PY