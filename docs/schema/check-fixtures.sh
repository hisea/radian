#!/usr/bin/env bash
# 校验 docs/schema/fixtures/ 下每个 fixture 是否符合 fixtures/README.md 声明的期望。
# 只用 python3 -I 与标准库。任何一个 fixture 不符合期望即退出非 0。
set -euo pipefail
cd "$(dirname "$0")"
python3 -I - <<'PY'
import json
import re
import sys

SUPPORTED = 1
REQUIRED = ["schema_version", "session_id", "seq", "ts", "event", "critical", "data"]
KNOWN_EVENTS = {
    "SessionCreated", "InboxEnqueued", "InputClaimed", "InputAdmitted",
    "TurnStarted", "MessageAppended", "ToolExecutionStarted", "ToolExecutionSettled",
    "TranscriptReplaced", "TurnFinished", "ConfigChanged", "ContextCompacted",
    "Rewound", "PermissionResolved", "ModelUsageRecorded", "SessionForked",
}
RFC3339_UTC = re.compile(
    r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z$"
)

# 每个 fixture 的期望（与 fixtures/README.md 一一对应）。
#   outcome: accept | accept_with_skips | policy_reject | readonly_or_refuse | accept_drop_tail
EXPECT = {
    "v1-minimal.jsonl": dict(outcome="accept", events=9, skips=[], repair={}),
    "v1-forward.jsonl": dict(
        outcome="accept_with_skips",
        events=6,
        accepted=5,
        skips=[(3, "HeartbeatRecorded")],
        reserved=["agent_id", "profile_ref", "workspace_binding", "parent"],
        repair={},
    ),
    "unknown-optional-field.jsonl": dict(outcome="accept", events=4, skips=[], repair={}),
    "unknown-event-noncritical.jsonl": dict(
        outcome="accept_with_skips", events=5, accepted=4,
        skips=[(1, "SpinProgressRecorded")], repair={},
    ),
    "unknown-event-critical.jsonl": dict(
        outcome="policy_reject",
        events=2,
        reject_at=(2, "LedgerCommitted"),  # (1 起的行号, 事件名)
    ),
    "schema-version-newer.jsonl": dict(outcome="readonly_or_refuse", events=2, version=2),
    "truncated-tail.jsonl": dict(outcome="accept_drop_tail", events=5, tail_bytes_nonempty=True, repair={}),
    "crash-after-started.jsonl": dict(
        outcome="accept",
        events=8,
        skips=[],
        repair={"call-2": "outcome_unknown_after_crash"},
    ),
}


class Corruption(Exception):
    pass


def check_envelope(ev, lineno, version, session_id):
    for f in REQUIRED:
        if f not in ev:
            raise Corruption(f"line {lineno}: missing envelope field {f}")
    if not isinstance(ev["schema_version"], int) or isinstance(ev["schema_version"], bool):
        raise Corruption(f"line {lineno}: schema_version not int")
    if not isinstance(ev["session_id"], str):
        raise Corruption(f"line {lineno}: session_id not string")
    if not isinstance(ev["seq"], int) or isinstance(ev["seq"], bool):
        raise Corruption(f"line {lineno}: seq not int")
    if not isinstance(ev["ts"], str) or not RFC3339_UTC.match(ev["ts"]):
        raise Corruption(f"line {lineno}: ts not RFC3339 UTC: {ev['ts']!r}")
    if not isinstance(ev["event"], str):
        raise Corruption(f"line {lineno}: event not string")
    if not isinstance(ev["critical"], bool):
        raise Corruption(f"line {lineno}: critical not bool")
    if not isinstance(ev["data"], dict):
        raise Corruption(f"line {lineno}: data not object")
    if ev["schema_version"] != version:
        raise Corruption(f"line {lineno}: schema_version changed mid-file")
    if ev["session_id"] != session_id:
        raise Corruption(f"line {lineno}: session_id changed mid-file")


def simulate_repair(events):
    """ADR 0002 的 repair：返回 {call_id: error_code}（需要补合成结果的调用）。"""
    declared, answered, started, settled = [], set(), set(), set()
    for ev in events:
        if ev["event"] == "MessageAppended":
            msg = ev["data"]["message"]
            if "assistant" in msg:
                declared += [c["id"] for c in msg["assistant"].get("tool_calls", [])]
            elif "tool" in msg:
                answered.add(msg["tool"]["result"]["call_id"])
        elif ev["event"] == "ToolExecutionStarted":
            started.add(ev["data"]["call_id"])
        elif ev["event"] == "ToolExecutionSettled":
            settled.add(ev["data"]["call_id"])
    out = {}
    for call in declared:
        if call in answered:
            continue
        out[call] = "outcome_unknown_after_crash" if call in started else "not_started_after_crash"
    return out


def check_fixture(name, exp):
    raw = open(f"fixtures/{name}", "rb").read()
    ends_with_lf = raw.endswith(b"\n")
    allow_tail = exp["outcome"] == "accept_drop_tail"
    if not ends_with_lf and not allow_tail:
        raise Corruption(f"{name}: 文件不以 LF 结尾（仅 truncated-tail 允许）")
    if ends_with_lf and allow_tail and exp.get("tail_bytes_nonempty"):
        raise Corruption(f"{name}: 期望存在 torn tail，但文件以 LF 结尾")

    body = raw if ends_with_lf else raw[: raw.rfind(b"\n") + 1]
    tail = raw[len(body):]
    if allow_tail and exp.get("tail_bytes_nonempty") and not tail:
        raise Corruption(f"{name}: 期望非空 torn tail，实际为空")

    lines = [l for l in body.split(b"\n") if l != b""]
    if len(lines) != exp["events"]:
        raise Corruption(f"{name}: 完整行数 {len(lines)} != 期望 {exp['events']}")

    version, session_id = None, None
    expect_seq = 0
    events, skips, index_next = [], [], 0
    for i, line in enumerate(lines, start=1):
        try:
            ev = json.loads(line)
        except json.JSONDecodeError as e:
            raise Corruption(f"{name}: line {i}: JSON 解析失败: {e}") from None
        if version is None:
            version = ev.get("schema_version")
            session_id = ev.get("session_id")
        check_envelope(ev, i, version, session_id)
        if ev["seq"] != expect_seq:
            raise Corruption(f"{name}: line {i}: 期望 seq {expect_seq}，得到 {ev['seq']}")
        expect_seq += 1
        # v1 读取方策略
        if ev["event"] not in KNOWN_EVENTS:
            if exp["outcome"] == "policy_reject":
                at, name_ev = exp["reject_at"]
                if i == at and ev["event"] == name_ev:
                    if not ev["critical"]:
                        raise Corruption(f"{name}: line {i}: 期望 critical:true 的未知事件")
                    return f"PASS {name}: 拒绝打开（line {i}, {ev['event']}, critical）"
                raise Corruption(f"{name}: line {i}: 未知的拒绝点")
            if ev["critical"]:
                raise Corruption(f"{name}: line {i}: 未知 critical 事件未被策略拒绝")
            skips.append((ev["seq"], ev["event"]))
            continue
        if ev["event"] == "SessionCreated" and ev["seq"] != 0:
            raise Corruption(f"{name}: line {i}: SessionCreated 不在 seq 0")
        if ev["event"] == "MessageAppended":
            idx = ev["data"].get("index")
            if idx != index_next:
                raise Corruption(
                    f"{name}: line {i}: MessageAppended.index {idx} != transcript 长度 {index_next}"
                )
            index_next += 1
        events.append(ev)

    outcome = exp["outcome"]
    if outcome == "policy_reject":
        raise Corruption(f"{name}: 期望拒绝打开，但未出现声明的 critical 未知事件")
    if outcome == "readonly_or_refuse":
        if version != exp.get("version"):
            raise Corruption(f"{name}: schema_version {version} != 期望 {exp['version']}")
        if version <= SUPPORTED:
            raise Corruption(f"{name}: 期望高于支持版本")
        # 追加模式必须拒绝：策略检查
        return f"PASS {name}: schema v{version} > v{SUPPORTED}，只读/拒绝打开，禁止追加写入"
    if skips != exp.get("skips", []):
        raise Corruption(f"{name}: skips {skips} != 期望 {exp.get('skips', [])}")
    if outcome == "accept_with_skips":
        got = len(events)
        if got != exp.get("accepted"):
            raise Corruption(f"{name}: 接受事件数 {got} != 期望 {exp['accepted']}")
    if "reserved" in exp:
        created = next(e["data"] for e in events if e["event"] == "SessionCreated")
        for field in exp["reserved"]:
            if field not in created:
                raise Corruption(f"{name}: SessionCreated 缺预留字段 {field}")
    repair = simulate_repair(events)
    if repair != exp.get("repair", {}):
        raise Corruption(f"{name}: repair {repair} != 期望 {exp.get('repair', {})}")
    if outcome == "accept_drop_tail":
        return f"PASS {name}: 接受，丢弃 torn tail {len(tail)} 字节，replay {len(events)} 个事件"
    if outcome == "accept_with_skips":
        return f"PASS {name}: 接受 {len(events)} 个事件，跳过 {len(skips)} 个（{[s[1] for s in skips]}）"
    return f"PASS {name}: 接受 {len(events)} 个事件，0 跳过，repair={repair or '∅'}"


failures = 0
for name in sorted(EXPECT):
    try:
        print(check_fixture(name, EXPECT[name]))
    except Corruption as e:
        print(f"FAIL {e}")
        failures += 1
if failures:
    print(f"\n{failures} 个 fixture 不符合期望")
    sys.exit(1)
print("\n全部 fixture 符合 README 期望")
PY
