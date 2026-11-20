"""Run the paired context-budget experiment and replay its scores."""

import argparse
import hashlib
import json
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from compare import audited_scores, interval
from grade import digest, grade, summarize
from run import CACHE, HERE, MANIFEST, review, review_flags

STUDY = HERE / "budget-study"
FRESH = json.loads((STUDY / "fresh.json").read_text())
CASES = MANIFEST["cases"] + FRESH["cases"]
BUDGETS = [80000, 24000]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("action", choices=["review", "grade", "summary"])
    p.add_argument("--output", type=Path, default=CACHE / "budget-study" / "results")
    p.add_argument(
        "--binary", type=Path, default=HERE.parent / "target/release/nitpick"
    )
    p.add_argument("--jobs", type=int, default=1)
    p.add_argument("--split", choices=["original", "fresh", "all"], default="all")
    p.add_argument(
        "--audit",
        type=Path,
        help="Replay a frozen match audit when summarizing the fresh PRs",
    )
    p.add_argument(
        "--requests",
        type=Path,
        default=CACHE / "budget-study/requests",
        help="Local review payload directory",
    )
    args = p.parse_args()
    if args.audit and (args.action != "summary" or args.split != "fresh"):
        p.error("--audit requires summary --split fresh")
    if MANIFEST["dataset"] != FRESH["dataset"] or len({c["id"] for c in CASES}) != len(
        CASES
    ):
        p.error(
            "Case manifests must have distinct case IDs and the same dataset revision"
        )
    cases = [
        c
        for c in CASES
        if args.split == "all" or (c["split"] == "fresh") == (args.split == "fresh")
    ]
    binary = args.binary.resolve()
    if args.action == "review":
        binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()

    def work(item):
        i, case = item
        for budget in BUDGETS if i % 2 == 0 else reversed(BUDGETS):
            root = args.output / str(budget)
            if args.action == "review":
                flags = review_flags(budget, None)
                fingerprint = {
                    "binary_sha256": binary_hash,
                    "flags": flags,
                    "cases_sha256": digest(CASES),
                }
                review(
                    case,
                    binary,
                    flags,
                    root,
                    fingerprint,
                    debug_dir=args.requests / str(budget) / case["id"],
                )
            elif args.action == "grade":
                grade(case, root)

    if args.action in ("review", "grade"):
        with ThreadPoolExecutor(args.jobs) as pool:
            list(pool.map(work, enumerate(cases)))
    else:
        by_arm = [
            [summarize([c], args.output / str(b)) for c in cases] for b in BUDGETS
        ]
        result = {str(b): summarize(cases, args.output / str(b)) for b in BUDGETS}
        result["paired_f1_delta_24k_minus_80k_95pct"] = interval(*by_arm)
        if args.audit:
            audit = {r["case"]: r for r in json.loads(args.audit.read_text())}
            if set(audit) != {c["id"] for c in cases}:
                p.error("Audit must cover exactly the fresh PRs")
            rows, scores = zip(
                *(
                    audited_scores(cases, args.output / str(b), audit, str(b))
                    for b in BUDGETS
                )
            )
            result["audit"] = {
                **dict(zip(map(str, BUDGETS), scores)),
                "paired_f1_delta_24k_minus_80k_95pct": interval(*rows),
            }
        print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
