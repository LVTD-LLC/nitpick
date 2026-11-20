# Fixed-binary context budget comparison · 20 November 2026

**Keep the 80k default.** On ten fresh PRs the 24k budget used 46% fewer input tokens and cost 45% less. It caught eight curated issues against nine at 80k in an arm-masked second reading. This small sample supports 24k as a cost-saving option. It leaves substantial uncertainty about a change to the default.

| Fresh PRs · 27 references | 80k | 24k |
|---|---:|---:|
| Audited reference matches | 9 | 8 |
| Missed references | 18 | 19 |
| Emitted findings | 38 | 36 |
| Reference precision | 23.7% | 22.2% |
| Reference recall | 33.3% | 29.6% |
| Reference F1 | 27.7% | 25.4% |
| Provider-reported input tokens | 359,553 | 192,776 |
| Review cost, USD | 0.1430 | 0.0783 |

The automated judge ranked the arms in the opposite order, crediting 11 matches at 80k and 13 at 24k. One 24k Sentry grade credited a Redis span-count finding against a reference about backwards pagination. The indexed reference describes a different function and failure. The [saved grade](results/24000/grades/sentry-2.json) makes the disagreement inspectable.

The budgets tied in match count on seven fresh PRs. 80k found one more issue on Grafana #80329 and Sentry #1. 24k found one more on Sentry #2. The paired bootstrap interval for the 24k minus 80k F1 difference was **−16.6 to +7.8 percentage points**. Sentry #1 and #2 have different bases but the same head and overlapping pagination code. Excluding both leaves 6/19 reference matches at 80k and 5/19 at 24k. Discourse #7 also contains two reference comments about the same selector change. These dependencies limit the precision of any estimate.

[Match decisions and PR-level explanations](fresh-audit.json) · [Fresh PR revisions](fresh.json)

## Automated grading and cost

| PR set | Budget | Judge matches | Missed | Findings | Judge-labeled false positives | Uncertain | Input tokens | Review cost |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Original 20 | 80k | 26 / 68 | 42 | 76 | 24 | 17 | 831,250 | $0.3202 |
| Original 20 | 24k | 28 / 68 | 40 | 104 | 16 | 25 | 419,108 | $0.1754 |
| Fresh 10 | 80k | 11 / 27 | 16 | 38 | 13 | 11 | 359,553 | $0.1430 |
| Fresh 10 | 24k | 13 / 27 | 14 | 36 | 11 | 8 | 192,776 | $0.0783 |

The retained successful review and grading responses across all 30 pairs report $3.98 combined. The reference counts include style and test issues. The judge's false-positive and uncertain classifications have no independent verification. The masked second reading above addresses reference matches on the fresh PRs. Three pairs on the original set and one fresh pair had byte-identical user messages at both budgets. Output and grading variation on such pairs cannot be attributed to changed context.

## What the misses tell us

Of ten selected historical misses, the reconstructed 24k payload lacked decisive source for four. Three involved Cal.com's new OAuth helper files. With the fix, the 24k reviewer caught the raw-response mismatch. The remaining partial case involved unchanged lines inside Grafana's device-cache function. On the other six cases the necessary code was already present, including one questionable reference about Python's hashing of integer tuples. These cases suggest a focused test of retaining a complete enclosing function when a context window excludes a write or error path. The other misses warrant evaluating how the reviewer traces behavior through code it already has.

[Ten diagnoses with the evidence visible in each arm](MISSES.md)

## Design and replay

Both arms used release binary `057019e38c2efb58dab28302a690ce6e0acc3b0058bddb63e67ab9ae458bb69a` (SHA-256) built from the Rust changes in `3146b685902a392d69d4974effb047fa5c939bcc`. We ran each pinned PR once at each budget. We alternated which budget ran first across PRs, and supplied no focus instruction. We checked all 60 successful OpenRouter request/response pairs. They used the same system prompt, response schema, model, temperature (0), completion allowance (4,096), and upstream provider (OpenAI). Input token counts can exceed the specified budget because the CLI budgets estimated context tokens before adding the instructions and schema. Even temperature zero allows variation in reviewer outputs.

The original 20 PRs are a repeat measurement. The ten fresh cases were the next two URLs per project (ranks five and six) under the [original hash ordering](../README.md), selected before the run. For the fresh audit, Codex compared each finding with the references under shuffled A/B labels within each PR, before decoding the budget-to-label mapping. Credit required the same concrete failure, with at most one match per reference. The audit is a second model reading. The automated judge's original decisions are also available in the grade files.

To recompute scores from the checked-in reviews and grades, run from the repository root with Python 3.10 or newer. The optional audit verifies the review hashes.

```sh
python3 benchmark/budget_study.py summary --output benchmark/budget-study/results --split original
python3 benchmark/budget_study.py summary --output benchmark/budget-study/results --split fresh \
  --audit benchmark/budget-study/fresh-audit.json
```

For a fresh paid run, build the release binary and provide the OpenRouter key following the [harness instructions](../README.md). The default output, raw request payloads and PR checkouts live under the ignored `target/benchmark/` directory. Use a new output and request directory for an independent repetition. The reported costs cover retained successful responses. Transient OpenRouter credit reservations caused some HTTP 402 responses during parallel execution. Sequential resumption completed affected calls.

```sh
cargo build --locked --release
python3 benchmark/budget_study.py review
python3 benchmark/budget_study.py grade
python3 benchmark/budget_study.py summary --split fresh
```
