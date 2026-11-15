# Release model, commands and evidence

Read before you plan, edit, cut, rebase, adopt or reap a release, or read membership or carriage output. Commands write locally and never push. The publish remote is the release remote when configured, else origin. Publish a new release name with `jj git push --remote <publish-remote> --bookmark <name>`. A release that the publish remote moves only forward (a fixed release branch) is pushed by the fast-forward rebase, then `adopt`, then a plain `git push`, never `jj git push`: see [Publishing](#publishing).

## Plan and keep the composition

`knives release [--repo REPO] [--consumer DIR]...` with no subcommand plans. It shows what a cut would hold, and whether parents still match branch tips. It shows branches missing or advanced past their released parent, members that carry prior release merges, and consumer pins. Options go before the subcommand: `knives release --consumer DIR include feat/x`.

- A release is a flat octopus merge. Its direct parents are its members. Members reach the upstream base, which is never a direct parent. A member that merges upstream stays a member until an edit changes that.
- `include` adds one parent. `drop` removes one. When no other member's history carries the dropped parent, `drop` says after the write whether the release lost that content, or that the parent left the parent set and the release's content did not change. `advance` moves members to branch tips. `rebase` moves the shared base. `adopt` records a release that you finished by hand ([adopt](#knives-release-adopt-name---why-text)).
- Edits duplicate the release onto the new parent set and keep recorded conflict resolutions. The new composition can bring new conflicts.
- Every write (`cut`, `include`, `drop`, `advance`, `rebase`) resolves one kind of conflict itself. That is a `CHANGELOG.md` or `RELEASE_NOTES.md`, in any directory, where each side only inserted its own block at the conflicting place. The write keeps every block verbatim. The blocks the release already had go below the blocks the write brings in. The output names each such file with the first line of each block, and the ledger event ends with `; union: <files>`.
- knives never renumbers. Two blocks that claim one version stay as their members wrote them. Renumber by hand and record it with `adopt --why`. Every other conflict is yours: a release log where a side changed or removed lines another side kept, a release log where a side inserted a block with no final newline and something else would follow it, and every other file.
- The write unions a block with no final newline only where it ends the file. Anywhere else, its last line would run into the next line. The write never adds a byte to keep them apart, so it leaves that conflict to you.
- The cut audit, `members --verify` and `--carries` for a member of a release read a member's block that such a union holds as carried.
- A **fast-forward carrier** is a parent that is an earlier write of the same release name. The fast-forward rebase in [Publishing](#publishing) adds one. A parent is a carrier when the ledger records it as a cut or edit of the name, when it is the publish remote's commit of the name, when it is a visible rewrite of either, when a recorded cut or edit event of the name names its change id (a pushed copy keeps its write's change id), or when a recorded event of the name lists it with the label `<name> earlier write (carrier, not a member)`. A carrier is never a member.
- The plan and `members` label a carrier `<name> earlier write (carrier, not a member)` and do not count it as stacked. The cut's carried rows do not name it. `adopt` records it.
- In the ledger, only `adopt`'s record of the fast-forward parent that it took writes that label. It never writes it for a member of the write that it replaces. The label is the only record of a carrier that no event records as a write, such as a published resolution that you squashed into the release ([the squash remedy](#knives-release-adopt-name---why-text)). `adopt` reads such a parent as a carrier only while the publish ref names it. So push in the order that [Publishing](#publishing) gives: the fast-forward rebase, `adopt`, then a plain `git push`. A push before that `adopt` (in the squash remedy, the second `adopt`) leaves the carrier unlabelled. The next write then takes it for a member, stacked on a prior merge.
- Every write that composes from the release's parents (`include`, `drop`, `advance`, `cut`, `rebase`) leaves the carriers out. It first writes the release again on its other parents with the release's own tree. So a resolution that you made over the carrier becomes the release's own change and carries forward. Each push adds one carrier, and the next write leaves it out, so carriers do not pile up.
- Keep stated cross-fork dependencies. A dependent without its requirement ships a release that cannot work.

### One branch is member and PR head

- The branch forks from the shared release base and is linear past it. A rebase onto newer upstream is intentional, for example when a maintainer asks. It is never a side effect of starting work.
- `release advance` follows the change id as well as ancestry. To move the whole composition, use `release rebase`. The fork point of a member alone is not a finding.
- Never make a second "release-lineage" or "sibling" branch with the same PR content on an older base. If it cannot compose, rebase the release. Do not split review and conflict work across two copies.

### Raw rebase and retained releases

`advance` and `rebase` change only the release in hand. A raw `jj rebase -b`, `-s` or `-r` can reach older release/keep merges. Those still pin member commits for consumers or for conflict evidence. Before a rebase by hand, run the bidirectional gate from **fork-work** (§7):

1. Is the branch a parent of any release/keep merge?
2. Does it descend from any release/keep merge?

Bare `release members` answers neither for older releases. It sees only the current composition. Name a release to see its parents. Keep published merge commit ids.

- A member must carry no merge past known trunk positions: `<trunk>@upstream`, `<trunk>@origin` and local trunk. A branch built on a release merge carries all parents of that merge.
- `include`, `advance` and the first `cut` refuse such `stacked-history`. Planning reports existing stacked members. A merge that a trunk position reaches belongs to trunk. If every local view lags the branch base, the detail says that these can be upstream merges. `sync` fetches them.
- The exception: a merge whose side parents (every parent after the first) the release's other members already carry, and that is no release cut: no release ref names it, and the ledger records no knives write of a release with its change id. The branch then brings only its own line (`git rev-list <branch> ^<release>`). `include` takes a branch that merges the members it descends from, one or several, beside them, which stay members. A branch the last event recorded as one of those members continues that member: a named `advance` replaces it and keeps the members it merged. Both name the merge. The plan measures each member against the others and notes it, and names the verb that takes each branch. A bare `advance` leaves such a branch alone. A merge whose side parent the release lacks still refuses, and the refusal lists every commit the branch would bring. So does a branch that merged its own earlier tip back in.
- After that merge was pushed anywhere, `jj rebase -b <branch> -d <trunk>` is not the remedy, because it rewrites the merge. Rebase only the branch's own commits, from the ref that the gate found:

```text
jj rebase -s '<release-or-keep-ref>..<branch>' -d <trunk>@upstream
```

- This keeps the change ids for `advance` and keeps the merge id. `-b` avoids this hazard only when the merge was never pushed. The other direction of the check still applies.
- When a released parent merges upstream and its branch continues, ancestry cannot identify it, because every new trunk branch descends from it. The cut/edit record names the member. Planning offers `advance <branch>`, or `rebase` to retire the parent. `include` refuses a second copy. A named advance says when its match rests on the record alone.
- A cut carries exactly what its parents hold. The plan says `N parent(s), flat` only when no member carries a prior release merge. Otherwise it counts the stacked members.

## Names and consumer gates

- **Dated scheme** (no `release_branch`): names are exactly `release/YYYY-MM-DD` or `release/YYYY-MM-DD.N`, never `.N.M`. Make a name only with `release cut NAME`, when the pin gate needs a new name. Never make a release bookmark by hand and then advance it.
- **Named series** (`[repos.<name>.series.<series>]`): one dated line per base. The default series is the entry's own `base` with `release/YYYY-MM-DD[.N]` cuts. Series `<series>` cuts `release/<series>/YYYY-MM-DD[.N]` on its own base.
  - Every release verb takes `--series <series>` (`knives release --series 9.2 cut release/9.2/2026-10-20`). It then sees only that series' cuts, base, ledger baseline, reap set and consumer pins (`refs/heads/release/<series>/*`). Without it, the verb acts on the default series.
  - A branch belongs to the series whose base it forks from latest. The first cut composes only that series' branches, and the plan lists only them. `include` or `advance` of a branch from another series is refused, naming the series it fits.
  - A branch that fits no series is refused, with one of two reasons. It forks from two bases at the same point. When one base is an ancestor of another, every branch at or below the older base's tip ties. Or it has no single fork point with some base (criss-cross).
  - Every series' base must resolve. If a declared base does not, every release verb exits `3` and names it (`fetch it, or set the series' base_remote`). It never assigns that series' branches to another.
  - A cut name equal to a series' prefix (`release/9.2`) is refused, because git cannot hold it beside `release/9.2/<date>`.
  - `status` and `repos` show only the default series' newest cut. Read a named series through `knives release --series <series>`. `audit` checks drift for every series' cuts and notes each branch that fits no series.
- **Fixed scheme** (`release_branch = "<name>"`): cuts move that branch in place, through the jj allow-backwards mechanism. `release cut` takes no name, and a dated name is refused. The cut uses the local composition, including unpushed edits.
- Under the fixed scheme, release verbs read the published position from the local `<name>@<publish-remote>` tracking bookmark, not the live remote. Fetch first when freshness matters. `consumers` and `pushed` read the live remote.
- Every plan, cut and edit reads registered `consumers` and `--consumer <DIR>` scans. With neither, no pin is known. The plan says so and treats the release as unpinned, and verbs continue. An install-based consumer can have no lockfile to register.
- An unreadable consumer that is configured or passed is a problem and blocks the verbs. So does a shared ledger that the plan could not pull, because the drop guard would check a stale cut ([ledger](ledger.md#sharing-between-machines)).

`include`, `drop`, `advance` and `rebase` refuse with exit `3` in these cases:

- Every pin **of this release** is frozen on a revision, so an in-place edit reaches nobody. Under the dated scheme, make a new dated name. Under the fixed scheme, the refusal says to update the frozen pins or change the scheme, because a fixed branch cannot reach revision pins.
- Pins frozen on older releases do not block. A release with no pins is free to edit. A pin is of this release when it names the branch, or freezes the commit that the publish remote holds (a lock `?rev=<sha>` or `#<sha>`).
- Upstream trunk does not resolve, so base and members cannot be told apart. Fetch upstream first.
- The release in hand is unrecorded: you changed it by hand, and no `adopt` recorded it. See [Adopt a hand resolution before the next write](#adopt-a-hand-resolution-before-the-next-write).
- Every parent of the release in hand is a carrier, so the release has no member to compose. The refusal prints the remedy that `adopt` prints ([adopt](#knives-release-adopt-name---why-text)).

## `knives release cut [NAME] [--allow-drop]`

- A normal cut carries the composition in hand: previous parents **verbatim by commit id**, even if a member bookmark is now divergent. It adds no branches and advances no members. Only the first cut, with no composition, starts from every branch.
- The cut audits the candidate first. The net diff of each member, from the members' fork point with upstream trunk, must be in the cut tree. Divergence that the release in hand already has is carried forward: `diverges where the release in hand (a recorded write) already does; carried forward`.
- The cut refuses an unrecorded release in hand, exit `3`, and writes nothing, whatever its audit would say ([below](#adopt-a-hand-resolution-before-the-next-write)). So the divergence it carries forward is a resolution that knives wrote or `adopt` recorded, a release that knives could not check, or a release whose name has no recorded write at all. For a release that knives could not check, the carried rows say `diverges where the unrecorded in-hand release already does`, and say to record it with `knives release adopt` and cut again ([unchecked](#when-knives-cannot-check-the-release-in-hand)).
- A failed audit writes no cut. A pass makes and names the release in one operation, except for the fixed recording path below.
- A cut lists a workspace to remove only when the ledger names its branch, no bookmark of that branch is left locally or on any remote, and no live claim holds it. It never lists the default workspace, a workspace knives did not make, or a `knives start` workspace whose release a reap forgot.

### Gates

1. A dated name must sort after the newest release. A reused name, an older date or an older suffix is refused before the build, exit `3`, because reap keeps only the newest name.
2. The orphan gate refuses to strand commits that only the previous lineage reaches. `--allow-drop` states the intended exception. After the reap, the cut says what it dropped: a commit it does not carry counts as dropped only when it is no longer visible. Such commits rest on the previous cut, so the reap usually keeps that cut and drops nothing.
3. Before the content audit, knives compares the candidate with the previous published cut. The **same tree and parents** is refused as identical, exit `3`, except as below. New members come through `include`, new tips through `advance`, and a new base through `rebase`, not through a new name.
4. The composition gate reads the ledger event of the previous cut, which survives a moved bookmark. Each recorded member must be a parent or an ancestor of a member or the base. Otherwise it needs a reachable rewrite of its change id, a commit with exactly its tree reachable from a parent (a history recreated outside jj; the cut names the twin), or its net diff in the candidate tree. A member that came through upstream base passes.
5. A missing member refuses the cut, however it was lost: a merge rebuilt by hand, a moved bookmark, or a `drop` since the last cut. A recorded commit that does not resolve counts as dropped.

`--allow-drop` states that the drop is intended. It is not a remedy for a refusal. The new cut event names the dropped members. Keep the reason and its dependency effects.

### Identical-cut exceptions

- When **every pin of the previous dated release is frozen**, or the previous dated release is **published**, a new later dated name can start identical, exit `0`. It says to `include`, `advance`, `drop` or `rebase` **before you push**. It is the composition to edit, not identical content to publish.
- A dated release is published when a tag names its local or publish-remote commit, or a `record: published …` note on the release names that commit in its evidence. Other `record:` rows do not count. `include`, `drop`, `advance` and `rebase` refuse a published release, exit `3`, and name what marks it: cut a new dated name first, then edit that.
- A fixed release has a **ledger-recording** path instead. It applies when the last cut event is missing or names a different commit than the published branch. Then `cut` audits that published composition, runs the composition and drop checks, and records that commit as the cut. It makes no commit and pushes nothing. Weak evidence can still give findings. When the ledger already records that commit, an identical cut is refused.
- A fixed release that was pushed by the fast-forward rebase has a carrier. The candidate leaves the carrier out, so it does not match the published commit. Neither the recording path nor the identical-cut refusal fires for it. Record such a commit with `adopt`, before the push ([Publishing](#publishing)).
- A member rewritten to the same content is a new parent, so it is a new composition. The comparison is with the published copy. An unpushed previous cut is not compared.

Cut and edit events keep full parent commits, every bookmark on them, and the commit and change id each wrote, in the [ledger](ledger.md). Use that evidence, not the current bookmark names, to find lost members.

## Editing members

- **`include <branch> [--why "..."]`** adds one branch or revision as a parent. It does not move a member that grew, rebased (inside or outside jj) or merged upstream, which is `advance`. It refuses a second copy and explains the match. It refuses a tip that trunk already reaches, because that moves the base. Use `rebase` onto that trunk instead.
- **`drop <branch> --why "..."`** removes the parent, not the branch. A moved branch matches by ancestry or by the change id of the parent. For a rebuild outside jj, name the commit id of the parent. Removal never guesses from a reused name. `--why` is required and recorded on the release commit. With no `--why`, it is a usage error (exit `2`). When no other member's history carries the parent, the output after the write says `no remaining member carries <branch>'s content; the release loses it`, or, when the release's tree did not change, `<branch> left the parent set of <name>, and the release's content did not change`.
- **`advance [<branch>...] [--from <old-sha>]`** moves named branches exactly. Bare, it moves every member whose branch moved. Ancestry matches growth, and the change id past trunk matches a jj rewrite. Parents that trunk reaches have no unique successor, and `rebase` retires them.
- A named branch with no ancestry or change-id match can match by the `branch@commit` record of the last event. The output says so, because a reused name would move the member onto unrelated content.
- Bare advance refuses when one branch would replace more than one parent. A stacked integration branch does not prove that it replaced them. `--from` names the exact parent to replace and needs exactly one named branch. Use it when no record of the parent exists. Neither form advances onto a tip that trunk reaches.
- Each edit writes a new commit under the release name and abandons the commit it replaced, so no anonymous head stays, and says so. The old commit stays, with a note naming why, when another bookmark, a tag, a remote ref other than the release's own tracked ref on its publish remote, a workspace working copy on it or above it, or any other descendant rests on it. A working copy parked on the release keeps it because rewriting that working copy would leave its workspace stale; the audit does not report it as an anonymous head while the working copy is there. Every later edit of the name, and the reap that forgets the name, also abandons each earlier commit the ledger records under it that is now a visible head nothing rests on (no publish-ref exception here), such as one left behind when that working copy moved on, and says so. An edit that leaves the release with conflicts abandons no earlier commit: they are the record of the last resolution, as with reap. The publish remote keeps the old commit until you push.
- In the same operation, every write (the cut, each edit, `rebase`, `adopt` and reap) also abandons each new head it leaves that nothing holds, when that head is an earlier write of the name or a commit whose newer version is still visible, and names each one. Nothing holds a head when no bookmark, workspace working copy or tag names it, and no remote ref other than the name's own tracked ref on its publish remote. So a carrier that a write leaves out stays hidden behind its publish ref. Any other head the write leaves, such as a member with no bookmark, stays visible for the audit to report. While the written release has conflicts, nothing is hidden.

## `knives release adopt [NAME] [--why TEXT]`

Records the commit a release name holds now as the name's newest knives write. Use it after you finish a release by hand: you resolved a conflict, renumbered a release log, repaired the composition on the release commit, or ran the fast-forward rebase before a push. With no `NAME`, it takes the release in hand of the selected series. It makes no commit and moves no bookmark.

- It pulls the ledger and reads the plan, as the edit verbs do. The commit is the local bookmark's. With no local bookmark, it fetches, then reads the publish remote's ref of the name.
- The write it replaces is the newest recorded write whose recorded parents, carriers left out, match the commit's parents, carriers left out. A parent matches when it is the recorded commit or a visible rewrite of it with the same change id. A recorded parent that this repository does not hold matches by commit id only.
- The baseline is that write rebuilt on the commit's member parents: the tree a knives write onto them holds, with the release-log union, the tree that decides whether the release in hand is recorded ([below](#adopt-a-hand-resolution-before-the-next-write)). So a member rewritten in place, which makes jj rebase the release, is no change of the commit's own. A commit equal to an older recorded write is measured against that write itself. Where this repository does not hold the write, or the write's event carries the `; unchecked:` mark, the baseline is a fresh merge of the commit's member parents with the release-log union. The output names the baseline.
- When the commit is the newest recorded write, or a copy of it with the same change id, parents and tree, `adopt` writes nothing and exits `0`. The exception is a newest write whose event carries the `; unchecked:` mark: `adopt` records the commit again, with no mark, and so ends the mark ([unchecked](#when-knives-cannot-check-the-release-in-hand)). A commit equal to an older recorded write is recorded as the newest, so the audit clears.
- Otherwise it abandons the earlier writes of the name that are visible heads nothing rests on, and records an `edited` event ([ledger](ledger.md)). Then it prints the adopted commit, the write it replaces and the baseline; the files resolved by hand; the files changed beyond the baseline's conflicts, with the reason; for each member and changed file, how many of the member's non-blank added lines, from its fork point with trunk, the commit does not hold; each carrier, labelled `<name> earlier write (carrier, not a member)`; and each commit it abandoned.
- It exits `1` only against a recorded write, never against a fresh merge, and only for one of two losses: any of a member's added lines lost from a file that the baseline left without a conflict and that is not a release log, or none of the blocks a member added left in a release log or in a file that the baseline left conflicted. A block cut down there is the normal shape of a resolution: the counts report it, and the exit stays `0`. It records first. Read the counts, and repair the release if the resolution lost a member's change. A heading line that you renumbered is expected in the counts.
- A file that the commit changes and the baseline left without a conflict needs `--why`. Such a change is a repair, a restore or a renumber. Without `--why`, `adopt` refuses and names the files. The event keeps the reason.

It refuses, exit `3`, and writes nothing in these cases:

- The plan has problems, or a shared ledger did not pull.
- Upstream trunk does not resolve. Each member's added lines are counted from its fork point with trunk.
- The local bookmark is divergent. Or there is no local bookmark, and the publish remote holds no ref of the name after a fetch.
- The ledger records no cut or edit of the name. Cut it first.
- The commit still has conflicts (it names them), or it is a workspace's working copy (run `jj new` there first).
- A file outside the baseline's conflicts changed, and there is no `--why`.
- Every parent of the commit is a carrier: you made the resolution as a commit on top of the release, or on top of `<name>@<remote>`. Such a release has no member, so no write can compose it. Every write refuses it too, also for a name with no recorded write. The remedy depends on the state:
  - A dated or series release that is published (a tag or a `record: published` note names its commit) gets [the published remedy](#the-published-remedy), whether or not the publish remote holds the commit. Its first step moves `<name>` to W, the recorded write that the resolution sits on.
  - Every other release gets the squash remedy. It starts `every parent of <name> is an earlier write:`. Where the name exists only as `<name>@<remote>`, its first step is `jj bookmark track <name>@<remote>`, because a squash moves only local bookmarks. Then squash the resolution into the release commit that it sits on: `jj squash --from <commit> --into <release commit> --use-destination-message`. That flag keeps the release commit's own description and opens no editor. The remedy adds `--ignore-immutable` after it when the publish remote holds that release commit. Then adopt. For a fixed release whose publish remote holds that release commit, the remedy also says to adopt again after the fast-forward rebase, before the push ([carriers](#plan-and-keep-the-composition)). For a name with no recorded write, the last step is to run the write again, because there is nothing to adopt against.
- No recorded write matches the commit's parents, carriers left out: the members changed outside knives. `adopt` prints this remedy:

  ```text
  <name>'s members differ from every recorded write; make the change with `knives release include`, `drop` or `advance` (a member added by hand: drop it, then include it); that write records the new composition, marked unchecked
  ```

  That `include`, `drop` or `advance` goes ahead and marks its ledger event `; unchecked: no match`. Then run `knives release adopt <name>`. It records the release and ends the mark ([unchecked](#when-knives-cannot-check-the-release-in-hand)).
- The release is published (a tag or a `record: published` note names its commit), and the publish remote does not hold the commit. `adopt` prints [the published remedy](#the-published-remedy).

A name of another series exits `2` and names the series that owns it.

### The published remedy

Consumers were given the commit of a published release, so a hand edit of it cannot be recorded under its name. You redo the edit under a new dated name. `adopt` and every write print this remedy as one line when a dated or series release is published (a tag or a `record: published` note names its commit) and the publish remote does not hold the hand edit, R. The publish remote holds R when its ref of the name is R or descends from R. A release whose every parent is a carrier gets this remedy whether or not the publish remote holds R. The line starts `<name> is published (<what marks it>), so a hand edit cannot be recorded under it:`. Then it gives these steps, in this order:

1. Move `<name>` back to its published commit, `<name>@<remote>`. When every parent of R is a carrier, move it to W instead: the recorded write that the resolution sits on, which is R's own carrier parent. Where a local `<name>` sits at R, the move needs `--allow-backwards`, and the step gives the command: `jj bookmark set <name> -r <commit> --allow-backwards`.
2. If the working copy is on R, or on an empty commit on R, move it off with `jj new 'trunk()'`. Never use `jj new <W>`: it leaves the working copy on a descendant of W.
3. Cut a new dated name. In two states this cut exits `1` once ([below](#the-new-cuts-expected-exit-1)).
4. Redo the edit on the new cut: `jj restore --from <R> --into <new name>`. Then adopt the new name: `knives release adopt <new name> --why '<what the edit does>'`.
5. Where the publish remote does not hold R: `jj abandon <R>`. The remedy gives this step only when nothing else rests on R: no descendant, no bookmark or tag other than `<name>`, and no working copy of another workspace. Otherwise it names what rests on R and stops there, because an abandon would rebase that work: `then stop: <R> is not to be abandoned, as <what keeps it>`. It then gives no abandon and no forget.
6. Last, `jj bookmark forget <name>`. Never use `jj bookmark delete`: a push carries a delete to the remote. The forget removes the local name and untracks the remote ref, so no later cut plans against the old name. Reap still reads the earlier writes of the name.

The remedy never moves the old name or its tag on the publish remote. When the published commit is R and rests on W, the new cut's reap keeps the old name without a finding ([reap](#knives-release-reap)). Audit then keeps one `release-drift` line on the old published name ([reports](reports.md#knives-audit-repo---all---no-github)).

#### The new cut's expected exit 1

In each of two states, the new cut of step 3 exits `1` once. The cut is still written. The finding is true at that moment, and the steps after the cut end it. The remedy gives the note for a state only in that state.

- `<name>@<remote>` names a commit other than the one that step 1 moved `<name>` to. This occurs, for example, when every parent of R is a carrier and the publish remote holds R. The cut prints `<name> names both <a> and <b>, and their trees differ (<n> files)`, with the two commits in either order. The remedy says `that cut exits 1 once`. The finding is true: the local name and its remote ref name two different trees. The forget in step 6 removes the local name and ends it.
- The publish remote does not hold R, and R is a visible descendant of the commit that step 1 moved `<name>` to. Only the old line reaches R, so the cut needs `--allow-drop`. The remedy names the flag and the commits that the cut must find: R first, then each commit that rests on R. The cut must print `--allow-drop: nothing dropped: the <n> commit(s) the cut does not carry are still visible: <commits>`, with exactly those commits. Its reap must print `kept <name>: it has descendants, whose heads are <heads>`. The keep is a finding because the publish remote does not hold R. The abandon in step 5 and the forget in step 6 end it. Any other `--allow-drop` answer, or any other commit in it, is a stop.

### Adopt a hand resolution before the next write

- The release in hand is **unrecorded** when its tree differs from the tree of its matched recorded write rebased onto the release's parents, carriers left out. A jj rebase of that write, a fast-forward carrier copy and a metadata rewrite are recorded. A hand edit is not.
- A release equal to any recorded write of its name, or a copy of one with the same change id, parents and tree, is recorded.
- While the release in hand is unrecorded, `cut`, `include`, `drop`, `advance` and `rebase` refuse, exit `3`, and write nothing. The cut refuses whatever its audit would say. Each refusal names the remedy as a command: `knives release adopt <name>`. `adopt` asks for `--why` only for a change outside the conflicts.
- When knives cannot check the release in hand, the write goes ahead and says so ([below](#when-knives-cannot-check-the-release-in-hand)). `adopt` records such a release when this repository does not hold its matched write, or when that write's event carries the `; unchecked:` mark. It refuses one whose members match no recorded write: an `include`, `drop` or `advance` records that composition, and `adopt` then records the release ([adopt](#knives-release-adopt-name---why-text)).
- A release whose every parent is a carrier makes every write refuse, with the remedy that `adopt` prints ([adopt](#knives-release-adopt-name---why-text)). An unrecorded published release that the publish remote does not hold gets [the published remedy](#the-published-remedy).
- A release whose name has no recorded write at all is not refused, unless its every parent is a carrier. So a fork whose ledger predates knives is not blocked.
- After a hand restore of a release (`jj restore --from <commit> --into <name> <path>...`), run `knives release adopt <name> --why '<what was restored and why>'`, then cut.

### When knives cannot check the release in hand

knives cannot check the release in hand against its recorded writes in two cases. In the first, this repository does not hold the matched write: another machine made it, and nobody fetched it here. In the second, no recorded write matches the release's parents, carriers left out: the members changed outside knives. Then `cut`, `include`, `drop`, `advance` and `rebase` do not refuse. The exception is a release whose every parent is a carrier, which every write refuses ([adopt](#knives-release-adopt-name---why-text)). Each other write goes ahead. Before it writes, it prints one of these lines, after the repository's name:

```text
knives could not check <name> at <commit> against its recorded writes: the recorded write its members match, the recorded <cut|edit> <id>, is not in this repository; knives did not fetch; `knives release adopt <name>` records it
knives could not check <name> at <commit> against its recorded writes: its members match no recorded write, so they changed outside knives; knives did not fetch; make the change with `knives release include`, `drop` or `advance` (a member added by hand: drop it, then include it); that write records the new composition, marked unchecked
```

- The write's ledger event ends with `; unchecked: W absent` or `; unchecked: no match` ([ledger](ledger.md#automatic-entries)). `audit` shows a note while that write is the newest write of the name ([reports](reports.md#knives-audit-repo---all---no-github)). So no later pod reads the release as measured.
- The mark is sticky. The release stays unchecked while the event of the recorded write that it equals or matches carries the mark. A hand edit over such a write is unchecked too, not refused. Each write over it marks its own event with the same reason, and prints:

  ```text
  knives could not check <name> at <commit> against its recorded writes: the release in hand is unchecked since the recorded <cut|edit> <id> (<reason>); knives did not fetch; `knives release adopt <name>` records it
  ```

- `knives release adopt <name>` ends the mark. Its event carries no mark. Nothing measured the marked write, so `adopt` measures the release against a fresh merge of its members.

### Why `adopt` beside the fixed recording path

The fixed scheme's cut records a published commit that the ledger lacks (the recording path above). `adopt` is still needed:

- Dated and series names have no recording path.
- The recording path records only what the publish remote holds. `adopt` records a resolution before it is pushed.
- `adopt` records a name that this repository holds only as the publish remote's ref.
- A fixed release pushed by the fast-forward rebase has a carrier. The candidate leaves the carrier out, so it no longer matches the published commit. The recording path and its "nothing to cut" refusal do not fire for it. `adopt` is how it is recorded.

## `knives release rebase [REF]`

- Moves members and the release merge onto the target. Member bookmarks and workspaces follow, and recorded resolutions replay. Every other release name that is pushed or published stays on its commit, and the output says so. One never pushed follows its rewrite, also said. It does not authorize a broad raw rebase.
- Bare, it targets the first upstream trunk commit that holds the merge commits of all our PRs that **merged, not closed**. With none merged, there is no default, so give a commit.
- A merge commit missing from local trunk refuses until `sync`. An outdated knives immutability rule refuses before any move and names `start <branch>` to refresh it.
- Pushed or published earlier releases keep their names and commit ids, and every tag stays where it was. The rebase's copies of them that nothing rests on are abandoned together and reported. A copy stays when a bookmark, a workspace working copy, or a descendant outside the set rests on it. A note says why. Copies stacked only on each other go together.
- After a bare rebase, also a no-op, members with merged PRs and no work past the target are removed, with the reason recorded. `--no-drop` keeps them. A branch with later work stays and says so.
- An unheld stale parent refuses, exit `3`. The refusal names its continuing branch and `advance`, or `drop` when none continues. A legacy trunk parent is shed, because base is not membership.
- When every member merged, rebase refuses to leave trunk as the only parent. A drop of the last parent also refuses. Reap the composition or include new work.

## `knives release reap`

Forgets superseded dated refs locally and on tracking remotes, then abandons their merges. It also runs after a dated cut. **No remote repository changes.** A fetch can bring tracking refs back. Standalone `reap` prints one JSON or TOON document (`repo`, `reaped`, `forgotten_only`, `kept`, `notes`, `abandoned_earlier`) under `--json` or for an agent; the reap inside a cut prints prose with the cut.

- While the live cut has unresolved conflicts, every superseded cut stays as the resolution record.
- A cut pinned outside release refs (a tag, an untracked remote bookmark, a local bookmark of another name such as `keep/…`) has its refs forgotten and its commit kept, with the pin named. Exit `0`, which is normal for a tagged or retained release.
- A superseded cut that something rests on stays untouched, its refs too, and the output names what rests on it. Exit `1`, except for the quiet keep below. That is a workspace working copy on it or above it, empty or not, or any other descendant, on the cut, on a commit an edit wrote under its name, or on a rewrite of either: the rule an edit uses to keep the commit it replaced. Abandoning the cut would rewrite each of them onto the cut's parents, which can leave a working copy's workspace stale and make it a conflicted merge. A cut made from the workspace that sits on the previous cut keeps that cut too. Move your own empty working copy (`jj new <live cut>`, which drops it), then run `knives release reap`. A working copy with changes stays a descendant until it is rebased off the cut. Another workspace's working copy is its owner's to move.
- The quiet keep: a superseded name kept only for what that name published exits `0`, and the output still has its `kept <name>` line. Two conditions must be true. Every descendant of the name's ref commits and earlier writes is reachable from the name's own ref on the publish remote, or from a tag on one of those commits or descendants. And nothing else names any of those descendants: no workspace working copy, no local bookmark, no other remote bookmark and no other ref. The name's own local bookmark does not count. The old name after [the published remedy](#the-published-remedy) is kept so: consumers were given what it published, and the new cut redoes it. This rule applies to `knives release reap` and to the reap after a cut. Every other keep exits `1`.

## `knives release members`

```text
knives release members [REF] [--verify]
knives release members [REF] --carries REV
knives release members --census [--no-github]
```

- Bare: direct parents, commits, holding bookmarks, and branch tips advanced past them. `REF` picks another release.
- `--verify` replays each member into the release, one replay per member. Problems exit `3`, missing or unexplained content `1`, else `0`.
- `--carries REV` asks whether the net content of the revision is carried. With a target, it checks only that target. Without one, it checks live releases and upstream trunk, then superseded cuts.
- `--census` asks the deletion-safety question for every maintained branch. `--no-github` is valid only with `--census`, and the orphan test is then unknown. It does not combine with `REF`, `--verify` or `--carries`.

The verdicts are those in the main skill: `carried-exact`, `carried-rewritten`, `carried-rebased`, `NOT carried`, `conflicted`. An explicit target exits `0` for any carried verdict, `1` for not carried or conflicted, `3` when unresolved or uncheckable. A superseded target can say carried, but the untargeted safety question still needs a live release or upstream trunk. Neither grep nor absence from bare `members` answers deletion or rebase safety.

## Publishing

Release commands never push. Publish a new release name with `jj git push --remote <publish-remote> --bookmark <name>`.

A release that the publish remote moves only forward (a fixed release branch) needs a fast-forward. A knives write does not descend from the published head, so git refuses a plain push of it. Rebase it onto the published head, adopt it, then push the commit:

```sh
jj git fetch --remote <publish-remote>                                   # read the published head as it is now
old=$(jj log -r '<name>@<publish-remote>' --no-graph -T commit_id)     # the published head
pre=$(jj log -r <name> --no-graph -T commit_id)                         # the write knives made
jj rebase -r <name> -d <parent-1> -d <parent-2> -d "$old"               # each parent as its own -d, in the release's order, then the published head
jj restore --from "$pre" --into <name>                                   # the rebase can add the published head's content; put the write's tree back
JJ_USER='<author>' JJ_EMAIL='<email>' jj metaedit --update-author <name> # only when the author must change
knives release adopt <name> --why "fast-forward of $pre onto the published head $old"
git push <publish-remote> "$(jj log -r <name> --no-graph -T commit_id):refs/heads/<name>"
```

- Run `adopt` after the last rewrite, the author change included, and before the push. The publish ref still names the old head then, so the old head reads as a carrier, and the ledger's newest write is the commit that the push publishes.
- Change the release commit's author only with `jj metaedit`. It keeps jj's `change-id` header in the git commit. Git plumbing drops that header, and a fresh pod then cannot read the pushed copy as a carrier by its change id.
- Push with no `+` and no `--force`. Stop when git refuses. Never use `jj git push` here: it moves the remote branch whenever the branch is still where jj last fetched it, as `git push --force-with-lease` does, so it hides the non-fast-forward refusal that is this branch's only backstop.
- `adopt` writes a ledger entry, which starts the background sweep, and the sweep pushes a shared ledger by itself. So when git then refuses the push, the ledger holds a recorded write that the fork does not hold. Nothing in the ledger says that the fork published it. The next pod starts from the published head, which an earlier `adopt` recorded, so its writes are not refused. Its audit reports `release-drift` against the newest recorded write, which it lacks, until its first write or an `adopt`.
- Without the `adopt` step, nothing records the pushed head. When the head's parents match a recorded write that the next pod does not hold, that pod cannot check the head. Its first write goes ahead, prints the unchecked line and marks its event `; unchecked: W absent` ([unchecked](#when-knives-cannot-check-the-release-in-hand)). One `adopt` records the head. After two pushes in a row without `adopt`, a fresh pod that holds neither recorded write still reads the older pushed copy as a carrier by its change id. So the head matches the newer write, which that pod does not hold, and the write goes ahead unchecked in the same way. `adopt` records it.
