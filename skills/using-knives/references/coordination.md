# Fetching and coordinating work

Read before `sync`, `preflight`, `start`, `finish`, `track` or `depends`. These are not read-only reports.

## `knives sync [REPO|--all]`

Fetches every remote and tracked PR head. When another fetch of the same clone wins a ref update, each fetch is retried up to four more times after short jittered waits (under 1.5 s in all). git reports every ref-lock failure the same way, so one that is no race (a lock file a crashed git left behind, a ref directory it may not write) also costs those retries before its error returns. Then it classifies the change of each tracked PR **since the last sync**:

| Transition | Meaning |
|---|---|
| `new` | First sighting, in any forge state. Recorded silently. |
| `unchanged` | Nothing changed, also for a PR settled last time. |
| `advanced` | The head of an open PR moved. |
| `merged`, `closed` | Settled since the last sync. |
| `reopened` | Recorded as settled, open now. |

- Each row also has `forge_state` (`open`, `merged`, `closed`). It is absent under `--no-github` (text `unknown`). A transition is not the current state: `new` can be a merged PR. Forge state wins over head movement.
- Only transitions write ledger events. Two are silent: first sightings, and a transition the ledger already holds as that PR's newest, which another machine recorded ([sharing](ledger.md#sharing-between-machines)).
- A run without the forge does not overwrite forge-backed state. `--no-github` still fetches and checks heads.
- Bare `sync` uses the checkout you stand in. Outside one, it asks for a name or `--all`. `--all` gives one array.
- Forge states are stored as `pull_states`, keyed `<owner>/<name>#<number>` by upstream name. Legacy spellings (`new`, `advanced`, `unchanged`) read as open. A forge-backed sync replaces them and reports it once. An unknown spelling is a problem, exit `3`. The row is still recorded and the bad record removed, so the problem does not recur.

### Comment activity

For an open tracked PR with comments newer than the last sync mark, sync prints `#<n> has comment activity newer than the last sync`. This is information, exit `0`. A failed comment query is a problem, exit `3`.

- It costs one forge call per open tracked PR.
- `comment_marks` (same keys) only moves forward. The first sighting records a silent mark.
- Edits to old comments are invisible, because `createdAt` does not change.

## `knives preflight [REPO]`

Reports upstream contribution facts: convention files, changes to them since last seen, any stated open-PR cap, and branch state. It does not judge. **pr-preflight** owns the contribution gate. **maintaining-fork-pr** and **maintaining-fork-release** run it.

## `knives start <branch>`

Claims the branch and opens a jj workspace.

- An existing branch (local, or on `origin` or the publish remote after fetch) continues from its tip. A fetch that loses a ref-update race to another fetch of the same clone is retried up to four more times. Workspace `@` is an empty child of the tip. When ready, run `jj bookmark set <branch> -r @`, or squash into the branch.
- A name only on upstream is the branch of someone else. Our branch of that name is new.
- A new branch starts on the shared release base, or on fetched upstream trunk when no release exists. It never starts on `@`, because a release merge brings in its other members. Moving the release to newer upstream is `knives release rebase`.
- A divergent bookmark has no single tip. `start` refuses and names the tips.

### Claim identity and locking

- The holder identity is `KNIVES_OWNER` (from the OpenCode plugin), `CLAUDE_CODE_SESSION_ID` or `OMP_SESSION_ID`. Two agents under one OS user are separate claimants. The second `start` refuses with the holder's name.
- With no harness identity, the claim is anonymous (`<user>/os-user`) and resumes only inside its own workspace. Status shows `<id>/<kind>`.
- A `start` refused because another identity holds the claim also names the claim's workspace, every process this machine can see inside it with its process id and command, and whether the claim was taken in this place, in another place, or in a place it does not record. It says what each of those facts proves, and that the list leaves out this run's own processes, the processes of its harness session, and any process this user cannot inspect. No other surface scans the workspace: the `finish` refusal, the notice after a seizure, the notice after a resume, the hook's claim rows and `knives status` print what they printed before.
- Standing in the claimed workspace resumes a claim whatever name holds it. It releases the claim only where the claim's owner name and yours are the same string. Where they differ, `finish` gives the claim refusal.
- `knives start <branch> --why "…"` by the claim's holder replaces the reason on the claim, from inside the workspace or outside it. The claim keeps the time and the place it was taken. A `start` with no `--why` keeps the reason. A resume by possession under another owner name writes nothing to the claim.
- `start`, `finish`, `track`, `depends` and `sync` hold the lock of the fork they act on (`fork-locks/<upstream>.lock` beside `state.json`) while they work. Another of these commands on the same fork waits up to one minute for it. Pauses start at 20 ms and double to 2 s, with jitter. A paused `start` can be in a queue. A command on another fork does not wait.
- The write to `state.json` takes `state.lock` only for that write. It also waits up to one minute.
- The OS advisory lock on a lock file ends with its process, also on SIGKILL, Ctrl-C or panic. **Never remove a lock file.** A held lock belongs to a running process.
- If a claim changed while a command ran (an older knives, or a hand edit), the save writes nothing to `state.json` and the command exits `3`. The workspace and the ledger event that the command made before the save stay. Run `knives status` before you run the command again.
- A knives older than the fork locks takes only `state.lock`. Do not run it at the same time as this one.
- When the one-minute wait runs out, the command exits `3` and names the holder:

```text
another knives command (pid 4242, holding for 73s) is holding <path>;
try again in a moment
```

With no readable pid, it says `holder unknown, lock written 73s ago`. The sighting and hook-session locks wait one second, with pauses from 20 ms to 200 ms.

### Repository immutability rule

`start` writes the fork's `immutable_heads()` only when the repository config states none. The rule pins trunk on every knives remote and `remote_tags(remote=exact:"upstream")`. It reports the file, the rule, and any user-level rule that it hides.

- The jj default `untracked_remote_bookmarks()` can pin superseded releases or the PR heads of other forks. Its `tags()` can pin every member under a release tag.
- Fork-release and `keep/*` tags do not make commits immutable. A rewrite never moves a tag. The tagged original keeps its id locally and remotely.
- The rule uses the jj table form, with a `doc` that names knives. When the registry entry changes, a later `start` refreshes the knives rule (`refreshed in`) and leaves a human rule alone. Status reports both kinds of mismatch.

## `knives finish <branch>`

Releases the claim and removes the workspace. It forgets the workspace in jj, removes the directory, and clears the stale Git worktree registration, unlocking it first if it is locked. `start` also clears a stale registration at its own path, but refuses on a locked one, which may be another machine's live workspace in a shared store; when it is this branch's, `knives finish <branch>` clears it. `start --force --why` takes the workspace over: it forgets the stale jj workspace and clears the registration, locked or not, before rebuilding.

- Finish when active work stops, also for a wait such as review. A claim means "working here now".
- The branch, bookmark and PR stay. Before it forgets the workspace, `finish` snapshots the working copy into jj and prints `its working copy is kept as change <id> (commit <id>)`. When jj cannot snapshot all of it (a stale working copy, a new file over `snapshot.max-new-file-size`, a file name that is not UTF-8), `finish` releases the claim all the same, keeps the directory and its jj registration exactly as they are, says what jj refused and the recovery jj named, and exits `3`. What jj could snapshot stays in its commit, and the next `knives start <branch>` adopts that directory and leaves what is in it alone. A workspace whose working-copy state jj cannot read also exits `3` and releases nothing, with `--force` and without it, because `knives start` refuses to adopt such a directory. `--no-cleanup` keeps the directory for files jj ignores (build output, `.env`).
- `finish` refuses (exit `2`) while another process on this machine has its working directory or an open file in the workspace, and names each one by pid and command. Your own shell, your harness and the processes your session started do not count; a duplicate harness of the same session does.
- `finish` also refuses when the claim was taken on another host or agent box: from here it cannot see the processes there. Make sure nothing there uses the workspace, then add `--force --why "<reason>"`. `--force` also overrides the live-process refusal; the ledger records the reason.
- `finish` refuses (exit `2`) when another identity holds the claim, names the holder and its last sighting, and offers `knives finish <branch> --force --why "<reason>"`. That is how the claim of an agent that is gone is released. A forced release over a claim another owner name held leaves that workspace exactly as it is: it removes no directory and forgets no jj workspace.
- A caller whose own claim this fork's ledger says another owner took or released is told that instead: who did it, when, why, and who holds the branch now. Where another identity holds the claim now, `finish` changes nothing on disk and offers `knives start <branch> --force --why "<reason>"`. Where nobody holds it, `finish` offers `knives start <branch>` and disposes of the caller's own workspace as any finish does. Both exit `1`. Neither offers `finish --force`, which would release the live claim of whoever holds the branch now.
- `finish` finds that record only where your owner name is the same after the takeover. A session that ended and came back under a new name matches no record, and gets the refusal above.
- `--superseded-by <branch>` records where the work went.
- A finish takes the reason with the claim. While the claim lives, a later `knives start --why` by its holder replaces that reason. Record lasting reasons and evidence in [the ledger](ledger.md) first.

## State a PR link or dependency

Inference matches branches to PR heads from our repository copies, and reads a `pr-<n>` bookmark as that PR head. It can miss older PRs, closed PRs, or a superseding PR by someone else. State those:

```text
knives track <branch> --pr 4545    # any number, state or author
knives track <branch> --fork-only # legacy statement: placement never records fork-only (fork-work §3)
knives track <branch> --forget    # back to inference
knives depends <branch> --on <repo>#<number>
```

- `depends` is explicit, can cross forks, and is never inferred from names. Status reports dependencies that have not merged. When you edit a release, keep the requirement of each dependent.
- `depends` and `track --forget` pull the shared ledger first. When the pull fails, they refuse with exit `3`. A forget removes the statement of the branch, and `depends` records its list from the last pull of this machine ([sharing](ledger.md#sharing-between-machines)).
