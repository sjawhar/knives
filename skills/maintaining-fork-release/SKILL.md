---
name: maintaining-fork-release
description: Use when asked to review, fix, or land the fork pull requests as a set — a sweep over the open pull requests of the forks knives manages rather than work on one of them. One pull request is the `maintaining-fork-pr` skill; a knives command is `using-knives`; stepping into a fork checkout is `fork-work`.
---

# Sweeping the fork pull requests

## You own the sweep

You are the owner, the agent orchestrating the sweep. The scope is the open pull requests of the
forks that have a `##` section in the **estate document** — the per-library tacit-knowledge document
your estate keeps for its forks: one `##` section per repo, named for the tail of the registry entry's
upstream slug, plus one `## For every dispatch` section holding the outbound-prose rule and the term
list of forbidden identifiers — and nothing else: knives manages more repositories (`knives repos`),
out of scope. The **maintainer of record** is the human who owns the estate: `gh auth status` names
the account every publish action writes as, and `decision:` notches escalate to them. You own every
pull request you sweep until it merges or the maintainer of record reassigns it: one you dispatched and
never heard back about is still yours. The numbered steps below are in order; each ends with the
observable state that lets you move on. What an owner does is `maintaining-fork-pr`, pasted whole into
every dispatch; this skill does not restate it, and names that skill's section where an owner rule
matters here.

**Done when** you can name the forks in scope, not from memory: the registry is
`~/.config/knives/repos.toml`; a repo's upstream slug is the `owner/repo` path of its `upstream`
URL with any `.git` stripped (the PR URL carries the same `owner/repo`); the `##` section name is
that slug's tail, which may differ from the registry name.

## 1. Population

Create the sweep's scratch directory first, outside `/tmp` and outside every workspace
(`~/.cache/knives-sweep/<date>/`); name it in the report and in every dispatch. It holds working
material only (the JSON below, the reviewer packets); the record is the notches and the published PR.

```
knives status --all --json
```

One array, one report per registry entry (every managed repository, not only the forks in scope),
each with `repo`, `releases[]` and `branches[]` (the row shape is `using-knives`'s; you read two
fields, `pr.state` and `claim`). The population is every row of a fork in scope whose `pr.state` is
`"open"`: `jq '.[] | select(.repo == "<repo>") | .branches[] | select(.pr.state? == "open")'`. Count
it; that count is the first line of your report, and it comes from this run — never from memory, a
prior sweep, a tracker, or a `gh` search (`gh` is the maintainer of record's account and `--author
@me` resolves to them; the registry, not a search, defines the population). Open rows in the other
reports are listed once as out of scope and are not dispatched. A row with `claim` set is already
held; note the holder by its `why` text (`maintaining-fork-pr` step 1). The facts about each pull
request (head, mergeability, review decision, checks, threads, template) are the owner's to gather in
its recon with `gh` and `jj`; you dispatch from the status row alone. `status` exits non-zero on
findings with the JSON still complete (`using-knives`). Save the output whole to the scratch directory
(`status.json`) so the report's every number points at a file.

**Done when** the report's first line reads `Population: <n> open pull requests across <m> forks
in scope (knives status --all --json, <timestamp>)`, every open row in scope is saved, and
out-of-scope open rows are listed.

## 2. Dispatch

One `maintaining-fork-pr` owner per pull request. No role lanes: no "rebase lane", no "review
lane", no agent that touches several branches. Waves of eight: a wave's owners are dispatched
together; the next wave goes out when every dispatched branch shows a `claim` in `knives status
<repo> --json`, or its owner reported a refusal, or a stopping `decision:` or a `handback:` notch is
present. `start` waits for the claim lock (`using-knives` has the wait), so eight concurrent starts
serialising behind each other's fetches is normal — a waiting `start` waits on a live holder, not a
hung owner. The owners of one fork share one jj operation log, so a wave stays safe only because
owners append, never rewrite; `maintaining-fork-pr` step 4 lists the sanctioned rewrites, one of which
— `knives release rebase` — is yours (step 4 below).

The dispatch prompt is, in this order:

1. The whole text of the `maintaining-fork-pr` skill (`skills/maintaining-fork-pr/SKILL.md`, beside this skill).
2. The pull request's coordinates: registry repo name and upstream slug, number, URL, branch, and the
   branch's whole status row from step 1, pasted as JSON (no `head`: the owner derives it from
   `headRefOid` in its step 2; the row's `tip` may be absent — divergent — and is not the head).
3. The repo's section of the estate document and its `## For every dispatch` section, both pasted
   whole. The repo's `##` section is named for the upstream repository — the tail of the upstream
   slug in the coordinates — not for the registry name; it carries the gate commands, what a PR owes
   those maintainers, *CI reality* and the maintainers' known positions. `## For every dispatch`
   carries the outbound-prose rule and the term list. The owner runs in a fork workspace where your
   estate's skills are not discoverable, so both arrive in the prompt or not at all.
4. The whole text of the `pr-preflight` skill (`skill://pr-preflight`, shipped with knives; a fork
   workspace may not resolve it, or may resolve a stale copy).
5. Your address for the reviewer relay below (your `hub` id, or your messaging session id) and the
   sweep's scratch directory path from step 1.
6. The term list of forbidden identifiers the owner scans the fork-point diff and PR body for by
   hand, from `## For every dispatch` — always.

A pull request whose row shows `claim` held by someone who is not one of your owners is not
dispatched: name the holder in the report and leave it. A divergent bookmark is just a row; the
owner's step 1 meets the refusal and resolves it.

**Done when** every open pull request in the population has exactly one owner dispatched (or a
named reason it was not), and every dispatched branch of the wave shows a `claim` in `knives
status <repo> --json` (or its owner reported a refusal, or a stopping `decision:` or a `handback:`
notch is present) before the next wave goes out.

## 3. Record

Owners write the record as notches on their branch, with the prefixes and `--evidence` rule
`maintaining-fork-pr` defines. You read it, `--repo` because you stand in no fork checkout (`status`
takes the name positionally), `--json` saved to the scratch directory and read there:

```
knives notch <branch> --repo <repo> --json   # the owner's chain, oldest first
knives status <repo> --json | jq '.branches[] | select(.name=="<branch>")'   # the row after the owner's push: tip, claim gone
```

A `handback:` notch followed by a released claim (`knives status` shows none on the branch) is a
finished owner. An owner's final message that disagrees with its notches is wrong; the notch is the
record. Acceptance is dispositions-with-evidence, never PR counts or URLs: a thread is addressed when
a `record:` notch carries one of its three shapes for it (`-> <commit>` | `declined: <reason>` |
`already answered <url>`), with the new reply's url as `--evidence` where a reply was published
(`maintaining-fork-pr` step 6's template).

**Done when** every dispatched branch has a `handback:` (or a stopping `decision:`) notch and a
post-run status row saved beside the pre-run one.

## Mid-wave duties

**The reviewer relay.** An owner has no `task` tool, so its step 6 judgment is yours to dispatch. The
owner sends you the path of its packet, `reviewer-packet-<n>.md` in the scratch directory (questions,
head, workspace path, reviewer rule, recon notch and working list, fork-point diff, drafts, gate log
paths). Dispatch a fresh-context reviewer agent with that packet, unedited, and relay the verdict to
the owner verbatim within 20 minutes of the path arriving (the owner waits 30 from sending, then hands
back). The verdict vocabulary, stated once here: `PASS (no repairs)`, `PASS per fix (k/k)` or `FAIL:
<kind>` with `kind` one of `fix`, `body`, `drift`, and one clause of evidence per question. You add
nothing, soften nothing, never rule yourself; the owner pastes it into its `verify:` notch, which the
census's thermonuclear cell reads. A `decision: reviewer verdict outstanding` notch is yours, not the
maintainer of record's: it names the repair commit ids and says the bookmark was not moved; relay the
verdict and re-dispatch the owner at its step 6 with the head and those commit ids in the coordinates.

**A branch moved under an owner.** The notch `maintaining-fork-pr` "If the branch moves under you"
defines — `decision: branch moved under me, <old tip>→<new tip>; pending: <k> repair commits |
rehome; which head does the PR get` — means an actor other than you rebased a release the branch
belongs to and the owner handed back with a push pending. You decide which head the PR gets: when
`jj -R <fork path> --ignore-working-copy diff --from <old tip> --to <new tip>` (the path from
`knives repos --json`) passes the owner's step 3 test, the new tip — the pending commits already sit
on it, and the re-dispatched owner records a `rehome:` notch and pushes as its step 6 says; anything
else is a decision for the maintainer of record, reported, not guessed. Re-dispatch with the head you
chose in the coordinates.

**Done when** every packet path an owner sent has a verdict relayed verbatim, every timed-out owner
is re-dispatched with it, and every moved-branch `decision:` notch has your ruling on the head and a
re-dispatch (or a line for the maintainer of record).

## 4. Single writer

A branch is touched only under a `knives start` claim, by the agent holding it (the pre-claim
exception is `maintaining-fork-pr` step 1's). You never push to, rebase, or amend a branch an owner
holds. If you must act on a branch yourself — an owner died mid-claim, a hand-back left something
undone — you claim it like any owner, `knives start <branch> --repo <repo> --why "<PR number>: <what
you are finishing>"`, then follow `maintaining-fork-pr` yourself through its step 7.

**Between waves, and only then**, the one move that is yours: a release-member rehome. An owner
whose branch is a parent of the release in hand cannot rebase it alone — `knives release rebase`
moves every member and the release together — so it hands back a `decision:` notch asking for the
rehome (`maintaining-fork-pr` step 3). When one or more owners have, and no parent is claimed:

```
knives release --repo <repo> members --json | jq -r '.members[].held_by[0]'              # before: the members (release in hand only)
knives status <repo> --json | jq '.branches[] | select(.claim != null) | .name'          # before: none of them
knives release --repo <repo> rebase [<target>]
knives notch <release> --repo <repo> -m "rehome: onto <target> for #<n>, #<m>" --evidence <release commit>
jj -R <fork path> --ignore-working-copy log -r '<target> ~ ::<member>' --no-graph   # after, per member: empty = on the target
```

Bare, `rebase` targets the first upstream trunk commit that contains every merged pull request
and needs an explicit target when nothing has merged (`knives release rebase --help`). Run it
once per release per gap. Its exit 0 is no proof that the members moved: it reports "already
contains <target>" from the release commit's ancestry, and members have been seen still on the old
base after it, so confirm per member before re-dispatching; a member for which that log prints
anything is still on the old base, reported as such, never assumed moved. Then re-dispatch those pull
requests in the next wave with their new rows.

**Done when** no branch was written to by two agents in the sweep — every push in the report
matches a notch by the claim holder at that time — and every rehome has a `rehome:` notch on
the release with the new commit as evidence.

## 5. Report

The report is assembled from step 3's notches and rows. Per pull request, in this order:

- **What maintainers asked** — from the `recon:` notch (asks and unresolved threads).
- **What we changed** — one line per `repair:`/`record:` notch, the commit named; a thread is
  addressed when the `record:` notch carries one of its three shapes for it: `-> <commit>`,
  `declined: <reason>`, or `already answered <existing reply url>`.
- **Evidence** — the PR URL, the commits, the `verify:` notch's quoted gate output.
- **What is unverified** — from the `verify:`/`handback:` notches; runs without a conclusion and
  `action_required` runs are unverified, not passing.
- **Forbidden** — the `record:` notch's remaining count, each remaining hit named as intended.
- **Decisions only the maintainer of record can make** — every `decision:` notch, quoted, with its
  evidence, except the two kinds Mid-wave duties already ruled on (`reviewer verdict outstanding`; a
  moved branch whose diff passed the step-3 test), which are reported as ruled, with your ruling.

Never a headline defect count without severity. No pull request is "handled" in prose unless the
notch that says so is cited; one with no notches from this sweep is reported as not swept, with the
reason. **Done when** every line of the report cites a notch, a commit, a URL, or a saved row field,
and the first line is step 1's population count.

## 6. After the sweep: the census

Then answer for CI, review comments, verification and the forbidden scan on every swept pull request,
one line each: `<repo>#<n>: CI <…> / review comments <…> / thermonuclear <…> / e2e <…> / forbidden
<…>`.

- **CI** — the histogram the owner's `verify:` notch quotes from the workflow runs on the pushed head;
  a suite awaiting maintainer approval reads `action_required`, and that is what you write, not
  "green".
- **review comments** — unresolved threads after the push (the `handback:` notch's count, from the
  owner's step-7 read) and how many the owner's `record:` notch answered (any of its three shapes).
- **thermonuclear** — the fresh-context review you dispatched on the owner's packet (Mid-wave
  duties), as its `verify:` notch records the verdict; absent means not run, and you write that.
- **e2e** — end-to-end evidence on the real surface, as the owner's `verify:` notch quotes it: the
  repo's full gate and the test target run in the claimed workspace; a CI run that did not start
  is not this.
- **forbidden** — the `record:` notch's remaining count (`0`, or each hit as intended).

**Done when** every pull request in the population has a census line and every cell names its
notch or row field, including the cells that read `not run`.

## What the orchestrator never does

- Open a pull request or issue the maintainer of record did not ask for.
- Route around a human-tier credential gate; a timed-out grant is a no.
- Ask maintainers for workflow approval.
- Keep the record anywhere but notches and the published PR; scratch lives in the sweep's directory.
- Touch the estate document or the skills mid-sweep; a gap in this text is reported after the sweep.

## Where the detail lives

The commands and their JSON are the `using-knives` skill; what an owner does with one pull
request is `maintaining-fork-pr`; how the estate uses each library, per fork, is the estate
document; what any agent does before touching a fork checkout is `fork-work`.
