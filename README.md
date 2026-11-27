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
since the last run. When another fetch of the same clone wins a ref update, the remote fetch and
each carried foreign pull-request fetch are retried up to four more times after short jittered waits
(under 1.5 s in all). git reports every ref-lock failure the same way, so one that is no race (a lock
file a crashed git left behind, a ref directory it may not write) also costs those retries before its
error returns.

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

So each fork has a ledger directory at `~/.config/knives/ledger/<owner>/<name>/`, beside the
state file, named for its upstream repository in lowercase so that every machine files it in
the same place. Each entry is an immutable Markdown file with TOML frontmatter between `+++`
fences and a prose body, carrying the writer's `git config user.email` when one is set. A write
is one atomic no-clobber rename, and an entry is never rewritten. Outside `knives ledger
migrate`, a machine removes an entry in two cases only: its own uncommitted repeat of a transition
another machine already shared, and the entry of a claim change that the ledger's repository did
not take. Every command
that witnesses something writes to the ledger as part of doing it: claims taken and handed back,
pull requests stated, dependencies recorded, the full parent set of every release cut, and each
tracked pull request that merged, closed, reopened or advanced. Agents add their own judgments by
hand:

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
commit-shaped evidence and anchors against the repository as it is now. A note is anchored at
the subject's tip that its `--evidence` names, local or on a remote, else at its local bookmark;
an entry its evidence anchored records that ref as `anchor_ref`. When its evidence names none of
the subject's tips, and that local bookmark is missing or a remote holds the subject elsewhere, the
write says so on stderr. `--verify` flags an anchor as moved when the ref it was read from is at
another commit now.

A fork's ledger travels between machines when its `repos.toml` entry names the repository its
ledger belongs to (`ledger = "<owner>/<name>"`), and a git directory over the ledger root has that
repository as its `origin` and names this machine (`knives.machine`): the root's own `.git`, or
one in `~/.config/knives/ledger-repositories/`, one per repository the ledgers travel through.
Where no git directory has that `origin`, knives makes one in `ledger-repositories/` the first
time it needs one, and says so on stderr. Every write sends its entries once it has decided,
before the command exits: it commits them to the machine's own ref, pulls every other machine's,
and pushes. A write whose entries did not reach the ledger, a `knives notch` note among them,
exits `3` after one line per fork:

```
<fork>: <n> ledger entries this command wrote did not reach the ledger <ledger> (<git dir>): <cause>; `knives ledger sweep` sends them
```

For one entry the line reads `1 ledger entry`. The entries stay on the machine. One sweep or send
runs at a time: each waits up to a minute for `ledger.lock`, beside the ledger root, while another
holds it. A sweep passes over every git directory until a pass finds nothing new, and nothing ever
rebases.
`ledger-sweep.log`, beside `ledger.lock`, holds the problems of the last sweep or send, and exists
only while it failed. A command that decides from the ledger (`status`, `sync`, `audit`, `pushed`,
the release plan, cut and edits, `release members` without a `REF`) pulls it first, and says so in
its problems when it cannot pull it, or cannot join it.
When two machines both sync after one merge, the second does not record it again; only two that
each commit it before either pulls keep both.

On such a fork, a branch's claim is a ref on the ledger's repository, so it holds on every machine
that shares the ledger and runs a knives that reads claim refs. `start` and `finish` pull no
ledger before they decide; they read the claim, and change it in one push with the entry that
records the change. When they cannot read it or push the change, they exit `3`: `start` records no
claim and opens no workspace, and `finish` leaves the state file and the workspace as they were.
The `using-knives` skill's ledger reference has the setup, and
[docs/shared-claims.md](docs/shared-claims.md) describes the claim refs.

An older knives kept branch statements in `state.json` and filed each fork's ledger under its
registry key. This one refuses such a state file or ledger, naming `knives ledger migrate`, which
moves both once: every statement onto the ledger, and every directory, `state.json` key and
workspace sighting to the fork's upstream name. A second run changes nothing.

`knives status` carries the newest human note for each branch, preferring it over a newer machine
event. Its compact notch cell prefixes a disposition, if any, and appends the count of entries it
masks, so the question "what is this weird branch" is usually answered before you ask it.

## Install

### Prerequisites

- `git`, `curl`, `tar`.
- `gh`, the GitHub CLI, logged in (`gh auth login`). Every command that reads the forge goes
  through it, and every call names its repository: a fork's pull requests are read from its
  upstream's repository, or from its origin's when the upstream is a local path. gh takes none
  from the checkout's remotes, so a `gh repo set-default` there changes no report.
  Install: <https://github.com/cli/cli#installation>.
- `jj` 0.45.1 with jj-vcs/jj#9941. knives embeds `jj-lib` 0.45.1 (pinned in `Cargo.toml`), and
  the installed `jj` must agree with it about the on-disk repository format. Upstream 0.45.1 lacks
  #9941 (upstream ships it from 0.46.0), so install the 0.45.1 build that carries it:

  ```
  t=v0.45.1-sami.20260910-043938
  curl -fsSL "https://github.com/sjawhar/jj/releases/download/$t/jj-$t-x86_64-unknown-linux-musl.tar.gz" \
    | tar xzf - ./jj
  install -Dm755 jj ~/.local/bin/jj
  ```

  (On macOS or ARM Linux, take the matching asset from the
  [same release](https://github.com/sjawhar/jj/releases/tag/v0.45.1-sami.20260910-043938).)

knives reads a checkout's identity through git, so a jj workspace must carry a `.git` file beside
its `.jj`: work in a colocated jj repository (`jj git init --colocate`, or `jj git clone --colocate`).
`jj workspace add` then writes the `.git` file for each new workspace, because `git.colocate` is
`true` by default (jj-vcs/jj#9941; `--colocate` forces it, `--no-colocate` opts out).

### The binary

Release archives are built for Linux x86_64 only; elsewhere, build from source (below).

Download the latest release archive and unpack it under `~/.local/opt`:

```
v=$(curl -fsSL https://api.github.com/repos/sjawhar/knives/releases/latest | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p')
curl -fsSLO "https://github.com/sjawhar/knives/releases/download/$v/knives-$v-linux-x86_64.tar.gz"
mkdir -p ~/.local/opt && tar xzf "knives-$v-linux-x86_64.tar.gz" -C ~/.local/opt
install -Dm755 ~/.local/opt/knives-$v-linux-x86_64/bin/knives ~/.local/bin/knives
```

Make sure `~/.local/bin` is on your `PATH` (for bash: `echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc`,
then open a new shell), and check with `knives --version`.

Or with [mise](https://mise.jdx.dev), using the `v` resolved in the first line above:

```
mise use -g "github:sjawhar/knives@$v"
```

A bare `github:sjawhar/knives` can resolve to an older release, because mise skips releases younger
than `minimum_release_age` (24 hours by default, or the value you configure); add `minimum_release_age_excludes = ["github:sjawhar/knives"]` under
`[settings]` to take the newest.

### Plugins

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

`<prefix>` is the unpacked archive: `$HOME/.local/opt/knives-<tag>-linux-x86_64` for the download
above (write the absolute path), or `$(mise where github:sjawhar/knives)` with mise. It changes with
each release, so update `opencode.json` when you upgrade.

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
ledger = "company/knives-ledger"      # optional: the repository this fork's ledger is shared through

[repos.tool]
upstream = "https://forge.example/org/tool"
origin = "https://forge.example/ours/tool"
release_branch = "integration"        # optional: fixed release branch instead of dated cuts
workspaces = "~/.worktrees/tool"      # optional: where `knives start` opens branch workspaces

[repos.prover]
upstream = "https://forge.example/org/prover"
origin = "https://forge.example/ours/prover"
base = "v9.3"                         # the default series: cuts release/YYYY-MM-DD[.N|-<label>]

[repos.prover.series."9.2"]             # optional: a second release series, cuts release/9.2/YYYY-MM-DD[.N|-<label>]
base = "v9.2+ours"    #   its own base; `knives release --series 9.2 ...` acts on it
base_remote = "origin"                #   optional: the remote holding the base (defaults to upstream)

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
A checkout whose `origin` is an entry's but whose `upstream` matches no entry (the upstream was
renamed in `repos.toml` or in the checkout, not both) does not bind: `status`, `repos`, a named
verb and the session hook each name the fork, both spellings and the fix, `git remote set-url
upstream` or the `repos.toml` edit, rather than reading the fork as absent.

`knives start` opens a branch's workspace beside the checkout, named for the branch: the
`<name>/default` layout, where the workspaces are `default`'s siblings. A checkout at `~/<name>`
has no room for siblings, so its entry sets `workspaces` to the directory they go in instead.
`~` expands and a relative value is taken from the config directory, so write it as `~/…`. A
`workspaces` inside the checkout is refused by `knives start` and `finish` whenever they would use
it. The setting places new workspaces only: an existing one is found at the path jj recorded for
it, so changing `workspaces` leaves the live ones where they are, and `start`, `finish` and the
possession check still find them there.

`consumers` records forge slugs, not checkout paths. Knives reads supported pin files from each
consumer repository's trunk and caches that scan by the trunk commit. If the forge is unavailable,
cached results are labeled as such and the command is incomplete; pass `--consumer PATH` for an
ad-hoc local scan without recording the path.

`forbidden` lists identifiers an upstream-bound diff must not add: your org, your product, your
hosts. `knives audit` reports every line a branch adds over its fork point with the upstream trunk
that contains one; a branch stated `--fork-only` is exempt. Absent, no scan runs. The `using-knives`
skill has the matching and load rules.

Values you do not want in a shared registry (internal names for `forbidden`, private consumer
slugs) go in an optional `repos.local.toml` beside `repos.toml`. It holds only `[repos.<name>]`
tables, each with `forbidden` and/or `consumers`, for entries `repos.toml` already has; its lists
are appended to that entry's and checked as one, so a term in both files is a duplicate. Any other
key or table, or an entry `repos.toml` lacks, is refused with the overlay named:

```toml
[repos.libcore]
forbidden = ["acme-corp"]
consumers = ["an-org/a-consumer"]
```

`ledger` names, as `<owner>/<name>`, the repository a fork's ledger belongs to: the one that
carries its entries between machines. A repository name may start with a dot
(`someone/.knives-ledger`). Without it the fork's ledger and its claims stay on each machine that
writes them. With it, the fork's claims are held on that repository too, for every machine that
shares it. A
fork whose `upstream` is a filesystem path names no repository another machine could share, so
`ledger` is refused on it. knives 4.0.1 and older refuse an entry key that they do not know, so
an older knives refuses a `repos.toml` that sets a key it predates, such as `ledger`.
A newer knives ignores an unknown key of a `[repos.<name>]` or series table, and names it in one
line on stderr:
`knives: warning: <registry path>: [repos.<name>] has an unknown key <key>; it is ignored`
(a series table is named `[repos.<name>.series.<series>]`). Every other table refuses an unknown
key. Upgrade knives on every machine that reads the file before adding a new key.

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
| `knives audit [REPO] [--all] [--no-github]` | reconcile remote refs, open pull heads, recorded cuts and edits, and anonymous heads, with one row of facts per maintained branch (the `using-knives` skill has the fields); reports only and never repairs |
| `knives status` | the main report |
| `knives pr NUMBER [--repo REPO] [--timeline]` | one pull request's live state; `--timeline` adds its bounded forge event log |
| `knives sync` | fetch, then classify what happened to each tracked pull request |
| `knives preflight` | the facts to check before contributing upstream |
| `knives start` | take a branch and get your own workspace: on its tip, or on the release's shared base for a new one; waits for the claim lock, and a refusal names the holder (the `using-knives` skill has the lock's mechanism, wait and messages); states the fork's `immutable_heads()` (trunk and tags, the trunk named on every knives remote, the release branch where it is published, and every tag fetched from upstream, so a hand command cannot rewrite a published release) in the repo's jj config where none is stated, and refreshes the one it wrote earlier. A release name's start is the release claim, the write lease for the release's refs: its workspace rests on a new empty commit off the root commit, not on a release commit, and its line says so. On a fork whose `ledger` is set, the claim is held on that ledger's repository, for every machine that shares it, and a refusal names the machine that holds it; when `start` cannot read or write that claim, it records no claim, opens no workspace and exits `3` |
| `knives finish` | hand a branch back so another agent can pick it up; its bookmark and any open pull request survive. It snapshots the workspace into jj first and names the commit that keeps it, and refuses while another process on this machine works in the workspace or when the claim was taken on another host or agent box. When jj cannot snapshot the working copy whole it hands the claim back all the same, keeps the workspace exactly as it is, and exits `3`, and the next `knives start` of that branch adopts that directory. The ledger records its `--why`, with or without `--force`. The finish of a release claim, once jj holds no workspace of the claim (it forgot it, or there was none), abandons the earlier commits of the release that nothing holds, in one jj operation, and names each one that something other than the release holds, with what holds it; while the release holds conflicts, it abandons none and says so. On a fork whose `ledger` is set, it releases the claim on that ledger's repository; when it cannot read or write that claim, it leaves the state file and the workspace as they were and exits `3` |
| `knives track` | state which upstream pull request a branch belongs to, when inference cannot find it; when the upstream and the fork's own repository are two forge repositories, the upstream is asked, and a number it does not have is refused |
| `knives depends BRANCH [--on REQ]... [--forget REQ]...` | record that a branch cannot land before a pull request on its fork's upstream or a member branch, and say which in the record; `REPO#N` is a pull request on that fork's upstream, with no forge call; a pull request on a fork's own repository is refused, since pull requests go upstream, and a record of one an older knives wrote is still read, reported by `status` and taken back by `--forget` as it is spelled; `--forget` takes a requirement back, with `--on` corrects one, takes nothing back (exit 3) when the forge cannot say whether more member records are meant, and every machine running this knives that shares the ledger reads the same list |
| `knives notch [SUBJECT] [-m TEXT] [--disposition TOKEN]` | read the ledger or write a human note; `-m` must hold text, and an empty or whitespace-only one is a usage error (exit `2`) that writes nothing; dispositions require evidence, `--dispositions` reads terminal rulings, and `--verify` re-checks selected entries |
| `knives ledger sweep` | commit, pull and push the shared ledger; every write sends its own entries before it exits, so a sweep sends what a write could not, or pulls |
| `knives ledger migrate` | move what an older knives left onto what this one reads: the branch statements `state.json` kept, one statement entry each, and every fork's ledger directory, state keys and workspace sightings from its registry key to its upstream name; a second run changes nothing |
| `knives release` | plan a release, edit its membership, cut one, record one cut or finished by hand, or reap superseded cuts |
| `knives release cut NAME --at REV` | record a release cut outside knives as NAME's recorded cut; no commit is made. REV must name one commit with no conflict that shares history with the trunk, NAME must sort after every other release of the series (record the older name first), and every ref of NAME must name REV; a NAME no ref holds gets a local bookmark at REV; a published NAME the publish remote does not hold is refused with the commands that push it and record it again. A parent the trunk holds is never a member, REV must merge a member, and a REV that merges the series' previous release is refused. The commit is audited as a cut is, its drift measured from the release it follows, and it must carry each member of the previous cut's event and of the newest write (cut, edit or `adopt`) of the newest release the ledger records in the series, and the fix by hand of each `adopt` of any release of the series, file by file, at each file where the file's reference, the newest recorded write that holds the file with no conflict, still holds the adopt's content: REV's own tree must carry the adopt's change there, by a replay; a write that leaves a fixed file conflicted neither ends its hold nor is its reference, and a deletion is content, so a REV that holds a file the fix deleted lacks the fix; an older adopt event that names its files in its text alone holds each path of up to 16 pieces that its text may name and the adopt's own tree holds; an adopt whose commit this clone does not hold holds nothing, and the command names it; a clone that does not hold the newest write is refused and told to fetch it, and a REV that lacks a file whose reference walk stops at a write this clone does not hold is told to fetch that write where origin or the publish remote has a branch or tag at it, and else to keep the new content with `adopt`; `--allow-drop DROP` lets REV go without the one drop it names (below). On a fixed release branch, `knives release cut --at REV` takes no NAME and records REV, a commit the publish remote holds, as the branch's cut (below) |
| `knives release adopt [NAME] [--why TEXT]` | record the commit a release name holds now, finished by hand or re-signed, as that name's newest knives write; it reports what the commit changed against the write it replaces and how many of each member's added lines it no longer holds, and asks for `--why` for a change outside that write's conflicts; a `--why` must hold a reason, and one that is empty or only whitespace is a usage error (exit `2`) |
| `knives release members [REF] [--verify] [--carries REV] [--census] [--no-github]` | list a release's direct member parents, their holders and advances; `--verify` reads each member's own lines in the release and names each line it lacks; `--carries REV` asks whether REV's content is carried — by REF, or by every live release and upstream trunk; `--census` asks that of every maintained branch, conditionally checks superseded releases, and reports qualified orphans |
| `knives register [DIR]` | print the registry entry for a checkout, or `already registered as <name>`; writes nothing |
| `knives hook` | harness plumbing, not for humans |
| `knives gh` | fork-aware `gh` passthrough |

When the environment indicates an agent is running a command (or stdout is not a terminal),
the report is emitted as [TOON](https://github.com/toon-format/toon) — the same structure as
JSON at fewer tokens. `--json` forces JSON exactly; `--text` forces prose. The two machine
encodings are lossless renderings of one report.

Exit codes: `0` nothing to report, `1` findings, `2` usage, `3` something could not be
answered, or a write did not reach the ledger its fork shares.

### Timing

Set `KNIVES_TIMING=1` to write one `timing gh <repo> <duration>ms: <argv summary>` line to
standard error for every forge call. Timing is diagnostic output and does not change the report.

## GitHub CLI passthrough

`knives gh -- <args...>` absorbs the fork-routing logic of the `gh` bash shim:

* **Shim re-entry**: invoked with the `knives-gh-shim` first on PATH and no `KNIVES_GH_SHIM_DEPTH` marker, `knives gh` hands the call to that shim verbatim and does nothing else. The shim applies its agent-session rules (no keyring login, the App routing include) and re-enters knives with the depth marker and `KNIVES_REAL_GH` set, so a direct `knives gh` never runs gh on the user's own login where `gh` would not. The depth marker decides, not `KNIVES_REAL_GH`: a hand-set override with the shim on PATH still re-enters it, so the override is not a way around the shim.
* **Target resolution**: the repository of a `gh pr`, `gh issue` or `gh agent-task view` selector URL (`gh pr view https://<host>/<owner>/<repo>/pull/<n>`, `.../issues/<n>`; gh acts on that repository whatever `-R` says, so it comes first), `-R` passthrough, the repository a `gh repo view|clone|fork|edit|delete|archive|unarchive|sync|set-default` call names (`gh repo view <owner>/<repo>`, also after `--`; `gh repo fork --org <org>` routes by the organization it creates the fork in), `GH_REPO`, `gh repo set-default` markers, remote preference, and `gh api` owner extraction. A target whose owner is no GitHub login routes nothing, and no later source stands in for it: `gh api repos/a:b/x` and `-R a:b/x` act on owner `a:b` (gh reads `-R` and `GH_REPO` as a URL or as `[HOST/]OWNER/REPO`, never as `host:owner/repo`), so knives mints no token, says so on stderr, and gh runs on its own auth.
* **Node-id GraphQL calls**: a `gh api graphql` call in which knives finds no owner (no `repository(owner:)` or `organization(login:)` in the query or its fields) and no `GH_REPO`, such as a mutation that takes only a node id (`enqueuePullRequest`, `archivePullRequest`), is routed by the checkout. A GitHub node id holds at most its repository's database id, never its owner, so knives cannot know whether the checkout's owner is the node's. GitHub answers a node of another owner with `Could not resolve to a node with the global id`, as it does a node that does not exist. For such a call knives copies gh's stderr through as it arrives, and when gh fails with that answer, adds one line after it: the owner knives ran the call as, the checkout that chose it, and `GH_REPO=<owner>/<repo>` as the fix (`gh api` takes no `-R`). stdout and the exit status are gh's.
* **Token export**: queries git credential config for `gh-app-token`, asks it over the credential-helper protocol, and exports `GH_TOKEN` for the child process. The helper's stderr is relayed; when it exits non-zero, answers `quit=1`, or answers something that is not UTF-8, `knives gh` exits with that code (1 for the last two) instead of running gh. Only an exit-0 answer with no password leaves gh on its own auth. gh holds that token for the whole call, so knives tells the helper, through `GH_APP_TOKEN_MIN_LIFETIME`, how many seconds it must stay valid: a fresh installation token's full hour for a call that watches (`gh run watch`, `gh pr checks --watch`, `gh agent-task create|view --follow`), half an hour for any other. The helper mints afresh rather than hand out a cached token with less life left.
* **Watch restarts**: `gh run watch` and `gh pr checks --watch` on a token knives minted run for 55 minutes at most, since gh cannot take a new token mid-call. Still watching then, gh is ended (SIGTERM), the terminal is taken out of gh's alternate screen, a fresh token is minted, one stderr line says so, and the same command runs again; the last run's exit status is the call's, so `--exit-status` and `pr checks`' exit 8 keep their meaning. `gh agent-task … --follow` is never restarted: gh runs agent-task commands only on a device-flow OAuth login, never an App token, and a rerun `create` would submit a second task. Neither is a watch on an inherited `GH_TOKEN`. A positive `KNIVES_GH_WATCH_RESTART_SECS` overrides the 55 minutes (for tests).
* **Detached HEAD compensation**: inside a jj checkout, where git's HEAD is detached, `gh pr` subcommands get the active jj bookmark injected as their target when the call names none. Those `gh pr` calls run gh with a `git` wrapper first on its PATH that answers HEAD reads (`symbolic-ref HEAD`, `rev-parse --abbrev-ref HEAD`, `branch --show-current`) with that bookmark, or `__jj_detached__` when the working copy has none; every other git invocation goes to the next `git` past the wrapper on PATH, and that git runs with PATH cut to the entries after the wrapper, so a shim below it that scans PATH for the real git cannot pick the wrapper and exec back into it.

The `--` delimiter is required. All arguments after `--` are passed to `gh` verbatim.

The routing table stays in gitconfig (`gh-resolved` markers, credential helpers). Knives reads it, does not own it.

Escape hatches:

* `KNIVES_GH_BYPASS` on the shim bypasses `knives gh` entirely; a direct `knives gh` reaches the shim first, so the bypass holds there too.
* `KNIVES_REAL_GH` points `knives gh` at a specific real `gh` binary on the shim's inner pass. A value that is itself a marker-bearing shim is ignored in favor of the PATH scan — a poisoned override must never re-enter the shim — and setting it does not skip the shim re-entry above.

## Release workflow

A release is a flat octopus merge of feature and fix branches, and its parent set is its membership: a branch is in the release exactly when the release has its parent. The upstream base is never a direct parent — every member forks from it, so it is reachable through each of them, and there is no role to classify. Membership changes only through stated edits. `knives release include` adds one parent, `knives release drop` removes one (and, when no remaining member's history carries the dropped parent, says once the release is written whether it lost that content or kept exactly the content it had), and `knives release advance` moves member parents to their branches' current tips — refusing outright, rather than silently deduping, if the same branch would replace more than one parent, and accepting `--from <old-sha>` to name one branch's old parent directly when a `jj duplicate` rebuild has left it with no ancestry back to the commit it replaces. A drop requires `--why`, the reason recorded on the release; it must hold a reason, and one that is empty or only whitespace is a usage error (exit `2`). `knives release rebase` moves the composition onto a newer upstream commit, the equivalent of `jj rebase -b <release> -d <target>`. Each member's commits that the target's history does not hold move, with their bookmarks. A member that the target's history holds does not move. Bare, it targets the first upstream trunk commit that contains every merged pull request, then drops the members whose landed branches carry nothing more (`--no-drop` keeps them); with nothing merged it asks for a commit. A flat release has no base parent, so the target reaches it only through a parent on the target: a member, or a carrier (an earlier write of the release). The drop does not keep the carriers, so a carrier on the target does not count. So where no member that stays is on the target, the drop keeps the landed members that are, and says why. Where none is, it drops nothing and exits `3`. When the move leaves the rebased tip of a member conflicted, the rebase names that member, by its branch or by its short commit id, with its conflicted files, and exits `1`. The member stays in the release: resolve it on its branch. The report that ends the command names it: the rebase's own, or the landed drop's when the drop writes. That report says `no conflicts in this release` only when neither the release nor a moved member holds a conflict. A conflicted release with no conflicted member still exits `0`. A landed drop that refuses exits `3`, also when a moved member is conflicted. A rebase that moves nothing, because the release already contains the target, names no member. Each edit duplicates the release onto the changed parent set, and keeps a resolution only where the same conflict comes back, as git rerere does. Where the plain merge of the release's own parents, its carriers left out (what `jj new` of them holds), leaves a conflict in a file, the release's own content there is its resolution, whether or not an adopt recorded it. The edit keeps that resolution where the plain merge of the new parents leaves the same conflict there. Two other kinds of file keep what the duplicate writes in them. One is a `CHANGELOG.md` or `RELEASE_NOTES.md` whose conflict the release-log union joins: it is left to the union. The other is a file whose every conflict hunk the release still leaves open, with the sides it had. Elsewhere the edit writes the file as that plain merge, with or without a conflict, and names the file where that changed it: `<repo>: <path>: written as the plain merge of the new parents (the resolution answers another conflict), with no conflict` (or `which leaves a conflict`). Where the release had also changed lines outside that conflict, a second line says the hand change was not carried. The edit still exits `0`. So a drop or an advance of the member whose version a resolution took keeps the other members' lines, and a conflict that changed is left for a person to resolve and adopt. Before an edit writes anything, it checks what it would write. For each remaining member, it reads each line of the member's own change at the line's place in three versions of the file: the written one, the release in hand, and the plain merge of the new parents. It reads no line of a member that the trunk holds on its first-parent line where no other parent gives that member a base (below). A conflict counts as text there, with its markers. A line counts as lost when the written file lacks it away from any conflict, the release in hand holds it or holds it at a conflict (or the edit brings it in), and the plain merge holds it or holds it at a conflict. Then the edit writes nothing and exits `3`. It prints `<repo>: refusing to write <release>: <path> would lose <member>'s lines; nothing written`, each lost line, and the remedy: run the edit again with `--allow-loss <reason>`, put the named lines back by hand, and record that with `knives release adopt <release> --why <reason>`. A line at a conflict that the edit leaves is not counted: that conflict is reported for a person, as before. `include`, `drop` and `advance` take `--allow-loss <reason>`. An empty or whitespace-only reason is a usage error (exit `2`). With it, the edit writes what the check refused, prints `<repo>: <path>: <member>'s lines lost by this write (--allow-loss)` with each line, and exits `0`. Its ledger event records `; lost: <path> (<member>: <n> line(s))` for each path and member, then `; why lost: <reason>`, after the delta. Where nothing counts, the flag changes nothing. After the check's lines, for each member that the trunk holds on its first-parent line, the edit names the base that it read the member from, or says that it read no line of it. A conflicted release log can hold `## [Unreleased]` more than once, and then its `[Unreleased]` section holds no line. An entry that the union joined into another list can then read as lost although the file still holds it, so check the named lines before you put any back. `knives audit` does not read `; lost:`. `knives release cut` names a new cut of the composition in hand, verbatim: nothing joins, nothing advances, and a branch created since the last release enters through `include`, never by existing. Only the first cut, with no composition to carry, starts from every branch.

Every release write resolves one kind of conflict itself. Where a `CHANGELOG.md` or `RELEASE_NOTES.md`, in any directory, conflicts only because each side inserted its own block at the same place, the write keeps what each side inserted, with the blocks the release already had below the blocks the write brings in. At most places it writes each block whole, byte for byte. In a file with one `## [Unreleased]` heading, inside that section, where every block is plain `### ` lists, it joins lists: for each `### ` heading, the lists of the last block that has the heading stay as that block wrote them, and every other block's lists of that heading join its first one, each entry line byte for byte. So the heading is written once, unless that last block holds it more than once. Where a place is not plain lists in a file's one `[Unreleased]` section, the write prints a line that names each `### ` heading that two of its blocks leave in one section there, for you to join by hand; that line does not name a heading that one block holds more than once. It names each such file with the first line of each block, and its ledger event lists the files. knives never renumbers, so two blocks that claim one version stay as their members wrote them. A conflict in which a side changed or removed lines another side kept, every conflict in another file, a renumber and a repair of the composition stay a person's. So does a side's inserted block with no final newline that something else would follow. Written verbatim, its last line would run into the next line, and the write never adds a byte to keep them apart. Such a block is unioned only where it ends the file. The content checks (the audits of `release cut`, its composition gate and `members --verify`) read a member by its own lines (below). A non-blank line that a member added to a `CHANGELOG.md` or `RELEASE_NOTES.md` is held wherever the line diff puts it, when the release's file has exactly one `## [Unreleased]` section and that section holds the same line, trailing whitespace aside. So an entry that the union joined into another list still reads as carried. A line that only a released section holds is not held this way. In a file with no `## [Unreleased]` heading, or with more than one, a line is read as in any other file.

After every release write (`cut`, `cut --at`, `include`, `drop`, `advance`, `rebase`) and every `adopt` that records a tree, knives reads each `CHANGELOG.md` and `RELEASE_NOTES.md` in the release's tree, whether the command changed it or not. It skips a file that is still conflicted and a file with no single `## [Unreleased]` heading. For each `### ` heading that the `[Unreleased]` section holds more than once, it prints one line, once for each file and heading: ``CHANGELOG.md: [Unreleased] holds `### Added` twice; knives changed no line for it; join them by hand before you push``. The line says `twice` for a heading held twice and `N times` for one held more often. A command prints these lines once, for the release its last write left. When a step after a write fails, the command still prints them for that write, then reports the failure. It also names a heading that the left-twice line names inside that one `[Unreleased]` section. knives changes no byte for it. A clean merge can leave such a heading: a member's own new `### Added` block above the release's `### Added` list merges with no conflict. Join the lists by hand, then record the release with `knives release adopt --why`.

A person finishes the rest on the release commit itself, then runs `knives release adopt [NAME] [--why TEXT]`. It records the commit the name holds now as the name's newest knives write: one `edited` ledger event, which `audit` and every reader of edit events already read. It matches the commit to the newest recorded write whose parents equal the commit's parents, carriers left out on both sides, each parent matching as the recorded commit or a visible rewrite of it. It measures the commit against that write rebuilt on the commit's member parents (the write itself when the commit equals an older recorded write), so a member rewritten in place is no change of the commit's own; where this repository does not hold the write or its event carries the `; unchecked:` mark, it measures against a fresh merge of the member parents with the release-log union. It reports the files resolved by hand; the files changed beyond the baseline's conflicts, which need `--why`; and, for each member and changed file, how many of the lines the member added since its fork point the commit no longer holds. It reads a member that the upstream trunk contains from where the trunk took it in (below). A `--why` must hold a reason: one that is empty or only whitespace is a usage error (exit `2`), refused before anything is read or written. It records first, then exits `1` only when it measured against a recorded write, never against a fresh merge, and found a member's added line lost from a file the baseline left without a conflict that is not a release log, or none of a member's blocks left in a release log or a conflicted file; a block cut down there is reported and exits `0`. It refuses (exit `3`, nothing written) a commit that still has conflicts, one a workspace has checked out, a parent set no recorded write matches, and a hand edit of a published release that the publish remote does not hold. For that last one it prints the published remedy: the edit is redone under a new dated name, and the old name is forgotten, never deleted (the `using-knives` skill's releases reference has the steps, and the new cut's expected exit `1`).
Where the commit changed a file, the `adopt` event also carries `fix_files`: one `{path, change}` record for each file of its fix by hand, each path whole. `change` is `resolved` for a file resolved by hand, or `beyond` for a file changed beyond the baseline's conflicts. The composition gate reads that list before the event's text. An adopt event that an older knives wrote has none.
`adopt` refuses a name the ledger records no cut or edit of, such as a release cut outside knives: it has no write to replace. `knives release cut NAME --at <commit>` records such a release first (below), or a knives write over it does.

Until that record exists, the release in hand is unrecorded: its tree differs from the tree of its matched recorded write rebased onto the release's parents, carriers left out. While it is, `cut`, `include`, `drop`, `advance` and `rebase` refuse (exit `3`), write nothing, and name `knives release adopt <name>`, which asks for `--why` only for a change outside the conflicts. Where knives cannot check the release in hand — this repository does not hold the matched write, no recorded write matches the release's parents, or the ledger records no write of its name — the write goes ahead. It prints one line that says knives could not check the release, why, that knives did not fetch, and what records it, and its ledger event ends with `; unchecked: <reason>`: `W absent`, `no match` or `no record`. A cut still runs its composition gate, which refuses while this repository does not hold the newest recorded write of the series. `audit` notes such a write. A release that is a marked write, or a jj rebase or metadata rewrite of it, stays unchecked: a write over it goes ahead and carries the mark on until `adopt` records the release. A release that carries a change by hand on top of a marked write that this repository holds, such as a conflict resolved by hand, is unrecorded: every write refuses it until `adopt` records it. A release whose every parent is an earlier write of it has no member to compose, so every write refuses it, with a remedy by state. A release that equals any recorded write of its name, or a copy of one with the same change id, parents and tree, is recorded, or unchecked where that write is marked. A release whose name has no recorded write at all is never refused, unless its every parent is an earlier write of it, so a fork whose ledger predates knives is not blocked; a write over it is marked `; unchecked: no record`, and `adopt` records the name that write wrote.

A release re-signed or metaedited after a knives write, by `jj describe`, `jj sign`, `jj metaedit` or a push that signs, keeps that write's change, parents and tree under a new commit id. Run `adopt` on the machine that re-signed it, after the last rewrite, while that write is unpublished. It records the new commit, the one that is pushed, in one event that holds no fix by hand, and it says which commit it recorded. Another machine on the same ledger then reads the release as recorded once it has fetched it. Without that `adopt`, the ledger names only the old commit, which no other machine holds: there `audit` reports `could not compare` and `release-drift`, and the next cut is refused. That `adopt` changes no content, so a tag on the new commit, or a publish remote that does not hold it yet, does not refuse it. Conflicts that the knives write left do not refuse it either: they are that write's own. It still refuses a commit that a workspace has checked out. The write is published when the publish remote holds the recorded write (its old commit id), a tag is on that commit, or a `record: published` note names that commit, for every release scheme, a fixed release too. Then the `adopt` prints `nothing to adopt`, exits `0` and writes no event: recording the new commit would make every other machine wait for a push that moves a published name. Where the newest event of the name is marked `; unchecked:`, `adopt` reads the commit, the write itself or a new copy of it, as any release knives could not check: it checks it as any hand edit, so it may refuse it (with the published remedy, say), and otherwise it measures it and records it, which ends the mark. Otherwise an `adopt` of the newest recorded write itself writes nothing and prints `nothing to adopt`.

A push made a fast-forward of the release's published position — the release rebased onto its own parents and `<release>@<publish remote>`, its tree then put back with `jj restore` — gives the release an earlier write of itself as one more parent: a carrier. A carrier is any parent the ledger records as a cut or edit of the name, the publish remote's commit of it, a visible rewrite of either, a commit whose change id a recorded cut or edit event of the name names (a pushed copy keeps its write's change id), or a parent a recorded event of the name lists as `<name> earlier write (carrier, not a member)`, a label that only `adopt`'s record of the fast-forward parent it took writes; it is never a member. `knives release adopt` accepts it, the plan and `members` label it `<name> earlier write (carrier, not a member)` and count it as no stacked member, and every write that composes from the release's parents (`include`, `drop`, `advance`, `cut`, `rebase`) leaves it out: the release is first written again on its other parents with its own tree, so whatever it resolved over the carrier becomes its own change and carries forward. Each push adds one carrier and the next write leaves it out, so carriers never pile up. The fork runs `knives release adopt` after the fast-forward rebase and before the push, so the ledger's newest write is the commit the push publishes, and pushes that commit with a plain `git push`, never `jj git push`, which moves the remote branch whenever it is still where jj last fetched it, a sideways or backward move included. A write then hides, in the same operation, each head it leaves that nothing holds (no bookmark, working copy, tag, or remote ref other than the name's own tracked ref on its publish remote) when that head is an earlier write of the name or a commit whose newer version is still visible, and names each one; so a carrier a write leaves out stays hidden behind its publish ref. Any other head it leaves, such as a member with no bookmark, stays visible for audit to report. While the written release carries conflicts it hides nothing.
Without that `adopt`, nothing records the pushed commit: another machine's next edit over it is marked `; unchecked: W absent`, and its next cut is refused until it holds the newest recorded write. A push that git refuses after `adopt` leaves a recorded write that the fork lacks, and `audit` reports `release-drift` for it until the next write or `adopt`.

One branch is the release member, and the upstream pull request head unless the two need
different bases; there is never a second copy in the release lineage. New branches fork from the release's shared base (`knives start` puts them there,
or on the fetched upstream trunk when no release exists) and are linear past it, so a new branch
composes into the release without forcing a rebase; an existing branch is continued from its tip,
and a name that exists only on upstream is still a new branch here. Moving the composition to a newer trunk is
`knives release rebase`, a decision of its own. When a branch is rebased onto a newer trunk,
`advance` still recognises it as the same member by change id, which `jj rebase` keeps. A
branch that does not compose into the release means the release is behind: move the release,
never mint a "release-lineage" sibling of the branch on an older base. A member whose own
history carries a release merge (one a release ref names, or one the ledger records knives
writing), or a merge bringing in a line the rest of the release lacks, is reported as
`stacked-history`, and the plan stops calling the cut `flat`. A merge whose side parents the
release already carries brings nothing but the member's own line: `include` takes such a
branch beside the members it merges, a named `advance` takes one that continues a member
(the one the last event recorded under its name, keeping the others it merged), and both
name the merge.

`knives release members --verify` reads each member's own lines in the release, against the
release it replaced (in a dated series the newest of our releases named before it, never a later
one; on a fixed branch the branch's release in hand, for a commit with no recorded write): a line
that release also lacked, of a member it holds, is carried from it, and the member's row says so.
A member built on a release, a landing among them, starts at that release, so it answers for
everything its first-parent line holds over it. A member whose every lost line a member built on
it changed again (a line it added that the later member rewrote, or one it removed that the later
member put back: the commit the two share holds the member's version, and the later member's tip
does not) reads unresolved: the row names those members and the lines, and the exit is `1`. A
line that only the release, or a member not built on it, changed stays missing. A text file the
member changed that the release no longer holds at its path reads missing (`absent from the
release`): no other file is read in its place, so a move another member made is the member's
review to judge. A release whose commit is a hand edit on top of its cut is verified through the
cut's members, with a note that says so.
`knives release members --carries REVISION` compares the revision with every live release and the
upstream trunk. `knives release members TARGET --carries REVISION` asks exactly one target.
`knives release members --census` asks that of every maintained branch.

Against a release, `--verify`, the audits of `release cut` and its composition gate read a
member by its own lines, and so do `--carries` and `--census` for a tip that the release reaches
(below). Its own change is the diff from its fork point with the
upstream trunk to its tip. So the upstream commits after that fork point are not charged to it,
however far the trunk has moved, and members that fork at different trunk commits are each
measured from their own fork point.

A revision that the upstream trunk contains is its own fork point with the trunk, so its change
is read from another base. A release that reaches the revision carries it. Otherwise the base
depends on how the trunk took the revision in:

* Upstream merged it by a merge commit, also through another branch: the base is its fork point
  with the first parent of that merge.
* Its tip is on the trunk's first-parent line, as after a fast-forward or a direct push: the
  history does not record where it began. The base is the newest commit of that line below the
  tip that a parent of the release holds. That base charges the revision with each trunk commit
  between the base and the commit where the revision began.

The composition gate does not read a recorded member that the trunk contains and the release
does not reach. It counts that member as dropped. The check of `include`, `drop` and `advance`
and the measure of `knives release adopt` read a member that the trunk contains from these bases
too, although the release holds it as a parent. On the trunk's first-parent line its base comes
from the other parents: the newest commit of that line below the member's tip that another parent
holds, where that parent does not hold the tip. The output names that base. Where no other parent
gives a base, the check and the measure read no line of the member, refuse nothing for it, and
say so.

The change is read with no rename detection: a rename is the deletion of the old path and the
addition of the new one. The release carries the member when all of these hold:

* The release holds each non-blank line that the member added.
* The release does not hold again, at the place where the member removed them, all the non-blank
  lines of a hunk that the member removed. Nor does it hold again there any line that the member
  removed and that holds a letter or a digit, whatever it holds of the lines around it. A line of
  punctuation alone (`}`, `},`, `);`) counts only with its whole hunk: another change's new code at
  the same place ends in such lines too.
* Where the release changed a line that the member added or changed, it holds the line by its
  words. It holds at least one of the line's words and every word that the member's change added
  to it. It does not hold again, at that line, all the words that the member's change removed from
  it. A line that the member changed only in whitespace is held only where the line diff keeps it
  byte for byte.
* A text file that the member adds with no non-blank line is held where the release has a file at
  that path. A file that the member deletes is held where the release has no file there. The
  executable bit of a file that the member adds, or whose bit it changes, is held where the
  release's file has the tip's bit.
* A path that is not text (a binary file, a symlink or a submodule) is held where the release holds
  there what the member's tip holds. A text file that the release holds as a binary file is lost
  whole.
* A line that the member added to a `CHANGELOG.md` or `RELEASE_NOTES.md` is also held by the
  release-log rule (above).

An edit of another change next to the member's lines, or on the same line, that keeps the member's
own words does not make the member read missing, unless it puts back what the member removed. A
range of more than 30000 words on either side is not followed by its words: its lines count as
lost, and the output says that the word step did not run there. To find the line that an added
line replaced, knives pairs the lines that the member's hunk removes with the lines it adds. It
pairs only in a hunk where the word step follows a range. It pairs no line of more than 30000
words: the line rule alone reads such a line. Each command names each line that a member lacks,
with its path. It also names each line that the release holds only by its words, with the
release's lines that hold them, so that you see the words that another change put around the
member's own. Such a line does not make the member read missing and changes no exit status.
Each command names these lines in its text. `members --verify` also names them in its JSON
report.

A path where the release, or the member's tip, holds an unresolved conflict cannot be read. Nor can
a file that the member adds, where the release holds another version of it, blank lines aside, and
the reading lost no line of it: the member's change from the trunk holds the file whole, so it
cannot show a line that the member's own history removed from it, as for a member stacked on a
branch that adds the file. A person reads such a file; an edit of it by another change, or a hand
resolution of it, needs that look too. A cut's audit carries such a file forward where the release
before it held the same change. A member with
such a path reads inconclusive, where the member lacks nothing else. A member with such a path and
a lost line elsewhere reads missing. A conflict in another path of the release does not change the
reading.

`--verify` prints one row for each member: `<member>: carried (replay empty)` for a member that
the release carries, `<member>: content check inconclusive (an unresolved conflict, or a file it
adds that the release changed; read the path named below, then re-check)` for an inconclusive one,
and `!! <member>: the cut tree is missing or diverges from
the member's content` for a missing one, with each line that it lacks below the row. A missing
member is a finding (exit `1`). With `--json`, `audit.members` holds each member's reading:
`reading`, `lost`, `held_by_words` and `word_step_skipped`.

`--carries` and `--census` read a tip that a release reaches by its own lines too, from the same
base and through the same reader as `--verify`. Against the upstream trunk, which knives does not
compose, an ancestor tip is `carried-exact`. Against a release, an ancestor tip is
`carried-exact` where the release holds every line that the tip's change adds, does not hold
again a hunk that it removes, holds no line of it only by its words, and holds no unresolved
conflict in a path of it, nor does the tip. Lines that other members added or edited next to the
tip's lines change none of its lines, so they do not matter. Otherwise it is `merged-then-edited`
(exit `3`): a person judges it, with the release's commits since that changed its files; no
`knives depends` record makes it carried.

The two commands differ in two places, where `--verify` reports and passes and `--carries` leaves
the judgment to a person. A line that the release changed, so that it holds the line only by its
words, is a changed line: `--verify` reads the member carried (exit `0`) and names the line, and
`--carries` reads `merged-then-edited` (exit `3`). A path with an unresolved conflict, in the
release or at the tip, reads inconclusive in `--verify`, which exits `0` where the member lacks
nothing else, and `merged-then-edited` in `--carries`. `--carries` and `--census` name no line;
`--verify` does. What the own-lines reading does not read, it does not read for `--carries`
either, so both read these as carried: blank lines that the release dropped; and a removed hunk
that the release writes back edited, or of which it puts back only lines of punctuation alone
(`}`, `},`) while other lines of the hunk stay removed.

A tip that the target does not reach is read by a replay of its change instead. Against either
target it is `carried-rewritten` when its net change replays empty, else `carried-rebased` when a
rewrite that stands for it (a successor in jj's operation log, or a commit with its tree) is an
ancestor and, against a release, the release holds every line of the rewrite's own change too, as
above, else `conflicted` or `NOT carried`.

Before cutting, the orphan gate looks for commits that only the previous release or its descendants reach. These keep a commit: a local bookmark, a bookmark of `origin` or of the publish remote (tracked or not), each target of such a bookmark that is conflicted, a parent of the previous release, and the upstream trunk. A bookmark with a release name keeps nothing. On a fork with a fixed release branch, a release name is that branch's name alone. On any other fork, it is a name that starts with `release/`. A bookmark of `upstream` other than the upstream trunk, or of any other remote, keeps nothing either, because its owner can delete it. When a commit that nothing keeps would be stranded, the cut refuses (exit `3`) and lists each such commit ID. Pass `--allow-drop <commit>` for each of them when dropping it is intentional. The published remedy lists the commits that a cut would drop through the same keepers.

`--allow-drop DROP` lets a cut or record go without the one drop that DROP names, and only that one; repeat it for each. DROP is a commit, by at least its first 12 characters (a member the gate names, a member whose line a fixed branch's published commit lacks, a commit the orphan gate names, or the newest recorded write where this repository does not hold it), or a file of a fix by hand, by its path for every adopt that lacks it there or as `ADOPT:PATH` for one adopt. A drop that no DROP names still refuses, and a DROP that names no drop refuses the write (exit `3`, nothing written). Each refusal ends with `  name each drop: --allow-drop …`, giving the flag of every drop it lists.

On a fixed release branch whose composition in hand is the publish remote's commit, `cut` records that commit and never makes another: a cut of it would make its tree on its parents, the branch's earlier writes left out, and the event lists each of those as a carrier, not a member. With a previous release, a composition in hand that is neither the publish remote's commit nor a write the ledger records (one made by hand on it, on a rewrite of it, or afresh) is refused (exit `3`, nothing written), with the push before the cut: a cut of it would make a commit of its own, which the remote cannot take as a fast-forward, the published commit and its members left out. A write knives recorded and not pushed yet, such as an `include`'s, is cut as before, and that cut's event keeps the recorded members it does not merge, as a record's does. `knives release cut --at REV`, with no NAME, records REV the same way: REV must be the publish remote's commit of the branch or in its history, and not the newest recorded write or in its history. So releases published one on the other and never recorded are recorded in order, the oldest first, each by its own commit. A parent of the recorded commit that is an earlier write of the branch (a write the ledger records, a rewrite of one, a parent a recorded event labels a carrier, or the publish remote's commit of the branch) is a carrier, and every other parent is a member. knives reads no role from a parent's place, its descent or a member's bookmarks. Where no parent is an earlier write and a parent holds one, that parent may be a release published on that write and never recorded, or a member built on it, and nothing in the ledger says which: the cut refuses (exit `3`, nothing written), names each such parent and the write it holds, and says to record the release it was made on first with `--at`. The record reads each member's own lines against the recorded write the commit follows, never against the commit itself: a line the commit lacks is carried forward only where that write recorded the member and lacked the line too. A member whose line the commit lacks is otherwise missing, and the cut is refused (exit `3`, nothing written), naming the lines and giving `--allow-drop` with the member's commit; with it, the commit is recorded and the event ends `; without the lines of <n> member(s) that it lacks: <member>@<commit>`. The drift is measured from the commit's first carrier (else the followed write), so a change by hand that no member explains is named. The event also keeps, as recorded members, each member of the previous cut and of the newest recorded write that the commit does not merge, whether it carries it through a carrier or a rewrite, or `--allow-drop` waived a line of one its history holds (`; keeps <n> recorded member(s) it does not merge: …`), so a later release that loses one of their remaining lines is refused. A cut over a composition in hand that no knives write or adopt measured reads its members against the newest recorded write in the same way; where this clone does not hold that write, it carries no member's lost line forward, so `--allow-drop` naming the absent write never lets a member's lost line through. When the ledger's newest write of the branch is a commit the publish remote never held and nothing keeps (abandoned), the cut is refused and names it; `knives release cut --corrects <commit>` (with `--at REV` to record REV) then withdraws that write with a `withdrew <branch>'s recorded write …` event and records the commit in its place, and no reader takes the withdrawn write for a write of the branch again. The cut prints that event's line, which names each member of the withdrawn write that the recorded commit does not carry. `--corrects` refuses a write that is still visible: it is not abandoned, and is still to be pushed. It also refuses a write the publish remote holds, itself or a rewrite of it, at its commit or in its history: that write was published.
A visible newest write that `--corrects` refuses is kept by moving the branch to it only where that leaves nothing the publish remote holds behind; otherwise the refusal says to abandon it and run `--corrects` again, which records the published commit, then the fix-forward below for what the write held.

Before the content audit runs, a cut with the same tree and the same parents as the previous cut on the publish remote is refused: a new name would ship nothing and only ask consumers to re-pin. Two exceptions: a dated release every consumer pins by revision, and a published dated release (a tag names its commit, or a `record: published …` note on it names that commit). The edit verbs refuse both — editing in place would reach nobody, or move what was published — and say to cut a new dated release, so a verbatim cut under a name that sorts after it is allowed and says why, and the edits then apply to the new name. A fixed release branch (which has no other name to take) stays refused. Before either comparison, a dated name that does not sort after the newest release is refused outright: the reap after a dated cut keeps only the newest name, so such a cut would be created and taken straight back in the same command. Each series has one order. A name is `release/[<series>/]YYYY-MM-DD[.N|-<label>]`, where `.N` is one or more digits (never `.N.M`), a `<label>` holds no `/`, and git can hold the whole name as a ref. It sorts by its date, then by the rest of the name in version order: a run of digits compares as a number, any other run as bytes, and a shorter name sorts before a longer one that starts with it. So `release/9.2/2026-11-15-speed-8.0.9` sorts before `…-speed-8.0.10`, and that before `…-speed-8.1` and `…-speed-8.1.1`, and a `.N` name sorts after each `-<label>` name of its date, as `-` sorts before `.`. A later name on one date supersedes an earlier one. A name the series cannot sort, such as `release/9.2/foo`, is a usage error (exit `2`) that gives the grammar; it is never the newest release, and the reap never takes it. A member rewritten with the same content is a new parent, so a cut of it is a new composition. A previous cut not yet pushed has no consumer to protect and is not compared.

A second pre-publish gate holds the candidate against two ledger events: the previous cut's event, and the newest recorded write (its cut, its newest edit or an `adopt`) of the newest release that the ledger records in the series. That release is the release in hand, unless this machine has not fetched a newer cut that the ledger records. They are the only records of a parent set that survive the release bookmark moving, since every edit relocates the name and abandons the commit it replaced unless something else rests on it. This repository must hold the newest write: a repository that does not hold it cannot tell what it carries, so the cut or record is refused (exit `3`), writes nothing, and names that write, its release and its commit, with the fetch that brings it and, for when the fetch does not bring it (a push that git refused after `adopt` recorded the write), `knives release adopt <release>`, which records the commit this repository holds as the newest write; it never falls back to holding less unless `--allow-drop` names that write's commit. A member that either event records counts once, so a member that `include` added after the last cut is held, and a member that `drop` removed after it is restated; a member of the previous cut whose change id a member of the newest write has is superseded by that member, as `advance` recorded it. A recorded member whose content the candidate does not carry (not a parent and not reached by the candidate's base, no rewrite that stands for it — a successor of it in jj's operation log, or a commit with its tree — that is a parent or that the base reaches, and the candidate lacks a line of its own, read from its fork point with the upstream trunk as above) refuses the cut and is named with the event that records it. The refusal then lists each line that such a member lacks, under `lines each member lacks:`. With `--allow-drop` naming the member, the cut lists the same lines after the lines that state the drop. A recorded member that is conflicted in one of its own paths, in the candidate or at the member's tip, and that lacks nothing else, is named as inconclusive and refuses nothing. An older version of the member that only shares its change id carries nothing for it; a member recreated outside jj, or reworded, with an identical tree is named as carried by its twin where that twin is a parent, and a member that landed upstream and entered the candidate through its base passes without comment. The gate holds each fix by hand that an `adopt` of any release of the series recorded, before or after that release's cut, file by file, each file against its reference: the newest recorded write of the series that holds the file with no conflict. A knives write that leaves a fixed file with conflict markers is no reference: it neither ends the hold on that file nor holds what a subject must hold there, so a record after an `advance` that left the fix conflicted must still carry the fix. A knives cut over that conflicted write goes ahead (exit `1`), names the fix as inconclusive at the file, and leaves the conflict to be resolved in place and adopted; a cut that leaves the file conflicted where the newest write holds no conflict there, as from a bookmark left at an older write, is refused and told to move the release to the newest write. The files of a fix are those of its adopt event's `fix_files` list and those that the adopt's change against its baseline changes; an adopt event that an older knives wrote names its files in its text alone, where a path that holds `, `, `; ` or ` (why: ` reads back in pieces, so only such a path that the adopt's own tree holds is a file of the fix. An adopt whose commit this repository does not hold holds nothing: nothing here can tell what it changed, so it counts neither for nor against the subject, the cut or record names it (`the fix by hand of <release>'s adopt <commit> holds nothing here: this repository does not hold <commit>, …`) and goes on, and the event records it as `; not held against <n> adopt(s) this repository does not hold: <commits>`. A deletion is content: where the fix deleted a file, the file's absence is the fix. The hold on a file lasts while its reference holds the adopt's content there: the adopt's change at that file replays empty onto the reference, so the hold on a file that the fix deleted ends only where the reference holds the file again. So a fix adopted on an earlier release and carried by each later release stays held; a later write that removes the fix from a file, or changes the fix's lines there again, ends the hold on that file alone, and the other files of the fix stay held; and `--allow-drop` naming a file lets a release go without the fix there. The candidate carries a held file when the adopt's change at that file replays empty onto it, so a member that changes other lines of a file whose lines the fix changed costs nothing (a file that the fix added has no other lines, and its replay onto other content there conflicts); a candidate that holds a file the fix deleted lacks the fix. Only the candidate's own tree counts, as REV's tree does for a record: an adopt on its first-parent line carries nothing for it, since its own merge can undo the fix. A fix is no member's content, so the audit's drift check leaves each file that the gate holds to the gate, and only those. A candidate that lacks a fix at a file is refused once, by the gate, naming the adopt, its release, its commit and each file it lacks. The refusal restores a member of the previous cut with `include`; for a member that only the newest write holds, for a fix, or for a newest write this repository does not hold, it says to fetch, then move the release to that write, naming its commit; and for a fix that names its files, to keep the new content of its files, it says to edit the newest write's release by hand to hold that content and run `knives release adopt <release>`. `--allow-drop` naming each drop states the drops are intended, and the new cut's ledger event then records exactly which members were dropped, a member that only the newest write held named with that write, a newest write this repository does not hold as `; not held against <release>'s newest recorded write (a cut) <commit>, which this repository does not hold`, and each fix it went without as `; without the fix by hand of <release>'s adopt <commit>, in <n> file(s): <files>`.
A refusal never says to move a release behind what its publish remote holds. Where `<release>@<publish remote>` holds commits that the newest write lacks (it is neither that write, nor in its history, nor a recorded write of the release or a rewrite of one, as a commit published outside knives on that write or beside it is), the refusal says, in place of the move, to record that commit as it is first: re-run with `--allow-drop` naming each drop it lists, so the ledger names what the published commit lacks. Then the fix-forward: `<release>` made a merge of that commit and a branch that carries what was dropped (`jj new <commit> <branch> -m '<release>, fixed forward on <commit>'`, `jj bookmark set <release> -r @`, then `jj new 'trunk()'`), pushed, then the cut again, which records that merge with the published commit, now the newest recorded write, as its carrier and the branch as a member. Nothing is merged into a commit the ledger has not recorded, and the refusal names no edit by hand, which would rewrite the published commit. Where this repository does not hold the newest write, it cannot tell whether that write holds the published commit, so the refusal says to fetch, then cut again.
A refusal gives the move to the newest write, where it names one, as `jj bookmark set <release> -r <commit> --allow-backwards`, and names a fetch from origin or the publish remote, where it has a branch or tag at the commit, as `fetch <remote>'s <ref>, which holds <commit>`.

`knives release cut` audits a candidate merge built in a scratch transaction that is never committed. The audit reads each member by its own lines in the candidate, each member from its own fork point with the upstream trunk (above). So it finds a member's line that an auto-merge or a lockfile resolution lost, and it names each such line, with its path, under the member's row. It also checks for unexplained diff drift against the previous release. A line that the candidate lacks is carried forward when the release in hand lacks it too. A member whose every lost line is carried forward reads as carried forward, not missing, and a member with any other lost line reads missing. A member reads inconclusive only where the candidate, or the member's tip, holds an unresolved conflict in one of its paths, and the member lacks nothing else that the release in hand does not lack too. Divergence the release in hand already carried is reported, never refused — the audit charges a cut only with divergence it introduces — and each such row says how the release in hand stands to its recorded writes. Over a recorded write, the divergence is a resolution knives wrote or `adopt` recorded, and the row says it is carried forward: the cut refuses an unrecorded release in hand before it audits, whatever the audit would say, so no cut carries an unmeasured hand edit of a recorded release forward under a new name. Over a release knives could not check, the cut says so, its carried rows name `knives release adopt`, and its event carries the `; unchecked:` mark. Over a release whose name has no recorded write at all, nothing recorded or measured the divergence, and `adopt` refuses such a release, so each row reads `the ledger records no write of the release in hand, so nothing measured its divergence from this member; check that divergence by hand`; the cut is still written, with the exit it would have without the rows, says knives could not check the release in hand, and its event carries the `; unchecked: no record` mark until `adopt` records the new name. A failed audit writes nothing at all — the candidate simply evaporates — and a passing one publishes the creation and naming of the release as a single operation, after verifying the published tree is identical to the audited one.

`knives release reap` cleans up superseded dated release bookmarks by forgetting their refs locally and across tracking remotes before abandoning their merge commits. Reaping also runs automatically after every successful dated cut. Superseded means "not the newest name of the series in its one order", so a later name on one date retires an earlier one, and a labelled name is read like any other. A superseded cut that something rests on — a workspace's working copy on it or above it, empty or not, or any other descendant, on the cut, on a commit an edit wrote under its name, or on a rewrite of either — is kept with its refs, the reason named, and the reap exits with findings: abandoning it would rebase what rests on it onto the merge's parents, rewriting a working copy from outside its workspace, which can leave the workspace stale, into a fresh, possibly conflicted merge. It is the rule an edit applies to the commit it replaces. A superseded name that a live release claim holds (`knives start <name>`, as this machine's state file records it) is kept too, with its refs, and the reap exits with findings: its line ends ``held by the release claim of <holder>; `knives finish <name>` releases it``. When the reap cannot read that state file, it keeps every superseded name and says why, because a name it reaped then could be one that a claim holds. One keep has no finding: a superseded name kept only for what that name published, where every descendant of its commits is reachable from the name's own ref on the publish remote or from a tag, and nothing else names one of them. That is the old name after the published remedy; the reap prints its `kept` line and exits `0` (the `using-knives` skill's releases reference has the rule). A later reap retires the cut once nothing rests on it: jj drops an empty, undescribed working copy when it moves on, and one with changes stays a descendant until it is rebased off the cut. A superseded cut that a tag, an untracked remote bookmark or another local bookmark (a `keep/…` retention name) still names has its refs forgotten and its commit kept. While the live cut still carries unresolved conflicts, every superseded cut is kept: the previous release is the only record of how those conflicts were last resolved, and an abandon-and-recut needs it. Reaping never modifies remote repositories; subsequent fetches may re-materialize forgotten remote refs as untracked bookmarks, which is harmless and cleared by the next reap. With `--json`, or for an agent, the standalone reap prints one document instead of prose.

An edit follows consumer pin state exactly as a rebase does: when every pin of that release is frozen on a revision, editing it in place would reach nobody, so the edit refuses and says to cut a new dated release; a consumer frozen on an older release is not reached either way and does not block the edit. A pin is of the release when it names the branch or when it freezes the exact commit the publish remote holds for it — a lockfile written from an installed commit (`?rev=<sha>`) never names the release, and read by name alone it counted as no pin at all. An edit or a rebase refuses a published dated release the same way, naming the tag or `record: published` note that marks it. A rebase leaves every other pushed or published release name on its commit; one never pushed follows its rewrite. An edit also refuses when the upstream trunk cannot be resolved, because it is the trunk that separates the release's base parents from its members.

When releases publish to a separate remote, `knives audit` holds each release ref on origin against the publish remote's ref of that name, both read live. Where both name the same commit, origin's ref is a copy, and a note says so: `origin has release ref <name> at <id>, a copy of <publish-remote>'s <name> at the same commit`. Where the publish remote has no such name, or has it at another commit (a re-signed copy too), origin's ref is misplaced: a `remote-drift` finding on `<name>@origin` whose detail ends `this ref is misplaced`.

### Recording a release cut outside knives: `knives release cut NAME --at REV`

`knives release cut NAME --at REV` records REV, a release that a person or a script composed outside knives, as NAME's recorded cut. It makes no commit, uses no temporary name and makes no throwaway cut. Then `adopt`, the edit verbs and `audit` read the release as recorded, on another machine after its next ledger pull. A hand change on a release that knives wrote is `adopt`'s to record, not this command's.

**Record the older name first.**

Its gates come in this order. Each refusal writes nothing and exits `3`, unless the gate says otherwise:

1. NAME is a dated cut of the series, in the grammar above, that git can hold as a ref. Else the command is a usage error (exit `2`). `--at` on a fixed release branch is a usage error too: a plain `cut` records that branch's published composition.
2. The plan has no problems, and a shared ledger pulls.
3. REV names one commit. That commit has no conflict, is no workspace's working copy, and shares history with the trunk: the root commit, and a commit made on the root apart from the trunk, are refused.
4. When the ledger records a write of NAME and its newest write is REV, the command says that NAME at REV `is already recorded`, writes nothing, and exits with the plan's exit (`0` when the plan is clean). When the newest write is another commit, the refusal names that commit: a commit made over a recorded write is recorded with `knives release adopt`.
5. NAME sorts after every other release name of the series, here, on the publish remote and in the ledger. The refusal names the newer name and says to record each release in order, the older name first.
6. Every ref of NAME, here and on the publish remote, names REV. With no ref, the record makes the local bookmark NAME at REV, after every check.
7. A published NAME (a tag or a `record: published` note names REV) is on the publish remote. REV only waits for its push, so the refusal gives the commands that make the bookmark where no ref exists, push NAME to the publish remote and run the record again. That is not the published remedy of a hand edit.

Each command that the record prints holds NAME as one shell word, single-quoted when NAME has a character outside `A-Za-z0-9._/+-`.

Then:

* **Members.** A parent the trunk holds is never a member. The members are REV's parents, carriers left out. REV is refused when it merges no member, when it merges the series' previous release, and when a member carries a merge past the trunk. For the previous release, the refusal ends `its parent <commit> is <name>, the series' previous release, which is never a member; a release must be a flat octopus merge of the trunk and its member branches: merge the members, not the previous release`. For a member that carries a merge past the trunk, one line names each such member and the merge it carries, and the refusal ends `a member above stacks on an earlier merge; a release must be a flat octopus merge of the trunk and its member branches: rebase each member above off the trunk`.
* **Audit.** REV must hold each member's own lines (above), each member measured from its own fork point with the upstream trunk. Each parent that the trunk holds is read the same way, measured from the fork point of all the members and the trunk. Nothing is carried forward from an earlier release, so a member whose own line REV lacks is missing, and the output names that line. A member whose every lost line another member changed too reads unresolved, with its lines, and the record goes ahead with exit `1`. A file that changed since the release REV follows, and that no member or trunk explains, refuses REV. That release is the newest other release of the series. The series' first record follows none, and every later record measures from it, so the check measures its files from its members' fork point with the trunk.
* **Composition gate.** It holds REV as it holds a cut's candidate (above), against the same two events: the previous cut's event, and the newest recorded write (its cut, its newest edit or an `adopt`) of the newest release that the ledger records in the series. REV's own tree must carry each member that either event records, so a member that `include` added after that release's cut is held too, and each fix by hand that the gate holds. The refusal names that release and each member REV does not carry, then the previous cut and each member that only it recorded. `--allow-drop` naming each drop states it and lists the lines that each dropped member lacks, and the event records it, naming each event that REV was held against as what it is.
* **Record.** One cut event, with `; recorded: composed outside knives` in its text and no `unchecked` mark. The command prints `recorded NAME at <commit> as its cut; nothing was committed`, and the push command when the publish remote does not hold NAME. It exits on the record's own result: a finding of the release it supersedes, such as a stale parent, does not change that exit. On a fork that shares its ledger, the command sends the event before it exits, as every write does. No orphan gate, reap, signing or push of the release runs.
* **Lock.** Like every release write, the command holds the fork's lock from before it reads the ledger until its last write. So records of one name that run at once on one machine take turns and write one event. A release write waits for the lock up to one minute, with pauses from 20 ms doubling to 2 s, with jitter; when the wait runs out it prints `another knives command (pid <pid>, holding for <n>s) is holding <lock file>; try again in a moment` (or `holder unknown, lock written <n>s ago` in place of `pid <pid>, holding for <n>s` where the lock file names no pid), writes nothing and exits `3`. The lock is beside the state file, so it orders the writes of one machine only.
* **Audit note.** On every machine, after its next ledger pull, `knives audit <repo>` shows no `has no recorded cut or edit event` note for NAME.

```bash
# series 9.2: a script composed 8.0.9, then 8.1, each a flat octopus merge: record the older name first
knives release --series 9.2 cut release/9.2/2026-11-15-speed-8.0.9 --at <commit of 8.0.9>
knives release --series 9.2 cut release/9.2/2026-11-17-speed-8.1 --at <commit of 8.1>
knives release --series 9.2 members release/9.2/2026-11-17-speed-8.1 --verify
```

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

The OpenCode hook, which the oh-my-pi extension (`omp/`) also drives, leaves out any instruction
file whose text the session already holds: one the session's system prompt carries, and the same
text injected earlier in the session from any checkout, until compaction clears that record. Only
oh-my-pi shows the hook its system prompt, so only there is the session's own `AGENTS.md` never
injected a second time. Parallel tool calls claim guidance under one lock, so one of them injects
it.

oh-my-pi also shows the hook which of the guidance blocks and claims notices it injected are still
in the tool results the model sees. omp's shake and its pruning of superseded reads rewrite those
results in place, and a compaction summarizes them away; none of these reliably reaches the hook as
an event. A block counts as held while the context shows it. Once it does not, its guidance is due
again, once, on the next call that touches that repository, and so is its fork's whole roster. A
block from a call of the current turn counts as held until the turn is over, since the context
does not show that turn's results yet. A session still running the oh-my-pi extension of knives
6.1.6 or earlier reports no separate list of claims notices (before 6.1.5, guidance blocks alone):
there a roster the session was told stands, as it did before 6.1.5, until a compaction or a later
extension's report.

The boundary that made the gap is a security control, so both adapters re-establish an equivalent
one rather than removing it. The `[trust]` rules are the allowlist: only a repository they name —
by identity, owner, or directory — contributes guidance, and a fork entry alone contributes the
managed notice and nothing more. Containment is checked by path components rather than string
prefix, symlinks are resolved first, and the `AGENTS.md` files from the touched file's directory
up to the repository root are injected nearest first, while `CONTRIBUTING.md` is mentioned rather
than injected. Guidance arrives wrapped in a per-injection nonce and framed as data, so a
repository whose files you are reading cannot forge an instruction to you.

Five skills ship with both adapters: `fork-work` for what to check before touching a fork,
`using-knives` for the CLI, `pr-preflight` for contributing upstream, `maintaining-fork-release`
for maintaining release members and open pull requests through release/consumer verification, and
`maintaining-fork-pr` for owning one PR end-to-end. Both read inherited decisions with `knives notch`
before acting and use the same ledger for new evidence; no separate inventory document is required.

## What it does not do

It does not create pull requests. That is `gh pr create`.

It does not replace jj. General version control stays where it is.

It does not keep machines in step on its own. Through a git repository you name, it shares two
things: what agents recorded (the ledger), and the claims of each fork that names that repository.
Nothing else travels. Sightings, pull request records and the claims of a fork that names no
ledger stay on the machine that made them.

It does not judge. Anything of the form "have you read the contributing guide and does this
change comply" is a question for a person or an agent, not a CLI, and the tool does not pretend
otherwise.

## Status

Early. It is used daily against about ten forks, and the checks it reports have each been
verified against real repositories rather than only against fixtures. Interfaces may still
move.
