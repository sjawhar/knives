# Fetching and coordinating work

Read this before `sync`, `preflight`, `start`, `finish`, `track` or `depends`.
These verbs are not interchangeable with read-only reports.

## `knives sync [REPO|--all]`

Fetches every remote and tracked PR head, then classifies each tracked PR's
transition **since the last sync**:

| Transition | Meaning |
|---|---|
| `new` | First sighting, whatever its forge state; recorded silently. |
| `unchanged` | Nothing changed, including a PR already settled last time. |
| `advanced` | Open PR's head moved. |
| `merged`, `closed` | Settled since the last sync. |
| `reopened` | Previously recorded settled; open now. |

Each row separately carries `forge_state`: `open`, `merged` or `closed`.
It is absent under `--no-github` (text: `unknown`). A transition is not the
current state: `new` can describe a merged PR. Forge state wins over head
movement, so a PR that merged and moved head is `merged`.

Only transitions write ledger events. First sightings are silent, and so is a
transition the fork's ledger already holds as that PR's newest, which another
machine recorded ([ledger](ledger.md#sharing-between-machines)). A run without
the forge does not overwrite forge-backed state. `--all` emits one array of
per-repository reports. Bare `sync` selects the managed checkout you are in;
outside one, it asks for a repository name or `--all`, rather than sweeping by
accident. `--no-github` skips PR/comment lookups, retaining local fetch and head
checks.

Last observed forge states are stored as `pull_states`, keyed
`<owner>/<name>#<number>` by the fork's upstream name.
Legacy transition spellings (`new`, `advanced`, `unchanged`) are read as open;
a forge-backed sync replaces them and reports that correction once. An unknown
spelling is a problem, exit `3`; the row is still classified/recorded and the
unreadable record removed, so the same problem does not recur indefinitely.

### Comment activity

For an open tracked PR, comments newer than the last sync mark produce the exact
note `#<n> has comment activity newer than the last sync`. This is informational,
exit `0`; a comment query failure is a problem, exit `3`.

The query costs one extra forge call per open tracked PR. `comment_marks`, keyed
the same way, advances only forward. First observation records a silent mark
to avoid old-activity noise. Edits to existing comments are invisible because
forge `createdAt` does not change.

## `knives preflight [REPO]`

Reports upstream contribution facts: convention files present, whether they
changed since last seen, any stated open-PR cap, and branch state. It reports,
not judges. **pr-preflight** owns the contribution gate;
**maintaining-fork-pr** and **maintaining-fork-release** own the per-PR and sweep
workflows that run it.

## `knives start <branch>`

Claims the branch and opens a jj workspace. Existing branches continue from their
tip, whether found locally or on our `origin`/publish remote after fetch.
A name only on upstream is somebody else's branch; a fork branch of that name
is new here. Divergent bookmarks have no one tip, so `start` refuses and names
the tips rather than choosing one.

For an existing branch, workspace `@` is an empty child of its tip. Move its
bookmark when ready with `jj bookmark set <branch> -r @`, or squash into the
branch. A new branch starts on the release's shared base, or fetched upstream
trunk when no release exists, never arbitrary `@`. Starting on a release merge
would silently inherit its other members. Moving the release to newer upstream
is a separate, intentional `knives release rebase`.

### Claim identity and locking

Holder identity comes from the harness: `KNIVES_OWNER` supplied by the OpenCode
plugin, `CLAUDE_CODE_SESSION_ID`, or `OMP_SESSION_ID`, whichever the shell
carries. Two agents under one OS user are separate claimants; the second `start`
refuses with the holder's name. Without a harness identity the shell is anonymous
(`<user>/os-user`), and that claim can resume only inside its own workspace.
Status displays `<id>/<kind>`.

Every claim-writing command (`start`, `finish`, `track`, `depends`) waits for one
claim-store lock. Wait budget: one minute; pauses double from 20 ms up to 2 s,
with jitter. A paused `start` may simply be queued behind another writer.
The OS advisory lock on `state.lock` releases when its process exits, including
SIGKILL, Ctrl-C or panic. **Do not remove the file.** There is no crash-surviving
lock to clear; a still-held lock belongs to a running process.

An exhausted wait exits `3` and names the holder:

```text
another knives command (pid 4242, holding for 73s) is holding <path>;
try again in a moment
```

Without a readable pid it reports `holder unknown, lock written 73s ago`.
Sidecar locks for sightings and hook session state wait one second, with pauses
from 20 ms to a 200 ms ceiling.

### Repository immutability rule

`start` writes the fork's `immutable_heads()` only when repository config states
none. The rule pins trunk on every knives remote and tags fetched from upstream
(`remote_tags(remote=exact:"upstream")`). It reports the file and rule, including
any user-level rule shadowed here.

jj's default `untracked_remote_bookmarks()` can pin fetched superseded releases
or other forks' PR heads; its `tags()` can pin every member under a release tag.
The knives rule keeps upstream-fetched tags immutable, while fork-release and
`keep/*` tags do not make the commits they point to immutable. A rewrite never
moves a tag: the tagged original retains its commit id locally and remotely.

The rule is written in jj's table form with a `doc` identifying knives. A later
`start` refreshes knives' own rule when the entry changes and reports `refreshed
in`; it leaves a human-stated rule alone. Status reports either kind of mismatch.

## `knives finish <branch>`

Releases the claim and removes its workspace: forgets jj registration, removes
the directory, then clears any Git worktree registration for that now-prunable
path. `start` also clears that stale registration at its own path before creating
a workspace.

Finish when active work stops, including an external wait such as PR review.
The claim means “working here now”, not “this branch matters”. The branch,
bookmark and PR survive; jj snapshots tracked working-copy content into a commit
reachable by change id. `--no-cleanup` retains the directory, needed for files
jj never tracked such as build output or an untracked `.env`.
`--superseded-by <branch>` records where the work went.

The claim's reason is removed on finish. Put durable reasoning and evidence in
[the notch ledger](ledger.md) before relying on it to explain the branch later.

## State a PR link or dependency

Inference associates branches with PR heads from our repository copies and
recognizes a `pr-<n>` bookmark as that fetched PR head. It cannot reliably recover
every intended association: pre-existing, closed or someone else's superseding
PRs may need an explicit statement.

```text
knives track <branch> --pr 4545    # any number, state or author
knives track <branch> --fork-only # deliberately no upstream PR
knives track <branch> --forget    # return to inference
knives depends <branch> --on <repo>#<number>
```

`depends` records that the branch cannot merge before the named PR, including
across forks. Status reports dependencies not yet merged. Preserve their required
content when editing a release: retaining a dependent while dropping its
requirement can ship a broken composition. This is an explicit relationship,
not a dependency inferred from similar branch names.
