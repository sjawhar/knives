# Read-only reports

Read this when running `repos`, `consumers`, `pushed`, `audit` or `pr`, or when
consuming their machine output. These commands report facts, not remedies. The
`knives ledger sweep` and `knives ledger migrate` reports are in the
[ledger reference](ledger.md).

## `knives repos`

Lists every managed entry: where its checkout was found on this machine, its
newest release and, for recorded consumer slugs, whether consumer trunks are
pinned behind the newest cut. An unplaced entry reads `not on this machine`, has
`path: null` and no release state. An ambiguous entry reads `ambiguous: 2
checkouts` with a problem naming both paths.

An unreadable candidate checkout appears as a `?` problem while an entry remains
unplaced, because it could be that entry's checkout. Once every entry is placed,
that candidate problem is dropped. An ambiguous entry or `?` line makes the
command incomplete, exit `3`. For scan boundaries and sweep behavior, see
[configuration](configuration.md).

Registered consumers are read by forge slug at their repository trunk, cached by
commit. Forge failure leaves cache-backed pins explicitly labeled and the result
incomplete. `repos` reads registered slugs only; ad-hoc `--consumer PATH` scans
belong to `consumers` and `release` and never persist the path. Under a fixed
release scheme, a branch-name pin without a locked commit is current by definition;
a locked commit is behind when it is an ancestor of the branch tip.

TOON and JSON share this document shape (`?` marks an optional field):

```text
{repos: [{name, upstream_name, path, release_remote?, newest_release?, behind?,
          notes?, problems?}], notes?, problems?, config_path}
```

`name` is the registry key, what you type. `upstream_name` is what the fork's
ledger, claims and state are kept under: its upstream's lowercase
`<owner>/<name>`, or the registry key when the upstream is a filesystem path.
A claim's `repo` in `state.json` is an `upstream_name`, so to find the checkout
for a claim, match the claim's `repo` against `upstream_name`, never `name`.
Every row carries it, placed or not.

## `knives consumers [FORK] [--consumer PATH]...`

Checks every registered forge consumer plus repeatable ad-hoc local scans against
the newest release on the **live publish remote**. It compares the local release
view with that remote and reports differences without editing consumers.

- Unavailable forge, including cache-backed results: incomplete.
- Missing local path: an unanswered problem.
- Reachable consumer that does not pin this fork: a note.
- Stale frozen locks, older/unknown release names and disagreement among
  consumers: reported pin state.
- A pin outside the release scheme, such as a consumer's own tag or branch:
  `off-scheme`, a fact, not “does not pin” and not a finding.

## `knives pushed [BRANCH]... [--repo REPO]`

Compares local bookmark tips with live owning refs, not stale tracking refs.
Release names use the publish remote; ordinary branches and PR heads use only
origin. A matching feature name on release or a release name on origin does not
substitute for its owning remote. Without a separate publish remote, the release
role reads origin.

Named refs are checked even without local bookmarks, exposing remote-only
branches after a silent local deletion. With no names, the local bookmarks form
the requested set. The report is `{repo, rows, problems?}`; each row is
`{branch, local?, verdicts}`. Verdicts are:

| Verdict | Observed comparison |
|---|---|
| `in-sync` | Local and live owning tip match. |
| `not-on-remote` | Local exists; its owning remote has no ref. |
| `differs` | Both exist at different commits; includes `remote_commit`. |
| `remote-only` | No local bookmark; owning remote has it. |
| `gone-everywhere` | Neither local nor owning remote has it. |
| `pull-head-differs` | A tracked origin pull head differs from the local tip. |

Missing, different, remote-only and differing tracked pull heads are findings.
`gone-everywhere` is not a finding. An unreadable live remote is incomplete,
not observed absence. Nothing repairs refs or pushes.

## `knives audit [REPO] [--all] [--no-github]`

Read-only reconciliation of remote drift, open pull heads, zombie remote
branches, release-cut evidence and anonymous heads. `--all` applies it to every
locatable managed repository; see [scan coverage](configuration.md).

Each local bookmark that is neither trunk nor a release name and has one target
gets a `branches` fact row. A divergent branch gets no row, but a problem:
`bookmark <name> is divergent (<n> targets); no row`. A divergent release-name
bookmark gets `release <name> is divergent (<n> targets)`; recorded-cut drift is
unread until resolved. Either is exit `3`. A divergent trunk belongs to status's
`divergence` finding, not audit's.

The report includes the upstream trunk's PR template once:
`template: {file, headings}`. It is `null` when there is no template or no forge
was asked. A branch row has this shape:

```jsonc
{
  "branch": "feat/x",
  "tip": "<full local commit id>",
  "origin_tip": "<full live commit id>", // or null
  "tip_matches_origin": true,         // false or null
  "fork_only": false,
  "pull": {                          // optional
    "number": 1426,
    "url": "https://forge.example/org/libcore/pull/1426",
    "head": "<headRefOid>",
    "head_matches_tip": true,
    "mergeable": "MERGEABLE",         // CONFLICTING, UNKNOWN or null
    "merge_state_status": "CLEAN",    // BEHIND, DIRTY, BLOCKED,
                                      // UNSTABLE, UNKNOWN or null
    "review_decision": "APPROVED",    // CHANGES_REQUESTED,
                                      // REVIEW_REQUIRED or null
    "checks": {                      // or null
      "total": 13,
      "pending": 0,
      "conclusions": {"SUCCESS": 11, "ACTION_REQUIRED": 2}
    },
    "unresolved_review_threads": 2,   // or null
    "template_missing": ["Approach"] // or null
  },
  "forbidden": [                     // optional
    {"file": "infra/app.py", "line": 9,
     "term": "acme-corp", "text": "<the added line>"}
  ]
}
```

### Interpret missing, null and observed values field by field

- `tip` is the local bookmark's commit. `origin_tip` is the live origin tip;
  it and `tip_matches_origin` are `null` when origin has no branch ref. That is
  observed ref absence, with no tip comparison possible, not a failed forge read.
- `fork_only` is the explicit `track --fork-only` statement.
- `pull` is absent when no **open** PR was answered for the branch. Absence does
  not by itself prove no PR exists: the forge might have been skipped, the pull
  might be settled, or a lookup might be unanswered. Read the problems and scope.
- `head` is the PR's head commit; `head_matches_tip` compares it with local tip.
- `mergeable` and `merge_state_status` are the forge's words. `null` means it has
  not computed them; `UNKNOWN` is not an affirmative mergeability observation.
- `review_decision: null` means the forge reports no decision, not approval.
- `checks.total` counts reported head checks, `pending` those with no conclusion,
  and `conclusions` groups upper-case forge conclusions. `ACTION_REQUIRED` means
  a workflow is held for maintainer approval; that workflow ran nothing.
  `total: 0` means no check runs, not passing checks. `checks: null` means the
  forge answered a pull without its checks field; the GitHub forge does not do so.
- `unresolved_review_threads: 0` is an observed zero. `null` means the forge did
  not answer **or its thread list exceeded one page**; GitHub reads the first
  100 and leaves the count unanswered if another page exists. This alone does
  not fail the batch, so exit `0` does not turn that unknown into zero.
- `template_missing` lists template headings absent from the PR body, matched
  case-insensitively. HTML comments and fenced blocks contribute no headings.
  An empty list means none missing; `null` means no trunk template or unanswered
  body, not a checked complete body.
- `forbidden` lists added diff lines containing configured forbidden terms as
  case-insensitive substrings. Empty means no hits in a performed scan. Absent
  means no terms configured, fork-only exemption, or an unreadable diff; the last
  has a problem naming the branch. It is not interchangeable with empty.

The forbidden scan measures only additions from
`fork_point(<trunk>@upstream | <branch>)` to the branch. With no shared history,
the root is the fork point and the whole tree counts as added. An unresolved
upstream trunk produces one problem and omits every row's `forbidden` field.

Rows are observations and never move the exit code. Findings and problems do;
an unreadable diff or template is a problem and exit `3`. `--no-github` leaves
all pulls absent, template null, and adds the problem
`open pull-head reconciliation was skipped (--no-github)`.

Text renders a `branches:` block: branch, short tip, origin `same`/`differs`/
`absent`, optional fork-only, PR state and head comparison, check totals and
conclusions, unresolved threads, missing headings and forbidden hits.
A `-` is a display placeholder, not an observed zero. Use JSON/TOON for fields.

## `knives pr NUMBER [--repo REPO] [--timeline]`

Reads one PR's current state. `--timeline` adds a separate, on-demand bounded
forge event-log read: force pushes with before/after commit and tree ids,
deletion/restoration, closure/reopening and merge events. This is history for
that PR, not an audit repair path.
