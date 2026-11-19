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

- The claim holder is `KNIVES_OWNER` (the OpenCode plugin sets it), else `CLAUDE_CODE_SESSION_ID`, else `OMP_SESSION_ID`, else `PI_SESSION_ID` (set by an Oh My Pi build that sets no `OMP_SESSION_ID`). Two agents under one OS user are separate claimants. With none set, the claim is anonymous (`<user>/os-user`) and resumes only inside its own workspace.
- On a fork whose registry entry sets `ledger`, the claim that decides is a ref on the ledger's repository: it holds on every machine that shares the ledger, and a refusal names the holder's machine. Read [shared-claims.md](shared-claims.md) before `start` or `finish` there. When `start` or `finish` cannot read or change that claim, it exits `3`, and `state.json` and the workspace stay as they were.
- `start` refuses (exit `2`) a branch another identity holds. It names the holder, the claim's workspace, the processes this machine sees in it, and what those facts prove. `start --force --why "<reason>"` takes the claim over.
- Standing in the claimed workspace resumes a claim under any owner name, but `finish` releases only a claim whose owner name equals yours. An anonymous claim (`<user>/os-user`) is finished only from inside its own workspace: outside it, two anonymous owners never match, even under one user name.
- `start --why "…"` by the holder replaces the claim's reason and keeps its time and place.
- `finish` snapshots the working copy into jj and prints `its working copy is kept as change <id> (commit <id>)`, then forgets the workspace and removes the directory. Files jj never tracks (build output, an ignored `.env`) are lost; `--no-cleanup` keeps the directory.
- When jj cannot snapshot all of it, `finish` releases the claim anyway, keeps the directory and its jj registration as they are, and exits `3`. The next `start` of that branch adopts the directory.
- `finish` refuses (exit `2`) while another process on this machine has its working directory or an open file in the workspace (your own shell, harness and session processes do not count), while another identity holds the claim (except in the case below), and when the claim was taken on another host or agent box. After you check that nothing uses the workspace: `finish --force --why "<reason>"`. A forced release of another owner's claim leaves that workspace as it is.
- When this fork's ledger says another owner took or released your own claim, `finish` says who, when and why, and exits `1`. If someone holds the branch now, it changes nothing on disk and offers `knives start <branch> --force --why "<reason>"`. If nobody does, it offers `knives start <branch>` and disposes of your workspace as any finish does (a refusal or failure there raises the exit to `2` or `3`). Neither offers `finish --force`, which would release the new holder's claim.
- `--superseded-by <branch>` records where the work went.
- `--why "<reason>"` is recorded on every `finish`, with `--force` or without: as the reason of the release, or, where no claim was held, in the entry's text.

### Locks

- `start`, `finish`, `track`, `depends` and `sync` hold the fork's lock (`fork-locks/<owner>/<name>.lock` beside `state.json`) while they work. Another of them on the same fork waits up to one minute, with pauses from 20 ms doubling to 2 s, with jitter. A command on another fork does not wait. The `state.json` write takes `state.lock`, also with a one-minute wait. Sighting and hook-session locks wait one second, with pauses from 20 ms to 200 ms.
- On a fork that shares its ledger, a claim change also takes `ledger.lock`, then the ledger git directory's `knives-transport.lock`, each with a one-minute wait. A write's send waits as long for `ledger.lock`.
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
knives track <branch> --pr 4545     # any number, state or author
knives track <branch> --forget      # back to inference
knives depends <branch> --on <repo>#<number>
```

- `depends` is never inferred and can cross forks. `status` reports requirements that have not merged.
- `depends` and `track --forget` pull the shared ledger first and refuse (exit `3`) when the pull fails.
- `track --fork-only` is a legacy statement; placement never records fork-only (fork-work §3).
