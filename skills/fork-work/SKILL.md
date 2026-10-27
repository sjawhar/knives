---
name: fork-work
description: Use when you are about to work in, read or investigate a repository that can be a fork managed by knives. Use it before `knives start`, before any change, fix, refactor or test there, and before you clone, re-clone or make a scratch checkout of one. Use it when asked which branch to use, whether another agent works somewhere, or how to get a working copy. This applies even to ordinary coding or code reading.
---

# Working in a managed fork

If this skill disagrees with the code, a command's output or its `--help`, follow those and say which line of this skill is wrong.

## 1. Find out if it is a managed fork

Do this before you read code too: a conclusion from the wrong copy is wrong, and nothing tells you.

- Run `knives repos`. Listed (the repository you are in, or will clone): a managed fork. Other agents share it, it has an upstream, and a working copy exists. Not listed: this skill does not apply.
- `not on this machine` is a scan miss, not proof of absence. The scan covers `~` three levels deep, only jj checkouts, and skips dot-directories and symlinks. Before you clone, look deeper or elsewhere: `cd` into a possible checkout and run `knives status`. Every command except `knives repos` binds the checkout you stand in.
- A row that names two checkouts is a refusal. Do not choose between them. Ask.
- Must be managed but not registered: run `knives register` in it, and give the snippet to the human to paste into `repos.toml`. Never edit `repos.toml`. `already registered as <name>` means the list has it under that name.

## 2. Read the status and the notches

- Run `knives status` in the repository, with no argument. It shows each branch, its PR and PR state, whether it is in the upstream trunk, and the claims of other agents. It shows facts. The judgment is yours.
- Read the claims before you touch anything. Another agent holds your branch: that is a collision. Find it now, not after one of you loses work.
- `knives notch` shows recent claims, stated PRs, dependencies, releases and notes. Before you touch a branch you do not understand, run `knives notch <branch>`. The reason for an odd branch is usually one line there.
- Each entry carries the branch tip of its time. An entry at a commit that the branch moved past is a reason to check again, not a conclusion.
- The workflow that wrote a note's prefixes defines them. A note that says it is open is open.
- Record a decision worth keeping before you continue: superseded, parked by the owner, a promise to a reviewer, a PR moved to another branch. Cite a commit or `file:line`, because claims without one were the false ones:
  `knives notch <branch> -m "what you decided and why" --evidence <commit-or-ref>`

## 3. Challenge the placement before `knives start`

**Ownership test.** The author applies it here. The release owner applies it again at include time. *If the fork were replaced by upstream main tomorrow, would anyone but us miss this?* No: it belongs in the consumer.

- The consumer owns its deployment's operating policy: job lifetimes and reaping, caps, quotas, schedules, alerting, node sizing, who-may-do-what defaults. Put policy in its infrastructure: its scheduled jobs and janitors, or a config value the library already exposes.
- A fork member is for a defect in the library's own machinery. It is also for a capability nothing outside the library can give: a missing field, an endpoint, a knob that keeps upstream's default. That makes it upstream-bound.
- Fix where the policy has its owner, not where the mechanism is. Code in the fork does not put its policy in the fork.
- The fork looks like the right home for policy: that is a missing library capability. Ask the owner what goes upstream and how the fork carries it until then, never whether the fork can keep it without upstream.

**Challenge.** Before you make a branch, dispatch a fresh-context subagent. Knives records its verdict but does not judge or spawn. Brief it to argue that the change does not belong in the fork. The consumer's config, IaC or code gets the effect, or any user hits the defect. Give it the facts for answers 1 and 2 below. It returns one verdict with its reasoning:

| Verdict | When | Do |
|---|---|---|
| CONSUMER | The consumer gets the effect, a knob the library already exposes, or a change only we want. | No branch. Make the config value, stack transformation or consumer-side job. |
| UPSTREAM | Fork-member material (above), with the proof of answer 3. | A fork member on the release's shared base that also goes upstream (§4). |

- There is no third verdict. Never offer or record "fork-only", "not upstream-bound" or "a fork member that never goes upstream".
- Read-only investigation is exempt. A green gate, a plausible patch or a peer's "go" does not establish ownership. Placement is not a question for the human.
- Ownership not established: investigate. Never relabel an unsupported bug as "hardening". An unproven fix needs its evidence (a reproduction or a red→green test) before it counts.
- Reuse a valid `placement:` notch. Check it again when the evidence or scope changes.
- Record the verdict as the branch's first `placement:` notch. For a CONSUMER verdict with no branch, or a standing rule, the repository is the subject:
  `knives notch <branch> -m "placement: verdict: CONSUMER|UPSTREAM; proposer <id>; red-teamer <id>; <four answers>" --evidence <revision-or-url>`
  1. The observed failure or missing capability and its revision, not an inherited summary or counter.
  2. Our config, caller or open feature branch, or upstream: chosen after you check the real launch path, installed version, options and lifecycle or recovery mechanisms.
  3. Bug: a minimal reproduction on unmodified upstream at a cited revision, without our fork checkout or application patches. Feature: the general extension case that keeps upstream defaults.
  4. Why upstream review, CI, rebase and maintenance are worth it.

**A member that changes runtime behavior** (anything a production operator notices):

- Its `placement:` notch **and** its commit body state why it does not live in the consumer. That is the ownership test's answer, or the owner's ruling that put it in the fork. They state the recorded decision it implements (the ruling's id or link), or that it implements none and is a defect fix with red→green evidence.
- Build the ruling's exact words. "Idle" ruled and "from creation" built is the whole behavior, not a nuance.
- With neither statement, the release owner calls it `neither` and does not compose it (`maintaining-fork-release` step 5).
- The consumer's pin PR lists each production behavior change it carries, in plain numbers, and which deployments carry it, under `## Production behavior delta`. This is disclosure, not a gate.

## 4. Ship the fork member, and open the upstream PR alongside

- An upstream PR is a placement judgment, not a permission. The UPSTREAM `placement:` notch and the PR body are the written judgment. Ask nobody. Wait for no approval.
- The default for every fix is a fork member. To use it takes as long as to make it, in the same line of work:
  1. Push one signed commit on the release's shared base to the fork remote. Run `knives notch`.
  2. Give the tip and red→green evidence to the release-claim holder, who includes it in the release in hand and publishes. Every consumer pin of that release frozen on a revision (such as `rev = "release/…"`): the holder first cuts the next dated name, immediately. Nobody holds the claim: do this yourself.
  3. The consumer pins the published name and deploys. The fork member carries the change until upstream ships it.
- The release owner's process opens the upstream PR after the notch records the verdict, from a signed commit with an `Omp-Session` trailer. It gates nothing: not the release, the pin or the deploy. Never wait for upstream review or "the next cut".
- The include-time ownership test checks placement. It is not a review that the fix waits in.
- After the upstream PR opens, its review rounds, body edits, rebases and close are the owning session's work. Never park them on a human. The one exception is text the user's own rules send to them first: batch it into one ask, which is approval, not parking.

## 5. Get your workspace with `knives start`

- `knives start <branch> --why "what you are doing"` claims the branch and makes a jj workspace. An existing branch: on its tip, with `@` an empty child. A new branch: on the release's shared base (the upstream trunk when no release exists), so it composes without a rebase.
- A `start` that pauses waits for another agent's claim lock. Let it. A refusal names the holder. `using-knives` has the messages.
- As soon as your active work stops, also to wait for something external such as PR review, run `knives finish <branch>`. It returns the claim and removes the workspace. Nothing is lost: jj keeps the work as a commit by change id, and the branch, bookmark and open PR stay.

## 6. Never improvise

| Never | Do |
|---|---|
| Cut a new release name for an unpinned release | Edit the current release in place with `knives release`, and push the moved ref. A PR branch that names it is not a pin. Only a consumer's `main` is. |
| Clone again, into `/tmp` or anywhere | Use the existing checkout. A second one's bookmarks diverge. |
| Make a scratch or temporary checkout | `knives start`: a workspace that other agents see. |
| Make a branch in a managed fork's default workspace | `knives start`. Another agent can be there, and your own workspace never inherits a release merge as parent. In your own projects, branch normally. |
| Build a branch on a release merge | A PR from it asks the maintainer to review the whole fork. `knives status` and `knives preflight` show `stacked-history` only after a PR opens, so run the §7 gate at every PR state. |
| `jj duplicate` a branch | The release cannot match its new commit ids. `knives release advance --from` is the recovery, not the plan. |
| Keep two copies of a branch ("release-lineage", "sibling") | One branch is the release member and the PR head. It does not compose: move the release, not the branch. |
| `jj op restore` | It also discards the operations of other agents. |
| Push to `upstream` | Open a pull request. |

## 7. Bidirectional gate before a raw rebase

Run it before any `jj rebase -b`, `-s` or `-r` of a branch. When a release ref has ever been pushed, a raw rebase of an entangled branch is unsafe in both directions.

1. **Scheme check.** Get `newest_release` from `knives status` or `knives repos`, and set `RELEASE_REFS` for this repository's scheme:
   - `release/YYYY-MM-DD[.N]` (dated, the default, no `release_branch`): `bookmarks(glob:"release/*") | remote_bookmarks(glob:"release/*")`. The glob also catches an older cut not yet reaped.
   - Named release series (`[repos.<name>.series.<series>]`, each on its own base) cut `release/<series>/YYYY-MM-DD[.N]`. `status` and `repos` never show them: read them with `knives release --series <series>`. The dated glob above covers every series' cuts.
   - Any other name, such as `integration` (fixed: the one `release_branch` under `[repos.<name>]` in `repos.toml`, never with the prefix `release/`): `bookmarks(exact:"<name>") | remote_bookmarks(exact:"<name>")`. The dated glob never matches it, so the dated term hides a fixed-scheme member.
2. Run `jj log -r '(::<branch> | <branch>::) & (RELEASE_REFS | bookmarks(glob:"keep/*") | remote_bookmarks(glob:"keep/*"))'`. Use this fork's retention convention if it is not `keep/*`. Do not add `~ ::trunk()`: a release or keep ref is never an ancestor of its trunk.
3. **Empty:** the branch is alone. Use `jj rebase -b <branch> -d <trunk>@upstream`. It keeps the change ids, so `knives release advance <branch>` follows.
4. **Branch is a parent of the merge** (`<branch>:: & (...)` names it): it feeds a member's content into the release. Run `knives release members`. It shows only the release in hand (`src/release_carries.rs::run_release_members`), not a retained older cut (`reap_superseded` in `src/commands/release.rs`).
   - Listed: move it only with `knives release rebase`. It rebases all members and the merge together and replays recorded conflict resolutions.
   - Not listed: a retained release needs its exact commit, kept for local descendants or because a pin is frozen on it. The pin's name is the old `release/<date>` name until it is reaped, then a retention bookmark such as `keep/…`. `-b` and `-s` rewrite that commit and, on push, force-move the ref with no error. `-r` silently drops the branch as that release's parent. Use none of them. Get the release owner's sign-off, or wait until its last local descendant goes and `knives release reap` retires it.
5. **Branch descends from the merge** (`::<branch> & (...)` names it): `stacked-history`.
   - Merge ever pushed: `jj rebase -s '<release-or-keep-ref>..<branch>' -d <trunk>@upstream`. The range excludes the merge, so it moves only the branch's own commits and keeps the merge's commit id. Never `-b`: the destination does not contain the merge yet, so `-b` takes the merge as a root and rewrites that ref, retained or current.
   - Merge never pushed anywhere: plain `-b` is harmless, because nothing outside pins the old commit.

## More detail

`using-knives`: the three remotes, stating a PR that inference cannot find, recording that one branch cannot land before another, releases, JSON output. `maintaining-fork-pr`: one PR. `maintaining-fork-release`: the release sweep.
