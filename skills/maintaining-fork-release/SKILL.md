---
name: maintaining-fork-release
description: Use when asked to maintain a set of fork pull requests, address upstream debt, or verify and ship the forks' releases and consumer pins. For one pull request alone, use maintaining-fork-pr. For command semantics, use using-knives.
---

# Maintaining fork pull requests and releases

If this skill disagrees with the code, a command's output or its `--help`, follow those and say which line of this skill is wrong.

You own the requested outcome, not only the dispatched PRs.

- A PR-only request ends at verified PR maintenance: no release or deployment change. Upstream debt or good releases also need step 5.
- Obey explicit exclusions, for example "no deployment".
- `fork-work` governs workspace entry, `using-knives` command semantics, `using-jj` version control.

## 1. Read `knives notch` first

- The notches are the record of this work. There is no separate inventory document; do not make one.
- Map the requested repositories with `knives repos` and `~/.config/knives/repos.toml`. The upstream slug is the entry's `upstream` URL without `.git`, not the registry name.
- Read in this order, with names from the output before:

  ```sh
  knives repos
  knives status <repo>
  knives notch --repo <repo>
  knives release --repo <repo> members
  knives notch <release> --repo <repo>
  knives notch <branch> --repo <repo>
  knives notch --pr <number> --repo <repo>
  knives notch --verify <subject> --repo <repo>
  ```

- Bare `notch` shows only recent context: read each relevant chain in full. `--pr` also finds a PR's former branches.
- Unprefixed notes are valid context. A later note can supersede one. A recorded requirement still holds when a later release lost its implementation.
- Read output as printed, without `--json`, whenever you read it yourself. Use `--json` only when executed code filters, counts or joins it. Page a truncated capture in full.
- Read upstream contribution files and workflows for the gates, and project guidance for consumer wiring.

**Done when:** you read each repository's relevant chains and separate inherited obligations from stale assertions.

## 2. Account for release members and open PRs

- The population is the union of release members and open PR branches. Count an overlapping branch once, with both identities. Match PRs by upstream slug and number, release-only members by parent commit.
- When release parent, branch tip and PR head differ, keep all three in the row: a composition gap, not a duplicate.
- Use `knives status`, `knives release members`, `knives audit` and `knives consumers` for each repo. Reconcile the audit's open-head discrepancies before you claim full coverage.
- A merged or closed PR does not prove that a release no longer needs its delta.
- Exit `3`, `problems`, or a missing checkout, forge, record or page is not zero work: keep counts provisional and investigate.
- List repositories outside the requested set once. Do not dispatch them.
- Keep scratch in `~/.cache/knives-sweep/<unique-run>/<repo>/pr-<number>/` and `.../<repo>/release/`, never in a workspace or `/tmp`. Save outputs, diffs, drafts and gate logs there unchanged, and name the paths.
- The report starts: `Population: <p> open PRs and <r> release-only members, <u> distinct work items across <n> requested forks; coverage complete|incomplete (<timestamp>)`. Name every incomplete source.

**Done when:** every union member is accounted for and each count cites saved output.

## 3. Dispatch one owner per PR

- Give each PR one `maintaining-fork-pr` owner, at most eight active. See each start's claim or refusal before the next batch. The PR stays yours until merge or reassignment.
- Run owners and their reviewers on a strong model, not a fast tier, which works the steps as a checklist.
- A claim held outside this sweep: report its `why` and continue. A quiet `seen` does not prove the holder died. Never force a claim or restore the jj operation log.
- Read [references/owners.md](references/owners.md) before you dispatch an owner: what each dispatch holds.

Release-only members are yours, with the same claim, evidence and independent review as an owner's PR:

- Claim a named branch before anyone edits it. An anonymous member (a release parent no branch names) gets no new branch.
- Before any drop, read [references/drop-sweep.md](references/drop-sweep.md) and follow it.
- Classify with evidence (supplied upstream, proposed upstream, downstream configuration, obsolete) in `knives notch <branch> -m ... --evidence ...`, or on the release for an anonymous member.
- Invent no PRs. Do not include an open PR in a release automatically.

**Done when:** each open PR has an owner or a named blocker, and each release-only item an owner or a disposition.

## 4. Review, record and coordinate integration

- When an owner sends a packet, a handback or a `decision:`, read [references/owners.md](references/owners.md): the relay checks, accepted dispositions, and which questions reach the human.
- The claim holder writes the branch. A lone branch's rebase follows `fork-work`. A release-member rebase is yours.
- Before a release-wide write (a release rebase, a `rehome needed` handback, a hand resolution on the release commit), read [references/release-wide-writes.md](references/release-wide-writes.md): the claim barrier, the per-member checks and the `adopt` record.
- A stacked series (`stacked = true`): read `using-knives`' stacked-series reference (`references/stacked-series.md`).
- Record a release composed outside knives with `knives release cut <release> --at <commit>`: `using-knives`' hand-cut reference (`references/hand-cut.md`).

**Done when:** each packet has a verdict, each handback agrees with notches and current state, and each integration change has preserved-content evidence.

## 5. Finish release and consumer verification

Do this step when the request includes releases, upstream debt or consumption.

- Classify each member in the release notch before you compose it:
  - `upstream-PR <url>`: an open or merged upstream PR carries it.
  - `upstream-pending, placement <notch anchor or commit>`: its `placement:` notch and commit body record the UPSTREAM verdict, the defect or gap, and the reproduction or red→green test its upstream PR opens from.
  - `neither`: drop it or return it to its author. Never compose it.
- A classification is disclosure, not a gate: an `upstream-pending` member ships when the release is gated and published.
- Read [references/release-and-consumers.md](references/release-and-consumers.md) whenever you do this step, and do its five stages in order.
- An external prerequisite blocks publication or consumption: record the exact blocker and report the stage **blocked**.
- Decide membership, naming, rehome and compatible repairs yourself. Ask the human only for missing authority, a change of direction or a conflict with stated intent.

**Done when:** intended content is verified in the release and at the requested consumer or runtime boundary, or that boundary is reported blocked with evidence.

## 6. Report verified outcomes

- Build the report from saved observations and notches. Refresh the population for PRs that moved or merged; never lose a row or an original item's disposition.
- Statuses stay separate: **accounted for**, **verified**, **published**, **consumed**, **blocked**. A handback proves only accounting. A skipped gate or a unit suite is not end-to-end verification.
- Each cell cites a notch or saved observation:

  `<repo>#<n>: CI <head-specific run conclusions/pending> / review comments <unresolved, dispositions> / review <fresh-context verdict> / e2e <surface evidence or not run> / forbidden <remaining hits>`

  `<repo>/<release>: composition <verified|blocked> / published <revision|not published> / consumers <resolved revisions|blocked> / runtime <evidence|not run>`

- A release-only member row gives its owner or disposition, carriage and verification, with no claimed PR review.
- Pending CI stays yours after handback: watch it with a background watcher and repair failures. Approval-gated `action_required` stays **unrun**: reproduce the job locally. Do not ask maintainers to approve workflows.
- Do not open unsolicited PRs or issues, go around credential gates, or change shared configuration to make a gate green.
