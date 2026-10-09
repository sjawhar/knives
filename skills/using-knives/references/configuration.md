# Registry, checkout identity and plugin

Read this for repository binding, `knives register`, scan failures or registry,
trust and plugin options. No knives command edits `repos.toml`.

## The three remotes

- `upstream`: repository we contribute to, only through a PR.
- `origin`: our fork, holding ordinary pushed branches and PR heads.
- `release`: optional distinct publication location for internal consumption.
  If absent or explicitly equal to origin, the publish remote is `origin`.

Remote roles are not interchangeable. Reports can compare local release state
with the live publish remote; origin's matching release ref does not prove a
separate release remote has it.

## `knives register [DIR]`

Prints a paste-ready `[repos.<name>]` TOML entry to stdout, instructions to stderr.
Default DIR is the current directory; any subdirectory or `start` workspace of a
checkout works. If its upstream is already registered, prints `already registered
as <name>` and exits `0`: one upstream cannot identify two entries.

The caller/human pastes the snippet into `repos.toml`, replacing any existing
section of that name rather than appending a duplicate. Registry edits take
effect at the next hook event/tool call; no daemon or service restart is needed.

Requires the `upstream` and `origin` remotes; `release` is optional. A missing
required remote refusal names available remotes. A plain Git clone with no jj
store is refused (the hook binds those; fork verbs do not), as is non-colocated
jj (`.jj` without `.git`). An untracked remote resembling another fork of the
same upstream, compared case-insensitively by host/owner/slug, produces a warning:
origin must identify our own fork.

## Registry fields

`~/.config/knives/repos.toml` names repository identities and trust rules, not
checkout paths. `workspaces` and trust roots have their own path meanings.

```toml
[repos.libcore]
upstream = "https://forge.example/org/libcore"
origin = "https://forge.example/ours/libcore"
base = "main"
release = "https://forge.example/company/libcore"
consumers = ["company/workbench"]
forbidden = ["acme-corp", "internal.example"]
ledger = "company/knives-ledger"

[repos.tool]
upstream = "https://forge.example/org/tool"
origin = "https://forge.example/ours/tool"
release_branch = "integration"
workspaces = "~/.worktrees/tool"

[trust]
repos = ["company/workbench"]
owners = ["ours", "company"]
roots = ["~/projects/company"]
```

`upstream` and `origin` are required. Two entries cannot share upstream, and a
`path` field is refused. Optional fields:

- `base`: upstream trunk used for branching, trunk probes and PR targets;
  defaults to `main`. Upstreams using another trunk, such as `dev`, state it here.
- `release`: distinct publish URL; defaults to origin.
- `release_branch`: fixed release name instead of dated cuts. Cannot be empty,
  equal to base, or under the `release/` prefix.
- `consumers`: forge `owner/repo` slugs whose trunks pin releases. Cached by
  consumer commit; ad-hoc local paths belong in `--consumer PATH`, not this list.
- `workspaces`: parent directory for `start` workspaces and `finish` cleanup.
  Default is beside the checkout, fitting `<name>/default` with sibling
  workspaces. Set it for a checkout at `~/<name>` to avoid putting branches
  directly in home. `~` expands; relative values resolve from the config
  directory, so write `~/…`. `start` and `finish` refuse a value inside checkout.
- `forbidden`: identifiers an upstream-bound diff must not add, such as our
  organization, product or hosts. Audit scans case-insensitive substrings on
  added lines since the upstream fork point. Fork-only branches are exempt.
  Blank terms or case-insensitive duplicates are refused with the entry named.
  Absent/empty means no scan and no `forbidden` field. Audit reports hits, never
  blocks on them; no other command reads the list.
- `ledger`: the `<owner>/<name>` of the repository this fork's ledger belongs
  to, through which its entries travel between machines (the ledger reference
  has the setup). Absent, the fork's ledger is not shared. Letter case and a
  `.git` suffix do not matter. Refused when it is not `<owner>/<name>`, or when
  the fork's `upstream` is a filesystem path. An older knives refuses a file
  that sets it, as every entry refuses a field it does not know: upgrade every
  machine that reads the file before adding it.

## How a checkout is found

The entry is identified by its upstream matching the checkout's upstream remote;
case, trailing slash and `.git` suffix do not matter. Standing inside it or a
`start` workspace binds it. Origin/release mismatches do not prevent binding;
status and repos note `origin remote is <X>; registry says <Y>` (or release).

From elsewhere, naming a repo and sweeps scan HOME to depth three. `repos`
**always** scans, including when invoked inside a checkout outside that scope.
Two matching checkouts are refused with both paths; an absent one is not guessed.

The scan:

- Selects a `.git` directory beside a real `.jj` directory.
- Skips dot-prefixed directories, follows no symlinks and does not descend below
  a `.jj`. A plain Git repository is not a managed checkout and does not hide
  forks beneath it.
- Does not select workspaces with a `.git` file; they bind when you stand inside.
- Passes over non-colocated `.jj` without `.git` in silence. A fork verb run
  inside one refuses, explaining that knives reads a checkout through Git.

A deeper-than-three or outside-HOME checkout binds when you stand inside, for
every command except scan-only `repos`. HOME must be set: otherwise usage exit
`2`, `HOME is not set; knives scans $HOME for checkouts`, never a scan of `/`.

Knives needs colocated jj and the jj build named by the fleet's tool config,
matching the embedded jj-lib. Workspace `.git` files come from jj's colocated
Git-worktree support (after 0.45.0, jj-vcs/jj#9941). `jj workspace add` supplies
one by default from a colocated workspace with `git.colocate` true; `--colocate`
forces it. This manual does not authorize changing shared tool versions.

### Missing, ambiguous and unreadable scan results

`repos` lists every registry entry: unplaced ones have no release state;
ambiguous/unreadable candidates are problems. An unreadable checkout's remotes
are reported while an entry remains unplaced because it may be that checkout.
Once every entry is placed, that candidate error is dropped. A directory the
scan could not list is always reported.

A named repo's refusal appends `; could not read: <what>` for scan errors.
`repos` uses `?` problem lines; `status --all`, `sync --all` and `audit --all`
print `could not read: <what>` once on stderr in every output format.

A sweep omits a **definitely missing** entry, printing
`knives: <name>: not on this machine` to stderr, and exits according to entries
it did inspect. It does not promise a document row per registry entry.
Ambiguous entries stay as problem rows. If scan errors could conceal a missing
checkout, that entry also stays as a problem row; it is not treated as known
absence. Status prefixes such a row's problem with `could not gather:`.

## Trust is guidance injection, not fork access

A managed fork entry grants no trust. To inject its instructions, add a trust
rule independently:

- `repos`: forge slugs; any checkout remote naming that repository matches.
  Non-slug values are refused.
- `owners`: forge organization/user names; any checkout remote under one matches.
- `roots`: directory subtrees containing trusted repositories.

**Security boundary:** repo/owner matches are based on self-declared remote URLs,
not forge-authenticated identity. A checkout declaring a trusted remote matches.
Prefer roots when uncertain. Knives reads only the candidate checkout's local
Git config, not user/system config or environment overrides such as `GIT_DIR`,
`GIT_WORK_TREE` and any `GIT_CONFIG_*`.

Identity starts at the nearest `.git`, a marker Git refuses to deliver as cloned
content. A `.jj` of any shape can be committed as content, so `.jj` without `.git`
is not a repository identity; `.jj` beneath `.git` is that repository's content
and receives its verdict. A nested repository cannot borrow an enclosing
checkout's identity; a cloned tree has its own `.git` and remotes. Identity
resolution never runs jj, so it cannot write into another checkout.
A checkout with no remotes matches only via roots.

Trust permits **guidance-as-data injection only**, never fork-command access.

## OpenCode plugin

Ships alongside the CLI. Once per repository per session, when a call first
names a file in a registered repository, the plugin announces that the fork is
managed/shared and names claims. It appends trusted repository instructions as
**data**; a fork entry alone brings the notice, not guidance. An instruction file whose text the
session already holds is left out: one its system prompt carries (under oh-my-pi, whose adapter
passes the prompt along, that includes the session's own `AGENTS.md`), or the same text injected
earlier in the session from any checkout, until compaction. Under oh-my-pi, "held" also means
still in the model's context: when shake, pruning or compaction removes an injected block, the
next call touching that repository injects it again, once. It supplies `KNIVES_OWNER` to shell
environments.

The entry in `opencode.json` has three options, all defaulting on:

```jsonc
"plugin": [
  ["file://{env:HOME}/knives/default/plugin/knives.ts",
   { "notice": true, "guidance": true, "owner": true }]
]
```
