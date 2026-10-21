# Read-only reports

For `repos`, `consumers`, `pushed`, `audit`, `pr` and their machine output. They report facts, not remedies. `ledger sweep` and `ledger migrate` are in [ledger](ledger.md).

## `knives repos`

Lists every entry: where its checkout is on this machine, its newest release, and whether recorded consumer trunks pin behind the newest cut.

| Row | Shape | Exit |
|---|---|---|
| unplaced | `not on this machine`, `path: null`, no release state | — |
| ambiguous | `ambiguous: 2 checkouts`, with a problem that names both paths | `3` |
| upstream disagrees | `upstream disagrees with repos.toml`, `path: null`, with a problem that names the checkout, both spellings and the remedy | `3` |
| unreadable candidate | a `?` problem while an entry is unplaced, dropped when all are placed | `3` |

- Consumers are read by forge slug at their trunk, cached by commit. A forge failure labels cache-backed pins, and the result is incomplete.
- `repos` reads only registered slugs. `--consumer PATH` belongs to `consumers` and `release`, and knives never saves the path.
- Under a fixed release scheme, a branch-name pin with no locked commit is current. A locked commit is behind when it is an ancestor of the branch tip.
- Scan rules: [configuration](configuration.md).

```text
{repos: [{name, upstream_name, ledger?, path, release_remote?, newest_release?,
          behind?, notes?, problems?}], notes?, problems?, config_path}
```

- `name` is the registry key that you type.
- `upstream_name` is where the fork's ledger, claims and state are kept: the lowercase upstream `<owner>/<name>`, or the key when upstream is a path. Every row has it.
- A claim `repo` in `state.json` is an `upstream_name`. To find the checkout of a claim, match it against `upstream_name`, never `name`.
- `ledger` is the shared ledger repository, lowercase, with no `.git`. It is absent when the ledger is not shared.
- `repos` reads only the registry and checkouts, never the ledger or state file. Thus it works before `knives ledger migrate`.

## `knives consumers [FORK] [--consumer PATH]...`

Checks registered forge consumers and ad-hoc local scans against the newest release on the **live publish remote**. It reports where the local view differs, and never edits a consumer.

- Forge unavailable, also with cache-backed results: incomplete.
- Missing local path: an unanswered problem.
- A reachable consumer that does not pin this fork: a note.
- Frozen locks, older or unknown names, and consumers that disagree: pin state.
- A pin outside the release scheme (its own tag or branch): `off-scheme`. This is a fact. It is not "does not pin", and not a finding.

## `knives pushed [BRANCH]... [--repo REPO]`

Compares local tips with live owning refs, not tracking refs. Release names use the publish remote (origin when there is no separate one). Branches and PR heads use only origin. A name on the other remote does not count.

Named refs are checked even with no local bookmark, which shows remote-only branches after a silent local removal. With no names, all local bookmarks are checked. The report is `{repo, rows, problems?}`, and a row is `{branch, local?, verdicts}`.

| Verdict | Meaning | Finding |
|---|---|---|
| `in-sync` | Local and owning tip match. | no |
| `not-on-remote` | Local exists. The owning remote has no ref. | yes |
| `differs` | Different commits. Includes `remote_commit`. | yes |
| `remote-only` | No local bookmark. The owning remote has it. | yes |
| `gone-everywhere` | Neither has it. | no |
| `pull-head-differs` | A tracked origin pull head differs from the local tip. | yes |

An unreadable remote is incomplete, not absence. Nothing changes refs or pushes.

## `knives audit [REPO] [--all] [--no-github]`

A read-only check of remote drift, open pull heads, zombie remote branches, release-cut evidence and anonymous heads. `--all` checks every locatable repository ([scan](configuration.md)).

- Each local bookmark with one target that is not trunk or a release name has a `branches` row.
- A divergent branch has no row, but the problem `bookmark <name> is divergent (<n> targets); no row`. A divergent release name has `release <name> is divergent (<n> targets)`, and cut drift stays unread until it is resolved. Both exit `3`. A divergent trunk is the `divergence` finding of status.
- `release-drift` holds each local and published release name against the commits that the cut and edit events for it name in the [ledger](ledger.md). A release at the commit of its last cut, `include`, `drop`, `advance` or `rebase` is the work of knives, not drift. A commit with the same change id, parents and tree as that one is the same commit signed or metaedited: a note, not drift. The publish remote at an earlier recorded commit, before you push an edit, is also a note. A local name at an earlier recorded commit is drift, because it moved after knives wrote the newer one. When the release commit or the newest recorded commit cannot be read here, that is a problem, and the drift finding stays. A release name with no such event has a note.
- `template: {file, headings}` appears once: the PR template of upstream trunk. It is `null` with no template or no forge.

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

### Missing, null and observed values

| Field | Read it as |
|---|---|
| `tip` | The local bookmark's commit. |
| `origin_tip`, `tip_matches_origin` `null` | Origin has no branch ref. This is observed absence, not a failed read. |
| `fork_only` | The `track --fork-only` statement. |
| `pull` absent | No **open** PR answered. This does not prove that no PR exists: the forge was skipped, the PR settled, or a lookup got no answer. Read the problems. |
| `head` | The PR's head commit. |
| `head_matches_tip` | The PR head compared with the local tip. |
| `mergeable`, `merge_state_status` `null` | The forge has not computed them. `UNKNOWN` is not a mergeability observation. |
| `review_decision: null` | No decision. Not approval. |
| `checks` | `total` counts head checks, `pending` those without a conclusion, `conclusions` groups forge words. `ACTION_REQUIRED` is held for maintainer approval and ran nothing. `total: 0` means no checks ran, not passing. `null` is a pull answered without checks, which GitHub never does. |
| `unresolved_review_threads` | `0` is an observed zero. `null` means no answer **or more than one page** (GitHub reads 100). It does not fail the batch, so exit `0` does not make it zero. |
| `template_missing` | Template headings absent from the PR body, in any case. HTML comments and fenced blocks add no headings. `[]` means none missing. `null` means no template or no body answer. |
| `forbidden` | `[]` means the scan ran with no hits. Absent means no terms, fork-only, or an unreadable diff (that last one has a problem). Never read absent as empty. |

- The forbidden scan reads only additions from `fork_point(<trunk>@upstream | <branch>)`. With no shared history, the root is the fork point and the whole tree counts. An unresolved upstream trunk gives one problem and drops every `forbidden` field.
- Rows never move the exit. Findings and problems do. An unreadable diff or template is a problem, exit `3`.
- `--no-github` gives no pulls, `template: null`, and the problem `open pull-head reconciliation was skipped (--no-github)`.
- Text has a `branches:` block. A `-` there is a placeholder, not zero. Use JSON or TOON for fields.

## `knives pr NUMBER [--repo REPO] [--timeline]`

The current state of one PR. `--timeline` adds a bounded forge event log: force pushes with commit and tree ids, delete and restore, close and reopen, merge. This is history, not a way to change refs.
