"""Per-arm counts from one label's arm directories.

Usage: python3 abstat.py <arm-dir>...
The hosts keep jobs under $HOME/tb2/jobs/ and binaries under $HOME/tb2/bin/.
"""

import json, glob, sys
from datetime import datetime
for a in sys.argv[1:]:
    fin = [json.load(open(r)) for r in glob.glob(a + "/*/*__*/result.json")]
    fin = [d for d in fin if d.get("finished_at")]
    p = sum(((d.get("verifier_result") or {}).get("rewards") or {}).get("reward") == 1.0 for d in fin)
    to = sum("Timeout" in ((d.get("exception_info") or {}).get("exception_type") or "") for d in fin)
    mins = 0
    for d in fin:
        ae = d.get("agent_execution") or {}
        if ae.get("finished_at"):
            mins += (datetime.fromisoformat(ae["finished_at"]) - datetime.fromisoformat(ae["started_at"])).total_seconds() / 60
    started = len(glob.glob(a + "/*/*__*"))
    print(f"{a:16} started {started:2} finished {len(fin):2} pass {p:2} timeouts {to} agent_min {mins:.0f}")
