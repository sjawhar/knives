# oh-my-pi extension

`extensions/knives.ts` adapts the OpenCode plugin's hooks (`plugin/lib/internals.ts`) onto
oh-my-pi's extension events. It adds no exports to the plugin. It leaves oh-my-pi's built-in bash
tool in place, so its approval and sandbox behavior is unchanged. Each tool result's hook call
carries the session's effective system prompt (`ctx.getSystemPrompt()`), and the binary leaves
out any instruction file whose text that prompt already holds — the session's own `AGENTS.md`
or `CLAUDE.md`, and the guidance this adapter adds to the prompt each turn. The call also carries
the id of the assistant message that made it and the envelope nonces of the knives guidance blocks
in the tool results after the branch's latest compaction or `/clear`, so the binary injects again
a block that shake, pruning or compaction took out of the context.

Install:

    ln -sfn "$PWD/omp/extensions/knives.ts" ~/.omp/agent/extensions/knives-omp.ts

oh-my-pi caches extension load failures by mtime, so `touch` that symlink after editing.

To try a checkout without touching the installed plugin tree, run one session with discovery
off and this file loaded explicitly, against the checkout's build:

    cargo build
    KNIVES_BIN="$PWD/target/debug/knives" omp --no-extensions -e "$PWD/omp/extensions/knives.ts" -p "…"
