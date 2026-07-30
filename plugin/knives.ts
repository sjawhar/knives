// The plugin entry point, and nothing else.
//
// OpenCode's loader iterates EVERY export of a plugin module and treats each
// function as a plugin, calling it with (input, options): see
// getLegacyPlugins/isServerPlugin in packages/opencode/src/plugin/index.ts,
// where isServerPlugin is just `typeof value === "function"`. Exporting a
// helper here, even a pure one, means OpenCode calls that helper as a plugin
// and plugin loading fails.
//
// Testable internals therefore live in ./lib/internals.ts, which tests import
// directly. This file exports exactly one thing.
export { knivesPlugin } from "./lib/internals.ts";
