# Release model, commands and evidence

Read before you plan, edit, cut, rebase or reap a release, or read membership or carriage output. Commands write locally and never push. Publish separately with `jj git push --remote <publish-remote> --bookmark <name>`. The publish remote is the release remote when configured, else origin.

## Plan and keep the composition

`knives release [--repo REPO] [--consumer DIR]...` with no subcommand plans. It shows what a cut would hold, and whether parents still match branch tips. It shows branches missing or advanced past their released parent, members that carry prior release merges, and consumer pins. Options go before the subcommand: `knives release --consumer DIR include feat/x`.

- A release is a flat octopus merge. Its direct parents are its members. Members reach the upstream base, which is never a direct parent. A member that merges upstream stays a member until an edit changes that.
- `include` adds one parent. `drop` removes one, and says when no other member carries its content. `advance` moves members to branch tips. `rebase` moves the shared base.
- Edits duplicate the release onto the new parent set and keep recorded conflict resolutions. The new composition can bring new conflicts.
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

## `knives release cut [NAME] [--allow-drop]`

- A normal cut carries the composition in hand: previous parents **verbatim by commit id**, even if a member bookmark is now divergent. It adds no branches and advances no members. Only the first cut, with no composition, starts from every branch.
- The cut audits the candidate first. The net diff of each member, from the members' fork point with upstream trunk, must be in the cut tree. Divergence that the prior release carried (a recorded resolution) is reported as carried forward.
- A failed audit writes no cut. A pass makes and names the release in one operation, except for the fixed recording path below.

### Gates

1. A dated name must sort after the newest release. A reused name, an older date or an older suffix is refused before the build, exit `3`, because reap keeps only the newest name.
2. The orphan gate refuses to strand commits that only the previous lineage reaches. `--allow-drop` states the intended exception.
3. Before the content audit, knives compares the candidate with the previous published cut. The **same tree and parents** is refused as identical, exit `3`, except as below. New members come through `include`, new tips through `advance`, and a new base through `rebase`, not through a new name.
4. The composition gate reads the ledger event of the previous cut, which survives a moved bookmark. Each recorded member must be a parent or an ancestor of a member or the base. Otherwise it needs a reachable rewrite of its change id, or its net diff in the candidate tree. A member that came through upstream base passes.
5. A missing member refuses the cut, however it was lost: a merge rebuilt by hand, a moved bookmark, or a `drop` since the last cut. A recorded commit that does not resolve counts as dropped.

`--allow-drop` states that the drop is intended. It is not a remedy for a refusal. The new cut event names the dropped members. Keep the reason and its dependency effects.

### Identical-cut exceptions

- When **every pin of the previous dated release is frozen**, a new later dated name can start identical, exit `0`. It says to `include`, `advance`, `drop` or `rebase` **before you push**. It is the composition to edit, not identical content to publish.
- A fixed release has a **ledger-recording** path instead. It applies when the last cut event is missing or names a different commit than the published branch. Then `cut` audits that published composition, runs the composition and drop checks, and records that commit as the cut. It makes no commit and pushes nothing. Weak evidence can still give findings. When the ledger already records that commit, an identical cut is refused.
- A member rewritten to the same content is a new parent, so it is a new composition. The comparison is with the published copy. An unpushed previous cut is not compared.

Cut and edit events keep full parent commits, every bookmark on them, and the commit and change id each wrote, in the [ledger](ledger.md). Use that evidence, not the current bookmark names, to find lost members.

## Editing members

- **`include <branch> [--why "..."]`** adds one branch or revision as a parent. It does not move a member that grew, rebased (inside or outside jj) or merged upstream, which is `advance`. It refuses a second copy and explains the match. It refuses a tip that trunk already reaches, because that moves the base. Use `rebase` onto that trunk instead.
- **`drop <branch> --why "..."`** removes the parent, not the branch. A moved branch matches by ancestry or by the change id of the parent. For a rebuild outside jj, name the commit id of the parent. Removal never guesses from a reused name. `--why` is required and recorded on the release commit. With no `--why`, it is a usage error (exit `2`).
- **`advance [<branch>...] [--from <old-sha>]`** moves named branches exactly. Bare, it moves every member whose branch moved. Ancestry matches growth, and the change id past trunk matches a jj rewrite. Parents that trunk reaches have no unique successor, and `rebase` retires them.
- A named branch with no ancestry or change-id match can match by the `branch@commit` record of the last event. The output says so, because a reused name would move the member onto unrelated content.
- Bare advance refuses when one branch would replace more than one parent. A stacked integration branch does not prove that it replaced them. `--from` names the exact parent to replace and needs exactly one named branch. Use it when no record of the parent exists. Neither form advances onto a tip that trunk reaches.
- Each edit writes a new commit under the release name and abandons the commit it replaced, so no anonymous head stays, and says so. The old commit stays, with a note naming why, when another bookmark, a tag, a remote ref other than the release's own tracked ref on its publish remote, a workspace working copy on it or above it, or any other descendant rests on it. A working copy parked on the release keeps it because rewriting that working copy would leave its workspace stale; the audit does not report it as an anonymous head while the working copy is there. Every later edit of the name, and the reap that forgets the name, also abandons each earlier commit the ledger records under it that is now a visible head nothing rests on (no publish-ref exception here), such as one left behind when that working copy moved on, and says so. An edit that leaves the release with conflicts abandons no earlier commit: they are the record of the last resolution, as with reap. The publish remote keeps the old commit until you push.

## `knives release rebase [REF]`

- Moves members and the release merge onto the target. Bookmarks and workspaces follow, and recorded resolutions replay. It does not authorize a broad raw rebase.
- Bare, it targets the first upstream trunk commit that holds the merge commits of all our PRs that **merged, not closed**. With none merged, there is no default, so give a commit.
- A merge commit missing from local trunk refuses until `sync`. An outdated knives immutability rule refuses before any move and names `start <branch>` to refresh it.
- Earlier published releases keep their tags and commit ids. Untagged copies from the rebase are abandoned together and reported. A copy stays when a bookmark, a workspace working copy, or a descendant outside the set rests on it. A note says why. Copies stacked only on each other go together.
- After a bare rebase, also a no-op, members with merged PRs and no work past the target are removed, with the reason recorded. `--no-drop` keeps them. A branch with later work stays and says so.
- An unheld stale parent refuses, exit `3`. The refusal names its continuing branch and `advance`, or `drop` when none continues. A legacy trunk parent is shed, because base is not membership.
- When every member merged, rebase refuses to leave trunk as the only parent. A drop of the last parent also refuses. Reap the composition or include new work.

## `knives release reap`

Forgets superseded dated refs locally and on tracking remotes, then abandons their merges. It also runs after a dated cut. **No remote repository changes.** A fetch can bring tracking refs back.

- While the live cut has unresolved conflicts, every superseded cut stays as the resolution record.
- A cut pinned outside release refs (a tag, an untracked remote bookmark) has its refs forgotten and its commit kept, with the pin named. Exit `0`, which is normal for a tagged release.
- A superseded cut with local descendants (stacked work) stays untouched. Exit `1`.

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
