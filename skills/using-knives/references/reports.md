# Read-only reports

Read this when you run or read `knives repos`, `consumers`, `pushed`, `audit` or `pr`. They report facts, never remedies, and never change refs or push.

## `knives repos`

```text
{repos: [{name, upstream_name, ledger?, path, release_remote?, newest_release?,
          behind?, notes?, problems?}], notes?, problems?, config_path}
```

- `name` is the registry key you type. `upstream_name` is the lowercase upstream `<owner>/<name>` (the key when upstream is a path), under which the ledger, claims and state are kept. A claim's `repo` in `state.json` is an `upstream_name`: match it against `upstream_name`, never `name`.
- `not on this machine` (`path: null`) is a scan miss, not proof of absence. `ambiguous: 2 checkouts` and `upstream disagrees with repos.toml` are problems (exit `3`).
- `ledger` is the shared ledger repository, absent when the ledger is not shared.
- `repos` reads no ledger or state file, so it works before `knives ledger migrate`.
- Under a fixed release scheme, a pin by branch name with no locked commit is current.

## `knives consumers [FORK] [--consumer PATH]...`

Compares each consumer's pin with the newest release on the live publish remote. A forge failure is incomplete, also with cached results. A pin outside the release scheme (its own tag or branch) reads `off-scheme`: a fact, not "does not pin", and not a finding.

## `knives pushed [BRANCH]... [--repo REPO]`

Compares local tips with the live owning ref, not tracking refs: release names on the publish remote, branches and PR heads on origin only. A name on the other remote does not count. A named branch is checked even with no local bookmark.

| Verdict | Meaning | Finding |
|---|---|---|
| `in-sync` | Local and owning tip match. | no |
| `not-on-remote` | Local only. | yes |
| `differs` | Different commits; see `remote_commit`. | yes |
| `remote-only` | No local bookmark; the owning remote has it. | yes |
| `gone-everywhere` | Neither has it. | no |
| `pull-head-differs` | The tracked PR head on origin differs from the local tip. | yes |

An unreadable remote is incomplete (exit `3`), not absence.

## `knives audit [REPO] [--all] [--no-github]`

One `branches` row per local bookmark with one target that is not trunk or a release name. Rows never move the exit; findings and problems do. A divergent bookmark gets the problem `bookmark <name> is divergent (<n> targets); no row` instead (exit `3`).

```jsonc
{"branch": "feat/x", "tip": "<full local commit id>",
 "origin_tip": "<full live commit id>", "tip_matches_origin": true, "fork_only": false,
 "pull": {"number": 1426, "url": "…", "head": "<headRefOid>", "head_matches_tip": true,
          "mergeable": "MERGEABLE", "merge_state_status": "CLEAN", "review_decision": "APPROVED",
          "checks": {"total": 13, "pending": 0, "conclusions": {"SUCCESS": 11, "ACTION_REQUIRED": 2}},
          "unresolved_review_threads": 2, "template_missing": ["Approach"]},
 "forbidden": [{"file": "infra/app.py", "line": 9, "term": "acme-corp", "text": "<the added line>"}]}
```

| Field | Read it as |
|---|---|
| `origin_tip`, `tip_matches_origin` `null` | Origin has no such branch: observed absence, not a failed read. |
| `pull` absent | No open PR answered. The forge was skipped, the PR settled, or a lookup got no answer: read `problems`. |
| `mergeable`, `merge_state_status` `null` or `UNKNOWN` | Not computed by the forge. No mergeability observation. |
| `review_decision: null` | No decision. Not approval. |
| `checks` | `pending` counts checks with no conclusion. `ACTION_REQUIRED` waits for maintainer approval and ran nothing. `total: 0` means no checks ran, not passing. |
| `unresolved_review_threads` | `0` is an observed zero. `null` is no answer, or more than one page (100 threads); exit `0` does not make it zero. |
| `template_missing` | Headings of upstream's PR template absent from the body. `[]`: none missing. `null`: no template, or no body answer. |
| `forbidden` | `[]`: scanned, no hits. Absent: no terms, fork-only, or an unreadable diff (with a problem). Never read absent as empty. |

- `template: {file, headings}` appears once per report; `null` with no template or no forge.
- `--no-github` gives no `pull`, `template: null`, and the problem `open pull-head reconciliation was skipped (--no-github)`.
- In text, a `-` is a placeholder, not zero. Read the fields from JSON or TOON.

### Release drift

- `release-drift` holds each local and published release name against the commits its cut and edit events record. A release at its newest recorded write is not drift. A copy with the same change id, parents and tree (signed or metaedited) is a note. The publish remote at an earlier recorded write is a note: a newer write is not pushed yet.
- Adopt such a signed or metaedited copy on the machine that made it, while the recorded write is unpublished. Another machine does not hold the recorded commit, so until then its audit reports `could not compare` and `release-drift` for the release. A copy of a published write (the publish remote holds it, a tag is on it, or a `record: published` note names it) stays `nothing to adopt`.
- A local name at an earlier recorded write is drift, also when another machine's newer write is not fetched here. Fetch; do not adopt for that, because `adopt` records the commit you hold as the newest write.
- A hand-finished release, and the fast-forward rebase before a push, are drift until `knives release adopt <name>`.
- After the published remedy, one `release-drift` line stays on the old published name when its published commit is itself an all-carrier hand edit: a resolution made on top of the release and pushed under its name. It is true and needs no action.
- A note names each release whose newest recorded write ends `; unchecked: <reason>`, until `adopt` records it.

## `knives pr NUMBER [--repo REPO] [--timeline]`

One PR's live state. `--timeline` adds the head ref's event log: force pushes with commit and tree ids, delete, restore, close, reopen, merge. `head_ref_deleted` absent means no answer (for example a head repository that refuses the token), not `false`.
