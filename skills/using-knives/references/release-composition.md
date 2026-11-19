# Composing a release

Read this before you plan, cut, `include`, `drop`, `advance` or `rebase` a release, or read `knives release members`. Hand changes, unchecked writes, published releases, reap and pushing: [releases.md](releases.md).

The release model, the verbs and the release-log conflict union: README.md, "Release workflow", and `knives release <verb> --help`. One branch is both member and PR head (fork-work §6).

## The model's traps

- A member carries no merge past the known trunk positions (`<trunk>@upstream`, `<trunk>@origin`, local trunk). `include`, `advance` and the first `cut` refuse such `stacked-history` and list the commits. The exception is a merge, not a release cut, of members the release already carries: `include` and a named `advance` take it and name the merge. In a [stacked series](stacked-series.md), a member can also be built on the previous release.
- `advance` and `rebase` change only the release in hand. Before a raw `jj rebase` of a member, run fork-work's bidirectional gate (§7); it holds the commands.
- Keep stated dependencies (`knives depends`): a dependent without its requirement ships a release that cannot work.
- knives never renumbers a release log: renumber by hand and record it with `adopt --why`. Every conflict but the log union is yours. What the union joins and the lines it prints: [release-logs.md](release-logs.md).

## The verbs

- `--repo` and `--consumer DIR` go before the subcommand (`knives release --consumer DIR include feat/x`). `--series <name>` acts on a named series; without it, the default series.
- `include` never moves a member that grew, was rebased or merged upstream (that is `advance`), and refuses a tip that trunk already reaches (that is `rebase`).
- `advance` matches by ancestry, then by change id. A named branch can also match by the last event's `branch@commit` record, and says so: a reused name would move the member onto unrelated content. `--from <old-sha>` names the parent to replace, for exactly one branch, when nothing else matches (a `jj duplicate` rebuild).
- `drop` with a moved branch finds the parent by ancestry or change id; after a rebuild outside jj, name the parent's commit id.
- Each edit writes a new commit under the name and abandons the one it replaced, unless something else rests on it.

## Names, series and schemes

- Dated (default): `release/YYYY-MM-DD`, `release/YYYY-MM-DD.N` (digits, never `.N.M`) or `release/YYYY-MM-DD-<label>` (no `/`), in a form git can hold as a ref. Make a name only with `release cut NAME`, and a new one only when an edit in place is refused (frozen pins, a published release). A release composed outside knives: [`cut NAME --at REV`](hand-cut.md). It must sort after the newest release.
- Each series has one order: by date, then by the rest of the name in version order, where a run of digits compares as a number. So `-speed-8.0.9` sorts before `-speed-8.0.10`, and a `.N` name after each `-<label>` of its date. A later name on one date supersedes an earlier one; the reap keeps only the newest. A name outside the grammar is a usage error (exit `2`), never the newest release and never reaped.
- Named series (`[repos.<name>.series.<series>]`): cuts `release/<series>/YYYY-MM-DD[.N|-<label>]` on its own base. A branch belongs to the series whose base it forks from latest; a verb refuses a branch of another series and names that series. `status` and `repos` show only the default series; read a named one with `knives release --series <series>`.
- Fixed (`release_branch`): `cut` takes no name and moves that branch in place. Release verbs read the published position from the local `<name>@<publish-remote>` tracking bookmark, so fetch first when freshness matters.

## When an edit refuses

`include`, `drop`, `advance` and `rebase` refuse (exit `3`) and name the remedy when:

- every consumer pin of this release is frozen on a revision, so an in-place edit reaches nobody. A pin is of this release when it names the branch or freezes the commit the publish remote holds. Pins on older releases do not block.
- the release is published (a tag or a `record: published` note names its commit): cut a new dated name first.
- upstream trunk does not resolve: fetch upstream.
- the release in hand is unrecorded: adopt it ([releases.md](releases.md)).

An unreadable consumer, or a shared ledger that did not pull, blocks every plan, cut and edit. With no consumer registered or passed, the plan treats the release as unpinned.

## `knives release cut`

- It audits first: each member's net diff from the members' fork point with upstream trunk must be in the cut tree. A failed audit writes nothing.
- Refused, exit `3`: a dated name that does not sort after the newest; the same tree and parents as the previous published cut (use `include`, `advance` or `rebase`); an unrecorded release in hand.
- When every pin of the previous dated release is frozen, or it is published, a new later name may start identical (exit `0`). Edit it before you push.
- The orphan gate refuses to strand commits that only the previous lineage reaches, and lists them.
- The composition gate reads the previous cut's event and the newest recorded write (cut, newest edit or `adopt`) of the series' newest release. The cut must carry each member either records: as a parent, an ancestor of a member or the base, a rewrite that stands for it (a successor in jj's operation log, or a commit with its tree; not an older version with only its change id), or its net diff. This repository must hold the newest write, or the cut refuses.
- It also holds every `adopt`'s fix by hand (each file it resolved by hand or changed beyond its baseline's conflicts) at every later cut and `cut --at`, file by file, against the file's reference: the newest recorded write holding the file with no conflict. A deleted file's absence is a fix too. Only the cut's own tree counts. Where this repository can measure the fix, a later write that removes or changes it at a file ends the hold there. A clone that cannot (one without the adopt's commit, say) holds each file to what its reference holds, nothing included. There, an older adopt event with no `fix_files` ([ledger.md](ledger.md#entry-fields)) whose text does not read whole refuses every cut and record.
- A cut that leaves a held file conflicted, where the newest write holds that conflict too, goes ahead with exit `1` and calls the fix inconclusive there: resolve the file in place and `adopt`. To keep new content at a held file instead, edit the newest write's release by hand and `adopt` it.
- Each refusal names what is missing, the record that holds it, and the remedy: `knives release include <branch>` for a member of the previous cut, else fetch and `jj bookmark set <release> -r <commit> --allow-backwards`. Where a remote has a branch or tag at a write or adopt that a fix needs, the refusal names that fetch (`fetch <remote>'s <ref>, which holds <commit>`): do it first. When no fetch brings the newest write, `knives release adopt <release>` records the commit you hold.
- `--allow-drop` states an intended drop; it is not a remedy for a refusal. It is the last resort: it also waives the checks of each recorded member, of the newest recorded write, of each other fix by hand and of the commits only the previous release reaches. Its event names each record and what was dropped. Keep the reason and its dependency effects.
- Under the fixed scheme, when the last cut event is missing or names another commit than the published branch, `cut` records the published commit instead of making one.

## `knives release rebase [REF]`

- Other pushed or published release names, and every tag, stay on their commits.
- A merge commit missing from local trunk refuses until `knives sync`.

## `knives release members [REF]`

- Bare: the direct parents of the release in hand, their commits and holding bookmarks, and branch tips advanced past them. Name `REF` for an older release.
- `--verify` replays each member: exit `1` for missing or unexplained content, `3` for problems.
- `--carries REV` and `--census`: SKILL.md, "Is this content carried?".
