# `knives`: multi-fork, multi-agent maintenance

## Goal

Make the state of a fork cheap enough to query that no agent has a reason to guess, and make collisions between agents visible before they cost work.

The motivating setup is seven forks of one upstream ecosystem, each carrying between one and thirteen open upstream PRs as independent branches, integrated into flat octopus merges (dated or fixed) that a consumer repo pins. Several agents work these repos concurrently on one machine. Nothing below is specific to that setup; the tool is configured per repo and holds no knowledge of any particular user, org, or upstream.

## Non-goals

- Replacing `gh`. PR creation stays `gh pr create`. `fork` supplies facts.
- Replacing `jj`. General VCS work stays in jj and `jj-agent-status`.
- Keeping machines in step. Two things travel between machines, through a git repository the registry names (`ledger`): the ledger, and the claims of each fork that names that repository. Nothing else does: sightings, pull request records and the claims of a fork that names no ledger stay on the machine that made them.
- Judgment. Anything of the form "have you read and understood X" is a skill, not a CLI check.
- Fixing agent laziness. A tool cannot make an agent add a missing API method instead of declaring it impossible.

## The problems

Four, from observation. Each is something a tool can actually fix.

**1. Stale or partial PR and review state.** A review four days older than the branch head sent an agent to rewrite already-fixed code. A PR already carrying three of our replies nearly got a fourth. Three PRs had changes requested when the agent believed one did. Whether a review still applies to the current head is a comparison nobody makes by hand.

**2. A pinned revision mistaken for a branch.** Two agents independently reported that a cherry-pick had dropped a line. Both were reading the revision a release pinned, not the branch tip, which had moved. This has a precise mechanical cause and a precise detector; see below.

**3. Multi-agent collision.** Two agents cutting the same release. One agent's amendments rewriting another's release octopus into a conflicted state. One agent running `jj edit` on a branch another agent's workspace already had checked out, putting two workspaces' `@` on one change, which is the direct precondition for divergence.

**4. Agents ignoring the target repo's conventions.** This is the only one whose cost lands outside the team, and it has a mechanical cause rather than a behavioural one: an agent working in a fork does not receive that fork's `AGENTS.md`, because instruction injection is bounded to the instance directory. An upstream maintainer raised this directly after receiving PRs that ignored a documented contribution policy.

A fifth, raised from experience rather than observed in the audit: **two agents building the same fix on differently named branches**. Nothing detects this today.

## Configuration

Everything user-specific is configuration. No user, org, or repository name appears in the tool.

Per repo, `knives` knows a set of **remotes by role**:

| Role | Purpose | Required |
|---|---|---|
| `upstream` | the repo we contribute to; fetch only, including `pull/N/head` | yes |
| `origin` | where our branches get pushed | yes |
| `release` | where release bookmarks get pushed | no, defaults to `origin` |

Most repos need two roles. A third exists only when branches and releases must live in different places, which happens when the upstream cannot push to our fork (GitHub does not offer "allow edits by maintainers" for organisation-owned forks, so PR branches sometimes have to live on a fork the maintainer can push to, while releases live somewhere with different ownership). That is one configuration, not the model.

**Trunk is configuration.** `base` configures upstream's trunk branch (default `main`), which is the branch we fork from, measure landed against, and target pull requests at. It is accessible via `entry.trunk()` and `entry.upstream_trunk()`. Configurable per repository when an upstream default branch uses a name like `dev`.

**Two release schemes.** `release_branch` configures an optional fixed release branch name (`ReleaseScheme::Fixed`). When absent, the repository uses dated releases (`ReleaseScheme::Dated`, e.g. `release/YYYY-MM-DD`, `release/YYYY-MM-DD.N` or `release/YYYY-MM-DD-<label>`). Parse-time validation rejects an empty `release_branch`, a name matching trunk, or a name in the `release/` namespace.

**Named release series.** One fork can release on several bases at once (upstream's `v9.3`, and a frozen `v9.2+ours` branch kept on the fork). `[repos.<name>.series.<series>]` declares a series with its own `base` and `base_remote`; the entry's own `base` stays the default series, so an entry without series is unchanged. A series cuts `release/<series>/YYYY-MM-DD[.N|-<label>]` (`ReleaseScheme::Series`), whose parser requires exactly one segment after the series prefix: under a plain prefix parse `release/9.2/<date>` would have ordered after every date and become the default series' newest cut. One parser reads every cut of every scheme (`ReleaseScheme::cut_key`): a date, then nothing, `.N` or `-<label>`, and each series has that one order, so two names of one date are ordered by the rest of the name in version order. `--series` rebinds the entry (`RepoEntry::for_series`), so `trunk()` and `release_scheme()` follow the selected series in every release verb; `immutable_heads()` always pins the default trunk and every series' base, whichever is selected. A fixed-scheme entry's default series still reaps its leftover `release/YYYY-MM-DD` cuts (`RepoEntry::dated_scheme`). A branch belongs to the series whose base it forks from latest (`release_model::series_fit`: that series' fork point strictly descends from every other's); ties (including every branch at or below a base that is an ancestor of another base) and criss-crosses fit nothing and are refused with the fork points. Membership never works by elimination: with named series, a declared base that does not resolve stops every release verb (exit 3, the base named), and a base with no single fork point with a tip fits that tip to no series. Verbs outside `release` (audit's forbidden scan, status's overlap and landed probes, `start`) still measure every branch against the default base. Cuts, the plan's candidates, `include` and `advance` take only the selected series' branches; `audit` checks drift for every series' cuts and notes each branch that fits no series. Series names are one `/`-free segment that does not look like a dated cut (git cannot hold `release/X` and `release/X/<date>` together), bases are unique and outside `release/`, and series do not combine with `release_branch`.

**Verified:** splitting the roles across two remotes works with no mirroring step. A branch pushed only to the branch remote, made a parent of a release octopus pushed only to the release remote, was fully resolvable from a fresh repo fetching only the release ref. The push carries parent commits as objects.

Release parents are **upstream PR refs, not necessarily our branches**. `git fetch upstream pull/N/head` costs 0.48s and needs no fork. Carrying a maintainer's PR instead of ours, or a PR that was never ours, is the same operation as carrying our own.

Two remotes are required and a third is optional. `upstream` is what we contribute to.
`origin` is our own copy, which for most people is the whole story. `release` is a
separate remote that releases are cut on, for the case where releases are consumed
internally and should not sit in a personal fork; the release remote is optional, and it
falls back to `origin` when absent, because not every fork is consumed by anything.

`consumers` is optional too: forge slugs for repositories that pin this repo's releases. Knives
reads supported pin files from each consumer repository's trunk through the forge, not from a
recorded checkout. Each scan is cached by the consumer's trunk commit. When the forge is down,
cached pins are explicitly labeled as cache-backed and the result remains incomplete; no cache is
never treated as an answer. A local path is intentionally not persisted: `--consumer PATH` is an
ad-hoc local scan. Recording slugs lets `knives repos` say which consumer is pinned behind the
newest cut without requiring local checkout inventories.

### Consumer-pin census

`knives consumers [FORK] [--consumer PATH]...` checks every recorded forge consumer plus
explicit ad-hoc local scans against the newest release on the live publish remote. A pin names a
release reference and may freeze its resolved commit. The census reports an unavailable forge as
incomplete even when it can show cache-backed pins, a local path that is missing as an unanswered
problem, a consumer that does not pin the fork as a note, and a pin whose reference or frozen
commit differs from the newest live release as a finding. It also reports local release view
disagreement with the live publish remote rather than treating the checkout as authority. The
command never edits consumer checkouts.

**The registry names repositories, not directories.** A `[repos.*]` entry carries `upstream`,
`origin`, and the optional `base`, `release`, `release_branch`, `test_count_command`,
`consumers`, `workspaces`, `forbidden` (identifiers an upstream-bound diff must not add; `audit`
scans for them), and `ledger` (the repository the fork's ledger is shared through); there is no
path field, and a file that still has one is refused
on load with the entry named. `config::load` then appends an optional `repos.local.toml` beside
the file: `forbidden` and `consumers` only, for entries the registry already has, checked with
the registry's own lists, so private terms and consumer slugs stay out of a shared registry.
A checkout is the entry whose `upstream` its own `upstream` remote matches, so an entry
follows the repository to wherever it is cloned and a machine's layout is not configuration.
Two entries cannot share an `upstream`; the file is refused with both names. `origin` and
`release` are compared and reported as notes (`origin remote is <X>; registry says <Y>`), never
used to bind.

Remote spellings are normalised before comparison: a value that parses as a URL (`scheme://` or
`user@host:path`) compares as host without user and path without trailing `/` or `.git`,
case-insensitively; a value that does not (a filesystem path) compares as its trimmed text, so two
directories that differ by `.git` stay two directories.

Knives manages **colocated** jj checkouts — what `jj git init` and `jj git clone` make by default —
and reads every checkout through
git. A workspace must be colocated too: `jj workspace add` creates a git worktree for the new
workspace, writing the `.git` file beside its `.jj`, when the workspace it runs from is colocated
and `git.colocate` is `true` (jj-vcs/jj#9941, after 0.45.0) — the default in every checkout knives
manages, and `--colocate` forces it. Standing inside a checkout, or inside a `knives start` workspace of one, binds it: the
nearest `.git` above the current directory is the root (a directory, or the pointer file a
linked worktree carries, which git resolves to its checkout), a fork checkout is a root that also
holds a real `.jj` directory, and a clone nested inside a checkout is its own root and never
inherits the enclosing identity. A `.jj` with no `.git` beside it is not a repository to knives:
a fork verb run inside one is refused (`has a .jj but no .git; knives reads a checkout through
git, so it must be colocated`), the hook says nothing about it, and the scan passes it over.
Outside one, `knives repos`, `status --all`, and naming a repository scan
`$HOME` to depth three for colocated checkouts (`.git` beside a real `.jj` directory; a workspace
carries a `.git` file, is not a candidate, and binds when you stand inside it), skipping dot-directories, not following symlinks, and not
descending below a `.jj` (a `.git`-only directory is not a
checkout and does not hide what is beneath it). An entry with no checkout found is
`not on this machine`: a row on the `repos` listing, one stderr line and no row during a sweep,
never a failing exit, since the registry is shared across machines that hold different subsets.
An entry with two is refused with both paths named, because choosing would answer about the
wrong copy. A checkout the scan could not read is named beside the entries it may have been —
on the refusal, on the `repos` listing, once on stderr during a sweep — while some entry is
still unplaced; once every entry is placed it is dropped, since the scan locates entries rather
than auditing every repository under `$HOME`.

`[trust]` decides whose instructions the hook injects. It is separate from `[repos.*]`: a fork
entry grants fork commands and the managed notice, never guidance.
- `repos`: forge slugs (`owner/repo`) trusted by identity, matched against any remote of a checkout, case-insensitively, `.git` stripped from both sides. The file is refused on load when a value is not a slug.
- `owners`: forge owners trusted for instruction guidance, matched case-insensitively against the owner segment of every remote URL of a checkout.
- `roots`: directory subtrees where all contained repositories are trusted for instruction guidance; `~` expands and a relative value is taken from the config directory.
- **Security posture:** identity and owner matching read self-declared remote URLs from the candidate checkout's own git configuration file — `git config --local`, never the user's, the system's, or the environment's (`GIT_DIR`, `GIT_WORK_TREE`, and every `GIT_CONFIG_*` variable are removed before git runs). They are not forge-authenticated and grant guidance-as-data only, never fork-command access. The root is the nearest `.git`, the one marker a clone cannot carry: git refuses to check out a path component of that name whatever its type, so `.git` is always the repository's own, while a `.jj` — a store, a pointer file, a symlink, a working-copy record — is content any tree can commit and any clone delivers. So a `.jj` without `.git` is not a repository to knives, a `.jj` under a `.git` is that repository's content and gets that repository's verdict, a directory nested inside a checkout cannot inherit the enclosing checkout's identity, and a tree that arrives by clone cannot borrow another checkout's identity: the `.git` it arrives with names the remotes it was cloned from. jj is never run to decide identity, so a read never writes into anyone's checkout. Trust names repositories, so it follows a clone wherever it lands; a checkout with no remotes matches only via `roots`. No command writes the file: `knives register` prints an entry, and a human pastes it. Verdicts are recomputed from `repos.toml` on every hook event, so human edits to the file act as the approval mechanism.

## State

Local state is computed on demand. Store only what no amount of computing can recover:

- who is working on what, and why (the repo cannot know this; and it cannot be inferred from session working directories either, since an agent launched elsewhere may need to change a fork). On a fork whose registry entry names a `ledger`, this is one claim ref per branch on that ledger's repository, so it holds on every machine that shares it. The state file keeps a copy, which decides nothing
- why we carry a foreign PR as a release parent
- **what happened, and what was decided**: a ledger per fork, appended to and never rewritten, beside the state
  file. The two items above are current intent, rewritten whole on each change; `knives finish`
  deletes the one "why" the tool records. The ledger is the past tense: events this tool
  observed in its own commands, and judgments an agent asserted, each anchored to the
  subject's tip at write time.
- **branch statements**, on that ledger: a branch's stated pull request, its supersession
  pointer when one of our PRs closes in favour of another, its dependencies, and a
  **fork-only mark** for a branch we deliberately keep with no upstream PR yet. Without a mark,
  every such branch reads as an error in `knives status` forever. (2026-09-19: this mark
  predates Sami's ruling that "fork-only is not a real option" — a marked branch is an UPSTREAM
  member whose PR has not yet been opened, not a third destination.) Each is an entry carrying
  a `statement` field, never rewritten: per branch and kind, the newest `ts` is live, and
  stating it again, or forgetting it, is a newer entry.

A disposition is a terminal, past-tense human ruling: `merged-elsewhere`, `withdrawn`, or
`ruled-out`. It is an optional ledger field on a note, not a third kind and not derived state. A
write requires the ruling's text and at least one evidence item; a `#<n>` subject stamps that pull
request without trying to track a branch named `#<n>`. Readers can select notes, events, or
dispositions; the last class means notes that carry the optional field.

`knives notch --verify` re-checks selected entries against all commits visible to the repository
and its local bookmark tips. It flags a missing commit-shaped evidence token, a vanished anchor,
or a subject whose anchor no longer matches its local tip. That makes an old entry's context
inspectable without rewriting its past-tense record.

## Forge snapshot and cache

Forge discovery is not a local detector: listing a repository's pull requests finds the small
set of numbers that matter, but the list is too expensive to use as a report's fact source. The
snapshot separates those jobs. **The cache discovers; a live batch decides.** The discovery cache
is `$XDG_CACHE_HOME/knives/forge/<owner>/<repo>.json` (default
`~/.cache/knives/forge/<owner>/<repo>.json`); it stores cheap pull-request rows and a watermark,
along with the repository identity and cache schema. A missing, invalid, or deleted cache simply
uses the cold discovery path.

### Invariants

**I1 — no trust without a same-run sweep.** A warm run performs a live delta sweep and retains a
cached entry only when the sweep shows that nothing at or after the watermark touched it. A cold
run uses a live reseed. When both the sweep and reseed fail, the forge is not consulted and no
cached pull-request data is used.

**I2 — report-surfaced facts are live.** Every pull-request number that appears in branch rows,
sync classification, stated pulls, or dependencies gets its complete fact row in one live,
by-number batch in that run. Cache rows only discover numbers and preserve shadowed prior
history:

| Source | Fields |
|---|---|
| Cache and wide-list discovery | number, state, review decision, head-ref name and object ID, update time, draft and owner state, URL, base ref, merge commit |
| Live batch only | mergeability state, diff totals (additions, deletions, changed-file count), nullable head-ref object, review and commit timestamps, newest-tip tree and parent tree IDs, check rollup, newest comment |
For an open pull request, the same answered live fields drive three present-state findings in
`knives status` and bracketed `knives pr` flags. `empty-diff` requires answered zero additions,
deletions, and changed-file count; `deleted-head-ref` requires an answered null head-ref object;
and `empty-tip-commit` requires the newest tip tree to equal its sole parent tree. Unanswered
fields are silent: absence is not evidence of an incident. The head-ref object is read through
the head repository, which can refuse a token the base repository accepts (a fork whose
organization enforces SAML): the forge then nulls it and names that field in `errors`. knives
reads such a null as unanswered, not as an answered null, and the rest of the batch stands.

`knives pr NUMBER --timeline` issues one on-demand, bounded `last: 100` query for that pull
request's head-ref and state events. It reports force pushes, deletion and restoration, closure,
reopening, and merging from the forge's event log; knives stores no push or commit history.

**I3 — a failed live batch fails closed.** When discovery succeeds but any live-batch chunk fails,
no snapshot exists. The report treats the forge as unavailable, reads no cached facts, and neither
advances the watermark nor writes the cache.

**I4 — a lost cache write only loses freshness.** A writer reads once, merges each row by
`updatedAt` (with this run's freshly fetched row winning ties), then writes a temporary file and
renames it. Its watermark comes from that same read. Concurrent writers can lose a newer row and
make the next run refresh it, but a surviving file cannot claim another writer's watermark without
that writer's rows.

### Failure semantics

| Condition | Behavior |
|---|---|
| Sweep and live batch succeed | Build a snapshot and mark the forge consulted. |
| Sweep overflows before reaching an entry older than the watermark | Cold-reseed the cache, replacing its pull-request map; a successful live batch builds a snapshot. |
| Sweep fails | Attempt a cold reseed; a successful live batch builds a snapshot. |
| Any live-batch chunk fails | Mark the forge unconsulted; use no cache, do not advance the watermark, and do not write the cache. |
| Sweep and reseed both fail | Mark the forge unconsulted and use no cached pull-request data. |
| Cache is unreadable, corrupt, or has a schema or repository-identity mismatch | Ignore it and use the cold path. |
| Cache write or rename fails after live success | Keep the live snapshot, mark the forge consulted, and report the cache problem as a note without changing the command's exit. |

### Landed-verdict cache

The same cache file stores landed verdicts. Each entry is keyed by the resolved branch-tip commit
ID, resolved upstream-trunk commit ID, knives version, and landed-probe schema version. If either
ref does not resolve in the current checkout, the verdict cache is not read. An installed knives
upgrade, or a probe-schema change, produces a new key and therefore a fresh landed probe.

## Detection rules

Ten detection rules, all resting on mechanical fields and graph queries rather than reasoning:

**1. Stale release parent (`stale-parent`).** Rests on `Repo::bookmark_tips` compared against release parent commits. When a PR branch is rebased upstream, jj moves the local bookmark to the new commit but the octopus keeps the old one, leaving a parent whose bookmark has moved to a descendant. The release then ships pre-rebase code with nothing in the bookmark list saying so.

**2. Landed upstream (`landed`).** Rests on `classify_landed`, which replays the branch onto the upstream trunk (defaulting to `main`) inside a dropped jj-lib transaction — no operation records it, no ref the repository's own commands read moves, and no concurrent jj view sees it; each replay does stay in the commit store, under the internal ref jj gives every commit it writes, until a `jj util gc` — and inspects the tree diff. The replay goes commit by commit and stops at the first conflict, which already decides the verdict: a commit replayed onto a conflicted parent carries the conflict forward with its own change added, so down a long branch the unresolved conflict can grow, and the later replays then cost far more than the first; on the branches measured for this, replaying the whole range never finished. A matching landed-verdict cache key reuses that result; a changed branch tip, trunk tip, knives version, or probe schema runs the replay again:

| Result | Meaning |
|---|---|
| empty | landed verbatim, drop it |
| conflicted | the trunk has content in the way — a maintainer's edit, an unrelated later change, or the branch's own squash |
| clean and non-empty | not landed, keep carrying |

Authorship- and PR-number-agnostic, which matters because our work sometimes lands under someone else's PR number. The replay cannot recognise a squash merge, though: a branch replayed onto a trunk that already carries its squash conflicts with itself. So the forge's evidence settles what the replay cannot: a merged pull request whose `mergeCommit` the local upstream trunk contains, with the local branch holding nothing past the merged head, reads `in-trunk` whatever the replay said — including a divergent bookmark the probe never ran on. A branch carrying commits past the merged head keeps its replay verdict, with a note.

**3. Divergence (`divergence`).** Rests on `Repo::divergent_changes`, querying whether a single change ID maps to multiple commit IDs across disconnected clones or local rewrites. The general rule: a change rewritten while any other reference still points at its old commit diverges. Divergence is routine, but the observed failure is agents reading `/0`, `/1` suffixes and `??` bookmarks as corruption and stopping.

**4. Double checkout (`double-checkout`).** Rests on `Repo::workspaces`, checking if two workspaces hold `@` on the same change ID, visible in `jj workspace list`.

**5. Failing CI checks (`checks-failing`).** Rests on `ChecksSummary::failing()` — a hard failure or a check held for action — over red conclusion states (`FAILURE`, `TIMED_OUT`, `CANCELLED`, `STARTUP_FAILURE`, `ACTION_REQUIRED`, or `ERROR`) on open pull requests. The `ERROR` conclusion is what external CI posting commit statuses emits for an aborted or infrastructure-failed build, and missing it made a red pull request read as clean green. `ACTION_REQUIRED` also arrives from the tip commit's check suites, not only its rollup: a fork pull request whose workflows await a maintainer's approval has one suite per gated workflow with that conclusion and zero check runs, which the rollup omits entirely — so the rollup alone showed one green lint check and read `ok` on 11 of 20 open pull requests of one upstream. The checks cell tells the two apart (`failing` versus `action-required`); the finding detail names the held workflows.

**6. Wrong target base (`wrong-base`).** Rests on `PullRequest::base_ref_name` against `RepoEntry::default_base()`, flagging open pull requests targeting a branch name other than the expected base. It cannot tell a pull request aimed at our fork's trunk from one aimed at upstream's trunk because both are usually named `main`, the forge exposes no base-repository field, and `gh` resolves to upstream anyway. An empty base is unknown, not wrong. Only open pull requests are checked.

**7. Commits carried elsewhere (`carried-elsewhere`).** Rests on `Repo::branches_containing(tip)`, querying whether the branch tip is reachable from another reference. It reports where found and says nothing about what it means: whether a maintainer took the work, rebased it, or coincidentally landed the same content is the reader's judgment. Our own release cuts, `@git` refs, and trunk are excluded because releases contain these tips by construction, and reporting that buried the real signal.

**8. Branch file overlap (`branch-overlap`).** Rests on `jj::changed_files_between` path sets computed from `fork_point(trunk | branch)`, grouping files modified by two or more active branches. One finding per file, naming every branch. It is a path comparison and nothing more.

**9. Stacked history (`stacked-history`).** Rests on `Repo::merges_between(trunks, tip)`: merge commits reachable from a branch tip but not from any known trunk position (`release_model::trunk_positions`: the upstream view, the fork's, the local bookmark) that join two or more lines none of them reaches. A member of a flat release is linear past the trunk; a merge in that range — a release cut, usually — means the branch carries every parent of that merge. Measuring past one trunk view alone charged a branch with upstream's own merges whenever that view was behind the branch's base; when no release ref names a merge the detail says so and points at `knives sync`. Where a release is in hand, `release_model::member_history` also measures past what it carries (`Repo::merges_with_side_lines`): a merge that is no release cut (no release ref names it, and the ledger records no knives write under a release name with its change id), whose side parents (every parent after the first) the members the branch would not replace already carry, brings only the branch's own line. One rule (`release_model::integrates`) says which verb takes it: a branch that merges the members it descends from joins beside them through `include`; one the last event recorded as one of those members continues it, and a named `advance` replaces that member and keeps the others (`release_model::replaced_member`). Both name the merge, and the plan measures each member against the other members, notes it, and names the same verb; one whose side parent the release lacks, or that merged its own earlier tip back in, is stacked, and the detail lists every commit the branch would bring. `knives release` runs it on every member parent and stops calling a cut `flat` when one is stacked, and on every local branch the plan would otherwise point `include` at; `include`, `advance` and the first `cut` refuse a stacked branch with the same detail; `knives status` runs it on branches with open pull requests, because such a pull request submits the whole fork; `knives preflight`, the pre-contribution gate, runs it on every local branch. Those two and the first cut measure past the trunk alone. Those two also name a merge by the release refs of every series of the fork (`release_model::releases_of_every_series`). So the detail of a branch built on a named series' release names that release, as it names a default-series release. Observed on a real fork: a three-parent cut read `flat` while one parent contained the previous 26-parent cut, and the same branch became an upstream pull request of 61 commits and 140 files the maintainer questioned.

**10. Orphaned claim (`orphaned-claim`).** Rests on the claim store against `Repo::bookmark_tips` and `Repo::workspaces`: a claim on a branch that no bookmark on any remote and no workspace names. `finish` releases a claim; a bookmark deleted around it leaves the claim behind as a row with no tip and nothing to say why.

**11. Immutable-heads rule (`immutable-heads-rule`).** Rests on `jj::repo_immutable_heads`: the `revset-aliases."immutable_heads()"` the repository's own jj config states, read by asking jj (`jj config list --repo`) rather than re-discovering its config files. The rule a managed fork runs under is `RepoEntry::immutable_heads()` — jj's `trunk()`, the trunk by name on every remote knives knows, and every tag fetched from upstream: `trunk() | remote_bookmarks(exact:"<trunk>", exact:"upstream") | remote_bookmarks(exact:"<trunk>", exact:"origin") | remote_tags(remote=exact:"upstream")`, with the `release` remote's trunk added when one is configured — the trunks named outright because `jj git clone` pins `trunk()` to `<trunk>@origin` and the default alias picks whichever trunk-named ref is newest. jj's default adds `untracked_remote_bookmarks()`, and in a fork that pin walls every member tip beneath a superseded release ref a fetch re-materialized, or beneath another fork's pull request head, while protecting nothing — no local rewrite reaches a remote. It adds `tags()` too, and every release a fork publishes is tagged, so a tag on a release merge froze every member commit beneath it and refused `knives release rebase`. A rewrite moves bookmarks, never tags: the tag keeps naming the published original, locally and so on the remote. Upstream's tags stay pinned by where they were fetched from, `remote_tags(remote=exact:"upstream")`, not by a naming convention. Ancestry through `<trunk>@upstream` does not cover them: in oh-my-pi, 457 of the tags fetched from upstream sit off its trunk. Every other tag is the fork's own, its published releases and its `keep/*` retention tags alike, and all of them are rewritable. `knives release rebase` abandons the untagged copies its rebase makes of any of them (below). `knives start` writes the rule where none is stated, in jj's table form with a `doc` naming knives as the writer, and refreshes its own write when the entry's rule changes; a rule a human stated is never overwritten. Any stated rule that differs is reported with the config file as subject and both rules in the detail — the detail says which of the two cases it is. Absence is not reported, because `start` resolves it.

### Reproduced in the lab

Two of the rules above were reproduced end to end in a controlled lab (bare upstream,
our fork, and a second clone acting as maintainer) rather than reasoned about. The
evidence is kept because it is what the rules rest on.

**1. Stale release parent.** When a PR branch is rebased upstream, jj moves the local bookmark to the new commit but the octopus keeps the old one, leaving a parent whose bookmark has moved to a descendant. The release then ships pre-rebase code with nothing in the bookmark list saying so. Reproduced:

```
9e267d24  bookmarks=[feat/gamma]
0700338c  bookmarks=[feat/beta]
876dc2d6  bookmarks=[]          <- stale; feat/alpha moved to 118d0fcf
```

The contrast is the whole rule: a **local** rewrite auto-rebases the octopus and carries the bookmark with it, while a **remote** rewrite does not. This is problem 2 above.

**2. Landed upstream.** Ancestry-based detection fails for squash merges, which is the common path. Verified: after a squash merge the branch's content was in trunk while `trunk..branch` still reported one file changed, indistinguishable from an unlanded branch. Rebase the branch onto trunk and read the result:

| Result | Meaning |
|---|---|
| empty | landed verbatim, drop it |
| conflicted | landed but modified by the maintainer, drop after a human reads the delta |
| clean and non-empty | not landed, keep carrying |

## Command surface

Every command takes the repo from the directory you are standing in. Naming it is for when you are somewhere else, or want a different one; the checkout is then found by scanning `$HOME` to depth three. Requiring the name everywhere was the loudest complaint from actually using this.

```
knives register [DIR]          print a paste-ready [repos.<name>] snippet on stdout for the checkout
                               DIR (default: cwd) is inside, or `already registered as <name>` when
                               its upstream is an entry's; never writes; warns when an untracked
                               remote looks like another fork of upstream
knives repos                   the repos knives manages, where each checkout was found (or
                               `not on this machine`), their release state, and whether a
                               recorded consumer is pinned behind the newest cut
knives consumers [FORK] [--consumer PATH]...
                               compare consumer pins with the newest live published release
knives pushed [BRANCH]... [--repo REPO]
                               compare local tips with the live remote refs that own them
knives audit [REPO] [--all] [--no-github]
                               reconcile remote refs, open pull heads, recorded cuts and edits, and
                               anonymous heads, with one row of facts per maintained branch;
                               reports only, never repairs
knives sync [REPO|--all]       fetch all remotes and tracked pull/N/head refs; classify each
                               tracked PR as new | unchanged | advanced | merged | closed
knives preflight [REPO]        programmatic pre-contribution facts (see below)
knives status [REPO|--all]     problem-first status map; aligned branch rows (branch, state,
                               tip, push, pr, review, checks, landed, claim, seen, notch),
                               grouped findings, and unmatched workspaces
knives pr NUMBER [--repo REPO] [--timeline]
                               one pull request's live state; --timeline reads its bounded forge event log
knives start BRANCH            claim (waiting for the claim lock; a refusal names the holder, its
                               workspace and what runs in it, or, for a claim on a shared ledger
                               whose activity this machine cannot read, its machine and when it
                               was taken), create the workspace, base it on the release's shared
                               base (falling back to fetched trunk); --why from the holder
                               replaces the claim's reason. A release name's start is the release
                               claim, the write lease for the release's refs: its workspace rests
                               on a new empty commit off the root commit, not on a release commit.
                               On a fork that shares its ledger, start pushes the claim there
                               before it opens the workspace, and when it cannot read or push the
                               claim it records none and exits 3
knives finish BRANCH           hand back claim and remove workspace; the branch, its bookmark,
                               and any open pull request survive the release; the claim comes back
                               even when jj cannot snapshot the working copy whole, and a workspace
                               kept that way, or one whose claim another owner held, is left as it is.
                               The ledger records its --why, with or without --force. On a fork that
                               shares its ledger, finish releases the claim there before it removes
                               the workspace, and when it cannot, it leaves the state file and the
                               workspace as they were and exits 3. The finish of a release claim,
                               once jj holds no workspace of the claim, abandons the earlier
                               commits of the release that nothing holds, in one jj operation, and
                               names each one that something other than the release holds, with
                               what holds it; while the release holds conflicts, it abandons none
knives track BRANCH --pr N     state which PR a branch belongs to, overriding inference
knives depends BRANCH --on R#N  record that a branch cannot land before a pull request (upstream's or
                               the fork's own, the forge says which) or a member branch; --forget R
                               takes one back, and with --on corrects it
knives notch [SUBJECT]         read what happened here (bare: newest 20 human notes plus a
                               folded machine-event count; a subject: its whole chronology);
                               -m writes a note, --disposition requires --evidence
knives ledger sweep            commit, pull and push the shared ledger; single-flight; every write
                               sends its own entries before it exits, and exits 3 when they did
                               not arrive, so a sweep sends what a write could not, or pulls
knives ledger migrate          move what an older knives left onto what this one reads: the branch
                               statements state.json kept, onto the ledger, and each fork's ledger
                               directory, state keys and workspace sightings from its registry key
                               to its upstream name; a second run changes nothing
knives release [NAME]          plan, cut, edit, adopt or reap a release under the configured scheme
knives release cut [NAME]      name a new cut of the composition in hand, verbatim (first cut: every branch); refuses to orphan commits or to silently drop members that the previous cut's ledger event holds, or that the newest recorded write of the newest release the ledger records in the series holds, or a file of the fix by hand of an adopt of any release of the series where the file's reference (the newest recorded write that holds the file with no conflict) still holds that fix, held file by file by the candidate's own tree, a deletion as content and a conflicted write as no reference, and refuses a cut against a newest recorded write this repository does not hold, naming the fetch ([--allow-drop] overrides each), refuses a candidate with the same tree and the same parents as the previous cut on the publish remote, and refuses while the release in hand carries a hand resolution no adopt recorded; never pushes the release (on a fork that shares its ledger, the ledger event is pushed before it exits)
knives release cut NAME --at REV  record a release cut outside knives as NAME's recorded cut, with no commit and no temporary name; refuses a name git cannot hold as a ref, a commit that shares no history with the trunk, a conflicted commit or a working copy, a name that sorts before another release of the series, a ref of NAME at another commit, a published NAME the publish remote does not hold (naming the push that records it), a commit that merges no member, one that merges the previous release, and a member stacked on an earlier merge; refuses a commit that lacks a member of the previous cut's event or of the newest write of the newest release the ledger records in the series, or a file of the fix by hand of an adopt of any release of the series where the file's reference (the newest recorded write that holds the file with no conflict) still holds that fix, held file by file by REV's own tree, a deletion as content and a conflicted write as no reference, and a record against a newest write this repository does not hold, naming the fetch ([--allow-drop] overrides each); a parent the trunk holds is never a member; a NAME no ref holds gets a local bookmark; no reap; never pushes the release (on a fork that shares its ledger, the ledger event is pushed before it exits)
knives release reap            reap superseded dated release bookmarks everywhere locally and abandon their commits; all kept while the live cut carries conflicts; a name a live release claim holds is kept, and every name while the claims cannot be read
knives release include BRANCH  add a branch (or revision) to the release as one new parent; nothing else moves
knives release drop BRANCH     remove a branch's parent from the release; the branch and its bookmark are untouched; when no remaining member's history carries the parent, say after the write whether the release lost that content or its content did not change
knives release advance [BR..] [--from SHA]  move member parents to their branches' tips; named branches only, or every advanced member when bare; refuses a candidate that would replace more than one parent; --from names one branch's old parent directly, for a branch (e.g. `jj duplicate`-rebuilt) whose ancestry back to it is gone
knives release adopt [NAME] [--why TEXT]
                               record the commit a release name holds now, finished by hand, as
                               its newest knives write (an `edited` ledger event); reports the
                               files it resolved, the files it changed beyond its baseline's
                               conflicts (those need --why) and each member's added lines it no
                               longer holds; exits 1 only against a recorded write, for a member's
                               line lost from a file the baseline left clean that is not a release
                               log, or none of a member's blocks left in a release log or a
                               conflicted file
knives release members [REF] [--verify] [--carries REV] [--census] [--no-github]
                               the release's parents and holders; --verify reads each member's
                               own lines in the release and names each line it lacks;
                               --carries REV asks whether REV's content is carried (by REF, else
                               by every live release and upstream trunk); --census asks that of
                               every maintained branch
```

TOON is the machine default on any command when the environment says an agent is running it (or stdout is not a terminal): agents were grepping human output to count findings by detector, and JSON answered that at more tokens than the same structure needs. `--json` forces JSON exactly; `--text` forces prose.

### Mutation verification

`knives pushed` queries live remote refs and judges each name only against the remote that owns
it: release names against the publish remote, ordinary branches and pull heads against origin.
It reports equal, missing, differing, and remote-only branch states, including a branch that was
deleted locally while its remote ref remained. It changes neither remote nor local state.

`knives audit` applies that same reconciliation across a repository (or every registry entry with
`--all`), then checks open pull heads unless `--no-github`, zombie remote branches, each release
against the commits its recorded cuts and edits name, and anonymous heads. Its findings identify the observed drift; it never
repairs, deletes, pushes, or opens a pull request. A release finished by hand is drift until
`knives release adopt` records it as the name's newest write; that record is an edit event, which
the audit already reads. When releases publish to a separate remote, the audit also holds each
release ref on origin against the publish remote's ref of that name. At the same commit, origin's
ref is a copy, and the audit says so in a note. A name the publish remote does not have, or has at
another commit, makes origin's ref misplaced: a `remote-drift` finding that says what the publish
remote holds.
Per-pull history remains the separate, on-demand `knives pr <n> --timeline` read.

The report also carries `branches`, one row of facts per maintained branch (every local bookmark
that is neither the trunk nor a release name), and `template`, the upstream trunk's pull-request
template once per report; the `using-knives` skill is the field list. Every field is an
observation and `null` means unobserved; the facts never move the exit code. A divergent
(conflicted) bookmark has no single tip: a branch gets a `problems` line instead of a row, a
release-name bookmark gets its own `problems` line, and the trunk is `knives status`'s
`divergence` finding, not the audit's. The forbidden-identifier scans run on at most eight threads
however wide the machine: several owners audit one checkout in the same minute.

`knives notch` has two moods, split by `-m`: bare it reads, `-m` writes.
Reading is intentional and nothing injects notches into a session, so the bare form returns the
newest human notes and folds machine events into one count rather than letting routine events
hide decisions. `--events` reads the full chronology; `--dispositions` reads every terminal
ruling. `--verify` re-checks selected entries without writing. The `status` breadcrumb is the
other half: each branch shows its newest note when one exists, otherwise its newest event, with
the number of sibling entries it masks. A disposition token prefixes that compact text.

`knives repos` and `knives status` are deliberately separate: one answers "what am I maintaining", the other "what is the current state and what is being worked on right now". Conflating them was an earlier mistake in this design. `knives status` emits a problem-first map: `UNANSWERED` precedes the aligned branch table (`branch`, `state`, `tip`, `push`, `pr`, `review`, `checks`, `landed`, `claim`, `seen`, `notch`), followed by grouped findings. Absent display values are `-`; `push` displays `pushed` when its machine field is absent, and a divergent bookmark displays `divergent` in `tip`. `claim` and `seen` carry claim ownership and observation instead of a separate claims section. On-screen status display tokens use `failing` for failing checks and `none-ran` when no checks run.

### Which PR belongs to a branch

Inference finds an open pull request from our own copy of the repository, matched by head
branch name, with a `pr-<n>` bookmark understood as the fetched head of that number.
That is a good default and a bad rule, so `knives track` overrides it and accepts any
upstream PR, in any state, from any author: a PR opened before this tool existed, one the
maintainer closed because they wanted a different approach, or somebody else's that we
carry because ours was superseded. It asks the forge first. A fork's review PR has its own
number on the fork's repository, and stated as the upstream's number it named another PR;
the upstream numbers in the thousands, so most small fork numbers are upstream numbers
too. So `track --pr <n>` is refused when the fork's #n has the branch as its head and the
upstream's #n does not (naming both), when only the fork has #n, and when neither does.
An upstream #n whose head is another branch is accepted: it is someone else's PR we carry.
A head is the branch's only by its name and its owner together, the owner of the fork's own
repository as the forge names it now: an outside contributor's branch of the same name is
another head, and so is one of a former owner that a remote here still spells. The fork's
own repository is the one the checkout's `origin` remote names when that is a forge
repository, else the registry's, as `status` and `depends` read it.

### Dependencies

A branch can require a pull request in another managed repo. Dropping the required one
from a release without dropping the branch that needs it ships a release that cannot
work, which is exactly what happened when one repo's #4545 was dropped while a sibling's
#49 still needed it. An upstream pull request is satisfied when it has merged; an open PR
may still change or be rejected.

A requirement says what it names, because a fork and its upstream number their pull
requests apart: `<upstream>#<n>` is a pull request on the fork's upstream,
`origin:<owner>/<name>#<n>` a review pull request on the fork's own repository, and
`origin:<owner>/<name>:<branch>` a member branch of the fork. `knives depends --on
<repo>#<n>`, the registry name, used to mean the upstream's pull request, and recorded an
unrelated one where the fork's review pull request was meant. Another fork's key is first
read as below, and names this fork's own repository when that fork's upstream is it.
Otherwise, when the fork's upstream and own repository are both forge repositories, it asks
the forge which has #n, and refuses when both do, naming both, or when neither does; when
the forge names the fork's own repository as its upstream, #n is the upstream's with no
lookup of the number. When either is no forge repository, there is no such lookup, and #n
is the upstream's. A review pull request is
recorded only on the fork's own repository, and only when its head is a member branch of
the fork: a branch of the fork's owner that is not the dependent itself, a base, a release
or a fetched `pr-<n>` head. A review pull request on another fork's repository is refused:
its head is that fork's branch, which nothing here can judge, so the dependent names the
upstream pull request it becomes. The same branches are refused as member branches.

A member branch is satisfied once its current tip's work is in the upstream trunk: its row
is landed, or its landed verdict is in the trunk (set by the probe, or by its merged
upstream pull request when the trunk holds that merge and the tip holds nothing past what
it merged); or, for a member this checkout knows only as `<branch>@origin` (another
machine's, fetched), the same two tests on that copy: its merged upstream pull request
with a landing the trunk holds and nothing past what it merged, or an empty replay onto
the upstream trunk. A row judges the local tip only, so when `<branch>@origin` holds work
the local tip lacks (a local bookmark that did not follow a fetch), or the row has no single
local tip (a conflicted bookmark), that origin tip must pass too, as it would for a machine
holding only it. A merge alone is not enough: a pull
request merged into anything but the trunk, or a branch advanced past what merged, leaves
work the upstream lacks. A review pull request is satisfied once its head, the member it
carries, has landed upstream, judged at the head commit the forge reports for it, not at
whatever a branch of that name points at here: by that branch's row only when the local
tip is that commit, else by the commit itself. A head commit this checkout lacks (the
branch moved since the last fetch) is a problem until it is fetched; if its branch was
deleted, the problem says to fetch a ref that still holds the commit: the base it merged
into, or the pull request's own `refs/pull/<n>/head`. The report names the head. Its
own state is not the rule: a review pull request merges into a review base or into
another member, which says nothing about the upstream, and while the member is in the
fork's release only, dropping it from the release breaks the dependent. A review pull
request is judged by its head only on the fork's own repository, the one the forge names
for this fork's `origin` (a record under any other name counts when the forge resolves
that name to it, so a record from before a rename still counts, and a registry and a
checkout naming two live repositories are not merged), and when its head is a branch of
that repository's owner and a member branch; any other is a problem that says why, never
met. A member no copy here holds cannot be judged, and is a problem until it is fetched;
the problem says its work may still be in a review base it merged into before it was
deleted. Without the landed probe (`status --no-landed`), a member not shown landed stays
an unmet finding: what is not shown to have landed is still needed. So does one under
`--no-github`, or on a fork whose upstream is no forge repository: only the forge shows a
squash merge, and it was not asked, so the finding says the member is not known to have
landed. When the forge was asked and did not answer, a member not shown landed cannot be
judged: a problem, not an unmet finding. A member recorded under two spellings of the
fork's own repository, by machines whose `origin` remotes differ, is one requirement. A
member record on a repository no spelling here gives is judged as a member only when the
forge names that repository as the fork's own (a former name); a branch of another fork's
own repository, or of one the forge names otherwise, is no member of this one, whatever a
branch of its name here says: a problem.

A report checks the requirements of every row; of a dependent this checkout holds only as
`<branch>@origin`, another machine's branch nobody here claims; and of a dependent held
nowhere, deleted. The forge is asked about every pull request on this fork's upstream that
any of them requires, row or not. A deleted dependent is judged by the newest commit any
ledger entry records for it: an entry about the branch records its tip when written, and a
release write records each parent's commit and the bookmarks there; that record is kept across
forgets and after the branch is deleted. It is a lower bound on the dependent's work, not
its final head: the branch can move on with nothing written. So when that commit is here
and has not landed, the dependent's requirements are checked (a branch merged into a review
base and then deleted still carries its work there), and a note says so. When it has
landed, is not here, or was never recorded, the dependent is not taken as done, and its
requirements are judged too: one that is met says nothing, since nothing the dependent
carried later can change it; each one that is unmet or cannot be judged is a problem that
names the dependent, what could not be read, what was seen of the requirement, and the
`knives depends <dependent> --forget …` command that takes those back, with a fetch
suggested only when the recorded commit is not here. The command takes each requirement's
records in one run while the forge answers as it did for `status`: every record of its pull
request number or member on a repository that is not another fork's and that the forge did
not name as another, each as spelled (a member by its branch name when every such record
spells a spelling here and the name holds no `#`); a record on another fork's repository,
or one the forge named as another, alone. If the forge stops answering between `status`
and the forget, a record `status` left out because the forge named it apart may be meant
again: the forget is refused (exit 3) and prints a command that takes it too. The problem
says when the command takes a record it does not list. Under
`--no-landed` a recorded commit can still be shown landed by its merged pull request's
landing, not by a replay: one shown landed is judged as above, like a missing or
unrecorded one, and one not shown landed has its requirements checked as a row's, with a
note that says it could not be shown landed.

A requirement is taken back by a `depends` forget followed by the list without it, two
entries stamped in that order and written newest first: a run cut short between them
leaves the list as it was, never empty. `--forget` takes a requirement by any spelling
`--on` takes or the record's own; an `origin:` prefix always names this fork's own
repository. A review pull request recorded under two names of the fork's repository, as
before and after a rename, is one requirement: the forge ties the names, any spelling
takes back every record of it, and `--on` names it as the list already holds it. A
member recorded under two spellings here is one requirement too. A member record on a
repository that is neither a spelling here nor another fork's is a record of this fork's
member only when the forge names that repository as the fork's own, as for a review pull
request: with the forge down, a forget whose outcome depends on it is refused (exit 3),
and `--on` writes this fork's spelling beside it and says the two may be one requirement.
With the forge up, `--on` reuses a record the forge places as the fork's own, and writes
its own beside one the forge names as another repository's, which is another requirement.
A text spells a record when it parses to the same kind, number or branch, and repository,
the repository's letter case aside; a branch name's case always counts. A pull request
named `<owner>/<name>#<n>` by this fork's registered upstream is recorded with no forge
call, unless a spelling of this fork's own repository (its checkout's or registry's
`origin`) is that upstream too: the name then spells this fork's own repository first (see
below). One named by another fork's upstream, by its `<owner>/<name>` or its registry key,
may be this fork's own repository under another name, when a fork of this fork is
registered after a rename or a transfer, and nothing here tells the two apart: the forge
is asked, and it is this fork's review pull request when the forge names this fork's
repository so, or redirects the name to it; the other fork's upstream pull request
otherwise. `--forget` reads either spelling the same way. When the forge cannot be asked,
nothing is recorded (exit 3), and the refusal names that fork: a review pull request
recorded as an upstream one reads met once it merges anywhere, so refusing is the safe
failure. The refusal says to run it again when the forge answers, or, when the forge said it
cannot resolve a repository, names the repository it could not resolve (that upstream, or
this fork's own): it is gone, the token cannot see it, or the forge is failing, so run it
again later or check the repository. The cost is that an offline `--on` to another project's
pull request waits for the forge; and, with the forge up, one named by another fork's
upstream is refused when the forge cannot resolve that upstream or this fork's own
repository, since only those answers say whether the two are one. When this fork's own
repository names no forge repository (neither its checkout's `origin` nor its registry's is
a forge URL), it cannot be another fork's upstream under another name on the forge, and that
question is not asked: a pull request named by that upstream's `<owner>/<name>` is then
recorded with no forge call, while one named by the fork's registry key still asks the forge
which of that fork's two repositories has the number when both are forge repositories.
Another fork whose upstream is no forge repository, such as a path, has no `<owner>/<name>`:
its pull requests are named by its registry key exactly and recorded with no forge call, and
the key in another letter case is refused (exit 2), naming the key. This fork's own path
upstream is its registry key too, taken in any letter case: `DEMO#12` records `demo#12`,
with no forge call. With the forge down, then, a pull request is recorded only when naming it
needs no forge question: by `<owner>/<name>`, one of this fork's upstream, or of another
fork's when this fork's own repository names no forge repository; by a registry key, one of a
fork whose upstream or own repository names no forge repository (such as `local#12`), when
that fork is this one, its upstream is a path, or this fork's own repository names no forge
repository. One exception: when this fork's checkout or registry `origin` spells its
upstream, `<owner>/<name>#<n>` of that upstream names this fork's own repository first,
which only the forge can place, so it is refused (exit 3) offline. Such an `origin` is a
configuration error: set it to this fork's own repository. With the forge up, what each
spelling does depends on which `origin` spells the upstream. When only the registry's does,
and the checkout's names this fork's repository, that upstream's `<owner>/<name>` is refused
(exit 2: not this fork's own repository), while this fork's key is read as usual against the
checkout's repository: a number only the upstream has is the upstream's, one only the fork
has is its review pull request, and one both have is refused without offering the upstream
spelling, which has none while the registry `origin` spells the upstream. When this fork's
own repository spells the upstream, its key takes the upstream's number with no lookup, and
`<owner>/<name>` names a review pull request there. Any other text that only the forge can
read is refused (exit 3).
An upstream's `<owner>/<name>#<n>` is taken back without the forge. The forge has no
answer that means a repository does not exist: GitHub answers a name it cannot resolve the
same way for a repository that is gone, for one the token cannot see, and in some
outages. So a record whose repository the forge does not name, by that answer or by a
failure, is one the forge could not be asked about. The texts of one command are weighed
together: when counting each such record as named would take back more than the forge's
answers take, make a text ambiguous, or join two records a text names into one
requirement, nothing is taken back (exit 3), so neither an outage nor a repository the
token cannot see decides what a forget takes. Each such refusal prints a command that names
every record of that pull request number or member on a repository that is not another
fork's, each as it is spelled (a member by its branch name when every such record spells a
spelling here, and the name holds no `#`): every candidate is then named directly, so no
forge answer changes what it takes. It can take a record the forge showed is another
repository's: with the forge unable to say which repository one text names, any of them
may be meant; to keep one, run the forget again when the forge answers. When the text may
name two requirements, the refusal prints one such command for each, never one that takes
both. A text spelled exactly as a held upstream record names that record alone, whether or
not the registry knows its repository, and a `<repo>#<n>` text the registry reads as a pull
request names that pull request, as `--on` reads it, even when a member branch is named
so. A record on another fork's own repository is never a record of this fork's: a bare
branch name never names it, and a member token on that repository is refused unless a
record is spelled exactly so; then that record alone is taken back. Every machine
running this knives folds the two into the same list. A knives from before fork requirements
reads the same list only while it holds no member branch: it cannot read a list that
holds one, so the forget is the newest statement it reads there, and the branch requires
nothing. The forget restates the list the writing machine holds after its pull, and the
machines still agree on one list, but it can differ from what was meant, both ways. A
requirement another machine stated that had not reached the forgetting machine (unsent, or
sent after its pull) is not in
that list, and is lost on every machine. A list another machine writes after the forget
from a ledger that has not received it (the forget unsent, or sent after that machine's
pull) still holds the forgotten requirement, and brings it back; two forgets made that
way can undo each other. A fold that guarded against either would read the forget
differently from the knives before it.

### `knives preflight`

Only the programmatic half. It reports facts and does not ask questions:

- which convention files the target repo has (`AGENTS.md`, `CONTRIBUTING.md`, PR template) and whether they changed since last seen
- our current open-PR count against that repo, and any numeric limit its policy states
- whether the branch is claimed, stale, landed, or divergent

Everything of the form "have you read the contributing guide, and does this PR comply" is **skill-side**. A CLI cannot evaluate compliance and should not pretend to.

### `knives start`

Workspaces are effectively free: 0.15 to 0.55s to create, because tracked content is small even in large repos (one 3.2G checkout was 19M across 1278 tracked files, the rest being virtualenvs and the shared `.jj` store). The real cost of a new workspace is rebuilding language environments, not checkout.

`knives start` bases new work on the release's **shared base** when a release exists, falling back to the fetched trunk when none does, never on the current `@`. The shared base is the trunk point every member forks from, so a branch started there composes into the release without dragging newer upstream into the cut. Moving the composition to a newer trunk is `knives release rebase`: an intentional, separate decision, never a side effect of starting a branch. Never `@`, because an agent in a release workspace who runs `jj new` would inherit the release merge as a parent.

A release name is the exception. Its start is the release claim, the write lease for the release's refs, and its workspace rests on a new empty commit off the root commit, not on a release commit (`start::workspace_base`). A release write keeps each release commit it replaces while a working copy rests on it or above it, so a claim's working copy on the release would keep each commit that the claim's writes replace. A person moves it onto the release commit (`jj new <release>`) only for a hand resolution, and the finish of the claim abandons each earlier write of the release that nothing holds (`branch_verbs::sweep_earlier_writes`).

A `start` refused because another identity holds the claim prints what the refused caller needs to judge it: the claim's workspace, every process this machine can see inside it with its process id and command, whether the claim was taken in this place, in another place, or in a place it does not record, what each of those facts proves, and what the list leaves out — this run's own processes, its harness session's processes, and any process this user cannot inspect. A holder's name sorts nothing when one machine holds forty claims of one session, and the only evidence on this machine that the work is live is a process inside the workspace. The refusal is decided from the state file, and on a fork that shares its ledger from the ledger's claim too, and printed before the repository is opened, so a checkout whose operation log cannot be read still says who holds the branch: it reports that the log could not be read in place of a sighting, and exits `2` rather than failing. The durable token for that state, which the ledger records, is `unreadable-operation-log`. No other surface scans the workspace: not the `finish` refusal, the notices after a seizure and after a resume, the hook's claim rows or `knives status`.

On a fork whose registry entry names a `ledger`, the claim that decides is a claim ref on that ledger's repository, `refs/knives-claims/<owner>/<name>/<branch>` (`ledger_git::claims`). It holds a commit with no parent, whose one file, `claim.toml`, records the holder. Every machine that shares the ledger reads that one ref. A take, a seize, a restate and a release are each one atomic push with the machine's own ref, which carries the entry that records the change. The push is leased on the claim commit the command read, or on no claim for a take, so when two machines start one branch at once, the ledger's repository takes exactly one of the two pushes. For the push, the command takes `ledger.lock` and then the ledger git directory's transport lock, `knives-transport.lock`, waiting up to a minute for each, under the fork's lock. A sweep takes the same two in the same order: either may wait for the other, but the one order means the two never deadlock. A claim that cannot be read or pushed is no claim: `start` records none, opens no workspace and exits `3`. A claim's activity can be read only on the machine that took it, in a config home that recorded that claim. Anywhere else, this machine's sightings and process table hold nothing of it. So the refusal of such a claim names the machine and when the claim was taken, and says why its activity cannot be read here. It prints no workspace, sighting, process or place line: each would read as evidence that nothing works on the branch. Standing in a workspace resumes a claim only on the machine that took it. A claim in the state file with no claim commit is a local claim: an older knives took it, or it was taken before the registry named a ledger. It binds this machine, as it always did. The state file's copy of a ledger claim decides nothing, and `knives status`, the hook and preflight read the copy, so they list this machine's claims only. [shared-claims.md](shared-claims.md) has the refusals, the messages and the join's details.

A fork whose `ledger` no git directory here has as its `origin` has that ledger joined where knives routes the fork to it (`ledger_sweep::join`): knives makes the git directory in `ledger-repositories` itself, as one is made by hand. A ledger that cannot be joined is tried again on every route, so `start`, `finish`, a pull and the send of a write join only the ledger of each fork they route, and a ledger of another fork that cannot be reached costs them nothing. A sweep joins every ledger the registry names. The send's `ledger-sweep.log` names no unreached ledger of a fork it did not route: it has nothing true to say of that ledger, and the next sweep says it again. A problem of the send's passes can still name another fork the same git directory carries. Each config home must write a ref of its own, so the machine name a join writes is never one whose ref the ledger's repository already holds, unless the ref is the session's own. In a harness session it is that session's name on the host, so a config home the same session makes again takes up the ref the ledger's repository holds under it and resumes its claims. In no harness session it is `<short host name>-<8 random hex digits>`, written once to the `knives.machine` of each git directory the join makes: two config homes of one host that join before either pushes leave the repository no ref to tell them apart by, so random digits keep their names apart. A git directory made by hand or by an installer keeps the `knives.machine` it was given; a join in another config home picks digits of its own, so the two cannot share a name. A repository that already holds the ref of the name a join drew holds another config home's, maybe still in use, and a join that took it up would be a second writer of it: the first push of either would leave the other refused at every write after. So that join draws other random digits, and takes nothing up. A write refused as another writer of its machine's ref names the command that gives the machine a name of its own (`git --git-dir=<git dir> config knives.machine <unique-name>`). The git directory's `origin` is the URL as the registry spells it, credentials and all, and every message that names it shows it with no userinfo.

Every claim writer (`start`, `finish`, `track`, `depends`, `sync`) takes the lock of the fork it decides about, `fork-locks/<upstream>.lock` beside the state file, and holds it from before it reads the fork's claims and statements until it is done; it waits for it with growing, jittered pauses, so a wave of concurrently dispatched `start`s on one fork serialises instead of failing, while a `start` on another fork does not wait at all. Every release write (`release cut`, with `--at` too, `include`, `drop`, `advance`, `rebase`, `adopt`, `reap`) takes the same lock (`store::ForkLock`, taken once in `dispatch_release`) and holds it from before its first read of the ledger until its last write, so two release writes on one fork on one machine take turns and the later one reads the earlier one's event: three `cut NAME --at REV` at once on one machine write one event, where they wrote three. The lock is beside the state file, so it orders the writes of one machine only: a release write on another machine reads the event after its ledger pull. The lock is the kernel's lock on one open file, so a second acquire of it in the same process waits like any other waiter: a release verb opens the store only to read (`Store::open`, no lock), and a ledger append takes no lock, so nothing it runs takes the lock again. The plan and `members` write nothing and take none. The state file's own lock, `state.lock`, is held only by a save, for one read and write of the file: the save replays the command's changes onto the file as it is then, so a change another fork's writer saved meanwhile survives, and a claim that moved since the command read it (a writer that bypassed the fork's lock) fails the save with nothing written. `preflight` decides nothing from what it read and takes no fork lock; `ledger migrate` takes every registered fork's lock, then rewrites the whole file holding `state.lock` for the whole run. A claim refused at the save is refused only in the state file: `start` has already made its workspace, or restated the reason of the claim it resumed, and appended its ledger event, and `finish` has already appended its release event, because the event is written first so that no state change lacks its provenance. On a fork that shares its ledger, the ledger's repository holds the claim's change before the save. A take or a seize whose save fails releases the claim there again, and a `finish` has already deleted the claim ref. `finish` settles who may hand the claim back before it reads the workspace, and disposes of the workspace after the save, so a refusal at either point leaves the directory as it is. The knives before fork locks takes only `state.lock`, so it and this one exclude each other only at the save; they must not run concurrently on one machine. A waiter on a fork's lock, a release write included, and on `state.lock` waits up to one minute (`lock::LockWait::CLAIM`: pauses from 20 ms doubling to 2 s, with jitter). A release write whose wait runs out gets `LockError::Held`, which names the holder's pid where the lock file holds one and otherwise says the holder is unknown and how long ago the lock was written, prints it, writes nothing and exits `3` (`main`'s error exit). The `using-knives` skill states the claim writers' wait and refusal messages. Each lock is the kernel's advisory lock on its file (`File::try_lock`, `flock` on Linux), held by the open handle and gone with the holder's process. A lock file is never unlinked: it is the inode every waiter locks, and a path delete after an unlock would put two waiters on two inodes, each holding "the" lock. A knives that takes `state.lock` by exclusive create and unlinks it on drop reads the persistent file as always held, and holds no OS lock while it runs, so this binary acquires beside it and its unlink leaves the two on two inodes; the two binaries must not run concurrently on one machine, hooks included. The holder writes its pid into the file after acquiring; a refusal names it. `start` holds its fork's lock across `fetch_all` and workspace creation, so a wave of N concurrent `start`s on one fork waits about N times one start's hold, and `start`s on N different forks wait for none. The sidecar locks — sightings, the hook's session state — wait a fraction of the claim budget: a stale one must never stall a hook event.

`knives start` also states the fork's `immutable_heads()` (detector 11 has the rule and why) in the repository's own jj config when that config states none, refreshes the one it wrote earlier when the entry's rule has changed, and says so on stdout — naming a user-level rule the write now shadows, when there is one. knives' own library-side rewrites (`describe`, `abandon`, reap) keep jj's default pin set on purpose: a rebase may move a member out from under a stale ref; knives never rewrites what a remote still names.

`knives release rebase` runs `jj rebase -b <release> -d <target>`, which rewrites every descendant of the moved members, including earlier published releases that rest on them but are no longer ancestors of the release in hand. Their tags stay on the originals, which keep their commit ids, and the rebase leaves an untagged copy of each with the original's change id; the tagged original is hidden by the rewrite, still named by its tag. That covers every tagged commit the rebase copies, fork releases and `keep/*` retention tags alike, since upstream's tags are immutable and a rebase that would copy one refuses. Before the rebase the command records the tagged commits in `roots(<target>..<release>)::` that are not ancestors of the release; afterwards it finds each one's copy (same change id, not the original, descended from the target) and abandons it in one operation, `knives: <release>: abandon rebased copies of tagged releases`, through the same `assert_mutable` gate as every other in-process rewrite. A copy with a local bookmark or a workspace's working copy on it, or a descendant that is not itself a copy being abandoned, is somebody's work: it stays, and the report names it and what rests on it. Copies stacked on each other, a retention tag on a commit built on an old release, go together, children first. Before any of this the command refuses, naming `knives start` as the refresh, when the checkout's config still states a rule an earlier `knives start` wrote that is no longer the fork's: the tag-pinning rule would make the rebase fail with jj's bare "is immutable".

A branch that does get rebased onto a newer trunk (a maintainer asks for it) is still its own member: `advance`, `include` and `drop` match a member to its branch by change id as well as ancestry (`MemberSuccession`), and fall back to the parent set the release's last cut or edit event recorded — a `parents` field naming every bookmark at each parent (`release_model::member_parents`), which is what answers for a branch rebased outside jj or landed upstream, where the repository itself no longer can. Reading a rebased branch as a stranger to its release is what led agents to keep a second "release-lineage" copy of each pull request branch. One branch, on the shared base, rebased only when someone decides to; the release follows.

### `knives release`

Cuts, edits or repairs a release under the repository's configured scheme. Everything here is a check, never a prompt: a CLI in a non-interactive agent session has nobody to ask.

A release's parent set is its membership: a flat merge of feature and fix branches, never the upstream base — members fork from it, so it is reachable through every one of them, and no base/member role exists to classify. `include`, `drop` and `advance` are the membership edits. Each duplicates the release onto the changed parent set and writes each file by a rule close to git rerere's (`jj::edit_candidate`). The edit reads the plain merge of the release in hand's own parents, its carriers left out, with no release-log union, as `jj new` makes it. Where that merge leaves conflict hunks in a file, the release's own content there is a resolution, whether or not an adopt recorded it, unless the release still leaves every one of those hunks open with the sides it had. The edit keeps a resolution, as the duplicate writes it, only where the plain merge of the new parents leaves the same list of hunks. Each hunk is read as its bases and its sides, each a multiset, after a side that equals a base cancels with it (`Merge::simplify`); a conflict that is not between regular files is compared whole. Everywhere else the edit writes the plain merge of the new parents in the file and names the file where that changed it, and says so when the release had also changed lines outside the conflict hunks, a hand change that merge does not carry. A `CHANGELOG.md` or `RELEASE_NOTES.md` whose old-merge conflict the release-log union joins, as every write applies it, is left to the union; the union then runs over what the edit wrote. The edit builds that write as a scratch candidate (`jj::EditCandidate`) and checks it before it commits anything (`jj::EditCandidate::lost_lines`, `release_edit::edit_losses`). The check reads each path where the candidate differs from the release in hand, and each path that an included or advanced member changes. It places each line of each remaining member's own change (`own_lines::place`), from the member's fork point with the trunk, or, for a member that the trunk contains, from where the trunk took it in (`release_edit::HeldBase`, below), in three versions of the file: the candidate, the release in hand and the plain merge of the new parents, each with its conflicts written out in jj's snapshot style (`jj::edit::placing`). A line counts as lost where the candidate lacks it away from any conflict, the release in hand holds it, holds it at a conflict or the edit brings it in, and the plain merge holds it or holds it at a conflict. Then the edit writes nothing, names each path, member and line with the remedy, and exits `3`. With `--allow-loss <reason>` it writes the candidate and names the same lines as lost. Its event then records `; lost: <path> (<member>: <n> line(s))` for each path and member, then `; why lost: <reason>`, after the delta and before the parents (`release_edit::loss_event_text`), so `release_model::unchecked_mark` never reads the reason as the mark. Where no line counts, the flag changes nothing. The publish rebuilds the tree in a fresh transaction, refuses if it differs from the checked one, and then writes as `jj::write_release` does (`jj::EditCandidate::publish`). A rebase moves the release with jj's rebase, and its landed drop keeps the duplicate. A cut names the composition in hand verbatim and keeps the duplicate too: its composition gate holds each fix by hand. Nothing recomputes membership from the branch list after the first cut.

A release made outside knives is recorded with `knives release cut NAME --at REV` (below). `in_hand_write` and `adopt` measure a matched write's own change against its parents less its carriers (`release::write_composition`) and rebase that change onto the release's members (`Repo::holds_rebased_write`), so a jj rebase of the release over a member rewritten in place reads as recorded.

A release finished by hand is recorded with `knives release adopt [NAME] [--why TEXT]`. The record is an edit event (`edited <release> as <commit> (change <id>) with <n> parent(s): adopted: …; parents: …`), so `release_model::recorded_writes`, the audit and every older knives that reads edit events take the commit as the name's newest write with no reader change; the ledger is shared with machines on older builds. Its `parents` copy the event of the write it replaces, so `advance` and `include` keep each member's branch names. That write is the newest recorded write whose recorded parents, carriers left out, match the commit's parents, carriers left out, by commit or by a visible rewrite. The commit is measured against that write rebuilt on the commit's member parents, the tree `Repo::holds_rebased_write` reads (the write itself when the commit equals an older recorded write), so a member rewritten in place, which makes jj rebase the release, is no change of the commit's own; where this repository does not hold the write or its event carries the `; unchecked:` mark (`Replaced::is_baseline`), it is measured against a fresh merge of the member parents with the release-log union, and the output says which. A file changed beyond that baseline's conflicts is a repair or a renumber, so it needs `--why`, which the event keeps. The event's `fix_files` (`ledger::Entry::fix_files`, `release_adopt::fix_files`) list the files the commit resolved by hand, then those it changed beyond the conflicts, each path whole with how it changed (`ledger::FixFile`, `ledger::FixChange`), beside the text, which names the same files joined by `, ` as every knives reads them. The text cannot give back a path that holds `, `, `; ` or ` (why: `, and the list can. The list is an optional frontmatter key, the ledger's compatible extension point: an older knives ignores it, and this one reads an event without it, as an older knives writes it, from the text. A word for how a file changed that this knives does not know reads as `FixChange::Unknown`, and its path stays a file of the fix. For each member and changed file, `adopt` counts the member's non-blank added lines, from its fork point with the trunk, or, for a member that the trunk contains, from where the trunk took it in (`release_edit::HeldBase`, below), that the commit does not hold. It exits `1` (`LostLines::is_finding`) only when the baseline is a recorded write, never an automerge, and only for a member's line lost from a file the baseline left without a conflict that is not a release log, or for none of a member's blocks left in a release log or a conflicted file; a block cut down there is reported, and the exit stays `0`. It records before it exits, and does not refuse: a person reviewed the resolution, and a gate needs a measured failure of review first.

A release cut outside knives, by a person or a script, is recorded with `knives release cut NAME --at REV` (`release_cut::record_hand_cut`). It makes no commit, uses no temporary name and makes no throwaway cut. The record is the cut event a knives cut writes, with `; recorded: composed outside knives` in its text and no `unchecked` mark, so `adopt`, the edit verbs and another machine's audit read the name as recorded with no reader change. The name's grammar comes first (`requested_cut`: a usage error, exit `2`; on a fixed scheme `--at` is a usage error that names the fixed recording path). A name git cannot hold as a ref has no `CutKey` (`ids::ref_segment_flaw`, the one rule a series name is held to as well), so it is never the newest cut nor reaped, and `release::cut_name` refuses it with the rule it breaks. Then the pulled plan. Then the gates come in order (`hand_cut_gates`), each a refusal that writes nothing and exits `3`: REV is one commit that shares history with the trunk (`jj::Repo::shares_history`: a common ancestor other than the root commit, so the root commit and a commit made on the root apart from the trunk are refused), with no conflict and no workspace on it (`release_adopt::refuse_unfinished_commit`, which `adopt` shares); a recorded write of NAME is REV, which is already the record, or the commit is `adopt`'s, and the refusal names the recorded commit; NAME sorts after every other release of the series, locally, on the publish remote and in the ledger (`newer_release`, which reads the newest other ref as `release_model::previous_release_for_record` does, once, before the gates); each ref of NAME names REV, and with none the local bookmark NAME is made at REV, after every check; a published NAME is one the publish remote holds. A published REV the publish remote lacks only waits for its push, so that refusal gives the commands that make the bookmark where no ref exists, push NAME to the publish remote and run the record again (`unpushed_record_remedy`), never the published remedy of a hand edit. Each command a record prints holds NAME as one shell word (`ledger::shell_word`): a label may hold `$`, a quote or a backtick, so a name with any character outside `A-Za-z0-9._/+-` is single-quoted. The command exits with the worse of the plan's exit and its own (`Exit::worst`), so a record, or a name already recorded at REV, exits `0` when the plan is clean. The members are REV's parents, carriers left out (`hand_cut_composition`, which keeps each member with where it came from, `release::parent_sources`), and a parent a trunk position reaches (`release_model::trunk_reaches`) is never one: it is kept apart as a base, which the event does not record and the audit reads beside the members (`AuditContext::trunk_parents`). REV is refused when it merges no member, when one is the series' previous release (`release_model::previous_release_for_record`, which is never a member; `previous_parent_remedy`), and when one carries a merge past the trunk (`chain_record_remedy`). Those two refusals share one clause (`release_cut::FLAT_RELEASE`): the first ends `which is never a member; a release must be a flat octopus merge of the trunk and its member branches: merge the members, not the previous release`, and the second ends `a member above stacks on an earlier merge; a release must be a flat octopus merge of the trunk and its member branches: rebase each member above off the trunk`. The audit reads each member and each base by its own lines (`release::audit_cut`, below): each member from its own fork point with the upstream trunk (`AuditContext::upstream_trunk`), and each base from the fork point of all the members and the trunk (`HandCut::base`, `AuditContext::trunk_parent_base`). Like `members --verify`, it carries nothing forward from an earlier release (`AuditContext::previous` is `None`), so a member whose own line REV lacks is missing, and the line is named. Its drift is measured apart from that (`AuditContext::drift_from`, which carries the `DriftBase` its rows name): from the newest other release of the series, the release in hand a cut measures from, so a file no member, base or trunk explains refuses REV. A file that the gate holds of a fix by hand is left out of the drift, and only such a file (`release::CutAudit::leave_fixes_to_gate`, with the gate's `CompositionGate::fixes`, after a knives cut's audit too): a fix is no member's content, so no member tip explains its loss, and the gate, not the drift check, judges that file and refuses a subject that lacks it, once, naming its adopt and the file. The series' first record follows no release, so its drift is measured from its members' fork point with the trunk (`HandCut::base`, `AuditContext::drift_trunk`). A first knives cut builds its own tree and runs no drift check; a first record can hold any change, and every later record measures from it, so a change no member explains refuses it as it refuses a later record. Each row of such a file names the base it was measured from (`release::unexplained_row`, `release::DriftBase`): `the members' fork point with the trunk` for a first record, `the previous release` for every other cut and record. The composition gate follows. Every composition gate, a knives cut's, the fixed scheme's recording path's and a record's, holds its subject against one baseline (`release_model::CompositionBaseline::read`, whose `except` is the name of the record the gate holds, `release_cut::CompositionGate::read_baseline`): the previous cut's event (`release_model::last_series_cut`), and the newest write of the release in hand (`release_model::newest_release_write`, the first of the series' writes in their one order, `release_model::series_write_events`), which is the newest name in the series' one order that the ledger records a cut or edit of (the fixed branch under a fixed scheme), NAME aside for a record, at its newest cut, edit or `adopt` event. The gate never holds its subject to less because this repository lacks something: a recorded member it cannot resolve counts as dropped, and a newest write it does not hold refuses the subject (`CompositionCheck::absent_write`), since nothing here can tell what that write carries. The members of each event are its evidence after the commit, less, for the newest write, the parents its parent set labels as a carrier (`release_model::event_members`). The subject must carry each member that either event records, once each (`CompositionBaseline::members`, `release::uncarried_recorded_members`); a reachable rewrite of a member carries it only when it stands for it (`jj::Repo::standing_rewrites_of`, with `jj::Repo::stands_for`: a successor of it, which has it among its predecessors in jj's operation log, or a commit with its tree), so an older version of the member, which only shares its change id, carries nothing for it. A recorded member that none of these carries is read by its own lines in the subject, from its own fork point with the trunk (`jj::Repo::member_lines`). It is dropped where the subject lacks one of its lines, and inconclusive (`CompositionCheck::inconclusive`), which refuses nothing, where the subject leaves one of its paths conflicted and it lacks nothing else. The one change-id rule the gate keeps is the succession the ledger records: a member of the previous cut whose change id a member of the newest write has is superseded by that member, as `advance` recorded it, and is not held, while a member this repository does not hold stays held. The cut event restates a member a `drop` removed after that cut, and the newest write holds a member an `include` added after it. No ancestry carries a fix by hand. Each `adopt` the ledger records of any release of the series, before or after that release's cut, that recorded a fix by hand is held file by file, each file against its reference (`release::adopted_fixes`, `release::adopted_fix`, `release::measured_fix_files`, `CompositionDelta::fixes`, `release::fix_lacks`; `release_model::adopted_fix_replaces` reads the write an adopt replaced from the adopt event's own text). A file's reference is the newest of the series' recorded writes, the newest release first and each release's newest write first (`release_model::series_writes`, whose first is the newest write, as both read `release_model::series_write_events`), that holds the file with no conflict (`release::FixReferences::of`, `jj::Repo::conflicted_at`, `release::HeldFile::reference`, `release::FixReference`): a knives write that leaves the file with conflict markers neither ends its hold nor is what a subject is held to there, and the walk stops at a write this repository does not hold, which leaves the file no reference here and is named with its release (`FixReference::Unread`). The fix is the adopt's change against the baseline `adopt` measured it against, the replaced write rebuilt on the adopt's composition or an automerge of that composition (`release::fix_baseline`, which reads it by the one rule `adopt` reads it by, `release::baseline_write` and `release::resolution_baseline`; `jj::ResolutionBaseline`). Its files are the paths that change holds (`jj::resolution_changes`), where this repository can measure the adopt, and the files its event names (`release_model::adopted_fix_text`, which reads the event once, and `release_model::AdoptedFixText::files`, which gives the event's `fix_files` list, each path whole and once). An event without the list, as an older knives writes it, names its files in the text's `resolved by hand` and `changed beyond the baseline's conflicts` parts alone (`AdoptedFixFiles::from_text`, `release_model::text_lists`), where a path that holds `, `, `; ` or ` (why: ` reads back in pieces: so it names every run of at most 16 of a list's pieces (`release_model::TEXT_PATH_PIECES`) joined back with the separators between them, each run once and as a slice of the text (`release_model::text_pieces`, which reads the list once for each separator, and `release_model::text_paths`, which skips a run already named by a hash set), so a list of n pieces gives at most 16·n paths, never every run of a list that any peer's ledger may carry. Where this repository measures the adopt, its change gives the fix's real files, so of those runs only the paths the adopt's own tree holds are files of the fix (`jj::Repo::held_paths`): a piece such as `y.txt` of `x, y.txt` is none. Only an adopt that nothing here can measure holds every run, as no reading of the text can tell them apart, so the gate can hold more than the fix. Where nothing here can measure the adopt and its text does not read whole, nothing here reads the fix's files whole (`release_model::AdoptedFixText::unread`, read in the same one parse as the files). The text does not read whole where the text after `resolved by hand: ` holds a part that `adopt` writes after that list, once or more: `; changed beyond the baseline's conflicts: `, `; why: `, `; carriers: ` or `; added lines absent: ` (`release_model::RESOLVED_LIST_ENDS`). A path and a reason are free text, so the list may end there or name a path that holds that part: the text reads two ways, and the reading that ends the list there may name fewer files (`UnreadFixText::Marker`). Nor does it read whole where a list has more than 16 pieces, as such a list may name a path of more pieces, which the text does not give (`UnreadFixText::Pieces`, which counts a list's pieces no further than the first past the bound). `; parents: ` is none of those parts: every adopt event ends with its parents, so the last `; parents: ` is the one `adopt` wrote. The list of files changed beyond the conflicts ends at its last ` (why: `, so it reads at least whole. `release::adopted_fix` then reads no path of the text, and the gate refuses every subject whatever it holds (`release::fix_findings`, which records the fix in `CompositionCheck::dropped_fixes` with no file, `release::FixLack::Unread`, so `CompositionCheck::refuses` stays the one predicate). A clone that measures the adopt reads the fix's files from its change, so it reads such an event as it always did. The `fix_files` list of a newer event reads whole. A string that is no repository path, as a peer's ledger may carry, is no file (`jj::is_repository_path`), so it is never held and never an error. A deletion is content: a path's absence is a value as any other (`jj::Repo::same_path_value`). The hold on a file of the change lasts while the change replays empty onto the file's reference there (`jj::Repo::uncarried_resolution_paths`, which replays the whole change once for each reference and reads each path's outcome on its own; a path the baseline left conflicted is carried only where the target holds the adopt's value, since no three-way replay over a conflict can read one), so the hold on a file that the fix deleted ends only where its reference holds the file again. The hold on a named file that the change leaves out, as where this repository measures against an automerge, lasts while its reference holds what the adopt holds there, nothing at both included. A fix that nothing here can measure (`release::Unmeasured`: this repository does not hold the adopt, `Unmeasured::Absent`) holds each named file to what its reference holds there: a reference that holds nothing there holds the fix as an absence, and that hold ends only where a later reference holds the file again. A file with no reference stays held. So a fix that a later cut or edit kept stays held at every later release, and, where this repository measures the fix, a later write that removes the fix from a file, or changes its lines there again, ends the hold on that file alone. Where this repository does not hold the newest write, no fix is held, and the gate refuses for that write. In the subject's own tree, the candidate a cut builds or REV, a held file is carried where the change replays empty there (`HeldFile::replayed`, `jj::Candidate::uncarried_resolution_paths`), so a member that changes other lines of a file whose lines the fix changed costs nothing; else where the subject holds exactly what the file's reference holds there, nothing at both included, as a later adopt or `include` can change that file again; a file nothing here measures has that test alone, and a file with no reference has the replay alone. So a subject that holds a file that the fix deleted lacks the fix. A held file that the subject leaves conflicted is inconclusive (`CompositionCheck::inconclusive_fixes`, `CompositionCheck::is_inconclusive`) only where the newest recorded write is conflicted there too: the conflict came from a recorded write, so the knives cut goes ahead, exit `1`, names the fix as inconclusive, lists it in its event after `; unverified: `, and leaves the conflict for the person to resolve in place and adopt. Elsewhere, as for a cut from a bookmark left at a conflicted write that a later adopt resolved, the file lacks the fix and the cut is refused with the command that moves the release to the newest write. A record never leaves a conflict, as a conflicted REV is refused first. A fix the subject lacks is named by its adopt with each file it lacks (`CompositionCheck::dropped_fixes`, `release::LackedFix`, whose `release::FixLack` is either the files it lacks or why nothing here reads them whole). One predicate decides whether the gate refuses (`CompositionCheck::refuses`). Each message names a member with the event that records it (`release_cut::HeldRecords`, which also carries the record the gate holds, set once in `CompositionGate::held`; the words for each kind of event are `release_cut::HeldRecord`'s, which the refusal, the `--allow-drop` line, the inconclusive row and the cut event share): a cut names the previous cut first, as it always has, and a member only the newest write holds as `which <release>'s newest recorded write (an edit) holds`; a record names the newest write first, as `the newest release that the ledger records in the series`, and a member only the previous cut holds with that cut. The newest write, where it is not held here, is named with its kind, its commit and its release, and each fix the subject lacks with its adopt's release, its commit and each file it lacks (`release_cut::fix_title`, `release_cut::fix_files`, which the messages and the cut event share), and for a fix nothing here can measure, why, with the write whose content held it: the newest write, or the reference of each file where the newest write is not its reference (`release_cut::unmeasured_fix`); a fix whose files nothing here reads whole is named with why instead: its event is an older one that names the files of the fix in its text alone, in a list past the bound or one that the part it names follows (``in a list that `; why: ` follows, which may end there or name a path that holds it``), and the subject cannot be held against it (`release_cut::unread_fix`, with `release_cut::unmeasured_why`, which `unmeasured_fix` shares). A refusal ends with its remedy (`refusal_remedy`, with the newest write's findings in `release_cut::WriteFindings`; what each lacked fix asks for is `release_cut::fix_remedy`'s, the one place that reads a lacked fix's shape for its remedy, `release_cut::FixRemedy`): a record is built again carrying each fix, and after a fetch when this repository does not hold that write, carrying what it carries; where a subject lacks a file that has no reference here, no commit carries that file until this repository holds the write where the walk for its reference stopped, and where it lacks a fix whose files nothing here reads whole, of an adopt this repository does not hold, nothing here reads that fix until it holds the adopt, so the remedy first says to fetch that write or that adopt, named by the remote and the branch or tag that has it now (`release_cut::unread_fetch`), a record before it is built again and a cut in place of the bare fetch below; a cut gets a member of the previous cut back with `include`, and for a member only the newest write holds, a write not held here, or a fix it lacks, it is told to fetch, then move that release to the write (`jj bookmark set`), because the release here is older than that write and an `include` on it would write a second write beside it. That first fetch is named only where origin or the publish remote has a branch or tag at that commit: an edit abandons the commit it replaced and a later write moves the ref that held it, so a fetch may bring nothing. `release_cut::fix_remedy` asks the fork's remotes (`release_cut::Remotes`: origin, and the publish remote where it is another remote, each read at most once per refusal, and only when a fix asks, with one `git ls-remote` of its branches and tags, `jj::remote_refs`) for a ref at a full commit id that starts with the ledger's, which may be short, as the audit compares a recorded commit (`release_cut::Advertised::at`, which names a peeled tag without its `^{}`). Where neither has the write where a lacked file's reference walk stopped, that file counts as one with no reference, so the adopt of the new content stays the way on; where neither has the adopt, that fix has no step, as below. For a write not held here, both remedies go on (`release_cut::adopt_remedy`): where the fetch does not bring that commit, as when git refused the push of a write that `adopt` had recorded, `knives release adopt <release>`, its release one shell word, records the commit this repository holds as the release's newest write. For a fix that names its files, both remedies go on too (`release_cut::fix_adopt_remedy`): to keep the new content of its files, the newest write's release is edited by hand to hold it and adopted, so that adopt holds those files. A fix whose files nothing here reads whole, of an adopt this repository does not hold and no remote that was read has a branch or tag at (`release_cut::FixRemedy::NoRemote`), has no step that meets it: no fetch brings what reads it, and no commit carries it here. So a record's remedy asks for no commit for it (`release_cut::record_remedy`, which gives `None` where nothing else is lacked), a cut is not told to fetch and move for it, and the line gives the remedies of the other findings, if any, then `re-run with --allow-drop to state the drop is intended: nothing in this repository can hold a release against <fix>`, after `then` where remedies come before it (`release_cut::closing_line`). For an adopt that no remote that was read has, the fix is named with why, naming exactly the remotes that were read (`release_cut::unheld_fix`, with `release_cut::Remotes::answered`): `, as <remote> has no branch or tag at <commit>` for one, `, as neither origin nor <publish remote> has a branch or tag at <commit>` for both, and `, as no remote could be read for a branch or tag at <commit>` for none; and wherever the remotes were read and one could not be, the line ends `; <remote> could not be read`. A refusal that holds a fix whose files nothing here reads whole, whatever that fix's remedy, then ends saying that `--allow-drop` is the last resort (`release_cut::last_resort`): `; --allow-drop is the last resort: it also waives the checks of each recorded member, of the newest recorded write and of each other fix by hand`, and for a knives cut (not a record) `…, of each other fix by hand and of the commits reachable only from the previous release`, as `--allow-drop` goes past every finding of `CompositionCheck::refuses`, and for a knives cut (not a record) the orphan gate too, not that fix alone; every other refusal ends as before. A record is asked for a commit that carries a fix only where a commit can carry it (`release_cut::FixRemedy::commit_carries`), and only those fixes are counted in `that fix`, `those fixes` or `each fix`: where each file of a fix that the record lacks has no reference at all (`FixReference::Conflicted`) and is no file of the adopt's change, no commit carries that fix until an adopt gives those files a reference, so the record keeps the adopt of the new content and `--allow-drop` for it; a file of the adopt's change, carried by a commit that the change replays empty onto, keeps the commit step. `--allow-drop` states the drop, and the event restates each event as what it is, `; previous cut <name> recorded <n> member(s)` or `; <release>'s newest recorded write (an edit) holds <n> member(s)`, a member only the other event held as `; dropped from <event>: …`, a newest write this repository does not hold as `; not held against <event> <commit>, which this repository does not hold`, each fix the subject went without as `; without the fix by hand of <release>'s adopt <commit>, in <n> file(s): <files>`, and a fix whose files nothing here reads whole as `; not held against the fix by hand of <release>'s adopt <commit>, whose files this repository cannot read whole` (`release_cut::UNREAD_FILES`). The gate reads that release from the ledger, never from the newest other ref: on a fork whose older refs sit on other lines, that ref is no release REV follows. The fixed scheme's path that records a published commit the ledger lacks runs the same two checks (`checked_committed_cut`) against the same baseline, and records only while the previous cut's event names another commit. No orphan gate, reap, signing or push of the release runs.

A release write refuses over an unrecorded release. The release in hand is unrecorded when its tree differs from the tree of its matched recorded write rebased onto the release's parents, carriers left out: the merge of those parents, plus the write, minus the merge of the write's own parents. So a jj rebase of the write, a fast-forward carrier copy and a metadata rewrite read as recorded, and a hand edit does not; a release equal to any recorded write of its name reads as recorded without that computation. While the release is unrecorded, `cut` refuses (exit `3`, nothing written) whatever its audit would say, and `include`, `drop`, `advance` and `rebase` refuse (exit `3`) before they write, each naming `knives release adopt <name>`. Without the refusal, one cut would carry an unmeasured hand edit forward under a new name, and the next audit would read it as a recorded write. Where the release is a published dated or series release (a tag or a `record: published` note) that the publish remote does not hold, the refusal prints the published remedy instead (`release::published_remedy_text`): the edit is redone under a new dated name and the old name is forgotten, never deleted; the `using-knives` skill's releases reference has the steps. knives refuses only where it can measure. A matched write this repository does not hold leaves the tree uncomputable, a release whose parents no recorded write matches has nothing to compute against, and a name the ledger records no write of has no write to compute from, so such a release is unchecked (`release::Unchecked`, `release::InHandWrite::NoRecord`): the write goes ahead, prints one line saying knives could not check the release in hand, why, that it did not fetch, and what records it, and ends its ledger event with `; unchecked: W absent`, `; unchecked: no match` or `; unchecked: no record`. A cut over such a release still runs its composition gate, which refuses while this repository does not hold the newest recorded write of the series (`CompositionCheck::absent_write`). A release that is a marked write, or a jj rebase or metadata rewrite of it, is unchecked too (`release::Unchecked::Inherited`), and each write over it carries the reason on, until `adopt`, whose event carries no mark, records it. The tree decides before the mark: where this repository holds the marked write, a release whose tree carries a change on top of it, such as a conflict resolved by hand, is unrecorded, and every write refuses it until `adopt` records it (`release::in_hand_write`). The audit notes the newest write of a name while its event carries the mark. A release whose every parent is a carrier has no member to compose, so every write refuses it whatever its recorded writes, with a remedy by state (`release::no_match_refusal`): the published remedy for a published dated or series release, otherwise a squash of the resolution into the release commit, then `adopt`. A name with no recorded write at all is never refused, except by that all-carrier refusal, so a fork whose ledger predates knives is not blocked; each write over it is marked `; unchecked: no record`. Nothing measures such a release, and `adopt` has no write of it to replace, so a cut over it still writes, with its exit unchanged, and prints each divergence it carries as unmeasured (`release::CarriedFrom::Unledgered`): `the ledger records no write of the release in hand, so nothing measured its divergence from this member; check that divergence by hand`. Its event carries the mark, so `adopt` of the new name records it, and the cut after that carries the divergence forward as a recorded write's.

A fast-forward carrier is a parent that is an earlier write of the same release name, which a push adds when it rebases the release onto its published position to make the push a fast-forward. The ledger's cut and edit events of the name, the publish remote's commit of it, the visible rewrites of either, the change ids those events name (a pushed copy keeps its write's change id), and the parents a recorded event of the name lists under the carrier label define the set (`release_model::EarlierWrites::carriers`). Only `adopt`'s record of the fast-forward parent it took writes that label (`release_model::carrier_label`), never for a member of the write it replaces. Once the publish ref has moved past it, the label is the only record of a carrier that no event records as a write, such as a published resolution squashed into the release. A recorded write is a release merge and a member is a feature branch, so the two never share a change id, and the test cannot take a member for a carrier. The plan and `members` label a carrier `<name> earlier write (carrier, not a member)` and do not count it as stacked. Every write that composes from a release's parents leaves the carriers out: it writes an in-between commit on the other parents with the release's own tree, duplicates that, and records the in-between commit abandoned, in the same transaction (`rebase`, whose jj CLI rebase cannot share one, does this in a write of its own first). So a resolution made over a carrier becomes the release's own change, and no working copy moves. Kept as a member, a carrier would pile up, one more with every push. After a write records its abandons, it also abandons, until none is left, each new visible head that nothing holds (a bookmark, a working copy, a tag, or a remote ref other than the name's own tracked ref on its publish remote) when that head is an earlier write of the name or a commit whose newer version is still visible, and names each one. So a carrier a write leaves out stays hidden behind its publish ref, and anything else the write exposes, such as a member with no bookmark, stays visible for the audit to report. While the written release carries conflicts, nothing is hidden.

Every check of whether a release carries a member reads the member by its own lines (`own_lines`,
through `jj::Repo::member_lines`, or `jj::Candidate::member_lines` for a cut's candidate):
`members --verify`, the audits of a knives cut and of `cut NAME --at` (`release::audit_cut`), the
composition gate (`release::uncarried_recorded_members`), and `--carries` and `--census` against a
live or superseded release (`carriage::check`). A release merge names its members as parents, so
every member is an ancestor whatever the merge's tree holds, and only the content shows whether a
hand resolution kept a member's change. The member's own change is the diff from its fork point
with the upstream trunk (`Repo::fork_point`) to its tip, read with no rename detection. So the
trunk's commits after that fork point are never charged to
the member, and members that fork at different trunk commits are each measured from their own fork
point. A revision that the trunk contains is its own fork point (`Repo::change_base` reads only
that the trunk contains it). `Repo::release_base` asks first whether the release reaches such a
revision: then the release carries it by ancestry, and nothing more is read. Otherwise
`Repo::taken_in` reads how the trunk took it in: `roots(first_ancestors(trunk) & revision::)`,
one revset, is the oldest of the trunk's first-parent commits that contain it. Where that commit
is a merge, the base is the
revision's fork point with that merge's first parent (`TakenIn::Merged`). Where it is the
revision itself, as after a fast-forward or a direct push (`TakenIn::OnTrunkLine`), the base
is the newest commit of the revision's first-parent line, other than the root commit, that one of
the release's parents holds (`Repo::line_base`; for a cut, its candidate's parents). The composition gate
keeps its guard: a recorded member that the trunk contains and the subject does not reach is
dropped with no reading. R11's check and adopt's measure read a member that the trunk contains
from the same bases, though the release holds it as a parent (`release_edit::HeldBase`). On the
first-parent line its holders are the release's other parents that do not contain its tip. The
output names the base it read the member from, or says that it read no line of the member where
no other parent gives a base. The rule follows each line of that change from the tip to the
release, as `git blame --reverse` follows a line forward, instead of replaying the change as one
commit. The member's hunks are the line diff of its base and the tip, and the survival diff is the line
diff of the tip and the release (jj-lib's `ContentDiff::by_line`). The release carries the member
when it holds each non-blank line that the member added, and does not hold again, at the hunk's
place, all the non-blank lines of a hunk that the member removed. A line that the survival diff
replaces is followed by its words (`own_lines::words`, with `own_lines::difflib`, a port of
Python's `difflib.SequenceMatcher`). It is held when the release's replaced range holds at least
one of its words and every word that the member's change added to it, and does not hold again, at
that line, all the words that the change removed from it. A line that the member changed only in
whitespace is held only where the line diff keeps it byte for byte. A range of more than 30000
words on either side (`own_lines::WORD_LIMIT`) is not followed by words: its lines count as lost,
and the reading names the range. The word step builds a range's word diff before it asks for a
line's pair, so the pairing of a member hunk's removed and added lines (`words::pair_hunk`) runs
once per hunk, and only for a hunk where the word step follows a range. It pairs no line of more
than 30000 words, so its matcher never compares a line longer than the word step's bound. A
release log follows its own clause (under "Expect real
conflicts" below). A path that is not text (a NUL byte in its first 8000 bytes, a symlink or a
submodule) is carried where the release holds the tip's value there, a deletion where the release
holds nothing there, an added text file with no non-blank line where the release holds a file
there, and a changed executable bit where the release's file has the tip's bit. An edit of another
change next to the member's lines, in the same merge hunk or on the same line, that keeps the
member's own words makes no member read missing, unless it puts back what the member removed. A
path where the release, or the member's tip, holds an unresolved conflict cannot be read. There
the member reads inconclusive (`own_lines::Reading::Inconclusive`), and `--carries` reads
`conflicted`, where it lacks nothing else (in a cut's audit, nothing else that the release in hand
does not lack too), and missing where it also lacks a line elsewhere. A conflict in any other path
changes nothing. Each command names in its text each line that a member lacks, with its path, and
each line that the release holds only by its words, with the release's lines that hold them
(`release::lost_lines` and `release::held_by_words_lines`, the one renderer of all four text
reports). `members --verify`, `--carries` and `--census` also name them in their JSON report
(`own_lines::MemberLines`). A line held only by its words does not make the member read missing.

`knives release members --carries REVISION` is the content answer rather than an ancestry guess.
With no REF it compares the revision with every live release and the upstream trunk, reporting an
exact, rewritten, rebased, conflicted, or not-carried verdict with the commit that is its evidence;
`knives release members REF --carries REVISION` retains the explicit single-target query. It also
answers when no release exists, against the upstream trunk alone, rather than refusing; that is the
only target needed to decide whether unreleased work would be orphaned. A revision is safe only
when a live release or the trunk carries its content; a superseded release is consulted only after
those targets miss and does not make the result safe. Against the upstream trunk, which knives does
not compose, an ancestor tip is carried-exact. Any other tip's net change is replayed onto the
trunk from their common ancestor (`carriage::trunk_check`, `Repo::tree_replay_outcome`): an empty
replay is carried-rewritten, and otherwise a standing rewrite that the trunk reaches makes it
carried-rebased. Against a release, the revision's own lines decide, by the rule above
(`carriage::release_check`). It is carried-exact where the release reaches the tip and holds its
own lines, and carried-rewritten where the release does not reach the tip and holds them. Where the
release does not reach the tip, it is carried-rebased where the release reaches a standing rewrite
of it (`jj::Repo::standing_rewrites_of`) and holds that rewrite's own lines, read from the
rewrite's own base by the rule above. It is conflicted where a path of its change holds an
unresolved conflict, in the release or at the tip, and the release lacks nothing else, and
not-carried otherwise. The text names the lines under the verdict, and each JSON check holds the
reading (`TargetCheck::lines`).

`knives release members --census` turns that same check into a census. Its primary matrix contains
every maintained branch against the live releases and upstream trunk. Superseded releases are
checked only when that matrix is complete and every answered verdict is not-carried; an unanswered
row stays unanswered. Anonymous heads belong to `knives audit`'s orphan-commit detector.

The `orphans` list may contain qualified unknown entries: a branch's content is not carried
anywhere, but its unanswered pull state makes it a deletion-unsafe candidate. The row's `orphan`
field is three-state: `true` is a deletion-safe proven orphan, `false` is not an orphan, and
`null` means carriage or pull state is unanswered. Consumers must handle `null` as incomplete.

`knives release members [REF]` reads the release's direct parent list, which is the membership
source of truth, and names every bookmark still holding a parent plus branch tips that advanced
beyond one. `--verify` reads each member's own lines in the release by the rule above
(`release::gather_members`, `release::audit_cut`), with no previous release and no drift check.
Each member has one row: `<member>: carried (replay empty)`, `<member>: content check inconclusive
(…)`, or `!! <member>: the cut tree is missing or diverges from the member's content`, with the
lines that it lacks below the row. The JSON's `audit` names the missing members in `missing` and
the inconclusive ones in `inconclusive`. A member in neither list is carried. `carried`, which
names the members a cut carries forward, stays empty here. `audit.members` holds each member's
reading (`release::CutAudit::members`). A missing member is a finding. Parent counts come from the
repository's parent list, not text that happens to look like a parent declaration.

After the orphan gate and before the content audit, a cut is refused (exit `3`) when it has the same tree and the same parents as the previous cut on the publish remote: the candidate is built by duplicating the in-hand previous release onto the same parents, so its tree and parents always match the local previous release, and the published copy is the one consumers can pin. Two exceptions: a dated previous release every consumer pin freezes, and a published dated previous release — a tag names its local or published commit, or a `record: published …` note on the release names one in its evidence (`release::published_release`; other `record:` rows do not count). The edit verbs refuse both and send the operator to a new dated cut, so a verbatim cut under a name that sorts after it is allowed (exit `0`) and says why — that name is the composition the edits then apply to. The exception is judged from the same pin scan the plan printed, against the release the candidate is compared with; a fixed release branch (no other name to take) stays refused, and a fixed branch is never published in this sense, since it moves forward by design. Ahead of both comparisons, a dated name that does not sort after the newest release — by the same order the reap keeps the newest name by (`ReleaseScheme::cut_key`: the date, then the rest of the name in version order, then that rest as bytes) — is refused (exit `3`) before a candidate is built: the reap that follows every dated cut keeps only the newest name, so the cut would be created and taken straight back in the same command, reported as success. A name the series owns but that order cannot read, such as `release/9.2/foo`, has no key: `cut_name` refuses it as a usage error (exit `2`) with the grammar, it is never the newest release, and the reap never takes it. A member rewritten with the same content is a different parent, so a cut of it is a new composition and lands; a previous cut not yet pushed is not compared.

Every mutating verb applies as **one jj operation**, written through jj-lib in a single transaction and described in the operation log as knives' own act (`knives: release/X: included feat/y`, `knives: cut release/X`, `knives: reap …`) rather than as a trail of raw `jj` invocations. Failure before the commit writes nothing, so a half-applied edit is unconstructible. A cut audits a **candidate** built in a scratch transaction that is never committed: a failed audit evaporates without a trace, and a passing one rebuilds the spec, verifies the published tree is identical to the audited one, and creates-and-names the release in one operation. Git refs are exported in the same step, so `git` and `gh` in a colocated checkout see the result immediately. Rewrites honor jj's stock `immutable_heads()` (trunk, tags, untracked remote bookmarks) — the reap flow depends on that refusal. Identity for written commits resolves the way the jj CLI resolves it — the same system, user, repo and workspace files and `JJ_USER`/`JJ_EMAIL`, with `[[--scope]]` conditions judged by jj-lib's own resolver — and so does the `signing` table (`behavior`, `backend`, `key`, `backends.*`) for every commit knives publishes — a committed transaction, and the merge of divergent operations any load may make: an unsigned cut under a signing policy shows Unverified, and re-signing it by hand changes its id. Scratch transactions (the landed probes, a cut's candidate) sign nothing, since nothing they write is published. Every other behavioral setting stays at jj's defaults. Only `jj git fetch` and the composition rebase (`release rebase`, defined as `jj rebase -b <release> -d <target>`) remain subprocess calls, deliberately (#18).

- **Support both dated and fixed release schemes.**
  - **Dated scheme (default):** `knives release` inspects consumer pins to decide whether a release is pinned. Pinned releases require a new dated suffix (`release/YYYY-MM-DD`); unpinned releases repair in place. `knives release cut NAME` executes the cut, and `knives release cut NAME --at REV` records one cut outside knives. The fixed scheme refuses `--at` as a usage error: its own cut records a published composition the ledger lacks.
  - **Fixed scheme (`release_branch` set):** `knives release` takes no name argument and advances the fixed branch in place via a sideways bookmark move (`jj::set_bookmark_anywhere`). Publishing remains manual. A cut does not descend from the published position, so a publish remote that takes only fast-forward moves gets the release rebased onto its parents and the published position, its tree put back, then `knives release adopt` after that last rewrite, then a plain `git push <remote> <commit>:refs/heads/<name>`, never `jj git push`, which moves the remote branch sideways whenever it is still where jj last fetched it. The release then carries the published position as a carrier, which a cut's candidate leaves out, so the candidate never matches the published commit: neither the cut's path that records a published fixed commit the ledger lacks nor its identical-cut refusal fires for it, and `adopt` is what records it. The cut carries the *local* composition in hand under this scheme as under the dated one (`release::previous_release_for_cut`), unpushed edits included; reading the publish remote instead once made a fixed cut duplicate the stale published position and silently revert them. The published position is read separately from the publish remote-tracking reference (`{fixed}@release` or `{fixed}@origin`) and reported alongside (`release::previous_position`). Fixed release selection considers only the local fixed branch and its publish-remote counterpart.
- Build from explicit commit IDs and verify the parent count before pushing.
- Expect real conflicts. knives writes one resolution itself, at every release write (the cut and its publish rebuild, `include`, `drop`, `advance`, `rebase`): a `CHANGELOG.md` or `RELEASE_NOTES.md`, in any directory, whose conflict is only each side inserting its own block at the same place (`changelog_union`). The blocks the release already had go below the ones the write brings in: the union writes the earlier release's block last where that release inserted its own text there, and the other sides' blocks before it in jj's term order, as in a fresh composition with no earlier release or where `Merge::simplify` cancelled the earlier release's term. It names each file with the first line of each block it wrote. Most places it writes each block whole, byte for byte, as knives 4.0.1 did. It joins lists only at the one shape a reader is sure of (`changelog_union::joined_blocks`): in a file with one `## [Unreleased]` heading line outside code fences, the place is inside that section (no other `#` or `##` heading or setext underline follows that line above the place, and no code fence is open there), and every block written there is plain `### ` lists (`changelog_union::plain_lists`: blank lines, then `### ` headings, each over blank lines, list items and continuation lines that start with whitespace; no other heading, no code fence, no setext underline, no prose, and a final newline). The union counts that heading line in the whole file it writes: where it joined at a place and the file holds a second one below, it writes the union again with every block whole, so a joined entry always stands in a file's one `[Unreleased]` section, where the content checks hold it. There each `### ` heading, such as `### Fixed`, has one target, its first list in the last block that has it, and each earlier block's list under that heading joins it: its entries go above the target's own, in the union's order of sides. Entry lines are kept byte for byte. A joined list's heading and its blank lines are written once; where the target has no entries of its own, its blank lines are written below the joined entries too, so a blank line still parts them from the next heading. A block keeps the blank lines before its first list, and a block left with blank lines only is neither written nor named. One block can be the target under one heading and lose its list under another; a heading is the same heading only where its text is the same, so one that differs by case or by a trailing space keeps its own list. Where the place is not plain lists in a file's one `[Unreleased]` section, each block is written whole, and the write prints one line for the file that names each `### ` heading that two of the blocks there leave in one section (``CHANGELOG.md: left `### Fixed` twice: knives joins only …; join them by hand``); a heading that one block holds more than once stays as that block wrote it, and that line does not name it. The content checks read a release log as knives writes it (`own_lines`, with `changelog_union::unreleased_lines`). A non-blank line that a member added to a release log is held wherever the line diff puts it, when the release's file has exactly one `## [Unreleased]` heading line and that section holds the same line, trailing whitespace aside. The section runs from that heading line to the next `#` or `##` heading or setext underline outside a code fence, as the union reads it (`changelog_union::UnreleasedReader`). So an entry that the union joined into another list, or moved within the section, still reads as carried. Whoever wrote the line does not matter: a line that another member or the release also holds counts, as the check counts no occurrences. A line that only a released section holds, such as `## [1.0.0]`'s, is not held this way, and a release with no `## [Unreleased]` heading line or more than one holds no line this way. Elsewhere a line is read as in any other file (`knives release members`, above). That resolution is the same every time, so a person writing it by hand adds nothing. It never renumbers: two blocks that claim one version stay, and settling them is a person's. Everything else stays a person's too: a release-log conflict where a side changed or removed lines another side kept, a side's inserted block with no final newline that something else would follow, and every conflict in another file. Written verbatim, such a block's last line would run into the next line, and the union never adds a byte to keep them apart, so it unions such a block only where it ends the file. Independent branches that each append a config key land in the same regions; one ten-parent cut carried a 4-sided conflict in one file and a 3-sided one in another. Keep every branch's addition, keep a shared helper defined exactly once, land a config key in every loader, and record the result with `knives release adopt` before the next write.
- Name each `### ` heading that a written changelog repeats in `[Unreleased]`. After every release write (the cut, `cut --at`, `include`, `drop`, `advance`, `rebase`) and every `adopt` that records a tree, knives reads each `CHANGELOG.md` and `RELEASE_NOTES.md` that `changelog_union::eligible` names in the tree that the command writes or records, whether the command changed it or not (`jj::repeated_headings`). It skips a file that the tree leaves conflicted, and a file with no single `## [Unreleased]` heading line. It counts the `### ` headings of that section by their exact line, outside code fences (`changelog_union::repeated_headings`). For each heading that the section holds more than once, it prints one line, once for each file and heading in a command: ``CHANGELOG.md: [Unreleased] holds `### Added` twice; knives changed no line for it; join them by hand before you push``. The line says `twice` for a heading held twice and `N times` for one held more often. When and for which write the lines print is README.md's rule ("Release workflow"; `release::with_last_write_headings`). It also names a heading that the union's left-twice line names inside that one `[Unreleased]` section. It changes no byte. A clean merge can leave such a heading: a member's own new `### Added` block can merge with no conflict above the release's `### Added` list. Joining the lists is a person's, recorded with `knives release adopt --why`.
- Compare the merged test count against a single contributing branch. A total lower than one branch's own count means a branch's tests were dropped.
- Stamp per-parent provenance recording where each parent came from — the branch holding it, the trunk it descends from, or its own id — so `knives sync` knows what to check. This records provenance and pins nothing, since a jj octopus's parents are already specific commits.
- **Clean up workspaces whose branch no longer exists anywhere.** A branch the cut did not carry is not dropped, merely not a member. A workspace is named as cruft only when the ledger names its branch, no bookmark of that branch is left locally or on any remote, and no live claim holds it: a workspace knives did not make, or one an agent holds, is someone's work.

Releases stay **flat**. A nested integration node was considered and rejected: it makes dropping a landed parent harder, forces staleness detection to recurse, creates code that cannot be upstreamed until several PRs land, and destroys the empty-merge invariant that makes a cut verifiable. The case that prompted it dissolved by exposing an object rather than copying its fields.

## Harness adapters

`knives hook` holds the hook core. `knives hook claude-code` and `knives hook opencode` read one
event from standard input, apply the same repository and guidance logic, and write their
harness-specific response. The command logs failures but exits successfully, so a hook failure
does not interrupt an agent session.

| Shape | Harness | Adapter | Event behavior |
|---|---|---|---|
| A | Claude Code | A plugin-bundled shell hook calls `knives hook claude-code`. | `SessionStart` emits a notice when the working directory is a managed repository. `PostToolUse` handles relevant tools that name a path, adding an unspent notice and guidance. `PreCompact` and a compact `SessionStart` clear session state. `SessionEnd` deletes it. |
| B | OpenCode | The in-process TypeScript plugin is a shim that spawns `knives hook opencode`. | `tool.execute.after` uses the notice and guidance parts in one response budget. `chat.system` returns formatted guidance and its raw bodies. `shell.env` uses `KNIVES_OWNER` when set, then resolves an owner from a managed working directory. `compacting` clears session state. |
| C | oh-my-pi | The `omp/extensions/knives.ts` extension adapts the OpenCode hook core. | `resources_discover` exposes bundled skills. `tool_result` and `before_agent_start` apply the notice and guidance hooks, and `session.compacting` clears state. It leaves Pi's built-in `bash` tool intact, preserving its approval and sandbox behavior. |

Each session records `noticed` and `guided` flags for every repository. The Claude Code adapter
marks only `noticed` at `SessionStart`, then marks guidance after a relevant foreign-repository
tool use. It omits guidance when the event working directory is in that repository, because
Claude Code already loads the session repository's `CLAUDE.md`. The OpenCode adapter marks both
flags after any nonempty addition, so its notice and guidance consume one budget.

The OpenCode protocol fails soft. For a parsed event, a processing failure returns that event's
empty response envelope. Malformed input returns an empty object. Both cases keep the hook exit
successful.

Every hook invocation arms a watchdog thread that ends the process after 30 seconds
(`KNIVES_HOOK_DEADLINE_MS` overrides; zero or absurd values fall back). Harnesses spawn the hook
with a piped stdin and can abandon the handler that would write it — OMP times handlers out at
30 seconds and keeps the session moving — which without the watchdog leaves the process blocked in
its stdin read forever. On 2026-08-25 that leaked one immortal process per agent tool call across
~22 sessions until ~13k concurrent `knives` processes exhausted a devbox. A response is worthless
after the harness's own timeout anyway, so dying loses nothing. The watchdog exits `Incomplete`
(3), never clap's usage code (2): both the Claude Code wrapper and the TypeScript shim read 2 as
"binary too old" and 3 as load, which degrades silently. The TypeScript shim adds the same
guarantee from its side: it SIGKILLs its child after 10 seconds (`KNIVES_INVOKE_TIMEOUT_MS`
overrides, bounded to the 32-bit timer range) without condemning the binary, and refuses to hold
more than four children in flight per process, degrading to an empty response instead. The Claude
Code shell wrapper bounds its own stdin read with `timeout 35 cat` where timeout(1) exists,
covering the window before the binary's watchdog can arm.

The OMP extension uses Pi's native `bash` implementation rather than replacing it. Oh My Pi
exports its session id into its tool shells as `OMP_SESSION_ID` or, in other builds,
`PI_SESSION_ID` (and `CLAUDECODE=1`); the bash output is not a terminal, so CLI output is
machine-readable through the agent-environment path. Commands that need an owner take the first
harness session variable set — `KNIVES_OWNER`, `CLAUDE_CODE_SESSION_ID`, `OMP_SESSION_ID`, then
`PI_SESSION_ID` — then the managed working directory's recorded owner (never one an anonymous
claim recorded), before falling back to the operating-system user. Two sessions of one harness
under one OS user are two claimants; only the harness's own name for the session tells them apart.
A claim taken under the operating-system user is anonymous (`os-user`), and two anonymous names
never match, even under one user name: `start` resumes such a claim only from inside its own
workspace, and `finish` releases it without `--force` only from there, under the same user name.

### Trust boundary

Instruction injection is bounded to the instance directory:

```
while (current.startsWith(root) && current !== root)   // root = instance directory
```

For a session rooted in one repo reading a file in another, that is false on entry and nothing is injected. An agent can therefore read, edit and open PRs against a fork while that fork's `AGENTS.md` is never in context. That is problem 4, and it is mechanical rather than careless.

**The boundary is a security control and this design must not defeat it.** If any read injected the read file's directory guidance, reading untrusted content would become a prompt-injection vector: an adversarial `AGENTS.md` arrives as a `<system-reminder>`, which a model treats as instruction rather than data. The risk is acute for anyone who authors adversarial fixture trees on purpose.

So the adapters re-establish an equivalent boundary rather than removing it:

- **The allowlist is `[trust]`, which provides three ways to name guidance roots.** `repos` names repositories by identity (`owner/repo`, matched against any remote of a checkout), `owners` names forge owners (matched case-insensitively against remote URLs), and `roots` names directory subtrees. Any one rule true is enough; any tree none of them names receives no guidance.
  `[repos.*]` names maintained forks and provides fork commands and the managed notice only; a fork entry grants no guidance, and trust for a fork's own instructions is a `[trust]` rule like any other.
  - Distinct sections preserve parse-time invariants. A fork entry requires `upstream` and `origin` so malformed entries fail on load, and a `[trust] repos` value that is not a forge slug fails there too. No command writes the file, so nothing can drop a table on rewrite.
- **Inject a trusted repo's own `AGENTS.md` files**, from the touched file's directory up to the repository root, nearest first, plus our own overlay, which lives outside the repo. Trusting a repository means trusting its instruction files wherever they sit in it.
- **Mention `CONTRIBUTING.md` rather than injecting it.** Flagging that it exists is what the agent needs; its contents are long, and every injected byte is instruction-channel surface.
- **Containment by `relative()`, never string prefix**, so a sibling path like `<dir>-2` cannot pass as `<dir>`.
- **Canonicalise symlinks before the containment test**, or a symlink inside a managed repo can smuggle an untrusted tree's guidance in.

Both adapters inspect write-side tools as well as reads, so a grep-then-edit sequence can reach
the same notice and guidance path as a read.

The OpenCode adapter also supports **claim-token injection**. Claims cannot live in shell
environment variables, because each tool call is its own process and subagents are spawned by
the harness rather than by that shell, so an `export` reaches nothing.

`config.instructions` is the non-adapter alternative and does support absolute out-of-tree
paths, but it is static: pointing it at every managed repo injects all of them into every session
regardless of relevance.

One genuine upstream bug found nearby: the boundary test uses a raw string prefix, which widens the trust boundary past intent. A `relative()`-based containment check already exists elsewhere in the same codebase and is the model to copy. A second issue, guidance being silently dropped for image and PDF reads, fails closed and is low severity.

### What it injects, and when

When a relevant call names a file inside a repository the registry knows — a managed fork, a
trusted repository, or both — the adapters can produce these two parts:

- **A notice**, for a managed fork. That this is a knives-managed fork, that another agent may
  be working in it, which branches are claimed and by whom, and to use knives rather than jj or
  git directly. This is the one place the tool addresses the reader directly. It is emitted
  even when the repository has no `AGENTS.md`, which used to mean nothing was emitted at
  all and an agent was never told where it had walked into.
- **The repository's own guidance**, for a repository `[trust]` names, when it has any, framed
  as data. A managed fork that no trust rule names gets the notice and no guidance.

Triggered by files a call actually names, `path`, `filePath`, or an absolute or `~`-rooted path
in a command, and deliberately not by a command's working directory. A
batch of `gh` and `git` calls whose working directory sat inside a repository used to
spend that repository's one injection while touching no repository content, so a later
read of a real file got nothing.

The record is stored in a file for each harness and session. Its per-repository `noticed` and
`guided` flags survive calls made through different OpenCode plugin instances and prevent a
repository from spending the same announcement budget more than once.

### OpenCode configuration

An OpenCode installation from the release archive loads this plugin:

```jsonc
"plugin": [
  ["file:///<prefix>/share/knives/opencode/plugins/knives.ts",
   { "notice": true, "guidance": true, "owner": true }]
]
```

For development from a checkout, use its plugin path instead:

```jsonc
"plugin": [
  ["file://{env:HOME}/knives/default/plugin/knives.ts",
   { "notice": true, "guidance": true, "owner": true }]
]
```

All default to on, and a plain string entry keeps them that way. They are separable
because they cost very different amounts: the notice is a couple of hundred bytes,
the guidance can be 35KB of somebody else's contribution rules, and `owner` only exports
`KNIVES_OWNER` into shell environments.

### OpenCode binary discovery

The OpenCode shim resolves the binary in this order: `KNIVES_BIN`, the sibling
`<prefix>/bin/knives` in an installed release tree, the development tree's
`target/debug/knives`, then `knives` on `PATH`. The development tree comes before `PATH` so a
`file://` development install uses the checkout build instead of an older installed binary.
The Claude Code shell hook uses an executable `KNIVES_BIN`, then `knives` from `PATH`. It always
exits zero, so failures never break the session.

## Enforcement

Layered, cheapest first, escalating only on evidence:

1. **Default-correct paths.** `knives start` bases on the fetched trunk (defaulting to `main`); `knives preflight` before contributing.
2. **Detectors in `knives status`**, including two workspaces on one change.
3. **Advisory claims** with a description, plus `knives wip` showing file overlap between active claims. File overlap is the strongest duplicate-work signal available and is what the prior art converged on; every real collision observed was same-file.
4. **Hard refusal**, only if 1 to 3 prove insufficient. A `jj` shim is the mechanism and follows an established local pattern, where a shims directory sits first on `PATH` and already contains a `gh` wrapper written for jj, with tests.

Layer 2 exists in this form because the author of this spec caused a two-workspaces-on-one-change collision by hand within an hour of writing it. Habit and documentation did not prevent it; a one-line check would have.

## Verification

Measured or reproduced, not reasoned: workspace creation cost and tracked-versus-total size; the split-remote topology, end to end from a fresh repo; foreign `pull/N/head` fetch against a real upstream; stale parent after remote rebase; local-rewrite propagation; squash-merge landing detection and its three outcomes; cross-clone divergence; and codegraph's silent-staleness failure.

| What | Result |
|---|---|
| `jj workspace add`, largest repo | 0.545s |
| `jj workspace add` at a named revision | 0.151s, 177 files |
| Largest repo's tracked content | 19M across 1278 files, of 3.2G on disk |
| `git fetch upstream pull/N/head` | 0.48s |
| `codegraph init` on a fresh workspace | 1.68s |
| `codegraph sync` on a stale index | 415ms |
| Stale codegraph index queried for a file present on disk | `No results found` |
| Change ID for one commit across two disconnected clones | identical |

## Open questions

- Closed-not-merged while the branch lives on, which staleness bots produce. Distinguish from supersession and from a deliberate fork-only branch. (2026-09-19: same note as above — a deliberate fork-only branch predates the ruling that fork-only is not a real option; treat it as an UPSTREAM member awaiting its PR, not a third state.)
- A foreign `pull/N/head` advancing under a release, and whether re-cutting should be automatic or offered.
- Whether the claim gate's hard refusal earns its cost. The finish guard's did not: holding a claim through review blocked other agents for nothing, since a released branch, its bookmark, and its pull request all survive.
- Workspace lifecycle beyond what `knives finish` cleans up. They are cheap to create, which is why they accumulate.
- Codegraph integration. A stale index answers queries with silence rather than a warning, and `sync` costs 415ms, so any integration must sync before querying. Deferred; not important to resolve now.
