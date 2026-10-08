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

Every command that reads branch statements (`status`, `audit`, `pushed`, `track`,
`start`, a `notch` write, and the rest) reads every repository's ledger when it
starts. An unreadable entry in any of them stops the command with exit `3` naming
the file, rather than reading that ledger's statements as absent.
