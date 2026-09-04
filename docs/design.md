# `knives`: multi-fork, multi-agent maintenance

## Goal

Make the state of a fork cheap enough to query that no agent has a reason to guess, and make collisions between agents visible before they cost work.

The motivating setup is seven forks of one upstream ecosystem, each carrying between one and thirteen open upstream PRs as independent branches, integrated into flat octopus merges (dated or fixed) that a consumer repo pins. Several agents work these repos concurrently on one machine. Nothing below is specific to that setup; the tool is configured per repo and holds no knowledge of any particular user, org, or upstream.

## Non-goals

- Replacing `gh`. PR creation stays `gh pr create`. `fork` supplies facts.
- Replacing `jj`. General VCS work stays in jj and `jj-agent-status`.
- Cross-machine coordination.
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

**Two release schemes.** `release_branch` configures an optional fixed release branch name (`ReleaseScheme::Fixed`). When absent, the repository uses dated releases (`ReleaseScheme::Dated`, e.g. `release/YYYY-MM-DD`). Parse-time validation rejects an empty `release_branch`, a name matching trunk, or a name in the `release/` namespace.

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
`consumers`, and `workspaces`; there is no path field, and a file that still has one is refused
on load with the entry named.
A checkout is the entry whose `upstream` its own `upstream` remote matches, so an entry
follows the repository to wherever it is cloned and a machine's layout is not configuration.
Two entries cannot share an `upstream`; the file is refused with both names. `origin` and
`release` are compared and reported as notes (`origin remote is <X>; registry says <Y>`), never
used to bind.

Remote spellings are normalised before comparison: a value that parses as a URL (`scheme://` or
`user@host:path`) compares as host without user and path without trailing `/` or `.git`,
case-insensitively; a value that does not (a filesystem path) compares as its trimmed text, so two
directories that differ by `.git` stay two directories.

Standing inside a checkout, or inside a `knives start` workspace of one, binds it: the nearest
`.jj` or `.git` above the current directory is the root, a workspace's `.jj/repo` pointer is
followed to its checkout, and a clone nested inside a checkout is its own root and never inherits
the enclosing identity. Outside one, `knives repos`, `status --all`, and naming a repository scan
`$HOME` to depth three for jj checkouts (`.jj/repo` a directory), skipping dot-directories, not
following symlinks, and not descending below a jj checkout (a `.git`-only directory is not a
checkout and does not hide what is beneath it). An entry with no checkout found is
`not on this machine`; an entry with two is refused with both paths named, because choosing
would answer about the wrong copy. What the scan could not read is always named beside the
entries it may have been: on the refusal, on the `repos` listing, once on stderr during a sweep.

`[trust]` decides whose instructions the hook injects. It is separate from `[repos.*]`: a fork
entry grants fork commands and the managed notice, never guidance.
- `repos`: forge slugs (`owner/repo`) trusted by identity, matched against any remote of a checkout, case-insensitively, `.git` stripped from both sides. The file is refused on load when a value is not a slug.
- `owners`: forge owners trusted for instruction guidance, matched case-insensitively against the owner segment of every remote URL of a checkout.
- `roots`: directory subtrees where all contained repositories are trusted for instruction guidance; `~` expands and a relative value is taken from the config directory.
- **Security posture:** identity and owner matching read self-declared remote URLs from the candidate checkout's own jj or git configuration. They are not forge-authenticated and grant guidance-as-data only, never fork-command access. Remotes are read from the nearest repository root (a workspace's from the checkout its `.jj/repo` pointer names), so a directory nested inside a checkout cannot inherit the enclosing checkout's identity. Trust names repositories, so it follows a clone wherever it lands; a checkout with no remotes matches only via `roots`. No command writes the file: `knives register` prints an entry, and a human pastes it. Verdicts are recomputed from `repos.toml` on every hook event, so human edits to the file act as the approval mechanism.

## State

Local state is computed on demand. Store only what no amount of computing can recover:

- who is working on what, and why (the repo cannot know this; and it cannot be inferred from session working directories either, since an agent launched elsewhere may need to change a fork)
- why we carry a foreign PR as a release parent
- supersession pointers, when one of our PRs closes in favour of another
- **fork-only marks**: a branch we deliberately keep with no upstream PR. This should be the minority, but it is real, and it covers CI we want on our fork but not upstream. Without a mark, every such branch reads as an error in `knives status` forever.
- **what happened, and what was decided**: an append-only ledger per repo, beside the state
  file. Everything above is current intent, rewritten whole on each change; `knives finish`
  deletes the one "why" the tool records. The ledger is the past tense: events this tool
  observed in its own commands, and judgments an agent asserted, each anchored to the
  subject's tip at write time.

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
fields are silent: absence is not evidence of an incident.

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

**2. Landed upstream (`landed`).** Rests on `classify_landed`, which replays the branch onto the upstream trunk (defaulting to `main`) inside a dropped jj-lib transaction — a pure read that writes no operation and is invisible to concurrent agents — and inspects the tree diff. A matching landed-verdict cache key reuses that result; a changed branch tip, trunk tip, knives version, or probe schema runs the replay again:

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

**9. Stacked history (`stacked-history`).** Rests on `Repo::merges_between(trunks, tip)`: merge commits reachable from a branch tip but not from any known trunk position (`release_model::trunk_positions`: the upstream view, the fork's, the local bookmark) that join two or more lines none of them reaches. A member of a flat release is linear past the trunk; a merge in that range — a release cut, usually — means the branch carries every parent of that merge. Measuring past one trunk view alone charged a branch with upstream's own merges whenever that view was behind the branch's base; when no release ref names a merge the detail says so and points at `knives sync`. `knives release` runs it on every member parent and stops calling a cut `flat` when one is stacked, and on every local branch the plan would otherwise point `include` at; `include`, `advance` and the first `cut` refuse a stacked branch with the same detail; `knives status` runs it on branches with open pull requests, because such a pull request submits the whole fork; `knives preflight`, the pre-contribution gate, runs it on every local branch. Observed on a real fork: a three-parent cut read `flat` while one parent contained the previous 26-parent cut, and the same branch became an upstream pull request of 61 commits and 140 files the maintainer questioned.

**10. Orphaned claim (`orphaned-claim`).** Rests on the claim store against `Repo::bookmark_tips` and `Repo::workspaces`: a claim on a branch that no bookmark on any remote and no workspace names. `finish` releases a claim; a bookmark deleted around it leaves the claim behind as a row with no tip and nothing to say why.

**11. Immutable-heads rule (`immutable-heads-rule`).** Rests on `jj::repo_immutable_heads`: the `revset-aliases."immutable_heads()"` the repository's own jj config states, read by asking jj (`jj config list --repo`) rather than re-discovering its config files. The rule a managed fork runs under is `RepoEntry::immutable_heads()` — jj's `trunk()`, tags, and the trunk by name on every remote knives knows: `trunk() | tags() | remote_bookmarks(exact:"<trunk>", exact:"upstream") | remote_bookmarks(exact:"<trunk>", exact:"origin")`, plus the `release` remote when one is configured — named outright because `jj git clone` pins `trunk()` to `<trunk>@origin` and the default alias picks whichever trunk-named ref is newest. jj's default adds `untracked_remote_bookmarks()`, and in a fork that pin walls every member tip beneath a superseded release ref a fetch re-materialized, or beneath another fork's pull request head, while protecting nothing — no local rewrite reaches a remote. `knives start` writes the rule where none is stated, in jj's table form with a `doc` naming knives as the writer, and refreshes its own write when the entry's rule changes; a rule a human stated is never overwritten. Any stated rule that differs is reported with the config file as subject and both rules in the detail — the detail says which of the two cases it is. Absence is not reported, because `start` resolves it.

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
                               reconcile remote refs, open pull heads, recorded cuts, and
                               anonymous heads; reports only, never repairs
knives sync [REPO|--all]       fetch all remotes and tracked pull/N/head refs; classify each
                               tracked PR as new | unchanged | advanced | merged | closed
knives preflight [REPO]        programmatic pre-contribution facts (see below)
knives status [REPO|--all]     problem-first status map; aligned branch rows (branch, state,
                               tip, push, pr, review, checks, landed, claim, seen, notch),
                               grouped findings, and unmatched workspaces
knives pr NUMBER [--repo REPO] [--timeline]
                               one pull request's live state; --timeline reads its bounded forge event log
knives start BRANCH            claim, create the workspace, base it on the release's shared base (falling back to fetched trunk)
knives finish BRANCH           hand back claim and remove workspace; the branch, its bookmark,
                               and any open pull request survive the release
knives track BRANCH --pr N     state which PR a branch belongs to, overriding inference
knives depends BRANCH --on R#N  record that a branch cannot land before something else
knives notch [SUBJECT]         read what happened here (bare: newest 20 human notes plus a
                               folded machine-event count; a subject: its whole chronology);
                               -m writes a note, --disposition requires --evidence
knives release [NAME]          plan, cut, edit or reap a release under the configured scheme
knives release cut [NAME]      name a new cut of the composition in hand, verbatim (first cut: every branch); refuses to orphan commits or to silently drop members the previous cut's ledger event recorded ([--allow-drop] overrides); never pushes
knives release reap            reap superseded dated release bookmarks everywhere locally and abandon their commits; all kept while the live cut carries conflicts
knives release include BRANCH  add a branch (or revision) to the release as one new parent; nothing else moves
knives release drop BRANCH     remove a branch's parent from the release; the branch and its bookmark are untouched
knives release advance [BR..] [--from SHA]  move member parents to their branches' tips; named branches only, or every advanced member when bare; refuses a candidate that would replace more than one parent; --from names one branch's old parent directly, for a branch (e.g. `jj duplicate`-rebuilt) whose ancestry back to it is gone
knives release carries [REVISION] [--in TARGET] [--all]
                               bare replay is multi-target; --in selects one explicit target;
                               --all censuses maintained branches
```

TOON is the machine default on any command when the environment says an agent is running it (or stdout is not a terminal): agents were grepping human output to count findings by detector, and JSON answered that at more tokens than the same structure needs. `--json` forces JSON exactly; `--text` forces prose.

### Mutation verification

`knives pushed` queries live remote refs and judges each name only against the remote that owns
it: release names against the publish remote, ordinary branches and pull heads against origin.
It reports equal, missing, differing, and remote-only branch states, including a branch that was
deleted locally while its remote ref remained. It changes neither remote nor local state.

`knives audit` applies that same reconciliation across a repository (or every registry entry with
`--all`), then checks open pull heads unless `--no-github`, zombie remote branches, recorded
release-cut evidence, and anonymous heads. Its findings identify the observed drift; it never
repairs, deletes, pushes, or opens a pull request. Per-pull history remains the separate,
on-demand `knives pr <n> --timeline` read.

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
number, in any state, from any author: a PR opened before this tool existed, one the
maintainer closed because they wanted a different approach, or somebody else's that we
carry because ours was superseded.

### Dependencies

A branch can require a pull request in another managed repo. Dropping the required one
from a release without dropping the branch that needs it ships a release that cannot
work, which is exactly what happened when one repo's #4545 was dropped while a sibling's
#49 still needed it. Satisfied means merged; an open PR may still change or be rejected.

### `knives preflight`

Only the programmatic half. It reports facts and does not ask questions:

- which convention files the target repo has (`AGENTS.md`, `CONTRIBUTING.md`, PR template) and whether they changed since last seen
- our current open-PR count against that repo, and any numeric limit its policy states
- whether the branch is claimed, stale, landed, or divergent

Everything of the form "have you read the contributing guide, and does this PR comply" is **skill-side**. A CLI cannot evaluate compliance and should not pretend to.

### `knives start`

Workspaces are effectively free: 0.15 to 0.55s to create, because tracked content is small even in large repos (one 3.2G checkout was 19M across 1278 tracked files, the rest being virtualenvs and the shared `.jj` store). The real cost of a new workspace is rebuilding language environments, not checkout.

`knives start` bases new work on the release's **shared base** when a release exists, falling back to the fetched trunk when none does, never on the current `@`. The shared base is the trunk point every member forks from, so a branch started there composes into the release without dragging newer upstream into the cut. Moving the composition to a newer trunk is `knives release rebase`: an intentional, separate decision, never a side effect of starting a branch. Never `@`, because an agent in a release workspace who runs `jj new` would inherit the release merge as a parent.

`knives start` also states the fork's `immutable_heads()` (detector 11 has the rule and why) in the repository's own jj config when that config states none, refreshes the one it wrote earlier when the entry's rule has changed, and says so on stdout — naming a user-level rule the write now shadows, when there is one. knives' own library-side rewrites (`describe`, `abandon`, reap) keep jj's default pin set on purpose: a rebase may move a member out from under a stale ref; knives never rewrites what a remote still names.

A branch that does get rebased onto a newer trunk (a maintainer asks for it) is still its own member: `advance`, `include` and `drop` match a member to its branch by change id as well as ancestry (`MemberSuccession`), and fall back to the parent set the release's last cut or edit event recorded — a `parents` field naming every bookmark at each parent (`release_model::member_parents`), which is what answers for a branch rebased outside jj or landed upstream, where the repository itself no longer can. Reading a rebased branch as a stranger to its release is what led agents to keep a second "release-lineage" copy of each pull request branch. One branch, on the shared base, rebased only when someone decides to; the release follows.

### `knives release`

Cuts, edits or repairs a release under the repository's configured scheme. Everything here is a check, never a prompt: a CLI in a non-interactive agent session has nobody to ask.

A release's parent set is its membership: a flat merge of feature and fix branches, never the upstream base — members fork from it, so it is reachable through every one of them, and no base/member role exists to classify. `include`, `drop` and `advance` are the membership edits, each duplicating the release onto the changed parent set so recorded conflict resolutions carry forward; a cut names the composition in hand verbatim. Nothing recomputes membership from the branch list after the first cut.

`knives release carries REVISION` is the content answer rather than an ancestry guess. Bare
form compares the revision with every live release and the upstream trunk, reporting an exact,
rewritten, conflicted, or not-carried verdict with the commit the replay judged. This deliberately
changes the earlier release-in-hand contract: bare `carries` is multi-target, while `--in TARGET`
retains the explicit single-target query. It also answers when no release exists, against the
upstream trunk alone, rather than refusing; that is the only target needed to decide whether
unreleased work would be orphaned. A revision is safe only when a live release or the trunk
carries its content; a superseded release is consulted only after those targets miss and does not
make the result safe.

`knives release carries --all` turns that same check into a census. Its primary matrix contains
every maintained branch against the live releases and upstream trunk. Superseded releases are
checked only when that matrix is complete and every answered verdict is not-carried; an unanswered
row stays unanswered. Anonymous heads belong to `knives audit`'s orphan-commit detector.

The `orphans` list may contain qualified unknown entries: a branch's content is not carried
anywhere, but its unanswered pull state makes it a deletion-unsafe candidate. The row's `orphan`
field is three-state: `true` is a deletion-safe proven orphan, `false` is not an orphan, and
`null` means carriage or pull state is unanswered. Consumers must handle `null` as incomplete.

`knives release members [REF]` reads the release's direct parent list, which is the membership
source of truth, and names every bookmark still holding a parent plus branch tips that advanced
beyond one. `--verify` replays every member's content into the release, reports missing and
unexplained audit buckets, and makes either bucket a finding. Parent counts come from the
repository's parent list, not text that happens to look like a parent declaration.

Every mutating verb applies as **one jj operation**, written through jj-lib in a single transaction and described in the operation log as knives' own act (`knives: release/X: included feat/y`, `knives: cut release/X`, `knives: reap …`) rather than as a trail of raw `jj` invocations. Failure before the commit writes nothing, so a half-applied edit is unconstructible. A cut audits a **candidate** built in a scratch transaction that is never committed: a failed audit evaporates without a trace, and a passing one rebuilds the spec, verifies the published tree is identical to the audited one, and creates-and-names the release in one operation. Git refs are exported in the same step, so `git` and `gh` in a colocated checkout see the result immediately. Rewrites honor jj's stock `immutable_heads()` (trunk, tags, untracked remote bookmarks) — the reap flow depends on that refusal. Identity for written commits resolves the way the jj CLI resolves it (`JJ_USER`/`JJ_EMAIL`, repo config, user config); every behavioral setting stays at jj's defaults. Only `jj git fetch` and the composition rebase (`release rebase`, defined as `jj rebase -b <release> -d <target>`) remain subprocess calls, deliberately (#18).

- **Support both dated and fixed release schemes.**
  - **Dated scheme (default):** `knives release` inspects consumer pins to decide whether a release is pinned. Pinned releases require a new dated suffix (`release/YYYY-MM-DD`); unpinned releases repair in place. `knives release cut NAME` executes the cut.
  - **Fixed scheme (`release_branch` set):** `knives release` takes no name argument and advances the fixed branch in place via a sideways bookmark move (`jj::set_bookmark_anywhere`). Publishing remains a manual `jj git push`. The cut carries the *local* composition in hand under this scheme as under the dated one (`release::previous_release_for_cut`), unpushed edits included; reading the publish remote instead once made a fixed cut duplicate the stale published position and silently revert them. The published position is read separately from the publish remote-tracking reference (`{fixed}@release` or `{fixed}@origin`) and reported alongside (`release::previous_position`). Fixed release selection considers only the local fixed branch and its publish-remote counterpart.
- Build from explicit commit IDs and verify the parent count before pushing.
- Expect a real conflict and resolve it in the merge. Independent branches that each append a config key land in the same regions; one ten-parent cut carried a 4-sided conflict in one file and a 3-sided one in another. Resolve as a union, keep a shared helper defined exactly once, and land a config key in every loader.
- Compare the merged test count against a single contributing branch. A total lower than one branch's own count means a branch's tests were dropped.
- Stamp per-parent provenance recording where each parent came from — the branch holding it, the trunk it descends from, or its own id — so `knives sync` knows what to check. This records provenance and pins nothing, since a jj octopus's parents are already specific commits.
- **Clean up workspaces whose branch no longer exists anywhere.** A branch the cut did not carry is not dropped, merely not a member; only a workspace with no bookmark left holding its branch is cruft.

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
30 seconds and keeps the session moving — which without the watchdog leaves the process parked in
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

The OMP extension uses Pi's native `bash` implementation rather than replacing it. OMP exposes no
session environment variable to tool shells; its bash output is not a terminal, so CLI output is
machine-readable through the non-terminal fallback. Commands that need an owner use
`KNIVES_OWNER`, then the Claude Code session identifier, then the managed working directory's
recorded owner, before falling back to the operating-system user.

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
- **Inject only root-level guidance from a trusted repo**, plus our own overlay, which lives outside the repo. A nested `AGENTS.md` inside a trusted repo is *mentioned, not injected*.
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

- Closed-not-merged while the branch lives on, which staleness bots produce. Distinguish from supersession and from a deliberate fork-only branch.
- A foreign `pull/N/head` advancing under a release, and whether re-cutting should be automatic or offered.
- Whether the claim gate's hard refusal earns its cost. The finish guard's did not: holding a claim through review blocked other agents for nothing, since a released branch, its bookmark, and its pull request all survive.
- Workspace lifecycle beyond what `knives finish` cleans up. They are cheap to create, which is why they accumulate.
- Codegraph integration. A stale index answers queries with silence rather than a warning, and `sync` costs 415ms, so any integration must sync before querying. Deferred; not important to resolve now.
