# Rehoming the branch

Read this before a rehome (step 3), and before a squash or rebase a maintainer asks for. Take the first case that matches: a looser case would rewrite what a release names.

1. Run `fork-work`'s bidirectional gate (§7).
2. The branch is a parent of a current release merge: record `rehome needed: parent of <release>` and hand back. The orchestrator moves the release after every member claim is released.
3. The branch is an ancestor of a retained release or keep merge: record `rehome needed: ancestor of retained release <ref>` and hand back. Do not `-b`, `-s` or `-r` it.
4. Otherwise (the gate is empty, or the branch descends from a pushed release or keep merge) the rehome is yours: use the rebase that §7 gives for that case.
5. Write a `rehome:` notch with the old and new commits as evidence; the new commit is the expected tip. Check that the patch survived without conflicts, and run the gates again. Push only after review (step 6).
