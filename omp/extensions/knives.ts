import type {
  ExtensionAPI,
  ExtensionContext,
} from "@oh-my-pi/pi-coding-agent/extensibility/extensions/types";
import type { SessionEntry } from "@oh-my-pi/pi-coding-agent/session/session-entries";

import {
  bundledSkillDirectory,
  createKnivesHooks,
  type KnivesHooks,
  type ModelContext,
  readOptions,
  relevantTools,
} from "../../plugin/lib/internals.ts";

// The envelope the binary wraps each guidance block in; its nonce is unique per injection.
const guidanceEnvelope =
  /<knives-guidance-([0-9a-f]+) repo="[^"]*">[\s\S]*?<\/knives-guidance-\1>/g;

/**
 * What the tool hook needs to know about the model's context at this call: the turn that made it
 * (the id of the assistant message holding the call) and the nonces of the knives guidance blocks
 * still in the tool results the model sees. omp's shake, pruning and compaction rewrite those
 * results with no event the binary could key on, so this is what tells it a block was lost.
 *
 * Only entries after the latest compaction or `/clear` boundary count. A compaction's kept tail
 * may hold a block too; counting it as lost costs one more copy, where counting a lost block as
 * held would leave the session without that guidance.
 */
function modelContext(branch: readonly SessionEntry[], toolCallId: string): ModelContext {
  let turn = toolCallId;
  for (let index = branch.length - 1; index >= 0; index--) {
    const entry = branch[index];
    if (
      entry?.type === "message" &&
      entry.message.role === "assistant" &&
      entry.message.content.some((part) => part.type === "toolCall" && part.id === toolCallId)
    ) {
      turn = entry.id;
      break;
    }
  }
  let start = 0;
  for (let index = branch.length - 1; index >= 0; index--) {
    const type = branch[index]?.type;
    if (type === "compaction" || type === "reset_boundary") {
      start = index + 1;
      break;
    }
  }
  const guidance = new Set<string>();
  for (const entry of branch.slice(start)) {
    if (entry.type !== "message" || entry.message.role !== "toolResult") continue;
    for (const part of entry.message.content) {
      if (part.type !== "text") continue;
      for (const match of part.text.matchAll(guidanceEnvelope)) {
        if (match[1] !== undefined) guidance.add(match[1]);
      }
    }
  }
  return { turn, guidance: [...guidance] };
}

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

  pi.on("tool_result", async (event, ctx: ExtensionContext) => {
    if (!relevantTools.has(event.toolName) || sessionId === undefined || hooks === undefined)
      return;

    const output = { title: "", output: "", metadata: {} };
    await hooks["tool.execute.after"](
      {
        tool: event.toolName,
        sessionID: sessionId,
        // OMP assigns this opaque id to the actual tool call, so preserve it for the hook boundary.
        callID: event.toolCallId,
        args: event.input,
        // What the session already holds — its AGENTS.md, CLAUDE.md, this plugin's own
        // chat guidance — so the binary injects none of it into a tool result again.
        system: ctx.getSystemPrompt(),
        // Which of the blocks it injected the model still sees, so it injects a lost one again.
        context: modelContext(ctx.sessionManager.getBranch(), event.toolCallId),
      },
      output
    );

    if (output.output.length === 0) return;
    return { content: [...event.content, { type: "text", text: output.output }] };
  });

  pi.on("before_agent_start", async (event) => {
    if (sessionId === undefined || hooks === undefined) return;

    // The turn's freshly built base, not `ctx.getSystemPrompt()`: that still holds the previous
    // turn's prompt with this block in it, and returning nothing publishes the base without it.
    const baseSystem = event.systemPrompt;
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
