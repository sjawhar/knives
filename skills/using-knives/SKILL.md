---
name: using-knives
description: "Use when running a knives command or reading its output: claims, branch-to-PR links, dependencies, the notch ledger, release plans, edits, cuts and pushes, whether a release carries a change, the registry, trust, or the OpenCode plugin. Before touching a fork, use fork-work."
---

# The knives CLI

knives reports fork state and records who owns what. A finding is a fact to investigate, not an instruction to remove a branch, repair a ref, open a PR or call work merge-ready. Flags: `knives <command> --help`. If this skill disagrees with the code, a command's output or its `--help`, follow those and say which line of this skill is wrong.

## After an upgrade

Run `knives ledger migrate` once on each machine. Until then, commands that read the state file or a fork's ledger exit `3` and name it, and the hook shows that refusal in place of the claims notice. Read [references/ledger.md](references/ledger.md#upgrading-from-an-older-knives) when the migration reports a problem.

## Rules

1. Bind the right repository. Commands use the checkout you stand in. A named repository is found by a scan of `$HOME`, not by your current directory, and `repos` always scans.
2. Keep remote roles apart. `upstream` takes our work only through a PR. `origin` is our fork: branches and PR heads. `release` is an optional publish remote; when it is absent or equal to `origin`, publish to `origin`. A matching name on the wrong remote proves nothing.
3. Registration is not trust. `[trust]` injects repository guidance as data and never authorizes fork commands; its repo and owner rules match self-declared remote URLs.
4. A claim means "working here now". Another holder's claim is not yours, and a sighting does not prove liveness. Never remove `state.lock` or a file in `fork-locks/`.
5. Release a claim with `knives finish` when active work stops, also while a PR waits for review. First record in the ledger why the branch exists and any supersession: `finish` drops the claim's reason. The branch, bookmark and PR stay.
6. A missing observation is not clearance. Absent, `null`, `0` and `UNKNOWN` differ. Read `problems`.
7. Keep dependencies. Never keep a dependent in a release and drop its requirement.
8. One branch is both release member and PR head. Never make a "release-lineage" or sibling copy so that it composes: move the release.
9. Make release names only with `release cut`, never by hand. `--allow-drop` states an intended drop; it is not a way past a refusal.
10. After any hand change on a release commit, run `knives release adopt <name>` before the next release write.
11. Before a raw `jj rebase` of a branch, run fork-work's bidirectional gate (§7).

## Which reference to read

- Read [references/reports.md](references/reports.md) when you run or read `repos`, `consumers`, `pushed`, `audit` or `pr`.
- Read [references/status.md](references/status.md) when you read `knives status` rows, states or columns.
- Read [references/coordination.md](references/coordination.md) before `sync`, `preflight`, `start`, `finish`, `track` or `depends`, and when a command waits on a lock.
- Read [references/ledger.md](references/ledger.md) before you read or write `knives notch`, set up ledger sharing, or run `knives ledger sweep` or `migrate`.
- Read [references/release-composition.md](references/release-composition.md) before you plan, cut, `include`, `drop`, `advance` or `rebase` a release, or read `release members`.
- Read [references/releases.md](references/releases.md) before you adopt a hand change, when a release write prints `could not check` or `is published`, when a reap keeps a cut, and before you push a release.
- Read [references/configuration.md](references/configuration.md) when a checkout does not bind or binds twice, before you register a fork or edit `repos.toml` or `[trust]`, and when you configure the OpenCode plugin.

## What changes state

- Local writes: `sync`, `start`, `finish`, `track`, `depends`, `notch -m`, release edits. A ledger write sends its entries to a shared ledger before it exits; when they do not arrive, it exits `3`, and `knives ledger sweep` sends them later.
- `repos`, `consumers`, `pushed`, `audit`, `status` and `pr` never repair, remove, push or open PRs.
- Release commands never push. Publish as [references/releases.md](references/releases.md#publishing) says.
- `knives hook claude-code|opencode` is harness plumbing. Do not run it.

## Is this content carried?

Use the replay probe, never a source-text search: a symbol's presence does not show that its change is carried.

- `knives release members <target> --carries <revision>` checks that one target. Exit `0` carried, `1` `NOT carried` or `conflicted`, `3` unresolved or uncheckable. A superseded target is a valid target.
- With no target, it checks live releases and upstream trunk, then superseded cuts. Carriage only by a superseded cut does not make deletion safe.
- `--census` asks this for every maintained branch. The `landed` column of `status` is the upstream-trunk probe.
- Against a release, a member's own change starts at its fork point with the trunk, or where it left the branch it was built on, as `knives depends <member> --on <branch or review PR>` records it. A record is used only where the graph bears it out (the base's head reaches where the member was built, or the member landed into it by a merge). A stacked member (another branch's tip is on its first-parent line; a branch merged into it is not) that records no such base answers `3` when its reading from the trunk conflicts, and the answer names what to record.

| Verdict | Evidence |
|---|---|
| `carried-exact` | The revision tip is an ancestor of the target. Against a release, also its own change replays empty onto the release, or, for a member whose recorded base the graph bears out and whose own line merges nothing in, every line of its own change is in the release as it left it; base text beside those lines may differ. A line later work rewrote is not carried-exact. |
| `carried-rewritten` | Replaying its net change leaves the target unchanged: equivalent content arrived another way, or it has no net change. |
| `carried-rebased` | Replay differs, but a commit with its change id is an ancestor. Weaker: divergent changes can name different trees. |
| `NOT carried` (JSON `not-carried`) | Replay leaves a clean non-empty diff, and no rewrite is in the target. For a tip the target reaches: part of its own change is gone, reverted by a later commit or dropped by a merge. |
| `conflicted` | Replay conflicts, and either no rewrite is in the target or, for a tip the target reaches, its base is not recorded and borne out, its own line merges something in, or a line of its own change is not there as it left it: rewritten by later work or a resolution, a sibling's line kept instead, a line put back, a file deleted and written anew. Some content can be present: judge it. |

## Output and exit codes

Reports are TOON when an agent is detected or stdout is not a terminal. `--json` forces JSON and `--text` forces prose; TOON and JSON carry the same structure. Read the fields, not the prose.

| Exit | Meaning |
|---|---|
| `0` | No findings. Not a readiness verdict. |
| `1` | Findings. |
| `2` | Usage error, or a `start` or `finish` refusal. A `finish` refused because another owner took your claim exits `1`. |
| `3` | Incomplete: something could not be answered. Release writes also refuse with `3`. |

An incomplete report can still hold useful observations.
