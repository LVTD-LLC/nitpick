# Why ten reference issues were missed

We selected ten correctness-related references across five of the original PRs that both historical arms missed under the stricter second reading. This is a diagnostic sample chosen for distinct failure types. Three references involve the same Cal.com PR, so these counts do not estimate prevalence across the dataset.

For the historical runs we reconstructed the user messages with the saved binary, pinned Git range and original flags. For the fixed-binary runs we inspected the actual successful request bodies saved by Nitpick. We looked for the *chain of evidence* needed to diagnose each failure, including the caller when it matters. Presence means that a reviewer could reason about the issue from the supplied code. It does not establish why a particular model response omitted it.

| PR and zero-based reference | Evidence needed | Historical 24k | Historical 80k | Fixed 24k | Fixed 80k |
|---|---|---|---|---|---|
| Cal.com #11059 · 0 | The helper assigns the literal string `"refresh_token"` when the parsed credential lacks one. | Missing helper | Visible | Visible | Visible |
| Cal.com #11059 · 2 | The helper returns the `safeParse` wrapper. The Google caller persists that result as the credential key. | Missing helper | Visible | Visible | Visible |
| Cal.com #11059 · 3 | The sync branch returns a raw fetch response. The caller reads `res.data` and then dereferences the token. | Missing helper | Visible | Visible | Visible |
| Discourse #4 · 5 | The new ERB template closes its conditional with `end if`. | Visible | Visible | Visible | Visible |
| Discourse #4 · 6 | The feed job calls `i.content.scrub`; an item without content reaches the method call. | Visible | Visible | Visible | Visible |
| Grafana #79265 · 0 | The device limit count and later database insert are separate operations. Two distinct new devices can pass the count check. | Visible | Visible | Visible | Visible |
| Grafana #79265 · 4 | A cache hit returns success. The cache write comes before the store call that may fail, and the error path retains the entry. | Partial | Visible | Partial | Visible |
| Grafana #76186 · 2 | The test logger records calls on the receiver. Its new `FromContext` constructs another logger, so assertions on the original lose those calls. | Visible | Visible | Visible | Visible |
| Sentry #3 · 0 | The event factory checks the truthiness of the sample rate before assigning it. A numeric zero is skipped. | Visible | Visible | Visible | Visible |
| Sentry #3 · 1 | Both cache operations hash sorted integer project IDs. See the reference-quality check below. | Visible | Visible | Visible | Visible |

**Context retrieval.** The historical 24k reconstruction dropped the bodies of both new OAuth helpers. Restoring new-file patches exposed all three Cal.com failure mechanisms. The fixed 24k reviewer caught the raw-response mismatch. The 80k reviewer saw that same path and centered its findings on other integrations. On Grafana #79265, 24k preserved the cache lookup but windowed out lines 87–95 of `tagDeviceUI`, which contain the cache write and failing store call. Those lines were unchanged, so the raw diff could not recover them. This is a specific case for preserving the rest of a relevant function when a window cuts through it.

**Reviewer behavior.** The ERB typo, zero-value branch and separate count/insert were already present in both historical inputs. On Grafana #76186, both fixed-budget reviewers noticed that `FromContext` creates a new logger but diagnosed lost context attributes. Neither explained the lost calls on the original receiver. Improving retrieval alone would do little for these examples. A reviewer experiment can target the operation's observable consequence with this same input.

**Reference quality.** The Sentry #3 hash reference ascribes a cross-process change to Python's hash randomization. In the pinned code, `SnubaParams.project_ids` returns `list[int]`, and invalidation accepts `Sequence[int]`. Three separate Python processes with hash seeds 1, 2 and 73 returned identical values for the same sorted integer tuple on this host. The fixed 80k review repeated the reference’s hash-randomization claim. We kept the upstream reference in the metrics and flagged it here. A missed reference can itself be wrong.

The [per-reference source checks](evidence.json) list the literal code fragments and presence flags. The [pinned original revisions](../cases.json) identify the exact source.
