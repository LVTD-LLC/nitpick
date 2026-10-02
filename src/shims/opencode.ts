// nitpick for OpenCode: background code review while the agent works.
// Installed by `nitpick watch install opencode`. Needs the `nitpick` binary on PATH.
// Docs: https://github.com/LVTD-LLC/nitpick#watch
import type { Plugin } from "@opencode-ai/plugin";

type HookOut = { context?: string | null; block?: boolean; reason?: string | null; note?: string | null };

export const NitpickPlugin: Plugin = async ({ client, $, directory }) => {
  const call = async (event: string, tool?: string): Promise<HookOut | null> => {
    const args = ["hook", "generic", event, "--cwd", directory];
    if (tool) args.push("--tool", tool);
    try {
      const r = await $`nitpick ${args}`.cwd(directory).quiet().nothrow();
      const text = r.stdout.toString().trim();
      if (r.exitCode !== 0 || !text) return null;
      return JSON.parse(text) as HookOut;
    } catch {
      return null;
    }
  };
  // OpenCode has no stop gate; a re-prompt on idle is how the agent is sent
  // back. nitpick itself caps how often that happens (watch.max_stop_blocks).
  let busy = false;
  return {
    event: async ({ event }) => {
      if (event.type === "session.created") {
        await call("session-start");
      } else if (event.type === "session.idle" && !busy) {
        busy = true;
        try {
          const out = await call("stop");
          if (out?.block && out.reason) {
            const sessionID = (event.properties as { sessionID: string }).sessionID;
            await client.session.prompt({ path: { sessionID }, body: { parts: [{ type: "text", text: out.reason }] } });
          } else if (out?.note) {
            await client.tui.showToast({ body: { message: out.note, variant: "info" } }).catch(() => {});
          }
        } finally {
          busy = false;
        }
      }
    },
    "tool.execute.after": async (input, output) => {
      const out = await call("tool", input.tool);
      if (out?.context) output.output += `\n\n${out.context}`;
    },
  };
};

export default NitpickPlugin;
