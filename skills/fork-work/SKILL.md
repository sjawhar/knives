---
name: fork-work
description: Use when about to work in, read or investigate a repository that may be a fork managed by knives: before `knives start`, before any change, fix or test there, before cloning or making a scratch checkout of one, or when asked which branch or working copy to use.
---

# Working in a managed fork

## 1. Find out if it is a managed fork

Do this before you read code too: the wrong copy gives wrong answers, and nothing says so.

1. Run `knives repos`. Not listed: this skill does not apply. Listed: other agents share it, and a checkout exists.
2. `not on this machine` means the scan of `~` (three levels, colocated jj checkouts, no dot-directories or symlinks) found none, not that none exists. Before you clone, `cd` into a likely checkout and run `knives status`: a command given no repository name reads the checkout you stand in.
3. `ambiguous: N checkouts`: do not pick one. Ask.
4. Should be managed but is not listed: run `knives register` in it and give the snippet it prints to the human for `repos.toml`. Do not edit `repos.toml`.

## 2. Read the status and the notches

1. Run `knives status` in the checkout. Read the other agents' claims before you touch anything: another agent on your branch is a collision. On a fork that shares its ledger, it lists only this machine's claims, and `knives start` refuses a branch another owner holds on any machine.
2. Run `knives notch` for recent decisions, and `knives notch <branch>` before you touch a branch you do not understand. An entry at a commit the branch has since moved past is a reason to check again, not a conclusion.
3. Record a decision worth keeping (superseded, stopped by its owner until an event, a promise to a reviewer, a PR moved), with a commit or `file:line`:
   `knives notch <branch> -m "what you decided and why" --evidence <commit-or-ref>`

## 3. Challenge the placement before `knives start`

Read-only investigation is exempt. Placement is your judgment, not a question for the human.

**Ownership test** (you apply it now; the release owner again at include time): *if upstream main replaced the fork tomorrow, would anyone but us miss this?* No: it belongs in the consumer.

- The consumer owns its deployment's operating policy: job lifetimes and reaping, caps, quotas, schedules, alerting, node sizing, who-may-do-what defaults. Policy goes in its jobs, infrastructure or a config value the library exposes, even when the mechanism is library code.
- A fork member is for a defect in the library's own machinery, or a capability nothing outside the library can give (a missing field, an endpoint, a knob that keeps upstream's default). Both are upstream-bound.
- Policy that seems to need the fork is a missing library capability: ask its owner what goes upstream and how the fork carries it until then, not whether the fork can keep it.

**Challenge.** Before you make a branch:

1. Dispatch a fresh-context subagent briefed to argue that the change does not belong in the fork: the consumer's config, IaC or code gets the effect, or not every library user hits the defect. Give it the facts for answers 1 and 2 below.
2. It returns one verdict. There is no third: never record "fork-only" or "a member that never goes upstream".
   - CONSUMER: the consumer gets the effect, or only we want it. No branch: make the config value, stack transformation or consumer-side job.
   - UPSTREAM: fork-member material with the proof of answer 3. A fork member on the release's shared base, shipped as in §4, that also goes upstream.
3. Ownership not established: investigate. A green gate, a plausible patch or a peer's "go" establishes nothing. An unsupported bug is not "hardening"; a fix counts only with its reproduction or red→green test.
4. Record the verdict as the branch's first `placement:` notch (the repository is the subject for a CONSUMER verdict with no branch, or a standing rule). Reuse a valid one; challenge again when the evidence or scope changes.
   `knives notch <branch> -m "placement: verdict: CONSUMER|UPSTREAM; proposer <id>; red-teamer <id>; <four answers>" --evidence <revision-or-url>`
   1. The observed failure or missing capability and its revision, not an inherited summary.
   2. Where the fix goes (our config, caller or open branch, or upstream), chosen after checking the real launch path, installed version, options and recovery mechanisms.
   3. Bug: a minimal reproduction on unmodified upstream at a cited revision, without our patches. Feature: the general extension case, keeping upstream's defaults.
   4. Why upstream review, CI, rebase and maintenance are worth it.

**A member that changes runtime behavior** (anything a production operator notices): its `placement:` notch and commit body state why it does not live in the consumer (the ownership test's answer, or the owner's ruling that put it in the fork), and the ruling it implements (its id or link), or that it implements none and is a defect fix with red→green evidence. Build the ruling's exact words. With neither statement, the release owner calls it `neither` and does not compose it (`maintaining-fork-release` step 5).

## 4. Ship the fork member, and open the upstream PR alongside

Shipping a fork member is part of the same work:

1. Push one signed commit on the release's shared base to the fork remote. Record it with `knives notch`.
2. Give the tip and the red→green evidence to the release-claim holder, who includes it in the release in hand and publishes; when every consumer pin of that release is frozen on a revision, they first cut the next dated name. Nobody holds the release claim: do this yourself (`maintaining-fork-release`).
3. The consumer pins the published name and deploys. The member carries the change until upstream ships it.

- A member's PR on the fork remote is for review only. Base it on its series' base, never on a `release/…` cut: merging into a cut fast-forwards that published ref away from the tree consumers pinned. Only `knives release` puts a member in a release.
- The release owner's process opens the upstream PR (`pr-preflight`) from a signed commit with an `Omp-Session` trailer, once the `placement:` notch says UPSTREAM. It gates nothing: not the release, the pin or the deploy. Ask nobody; wait for no approval.
- After it opens, its review rounds, body edits, rebases and close are the owning session's work, not a human's.

## 5. Get your workspace with `knives start`

1. `knives start <branch> --why "what you are doing"` claims the branch and opens a jj workspace on it. Work only there. A `start` that pauses is waiting for another agent's claim lock: let it finish.
2. When your active work stops, also to wait for review, run `knives finish <branch>`. It returns the claim and removes the workspace; jj keeps the work by change id, and the branch, bookmark and PR stay.
3. A subagent that runs as a session of its own runs knives with `KNIVES_OWNER` set to its parent's owner name (the parent reads it from its claim in `knives status` and passes it on), so its `knives start <branch>` resumes the parent's claim. The parent never releases its claim for the subagent.
4. For a release name, `knives start <release name>` takes the release claim. Its workspace rests on the root commit, not on a release commit. A hand resolution on the release commit follows step 2 of `maintaining-fork-release`'s `references/release-wide-writes.md`.

## 6. Never improvise

| Don't | Do |
|---|---|
| Clone again, or make a scratch or `/tmp` checkout | The existing checkout and `knives start`. A second clone's bookmarks diverge, and other agents cannot see it. |
| Branch in the fork's default workspace | `knives start`. Another agent can be there, and a branch there can inherit a release merge as parent. |
| Build a branch on a release merge | A PR from it asks the maintainer to review the whole fork. `knives status` flags `stacked-history` only on a branch with an open PR; `knives preflight` checks every branch. |
| Cut a new release name for a release no consumer pins | Edit the release in hand (`knives release include`, `advance`, `drop`) and push the moved ref. `knives release` lists the pins; a PR branch naming the release is not one. |
| `jj duplicate` a branch, or keep a second copy | One branch is the release member and the PR head. If it does not compose, move the release, not the branch. `knives release advance --from` recovers a duplicate; it is not a plan. |
| `jj op restore` | Fix forward: it also discards other agents' operations. |
| Push to `upstream` | Open a pull request. |

## 7. Bidirectional gate before a raw rebase

Before any `jj rebase -b`, `-s` or `-r` of a branch, read [references/rebase-gate.md](references/rebase-gate.md) and run its gate. A raw rebase can rewrite a pushed release or retention merge, through a merge the branch is built on or one built on the branch.
