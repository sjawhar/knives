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

What is managed, where each checkout is, the newest release each has cut, and, where consumers are recorded, whether those consumers are pinned behind the newest cut. Trusted entries, which are repositories whose instructions we read but do not maintain, are listed separately.

Pins are read from each consumer's origin trunk rather than its working copy. Notes report when a consumer checkout is behind its origin trunk (`checkout is N commit(s) behind its <branch>`), when no origin trunk resolves (`no origin trunk resolved; pins read from the working copy`), or when a consumer is not a repository (`not a repository; pins read from the working copy`). Under a fixed scheme, a branch-name pin with no locked commit is current by definition, and a locked commit is behind when it is an ancestor of the branch tip.
### `knives status [REPO|--all]`

The main report. Per branch: local tip, push status, pull request and its state, review decision, CI check status, landed verdict against upstream trunk, flags, and newest ledger entry. Plus claims other agents hold, the releases scanned, findings grouped one line per kind, and anything it could not answer.

`--verbose` prints one block per finding instead of one line per kind. `--no-landed` skips the trunk probe, which is the slow part. `--no-github` skips pull request lookups. Set `KNIVES_TIMING` (any value) to print a phase-timing line (`releases`, `forge`, `probes`, and `total`) to stderr; the report's stdout/JSON contract is unchanged.

#### Branch table columns

Branch rows are rendered as an aligned table with 9 columns. Empty cells render as `-` for `review`, `checks`, `landed`, `flags`, and `notch` columns (`branch`, `tip`, and `push` always carry values or state tokens, while `pr` reports a number or state token).

1. `branch`: local bookmark name.
2. `tip`: short commit hash, or `divergent` if the bookmark has multiple tips.
3. `push`: relation between local and origin tips:
   - `pushed` if local matches origin tip.
   - `unpushed` if origin has no remote-tracking ref for this branch.
   - `unpushed-commits` if local is ahead of origin.
   - `origin=<id> (behind)` if origin is ahead of local.
   - `origin=<id> (diverged)` if local and origin have diverged.
   - `origin=<id> (unresolved)` if ancestry could not be determined.
4. `pr`: pull request details:
   - `#<n>` or `#<n> <state>` for closed/merged pull requests.
   - `#<n> draft` for draft pull requests.
   - `#<n> <state> (stated)` for explicitly tracked pull requests.
   - `no-pr` if no pull request is associated.
5. `review`: `APPROVED`, `CHANGES_REQUESTED`, `no-review`, or `-` if no PR exists.
6. `checks`: CI check status for open pull requests:
   - `ok` if checks passed.
   - `failing` if CI checks failed.
   - `none-ran` if no checks ran.
   - `-` if no open PR exists or checks were not consulted. CI still in flight renders the same as all-green (`ok`) because the vocabulary has no `checks-running`. Checks are consulted only for open pull requests.
7. `landed`: verdict against upstream's trunk (`in-trunk`, `conflicts-with-trunk`, `not-in-trunk`, `landed?`, or `-`).
8. `flags`: comma-separated flags (`CONFLICTING`, `behind-base`, `review-stale`, `fork-only`) or `-`.
9. `notch`: the newest ledger entry for this branch as one truncated token with its age (`"superseded by #1157…" (3d)`), or `-` when there is none. `knives notch <branch>` prints it in full.

Trunk verdicts say what was observed, not what it means:

- `in-trunk`: replaying the branch onto the trunk produced nothing, so the trunk has it.
- `conflicts-with-trunk`: replaying it conflicts. This does not mean the maintainer took it and modified it: a branch declined upstream, whose files were later touched by unrelated work, conflicts identically. The tool cannot tell those apart and does not try.
- `not-in-trunk`: it applies cleanly and is not empty, so the trunk lacks it.
- `landed?`: local differs from origin, so replaying would judge content the pull request does not contain. Refusing to answer beats guessing.

An `unanswered` section means the run is incomplete and some of the report is missing. Read it before trusting the rest.

#### Findings

Findings appear grouped one line per kind at the end of the status report:

- `double-checkout`: two or more workspaces hold `@` on the same change.
- `stale-parent`: a release parent's bookmark has moved to a descendant.
- `divergence`: a change ID exists on two or more commits.
- `stale-review`: the newest review predates the newest commit on the branch.
- `claim-overlap`: active claims touch the same file.
- `unmet-dependency`: a required pull request dependency is not merged yet.
- `unmergeable`: the pull request conflicts with its base branch according to the forge.
- `checks-failing`: the forge reported a red CI conclusion (`FAILURE`, `TIMED_OUT`, `CANCELLED`, `STARTUP_FAILURE`, `ACTION_REQUIRED`, or `ERROR`).
- `wrong-base`: the pull request targets a branch whose name differs from the repo's configured base branch. Only open pull requests are checked; an empty base is unknown, not wrong.
- `carried-elsewhere`: the branch tip is reachable from another reference. Trunk, `@git` refs, and our own release cuts are excluded.
- `branch-overlap`: two or more of our branches change the same file, which conflicts when a release merges them. One finding per file, naming every branch.
### `knives sync [REPO|--all]`

Fetches every remote and every tracked pull request head, then classifies each tracked pull request as `new`, `unchanged`, `advanced`, `merged` or `closed`. Forge state wins over head movement: a merged pull request whose head also moved is merged.

Running `sync` with no arguments inside a managed repository selects that repository. Outside any managed repository, it asks for a repository name or `--all`.

`sync` also checks for new comment activity on open tracked pull requests. When a pull request has comments newer than the last sync mark, it prints a note: `#<n> has comment activity newer than the last sync`. Agents can grep for this exact string. Activity goes to notes (exit 0, informational). A comment query failure goes to problems (exit 3). `--no-github` skips pull-request and comment lookups while retaining local fetch and head checks.

Checking comments costs one extra forge call per open tracked pull request. The mark lives in state as `comment_marks`, keyed `<repo>#<number>`, and advances forward. The first time a pull request is seen, the mark is recorded silently without printing a note, avoiding noise on first run. Edited comments are invisible because the forge `createdAt` timestamp does not move on edit.

### `knives preflight [REPO]`

The facts you need before contributing upstream: convention files present and whether they have changed since last seen, any stated cap on open pull requests, branch state. It reports; the judgment is yours. The `pr-preflight` skill walks the gate.

### `knives start <branch>` and `knives finish <branch>`

`start` claims the branch and opens a jj workspace for it, based on the release's shared base (or fetched upstream trunk if no release exists) rather than wherever `@` happens to be. An agent sitting in a release workspace who runs `jj new` silently inherits the release merge as a parent.

`finish` hands the claim back and removes the workspace. No work is lost: jj snapshots a working copy into a commit, so it is in the repository and reachable by change id. `--no-cleanup` keeps the directory, which matters only for files jj never tracked, such as build output or an untracked `.env`. `--superseded-by <branch>` records where the work went.

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

Two moods on one command. Bare, it reads:

```
knives notch                      # the newest 20 entries across this repo
knives notch <branch>             # that ref's whole chronology, oldest first
knives notch release/2026-08-15   # a release is a subject like any branch
knives notch --pr 4545            # only entries stamped with that pull request
knives notch --repo other         # a repo you are not standing in
```

With `-m`, it writes:

```
knives notch <branch> -m "superseded by #1157; upstream wanted the trait approach" \
  --evidence 06d778b9 --evidence other-repo#1157 --pr 4891
knives notch -m "this fork needs a cut before the pin moves"   # about the repo itself
```

`--repo` works in both moods, and it is the flag for the case that keeps happening: you
are standing in the consumer fork when you learn something about the library fork, and the
entry belongs in the library's ledger. `--pr` filters reads; with `-m`, it stamps the entry
explicitly and otherwise the tracked pull request is the fallback. `--evidence` is repeatable
and requires `-m`.

A `knives start` workspace resolves its registered checkout through `.jj/repo`, so ordinary
commands infer that repository there. Keep `--repo <name>` for a cross-repository write.

#### What an entry holds

| Field | Written by | Content |
|---|---|---|
| `ts` | automatically | when it was written, RFC 3339 UTC |
| `owner` | automatically | the same identity a claim gets |
| `subject` | you | the ref it is about; absent for an entry about the repository |
| `kind` | automatically | `event` when a knives command observed it, `note` when an agent asserted it |
| `text` | you, or the command | the entry itself |
| `evidence` | you, optional | commit ids, `file:line`, `<repo>#<number>`, URLs, and they may name other repos |
| `anchor` | automatically | the subject's tip at write time, absent when it did not resolve |
| `pr` | `--pr` on write, otherwise automatically | caller-supplied write stamp, or the pull request `knives track` states for the subject |

Two kinds, not three. The question a reader has is whether a machine observed this or an
agent asserted it. Supersessions and parkings arrive as events, through `finish
--superseded-by` and `start --why`; everything you assert by hand is a note.

`anchor` is why this record does not rot. A stored disposition goes wrong the moment
upstream moves: a branch census that inferred "not a release parent, therefore unhomed"
produced 54 findings of which 5 were false, and a parity audit's finding was true at its
recorded commit and stale two hours later. A past-tense entry anchored to a commit stays
true. So the ledger holds events and judgments, never derived state — if a detector can
compute it, do not write it down.

#### What writes entries without being asked

Every command that already witnesses something records it as part of doing it. A failed
ledger write fails the command.

| Command | Entry |
|---|---|
| `start`, `claim` | `claimed: <why>` on the branch |
| `finish`, `release-claim` | `claim released`, `claim released; superseded by <branch>`, or bare `superseded by <branch>` for an unheld finish with `--superseded-by` |
| `track --pr/--fork-only/--forget` | the statement that changed |
| `depends --on` | `requires <repo>#<number>` |
| `release cut` | the whole parent set, branch names and commit ids, plus the previous cut's carried-parent delta |
| `sync` | one entry per tracked pull request that merged, closed or advanced |

Nothing is recorded for a pull request that did not move, and nothing injects any of this
into a session: reading the ledger is intentional, and that is the point.

#### In `knives status`

Each branch row carries its newest entry. In JSON that is `last_notch: {ts, kind, text}`,
absent when the branch has none; in text it is one truncated token at the end of the line,
`"superseded by #1157…" (3d)`. Repo-level entries appear separately as
`repo_notches: {count, last}` in JSON and as `notches  <N> repo-level, newest: "<text>" (<age>)`
above the branch table. It is a local ledger read, so it costs nothing.

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

With no arguments or subcommand, plans a release: reports what a cut would contain, whether every parent is still at its branch tip, which local branches are not in the release (or have advanced past their released parent), and consumer pin state. Planning is the default; release commands write only locally and never push.

A release is a flat octopus merge of feature and fix branches, and its parent set is the membership: a branch is in the release exactly when the release has its parent. The upstream base is never a direct parent — members fork from it, so it is reachable through every one of them, and there is no base/member role to classify: a member that lands upstream stays a droppable, advanceable member. Membership changes only through stated edits — `include` adds one parent, `drop` removes one (and states when no remaining member carries the dropped content), `advance` moves members to their branch tips — each rebuilt by duplicating the release onto the changed parent set, so recorded conflict resolutions carry forward and only the change itself can surface new conflicts. Publishing remains a manual `jj git push --bookmark <name>` operation.

#### Scheme variants

- Dated scheme (default, when `release_branch` is absent): cuts create a new dated branch named `release/YYYY-MM-DD` (or `.1`, `.2` for repair cuts). Cutting requires an explicit name argument: `knives release cut release/YYYY-MM-DD`.
- Fixed scheme (when `release_branch = "<name>"` is set): cuts advance the configured release branch in place using jj's internal `--allow-backwards` mechanism. Cutting needs no name argument (`knives release cut` alone); passing a dated name is refused. A cut carries the local composition in hand, unpushed edits included; the *published* position (read from the publish remote, `release` when set in the entry, falling back to `origin`) is what consumers observe and is reported alongside.

#### Release subcommands and options

- `knives release cut [NAME] [--allow-drop]`: audits a candidate cut of the composition in hand — the previous release's parents carried verbatim, nothing joining and nothing advancing — and names it only when the audit passes: each member's net diff, measured from the members' fork point with the upstream trunk, must be present in the cut tree. Divergence the previous release already carried (a recorded conflict resolution) is reported as carried forward, never refused. A failed audit writes nothing at all; a passing one creates and names the release as one operation. Only the first cut, with no composition to carry, starts from every branch. The orphan gate refuses a cut that would strand commits reachable only from the previous lineage; `--allow-drop` overrides it.
- `knives release reap`: reaps superseded dated release bookmarks by forgetting their refs locally and across tracking remotes, then abandoning their merge commits. Reaping also runs automatically after every successful dated cut and never modifies remote repositories. While the live cut still carries unresolved conflicts, every superseded cut is kept: the previous release is the only record of how those conflicts were last resolved, and an abandon-and-recut needs it.
- `knives release rebase [REF]`: the equivalent of `jj rebase -b <release> -d <REF>`. Bare, it asks the forge which of our pull requests merged (merged, not closed) and targets the first upstream trunk commit that contains every one of their merge commits — the point past which nothing merged is missing from the members' shared history; with nothing merged there is no default, and it asks for a commit. Every member branch's commits move onto the target and the release merge moves with them, bookmarks and workspaces following; recorded conflict resolutions replay as ordinary rebase semantics. After a bare rebase (or a bare run that finds the release already at its target), members whose pull requests landed and whose branches carry nothing past the target are dropped, the reason recorded on the release; `--no-drop` keeps them, and a branch with work past its pull is kept and says so. An unheld stale parent (a branch that has moved on) refuses with `Incomplete` — fix the branch or drop it first; a legacy trunk parent is shed on the way through, since the base is never a parent. A merged pull request whose merge commit is not in the local trunk view also refuses — `knives sync` first. A composition whose every member has landed refuses to rebase (the trunk would become its only parent) and refuses to drop its last parent: reap it or include new work.
- `knives release include <branch> [--why "..."]`: add a branch (or any revision) to the release in hand as one new parent. Nothing else changes; a member whose branch has advanced is not moved — that is `advance`'s job, and `include` says so instead of improvising.
- `knives release drop <branch> --why "..."`: remove a branch's parent from the release in hand. The branch and its bookmark are untouched. A branch that advanced past its released parent still resolves by ancestry; a commit id works when no bookmark does. The reason is recorded on the release commit itself, and is required: dropping shipped content without one is how a release becomes unexplainable later, so omitting it is a usage error.
- `knives release advance [<branch>...]`: move member parents to their branches' current tips. Named branches move exactly; a bare `advance` moves every member whose branch has advanced. The trunk parent is `rebase`'s domain.
- All three edits share a rebase's two refusals, both `Incomplete`: when every pin of the release is frozen on a revision, editing it in place would reach nobody, so cut a new dated release instead; and when the upstream trunk cannot be resolved, nothing can separate the release's base parents from its members, so fetch upstream first.
- `knives release --consumer <DIR>`: scans an extra consumer checkout directory alongside any consumers recorded in `repos.toml`. Repeatable (`--consumer <DIR1> --consumer <DIR2>`), because a fork can be consumed by several checkouts sitting on different releases. It widens planning and cutting; `include`, `drop`, `advance` and `rebase` read the recorded consumers only, so record a consumer that should count towards their pin gate.

### `knives register [DIR]`

Prints a paste-ready `[repos.<name>]` TOML entry to stdout, with diagnostic instructions on stderr.

Writes nothing to `repos.toml` directly. The human or caller pastes the stdout snippet into `repos.toml`. Replace any existing `[repos.<name>]` section rather than appending a duplicate entry. Registry edits take effect on the next hook event or tool call without needing a daemon or service restart.

### `knives init [DIR]`

Reads a checkout's remotes and outputs a registry entry or adopts the repository into the registry.

Expects remotes named for their roles: `upstream` (what we contribute to) and `origin` (our fork where branches push and PR heads live), plus an optional `release` remote. Warns if an untracked remote looks like another fork of upstream (detected via case-insensitive owner and slug comparison on the same host), reminding that `origin` must point to your own fork.

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
consumers = ["~/workbench/default"] # optional: consumer checkouts pinning this repo's releases

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
  - `consumers`: checkouts that pin this repository's releases.

- `[trusted.*]`: unmaintained repositories whose agent instructions are trusted for reading.

- `[trust]`: rules for trusting instructions from unmanaged repositories.
  - `roots`: array of directory paths; any repository inside these subtrees is trusted for guidance.
  - `owners`: array of forge organization or user names.

> **SECURITY:** `owners` matches self-declared remote URLs read from the candidate checkout's own git config — not forge-authenticated; any repo that declares itself a checkout of a trusted owner's repo (by remote URL or a `gitdir:` pointer) is accepted; the probe verifies the directory is the repository's own git toplevel, so nested directories do NOT inherit an enclosing repo's identity; owner rules read GIT remote config, so jj-only (non-colocated) checkouts match only via `roots`; grants guidance-as-data injection only (same grant as a `[trusted]` entry), never fork-command access; prefer `roots` when in doubt.

Edits to `repos.toml` take effect on the next hook event or tool call (reloaded per event) — no restart required.

## JSON

`--json` on any command, and it is the default when the environment indicates an agent is running it, so nothing has to grep prose to count findings. `--text` forces prose.

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
