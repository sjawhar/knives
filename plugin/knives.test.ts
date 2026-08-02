import { expect, test } from "bun:test";
import { chmod, mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";

import {
  bundledSkillDirectory,
  createKnivesHooks,
  readOptions,
  siblingBinary,
} from "./lib/internals.ts";

// allow: SIZE_OK — Task 8 constrains both fake and real integration layers to this test file.
const realBinary = process.env["KNIVES_BIN"];
const originalEnvironment = { ...process.env };
type Repository = { readonly home: string; readonly root: string; readonly file: string };

async function write(path: string, contents: string): Promise<void> {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, contents);
}

async function repository(): Promise<Repository> {
  const home = await mkdtemp(join(tmpdir(), "knives-plugin-"));
  const root = join(home, "managed");
  const file = join(root, "src", "file.ts");
  await write(join(root, "AGENTS.md"), "PLUGIN_GUIDANCE");
  await write(file, "export {}\n");
  await write(
    join(home, "repos.toml"),
    `[repos.managed]\npath = "${root}"\nupstream = "u"\norigin = "o"\n`
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
    '#!/bin/sh\nprintf \'%s\\n\' "$@" >> "$MOCK_RECORD.args"\ninput=$(cat)\nprintf \'%s\\n\' "$input" >> "$MOCK_RECORD.stdin"\nprintf \'%s\' "$MOCK_RESPONSE"\n'
  );
  await chmod(binary, 0o755);
  return { binary, record };
}

function restoreEnvironment(): void {
  for (const key of Object.keys(process.env)) {
    if (!(key in originalEnvironment)) delete process.env[key];
  }
  Object.assign(process.env, originalEnvironment);
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

test.serial("fails soft when the configured binary is absent", async () => {
  process.env["KNIVES_BIN"] = "/definitely/not/knives";
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
  } finally {
    restoreEnvironment();
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

test("discovers a sibling install binary from the release archive layout", () => {
  const module = resolve(
    tmpdir(),
    "prefix",
    "share",
    "knives",
    "opencode",
    "plugins",
    "lib",
    "internals.ts"
  );
  expect(siblingBinary(module)).toBe(resolve(tmpdir(), "prefix", "bin", "knives"));
  expect(readOptions({ guidance: false })).toEqual({
    notice: true,
    guidance: false,
    owner: true,
    skills: true,
  });
});

test.serial.skipIf(realBinary === undefined)("injects once through the real binary", async () => {
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
  restoreEnvironment();
});

test.serial.skipIf(realBinary === undefined)(
  "preserves the binary budget after pathless bash",
  async () => {
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
    restoreEnvironment();
  }
);

test.serial.skipIf(realBinary === undefined)(
  "fails closed and keeps valid roots through the real binary",
  async () => {
    await withRepository(async ({ home, root, file }) => {
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
      await write(
        join(home, "repos.toml"),
        `[repos.gone]\npath = "/not/here"\nupstream = "u"\norigin = "o"\n\n[repos.managed]\npath = "${root}"\nupstream = "u"\norigin = "o"\n\n[trusted.work]\npath = "${trusted}"\n`
      );
      const valid = output();
      await hooks["tool.execute.after"](
        { tool: "apply_patch", sessionID: "valid", callID: "three", args: { filePath: file } },
        valid
      );
      expect(valid.output).toContain("PLUGIN_GUIDANCE");
    });
    restoreEnvironment();
  }
);
