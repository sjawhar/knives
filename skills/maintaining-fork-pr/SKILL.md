---
name: maintaining-fork-pr
description: Use when dispatched to own one fork pull request — an orchestrator running the fork sweep has handed you a single PR's coordinates (repo, number, URL, branch; the PR facts you gather yourself) and the repo's section of the estate document, and you are the agent who takes that PR from its current state to reviewed, repaired, verified and handed back.
---

# Owning one fork pull request

One agent, one pull request, one claim held from the first step to the last: one agent owns each PR
end-to-end through every step below. The record is the branch's notch chain and, after step 6, the
published PR; the workspace holds code only. Drafts, gate logs and the reviewer packet live in the
sweep's scratch directory your dispatch names (failing that, one under your session's temp dir, named
by path in your report): anything written inside the workspace snapshots into `@`.

Your dispatch supplies what this skill leaves open: the PR's coordinates (registry repo name and
upstream slug, number, URL, branch); the branch's `knives status` row; the repo's section of the
**estate document** (the per-library tacit-knowledge document your estate keeps; its `##` section for
this repo carries the gate commands, what a PR owes these maintainers, *CI reality*, the maintainers'
known positions and the outbound-prose rule); the `pr-preflight` skill text; the orchestrator's address
and scratch directory; and the **term list** of forbidden identifiers you scan the diff and body for
(if the dispatch carries none, ask the orchestrator for it over `hub` before recon — never guess a
list). The **maintainer of record** is the human who owns the estate: `gh auth status` names the
account, every publish action writes upstream as it, and `decision:` notches escalate to them. Read
all of it before step 1. `fork-work` is the on-ramp for a managed fork; `using-knives` the reference
for every knives command; `using-jj` for version control. PR facts (head, mergeability, review
decision, checks, threads, template, body) you gather yourself in step 2 with `gh` and `jj`; knives
supplies the claim, the status row and the release membership. Every notch begins with one of
`recon:`, `rehome:`, `repair:`, `record:`, `verify:`, `decision:`, `handback:` and cites `--evidence`
(a commit id, a `file:line`, a PR URL); a `file:line` is cited at the head commit the notch is anchored
to, never a patch line. An entry without a prefix or evidence is invisible. The `knives notch` and
`knives status` displays truncate a note near 768 characters; `--json` is complete (`knives notch
<branch> --json` is `.entries[]`, oldest first), so read notches that way.

## 1. Claim

Until you stand in the workspace, every knives command takes `--repo <repo>`, the registry name
(`status` takes the name positionally instead). `status` exits 1 on findings and 3 when some branch
could not be answered (`.problems`); the JSON is complete either way, so proceed when your row is.

```
knives start <branch> --repo <repo> --why '<PR number and what you were dispatched to do, one line>'
knives status <repo> --json | jq '.branches[] | select(.name=="<branch>")'
jj log -r '@ | <branch>'     # in the workspace: @ is an empty child of the branch tip
```

Then `cd` into the workspace `start` prints; every command below runs there. A `start` that pauses is
waiting for another writer on the claim lock (`using-knives` has the wait): let it. A refused `start`
names the holder: report it to the orchestrator verbatim and stop; never `--force`, the holder is
working. A `start` refused because the bookmark is **divergent** names the tips: compare them (`jj -R
<fork path> --ignore-working-copy diff --from <a> --to <b>`, path from `knives repos --json`); same
content, run the `jj bookmark set … --allow-backwards` the refusal prints (with `-R <fork path>
--ignore-working-copy`: no workspace exists yet) on the tip `<branch>@origin` names (the PR head: `jj
-R <fork path> --ignore-working-copy bookmark list <branch> --all-remotes`; the one write allowed
without a claim, since none can exist on a branch `start` refuses) and start again; different
content, or neither tip origin's, `knives notch <branch> --repo <repo> -m "decision: <branch> is
divergent, <a> vs <b>: <what differs>"` with both tips as `--evidence`, report it to the orchestrator
verbatim, and stop. The bookmark stays on the tip until step 6. Every agent here may share one OS
identity, so a claim is yours by its `why` text. **Done when** `knives start` returned a workspace
path, you stand in it, and your row's `claim.why` is the text you gave.

## 2. Recon

Read everything before you change anything, in this order. `<trunk>` is the upstream trunk name
(the registry's `base` for the repo), `<head>` the `headRefOid` the first `--json` call prints; every
`gh` call takes `--repo <upstream slug>` (a bare `gh pr` in the fork resolves to our copy); the graphql
call takes the slug's two halves, `<upstream org>` and `<upstream repo>` (never the registry name
`<repo>`), and fetches the inline threads `--comments` omits (unresolved = `isResolved` false):

```
gh pr view <n> --repo <upstream slug> --json title,headRefOid,baseRefName,mergeable,mergeStateStatus,reviewDecision,body,comments,reviews   # baseRefName must be <trunk>, else `decision: PR base is <baseRefName>, not <trunk>` with the PR URL as --evidence, report it to the orchestrator verbatim, and stop; comments/reviews are the footprint step 7 compares against
gh pr view <n> --repo <upstream slug> --json body --jq .body > <scratch>/body-<n>.md
gh pr view <n> --repo <upstream slug> --comments
gh pr checks <n> --repo <upstream slug>
gh api 'repos/<upstream slug>/actions/runs?head_sha=<head>' --jq '.workflow_runs[] | [.name,.status,.conclusion,.html_url] | @tsv'
gh api graphql -F owner=<upstream org> -F repo=<upstream repo> -F n=<n> -f query='query($owner:String!,$repo:String!,$n:Int!){
  repository(owner:$owner,name:$repo){ pullRequest(number:$n){ reviewThreads(first:100){ nodes{
    id isResolved path line comments(first:50){ nodes{ author{login} body url } } } } } } }'
jj log -r '<branch>'                                                # tip must equal <head> (re-read both once: forge lag heals itself); else `decision: local tip <tip> is not the PR head <head>` with both as --evidence, report it to the orchestrator verbatim, and stop
jj log -r 'fork_point(<trunk>@upstream | <branch>)::<branch>'      # the commits the PR carries
jj diff --git --from 'fork_point(<trunk>@upstream | <branch>)' --to <branch> > <scratch>/patch-<n>.diff
jj --ignore-working-copy file show -r <branch> <file> | grep -n '<text>'   # line numbers for file:line come from file show, never from the diff
grep -n -i -E '^\+.*(<term>|<term>)' <scratch>/patch-<n>.diff; grep -n -i -E '<term>|<term>' <scratch>/body-<n>.md   # the forbidden scan: added lines and body only, not thread metadata
jj --ignore-working-copy file show -r <trunk>@upstream .github/pull_request_template.md   # or PULL_REQUEST_TEMPLATE.md, .github/PULL_REQUEST_TEMPLATE/*.md
knives notch <branch> --json
knives release members --json      # bare: the release in hand
```

`gh pr checks` lists only the runs that started; the workflow-runs call is where a fork's suites sit
in `action_required` waiting for a maintainer (the repo section's *CI reality* says whether that is
this fork); step 6 re-runs it for the counts. The diff is taken from the fork point, never `--from
<trunk>@upstream`, which once trunk has moved is the reversed trunk, not the PR. `(divergent)` on a
branch commit in `jj log` means an older copy of the change survives in the store: note it, cite commit
ids not change ids, never abandon it. The template's `#`/`##` headings outside HTML comments and fences
must each appear in the body; list the missing ones. The branch is a **member** when the release in
hand's `members[].held_by` names it (`jq --arg b <branch> '[.members[].held_by[]] | index($b) !=
null'`), otherwise a **lone branch** (knives holds one release in hand per repo; the status report's
other `releases[]` entries are published or superseded cuts whose commits keep their parents when a lone
branch moves); the recon notch records it for step 3. A notch anchor not on the branch is a predecessor
of one of the branch's commits (`jj log -r <anchor>`, same change id); after a `jj duplicate` in the
branch's past there is no change-id link, and the ledger text says when.

Collect: each maintainer ask still pending (one clause each; a thread holding two asks is two); each
defect in the diff (`file:line`, one clause each); each reply of ours whose named commit is no longer
on the head, or that belongs to another PR (its disposition is a correcting reply in step 5, unless a
later reply on the thread already corrects it: then `addressed (already, <url>)`); each *What a PR owes
the maintainers* item of the repo section, checked at the head, one clause each; each template heading
missing from the body; each body claim to re-verify (the body's Testing commands are judged at step 4;
step 5's body carries the final truth); and the forbidden hits from the scan above (an identifier in
prose counts). Every ask, finding and forbidden hit carries a disposition; a non-defect observation is
a `note:` clause or omitted. The whole list is your working list in the scratch directory (the
reviewer packet carries it); the notch records what has a reader:

```
knives notch <branch> --pr <n> -m "recon: <member of <release> | lone branch>; <k> maintainer asks pending: <one clause each, with disposition>; findings: <file:line one clause each, with disposition>; forbidden hits: <n, each with disposition>; template headings missing: <n>" \
  --evidence <pr-url> --evidence <file:line> …
```

A disposition is one of: addressed (already, at which commit), to address (step 4), declined (with
the reason you will give the maintainer), question for the maintainer of record (Escalation), or
`unverifiable now (<why>)` for a body claim true when written that rests on external state you cannot
reach. A resolved thread still gets one per ask: `isResolved` says the maintainer closed it, not that
the head obeys.

**Done when** the `recon:` notch exists, names member-or-lone, and every ask in every review thread,
resolved or not, and every finding has one of the dispositions in it.

## 3. Rehome

Rehoming is conditional. Move the branch only when the forge says the PR conflicts (`mergeable:
CONFLICTING` in step 2's `--json`) or the recon found something you cannot reproduce or verify without
the branch on the current trunk. A `MERGEABLE` PR awaiting review is left where it is: the force-push
that follows a rebase resets every reviewer's context for nothing. A lone branch is yours to move: `jj
rebase -b <branch> -d <trunk>@upstream`. A member is not: `knives release rebase` moves every member
branch of that release, including branches other owners hold right now. Write `decision: parent of
<release>; needs knives release rebase between waves` with the release commit as `--evidence` and hand
back (step 7); the orchestrator rebases once, between waves, and re-dispatches you. Never `jj
duplicate` a feature or fix branch, whatever the shape: knives has the sanctioned moves. **Done when**
either no rebase was needed (the `handback:` notch says so), or `jj diff --from <old tip> --to <new
tip>` shows trunk context only and the notch `rehome: <branch> onto <trunk>@upstream; old→new diff is
trunk context only` carries both tips as `--evidence`; the push is step 6's.

## 4. Repair

One commit per recon finding, on `@` in your workspace; a commit that fixes two findings cannot be
reviewed against either. Your `repair:` commit's body records red → green on the same head: the
failing command and its output before, the passing command and its output after; a fix whose failure
you never saw is a guess. Pre-sweep commits carry no such body; the reviewer judges them empirically
in step 6. Nothing is pushed here; step 6 pushes, after the reviewer's verdict. You are
**append-only** on the branch: a repair is a new commit on top of the tip, and step 6's `jj bookmark
set` moves the bookmark to it. Never `jj squash --into`, `jj describe`, `jj rebase` or `jj abandon`
an existing commit of the branch, or anything else in the store: every owner in the fork shares one
jj repository and one operation log, and a rewrite there rewrites descendants they all share (release
merges included) and collides with theirs in the op log. A change that needs a rewrite is a
`decision:` notch and a handback. The only rewrites in a sweep are step 3's lone-branch rebase, the
orchestrator's between-waves `knives release rebase`, and step 1's same-content divergence recovery.

Then run the repo's full gate, the exact commands from the repo section, yourself, in the claimed
workspace, and keep the output for the `verify:` notch (the gate log paths go into the reviewer
packet); never deferred to CI, never reported from a CI run that did not start (the repo section's
*CI reality*). Run every body *Testing & validation* command as written, once, unless it is textually
a gate line; the body's counts (`N passed`) are judged against that run, its output kept beside the
gate log. After the gate, `jj status` must show no changes: a change here is a finding, not a repair. A
recon that found nothing to repair is a legitimate outcome; a repair invented to have something to show
is a defect. Each repair is a notch, `repair: <finding>; red: <failing command>; green: <passing
command>`, with the commit id and the `file:line` as `--evidence`. **Done when** every `to address`
disposition from the recon has a `repair:` notch naming its commit, and the gate ran green in your
workspace on the head those commits produced.

## 5. Record (draft)

The PR body and the review threads are the maintainers' view of the branch. Draft what will make
them true of the head; publish nothing yet: a statement naming a commit waits until step 6 has that
commit on the PR head. Draft in the scratch directory, never in the workspace:

- The body. The maintainer's PR template stays verbatim; every template heading (step 2) appears in the
  final body; for every change since the last human maintainer review (a bot review does not anchor
  the window; with no human review yet the window is empty and the body describes the change as a
  whole), the body states what was requested and what was added.
- One factual reply per review thread that lacks any reply true of the head (an existing reply naming a
  commit no longer on the head, or pasted from another PR, counts as lacking; a later reply that
  already corrects it does not), naming the commit that answers each ask in it (or the reason it is
  declined), keyed by the thread `id` from step 2.
- Whether a re-review is due, and from whom: only a human maintainer whose review is outstanding
  (`reviewDecision` is `CHANGES_REQUESTED`, or their threads are answered by your commits); a bot's
  review never triggers one.
- The forbidden hits on the final diff and body (as in step 2): none, or each remaining hit `declined
  (<why>)`; a hit without a disposition is a finding and blocks publish.

For what a PR body owes the repository, the `pr-preflight` skill's Step 2 checks apply to an existing
PR (its other steps are for opening one); walk them, do not restate them. Its text arrives in your
dispatch (a fork workspace may not resolve `skill://pr-preflight`, or may resolve a stale copy); with
neither, a `decision:` handback beats guessing. Check 1's facts come from step 2 (the status row's
`landed`, tip == head, a single-tip bookmark) where `knives preflight` prints `landed: not probed`. Run
`knives preflight` from the workspace for Check 2: it exits 1 whenever it has anything to say and
prints every branch; read your branch's row and the convention-file block (it records the digests it
reports, so `CHANGED since last seen` shows once per change — read the file then). One override: where
its Check 4 says to open an issue the repository requires, the owner never does; that is Escalation.
Prose you write in this step (body, replies) follows the repo section's outbound-prose rule: identify
as the agent acting on the PR author's behalf, plain and factual (the template's text is not yours).
**Done when** the draft body, every thread reply and the re-review decision exist in the scratch
directory, and the draft body is true of the head the reviewer is about to see.

## 6. Verify, push, publish

**Judgment** first, on the local head, by a fresh-context reviewer the orchestrator dispatches (a
dispatched owner has no `task` tool). Write the packet as `reviewer-packet-<n>.md` in the scratch
directory: the questions, the head commit id and the claimed workspace path, the reviewer rule (read
commands and the gate only, in your workspace; it commits nothing, writes no notch, no jj or gh
mutation), the recon notch and your working list, `patch-<n>.diff` and the commit list at `<final
commit>`, the draft body and replies, the gate log paths; send its path to the orchestrator's address
from your dispatch; wait up to 30 minutes from sending (the window may take several `wait` calls; an
acknowledgement is not the verdict; do not send again), then `decision: reviewer verdict outstanding;
repair commits <ids from the repair: notches>; bookmark not moved` and hand back (the orchestrator
relays the verdict and re-dispatches you at this step). A finding here is each recon ask marked `to
address` or `addressed` and each diff defect. The questions, per finding: does this change address it
and nothing else; is the body true of the head; for your `repair:` commits, do the red → green claims
reproduce; for pre-sweep commits, does the finding reproduce at the fork point and pass at the head;
and once: are any forbidden terms left in the added lines or the body? A question with no object gets
the clause `none`. Paste the verdict the relay returns into the `verify:` notch verbatim; a FAIL blocks
the push and is a `verify:` notch too, before you loop (`fix` → step 4, `body` → step 5, `drift`, the
packet's head no longer the branch tip → "If the branch moves under you"); re-verify from here.

**Push**, after PASS and only then: the bookmark is still on the old tip, so `jj bookmark set <branch>
-r <final commit>` then `jj git push --remote origin -b <branch>` (`origin` is the registry remote the
PR head lives on; the estate document names exceptions; a rehomed branch pushes the same way). Record
the head before and after; with no repairs and no rehome there is no push, say so.

**Deterministic**, on the head the PR now has (the pushed head, or the recon head when nothing was
pushed), always both halves: the repo's gate as it ran in your workspace (the last line per gate
command; the full logs by path in the scratch directory), and the CI state from step 2's workflow-runs
call re-run with that head: the histogram of `conclusion` values spelled out (`success n, failure n,
action_required n, …`; `action_required` is not run for us) plus the runs with no conclusion yet. A
`failure` is a finding: back to step 4, nothing published.

```
knives notch <branch> --pr <n> -m "verify: reviewer: <verdict> (<one clause of evidence per question>); pushed <old head>→<new head> | no push; gate: <command>: <last line> …; CI: <conclusion histogram>, <n> without conclusion" \
  --evidence <final head> --evidence <pr-url>
```

**Publish**, once the head is on the PR and its CI read: the body (skip `gh pr edit` when the draft
equals the published body, `cmp`), each reply on its thread, the re-review request where due, then the
`record:` notch. Every publish action is the maintainer of record's own account (`gh auth status`)
writing upstream: only drafted, reviewer-PASSed text is published.

```
gh pr edit <n> --repo <upstream slug> --body-file -            # draft body on stdin
gh api graphql -F id=<thread id> -f body='<reply>' -f query='mutation($id:ID!,$body:String!){ addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:$id, body:$body}){ comment{ url } } }'
gh pr edit <n> --repo <upstream slug> --add-reviewer <login>   # only a human maintainer whose review is outstanding
knives notch <branch> --pr <n> -m "record: body updated (requested-vs-added per change) | body unchanged (true of <head>); threads: <thread id> -> <commit> | declined: <reason> | already answered <existing reply url> (one each); re-review requested from <login> | none outstanding; forbidden hits remaining: <n, each declined (<why>)>" \
  --evidence <pr-url> --evidence <reply url>   # one reply url per new reply, from the mutation's comment{ url }
```

**Done when** the reviewer's verdict is a PASS, the pushed head is the PR head, the `verify:` notch
carries the gate's last lines and the CI histogram, and body and replies are published.

## 7. Hand back

Standing in the claimed workspace (`finish` from outside is refused: same identity, as in step 1),
take the final PR state, write the notch, then release the claim, in that order: the notch's
`unresolved threads: <n>` and footprint must post-date the push, and `finish` removes the workspace
you stand in and appends a `claim released` event to the branch. The final PR state is the fresh
`gh pr view --json` below, compared with step 2's (comments and reviews prove the publish did exactly
what `record:` says); `mergeable`/`mergeStateStatus` `UNKNOWN` there with nothing changed on the PR is
the forge recomputing: re-run after a minute, or quote step 2's value with its timestamp.

```
gh pr view <n> --repo <upstream slug> --json headRefOid,mergeable,mergeStateStatus,reviewDecision,comments,reviews
gh api graphql -F owner=<upstream org> -F repo=<upstream repo> -F n=<n> -f query='query($owner:String!,$repo:String!,$n:Int!){ repository(owner:$owner,name:$repo){ pullRequest(number:$n){ reviewThreads(first:100){ nodes{ isResolved } } } } }' --jq '[.data.repository.pullRequest.reviewThreads.nodes[] | select(.isResolved|not)] | length'   # unresolved threads after the push
knives notch <branch> --json
knives notch <branch> --pr <n> -m "handback: <what changed>; rehome: none needed (<why>) | onto <trunk>@upstream; unresolved threads: <n>; unverified: <what, or none; every unverifiable-now claim>; decisions for the maintainer of record: <list, or none>" \
  --evidence <final head> --evidence <pr-url>
knives finish <branch>
```

The report to the orchestrator is the notch chain (json), that final PR state (json) and the published
PR, nothing else; scratch is named by path, not pasted. `finish` releases the claim; branch, bookmark
and PR survive by change id. **Done when** the `handback:` notch is your last note and, from any other
directory, your row in `knives status <repo> --json` has no `claim`.

## If the branch moves under you

Another actor's `knives release rebase` can reparent the branch at any step: the working copy goes
stale, `@` follows onto the new tip, notch anchors switch. Run `jj workspace update-stale` once; record
both tips from `jj bookmark list <branch> --all-remotes` (local is the new tip, `@origin` the old one,
still the PR head); never push from that state. Re-run the gate only when you hold a repair to push;
kept output stands, cited at the tip it ran on; later notches cite `file:line` at the new tip and name
both tips. With a push pending (a repair, a rehome), write the notch `decision: branch moved under me,
<old tip>→<new tip>; pending: <k> repair commits | rehome; which head does the PR get` with both tips
as `--evidence` and hand back (step 7): the orchestrator decides which head the PR gets and
re-dispatches you. Otherwise go on.

## Credentials and escalation

You may use the fork remotes as `gh` is already authenticated (the maintainer of record's account, as
step 6 says), and the repo's own venv or toolchain in the claimed workspace; nothing else. Any other
credential or grant is a request to the maintainer of record through the orchestrator (a `decision:`
notch, then hand back); a timeout is a no. Four more things stop the owner where they stand: a
maintainer asking for A or B; a question of whether a change belongs in the fork or upstream; a repair
*you* would make that removes functionality or flips a default (the PR's own thesis is not your
removal); a PR template or `CONTRIBUTING` requirement for an issue that does not exist (the owner never
opens an issue, whatever `pr-preflight` Check 4 says). Each is a notch, `decision: <the question, the
options, what each costs>`, with the PR URL and the `file:line` as `--evidence`. Then straight to step
7 with the decision named in the `handback:` notch: nothing is pushed, nothing is published; the
re-dispatched owner drafts after the ruling, phrased for the maintainers — no internal person, tool or
process name in upstream prose (the term list plus `notch`, `knives`, `orchestrator`). Do not pick a
side to keep the sweep moving.
