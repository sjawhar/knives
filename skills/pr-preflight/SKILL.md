---
name: pr-preflight
description: Pre-PR contribution judgment gate for upstream repositories. Use when opening a PR, contributing upstream, doing a pre-PR check, preparing to run gh pr create, or contributing to a fork.
---

# Upstream PR Preflight Gate

> **Where a change lives comes before whether it goes upstream.** A managed fork is a product the consumer repository uses; the consumer is where its own deployment's operating policy lives — lifetimes and reaping of jobs, caps, quotas, schedules, alerting, node sizing, who-may-do-what defaults — and is the default home for anything policy-shaped. A fork member is for a defect in the library's own machinery or a capability nothing outside the library can provide, which is also what makes it upstream-bound. `fork-work` carries the test (*if the fork were replaced by upstream main tomorrow, would anyone but us miss this?* No means the consumer) and the valve (when the fork genuinely looks like the right home for something policy-shaped, that is a question to the owner with options, and the owner's answer decides — a default with a valve, not a prohibition). A change that belongs in the consumer has no branch in the fork and no upstream PR. Two questions, side by side: this one is *fork or consumer*; the callout below is *upstream or fork-only*. `maintaining-inspect` owns both.

> **An upstream PR requires `verdict: UPSTREAM`.** Every fork branch carries a placement verdict — the placement red-team's ruling (skill `fork-work`), recorded as a `placement: verdict:` notch by `knives start --placement` — and only `UPSTREAM` leads to an upstream pull request; `knives gh` refuses `gh pr create` toward a registered upstream for any other verdict. UPSTREAM means: a defect any user of the library would hit, fixed with evidence (a reproduction or red→green test), or a capability users outside this deployment need, extended the general way. **We do not change upstream defaults to suit one deployment's preferences** — a deployment-preference change is CONSUMER or FORK, never a PR. And **most fork work never becomes a PR — too many open PRs is a cost**, paid by the upstream maintainer and by every later sweep of ours: the default for a fix is a fork member that rides the release cut, invisible to upstream. Nobody is asked and no approval is awaited; the verdict is the written judgment.

## Overview

Opening a pull request against an upstream repository requires adhering to that project's specific contribution guidelines. The `knives` CLI provides programmatic facts through `knives preflight`. The agent provides the human judgment to evaluate compliance before executing `gh pr create`.

Never open an upstream PR without walking this gate, and never without the branch's recorded `verdict: UPSTREAM` (the callout above). The gate and that verdict are what make the PR defensible; neither is a permission to ask a human for.

Complete `fork-work`'s placement red-team before implementation — `knives start` will not create the branch without its verdict; Check 7 re-reads the verdict before publication.

## Step 1: Obtain Programmatic Facts

Run the preflight command for the target repository:

```bash
knives preflight
```

Read the command output to identify three categories of facts:
1. Convention files present: `AGENTS.md`, `CONTRIBUTING.md`, and PR template files, including whether they changed since last seen.
2. Open PR accounting: current open PR count and repository policy cap limits.
3. Branch status: whether the branch is claimed, stale, landed, or divergent.

`knives preflight` supplies the facts. You supply the judgment.

## Step 2: Verification Checklist

Walk each check sequentially. Each check specifies what facts to verify, what evidence satisfies it, and what remediation action to take if the check fails.

### Check 1: Branch State Integrity
- **Verification**: Verify that the branch status reported by `knives preflight` is active and clean.
- **Evidence**: Output showing the branch is not stale, landed, or divergent.
- **If Failed**:
  - *Landed*: Do not open a PR. Close or delete the local branch because the changes already exist upstream.
  - *Stale*: Rebase the branch onto the latest upstream default branch and re-test all changes.
  - *Divergent*: Reconcile local and remote commit history, resolve conflicts, and ensure a single canonical branch tip exists before proceeding.

### Check 2: Open PR Limits and Branch Claims
- **Verification**: Verify our open PR count against the target repo is below the repo policy cap, and that the branch is claimed by your current workspace.
- **Evidence**: Current open PR count is strictly less than the repository limit, and claim status matches your active workspace.
- **If Failed**:
  - *Cap Reached*: Do not open a new PR. Review open PRs against the target repo. Folding the fix into an existing open PR is the owning session's own judgment and work, under the same placement question as any upstream change: fold it when it belongs there; otherwise the fix stays a fork member (the callout at the top of this skill) or waits until open PRs are merged or closed.
  - *Unclaimed or Claimed Elsewhere*: Claim the branch in your active workspace before making commits or submitting work.

### Check 3: Policy File Review
- **Verification**: Locate and read the target repository's convention files (`AGENTS.md`, `CONTRIBUTING.md`, and `.github/PULL_REQUEST_TEMPLATE.md` or equivalent).
- **Evidence**: Direct reading of all convention files present in the target repository.
- **If Failed**:
  - *Files Unread*: Read each convention file completely using file reading tools. Do not assume default standards or skip reading repository policies.

### Check 4: Issue Reference Requirement
- **Verification**: Check `CONTRIBUTING.md` and the PR template to determine if the target repository requires linking an open issue in the PR description.
- **Evidence**: Valid issue reference (such as "Fixes #123" or "Closes #123") included in the draft PR description.
- **If Failed**:
  - *Reference Missing*: Find the upstream issue ID corresponding to this work. If no issue exists and repository policy requires one, search existing open issues or open an issue first. Include the required issue link in the PR body.

### Check 5: AI Authorship Disclosure
- **Verification**: Check repository policies to determine if the target project requires disclosing AI assistance or automated authoring.
- **Evidence**: Explicit AI authorship disclosure statement included in the PR description or commit message when required.
- **If Failed**:
  - *Disclosure Missing*: Add the required disclosure notice (for example, "Authored with AI assistance") to the PR description before submitting.

### Check 6: Scope and Package Routing
- **Verification**: Check repository rules to verify whether the proposed change targets the correct package or directory structure. Verify if feature additions are restricted from core packages and routed to plugin or extension packages.
- **Evidence**: Modified file paths match the permitted contribution locations for the given change type.
- **If Failed**:
  - *Incorrect Routing*: Refactor and relocate the changes to the permitted package, directory, or extension location defined by upstream policy.

### Check 7: Placement verdict
- **Verification**: The branch's newest `placement: verdict:` notch rules `verdict: UPSTREAM`, its judge differs from the proposer, and its evidence covers the published diff — the alternative it rejected is still the alternative, and the class is still what the PR claims. Prose `placement:` notes on the branch are context, not the verdict.
- **Evidence**: The `placement: verdict:` notch, read with `knives notch <branch>`. For a member that changes runtime behavior, the notch and the commit body also carry the ownership answer and the ruling it implements (`fork-work`).
- **If Failed**:
  - *Missing*: No PR; return to `fork-work`'s placement red-team. `knives gh` refuses the create anyway.
  - *Verdict is FORK or CONSUMER*: No PR. A FORK member ships through the release; a CONSUMER change belongs in the consumer and its branch should be finished.
  - *Stale (scope or evidence moved on)*: Re-run the red-team and record the fresh verdict before opening.

## Step 3: Record What You Promised

A pull request review is a conversation with a person who will not be here next session,
and a promise made in a review thread is invisible to the next agent. Before opening the
pull request, and again after every review round that leaves you owing something, record
it:

```bash
knives notch <branch> -m "promised the maintainer we would split the config change out" \
  --evidence <repo>#<number>
```

Promises belong in notches, not in a session that ends. `knives notch <branch>` before you
answer a review is how you find out what you already owe. Which review threads are still
unanswered is a different question, derived from the forge, and not this.

## Step 4: Execution

When all seven checks pass with verified evidence and the placement judgment is written down (Check 7's notch, restated in the PR body), proceed with opening the pull request. Every later push or edit to it is the owning session's own work under the same placement question — not a new permission:

```bash
gh pr create --title "..." --body "..."
```

Ensure the title and body follow the target repository template, including all required issue links and policy disclosures.
