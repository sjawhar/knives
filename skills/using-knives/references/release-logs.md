# Release logs

Read this when a release write or `adopt` prints a line about a `CHANGELOG.md` or `RELEASE_NOTES.md`, and before you join their lists by hand. The full rules: README.md, "Release workflow".

Every release write unions a `CHANGELOG.md` or `RELEASE_NOTES.md`, in any directory, where each side only inserted its own block. knives never renumbers, and every other release-log conflict is yours.

- It joins lists only in a file with one `## [Unreleased]` heading, inside that section, where every block there is plain `### ` lists: every other block's lists of a heading join the first list of that heading in the last block that has it. Elsewhere it writes each block whole, and for each heading that two blocks leave in one section it prints ``<file>: left `### X` twice: …; join them by hand``.
- After every write (`cut`, `cut --at`, `include`, `drop`, `advance`, `rebase`) and every `adopt` that records a tree, knives prints ``<file>: [Unreleased] holds `### X` twice; knives changed no line for it; join them by hand before you push`` for each `### ` heading that the one `[Unreleased]` section holds twice. A clean merge can leave one.
- Join such lists by hand and record it with `adopt --why` before you push.
- The cut audit, `members --verify` and `--carries` read a member's block as carried where the release holds it whole anywhere in the file, or, for plain `### ` lists, where the one `[Unreleased]` section holds each of its lists as one run of lines, in order, under one list of the same heading. Entries moved within a list, or split between two lists, read as missing.
