# Changelog

## 0.3.3 — 2026-11-20

- Preserve the full patch for new files when a tight context budget drops their numbered listings. Read untracked files correctly when Git uses mnemonic diff prefixes.
- Add a reproducible review benchmark and a comparison of 24k and 80k context budgets.
- Stop sending PostHog `$exception` events (error tracking). Failures are still visible through exit codes on `cli_command_completed` and failed-model counts on `$ai_trace`.

## 0.3.2 — 2026-10-09

- Add opt-out, privacy-limited CLI usage events, per-attempt AI usage/cost diagnostics, sanitized errors, and native OTLP logs/traces. Keep hooks/context offline and bound network overhead.
