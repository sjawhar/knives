# Recording a release cut outside knives

Read this before you record a release that a person or a script composed outside knives, such as a compose script's chain of merges. Hand changes on a release knives wrote are `adopt`'s: [releases.md](releases.md).

## `knives release cut NAME --at REV`

Records REV as NAME's recorded cut. It makes no commit. Then `adopt`, the edit verbs and `audit` read the release as recorded, on another machine after its next ledger pull. **Record the older name first.**

Its gates, in order. Each refusal writes nothing and exits `3`, unless it says otherwise:

1. NAME is a dated cut of the series ([names](release-composition.md#names-series-and-schemes)) that git can hold as a ref; else a usage error (exit `2`). `--at` on a fixed release branch is a usage error too: a plain `cut` records that branch.
2. The plan has no problems, and a shared ledger pulls.
3. REV names one commit that has no conflict, is no workspace's working copy, and shares history with the trunk: not the root, nor a commit made on it apart from the trunk.
4. When NAME's newest recorded write is REV, it says `is already recorded`, writes nothing and exits with the plan's exit. When it is another commit, it refuses: record a commit made over a recorded write with `adopt`.
5. NAME sorts after every other release name of the series, locally, on the publish remote and in the ledger. The refusal names the newer name.
6. Every ref of NAME, locally and on the publish remote, names REV. With none, it makes the local bookmark NAME at REV.
7. A published NAME (a tag or a `record: published` note names REV) is on the publish remote. The refusal gives the push and the command to run again; it is not the published remedy.

Then:

- **Members.** A parent the trunk holds is never one. In a [stacked series](stacked-series.md), the members are the reading's, and the previous release is recorded first under its label. Elsewhere they are REV's parents, carriers left out, and REV is refused when it merges no member. It is also refused when it merges the series' previous release, or when a member carries a merge past the trunk: only a stacked series reads a release made on its previous release, and these two refusals name the `stacked = true` setting.
- **Audit.** Each member and each trunk parent must replay empty onto REV, from its fork point with the trunk (in a stacked series, with the previous release). Nothing is carried forward. A file that changed since the release REV follows (the previous or newest other release; for the first record, the members' fork point with the trunk) and that nothing explains refuses REV.
- **Composition gate**, as for a cut ([the gate](release-composition.md#knives-release-cut)): REV's own tree carries each member of the previous cut and of the newest recorded write, and each fix by hand the gate holds.
- **Record.** One cut event with `; recorded: composed outside knives` and no `unchecked` mark, and the line `recorded NAME at <commit> as its cut; nothing was committed`, with the push command when the publish remote lacks NAME. No orphan gate, no reap, no push.
- Then `knives audit <repo>` on every machine, after its next ledger pull, shows no `has no recorded cut or edit event` note for the name.

```bash
# series 9.2 is stacked; a script made 8.0.9, then 8.1 on it: record the older name first
knives release --series 9.2 cut release/9.2/2026-11-15-speed-8.0.9 --at <commit of 8.0.9>
knives release --series 9.2 cut release/9.2/2026-11-17-speed-8.1 --at <commit of 8.1>
knives release --series 9.2 members release/9.2/2026-11-17-speed-8.1 --verify
```
