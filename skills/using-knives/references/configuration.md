# Registry, checkout binding, trust and plugin

Read this when a checkout does not bind or binds twice, before you register a fork or change `repos.toml`, `repos.local.toml` or `[trust]`, and when you configure the OpenCode plugin. The field list and an example are in the knives README, "Configuration". No knives command edits `repos.toml`: a person pastes what `knives register` prints.

## `knives register [DIR]`

- Prints a `[repos.<name>]` entry. Replace any section of that name; never add a duplicate. It takes effect at the next hook event, with no restart.
- It needs `upstream` and `origin` remotes and colocated jj (`.jj` beside `.git`). It refuses a plain Git clone and non-colocated jj.
- A fork that releases on several bases is one entry with release series, not a second entry.

## Registry traps

- One upstream `<owner>/<name>` may appear in only one entry, also across forges: ledger, claims and statements are keyed by that name, with no host.
- `workspaces` is the parent directory of `start` workspaces. Set it for a checkout at `~/<name>`, and write `~/…`: a relative value resolves from the config directory. A value inside the checkout is refused.
- `series.<name>`: one `/`-free segment that is a valid git ref component and does not look like a date. Each series' base differs from every other base and from the default trunk, and is not under `release/`. Use `base_remote = "origin"` for a base pinned on the fork. A base that does not resolve makes every release verb exit `3`.
- `forbidden`: case-insensitive substrings matched against lines a branch adds since its fork point with upstream trunk; `--fork-only` branches are exempt. Blank terms and case-insensitive duplicates are refused, also across `repos.local.toml`. Audit reports hits and never blocks.
- `repos.local.toml` beside `repos.toml` holds only `forbidden` and `consumers` for entries `repos.toml` has. Its lists are appended to the entry's.

## How a checkout is found

- An entry binds the checkout whose `upstream` remote matches it (case, a trailing `/` and `.git` ignored). Standing in the checkout or one of its `start` workspaces binds it.
- An `origin` or `release` mismatch still binds, with the note `origin remote is <X>; registry says <Y>`.
- When `origin` matches an entry but `upstream` matches none, nothing binds. Every surface names both spellings and the two fixes (`git remote set-url upstream …`, or the `repos.toml` edit); choose the right spelling.
- From elsewhere, a named repo and `--all` scan `$HOME` three levels deep; `repos` always scans. The scan takes a `.git` directory beside a real `.jj`, skips dot-directories and symlinks, and does not select workspaces. A deeper checkout binds only when you stand in it.
- A sweep leaves out a definitely missing entry with `knives: <name>: not on this machine` on stderr. Ambiguous entries, entries found only under a disagreeing upstream, and entries a scan error could hide stay as problems (`could not gather:` in status).
- With `HOME` unset: exit `2`, `HOME is not set; knives scans $HOME for checkouts`.

## Trust

- A fork entry grants no trust. `[trust]` `repos` (forge slugs), `owners` (forge users or orgs) and `roots` (directory subtrees) decide whose instructions the hooks inject.
- Trust injects repository guidance as data only. It never authorizes fork commands.
- `repos` and `owners` match self-declared remote URLs, not forge identity: any checkout that declares a trusted remote matches. Prefer `roots` when unsure.
- Identity starts at the nearest `.git`. A nested repository never borrows an outer identity, and a checkout with no remotes matches only by `roots`.

## OpenCode plugin

- Options, all default `true`: `notice`, `guidance`, `owner` (exports `KNIVES_OWNER` into shells) and `skills` (registers the bundled skills). Example: docs/design.md, "OpenCode configuration".
- Once per repository per session it says the fork is managed and names claims. A fork entry alone gives only that notice; trusted repository instructions are appended as data.
