# Telemetry contract

Project: [Nitpick](https://us.posthog.com/project/654831), US Cloud.
All collection is optional; see README for opt-out and build configuration.

| Signal | Contract |
| --- | --- |
| Product usage | `cli_command_completed`: command enum, duration in ms, exit code (1 = findings, not an application failure) |
| AI trace | `$ai_trace`: review model/failure/finding counts, changed-file count, estimated context tokens, context-build ms |
| AI generation | `$ai_generation`: one per HTTP attempt including schema fallback/retries, provider enum, allowlisted model, latency seconds, numeric usage/cost if provided, HTTP status and fixed failure category |
| Errors | `$exception`: only fixed `command_failed` / `model_review_failed` categories, no arbitrary error text or native stack |
| Native logs | OTLP/HTTP JSON `/i/v1/logs`; fixed event name body, typed allowlisted properties, shared trace/span IDs where present |
| Native traces | OTLP/HTTP JSON `/i/v1/traces`; one review parent with child HTTP-generation spans and accurate start/end nanoseconds |

The wire boundary is `src/telemetry.rs`. Only explicit structured fields are
constructed; there is no generic terminal/log exporter. `model_label` is a finite
allowlist: expand it deliberately when adding known public models. Never capture
`Provider.base_url`, `Completion.content`, request messages, error strings or
repository metadata. No prompts/outputs means content evaluations and prompt
inspection are deliberately unavailable. Token/cost reports are best-effort;
unknown usage remains unknown, and provider failures may be billed without
returning usage. Spans/logs have no hostname, username, or process arguments.

Added crates: `uuid` for OS-random, non-derived identifiers and minimal `chrono`
(std only, no local-time probing) for correct RFC3339 event start timestamps. We use the
existing reqwest transport and documented OTLP JSON wire format instead of
installing a general-purpose log bridge that might upload private diagnostic
strings. Event UUIDs support ingestion deduplication; there is no retry spool.
A 1,500 ms overall deadline bounds concurrent export of all three payloads. DNS,
TLS, HTTP failures and queue overflow are nonfatal; offline delivery is not
guaranteed. One flush occurs per completed foreground command, or per background-worker review.
Foreground commands spend at most 1.5 seconds on network export.
Hook callbacks/context/help/version do not initialize or export telemetry.

## Verification

Run repository checks from AGENTS.md. Telemetry unit tests cover opt-out,
identity stability/corruption, public-model allowlisting, trace correlation.
Use a local mock OpenAI-compatible provider with synthetic private marker strings
to check numeric usage and privacy without sending source to a real model.
Inspect PostHog events, log records and spans independently: HTTP acceptance is
not sufficient proof of indexed data. A deliberately failing review should
preserve stderr and exit code 2 while reporting only a fixed failure category.

## Release

Set `NITPICK_POSTHOG_PROJECT_TOKEN` to the public write-only project token in the
build environment. Official Homebrew releases set that environment variable in
the formula; source builds without it remain telemetry-free. Management keys
(`phx_`) must never be compiled in. No runtime endpoint override is accepted.
Set `NITPICK_TELEMETRY=0` for dogfooding and benchmarks to avoid polluting analytics.

References: [capture API](https://posthog.com/tutorials/api-capture-events),
[manual AI capture](https://posthog.com/docs/ai-observability/installation/manual-capture),
[logs](https://posthog.com/docs/logs/installation/rust),
[traces](https://posthog.com/docs/distributed-tracing/start-here).
