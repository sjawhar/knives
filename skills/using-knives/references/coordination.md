# Fetching, claiming and stating work

Read this before `sync`, `preflight`, `start`, `finish`, `track` or `depends`, and when a command waits on a lock. These change local state. `start` puts an existing branch on its tip, with `@` an empty child, and a new branch on the release's shared base (the upstream trunk when no release exists).

## `knives sync [REPO|--all]`

Fetches every remote and tracked PR head, then classifies each tracked PR by what changed since the last sync:

| Transition | Meaning |
|---|---|
| `new` | First sighting, in any forge state: a `new` PR can be merged. Recorded silently. |
| `unchanged` | Nothing changed, also for a PR settled last time. |
| `advanced` | An open PR's head moved. |
| `merged`, `closed` | Settled since the last sync. |
| `reopened` | Recorded as settled, open now. |

- `forge_state` (`open`, `merged`, `closed`) is the current state; the transition is not. It is absent under `--no-github`, which still fetches and checks heads and does not overwrite forge-backed state.
- Only transitions write ledger events.
- `#<n> has comment activity newer than the last sync` is information (exit `0`). It does not see edits to old comments. A failed comment query exits `3`.

## `knives preflight [REPO]`

Reports upstream contribution facts: convention files and their changes since last seen, any stated open-PR cap, branch state. It does not judge; pr-preflight owns the contribution gate.

## Claims: `start` and `finish`

- The claim holder is `KNIVES_OWNER` (the OpenCode plugin sets it), else `CLAUDE_CODE_SESSION_ID`, else `OMP_SESSION_ID`, else `PI_SESSION_ID`. Two agents under one OS user are separate claimants. With none set, it is the owner knives has recorded for the fork (`workspace-derived`), else the claim is anonymous (`<user>/os-user`) and resumes only inside its own workspace.
- `start` refuses (exit `2`) a branch another identity holds. It names the holder, the claim's workspace, the processes this machine sees in it, and what those facts prove; where this machine cannot read the claim's activity (it was taken on another machine, or in a config home that did not record it), it says so and names the claim's machine in their place. `start --force --why "<reason>"` takes the claim over.
- Standing in the claimed workspace resumes a claim under any owner name (on a fork that shares its ledger, only on the machine that took it), but `finish` releases only a claim whose owner name equals yours.
- `start --why "…"` by the holder replaces the claim's reason and keeps its time and place.
- `finish` snapshots the working copy into jj and prints `its working copy is kept as change <id> (commit <id>)`, then forgets the workspace and removes the directory. Files jj never tracks (build output, an ignored `.env`) are lost; `--no-cleanup` keeps the directory.
- When jj cannot snapshot all of it, `finish` releases the claim anyway, keeps the directory and its jj registration as they are, and exits `3`. The next `start` of that branch adopts the directory.
- `finish` refuses (exit `2`) while another process on this machine has its working directory or an open file in the workspace (your own shell, harness and session processes do not count), while another identity holds the claim (except in the case below), and when the claim was taken on another host or agent box. After you check that nothing uses the workspace: `finish --force --why "<reason>"`. A forced release of another owner's claim leaves that workspace as it is.
- When this fork's ledger says another owner took or released your own claim, `finish` says who, when and why, and exits `1`. If someone holds the branch now, it changes nothing on disk and offers `knives start <branch> --force --why "<reason>"`. If nobody does, it offers `knives start <branch>` and disposes of your workspace as any finish does (a refusal or failure there raises the exit to `2` or `3`). Neither offers `finish --force`, which would release the new holder's claim. On a fork that shares its ledger, `finish` reads the ledger's claim instead and removes this machine's record of yours in both cases; where nobody holds the branch, it says to run `knives ledger sweep` then `knives notch <branch>` to read by whom.
- `--superseded-by <branch>` records where the work went.

### Locks

- `start`, `finish`, `track`, `depends` and `sync` hold the fork's lock (`fork-locks/<owner>/<name>.lock` beside `state.json`) while they work. Another of them on the same fork waits up to one minute, with pauses from 20 ms doubling to 2 s, with jitter. A command on another fork does not wait. The `state.json` write takes `state.lock`, also with a one-minute wait. Sighting and hook-session locks wait one second, with pauses from 20 ms to 200 ms.
- Each lock is the OS advisory lock on its file and ends with its process, also on SIGKILL. Never remove a lock file: a held lock belongs to a running process.
- When the wait runs out, the command exits `3`:

  ```text
  another knives command (pid 4242, holding for 73s) is holding <path>; try again in a moment
  ```

  With no readable pid it says `holder unknown, lock written 73s ago`.
- If the claim changed while a command ran (an older knives, or a hand edit), the save writes nothing to `state.json` and exits `3`; the workspace and the ledger event stay. Run `knives status` before you retry.
- A knives older than the fork locks takes only `state.lock`. Do not run it beside this one.

### The immutability rule

`start` writes the fork's `immutable_heads()` into the repository's jj config only when that config states none: trunk on every knives remote, plus tags fetched from upstream. It refreshes a rule it wrote and never overwrites a human one. jj's default would pin superseded releases and other forks' PR heads. Fork release and `keep/*` tags do not make commits immutable.

## State a PR link or dependency

Inference matches branches to PR heads from our copies and reads a `pr-<n>` bookmark as that PR's head. It can miss older or closed PRs and a superseding PR by someone else. State them:

```text
knives track <branch> --pr 4545                # an upstream PR, any state or author
knives track <branch> --forget                 # back to inference
knives depends <branch> --on <repo>#<number>   # <repo>: the registry name; the forge says whose
knives depends <branch> --on <owner>/<name>#<number>
knives depends <branch> --on <member-branch>   # only once every machine on the ledger runs a knives that reads it; see below
knives depends <branch> --forget <requirement> [--on <requirement>]
```

- `depends` is never inferred and can cross forks. Its record names each requirement as one of: `<upstream>#<n>`, a pull request on the fork's upstream; `origin:<owner>/<name>#<n>`, a review pull request on the fork's own repository; `origin:<owner>/<name>:<branch>`, a member branch of this fork.
- **Record no member-branch requirement until every machine that shares the fork's ledger runs a knives that reads it** (the first release with `depends --forget`). An older knives cannot read the entry: on that machine `status` and `notch` exit `3` and every release write on the fork refuses, and that stays until the machine upgrades, because a later forget does not remove the entry. A review-PR requirement makes an older knives' `status` exit `3` too, until it is forgotten.
- `--on <repo>#<n>` with the registry name asks the forge which repository has #n. Both the upstream and the fork's own repository have it: refused (exit `2`), naming both and the spelling to use; name the one you mean by its `<owner>/<name>`. Neither has it: refused (exit `2`). The forge cannot answer: exit `3`, nothing recorded.
- `--on <owner>/<name>#<n>` names the upstream's pull request, or the fork's when `<owner>/<name>` is its own repository (also by the name the forge gives a renamed one). The branch's own review PR is refused. A bare branch name must be a bookmark here or on origin; a base, a release and a fetched `pr-<n>` head are refused.
- `--forget` takes a requirement back, spelled as the record or as `--on` spells it; with `--on`, it corrects one. The removal is a ledger entry: every machine running this knives that shares the ledger reads the same list. It restates the list this machine holds after its pull, so a requirement another machine stated that had not reached it is lost.
- `status` reports an upstream PR that has not merged, and a member branch that has not landed upstream, judged from its row or, when this checkout has no bookmark of it, from `<branch>@origin`. A review PR is met once its head, the member it carries, has landed upstream; the report names the head. A member no copy here holds is a problem: fetch it, or forget the requirement if it landed or was dropped.
- `track --pr <n>` states the upstream PR the branch becomes. On a fork whose upstream and own repository are both forge repositories it asks the forge, and refuses (exit `2`): when the fork's #n has the branch as its head and the upstream's #n does not, naming both; when only the fork has #n; and when neither has it. An upstream #n with another head is accepted (a PR the branch carries). A branch that waits for a review PR says so with `depends --on <owner>/<name>#<n>`. The forge cannot answer: exit `3`, nothing recorded.
- `depends` and `track --forget` pull the shared ledger first and refuse (exit `3`) when the pull fails.
- `track --fork-only` is a legacy statement; placement never records fork-only (fork-work §3).
