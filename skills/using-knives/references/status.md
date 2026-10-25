# Status fields and observations

For `knives status [REPO|--all]` and its JSON. A state is the strongest observed condition, not advice.

## Layout

Text order: header (repository, trunk, newest release, forge use), then `UNANSWERED` when present, branch rows, findings, repo notches, unmatched workspaces, notes. A claim on a deleted branch still has a row.

Machine output (`?` means omitted when absent):

```text
repo, trunk, newest_release?, forge: {consulted, elapsed_ms}, problems?,
branches, findings?, releases?, repo_notches?, other_workspaces?, notes?

branch: {name, state, tip?, push?, origin_tip?, pr?, review?, checks?,
         landed?, flags?, claim?, last_seen?, seen?, workspace?, notch?}
pr:     {number, state, draft?, stated?, activity_at?, prior?}
claim:  {id, kind, since, why}
notch:  {ts, kind, text, disposition?, anchor?, count}
```

- `activity_at` is the newest review or comment time.
- A notch `anchor` is the subject tip at write time. It tells an old note from a note about the current tip.
- `--all` returns one array. One named repository returns an object.
- An entry that cannot be gathered, or an ambiguous checkout, becomes a `problems` report. A definitely absent checkout is left out with a stderr notice. If scan errors could hide it, it stays as a problem report. Scan rules: [configuration](configuration.md).
- Under `--no-github`, an unknown PR state does not mean no PR.
- `--no-landed` skips the trunk replay probe, which is the slow part.
- Set `KNIVES_TIMING` (`env KNIVES_TIMING=1 knives status`) to print phase times to stderr. Stdout does not change. The phases are `repository-open`, `health`, `divergent-changes`, `releases`, `setup`, `forge`, `probes`, `origin-relations`, `divergent-rows`, `carried-findings`, `touching`, `claims`, `report` and `total`. Phases overlap, and `total` is wall time.

## State precedence

The first match wins:

1. `fork-only`: stated to have no upstream PR.
2. `divergent`: the bookmark has no single tip.
3. `landed`: the trunk probe found its content in trunk.
4. `conflicted`: the forge says the open PR conflicts.
5. `checks-failing`: open PR checks failed or wait for approval. The checks cell says `failing` or `action-required`.
6. `changes-requested`: open PR decision is `CHANGES_REQUESTED`.
7. `approved`: open PR decision is `APPROVED`.
8. `draft`: the open PR is a draft.
9. `awaiting-review`: an open PR with none of the above.
10. `merged`: the PR merged, but the trunk probe did not say `in-trunk`.
11. `closed`: the PR is closed.
12. `no-pr`: the forge answered, and no PR is associated.
13. `unknown`: the forge was not asked, and no PR is stated.

Thus approval does not override held checks. The state does not show every observation.

## Text columns

`-` is a missing value, not zero.

| Column | Values |
|---|---|
| branch | local bookmark |
| state | see above |
| tip | short hash, `divergent` or `-` |
| push | `pushed` (default), `unpushed`, `unpushed-commits`, `origin=<id> (behind\|diverged\|unresolved)` |
| pr | `#<n>`, non-open state, draft, `(stated)`, `(activity <age>)`, `prior #<n> <state>` |
| review | forge decision. A comment-only review gives `no-review`, and the activity age shows that something was said. |
| checks | `ok`, `failing` (ran and failed), `action-required` (held for maintainer approval, ran nothing, even beside a green check), `pending`, `none-ran` |
| landed | `in-trunk`, `conflicts-with-trunk`, `not-in-trunk`, `landed?` |
| claim | short owner id and kind, such as `ubuntu/os-user` |
| seen | age of the latest sighting, `none-since-claim`, `none-within-window`, `-` |
| notch | newest human note, else newest event: token, age, anchor (`(3d @1a2b3c4d5e6f)`), and `+N` hidden siblings |

About `landed`: a merged PR whose landing commit is in upstream trunk reads `in-trunk` from forge evidence, when the branch holds nothing past what merged. A branch with later commits keeps its replay verdict, with a note. A squash can conflict with its own squash. Divergent bookmarks are never replayed.

## Claim sightings

`last_seen` is the newest RFC 3339 sighting from these sources:

- a working-copy move of the branch's workspace in the jj operation log,
- the owner-and-kind record in `seen.json`,
- the workspace record in `seen.json`.

With no sighting, `seen` is `none-since-claim` or `none-within-window`.

Sightings do not prove liveness. A read-only command on a clean tree writes no operation. A change that moves no working copy names no workspace. Both sources have a limited window. `none-within-window` means that the window ran out, not "never" or "stopped".

## Findings

`findings` is `{kind, items: [{subject, detail}]}`, in detector order. Text prints one `kind count subjects` line per kind, with the first eight subjects and then `and N more`. `--verbose` prints one `kind subject: detail` line per finding.

- **unconfigured-remote**: a tracking ref names a remote that is not configured, so fetch never updates it.
- **stacked-history**: an open-PR branch carries merges past trunk that no known trunk position reaches (`<trunk>@upstream`, `<trunk>@origin`, local). Usually this is a release cut, so the PR carries all parents of that merge. If every local trunk view is behind the branch base, the detail says that these can be upstream merges. `sync` fetches them.
- **orphaned-claim**: no bookmark on any remote and no workspace names the claimed branch. Deleting a bookmark does not release a claim. `finish` does.
- **immutable-heads-rule**: the repository jj config states an `immutable_heads()` that is not the registry rule (trunk on every knives remote, plus upstream-fetched tags). The subject is the config file. The detail tells a knives write (the next `start` refreshes it) from a human rule (never overwritten).

Ledger: [ledger](ledger.md). Ownership changes: [coordination](coordination.md).
