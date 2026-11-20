"""Measure complete new-file source visibility in the actual Nitpick context packs."""

import argparse
import json
import subprocess
from pathlib import Path

from run import MANIFEST, checkout


def count(case, binary, budget):
    repo = checkout(case)
    output = subprocess.check_output(
        [
            binary,
            "context",
            "--json",
            "-q",
            "--budget",
            str(budget),
            "--range",
            case["base"] + ".." + case["head"],
        ],
        cwd=repo,
    )
    pack = json.loads(output)
    missing = []
    total = 0
    for f in pack["files"]:
        if f["status"] != "added" or not f["added"]:
            continue
        path = f["path"]
        entry = subprocess.check_output(
            ["git", "-C", repo, "ls-tree", case["head"], "--", path]
        )
        if entry.startswith(b"160000 commit "):
            continue  # A submodule pointer contains no source blob in this checkout.
        total += 1
        patch = subprocess.check_output(
            [
                "git",
                "-C",
                repo,
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                "--unified=3",
                case["base"],
                case["head"],
                "--",
                path,
            ]
        )
        # Compare complete single-file patches, or verify every original numbered line.
        if patch.decode(errors="replace").strip() in pack["diff_text"]:
            continue
        blob = subprocess.check_output(
            ["git", "-C", repo, "show", case["head"] + ":" + path]
        )
        lines = blob.decode(errors="replace").splitlines()
        listing = (f["listing"] or "").splitlines()
        if listing != [f"{i:5}| {line}" for i, line in enumerate(lines, 1)]:
            missing.append(path)
    return {
        "case": case["id"],
        "split": case["split"],
        "nonempty_added_text_files": total,
        "missing": missing,
    }


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("binary", type=Path)
    p.add_argument("--budget", type=int, default=24000)
    args = p.parse_args()
    print(
        json.dumps(
            [count(c, args.binary.resolve(), args.budget) for c in MANIFEST["cases"]],
            indent=2,
        )
    )
