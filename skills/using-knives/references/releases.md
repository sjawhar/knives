# Recording, repairing, reaping and publishing a release

Read this when you finish a release by hand, when a write refuses or prints `could not check` or `is published`, when a reap keeps a cut, and before you push a release. Composition and edits: [release-composition.md](release-composition.md).

A **carrier** is a parent that is an earlier write of the same release name, as the fast-forward rebase in [Publishing](#publishing) adds. It is never a member. Output labels it `<name> earlier write (carrier, not a member)`, and writes leave it out of the parent set.

## `knives release adopt [NAME] [--why TEXT]`

Records the commit a release name holds now as its newest knives write. It makes no commit and moves no bookmark. Run it after any hand change on the release commit: a conflict resolved, a release log renumbered, a repair or restore, the fast-forward rebase before a push, or a new description or signature (below).

1. Make sure the commit has no conflicts and is no workspace's working copy (run `jj new` there first).
2. Run `knives release adopt <name>`. Add `--why '<reason>'` when the commit changes a file the baseline left without a conflict; without it, `adopt` refuses and names the files.
3. Read the counts: per member and file, the member's added lines the commit no longer holds. A renumbered heading line is expected there.
4. Exit `1`: measured against a recorded write, a member lost added lines from a file that was neither conflicted nor a release log, or lost all its blocks in a release log or a conflicted file. It has already recorded. Repair the release, then adopt again.

- With no local bookmark, `adopt` fetches and reads `<name>@<publish-remote>`.
- After a hand `jj restore --from <commit> --into <name> <path>...`, adopt with `--why`, then cut.
- After an edit run with `--allow-loss '<reason>'` ([release-composition.md](release-composition.md)), its ledger event holds `; lost: <path> (<member>: <n> line(s))` and `; why lost: <reason>`. Put the named lines back by hand, then `knives release adopt <name> --why '<reason>'`: a line put back is a change beyond the write's conflicts. Read each named line first: where a conflicted release log holds `## [Unreleased]` more than once, a line the file still holds can be named. `--allow-loss` is not `--allow-drop`: the cut's `--allow-drop` leaves out members or commits, and an edit's `--allow-loss` writes a file that loses a remaining member's lines.
- `adopt` refuses a name the ledger records no write of; the next write over it records the name.
- A release re-signed or metaedited after a knives write, by `jj describe`, `jj sign`, `jj metaedit` or a push that signs, has a new commit id with the same change, parents and tree. Adopt it on the machine that re-signed it, after the last rewrite, while that write is unpublished. `adopt` records the new commit, so another machine can read the release once it fetches it. A tag on that commit, a publish remote that does not hold it yet, or conflicts that the knives write left, do not refuse this adopt.
- The write is published when the publish remote holds it, a tag is on it, or a `record: published` note names it, for a fixed release too. Then `adopt` prints `nothing to adopt`, exits `0` and records nothing. Recording the new commit would make every other machine wait for a push that moves a published name.
- Skip that adopt and the ledger names only the old commit, which no other machine holds: there `audit` reports `could not compare` and `release-drift`, and the next cut is refused.
- Where the newest event of the name is marked `; unchecked:`, `adopt` reads the new commit as any release knives could not check ([below](#when-knives-cannot-check-the-release-in-hand)): it may refuse it as published, and otherwise it measures it and records it.
- An adopt of the newest recorded write itself writes nothing and prints `nothing to adopt`, unless its event is marked `; unchecked:`. Then it records it, which ends the mark.

## Adopt a hand resolution before the next write

A release in hand is **unrecorded** when its tree differs from its matched recorded write rebuilt on the release's parents, carriers left out. A jj rebase of that write, a carrier copy and a metadata rewrite stay recorded; a hand edit does not. While it is unrecorded, `cut`, `include`, `drop`, `advance` and `rebase` refuse with exit `3`, write nothing, and name `knives release adopt <name>`.

## When knives cannot check the release in hand

Three cases: this repository lacks the matched write (made on another machine, not fetched here), no recorded write matches the release's parents (the members changed outside knives), or the ledger records no write of the name. The write goes ahead, prints `knives could not check <name> at <commit> against its recorded writes: …` with the remedy, and its ledger event ends `; unchecked: W absent`, `no match` or `no record`.

- A release that is such a marked write, or a jj rebase or metadata rewrite of it, stays unchecked: every later write over it repeats the mark, and `audit` shows a note while the newest write of the name carries it.
- A release that carries a hand change on top of a marked write that this repository holds, such as a conflict resolved by hand, is unrecorded (above): every write refuses it until `knives release adopt <name>` records it.
- `knives release adopt <name>` ends the mark. For `no match`, first make the change with `knives release include`, `drop` or `advance` (a member added by hand: drop it, then include it), then adopt.
- A release whose every parent is a carrier (a resolution committed on top of the release) has no member. Every write and `adopt` refuse it and print a squash remedy, or, for a published dated or series release, the published remedy.

## The published remedy

A dated or series release is **published** when a tag names its commit, or a `record: published …` notch on the release names that commit in its evidence. Consumers were given that commit, so a hand edit R cannot be recorded under the name while the publish remote lacks R, nor when every parent of R is a carrier. `adopt` and every write then print one line, `<name> is published (<what marks it>), so a hand edit cannot be recorded under it: …`. Follow its steps in order:

1. Move `<name>` back to its published commit or, when every parent of R is a carrier, to W, the recorded write R sits on. The step gives `jj bookmark set <name> -r <commit> --allow-backwards` when the local name sits at R.
2. If the working copy is on R or on an empty commit on R: `jj new 'trunk()'`. Never `jj new <W>`.
3. Cut a new dated name.
4. `jj restore --from <R> --into <new name>`, then `knives release adopt <new name> --why '<what the edit does>'`.
5. Where the publish remote lacks R: `jj abandon <R>`. When something else rests on R, the line ends `then stop: <R> is not to be abandoned, as …`; stop there.
6. `jj bookmark forget <name>`. Never `jj bookmark delete`: a push carries a delete to the remote.

The remedy never moves the old name or its tag on the publish remote.

### The new cut's expected exit 1

The cut of step 3 exits `1` once in two states. It is still written, and the later steps end the finding.

- `<name>@<remote>` names another commit than step 1 moved `<name>` to. The cut prints `<name> names both <a> and <b>, and their trees differ (<n> files)`. Step 6 ends it.
- The remote lacks R, and R descends from the step 1 target. The cut takes `--allow-drop` and must print `--allow-drop: nothing dropped: the <n> commit(s) the cut does not carry are still visible: …` with exactly the commits the remedy names; its reap must print `kept <name>: it has descendants, whose heads are …`. Any other answer is a stop. Steps 5 and 6 end it.

## `knives release reap`

Runs after every dated cut, and alone. It forgets superseded dated refs locally and on tracking remotes and abandons their merges. It never changes a remote; a fetch can bring tracking refs back.

- While the live cut has conflicts, every superseded cut stays: it records the last resolution.
- A cut pinned by a tag, an untracked remote bookmark or a local bookmark of another name (`keep/…`) loses its refs and keeps its commit. Exit `0`.
- A cut that something rests on stays with its refs; the reap names it and exits `1`. That is a workspace working copy on or above it, empty or not, or any descendant of the cut, of a commit an edit wrote under its name, or of a rewrite of either. Move your own empty working copy with `jj new <live cut>` and reap again. Another workspace's working copy is its owner's to move.
- The quiet keep exits `0` with its `kept <name>` line: every descendant is reachable from the name's own ref on the publish remote or from a tag on those commits, and nothing else (no working copy, local bookmark or other ref) names one. The old name after the published remedy is kept this way.

## Publishing

Release commands never push. Publish a new release name with `jj git push --remote <publish-remote> --bookmark <name>`. A push that signs (`git.sign-on-push`) pushes a new commit id: run `knives release adopt <name>` after it, on the same machine.

A release the publish remote moves only forward (a fixed release branch) needs a fast-forward: a knives write does not descend from the published head.

```sh
jj git fetch --remote <publish-remote>
old=$(jj log -r '<name>@<publish-remote>' --no-graph -T commit_id)
pre=$(jj log -r <name> --no-graph -T commit_id)
jj rebase -r <name> -d <parent-1> -d <parent-2> -d "$old"     # every parent in order, then the published head
jj restore --from "$pre" --into <name>                         # put the write's tree back
JJ_USER='<author>' JJ_EMAIL='<email>' jj metaedit --update-author <name>   # only to change the author
knives release adopt <name> --why "fast-forward of $pre onto the published head $old"
git push <publish-remote> "$(jj log -r <name> --no-graph -T commit_id):refs/heads/<name>"
```

- Adopt after the last rewrite and before the push: the publish ref still names the old head then, so `adopt` reads it as a carrier.
- Change the author only with `jj metaedit`. Git plumbing drops jj's `change-id` header, and another machine then cannot read the pushed copy as a carrier.
- Push with no `+` and no `--force`, and stop when git refuses. Never `jj git push` here: it moves the remote branch whenever it is where jj last fetched it, which hides the non-fast-forward refusal.
- Skip `adopt` and nothing records the pushed head: another machine's next write goes ahead marked `; unchecked: W absent`. A push git refuses after `adopt` leaves a recorded write the fork lacks: audit reports `release-drift` until the next write or `adopt`.
