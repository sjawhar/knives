# Composing a release

Read this before you plan, cut, `include`, `drop`, `advance` or `rebase` a release, or read `knives release members`. Hand changes, unchecked writes, published releases, reap and pushing: [releases.md](releases.md).

The release model, the verbs and the release-log conflict union: README.md, "Release workflow", and `knives release <verb> --help`. One branch is both member and PR head (fork-work §6).

## The model's traps

- A member carries no merge past the known trunk positions (`<trunk>@upstream`, `<trunk>@origin`, local trunk). `include`, `advance` and the first `cut` refuse such `stacked-history` and list the commits. The exception is a merge, not a release cut, of members the release already carries: `include` and a named `advance` take it and name the merge.
- `advance` and `rebase` change only the release in hand. Before a raw `jj rebase` of a member, run fork-work's bidirectional gate (§7); it holds the commands.
- Keep stated dependencies (`knives depends`): a dependent without its requirement ships a release that cannot work.
- knives never renumbers a release log: renumber by hand and record it with `adopt --why`. Every conflict but the log union is yours.

## The verbs

- `--repo` and `--consumer DIR` go before the subcommand (`knives release --consumer DIR include feat/x`). `--series <name>` acts on a named series; without it, the default series.
- `include` never moves a member that grew, was rebased or merged upstream (that is `advance`), and refuses a tip that trunk already reaches (that is `rebase`).
- `advance` matches by ancestry, then by change id. A named branch can also match by the last event's `branch@commit` record, and says so: a reused name would move the member onto unrelated content. `--from <old-sha>` names the parent to replace, for exactly one branch, when nothing else matches (a `jj duplicate` rebuild).
- `drop` with a moved branch finds the parent by ancestry or change id; after a rebuild outside jj, name the parent's commit id.
- Each edit writes a new commit under the name and abandons the one it replaced, unless something else rests on it.

## Names, series and schemes

- Dated (default): `release/YYYY-MM-DD` or `release/YYYY-MM-DD.N`, never `.N.M`. Make a name only with `release cut NAME`, and a new one only when an edit in place is refused (frozen pins, a published release). It must sort after the newest release.
- Named series (`[repos.<name>.series.<series>]`): cuts `release/<series>/YYYY-MM-DD[.N]` on its own base. A branch belongs to the series whose base it forks from latest; a verb refuses a branch of another series and names that series. `status` and `repos` show only the default series; read a named one with `knives release --series <series>`.
- Fixed (`release_branch`): `cut` takes no name and moves that branch in place. Release verbs read the published position from the local `<name>@<publish-remote>` tracking bookmark, so fetch first when freshness matters.

## When an edit refuses

`include`, `drop`, `advance` and `rebase` refuse (exit `3`) and name the remedy when:

- every consumer pin of this release is frozen on a revision, so an in-place edit reaches nobody. A pin is of this release when it names the branch or freezes the commit the publish remote holds. Pins on older releases do not block.
- the release is published (a tag or a `record: published` note names its commit): cut a new dated name first.
- upstream trunk does not resolve: fetch upstream.
- the release in hand is unrecorded: adopt it ([releases.md](releases.md)).

An unreadable consumer, or a shared ledger that did not pull, blocks every plan, cut and edit. With no consumer registered or passed, the plan treats the release as unpinned.

## `knives release cut`

- It audits first: each member's net diff from the members' fork point with upstream trunk must be in the cut tree. A failed audit writes nothing and names each member whose change the tree lacks or changed.
- A member stacked on other members of the cut (a pull request into another member's branch) is held to its own change, replayed from the members it is stacked on, never from the trunk. Where members stacked on it rewrote its lines, the tree may hold them as those members left them, read at the files its own change touches only. Only the cut's members count: a branch stacked on a member that the cut does not take excuses nothing.
- A member is stacked when another maintained branch's tip is on its first-parent line; a branch merged into its line is not that. A member built on a branch the cut carries (the cut's base, another member, or the previous release a stacked cut builds on reaches its tip) needs no record: the audit holds it to its own change. A member built on a branch the cut does not carry records it: `knives depends <member> --on <branch>`, or the review pull request on the fork's own repository whose head that branch is.
- Refused, exit `3`, whatever `--allow-drop` says, before the audit (`cut` and `cut --at`): a member built on a branch the cut does not carry that records no base the graph bears out, or several (the refusal names the `knives depends` to run); a member whose recorded base the cut leaves out (`X is stacked on Y, which this release leaves out`), so that neither the trunk nor another member reaches where X's own change starts; a member its base took with no merge. Include Y, or leave out X too.
- Refused the same way at the member gate: a member stacked on a branch the release's record holds and the cut leaves out.
- Refused, exit `3`: a dated name that does not sort after the newest; the same tree and parents as the previous published cut (use `include`, `advance` or `rebase`); an unrecorded release in hand.
- When every pin of the previous dated release is frozen, or it is published, a new later name may start identical (exit `0`). Edit it before you push.
- The orphan gate and the member gate refuse to strand commits that only the previous lineage reaches, or to lose a member the previous cut recorded, and list them. `--allow-drop` states an intended drop; it is not a remedy for a refusal. Keep the reason and its dependency effects.
- Under the fixed scheme, when the last cut event is missing or names another commit than the published branch, `cut` records the published commit instead of making one.

## `knives release rebase [REF]`

- Other pushed or published release names, and every tag, stay on their commits.
- A merge commit missing from local trunk refuses until `knives sync`.

## `knives release members [REF]`

- Bare: the direct parents of the release in hand, their commits and holding bookmarks, and branch tips advanced past them. Name `REF` for an older release.
- `--verify` replays each member: exit `1` for missing or unexplained content, `3` for problems. A release whose commit is a hand edit on top of its cut is read through that cut's parents, so the edit cannot hide a member it dropped.
- `--carries REV` and `--census`: SKILL.md, "Is this content carried?".
