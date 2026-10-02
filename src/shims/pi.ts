// nitpick for pi: background code review while the agent works.
// Installed by `nitpick watch install pi`. Needs the `nitpick` binary on PATH.
// Docs: https://github.com/LVTD-LLC/nitpick#watch
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

type HookOut = { context?: string | null; block?: boolean; reason?: string | null; note?: string | null };

export default function (pi: ExtensionAPI) {
  const call = async (event: string, cwd: string, tool?: string, timeout = 15_000): Promise<HookOut | null> => {
    const args = ["hook", "generic", event, "--cwd", cwd];
    if (tool) args.push("--tool", tool);
    try {
      const r = await pi.exec("nitpick", args, { cwd, timeout });
      if (r.code !== 0 || !r.stdout.trim()) return null;
      return JSON.parse(r.stdout) as HookOut;
    } catch {
      return null;
    }
  };

  pi.on("session_start", async (_event, ctx) => {
    const out = await call("session-start", ctx.cwd);
    if (out?.context) {
      pi.sendMessage({ customType: "nitpick", content: out.context, display: false, details: undefined }, { deliverAs: "nextTurn" });
    }
  });

  // After every tool call: record the edit, start the background worker if
  // needed, and append any waiting findings to the tool result so the model
  // sees them with its next step.
  pi.on("tool_result", async (event, ctx) => {
    const out = await call("tool", ctx.cwd, event.toolName);
    if (out?.context) {
      return { content: [...event.content, { type: "text" as const, text: out.context }] };
    }
  });

  // The agent is about to finish: review what is left and, if there is a
  // high finding, hand it over and ask for one more pass.
  pi.on("agent_before_settle", async (event, ctx) => {
    if (event.outcome !== "completed" || event.continue) return;
    const out = await call("stop", ctx.cwd, undefined, 600_000);
    if (out?.block && out.reason) {
      pi.sendMessage({ customType: "nitpick", content: out.reason, display: true, details: undefined }, { deliverAs: "nextTurn" });
      return { continue: true };
    }
    if (out?.note && ctx.hasUI) ctx.ui.notify(out.note, "info");
  });
}
