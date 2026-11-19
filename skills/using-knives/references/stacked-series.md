# Stacked release series

Read this before you cut, edit, adopt, record, reap or rebase in a series with `stacked = true`. [release-composition.md](release-composition.md) and [releases.md](releases.md) apply, except as below. The full rules: README.md, "Release workflow".

Set `stacked = true` on a series table ([configuration.md](configuration.md)) when each release builds on the previous one and never composes again from the trunk, as a frozen version line does. The default series is never stacked. Below, C is a release, and P is its previous release.

## The model

- C is one merge of P and its new members. P is the first parent and never a member. The first cut of the series has no P, so it is a flat cut.
- knives reads C down its first-parent line. P is the nearest commit there that a cut or edit event of the series records as its write or labels as its previous release, else the nearest one a release ref of the series names. The members are the side parents of each merge from C down to P. So the cut `[P, D, M6, M5]` and the chain `merge(merge(merge(P, D), M6), M5)` read alike.
- A commit with one parent between C and P has no member that explains it. The plan and `members` report it (exit `3`), and every write refuses C.
- A member can be built on P: `include`, `advance` and the plan measure its merges past the trunk positions and P, so it is no `stacked-history` there. A merge past them still refuses.

## What changes

- `cut NAME` starts NAME as one commit on P, with P's tree. It goes ahead only when P is published (a tag or a `record: published` note) or every pin of P is frozen, and says that NAME `starts identical to` P. Otherwise it refuses with `nothing to cut`: `include` into P itself. After the cut, `include` each member.
- `drop` of P refuses, `advance` never moves P, and `rebase` refuses: there is no base to move. Never move a member onto the trunk. To move one onto a newer release, run fork-work's bidirectional gate (§7): it gives the rebase by hand, then `knives release advance <branch>`.
- `members` shows P on a row of its own, `<P> previous release (not a member)`, then the reading's members. The cut audit and `members --verify` replay each member from its fork point with P.
- The composition gate takes P as the base. The newest recorded write of the series, or a commit that stands for it (a successor in jj's operation log, or a commit with its tree, whatever its change id), must be on the first-parent line of the release the cut starts on, or of REV for `cut --at`, that commit included. A commit that only shares the write's change id, such as the edit an `adopt` replaced, does not stand for it. Otherwise a cut is told to fetch and move the release to that write, and a record to build REV on it.
- No orphan gate runs: the cut keeps P, and its reap abandons nothing the cut descends from.
- Every edit, the unrecorded check and `adopt` measure a release's own change against the merge of its P and its members, never against its literal parents. So a chain keeps what each inner merge resolved. `adopt` counts each member's added lines from its fork point with P: a member's deletion of P's lines is no lost line.
- Each cut, edit and adopt event records P first in its `parents`, labelled `<P> previous release (not a member)` with no bookmarks, then the reading's members.
- The reap forgets every ref of a superseded name that the live release descends from, and keeps its commit, also while the live cut has conflicts. It says `reaped <name> (refs forgotten everywhere; commit kept, <live> descends from it)` and exits `0`.
- Only a stacked series records a chain of merges made outside knives, with [`cut NAME --at REV`](hand-cut.md).
