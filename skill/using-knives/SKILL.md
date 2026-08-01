---
name: using-knives
description: Reference manual for the knives CLI, which reports and coordinates state across several forks of upstream repositories worked by several agents. Use when running any knives command, when interpreting what one printed, or when you need the detail behind it: what the upstream, origin and release remotes mean, how a branch is matched to a pull request and how to state one it cannot find, recording that one branch cannot land before another, planning and cutting dated releases, JSON output, and the OpenCode plugin's options. For the shorter question of what to do before touching a fork at all, use the fork-work skill.
---

# The knives CLI

## What it is for

Several forks of several upstreams, worked concurrently by several agents. Two things go wrong without help: an agent guesses at state that could have been queried, and two agents collide on the same branch. knives answers the first and makes the second visible.

It reports. It does not advise: an earlier version attached a suggested fix to every finding, and the suggestions were wrong often enough to be a liability, telling you to drop a branch that had never landed, to open a pull request that already existed. What a report says is what is true, and what to do about it is yours to decide.

Every command takes its repo from the directory you are standing in. Name one only when you are somewhere else, or want a different one.

## The commands

### `knives repos`

What is managed, where each checkout is, the newest release each has cut, and, where a consumer is recorded, whether that consumer is pinned behind the newest cut. Trusted entries, which are repositories whose instructions we read but do not maintain, are listed separately.

### `knives status [REPO|--all]`

The main report. Per branch: local tip, where origin has it, the pull request and its state, the review decision, whether the review predates the newest commit, and how the branch relates to the upstream trunk. Plus claims other agents hold, the releases scanned, findings grouped one line per kind, and anything it could not answer.

`--verbose` prints one block per finding instead of one line per kind. `--no-landed` skips the trunk probe, which is the slow part. `--no-github` skips pull request lookups.

#### Branch-line tokens

A branch line renders tokens in order:

- Local tip: short commit hash, or `divergent` when local working copy has multiple commits for this change.
- Origin relation:
  - `unpushed` if origin has no counterpart for this branch.
  - `unpushed-commits` if local is ahead of origin.
  - `origin=<id> (behind)` if origin is ahead of local.
  - `origin=<id> (diverged)` if local and origin histories have forked, usually after a rewrite post-push.
  - `origin=<id> (unresolved)` if ancestry could not be determined. A matching entry appears under `unanswered`. This is not a claim about history.
  - `pushed` if local tip matches origin tip.
- Pull request tokens:
  - `#<n>` followed by lowercased state when closed or merged (for example, `closed`, `merged`).
  - Review decision: `no-review` or forge decision (`APPROVED`, `CHANGES_REQUESTED`).
  - `draft` if the pull request is marked as draft.
  - `checks-failing` if the forge reported failing CI checks on an open pull request.
  - `no-checks` if we asked the forge and no checks had run on an open pull request. A branch whose checks were not consulted shows no token at all. CI still in flight renders the same as all-green (no token) because the vocabulary has no `checks-running`. Checks are consulted only for open pull requests.
  - `CONFLICTING` if the forge reports merge conflicts with the base branch.
  - `behind-base` if the pull request base branch has moved on, which is not a conflict.
- Landed verdict: `in-trunk`, `conflicts-with-trunk`, `not-in-trunk`, or `landed?`.
- `review-stale` if the newest review predates the newest commit on the branch.
- `#<n> <state> (stated)` if a pull request was explicitly stated for this branch rather than inferred.
- `fork-only` if marked as deliberately having no upstream pull request.
- `no-pr` if no pull request was inferred or stated.

Trunk verdicts say what was observed, not what it means:

- `in-trunk`: replaying the branch onto the trunk produced nothing, so the trunk has it.
- `conflicts-with-trunk`: replaying it conflicts. This does not mean the maintainer took it and modified it: a branch declined upstream, whose files were later touched by unrelated work, conflicts identically. The tool cannot tell those apart and does not try.
- `not-in-trunk`: it applies cleanly and is not empty, so the trunk lacks it.
- `landed?`: local differs from origin, so replaying would judge content the pull request does not contain. Refusing to answer beats guessing here, because the answer used to be "already upstream, drop it".

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
- `wrong-base`: the pull request targets a branch whose name differs from the repo's configured base branch. It cannot tell a pull request aimed at our fork's `main` from one aimed at upstream's `main` because both are named `main` and `gh` resolves to upstream anyway. An empty base is unknown, not wrong. Only open pull requests are checked.
- `carried-elsewhere`: the branch tip is reachable from another reference. It reports where it was found and says nothing about what it means, whether a maintainer rebased it, took it, or coincidentally landed the same content. Our own release cuts, `@git` refs, and trunk are excluded.
- `branch-overlap`: two or more of our branches change the same file, which conflicts when a release merges them. One finding per file, naming every branch. It is a path comparison and nothing more.

### `knives sync [REPO|--all]`

Fetches every remote and every tracked pull request head, then classifies each tracked pull request as `new`, `unchanged`, `advanced`, `merged` or `closed`. Forge state wins over head movement: a merged pull request whose head also moved is merged.

Running `sync` with no arguments inside a managed repository selects that repository. Outside any managed repository, it asks for a repository name or `--all`.

`sync` also checks for new comment activity on open tracked pull requests. When a pull request has comments newer than the last sync mark, it prints a note: `#<n> has comment activity newer than the last sync`. Agents can grep for this exact string. Activity goes to notes (exit 0, informational). A comment query failure goes to problems (exit 3). `--no-github` skips pull-request and comment lookups while retaining local fetch and head checks.

Checking comments costs one extra forge call per open tracked pull request. The mark lives in state as `comment_marks`, keyed `<repo>#<number>`, and advances forward. The first time a pull request is seen, the mark is recorded silently without printing a note, avoiding noise on first run. Edited comments are invisible because the forge `createdAt` timestamp does not move on edit.

### `knives preflight [REPO]`

The facts you need before contributing upstream: convention files present and whether they have changed since last seen, any stated cap on open pull requests, branch state. It reports; the judgment is yours. The `pr-preflight` skill walks the gate.

### `knives start <branch>` and `knives finish <branch>`

`start` claims the branch and opens a jj workspace for it, based on the fetched upstream trunk rather than wherever `@` happens to be. An agent sitting in a release workspace who runs `jj new` silently inherits the release merge as a parent.

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

### `knives release`

With no name, plans: what a cut would contain, whether every parent is still its branch tip, and who pins the current release. With a name, cuts it. Planning is the default because cutting is the only thing knives writes, and it still never pushes.

A cut is a flat octopus merge whose parents are the branch tips, pushed to the release remote. Nothing needs mirroring first: the push carries the parent commits.

### `knives init [DIR]`

Reads the remotes of a checkout and writes it into the registry.

## The three remotes

- `upstream`: what we contribute to. Only ever through a pull request.
- `origin`: our own copy.
- `release`: optional, for where releases are consumed internally rather than from a personal fork. Falls back to `origin` when absent. Not every fork needs one.

## The registry

`~/.config/knives/repos.toml`. Two kinds of entry:

```toml
[repos.scout]
path = "~/forks/scout/default"
upstream = "https://forge.invalid/org/scout"
origin = "https://forge.invalid/ours/scout"
base = "main"                         # optional: branch upstream expects PRs against (defaults to main)
consumers = ["~/workbench/default"] # optional: who pins this repo's releases

[trusted.workbench]
path = "~/workbench/default"       # instructions read, not maintained
```

`[repos.*]` is what we maintain forks of, and a fork entry must carry its remotes: that is enforced when the file parses. `base` is optional and defaults to `main`, specifying the branch upstream expects pull requests against. A fork whose trunk is `develop` needs `base = "develop"` set, or every pull request triggers a `wrong-base` finding.

`[trusted.*]` is a repository whose agent instructions should reach an agent but which we do not maintain, so it needs only a path. No fork command touches a trusted entry.

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
