# AGENTS.md

Instructions for any coding agent working in this repository. `CLAUDE.md` points here; keep this file canonical.

## What this is

`nitpick` is a Rust CLI that AI coding agents run before opening a PR. It sends the git diff, the full changed files, and related code from the rest of the repo to a model on OpenRouter (or a local Ollama / llama.cpp server) and prints structured findings. Exit code 1 means findings at or above `--fail-on`. `nitpick watch` does the same in the background while an agent works, driven by the harness's hooks (`nitpick hook <harness> <event>`), and hands findings back to the agent through those hooks. The code is written for agents to maintain and for agents to run; optimize for correctness and runtime speed, not for prose-like readability.

Read `README.md` for user-facing behavior and configuration.

## Toolchain

- Rust stable, currently 1.98 (`rust-version` in `Cargo.toml`), edition 2024. Use current stable; do not pin older.
- No CI. There is deliberately no GitHub Actions workflow. You are the CI: run the checks below locally before every commit and report their output. Do not add a workflow back without being asked.

## Commands

Run all of these before committing. Each must be clean.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

Fix formatting with `cargo fmt --all`. Configuration is in `rustfmt.toml` (120 columns).

Dogfood the change on itself:

```bash
./target/release/nitpick context            # exact payload that would be sent, no network
./target/release/nitpick context --json     # same as a structured pack with stats
source ~/.zshrc && ./target/release/nitpick -v -m nvidia/nemotron-3-ultra-550b-a55b:free
```

The OpenRouter key is `NITPICK_OPENROUTER_API_KEY`, exported from `~/.zshrc`; it is not in the app environment by default. `NITPICK_DEBUG_DIR=/some/dir` writes every request and raw response to disk.

Benchmarking the context engine: clone a mid-size public repo into a scratch directory, touch one file, and time `nitpick context -q`. Target is under 500 ms on a repo of ~1,000 files in release mode.

## Layout

```
src/main.rs      clap CLI, settings merge (flags > env > .nitpick.toml > defaults), review loop
src/git.rs       shells out to git: diff modes, base detection, untracked files, file contents
src/diff.rs      unified diff parser; new_ranges() is hunk spans, changed_ranges() is precise
src/lang.rs      tree-sitter definitions (TS/TSX/JS/Python/Rust/Go), import regexes, identifier filter
src/search.rs    one parallel walk with the ripgrep crates; is_code_file / is_junk filters
src/context.rs   the context engine: changed files, definitions, call sites, imports, tests, budget
src/prompt.rs    system prompt (schema embedded) and the sectioned user message
src/llm.rs       OpenAI-compatible client; schema fallback, retries, max_tokens doubling
src/review.rs    Review/Finding types, lenient JSON parsing with aliases, merging, rendering
src/run.rs       Settings resolution (flags > config > defaults) and the parallel model loop, shared by review and watch
src/watch.rs     watch mode: state under <git-dir>/nitpick (baselines, trigger, worker lock, inbox), change detection,
                 incremental review, the worker loop, the stop decision, status/log/run/reset
src/hooks.rs     `nitpick hook`: per-harness stdin parsing and output JSON; `watch install/uninstall`
src/shims/       TypeScript shims for pi, OpenCode and OpenClaw, embedded with include_str! and written by `watch install`
src/config.rs    .nitpick.toml (+ ~/.config/nitpick/config.toml) loading, the [watch] table, the `init` template
```

Unit tests live next to the code in `#[cfg(test)]` modules. Add one for every parser or resolver change; `src/context.rs` tests show how to test import resolution with a fake file set.

### Watch mode, in one paragraph

Hooks are thin and must never slow or break the agent: `hooks::run` catches every error, prints it to stderr and exits 0. The `tool` event touches `<git-dir>/nitpick/trigger` and spawns a detached `nitpick watch worker` (null stdio, own process group) unless `worker.lock` has a fresh heartbeat. The worker waits for `debounce_secs` of quiet, computes `pending_changes` (every dirty file plus every file with a shadow whose content differs from its shadow, or from HEAD when it has no shadow), writes before/after copies to a temp tree, diffs them with `git diff --no-index --src-prefix= --dst-prefix=`, and feeds that diff to `context::build_from_diff` with `DiffMode::Snapshot` so line numbers come from the exact bytes reviewed. Findings at or above `deliver` go to `inbox/`; the next hook drains them into the harness's "additional context" field. The `stop` event waits for the worker, reviews leftovers inline, and blocks (Claude/Codex `decision: block`, Cursor `followup_message`, generic `block: true`) when anything is at or above `fail_on`, at most `max_stop_blocks` times in a row. Every harness-specific detail (field names, which events can carry context) is in `hooks.rs`; keep it there.

To test a hook by hand, pipe a payload in: `printf '{"cwd":"%s","tool_name":"Edit"}' "$PWD" | nitpick hook claude tool`. `nitpick watch status` and the `log` file under the state dir show what happened.

## Constraints that matter

- Line numbers sent to the model come from `number_lines` in `src/context.rs` and must match the post-change file exactly. Any change to diff parsing or windowing needs a test proving line numbers still line up.
- Never slice `lines[(s - 1)..e]` without checking `s >= 1` and `e <= lines.len()`. A panic here kills every review.
- Everything sent to a model goes through `prompt::user_message`. If you add a context source, label it with a `reason` so the model knows why it is there.
- Structured output is best effort. Providers reject `response_format` in surprising ways (one returns 502). Keep the schema in the system prompt and keep `review::parse_lenient` tolerant; never make a successful review depend on `response_format` being honored.
- Free OpenRouter models are flaky (429, empty 200 bodies, multi-minute hangs). Test with `--timeout` set and expect to rerun. Treat a provider failure as a retry, not a code bug, unless the debug dump says otherwise.
- Do not add dependencies for convenience. The binary is a single static executable; startup time is a feature.
- Do not commit `NITPICK_DEBUG_DIR` output, `.nitpick.local.toml`, or anything under `target/`.
- `api_key` is honored only from the user-level config file, never from a repo's `.nitpick.toml` (`config::load` clears it). A cloned repo must not be able to point nitpick at a secret.
- The watch worker runs with whatever environment the agent's hook had. Hooks in GUI-launched agents may lack the shell profile; that is what the user-level config file is for.

## Git

- Work on `main` directly for small changes; branch for anything you want reviewed. Commits are pushed to `LVTD-LLC/nitpick`.
- Commit messages: imperative summary line, then why, wrapped at 72 columns.
- Before pushing, run the full command list above and run `nitpick` on your own diff. Fix or explicitly dismiss every high or blocker finding in the commit message.

## Releasing

Homebrew formula lives in `LVTD-LLC/homebrew-tap` (`Formula/nitpick.rb`) and builds from the tagged source tarball. To release:

1. Bump `version` in `Cargo.toml`, run the checks, commit.
2. `git tag vX.Y.Z && git push origin main vX.Y.Z`
3. `curl -sL https://github.com/LVTD-LLC/nitpick/archive/refs/tags/vX.Y.Z.tar.gz | shasum -a 256`
4. Update `url` and `sha256` in the tap formula, add a `CHANGELOG.md` line there, open and merge the PR.
