---
name: maintaining-fork-pr
description: Use when you own one existing fork pull request — its path to merge or close is yours — through recon, repair, independent review, publication and handback. The orchestrator supplies the PR coordinates and inherited knives records, not a separate estate document; the judgment about what the PR needs is yours, not the dispatch's.
---

# Owning one fork pull request

One agent owns one PR end to end, under one branch claim. The orchestrator owns the wider release/consumer outcome. Your handback ends active work in the claimed workspace; it does not declare the release ready or end responsibility for pending CI and maintainer obligations.

## What owning means

You are this pull request's maintainer toward upstream. Its job is to get merged or get closed; yours is to move it toward whichever is right, and to hold a view on every question it raises. The steps below are the order of operations for doing that, not a checklist that produces the outcome when worked mechanically.

Judge every item from its own record, not from a rule about its kind. A thread that names a commit no longer on the head is a defect only if a maintainer reading it today would misunderstand what the PR does now. A resolved thread was resolved by a maintainer; you reopen it only if the head has since broken what it settled. A comment on the PR reaches a human maintainer's inbox and is noise unless it moves the PR. A body sentence that is stale but harmless is not worth a publish. Nothing in this skill is a "must reply" or "must correct" — every item asks whether acting on it moves the PR toward merge or close, and the record is where the answer lives.

A maintainer's design question is yours to answer, not to forward. Read the thread, the diff and the notch chain, form a position, and either implement it or state it on the thread with your reasoning. The one thing you do not do is send it back up with "Sami decision" attached and no position of your own. That is how one PR carried the same A-or-B question through four owners and three weeks of `decision:` notches, each owner re-escalating a choice that had been made and recorded on 2026-09-05, because none of them read past the prefix.

**Before writing any `decision:` notch**, three things must be true and the notch must say so: (1) you read the entire chain for this branch and the release, and no prior note answers it — quote the closest one and say why it does not; (2) you have a recommendation and give it, with the evidence that produced it; (3) the choice needs authority you lack — money, an upstream commitment, removing something the maintainer of record asked for — not merely confidence you lack. A `decision:` that fails any of the three is your own work not yet done. When a prior note does answer it, cite it, apply it, and move on; the human should never be asked twice.

## 1. Read `knives notch`, then claim

**Read the existing `knives notch` history before making a recon or repair decision.** This is where prior maintainers and agents recorded promises, settled choices, rejected approaches, release relationships and verification evidence. It is not merely somewhere to write your final report. No separate estate document is required.

Your dispatch supplies the registry repo name, upstream slug, PR number/URL, branch, current status row, relevant captured notches/guidance, the `fork-work` **Owning layer and contract evidence** record (or a statement that none exists), the shipped `pr-preflight` skill, the orchestrator's address and a run/repo/PR-qualified scratch path. Ask the orchestrator for genuinely missing coordinates; derive current requirements from the repo and its records, not an invented document. Read `fork-work`, `using-knives` and `using-jj` for the tools' contracts.

```sh
knives notch <branch> --repo <repo>
knives notch --pr <number> --repo <repo>
knives status <repo>
knives start <branch> --repo <repo> --why '<PR number>: <work being owned>'
```

Read full named chains, including older unprefixed notes and events; the workflow prefixes below organize new writes, not which history counts. Bare `knives notch --repo <repo>` shows recent repository context, not every historical obligation. An old anchor may describe another tip: check its evidence and any later correcting note before inheriting it. For every new workflow record, use the matching prefix (`placement:`, `recon:`, `rehome:`, `repair:`, `verify:`, `record:`, `decision:`, `handback:`), stamp the PR with `--pr` when it concerns that PR, and cite evidence that names the commit, URL or file state you relied on; `file:line` evidence comes from the file at that cited commit, not from a diff hunk. Do not write new unprefixed or evidence-free obligations. Do not rewrite or delete existing entries.

**Direct Knives reads use the examples as written, without `--json`.** Reading individual
fields yourself or returning a JSON final report does not make the model a programmatic parser.
Use `--json` only for bytes consumed by executed code performing a real filter, calculation or
join, and retain that consumer in the evidence. Identity formatting such as `jq .`, saving a
`.json` filename, or an imagined later parser is not a reason to add the flag.

Save default compact output for records you will read yourself. Both serializations contain
the same data; if the harness clips output, recover or page the full captured output rather
than switching to JSON and assuming the missing content returned.

A claim is yours by the `why` text and workspace, not merely a shared OS username. Enter the workspace `start` prints and confirm your row. `@` begins as an empty child of the branch. A claim-lock wait is normal; a refusal naming another holder goes to the orchestrator, never a forced takeover. Quiet `seen` data does not prove the holder died.

If a divergent bookmark prevents `start`, use `knives repos` for the checkout path and compare its explicit tips with `jj -R <path> --ignore-working-copy diff --from <a> --to <b>`. Same content and one tip matching the live PR's origin ref: apply only the same-content bookmark recovery printed by the refusal, then retry `start`. This is the pre-claim exception. Different content or an ambiguous origin tip: record the tips and ask the orchestrator to resolve ownership/content; do not select a winner by recency.

Keep drafts, logs and packets in your per-PR scratch directory, not the workspace. For example, `<run>/<repo>/pr-<number>/reviewer-packet.md`; PR numbers alone are not unique across repos. Workspace files auto-snapshot into `@`.

**Done when:** inherited obligations have been read and your branch row confirms your claim and workspace.

## 2. Recon: complete records and current evidence

Read the live PR, its full conversation and its diff before changing anything. Every forge call names the **upstream slug**, not the registry name or an implicit fork remote.

```sh
knives pr <number> --repo <repo>
knives audit
gh pr view <number> --repo <upstream-slug> --comments   # overview only; paginated APIs below are the completeness source
gh api --paginate 'repos/<upstream-slug>/actions/runs?head_sha=<head>'
knives release members
knives notch <release>
knives preflight
```

Take `<head>` from `knives pr`'s `head` and the PR base from its `base`; these are live forge facts. `knives audit` supplies the branch's local tip, origin tip and PR-head comparison. Record three distinct identities throughout the workflow:

- **Original PR head:** the remote head observed at recon.
- **Expected branch tip:** the local bookmark tip, initially equal to that head; update it only after your legitimate rehome.
- **Candidate head:** the exact commit containing your proposed repairs, initially the expected branch tip.

Check the branch's audit row before repair: local tip must equal the PR head. On mismatch refresh each once; `knives pr <number> --timeline` can explain head movement, but do not overwrite either side to make the row agree. A non-trunk base is a scope/dependency question for the orchestrator, not permission to retarget it. Save the complete API responses for the initial body, comments, reviews and threads for the post-publish comparison; `gh pr view --comments` is not a completeness source.

Read **all** top-level PR comments, submitted reviews, review threads (including resolved ones), and comments in each thread. GraphQL connections for `comments`, `reviews`, `reviewThreads` and nested thread `comments` must include `pageInfo { hasNextPage endCursor }` and follow `after` until complete. Paginate nested comment connections separately; paginating the outer query does not paginate each thread. Check returned counts against `totalCount` where available. A `first:100` or `first:50` response is not proof of completeness. Missing access or an unfinished page leaves recon incomplete.

Read checks and workflow runs for the exact head, following **every** REST page (`gh api --paginate` for the workflow-runs endpoint filtered by `head_sha`). `gh pr checks` alone may omit approval-gated workflows. Record conclusions and runs still without a conclusion. `action_required` means unrun, not passing; absent runs mean unobserved, not passing. Preserve the identity of the run and tested head.

Use the PR's fork point for its diff:

```sh
jj log -r 'fork_point(<trunk>@upstream | <branch>)::<branch>'
jj diff --git --from 'fork_point(<trunk>@upstream | <branch>)' --to <branch>
```

Do not diff directly from a newer trunk and mistake reversed trunk changes for the PR. Obtain `file:line` evidence from the file at the stated commit, never from diff line numbers. A divergent *change* in `jj log` is not a divergent bookmark; cite explicit commits and do not abandon predecessor copies.

Use `knives preflight` as facts, not judgment: convention-file presence and changed-digest markers, branch state, claim facts and any stated open-PR cap. In this existing-PR workflow the cap is an upstream norm to note, not a stop sign for repairing or publishing the already-open PR. Read the current upstream contribution files, PR template and workflow definitions named by `knives preflight`, especially any file reported `CHANGED since last seen`. They supply the actual gates and body requirements; project-specific guidance may add consumer verification. Capture exact commands, required dependencies/credentials and the observable behavior to exercise. Do not invent gate commands or assume that an old notch's successful run covers this head.

Start template/identifier checks with the branch's `knives audit` fields `template_missing` and `forbidden`. Missing or null fields can mean unobserved data, no configured terms, or a fork-only exemption; read the registry and reported problems rather than treating absence as zero hits. These fields describe the current bookmark and published body. They do not scan a later unpushed candidate or draft prose, which still require an explicit candidate scan before review.

Build a working list with one disposition for each item, judged from what you read — the thread, the diff at the head, and the chain — not from the item's category:

- A maintainer ask. Check whether the head does what was asked; a thread being resolved is evidence a maintainer was satisfied, and you disturb that only when the head has since stopped doing it.
- A defect you found by examining the **whole diff**, not just review comments.
- An inherited promise or decision from `knives notch`, and whether the head satisfies it. A decision already recorded is applied, not re-asked.
- A body or reply claim that would mislead a maintainer about what the PR does now. A commit id that has since been rebased away is not, by itself, misleading — the prose is usually still true. Ask what a reviewer reading it today would believe, and whether that belief is wrong.
- A missing template requirement or heading.
- A forbidden identifier in added lines or proposed upstream prose, using the configured registry terms and applicable publication rules. If no list is configured, state that; still check for private hosts, credentials and internal process details.
- Whether the owning layer is still evidenced: the `fork-work` placement record exists and its evidence covers this head. If no record exists, or the diff outgrew it, this is a recon item with the ordinary dispositions — investigate the layer before repair, never backfill a plausible record to unblock finished work.

Dispositions are: already addressed (commit/reply evidence), to repair, declined (reason to give upstream), leave alone (say why — this is a real disposition, not a gap), genuine decision needed (only after the three conditions in "What owning means"), or currently unverifiable (specific external prerequisite). An outstanding promise is not already addressed. Note whether the branch is a release member, using the actual parent associations from `release members`; an advanced branch may succeed an older released parent.

Record the recon and its evidence:

```sh
knives notch <branch> --pr <number> -m 'recon: <member/lone>; asks and inherited obligations: <dispositions>; findings: <file:line and disposition>; template: <missing>; forbidden: <hits and dispositions>' --evidence <PR-url> --evidence <commit-or-file:line>
```

**Done when:** every observed ask/finding/obligation has a disposition, all pages are read, and the recon notch names the original head and release relationship.

## 3. Rehome only when needed

Rehome when the PR conflicts, a maintainer actually requests it, or verification requires newer trunk behavior. Do not reset review context merely because a mergeable PR is behind trunk.

A lone branch follows `fork-work`'s `jj rebase -b <branch> -d <trunk>@upstream`. A release member does not move independently: record `rehome needed: parent of <release>` and hand back to the orchestrator, which performs release-wide changes only with all affected member claims released. Never duplicate a feature/fix branch or construct a release-lineage copy.

After your legitimate rehome, record both old/new commits, update the expected branch tip and verify patch preservation and conflict-free ancestry. The remote still has the original PR head until publication. A trunk-context-only diff is evidence of a rehome, not a substitute for exercising the resulting code. Push only after candidate review.

**Done when:** no rehome was needed, or its exact content effect and resulting expected tip are recorded in a `rehome:` notch with both commits as evidence.

## 4. Repair and exercise the candidate

Append a reviewable repair commit per finding on your own `@`; do not rewrite an existing branch commit or any shared descendant. Describe only the new commits you created. The sanctioned ancestry changes are the lone-branch rehome, the orchestrator's release-wide rebase and same-content bookmark recovery above. Never restore the shared operation log.

Each repair records the observed failure and successful reproduction after the fix, with exact commands/output and commits. Keep the red and green conditions comparable. If a claim cannot be reproduced, investigate its premise; do not manufacture a passing test or silently weaken a gate. Pre-sweep changes are evaluated against their fork point where appropriate.

Run the complete applicable repo gates from recon in the claimed workspace. Run every distinct Testing/validation command asserted by the PR body, rather than trusting historical counts. Then exercise the changed public behavior on the candidate. Separate evidence for static checks, unit/integration tests and **end-to-end behavior on the real surface**. A unit suite is not automatically e2e. If real runtime proof belongs to the orchestrator's consumer environment, specify the exact behavior/candidate to test and record it as unverified until that proof arrives.

Record each repair via `knives notch <branch> -m 'repair: <finding>; red: <command/result>; green: <command/result>' --evidence <repair-commit> --evidence <file:line>`. Preserve complete logs outside the workspace. Identify the final candidate explicitly: a commit/tree, not a dirty working copy. If a gate or generator leaves tracked changes, stop and either make that output part of a named repair commit through the repo's normal mechanism or rerun/clean until the candidate tree is unchanged; then rerun affected gates and re-review. Do not hide generated deltas or let them ride to publish outside the reviewed candidate.

**Done when:** every repair disposition has a commit and reproduction evidence, applicable gates pass on the candidate, and runtime evidence is either observed or explicitly identified as a blocking verification dependency for the requested outcome.

## 5. Draft truthful upstream communication

Draft in scratch; publish only after the reviewer approves the candidate and the remote has it.

Every word you publish lands in a human maintainer's notifications. The bar for a comment is that the maintainer needs to read it to review or merge the PR: an ask answered, a change since their last look explained, a wrong claim that would misdirect their review corrected. A comment that only tidies the record — restates what the diff already shows, updates a commit id in an old reply, re-announces an unchanged position — fails that bar and is not drafted. When you are unsure, the answer is no; the notch chain is where tidiness goes.

- Preserve the repository template and all required headings. Describe the final change and requested-versus-added changes since the last human review.
- Answer each ask the maintainer is still waiting on: with the commit that does it, a reason for declining, or your position on the question they raised. Do not repeat an existing accurate reply; do not reply on a thread the maintainer resolved unless the head broke what it settled.
- Record any remaining promise with its concrete obligation; a promise to do work is not its completion.
- Request re-review only from a human maintainer whose review is outstanding or whose requests the candidate addresses. A bot review does not justify a human re-review request.
- Scan final added lines and draft prose for configured forbidden terms and private/internal details. Give each intended remaining hit a reason; an unexplained hit blocks publication.

Use `pr-preflight` only for requirements that apply to an already-open upstream PR: policy files, issue-reference requirements, AI disclosure, scope/package routing, the owning-layer record (Check 7) and promise recording after review rounds. Its open-PR capacity gate is a new-PR opening rule and must not block repairing or publishing this existing PR; its branch-claim/status facts still inform ownership and drift. Do not open an issue/PR just to satisfy a template: establish the existing issue or escalate the missing authority. A formatting or naming judgment within the established policy is yours, not a new human decision.

Follow the user's applicable outbound-prose/disclosure rules and upstream policy. Do not leak internal people, hosts, tooling, notch contents or coordination into upstream prose. Evidence intended for the maintainer must be understandable in that repository's terms.

**Done when:** the body/replies are true of the exact candidate and inherited obligations are answered or honestly outstanding.

## 6. Independent review, push, and publish

Write `<per-PR-scratch>/reviewer-packet.md` with:

- Repo, PR, branch, original PR head, expected branch tip, candidate head and claimed workspace.
- **Owning layer and contract evidence** from `fork-work`: the evidenced failure or capability, actual consumer/upstream contract, considered configuration/caller/feature-branch alternatives, and the independent placement decision. The reviewer challenges this evidence, not merely whether the proposed patch passes tests. An unsupported upstream premise must be resolved before publication, not renamed as hardening to preserve the PR.
- Full fork-point-to-candidate diff and commit list; recon notch, inherited notch context and working list.
- Draft body/replies, configured scan terms, exact gate commands and full log paths.
- Reviewer rules: independently examine the **whole diff** for correctness, security and regressions, including when the owner reports no repairs. Check every recon/inherited obligation, scope of each fix, body truth, red/green evidence and remaining forbidden hits. Reproduce applicable claims; do not merely accept the owner's logs. No commit, notch, bookmark or forge mutation. Any gate-generated change must be reported, never silently folded in.
- Required first-line verdict: `PASS (no repairs)`, `PASS per fix (k/k)`, or `FAIL: fix|body|drift|placement`, followed by evidence per question — including the owning layer — and any newly discovered finding. A question with no object says `none`.

Send the packet to the orchestrator for a fresh-context reviewer. Wait up to 30 minutes; an acknowledgement is not a verdict. On timeout, write `decision: reviewer verdict outstanding; repair commits <ids>; bookmark not moved` with evidence and hand back. The orchestrator owns obtaining the verdict and resuming you, not the human.

**Candidate growth is not drift when it is intentional and reviewed.** Candidate B may intentionally differ from bookmark A until publication. Drift means the observed local bookmark differs from the packet's **expected branch tip**, the remote differs from its **original PR head**, the candidate commit/tree differs from what was reviewed, or any tracked working-copy change exists outside the candidate. Check these identities immediately before publishing and after any owner or reviewer command that can rewrite generated files. If a gate generated changes, the verdict is not a pass on those files; fold them into a new candidate through step 4 and send a fresh packet. An unrelated branch moving is not this PR drifting.

Paste the returned verdict into a `verify:` notch verbatim. A malformed verdict returns for clarification. A FAIL blocks publication: repair code/body, resolve actual drift, or — for `placement` — return to `fork-work`'s challenge and establish the owning layer before any further repair; then refresh the packet and obtain a new review. A verdict for an old tree does not approve a new candidate.

After PASS and the identity/tree checks:

```sh
jj bookmark set <branch> -r <candidate>
jj git push --remote origin -b <branch>
```

Use the actual configured PR-head remote if it differs from `origin`; `knives pushed <branch>` and a fresh `knives pr <number>` must confirm the remote branch and live PR have the candidate. With no repairs/rehome, there is no push. Re-read paginated workflow runs on the exact published head. Failure is actionable repair work. Executable pending runs need a real watcher, owned by you or explicitly retained by the orchestrator after handback; do not sample once and forget them. `action_required` is unrun, not a request to upstream for approval. Missing/blocked runtime evidence stays explicit.

Record `verify:` with the verbatim reviewer verdict, original-to-published commits (or no push), each local gate's last result and complete log path, real-surface evidence separately, and CI conclusions/pending on that head. A passing gate at another commit does not fill this record.

Publish only the exact draft text the reviewer approved, after the candidate is on the PR. If you edit body or reply prose after review, refresh the packet. Compare the published body to the approved draft before editing and skip an unchanged body; post necessary thread replies, and request re-review where due and permitted. If that request is unavailable to an outside contributor, record the actual limitation instead of claiming it succeeded. Save every returned reply URL. Record:

```sh
knives notch <branch> --pr <number> -m 'record: body <updated|unchanged>; threads: <id> -> <commit> | declined: <reason> | already answered <URL> | left alone: <reason>; promises outstanding: <obligation or none>; re-review: <actual result>; forbidden remaining: <hits/reasons>' --evidence <PR-url> --evidence <new-reply-url>
```

Only include evidence arguments that exist. Publish no fabricated counts or blanket green claims while required verification is pending.

**Done when:** the exact candidate has independent approval, the live PR head matches, publication is confirmed, and all pending checks/runtime obligations have an explicit continuing owner.

## 7. Hand back without losing the release obligation

Freshly read the PR head, comments/reviews and **all pages** of review threads after publication. Compare with recon to confirm the changes actually made, and count unresolved threads without a page cap. Read `knives notch <branch>` once more to reconcile the record.

Write `handback:` with the original/candidate/published commits, what changed, rehome result, unresolved-thread count, outstanding promises, remaining verification/CI and its owner, and the release relationship: member/lone, released parent commit if any, whether that parent equals the published head/candidate, and who owns any required `knives release advance` or release-wide action. Do not claim the released parent advanced just because the PR bookmark moved. If the dispatch explicitly requested release or consumer assurance, include the exact proof or continuing owner; otherwise do not expand a PR-only handback into a release-ready claim. Cite the head, PR and relevant release commit.

Run `knives finish <branch>` from the claimed workspace, then confirm the claim is released from outside it. Scratch survives outside the removed workspace. Return the notch chain and saved final observations to the orchestrator. A stopped worker's handback is not a successful repair or a verified release.

## Cross-scope decisions and interruptions

Bring ordinary placement, membership, compatible repair and coordination questions to the orchestrator with evidence and your recommendation. Apply recorded choices rather than asking the human to make them again. The orchestrator decides within the authorized goal; only missing authority, a genuine change in direction, or an unresolved conflict with user intent reaches the human — and it reaches them as a question you have already done the work on: what was asked, what the record says, what you would do and why, what changes if they choose otherwise. A maintainer's technical question about the PR's design is never in this category; that is your work (see "What owning means"). Do not change defaults/remove functionality contrary to the approved goal merely to make a gate pass. New external credentials require the existing approval mechanism; never route around a denial or timeout. Finish independent reachable work while a dependency waits.

If another actor reparents your branch, update the stale workspace once and record the old/new tips and reparented repair commits. Do not push. Notify the orchestrator with the pending candidate and hand back if necessary; it chooses the head using content evidence and resumes you. Re-run review and affected verification on the resulting candidate. Never assume an old-tree PASS survived a rebase, even when it has no textual conflicts.
