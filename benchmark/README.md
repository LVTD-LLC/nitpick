# Nitpick review benchmark

The [measured results](RESULTS.md) cover 20 public PRs, 40 Nitpick reviews and a separate test of source coverage. Each review ran the release CLI against an actual Git checkout at the PR head. The reviewer received the same inputs it gets for a local revision-range review.

## Dataset and experiment

[cases.json](cases.json) pins the review head, merge base and split for each PR. Starting with the five golden-comment files in [Martian's offline benchmark](https://github.com/withmartian/code-review-benchmark/tree/e616e849755441da38f18bf3adba2c9583b03803/offline), we sorted each project's ten PR URLs by the binary SHA-256 digest of `nitpick-v1:` followed by the URL. The first two went into development and the next two into holdout. Some source URLs point to Martian's evaluation forks. The manifest pins the actual reviewed commits on those forks. The reference data carries [Martian's MIT license](MARTIAN-LICENSE).

Both arms used Nitpick revision `7cda2c4a8166fe11792708380e8500ac6c5a8d83`, OpenRouter's `openai/gpt-4.1-mini`, temperature 0, a 4,096-token starting output allowance and 400 lines per changed file before windowing. Baseline used the stock system prompt and 80k context budget. The candidate used a 24k context budget and [one additional focus instruction](focus.txt). These two changes were tested together. We froze the candidate before examining held-out grades. The source-coverage fix was developed separately. Production prompt and budget defaults retain their original values.

## Replay the recorded comparison

Run from the Nitpick repository root with Python 3.10 or newer. These commands use the checked-in reports and grades and run offline.

```sh
python3 benchmark/compare.py benchmark/results/baseline benchmark/results/tuned --split dev
python3 benchmark/compare.py benchmark/results/baseline benchmark/results/tuned --split holdout --audit benchmark/results/holdout-match-audit.json
python3 -m unittest discover -s benchmark
```

Each PR has a JSON report with its revision pair, binary hash, model settings, findings and provider-reported usage. Its corresponding file under `grades/` contains indexed reference comments, one classification per finding and the judge's reasoning. A missed issue is a reference index absent from the matched rows. Summary generation requires the complete split and checks that each grade belongs to the recorded review.

## Run fresh reviews

Use Git, Python and the OpenRouter key in `NITPICK_OPENROUTER_API_KEY` or `OPENROUTER_API_KEY`. Building requires Rust 1.98 or newer. These commands build the historical reviewer in a temporary directory and reuse it for both arms. They write fresh results under `target/`. Explicitly reuse the same binary when resuming a run.

```sh
original=$(mktemp -d)
git archive 7cda2c4a8166fe11792708380e8500ac6c5a8d83 | tar -x -C "$original"
cargo build --locked --release --manifest-path "$original/Cargo.toml" --target-dir "$original/target"
binary="$original/target/release/nitpick"

python3 benchmark/run.py prepare --split all --jobs 3
for split in dev holdout; do
  python3 benchmark/run.py review --split "$split" --binary "$binary" --output target/benchmark/baseline
  python3 benchmark/grade.py grade target/benchmark/baseline --split "$split"
  python3 benchmark/run.py review --split "$split" --binary "$binary" --output target/benchmark/tuned \
    --budget 24000 --focus benchmark/focus.txt
  python3 benchmark/grade.py grade target/benchmark/tuned --split "$split"
done
python3 benchmark/compare.py target/benchmark/baseline target/benchmark/tuned
```

Public shallow checkouts used about 3.1 GiB. The runner fetches both commits, checks out the pinned head for related-code search, and passes the merge-base-to-head range to Nitpick. Golden comments, other tools' reviews and PR discussion stay outside the checkout. Exit codes 0 and 1 are successful reviews. A failed request creates no review record. Rerun the same command to resume. Model IDs, provider routing and sampling can yield different fresh reviews.

## Scoring

The grader uses `anthropic/claude-sonnet-4.6` via OpenRouter. It receives experiment inputs without an arm label (findings, reference comments, the PR diff and source windows) with the same rubric for both arms. Large diffs are limited to the first and last 50,000 characters, and each finding gets up to 30 surrounding lines on each side. The judge retains uncertainty when it lacks sufficient evidence.

- **Reference recall** is the share of all curated issues matched, including style, documentation and test gaps. **Reference precision** is matched findings divided by all emitted findings. **F1** is their harmonic mean, pooling counts across PRs.
- A finding can earn one reference match, and each reference can earn credit once. Remaining findings are classified as supported extra issues, contradicted or misattributed claims (false positives), duplicates, or uncertain. Each category can be audited in the saved grades.
- The comparison resamples whole PR pairs 10,000 times with a fixed seed to estimate uncertainty in the F1 difference. This is a custom single-judge evaluation. Its scores should be compared within this experiment. The public dataset, incomplete labels and small holdout limit generalization.

## Verify source coverage

```sh
cargo build --release --locked
python3 benchmark/coverage.py "$binary" > target/benchmark/coverage-before.json
python3 benchmark/coverage.py target/release/nitpick > target/benchmark/coverage-after.json
```

This check calls `nitpick context` at a 24k budget and verifies that each nonempty added text file in the context pack has either its complete Git patch or its complete numbered listing. It excludes submodule pointers and empty files. Saved before-and-after records list every missing path. This measurement needs no model credential.
