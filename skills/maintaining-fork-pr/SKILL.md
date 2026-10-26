---
name: maintaining-fork-pr
description: Use when you own one existing fork pull request through to merge or close, or an orchestrator dispatches you to maintain one fork PR.
---

# Owning one fork pull request

- You own one PR end to end, under one branch claim. The orchestrator owns the release and consumer outcome.
- Handback ends your work in the workspace. It does not make the release ready. It does not end the duty for pending CI and maintainer obligations.

## Owning means judging

- You are the PR's maintainer toward upstream. Move it toward merge or close, whichever is right. Hold a view on each question it raises. The steps give an order of work, not a checklist that makes the outcome.
- Judge each item from its record (thread, diff at head, notch chain), not from its kind. No item is "must reply" or "must repair". Act only when the action moves the PR. The record is where the answer lives.
  - A thread that names a commit not on the head is a defect only if it misleads a maintainer who reads it today.
  - A maintainer resolved each resolved thread. Reopen one only if the head broke what it settled.
  - Each comment goes to a maintainer's inbox. It is noise unless it moves the PR.
  - A stale but harmless body sentence is not worth a publish.
- Answer a maintainer's design question yourself. Read the thread, diff and chain and form a position. Implement it, or state it on the thread with your reasons. Never send it up as a human decision without your position.
- A `decision:` notch must show all three:
  1. You read the whole chain for the branch and the release, and no note answers it. Quote the closest note and say why not.
  2. Your recommendation and its evidence.
  3. The choice needs authority you lack (money, an upstream commitment, removing what the maintainer of record asked for), not confidence you lack.

  A `decision:` that fails one is your own work not yet done. If a note answers it, cite it, apply it and continue. Never ask the human twice.

## 1. Read `knives notch`, then claim

- Read the notch history before any recon or repair decision. It holds promises, settled choices, rejected approaches, release relationships and evidence. You need no estate document.
- The dispatch gives the repo, upstream slug, PR, branch, status row, captured notches and guidance, and the `placement:` notch or "none". It also gives `pr-preflight`, the orchestrator's address and a run/repo/PR-qualified scratch path. Ask the orchestrator only for missing coordinates. Get requirements from the repo and its records.
- Read `fork-work`, `using-knives` and `using-jj` for the tool contracts.

```sh
knives notch <branch> --repo <repo>
knives notch --pr <number> --repo <repo>
knives status <repo>
knives start <branch> --repo <repo> --why '<PR number>: <work being owned>'
```

- Read full named chains, also old unprefixed notes and events. Bare `knives notch --repo <repo>` shows only recent context. An old anchor can describe another tip: confirm its evidence and later corrections first.
- Give each new record its prefix: `placement:`, `recon:`, `rehome:`, `repair:`, `verify:`, `record:`, `decision:` or `handback:`. Add `--pr` when it is about the PR. Cite evidence that names the commit, URL or file state. Take `file:line` from the file at the cited commit, not from a diff hunk. Never write an unprefixed or evidence-free obligation. Never rewrite or delete entries.
- Run Knives reads as written, without `--json`. Your own reading and a JSON final report are direct reads. Use `--json` only when executed code does a real filter, calculation or join, and keep that consumer in the evidence. `jq .` or a `.json` file name is not a reason. If output is clipped, page the full captured output. JSON does not recover it.
- The claim is yours by its `why` text and workspace, not by a shared OS user. Enter the workspace that `start` prints and confirm your row. `@` starts as an empty child of the branch.
- A claim-lock wait is normal. Send a refusal that names another holder to the orchestrator. Never force a takeover. A quiet `seen` does not prove the holder is dead.
- A divergent bookmark stops `start`: compare the tips with `jj -R <path> --ignore-working-copy diff --from <a> --to <b>` (path from `knives repos`).
  - Same content, one tip is the live PR's origin ref: apply only the same-content recovery that the refusal prints, then retry.
  - Else: record the tips and ask the orchestrator. Never pick by recency.
- Keep drafts, logs and packets in the per-PR scratch path (for example `<run>/<repo>/pr-<number>/`), not in the workspace. PR numbers alone are not unique across repos. The workspace snapshots into `@`.

**Done when:** you read the inherited obligations, and your row shows your claim and workspace.

## 2. Recon

Read the live PR, its full conversation and its diff before you change anything. Each forge call names the upstream slug.

```sh
knives pr <number> --repo <repo>
knives audit
gh pr view <number> --repo <upstream-slug> --comments   # overview only
gh api --paginate 'repos/<upstream-slug>/actions/runs?head_sha=<head>'
knives release members
knives notch <release>
knives preflight
```

- Take `<head>` and the base from `knives pr`. Track three identities:
  - **Original PR head:** the remote head at recon.
  - **Expected branch tip:** the bookmark tip. It is that head until your legitimate rehome.
  - **Candidate head:** the commit with your repairs. It starts as the expected tip.
- Before repair, the `knives audit` local tip must equal the PR head. If not, refresh each once (`knives pr <number> --timeline` shows head movement). Never overwrite a side to make them agree.
- A non-trunk base is a scope question for the orchestrator. Do not retarget.
- Read all comments, reviews, review threads (resolved too) and each thread's comments. Each GraphQL connection, and each nested thread `comments` separately, needs `pageInfo { hasNextPage endCursor }` and `after` until complete. Compare counts to `totalCount`. `first:100` and `gh pr view --comments` do not prove completeness. Missing access or an unread page leaves recon incomplete. Save the full responses for the initial body, comments, reviews and threads for the post-publish comparison.
- Read the CI status and workflow runs for the exact head on all REST pages. `gh pr checks` can omit approval-gated workflows. Record conclusions, runs with none, run ids and the tested head. `action_required` and absent runs are not passes.
- Diff from the fork point, never from newer trunk:

  ```sh
  jj log -r 'fork_point(<trunk>@upstream | <branch>)::<branch>'
  jj diff --git --from 'fork_point(<trunk>@upstream | <branch>)' --to <branch>
  ```

- A divergent change is not a divergent bookmark. Cite explicit commits. Never abandon predecessor copies.
- `knives preflight` gives facts, not judgment. Its open-PR cap is an upstream norm to note, not a stop for this open PR. Read the contribution files, PR template and workflows it names, mainly those marked `CHANGED since last seen`. They set the gates and body requirements. Project guidance can add consumer tests. Capture exact commands, dependencies, credentials and the behavior to exercise. Never invent a gate. An old notch's run does not cover this head.
- Start template and identifier scans from the `template_missing` and `forbidden` fields of `knives audit`. A missing or null field is not zero hits: read the registry and problems. These fields cover only the bookmark and the published body. Scan the candidate and draft prose yourself before review.
- Make a working list. Judge each item from the thread, the diff at head and the chain:
  - Maintainer asks: does the head do it? Disturb a resolved thread only if the head stopped doing it.
  - Defects you find in the **whole diff**, not only in review comments.
  - Inherited promises and decisions: does the head meet them? Apply a recorded decision. Never ask it again.
  - Body or reply claims that mislead a maintainer today. A rebased-away commit id alone does not.
  - Missing template requirements or headings.
  - Forbidden identifiers in added lines or proposed prose, per registry terms and publication rules. With no list, say so, and still look for private hosts, credentials and internal process details.
  - Does the `placement:` notch cover this head? If missing or stale, investigate now. Never backfill it to unblock finished work.
  - A runtime-behavior member: do its notch and commit body carry the `fork-work` ownership answer and ruling? Does the head implement the ruling's words? A mismatch is a finding.
- Give each item one disposition:
  - already addressed, with evidence (an open promise is not addressed)
  - to repair
  - declined, with a reason for upstream
  - leave alone, with the reason (a real disposition, not a gap)
  - decision needed, only after the three conditions
  - unverifiable, with the external prerequisite
- Get release membership from the parents in `release members`. An advanced branch can succeed an older released parent.
- Write the recon notch:

  ```sh
  knives notch <branch> --pr <number> -m 'recon: <member/lone>; asks and inherited obligations: <dispositions>; findings: <file:line and disposition>; template: <missing>; forbidden: <hits and dispositions>' --evidence <PR-url> --evidence <commit-or-file:line>
  ```

**Done when:** each item has a disposition, you read every page, and the recon notch names the original head and the release relationship.

## 3. Rehome only when needed

- Rehome only if the PR conflicts, a maintainer asks, or verification requires newer trunk behavior. A mergeable PR behind trunk is not a reason.
- Run the `fork-work` bidirectional gate first.
- Lone branch (gate clean): `jj rebase -b <branch> -d <trunk>@upstream`.
- Descends from a pushed release/keep merge (`stacked-history`): this rehome is yours, with no hand-back. Run `jj rebase -s '<release-or-keep-ref>..<branch>' -d <trunk>@upstream`. A bare `-b` rewrites the merge.
- A direct release member: record `rehome needed: parent of <release>` and hand back. The orchestrator changes the release only after all member claims are released.
- An ancestor of a release/keep merge that is not a current member: record `rehome needed: ancestor of retained release <name/ref>` and hand back. Never `-b`, `-s` or `-r` it.
- Never duplicate a feature branch or build a release-lineage copy.
- After the rehome, record the old and new commits and update the expected tip. Make sure that the patch and a conflict-free ancestry stay. A trunk-context-only diff does not replace exercising the code. Push only after candidate review.

**Done when:** no rehome was needed, or a `rehome:` notch records its effect and new expected tip, with both commits as evidence.

## 4. Repair and exercise the candidate

- Append one reviewable repair commit per finding on your `@`. Never rewrite a branch commit or a shared descendant. Describe only your commits. The only sanctioned ancestry changes are the lone-branch rehome, the orchestrator's release-wide rebase, the same-content recovery, and a squash or rebase a maintainer asks for, which follows step 3's rules for a rehome. Never restore the shared operation log.
- Each repair records the failure (red) and the result after the repair (green) under the same conditions, with commands, output and commits. If a claim does not reproduce, investigate its premise. Never fake a pass or quietly weaken a gate. Judge pre-sweep changes against their fork point where that applies.
- In the claimed workspace, run each applicable gate and each distinct test command in the PR body. Do not trust old counts. Then exercise the changed public behavior.
- Keep separate evidence for static analysis, unit/integration tests and **end-to-end behavior on the real surface**. A unit suite is not e2e. If runtime proof belongs to the orchestrator's consumer environment, name the behavior and candidate to test. It stays unverified until the proof comes.
- Write a notch for each repair, and keep full logs outside the workspace:

  ```sh
  knives notch <branch> -m 'repair: <finding>; red: <command/result>; green: <command/result>' --evidence <repair-commit> --evidence <file:line>
  ```

- The final candidate is a commit/tree, never a dirty working copy. If a gate leaves tracked changes, stop. Make them a named repair commit through the repo's normal mechanism, or rerun/clean until the tree has no change. Then rerun the affected gates and review again. Never let generated changes reach publication outside the reviewed candidate.

**Done when:** each repair has a commit and red/green evidence, and the gates pass on the candidate. Runtime evidence is observed or named as a blocking dependency.

## 5. Draft upstream communication

- Draft in scratch. Publish only after review approves the candidate and the remote has it.
- Draft a comment only if the maintainer needs it to review or merge. Examples: an answered ask, a change since their last look, a wrong claim that misdirects review. Never draft tidying: a restated diff, a new commit id in an old reply, an unchanged position. If unsure, do not. Tidiness goes in the notch chain.
- Keep the template and its required headings. Describe the final change, and the requested-versus-added changes since the last human review.
- Answer each ask the maintainer still waits on: the commit, a reason to decline, or your position. Do not repeat an accurate reply. Do not reply on a resolved thread unless the head broke what it settled.
- Record each remaining promise with its concrete obligation. A promise is not its completion.
- Request re-review only from a human whose review is outstanding or whose requests the candidate addresses. A bot review never justifies one.
- Each remaining forbidden-term or private-detail hit needs a reason. An unexplained hit blocks publication.
- Use `pr-preflight` only for what applies to an open PR: policy files, issue references, AI disclosure, scope and package routing, promise recording. Its open-PR cap does not apply here. Its claim and status facts inform ownership and drift.
- Never open an issue or PR only to satisfy a template. Find the existing issue, or escalate the missing authority. Format and naming choices inside the policy are yours.
- Follow the user's outbound-prose and disclosure rules and the upstream policy. Draft in the first person, as the user who opens the PR, with no AI header where the user's rules say so, because the maintainer reads the text as theirs. A disclosure the upstream policy requires (`pr-preflight` check 5) still goes in, in its form. Keep internal people, hosts, tooling, notch contents and coordination out of upstream prose. Write evidence in the repository's terms.

**Done when:** the body and replies are true of the exact candidate, and inherited obligations are answered or honestly outstanding.

## 6. Independent review, push and publish

- Write `<per-PR-scratch>/reviewer-packet.md` and send it to the orchestrator. Before any push or publish, three reviewers get it for the same candidate: a fresh-context reviewer, and the `thermonuclear-deep-review` and `thermonuclear-code-quality` agents where the harness defines them. Each verdict names the candidate head. All run because a defect a maintainer finds costs them our work and us their trust. The packet contains:
  - Repo, PR, branch, original PR head, expected branch tip, candidate head and claimed workspace.
  - The `placement:` notch. The reviewer challenges its evidence, not only whether the patch passes.
  - The full fork-point-to-candidate diff and commit list. The recon notch, inherited notch context and working list.
  - Draft body and replies, configured scan terms, exact gate commands and full log paths.
  - Reviewer rules: examine the **whole diff** yourself for correctness, security and regressions, also when the owner reports no repairs. Confirm every recon and inherited obligation, the scope of each repair, body truth, red/green evidence and remaining forbidden hits. Reproduce applicable claims, not only the owner's logs. Make no commit, notch, bookmark or forge change. Report any gate-generated change, never fold it in.
  - Required first-line verdict: `PASS (no repairs)`, `PASS per fix (k/k)`, or `FAIL: fix|body|drift|placement`. Then evidence per question and any new finding. A question with no object says `none`.
- Wait up to 30 minutes. An acknowledgement is not a verdict. On timeout, write `decision: reviewer verdict outstanding; repair commits <ids>; bookmark not moved` with evidence and hand back. The orchestrator, not the human, gets the verdict and resumes you.
- Drift is any of these:
  - the local bookmark is not the expected branch tip
  - the remote is not the original PR head
  - the candidate commit/tree is not the reviewed one
  - a tracked working-copy change is outside the candidate

  A reviewed candidate ahead of the bookmark is not drift. Another branch that moves is not drift. Confirm these identities before you publish and after each command that can rewrite generated files.
- A PASS does not cover gate-generated files. Make a new candidate through step 4 and send a fresh packet.
- Paste each verdict verbatim into a `verify:` notch. Return a malformed verdict for clarification. A FAIL, or a blocking finding from either thermonuclear reviewer, blocks publication: repair the code or body, or resolve the drift. A `placement` FAIL goes back to the `fork-work` challenge first. Then refresh the packet for a new review. A verdict on an old tree never approves a new candidate.
- After every reviewer passes, and after you confirm the identities and the tree:

  ```sh
  jj bookmark set <branch> -r <candidate>
  jj git push --remote origin -b <branch>
  ```

- Use the PR-head remote if it is not `origin`. `knives pushed <branch>` and a fresh `knives pr <number>` must show that the remote branch and the live PR carry the candidate. With no repair, rehome or requested squash, do not push.
- Read the workflow runs on the published head again, all pages. A failure is repair work. Pending runs need a real watcher, yours or one the orchestrator explicitly keeps after handback. Never sample once and forget. `action_required` is unrun. It is not a cue to ask upstream for approval. Keep missing runtime evidence explicit.
- Write `verify:` with the verbatim verdicts and the original-to-published commits (or no push). Add each gate's last result and log path, real-surface evidence separately, and CI conclusions and pending runs on that head. A pass at another commit does not count.
- Publish only the exact text that review approved, after the candidate is on the PR. Prose you edit after review needs a refreshed packet. Skip a body that has no change from the live one. Post the necessary replies and request re-review where due and permitted. If you cannot request it, record that limit. Save each reply URL.
- Write the record notch, with only evidence arguments that exist:

  ```sh
  knives notch <branch> --pr <number> -m 'record: body <updated|unchanged>; threads: <id> -> <commit> | declined: <reason> | already answered <URL> | left alone: <reason>; promises outstanding: <obligation or none>; re-review: <actual result>; forbidden remaining: <hits/reasons>' --evidence <PR-url> --evidence <new-reply-url>
  ```

- Publish no invented counts or blanket green claims while verification is pending.

**Done when:** the exact candidate has independent approval, and the live PR head matches it. You saw the publication. Each pending CI run and runtime obligation has a named owner.

## 7. Hand back without losing the release obligation

- After publication, read the PR head, comments, reviews and **all pages** of threads again. Compare with recon, and count unresolved threads with no page cap. Read `knives notch <branch>` again.
- Write `handback:` with:
  - the original, candidate and published commits, what changed, and the rehome result
  - the unresolved-thread count, open promises, and remaining verification/CI with its owner
  - the release relationship: member or lone, the released parent commit, and whether it equals the published head or candidate
  - who owns any `knives release advance` or release-wide action
  - citations of the head, PR and release commit
- Never claim that the released parent advanced because the PR bookmark moved. Give release or consumer proof, or its owner, only if the dispatch asked. Otherwise make no release-ready claim.
- Run `knives finish <branch>` from the workspace. From outside it, make sure the claim is gone. Return the notch chain and final observations to the orchestrator. A stopped worker's handback is not a successful repair or a verified release.

## Cross-scope questions and interruptions

- Take placement, membership, compatible repair and coordination questions to the orchestrator with evidence and your recommendation. It decides within the approved goal. Apply recorded choices. Never make the human choose again.
- Only missing authority, a real change in direction, or an unresolved conflict with user intent goes to the human. Send it as a worked question: the ask, the record, what you would do and why, and what changes if they choose otherwise. A maintainer's design question is never one of these.
- Never change defaults or take out functionality against the approved goal to make a gate pass.
- Get new external credentials through the existing approval mechanism. Never go around a denial or timeout.
- Finish independent reachable work while a dependency waits.
- If another actor reparents your branch, update the stale workspace once. Record the old and new tips and the reparented repair commits. Do not push. Give the orchestrator the pending candidate and hand back if necessary. It picks the head from content evidence and resumes you. Then run review and the affected tests again. An old-tree PASS never survives a rebase, even one with no textual conflicts.
