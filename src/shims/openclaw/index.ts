// nitpick for OpenClaw: background code review while the agent works.
// Installed by `nitpick watch install openclaw`. Needs the `nitpick` binary on PATH.
// Grant the hooks conversation access in openclaw.json:
//   plugins.entries.nitpick.hooks.allowConversationAccess = true
// Docs: https://github.com/LVTD-LLC/nitpick#watch
import { definePluginEntry } from "openclaw/plugin-sdk/plugin-entry";
import { execFile } from "node:child_process";

type HookOut = { context?: string | null; block?: boolean; reason?: string | null; note?: string | null };

function call(event: string, cwd: string | undefined, tool?: string, timeout = 15_000): Promise<HookOut | null> {
  const args = ["hook", "generic", event];
  if (cwd) args.push("--cwd", cwd);
  if (tool) args.push("--tool", tool);
  return new Promise((resolve) => {
    execFile("nitpick", args, { cwd, timeout }, (err, stdout) => {
      if (err || !String(stdout).trim()) return resolve(null);
      try {
        resolve(JSON.parse(String(stdout)) as HookOut);
      } catch {
        resolve(null);
      }
    });
  });
}

export default definePluginEntry({
  id: "nitpick",
  name: "nitpick",
  description: "Background AI code review of the agent's edits, delivered as [nitpick] notes.",
  register(api) {
    api.on("session_start", async (_event, ctx) => {
      await call("session-start", ctx.workspaceDir);
    });
    api.on("after_tool_call", async (event, ctx) => {
      await call("tool", ctx.workspaceDir, event.toolName);
    });
    api.on("before_prompt_build", async (_event, ctx) => {
      const out = await call("prompt", ctx.workspaceDir);
      if (out?.context) return { prependContext: out.context };
    });
    api.on("before_agent_finalize", async (_event, ctx) => {
      const out = await call("stop", ctx.workspaceDir, undefined, 600_000);
      if (out?.block && out.reason) {
        return { action: "revise", reason: out.reason, retry: { instruction: out.reason, idempotencyKey: "nitpick", maxAttempts: 2 } };
      }
    });
  },
});
