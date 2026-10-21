# Fetching and coordinating work

Read before `sync`, `preflight`, `start`, `finish`, `track` or `depends`. These are not read-only reports.

## `knives sync [REPO|--all]`

Fetches every remote and tracked PR head. Then it classifies the change of each tracked PR **since the last sync**:

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

- An existing branch (local, or on `origin` or the publish remote after fetch) continues from its tip. Workspace `@` is an empty child of the tip. When ready, run `jj bookmark set <branch> -r @`, or squash into the branch.
- A name only on upstream is the branch of someone else. Our branch of that name is new.
- A new branch starts on the shared release base, or on fetched upstream trunk when no release exists. It never starts on `@`, because a release merge brings in its other members. Moving the release to newer upstream is `knives release rebase`.
- A divergent bookmark has no single tip. `start` refuses and names the tips.

### Claim identity and locking

- The holder identity is `KNIVES_OWNER` (from the OpenCode plugin), `CLAUDE_CODE_SESSION_ID` or `OMP_SESSION_ID`. Two agents under one OS user are separate claimants. The second `start` refuses with the holder's name.
- With no harness identity, the claim is anonymous (`<user>/os-user`) and resumes only inside its own workspace. Status shows `<id>/<kind>`.
- `start`, `finish`, `track` and `depends` wait up to one minute for one claim-store lock. Pauses start at 20 ms and double to 2 s, with jitter. A paused `start` can be in a queue.
- The OS advisory lock on `state.lock` ends with its process, also on SIGKILL, Ctrl-C or panic. **Never remove the file.** A held lock belongs to a running process.
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

Releases the claim and removes the workspace. It forgets the workspace in jj, removes the directory, and clears the stale Git worktree registration, unlocking it first if it is locked. `start` also clears a stale registration at its own path, but leaves a locked one alone.

- Finish when active work stops, also for a wait such as review. A claim means "working here now".
- The branch, bookmark and PR stay. jj already put tracked files into a commit that its change id reaches. `--no-cleanup` keeps the directory for untracked files (build output, `.env`).
- `--superseded-by <branch>` records where the work went.
- The reason on the claim goes away. Record lasting reasons and evidence in [the ledger](ledger.md) first.

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
