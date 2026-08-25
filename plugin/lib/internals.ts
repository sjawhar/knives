import { stat } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// allow: SIZE_OK — the adapter's OpenCode hook contracts and binary boundary are one seam.
export const relevantTools: ReadonlySet<string> = new Set([
  "read",
  "grep",
  "glob",
  "edit",
  "write",
  "apply_patch",
  "bash",
]);
const binaryCacheKey = "__knives_opencode_failed_binaries__";
const warningKey = "__knives_opencode_binary_warning_emitted__";
const inflightKey = "__knives_opencode_inflight__";
// One agent session rarely has more than one hook in flight; a saturated gate
// means children are not finishing, and adding more is how a loaded devbox
// became a fork bomb (2026-08-25, ~13k concurrent knives processes). Hooks are
// advisory, so degrading to no response is always safe.
const inflightCap = 4;
const defaultInvokeTimeoutMs = 10_000;

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
type BinaryFailure = "missing" | "outdated" | "invalid_response";
type ToastVariant = "info" | "success" | "warning" | "error";
type KnivesClient = {
  readonly tui: {
    readonly showToast: (options: {
      readonly body: {
        readonly title?: string;
        readonly message: string;
        readonly variant: ToastVariant;
      };
    }) => unknown;
  };
};
type BinaryWarning = {
  readonly candidate: string;
  readonly failure: BinaryFailure;
  readonly stderr: string;
};

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
  input: { readonly directory?: string; readonly client?: KnivesClient },
  options?: Readonly<Record<string, unknown>>
) => Promise<KnivesHooks>;
export type KnivesOptions = {
  readonly notice: boolean;
  readonly guidance: boolean;
  readonly owner: boolean;
  readonly skills: boolean;
};

// The gate's shape is frozen: two independently built plugin versions in one
// process share the globalThis slot, so a shape change silently breaks the cap
// (NaN arithmetic). A key rename is the migration path if it ever must change.
type InflightGate = { count: number };

type GlobalCarrier = typeof globalThis & {
  [binaryCacheKey]?: Set<string>;
  [warningKey]?: boolean;
  [inflightKey]?: InflightGate;
};

function isRecord(value: unknown): value is JsonRecord {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function stringValue(value: unknown): string | null {
  return typeof value === "string" && value.length > 0 ? value : null;
}

function stringArray(value: unknown): readonly string[] | null {
  return Array.isArray(value) && value.every((entry) => typeof entry === "string") ? value : null;
}

function failedBinaries(): Set<string> {
  // Cache only each failed path: a later KNIVES_BIN override selects a fresh candidate.
  // Spawn failures mean that path is missing; a nonzero hook exit usually means an old binary.
  const carrier = globalThis as GlobalCarrier;
  const existing = carrier[binaryCacheKey];
  if (existing !== undefined) return existing;
  const created = new Set<string>();
  carrier[binaryCacheKey] = created;
  return created;
}

function inflightGate(): InflightGate {
  // On globalThis for the same reason as failedBinaries: the module can load
  // more than once in one process, and the cap must hold per process.
  const carrier = globalThis as GlobalCarrier;
  const existing = carrier[inflightKey];
  if (existing !== undefined) return existing;
  const created: InflightGate = { count: 0 };
  carrier[inflightKey] = created;
  return created;
}

function invokeTimeoutMs(): number {
  // Bounded above by the 32-bit timer range: setTimeout clamps larger values
  // to 1ms, which would silently kill every child immediately.
  const parsed = Number.parseInt(process.env["KNIVES_INVOKE_TIMEOUT_MS"] ?? "", 10);
  return parsed > 0 && parsed <= 2_147_483_647 ? parsed : defaultInvokeTimeoutMs;
}

function warnOnce(client: KnivesClient | undefined, warning: BinaryWarning): void {
  const carrier = globalThis as GlobalCarrier;
  if (carrier[warningKey] === true) return;
  carrier[warningKey] = true;
  const firstLine = warning.stderr.split(/\r?\n/, 1)[0] ?? "";
  const detail = JSON.stringify(firstLine).slice(1, -1).trim().slice(0, 120);
  const diagnostic = detail.length === 0 ? "" : ` Detail: ${detail}`;
  const binaryPath = [...warning.candidate]
    .map((character) => {
      const code = character.charCodeAt(0);
      return code < 32 || code === 127 ? " " : character;
    })
    .join("");
  const message =
    warning.failure === "missing"
      ? `could not start \`${binaryPath}\`: binary is missing; update knives or set KNIVES_BIN.`
      : warning.failure === "outdated"
        ? `ran but exited nonzero: \`${binaryPath}\` is likely too old for this plugin (needs the \`hook\` subcommand); update knives or set KNIVES_BIN.`
        : `received no valid hook response from \`${binaryPath}\`; update knives or set KNIVES_BIN.`;
  const toastMessage =
    warning.failure === "missing"
      ? `Could not start ${binaryPath}: binary is missing; update knives or set KNIVES_BIN.`
      : warning.failure === "outdated"
        ? `Ran but exited nonzero: ${binaryPath} is likely too old for this plugin (needs the hook subcommand); update knives or set KNIVES_BIN.`
        : `Received no valid hook response from ${binaryPath}; update knives or set KNIVES_BIN.`;
  if (client !== undefined) {
    try {
      void Promise.resolve(
        client.tui.showToast({
          body: { title: "knives", message: `${toastMessage}${diagnostic}`, variant: "warning" },
        })
      ).catch(() => undefined);
    } catch {
      // no-excuse-ok: catch -- a nonconforming client must not break the hook that warns through it.
    }
    return;
  }
  // Headless callers, tests, and older OpenCode versions have no TUI client, so stderr is their only warning surface.
  console.error(`knives OpenCode plugin ${message}${diagnostic}`);
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

function developmentBinary(modulePath: string): string {
  return resolve(dirname(modulePath), "..", "..", "target", "debug", "knives");
}

export async function resolveBinary(modulePath: string): Promise<string> {
  const configured = stringValue(process.env["KNIVES_BIN"]);
  if (configured !== null) return configured;
  const sibling = siblingBinary(modulePath);
  if (await isFile(sibling)) return sibling;
  // A file:// dev install must use its checkout's build before any stale PATH binary.
  const development = developmentBinary(modulePath);
  if (await isFile(development)) return development;
  return "knives";
}

async function binary(): Promise<string | null> {
  const candidate = await resolveBinary(fileURLToPath(import.meta.url));
  return failedBinaries().has(candidate) ? null : candidate;
}

function failBinary(client: KnivesClient | undefined, warning: BinaryWarning): null {
  failedBinaries().add(warning.candidate);
  warnOnce(client, warning);
  return null;
}

function parsedRecord(text: string): JsonRecord | null {
  try {
    const parsed: unknown = JSON.parse(text);
    return isRecord(parsed) ? parsed : null;
  } catch {
    // no-excuse-ok: catch -- a non-JSON response is classified by the caller.
    return null;
  }
}

async function hookResponse(
  client: KnivesClient | undefined,
  candidate: string,
  request: JsonRecord
): Promise<JsonRecord | null> {
  const child = Bun.spawn([candidate, "hook", "opencode"], {
    stdin: "pipe",
    stderr: "pipe",
    env: process.env,
  });
  // The timer guarantees the child cannot outlive this handler: a harness that
  // times the handler out and abandons it would otherwise leave the child
  // parked in its stdin read forever. SIGKILL, not SIGTERM: the response is
  // already discarded, and releasing the gate slot must not depend on the
  // child's cooperation. A timeout is load, not a broken binary, so it
  // degrades to null without condemning the candidate.
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    child.kill("SIGKILL");
  }, invokeTimeoutMs());
  try {
    let wrote = true;
    try {
      await child.stdin.write(JSON.stringify(request));
      await child.stdin.end();
    } catch {
      // no-excuse-ok: catch -- an old binary closes stdin early; classification happens below.
      wrote = false;
    }
    const [stdout, stderr, exitCode] = await Promise.all([
      child.stdout.text(),
      child.stderr.text(),
      child.exited,
    ]);
    if (timedOut) return null;
    // Exit 3 (Incomplete) is the binary's own watchdog giving up under load —
    // its hook otherwise always exits 0, and an old binary without the hook
    // subcommand fails with clap's usage code 2. Load never condemns a binary.
    if (exitCode === 3) return null;
    if (exitCode !== 0) return failBinary(client, { candidate, failure: "outdated", stderr });
    return (
      (wrote ? parsedRecord(stdout) : null) ??
      failBinary(client, { candidate, failure: "invalid_response", stderr })
    );
  } finally {
    clearTimeout(timer);
  }
}

async function invoke(
  client: KnivesClient | undefined,
  request: JsonRecord
): Promise<JsonRecord | null> {
  const candidate = await binary();
  if (candidate === null) return null;
  const gate = inflightGate();
  if (gate.count >= inflightCap) return null;
  gate.count += 1;
  try {
    // `return await`, not `return`: the catch below must observe a spawn failure.
    return await hookResponse(client, candidate, request);
  } catch {
    // no-excuse-ok: catch -- the plugin boundary intentionally degrades when the optional binary is unavailable.
    return failBinary(client, { candidate, failure: "missing", stderr: "" });
  } finally {
    gate.count -= 1;
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
    resolve(here, "..", "..", "skills"),
    // Legacy archives used `skill`; retain it only after the release `skills` directory.
    resolve(here, "..", "..", "skill"),
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
  options: KnivesOptions,
  client?: KnivesClient
): KnivesHooks {
  return {
    config: async (config) => {
      if (!options.skills) return;
      const directory = await bundledSkillDirectory();
      if (directory !== null) addSkillPath(config, directory);
    },
    "shell.env": async (input, output) => {
      if (!options.owner) return;
      const response = await invoke(client, { event: "shell.env", cwd: input.cwd });
      const owner = response === null ? null : stringValue(response["owner"]);
      if (owner !== null) output.env["KNIVES_OWNER"] = owner;
    },
    "tool.execute.after": async (input, output) => {
      if (!relevantTools.has(input.tool)) return;
      const response = await invoke(client, {
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
      const response = await invoke(client, {
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
      await invoke(client, { event: "compacting", session_id: input.sessionID });
    },
  };
}

export const knivesPlugin: Plugin = async (input, options) =>
  createKnivesHooks(input.directory, readOptions(options), input.client);
