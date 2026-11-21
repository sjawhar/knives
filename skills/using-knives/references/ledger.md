# The notch ledger

Read this before you read or write `knives notch`, when you interpret a ledger entry, set up sharing between machines, or run `knives ledger sweep` or `knives ledger migrate`.

Where the ledger lives and how an entry is stored: README.md, "What it remembers". `finish` drops the claim's reason: a reason survives only as a notch.

## Reading and writing

```bash
knives notch                  # newest 20 human notes; machine events folded into one count
knives notch <branch>         # a subject's full history, oldest first
knives notch --events         # every machine event
knives notch <branch> -m 'superseded by #1157; upstream wanted the trait approach' \
  --evidence 06d778b9 --evidence other-repo#1157
knives notch '#4545' -m 'split to a plugin' --disposition ruled-out --evidence <url>
knives notch -m 'this fork needs a cut before the pin moves'   # about the repository
knives notch --verify '#4545'
```

- A `#<n>` subject is a PR, not a branch. It stamps that number, so `--pr <n>` finds it.
- `--repo <name>` records about a library fork while you stand in its consumer.
- `--verify` re-checks commit evidence and anchors against the repository as it is now. It checks evidence, not judgment, and never edits an entry.
- Record events and decisions. Do not copy what a detector computes.
- Exits: `0` done, `1` `--verify` flags, `2` usage, `3` an unreadable directory or entry, or a note that did not reach a shared ledger. No ledger yet: `0`, `no notches yet`.

## Entry fields

| Field | Meaning |
|---|---|
| `kind` | `event`: a command's observation. `note`: an agent's assertion. A hand-written entry is always a note. |
| `disposition` | Optional terminal ruling on a note: `merged-elsewhere`, `withdrawn`, `ruled-out`. |
| `anchor` | The subject's tip when written. A note about an older tip can be stale. |
| `pr` | `--pr`, else the subject's tracked PR. |
| `statement` | `{kind, value}` from `track`, `depends`, `finish --superseded-by` or `ledger migrate`. Kind `pull`, `fork-only`, `superseded` or `depends`. |
| `claim_change` | `claimed`, `taken`, `released` or `resumed`, with `claim_change_why` and, when it displaced an owner, `previous_owner`, `previous_kind`, `previous_started`, `previous_last_seen`, `previous_why`. |

- Per branch and statement kind, the entry with the greatest `ts` is live. A statement with no value is a forget.
- A `depends` list is every requirement stated since the newest `depends` forget, so two machines that each add one keep both. `depends --forget` writes a forget and then the list it leaves; every machine running this knives reads the same list. The forget restates the list this machine holds after its pull: a requirement another machine stated that had not reached it (unsent, or sent after the pull) is lost on every machine.
- A knives from before fork requirements reads an `origin:<owner>/<name>:<branch>` requirement as a statement it cannot read: `status` and `notch` exit `3` and release writes on the fork refuse, until it upgrades. After a forget whose remaining list holds a member branch, it reads no requirement at all for that branch. It reads an `origin:<owner>/<name>#<n>` requirement as one on a fork its registry does not have (a `status` problem, exit `3`). Upgrade every machine that shares the ledger before anyone records either kind.
- Only the `statement` field states anything; prose does not. A statement this knives cannot fully read states nothing and is a problem (exit `3`), and release writes refuse.
- Commands record what they see: claims (`start`, `finish`), statements (`track`, `depends`), PR transitions (`sync`), and every release write. A release write records each parent's full commit and all its bookmarks, and the commit and change id it wrote. Find a lost member from that record, not from current bookmark names.

## Storage

- File name `YYYYMMDDTHHMMSS.nnnnnnnnnZ-xxxx.md`; TOML frontmatter between `+++` fences, then prose. Name order is time order.
- knives never rewrites an entry. Only `ledger migrate` (it moves files), the discard of this machine's uncommitted repeat, and the undo of a claim push the ledger's repository did not take remove one.
- Another `*.md` in a fork directory, or an entry that does not parse, is a problem naming the file (exit `3`). A multi-fork `status`, `audit` or `sync` marks that fork `ledger unavailable: …` and reports the rest.
- An entry of an unknown `kind`, from a newer knives, is left out of reads and stops nothing.

## Upgrading from an older knives

An older knives kept branch statements in `state.json` and filed each fork under its registry key. Run `knives ledger migrate` once on each machine. Until then, every command that opens the state file or that fork's ledger exits `3` and names it; `repos`, `consumers`, `pr`, `register`, `gh` and release commands that read no ledger still work.

- `--dry-run` takes no lock, pulls nothing and writes nothing. It cannot see failures that only a network read or a write would hit.
- Any problem exits `3` and keeps the affected statements in `state.json`. Fix it and run again; a clean second run changes nothing.
- A ledger directory that no entry claims (its `upstream` changed, or two registries spell it differently) is a problem on every pull-first command and blocks writes on every fork. Set the right `upstream`, or move the entries to the right fork.

## Sharing between machines

A fork's ledger travels when its `repos.toml` entry sets `ledger = "<owner>/<name>"` and a git directory over the ledger root has that repository as `origin` and names this machine. The machine needs read and write access; without it, the pull fails.

```bash
git init ~/.config/knives/ledger
git -C ~/.config/knives/ledger remote add origin <ledger repository URL>
git -C ~/.config/knives/ledger config knives.machine <name>
# one per further ledger repository:
git init --bare ~/.config/knives/ledger-repositories/<dir>
git --git-dir ~/.config/knives/ledger-repositories/<dir> config core.bare false
git --git-dir ~/.config/knives/ledger-repositories/<dir> remote add origin <URL>
git --git-dir ~/.config/knives/ledger-repositories/<dir> config knives.machine <name>
```

- `knives.machine` is unique among the machines that share that repository, and is one ref name component: it spells `refs/knives/<machine>`.
- Never `git clone` a ledger repository into `ledger/`. knives reads only `refs/knives/*` and `refs/knives-claims/*`; the clone's `main` is an old snapshot that every read refuses.
- Each git directory carries only the forks whose `ledger` names its `origin`. For a `ledger` no `origin` names, knives makes a git directory in `ledger-repositories/` the first time it needs one and says so on stderr; one it cannot make is a problem (docs/shared-claims.md, "Joining a ledger").

### The sweep

- Every command that writes an entry sends it before it exits; when it does not arrive, the command exits `3` and names the cause, and `knives ledger sweep` sends it later. Per git directory, a sweep fetches every machine's ref, writes in entries the root lacks, commits this machine's new entries to `refs/knives/<machine>`, and pushes only that ref. It never rebases.
- Run by hand, a sweep waits up to a minute for one already running, then reports its own pass. Any problem exits `3`.
- `ledger-sweep.log` beside the state file exists only while the last sweep or send failed. `unsent` counts this machine's entries that no fetched ref holds.
- `sync` pulls first and skips a transition the ledger already holds as that PR's newest. Two machines that both committed one before either pulled keep both.

### Which commands pull first

- Pull first: `status`, `sync`, `audit`, `pushed`, `depends`, `track --forget`, and release commands that read the ledger: the plan, `cut`, `rebase`, `include`, `drop`, `advance`, `adopt`, and `members` without `REF`.
- No pull: `notch`, `start`, `finish`, `track --pr`, `track --fork-only`, `members <REF>`, `--carries`, `--census`, `reap`.
- A failed pull is a problem. Reports answer from local entries and exit `3`. Release writes, `depends` and `track --forget` refuse and write nothing: a stale ledger gives the wrong newest cut, or would erase another machine's statement.
