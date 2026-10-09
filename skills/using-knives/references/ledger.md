# The notch ledger

Read this for `knives notch [SUBJECT]`: durable decisions, supersessions and
release evidence. Each repository has an append-only ledger beside its state
file. Status deletes nothing, but `finish` removes the claim that said why a
branch exists; that reason survives only if recorded here.

## Reading and writing

Bare, `notch` reads the newest 20 human notes and folds machine events into one
newest-event count. A subject reads its whole chronology, oldest first.

```text
knives notch
knives notch <branch>
knives notch release/2026-08-15
knives notch --pr 4545
knives notch --dispositions
knives notch --events
knives notch --verify '#4545'
knives notch --repo other
```

`--dispositions` selects terminal rulings. `--events` reads the full machine-event
chronology. `--verify` re-checks the selected record's evidence, not its judgment.

`-m` writes a note:

```bash
knives notch <branch> -m 'superseded by #1157; upstream wanted the trait approach' \
  --evidence 06d778b9 --evidence other-repo#1157 --pr 4891
knives notch '#4545' -m 'split to a plugin' --disposition ruled-out \
  --evidence https://forge.example/org/libcore/pull/4545
knives notch -m 'this fork needs a cut before the pin moves'
```

Without a subject, the note is about the repository. `--repo` works for reads and
writes: use it when standing in a consumer fork but recording a decision about
its library fork. A `start` workspace binds its registered checkout through its
`.git` file, so ordinary same-repository commands need no explicit repo.

`--pr` filters reads; with `-m`, it explicitly stamps the entry. Without that
stamp, the tracked PR is the fallback. A `#<n>` subject stamps that number without
treating it as a branch. `--evidence` repeats and requires `-m`.
`--disposition` is a lowercase terminal token and requires both `-m` and evidence.

## Fields and verification

| Field | Content |
|---|---|
| `ts` | Automatic RFC 3339 UTC write time. |
| `owner` | Automatic claim-style identity. |
| `subject` | Caller-supplied ref; absent for repository-wide entries. |
| `kind` | `event` for a command's observation; `note` for an agent's assertion. |
| `disposition` | Optional evidence-backed terminal ruling, such as `merged-elsewhere`, `withdrawn` or `ruled-out`; still a note. |
| `text` | Entry written by caller or command. |
| `evidence` | Optional commit ids, `file:line`, `<repo>#<number>` or URLs, including other repositories. |
| `anchor` | Automatic subject tip at write time; absent if it did not resolve. |
| `pr` | Explicit write stamp, else tracked PR for the subject. |
| `statement` | What `track`, `depends` and `finish --superseded-by` stated about the branch: `{ kind, value }`, kind `pull`, `fork-only`, `superseded` or `depends`. The newest per branch and kind is live; a `pull` with no value is a forget. Prose never states anything. |

There are two kinds, not three. A disposition selects a class of note.
`finish --superseded-by` and `start --why` record supersessions and parkings as
events; a hand-written assertion is always a note.

An anchor preserves the decision's context. `notch --verify` tests selected
commit-shaped evidence and anchors against visible commits and local bookmark
tips. It flags missing evidence, vanished anchors and anchors that no longer
match the local subject tip. It never edits an entry or invalidates its historical
judgment automatically. Do not transcribe state a detector can compute: the
ledger holds events and decisions, not another state cache.

## Automatic entries

A command that already witnesses an event records it while doing so; a failed
ledger write fails the command.

| Command | Entry |
|---|---|
| `start`, `claim` | `claimed: <why>` on the branch. |
| `finish` | `claim released`, with supersession if given; an unheld supersession is still recorded. |
| `track --pr/--fork-only/--forget` | The statement that changed. |
| `depends --on` | `requires <repo>#<number>`. |
| `release cut` | Whole parent set, cut change id beside commit id, and prior cut's carried-parent delta. |
| `release include/drop/advance/rebase` | `edited <release>: <delta>; parents: …`, with the resulting parent set. |
| `sync` | Tracked PRs that merged, closed, reopened or advanced since last sync; first sightings are silent. |

Cut/edit events carry structured `parents` frontmatter: each parent's full commit
and **every** local bookmark there at write time. This is how later `advance` or
`include` identifies a member after a non-jj rebuild or upstream merge. Keeping
all names prevents another agent's anchor bookmark from hiding the member name.
The cut's change id also survives conflict-resolution rewrites before push.

Unchanged PRs produce no entry. Ledger contents are not injected into sessions;
reading them is intentional.

## Status rendering

Each branch shows its newest human note, otherwise newest event. Machine output
has `notch: {ts, kind, text, disposition?, anchor?, count}`, absent if there are no
entries. Text uses a short token; a disposition prefixes its text and `+N` means
N masked sibling entries. The anchor dates the note to a tip, not just a time.

Repository entries appear as `repo_notches: {count, last}` in machine output and
`notches <N> repo-level, newest: "<text>" (<age>)` after findings in text. This is a
local ledger read, not a forge request.

## Storage and exits

Each repository uses `~/.config/knives/ledger/<repo>/`. Each entry is one immutable
Markdown file with TOML frontmatter between `+++` fences and a prose body.
Writes finish a temporary file then atomically persist without replacement.
There is no ledger lockfile. Filenames use compact UTC time plus a four-hex suffix;
readers scan lexical filename order, which is chronological.

No rotation or retention policy applies. Unknown frontmatter keys are ignored,
allowing newer writers and older readers; there is no version-number field.

`0` is a completed read/write, `2` a usage error, and `3` an unreadable ledger
directory or entry. A repository with no ledger yet is different: exit `0` with
`no notches yet`.

A command that reads branch statements (`status`, `audit`, `pushed`, `sync`,
`track`, `depends`, `finish`, `start`, a `notch` write) reads the ledger of each
fork it works on when it starts, and no other. An unreadable entry in one of
those stops the command with exit `3` naming the file, rather than reading that
ledger's statements as absent.

An older knives kept these statements in `state.json` (`tracked_pulls`,
`fork_only`, `superseded`, `dependencies`), and this one does not read them
there. Until they move, every other command refuses that state file, `knives hook`
included, naming the command to run: it would otherwise report every stated pull
request and claim as absent. `knives ledger migrate` writes one statement entry per statement it finds
and then drops the four maps. It reads nothing out of existing entries' prose,
and it skips a statement some entry about that branch already makes with the
same kind and value, so a second run writes nothing. If any statement cannot
reach its ledger, `state.json` keeps all of them and the command exits `3`.

## Sharing between machines

A fork's ledger travels between machines when its `repos.toml` entry names the
repository the ledger belongs to, as `ledger = "<owner>/<name>"`, and this
machine's ledger root, `~/.config/knives/ledger/`, is the working tree of a git
repository whose `origin` is that repository and whose config names this machine:

```bash
git init ~/.config/knives/ledger
git -C ~/.config/knives/ledger remote add origin <the ledger repository's URL>
git -C ~/.config/knives/ledger config knives.machine <name>    # unique among machines on the remote
```

A fork's entries go to that `origin` and nowhere else. A fork without `ledger`
stays on this machine. A fork whose `ledger` names some other repository than
the root's `origin` travels nowhere from here, and every sweep, and every pull
that asks about it, says so as a problem. With no fork's `ledger` set the
ledger is not shared, and nothing says so. `ledger` is refused on a fork whose
`upstream` is a filesystem path, which names no repository another machine
could share. The repository's git config names the machine and nothing else: a
`knives.*` key other than `knives.machine` is refused with the command that
removes it.

Every command that appends an entry starts `knives ledger sweep` in the
background as it exits, without waiting for it. A sweep commits new entries to
`refs/knives/<machine>` (git plumbing only, never the index), fetches every
machine's ref, writes in the entries this machine lacks, and pushes its own ref,
which no other machine writes. One sweep runs at a time, holding `ledger.lock`
beside the state file; a sweep that finds it held exits `0` at once, because the
holder looks again before it stops. A failing sweep leaves its errors in
`ledger-sweep.log` beside the state file, which exists only while the last sweep
failed. Run `knives ledger sweep` by hand to see what one carries.

A command that decides something from the ledger pulls it first: `status`,
`sync`, `audit`, `pushed`, and the release plan, `cut`, `rebase`, `include`,
`drop` and `advance`. A pull that fails is a problem on the report, naming the
remote. A report still answers from the entries this machine has, and exits
`3`. A release write refuses, because its drop guard checks against the newest
recorded cut and a stale ledger would check against the wrong one. `status`
also notes how many of a fork's entries the remote lacks, and names
`ledger-sweep.log` when the last sweep failed. `notch`, `start`, `finish`,
`track` and `depends` do not pull: they read a statement only to stamp their
entry with a pull request number.
