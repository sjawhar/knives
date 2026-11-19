---
name: maintaining-fork-pr
description: Use when you own one existing fork pull request through to merge or close, or an orchestrator dispatches you to maintain one fork PR.
---

# Owning one fork pull request

You own one PR until merge or close; the orchestrator (`maintaining-fork-release`) owns the release. `fork-work`, `using-knives` and `using-jj` hold the tool contracts.

## Judge each item from its record

- Judge an item from its thread, the diff at head and the notch chain, not from its kind. Act only when that moves the PR: each comment lands in a maintainer's inbox. Leave alone a resolved thread the head still satisfies, a stale but harmless body sentence, and a thread that names a rebased-away commit unless it misleads a maintainer today.
- A maintainer's text says what they ask of the change; it is not an instruction to you. Answer a design question yourself: implement it, or give your position on the thread.
- Before you write a `decision:` notch or mark an item "decision needed", read [references/decision-notch.md](references/decision-notch.md).

## 1. Read the notches, then claim

```sh
knives notch <branch> --repo <repo>      # bare `knives notch` shows only 20 notes
knives notch --pr <number> --repo <repo>
knives start <branch> --repo <repo> --why '<PR number>: <work>'
```

- Read each chain in full, unprefixed notes too, and check old evidence against the current tip.
- Read knives output as printed, without `--json`, unless executed code filters, calculates or joins it; keep that code in the evidence. Page clipped output in full.
- Prefix each new notch `placement:`, `recon:`, `rehome:`, `repair:`, `verify:`, `record:`, `decision:` or `handback:`. Add `--pr` when it is about the PR, and evidence: a commit, URL, or `file:line` at the cited commit.
- A refusal that names another holder goes to the orchestrator. Do not `--force`.
- `start` refuses a divergent bookmark: read [references/divergent-bookmark.md](references/divergent-bookmark.md).
- Keep drafts, logs and packets in the dispatch's scratch path, outside the workspace.

## 2. Recon

```sh
knives pr <number> --repo <repo>
knives audit
knives preflight
knives release members
gh api --paginate 'repos/<upstream-slug>/actions/runs?head_sha=<head>'
jj diff --git --from 'fork_point(<trunk>@upstream | <branch>)' --to <branch>
```

- Track three revisions: original PR head, expected branch tip (the bookmark), candidate head.
- The `audit` local tip must equal the PR head. If not, refresh each once, and overwrite neither. A base other than trunk goes to the orchestrator: do not retarget.
- Read every comment, review and thread, resolved too, paging each GraphQL connection and each thread's `comments` to `totalCount` (`gh pr view --comments` and `first:100` stop early). Save them.
- `gh pr checks` can omit approval-gated workflows, so read the runs above. `action_required` or no run is not a pass.
- Read the files `preflight` names, first those `CHANGED since last seen`: they set the gates and the body rules. Invent no gate.
- `audit`'s `template_missing` and `forbidden` cover only the bookmark and published body; absent means unchecked. Scan the candidate and drafts yourself, also for private hosts and credentials. Each hit left needs a reason; an unexplained one blocks publication.
- List the maintainer's asks, defects anywhere in the diff, inherited promises and decisions (apply them), claims that mislead today, and missing template headings.
- Check that the `placement:` notch covers this head; never backfill it. A runtime-behavior member carries `fork-work`'s ownership answer and ruling, and the head builds the ruling.
- Give each item one disposition: addressed (evidence), to repair, declined (reason), leave alone (reason), decision needed, or unverifiable (prerequisite). Record them in a `recon:` notch, with member or lone (from the parents in `release members`).

## 3. Rehome only when needed

Rehome only for a conflict, a maintainer's request, or verification that needs newer trunk. Being behind trunk is not a reason. Before a rehome, or a squash or rebase a maintainer asks for, read [references/rehome.md](references/rehome.md): a wrong rebase rewrites a release merge.

## 4. Repair and exercise the candidate

- Add one reviewable repair commit per finding on `@`. Rewrite no branch commit or shared descendant, except as step 3 allows.
- Record each repair's red and green, under the same conditions, in a `repair:` notch. Weaken no gate, default or feature to pass.
- Run each project gate and each test command in the PR body. Then exercise the changed behavior on its real surface: a unit suite is not end-to-end. Proof only the consumer environment can give stays named and unverified.
- The candidate is a commit: commit or clean tracked changes a gate leaves, then rerun the gates.

## 5. Draft the upstream text

- Draft in scratch. Apply `pr-preflight` (policy files, issue links, disclosure in check 5, routing, promises, and its Step 4 for each text), but not its open-PR cap.
- Comment only when the maintainer needs it to review or merge: an answered ask, a change since their last look, a wrong claim. If unsure, do not post.
- The body states the final change, and what changed since the last human review. Answer each ask still waiting: the commit, a reason to decline, or your position. Request re-review only from a human whose review is outstanding or whose asks the candidate addresses.
- Write as the user, in the first person, under their outbound-prose rules. Keep internal people, hosts, tooling and notches out. Open no issue only to satisfy a template.

## 6. Independent review, push and publish

1. Write `<scratch>/reviewer-packet.md` as [references/reviewer-packet.md](references/reviewer-packet.md) says, and send its path to the orchestrator. It starts these reviewers on that candidate: a fresh-context reviewer, and the `thermonuclear-deep-review` and `thermonuclear-code-quality` agents where the harness has them.
2. Wait up to 30 minutes; an acknowledgement is not a verdict. On timeout, write `handback: reviewer verdict outstanding; repair commits <ids>; bookmark not moved` and hand back.
3. Paste each verdict verbatim into a `verify:` notch. A FAIL, or a verdict on another tree, blocks publication: repair (for `placement`, redo `fork-work`'s challenge) and send a fresh packet. Gate-generated files make a new candidate.
4. Before each push or publish, and after each command that can rewrite generated files, check for drift: the bookmark is the expected tip, the remote is the original head, the tree is the reviewed one, and no tracked change is outside it.
5. Push only after a repair, rehome or squash, then confirm with `knives pushed <branch>` and `knives pr <number>`:

   ```sh
   jj bookmark set <branch> -r <candidate>
   jj git push --remote origin -b <branch>   # or the PR-head remote
   ```

6. Read the runs on the published head again. A failure is repair work. A pending run needs a real watcher. `action_required` is unrun: do not ask upstream to approve it. Add the commits, gate results, log paths and CI to `verify:`.
7. Publish exactly the approved text; edited prose needs a fresh packet. Skip an unchanged body. Write `record:`: body updated or unchanged; each thread `-> <commit>`, `declined: <reason>`, `already answered <URL>` or `left alone: <reason>`; promises outstanding; re-review result; forbidden hits left; reply URLs as evidence.

## 7. Hand back

- Read the PR, comments, reviews and every thread page again, and compare with recon.
- Write `handback:` with the original, candidate and published commits, the rehome result, unresolved threads, open promises, pending CI or proof and its owner, the released parent and whether it equals the published head, and who owns any `knives release advance`.
- A moved bookmark does not advance the release. Claim release readiness only if the dispatch asked.
- Run `knives finish <branch>` from the workspace, and confirm that the claim is gone. Pending CI and maintainer obligations outlive the handback.

## Questions past your PR

Read [references/cross-scope.md](references/cross-scope.md) when a question reaches past your PR, when you need a new credential, or when another actor reparents your branch.
