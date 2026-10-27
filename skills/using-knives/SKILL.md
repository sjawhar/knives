---
name: using-knives
description: "Use when running any knives command or reading its output. Also use for fork remotes (upstream, origin, release), branch-to-PR matching, branch dependencies, release plans and cuts, and whether a release carries a change. Also use for the notch ledger and its sharing or migration, JSON output, or OpenCode plugin options. For checks before touching a fork, use fork-work."
---

# The knives CLI

If this skill disagrees with the code, a command's output or its `--help`, follow those and say which line of this skill is wrong.

knives reports fork state and shows who owns what. A finding is a fact to investigate. It is not an instruction to remove a branch, repair a ref, open a PR, or call work merge-ready. For what to do before touching a fork, use **fork-work**. Flags: `knives <command> --help`.

## After an upgrade

Run `knives ledger migrate` once on each machine. Until the migration moves the old data, commands that read the state file or a fork's ledger exit `3` and name it. The hook puts that refusal in the agent's context in place of the claims notice. See [upgrading](references/ledger.md#upgrading-from-an-older-knives).

## Before acting

- **Bind the right repository.** Commands use the checkout you are in. A named repository is found by a scan of home, not taken from the current directory. `repos` always scans. Scan scope, ambiguous checkouts, registry: [configuration](references/configuration.md).
- **Keep remote roles separate.** `upstream` takes our work only through a PR. `origin` is our fork, with ordinary branches and PR heads. The optional `release` remote is where releases are consumed. When it is absent or equal to `origin`, publish to `origin`. A matching name on the wrong remote proves nothing.
- **Registration is not trust.** Trust injects guidance as data and never authorizes fork commands. Repo and owner trust match self-declared remote URLs, not forge authentication. Read [configuration](references/configuration.md) before you grant it.
- **Respect ownership.** A claim names an active worker, not an important branch. Harness session identities tell apart agents under one OS user. Another holder's claim is not yours. Sightings do not prove liveness. Before `start` or `finish`, read [coordination](references/coordination.md). Never remove `state.lock`.
- **Release a claim when active work stops**, also while a PR waits for review. First record in the ledger why the branch exists and any supersession. `finish` removes the workspace, not the branch or PR. To keep files jj never tracked, use `--no-cleanup`. Check this before cleanup.
- **A missing observation is not clearance.** Missing, `null`, zero and `UNKNOWN` differ. Read `problems` and the field definitions: [audit](references/reports.md), [status](references/status.md).
- **Keep dependencies.** `depends <branch> --on <repo>#<number>` says the branch cannot merge before that PR. Never keep a dependent and drop its requirement: the release cannot work. A status report does not authorize discarding either.
- **One branch is both release member and PR head.** It is linear past the shared release base. Never make a second "release-lineage" or sibling copy so that it composes. Moving the release base is an intentional release operation.
- **Change releases explicitly.** Make release names only with `release cut`, never by hand. `--allow-drop` states an intended drop. It is not a way past a refusal. Before any release change, read the pin gates and cut evidence in [releases](references/releases.md). A fork with named release series (one per base, such as 9.3 and 9.2) needs `knives release --series <name>` to act on any series but the default.
- **Before a raw branch rebase**, run fork-work's bidirectional gate (§7). Is the branch a parent of a release/keep merge, or does it descend from one? Bare `release members` sees only the release in hand. Keep published merge ids, because a broad `jj rebase -b` can rewrite them. Read the [rebase rules](references/releases.md) first.

## Commands

Open a reference when you run the command or read its output.

| Question or action | Command | Reference |
|---|---|---|
| Known forks and consumer pins | `repos`, `consumers` | [reports](references/reports.md) |
| Does the live owning remote hold this tip? | `pushed [BRANCH]...` | [reports](references/reports.md) |
| What differs across the estate? | `audit [REPO] [--all]` | [reports](references/reports.md) |
| One PR's state or event history | `pr NUMBER [--timeline]` | [reports](references/reports.md) |
| Each branch's state, owner, latest note | `status [REPO\|--all]` | [status](references/status.md) |
| Fetch and classify changes since last look | `sync [REPO\|--all]` | [coordination](references/coordination.md) |
| Upstream contribution facts | `preflight [REPO]` | [coordination](references/coordination.md) |
| Claim or release work | `start`, `finish` | [coordination](references/coordination.md) |
| State a PR link or prerequisite | `track`, `depends` | [coordination](references/coordination.md) |
| Read or record decisions and evidence | `notch [SUBJECT]` | [ledger](references/ledger.md) |
| Share the ledger, or migrate an old one | `ledger sweep`, `ledger migrate` | [ledger](references/ledger.md) |
| Plan, edit, cut, rebase, reap a release | `release ...` | [releases](references/releases.md) |
| Identity, trust, plugin | `register`, registry, plugin | [configuration](references/configuration.md) |

- These change local state: `sync`, `start`, `finish`, `track`, `depends`, ledger writes, release edits. A ledger write starts a background sweep that pushes it when the ledger is [shared](references/ledger.md#sharing-between-machines).
- These pull a shared ledger first: `status`, `sync`, `audit`, `pushed`, and release commands that read the ledger.
- `audit` and `pushed` never repair, remove, push or open PRs.
- Release commands never push. Publish with `jj git push --remote <publish-remote> --bookmark <name>`.
- `knives hook claude-code|opencode` is harness plumbing. Do not run it.

## Is this content carried?

Use the replay probe, never a source-text search. A symbol's presence does not show that its change is carried.

- `knives release members <target> --carries <revision>` checks that target. Exit `0` is carried, `1` is `NOT carried` or `conflicted`, `3` is unresolved or uncheckable. A superseded target is a valid target.
- With no `<target>`, it checks live releases and upstream trunk, then superseded cuts. Carriage only by a superseded cut does not make deletion safe. `--census` asks this for every maintained branch.
- The `landed` column of `status` is the upstream-trunk probe.
- Bare `members` lists direct parents. `--verify` replays each member. Details: [releases](references/releases.md).

| Verdict | Evidence |
|---|---|
| `carried-exact` | The revision tip is an ancestor of the target. |
| `carried-rewritten` | Replay of its net change leaves the target unchanged. Equivalent content arrived differently, or it has no net change. |
| `carried-rebased` | Replay differs, but a commit with its change id is an ancestor. Weaker than rewritten: divergent changes can name different trees. |
| `NOT carried` | Replay leaves a clean non-empty diff, and no rewrite is in the target. |
| `conflicted` | Replay conflicts, and no rewrite is in the target. Some content can be present, so this needs judgment. |

## Output and exit codes

Reports use TOON when an agent is detected or stdout is not a terminal. `--json` forces JSON and `--text` forces prose. TOON and JSON carry the same structure. Read the fields, not the prose for people.

| Exit | Meaning |
|---|---|
| `0` | No findings. Not a readiness verdict. |
| `1` | Findings. |
| `2` | Usage error. |
| `3` | Incomplete: something could not be answered. |

An incomplete report can still hold useful observations. Audit branch rows do not change the exit, but findings and problems do. A `null` thread count is not zero, even at exit `0`.
