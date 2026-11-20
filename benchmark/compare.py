"""Compare two saved runs on the same PRs, with a paired PR-level bootstrap."""

import argparse
import json
import random
from pathlib import Path

from grade import digest, summarize
from run import MANIFEST


def interval(before, after, seed=20261118, samples=10000):
    """Resample whole PR pairs and recompute the difference in micro-F1."""
    rng = random.Random(seed)
    deltas = []

    def micro_f1(rows, indices):
        hits = sum(rows[i]["match"] for i in indices)
        total = sum(rows[i]["findings"] + rows[i]["references"] for i in indices)
        return 2 * hits / total

    for _ in range(samples):
        indices = rng.choices(range(len(before)), k=len(before))
        deltas.append(micro_f1(after, indices) - micro_f1(before, indices))
    deltas.sort()
    return [deltas[int(samples * q)] for q in (0.025, 0.975)]


def audited_scores(cases, root, audit, arm):
    rows = []
    for case in cases:
        name = case["id"] + ".json"
        review = json.loads((root / name).read_text())
        record = audit[case["id"]][arm]
        if record["report_sha256"] != digest(review):
            raise ValueError(f"Audit does not apply to {root / name}")
        n = len(review["report"]["findings"])
        m = len(json.loads((root / "grades" / name).read_text())["references"])
        pairs = record["pairs"]
        if (
            len({i for i, _ in pairs}) != len(pairs)
            or len({j for _, j in pairs}) != len(pairs)
            or any(i not in range(n) or j not in range(m) for i, j in pairs)
        ):
            raise ValueError(f"Invalid audited pair for {name}")
        rows.append({"match": len(pairs), "findings": n, "references": m})
    hits, n, m = (sum(r[k] for r in rows) for k in ("match", "findings", "references"))
    return rows, {
        "match": hits,
        "missed": m - hits,
        "precision": hits / n,
        "recall": hits / m,
        "f1": 2 * hits / (n + m),
    }


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("before", type=Path)
    p.add_argument("after", type=Path)
    p.add_argument("--split", choices=["dev", "holdout", "all"], default="holdout")
    p.add_argument(
        "--audit",
        type=Path,
        help="Replay the second reading of the recorded holdout matches",
    )
    args = p.parse_args()
    cases = [c for c in MANIFEST["cases"] if args.split in ("all", c["split"])]
    before = [summarize([c], args.before) for c in cases]
    after = [summarize([c], args.after) for c in cases]
    result = {
        "baseline": summarize(cases, args.before),
        "candidate": summarize(cases, args.after),
        "paired_f1_delta_95pct_bootstrap": interval(before, after),
    }
    if args.audit:
        audit = {r["case"]: r for r in json.loads(args.audit.read_text())}
        if set(audit) != {c["id"] for c in cases}:
            p.error("audit and selected split must cover the same PRs")
        left, baseline = audited_scores(cases, args.before, audit, "baseline")
        right, candidate = audited_scores(cases, args.after, audit, "tuned")
        result["match_audit"] = {
            "baseline": baseline,
            "candidate": candidate,
            "paired_f1_delta_95pct_bootstrap": interval(left, right),
        }
    print(json.dumps(result, indent=2))
