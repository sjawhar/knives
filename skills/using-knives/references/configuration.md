# Registry, checkout identity and plugin

For repository binding, `knives register`, scan failures, and registry, trust or plugin options. No knives command edits `repos.toml`.

## The three remotes

- `upstream` takes our work only through a PR.
- `origin` is our fork, with ordinary branches and PR heads.
- `release` is an optional separate publish location. When it is absent or equal to origin, publish to `origin`.

Roles do not substitute. A release ref on origin does not prove that a separate release remote has it.

## `knives register [DIR]`

Prints a `[repos.<name>]` TOML entry to stdout, with instructions on stderr. DIR defaults to the current directory. A subdirectory or `start` workspace also works. An upstream that is already registered prints `already registered as <name>`, exit `0`.

- Paste the entry into `repos.toml`. Replace any section of that name, and never add a duplicate. It takes effect at the next hook event. Nothing needs a restart.
- It needs the `upstream` and `origin` remotes. The refusal names the remotes it found.
- It refuses a plain Git clone with no jj store (the hook binds those, fork verbs do not). It also refuses non-colocated jj (`.jj` without `.git`).
- An untracked remote that looks like another fork of the same upstream (host, owner, slug, any case) gives a warning. Origin must be our own fork.

## Registry fields

`~/.config/knives/repos.toml` holds repository identities and trust rules, not checkout paths. Only `workspaces` and trust `roots` name paths, each with its own meaning.

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

[repos.prover]
upstream = "https://forge.example/org/prover"
origin = "https://forge.example/ours/prover"
base = "v9.3"

[repos.prover.series."9.2"]
base = "v9.2+ours"
base_remote = "origin"

[trust]
repos = ["company/workbench"]
owners = ["ours", "company"]
roots = ["~/projects/company"]
```

`upstream` and `origin` are required. knives refuses these:

- two entries with one upstream, or one upstream `<owner>/<name>` on two forges. The fork's ledger, claims and statements are kept under that name, which has no host.
- an owner or name that is empty, `.`, `..` or `.git`, or that holds `\` or a control character.
- a `path` field.

| Field | Meaning |
|---|---|
| `base` | Upstream trunk for branching, trunk probes and PR targets. Default `main`. Set it for another trunk, such as `dev`. |
| `release` | Separate publish URL. Default: origin. |
| `release_branch` | Fixed release name instead of dated cuts. Not empty, not `base`, not under `release/`. |
| `consumers` | Forge `owner/repo` slugs whose trunks pin releases, cached by consumer commit. Local paths go in `--consumer PATH`. |
| `workspaces` | Parent directory of `start` workspaces and of `finish` cleanup. Default: beside the checkout, which fits `<name>/default`. Set it for a checkout at `~/<name>`. `~` expands. A relative value resolves from the config directory, so write `~/…`. `start` and `finish` refuse a value inside the checkout. |
| `forbidden` | Terms that an upstream-bound diff must not add (our org, product, hosts). Audit scans added lines since the upstream fork point for case-insensitive substrings. Fork-only branches are exempt. Blank terms and case-insensitive duplicates are refused, with the entry named. Absent or empty: no scan and no field. Audit reports hits and never blocks. Nothing else reads it. |
| `ledger` | `<owner>/<name>` of the repository the fork's ledger travels through. The name can start with a dot (`someone/.knives-ledger`), the owner cannot. Case and `.git` do not matter. knives keeps it lowercase without the suffix, as `knives repos` shows. Absent: the ledger stays on each machine. Refused when it is not `<owner>/<name>`, or when `upstream` is a filesystem path. Setup: [sharing](ledger.md#sharing-between-machines). |

Every entry refuses unknown fields. Thus an older knives refuses a file that sets `ledger`. Upgrade knives on every machine that reads the file before you add it.

## How a checkout is found

- An entry binds to a checkout whose `upstream` remote matches it. Case, a trailing slash and `.git` do not matter. Standing in the checkout or a `start` workspace binds it.
- An origin or release mismatch still binds. Status and repos note `origin remote is <X>; registry says <Y>` (or the same for release).
- When origin matches an entry but upstream matches none, nothing binds. One side was renamed, and you choose which spelling is right. `status` inside the checkout shows the entry row with the problem. `repos`, `status --all` and a named repo show it in place of `not on this machine`. The hook puts it in context in place of the claims notice. Each names the checkout, the entry, both upstream spellings and the two fixes: `git -C <checkout> remote set-url upstream <registry's>`, or `upstream = "<checkout's>"` in `repos.toml`.
- From elsewhere, a named repo and sweeps scan HOME to depth three. `repos` **always** scans, even inside a checkout outside that scope. Two matches are refused with both paths. An absent checkout is never guessed.
- The scan selects a `.git` directory beside a real `.jj` directory. It skips dot-directories, follows no symlinks, and stops below a `.jj`. A plain Git repository is not managed and does not hide forks under it.
- The scan does not select workspaces (a `.git` file). They bind when you stand inside. It skips non-colocated `.jj` silently, and a fork verb inside one refuses.
- A checkout deeper than three or outside HOME binds when you stand inside it, for every command except `repos`. With HOME unset, the exit is `2` with `HOME is not set; knives scans $HOME for checkouts`. knives never scans `/`.
- knives needs colocated jj, at the jj build that the fleet's tool config names, which matches the embedded jj-lib. Workspace `.git` files come from jj colocated worktree support (after 0.45.0, jj-vcs/jj#9941). `jj workspace add` makes one from a colocated workspace with `git.colocate` true, or with `--colocate`. This does not authorize a change to shared tool versions.

### Missing, ambiguous and unreadable results

- `repos` lists every entry. An unplaced entry has no release state. Ambiguous or unreadable candidates are problems.
- An unreadable checkout stays in the report while any entry is unplaced, because it can be that entry. It is dropped when all entries are placed. A directory the scan could not list is always reported.
- A named repo's refusal adds `; could not read: <what>`. `repos` prints `?` lines. `status --all`, `sync --all` and `audit --all` print `could not read: <what>` once on stderr, in every output format.
- A sweep leaves out a **definitely missing** entry, with `knives: <name>: not on this machine` on stderr. It exits by the entries it inspected and does not promise one row per entry.
- These stay as problem rows: ambiguous entries, entries found only under a disagreeing upstream, and entries that scan errors could hide. Status starts such a problem with `could not gather:`.

## Trust is guidance injection, not fork access

A fork entry grants no trust. Add a trust rule separately:

- `repos`: forge slugs. Any checkout remote that names the repository matches. Non-slugs are refused.
- `owners`: forge users or orgs. Any remote under one matches.
- `roots`: directory subtrees.

**Security boundary:** repo and owner rules match self-declared remote URLs, not forge identity. A checkout that declares a trusted remote matches. Prefer `roots` when unsure. knives reads only the local Git config of the checkout. It never reads user or system config, or `GIT_DIR`, `GIT_WORK_TREE` and `GIT_CONFIG_*`.

- Identity starts at the nearest `.git`, which Git does not clone as content. A `.jj` can be content, so `.jj` without `.git` is not an identity. A `.jj` under `.git` gets that repository's verdict.
- A nested repository never borrows an outer identity. Identity never runs jj, so it cannot write into another checkout. A checkout with no remotes matches only by `roots`.
- Trust allows **guidance-as-data injection only**, never fork-command access.

## OpenCode plugin

Ships with the CLI. A call can first name a file in a registered repository. Then, once per repository per session, the plugin says that the fork is managed and shared, and names claims.

- It appends trusted repository instructions as **data**. A fork entry alone gives the notice, not guidance.
- It skips an instruction file that the session already holds, until compaction. That is a file in its system prompt (under oh-my-pi, also the session's own `AGENTS.md`). It is also a file injected earlier in the session from any checkout.
- Under oh-my-pi, when shake, pruning or compaction removes an injected block, the next call to that repository injects it again, once.
- It sets `KNIVES_OWNER` in shells.

In `opencode.json`, all options default on:

```jsonc
"plugin": [
  ["file://{env:HOME}/knives/default/plugin/knives.ts",
   { "notice": true, "guidance": true, "owner": true }]
]
```
