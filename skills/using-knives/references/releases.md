# Release model, commands and evidence

Read this before planning, editing, cutting, rebasing or reaping a release, or
when interpreting membership/carriage output. Commands write locally and never
push. Publication is a separate `jj git push --remote <publish-remote>
--bookmark <name>`: use the distinct release remote when configured, else origin.

## Plan and preserve the composition

`knives release [--repo REPO] [--consumer DIR]...` without a subcommand plans:
what a cut would contain, whether parents still match branch tips, local branches
absent or advanced past their released parents, members carrying prior release
merges, and consumer pins. Put these options before a subcommand, for example
`knives release --consumer DIR include feat/x`.

A release is a flat octopus merge. Its direct parent set is its membership.
Upstream base is reachable through the members, never a direct parent. A member
that merges upstream remains a member until an explicit edit changes that.

- `include` adds one parent.
- `drop` removes one, stating when no remaining member carries its content.
- `advance` moves members to branch tips.
- `rebase` moves the composition's shared base.

Edits rebuild by duplicating the release onto the changed parent set, retaining
recorded conflict resolutions; the changed composition can introduce new conflicts.
Keep stated cross-fork dependencies: retaining a dependent while dropping its
requirement can ship a release that cannot work.

### One branch is both release member and upstream PR head

The branch forks from the release's shared base and is linear past it. A branch
may intentionally rebase onto newer upstream, for example at a maintainer's
request; that is not a side effect of starting work. `release advance` follows
its change id as well as ancestry. To move the whole composition, use
`release rebase`. A member's fork point alone is not a finding.

Never mint a second “release-lineage” or “sibling” branch to carry the same PR
content on an older base. If it cannot compose, rebase the release rather than
split review and conflict resolution across two copies.

### Raw rebase and retained releases

Release-aware `advance`/`rebase` reshape the release in hand. A raw
`jj rebase -b`/`-s`/`-r` can also reach older release/keep merges that still pin
member commits for consumers or conflict-resolution evidence. Before rebasing
by hand, use **fork-work**'s bidirectional check:

1. Is the branch a parent of any release/keep merge?
2. Does it descend from any release/keep merge?

Bare `release members` answers neither question for retained older releases; it
only sees the current composition. Name another release explicitly to inspect
its parents. Preserve already-published merge commit ids.

A member must carry no merge past known trunk positions. A branch built on a
release merge carries all that merge's parents. `include`, `advance` and the
first `cut` refuse such `stacked-history`; planning reports existing stacked
members. Trunk positions are `<trunk>@upstream`, `<trunk>@origin`, local trunk.
A merge reachable from one is trunk's, not the branch's. If all local views lag
behind the branch base, the detail warns these may be upstream's own merges;
`sync` fetches them.

Once that merge was pushed anywhere, raw `jj rebase -b <branch> -d <trunk>` is
not the repair: its root computation includes and rewrites the release merge.
Rebase only the branch's own commits, with the release/keep ref identified by
the ancestry check:

```text
jj rebase -s '<release-or-keep-ref>..<branch>' -d <trunk>@upstream
```

This keeps branch change ids for `advance` and leaves the merge id untouched.
Plain `-b` is safe from this particular hazard only if the merge was never pushed.
The other direction of the ancestry check still matters.

When a released parent merges upstream and its branch continues, ancestry alone
cannot identify it: every fresh trunk branch descends from that parent. The
cut/edit record names the member. Planning offers `advance <branch>` to move it
or `rebase` to retire the merged parent; `include` refuses a second copy. A named
advance says when its match rests on the record alone.

A cut carries exactly what its parents hold. The plan says `N parent(s), flat`
only when no member carries a prior release merge; otherwise it counts stacked
members.

## Release names and consumer gates

**Dated scheme:** when `release_branch` is absent, valid names are exactly
`release/YYYY-MM-DD` or `release/YYYY-MM-DD.N`, never `.N.M`. Create names only
with `release cut NAME`, when the in-place edit pin gate requires a new name.
Do not hand-compose a release bookmark and then advance it.

**Fixed scheme:** `release_branch = "<name>"` means cuts advance that branch in
place through jj's internal allow-backwards mechanism. Use `release cut` without
a name; a dated name is refused. The cut uses the local composition in hand,
including unpushed edits. Release verbs report the published position from the
local `<name>@<publish-remote>` tracking bookmark, not a live remote read. Fetch
first when freshness matters; `consumers` and `pushed` query the live remote.

Every planning/cut/edit verb reads registered `consumers` forge slugs and
repeatable `--consumer <DIR>` ad-hoc scans. With neither, no pin is known: the
plan says so, treats the release as unpinned, and verbs proceed. An install-based
consumer may have no lockfile to register. A configured/passed unreadable
consumer is a problem and blocks the verbs until readable. So does a shared
ledger the plan could not pull first, since the drop guard would check against
a stale cut ([ledger](ledger.md#sharing-between-machines)).

`include`, `drop`, `advance` and `rebase` share these incomplete (`3`) refusals:

- Every pin **of this release** is frozen on a revision: editing in place would
  reach nobody. Under the dated scheme, create a new dated name. Under the fixed
  scheme, the refusal instead names updating the frozen consumer pins or changing
  the release scheme before editing; a fixed branch cannot reach revision pins.
  Consumers frozen on older releases do not block this edit. A release with no
  pins is freely editable. A pin is of this release if it names the branch or
  freezes the exact commit the publish remote holds, including a lock's
  `?rev=<sha>` or `#<sha>` fragment.
- Upstream trunk cannot be resolved: base and members cannot be separated;
  fetch upstream first.

## `knives release cut [NAME] [--allow-drop]`

A normal cut carries the composition in hand: previous parents **verbatim by
commit id**, even if a member bookmark is now divergent. It neither adds branches
nor advances members. Only the first cut, with no composition to carry, starts
from every branch.

The candidate is audited before naming it. Each member's net diff from the
members' fork point with upstream trunk must be present in the cut tree.
Divergence already carried by the prior release (recorded conflict resolution)
is reported as carried forward, not refused. Failed audit writes no cut;
a passing audit creates and names the release as one operation, except for the
existing-published-composition recording path below.

### Cut gates and explicit drops

1. A dated name must sort after the newest release. Reusing its name, an older
   date or an older suffix is refused before building, exit `3`, because reaping
   keeps only the newest name.
2. The orphan gate refuses to strand commits reachable only from previous lineage.
   `--allow-drop` states the intentional exception.
3. Before content audit, a candidate with the **same tree and same parents as the
   previous published cut** is normally refused as identical, exit `3`, creating
   nothing. The two exceptions below concern frozen dated pins and an unrecorded
   published fixed composition. New membership comes through `include`, new
   member tips through `advance`, and a new base through `rebase`, not through a
   gratuitous name change.
4. The composition gate uses the previous cut's ledger event, which survives a
   moved release bookmark. A recorded member must be a parent, an ancestor of
   a member/base, have a reachable rewrite of its change id, or have its net diff
   carried in the candidate tree. A member entering through upstream base passes.
   A missing member refuses the cut even if lost by a hand-rebuilt merge,
   out-of-band bookmark movement or `drop` since the last cut. An unresolvable
   recorded commit counts as dropped, never as carried.

`--allow-drop` is an explicit statement that the drop is intended, not a repair
suggestion. The new cut event names exactly which recorded members were dropped.
Preserve both the reason and dependency consequences.

### Identical-cut exceptions

When **every pin of the previous dated release is frozen**, editing that release
would reach nobody. A new dated name sorting after it may therefore start with
identical parents/tree, exit `0`, and says to `include`, `advance`, `drop` or
`rebase` that new unpinned composition **before pushing**. It is the composition
to edit, not a reason to publish identical content.

A fixed release cannot use the dated-name exception. It has a separate
**ledger-recording** path: if the last cut event is missing or records a different
commit from the published fixed branch, `cut` audits that published composition
and applies the recorded-composition/drop checks, then records the existing
published commit as the cut. It creates no new commit and pushes nothing.
Inconclusive audit/composition evidence can still yield findings. If the ledger
already records that published commit, an identical fixed cut remains refused.

A member rewritten to the same content is a different parent, so that is a new
composition. The comparison is with the published copy, not the identical local
duplicate. An unpushed previous cut has no published consumer position to protect
and is not compared.

Cut/edit events retain full parent commits and every bookmark on them, plus the
cut's change id, in the [ledger](ledger.md). Use this structured evidence rather
than inferring lost membership from today's bookmark names.

## Editing members

### `knives release include <branch> [--why "..."]`

Adds one branch or revision as a parent, changing nothing else. It does not move
an existing member that grew, rebased (inside or outside jj), or merged upstream;
that is `advance`. It refuses a second copy and explains the match. A tip already
reachable from trunk is refused: adding a base parent would move the shared base.
Use `rebase` onto trunk containing it instead.

### `knives release drop <branch> --why "..."`

Removes the member parent without touching the branch/bookmark. A moved branch
matches its parent through ancestry or that parent's change id on the branch.
For a rebuild outside jj, name the parent's commit id: destructive removal does
not guess from a reused name. `--why` is required and recorded on the release
commit; omitting it is a usage error.

### `knives release advance [<branch>...] [--from <old-sha>]`

Named branches move exactly; bare advances every member whose branch moved.
Ancestry matches a grown branch; the parent's change id past trunk matches a jj
rewrite. Trunk-reached parents have no unique ancestry successor, because every
new trunk branch descends from them; `rebase` retires them.

For a **named** branch unmatched by ancestry/change id, the last cut/edit event's
`branch@commit` record can identify its parent. The output says that match rests
on the record alone: reusing a bookmark name for unrelated work would move that
member onto unrelated content.

Bare advance refuses when the same branch would succeed more than one parent.
A stacked integration branch is not evidence it replaced all of them.
`--from <old-sha>` bypasses matching by naming the exact parent to replace;
it requires exactly one named branch, useful when no record of it exists.
Neither form advances onto a trunk-reached tip, which would create a base parent.

## `knives release rebase [REF]`

Moves member branches and the release merge onto the target, with bookmarks and
workspaces following and recorded conflict resolutions replaying as ordinary
rebase semantics. It is the release-aware equivalent of rebasing the composition,
not authorization to run a broad raw branch rebase yourself.

Bare, it asks which of our PRs **merged, not closed**, then targets the first
upstream trunk commit containing all their merge commits. With none merged there
is no default; supply a commit. If a merge commit is missing from local trunk,
it refuses until `sync` fetches it. An outdated knives-written immutability rule
also refuses before moving anything and names `start <branch>` to refresh it.

Earlier published releases on member commits retain tags and original commit
ids. Untagged copies created by the rebase are abandoned together and reported
as copies of those originals. A copy stays if a bookmark, workspace working copy
or descendant outside that abandoned copy set rests on it; a note names why.
Copies stacked only on each other can be abandoned together.

After a bare rebase, including an already-at-target no-op, members with merged
PRs and no work past the target are removed with the reason recorded.
`--no-drop` keeps them. A branch carrying later work stays and says so.

An unheld stale parent refuses as incomplete, naming its continuing branch by
ancestry/change id and `advance`, or `drop` if none continues it. A legacy trunk
parent is shed because base is not membership. If every member merged upstream,
rebase refuses to leave trunk as sole parent; dropping the final parent also
refuses. Reap the obsolete composition or include new work.

## `knives release reap`

Forgets superseded dated refs locally and across tracking remotes, then abandons
their merge commits. It also runs after successful dated cuts. **No remote
repository is modified.** A later fetch can restore tracking refs.

While the live cut has unresolved conflicts, every superseded cut is retained as
the prior resolution record. A cut pinned outside release refs, such as by a tag
or untracked remote bookmark, has its refs forgotten but its commit kept, with
that pin named; exit `0`, expected for a tagged release. A superseded cut with
local descendants (someone's stacked work) remains untouched; exit `1`.

## `knives release members`

The parser keeps these questions separate:

```text
knives release members [REF] [--verify]
knives release members [REF] --carries REV
knives release members --census [--no-github]
```

- Bare: read direct parents, commits, holding bookmarks and branch tips advanced
  past them. `REF` inspects a named release instead of the release in hand.
- `--verify`: replay each member into that release and report missing content;
  one replay per member. Problems return `3`; missing/unexplained audit content
  returns `1`; otherwise `0`.
- `--carries REV`: ask whether that revision's net content is carried. With a
  target, check only it; without one, check live releases and upstream trunk,
  consulting superseded cuts only after those miss.
- `--census`: ask the deletion-safety question for every maintained branch.
  `--no-github` is only valid beside `--census`; its orphan test is then unknown.
  It cannot be combined with `REF`, `--verify` or `--carries`.

Use the main skill's exact `carried-exact`, `carried-rewritten`, `carried-rebased`,
`NOT carried` and `conflicted` meanings. An explicit target returns `0` for any
carried verdict, `1` for not-carried/conflicted, `3` for unresolvable/uncheckable.
A superseded target can return carried; the untargeted safety question still
requires a live release or upstream trunk. Neither source grep nor absence from
bare `members` answers the deletion/rebase safety question.
