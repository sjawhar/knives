# Composing a release

Read this before you plan, cut, `include`, `drop`, `advance` or `rebase` a release, or read `knives release members`. Hand changes, unchecked writes, published releases, reap and pushing: [releases.md](releases.md).

The release model, the verbs and the release-log conflict union: README.md, "Release workflow", and `knives release <verb> --help`. One branch is the member, and the upstream PR head unless the two need different bases (fork-work §6).

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
- the write would lose lines of a remaining member (`include`, `drop` and `advance` only). The edit names each path, member and line, and writes nothing. A line at a conflict that the write leaves is not counted. Read the named lines first. Then run the edit again with `--allow-loss '<reason>'`, put the lines back by hand, and record that with `knives release adopt <name> --why '<reason>'` ([releases.md](releases.md)).

`--allow-loss` and the cut's `--allow-drop` differ. `--allow-drop` lets a cut leave out members, commits, a fix by hand, or a newest write that this repository does not hold. `--allow-loss` lets an edit write a file that loses a remaining member's lines. Each needs a stated intent; neither is a way past a refusal you have not read.

An unreadable consumer, or a shared ledger that did not pull, blocks every plan, cut and edit. With no consumer registered or passed, the plan treats the release as unpinned.

## `knives release cut`

- It audits first: the cut tree must hold each member's own lines, the change from that member's own fork point with upstream trunk to its tip. The audit names each line the tree lacks, with its path. A line that the previous release also lacks is carried forward, not a loss. A failed audit writes nothing.
- Refused, exit `3`: a dated name that does not sort after the newest; the same tree and parents as the previous published cut (use `include`, `advance` or `rebase`); an unrecorded release in hand.
- When every pin of the previous dated release is frozen, or it is published, a new later name may start identical (exit `0`). Edit it before you push.
- The orphan gate and the member gate refuse to strand commits that only the previous lineage reaches, or to lose a member the previous cut recorded, and list them. A bookmark that is not a release name keeps a commit when it is local, or on `origin` or the publish remote, tracked or not. The upstream trunk keeps a commit too. Any other bookmark of `upstream`, or of any other remote, keeps nothing. `--allow-drop` states an intended drop; it is not a remedy for a refusal. Keep the reason and its dependency effects.
- Under the fixed scheme, when the last cut event is missing or names another commit than the published branch, `cut` records the published commit instead of making one.

## `knives release rebase [REF]`

- Other pushed or published release names, and every tag, stay on their commits.
- A merge commit missing from local trunk refuses until `knives sync`.

## `knives release members [REF]`

- Bare: the direct parents of the release in hand, their commits and holding bookmarks, and branch tips advanced past them. Name `REF` for an older release.
- `--verify` reads each member's own lines in the release, from the member's fork point with upstream trunk. It names each line the release lacks, with its path, and each line the release holds only by its words. Exit `1` for a missing member, `3` for problems.
- A member reads inconclusive only where the release, or the member's tip, holds an unresolved conflict in one of its paths and the member lacks nothing else. A member with such a path and a lost line elsewhere reads missing.
- `--carries REV` and `--census`: SKILL.md, "Is this content carried?".
