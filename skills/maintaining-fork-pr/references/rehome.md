# Rehoming the branch

Read this before a rehome (step 3), and before a squash or rebase a maintainer asks for (step 4), which follows the same rules.

- Run the `fork-work` bidirectional gate first.
- Lone branch (gate clean): `jj rebase -b <branch> -d <trunk>@upstream`.
- Descends from a pushed release/keep merge (`stacked-history`): this rehome is yours, with no hand-back. Run `jj rebase -s '<release-or-keep-ref>..<branch>' -d <trunk>@upstream`. A bare `-b` rewrites the merge.
- A direct release member: record `rehome needed: parent of <release>` and hand back. The orchestrator changes the release only after all member claims are released.
- An ancestor of a release/keep merge that is not a current member: record `rehome needed: ancestor of retained release <name/ref>` and hand back. Never `-b`, `-s` or `-r` it.
- Never duplicate a feature branch or build a release-lineage copy.
- After the rehome, record the old and new commits and update the expected tip. Make sure that the patch and a conflict-free ancestry stay. A trunk-context-only diff does not replace exercising the code. Push only after candidate review.
