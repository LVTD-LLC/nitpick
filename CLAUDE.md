# CLAUDE.md

Read `AGENTS.md` first; it is the canonical guide for this repo and everything there applies to Claude Code.

Claude-specific notes:

- Run the four check commands from `AGENTS.md` yourself before every commit. There is no CI to catch mistakes.
- The OpenRouter key is not in your environment. Prefix live runs with `source ~/.zshrc`.
- Long model calls (2 to 5 minutes on free models) should run in the background so you can keep working; write their output to a file under the scratchpad and read it when the task notification arrives.
