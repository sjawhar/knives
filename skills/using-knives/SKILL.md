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

The main report. Per branch: local tip, push status, pull request and its state, review decision, CI check status, landed verdict against upstream trunk, and flags. Plus claims other agents hold, the releases scanned, findings grouped one line per kind, and anything it could not answer.

`--verbose` prints one block per finding instead of one line per kind. `--no-landed` skips the trunk probe, which is the slow part. `--no-github` skips pull request lookups.

#### Branch table columns

Branch rows are rendered as an aligned table with 8 columns. Empty cells render as `-` for `review`, `checks`, `landed`, and `flags` columns (`branch`, `tip`, `push`, and `pr` always carry values or state tokens).

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

### `knives release [REPO]`

With no arguments or subcommand, plans a release: reports what a cut would contain, whether every parent is still at its branch tip, and consumer pin state. Planning is the default because cutting is the only thing knives writes, and it still never pushes.

A release cut is a flat octopus merge combining the tips of all maintained branches. Publishing remains a manual `jj git push --bookmark <name>` operation.

#### Scheme variants

- Dated scheme (default, when `release_branch` is absent): cuts create a new dated branch named `release/YYYY-MM-DD` (or `.1`, `.2` for repair cuts). Cutting requires an explicit name argument: `knives release cut release/YYYY-MM-DD`.
- Fixed scheme (when `release_branch = "<name>"` is set): cuts rebuild the flat octopus merge and advance the configured release branch in place using jj's internal `--allow-backwards` mechanism. Cutting needs no name argument (`knives release cut` alone); passing a dated name is refused. The previous release position is read from the publish remote (`release` when set in the entry, falling back to `origin`).

#### Release subcommands and options

- `knives release rebase [REF]`: adds an upstream commit (defaulting to upstream's trunk) to the release in hand, keeping its branch parents — it does not re-cut. For when a pull request has merged upstream: until the release contains the commit that merge landed in, dropping the local branch takes the change out of the release with it. Whether this can happen in place or needs a new dated name follows from consumer pin state (a consumer following the branch sees a repair; one frozen on a revision does not). A permitted in-place repair rebuilds the flat merge and moves the release bookmark, including when jj considers that move sideways; integration coverage verifies that parent topology is retained.
- `knives release include <branch> --why "..."` and `knives release drop <branch> --why "..."`: state whether a branch belongs in the next release. Membership is every branch until anything is stated, after which membership is **exactly** what was stated — stating one `include` or `drop` converts the cut from "all branches" to "only stated branches". `drop` records the reason so subsequent cuts do not re-include the branch.
- `knives release --consumer <DIR>`: scans an extra consumer checkout directory alongside any consumers recorded in `repos.toml`. Repeatable (`--consumer <DIR1> --consumer <DIR2>`), because a fork can be consumed by several checkouts sitting on different releases.

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
