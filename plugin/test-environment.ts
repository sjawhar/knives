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
//   or a link to a program of another name) rather than the program.
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

/** Whether `candidate`, found on PATH as `program`, is that program: a file of
 * that name, links followed, that is not a script. */
function isProgram(candidate: string, program: string): boolean {
  let target: string;
  try {
    target = realpathSync(candidate);
  } catch {
    return false;
  }
  if (basename(target) !== program) return false;
  const start = Buffer.alloc(2);
  let read = 0;
  try {
    const descriptor = openSync(target, "r");
    try {
      read = readSync(descriptor, start, 0, 2, 0);
    } finally {
      closeSync(descriptor);
    }
  } catch {
    return false;
  }
  return read === 2 && start.toString("latin1") !== "#!";
}

/** Whether `directory` holds a git or jj that is a wrapper, not the program. */
function wrapsGitOrJj(directory: string): boolean {
  return ["git", "jj"].some((program) => {
    const candidate = join(directory, program);
    try {
      if (!statSync(candidate).isFile()) return false;
    } catch {
      return false;
    }
    return !isProgram(candidate, program);
  });
}

const path = (process.env["PATH"] ?? "")
  .split(delimiter)
  .filter((directory) => directory.length > 0 && !wrapsGitOrJj(directory))
  .join(delimiter);

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
