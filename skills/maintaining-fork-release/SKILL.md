---
name: maintaining-fork-release
description: Use when asked to maintain a set of fork pull requests, address upstream debt, or verify and ship the forks' releases and consumer pins. For one pull request alone, use maintaining-fork-pr. For command semantics, use using-knives.
---

# Maintaining fork pull requests and releases

You own the requested outcome, not only the dispatched PRs.

- A PR-only request ends at verified PR maintenance. Do not turn it into a release or deployment change.
- A request to address upstream debt or to make releases good also needs step 5.
- Obey explicit exclusions, for example "no deployment".
- `fork-work` governs workspace entry. `using-knives` governs command semantics and flags. `using-jj` governs version control.

## 1. Read `knives notch` first

- `knives notch` is the read/write record of this work: decisions, maintainer promises, rejections, release changes and evidence. Read it before you decide, dispatch or repair.
- There is no separate estate document or Markdown inventory. Do not find or create one. Record new branch and release decisions in `knives notch`.
- Map the requested repositories through `knives repos` and `~/.config/knives/repos.toml`. The upstream slug is the entry's `upstream` URL without `.git`. It is not the registry name.
- A project skill can name a library family and its consumer requirements. It does not replace the live records.
- Read in this order, with names from the output before:

  ```sh
  knives repos
  knives status <repo>
  knives notch --repo <repo>
  knives release --repo <repo> members
  knives notch <release> --repo <repo>
  knives notch <branch> --repo <repo>
  knives notch --pr <number> --repo <repo>
  ```

- Bare `notch` shows only recent context. Read every relevant branch and release chain in full. `--pr` also finds a PR's former branches.
- Notes without this workflow's prefixes are valid context. Never discard them.
- Verify each note's time, anchors and evidence against the current head. A later note can supersede it. A recorded requirement still holds when a later release lost its implementation.
- Run `knives notch --verify <subject> --repo <repo>` and investigate each discrepancy. An old assertion is not current proof.
- Read output as the commands print it, with no `--json`, whenever you read it yourself. A field lookup, a saved capture and a JSON final report are still reading it yourself.
- Use `--json` only when executed code filters, counts or joins it, and keep that consumer with the evidence. `jq .`, a reprint wrapper or a `.json` file name is not such a need. Do not invent a parser step.
- Save native output unchanged. JSON does not prevent truncation: recover the full capture or page the saved file, and never conclude from an elision.
- Read upstream contribution files and workflows for current gates. Read project guidance for consumer wiring.

**Done when:** you know each requested repository's identity, you read its relevant notch chains, and you separate inherited obligations from stale assertions.

## 2. Account for release members and open PRs

- The population is the **union of release members and open PR branches**. Maintain an open PR outside the release. Account for a release member without a PR. Count an overlapping branch once, with both identities.
- Deduplicate PR aliases by upstream slug and PR number. Identify release-only members by release parent commit.
- When release parent, branch tip and PR head differ, keep all three in the row. That is a composition gap, not a duplicate.
- Use current `knives status <repo>`, `knives release --repo <repo> members`, `knives audit <repo>` and `knives consumers <repo>`. Reconcile the open-head discrepancies of the audit before you claim full coverage.
- Read a member's notch chain before you classify it. A merged or closed PR does not prove that a release no longer needs its delta.
- `status` exit 1 means findings. Exit 3 or `problems` means unanswered data. A missing checkout, forge, record or page is not zero work. Keep counts provisional, investigate that source, and continue on complete repositories.
- A search result corroborates. It does not replace the registry and release membership.
- List repositories outside the requested set once. Do not dispatch them.
- Make one scratch directory, `~/.cache/knives-sweep/<unique-run>/`, never in a workspace or `/tmp`. Use a run-unique suffix, not only the date. Make `<repo>/pr-<number>/` per PR and `<repo>/release/`.
- Save pre/post output, diffs, drafts and gate logs there in native formats. Name the paths in dispatches and reports. Scratch is working material. Notches and published PRs are the record.
- The report starts: `Population: <p> open PRs and <r> release-only members, <u> distinct work items across <n> requested forks; coverage complete|incomplete (<timestamp>)`. Name every incomplete source.
- A PR-only report labels release-only items as outside its repair scope. It does not claim them verified.

**Done when:** you account for every union member, every count cites saved output, and each missing observation is explicit.

## 3. Dispatch one owner per PR

- Give each PR one `maintaining-fork-pr` owner, end to end. Keep at most eight owners active, and dispatch at most eight at a time.
- See each start's claim or refusal before the next batch. Claim-lock waits are normal. Free the slot on handback, but stay responsible for the PR until merge or reassignment.
- **Owners and the step 4 reviewers run on a strong model, never the default fast tier**. A fast model works the steps as a checklist: it posts replies nobody needed and reopens settled threads and decisions.
- Before the first dispatch, find the model of the generic task agent. If it is fast, select a strong agent or model override and name it in the dispatch record. Only read-only recon that an owner judges again can use a fast tier.
- Respect a claim held outside this sweep. Report its `why` text and continue other work. A quiet `seen` field does not prove the holder died.
- If an owner stops, find its state through coordination and its claim and workspace. Then use the supported claim-recovery procedure. Never force a claim or restore the shared jj operation log.

Each dispatch holds, in order:

1. The whole shipped `maintaining-fork-pr` skill.
2. Registry name, upstream slug, PR number and URL, branch, and its current status row as captured. For a release-member branch, also the member row and parent commit. The owner gets the PR head from the forge. It reports original PR head, expected branch/bookmark head and candidate head as separate revisions.
3. The relevant `knives notch` history: promises, decisions, membership evidence and open obligations. Give full entries or a readable saved path, never your summary. The owner refreshes it before it acts.
4. Upstream and project guidance with source paths, and recorded publication constraints. The owner derives the gates from current repository files. Also the registry's configured `forbidden` terms and outbound-prose rules. If none are configured, say so and invent none. An empty list is not a privacy waiver.
5. The whole shipped `pr-preflight` skill, your reviewer-relay address and the per-PR scratch path.
6. The branch's `placement:` notch, or a statement that none exists. Before you dispatch upstream-bound work that you propose, complete the challenge in `fork-work`.

Release-only members:

- Handle them yourself with the same claim, evidence and independent-review discipline. Do not invent PR numbers or open PRs to fill the inventory.
- Claim a named branch before anyone edits it. Carry, drop or classify an anonymous member from release evidence and notches. Do not make a branch for it.
- Give a bounded branch implementation to one owner only when the requested scope authorizes it.
- Classify with evidence: supplied upstream, still proposed upstream, downstream configuration, or obsolete. Record reason and disposition with `knives notch <branch> -m ... --evidence ...`, or on the release or repo subject for an anonymous member.
- Do not include an open PR in a release automatically. A branch with no upstream PR yet still needs one.

**Done when:** each open PR has one owner or a named blocker. Each release-only item has an owner or a disposition that fits the scope.

## 4. Review, record and coordinate integration

### Reviewer relay

- When an owner sends its packet path, verify its repo/PR identity, original PR head, expected head and candidate head against the dispatch.
- Send a packet back to recon when it has no `placement:` notch or uses approval or tests in place of evidence.
- Send it back when its PR evidence is a first page or truncated. Comments, reviews, threads, CI runs and workflow runs must be complete for the judged head. A `null` thread count, elided output or `first:100` with more pages is incomplete.
- Dispatch the three reviewers `maintaining-fork-pr` step 6 names, each fresh and each with the packet unedited. The fresh-context reviewer's verdict is `PASS (no repairs)`, `PASS per fix (k/k)` or `FAIL: fix|body|drift|placement`, with evidence for each question.
- Each reviewer examines the **whole PR diff**, not only the owner's findings, even when recon found nothing.
- Relay each verdict verbatim within 20 minutes. Do not use your judgment in its place or soften a FAIL. Send a malformed verdict back to the reviewer.
- After 30 minutes, the owner records `decision: reviewer verdict outstanding; repair commits <ids>; bookmark not moved` and hands back. Then the verdict is your work, not the human's. Get it and resume the owner at verification, with its original, expected and candidate revisions.

### Notches are the handback record

- Read each owner's records yourself: `knives notch <branch> --repo <repo>` and `knives status <repo>`.
- New entries use `placement:`, `recon:`, `rehome:`, `repair:`, `record:`, `verify:`, `decision:` and `handback:`, each with `--evidence`. Read older formats too.
- `handback:` and a released claim end the worker's ownership, not the task.
- Accept a thread disposition only with evidence: `-> <commit>`, `declined: <reason>` or `already answered <reply-url>`.
- Record open promises with the reply URL. Do not count them as done. Verify that new replies are published before you report them as addressed.

### Decisions that reach the human

You are the last filter before a question reaches the maintainer of record. Before you forward an owner's `decision:`:

- Read the whole chain of the branch. If an earlier note answers it, write a `decision:` notch that quotes that note and re-dispatch the owner with the answer. Never show the human the question.
- Read the maintainer's words, not the owner's summary. Quote the ask when you show it to the human.
- Bring a position: the quoted ask, the record, what you would do and why, and what changes under the alternative. Use the repository's terms and expand every identifier on first use.
- A design or scope question goes to the human only when it needs authority you lack. Examples: money, an upstream commitment, or removal of what the human asked for. Missing confidence is your work.
- Record the ruling as a `decision:` notch on the branch, with its full reasoning. When the ruling sets a standing rule, record it also with `knives notch --repo <repo>`, which every dispatch reads.

### Single writer and release-wide changes

- The claim holder writes the branch. Owners add repair commits and never rewrite shared ancestry. A lone branch's rebase follows `fork-work`. A release-member rebase is yours.
- The release claim is the write lease for release refs and release-wide changes. It does not let you push an owner's PR head.
- Before a release-wide write, stop admitting owners for that repo and wait until **every affected member claim is released**. Hold the release claim while you integrate. Verify again just before the write that no claim exists. If a new external claim appears, back off. Other repositories continue.
- Select the rebase target from the need: `knives release --repo <repo> rebase [<target>]`. Bare rebase uses the merged-PR target and can need an explicit one. Record `rehome:` with the target and resulting release commit.
- Exit 0 is not sufficient. Verify that each affected member is on the target, kept its patch and has no conflicts. Use `knives release --repo <repo> members <release> --verify`. A member still on the old base is unresolved work.
- Also run `jj -R <checkout> --ignore-working-copy log -r '<target> ~ ::<member-commit>' --no-graph` per member. Empty output means the target is an ancestor. Content carriage does not prove that each member moved.
- **A member with no conflict left is not yet a member that works**. At each rebased tip, conflicted or clean, run the type checker and its own tests. Audit the new trunk call sites against what the member changed. Repair a break on the member, not the release commit. Then call the rehome resolved.
- Resume verification for **all affected PR heads**. Never push a rewritten head without its claim and fresh verification. Keep its original, expected and candidate heads separate in the re-dispatch.
- If an external actor moves a branch under an owner, inspect the content from old to new. You can rule on trunk-only movement: record the original head, the new head and the reparented candidate commits. Then the owner reviews and tests again before it pushes.
- A conflict with the user's intent needs a real decision. Never use a verdict again for a different tree.

**Done when:** each packet has a verdict, each handback agrees with notches and current state, and each integration change has one owner and preserved-content evidence.

## 5. Finish release and consumer verification

Do this step when the request includes releases, upstream debt or consumption. PR-only scope does not authorize release edits or deployment.

1. **Reconcile intended content.** Read the release and member notches again. Run `knives release --repo <repo>`, `knives release --repo <repo> members --census` and `knives consumers <repo>`.
   - For each maintained delta, record if the release must carry it, inherits it upstream, excludes it deliberately, or needs repair.
   - Prove carriage with `members <target> --carries <revision>`. A merged upstream PR or a source-text match is not proof. Investigate a conflicted probe. Do not drop it automatically.
2. **Compose the intended release** with `release advance`, `include`, `drop --why`, `rebase` and `cut`, under the integration barrier.
   - Advance repaired members to verified heads. Include an unreleased PR only when it is intended and ready to ship.
   - Read the real consumer pins before you select an in-place edit or a new cut. A consumer frozen on an older release does not block an edit of the current one.
   - When the tool refuses because every pin of the edited release is frozen, cut a new release. Make no parallel lineage and no forced ref move. Keep required old refs reachable. Never cut an identical release only for a new name.
   - **Classify each member in the release notch before you compose it**, as one of:
     - `upstream-PR <url>`: an open or merged upstream PR carries it.
     - `upstream-pending, placement <notch anchor or commit>`: its `placement:` notch and commit body record the UPSTREAM verdict and the defect or capability gap. They also record the reproduction or red→green test that its upstream PR opens from.
     - `neither`.
   - Drop a `neither` member (`drop --why`) or return it to its author. Never compose it.
   - The classification is disclosure, not a gate. An `upstream-pending` member ships when the release is gated and published. The consumer pins it without a wait for the upstream PR.
   - Apply the ownership test of `fork-work` again: *if upstream main replaced the fork tomorrow, would anyone but us miss this?*
   - Operating policy (job lifetimes and reaping, caps, quotas, schedules, alerting) lives in the consumer. A fork member repairs the library's own machinery or adds a capability that nothing outside the library can give.
   - A policy-shaped member is a missing capability (UPSTREAM, with its PR in the same work) or CONSUMER work, which is `neither`. There is no third class.
   - **Sweep each parent that leaves the candidate, line by line, before you cut or publish.** Read and follow [drop sweep](references/drop-sweep.md) before any parent leaves. Sweep all of them, not only those a reviewer named.
   - Do not make a drop whose sweep is not finished. Keep the parent or its rebuild.
   - A heading, `--why`, `--allow-drop`, the drop note or a quiet orphan gate says nothing about the content behind a parent. A member's own content can be superseded while a line that reaches the release only through it is not.
3. **Verify the composed candidate.** Run all applicable gates and exercise the changed public surface on the candidate. Verify every member and integration resolution.
   - Before you publish, run the **resolution-delta guard**. Each later `cut`, `advance` and `rebase` carries a recorded resolution forward. Thus a whole-file restore from one parent silently reverts the content of the other parents in that file.

     ```sh
     jj new '<release>-' -m guard                                   # fresh automerge of the release's parents
     guard=$(jj log -r @ --no-graph -T commit_id)                   # save its id: the restores below need it
     jj diff --from @ --to <release> --name-only | sort > delta      # the recorded resolution delta
     jj resolve -r @ --list | awk '{print $1}' | sort > conflicted   # the paths that genuinely conflict
     comm -23 delta conflicted                                       # must print nothing, or only recorded semantic conflicts
     jj abandon @                                                    # drop the guard commit
     ```

   - For each printed path, put the automerge content back (`jj restore --from "$guard" --into <release> <path>`). Then resolve only the conflicted paths again, hunk by hunk.
   - Never resolve a release conflict with `jj restore --from <one parent>` of a whole file.
   - The exception is a **semantic conflict**: the members merge cleanly, but the composition fails a gate. Repair it on the release commit, never on a member, because a member must stay valid alone.
   - Record a semantic conflict in the release notch: path, the two members, the gate error and the hunk verbatim. `jj diff --from "$guard" --to <release> -- <path>` must be byte-equal to that hunk. Any other printed path is a defect.
   - For a pure restructure, require tree identity with the verified reference. Otherwise, account for the exact intended delta.
   - Run **absolute consumer capability contracts**. Comparisons to the previous cut and all structural gates accept a stated drop as expected.
   - Find the contract tests: search the consumer's test trees for the pinned package's name and for the symbols the pin exposes.
   - Run the static tests with the candidate in place of the pin (a `file://` pin, relocked). Live tests need a deployment. Before publish, run them only on a dev stack deployed from the candidate. Otherwise, run them on the pin after publish.
   - A required capability recorded in `knives notch` and lost earlier is a defect to repair, not a baseline.
   - **Judge test failures against a control, not against zero**. Run the same command in the same environment on a pristine export of the target trunk that the suite can run in. Never use a fork workspace. For example, use `git archive <target> | tar -x` and install dependencies.
   - Compare failure sets by test name, not chunk label. A candidate-only failure counts when it occurs in isolation on the candidate and not on the control. A test file only the candidate has counts when it fails deterministically in isolation.
   - A scrubbed run (`env -i`, no ambient credentials, plain `TERM`) is a cheap first test.
4. **Verify the consumer chain.** Identify consumer trunk pins, resolved lock commits, build inputs, submodule or binary artifacts, and intermediate packages that inject their own dependencies.
   - A direct pin does not control a runner's separately installed version. Verify the whole chain against the candidate.
   - Use the project's deployment and verification guidance, not an unrelated upstream dev environment. Local gates, a changed lockfile, a published branch and deployed behavior are separate evidence.
5. **Publish and consume deliberately.** A local cut or edit is not published. A published ref is not consumed until the authoritative consumer resolves it.
   - Publish to the registry's publish remote (`release`, otherwise `origin`). Verify the remote ref with `knives pushed`.
   - Do the consumer pin, build and deployment that the goal requires. Never deploy when excluded, and never bypass credentials or approvals.
   - Verify the final resolved revision, and the running revision where required, and exercise the behavior there. Verify again after a relevant change.
   - Record `record:` and `verify:` on the release with candidate, published and consumed commits, and gate and runtime evidence.
   - **The pin PR body discloses the delta** under `## Production behavior delta`. For each member that the release adds or moves, state what production does differently, in plain numbers, and which deployments carry it. Mark upstream-pending members. Write `none` when nothing changes.
   - Write the delta from the member classification and notches, not from the release title. It is disclosure, not a gate.

When a real external prerequisite blocks publication or consumption, finish other work, record the exact blocker and report the stage **blocked**. Do not report it ready with a follow-up.

Decide ordinary membership, naming, rehome and compatible-repair questions from evidence and earlier notches. Ask the human only for missing authority, a change in direction, or a conflict with stated intent.

**Done when:** intended content is verified in the release and at the requested consumer or runtime boundary, or you report that boundary blocked with evidence. A PR push alone cannot finish this step.

## 6. Report verified outcomes

- Build the report from saved observations and `knives notch` chains. Refresh the population for PRs that moved or merged. Reconcile them, and never lose a row.
- Every original item keeps a disposition. Account for every new in-scope obligation.
- Per PR: maintainer asks, each repair or disposition with evidence, verification on the exact head, unresolved threads, remaining forbidden hits with reasons, real blockers.
- Per release-only member: owner or disposition, carriage evidence and verification. Do not pretend a PR review occurred.
- Per release: intended composition, candidate/published/consumed revisions, absolute capability and runtime evidence, and any unmet boundary.
- Use separate statuses: **accounted for**, **verified**, **published**, **consumed**, **blocked**. A handback or full inventory proves only accounting. A decision note is not a repair.
- Never report a skipped gate, an unstarted workflow or a unit suite as end-to-end verification.
- Each cell cites a notch or saved observation:

  `<repo>#<n>: CI <head-specific run conclusions/pending> / review comments <unresolved, dispositions> / review <fresh-context verdict> / e2e <actual surface evidence or not run> / forbidden <remaining hits>`

  `<repo>/<release>: composition <verified|blocked> / published <revision|not published> / consumers <resolved revisions|blocked> / runtime <evidence|not run>`

- Pending executable CI stays yours after handback. Watch it with a real background watcher or event subscription, and repair failures before you call the PR verified.
- Approval-gated `action_required` stays **unrun**. Reproduce the job locally and state the limit. Never ask maintainers to approve workflows.
- Continue other work while an item waits. Upstream PRs stay yours after handback. Release verification does not wait for an upstream merge.
- Do not open unsolicited PRs or issues, go around credential gates, or change shared or global configuration to make a gate green.
- Evidence and the requested boundary decide completion, not the number of handbacks.
