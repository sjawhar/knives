# `knives`: multi-fork, multi-agent maintenance

## Goal

Make the state of a fork cheap enough to query that no agent has a reason to guess, and make collisions between agents visible before they cost work.

The motivating setup is seven forks of one upstream ecosystem, each carrying between one and thirteen open upstream PRs as independent branches, integrated into dated flat octopus merges that a consumer repo pins. Several agents work these repos concurrently on one machine. Nothing below is specific to that setup; the tool is configured per repo and holds no knowledge of any particular user, org, or upstream.

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

Per repo, `fork` knows a set of **remotes by role**:

| Role | Purpose | Required |
|---|---|---|
| `upstream` | the repo we contribute to; fetch only, including `pull/N/head` | yes |
| `origin` | where our branches get pushed | yes |
| `release` | where dated `release/*` bookmarks get pushed | no, defaults to `origin` |

Most repos need two roles. A third exists only when branches and releases must live in different places, which happens when the upstream cannot push to our fork (GitHub does not offer "allow edits by maintainers" for organisation-owned forks, so PR branches sometimes have to live on a fork the maintainer can push to, while releases live somewhere with different ownership). That is one configuration, not the model.

**Verified:** splitting the roles across two remotes works with no mirroring step. A branch pushed only to the branch remote, made a parent of a release octopus pushed only to the release remote, was fully resolvable from a fresh repo fetching only the release ref. The push carries parent commits as objects.

Release parents are **upstream PR refs, not necessarily our branches**. `git fetch upstream pull/N/head` costs 0.48s and needs no fork. Carrying a maintainer's PR instead of ours, or a PR that was never ours, is the same operation as carrying our own.

Two remotes are required and a third is optional. `upstream` is what we contribute to.
`origin` is our own copy, which for most people is the whole story. `release` is a
separate remote that dated releases are cut on, for the case where releases are consumed
internally and should not sit in a personal fork; the release remote is optional, and it
falls back to `origin` when absent, because not every fork is consumed by anything.

`consumers` is optional too: the checkouts that pin this repo's releases. A list, because
a fork can be consumed by several things at once and they can sit on different releases.
the pin logic always reasoned about that while the registry could only record one of them,
so one consumer being current silently answered for all of them. Recorded so that
`knives repos` can say which consumer is pinned behind the newest cut without being asked,
since nobody runs a command to answer a question they have not thought of.

## State

Compute anything cheap. The detectors below are all sub-second and local, so they run on demand and nothing caches them.

Store only what no amount of computing can recover:

- who is working on what, and why (the repo cannot know this; and it cannot be inferred from session working directories either, since an agent launched elsewhere may need to change a fork)
- why we carry a foreign PR as a release parent
- supersession pointers, when one of our PRs closes in favour of another
- **fork-only marks**: a branch we deliberately keep with no upstream PR. This should be the minority, but it is real, and it covers CI we want on our fork but not upstream. Without a mark, every such branch reads as an error in `knives status` forever.

## Detection rules

Eight detection rules, all resting on mechanical fields and graph queries rather than reasoning:

**1. Stale release parent (`stale-parent`).** Rests on `Repo::bookmark_tips` compared against release parent commits. When a PR branch is rebased upstream, jj moves the local bookmark to the new commit but the octopus keeps the old one, leaving a parent whose bookmark has moved to a descendant. The release then ships pre-rebase code with nothing in the bookmark list saying so.

**2. Landed upstream (`landed`).** Rests on `classify_landed`, which replays the branch onto `main` in a temporary commit and inspects the tree diff:

| Result | Meaning |
|---|---|
| empty | landed verbatim, drop it |
| conflicted | landed but modified by the maintainer, drop after a human reads the delta |
| clean and non-empty | not landed, keep carrying |

Authorship- and PR-number-agnostic, which matters because our work sometimes lands under someone else's PR number.

**3. Divergence (`divergence`).** Rests on `Repo::divergent_changes`, querying whether a single change ID maps to multiple commit IDs across disconnected clones or local rewrites. The general rule: a change rewritten while any other reference still points at its old commit diverges. Divergence is routine, but the observed failure is agents reading `/0`, `/1` suffixes and `??` bookmarks as corruption and stopping.

**4. Double checkout (`double-checkout`).** Rests on `Repo::workspaces`, checking if two workspaces hold `@` on the same change ID, visible in `jj workspace list`.

**5. Failing CI checks (`checks-failing`).** Rests on `ChecksSummary::failing()`, checking for red conclusion states (`FAILURE`, `TIMED_OUT`, `CANCELLED`, `STARTUP_FAILURE`, `ACTION_REQUIRED`, or `ERROR`) on open pull requests. The `ERROR` conclusion is what external CI posting commit statuses emits for an aborted or infrastructure-failed build, and missing it made a red pull request read as clean green.

**6. Wrong target base (`wrong-base`).** Rests on `PullRequest::base_ref_name` against `RepoEntry::default_base()`, flagging open pull requests targeting a branch name other than the expected base. It cannot tell a pull request aimed at our fork's `main` from one aimed at upstream's `main` because both are named `main`, the forge exposes no base-repository field, and `gh` resolves to upstream anyway. An empty base is unknown, not wrong. Only open pull requests are checked.

**7. Commits carried elsewhere (`carried-elsewhere`).** Rests on `Repo::branches_containing(tip)`, querying whether the branch tip is reachable from another reference. It reports where found and says nothing about what it means: whether a maintainer took the work, rebased it, or coincidentally landed the same content is the reader's judgment. Our own release cuts, `@git` refs, and trunk are excluded because releases contain these tips by construction, and reporting that buried the real signal.

**8. Branch file overlap (`branch-overlap`).** Rests on `jj::changed_files_between` path sets computed from `fork_point(trunk | branch)`, grouping files modified by two or more active branches. One finding per file, naming every branch. It is a path comparison and nothing more.

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

**2. Landed upstream.** Ancestry-based detection fails for squash merges, which is the common path. Verified: after a squash merge the branch's content was in `main` while `main..branch` still reported one file changed, indistinguishable from an unlanded branch. Rebase the branch onto `main` and read the result:

| Result | Meaning |
|---|---|
| empty | landed verbatim, drop it |
| conflicted | landed but modified by the maintainer, drop after a human reads the delta |
| clean and non-empty | not landed, keep carrying |

## Command surface

Every command takes the repo from the directory you are standing in. Naming it is for when you are somewhere else, or want a different one. Requiring the name everywhere was the loudest complaint from actually using this.

```
knives init [DIR]              configure remote roles for a repo; verify one repo per fork
knives repos                   the repos knives manages, their release state, and whether a
                               recorded consumer is pinned behind the newest cut
knives sync [REPO|--all]       fetch all remotes and tracked pull/N/head refs; classify each
                               tracked PR as new | unchanged | advanced | merged | closed
knives preflight [REPO]        programmatic pre-contribution facts (see below)
knives status [REPO|--all]     per branch: local tip, origin tip, PR number, review decision,
                               whether the review predates the head, claims, active workspaces,
                               and the detectors. One line per kind of finding; --verbose for one per finding
knives start BRANCH            claim, create the workspace, base it on fetched main
knives finish BRANCH           hand back claim and remove workspace
knives track BRANCH --pr N     state which PR a branch belongs to, overriding inference
knives depends BRANCH --on R#N  record that a branch cannot land before something else
knives release                 plan a dated release
knives release cut NAME        cut it; the only thing knives writes, and it never pushes
knives release include BRANCH  state that a branch belongs in the next release
knives release drop BRANCH     state that it does not; survives the every-branch fallback
knives release rebase [REF]    add an upstream commit, keeping the branch parents
```

`--json` on any command, and it is the default when the environment says an agent is running it. Agents were grepping human output to count findings by detector.

`knives repos` and `knives status` are deliberately separate: one answers "what am I maintaining", the other "what is the current state and what is being worked on right now". Conflating them was an earlier mistake in this design.

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

`knives start` always bases new work on **fetched `main`**, never on the current `@`. That single default removes the most common accident: an agent sitting in a release workspace runs `jj new` and silently inherits the release merge as a parent.

### `knives release`

Cuts or repairs a dated release. Everything here is a check, never a prompt: a CLI in a non-interactive agent session has nobody to ask.

- **Determine whether the release is pinned by inspecting the consumer's pin locations.** Pinned means frozen and the next cut takes a new dated suffix; unpinned means repair in place. Nothing about this requires asking a human, and a needless dated name burns the name and forces a re-pin nobody wanted.
- Build from explicit commit IDs and verify the parent count before pushing.
- Expect a real conflict and resolve it in the merge. Independent branches that each append a config key land in the same regions; one ten-parent cut carried a 4-sided conflict in one file and a 3-sided one in another. Resolve as a union, keep a shared helper defined exactly once, and land a config key in every loader.
- Compare the merged test count against a single contributing branch. A total lower than one branch's own count means a branch's tests were dropped.
- Stamp per-parent provenance recording which PR ref each parent came from, so `knives sync` knows what to check. This records provenance and pins nothing, since a jj octopus's parents are already specific commits.
- **Clean up workspaces belonging to branches the cut has dropped.**

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

Each session records `noticed` and `guided` flags for every repository. The Claude Code adapter
marks only `noticed` at `SessionStart`, then marks guidance after a relevant foreign-repository
tool use. It omits guidance when the event working directory is in that repository, because
Claude Code already loads the session repository's `CLAUDE.md`. The OpenCode adapter marks both
flags after any nonempty addition, so its notice and guidance consume one budget.

The OpenCode protocol fails soft. For a parsed event, a processing failure returns that event's
empty response envelope. Malformed input returns an empty object. Both cases keep the hook exit
successful.

### Trust boundary

Instruction injection is bounded to the instance directory:

```
while (current.startsWith(root) && current !== root)   // root = instance directory
```

For a session rooted in one repo reading a file in another, that is false on entry and nothing is injected. An agent can therefore read, edit and open PRs against a fork while that fork's `AGENTS.md` is never in context. That is problem 4, and it is mechanical rather than careless.

**The boundary is a security control and this design must not defeat it.** If any read injected the read file's directory guidance, reading untrusted content would become a prompt-injection vector: an adversarial `AGENTS.md` arrives as a `<system-reminder>`, which a model treats as instruction rather than data. The risk is acute for anyone who authors adversarial fixture trees on purpose.

So the adapters re-establish an equivalent boundary rather than removing it:

- **The allowlist is the registry, which names two kinds of tree.** `[repos.*]` is what we maintain forks of. `[trusted.*]` is a repository whose instructions we want an agent to see but which we do not maintain, such as a company repo with no upstream to contribute to. Both are trust roots for guidance; only the first is touched by any fork command. Any other tree gets nothing: fixtures, scratch clones, downloaded repos.
  - Two sections rather than one section with optional remotes. A fork entry must carry `upstream` and `origin`, enforced when the file is parsed; relaxing that to fit a non-fork repository would trade a parse-time failure for a failure at the first query. Both sides of the tool have to know the section exists: an unrecognised header invalidates the whole registry in the configuration parser, so one trusted entry would otherwise disable guidance everywhere, and serde would drop the section the next time `init` rewrote the file.
- **Inject only root-level guidance from a managed repo**, plus our own overlay, which lives outside the repo. A nested `AGENTS.md` inside a managed repo is *mentioned, not injected*.
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

When a relevant call names a file inside a managed repository, the adapters can produce these
two parts:

- **A notice.** That this is a knives-managed fork, that another agent may be working in
  it, which branches are claimed and by whom, and to use knives rather than jj or git
  directly. This is the one place the tool addresses the reader directly. It is emitted
  even when the repository has no `AGENTS.md`, which used to mean nothing was emitted at
  all and an agent was never told where it had walked into.
- **The repository's own guidance**, when it has any, framed as data.

Triggered by files a call actually names, `path`, `filePath`, or an absolute or `~`-rooted path
in a command, and deliberately not by a command's working directory. A
batch of `gh` and `git` calls whose working directory sat inside a repository used to
spend that repository's one injection while touching no repository content, so a later
read of a real file got nothing.

The record is stored in a file for each harness and session. Its per-repository `noticed` and
`guided` flags survive calls made through different OpenCode plugin instances and prevent a
repository from spending the same announcement budget more than once.

### OpenCode configuration

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
The Claude Code shell hook resolves `knives` from `PATH` and exits silently when it is absent.

## Enforcement

Layered, cheapest first, escalating only on evidence:

1. **Default-correct paths.** `knives start` bases on `main`; `knives preflight` before contributing.
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
- Whether hard refusal earns its cost.
- Workspace lifecycle beyond what `knives finish` cleans up. They are cheap to create, which is why they accumulate.
- Codegraph integration. A stale index answers queries with silence rather than a warning, and `sync` costs 415ms, so any integration must sync before querying. Deferred; not important to resolve now.
