import { expect, test } from "bun:test";
import { chmod, mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

import * as knivesEntry from "./knives.ts";
import {
  bundledSkillDirectory,
  createKnivesHooks,
  readOptions,
  relevantTools,
  resolveBinary,
} from "./lib/internals.ts";

// allow: SIZE_OK — Task 8 constrains both fake and real integration layers to this test file.
const realBinary = process.env["KNIVES_BIN"] ?? "";
const originalEnvironment = { ...process.env };
type Repository = { readonly home: string; readonly root: string; readonly file: string };
type Toast = {
  readonly body: {
    readonly title?: string;
    readonly message: string;
    readonly variant: "info" | "success" | "warning" | "error";
  };
};
const mockRecordGuard =
  'case "$MOCK_RECORD" in\n  /*) ;;\n  *) printf \'%s\\n\' "MOCK_RECORD must be an absolute path" >&2; exit 1 ;;\nesac\n';

async function write(path: string, contents: string): Promise<void> {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, contents);
}

function gitRepository(root: string, remotes: ReadonlyArray<readonly [string, string]>): void {
  const init = Bun.spawnSync(["git", "-C", root, "init", "--quiet"]);
  if (init.exitCode !== 0) throw new Error(`git init failed: ${init.stderr.toString()}`);
  for (const [name, url] of remotes) {
    const added = Bun.spawnSync(["git", "-C", root, "remote", "add", name, url]);
    if (added.exitCode !== 0) throw new Error(`git remote add failed: ${added.stderr.toString()}`);
  }
}

async function repository(): Promise<Repository> {
  const home = await mkdtemp(join(tmpdir(), "knives-plugin-"));
  const root = join(home, "managed");
  const file = join(root, "src", "file.ts");
  await write(join(root, "AGENTS.md"), "PLUGIN_GUIDANCE");
  await write(file, "export {}\n");
  // Managed (upstream matches the entry) and trusted (origin under a trusted owner).
  gitRepository(root, [
    ["upstream", "https://forge.invalid/maintainer/managed"],
    ["origin", "https://forge.invalid/ours/managed"],
  ]);
  await write(
    join(home, "repos.toml"),
    `[repos.managed]\nupstream = "https://forge.invalid/maintainer/managed"\norigin = "https://forge.invalid/ours/managed"\n\n[trust]\nowners = ["ours"]\n`
  );
  return { home, root, file };
}

async function withRepository(run: (value: Repository) => Promise<void>): Promise<void> {
  const value = await repository();
  try {
    await run(value);
  } finally {
    await rm(value.home, { recursive: true, force: true });
  }
}

async function mockBinary(): Promise<{ readonly binary: string; readonly record: string }> {
  const directory = await mkdtemp(join(tmpdir(), "knives-mock-"));
  const binary = join(directory, "mock-knives.sh");
  const record = join(directory, "requests");
  await write(
    binary,
    `#!/bin/sh\n${mockRecordGuard}printf '%s\\n' "$@" >> "$MOCK_RECORD.args"\ninput=$(cat)\nprintf '%s\\n' "$input" >> "$MOCK_RECORD.stdin"\nprintf '%s' "$MOCK_RESPONSE"\n`
  );
  await chmod(binary, 0o755);
  return { binary, record };
}

async function oldBinary(): Promise<{ readonly binary: string; readonly record: string }> {
  const directory = await mkdtemp(join(tmpdir(), "knives-old-binary-"));
  const binary = join(directory, "old-knives.sh");
  const record = join(directory, "requests");
  await write(
    binary,
    `#!/bin/sh\n${mockRecordGuard}printf '%s\\n' "$@" >> "$MOCK_RECORD.args"\ncat >/dev/null\nprintf '%s\\n' "error: unrecognized subcommand 'hook'" >&2\nprintf '%s\\n' "CHILD_STDERR_MUST_NOT_LEAK" >&2\nexit 2\n`
  );
  await chmod(binary, 0o755);
  return { binary, record };
}

async function immediatelyExitingBinary(): Promise<string> {
  const directory = await mkdtemp(join(tmpdir(), "knives-exiting-binary-"));
  const binary = join(directory, "exiting-knives.sh");
  await write(binary, "#!/bin/sh\nexit 0\n");
  await chmod(binary, 0o755);
  return binary;
}

function restoreEnvironment(): void {
  for (const key of Object.keys(process.env)) {
    if (!(key in originalEnvironment)) delete process.env[key];
  }
  Object.assign(process.env, originalEnvironment);
}

function resetBinaryFailureState(): void {
  Reflect.deleteProperty(globalThis, "__knives_opencode_failed_binaries__");
  Reflect.deleteProperty(globalThis, "__knives_opencode_binary_warning_emitted__");
  Reflect.deleteProperty(globalThis, "__knives_opencode_inflight__");
}

async function mockRequest(
  response: string,
  run: (record: string) => Promise<void>
): Promise<void> {
  const mock = await mockBinary();
  process.env["KNIVES_BIN"] = mock.binary;
  process.env["MOCK_RECORD"] = mock.record;
  process.env["MOCK_RESPONSE"] = response;
  try {
    await run(mock.record);
  } finally {
    restoreEnvironment();
    await rm(dirname(mock.binary), { recursive: true, force: true });
  }
}

function output(): { title: string; output: string; metadata: unknown } {
  return { title: "tool", output: "tool output", metadata: null };
}

async function realHook(
  input: Record<string, unknown>
): Promise<{ readonly status: number; readonly output: string }> {
  const child = Bun.spawn([realBinary, "hook", "opencode"], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
    env: process.env,
  });
  await child.stdin.write(JSON.stringify(input));
  await child.stdin.end();
  const [status, output] = await Promise.all([
    child.exited,
    child.stdout.text(),
    child.stderr.text(),
  ]);
  return { status, output };
}

test.serial("does not spawn for an irrelevant tool", async () => {
  await mockRequest('{"addition":"unused"}', async (record) => {
    const result = output();
    await createKnivesHooks(undefined, readOptions(undefined))["tool.execute.after"](
      { tool: "webfetch", sessionID: "s", callID: "c", args: {} },
      result
    );
    expect(result.output).toBe("tool output");
    await expect(readFile(`${record}.stdin`, "utf8")).rejects.toThrow();
  });
});

test("the OMP adapter shares the plugin's relevant-tool list", () => {
  expect([...relevantTools].sort()).toEqual(
    ["apply_patch", "bash", "edit", "glob", "grep", "read", "write"].sort()
  );
});

test.serial("forwards tool input and appends an addition verbatim", async () => {
  await mockRequest('{"addition":"\\n\\nFROM_BINARY"}', async (record) => {
    const result = output();
    await createKnivesHooks(undefined, readOptions({ notice: false, guidance: true }))[
      "tool.execute.after"
    ]({ tool: "apply_patch", sessionID: "s", callID: "c", args: { filePath: "/tmp/a" } }, result);
    expect(result.output).toBe("tool output\n\nFROM_BINARY");
    expect(await readFile(`${record}.args`, "utf8")).toBe("hook\nopencode\n");
    expect(JSON.parse(await readFile(`${record}.stdin`, "utf8"))).toEqual({
      event: "tool.execute.after",
      session_id: "s",
      tool: "apply_patch",
      args: { filePath: "/tmp/a" },
      parts: { notice: false, guidance: true },
    });
  });
});

test.serial("suppresses system guidance only when every body is already present", async () => {
  await mockRequest('{"system":"SYSTEM","bodies":["BODY"]}', async () => {
    const hooks = createKnivesHooks("/repo", readOptions(undefined));
    const duplicate = { system: ["contains BODY"] };
    const missing = { system: ["other"] };
    await hooks["experimental.chat.system.transform"]({ sessionID: "s" }, duplicate);
    await hooks["experimental.chat.system.transform"]({ sessionID: "s" }, missing);
    expect(duplicate.system).toEqual(["contains BODY"]);
    expect(missing.system).toEqual(["other", "SYSTEM"]);
  });
});

test.serial("adds mention-only system guidance when the binary returns no bodies", async () => {
  await mockRequest('{"system":"SYSTEM","bodies":[]}', async () => {
    const result = { system: ["core"] };
    await createKnivesHooks("/repo", readOptions(undefined))["experimental.chat.system.transform"](
      { sessionID: "s" },
      result
    );
    expect(result.system).toEqual(["core", "SYSTEM"]);
  });
});

test.serial("sets owner, forwards compaction, and skips chat without a session", async () => {
  await mockRequest('{"owner":"owner-from-binary"}', async (record) => {
    const hooks = createKnivesHooks("/repo", readOptions(undefined));
    const shell = { env: {} as Record<string, string> };
    await hooks["shell.env"]({ cwd: "/repo" }, shell);
    await hooks["experimental.session.compacting"]({ sessionID: "s" }, { context: [] });
    await hooks["experimental.chat.system.transform"]({}, { system: [] });
    expect(shell.env["KNIVES_OWNER"]).toBe("owner-from-binary");
    const events = (await readFile(`${record}.stdin`, "utf8"))
      .trim()
      .split("\n")
      .map((entry) => JSON.parse(entry));
    expect(events).toEqual([
      { event: "shell.env", cwd: "/repo" },
      { event: "compacting", session_id: "s" },
    ]);
  });
});

test.serial("does not export an empty owner", async () => {
  await mockRequest('{"owner":""}', async () => {
    const shell = { env: {} as Record<string, string> };
    await createKnivesHooks("/repo", readOptions(undefined))["shell.env"]({ cwd: "/repo" }, shell);
    expect(shell.env).toEqual({});
  });
});

test.serial("does not spawn shell owner lookup when owner is disabled", async () => {
  await mockRequest('{"owner":"owner-from-binary"}', async (record) => {
    const shell = { env: {} as Record<string, string> };
    await createKnivesHooks("/repo", readOptions({ owner: false }))["shell.env"](
      { cwd: "/repo" },
      shell
    );
    expect(shell.env).toEqual({});
    await expect(readFile(`${record}.stdin`, "utf8")).rejects.toThrow();
  });
});

test.serial("fails soft once without inheriting an old binary's stderr", async () => {
  const old = await oldBinary();
  const warnings: string[] = [];
  const originalError = console.error;
  resetBinaryFailureState();
  process.env["KNIVES_BIN"] = old.binary;
  process.env["MOCK_RECORD"] = old.record;
  console.error = (message?: unknown) => warnings.push(String(message));
  try {
    const hooks = createKnivesHooks("/repo", readOptions(undefined));
    const tool = output();
    const shell = { env: {} as Record<string, string> };
    await hooks["tool.execute.after"](
      { tool: "read", sessionID: "s", callID: "c", args: {} },
      tool
    );
    await hooks["shell.env"]({ cwd: "/repo" }, shell);
    await hooks["experimental.chat.system.transform"]({ sessionID: "s" }, { system: [] });
    await hooks["experimental.session.compacting"]({ sessionID: "s" }, { context: [] });
    expect(tool.output).toBe("tool output");
    expect(shell.env).toEqual({});
    expect(await readFile(`${old.record}.args`, "utf8")).toBe("hook\nopencode\n");
    expect(warnings).toHaveLength(1);
    expect(warnings[0]).toContain(old.binary);
    expect(warnings[0]).toContain("ran but exited nonzero");
    expect(warnings[0]).toContain("likely too old for this plugin");
    expect(warnings[0]).toContain("needs the `hook` subcommand");
    expect(warnings[0]).toContain("update knives or set KNIVES_BIN");
    expect(warnings[0]).toContain("error: unrecognized subcommand 'hook'");
    expect(warnings[0]).not.toContain("CHILD_STDERR_MUST_NOT_LEAK");
  } finally {
    console.error = originalError;
    restoreEnvironment();
    resetBinaryFailureState();
    await rm(dirname(old.binary), { recursive: true, force: true });
  }
});

test.serial(
  "shows one warning toast for binary failures when the client is available",
  async () => {
    const first = await oldBinary();
    const second = await oldBinary();
    const toasts: Toast[] = [];
    const errors: string[] = [];
    const originalError = console.error;
    const client = {
      tui: {
        showToast: async (toast: Toast) => {
          toasts.push(toast);
        },
      },
    };
    resetBinaryFailureState();
    console.error = (message?: unknown) => errors.push(String(message));
    try {
      const hooks = createKnivesHooks("/repo", readOptions(undefined), client);
      process.env["KNIVES_BIN"] = first.binary;
      process.env["MOCK_RECORD"] = first.record;
      await hooks["tool.execute.after"](
        { tool: "read", sessionID: "first", callID: "c", args: {} },
        output()
      );
      process.env["KNIVES_BIN"] = second.binary;
      process.env["MOCK_RECORD"] = second.record;
      await hooks["tool.execute.after"](
        { tool: "read", sessionID: "second", callID: "c", args: {} },
        output()
      );

      const [toast] = toasts;
      expect(toasts).toHaveLength(1);
      expect(toast?.body).toMatchObject({ title: "knives", variant: "warning" });
      expect(toast?.body.message).toStartWith("Ran but exited nonzero:");
      expect(toast?.body.message).toContain(first.binary);
      expect(toast?.body.message).not.toContain(`\`${first.binary}\``);
      expect(toast?.body.message).toContain("needs the hook subcommand");
      expect(toast?.body.message).toContain("update knives or set KNIVES_BIN");
      expect(toast?.body.message).toContain("error: unrecognized subcommand 'hook'");
      expect(errors).toEqual([]);
    } finally {
      console.error = originalError;
      restoreEnvironment();
      resetBinaryFailureState();
      await rm(dirname(first.binary), { recursive: true, force: true });
      await rm(dirname(second.binary), { recursive: true, force: true });
    }
  }
);

test.serial("fails soft once when the client toast throws synchronously", async () => {
  const attempts: Toast[] = [];
  const errors: string[] = [];
  const originalError = console.error;
  const client = {
    tui: {
      showToast: (toast: Toast) => {
        attempts.push(toast);
        throw new Error("toast client failed");
      },
    },
  };
  resetBinaryFailureState();
  process.env["KNIVES_BIN"] = "/definitely/not/knives-sync-throw";
  console.error = (message?: unknown) => errors.push(String(message));
  try {
    const hooks = createKnivesHooks("/repo", readOptions(undefined), client);
    const result = output();
    await hooks["tool.execute.after"](
      { tool: "read", sessionID: "first", callID: "c", args: {} },
      result
    );
    await hooks["tool.execute.after"](
      { tool: "read", sessionID: "second", callID: "c", args: {} },
      output()
    );
    expect(result.output).toBe("tool output");
    expect(attempts).toHaveLength(1);
    expect(errors).toEqual([]);
  } finally {
    console.error = originalError;
    restoreEnvironment();
    resetBinaryFailureState();
  }
});

test.serial("fails soft once when the client toast returns a non-thenable", async () => {
  const toasts: Toast[] = [];
  const errors: string[] = [];
  const originalError = console.error;
  const client = {
    tui: {
      showToast: async (toast: Toast) => {
        toasts.push(toast);
      },
    },
  };
  Reflect.set(client.tui, "showToast", (toast: Toast) => {
    toasts.push(toast);
    return undefined;
  });
  resetBinaryFailureState();
  process.env["KNIVES_BIN"] = "/definitely/not/knives-non-thenable";
  console.error = (message?: unknown) => errors.push(String(message));
  try {
    const hooks = createKnivesHooks("/repo", readOptions(undefined), client);
    const result = output();
    await hooks["tool.execute.after"](
      { tool: "read", sessionID: "first", callID: "c", args: {} },
      result
    );
    await hooks["tool.execute.after"](
      { tool: "read", sessionID: "second", callID: "c", args: {} },
      output()
    );
    expect(result.output).toBe("tool output");
    expect(toasts).toHaveLength(1);
    expect(toasts[0]?.body.message).toBe(
      "Could not start /definitely/not/knives-non-thenable: binary is missing; update knives or set KNIVES_BIN."
    );
    expect(errors).toEqual([]);
  } finally {
    console.error = originalError;
    restoreEnvironment();
    resetBinaryFailureState();
  }
});

test.serial("a_rejecting_toast_promise_never_surfaces_as_an_unhandled_rejection", async () => {
  const toasts: Toast[] = [];
  const errors: string[] = [];
  const unhandled: unknown[] = [];
  const originalError = console.error;
  const captureUnhandled = (reason: unknown) => unhandled.push(reason);
  const client = {
    tui: {
      showToast: async (toast: Toast) => {
        toasts.push(toast);
        return Promise.reject(new Error("toast transport down"));
      },
    },
  };
  resetBinaryFailureState();
  process.env["KNIVES_BIN"] = "/definitely/not/knives-rejecting-toast";
  console.error = (message?: unknown) => errors.push(String(message));
  process.on("unhandledRejection", captureUnhandled);
  try {
    const hooks = createKnivesHooks("/repo", readOptions(undefined), client);
    const result = output();
    await hooks["tool.execute.after"](
      { tool: "read", sessionID: "first", callID: "c", args: {} },
      result
    );
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    expect(result.output).toBe("tool output");
    expect(toasts).toHaveLength(1);
    expect(toasts[0]?.body).toMatchObject({ title: "knives", variant: "warning" });
    expect(unhandled).toEqual([]);
    expect(errors).toEqual([]);
  } finally {
    process.off("unhandledRejection", captureUnhandled);
    console.error = originalError;
    restoreEnvironment();
    resetBinaryFailureState();
  }
});

test.serial("threads the OpenCode client through knivesPlugin binary warnings", async () => {
  const directory = "/repo";
  const toasts: Toast[] = [];
  const errors: string[] = [];
  const originalError = console.error;
  const client = {
    tui: {
      showToast: async (toast: Toast) => {
        toasts.push(toast);
      },
    },
  };
  resetBinaryFailureState();
  process.env["KNIVES_BIN"] = "/definitely/not/knives-plugin-entry";
  console.error = (message?: unknown) => errors.push(String(message));
  try {
    const hooks = await knivesEntry.knivesPlugin({ directory, client }, {});
    await hooks["tool.execute.after"](
      { tool: "read", sessionID: "entry", callID: "c", args: {} },
      output()
    );
    expect(toasts).toHaveLength(1);
    expect(toasts[0]?.body).toMatchObject({ title: "knives", variant: "warning" });
    expect(errors).toEqual([]);
  } finally {
    console.error = originalError;
    restoreEnvironment();
    resetBinaryFailureState();
  }
});

test.serial("fails soft when the configured binary is absent", async () => {
  const warnings: string[] = [];
  const originalError = console.error;
  resetBinaryFailureState();
  process.env["KNIVES_BIN"] = "/definitely/not/knives";
  console.error = (message?: unknown) => warnings.push(String(message));
  try {
    const hooks = createKnivesHooks("/repo", readOptions(undefined));
    const tool = output();
    const shell = { env: {} as Record<string, string> };
    await hooks["tool.execute.after"](
      { tool: "read", sessionID: "s", callID: "c", args: {} },
      tool
    );
    await hooks["shell.env"]({ cwd: "/repo" }, shell);
    await hooks["experimental.chat.system.transform"]({ sessionID: "s" }, { system: [] });
    await hooks["experimental.session.compacting"]({ sessionID: "s" }, { context: [] });
    expect(tool.output).toBe("tool output");
    expect(shell.env).toEqual({});
    expect(warnings).toHaveLength(1);
    expect(warnings[0]).toContain("could not start");
    expect(warnings[0]).toContain("binary is missing");
  } finally {
    console.error = originalError;
    restoreEnvironment();
    resetBinaryFailureState();
  }
});

test.serial("contains an early stdin close without an unhandled rejection", async () => {
  const binary = await immediatelyExitingBinary();
  const warnings: string[] = [];
  const unhandled: unknown[] = [];
  const originalError = console.error;
  const captureUnhandled = (reason: unknown) => unhandled.push(reason);
  resetBinaryFailureState();
  process.env["KNIVES_BIN"] = binary;
  console.error = (message?: unknown) => warnings.push(String(message));
  process.on("unhandledRejection", captureUnhandled);
  try {
    const result = output();
    await createKnivesHooks("/repo", readOptions(undefined))["tool.execute.after"](
      {
        tool: "read",
        sessionID: "s",
        callID: "c",
        args: { payload: "x".repeat(16 * 1024 * 1024) },
      },
      result
    );
    await new Promise<void>((resolve) => setImmediate(resolve));
    expect(result.output).toBe("tool output");
    expect(warnings).toHaveLength(1);
    expect(unhandled).toEqual([]);
  } finally {
    process.off("unhandledRejection", captureUnhandled);
    console.error = originalError;
    restoreEnvironment();
    resetBinaryFailureState();
    await rm(dirname(binary), { recursive: true, force: true });
  }
});

test("keeps config skills purely in TypeScript", async () => {
  const directory = await bundledSkillDirectory();
  if (directory === null) throw new Error("bundled skills directory is missing");
  const config: Record<string, unknown> = {};
  await createKnivesHooks(undefined, readOptions(undefined)).config(config);
  expect((config["skills"] as { paths: string[] }).paths).toContain(directory);
  const disabled: Record<string, unknown> = {};
  await createKnivesHooks(undefined, readOptions({ skills: false })).config(disabled);
  expect(disabled).toEqual({});
});

test("exports only the plugin entry point", () => {
  expect(Object.keys(knivesEntry)).toEqual(["knivesPlugin"]);
});

test("keeps option defaults independent from binary discovery", () => {
  expect(readOptions({ guidance: false })).toEqual({
    notice: true,
    guidance: false,
    owner: true,
    skills: true,
  });
});

test.serial("resolves packaged and development binaries through real file probes", async () => {
  const root = await mkdtemp(join(tmpdir(), "knives-binary-layout-"));
  const packagedModule = join(
    root,
    "prefix",
    "share",
    "knives",
    "opencode",
    "plugins",
    "lib",
    "internals.ts"
  );
  const packagedBinary = join(root, "prefix", "bin", "knives");
  const packagedDevelopmentBinary = join(
    root,
    "prefix",
    "share",
    "knives",
    "opencode",
    "target",
    "debug",
    "knives"
  );
  const developmentModule = join(root, "development", "plugin", "lib", "internals.ts");
  const developmentBinary = join(root, "development", "target", "debug", "knives");
  await write(packagedModule, "");
  await write(packagedBinary, "#!/bin/sh\n");
  await chmod(packagedBinary, 0o755);
  await write(packagedDevelopmentBinary, "#!/bin/sh\n");
  await chmod(packagedDevelopmentBinary, 0o755);
  await write(developmentModule, "");
  await write(developmentBinary, "#!/bin/sh\n");
  await chmod(developmentBinary, 0o755);
  process.env["KNIVES_BIN"] = "";
  try {
    expect(await resolveBinary(packagedModule)).toBe(packagedBinary);
    await rm(packagedBinary);
    expect(await resolveBinary(packagedModule)).toBe(packagedDevelopmentBinary);
    await rm(packagedDevelopmentBinary);
    expect(await resolveBinary(packagedModule)).toBe("knives");
    expect(await resolveBinary(developmentModule)).toBe(developmentBinary);
    await rm(developmentBinary);
    expect(await resolveBinary(developmentModule)).toBe("knives");
  } finally {
    restoreEnvironment();
    await rm(root, { recursive: true, force: true });
  }
});

test.serial.skipIf(realBinary.length === 0)(
  "adds managed chat guidance from the real binary",
  async () => {
    try {
      await withRepository(async ({ home, root }) => {
        process.env["KNIVES_CONFIG_HOME"] = home;
        const request = { event: "chat.system", session_id: "chat", directory: root };
        const response = await realHook(request);
        const payload = JSON.parse(response.output);
        expect(response.status).toBe(0);
        expect(payload.system).toContain("<knives-guidance-");
        expect(payload.bodies).not.toHaveLength(0);

        const system = { system: [] as string[] };
        await createKnivesHooks(root, readOptions(undefined))["experimental.chat.system.transform"](
          { sessionID: "chat" },
          system
        );
        expect(system.system).toHaveLength(1);
        expect(system.system[0]).toContain("<knives-guidance-");
      });
    } finally {
      restoreEnvironment();
    }
  }
);

test.serial.skipIf(realBinary.length === 0)(
  "passes through unmanaged chat responses without a shim insertion",
  async () => {
    try {
      await withRepository(async ({ home }) => {
        const unmanaged = join(home, "unmanaged");
        await write(join(unmanaged, "file.ts"), "export {}\n");
        process.env["KNIVES_CONFIG_HOME"] = home;
        const request = { event: "chat.system", session_id: "chat", directory: unmanaged };
        const response = await realHook(request);
        expect(response.status).toBe(0);
        expect(JSON.parse(response.output)).toEqual({ system: "", bodies: [] });

        const system = { system: ["base"] };
        await createKnivesHooks(unmanaged, readOptions(undefined))[
          "experimental.chat.system.transform"
        ]({ sessionID: "chat" }, system);
        expect(system.system).toEqual(["base"]);
      });
    } finally {
      restoreEnvironment();
    }
  }
);

test.serial.skipIf(realBinary.length === 0)("injects once through the real binary", async () => {
  try {
    await withRepository(async ({ home, root, file }) => {
      process.env["KNIVES_CONFIG_HOME"] = home;
      const hooks = createKnivesHooks(undefined, readOptions(undefined));
      const first = output();
      const second = output();
      await hooks["tool.execute.after"](
        { tool: "read", sessionID: "real", callID: "one", args: { filePath: file } },
        first
      );
      await hooks["tool.execute.after"](
        { tool: "read", sessionID: "real", callID: "two", args: { filePath: file } },
        second
      );
      expect(first.output).toContain("<knives-notice-");
      expect(first.output).toContain("PLUGIN_GUIDANCE");
      expect(second.output).toBe("tool output");
      const outside = join(home, "outside");
      await write(join(outside, "AGENTS.md"), "OUTSIDE");
      await symlink(outside, join(root, "outside-link"));
      const escaped = output();
      await hooks["tool.execute.after"](
        {
          tool: "apply_patch",
          sessionID: "escape",
          callID: "three",
          args: { filePath: join(root, "outside-link", "file.ts") },
        },
        escaped
      );
      expect(escaped.output).toBe("tool output");
    });
  } finally {
    restoreEnvironment();
  }
});

test.serial.skipIf(realBinary.length === 0)(
  "preserves the binary budget after pathless bash",
  async () => {
    try {
      await withRepository(async ({ home, file }) => {
        process.env["KNIVES_CONFIG_HOME"] = home;
        const hooks = createKnivesHooks(undefined, readOptions(undefined));
        const pathless = output();
        const named = output();
        await hooks["tool.execute.after"](
          { tool: "bash", sessionID: "budget", callID: "one", args: { command: "gh pr list" } },
          pathless
        );
        await hooks["tool.execute.after"](
          { tool: "apply_patch", sessionID: "budget", callID: "two", args: { filePath: file } },
          named
        );
        expect(pathless.output).toBe("tool output");
        expect(named.output).toContain("PLUGIN_GUIDANCE");
      });
    } finally {
      restoreEnvironment();
    }
  }
);

test.serial.skipIf(realBinary.length === 0)(
  "fails closed and keeps valid roots through the real binary",
  async () => {
    try {
      await withRepository(async ({ home, file }) => {
        process.env["KNIVES_CONFIG_HOME"] = home;
        const hooks = createKnivesHooks(undefined, readOptions(undefined));
        await rm(join(home, "repos.toml"));
        const missing = output();
        await hooks["tool.execute.after"](
          { tool: "read", sessionID: "missing", callID: "one", args: { filePath: file } },
          missing
        );
        expect(missing.output).toBe("tool output");
        await write(join(home, "repos.toml"), "[[[invalid");
        const malformed = output();
        await hooks["tool.execute.after"](
          { tool: "read", sessionID: "malformed", callID: "two", args: { filePath: file } },
          malformed
        );
        expect(malformed.output).toBe("tool output");
        const trusted = join(home, "trusted");
        await write(join(trusted, "AGENTS.md"), "TRUSTED_GUIDANCE");
        gitRepository(trusted, [["origin", "https://forge.invalid/company/work"]]);
        await write(
          join(home, "repos.toml"),
          `[repos.gone]\nupstream = "https://forge.invalid/maintainer/gone"\norigin = "https://forge.invalid/ours/gone"\n\n[repos.managed]\nupstream = "https://forge.invalid/maintainer/managed"\norigin = "https://forge.invalid/ours/managed"\n\n[trust]\nowners = ["ours"]\nrepos = ["company/work"]\n`
        );
        const valid = output();
        await hooks["tool.execute.after"](
          { tool: "apply_patch", sessionID: "valid", callID: "three", args: { filePath: file } },
          valid
        );
        expect(valid.output).toContain("PLUGIN_GUIDANCE");
      });
    } finally {
      restoreEnvironment();
    }
  }
);

test.serial("a hung binary is killed at the invoke timeout without being condemned", async () => {
  // The 2026-08-25 devbox collapse: a hook child that never finishes must not
  // outlive its handler, and a timeout is load, not a broken binary.
  resetBinaryFailureState();
  const directory = await mkdtemp(join(tmpdir(), "knives-hung-binary-"));
  const binary = join(directory, "hung-knives.sh");
  const pids = join(directory, "pids");
  const respond = join(directory, "respond");
  await write(
    binary,
    `#!/bin/sh\nif [ -f "${respond}" ]; then\n  cat >/dev/null\n  printf '%s' '{"addition":"REVIVED"}'\n  exit 0\nfi\nprintf '%s\\n' "$$" >> "${pids}"\nexec sleep 30\n`
  );
  await chmod(binary, 0o755);
  process.env["KNIVES_BIN"] = binary;
  process.env["KNIVES_INVOKE_TIMEOUT_MS"] = "250";
  const warnings: string[] = [];
  const originalError = console.error;
  console.error = (message?: unknown) => warnings.push(String(message));
  try {
    const hooks = createKnivesHooks("/repo", readOptions(undefined));
    const started = Date.now();
    const result = output();
    await hooks["tool.execute.after"](
      { tool: "read", sessionID: "s", callID: "c", args: {} },
      result
    );
    expect(Date.now() - started).toBeLessThan(5_000);
    expect(result.output).toBe("tool output");
    expect(warnings).toEqual([]);
    // The child is dead, not merely raced past: its PID no longer exists.
    const recorded = (await readFile(pids, "utf8")).trim().split("\n");
    expect(recorded).toHaveLength(1);
    const pid = Number.parseInt(recorded[0] ?? "", 10);
    expect(pid).toBeGreaterThan(0);
    expect(() => process.kill(pid, 0)).toThrow();
    // And the candidate was not condemned: the very same path answers the next
    // invocation once it behaves.
    await writeFile(respond, "");
    const revived = output();
    await hooks["tool.execute.after"](
      { tool: "read", sessionID: "s", callID: "c2", args: {} },
      revived
    );
    expect(revived.output).toBe("tool outputREVIVED");
  } finally {
    console.error = originalError;
    restoreEnvironment();
    resetBinaryFailureState();
    await rm(directory, { recursive: true, force: true });
  }
});

test.serial("in-flight invocations are capped so slow children cannot pile up", async () => {
  resetBinaryFailureState();
  const directory = await mkdtemp(join(tmpdir(), "knives-slow-binary-"));
  const binary = join(directory, "slow-knives.sh");
  const record = join(directory, "started");
  await write(binary, `#!/bin/sh\nprintf 'x\\n' >> "${record}"\nexec sleep 30\n`);
  await chmod(binary, 0o755);
  process.env["KNIVES_BIN"] = binary;
  // Generous kill window so all four admitted children reliably record their
  // start even on a loaded runner; the cap assertion is what matters here.
  process.env["KNIVES_INVOKE_TIMEOUT_MS"] = "2000";
  try {
    const hooks = createKnivesHooks("/repo", readOptions(undefined));
    await Promise.all(
      Array.from({ length: 6 }, () =>
        hooks["experimental.session.compacting"]({ sessionID: "s" }, { context: [] })
      )
    );
    const started = await readFile(record, "utf8");
    expect(started.split("\n").filter((line) => line.length > 0)).toHaveLength(4);
    // The gate is an in-flight limit, not a latch: once the timed-out children
    // are gone, the next invocation is admitted again.
    process.env["KNIVES_INVOKE_TIMEOUT_MS"] = "250";
    await hooks["experimental.session.compacting"]({ sessionID: "s" }, { context: [] });
    const after = await readFile(record, "utf8");
    expect(after.split("\n").filter((line) => line.length > 0)).toHaveLength(5);
  } finally {
    restoreEnvironment();
    resetBinaryFailureState();
    await rm(directory, { recursive: true, force: true });
  }
});
