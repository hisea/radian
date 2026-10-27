"""What each arm of one capture run actually sent to the model.

Reads `<label>/capture/<arm>.jsonl` (written by `capture_addon.py`, one file per
arm) and joins each record to its trial through the `capture.json` files the
capture mixin writes into a trial's agent log dir (`container_ip` there is the
`client_ip` of the record).

    python3 benchmarks/terminal-bench/ab/capture_check.py <label-dir> \\
        [--expect-equal model,reasoning_effort,...]

Per arm it prints the number of requests and, for every captured field, the
distinct values with counts — once for the first request of each trial and once
for all requests. Then a `DIFFERENCES` block for the `--expect-equal` fields
(default `model,reasoning_effort,thinking,max_tokens,stream_options`): every
field whose value set is not the same in every arm. Exit 1 when there is at
least one such field, 0 otherwise. For an `ab-system.sh` label, pass every
field but `system`: the two arms differ exactly in their system prompt.

Exit 2 means the label has no capture to read (no `capture/*.jsonl`).
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from capture import CAPTURE_FILENAME
from capture_addon import LINE_KEYS

DEFAULT_EXPECT_EQUAL = (
    "model",
    "reasoning_effort",
    "thinking",
    "max_tokens",
    "stream_options",
)

# A field a record does not carry at all (as opposed to carrying null).
ABSENT = "<absent>"


def load_records(path: Path) -> list[dict]:
    """Every parsable JSON object in one capture file, in file order."""
    records = []
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return records
    for line in text.splitlines():
        if not line.strip():
            continue
        try:
            record = json.loads(line)
        except ValueError:
            continue
        if isinstance(record, dict):
            records.append(record)
    return records


def trial_map(arm_dir: Path) -> dict[str, str]:
    """`container_ip` -> the trial directory (label-relative) that wrote it."""
    trials: dict[str, str] = {}
    for path in sorted(arm_dir.rglob(CAPTURE_FILENAME)):
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        ip = data.get("container_ip") if isinstance(data, dict) else None
        if not isinstance(ip, str) or not ip:
            continue
        trials.setdefault(ip, path.parent.relative_to(arm_dir.parent).as_posix())
    return trials


def normalize(value) -> str:
    """One comparable text for a captured value (`<absent>` when missing)."""
    if value is ABSENT:
        return ABSENT
    return json.dumps(value, sort_keys=True, ensure_ascii=False)


def distinct(records: list[dict], field: str) -> dict[str, int]:
    """Value -> count for one field, in first-seen order."""
    counts: dict[str, int] = {}
    for record in records:
        key = normalize(record[field]) if field in record else ABSENT
        counts[key] = counts.get(key, 0) + 1
    return counts


def first_of_each_trial(records: list[dict]) -> list[dict]:
    """The first record of every client address, in file order."""
    seen: set[str] = set()
    firsts = []
    for record in records:
        ip = record.get("client_ip") or "<unknown>"
        if ip in seen:
            continue
        seen.add(ip)
        firsts.append(record)
    return firsts


def record_fields(records: list[dict]) -> list[str]:
    """Every captured field except the line's own bookkeeping keys."""
    names = set()
    for record in records:
        names.update(record.keys())
    return sorted(names - set(LINE_KEYS))


def print_values(title: str, records: list[dict], fields: list[str]) -> None:
    print(f"  {title}")
    for field in fields:
        counts = distinct(records, field)
        shown = ", ".join(f"{value} ({count})" for value, count in counts.items())
        print(f"    {field}: {shown}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Report what each A/B arm sent to the model.",
    )
    parser.add_argument("label_dir", help="the job label directory (<label>)")
    parser.add_argument(
        "--expect-equal",
        default=",".join(DEFAULT_EXPECT_EQUAL),
        help="comma-separated fields that must not differ between arms "
        f"(default: {','.join(DEFAULT_EXPECT_EQUAL)})",
    )
    args = parser.parse_args(argv)

    label_dir = Path(args.label_dir)
    capture_dir = label_dir / "capture"
    files = sorted(capture_dir.glob("*.jsonl")) if capture_dir.is_dir() else []
    if not files:
        print(f"{label_dir}: no capture to read ({capture_dir}/*.jsonl)", file=sys.stderr)
        return 2

    expected = [name.strip() for name in args.expect_equal.split(",") if name.strip()]
    arms: dict[str, list[dict]] = {}
    per_arm_values: dict[str, dict[str, dict[str, int]]] = {}
    for path in files:
        arm = path.stem
        records = load_records(path)
        arms[arm] = records
        trials = trial_map(label_dir / arm)
        print(f"arm: {arm}")
        print(f"  capture file: {path}")
        print(f"  requests: {len(records)}")
        print(f"  trials: {len(trials)} ({CAPTURE_FILENAME} files)")
        for ip, trial in trials.items():
            print(f"    {ip} -> {trial}")
        unmatched = sorted(
            {
                record.get("client_ip") or "<unknown>"
                for record in records
                if record.get("client_ip") not in trials
            }
        )
        if unmatched:
            print(f"  client addresses without {CAPTURE_FILENAME}: {', '.join(unmatched)}")
        fields = record_fields(records)
        firsts = first_of_each_trial(records)
        print_values(f"first request of each trial ({len(firsts)})", firsts, fields)
        print_values(f"all requests ({len(records)})", records, fields)
        per_arm_values[arm] = {
            field: distinct(records, field) for field in expected
        }
        print()

    differences = []
    for field in expected:
        sets = {
            arm: set(values.get(field, {}))
            for arm, values in per_arm_values.items()
        }
        non_empty = [values for values in sets.values() if values]
        if any(values != non_empty[0] for values in non_empty):
            differences.append(field)

    print("DIFFERENCES")
    if not differences:
        print(f"  none in: {', '.join(expected) if expected else '(no fields)'}")
        return 0
    for field in differences:
        print(f"  {field}")
        for arm in arms:
            counts = per_arm_values[arm][field]
            shown = ", ".join(f"{value} ({count})" for value, count in counts.items())
            print(f"    {arm}: {shown or ABSENT}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
