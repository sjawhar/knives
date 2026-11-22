// The environment every git and knives the plugin's tests start runs in, as
// tests/common/isolation.rs gives the Rust suite. Imported by knives.test.ts
// for its effect, which happens before any test runs.
//
// The test process inherits the environment of whoever runs the suite: a
// developer's shell, CI, or an agent session. Left as it is, the git a test
// starts reads that person's ~/.gitconfig and runs their git wrapper first on
// PATH, and knives reads the session's variables (KNIVES_OWNER and the
// harness session ids name a claim's owner). So this module, once:
//
// - removes every variable except TMPDIR, TERM, NODE_ENV, Bun's own (BUN_*),
//   and KNIVES_BIN, the binary under test CI names;
// - makes a new HOME for this process, removed after the file's tests, and points
//   XDG_CONFIG_HOME, GIT_CONFIG_GLOBAL and JJ_CONFIG at it (configs that sign
//   nothing), with GIT_CONFIG_NOSYSTEM set and both editors `false`;
// - drops from PATH every directory whose git or jj is a wrapper (a script,
//   or a link to a program of another name) rather than the program, and
//   throws, naming each git wrapper and the PATH it searched, when that leaves
//   no real git. Only git: these tests start git and never jj, and CI's plugin
//   job installs no jj.
//
// Bun.spawn and Bun.spawnSync start a child with the environment this process
// started with unless given `env`, so every spawn in the tests passes
// `env: process.env`, as the plugin itself does.

import { afterAll } from "bun:test";
import {
  closeSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readSync,
  realpathSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, delimiter, join } from "node:path";

/** Why the `program` in `directory` is a wrapper rather than the program (a
 * script, or a link to a program of another name), or null when `directory`
 * has no such file or it is the program. */
function wrapper(directory: string, program: string): string | null {
  const candidate = join(directory, program);
  try {
    if (!statSync(candidate).isFile()) return null;
    const target = realpathSync(candidate);
    if (basename(target) !== program) return `a link to ${target}`;
    const start = Buffer.alloc(2);
    const descriptor = openSync(target, "r");
    try {
      readSync(descriptor, start, 0, 2, 0);
    } finally {
      closeSync(descriptor);
    }
    return start.toString("latin1") === "#!" ? "a script" : null;
  } catch (error) {
    return statSync(candidate, { throwIfNoEntry: false })?.isFile()
      ? `unreadable (${error})`
      : null;
  }
}

const searched = process.env["PATH"] ?? "";
const kept: string[] = [];
const skipped: Array<{ program: string; file: string; why: string }> = [];
for (const directory of searched.split(delimiter).filter((entry) => entry.length > 0)) {
  const wrappers = ["git", "jj"].flatMap((program) => {
    const why = wrapper(directory, program);
    return why === null ? [] : [{ program, file: join(directory, program), why }];
  });
  if (wrappers.length === 0) kept.push(directory);
  else skipped.push(...wrappers);
}
const unreplaced = skipped.filter(
  ({ program }) =>
    program === "git" &&
    !kept.some((directory) =>
      statSync(join(directory, program), { throwIfNoEntry: false })?.isFile()
    )
);
if (unreplaced.length > 0) {
  throw new Error(
    [
      ...unreplaced.map(
        ({ program, file, why }) =>
          `knives tests: PATH has no real ${program}: skipped ${file}, ${why}`
      ),
      `knives tests: searched PATH=${searched}`,
    ].join("\n")
  );
}
const path = kept.join(delimiter);

/** This process's HOME, removed after the file's tests. */
export const testHome = mkdtempSync(join(tmpdir(), "knives-plugin-test-home."));
const gitConfig = join(testHome, ".gitconfig");
writeFileSync(
  gitConfig,
  "# The plugin tests' global git config (plugin/test-environment.ts).\n" +
    "[commit]\n\tgpgsign = false\n[tag]\n\tgpgsign = false\n"
);
const configHome = join(testHome, ".config");
const jjConfig = join(configHome, "jj", "config.toml");
mkdirSync(join(configHome, "jj"), { recursive: true });
writeFileSync(
  jjConfig,
  '# The plugin tests\' jj user config (plugin/test-environment.ts).\n[signing]\nbehavior = "drop"\n'
);
// Not process.on("exit"): `bun test` exits without emitting it.
afterAll(() => rmSync(testHome, { recursive: true, force: true }));

for (const name of Object.keys(process.env)) {
  const kept =
    ["TMPDIR", "TERM", "NODE_ENV", "KNIVES_BIN"].includes(name) || name.startsWith("BUN_");
  if (!kept) delete process.env[name];
}
Object.assign(process.env, {
  PATH: path,
  HOME: testHome,
  XDG_CONFIG_HOME: configHome,
  GIT_CONFIG_GLOBAL: gitConfig,
  GIT_CONFIG_NOSYSTEM: "1",
  JJ_CONFIG: jjConfig,
  JJ_EDITOR: "false",
  GIT_EDITOR: "false",
});
