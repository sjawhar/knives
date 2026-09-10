---
name: maintaining-fork-release
description: Use when asked to maintain a set of fork pull requests, address upstream debt, or verify and ship the forks' releases and consumer pins. One pull request alone is maintaining-fork-pr; command semantics are using-knives.
---

# Maintaining fork pull requests and releases

You own the requested outcome, not just the dispatched PRs. A PR-only request ends with verified PR maintenance; a request to address upstream debt or make releases good also requires release and consumer verification below. Respect explicit exclusions such as no deployment. Do not turn a PR-only request into a release or deployment change.

## 1. Start with `knives notch`, not a new document

**`knives notch` is the existing read/write record of this work. Read it before deciding, dispatching, or repairing.** It holds prior decisions, maintainer promises, rejections, release changes and verification evidence. There is no separate estate document to find or create, and no Markdown inventory to maintain beside knives.

Use the requested repositories, mapped through `knives repos` and the registry at `~/.config/knives/repos.toml`. An existing project skill can identify a library family and its consumer-specific requirements; it does not replace the live records. The registry name and upstream slug are different identifiers: take the slug from the entry's `upstream` URL, stripping `.git`.

Read these in order, substituting names from the preceding output:

```sh
knives repos
knives status <repo>
knives notch --repo <repo>
knives release --repo <repo> members
knives notch <release> --repo <repo>
knives notch <branch> --repo <repo>
knives notch --pr <number> --repo <repo>
```

Bare `notch` gives recent context (the newest 20 human notes and an event summary), not every branch's history. Named branch/release reads give the full chronology; `--pr` also finds records across a PR's former branches. Read all relevant chains. Older notes without this workflow's prefixes remain valid context: never discard them as invisible. Check timestamps, anchors and evidence against the current head; a note on an earlier tip may have been superseded. A recorded requirement still matters even if a later release lost its implementation. Use `knives notch --verify <subject> --repo <repo>` to check recorded anchors/evidence, then investigate discrepancies rather than treating an old assertion as current proof.

**For direct reading, run the command examples above as written: no `--json`.** Looking up
branch, commit or evidence fields, saving a capture, and preparing a JSON final report are
still direct reading. The model interpreting output is a reader, not an executed JSON parser.

Request `--json` only when the bytes feed executed code doing a real filter, count, join or
other programmatic operation that needs JSON. Keep that consumer with the command evidence.
`jq .`, parse-and-reprint wrappers, or a filename ending in `.json` do not create that need;
do not invent a parser step just to justify the flag. Save native output unchanged for records
you will read yourself.

Default machine output and JSON carry the same data; JSON does not defeat a harness's
line/output truncation. Recover the complete captured output or page its saved form when
truncated. Do not conclude from an elision or change serialization merely to read it yourself.

`fork-work` governs workspace entry; `using-knives` governs command semantics; `using-jj` governs version control. Read upstream contribution files and workflows for current gates and requirements. Read relevant project guidance for consumer wiring. Put new branch/release decisions in `knives notch`, not in another document.

**Done when:** requested repo identities are known and their relevant notch histories have been read, with inherited obligations distinguished from stale assertions.

## 2. Account for release members and open PRs

The maintained population is the **union of release members and open PR branches**, not only the open PRs. An open PR outside the release is still maintained; a release member without a PR is still accounted for; an overlapping branch is counted once as a work item but keeps both identities. Deduplicate PR aliases by upstream slug and PR number, and identify release-only members by the release parent commit. When a release parent, branch tip and PR head differ, keep all three revisions in the row; that is a composition gap to resolve, not a duplicate to collapse.

Use the current `knives status <repo>`, `knives release --repo <repo> members`, `knives audit <repo>` and, for consumer state, `knives consumers <repo>`. Reconcile open-head discrepancies the audit reports before claiming complete coverage. Read the corresponding notch chains before classifying any member. A merged or closed PR does not by itself prove that a release no longer needs its delta.

`status` exit 1 means findings; exit 3 or `problems` means unanswered data. A missing checkout, unconsulted forge, unreadable record, or incomplete page is not zero work. Keep counts provisional and investigate the missing source; proceed on complete independent repos while repairing the gap. A search result is corroboration, not a substitute for the registry and release membership. Repositories outside the requested set are listed once and not dispatched.

Create one scratch directory outside workspaces and `/tmp`, `~/.cache/knives-sweep/<unique-run>/`, with one `<repo>/pr-<number>/` directory per PR and a separate `<repo>/release/` directory. Include a run-unique suffix, not just the date. Save pre/post command output, diffs, drafts and gate logs there in their native formats; name the paths in dispatches and reports. Scratch is working material; notches and published PRs are the record.

Every sweep begins by telling the maintainer of record: `Population: <n> PRs; estimate ~<n*40min/8> wall-clock`. Calculate `n` from the complete live population and send that line **before dispatch**. The report then accounts for `<p>` open PRs, `<r>` release-only members, `<u>` distinct work items across `<n>` requested forks, with coverage and timestamp. State every incomplete source. A PR-only request labels release-only items outside its repair scope; it does not claim they were verified.

**Done when:** every union member is accounted for, all counts cite saved output, and missing observations are explicit rather than silently filtered away.

## 3. Dispatch one owner per PR

One `maintaining-fork-pr` owner per PR, end to end. Waves of eight limit owner concurrency: dispatch at most eight owners together and do not fill another owner slot until each current owner has claimed, refused, handed back, or raised a stopping decision. Each owner is dispatched with a `task` tool and may run several subagents — its own fresh-context reviewer and any gate runners — while retaining end-to-end responsibility for the PR.

**An owner runs on a strong model, never the harness's default fast/cheap subagent tier.** Owning a PR is judgment work — reading a maintainer's question and forming a position, deciding which stale text would actually mislead and which is harmless, recognizing that a `decision:` in the chain already answers today's question. A fast model works the steps as a checklist and produces exactly the failure this skill exists to prevent: correcting replies nobody needed, resolved threads reopened, settled decisions re-escalated. If the harness's generic task agent maps to a fast tier (check its model-role configuration before the first dispatch), select an agent type or model override that runs a strong model, and say which in the dispatch record. The fresh-context reviewer has the same requirement. Only read-only recon that will be re-judged by an owner may run on a fast tier.

A claim held by someone outside this sweep is respected: report its `why` text and continue independent work. Do not infer death from a quiet `seen` field. If an owner stops unexpectedly, establish its state through coordination and the existing claim/workspace, then use the supported claim-recovery procedure before taking over. Never blindly force a claim or restore the shared jj operation log.

Each dispatch contains, in order:

1. The whole shipped `maintaining-fork-pr` skill.
2. Registry repo name, upstream slug, PR number/URL, branch, and its current status row, in the format already captured. For a PR whose branch is a release member, also include the release member row and parent commit. The owner obtains the authoritative PR head from the forge and reports the original PR head, expected branch/bookmark head and candidate head as separate revisions.
3. The relevant **`knives notch` history**, including inherited promises, decisions, release membership evidence and unresolved obligations. Supply complete captured entries or a readable saved path; do not replace them with your summary. The owner refreshes them before acting.
4. Relevant upstream/project guidance, its source paths, and any recorded publication constraints. The owner derives exact gates from current repository files; there is no prerequisite per-repo document. Supply the registry's configured `forbidden` terms and applicable existing outbound-prose rules. If none are configured, say so; do not invent a list. An empty configured list is not a privacy waiver.
5. The whole shipped `pr-preflight` skill, the **per-PR** scratch path, and how the owner gets its fresh-context reviewer: authority to spawn one itself when its harness has a subagent tool, otherwise your address so it can send you the packet path — you then dispatch the reviewer and return the verdict verbatim within the owner's 30-minute window, adding and softening nothing. The verdict is the owner's to record either way.

The orchestrator handles release-only members under the same claim, evidence and independent-review discipline, without inventing PR numbers or opening PRs to satisfy the inventory. A named branch is claimed before anyone edits it; an anonymous member is carried, dropped or classified by release evidence and `knives notch` records, not by fabricating a branch. A bounded branch implementation can be delegated to one owner only when the requested scope already authorizes that work. Classify with evidence: supplied upstream, still proposed upstream, justified fork-only work, downstream configuration, or obsolete. Record the reason and disposition with `knives notch <branch> -m ... --evidence ...` when a branch exists, or on the release/repo subject for anonymous members. Do not automatically include an open PR in a release or upstream every fork-only branch.

Fork-only work uses one permanent `chore/fork-glue` branch. A `keep/*` branch is allowed only when its `--why` states the upstream pull request number that retires it; no temporary integration or release-lineage aliases.

**Done when:** every open PR has one owner or a named reason it cannot be worked, and every release-only item has an owner/disposition appropriate to the requested scope.

## 4. Gate the release in hand between waves

The orchestrator owns the composition gate. After every `release rebase`, `release advance`, or `release include`, and before dispatching the next wave for that fork, gate the release in hand. **Done when:** every member has one fork point against upstream trunk, no member is conflicted, and the release gate is green. A rebase that leaves a member conflicted or on an older base is failed work to repair, not a successful rebase to advance past.

An unpinned release is mutable: edit it in place and republish under the same name. Retain the prior remote head as `keep/release-<name>-<sha8>` on the release remote. A dotted successor is for a predecessor the consumer's main already pins; do not mint a new name merely because it was convenient to do so. A mutable release is safe only while every consumer pins it **by commit**, never by branch name — a name pin followed the moved branch and broke every deployed runner venv the first night this rule was used. The flip side: a commit pin goes stale the moment the lower release moves, so whoever republishes a release checks each registered consumer's lock (`knives consumers <repo>`) for a commit the republish superseded and re-cuts that consumer pin-only, in the same sitting.

## 5. Review, record, and coordinate integration

### Owner-owned fresh-context review

Each owner sends its own packet to a fresh-context reviewer using its `task` tool. The reviewer applies the unchanged verdict contract: `PASS (no repairs)`, `PASS per fix (k/k)`, or `FAIL: fix|body|drift`, with evidence for every question, and independently examines the **whole PR diff**, not only the owner's findings. Owners may dispatch gate runners as siblings, but reconcile their evidence themselves and remain accountable for the verdict and publication gate.

The owner rejects a packet whose PR evidence is only a first page or truncated capture. Comments, reviews, review threads, check runs and workflow runs must be complete for the PR head being judged, or the missing page is an unanswered source and the owner returns to recon. A `null` thread count, elided saved output, or bounded `first:100` result with more pages available is not a clean review input.

The owner records the verdict verbatim. A malformed verdict returns to its reviewer for correction. A reviewer timeout is the owner's work: obtain the verdict and resume verification with the original PR head, expected branch/bookmark head and candidate commits preserved.

### Notches are the handback record

Read, do not merely request, each owner's records:

```sh
knives notch <branch> --repo <repo>
knives status <repo>
```

New entries use `recon:`, `rehome:`, `repair:`, `record:`, `verify:`, `decision:` and `handback:`, each with `--evidence`. Preserve and read older formats. A `handback:` plus released claim ends the worker's active ownership, not the overall task. Accept a thread disposition only with evidence: `-> <commit>`, `declined: <reason>`, or `already answered <reply-url>`. Record outstanding promises explicitly, with the reply URL, rather than counting them as fulfilled. Verify new replies actually published before reporting them as addressed.

### Decisions that reach the human

You are the last filter before a question reaches the maintainer of record, and you own the same judgment `maintaining-fork-pr` asks of an owner. Before forwarding any `decision:` an owner wrote:

- Read the branch's whole chain yourself. If an earlier note answers it, the decision is closed: write a `decision:` notch quoting that note, re-dispatch the owner with the answer, and never show the human the question. One PR carried the same A-or-B question through four owners over three weeks because each orchestrator forwarded the newest `decision:` without reading the one dated before it.
- Read the maintainer's words yourself, not the owner's summary of them. A summary that says "declined the CIDR ask" for a thread that actually said "you introduced a second system, please unify" sends the human a question about the wrong thing and takes several rounds to unwind. When you put a maintainer's ask in front of the human, quote it.
- Bring a position. The human gets: what was asked (quoted), what the record says, what you would do and why, what changes under the alternative — in the repository's own terms, with every identifier expanded on first use. A bare `#1075` or a fork's internal vocabulary is not a question the human can answer.
- A question about the PR's design or scope is not a decision for the human unless it needs authority you lack (money, an upstream commitment, removing something the human asked for). Confidence you lack is your work to do, not theirs.
- A PR's disposition — keep, close, publish, hold fork-only — is the owner's call, made from the record and reported, never asked. A maintainer saying "I might want to close some of these" is a standing instruction to judge each one against that concern, not a ticket to bring each one back. One sweep produced thirteen such "decisions" after the maintainer had already spent a day on the sweep; every one had its answer in a notch (a branch's own note calling the change "our deployment configuration"; a maintainer's review asking for a split that was already prepared). Decide, notch `decision:` with the reasoning, do it, and put the disposition in the report.

Record the human's ruling as a `decision:` notch on the branch, in full — the reasoning, not just the verdict — so no later owner can re-open it by reading only the prefix. A ruling that establishes a standing rule (for example, "infra PRs bake in the upstream's defaults and our settings live in our own configuration") is also recorded on the repository subject with `knives notch --repo <repo>`, where every future dispatch reads it.

### Single writer and release-wide changes

Ordinary branch writes belong to its claim holder. Owners append repair commits; they do not rewrite shared ancestry. A lone branch's necessary rebase follows `fork-work`; a release-member rebase belongs to the orchestrator. The release claim is a seconds-long mutex on one release write, never a schedule: two owners whose members do not overlap include into the same release one after the other without coordinating, and each proves its member on its own integration branch (`jj new <member> <release>`) before touching the release. Never make an owner wait for another owner's claim, review, or publish; the only thing that waits is the write itself. One release in hand per fork, and every mutation under `knives start <release>`.

```sh
knives release --repo <repo> members
knives status <repo>
knives release --repo <repo> rebase [<target>]
```

Choose the target from the actual need; bare rebase uses the documented merged-PR target and may require an explicit target. Record `rehome:` with the target and resulting release commit. Exit 0 is not enough: confirm each affected member is actually on the target, its patch survived and its commits are conflict-free. A member still on the old base is unresolved work. Resume verification for **all affected PR heads**, not just the owner that requested the rebase; never push their rewritten heads without their claims and fresh verification, and keep the original PR head, expected bookmark head and candidate head distinct in the re-dispatch.

Use the existing release plan and `knives release --repo <repo> members <release> --verify` to check current parents and content carriage. Also check target ancestry explicitly for each member with `jj -R <checkout> --ignore-working-copy log -r '<target> ~ ::<member-commit>' --no-graph` (empty means the target is an ancestor), and check each member's conflicts. Content carriage or the release merge containing the target does not prove every member moved onto it.

If an external actor moves a branch under an owner, inspect old-to-new content. Trunk-context-only movement can be ruled on by the orchestrator, with the original head, chosen new head and reparented candidate commits recorded. Resume the owner to re-review/re-test the resulting candidate before pushing. A semantic conflict with the user's intent requires a real decision; do not guess or reuse a verdict for a different tree.

**Done when:** each packet has a verdict, each handback has been checked against notches and current state, and every integration mutation has one owner and preserved-content evidence.

## 6. Finish release and consumer verification

Run this phase when the request includes releases, upstream debt or consumption. PR-only scope does not authorize release edits or deployment.

1. **Reconcile intended content.** Read the release and member notches again, the release plan, the content census and consumers:
   ```sh
   knives release --repo <repo>
   knives release --repo <repo> members --census
   knives consumers <repo>
   ```
   For each maintained delta, record whether the intended release must carry it, inherits it upstream, deliberately excludes it, or needs repair. Use `members <target> --carries <revision>` for content carriage; an upstream PR's merged state or a source-text match alone is not proof. A conflicted probe needs investigation, not an automatic drop.
2. **Compose, gate, publish, then contribute upstream.** The order is fixed: branch → fresh-context review PASS → include/advance it into the release in hand at one fork point → gate the octopus → publish to the release remote → pin the consumer → agent-c CI and dev1 green → open or push the upstream PR **last**. Use `release advance`, `include`, `drop --why`, `rebase`, `cut`, and `republish` under the integration barrier. Advance repaired members only to verified heads. Include an unreleased PR only when it is intended and ready to ship. An unpinned predecessor is mutable: edit it in place, retain its old remote head, and `release republish`; make a dotted successor only when agent-c main already pins the predecessor. A consumer frozen on an older release is behind but does not block editing the current release; a tool refusal that every pin of the edited release is frozen on a revision means an in-place edit reaches nobody and a new cut is required. Do not generate an identical release or a parallel lineage.
3. **Verify the composed candidate.** Run its complete applicable gates and exercise the changed public surface on the candidate itself. Check every member and integration resolution. For a pure restructure, require tree identity with the verified reference; otherwise account for the exact intended delta. Then run **absolute consumer capability contracts**: a capability lost before the preceding release will escape a previous-cut comparison. Read historical requirements in `knives notch`; a previously lost required capability is a defect to fix, not a baseline to accept.
4. **Verify the consumer chain.** Identify authoritative consumer trunk pins, resolved lock commits, build inputs, submodule/binary artifacts and any intermediate package that injects its own dependencies. A consumer's direct package pin does not necessarily control a runner's independently installed version. Verify the full relevant chain against the intended candidate. Use existing project deployment/verification guidance for the real surface, not an upstream's unrelated dev environment. Local gates, a changed lockfile, a published branch and deployed behavior are distinct evidence.
5. **Publish and consume deliberately.** A local cut or in-place release edit is not published; `release republish` retains the old remote head and publishes exactly the replacement release and `keep/` ref. A published ref is not consumed unless the authoritative consumer resolves it. Verify the remote ref with `knives pushed`, then complete the consumer pin/build/deployment steps required by the user's goal. Do not deploy when explicitly excluded or bypass credentials/approvals. Verify the final resolved and, where required, running revision and exercise the intended behavior there. Re-run verification after a relevant change. Record `record:` and `verify:` on the release with exact candidate, published and consumed commits plus gate/runtime evidence.

If publication or consumption cannot proceed because of a real external prerequisite, complete independent work, record the exact blocker and report that stage **blocked**, not ready with a follow-up. Do not ask the human to ratify ordinary membership, naming, rehome or backward-compatible repair judgments already within the task; consult evidence and prior notches and decide. Ask only for missing authority, a genuine change in direction, or a conflict with the user's stated intent.

**Done when:** intended content is verified in the release and at the requested consumer/runtime boundary, or the unmet boundary is explicitly blocked with evidence. A PR push alone cannot satisfy this phase.

## 7. Report verified outcomes, not just completed accounting

Assemble the report from the saved live observations and the **`knives notch` chains**. Refresh the population for PRs that moved/merged during the sweep; reconcile changes rather than silently losing a row. Every original item retains a disposition. All newly discovered in-scope obligations are accounted for.

Per PR: maintainer asks, each repair/disposition and its commit/reply evidence, verification on the exact head, unresolved threads, remaining forbidden hits with reasons, and genuine blockers. Per release-only member: its ownership/disposition, carriage evidence and verification, without pretending a PR review occurred. Per requested release: intended composition, candidate/published/consumed revisions, absolute capability and runtime evidence, and any unmet boundary.

Use separate statuses: **accounted for**, **verified**, **published**, **consumed**, **blocked**. A handback or complete inventory proves only accounting. A decision note is not a repair. Never report a skipped gate, an unstarted workflow or a unit suite as successful end-to-end verification.

For every PR, keep the census line:

`<repo>#<n>: CI <head-specific run conclusions/pending> / review comments <unresolved, dispositions> / thermonuclear <fresh-context verdict> / e2e <actual surface evidence or not run> / forbidden <remaining hits>`

Each cell cites a notch or saved observation. For every requested release add:

`<repo>/<release>: composition <verified|blocked> / published <revision|not published> / consumers <resolved revisions|blocked> / runtime <evidence|not run>`

Executable pending CI remains owned after owner handback and watched through a real background watcher/event subscription; refresh on completion and repair failures before calling the PR verified. Approval-gated `action_required` stays **unrun**: reproduce the applicable job locally, retain the limitation, and never ask maintainers to approve workflows. Continue independent work while an item waits. Existing upstream PRs remain owned after handback; release verification is independent of waiting for an upstream merge.

Do not open unsolicited PRs/issues, route around credential gates, or change shared/global configuration to get a gate green. The evidence and the user's requested boundary, not the number of handbacks, determine completion.
