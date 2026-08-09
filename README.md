# knives

Reports the state of several forks worked by several agents at once.

If you maintain forks of upstream projects, carry patches on branches, and integrate them into
release branches, the state you need is spread across jj, the forge, and whatever your
consumers pin. Answering "is this branch still worth carrying" by hand means several commands
and a guess. Agents guess instead of asking, and two of them working the same repository
collide without noticing.

`knives` answers those questions in one command.

```
$ knives status
libcore
  releases    2 checked: release/2026-07-30, release/2026-07-28.2@origin
  branches    12
    branch                 tip        push                         pr           review             checks   landed  flags
    feat/client-headers    d9ae60977  pushed                       #4565 draft  REVIEW_REQUIRED    failing  -       CONFLICTING
    feat/response-filter   5da4e7a7a  origin=4c94fb019 (diverged)  #4561        REVIEW_REQUIRED    ok       -       behind-base
    fix/session-isolation  4e8975585  pushed                       #4559        CHANGES_REQUESTED  ok       -       review-stale
  findings    6
    divergence          2  qnslzxkkrmnl, qwpowwlkzuym
    checks-failing      2  #4526, #4565
    unmergeable         1  #4565
    stale-review        1  #4559
  claims      1
    feat/client-headers  ada  since 2026-07-31T21:27:31Z
      conflicts, lint, and the version gate
```

The first branch there reads as finished from every angle a person checks: approved, pushed,
nothing left to write. It is a draft, its CI is red, and the forge cannot merge it.

## No suggested fixes

Every finding used to carry a suggested fix. The suggestions were wrong often enough to be a
liability: drop a branch that had never landed, open a pull request that already existed,
re-cut a release nothing pinned. They are gone. A finding now says what was observed and
nothing about what it means.

The same rule decides what the tool refuses to say. Verdicts describe observations rather than
conclusions, so replaying a branch onto the trunk gives `in-trunk`, `conflicts-with-trunk`, or
`not-in-trunk`, never "the maintainer modified your work". A branch whose local tip differs
from origin gets `landed?`, because replaying it would judge content the pull request does not
contain, and refusing to answer beats guessing. Anything the tool could not determine goes to
an `unanswered` section and sets a non-zero exit, instead of being rendered as a fact.

No check reasons about intent. Each one is a forge field or a jj graph query. The reasoning is
yours.

## What it checks

Per branch: local tip, push status, pull request and state, review decision, CI check status, landed verdict against upstream trunk, and flags.

Across branches: divergent bookmarks, release parents that are no longer their branch tip, two
workspaces holding the same change, two branches changing the same file, commits carried into
somebody else's branch, and cross-fork dependencies that have not merged yet.

`knives sync` fetches every remote and classifies each tracked pull request as new, unchanged,
advanced, merged, or closed, and reports comment activity since the last run.

## Install

Download the release archive for your platform, then put `bin/` on your `PATH`:

```
tar xzf knives-v0.1.2-linux-x86_64.tar.gz
install -m755 knives-*/bin/knives ~/.local/bin/
```

Install the Claude Code plugin separately:

```
/plugin marketplace add sjawhar/knives
/plugin install knives@knives
```

The Claude Code plugin ships its hooks and skills. The binary still comes from the release archive
above. Without that binary, the hooks exit silently. An old binary emits a SessionStart system
message that asks you to update knives or set `KNIVES_BIN`.

Install the OpenCode plugin from the release archive by adding its path to `opencode.json`:

```jsonc
"plugin": [
  "file:///<prefix>/share/knives/opencode/plugins/knives.ts"
]
```

The plugin finds `<prefix>/bin/knives` automatically. It registers the skills included in the
archive without another configuration step.

Or build from source with a Rust toolchain:

```
cargo install --path .
```

Register a checkout and the tool reads its remotes:

```
knives init ~/forks/libcore/default
```

## Configuration

`~/.config/knives/repos.toml`:

```toml
[repos.libcore]
path = "~/forks/libcore/default"
upstream = "https://forge.example/org/libcore"
origin = "https://forge.example/ours/libcore"
base = "main"                         # optional: upstream's trunk (defaults to main)
release_branch = "release"            # optional: fixed release branch scheme (omit for dated release/YYYY-MM-DD)
consumers = ["~/workbench/default"] # optional: who pins this repo's releases

[trusted.workbench]
path = "~/workbench/default"       # instructions read, not maintained

[trust]
roots = ["~/projects/company"]      # optional: subtrees whose repos are trusted for guidance
```

A fork entry must carry `upstream` and `origin`. That is enforced when the file parses, so a
malformed entry fails there rather than at the first query. `release` is a fourth optional
remote for when releases are consumed somewhere other than your own fork; it falls back to
`origin`.

Every command takes its repo from the directory you are standing in. Name one only when you
are somewhere else.

## Commands

| | |
|---|---|
| `knives repos` | what is managed, the newest release each has cut, and whether consumer origin trunks are pinned behind it |
| `knives status` | the main report |
| `knives sync` | fetch, then classify what happened to each tracked pull request |
| `knives preflight` | the facts to check before contributing upstream |
| `knives start` / `finish` | take a branch and get your own workspace, then hand it back |
| `knives track` | state which pull request a branch belongs to, when inference cannot find it |
| `knives depends` | record that a branch cannot land before another repo's pull request |
| `knives release` | plan a release, edit its membership, cut one, or reap superseded cuts |
| `knives init` | register a checkout |
| `knives hook` | harness plumbing, not for humans |
| `knives gh` | fork-aware `gh` passthrough |

`--json` works on any of them, and is the default when the environment indicates an agent is
running it. `--text` forces prose.

Exit codes: `0` nothing to report, `1` findings, `2` usage, `3` something could not be
answered.

## GitHub CLI passthrough

`knives gh -- <args...>` absorbs the fork-routing logic of the `gh` bash shim:

* **Target resolution**: `-R` passthrough, `gh repo set-default` markers, remote preference, and `gh api` owner extraction.
* **Token export**: queries git credential config for `gh-app-token` and exports `GH_TOKEN` for the child process.
* **Detached HEAD compensation**: injects the active jj bookmark into `gh pr` subcommands when git reports no symbolic HEAD.

The `--` delimiter is required. All arguments after `--` are passed to `gh` verbatim.

The routing table stays in gitconfig (`gh-resolved` markers, credential helpers). Knives reads it, does not own it.

Escape hatches:

* `KNIVES_GH_BYPASS` on the shim bypasses `knives gh` entirely.
* `KNIVES_REAL_GH` points `knives gh` at a specific real `gh` binary.

## Release workflow

A release is a flat octopus merge of feature and fix branches, and its parent set is its membership: a branch is in the release exactly when the release has its parent. The upstream base is never a direct parent — every member forks from it, so it is reachable through each of them, and there is no role to classify. Membership changes only through stated edits. `knives release include` adds one parent, `knives release drop` removes one (saying so when no remaining member carries the dropped content), and `knives release advance` moves member parents to their branches' current tips. `knives release rebase` moves the whole composition onto a newer upstream commit — the equivalent of `jj rebase -b <release> -d <target>` — members and their bookmarks moving together; bare, it targets the first upstream trunk commit that contains every merged pull request, then drops the members whose landed branches carry nothing more (`--no-drop` keeps them); with nothing merged it asks for a commit. Each edit duplicates the release onto the changed parent set, so recorded conflict resolutions carry forward and only the change itself can surface new conflicts. `knives release cut` names a new cut of the composition in hand, verbatim: nothing joins, nothing advances, and a branch created since the last release enters through `include`, never by existing. Only the first cut, with no composition to carry, starts from every branch.

Before cutting, the orphan gate verifies that no commits exist reachable only from the previous release lineage or its descendants; if commits would be stranded without a remaining bookmark or upstream trunk reaching them, the cut refuses and lists the exact commit IDs. Passing `--allow-drop` overrides this refusal when dropping those commits is intentional.

`knives release cut` audits a candidate merge built in a scratch transaction that is never committed. The audit replays each member's net diff, measured from the members' fork point with the upstream trunk, against the candidate cut tree to ensure no carried content was lost in an auto-merge or lockfile resolution, and checks for unexplained diff drift against the previous release. Divergence the previous release already carried is a recorded conflict resolution: it is reported as carried forward, never refused — the audit charges a cut only with divergence it introduces. A failed audit writes nothing at all — the candidate simply evaporates — and a passing one publishes the creation and naming of the release as a single operation, after verifying the published tree is identical to the audited one.

`knives release reap` cleans up superseded dated release bookmarks by forgetting their refs locally and across tracking remotes before abandoning their merge commits. Reaping also runs automatically after every successful dated cut. While the live cut still carries unresolved conflicts, every superseded cut is kept: the previous release is the only record of how those conflicts were last resolved, and an abandon-and-recut needs it. Reaping never modifies remote repositories; subsequent fetches may re-materialize forgotten remote refs as untracked bookmarks, which is harmless and cleared by the next reap.

An edit follows consumer pin state exactly as a rebase does: when every pin of the release is frozen on a revision, editing it in place would reach nobody, so the edit refuses and says to cut a new dated release. An edit also refuses when the upstream trunk cannot be resolved, because it is the trunk that separates the release's base parents from its members.

## For agents

An agent working in a fork does not receive that fork's `AGENTS.md`, because instruction
injection is bounded to the directory the session started in. It can therefore read, edit, and
open pull requests against a repository whose contribution rules it has never seen.

Knives has adapters for OpenCode and Claude Code. The OpenCode plugin is a thin shim that calls
`knives hook opencode` in the binary. The Claude Code plugin installs shell hooks that call
`knives hook claude-code` in the same binary.

Both adapters identify managed repositories and report branches other agents hold. OpenCode
adds the notice and repository guidance on the first relevant tool call that names a file there.
Claude Code adds guidance when a relevant tool call reaches a foreign repository. It does not
add separate guidance for the session repository because Claude Code already loads that
repository's `CLAUDE.md`.

The boundary that made the gap is a security control, so both adapters re-establish an equivalent
one rather than removing it. The registry is the allowlist: only repositories listed there
contribute guidance, containment is checked by path components rather than string prefix,
symlinks are resolved first, and nested guidance inside a repository is mentioned rather than
injected. Guidance arrives wrapped in a per-injection nonce and framed as data, so a repository
whose files you are reading cannot forge an instruction to you.

Three skills ship with both adapters: `fork-work` for what to check before touching a fork,
`using-knives` for the CLI, and `pr-preflight` for contributing upstream.

## What it does not do

It does not create pull requests. That is `gh pr create`.

It does not replace jj. General version control stays where it is.

It does not coordinate across machines.

It does not judge. Anything of the form "have you read the contributing guide and does this
change comply" is a question for a person or an agent, not a CLI, and the tool does not pretend
otherwise.

## Status

Early. It is used daily against about ten forks, and the checks it reports have each been
verified against real repositories rather than only against fixtures. Interfaces may still
move.
