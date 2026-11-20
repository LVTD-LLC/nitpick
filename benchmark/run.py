"""Run Nitpick on immutable public PR snapshots. Python 3.10+ and git."""

import argparse
import hashlib
import json
import os
import subprocess
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

HERE = Path(__file__).resolve().parent
MANIFEST = json.loads((HERE / "cases.json").read_text())
CACHE = HERE.parent / "target" / "benchmark"
MODEL = "openai/gpt-4.1-mini"
ENDPOINT = "https://openrouter.ai/api/v1"


def review_flags(budget, focus):
    flags = [
        "--provider",
        "openrouter",
        "--base-url",
        ENDPOINT,
        "-m",
        MODEL,
        "--budget",
        str(budget),
        "--max-file-lines",
        "400",
        "--temperature",
        "0",
        "--timeout",
        "230",
        "--max-tokens",
        "4096",
        "--json",
    ]
    if focus:
        flags.extend(["--focus", focus.read_text().strip()])
    return flags


def save(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", dir=path.parent, delete=False) as f:
        json.dump(data, f, indent=2, ensure_ascii=False)
        f.write("\n")
    Path(f.name).replace(path)


def git(repo, *args):
    return subprocess.check_output(["git", "-C", repo, *args], text=True).strip()


def checkout(case):
    parent = CACHE / "repos"
    parent.mkdir(parents=True, exist_ok=True)
    repo = parent / case["id"]
    if not repo.exists():
        with tempfile.TemporaryDirectory(dir=parent) as tmp:
            stage = Path(tmp) / "checkout"
            stage.mkdir()
            git(stage, "init", "-q")
            git(
                stage,
                "remote",
                "add",
                "origin",
                case["url"].split("/pull/")[0] + ".git",
            )
            git(
                stage,
                "fetch",
                "--quiet",
                "--depth=1",
                "--no-tags",
                "origin",
                case["base"],
                case["head"],
            )
            git(stage, "checkout", "-q", "--detach", case["head"])
            stage.rename(repo)
    if git(repo, "rev-parse", "HEAD") != case["head"] or git(
        repo, "status", "--porcelain", "--ignored"
    ):
        raise RuntimeError(f"Dirty or incorrect checkout: {repo}")
    return repo


def review(case, binary, flags, output, fingerprint, *, debug_dir=None):
    path = output / (case["id"] + ".json")
    if path.exists():
        saved = json.loads(path.read_text())
        if saved["run"] != fingerprint or saved["case"] != case:
            raise ValueError(
                f"Run settings changed. Choose a new output directory: {path}"
            )
        return
    repo = checkout(case)
    if any((repo / name).exists() for name in (".nitpick.toml", "nitpick.toml")):
        raise ValueError(f"Unexpected reviewer configuration in {repo}")
    env = {k: v for k, v in os.environ.items() if not k.startswith("NITPICK_")}
    env.update(
        XDG_CONFIG_HOME=str(CACHE / "empty-config"),
        NITPICK_TELEMETRY="0",
        NITPICK_API_KEY=os.environ.get("NITPICK_OPENROUTER_API_KEY")
        or os.environ["OPENROUTER_API_KEY"],
    )
    if debug_dir is not None:
        env["NITPICK_DEBUG_DIR"] = str(debug_dir.resolve())
    rev = case["base"] + ".." + case["head"]
    cmd = [str(binary), *flags, "--range", rev]
    proc = subprocess.run(
        cmd,
        check=False,
        cwd=repo,
        env=env,
        text=True,
        capture_output=True,
        timeout=1200,
    )
    if proc.returncode not in (0, 1):
        raise RuntimeError(f"{case['id']}: {proc.stderr[-1200:]}")
    report = json.loads(proc.stdout)
    if (
        not report["head"]
        or not case["head"].startswith(report["head"])
        or len(report["models"]) != 1
        or not report["models"][0]["ok"]
        or report["models"][0]["model"] != MODEL
    ):
        raise ValueError(f"Incomplete review for {case['id']}")
    save(path, {"case": case, "run": fingerprint, "report": report})
    print(case["id"], len(report["findings"]), "findings", flush=True)


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("action", choices=["prepare", "review"])
    p.add_argument("--split", choices=["dev", "holdout", "all"], default="dev")
    p.add_argument(
        "--binary", type=Path, default=HERE.parent / "target/release/nitpick"
    )
    p.add_argument("--output", type=Path)
    p.add_argument("--focus", type=Path)
    p.add_argument("--budget", type=int, default=80000)
    p.add_argument("--jobs", type=int, default=1)
    args = p.parse_args()
    cases = [c for c in MANIFEST["cases"] if args.split in ("all", c["split"])]
    if args.action == "review":
        if not args.output:
            p.error("review needs --output")
        binary = args.binary.resolve()
        flags = review_flags(args.budget, args.focus)
        fingerprint = {
            "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
            "flags": flags,
            "manifest_sha256": hashlib.sha256(
                (HERE / "cases.json").read_bytes()
            ).hexdigest(),
        }

    def work(c):
        if args.action == "prepare":
            checkout(c)
            print(c["id"], "ready", flush=True)
        else:
            review(c, binary, flags, args.output, fingerprint)

    with ThreadPoolExecutor(args.jobs) as pool:
        list(pool.map(work, cases))
