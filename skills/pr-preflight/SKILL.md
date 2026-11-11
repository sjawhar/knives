---
name: pr-preflight
description: Use when opening or preparing a PR against an upstream repository or a fork, contributing upstream, before you run gh pr create, or before you write an issue, a comment or a review reply its maintainers read.
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
- Write the placement answer in the `placement:` notch, not the PR body: it is our reasoning about our fork, and an upstream maintainer reads every sentence of the body as a claim about the change. It is a judgment, not a permission: ask nobody, wait for no approval.
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
| 5 | AI disclosure | If policy requires it, the PR body or commit discloses AI assistance in the project's own wording | Add the notice the policy asks for, claiming no human review that did not happen. |
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

## Step 4: Write the Text the Maintainer Reads

Every text this project's maintainers read takes the three steps below: a PR title and body, an issue, a comment, a reply on a review thread. The three steps are a floor: do all three. The project's own policy and the user's configured outbound-prose rules add to them, and where either disagrees with a sentence below, follow the policy or the rule on that point and do the rest of the step. When you are not opening a PR, these three steps and check 3 are what applies: the other checks and Step 5 govern opening a PR. An issue you open for check 4 takes the three steps before you open it.

1. **Read before you write.** Check 3 gives you the convention files. Also read how the maintainers write: their last ten issues and pull requests, how they answer each other, and the words they use for the part you change. Search the project for your subject. A question they answered last month, asked again, costs you the reader. Write down what you find: their template, their terms and their level of detail. Do this reading once for the project and keep the notes. A later text reuses them, and you read again only when a convention file changes.
2. **Write in their conventions, in plain words.** Use their template and their headings. Say what happens: each sentence names who acts and what they do. Delete a phrase that names no action. Define each term where you first use it. Keep to one instruction per sentence, the active voice, no noun cluster longer than three words, and one meaning for each word. These constraints come from ASD-STE100, the Simplified Technical English standard. Use no capitals for emphasis. Put the issue link of check 4 and the disclosure of check 5 in the draft now, because an edit after the cut costs another cut.
3. **A fresh reader cuts it.** Give the draft and the notes from 1 above to a reader that had no part in the writing. Tell it to delete everything the maintainer does not need, and to keep what the project requires: the template's headings, the issue link of check 4 and the disclosure of check 5. By preference that reader runs on a model from another vendor than the writer's. Where the user's rules do not permit another vendor, a reader of the same model that did not write the text satisfies this step. The reader returns the cut text and a list of what it removed. Read that list, and put back anything the project requires. The cut text is what goes out. An edit after the cut takes this step again.

## Step 5: Execution

When all seven checks pass and the title and body have taken Step 4, open the PR:

```bash
gh pr create --title "..." --body "..."
```

- Follow the repo's template, with every required issue link and disclosure.
- Later pushes and edits are the owning session's work under the same placement question, not a new permission.
