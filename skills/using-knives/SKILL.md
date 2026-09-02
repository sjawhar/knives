---
name: using-knives
description: "Reference manual for the knives CLI, which reports and coordinates state across several forks of upstream repositories worked by several agents. Use when running any knives command, when interpreting what one printed, or when you need the detail behind it: what the upstream, origin and release remotes mean, how a branch is matched to a pull request and how to state one it cannot find, recording that one branch cannot land before another, planning and cutting releases, JSON output, and the OpenCode plugin's options. For the shorter question of what to do before touching a fork at all, use the fork-work skill."
---

# The knives CLI

## What it is for

Several forks of several upstreams, worked concurrently by several agents. Two things go wrong without help: an agent guesses at state that could have been queried, and two agents collide on the same branch. knives answers the first and makes the second visible.

It reports. It does not advise: an earlier version attached a suggested fix to every finding, and the suggestions were wrong often enough to be a liability, telling you to drop a branch that had never landed, to open a pull request that already existed. What a report says is what is true, and what to do about it is yours to decide.

Every command takes its repo from the directory you are standing in. Name one only when you are somewhere else, or want a different one.

`knives hook claude-code` and `knives hook opencode` are harness plumbing, not commands for people to run.

## The commands

### `knives repos`

What is managed, where each checkout is, the newest release each has cut, and, where consumer
slugs are recorded, whether their repository trunks are pinned behind the newest cut. Trusted
entries, which are repositories whose instructions we read but do not maintain, are listed
separately.

Registered consumers are fetched by forge slug: Knives reads supported pin files at the consumer
repository's trunk and caches the result by its commit. When the forge is down, cache-backed pins
are labeled as such and the result is incomplete. `--consumer PATH` is separate: it performs an
ad-hoc local scan and never persists the path. Under a fixed scheme, a branch-name pin with no
locked commit is current by definition, and a locked commit is behind when it is an ancestor of
the branch tip.

Like every report, it follows the machine-output rule: TOON when an agent runs it, `--json` for
JSON exactly. The document is `{repos: [{name, path, release_remote?, newest_release?, behind?,
notes?, problems?}], trusted: [{name, path}], notes?, config_path}`.

### `knives consumers [FORK] [--consumer PATH]...`

Checks every registered forge consumer for a fork, plus any repeatable ad-hoc `--consumer` local
scans, against the newest release on the live publish remote. An unavailable forge is incomplete,
including when cache-backed pins can be reported; a missing local path is an unanswered problem;
a reachable consumer that does not pin the fork is a note. It finds stale frozen locks, pins to
older or unknown release names, and disagreement between consumers without editing a consumer.
A pin at a reference outside the release scheme (a consumer's own tag or branch) is reported as a
fact with an `off-scheme` verdict — never as "does not pin", and never as a finding. The local
checkout's release view is compared with the live remote and reported when they differ.

### `knives pushed [BRANCH]... [--repo REPO]`

Reconciles local bookmark tips with the live refs that own them. Release names are checked only
against the publish remote; ordinary branches and pull-request heads are checked only against
origin. A named ref without a local bookmark is still checked, exposing a remote-only branch
after a silent local delete. It is read-only: findings name equal, missing, different, or
remote-only refs and never repair them.

### `knives audit [REPO] [--all] [--no-github]`

Runs the estate-wide, read-only reconciliation. It checks remote drift, open pull heads (unless
`--no-github`), zombie remote branches, recorded release-cut evidence, and anonymous heads.
`--all` applies those checks to every managed repository. Findings are facts for investigation:
the command never deletes refs, changes local bookmarks, pushes, repairs, or opens a pull request.

### `knives pr NUMBER [--repo REPO] [--timeline]`

Reads one pull request's present state. `--timeline` makes the separate on-demand bounded forge
event-log read, reporting force pushes with before/after commit and tree ids, deletion and
restoration, closure, reopening, and merge events. It is history for the named pull request, not
an audit repair path.

### `knives status [REPO|--all]`

The main status map. Its header names the repository, trunk, newest release, and whether the
forge was consulted. If anything could not be answered, `UNANSWERED` is the first content
section; branch rows, grouped findings, repo notches, unmatched workspaces, and notes follow.
Claims live in their branch rows, so a claim for a deleted branch still has a synthesized row
instead of disappearing into a separate section.

TOON and `--json` serialize the same report, in this order:
`repo`, `trunk`, `newest_release?`, `forge: {consulted, elapsed_ms}`, `problems?`, `branches`,
`findings?`, `releases?`, `repo_notches?`, `other_workspaces?`, and `notes?`. A branch is
`{name, state, tip?, push?, origin_tip?, pr?, review?, checks?, landed?, flags?, claim?,
last_seen?, seen?, workspace?, notch?}`. A `pr` cell is
`{number, state, draft?, stated?, activity_at?, prior?}`, where `activity_at` is when the
newest review or comment landed; `claim` is `{id, kind, since, why}`; and `notch` is
`{ts, kind, text, disposition?, anchor?, count}`, where `anchor` is the subject's tip when the
entry was written. Question-marked fields are omitted when absent. Under `--all` the machine
output is one array of these reports, one document; naming a repository gives that report as
one object.

`--verbose` prints one line per finding with its detail (`kind  subject: detail`) instead of
one line per kind.
`--no-landed` skips the trunk probe, which is the slow part. `--no-github` skips pull request
lookups. Set `KNIVES_TIMING` (any value) to print a phase-timing line with
`repository-open`, `health`, `divergent-changes`, `releases`, `setup`, `forge`, `probes`,
`origin-relations`, `divergent-rows`, `carried-findings`, `touching`, `claims`, `report`, and
`total` to stderr; `total` is wall time because phases overlap, and the report's stdout/JSON
contract is unchanged.

#### Branch state

`state` is one reported label per row, chosen in the following precedence order. It describes
the strongest observed condition; it does not recommend an action.

1. `fork-only`: the branch is stated to have no upstream pull request.
2. `divergent`: the bookmark has no single tip.
3. `landed`: the trunk probe observed its content in trunk.
4. `conflicted`: an open pull request is conflicting according to the forge.
5. `checks-failing`: an open pull request's checks are red — a check failed, or one is held for action (a fork pull request's workflows awaiting a maintainer's approval, which run nothing until then). The `checks` cell says which: `failing` or `action-required`.
6. `changes-requested`: an open pull request's review decision is `CHANGES_REQUESTED`.
7. `approved`: an open pull request's review decision is `APPROVED`.
8. `draft`: an open pull request is marked draft.
9. `awaiting-review`: an open pull request has none of the preceding observed states.
10. `merged`: the associated pull request is merged but the trunk probe did not report
    `in-trunk`.
11. `closed`: the associated pull request is closed.
12. `no-pr`: the forge answered and no pull request is associated with the branch.
13. `unknown`: the forge was not consulted and no pull request was stated.

#### Branch table columns

Text rows are rendered as an aligned table with 11 columns. Missing display values are `-`;
`push` defaults to `pushed`, and a row whose divergent bookmark has no `tip` displays
`divergent`.

1. `branch`: local bookmark name.
2. `state`: the reported state label above.
3. `tip`: short commit hash, `divergent`, or `-`.
4. `push`: `pushed`, `unpushed`, `unpushed-commits`, or
   `origin=<id> (behind|diverged|unresolved)`.
5. `pr`: `#<n>` with its non-open state, `draft`, `(stated)`, `(activity <age>)` for an open
   pull request whose newest review or comment is dated, and any `prior #<n> <state>` cells
   appended as applicable.
6. `review`: the forge's review decision for an open pull request. A comment-only review
   leaves it `no-review`; the `pr` cell's activity age is how you see that something was said.
7. `checks`: what the forge's checks say about an open pull request: `ok`, `failing` (a check
   ran and failed), `action-required` (a workflow the forge is holding for a maintainer's
   approval, so nothing of it ran — the usual state of a fork pull request whose only green
   check is the one that runs unconditionally), `pending`, or `none-ran`.
8. `landed`: the trunk verdict (`in-trunk`, `conflicts-with-trunk`, `not-in-trunk`, or
   `landed?`). A merged pull request whose landing commit the upstream trunk contains reads
   `in-trunk` from the forge's evidence when the local branch holds nothing past what merged,
   whatever the replay said — a squash always conflicts with its own squash, and a divergent
   bookmark is never replayed at all. A branch carrying commits past the merged head keeps
   its replay verdict, with a note saying so.
9. `claim`: the claimed owner's shortened id and kind, such as `ubuntu/os-user`.
10. `seen`: an age for the latest observation, `none-since-claim`,
    `none-within-window`, or `-`.
11. `notch`: the newest human note, otherwise newest event, collapsed to a short token with its
    age, the tip it was written against (`(3d @1a2b3c4d5e6f)`), and a `+N` count for masked
    sibling entries. The anchor is how you tell a note that still describes this branch from
    one that described an earlier tip.

#### Claim observations

For a claimed row, `last_seen` is the RFC 3339 timestamp of the newest observation, while
`seen` carries either unsighted result (`none-since-claim` or `none-within-window`). The
observation takes the newest of three streams: a working-copy move for the branch's workspace
from the jj operation walk, the owner-and-kind record in `seen.json`, and the repository
workspace record in `seen.json`.

These are descriptive observations, never a liveness guarantee. Read-only commands on a clean
tree write no operation; mutations that move no working copy are not workspace-attributable;
and the operation walk and pruned observation file have bounded coverage. An exhausted window
therefore reports `none-within-window`, not “never”.

#### Findings

`findings` is a sequence of `{kind, items}` groups; each item is `{subject, detail}`, every
finding of that kind in detector order with its one-line fact. The text report prints one
`kind  count  subjects` line per group, naming the first eight subjects and adding `and N more`
when needed; `--verbose` prints each subject with its detail.
`unconfigured-remote` reports a remote-tracking ref whose remote is not configured; its
commits stay pinned immutable and a fetch will never update it. `stacked-history` reports a
branch with an open pull request whose history past the trunk carries merge commits joining
lines no known trunk position (`main@upstream`, `main@origin`, local `main`) reaches — a
release cut, usually — so the pull request asks its reviewer to take everything those merges
carried; the detail names the releases, or says the merges may be upstream's own when every
local trunk view is behind the branch's base (`knives sync` fetches). `orphaned-claim` reports a claim on a branch that no
bookmark on any remote and no workspace still names: `finish` is what releases a claim, and a
bookmark deleted around it leaves the claim behind.

### `knives sync [REPO|--all]`

Fetches every remote and every tracked pull request head, then classifies what happened to each tracked pull request **since the last sync**: `new` (first sighting, whatever its forge state — recorded silently, like a comment mark; the forge already holds its history), `unchanged`, `advanced` (open, head moved), `merged` or `closed` (settled since the last sync; a pull request that was already settled last time is `unchanged`), or `reopened` (recorded settled, open now). Each row also carries `forge_state` — `open`, `merged` or `closed`; absent under `--no-github`, where the text view prints `unknown` — so a reader gets the transition and where the pull request stands now without confusing the two. A run without the forge records no state over one a forge-backed run observed. Forge state wins over head movement: a pull request that merged and whose head also moved is `merged`. Only transitions write ledger events. Under `--all` the machine output is one array of per-repository reports.

Running `sync` with no arguments inside a managed repository selects that repository. Outside any managed repository, it asks for a repository name or `--all`.

`sync` also checks for new comment activity on open tracked pull requests. When a pull request has comments newer than the last sync mark, it prints a note: `#<n> has comment activity newer than the last sync`. Agents can grep for this exact string. Activity goes to notes (exit 0, informational). A comment query failure goes to problems (exit 3). `--no-github` skips pull-request and comment lookups while retaining local fetch and head checks.

Checking comments costs one extra forge call per open tracked pull request. The mark lives in state as `comment_marks`, keyed `<repo>#<number>`, and advances forward. The first time a pull request is seen, the mark is recorded silently without printing a note, avoiding noise on first run. Edited comments are invisible because the forge `createdAt` timestamp does not move on edit.

### `knives preflight [REPO]`

The facts you need before contributing upstream: convention files present and whether they have changed since last seen, any stated cap on open pull requests, branch state. It reports; the judgment is yours. The `pr-preflight` skill walks the gate.

### `knives start <branch>` and `knives finish <branch>`

`start` claims the branch and opens a jj workspace for it, based on the release's shared base (or the fetched upstream trunk when no release exists) rather than wherever `@` happens to be. The shared base is where every member forks from, so a branch started there composes into the release without dragging newer upstream into the cut; moving the whole release to a newer trunk is `knives release rebase`, an intentional decision of its own, never a side effect of starting a branch. An agent sitting in a release workspace who runs `jj new` silently inherits the release merge as a parent, which is why `@` is never used.

`finish` hands the claim back and removes the workspace. Run it as soon as your active work on the branch stops — including when the work now waits on something external, such as an open pull request in review. A claim means "an agent is working here right now", not "this branch matters": holding one after you stop blocks every other agent from picking the branch up, and releasing one loses nothing. The branch, its bookmark, and any open pull request all survive the release, and the work itself is safe because jj snapshots a working copy into a commit, reachable by change id. `--no-cleanup` keeps the directory, which matters only for files jj never tracked, such as build output or an untracked `.env`. `--superseded-by <branch>` records where the work went.

### `knives track <branch>`

Which pull request a branch belongs to, stated rather than inferred.

Inference looks for an open pull request in our own copy of the repository whose head branch matches, and understands a `pr-<n>` bookmark as the fetched head of that number. That is a good default and a bad rule. It misses one opened before knives existed, one the maintainer closed because they wanted a different approach, and somebody else's that we carry because ours was superseded.

```
knives track <branch> --pr 4545      # any number, any state, any author
knives track <branch> --fork-only    # deliberately has no upstream pull request
knives track <branch> --forget       # back to inference
```

### `knives depends <branch> --on <repo>#<number>`

That the branch cannot land before that pull request does. Dependencies cross forks, which is the case that motivated it: dropping a required change from a release without dropping the branch that needs it ships a release that cannot work. `status` reports the ones that are not merged yet.

### `knives notch [SUBJECT]`

The record of what happened in this fork and what was decided. Each repository has an
append-only ledger directory beside the state file, with one immutable Markdown entry file
per entry. `knives status` deletes nothing, but `knives finish` does: it removes the claim
that said why a branch exists. The ledger is where that survives.

Two moods on one command. Bare, it reads the newest 20 human notes and folds all machine events
into one newest-event count:

```
knives notch                      # newest human notes, plus a machine-event fold
knives notch <branch>             # that ref's whole chronology, oldest first
knives notch release/2026-08-15   # a release is a subject like any branch
knives notch --pr 4545            # entries stamped with, or written for, that pull request
knives notch --dispositions       # every terminal ruling
knives notch --events             # the full machine-event chronology
knives notch --verify '#4545'     # re-check one selected record
knives notch --repo other         # a repo you are not standing in
```

With `-m`, it writes:

```
knives notch <branch> -m "superseded by #1157; upstream wanted the trait approach" \
  --evidence 06d778b9 --evidence other-repo#1157 --pr 4891
knives notch '#4545' -m "split to a plugin" --disposition ruled-out \
  --evidence https://forge.example/org/libcore/pull/4545
knives notch -m "this fork needs a cut before the pin moves"   # about the repo itself
```

`--repo` works in both moods, and it is the flag for the case that keeps happening: you
are standing in the consumer fork when you learn something about the library fork, and the
entry belongs in the library's ledger. `--pr` filters reads; with `-m`, it stamps the entry
explicitly and otherwise the tracked pull request is the fallback. A `#<n>` subject also stamps
that number without treating it as a branch. `--evidence` is repeatable and requires `-m`.
`--disposition` is a lowercase terminal token and requires both `-m` and evidence.

A `knives start` workspace resolves its registered checkout through `.jj/repo`, so ordinary
commands infer that repository there. Keep `--repo <name>` for a cross-repository write.

#### What an entry holds

| Field | Written by | Content |
|---|---|---|
| `ts` | automatically | when it was written, RFC 3339 UTC |
| `owner` | automatically | the same identity a claim gets |
| `subject` | you | the ref it is about; absent for an entry about the repository |
| `kind` | automatically | `event` when a knives command observed it, `note` when an agent asserted it |
| `disposition` | you, optional | a terminal ruling (`merged-elsewhere`, `withdrawn`, or `ruled-out`) backed by evidence; it remains a note |
| `text` | you, or the command | the entry itself |
| `evidence` | you, optional | commit ids, `file:line`, `<repo>#<number>`, URLs, and they may name other repos |
| `anchor` | automatically | the subject's tip at write time, absent when it did not resolve |
| `pr` | `--pr` on write, otherwise automatically | caller-supplied write stamp, or the pull request `knives track` states for the subject |

Two kinds, not three. The question a reader has is whether a machine observed this or an
agent asserted it. A disposition is a selectable class of note, not a new kind. Supersessions
and parkings arrive as events, through `finish --superseded-by` and `start --why`; everything
you assert by hand is a note.

`anchor` preserves the record's context: a past-tense ruling remains a record of what was
decided, while its current commit context can be checked again. `knives notch --verify` tests
selected commit-shaped evidence and anchors against all visible commits and local bookmark tips.
It flags missing evidence, vanished anchors, and subject anchors that no longer match the current
local tip; it never rewrites the entry. So the ledger holds events and judgments, never derived
state — if a detector can compute it, do not write it down.

#### What writes entries without being asked

Every command that already witnesses something records it as part of doing it. A failed
ledger write fails the command.

| Command | Entry |
|---|---|
| `start`, `claim` | `claimed: <why>` on the branch |
| `finish` | `claim released`, `claim released; superseded by <branch>`, or bare `superseded by <branch>` for an unheld finish with `--superseded-by` |
| `track --pr/--fork-only/--forget` | the statement that changed |
| `depends --on` | `requires <repo>#<number>` |
| `release cut` | the whole parent set (the cut's change id beside its commit, since resolving conflicts before the push rewrites the merge), plus the previous cut's carried-parent delta |
| `release include`, `drop`, `advance`, `rebase` | `edited <release>: <delta>; parents: …` — the parent set after the edit |

A cut or edit event also carries a `parents` field in its frontmatter: one item per parent with
its full commit and every local bookmark at it when the event was written. That record, not the
text, is what lets a later `advance` or `include` still tell which parent is which branch after
a rebase done outside jj, or after the member landed upstream; every name at the commit is kept,
so an anchor bookmark another agent left at a member's tip does not hide the member's own name.
| `sync` | one entry per tracked pull request that merged, closed, reopened or advanced since the last sync; a first sighting is recorded silently |

Nothing is recorded for a pull request that did not move, and nothing injects any of this
into a session: reading the ledger is intentional, and that is the point.

#### In `knives status`

Each branch row carries the newest human note if any, otherwise its newest event. In JSON and
TOON that is `notch: {ts, kind, text, disposition?, count}`, absent when the branch has none;
in text it is one truncated token at the end of the line. A disposition prefixes its text and
`+N` records N masked sibling entries. Repo-level entries appear separately as
`repo_notches: {count, last}` in machine output and as
`notches  <N> repo-level, newest: "<text>" (<age>)` after the findings. It is a local ledger
read, so it costs nothing.

#### Storage and exit codes

Each repository's ledger is `~/.config/knives/ledger/<repo>/`. Every entry is one Markdown
file with TOML frontmatter between `+++` fences and prose as its body. Entry files are
immutable: they are never rewritten or deleted. A write completes a temporary file and
atomically persists it without replacement to its final name; there is no lockfile. Its
filename is a compact UTC timestamp followed by a four-hex-character suffix; readers scan
entry files in lexicographic filename order, which is chronological.

There is no rotation or retention policy — an entry is about 300 bytes. Readers ignore
unknown frontmatter keys, so a newer binary can add one and an older one still reads the
entry. There is no version number and there never needs to be.

`0` is fine, `2` is a usage error, and `3` means the ledger exists but cannot be read — its
directory or an entry file within it. That is deliberately not the same as a repository
nobody has notched yet, which is `0` with `no notches yet`.

### `knives release [REPO]`

With no arguments or subcommand, plans a release: reports what a cut would contain, whether every parent is still at its branch tip, which local branches are not in the release (or have moved past their released parent), whether any member's own history carries a prior release merge, and consumer pin state. Planning is the default; release commands write only locally and never push.

A release is a flat octopus merge of feature and fix branches, and its parent set is the membership: a branch is in the release exactly when the release has its parent. The upstream base is never a direct parent — members fork from it, so it is reachable through every one of them, and there is no base/member role to classify: a member that lands upstream stays a droppable, advanceable member. Membership changes only through stated edits — `include` adds one parent, `drop` removes one (and states when no remaining member carries the dropped content), `advance` moves members to their branch tips — each rebuilt by duplicating the release onto the changed parent set, so recorded conflict resolutions carry forward and only the change itself can surface new conflicts. Publishing remains a manual `jj git push --bookmark <name>` operation.

#### One branch is the member and the pull request

A branch forks from the release's shared base (the trunk point its members share) and is linear past it. The same branch is the release member and the head of the upstream pull request. There is no second copy. In particular:

- **Rebasing a branch onto a newer trunk is sometimes necessary** — a maintainer asks for it, or the branch needs newer upstream. It is the branch's own decision, never a side effect of starting one. The release follows: `knives release advance <branch>` matches a member to its rebased branch by change id as well as by ancestry, so a rebased branch is still recognised as the same member, and `knives release rebase` moves the whole composition onto a newer trunk point when that is what is wanted. Which trunk point a member forks from is not a finding.
- **Never mint a "release-lineage" or "sibling" branch** carrying a pull request branch's content on an older base so the release can carry it. That doubles every conflict, splits every review, and the composition gate then records two members for one change. If a branch cannot compose into the release, the release is behind: `rebase` it.
- **A member's history past the trunk carries no merge.** A branch built on a release merge (or with any release merge in its history) carries every parent of that merge; `include`, `advance` and the first `cut` refuse it with the `stacked-history` detail, the plan says so instead of pointing at `include`, and a member that got in before this check reports `stacked-history` in the plan. The trunk is measured at every known position — `main@upstream`, `main@origin`, local `main` — so a merge one of them reaches is the trunk's, not the branch's; when every local view is behind the branch's base the detail says the merges may be upstream's own and `knives sync` fetches them. Rebase a genuinely stacked branch off the trunk — `jj rebase -b <branch> -d <trunk>` keeps its change ids, so `advance` still follows it.
- **A landed member whose branch kept going is still that member.** Once the trunk reaches a released parent, every fresh branch descends from it, so ancestry cannot say which branch was the member; the cut or edit record can. The plan names the landed parent and offers `advance <branch>` (moves the member to the branch's tip) or `rebase` (retires the landed parent); `include` refuses the second copy for that reason, and a named `advance` says the match rests on the record.
- **A release cut carries exactly what its parents hold.** The plan says `N parent(s), flat` only when no member's history carries a prior release merge; otherwise it counts the stacked members.

#### Scheme variants

- Dated scheme (default, when `release_branch` is absent): cuts create a new dated branch named `release/YYYY-MM-DD` (or `.1`, `.2` for repair cuts). Cutting requires an explicit name argument: `knives release cut release/YYYY-MM-DD`.
- Fixed scheme (when `release_branch = "<name>"` is set): cuts advance the configured release branch in place using jj's internal `--allow-backwards` mechanism. Cutting needs no name argument (`knives release cut` alone); passing a dated name is refused. A cut carries the local composition in hand, unpushed edits included; the *published* position (read from the publish remote, `release` when set in the entry, falling back to `origin`) is what consumers observe and is reported alongside.

#### Release subcommands and options

- `knives release cut [NAME] [--allow-drop]`: audits a candidate cut of the composition in hand — the previous release's parents carried verbatim, nothing joining and nothing advancing — and names it only when the audit passes: each member's net diff, measured from the members' fork point with the upstream trunk, must be present in the cut tree. Divergence the previous release already carried (a recorded conflict resolution) is reported as carried forward, never refused. A failed audit writes nothing at all; a passing one creates and names the release as one operation. Only the first cut, with no composition to carry, starts from every branch. The orphan gate refuses a cut that would strand commits reachable only from the previous lineage; `--allow-drop` overrides it.
- The composition gate: before publishing, the candidate is held against the previous cut's ledger event — the only record of a parent set that survives the release bookmark moving. A recorded member the candidate does not carry (not a parent, not an ancestor of one, and its net diff absent from the candidate tree) refuses the cut and is named, whether it vanished through a hand-rebuilt merge, an out-of-band bookmark move, or a `drop` since the last cut; a member that landed upstream and entered through the base passes without comment. `--allow-drop` states the drop is intended, and the new cut's event records exactly which members were dropped. A recorded commit the repository cannot resolve counts as dropped — unverifiable must not read as carried.
- `knives release reap`: reaps superseded dated release bookmarks by forgetting their refs locally and across tracking remotes, then abandoning their merge commits. Reaping also runs automatically after every successful dated cut and never modifies remote repositories. While the live cut still carries unresolved conflicts, every superseded cut is kept: the previous release is the only record of how those conflicts were last resolved, and an abandon-and-recut needs it.
- `knives release rebase [REF]`: the equivalent of `jj rebase -b <release> -d <REF>`. Bare, it asks the forge which of our pull requests merged (merged, not closed) and targets the first upstream trunk commit that contains every one of their merge commits — the point past which nothing merged is missing from the members' shared history; with nothing merged there is no default, and it asks for a commit. Every member branch's commits move onto the target and the release merge moves with them, bookmarks and workspaces following; recorded conflict resolutions replay as ordinary rebase semantics. After a bare rebase (or a bare run that finds the release already at its target), members whose pull requests landed and whose branches carry nothing past the target are dropped, the reason recorded on the release; `--no-drop` keeps them, and a branch with work past its pull is kept and says so. An unheld stale parent refuses with `Incomplete`, naming the branch that continues it (by ancestry or change id) and `knives release advance` as the way to move the member first, or `drop` when no branch continues it; a legacy trunk parent is shed on the way through, since the base is never a parent. A merged pull request whose merge commit is not in the local trunk view also refuses — `knives sync` first. A composition whose every member has landed refuses to rebase (the trunk would become its only parent) and refuses to drop its last parent: reap it or include new work.
- `knives release include <branch> [--why "..."]`: add a branch (or any revision) to the release in hand as one new parent. Nothing else changes; a member whose branch has moved on — grown past its released parent, rebased off it (by jj or outside it), or landed upstream — is not moved and not added a second time: that is `advance`'s job, and `include` says which case holds instead of improvising.
- `knives release drop <branch> --why "..."`: remove a branch's parent from the release in hand. The branch and its bookmark are untouched. A branch that moved past its released parent still resolves by succession (ancestry, or the parent's change id on the branch); a branch rebuilt outside jj does not, deliberately — a drop is destructive, so name the parent's commit id instead. The reason is recorded on the release commit itself, and is required: dropping shipped content without one is how a release becomes unexplainable later, so omitting it is a usage error.
- `knives release advance [<branch>...] [--from <old-sha>]`: move member parents to their branches' current tips. Named branches move exactly; a bare `advance` moves every member whose branch has moved on. The trunk parent is `rebase`'s domain. A branch succeeds its released parent when the parent is an ancestor of the branch tip (the branch grew) or when the parent's change id is on the branch past the trunk (the branch was rebased — `jj rebase` keeps change ids). A parent the trunk already reaches — a member that landed by merge commit, or a legacy base parent — has no successor: every trunk-forked branch descends from it and none of them is it; `rebase` retires landed members. When neither ancestry nor change id answers for a named branch, the last cut or edit event's `branch@commit` record for this release does, and the output says the match rests on that record alone (a bookmark name reused for unrelated work would be moved onto that member). It refuses rather than guess whenever that is unsafe: a bare advance refuses outright if the *same* branch would succeed more than one parent (a stacked integration branch satisfying the check for several stale parents at once is not evidence it replaced all of them). `--from <old-sha>` names the exact old parent one named branch replaces, bypassing every search — for a release with no record of the branch; requires exactly one named branch.
- All three edits share a rebase's two refusals, both `Incomplete`: when every pin of the release is frozen on a revision, editing it in place would reach nobody, so cut a new dated release instead; and when the upstream trunk cannot be resolved, nothing can separate the release's base parents from its members, so fetch upstream first.
- `knives release --consumer <DIR>`: runs an ad-hoc local scan alongside the registered forge
  slugs in `repos.toml`. Repeatable (`--consumer <DIR1> --consumer <DIR2>`), it widens planning
  and cutting; `include`, `drop`, `advance` and `rebase` read only the recorded forge consumers,
  so add a slug for a repository that should count towards their pin gate.

### `knives register [DIR]`

Prints a paste-ready `[repos.<name>]` TOML entry to stdout, with diagnostic instructions on stderr.

Writes nothing to `repos.toml` directly. The human or caller pastes the stdout snippet into `repos.toml`. Replace any existing `[repos.<name>]` section rather than appending a duplicate entry. Registry edits take effect on the next hook event or tool call without needing a daemon or service restart.

### `knives init [DIR]`

Reads a checkout's remotes and outputs a registry entry or adopts the repository into the registry.

Expects remotes named for their roles: `upstream` (what we contribute to) and `origin` (our fork where branches push and PR heads live), plus an optional `release` remote. Warns if an untracked remote looks like another fork of upstream (detected via case-insensitive owner and slug comparison on the same host), reminding that `origin` must point to your own fork.

## Is this content carried?

For any “is branch/fix X in release/trunk Y?” question, agents MUST use the replay probe, not a source-text search. Text search proved wrong on a real repository: it claimed an approved but unmerged fix was carried; replay proved it absent.

- `knives release carries <revision>` without `--in` evaluates the deletion-safety question across live releases and upstream trunk. `knives release carries --all` censuses maintained branches.
- `knives release carries <revision> --in <target>` asks only whether that exact target carries the revision. Its exit status is direct: `0` for either carried verdict, `1` for `NOT carried` or `conflicted`, and `3` if the target could not be resolved or checked. A target's live/superseded role is irrelevant to this explicit query.
- For the upstream trunk, the `landed` column from `knives status` is the trunk replay probe’s answer.
- Agents MUST NOT grep source text to answer either question.

The carriage vocabulary is exact:

- `carried-exact`: the revision tip is an ancestor of the target; the target contains that exact commit.
- `carried-rewritten`: replaying the revision's net tree change leaves the target unchanged; equivalent content arrived through different commits, or the revision has no net change.
- `NOT carried`: replaying leaves a clean, non-empty diff; the target lacks that content.
- `conflicted`: replaying conflicts; some content may be present or unrelated work touched the same files, so the result requires judgment.

## The three remotes

- `upstream`: what we contribute to. Only ever through a pull request.
- `origin`: our own copy.
- `release`: optional, for where releases are consumed internally rather than from a personal fork. Falls back to `origin` when absent. Not every fork needs one.

## The registry

`~/.config/knives/repos.toml`. Managed fork entries, trusted entries, and trust rules:

```toml
[repos.scout]
path = "~/forks/scout/default"
upstream = "https://forge.invalid/org/scout"
origin = "https://forge.invalid/ours/scout"
base = "main"                         # optional: upstream's trunk (defaults to main; set e.g. "dev" for opencode-style forks)
release_branch = "release"            # optional: fixed release branch scheme (omit for dated release/YYYY-MM-DD)
consumers = ["acme/workbench"]       # optional: forge slugs whose trunks pin this repo's releases

[trusted.workbench]
path = "~/workbench/default"       # instructions read, not maintained

[trust]
roots = ["~/projects/company"]      # subtrees whose repos are all trusted for guidance
owners = ["orgname"]               # forge owners whose repos are trusted for guidance
```

### Registry fields

- `[repos.*]`: managed forks. `upstream` and `origin` are required.
  - `base`: upstream's trunk — the branch we fork from, measure landed state against, and target pull requests at. Defaults to `main`. Configurable because upstreams use different trunk names (for example, opencode-style forks set `base = "dev"`).
  - `release_branch`: configures a fixed release branch scheme (e.g., `"release"` or `"integration"`). Must not be empty, equal to `base`, or sit under the `release/` prefix.
  - `consumers`: forge slugs for repositories that pin this repository's releases. Knives scans
    each slug's trunk through the forge and caches it by commit; use `--consumer PATH` for an
    ad-hoc local scan.

- `[trusted.*]`: unmaintained repositories whose agent instructions are trusted for reading.

- `[trust]`: rules for trusting instructions from unmanaged repositories.
  - `roots`: array of directory paths; any repository inside these subtrees is trusted for guidance.
  - `owners`: array of forge organization or user names.

> **SECURITY:** `owners` matches self-declared remote URLs read from the candidate checkout's own git config — not forge-authenticated; any repo that declares itself a checkout of a trusted owner's repo (by remote URL or a `gitdir:` pointer) is accepted; the probe verifies the directory is the repository's own git toplevel, so nested directories do NOT inherit an enclosing repo's identity; owner rules read GIT remote config, so jj-only (non-colocated) checkouts match only via `roots`; grants guidance-as-data injection only (same grant as a `[trusted]` entry), never fork-command access; prefer `roots` when in doubt.

Edits to `repos.toml` take effect on the next hook event or tool call (reloaded per event) — no restart required.

## Machine output

When the environment indicates an agent is running a command (or stdout is not a terminal), reports are emitted as TOON — the same structure as JSON at fewer tokens, so nothing has to grep prose to count findings. `--json` forces JSON exactly; `--text` forces prose.

## Exit codes

`0` nothing to report, `1` findings, `2` usage, `3` incomplete (meaning something could not be answered).

## The OpenCode plugin

Ships alongside the CLI. Once per repository per session, the first time a call names a file inside a managed repository, it announces that the repository is managed and shared, names any claims, and appends the repository's own `AGENTS.md` as data. It also exports `KNIVES_OWNER` into shell environments.

Configured from its entry in `opencode.json`, all defaulting to on:

```jsonc
"plugin": [
  ["file://{env:HOME}/knives/default/plugin/knives.ts",
   { "notice": true, "guidance": true, "owner": true }]
]
```
