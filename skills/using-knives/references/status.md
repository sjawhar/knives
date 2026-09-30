# Status fields and observations

Read this for `knives status [REPO|--all]`, especially before consuming its branch
state, checks, claim sightings or JSON. A state is the strongest observed
condition, not an action recommendation.

## Report layout and options

The header names repository, trunk, newest release and forge consultation.
`UNANSWERED`, when present, is the first content section. Branch rows, grouped
findings, repo notches, unmatched workspaces and notes follow. Claims appear in
branch rows: even a claim for a deleted branch gets a synthesized row.

TOON and JSON serialize the same structure, in this order (`?` means omitted
when absent):

```text
repo, trunk, newest_release?, forge: {consulted, elapsed_ms}, problems?,
branches, findings?, releases?, repo_notches?, other_workspaces?, notes?

branch: {name, state, tip?, push?, origin_tip?, pr?, review?, checks?,
         landed?, flags?, claim?, last_seen?, seen?, workspace?, notch?}
pr:     {number, state, draft?, stated?, activity_at?, prior?}
claim:  {id, kind, since, why}
notch:  {ts, kind, text, disposition?, anchor?, count}
```

`activity_at` is the newest review or comment time. A notch's `anchor` is the
subject tip when the entry was written. It tells an old note from a statement
about the current tip.

Under `--all`, machine output is one array, not separate JSON documents. A
locatable entry that cannot be gathered gets a `problems` report; ambiguous
checkouts also remain as problem reports. A definitely absent checkout is omitted
with a stderr notice. If scan errors could hide that checkout, it remains as a
problem report instead. See [configuration](configuration.md) for the full scan
contract. Naming one repository yields an object rather than an array.

- `--verbose`: one `kind subject: detail` line per finding rather than one per kind.
- `--no-landed`: skip the trunk replay probe, the slow part.
- `--no-github`: skip PR lookups; do not interpret unknown PR state as no PR.
- `KNIVES_TIMING` set to any value: print phase timing to stderr without changing
  stdout/JSON. For a single invocation, use `env KNIVES_TIMING=1 knives status`.
  Phases are `repository-open`, `health`, `divergent-changes`, `releases`, `setup`,
  `forge`, `probes`, `origin-relations`, `divergent-rows`, `carried-findings`,
  `touching`, `claims`, `report`, `total`. Total is wall time; phases overlap.

## Branch state precedence

First matching observed condition wins:

1. `fork-only`: explicitly stated to have no upstream PR.
2. `divergent`: bookmark has no single tip.
3. `landed`: the trunk probe observed its content in trunk.
4. `conflicted`: an open PR is conflicting according to the forge.
5. `checks-failing`: an open PR's checks failed or are held for action. The
   checks cell distinguishes `failing` from `action-required`.
6. `changes-requested`: open PR review decision is `CHANGES_REQUESTED`.
7. `approved`: open PR review decision is `APPROVED`.
8. `draft`: open PR is marked draft.
9. `awaiting-review`: open PR has none of those preceding conditions.
10. `merged`: associated PR is merged but the trunk probe did not say `in-trunk`.
11. `closed`: associated PR is closed.
12. `no-pr`: forge answered and no PR is associated with the branch.
13. `unknown`: forge was not consulted and no PR was stated.

Thus approval does not override action-required checks; fork-only precedes a
divergent bookmark. The state alone does not expose every other observation.

## The eleven text columns

Missing display values are `-`. `push` defaults to `pushed`; a divergent row
without a tip displays `divergent`.

1. **branch:** local bookmark name.
2. **state:** the precedence label above.
3. **tip:** short commit hash, `divergent` or `-`.
4. **push:** `pushed`, `unpushed`, `unpushed-commits`, or
   `origin=<id> (behind|diverged|unresolved)`.
5. **pr:** `#<n>` plus non-open state, draft, `(stated)`, `(activity <age>)` for
   dated open-PR review/comment activity, and any `prior #<n> <state>` cells.
6. **review:** the forge's open-PR review decision. A comment-only review leaves
   `no-review`; the PR activity age shows that something was said.
7. **checks:** `ok`, `failing`, `action-required`, `pending` or `none-ran`.
   `failing` means a check ran and failed. `action-required` is a workflow held
   for maintainer approval: it has run nothing, even if another unconditional
   check is green.
8. **landed:** `in-trunk`, `conflicts-with-trunk`, `not-in-trunk` or `landed?`.
   A merged PR whose landing commit is in upstream trunk reads `in-trunk` from
   forge evidence when the local branch carries nothing past what merged,
   regardless of replay. A squash can conflict with its own squash; divergent
   bookmarks are never replayed. A branch carrying later commits retains its
   replay verdict, with a note explaining why.
9. **claim:** shortened owner id and kind, such as `ubuntu/os-user`.
10. **seen:** latest observation age, `none-since-claim`, `none-within-window`,
    or `-`.
11. **notch:** newest human note, otherwise newest event, as a short token with
    age, anchor tip (for example `(3d @1a2b3c4d5e6f)`) and `+N` masked siblings.

## Claim sightings are bounded observations

A claimed row's `last_seen` is the newest RFC 3339 observation timestamp. `seen`
carries an unsighted result, `none-since-claim` or `none-within-window`.
The observation is the newest of:

- A working-copy move for the branch's workspace in the jj operation walk.
- The owner-and-kind record in `seen.json`.
- The repository workspace record in `seen.json`.

These do not guarantee liveness. A read-only command on a clean tree writes no
operation; a mutation moving no working copy is not attributable to a workspace.
Both the operation walk and pruned observation file have bounded coverage.
An exhausted window means `none-within-window`, not “never” or “owner stopped”.

## Findings

`findings` groups `{kind, items}`; each item is `{subject, detail}` in detector
order. Text prints one `kind count subjects` line per group, naming the first
eight subjects then `and N more`. `--verbose` prints every subject and detail.

- **unconfigured-remote:** a tracking ref names a remote absent from configured
  remotes, so fetch will never update it.
- **stacked-history:** an open-PR branch carries merge commits past trunk joining
  lines no known trunk position reaches. Usually this is a release cut, so the
  PR carries all that merge's parents. Trunk is checked at `<trunk>@upstream`,
  `<trunk>@origin` and local `<trunk>`. If every local view is behind the branch's
  base, the detail says these may be upstream's own merges; `sync` fetches them.
- **orphaned-claim:** no bookmark on any remote and no workspace still names a
  claimed branch. Deleting a bookmark does not release its claim; `finish` does.
- **immutable-heads-rule:** repository jj config states a different
  `immutable_heads()` from its registry entry's rule. The subject is the config
  file; detail names both rules and distinguishes a prior knives write (the
  next `start` refreshes it) from a human rule (nothing overwrites it).
  The intended rule names trunk on every knives remote and upstream-fetched tags.

For ledger contents and verification, read [ledger](ledger.md). For safe
ownership changes rather than observations, read [coordination](coordination.md).
