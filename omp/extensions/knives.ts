import type {
  ExtensionAPI,
  ExtensionContext,
} from "@oh-my-pi/pi-coding-agent/extensibility/extensions/types";

import {
  bundledSkillDirectory,
  createKnivesHooks,
  type KnivesHooks,
  readOptions,
  relevantTools,
} from "../../plugin/lib/internals.ts";

export default function knivesExtension(pi: ExtensionAPI): void {
  const options = readOptions(undefined);
  let sessionId: string | undefined;
  let hooks: KnivesHooks | undefined;

  pi.on("resources_discover", async () => {
    if (!options.skills) return {};
    const directory = await bundledSkillDirectory();
    return directory === null ? {} : { skillPaths: [directory] };
  });

  pi.on("session_start", async (_event, ctx: ExtensionContext) => {
    sessionId = ctx.sessionManager.getSessionId();
    hooks = createKnivesHooks(ctx.cwd, options);
  });

  pi.on("tool_result", async (event) => {
    if (!relevantTools.has(event.toolName) || sessionId === undefined || hooks === undefined)
      return;

    const output = { title: "", output: "", metadata: {} };
    await hooks["tool.execute.after"](
      // OMP assigns this opaque id to the actual tool call, so preserve it for the hook boundary.
      { tool: event.toolName, sessionID: sessionId, callID: event.toolCallId, args: event.input },
      output
    );

    if (output.output.length === 0) return;
    return { content: [...event.content, { type: "text", text: output.output }] };
  });

  pi.on("before_agent_start", async (_event, ctx: ExtensionContext) => {
    if (sessionId === undefined || hooks === undefined) return;

    const baseSystem = ctx.getSystemPrompt();
    const system = [...baseSystem];
    await hooks["experimental.chat.system.transform"]({ sessionID: sessionId }, { system });
    if (system.length === baseSystem.length) return;
    return { systemPrompt: system };
  });

  pi.on("session.compacting", async () => {
    if (sessionId === undefined || hooks === undefined) return;
    await hooks["experimental.session.compacting"]({ sessionID: sessionId }, { context: [] });
  });
}
