# nitpick

AI code review for AI agents.

`nitpick` sends your git diff, the full changed files, and the parts of the repo that matter for judging the change to a model on [OpenRouter](https://openrouter.ai) (or a local Ollama / llama.cpp server), and prints structured findings. It exits `1` when there are findings at or above a severity you choose, so a coding agent can run it before opening a PR and loop until the review is clean.

No GitHub Action, no waiting for CI. The agent that wrote the code gets a second opinion from a different model in one command.

```
$ nitpick
nitpick: working tree vs merge-base with main (3f1c2a9) · 2 file(s) · ~7.8k tokens context (6 snippets, 0 dropped, 175ms) · stealth/space-bunny-alpha via openrouter
nitpick: stealth/space-bunny-alpha done in 41.2s, 2 finding(s)
# nitpick: request changes

`working tree vs merge-base with main (3f1c2a9)` · branch `fix-ranges` · head `9e21be5` · 2 file(s) · ~7.8k tokens of context · 1/1 model(s) · 41.4s

**stealth/space-bunny-alpha** (request changes, 41.2s): Two small changes, both regressions. ...

## Findings (2)

### High

- **src/diff.rs:62** new_ranges now returns an exclusive end `[bug]`
  The doc comment and both callers treat the range as inclusive; `windowed_listing` slices `lines[..e]` and will panic on the last hunk of a file.
  Suggestion: restore `h.new_start + h.new_len - 1`.
- **src/search.rs:132** path_stem returns the extension instead of the stem `[bug]`
  `rsplit('.').next()` yields the text after the last dot. `context.rs:441` uses this to match test files, so no tests will ever be pulled into context.

2 finding(s) at or above `high`. Exit code 1.
```

## Install

```bash
brew install LVTD-LLC/tap/nitpick
```

Or from source, with Rust 1.98 or newer:

```bash
cargo install --git https://github.com/LVTD-LLC/nitpick
```

Both build from source; the binary is a single static executable.

## Setup

Set an OpenRouter key:

```bash
export NITPICK_OPENROUTER_API_KEY=sk-or-...
```

`OPENROUTER_API_KEY` also works. That is all that is required. Optionally write a config file into the repo:

```bash
nitpick init
```

## Usage

```
nitpick                          # review working tree vs merge-base with main (auto-detected)
nitpick --staged                 # only what is staged
nitpick --base develop           # diff against a different branch
nitpick --range main..HEAD       # explicit revision range
nitpick src/api/                 # limit to some paths
nitpick -m openai/gpt-5.5 -m anthropic/claude-opus-5-5   # several models in parallel, findings merged
nitpick -f "Focus on the SQL; we had injection bugs here before."
nitpick --json                   # machine-readable output
nitpick --fail-on medium         # stricter exit code
nitpick context                  # print exactly what would be sent, without calling a model
```

What gets diffed, in order of precedence: `--range`, `--staged`, `--base`, then auto-detect. Auto-detect compares the working tree (including untracked files) against the merge-base with `origin/HEAD`, `main`, or `master`. On the default branch itself, it compares against the upstream if the branch has diverged, otherwise against `HEAD`.

### Exit codes

| code | meaning |
|---|---|
| 0 | no findings at or above `--fail-on` (default `high`) |
| 1 | findings at or above `--fail-on` |
| 2 | error: no model answered, git failed, bad config |

### For agents

The [nitpick-skills](https://github.com/LVTD-LLC/nitpick-skills) repo packages a skill and plugin for Claude Code, Codex, Cursor, OpenClaw, OpenCode, and any Agent Skills client. It teaches the agent to install nitpick, run it before a PR, and loop on findings:

```bash
claude plugin marketplace add LVTD-LLC/nitpick-skills && claude plugin install nitpick@nitpick-skills
codex plugin marketplace add LVTD-LLC/nitpick-skills && codex plugin add nitpick@nitpick-skills
```

Without a plugin system, add something like this to your `CLAUDE.md` or `AGENTS.md`:

```
Before opening a PR, run `nitpick` from the repo root. Fix every finding at
severity high or blocker, then run it again until it exits 0. Findings at
medium or below are judgment calls; address or explain them in the PR.
```

`nitpick` prints progress on stderr and the review on stdout. Findings cite `path:line` in the post-change file. With `--json` you get the same data as an object with `verdict`, `findings[]`, per-model summaries, token counts and cost.

## What gets sent

1. The unified diff.
2. The full post-change content of each changed file, numbered. Files over `max_file_lines` are windowed around the changed lines. Brand-new files are sent once, as a listing, not twice.
3. Related context, all of it unchanged code, each piece labeled with why it is there:
   - definitions of symbols the diff references (tree-sitter for TypeScript, TSX, JavaScript, Python, Rust and Go; regex elsewhere)
   - call sites of symbols the diff changed
   - modules the changed files import (outlined if large)
   - the list of files that import each changed file
   - tests that match each changed file

Context is ranked and trimmed to `budget_tokens` (default 80,000). `nitpick context` shows the exact payload.

Search uses the same crates as ripgrep and respects `.gitignore`. Nothing leaves your machine except the request to the model provider you configured.

## Configuration

`.nitpick.toml` in the repo root. Every key is optional. Flags and `NITPICK_*` environment variables override it.

```toml
model = ["stealth/space-bunny-alpha", "nvidia/nemotron-3-ultra-550b-a55b:free"]
provider = "openrouter"        # openrouter | ollama | llamacpp | openai
# base_url = "http://localhost:11434/v1"
# api_key_env = "NITPICK_OPENROUTER_API_KEY"
# base = "main"
fail_on = "high"                # blocker | high | medium | low | nit
budget_tokens = 80000
max_file_lines = 400
max_tokens = 16000              # completion budget; doubled automatically if the model runs out
# reasoning = "medium"          # none | low | medium | high, for models that support it
# timeout_secs = 300
ignore = ["**/*.lock", "**/generated/**"]
instructions = """
This is a Django app. Be strict about N+1 queries and missing select_related.
Ignore anything about docstrings.
"""
```

Environment variables: `NITPICK_MODEL`, `NITPICK_PROVIDER`, `NITPICK_BASE_URL`, `NITPICK_API_KEY`, `NITPICK_REASONING`, `NITPICK_OPENROUTER_API_KEY`, `OPENROUTER_API_KEY`, `NITPICK_OPENAI_API_KEY`, `OPENAI_API_KEY`.

### Local models

Any OpenAI-compatible server works. Ollama and llama.cpp have presets:

```bash
nitpick --provider ollama -m qwen3-coder:30b
nitpick --provider llamacpp -m whatever-the-server-loaded
nitpick --provider openai --base-url http://localhost:1234/v1 -m local-model   # LM Studio, vLLM, etc.
```

No API key is required for `ollama` and `llamacpp`. Structured output is requested when the server supports it and the response is parsed leniently either way, so smaller models that wrap JSON in prose or use slightly different field names still work.

### Multiple models

Pass `-m` more than once (or a list in the config). Models run in parallel. Findings that agree (same file, nearby lines, similar title) are merged and shown as `(2/3 models)`, which is a useful confidence signal. The overall verdict is derived from the merged findings, not from any single model.

## Debugging

`NITPICK_DEBUG_DIR=/tmp/nitpick-debug nitpick -v` writes every request and raw response to that directory.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
./target/release/nitpick context     # dogfood on your own changes
```

There is no CI; run the checks above locally before committing. `AGENTS.md` has the full guide for coding agents working on this repo.

The crate is organized as: `git` (shelling out to git), `diff` (unified diff parser), `lang` (tree-sitter and import extraction), `search` (ripgrep crates), `context` (the pack builder and budget), `prompt`, `llm` (OpenAI-compatible client with fallbacks), `review` (schema, lenient parsing, merging, rendering), `config`, `main`.

## License

MIT
