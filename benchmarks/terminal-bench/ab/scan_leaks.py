"""Flags Terminal-Bench trials whose agent may have looked up the answer.

Usage: python3 scan_leaks.py <jobs dir>...
Scans every trial's agent logs (radian session journals, pi sessions, stdout)
for references to the benchmark's public sources, its solution and test files,
and the task's own name inside a URL. Prints one line per hit and a summary
per trial; exit 1 when any trial has a hit.

The hosts keep their jobs under $HOME/tb2/jobs/; pass that path's label
directories as the arguments.
"""

import json
import re
import sys
from pathlib import Path

PATTERNS = [
    r"terminal-bench",
    r"terminal_bench",
    r"tbench",
    r"laude-institute",
    r"harbor-framework",
    r"huggingface\.co/datasets",
    r"\bsolve\.sh\b",
    r"/solution\b",
    r"test_outputs\.py",
    r"/tests/test\.sh",
]


def trial_dirs(root: Path):
    for result in root.rglob("result.json"):
        trial = result.parent
        if (trial / "agent").is_dir():
            yield trial


def task_name(trial: Path) -> str:
    return trial.name.rsplit("__", 1)[0]


def main() -> int:
    flagged = 0
    for root in map(Path, sys.argv[1:]):
        for trial in sorted(trial_dirs(root)):
            name = task_name(trial)
            url_task = re.compile(r"https?://\S*" + re.escape(name), re.I)
            pats = [re.compile(p, re.I) for p in PATTERNS]
            hits = []
            for f in (trial / "agent").rglob("*"):
                if not f.is_file() or f.stat().st_size > 200_000_000:
                    continue
                text = f.read_text(errors="ignore")
                for p in pats + [url_task]:
                    for m in p.finditer(text):
                        ctx = text[max(0, m.start() - 80): m.end() + 80].replace("\n", " ")
                        hits.append((f.relative_to(trial), p.pattern, ctx))
            reward = None
            try:
                reward = json.loads((trial / "result.json").read_text()).get(
                    "verifier_result", {}
                ).get("rewards", {}).get("reward")
            except Exception:
                pass
            status = "HIT" if hits else "clean"
            print(f"{status}\t{trial}\treward={reward}\thits={len(hits)}")
            for f, pat, ctx in hits[:5]:
                print(f"    {f}  /{pat}/  ...{ctx}...")
            flagged += bool(hits)
    print(f"flagged trials: {flagged}")
    return 1 if flagged else 0


if __name__ == "__main__":
    sys.exit(main())
