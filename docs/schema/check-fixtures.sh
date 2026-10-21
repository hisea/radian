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
    "ProjectTrustChanged", "UserBash",
    "BackgroundJobStarted", "BackgroundJobSettled",
    "GoalSet", "GoalBudgetSet", "GoalProgress", "GoalDone",
    "GoalPaused", "GoalResumed", "GoalDropped", "GoalWait",
    "GoalBudgetExhausted",
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
    "inbox-wake.jsonl": dict(
        outcome="accept",
        events=3,
        skips=[],
        # ADR 0011：`wake` 是可选字段。(seq, wake)——一条带它、一条旧记录没有。
        inbox_wake=[(1, True), (2, None)],
        repair={},
    ),
    "inbox-degraded.jsonl": dict(
        outcome="accept",
        events=3,
        skips=[],
        # 唤醒预算的降级注入：`degraded` 是可选字段。(seq, degraded)——一条降级记
        # 录带 `true`，一条旧记录两个字段都没有（按 `false` 读）。
        inbox_degraded=[(1, True), (2, None)],
        repair={},
    ),
    "inbox-pending-wake.jsonl": dict(
        outcome="accept",
        events=3,
        skips=[],
        # 回合收尾的待决唤醒（ADR 0011）：`pending_wake` 是可选字段。
        # (seq, pending_wake)——一条待决记录带 `true`，一条旧记录没有该字段
        # （按 `false` 读）。
        inbox_pending_wake=[(1, True), (2, None)],
        repair={},
    ),
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
    "goal.jsonl": dict(
        outcome="accept",
        events=18,
        skips=[],
        goal=[
            (1, "GoalSet"),
            (10, "GoalProgress"),
            (11, "GoalBudgetSet"),
            (12, "GoalWait"),
            (13, "GoalPaused"),
            (14, "GoalResumed"),
            (15, "GoalDone"),
            (16, "GoalSet"),
            (17, "GoalBudgetExhausted"),
        ],
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
    "policy-approval.jsonl": dict(
        outcome="accept",
        events=4,
        skips=[],
        repair={},
        policy_resolved=(3, "policy", "safe-commands"),
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
    "context-files-imports.jsonl": dict(
        outcome="accept",
        events=1,
        skips=[],
        # (文件路径, [(导入路径, 字节数)]) —— ADR 0036 C2：快照列出每份文件的导入。
        context_imports=[
            (
                "/work/example/CLAUDE.md",
                [("/work/example/docs/conventions.md", 3481)],
            ),
        ],
        repair={},
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
                # ADR 0030 O2: `relation` is optional; an old `parent` without one reads as
                # "fork". When present it must be a non-empty string.
                if "relation" in parent and (
                    not isinstance(parent["relation"], str) or not parent["relation"]
                ):
                    raise Corruption(f"{name}: line {i}: parent.relation 必须是非空字符串")
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
        if ev["event"] == "PermissionResolved":
            # ADR 0006 §5.9：`policy` 只在 approver=policy 时出现（critical 的取值由 Rust
            # 侧的作者/读取方测试钉住，这里不重复：旧的 unknown-enum-values fixture 由另一
            # 个切片负责，本片不改它）。
            data = ev["data"]
            if "policy" in data:
                if not isinstance(data["policy"], str) or not data["policy"]:
                    raise Corruption(
                        f"{name}: line {i}: PermissionResolved.policy 必须是非空字符串"
                    )
                if data.get("approver") != "policy":
                    raise Corruption(
                        f"{name}: line {i}: 只有 approver=policy 的事件才能带 policy"
                    )
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
        expected = exp["parent"]
        want = {"session_id": expected[0], "seq": expected[1]}
        if len(expected) == 3:
            want["relation"] = expected[2]
        if created.get("parent") != want:
            raise Corruption(f"{name}: SessionCreated.parent {created.get('parent')!r} != 期望 {want}")
    if "forked" in exp:
        forked = next(e["data"] for e in events if e["event"] == "SessionForked")
        sid, seq = exp["forked"]
        if forked.get("child_session_id") != sid or forked.get("parent_seq") != seq:
            raise Corruption(f"{name}: SessionForked {forked!r} != 期望")
    if "context_imports" in exp:
        # ADR 0036 C2：`config_snapshot.context.files[]` 的导入是可选的加法字段——
        # 有导入的文件带 `imports: [{path, bytes}]`，没有的文件没有该键。
        created = next(e["data"] for e in events if e["event"] == "SessionCreated")
        files = created["config_snapshot"]["context"]["files"]
        for f in files:
            if not isinstance(f.get("path"), str) or not f["path"]:
                raise Corruption(f"{name}: context file 的 path 必须是非空字符串")
            if not isinstance(f.get("bytes"), int) or isinstance(f.get("bytes"), bool):
                raise Corruption(f"{name}: context file 的 bytes 不是整数")
            if not isinstance(f.get("truncated"), bool):
                raise Corruption(f"{name}: context file 的 truncated 不是布尔值")
            for imp in f.get("imports", []):
                if set(imp) != {"path", "bytes"}:
                    raise Corruption(f"{name}: imports 条目字段必须是 path/bytes：{imp!r}")
                if not isinstance(imp["path"], str) or not imp["path"]:
                    raise Corruption(f"{name}: imports.path 必须是非空字符串")
                if not isinstance(imp["bytes"], int) or isinstance(imp["bytes"], bool):
                    raise Corruption(f"{name}: imports.bytes 不是整数")
        got = [
            (f["path"], [(imp["path"], imp["bytes"]) for imp in f.get("imports", [])])
            for f in files
        ]
        if got != [tuple(x) for x in exp["context_imports"]]:
            raise Corruption(f"{name}: context_imports {got} != 期望 {exp['context_imports']}")
        if not any(imports for _, imports in got):
            raise Corruption(f"{name}: 期望至少一份文件带 imports")
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
    if exp.get("deterministic"):
        # ADR 0018 决定 3：确定性兜底的 ContextCompacted 带 deterministic:true，
        # 且不写 CompactionUsageRecorded（没有模型调用）。
        compacted = [e for e in events if e["event"] == "ContextCompacted"]
        if len(compacted) != 1 or compacted[0]["data"].get("deterministic") is not True:
            raise Corruption(f"{name}: 期望一条 deterministic:true 的 ContextCompacted")
        if any(e["event"] == "CompactionUsageRecorded" for e in events):
            raise Corruption(f"{name}: 确定性兜底不得写 CompactionUsageRecorded")
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
    if "inbox_wake" in exp:
        # ADR 0011 / ADR 0006 §5.5：`wake` 是 InboxEnqueued 的**可选**字段，缺省即
        # 旧记录。这里逐条钉住带字段与缺字段两种形状，并检查类型。
        got = []
        for e in events:
            if e["event"] != "InboxEnqueued":
                continue
            data = e["data"]
            if "wake" in data and not isinstance(data["wake"], bool):
                raise Corruption(f"{name}: seq {e['seq']}: InboxEnqueued.wake 不是布尔值")
            got.append((e["seq"], data.get("wake")))
        if got != [tuple(x) for x in exp["inbox_wake"]]:
            raise Corruption(f"{name}: inbox_wake {got} != 期望 {exp['inbox_wake']}")
        if not any(wake is None for _, wake in got):
            raise Corruption(f"{name}: 期望至少一条旧记录（没有 wake 字段）")
        if not any(isinstance(wake, bool) for _, wake in got):
            raise Corruption(f"{name}: 期望至少一条带布尔 wake 的记录")
    if "inbox_degraded" in exp:
        # 唤醒预算的降级注入（ADR 0006 §5.5）：`degraded` 是 InboxEnqueued 的**可选**
        # 字段，缺省即旧记录（不是降级）。这里逐条钉住带字段与缺字段两种形状，并检查类型。
        got = []
        for e in events:
            if e["event"] != "InboxEnqueued":
                continue
            data = e["data"]
            if "degraded" in data and not isinstance(data["degraded"], bool):
                raise Corruption(f"{name}: seq {e['seq']}: InboxEnqueued.degraded 不是布尔值")
            got.append((e["seq"], data.get("degraded")))
        if got != [tuple(x) for x in exp["inbox_degraded"]]:
            raise Corruption(f"{name}: inbox_degraded {got} != 期望 {exp['inbox_degraded']}")
        if not any(degraded is None for _, degraded in got):
            raise Corruption(f"{name}: 期望至少一条旧记录（没有 degraded 字段）")
        if not any(degraded is True for _, degraded in got):
            raise Corruption(f"{name}: 期望至少一条带 degraded:true 的降级记录")
    if "inbox_pending_wake" in exp:
        # 回合收尾的待决唤醒（ADR 0011 / ADR 0006 §5.5）：`pending_wake` 是
        # InboxEnqueued 的**可选**字段，缺省即旧记录（没有待决唤醒）。这里逐条钉住
        # 带字段与缺字段两种形状，并检查类型。
        got = []
        for e in events:
            if e["event"] != "InboxEnqueued":
                continue
            data = e["data"]
            if "pending_wake" in data and not isinstance(data["pending_wake"], bool):
                raise Corruption(f"{name}: seq {e['seq']}: InboxEnqueued.pending_wake 不是布尔值")
            got.append((e["seq"], data.get("pending_wake")))
        if got != [tuple(x) for x in exp["inbox_pending_wake"]]:
            raise Corruption(f"{name}: inbox_pending_wake {got} != 期望 {exp['inbox_pending_wake']}")
        if not any(pending is None for _, pending in got):
            raise Corruption(f"{name}: 期望至少一条旧记录（没有 pending_wake 字段）")
        if not any(pending is True for _, pending in got):
            raise Corruption(f"{name}: 期望至少一条带 pending_wake:true 的待决记录")
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
    if "policy_resolved" in exp:
        resolved = [
            (e["seq"], e["data"].get("approver"), e["data"].get("policy"))
            for e in events
            if e["event"] == "PermissionResolved"
        ]
        if resolved != [tuple(exp["policy_resolved"])]:
            raise Corruption(
                f"{name}: policy_resolved {resolved} != 期望 {[tuple(exp['policy_resolved'])]}"
            )
        # ADR 0006 §4：审批审计是 non-critical。
        for e in events:
            if e["event"] == "PermissionResolved" and e["critical"]:
                raise Corruption(f"{name}: PermissionResolved 必须是 critical:false")
    if "goal" in exp:
        # ADR 0047：每个 `Goal*` 事件都是非 critical 审计（持久目标从它们 fold 出来，
        # replay/repair 永远看不见）。形状：Set/BudgetSet 带可选的三元上限，
        # Progress 带 note，Done 带可选 note，Wait 带 secs，BudgetExhausted 带 kind；
        # Paused/Resumed/Dropped 的 data 是 `{}`。
        got = [(e["seq"], e["event"]) for e in events if e["event"].startswith("Goal")]
        if got != [tuple(x) for x in exp["goal"]]:
            raise Corruption(f"{name}: goal events {got} != 期望 {exp['goal']}")
        for e in events:
            if not e["event"].startswith("Goal"):
                continue
            if e["critical"]:
                raise Corruption(f"{name}: {e['event']} 必须是 critical:false")
            data = e["data"]
            if e["event"] in ("GoalSet", "GoalBudgetSet"):
                if e["event"] == "GoalSet" and (
                    not isinstance(data.get("objective"), str) or not data["objective"]
                ):
                    raise Corruption(f"{name}: seq {e['seq']}: GoalSet.objective 必须是非空字符串")
                if e["event"] == "GoalBudgetSet" and "objective" in data:
                    raise Corruption(f"{name}: seq {e['seq']}: GoalBudgetSet 不能带 objective")
                for field in ("max_turns", "max_duration_secs", "max_tokens"):
                    value = data.get(field)
                    if value is not None and (
                        not isinstance(value, int) or isinstance(value, bool) or value < 0
                    ):
                        raise Corruption(f"{name}: seq {e['seq']}: {field} 不是非负整数")
            elif e["event"] == "GoalProgress":
                if not isinstance(data.get("note"), str) or not data["note"]:
                    raise Corruption(f"{name}: seq {e['seq']}: GoalProgress.note 必须是非空字符串")
            elif e["event"] == "GoalDone":
                note = data.get("note")
                if note is not None and not isinstance(note, str):
                    raise Corruption(f"{name}: seq {e['seq']}: GoalDone.note 不是字符串")
            elif e["event"] in ("GoalPaused", "GoalResumed", "GoalDropped"):
                if data != {}:
                    raise Corruption(f"{name}: seq {e['seq']}: {e['event']} 的 data 必须是空对象")
            elif e["event"] == "GoalWait":
                secs = data.get("secs")
                if not isinstance(secs, int) or isinstance(secs, bool) or secs <= 0:
                    raise Corruption(f"{name}: seq {e['seq']}: GoalWait.secs 不是正整数")
            elif e["event"] == "GoalBudgetExhausted":
                if data.get("kind") not in ("turns", "duration", "tokens"):
                    raise Corruption(f"{name}: seq {e['seq']}: GoalBudgetExhausted.kind 非法")
            else:
                raise Corruption(f"{name}: seq {e['seq']}: 未知的 Goal 事件 {e['event']}")
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


# `radian exec --output jsonl` 的 closing `Result` 记录（ADR 0044 §2）：它不是 journal 行，
# 没有信封七字段，单独校验。`exec-result.jsonl` 恰好两行，以 LF 结尾：一行无 resume
#（`run_*` 与累计组字节相同），一行有 resume（`run_*` 只是本次运行）。
RESULT_FIELDS = {
    "type", "session_id", "outcome", "exit_code", "stop_reason",
    "turns", "usage", "cost", "run_turns", "run_usage", "run_cost", "text",
}
RESULT_OUTCOMES = {
    "completed", "refused", "budget_exceeded", "failed",
    "poisoned", "incomplete", "cancelled", "unknown",
}
RESULT_STOP_REASONS = {
    "completed", "failed", "budget", "deadline", "max_tokens",
    "content_filtered", "tool_use_without_calls", "refused", "cancelled", "terminated",
}
RESULT_USAGE_KEYS = {"input", "output", "cache_read", "cache_write"}


def check_one_result(ev, label):
    if not isinstance(ev, dict) or set(ev) != RESULT_FIELDS:
        raise Corruption(
            f"{label}: 字段集合不是 Result 的十二个字段: "
            f"{sorted(ev) if isinstance(ev, dict) else type(ev)}"
        )
    if ev["type"] != "Result":
        raise Corruption(f"{label}: type {ev['type']!r} != 'Result'")
    if not isinstance(ev["session_id"], str) or not ev["session_id"]:
        raise Corruption(f"{label}: session_id 必须是非空字符串")
    if ev["outcome"] not in RESULT_OUTCOMES:
        raise Corruption(f"{label}: outcome {ev['outcome']!r} 不在 ADR 0044 §2 的集合")
    if not isinstance(ev["exit_code"], int) or isinstance(ev["exit_code"], bool):
        raise Corruption(f"{label}: exit_code 不是整数")
    reason = ev["stop_reason"]
    if not isinstance(reason, str) or not (
        reason in RESULT_STOP_REASONS
        or (reason.startswith("unknown:") and len(reason) > len("unknown:"))
    ):
        raise Corruption(f"{label}: stop_reason {reason!r} 不在 ADR 0044 §2 的集合")
    if (
        not isinstance(ev["turns"], int)
        or isinstance(ev["turns"], bool)
        or ev["turns"] < 0
    ):
        raise Corruption(f"{label}: turns 不是非负整数")
    if (
        not isinstance(ev["run_turns"], int)
        or isinstance(ev["run_turns"], bool)
        or ev["run_turns"] < 0
    ):
        raise Corruption(f"{label}: run_turns 不是非负整数")
    if ev["run_turns"] > ev["turns"]:
        raise Corruption(f"{label}: run_turns {ev['run_turns']} > turns {ev['turns']}")
    for key in ("usage", "run_usage"):
        usage = ev[key]
        if not isinstance(usage, dict):
            raise Corruption(f"{label}: {key} 不是对象")
        unknown = set(usage) - RESULT_USAGE_KEYS
        if unknown:
            raise Corruption(f"{label}: {key} 有未知字段 {sorted(unknown)}")
        for field, value in usage.items():
            if not isinstance(value, int) or isinstance(value, bool) or value < 0:
                raise Corruption(f"{label}: {key}.{field} 不是非负整数")
    for key in ("cost", "run_cost"):
        cost = ev[key]
        if cost is not None:
            if not isinstance(cost, dict) or set(cost) != {"micros", "incomplete", "estimated"}:
                raise Corruption(f"{label}: {key} 不是 null 也不是完整的费用对象")
            if (
                not isinstance(cost["micros"], int)
                or isinstance(cost["micros"], bool)
                or cost["micros"] < 0
            ):
                raise Corruption(f"{label}: {key}.micros 不是非负整数")
            for field in ("incomplete", "estimated"):
                if not isinstance(cost[field], bool):
                    raise Corruption(f"{label}: {key}.{field} 不是布尔值")
    if not isinstance(ev["text"], str):
        raise Corruption(f"{label}: text 不是字符串")
    return ev


def check_result_fixture(name):
    raw = open(f"fixtures/{name}", "rb").read()
    if not raw.endswith(b"\n"):
        raise Corruption(f"{name}: 文件不以 LF 结尾")
    lines = [l for l in raw.split(b"\n") if l != b""]
    if len(lines) != 2:
        raise Corruption(f"{name}: 期望恰好两行 Result 记录（无 resume + 有 resume），得到 {len(lines)} 行")
    try:
        first = json.loads(lines[0])
        second = json.loads(lines[1])
    except json.JSONDecodeError as e:
        raise Corruption(f"{name}: JSON 解析失败: {e}") from None
    check_one_result(first, f"{name} line 1")
    check_one_result(second, f"{name} line 2")
    # 第一行无 resume：两组字节相同（`run_*` 与累计组相等）。
    if not (
        first["run_turns"] == first["turns"]
        and first["run_usage"] == first["usage"]
        and first["run_cost"] == first["cost"]
    ):
        raise Corruption(f"{name} line 1: 无 resume 时 run_* 必须与累计组相同")
    # 第二行有 resume：`run_*` 只是本次运行，小于累计。
    if not (second["run_turns"] < second["turns"]):
        raise Corruption(f"{name} line 2: 有 resume 时 run_turns 必须小于 turns")
    for field in RESULT_USAGE_KEYS:
        if field in second["run_usage"] and field in second["usage"]:
            if not second["run_usage"][field] <= second["usage"][field]:
                raise Corruption(f"{name} line 2: run_usage.{field} 必须 <= usage.{field}")
    if second["cost"] is not None and second["run_cost"] is not None:
        if not second["run_cost"]["micros"] < second["cost"]["micros"]:
            raise Corruption(f"{name} line 2: run_cost.micros 必须小于 cost.micros")
        if second["cost"].get("estimated") is not True:
            raise Corruption(f"{name} line 2: 有 resume 且含旧记录时 cost.estimated 必须为 true")
        if second["run_cost"].get("estimated") is not False:
            raise Corruption(f"{name} line 2: run_cost.estimated 必须为 false（本轮费用是现算的）")
    return f"PASS {name}: Result 记录形状正确（无 resume + 有 resume 各一行）"


RESULT_FIXTURES = ["exec-result.jsonl"]


failures = 0
for name in sorted(EXPECT):
    try:
        print(check_fixture(name, EXPECT[name]))
    except Corruption as e:
        print(f"FAIL {e}")
        failures += 1
for name in RESULT_FIXTURES:
    try:
        print(check_result_fixture(name))
    except Corruption as e:
        print(f"FAIL {e}")
        failures += 1
if failures:
    print(f"\n{failures} 个 fixture 不符合期望")
    sys.exit(1)
print("\n全部 fixture 符合 README 期望")
PY
