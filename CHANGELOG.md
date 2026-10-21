# Changelog

## Unreleased

- Stop sending PostHog `$exception` events (error tracking). Failures are still visible through exit codes on `cli_command_completed` and failed-model counts on `$ai_trace`.

## 0.3.2 — 2026-10-09

- Add opt-out, privacy-limited CLI usage events, per-attempt AI usage/cost diagnostics, sanitized errors, and native OTLP logs/traces. Keep hooks/context offline and bound network overhead.
