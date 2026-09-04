# Identity-based registry: checkouts found by their remotes, trust by repo identity

One PR. `repos.toml` is shared between machines through dotfiles, and it serves
two purposes that were never separated: it says which forks we maintain and how
their releases work, and it is the allowlist of repositories whose `AGENTS.md`
may be injected into an agent's context. Both purposes are keyed by `path`, the
one field that describes a machine rather than a repository, and `knives init`
rewrites the whole file — through the dotfiles symlink, for every machine.

This design removes `path`, binds a checkout to its entry by its remotes,
finds checkouts by scanning, makes trust its own list matched on repository
identity, and deletes the only command that writes the registry.

Settled while designing, so nobody re-proposes them:

- **`origin` and `release` stay in the registry.** They state the fork's shape:
  which fork is ours, whether releases publish to an org repository. Reading
  them from the checkout instead would turn a clone missing its `release`
  remote into publish-to-origin, silently. The registry says what should be;
  the checkout says what is.
- **Fork entries no longer grant guidance.** Agents clone to `/tmp` and work
  there; guidance has to follow the repository wherever it is cloned, which a
  fork entry keyed by `upstream` cannot do (a clone of our fork has only
  `origin`). Trust matches on any remote and is stated separately.
- **No second config file, no learned locations.** A `checkouts.toml` would be
  a whole file for one column; recording where a checkout was last seen is
  derived state the ledger rule forbids writing down. The scan is the detector.
- **`--all` and naming a repo from elsewhere stay.** The fleet sweep is how
  the registry is used, and `knives repos` is what points a well-behaved agent
  at the managed checkout instead of a temporary clone.

## 1. `path` leaves the registry

**Bug:** `RepoEntry.path` is the only registry field that differs between
machines with different layouts, and it is load-bearing for every verb:
`Registry::containing` (which repo am I standing in), `guidance_roots` (the
hook's allowlist), and the `name → entry → entry.path` chain behind every named
verb and `--all`.

**Decision:** `[repos.<name>]` holds identity and policy only. `upstream`,
`origin`, `release` (URLs, as today), `base`, `release_branch`,
`test_count_command`, `consumers`, `workspaces`. `workspaces` stays because
`~/.worktrees/tool` is a `~/`-relative preference, not the location of
something that exists; it is valid on every machine. Its default remains the
parent of the checkout — now the *found* checkout (part 2).

**Changes:**

- `RepoEntry` loses `path` and `resolved_path`. `workspace_root` moves to the
  resolved fork (part 6), since it needs the checkout's location.
- `Registry` and `RepoEntry` deserialize with `deny_unknown_fields`, so a
  `path` line, or a `[trusted.*]` table, fails the load. The error names the
  field and what replaced it: `path` — delete it, knives finds checkouts by
  their remotes; `[trusted.<x>]` — move to `[trust] repos`. No compatibility
  read: a registry the binary half-understands is a trust set it
  half-understands.
- Load rejects two entries with the same `upstream`: identity must be unique
  or binding is ambiguous by construction.
- `TrustedEntry` and `[trusted.*]` are deleted (part 4).

**Result:** the registry describes repositories. Nothing in it names a
directory except the optional `workspaces` preference.

## 2. A checkout is bound to its entry by its `upstream` remote

**Decision — identity is the remote named `upstream`; `origin` and `release`
are compared and reported.** A fork is defined by what it forks. A checkout is
entry X when its `upstream` remote's URL equals X's `upstream` after
normalisation. Binding never depends on `origin` or `release`: the lab
fixtures deliberately pair a local-path `origin` remote with a forge URL in the
registry, because `repo_slug` derives our fork's `owner/repo` from that URL to
pick our pins out of a consumer's manifest. But the registry says what should
be and the checkout says what is, so a difference is reported: once bound, the
checkout's `origin` and `release` remotes are compared to the entry's, and an
absent or different remote becomes a **note** on that repository in `status`
and `repos` — `origin remote is <X>; registry says <Y>`, `release remote
absent; registry says <Y>`. A note, not a finding, not a fallback: knives keeps
reading refs from the registry's URLs exactly as today, and pushes remain jj's
business. This catches a transferred fork whose registry `origin` went stale,
and a stranger's clone carrying our `upstream`. The lab carries one such note
permanently, by the nature of its fixture.

**Normalisation:** a value that parses as a remote URL (`remote_authority_and_path`
handles `scheme://host/path` and `user@host:path`) compares as (host without
user, path without trailing `/` or `.git`), case-insensitively, so
`git@forge.example:org/tool.git` and `https://forge.example/org/tool` are one
repository. A value that does not parse (a filesystem path, as in the lab)
compares as its trimmed string. One entry's registry `origin` today ends in
`.git` while the checkout's does not; that class of difference is absorbed.

**From the current directory:** walk up to the nearest `.jj`. If `.jj/repo` is
a file, the directory is a workspace: follow the pointer to the checkout, as
`workspace_checkout` does today. Read the checkout's remotes with
`jj git remote list --ignore-working-copy` (works colocated or not, and inside
workspaces; `git_remotes` required a colocated `.git` and is replaced by this
one reader). Match `upstream` against the registry. The nearest `.jj` wins, so
a repository nested inside another resolves to the inner one, and nested
directories never inherit an enclosing checkout's identity.

**Which verbs bind how.** Verbs that act on one repository (`start`, `finish`,
`notch`, `release`, `preflight`, …) take the named repository, else the one
the current directory binds to, else refuse with `Usage` and the reason:
inside no repository; inside one with no `upstream` remote ("not a managed
fork"); inside one whose `upstream` matches no entry (named, so
`knives register` is one step away). Verbs that report over many (`status`,
`sync`, `audit`, `repos`) keep today's rule: a name selects one, a binding
current directory selects that one, anything else means every entry — found
through the scan (part 3). Running `knives status` from a directory that is
not a fork is still the fleet sweep.

## 3. Finding checkouts from outside: the home scan

`knives repos`, `status --all`, `sync --all`, and any verb naming a repository
the current directory does not bind to (`knives status tool` from `~`,
`knives notch --repo tool` from inside another fork) need `name → checkout`
without a path in the shared file.

**Decision — scan `$HOME`.** Depth 3 (`~/forks/tool/default` is depth 3),
skip directories whose name starts with `.`, do not follow symlinks, stop
descending at a directory containing `.jj`. A directory whose `.jj/repo` is a
directory is a checkout; one whose `.jj/repo` is a file is a workspace and is
skipped (its checkout is found on its own). Each checkout's remotes are read
once; each entry binds to the checkout whose `upstream` matches.

Measured on the machine this was designed on: 742 directories visited, 85 jj
roots, all 16 registered upstreams found exactly once, 0.8 s in an interpreted
prototype. Stray clones of the same upstreams have no `upstream` remote and
match nothing; one unlisted checkout has an `upstream` remote and is ignored —
the registry remains the allowlist for fork commands.

**Rules:**

- The scan runs at most once per invocation, and only when a verb needs a
  checkout the current directory does not provide. Hooks never scan: they are
  driven by the file path an agent touched (part 4).
- Two checkouts for one entry: refused, both paths named. The tool does not
  pick.
- No checkout for an entry: `knives repos` prints `not on this machine`;
  `status --all` and `sync --all` report it in `unanswered` — the same class as
  today's "could not open <path>"; a named verb exits `Usage` saying so.
- No depth knob and no root list. Misses are named in output, never silent.
  If a layout deeper than three levels ever appears, the miss is visible and
  the knob is a one-line addition then.

**Rejected:** a per-machine root list (a second config file for one line); a
default of the whole filesystem (unbounded).

## 4. Trust is its own list, matched on any remote

**Bug:** every `[repos.*]` entry is a guidance root, `[trusted.*]` grants
guidance to a path, and nothing grants guidance to the temporary clone an agent
actually made.

**Decision:**

```toml
[trust]
repos  = ["some-org/some-repo"]     # a single repository, by identity
owners = ["some-user", "some-org"]  # every repository under an owner
roots  = ["~/projects/company"]     # unchanged: a subtree
```

- `repos` matches when any remote of the checkout has that `owner/repo` path
  (case-insensitive, `.git` stripped). `owners` is unchanged. `roots` is
  unchanged. `[trusted.*]` is deleted; its content moves to `repos`.
- Being a managed fork no longer grants guidance. A fork whose guidance you
  want is covered by `owners` (our forks all have an `origin` under our own
  account) or listed in `repos`.
- The hook's resolution becomes one path: touched file → nearest checkout root
  → remotes → two independent facts. **Managed** (its `upstream` matches a
  `[repos.*]` entry): the managed-and-shared notice, the claim roster, owner
  derivation for `KNIVES_OWNER`, and the `seen` observation. **Trusted** (any
  remote satisfies `[trust]`, or the root sits under `roots`): guidance
  injection. A checkout can be both, either, or neither; today's exclusive
  `GuidanceRootKind` becomes two booleans on the match.
- The per-root remote cache in `hook/state.rs` (`owner_remotes`) keeps its
  role and now caches the full remote list, since managed matching needs the
  `upstream` URL and trust matching needs every URL.

Security posture, stated plainly: a remote URL and a path are both facts a
local actor can set, and neither authenticates content. The gain here is not a
stronger boundary. It is that the trust set names repositories — what a human
approved — instead of directories that may be empty or repurposed, and that no
knives command writes it.

## 5. Nothing writes the registry

- `knives init` is deleted, with `config::save`, `InitOutcome::NameTaken`, and
  `same_directory`. The registry is edited by a human, and reloaded on every
  invocation, so there is nothing for a tool to write.
- `knives register [DIR]` keeps its job and drops `path` from its output. It
  reads the checkout's remotes, and: if `upstream` matches an entry, prints
  `already registered as <name>`; otherwise prints the `[repos.<name>]`
  snippet (`upstream`, `origin`, `release` when present; name from
  `guidance_name`). The miswired-origin warning stays.

## 6. Code shape

- `RepoEntry` is registry content: identity, remotes by role, policy. Its
  remote accessors (`remote(Role)`, `has_split_release`, `publish_remote`,
  `trunk`, `release_scheme`, `immutable_heads`) are unchanged.
- `Checkout { path: PathBuf, remotes: BTreeMap<String, String> }` is what a
  scan or a walk-up found.
- `Fork<'a> { name: RepoName, entry: &'a RepoEntry, checkout: Checkout }` is
  the pair every verb works on, resolved once in `main.rs` where `one_repo` and
  `selected` resolve names today. `workspace_root` lives here.
- The ~30 files reading `entry.path` retarget to `fork.checkout.path`; the
  ~20 reading `entry.remote(...)` are untouched. `Registry::containing`,
  `containing_direct`, `guidance_roots`, and `workspace_checkout` are replaced
  by a `bind` module: `bind::here(registry, cwd)`, `bind::scan(registry, home)`,
  and the shared normaliser and remote reader.
- `knives repos` renders the found checkout path, or `not on this machine`,
  where it rendered the registry path. JSON gains nothing and loses nothing;
  the `path` key now carries the found location or `null`.

## 7. Migration and release

- The registry in dotfiles loses its sixteen `path` lines; its two
  `[trusted.*]` entries become `[trust] owners = [<our account>, <our org>]`
  or two `repos` entries; the installer comment about `~/` paths is rewritten
  to say the file names repositories and knives finds them. Same line of work,
  separate repository: done alongside this PR, not after it.
- This is a breaking change to the registry format. The commit is `feat!:`;
  under the release workflow's rule that a breaking change below 1.0 is a
  major release, this cuts **v1.0.0**. Sequence on each machine: release,
  `mise up`, pull dotfiles. An old binary refuses the new file (`path`
  required) and a new binary refuses the old one (`path` unknown), so the two
  land together.

## Testing

- **Unit:** load rejects `path` and `[trusted.*]` with the named replacement,
  rejects duplicate `upstream`, parses `[trust] repos`; URL normalisation
  (`https` vs `git@`, `.git`, trailing `/`, case, non-URL passthrough);
  `bind::here` from a checkout, from a workspace, from a nested repository,
  from a repository with no `upstream`, from one matching no entry; the
  `origin`/`release` note for an absent remote, a different one, and none when
  they match after normalisation;
  `bind::scan` over a fixture home with checkouts, workspaces, dot-directories,
  a depth-4 miss, and a duplicate; hook matching for managed-not-trusted
  (notice, no guidance), trusted-not-managed (guidance, no notice), both, and
  `repos` matching on `origin` alone; `register` output for a registered and an
  unregistered checkout.
- **Lab:** the harness writes registries without `path` and sets `HOME` to its
  temporary directory so any scan is hermetic; every lab command already runs
  from inside the checkout. Its fixture's local-path `origin` remote earns the
  origin note on every run, so asserts on exact note lists gain that line;
  nothing else changes.
- **`no_hardcoded_identity`:** unchanged and must stay green; the scan root
  is `$HOME`, not a spelled path.
- **Docs:** README registry example, `docs/design.md` registry and trust
  sections, `using-knives` (registry, `register`, `repos`, the scan) and
  `fork-work` skills; `init` removed everywhere it is mentioned.

## Sequencing inside the PR

1. `Checkout`, `Fork`, `bind` module with normaliser and remote reader; unit
   tests for binding and scanning.
2. `RepoEntry` loses `path`; `deny_unknown_fields` and the named rejections;
   `main.rs` resolves a `Fork`; call sites retarget; lab fixtures updated.
3. Trust: `[trust] repos`, `[trusted.*]` deleted, hook match split into
   managed and trusted; cache carries full remote lists.
4. `init` and `save` deleted; `register` reshaped.
5. README, `design.md`, skills.
6. Dotfiles: registry rewritten, installer comment.
