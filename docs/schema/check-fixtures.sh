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
    "TranscriptReplaced", "TranscriptReplacementCommitted", "TranscriptReplacementAborted",
    "TurnFinished", "ConfigChanged", "ContextCompacted",
    "Rewound", "ContextBudgetReached", "PermissionResolved", "ApprovalPolicyChanged",
    "MemoryInjected", "ModelUsageRecorded", "CompactionUsageRecorded", "SessionForked",
    "ProjectTrustChanged", "UserBash", "GitCommitted",
    "BackgroundJobStarted", "BackgroundJobSettled",
}
# ADR 0005/0006 §5.5：InputSource 的 snake_case 全集。
SOURCES = {
    "user", "slash_command", "permission_decision", "scheduler",
    "mcp", "subagent", "system",
}
META_FIELDS = {"id", "source", "origin"}
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
    "unknown-enum-values.jsonl": dict(outcome="accept", events=6, skips=[], repair={}),
    "ask-user.jsonl": dict(outcome="accept", events=8, skips=[], repair={}),
    "memory-injected.jsonl": dict(
        outcome="accept",
        events=6,
        skips=[],
        memory_injected=[(3, "mem-1", "global"), (1, "mem-2", "prj-1")],
        repair={},
    ),
    "approval-policy-changed.jsonl": dict(
        outcome="accept",
        events=7,
        skips=[],
        approval_changed=(6, "auto"),
        repair={},
    ),
    "project-trust-changed.jsonl": dict(
        outcome="accept",
        events=6,
        skips=[],
        trust_changed=(5, "/work/example", True),
        repair={},
    ),
    "user-bash.jsonl": dict(
        outcome="accept",
        events=7,
        skips=[],
        user_bash=[(5, "echo hi", False, 0), (6, "false", True, 1)],
        repair={},
    ),
    "context-files.jsonl": dict(
        outcome="accept",
        events=1,
        skips=[],
        repair={},
    ),
    "git-committed.jsonl": dict(
        outcome="accept",
        events=6,
        skips=[],
        git_committed=(5, "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678", "feature/x", 3),
        repair={},
    ),
    "background-jobs.jsonl": dict(
        outcome="accept",
        events=7,
        skips=[],
        background_jobs={
            "starts": [
                (3, "job-1", "cargo test --workspace", ".radian/jobs/job-1.out"),
                (4, "job-2", "sleep 600", ".radian/jobs/job-2.out"),
            ],
            "settles": [(5, "job-1", "exited 0", 1200)],
        },
        repair={},
    ),
    "replace-committed.jsonl": dict(
        outcome="accept",
        events=6,
        skips=[],
        replacement=(2, 1, 2),
        uncommitted=[],
        repair={},
    ),
    "replace-uncommitted-eof.jsonl": dict(
        outcome="accept",
        events=4,
        skips=[],
        uncommitted=[(1, 2, 1, "end_of_file")],
        repair={},
    ),
    "replace-aborted-then-append.jsonl": dict(
        outcome="accept",
        events=6,
        skips=[],
        replacement=(3, 5, 3),
        uncommitted=[(5, 3, 1, "aborted")],
        repair={},
    ),
    "replace-pending-other-event.jsonl": dict(
        outcome="pending_event_reject",
        events=3,
    ),
    "replace-commit-mismatch.jsonl": dict(
        outcome="replacement_mismatch_reject",
        events=4,
    ),
    "compaction-committed.jsonl": dict(
        outcome="accept",
        events=9,
        skips=[],
        replacement=(6, 1, 2),
        uncommitted=[],
        repair={},
    ),
    "compaction-verbatim.jsonl": dict(
        outcome="accept",
        events=9,
        skips=[],
        replacement=(6, 1, 2),
        uncommitted=[],
        repair={},
    ),
    "compaction-candidate-only.jsonl": dict(
        outcome="accept",
        events=4,
        skips=[],
        uncommitted=[],
        repair={},
    ),
    "fork-parent.jsonl": dict(
        outcome="accept",
        events=6,
        skips=[],
        repair={},
        forked=("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", 4),
    ),
    "fork-child.jsonl": dict(
        outcome="accept",
        events=3,
        skips=[],
        repair={},
        parent=("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", 4),
    ),
    "usage-cache-semantics.jsonl": dict(
        outcome="accept",
        events=10,
        skips=[],
        # (event, input_includes_cache) —— ADR 0007：标记是可选的，旧记录没有它。
        usage_cache_marks=[
            ("ModelUsageRecorded", True),
            ("CompactionUsageRecorded", False),
            ("ModelUsageRecorded", None),
        ],
        repair={},
    ),
    "usage-fee-recorded.jsonl": dict(
        outcome="accept",
        events=9,
        skips=[],
        # (event, fee) —— ADR 0027：费用是可选字段，缺省即旧记录（恢复时估算）。
        usage_fees=[
            ("ModelUsageRecorded", {"micros": 3270}),
            ("ModelUsageRecorded", {"micros": 1000, "incomplete": True}),
            ("CompactionUsageRecorded", {"micros": 450}),
            ("ModelUsageRecorded", None),
        ],
        repair={},
    ),
    "replace-resets-index.jsonl": dict(
        outcome="accept",
        events=11,
        skips=[],
        # (1 起的行号, revision, len) —— len 必须等于其后的 MessageAppended 条数。
        replacement=(5, 1, 2),
        meta_present=True,
        interrupted_present=True,
        usage_present=True,
        repair={"call-9": "outcome_unknown_after_crash"},
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


def new_fold():
    return {
        "history": [],
        "pending": None,
        "settled": set(),
        "started": set(),
        "declared": set(),
        "uncommitted": [],
    }


def fold_push(st, ev, lineno, name):
    """ADR 0019 §2 的替换状态机（独立实现，不调用 Rust）。"""
    e = ev["event"]
    d = ev["data"]
    if e == "MessageAppended":
        msg = d["message"]
        if st["pending"] is not None:
            p = st["pending"]
            if d.get("index") != len(p["messages"]):
                raise Corruption(
                    f"{name}: line {lineno}: replacement message index {d.get('index')} "
                    f"!= {len(p['messages'])}"
                )
            if len(p["messages"]) >= p["declared_len"]:
                raise Corruption(f"{name}: line {lineno}: message beyond the declared len")
            p["messages"].append(msg)
        else:
            if d.get("index") != len(st["history"]):
                raise Corruption(
                    f"{name}: line {lineno}: MessageAppended.index {d.get('index')} "
                    f"!= transcript 长度 {len(st['history'])}"
                )
            if "assistant" in msg:
                for c in msg["assistant"].get("tool_calls") or []:
                    st["declared"].add(c.get("id"))
            st["history"].append(msg)
    elif e == "TranscriptReplaced":
        if st["pending"] is not None:
            raise Corruption(
                f"{name}: line {lineno}: 待定段内又出现 TranscriptReplaced（ADR 0019 §2）"
            )
        st["pending"] = {"revision": d["revision"], "declared_len": d["len"], "messages": []}
    elif e == "TranscriptReplacementCommitted":
        p = st["pending"]
        if p is None:
            raise Corruption(f"{name}: line {lineno}: 没有待定段时的 commit（ADR 0019 §2）")
        if (
            p["revision"] != d["revision"]
            or p["declared_len"] != d["len"]
            or len(p["messages"]) != d["len"]
        ):
            raise Corruption(f"{name}: line {lineno}: commit 与待定段不匹配")
        st["history"] = p["messages"]
        st["pending"] = None
        st["settled"].clear()
        st["started"].clear()
        st["declared"] = set()
        for msg in st["history"]:
            if "assistant" in msg:
                for c in msg["assistant"].get("tool_calls") or []:
                    st["declared"].add(c.get("id"))
    elif e == "TranscriptReplacementAborted":
        p = st["pending"]
        if p is None:
            raise Corruption(f"{name}: line {lineno}: 没有待定段时的 abort（ADR 0019 §2）")
        if p["revision"] != d["revision"]:
            raise Corruption(f"{name}: line {lineno}: abort 的 revision 与待定段不匹配")
        st["uncommitted"].append(
            (p["revision"], p["declared_len"], len(p["messages"]), "aborted")
        )
        st["pending"] = None
    elif e in ("ToolExecutionStarted", "ToolExecutionSettled"):
        if st["pending"] is not None:
            raise Corruption(f"{name}: line {lineno}: {e} 出现在待定段里（ADR 0019 §2）")
        cid = d.get("call_id")
        if cid not in st["declared"]:
            raise Corruption(
                f"{name}: line {lineno}: {e} 的 call_id {cid!r} "
                "未出现在更早的 assistant 消息中（ADR 0006 §7）"
            )
        (st["started"] if e == "ToolExecutionStarted" else st["settled"]).add(cid)
    else:
        # ModelUsageRecorded 与所有 runtime/未知事件：待定段内一律损坏。
        if st["pending"] is not None:
            raise Corruption(f"{name}: line {lineno}: {e} 出现在待定段里（ADR 0019 §2）")


def simulate_repair(events):
    """ADR 0002 的 repair，基于 ADR 0019 的 fold：commit 才换 history、清旧标记。"""
    st = new_fold()
    for lineno, ev in enumerate(events, start=1):
        fold_push(st, ev, lineno, "repair")
    declared, answered = [], set()
    for msg in st["history"]:
        if "assistant" in msg:
            declared += [c["id"] for c in msg["assistant"].get("tool_calls", [])]
        elif "tool" in msg:
            answered.add(msg["tool"]["result"]["call_id"])
    out = {}
    for call in declared:
        if call in answered:
            continue
        out[call] = "outcome_unknown_after_crash" if call in st["started"] else "not_started_after_crash"
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
    events, skips = [], []
    replacements = []
    fold = new_fold()
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
        if ev["event"] == "SessionCreated":
            if ev["seq"] != 0:
                raise Corruption(f"{name}: line {i}: SessionCreated 不在 seq 0")
            parent = ev["data"].get("parent")
            if parent is not None:
                if not isinstance(parent, dict):
                    raise Corruption(f"{name}: line {i}: SessionCreated.parent 不是对象")
                if not isinstance(parent.get("session_id"), str) or not parent["session_id"]:
                    raise Corruption(f"{name}: line {i}: parent.session_id 必须是非空字符串")
                # `seq` is ADR 0022's addition; the older reserved shape has no `seq`, which
                # a forward reader still ignores. If it is present, it must be an integer.
                if "seq" in parent and (
                    not isinstance(parent["seq"], int) or isinstance(parent["seq"], bool)
                ):
                    raise Corruption(f"{name}: line {i}: parent.seq 不是整数")
        if ev["event"] == "SessionForked":
            data = ev["data"]
            if not isinstance(data.get("child_session_id"), str) or not data["child_session_id"]:
                raise Corruption(
                    f"{name}: line {i}: SessionForked.child_session_id 必须是非空字符串"
                )
            if not isinstance(data.get("parent_seq"), int) or isinstance(data["parent_seq"], bool):
                raise Corruption(f"{name}: line {i}: SessionForked.parent_seq 不是整数")
        if ev["event"] == "MessageAppended":
            msg = ev["data"].get("message")
            if not isinstance(msg, dict) or len(msg) != 1:
                raise Corruption(f"{name}: line {i}: message 必须是单变体对象")
            body = next(iter(msg.values()))
            meta = body.get("meta")
            if meta is not None:
                if not isinstance(meta, dict):
                    raise Corruption(f"{name}: line {i}: meta 不是对象")
                unknown = set(meta) - META_FIELDS
                if unknown:
                    raise Corruption(f"{name}: line {i}: meta 未知字段 {sorted(unknown)}")
                if meta.get("id") == "" or ("id" in meta and not isinstance(meta["id"], str)):
                    raise Corruption(f"{name}: line {i}: meta.id 必须是非空字符串")
                if "source" in meta and meta["source"] not in SOURCES:
                    raise Corruption(
                        f"{name}: line {i}: meta.source {meta['source']!r} 不在 InputSource 集合"
                    )
        if ev["event"] in ("TranscriptReplaced", "TranscriptReplacementCommitted"):
            data = ev["data"]
            for field in ("revision", "len"):
                if not isinstance(data.get(field), int) or isinstance(data.get(field), bool):
                    raise Corruption(f"{name}: line {i}: {ev['event']}.{field} 不是整数")
        if ev["event"] == "TranscriptReplacementAborted":
            rev = ev["data"].get("revision")
            if not isinstance(rev, int) or isinstance(rev, bool):
                raise Corruption(f"{name}: line {i}: TranscriptReplacementAborted.revision 不是整数")
        if ev["event"] == "MemoryInjected":
            # ADR 0021 D6：只带 id/revision/scope，不带内容；critical 必须为 false。
            if ev["critical"]:
                raise Corruption(f"{name}: line {i}: MemoryInjected 必须是 critical:false")
            records = ev["data"].get("records")
            if not isinstance(records, list) or not records:
                raise Corruption(f"{name}: line {i}: MemoryInjected.records 必须是非空数组")
            for rec in records:
                if set(rec) != {"id", "revision", "scope"}:
                    raise Corruption(
                        f"{name}: line {i}: MemoryInjected 记录字段必须是 id/revision/scope"
                    )
                if not isinstance(rec["id"], str) or not isinstance(rec["revision"], int):
                    raise Corruption(f"{name}: line {i}: MemoryInjected 记录类型错误")
                scope = rec["scope"]
                if not (
                    scope == "global"
                    or (isinstance(scope, dict) and isinstance(scope.get("project"), str))
                ):
                    raise Corruption(f"{name}: line {i}: MemoryInjected.scope 形状错误")
        # ADR 0019 §2：唯一的替换状态机（本文件独立实现，不调用 Rust）。
        try:
            fold_push(fold, ev, i, name)
        except Corruption:
            if exp["outcome"] in ("pending_event_reject", "replacement_mismatch_reject"):
                return f"PASS {name}: 拒绝打开（line {i}, {ev['event']}）"
            raise
        if ev["event"] == "TranscriptReplaced":
            replacements.append((i, ev["data"]["revision"], ev["data"]["len"]))
        events.append(ev)

    if exp["outcome"] in ("pending_event_reject", "replacement_mismatch_reject"):
        raise Corruption(f"{name}: 期望拒绝打开，但文件被接受了")
    if fold["pending"] is not None:
        fold["uncommitted"].append(
            (
                fold["pending"]["revision"],
                fold["pending"]["declared_len"],
                len(fold["pending"]["messages"]),
                "end_of_file",
            )
        )

    # 替换段不完整（文件到此为止，或被下一个 marker 取代）→ ADR 0005 把已写入前缀
    # 当作可恢复状态，读取方接受，len 不导致拒绝。

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
    if "parent" in exp:
        created = next(e["data"] for e in events if e["event"] == "SessionCreated")
        sid, seq = exp["parent"]
        if created.get("parent") != {"session_id": sid, "seq": seq}:
            raise Corruption(f"{name}: SessionCreated.parent {created.get('parent')!r} != 期望")
    if "forked" in exp:
        forked = next(e["data"] for e in events if e["event"] == "SessionForked")
        sid, seq = exp["forked"]
        if forked.get("child_session_id") != sid or forked.get("parent_seq") != seq:
            raise Corruption(f"{name}: SessionForked {forked!r} != 期望")
    repair = simulate_repair(events)
    if repair != exp.get("repair", {}):
        raise Corruption(f"{name}: repair {repair} != 期望 {exp.get('repair', {})}")
    if "uncommitted" in exp:
        got_uncommitted = [tuple(x) for x in fold["uncommitted"]]
        if got_uncommitted != [tuple(x) for x in exp["uncommitted"]]:
            raise Corruption(
                f"{name}: uncommitted {got_uncommitted} != 期望 {exp['uncommitted']}"
            )
    if "replacement" in exp and replacements != [exp["replacement"]]:
        raise Corruption(f"{name}: replacements {replacements} != 期望 {[exp['replacement']]}")
    if exp.get("meta_present") and not any(
        e["event"] == "MessageAppended" and "meta" in next(iter(e["data"]["message"].values()))
        for e in events
    ):
        raise Corruption(f"{name}: 期望至少一条消息带 meta")
    if exp.get("interrupted_present") and not any(
        e["event"] == "MessageAppended"
        and e["data"]["message"].get("assistant", {}).get("interrupted") is True
        for e in events
    ):
        raise Corruption(f"{name}: 期望至少一条 interrupted assistant")
    if exp.get("usage_present") and not any(
        e["event"] == "ModelUsageRecorded" and "usage" in e["data"] for e in events
    ):
        raise Corruption(f"{name}: 期望带 usage 的 ModelUsageRecorded")
    if "usage_cache_marks" in exp:
        # ADR 0007：`input_includes_cache` 是 usage 对象里的**可选**字段——适配器必须写，
        # 但旧记录没有它（`None` = 未标记），读取方照旧。这里逐条钉住两种标记与缺席，并把
        # usage 的其余字段按「只出现报过的字段」检查（§5.10；`{}` 也合法）。
        optional_usage = {"input", "output", "total", "cache_read", "cache_write", "details",
                          "input_includes_cache"}
        got = []
        for e in events:
            if e["event"] not in ("ModelUsageRecorded", "CompactionUsageRecorded"):
                continue
            usage = e["data"]["usage"]
            if not isinstance(usage, dict):
                raise Corruption(f"{name}: {e['event']} 的 usage 不是对象")
            unknown = set(usage) - optional_usage
            if unknown:
                raise Corruption(f"{name}: {e['event']} 的 usage 有未知字段 {sorted(unknown)}")
            for field in ("input", "output", "total", "cache_read", "cache_write"):
                value = usage.get(field)
                if value is not None and (
                    not isinstance(value, int) or isinstance(value, bool) or value < 0
                ):
                    raise Corruption(f"{name}: {e['event']}.usage.{field} 不是非负整数")
            mark = usage.get("input_includes_cache")
            if mark is not None and not isinstance(mark, bool):
                raise Corruption(f"{name}: {e['event']}.usage.input_includes_cache 不是布尔值")
            if e["critical"]:
                raise Corruption(f"{name}: {e['event']} 必须是 critical:false")
            got.append((e["event"], mark))
        if got != [tuple(x) for x in exp["usage_cache_marks"]]:
            raise Corruption(
                f"{name}: usage_cache_marks {got} != 期望 {exp['usage_cache_marks']}"
            )
        if not any(mark is None for _, mark in got):
            raise Corruption(f"{name}: 期望至少一条未标记（旧）usage 记录")
        if not any(isinstance(mark, bool) for _, mark in got):
            raise Corruption(f"{name}: 期望至少一条带布尔标记的 usage 记录")
    if "usage_fees" in exp:
        # ADR 0027 / ADR 0007：`fee` 是 usage 记录里**可选**的兄弟字段
        # （micro-dollars 与 incomplete），缺省即旧记录（恢复方按当前目录重算并标估算）。
        got = []
        for e in events:
            if e["event"] not in ("ModelUsageRecorded", "CompactionUsageRecorded"):
                continue
            if e["critical"]:
                raise Corruption(f"{name}: {e['event']} 必须是 critical:false")
            fee = e["data"].get("fee")
            if fee is not None:
                if not isinstance(fee, dict) or set(fee) - {"micros", "incomplete"}:
                    raise Corruption(f"{name}: {e['event']} 的 fee 形状错误 {fee!r}")
                if "micros" not in fee or not isinstance(fee["micros"], int) or isinstance(
                    fee["micros"], bool
                ) or fee["micros"] < 0:
                    raise Corruption(f"{name}: {e['event']}.fee.micros 不是非负整数")
                if "incomplete" in fee and not isinstance(fee["incomplete"], bool):
                    raise Corruption(f"{name}: {e['event']}.fee.incomplete 不是布尔值")
            got.append((e["event"], fee))
        if got != [(ev, fee) for ev, fee in exp["usage_fees"]]:
            raise Corruption(f"{name}: usage_fees {got} != 期望 {exp['usage_fees']}")
        if not any(fee is None for _, fee in got):
            raise Corruption(f"{name}: 期望至少一条没有 fee 的旧记录")
        if not any(fee is not None and fee.get("incomplete") for _, fee in got):
            raise Corruption(f"{name}: 期望至少一条标不完整的 fee")
    if "memory_injected" in exp:
        got = []
        for e in events:
            if e["event"] != "MemoryInjected":
                continue
            for rec in e["data"]["records"]:
                scope = rec["scope"]
                if isinstance(scope, dict):
                    scope = scope["project"]
                got.append((rec["revision"], rec["id"], scope))
        if got != [tuple(x) for x in exp["memory_injected"]]:
            raise Corruption(
                f"{name}: memory_injected {got} != 期望 {exp['memory_injected']}"
            )
    if "approval_changed" in exp:
        changed = [
            (e["seq"], e["data"].get("policy"))
            for e in events
            if e["event"] == "ApprovalPolicyChanged"
        ]
        if changed != [exp["approval_changed"]]:
            raise Corruption(f"{name}: approval_changed {changed} != 期望 {[exp['approval_changed']]}")
        # ADR 0026 D9：开关审计是非 critical 事件。
        for e in events:
            if e["event"] == "ApprovalPolicyChanged" and e["critical"]:
                raise Corruption(f"{name}: ApprovalPolicyChanged 必须是 critical:false")
    if "trust_changed" in exp:
        changed = [
            (e["seq"], e["data"].get("path"), e["data"].get("decision"))
            for e in events
            if e["event"] == "ProjectTrustChanged"
        ]
        if changed != [tuple(exp["trust_changed"])]:
            raise Corruption(
                f"{name}: trust_changed {changed} != 期望 {[tuple(exp['trust_changed'])]}"
            )
        # ADR 0031 T2：信任审计是非 critical 事件。
        for e in events:
            if e["event"] == "ProjectTrustChanged" and e["critical"]:
                raise Corruption(f"{name}: ProjectTrustChanged 必须是 critical:false")
    if "user_bash" in exp:
        runs = [
            (
                e["seq"],
                e["data"].get("command"),
                e["data"].get("exclude_from_context"),
                e["data"].get("exit_code"),
            )
            for e in events
            if e["event"] == "UserBash"
        ]
        if runs != [tuple(run) for run in exp["user_bash"]]:
            raise Corruption(f"{name}: user_bash {runs} != 期望 {exp['user_bash']}")
        # ADR 0033：`!` 与 `!!` 的执行审计都是非 critical 事件。
        for e in events:
            if e["event"] == "UserBash" and e["critical"]:
                raise Corruption(f"{name}: UserBash 必须是 critical:false")
    if "git_committed" in exp:
        runs = [
            (
                e["seq"],
                e["data"].get("commit"),
                e["data"].get("branch"),
                e["data"].get("files"),
            )
            for e in events
            if e["event"] == "GitCommitted"
        ]
        if runs != [tuple(exp["git_committed"])]:
            raise Corruption(f"{name}: git_committed {runs} != 期望 {exp['git_committed']}")
        # ADR 0030 R10：提交审计是非 critical 事件。
        for e in events:
            if e["event"] == "GitCommitted" and e["critical"]:
                raise Corruption(f"{name}: GitCommitted 必须是 critical:false")
    if "background_jobs" in exp:
        starts = [
            (
                e["seq"],
                e["data"].get("job_id"),
                e["data"].get("command"),
                e["data"].get("output"),
            )
            for e in events
            if e["event"] == "BackgroundJobStarted"
        ]
        settles = [
            (
                e["seq"],
                e["data"].get("job_id"),
                e["data"].get("state"),
                e["data"].get("duration_ms"),
            )
            for e in events
            if e["event"] == "BackgroundJobSettled"
        ]
        if starts != [tuple(x) for x in exp["background_jobs"]["starts"]]:
            raise Corruption(f"{name}: background job starts {starts} != 期望")
        if settles != [tuple(x) for x in exp["background_jobs"]["settles"]]:
            raise Corruption(f"{name}: background job settles {settles} != 期望")
        # ADR 0037 决定 5：启动/结束记录是非 critical 审计。
        for e in events:
            if e["event"] in ("BackgroundJobStarted", "BackgroundJobSettled") and e["critical"]:
                raise Corruption(f"{name}: {e['event']} 必须是 critical:false")
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
