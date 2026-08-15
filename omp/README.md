# oh-my-pi extension

`extensions/knives.ts` adapts the OpenCode plugin's hooks (`plugin/lib/internals.ts`) onto
oh-my-pi's extension events. It adds no exports to the plugin and changes no plugin file. It
leaves oh-my-pi's built-in bash tool in place, so its approval and sandbox behavior is unchanged.

Install:

    ln -sfn "$PWD/omp/extensions/knives.ts" ~/.omp/agent/extensions/knives-omp.ts

oh-my-pi caches extension load failures by mtime, so `touch` that symlink after editing.
