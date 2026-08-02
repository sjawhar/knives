import { stat } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const relevantTools = new Set(["read", "grep", "glob", "edit", "write", "apply_patch", "bash"]);
const binaryCacheKey = "__knives_opencode_failed_binaries__";
const warningKey = "__knives_opencode_binary_warning_emitted__";

type ToolInput = {
  readonly tool: string;
  readonly sessionID: string;
  readonly callID: string;
  readonly args: unknown;
};
type ToolOutput = { title: string; output: string; metadata: unknown };
type ShellInput = { readonly cwd: string; readonly sessionID?: string; readonly callID?: string };
type ShellOutput = { readonly env: Record<string, string> };
type SystemInput = { readonly sessionID?: string };
type SystemOutput = { system: string[] };
type ConfigDraft = Record<string, unknown>;
type CompactingInput = { readonly sessionID: string };
type CompactingOutput = { context: string[]; prompt?: string };
type JsonRecord = Record<string, unknown>;

export type KnivesHooks = {
  readonly "tool.execute.after": (input: ToolInput, output: ToolOutput) => Promise<void>;
  readonly "shell.env": (input: ShellInput, output: ShellOutput) => Promise<void>;
  readonly "experimental.chat.system.transform": (
    input: SystemInput,
    output: SystemOutput
  ) => Promise<void>;
  readonly config: (config: ConfigDraft) => Promise<void>;
  readonly "experimental.session.compacting": (
    input: CompactingInput,
    output: CompactingOutput
  ) => Promise<void>;
};

export type Plugin = (
  input: { readonly directory?: string },
  options?: Readonly<Record<string, unknown>>
) => Promise<KnivesHooks>;
export type KnivesOptions = {
  readonly notice: boolean;
  readonly guidance: boolean;
  readonly owner: boolean;
  readonly skills: boolean;
};

type GlobalCarrier = typeof globalThis & {
  [binaryCacheKey]?: Set<string>;
  [warningKey]?: boolean;
};

function isRecord(value: unknown): value is JsonRecord {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function stringValue(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

function stringArray(value: unknown): readonly string[] | null {
  return Array.isArray(value) && value.every((entry) => typeof entry === "string") ? value : null;
}

function failedBinaries(): Set<string> {
  const carrier = globalThis as GlobalCarrier;
  const existing = carrier[binaryCacheKey];
  if (existing !== undefined) return existing;
  const created = new Set<string>();
  carrier[binaryCacheKey] = created;
  return created;
}

function warnOnce(): void {
  const carrier = globalThis as GlobalCarrier;
  if (carrier[warningKey] === true) return;
  carrier[warningKey] = true;
  console.error("knives OpenCode plugin could not run `knives hook opencode`; hooks are disabled");
}

async function isFile(path: string): Promise<boolean> {
  try {
    return (await stat(path)).isFile();
  } catch {
    // no-excuse-ok: catch -- an optional packaged binary must not break plugin loading.
    return false;
  }
}

async function isDirectory(path: string): Promise<boolean> {
  try {
    return (await stat(path)).isDirectory();
  } catch {
    // no-excuse-ok: catch -- an optional packaged skill directory must not break plugin loading.
    return false;
  }
}

export function siblingBinary(modulePath: string): string {
  return resolve(dirname(modulePath), "..", "..", "..", "..", "..", "bin", "knives");
}

async function binary(): Promise<string | null> {
  const configured = stringValue(process.env["KNIVES_BIN"]);
  const sibling = siblingBinary(fileURLToPath(import.meta.url));
  const candidate = configured ?? ((await isFile(sibling)) ? sibling : "knives");
  return failedBinaries().has(candidate) ? null : candidate;
}

function failBinary(candidate: string): null {
  failedBinaries().add(candidate);
  warnOnce();
  return null;
}

async function invoke(request: JsonRecord): Promise<JsonRecord | null> {
  const candidate = await binary();
  if (candidate === null) return null;
  try {
    const child = Bun.spawn([candidate, "hook", "opencode"], {
      stdin: "pipe",
      stderr: "ignore",
      env: process.env,
    });
    child.stdin.write(JSON.stringify(request));
    child.stdin.end();
    const [stdout, exitCode] = await Promise.all([child.stdout.text(), child.exited]);
    if (exitCode !== 0) return failBinary(candidate);
    const parsed: unknown = JSON.parse(stdout);
    return isRecord(parsed) ? parsed : failBinary(candidate);
  } catch {
    // no-excuse-ok: catch -- the plugin boundary intentionally degrades when the optional binary is unavailable.
    return failBinary(candidate);
  }
}

export function readOptions(raw: unknown): KnivesOptions {
  const flag = (name: string): boolean =>
    isRecord(raw) && typeof raw[name] === "boolean" ? raw[name] : true;
  return {
    notice: flag("notice"),
    guidance: flag("guidance"),
    owner: flag("owner"),
    skills: flag("skills"),
  };
}

export async function bundledSkillDirectory(): Promise<string | null> {
  const here = dirname(fileURLToPath(import.meta.url));
  for (const candidate of [
    resolve(here, "..", "..", "skill"),
    resolve(here, "..", "..", "skills"),
  ]) {
    if (await isDirectory(candidate)) return candidate;
  }
  return null;
}

function addSkillPath(config: ConfigDraft, directory: string): void {
  const skills = config["skills"];
  if (!isRecord(skills)) {
    config["skills"] = { paths: [directory] };
    return;
  }
  const paths = skills["paths"];
  if (!Array.isArray(paths) || !paths.every((entry) => typeof entry === "string")) {
    skills["paths"] = [directory];
    return;
  }
  if (!paths.includes(directory)) paths.push(directory);
}

export function createKnivesHooks(
  sessionDirectory: string | undefined,
  options: KnivesOptions
): KnivesHooks {
  return {
    config: async (config) => {
      if (!options.skills) return;
      const directory = await bundledSkillDirectory();
      if (directory !== null) addSkillPath(config, directory);
    },
    "shell.env": async (input, output) => {
      if (!options.owner) return;
      const response = await invoke({ event: "shell.env", cwd: input.cwd });
      const owner = response === null ? null : stringValue(response["owner"]);
      if (owner !== null) output.env["KNIVES_OWNER"] = owner;
    },
    "tool.execute.after": async (input, output) => {
      if (!relevantTools.has(input.tool)) return;
      const response = await invoke({
        event: "tool.execute.after",
        session_id: input.sessionID,
        tool: input.tool,
        args: input.args,
        parts: { notice: options.notice, guidance: options.guidance },
      });
      const addition = response === null ? null : stringValue(response["addition"]);
      if (addition !== null) output.output += addition;
    },
    "experimental.chat.system.transform": async (input, output) => {
      if (input.sessionID === undefined || sessionDirectory === undefined) return;
      const response = await invoke({
        event: "chat.system",
        session_id: input.sessionID,
        directory: sessionDirectory,
      });
      if (response === null) return;
      const system = stringValue(response["system"]);
      const bodies = stringArray(response["bodies"]);
      if (system === null || bodies === null || system.length === 0) return;
      if (
        bodies.length > 0 &&
        bodies.every((body) => output.system.some((entry) => entry.includes(body)))
      )
        return;
      output.system.push(system);
    },
    "experimental.session.compacting": async (input) => {
      await invoke({ event: "compacting", session_id: input.sessionID });
    },
  };
}

export const knivesPlugin: Plugin = async (input, options) =>
  createKnivesHooks(input.directory, readOptions(options));
