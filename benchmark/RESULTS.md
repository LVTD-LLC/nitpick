# Nitpick benchmark results · 18 November 2026

**The shorter review used 54% fewer input tokens, and each configuration caught seven of the 43 held-out reference issues in the second reading.** It emitted ten more findings. We kept the prompt and budget defaults. The benchmark also exposed a context bug with a reproducible fix. At a 24k budget, the fixed CLI delivered the complete source for **45 of 45** eligible new files, up from **27 of 45**.

## Held-out review quality

The holdout comprised ten PRs across five projects with 43 curated issues. The other ten PRs were used for development. Each arm ran once per PR. Both arms used the same Nitpick binary and GPT-4.1 mini. The candidate combined the shorter budget with an instruction to trace concrete failures and group repeated findings. See the [method and replay commands](README.md).

| Held-out metric | Baseline | Candidate |
|---|---:|---:|
| Findings emitted | 37 | 47 |
| Reference issues caught, second reading | 7 / 43 | 7 / 43 |
| Reference issues missed, second reading | 36 | 36 |
| Reference precision, second reading | 18.9% | 14.9% |
| Reference F1, second reading | 17.5% | 15.6% |
| Findings the automated judge marked false positive | 11 | 7 |
| Findings the automated judge marked uncertain | 10 | 21 |
| Review input tokens | 489,687 | 227,358 |
| Provider-reported review cost | $0.2073 | $0.1053 |

The seven matches in each arm occurred on different sets of issues. On Grafana #79265 the candidate caught two regressions involving an anonymous-device limit. On Sentry #67876 the baseline caught a reused OAuth state value and a missing metadata key. The candidate described the intended flow and missed both. [Grafana findings](results/tuned/grafana-79265.json) · [Sentry findings](results/baseline/sentry-67876.json)

**The raw judge overstated recall.** Claude Sonnet 4.6 credited 12 baseline matches and 15 candidate matches. A second reading by Codex checked all 84 findings against the 43 references, requiring the finding to identify the specific failure. For example, the judge matched a positive description of Keycloak's new anchor sanitizer to a reference about incorrect matcher handling. The finding itself said it detected no issue. The second reading was performed after scoring, with the arm identities visible. It is a sensitivity check by another model, and neither reading establishes human-verified accuracy. The 95% paired PR-bootstrap interval for the second reading's F1 difference was −19.6 to +14.1 percentage points. [Audit decisions for all ten PRs](results/holdout-match-audit.json) · [Keycloak review](results/tuned/keycloak-37429.json)

The false-positive counts come from the automated judge's assessment of contradictory or misattributed claims. It separately credited three supported extra findings for baseline and four for the candidate. The increase in uncertain findings makes the reduction from eleven to seven false positives inconclusive. Reference precision includes all emitted findings in its denominator, so a valid issue omitted by the dataset can lower it.

## Development and raw judge results

| Split and arm | Reference matches | Missed | Findings | Reference precision | Recall | F1 | Judge-labeled false positives |
|---|---:|---:|---:|---:|---:|---:|---:|
| Development · baseline | 7 / 25 | 18 | 30 | 23.3% | 28.0% | 25.5% | 7 |
| Development · candidate | 10 / 25 | 15 | 36 | 27.8% | 40.0% | 32.8% | 8 |
| Holdout · baseline | 12 / 43 | 31 | 37 | 32.4% | 27.9% | 30.0% | 11 |
| Holdout · candidate | 15 / 43 | 28 | 47 | 31.9% | 34.9% | 33.3% | 7 |

Development reviews showed repeated claims at multiple call sites and large context packs. For example, six findings on Cal.com #10967 repeated a nested-array diagnosis the judge found contradicted by the types. That motivated the focus instruction and the smaller token budget. The held-out experiment cannot separate their individual effects. [Development example](results/baseline/grades/cal_dot_com-10967.json)

The retained successful review and grading responses report **$2.72** across both arms and splits. That sum excludes rejected judge outputs, retries and the separate dogfood review. OpenRouter temporarily rejected concurrent requests with HTTP 402 while reserving credits for in-flight requests. Running sequentially and resuming completed all 40 reviews and grades. Paid stages default to one job.

## Verified context improvement

Nitpick normally replaces a new file's unified-diff hunk with a pointer to the numbered source listing. Under budget pressure it could later discard that listing, leaving the model with neither copy. The fix restores the raw patch when it drops listings and retains the patch when a listing cannot be produced. The full diff remains the minimum input and may exceed the requested context budget.

| Complete nonempty new text files at a 24k budget | Original CLI | Fixed CLI |
|---|---:|---:|
| Development | 2 / 12 | 12 / 12 |
| Holdout | 25 / 33 | 33 / 33 |
| All 20 PRs | 27 / 45 | 45 / 45 |

At the stock 80k budget, both binaries preserved all 45 files on this sample. The four PRs affected at 24k include Keycloak #37634, where eight added files disappeared from the original payload. The check compares emitted contents with Git at the pinned head. The review-quality measurements above used the historical CLI in both arms, so this separate before-and-after result establishes source visibility only. [Original coverage by path](results/coverage-before.json) · [Fixed coverage by path](results/coverage-after.json)

Local dogfooding also exposed a Git configuration issue. With `diff.mnemonicPrefix=true`, Git labeled untracked files with a `2/` prefix, and Nitpick looked for them at the wrong paths. Explicit diff prefixes restored numbered listings for all three benchmark scripts in the local check, up from zero. A regression test now covers the Git setting, post-change line numbering and an over-budget added file.
