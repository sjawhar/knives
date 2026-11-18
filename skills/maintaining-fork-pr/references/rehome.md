# Rehoming the branch

Read this before a rehome (step 3), and before a squash or rebase a maintainer asks for (step 4), which follows the same rules.

Run the `fork-work` bidirectional gate first. Then take the first case below that matches: they run most restrictive first, a branch can match more than one, and a looser case would rewrite what a release names.

- A direct release member: record `rehome needed: parent of <release>` and hand back. The orchestrator changes the release only after all member claims are released.
- An ancestor of a release/keep merge that is not a current member: record `rehome needed: ancestor of retained release <name/ref>` and hand back. Never `-b`, `-s` or `-r` it.
- Descends from a pushed release/keep merge (`stacked-history`): this rehome is yours, with no hand-back. Run `jj rebase -s '<release-or-keep-ref>..<branch>' -d <trunk>@upstream`. A bare `-b` rewrites the merge. Stacked-series exception (`stacked = true` on the series table): a member built on the previous release is not `stacked-history`, and `knives release rebase` refuses there. Do not move it onto the trunk. To move it onto a newer previous release, rebase it by hand onto that release (`jj rebase -b <branch> -d <newer release>`: the newer release holds the previous one, so only the branch's own commits move), then run `knives release advance <branch>`.
- Lone branch (gate clean): `jj rebase -b <branch> -d <trunk>@upstream`.
- After the rehome, record the old and new commits and update the expected tip. Make sure that the patch and a conflict-free ancestry stay. A trunk-context-only diff does not replace exercising the code. Push only after candidate review.
