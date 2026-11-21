# Status fields

Read this when you read `knives status [REPO|--all]` rows, states or columns. A state is the strongest observed condition, not advice. What each finding kind means: docs/design.md, "Detection rules".

- One named repository gives an object; `--all` gives an array. A claim on a deleted branch still has a row.
- `--no-landed` skips the trunk replay probe, the slow part; a required member branch not shown landed or merged then stays an unmet dependency. Under `--no-github`, an unknown PR state does not mean no PR.

## State precedence

The first match wins:

1. `fork-only`: stated to have no upstream PR.
2. `divergent`: the bookmark has no single tip.
3. `landed`: the trunk probe found its content in trunk.
4. `conflicted`: the forge says the open PR conflicts.
5. `checks-failing`: open PR checks failed or wait for maintainer approval.
6. `changes-requested`: the open PR's decision.
7. `draft`: the open PR is a draft.
8. `approved`: the open PR's decision.
9. `awaiting-review`: an open PR with none of the above.
10. `merged`: the PR merged, but the trunk probe did not say `in-trunk`.
11. `closed`: the PR is closed.
12. `no-pr`: the forge answered, and no PR is associated.
13. `unknown`: the forge was not asked or did not answer in full, and no PR is stated.

So approval never hides held checks, and the state does not show every observation.

## Columns

A `-` is a missing value, not zero.

| Column | Values |
|---|---|
| push | `pushed` (JSON: absent), `unpushed`, `unpushed-commits`, `origin=<id> (behind\|diverged\|unresolved)` |
| pr | `#<n>`, a non-open state, `draft`, `(stated)`, `(activity <age>)`, `prior #<n> <state>` |
| review | the forge decision; a comment-only review gives `no-review`, and the activity age shows something was said |
| checks | `ok`, `failing`, `action-required` (held for maintainer approval, ran nothing, even beside a green check), `pending`, `none-ran` |
| landed | `in-trunk`, `conflicts-with-trunk`, `not-in-trunk`, `landed?` (not judged) |
| claim | owner id and kind (`harness-session`, `workspace-derived`, `os-user`) |
| seen | age of the newest sighting, `none-since-claim`, `none-within-window` |
| notch | newest human note, else newest event: token, age, anchor, `+N` hidden siblings |

- `landed` replays the branch onto `<trunk>@upstream` and never reads a release, so a member that a release carries can read `conflicts-with-trunk`. Ask `knives release members --carries <branch>` for that.
- A merged PR whose landing commit is in trunk reads `in-trunk` from forge evidence while the branch holds nothing past it.
- `activity_at` is the newest review or comment time. A notch's `anchor` is the subject tip when it was written.

## Sightings

`last_seen` is the newest sighting: a working-copy move of the branch's workspace in the jj operation log, or a record in `seen.json`. Sightings do not prove liveness: a read-only command on a clean tree writes no operation. `none-within-window` means the window ran out, not "never" or "stopped".
