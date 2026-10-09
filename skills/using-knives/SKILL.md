---
name: using-knives
description: "Reference manual for the knives CLI, which reports and coordinates state across several forks of upstream repositories worked by several agents. Use when running any knives command, when interpreting what one printed, or when you need the detail behind it: what the upstream, origin and release remotes mean, how a branch is matched to a pull request and how to state one it cannot find, recording that one branch cannot land before another, planning and cutting releases, JSON output, and the OpenCode plugin's options. For the shorter question of what to do before touching a fork at all, use the fork-work skill."
---

# The knives CLI

knives reports fork state and makes concurrent ownership visible. **It reports;
it does not advise.** A finding is a fact to investigate, not an instruction to
delete a branch, repair a ref, open a pull request, or declare work merge-ready.

For what to do before touching a fork, use **fork-work**. This manual explains
commands and their evidence; it does not replace that workflow.

## Before acting

- **Bind the right repository.** Commands normally use the checkout you are in.
  A named repository is located by scanning home, not by assuming the current
  directory is its checkout. `repos` always scans. For scan scope, ambiguous
  checkouts and registry setup, read [configuration](references/configuration.md).
- **Keep remote roles separate.** `upstream` is what we contribute to, only
  through a pull request. `origin` is our fork, where ordinary branches and PR
  heads live. The optional `release` remote is where releases are consumed;
  publishing uses `origin` when `release` is absent or equals `origin`.
  A matching name on the wrong remote proves nothing about publication.
- **Registration is not trust.** Trust rules inject guidance as data, never
  authorize fork commands. Repo/owner trust matches self-declared remote URLs,
  not forge authentication; read [configuration](references/configuration.md)
  before granting it.
- **Respect ownership.** A claim names an active worker, not a branch's importance.
  Harness session identities distinguish agents under one OS user. Another
  holder's claim is not yours; bounded sightings are not a liveness guarantee.
  Before `start` or `finish`, read [coordination](references/coordination.md).
  Never remove `state.lock` to bypass a waiting writer.
- **Release active claims when work stops**, including while a PR waits for
  review. `finish` removes the workspace, not the branch or PR. Preserve files
  jj never tracked with `--no-cleanup`; inspect that consequence before cleanup.
  Record why the branch exists and any supersession in the ledger first.
- **Do not turn missing observations into clearance.** Read `problems` and the
  command's field definitions before interpreting a missing field, `null`, zero
  or `UNKNOWN`. They do not mean the same thing. See the
  [audit fields](references/reports.md) or [status fields](references/status.md)
  when consuming those reports.
- **Preserve dependencies.** `depends <branch> --on <repo>#<number>` records that
  the branch cannot merge before that PR. Dropping a required change while
  retaining its dependent can ship a release that cannot work. A status report
  is not authorization to discard either.
- **Keep one branch for the member and PR head.** It is linear after the shared
  release base. Do not create a second release-lineage or sibling copy to make
  it compose. Moving the release base is an intentional release operation.
- **Make release changes explicitly.** Create release names through `release cut`,
  never hand-composed bookmarks. `--allow-drop` states an intentional content
  drop; it is not a generic way past a refusal. Read the pin gates and cut
  evidence in [releases](references/releases.md) before any release mutation.
- **Before a raw branch rebase, use fork-work's bidirectional ancestry check:**
  does the branch parent a release/keep merge, or descend from one? Bare
  `release members` sees only the release in hand, not retained older cuts.
  Preserve published merge identities; a broad `jj rebase -b` can rewrite them.
  Read the [release model and rebase rules](references/releases.md) first.

## Choose the command and open its reference

Use these references when running the command or interpreting its output, not
as an unconditional reading list.

| Question or action | Command | Required detail |
|---|---|---|
| What forks and consumer pins are known here? | `repos`, `consumers` | [Reports](references/reports.md) |
| Does the live owning remote hold this tip? | `pushed [BRANCH]...` | [Reports](references/reports.md) |
| What differs across the estate? | `audit [REPO] [--all]` | [Audit rows and unknowns](references/reports.md) |
| What is one PR's state or event history? | `pr NUMBER [--timeline]` | [Reports](references/reports.md) |
| What is each branch's state, owner and latest note? | `status [REPO\|--all]` | [Status](references/status.md) |
| Fetch and classify changes since last observation | `sync [REPO\|--all]` | [Coordination](references/coordination.md) |
| Check upstream contribution facts | `preflight [REPO]` | [Coordination](references/coordination.md) |
| Claim or release active work | `start <branch>`, `finish <branch>` | [Coordination](references/coordination.md) |
| State PR association or a prerequisite | `track`, `depends` | [Coordination](references/coordination.md) |
| Read or record decisions and evidence | `notch [SUBJECT]` | [Ledger](references/ledger.md) |
| Plan, edit, cut, rebase or reap a release | `release` and its subcommands | [Releases](references/releases.md) |
| Configure identity, trust or the plugin | `register`, registry/plugin settings | [Configuration](references/configuration.md) |

`sync` fetches and records state; it is not read-only. `start`, `finish`,
`track`, `depends`, ledger writes and release edits also mutate local state,
and a ledger write hands its entry to a background sweep that pushes it when
the ledger is [shared](references/ledger.md#sharing-between-machines).
`audit` and `pushed` report without repairing, deleting, pushing or opening PRs.
Release planning is the default; release commands never push a release. Publication is a
separate, intentional `jj git push --remote <publish-remote> --bookmark <name>`.

`knives hook claude-code` and `knives hook opencode` are harness plumbing, not
commands for people to run.

## Is this content carried?

For “is branch/fix X in release/trunk Y?”, **use the replay probe, never a
source-text search**. A symbol's presence is not evidence that its fix is carried.

- `knives release members <target> --carries <revision>` checks that exact target.
  Exit `0`: carried; `1`: `NOT carried` or `conflicted`; `3`: unresolved or
  uncheckable. A superseded target is still a valid explicit target.
- Without `<target>`, `--carries <revision>` checks live releases and upstream
  trunk before superseded cuts. Superseded-only carriage does not establish
  deletion safety. `--census` asks that question for every maintained branch.
- `knives status`'s `landed` column answers the upstream-trunk probe.
- Bare `members` reads direct parents of the release in hand. `--verify` replays
  each member into it. For the option combinations, retained cuts and membership
  evidence, read [releases](references/releases.md).

The verdict vocabulary is exact:

| Verdict | Evidence |
|---|---|
| `carried-exact` | The revision tip is an ancestor of the target: that exact commit is present. |
| `carried-rewritten` | Replaying the revision's net tree change leaves the target unchanged; equivalent content arrived differently, or the revision has no net change. |
| `carried-rebased` | Replay is not cleanly equivalent, but a different commit with the revision's change id is an ancestor. The author-rebased change is carried; this is weaker than `carried-rewritten` because divergent changes can name different trees. |
| `NOT carried` | Replay leaves a clean non-empty diff and no rewrite is in the target. |
| `conflicted` | Replay conflicts and no rewrite is in the target. Some content may be present; this requires judgment. |

## Output and exit codes

Reports use TOON when an agent is detected or stdout is not a terminal.
`--json` forces JSON exactly; `--text` forces prose. TOON and JSON carry the same
structure. Read the command's reference for omitted and nullable fields, not
prose intended for display.

| Exit | Meaning |
|---|---|
| `0` | Completed without findings; not a readiness verdict. |
| `1` | Findings. |
| `2` | Usage error. |
| `3` | Incomplete: something could not be answered. |

An incomplete report can still contain useful observations. Branch fact rows
alone do not change audit's exit; findings and problems do. A `null` thread
count, for example, is not a zero count even if the command returned `0`.
The [reports](references/reports.md), [ledger](references/ledger.md) and
[releases](references/releases.md) references describe command-specific results.
