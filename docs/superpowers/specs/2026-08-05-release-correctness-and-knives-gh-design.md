# Release correctness and knives gh

Two PRs, independent of each other, closing the whole backlog. PR 1 is release
correctness: #4 (cut must never lose content), #7 (reap superseded cuts),
#9 (test coverage gaps), #10 (shared-base invariant), #11 (rebase accumulates
parents). PR 2 is #8 (knives owns the fork-shaped logic in the gh shim).
Issue #9 item 3 (observe the status pr-column collapse on a live forge) is not
a code change; it gets a closing comment and an eye kept on it.

## Decisions settled with Sami (2026-08-05)

- **#8 shape:** knives absorbs the brain. `~/.dotfiles/shims/gh` becomes a
  thin exec wrapper around `knives gh`; agents keep typing `gh` and never see
  the difference. The Python credential helper (`gh-app-token`) and the
  routing table (`gh-app-routes.gitconfig`) stay exactly where they are.
- **#7 divergence noise:** fix the reader, not the graph. Repos must survive a
  bare `jj git fetch` by any agent, so correctness cannot depend on knives
  being the only fetcher. The divergence detector learns to ignore commits
  whose only visibility comes from superseded release refs.
- **#10:** shared base is the invariant. Every member branch forks from one
  base commit; advancing it is a deliberate act.
- Minimal PR count is preferred; the release work lands as one PR because the
  pieces interact (reaping must never abandon what the pre-cut gate protects).

## Evidence gathered (this session)

Experiments in a throwaway repo on jj `0.43.0-sami.20260722`:

1. `jj bookmark forget --include-remotes <name>` then `jj abandon <commit>`
   reaps a superseded cut; the remote branch is untouched. Ordering is
   load-bearing: forget alone leaves the tracking ref pinning the commit and
   abandon refuses ("would rewrite immutable commits").
2. A later `jj git fetch` touching that remote re-materializes the ref as
   untracked, and re-forgetting does not stick: jj has no memory of forgotten
   refs, so every fetch brings the ref back as long as the branch exists on
   the remote. `knives sync` fetches `--all-remotes`, so routine syncs do this.
3. The re-materialized ref is an untracked remote bookmark, which is an
   immutable head. It does not appear in the default `jj log` revset. The
   human-visible graph noise is fully fixed by cut-time reaping alone.
4. `Repo::divergent_changes` (src/jj.rs) walks `repo.view().heads()`, which
   includes untracked remote refs, so divergence findings resurrected by a
   fetch would return without the detector filter.
5. `jj abandon` on a commit with descendants rebases the descendants onto the
   abandoned commit's parents and can manufacture conflicts. The "refuse when
   local descendants exist" gate is load-bearing, not paranoia.

Field reports from fork workspaces, confirmed in code:

6. `run_rebase` (src/main.rs) carries every existing release parent verbatim
   and appends the new upstream commit: parents accumulate, stale bases are
   kept forever. Its already-contains guard compares direct parents by
   identity instead of ancestry. (#11)
7. `knives start` bases new branches on the fetched upstream trunk tip
   (src/commands/start.rs, deliberate per its comment), which breaks the
   shared-base invariant and drags newer upstream into the next cut. The
   observed 15-conflict cut was this. `stale_parents` (src/detect/) flags the
   octopus's bookmarkless base parent as stale. (#10)
8. Observed live in a managed fork workspace on this machine: an agent
   manually forgetting fifteen superseded `release/*` bookmarks and
   abandoning their commits, by hand.
   This is #7's workflow, done manually, today.
9. Superseded release refs freeze history, not just clutter it: untracked
   remote refs are immutable heads, so every member commit in their ancestry
   is immutable and `jj rebase` refuses to rewrite it. Repairing a pre-knives
   fork (19 branches onto one shared base) required hand-forgetting ~146
   historical release refs across two publishing remotes and abandoning the old
   release commits before `jj rebase` would run at all. Reaping is
   the rebase unlock, not an aesthetic sweep.

## PR 1: release correctness

Closes #4, #7, #9, #10, #11. Files: `src/commands/release.rs`, `src/main.rs`
(`run_rebase`), `src/commands/start.rs`, `src/detect/stale_parents.rs`,
`src/detect/divergence.rs`, `src/jj.rs`, tests.

### 1.1 Ancestry, not identity

One helper answering "is X reachable from Y" (`jj log -r 'X & ::Y'` or
jj-lib's index), used by everything below: the trunk-containment probe (#4
third bullet, currently false-negatives when trunk is contained via a parent's
history), the rebase already-contains guard (#11), and base classification
(#10). Identity comparison of commit ids remains only where identity is the
question.

### 1.2 Pre-cut gate (#4)

Before building the merge: commits reachable from the previous release **or
its local descendants** but unreachable from member tips plus trunk are an
error listing the exact commits. Default deny; `--allow-drop` overrides after
the operator has read the list. This catches all three observed loss modes:
work living only in the release lineage, and commits stacked on a release
merge instead of a branch.

### 1.3 Post-cut audit (#4)

After building the candidate merge but **before** moving the release bookmark
or pushing: for each member branch, verify its changed hunks are byte-present
in the cut tree (same mechanism as the landed probe, pointed at the fresh
cut), and tree-diff the previous release against the new cut for content that
vanished without being conflict resolution. On failure the candidate merge is
abandoned and the cut errors; nothing was published, so the failure is cheap.
This is what would have caught the silently auto-merged-away fix on a moved
file and the wrong side of uv.lock.

### 1.3b Incremental cut construction (#12, Sami field report mid-execution)

Building every cut as a fresh flat octopus re-surfaces every previously
resolved conflict — the operator re-resolves the same regions on every cut.
A jj merge commit records its tree as a resolution-diff against the
auto-merge of its parents, so duplicating the PREVIOUS release onto the new
parent set preserves all prior resolutions (verified empirically, jj 0.43):
adding a branch or advancing the base surfaces zero old conflicts; dropping
a branch whose content was baked into a resolution surfaces one focused
conflict at exactly that spot (the safe behavior — the alternative silently
ships the dropped branch's lines, #4's inverse).

- `release cut`: when a previous release exists, the candidate is
  `jj duplicate -r <previous> -d <trunk> -d <tips…>` followed by a
  `jj describe` to the cut message; the fresh flat merge remains only for
  the first cut. The result is still a flat octopus of (trunk + member
  tips) — the non-negotiable shape is unchanged; only tree construction is.
- `release rebase` shares the primitive: the repaired release is the old one
  duplicated onto (kept parents + onto), not a from-scratch merge.
- The post-cut audit (1.3) is the complementary guard for resolution drift
  the duplicate path can carry.

### 1.4 Reap superseded cuts (#7)

After a successful cut (bookmark moved and pushed), every other dated release
bookmark is reaped: `jj bookmark forget --include-remotes <name>`, then
`jj abandon <commit>`, in that order. The enumeration matches the repo's
dated-release scheme on **any** remote (historical refs accumulate on more
than one publishing remote on real forks; upstream's own semver-style
`release/0.3.x` branches do not match the dated pattern and are never touched). Gates, each refusing that bookmark only:

- never the newest cut (just made);
- never the ref `release::previous_position` reads — that ref is the seam the
  fixed scheme depends on;
- never a cut with local descendants (#4's third loss mode; also evidence
  item 5) — those descendants are someone's work.

The remote is never touched; dated release branches are a published contract
(consumers pin them), so remote deletion is permanently out of scope.

The same sweep is exposed as `knives release reap`: operator-invoked,
idempotent, identical gates. It exists for pre-knives repos carrying years of
historical refs, and as the documented unlock when a rebase needs old-lineage
commits mutable (evidence item 9) — one command instead of ~146 hand-rolled
forgets. Consistent with the bare-fetch decision: correctness never depends
on it running; a later fetch re-materializing refs as untracked is expected
and harmless (evidence items 2-3), and re-running reap clears them again.
Cut-time reaping remains automatic and sweeps re-materialized refs too.

### 1.5 Divergence detector ignores superseded release refs (#7)

When `divergent_changes` collects heads, it skips heads whose only references
are dated-release refs (local or remote, tracked or untracked) that are
neither the newest release nor the previous-position seam. Re-materialized
refs after any fetch, by any tool, no longer resurrect findings. `bookmark
list --all-remotes` will still show re-fetched superseded refs; that is
accepted cosmetics, invisible in the default log (evidence item 3).

### 1.6 Shared-base invariant (#10)

The shared base is the newest trunk-reachable parent of the newest release.

- `knives start` bases new branches on it. Only when no release exists does
  it fall back to the fetched upstream trunk tip (current behavior).
- Pre-cut and preflight gain a mixed-base finding: a member whose trunk
  ancestry exceeds the shared base is named together with its actual base.
  This finding is what would have saved the agent in evidence item 7 an
  afternoon of conflict archaeology.
- `stale_parents` exempts the base parent instead of reporting "carries no
  bookmark". Older trunk-reachable parents (accumulated by the #11 bug) get
  their own finding naming them superseded bases.

### 1.7 Rebase replaces the base (#11)

`run_rebase` swaps every trunk-reachable parent that `onto` descends from for
the single `onto`, keeping branch parents. One replacement covers the normal
case (one old base); several covers merges already damaged by the
accumulation bug, so rebasing them self-heals. The already-contains guard
becomes an ancestry check. A stale parent the rebase cannot map to a current
bookmark tip is a refusal with an explanation, not silent carriage.

### 1.8 Tests (#9)

- Integration test for `release rebase`'s frozen-pin refusal path:
  `repair_effect(..) == NewDatedName` refuses and directs to a new dated cut
  (#9 item 1).
- Back-compat test: a hook session-state document written before
  `owner_remotes` existed still loads (#9 item 2).
- Integration coverage for each new gate above: pre-cut denial and override,
  post-cut audit failure abandoning the candidate, reap with each gate
  triggering, mixed-base finding, base-parent exemption, rebase base
  replacement and stale-parent refusal.

#9 item 3 (same-number pr-column collapse against a live forge) is
observation, not code; closed by comment when observed.

## PR 2: knives gh (#8)

Closes #8. New files under `src/commands/` plus `src/cli.rs` wiring; a
dotfiles commit (no PR; separate repo) shrinks the shim.

### 2.1 Command

`knives gh -- <args...>` (the `--` is required so gh's own flags are never
parsed as knives'; the shim invokes `knives gh -- "$@"`). It is plumbing
agents never see: the shim calls it.
Pipeline:

1. **Target resolution.** `-R/--repo` wins; else `gh repo set-default`
   resolution (`remote.<name>.gh-resolved`); else remote preference. In a
   registered fork, preference comes from knives' `Role` concept
   (`RepoEntry::remote(Role)`); otherwise the generic `upstream, github,
   origin` order the shim uses today. SSH/HTTPS remote URL normalization via
   the existing `hook::resolve` helpers.
2. **Owner extraction for API calls.** REST paths (`repos/{owner}/...`,
   `orgs/`, `users/`) and GraphQL bodies (`repository(owner:"X")`,
   `organization(login:"X")`), ported from the shim with its behavior table
   as Rust unit tests.
3. **Token.** If `GH_TOKEN` is unset: query git's own config for the
   credential helper matching the target
   (`git config --get-all` on the credential-helper key for the forge host),
   and when it names `!gh-app-token <profile>`, mint via `gh-app-token
   <profile> get` and set `GH_TOKEN` for the child. No match leaves `GH_TOKEN`
   unset, exactly like today. The routing table stays in
   `gh-app-routes.gitconfig`; knives reads, never owns.
4. **jj detached-HEAD compensation.** For the `gh pr` subcommands that need a
   current branch, when git reports no symbolic HEAD (jj-colocated repos),
   inject the current bookmark as `--head` or positional argument, porting the
   shim's per-subcommand table.
5. **Exec the real gh**: first executable `gh` on PATH whose canonical path is
   not the shim itself.

### 2.2 What does not move

`gh-app-token` stays Python and stays pinned to system `python3` (venv breaks
PyJWT); it is a git credential helper wired through gitconfig, and git itself
invokes it — that contract is untouched. The org-prefix routing table stays in
gitconfig as the single source of truth for both git and knives.

### 2.3 Degradation outside registered forks

The shim covers every repo under the routed org prefixes, so `knives gh` must
too: target resolution, token minting, and head injection all work from git
remotes plus gitconfig alone. The registry, when present, only sharpens remote
preference. In a directory that is not a git repo at all, `knives gh` execs
real gh with arguments untouched.

### 2.4 Shim shrink and rollout

After the knives release containing `knives gh` is installed, the shim body
becomes: exec `knives gh "$@"` when knives is on PATH, else exec real gh
(found the same way it does today). Parity is verified by running the shim's
existing behavior matrix against both implementations before the dotfiles
commit lands.

## Out of scope

- Deleting superseded release branches on any remote (published contract).
- Rewriting `gh-app-token` in Rust.
- Making `knives sync` re-reap re-materialized refs (rejected: repos must
  stay correct under bare `jj git fetch`, so nothing may depend on knives
  being the fetcher; the detector filter makes re-reaping unnecessary).
- `knives gh` as a documented agent-facing command; it exists for the shim.
