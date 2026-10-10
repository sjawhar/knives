# The notch ledger

Read this for `knives notch [SUBJECT]`, `knives ledger sweep` and `knives ledger
migrate`: durable decisions, supersessions, branch statements and release
evidence. Each fork has a ledger beside its state file, one immutable file per
entry. Status deletes nothing, but `finish` removes the claim that said why a
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
| `email` | Automatic: the writer's `git config user.email`, asked in the fork's checkout (where a repository's own identity overrides the user's), or where the command runs when it has none. Absent when git reports none, and on entries written before knives recorded it. |
| `subject` | Caller-supplied ref; absent for repository-wide entries. |
| `kind` | `event` for a command's observation; `note` for an agent's assertion. |
| `disposition` | Optional evidence-backed terminal ruling, such as `merged-elsewhere`, `withdrawn` or `ruled-out`; still a note. |
| `text` | Entry written by caller or command. |
| `evidence` | Optional commit ids, `file:line`, `<repo>#<number>` or URLs, including other repositories. |
| `anchor` | Automatic subject tip at write time; absent if it did not resolve. |
| `pr` | Explicit write stamp, else tracked PR for the subject. |
| `statement` | What `track`, `depends`, `finish --superseded-by` and `ledger migrate` stated about the branch: `{ kind, value }`, kind `pull`, `fork-only`, `superseded` or `depends` (the whole comma-joined requirement list). Per branch and kind, the entry with the greatest `ts` is live, whatever order entries arrived in; a statement with no value is a forget. An entry without the field states nothing, whatever its prose says. A kind this knives does not know, written by a newer one, reads and states nothing; a field other than `kind` and `value` fails the read. |

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
| `start` | `claimed: <why>` on the branch; `resumed`, or `resumed via workspace possession`, when it resumes a claim it holds. |
| `finish` | `claim released`, with supersession if given; an unheld supersession is still recorded. |
| `track --pr/--fork-only/--forget` | The statement that changed. |
| `depends --on` | `requires <owner>/<name>#<number>`: the required fork by its upstream name, however `--on` named it. |
| `release cut` | Whole parent set, cut change id beside commit id, and prior cut's carried-parent delta. |
| `release include/drop/advance/rebase` | `edited <release>: <delta>; parents: …`, with the resulting parent set. |
| `sync` | Tracked PRs that merged, closed, reopened or advanced since last sync. First sightings are silent, and so is a transition the fork's ledger already holds as that PR's newest ([sharing](#sharing-between-machines)). |

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

A fork's ledger is `~/.config/knives/ledger/<owner>/<name>/`, beside the state
file: its upstream repository's owner and name in lowercase, the
`upstream_name` that [`knives repos`](reports.md#knives-repos) lists, so every
machine files a fork in the same place whatever its registry key calls it
(`RepoEntry::upstream_name`, `src/config.rs`). A fork whose `upstream` is a
filesystem path is kept under its registry key. Each entry is one Markdown file
with TOML frontmatter between `+++` fences and a prose body. A write finishes a
temporary file, then atomically persists it without replacement, and takes no
lock: two writers never share a file. Filenames use compact UTC time plus a
four-hex suffix; readers scan lexical filename order, which is chronological.
Only a regular `*.md` file is an entry: every other file, and a symlink named
like one, is passed over by every reader and never shared.

An entry is never rewritten, and no rotation or retention policy applies. Two
things remove an entry file: `knives ledger migrate` moving it into its fork's
new directory, and a sweep discarding this machine's uncommitted repeat of a
transition another machine already committed (both below). Unknown frontmatter
keys are ignored, allowing newer writers and older readers; there is no
version-number field.

`0` is a completed read/write, `2` a usage error, and `3` an unreadable ledger
directory or entry. A repository with no ledger yet is different: exit `0` with
`no notches yet`.

A command that reads branch statements (`status`, `audit`, `pushed`, `sync`,
`track`, `depends`, `finish`, `start`, a `notch` note about a branch without
`--pr`) reads the ledger of each fork it works on when it starts, and no
other. An unreadable entry in one of those stops the command with exit `3`
naming the file, rather than reading that ledger's statements as absent. So
does a statement whose value its kind cannot hold: a `pull` that is not a
decimal number, or a `depends` part that is not `<owner>/<name>#<number>`.
Skipped, it would let an older statement, or a shorter list, read as live.

## Upgrading from an older knives

An older knives kept branch statements in `state.json` (`tracked_pulls`,
`fork_only`, `superseded`, `dependencies`), and kept each fork under its
registry key: its ledger in `ledger/<registry key>/`, its `state.json` keys and
its `seen.json` workspace sightings. This one reads none of them there. After
upgrading, run `knives ledger migrate` once on each machine.

Until it runs, the state file refuses to open while it holds those maps, or
while a `state.json` key or ledger directory still names a fork by its registry
key. Every command that opens it exits `3` naming `knives ledger migrate`,
among them `status`, `sync`, `audit`, `pushed`, `preflight`, `start`,
`finish`, `track`, `depends` and a `notch` write. A fork's ledger refuses the
same way while `ledger/<registry key>/` holds an entry file, so every read or
write of it does too (`notch`, and each `release` command that reads the
ledger), and so does the sweep. Read from the new places alone, each would
report every claim and statement as absent, or find no recorded cut for a
release's drop guard. The harness hook, Claude Code and OpenCode alike, puts
the refusal into the agent's context in place of the claims notice, once per
session for each repository, until the migration runs. Commands that open
neither still answer: `repos`, `consumers`, `pr`, `register`, `gh`, and the
`release` commands that read no ledger (listed below).

`knives ledger migrate`, in order:

1. Moves each `ledger/<registry key>/` entry file into
   `ledger/<owner>/<name>/`, contents untouched, holding `ledger.lock` so no
   sweep commits a half-moved fork. A file already there with the same bytes,
   pulled from a machine that migrated first, is the same entry, and the old
   copy goes; one with different bytes is a problem, and both stay. A registry
   key that is now another fork's owner directory (`acme` beside `acme/demo`)
   keeps the directories inside it.
2. Renames every `state.json` key that names a fork by its registry key:
   `claims` (and each claim's `repo`), `comment_marks`, `pull_states`,
   `foreign_parents`, `conventions` and `pull_heads`. A key whose new name is
   taken is a problem, and both stay.
3. Pulls each fork's ledger, then writes one statement entry per `state.json`
   statement (an event reading `migrated from state.json: …`), recording each
   dependency's required fork under its upstream name, then drops the four
   maps. It reads nothing out of existing entries' prose, and it skips a
   statement some entry about that branch already makes with the same kind and
   value. A fork whose pull fails has none of its statements written. A
   statement the ledger already makes otherwise for that branch and kind
   (another machine migrated first, or stated it since) is a problem naming
   both values, and is not written: the ledger's is what every machine reads.
   If this or an earlier step left a problem other than such a contradiction,
   `state.json` keeps every statement.
4. Renames each `seen.json` sighting `<registry key>/<workspace>` to
   `<upstream name>/<workspace>`, keeping the later stamp where both exist. An
   unreadable `seen.json` is a problem, but does not hold back the statements.

Any problem exits `3`, and a run after the fix finishes the job; a run with
nothing left to move changes nothing. `knives ledger migrate --dry-run`
reports the same, as what it would move, rename and write, and changes
nothing: it takes no lock, pulls nothing and writes no file, so it counts
from the ledger this machine has. TOON and JSON share its report:

```text
{dry_run?, moved: [{from, to}], renamed: [{map, from, to}], sightings: [{from, to}],
 wrote, already, problems}
```

`moved` gives each ledger directory's full old and new path, `renamed` each
`state.json` key with its map, and `sightings` each `seen.json` key. `wrote`
counts statement entries appended; `already`, statements the ledger already
carried. Every field is present, empty or `0` when there was nothing to do.

A fork is kept under its upstream's name, so a fork whose `upstream` changes,
or one two machines' registries spell as two repositories, leaves its entries
in a directory no fork is kept under, and reads its ledger without them. Each
directory under `ledger/` that holds entries no registry entry keeps (by its
upstream name or its registry key) is a problem naming the directory and the
two fixes: correct that fork's `upstream` in `repos.toml`, or move the entries
into the directory of the fork they belong to. Every command that pulls first
([sharing](#sharing-between-machines)) carries it, so a report exits `3` and a
write that replaces what it read refuses; so do a `notch` read and every sweep.

## Sharing between machines

A fork's ledger travels between machines when its `repos.toml` entry names the
repository the ledger belongs to, as `ledger = "<owner>/<name>"`
([configuration](configuration.md#registry-fields)), and a git directory over
the ledger root has that repository as its `origin`. knives reads two places,
each a git directory whose working tree is the ledger root,
`~/.config/knives/ledger/`:

- `~/.config/knives/ledger/.git`, the root's own repository, or the git
  directory it names when it is a gitfile (a linked worktree, or `git init
  --separate-git-dir`);
- each directory in `~/.config/knives/ledger-repositories/`, or symlink to one:
  one for each further repository the ledgers travel through. Anything else
  there is refused.

Each names this machine in its own git config, as `knives.machine`, unique
among the machines sharing that repository:

```bash
git init ~/.config/knives/ledger
git -C ~/.config/knives/ledger remote add origin <one ledger repository's URL>
git -C ~/.config/knives/ledger config knives.machine <name>

git init --bare ~/.config/knives/ledger-repositories/<dir>
git --git-dir ~/.config/knives/ledger-repositories/<dir> config core.bare false
git --git-dir ~/.config/knives/ledger-repositories/<dir> remote add origin <another ledger repository's URL>
git --git-dir ~/.config/knives/ledger-repositories/<dir> config knives.machine <name>
```

A machine joined only through `ledger-repositories` needs no `ledger/`
directory yet: the first pull or sweep creates it before git runs there.

Each git directory carries the forks whose `ledger` names its `origin`, letter
case and a `.git` suffix aside, and a fork's entries go through that one and
nowhere else. A fork without `ledger` is not shared. A fork whose `ledger` no
git directory's `origin` names travels nowhere from here, and every sweep, and
every pull that asks about it, says so as a problem. With no fork's `ledger`
set, nothing is shared, and nothing says so as a problem. Refused, each naming
the git directory and what fixes it: two git directories with one `origin`,
since which of them carries a fork would be a guess; one that carries forks
but names no machine; and any `knives.*` key other than `knives.machine`
(`destinations`, `src/ledger_sweep.rs`).

Every command that appends an entry starts `knives ledger sweep` in the
background as it exits, without waiting for it. A sweep takes `ledger.lock`
beside the state file without waiting; one that finds it held exits `0` at
once, because the holder looks again before it stops. The holder passes over
each git directory under that directory's own `knives-transport.lock`: it
fetches every machine's ref into `refs/knives-remotes/origin/<machine>` and
writes in the entries this machine lacks, discards this machine's repeated
transitions (below), commits what is new to `refs/knives/<machine>` (git
plumbing only, never the index), and pushes that ref, which no other machine
writes. A failed fetch does not stop the commit and push, and neither does an
entry that does not parse: it is a problem naming the file, left out of the
repeated-transition check, and, when it is this machine's own and not yet
committed, left out of the commit too, since every machine that took it would
fail to read that fork; the rest of the fork's entries go. A machine's ref only
moves forward, so a fetch refuses a peer's ref that moved backward on the
remote (rewritten, or deleted and pushed again); it still takes every other
machine's, and their entries are written in, while the refused ref is a problem
naming it until someone restores it. A fetch or push still
running after a minute is ended, and an HTTP transfer that stalls fails sooner,
so a remote that stops answering is a failure like any other rather than a wait
with no end. It passes again
until a pass finds nothing new. A failing sweep leaves its errors in
`ledger-sweep.log` beside the state file, which exists only while the last
sweep failed. Run `knives ledger sweep` by hand to see what one carries; TOON
and JSON share its report:

```text
{outcome, destinations?, problems?}
destination: {git_dir, remote, forks, commits, pulled, pushes, discarded, skipped?}
```

`outcome` is `swept`; `busy` when another sweep holds the lock and carries this
one's entries; `not-shared` when no git directory names a machine and no fork
sets `ledger`; or `failed` when the sweep could not start because where the
ledger travels could not be read, which its `problems` say. Each destination
is one git directory: `remote` is always
`origin`, `forks` lists the upstream names it carries, and the counts cover the
whole sweep: commits made, entries pulled in, pushes, and repeated transitions
discarded. `skipped`, by `<owner>/<name>` directory, counts the entries other
machines sent through that repository for a fork it does not carry here, which
this machine never writes in: what two machines whose registries send one fork
to different repositories see. Any problem exits `3`.

A sweep a write hands off prints to nowhere, so each sweep also keeps what it
discarded and left unread in `ledger-sweep.json` beside the state file, and
`status` reads it: the discards add up per fork, with when the newest went,
and the skipped count per repository is what the last pull from it found. The
file is removed once it holds nothing.

Every machine that syncs compares the forge with its own record, so two that
sync after one merge each see `#N merged`. `sync` pulls first, and writes no
transition the fork's ledger already holds as that pull request's newest, with
the same text and `pr` stamp; closed, reopened and closed again is still two
closes. When two machines each wrote it before either pulled, the one that
sweeps second deletes its own uncommitted copy after its fetch and before its
commit, counted in `discarded`. A committed entry is never deleted, so a repeat
two machines both committed stays (`repeated_transitions`,
`src/commands/sync.rs`).

A command that decides something from the ledger pulls it first: `status`,
`sync`, `audit`, `pushed`, each `release` command that reads the ledger (the
plan, `cut`, `rebase`, `include`, `drop`, `advance`, and `members` without a
`REF`, which reads it to name the release in hand), `depends`, and `track
--forget`. `members <REF>`, `--carries`, `--census` and `reap` read no ledger
and pull nothing. A pull that fails, a fork whose `ledger` no git directory
here reaches, and a setup the sweep refuses are each a problem on the report,
naming what failed. A report still answers from the entries this machine has,
and exits `3`. A release write refuses on any of them, because its drop guard
checks against the newest recorded cut and a stale ledger would check against
the wrong one. So do `depends` and `track --forget`, each exiting `3` having
written nothing: a `depends` statement replaces the whole list and a forget
erases the statement before it, so built from a ledger missing another
machine's newer statement, either would replace that statement on every
machine. A forget with nothing stated after the pull writes no statement.
`status` also notes how many of a fork's entries the remote lacks, names
`ledger-sweep.log` when the last sweep failed, and says from
`ledger-sweep.json` how many of the fork's entries other machines sent through
a repository this one does not send it through, and how many repeated
transitions sweeps here discarded. `notch`, `start`, `finish`,
`track --pr` and `track --fork-only` do not pull: they replace nothing they
read, and read a statement only to stamp their entry with a pull request
number.
