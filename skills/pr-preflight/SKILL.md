---
name: pr-preflight
description: Pre-PR contribution judgment gate for upstream repositories. Use when opening a PR, contributing upstream, doing a pre-PR check, preparing to run gh pr create, or contributing to a fork.
---

# Upstream PR Preflight Gate

## Overview

Opening a pull request against an upstream repository requires adhering to that project's specific contribution guidelines. The `knives` CLI provides programmatic facts through `knives preflight`. The agent provides the human judgment to evaluate compliance before executing `gh pr create`.

Never open an upstream PR without walking this gate.

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
  - *Cap Reached*: Do not open a new PR. Review open PRs against the target repo. Combine related fixes into an existing open PR, or wait until open PRs are merged or closed.
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

When all six checks pass with verified evidence, proceed with opening the pull request:

```bash
gh pr create --title "..." --body "..."
```

Ensure the title and body follow the target repository template, including all required issue links and policy disclosures.
