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
libcore  trunk main  release release/2026-07-30  forge consulted (72ms)
  UNANSWERED  1
    cannot tell whether feat/response-filter landed, because local differs from origin
  branches    3
    branch                 state           tip           push                         pr           review             checks   landed  claim                seen  notch
    feat/client-headers    conflicted      d9ae60977abc  pushed                       #4565 draft  approved           failing  -       ada/harness-session  17m   -
    feat/response-filter   checks-failing  5da4e7a7a123  origin=4c94fb019abc (behind)  #4561        no-review          failing  landed?  -                    -     -
    fix/session-isolation  approved        4e8975585def  pushed                       #4559        approved           ok       -       -                    -     -
  findings    6
    divergence        2  qnslzxkkrmnl, qwpowwlkzuym
    checks-failing    2  #4526, #4565
    unmergeable       1  #4565
    stale-review      1  #4559
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

Per branch: local tip, push status, pull request and state, review decision, CI check status, landed verdict against upstream trunk, flags, and the newest ledger entry.

Across branches: divergent bookmarks, release parents that are no longer their branch tip, two
workspaces holding the same change, two branches changing the same file, commits carried into
somebody else's branch, and cross-fork dependencies that have not merged yet.

`knives sync` fetches every remote, discovers tracked pull requests from the forge snapshot,
then classifies each as new, unchanged, advanced, merged, or closed and reports comment activity
since the last run.

## What it remembers

Local state is computed on demand. Forge discovery uses
`$XDG_CACHE_HOME/knives/forge/<owner>/<repo>.json` (default
`~/.cache/knives/forge/<owner>/<repo>.json`). **The cache discovers; a live batch decides.**
It selects pull request numbers from cache, then fetches a complete live row for every pull
request that reaches a report. A cache write failure after a successful live fetch emits a
`forge cache not saved: …` note and preserves the command's exit. Deleting the cache file is
always safe: the next status run uses cold forge discovery and re-runs its landed probes.

One thing cannot be computed: why. `knives finish` deletes the claim that said why a branch
exists, and after that the only honest answer to "what is this branch" is archaeology.

So each repository has a ledger directory at `~/.config/knives/ledger/<repo>/`, beside the
state file. Each entry is an immutable Markdown file with TOML frontmatter between `+++`
fences and a prose body. A write is one atomic `create_new`; entries are never rewritten or
deleted, and there is no lockfile. Every command that witnesses something
writes to the ledger as part of doing it: claims taken and handed back, pull requests
stated, dependencies recorded, the full parent set of every release cut, and each tracked
pull request that merged, closed or advanced. Agents add their own judgments by hand:

```
knives notch '#1413' -m "split to a plugin; the original branch will not land" \
  --disposition ruled-out --evidence https://forge.example/org/libcore/pull/1413
knives notch --dispositions
knives notch --verify '#1413'
```

A disposition is a terminal, past-tense ruling (`merged-elsewhere`, `withdrawn`, or
`ruled-out`) backed by evidence; it remains a human note, not a detector result. A `#<n>`
subject also stamps that pull request, so `knives notch --pr <n>` finds it without tracking a
fictional branch. Bare `knives notch` shows the newest human notes and folds machine events into
one count; `--events` reads their full chronology. `--verify` re-checks selected
commit-shaped evidence and anchors against the repository as it is now.

`knives status` carries the newest human note for each branch, preferring it over a newer machine
event. Its compact notch cell prefixes a disposition, if any, and appends the count of entries it
masks, so the question "what is this weird branch" is usually answered before you ask it.

## Install

knives reads a checkout's identity through git, so a jj workspace must carry a `.git` file beside
its `.jj`. That needs a jj whose `jj workspace add` registers a git worktree for the workspace
(`git.auto-register-worktrees`); upstream jj does not yet, so install a build that does — the one
your fleet's tool configuration names. Then download the knives release archive for your platform
and put `bin/` on your `PATH`:

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

Then stand inside a checkout and print its registry entry:

```
knives register
```

## Configuration

`~/.config/knives/repos.toml` names repositories, not directories. knives finds
the checkout by its `upstream` remote — walking up from where you stand, or
scanning `~` to depth three when you are not inside one.

```toml
[repos.libcore]
upstream = "https://forge.example/org/libcore"
origin = "https://forge.example/ours/libcore"
base = "main"                         # optional: upstream's trunk (defaults to main)
release = "https://forge.example/company/libcore"   # optional: where releases publish
consumers = ["company/workbench"]     # optional: forge slugs that pin this repo's releases
forbidden = ["acme-corp", "internal.example"]   # optional: identifiers an upstream-bound diff must not add

[repos.tool]
upstream = "https://forge.example/org/tool"
origin = "https://forge.example/ours/tool"
release_branch = "integration"        # optional: fixed release branch instead of dated cuts
workspaces = "~/.worktrees/tool"      # optional: where `knives start` opens branch workspaces

[trust]
repos = ["company/workbench"]         # repositories whose AGENTS.md is injected, by identity
owners = ["ours", "company"]          # every repository under these owners
roots = ["~/projects/company"]        # every repository under these directories
```

A fork entry does not grant guidance; `[trust]` does, and it follows the
repository wherever it is cloned. From anywhere inside a checkout,
`knives register` prints the entry for it; nothing writes the file for you.

A fork entry must carry `upstream` and `origin`. That is enforced when the file parses, so a
malformed entry fails there rather than at the first query. `release` is a third optional
remote for when releases are consumed somewhere other than your own fork; it falls back to
`origin`. Two entries cannot share an `upstream`: the entry is the repository's identity, so the
file is refused on load with both names.

A checkout is the entry whose `upstream` its own `upstream` remote matches; `.git`, a trailing
`/`, and letter case do not matter, and a value that is not a URL (a filesystem path) compares as
its trimmed text. A checkout whose `origin` or `release` remote differs from the registry still
binds, and `status` and `repos` carry a note saying so: `origin remote is <X>; registry says <Y>`.

`knives start` opens a branch's workspace beside the checkout, named for the branch: the
`<name>/default` layout, where the workspaces are `default`'s siblings. A checkout at `~/<name>`
has no room for siblings, so its entry sets `workspaces` to the directory they go in instead;
`finish` removes them from the same place. `~` expands and a relative value is taken from the
config directory, so write it as `~/…`. A `workspaces` inside the checkout is refused by `knives
start` and `finish`, the two verbs that use it.

`consumers` records forge slugs, not checkout paths. Knives reads supported pin files from each
consumer repository's trunk and caches that scan by the trunk commit. If the forge is unavailable,
cached results are labeled as such and the command is incomplete; pass `--consumer PATH` for an
ad-hoc local scan without recording the path.

`forbidden` lists identifiers an upstream-bound diff must not add: your org, your product, your
hosts. `knives audit` reports every line a branch adds over the upstream trunk that contains one,
as a case-insensitive substring; a branch stated `--fork-only` is exempt. Absent, no scan runs.

Every command takes its repo from the directory you are standing in, wherever that checkout
lives. Name one only when you are somewhere else; knives then scans `~` three directories deep
for jj checkouts, as `knives repos` and `status --all` always do. An entry with no checkout found
reads `not on this machine`, and an entry with two is refused with both paths named.

## Commands

| | |
|---|---|
| `knives repos` | what is managed, where each checkout was found (or `not on this machine`), the newest release each has cut, and whether registered forge consumers pin it |
| `knives consumers [FORK] [--consumer PATH]...` | compare registered forge consumers and ad-hoc local scans with the newest release on the live publish remote; reports only |
| `knives pushed [BRANCH]... [--repo REPO]` | compare local branches with the live remote refs that own them; reports only |
| `knives audit [REPO] [--all] [--no-github]` | reconcile remote refs, open pull heads, recorded cuts, and anonymous heads; per branch: tip vs origin, pull mergeability / merge state / review decision, check-run counts on the head, unresolved review threads, PR-template headings missing from the body, configured `forbidden` identifiers in the diff over upstream trunk; reports only and never repairs |
| `knives status` | the main report |
| `knives pr NUMBER [--repo REPO] [--timeline]` | one pull request's live state; `--timeline` adds its bounded forge event log |
| `knives sync` | fetch, then classify what happened to each tracked pull request |
| `knives preflight` | the facts to check before contributing upstream |
| `knives start` | take a branch and get your own workspace: on its tip, or on the release's shared base for a new one; waits with backoff up to a minute for the claim lock, and a refused wait names the holder's pid and how long it has held (a lock whose holder is not running is refused at once); states the fork's `immutable_heads()` (trunk and tags, with the trunk named on every knives remote) in the repo's jj config where none is stated |
| `knives finish` | hand a branch back so another agent can pick it up; its bookmark and any open pull request survive |
| `knives track` | state which pull request a branch belongs to, when inference cannot find it |
| `knives depends` | record that a branch cannot land before another repo's pull request |
| `knives notch [SUBJECT] [-m TEXT] [--disposition TOKEN]` | read the ledger or write a human note; dispositions require evidence, `--dispositions` reads terminal rulings, and `--verify` re-checks selected entries |
| `knives release` | plan a release, edit its membership, cut one, or reap superseded cuts |
| `knives release members [REF] [--verify] [--carries REV] [--census] [--no-github]` | list a release's direct member parents, their holders and advances; `--verify` audits every member's content in the release; `--carries REV` asks whether REV's content is carried — by REF, or by every live release and upstream trunk; `--census` asks that of every maintained branch, conditionally checks superseded releases, and reports qualified orphans |
| `knives register [DIR]` | print the registry entry for a checkout, or `already registered as <name>`; writes nothing |
| `knives hook` | harness plumbing, not for humans |
| `knives gh` | fork-aware `gh` passthrough |

When the environment indicates an agent is running a command (or stdout is not a terminal),
the report is emitted as [TOON](https://github.com/toon-format/toon) — the same structure as
JSON at fewer tokens. `--json` forces JSON exactly; `--text` forces prose. The two machine
encodings are lossless renderings of one report.

Exit codes: `0` nothing to report, `1` findings, `2` usage, `3` something could not be
answered.

### Timing

Set `KNIVES_TIMING=1` to write one `timing gh <repo> <duration>ms: <argv summary>` line to
standard error for every forge call. Timing is diagnostic output and does not change the report.

## GitHub CLI passthrough

`knives gh -- <args...>` absorbs the fork-routing logic of the `gh` bash shim:

* **Target resolution**: `-R` passthrough, `gh repo set-default` markers, remote preference, and `gh api` owner extraction.
* **Token export**: queries git credential config for `gh-app-token` and exports `GH_TOKEN` for the child process.
* **Detached HEAD compensation**: injects the active jj bookmark into `gh pr` subcommands when git reports no symbolic HEAD.

The `--` delimiter is required. All arguments after `--` are passed to `gh` verbatim.

The routing table stays in gitconfig (`gh-resolved` markers, credential helpers). Knives reads it, does not own it.

Escape hatches:

* `KNIVES_GH_BYPASS` on the shim bypasses `knives gh` entirely.
* `KNIVES_REAL_GH` points `knives gh` at a specific real `gh` binary. A value that is itself a marker-bearing shim is ignored in favor of the PATH scan — a poisoned override must never re-enter the shim.

## Release workflow

A release is a flat octopus merge of feature and fix branches, and its parent set is its membership: a branch is in the release exactly when the release has its parent. The upstream base is never a direct parent — every member forks from it, so it is reachable through each of them, and there is no role to classify. Membership changes only through stated edits. `knives release include` adds one parent, `knives release drop` removes one (saying so when no remaining member carries the dropped content), and `knives release advance` moves member parents to their branches' current tips — refusing outright, rather than silently deduping, if the same branch would replace more than one parent, and accepting `--from <old-sha>` to name one branch's old parent directly when a `jj duplicate` rebuild has left it with no ancestry back to the commit it replaces. `knives release rebase` moves the whole composition onto a newer upstream commit — the equivalent of `jj rebase -b <release> -d <target>` — members and their bookmarks moving together; bare, it targets the first upstream trunk commit that contains every merged pull request, then drops the members whose landed branches carry nothing more (`--no-drop` keeps them); with nothing merged it asks for a commit. Each edit duplicates the release onto the changed parent set, so recorded conflict resolutions carry forward and only the change itself can surface new conflicts. `knives release cut` names a new cut of the composition in hand, verbatim: nothing joins, nothing advances, and a branch created since the last release enters through `include`, never by existing. Only the first cut, with no composition to carry, starts from every branch.

One branch is both the release member and the upstream pull request head; there is never a
second copy. New branches fork from the release's shared base (`knives start` puts them there,
or on the fetched upstream trunk when no release exists) and are linear past it, so a new branch
composes into the release without forcing a rebase; an existing branch is continued from its tip,
and a name that exists only on upstream is still a new branch here. Moving the composition to a newer trunk is
`knives release rebase`, a decision of its own. When a branch is rebased onto a newer trunk,
`advance` still recognises it as the same member by change id, which `jj rebase` keeps. A
branch that does not compose into the release means the release is behind: move the release,
never mint a "release-lineage" sibling of the branch on an older base. A member whose own
history carries a release merge is reported as `stacked-history`, and the plan stops calling
the cut `flat`.

`knives release members --carries REVISION` compares the revision with every live release and the
upstream trunk; `knives release members TARGET --carries REVISION` asks exactly one target;
`knives release members --census` asks that of every maintained branch.

Before any gate runs, a cut whose tree equals the previous cut as the publish remote holds it is refused: a new name would ship nothing and only ask consumers to re-pin. A previous cut not yet pushed has no consumer to protect and is not compared.

Before cutting, the orphan gate verifies that no commits exist reachable only from the previous release lineage or its descendants; if commits would be stranded without a remaining bookmark or upstream trunk reaching them, the cut refuses and lists the exact commit IDs. Passing `--allow-drop` overrides this refusal when dropping those commits is intentional.

A second pre-publish gate holds the candidate against the previous cut's ledger event — the only record of a parent set that survives the release bookmark moving, since every edit relocates the name and the next cut reaps the superseded commit. A recorded member whose content the candidate does not carry (not a parent, not an ancestor of one, and its net diff absent from the candidate tree) refuses the cut and is named; a member that landed upstream and entered the candidate through its base passes without comment. `--allow-drop` states the drop is intended, and the new cut's ledger event then records exactly which members were dropped.

`knives release cut` audits a candidate merge built in a scratch transaction that is never committed. The audit replays each member's net diff, measured from the members' fork point with the upstream trunk, against the candidate cut tree to ensure no carried content was lost in an auto-merge or lockfile resolution, and checks for unexplained diff drift against the previous release. Divergence the previous release already carried is a recorded conflict resolution: it is reported as carried forward, never refused — the audit charges a cut only with divergence it introduces. A failed audit writes nothing at all — the candidate simply evaporates — and a passing one publishes the creation and naming of the release as a single operation, after verifying the published tree is identical to the audited one.

`knives release reap` cleans up superseded dated release bookmarks by forgetting their refs locally and across tracking remotes before abandoning their merge commits. Reaping also runs automatically after every successful dated cut. While the live cut still carries unresolved conflicts, every superseded cut is kept: the previous release is the only record of how those conflicts were last resolved, and an abandon-and-recut needs it. Reaping never modifies remote repositories; subsequent fetches may re-materialize forgotten remote refs as untracked bookmarks, which is harmless and cleared by the next reap.

An edit follows consumer pin state exactly as a rebase does: when every pin of that release is frozen on a revision, editing it in place would reach nobody, so the edit refuses and says to cut a new dated release; a consumer frozen on an older release is not reached either way and does not block the edit. An edit also refuses when the upstream trunk cannot be resolved, because it is the trunk that separates the release's base parents from its members.

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
one rather than removing it. The `[trust]` rules are the allowlist: only a repository they name —
by identity, owner, or directory — contributes guidance, and a fork entry alone contributes the
managed notice and nothing more. Containment is checked by path components rather than string
prefix, symlinks are resolved first, and the `AGENTS.md` files from the touched file's directory
up to the repository root are injected nearest first, while `CONTRIBUTING.md` is mentioned rather
than injected. Guidance arrives wrapped in a per-injection nonce and framed as data, so a
repository whose files you are reading cannot forge an instruction to you.

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
