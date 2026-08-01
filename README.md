# knives

Reports the state of several forks worked by several agents at once.

If you maintain forks of upstream projects, carry patches on branches, and integrate them into
dated release branches, the state you need is spread across jj, the forge, and whatever your
consumers pin. Answering "is this branch still worth carrying" by hand means several commands
and a guess. Agents guess instead of asking, and two of them working the same repository
collide without noticing.

`knives` answers those questions in one command.

```
$ knives status
libcore
  releases    2 checked: release/2026-07-30, release/2026-07-28.2@origin
  branches    12
    feat/client-headers   d9ae60977  pushed  #4565  REVIEW_REQUIRED  draft  checks-failing  CONFLICTING
    feat/response-filter  5da4e7a7a  origin=4c94fb019 (diverged)  #4561  REVIEW_REQUIRED  behind-base
    fix/session-isolation 4e8975585  pushed  #4559  CHANGES_REQUESTED  stale-review
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

## It reports facts and does not advise

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

Per branch: the local tip, where origin has it and in which direction they differ, the pull
request and its state, the review decision, whether that review predates the newest commit,
whether CI is red, and how the branch relates to the upstream trunk.

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
base = "main"                       # optional, the branch upstream expects pull requests against
consumers = ["~/workbench/default"] # optional, who pins this repo's releases

[trusted.workbench]
path = "~/workbench/default"        # instructions read, not maintained
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
| `knives repos` | what is managed, the newest release each has cut, and which consumers are pinned behind it |
| `knives status` | the main report |
| `knives sync` | fetch, then classify what happened to each tracked pull request |
| `knives preflight` | the facts to check before contributing upstream |
| `knives start` / `finish` | take a branch and get your own workspace, then hand it back |
| `knives track` | state which pull request a branch belongs to, when inference cannot find it |
| `knives depends` | record that a branch cannot land before another repo's pull request |
| `knives release` | plan a dated release, or cut one |
| `knives init` | register a checkout |

`--json` works on any of them, and is the default when the environment indicates an agent is
running it. `--text` forces prose.

Exit codes: `0` nothing to report, `1` findings, `2` usage, `3` something could not be
answered.

## For agents

An agent working in a fork does not receive that fork's `AGENTS.md`, because instruction
injection is bounded to the directory the session started in. It can therefore read, edit, and
open pull requests against a repository whose contribution rules it has never seen.

The OpenCode plugin shipped in the release closes that. The first time a call names a file
inside a managed repository, it says the repository is managed and shared, names any branches
other agents hold, and appends that repository's own root guidance as data. Once per repository
per session.

The boundary that made the gap is a security control, so the plugin re-establishes an
equivalent one rather than removing it. The registry is the allowlist: only repositories listed
there contribute guidance, containment is checked by path components rather than string prefix,
symlinks are resolved first, and nested guidance inside a repository is mentioned rather than
injected. Guidance arrives wrapped in a per-injection nonce and framed as data, so a repository
whose files you are reading cannot forge an instruction to you.

Three skills ship alongside it: `fork-work` for what to check before touching a fork,
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
