---
name: pr-preflight
description: Use when opening or preparing a PR against an upstream repository or a fork, contributing upstream, or before you run gh pr create.
---

# Upstream PR Preflight Gate

If this skill disagrees with the code, a command's output or its `--help`, follow those and say which line of this skill is wrong.

## Placement comes first

- Decide where the change lives before whether it goes upstream. Apply `fork-work`'s test: if upstream main replaced the fork tomorrow, would anyone but us miss this? No means the consumer.
- The consumer holds its deployment's operating policy (job lifetimes and reaping, caps, quotas, schedules, alerting, node sizing, who-may-do-what defaults). It is the default home for anything policy-shaped.
- A fork member is for a defect in the library's own machinery, or a capability nothing outside the library can provide. That makes it upstream-bound.
- A policy-shaped change that seems to need the fork is a missing library capability, not a standing exception. Ask the owner what goes upstream and how the fork carries it meanwhile. Never ask whether the fork can keep it without going upstream.
- A fix is CONSUMER or UPSTREAM:
  - CONSUMER: a knob the library already exposes, or a change only we want. No fork branch, no upstream PR.
  - UPSTREAM: a defect any library user would hit, fixed with evidence (a reproduction or a red→green test). An unproven fix needs that evidence first. It gets no exemption from going upstream.
- Every fork member gets an upstream PR. The only question is when it opens.
- Write the placement answer in the `placement:` notch and the PR body. It is a judgment, not a permission: ask nobody, wait for no approval.
- Never open an upstream PR without this gate and that written answer.
- Complete `fork-work`'s challenge before implementation. Check 7 verifies the notch before publication.

## A fork member ships at once

The member carries the change for the consumer from release publication until upstream ships it. The upstream PR opens alongside and gates none of these steps:

1. Make one signed commit on the release's shared base. Push it to the fork remote.
2. Record it with `knives notch`.
3. Give the tip and the red→green evidence to the release claim holder, who adds it to the release in hand and publishes. If every consumer pin of that release is frozen on a revision, they first cut the next dated release name, right then.
4. If nobody holds the release claim, do the handoff in item 3 yourself.
5. The consumer pins the published name and deploys.

The owning session does the open PR's review rounds, body edits, rebases and close. Never park them on a human. The one exception is text the user's own rules send to them first: batch it into one ask, which is approval, not parking.

## Step 1: Get Programmatic Facts

`knives preflight` needs a jj checkout with an `upstream` remote. Run it in your `knives start` workspace (`fork-work`). A plain git clone is not a jj checkout: make it one with `jj git init --colocate` before `knives preflight`. When the checkout has no `upstream` remote, add the fork's `upstream` URL from the knives registry (`repos.toml`):

```bash
git remote add upstream <upstream clone URL>   # only when there is no upstream remote
jj git init --colocate                         # plain git clone only
knives preflight
```

From the output, get the convention files (`AGENTS.md`, `CONTRIBUTING.md`, PR template) and if they changed since last seen. Get the open PR count and cap, and the branch status (claimed, stale, landed, divergent). The command reports. You judge.

## Step 2: Verification Checklist

Do the checks in order. Each needs evidence.

| # | Check | Evidence | If it fails |
|---|---|---|---|
| 1 | Branch state | `knives preflight` shows the branch is not stale, landed or divergent | Landed: no PR. Close or delete the local branch. Stale: rebase onto the latest upstream default branch and re-test. Divergent: reconcile local and remote history to one canonical tip. |
| 2 | PR cap and claim | Our open PR count is strictly below the cap. Your workspace holds the claim. | Cap reached: no new PR. Fold the fix into an open PR only if it belongs there. That call is the owning session's own judgment and work. Otherwise open it when the cap frees. The member rides the release meanwhile, never as a standing fork member. Unclaimed or claimed elsewhere: claim it before you commit or open the PR. |
| 3 | Policy files | You read every convention file in full (`AGENTS.md`, `CONTRIBUTING.md`, `.github/PULL_REQUEST_TEMPLATE.md` or equivalent) | Read them. Do not assume default standards. |
| 4 | Issue reference | If policy requires an issue, the draft body links it ("Fixes #123") | Find the upstream issue. If none exists and policy requires one, search open issues, else open one first. Include the required issue link in the PR body. |
| 5 | AI disclosure | If policy requires it, the PR body or commit discloses AI assistance | Add the notice (for example "Authored with AI assistance"). |
| 6 | Package routing | Changed paths are where repo rules allow this change type (for example, features in plugin packages, not core) | Move the change to the permitted location. |
| 7 | `placement:` notch | `knives notch <branch>` (repository subject for standing rules) shows it. The challenger is not the proposer. Its evidence covers the published diff. A runtime-behavior change also carries the ownership answer and its ruling in the notch and commit body (`fork-work`). | Missing: no PR. Return to `fork-work`'s challenge. Stale: re-run the challenge. |

## Step 3: Record What You Promised

The next agent cannot see a promise in a review thread. Before you open the PR, and after each review round that leaves you owing something, record it:

```bash
knives notch <branch> -m "promised the maintainer we would split the config change out" \
  --evidence <repo>#<number>
```

- Before you answer a review, run `knives notch <branch>` to see what you owe.
- Unanswered threads come from the forge, not from notches.

## Step 4: Execution

When all seven checks pass and the PR body restates Check 7's placement answer, open the PR:

```bash
gh pr create --title "..." --body "..."
```

- Follow the repo's template, with every required issue link and disclosure.
- Later pushes and edits are the owning session's work under the same placement question, not a new permission.
