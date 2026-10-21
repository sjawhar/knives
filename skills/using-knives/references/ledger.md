# The notch ledger

For `knives notch [SUBJECT]`, `knives ledger sweep` and `knives ledger migrate`: decisions, supersessions, branch statements and release evidence. Each fork has a ledger beside the state file, with one immutable file per entry. `finish` removes the claim that said why a branch exists. That reason survives only if it is recorded here.

## Reading and writing

Bare `notch` reads the newest 20 human notes and folds machine events into one count. A subject reads its full history, oldest first.

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

```bash
knives notch <branch> -m 'superseded by #1157; upstream wanted the trait approach' \
  --evidence 06d778b9 --evidence other-repo#1157 --pr 4891
knives notch '#4545' -m 'split to a plugin' --disposition ruled-out \
  --evidence https://forge.example/org/libcore/pull/4545
knives notch -m 'this fork needs a cut before the pin moves'
```

- With no subject, the note is about the repository.
- Use `--repo` to record a decision about a library fork while you stand in its consumer. A `start` workspace binds its checkout through its `.git` file.
- `--pr` filters reads. With `-m`, it stamps the entry. Without it, the tracked PR is the stamp. A `#<n>` subject stamps that number and is not a branch.
- `--evidence` needs `-m`. `--events` reads the full machine-event history. `--verify` checks evidence again, not judgment.

## Fields and verification

| Field | Content |
|---|---|
| `ts` | Automatic RFC 3339 UTC write time. |
| `owner` | Automatic claim-style identity. |
| `email` | Automatic `git config user.email`, asked in the fork's checkout, else where the command runs. Absent when git has none, and on older entries. |
| `subject` | The ref. Absent for repository entries. |
| `kind` | `event` for a command's observation, `note` for an agent's assertion. |
| `disposition` | Optional terminal ruling (`merged-elsewhere`, `withdrawn`, `ruled-out`). Still a note. |
| `text` | The entry. |
| `evidence` | Commit ids, `file:line`, `<repo>#<number>` or URLs, in any repository. |
| `anchor` | Automatic subject tip at write time. Absent when it did not resolve. |
| `pr` | Explicit stamp, else the subject's tracked PR. |
| `statement` | What `track`, `depends`, `finish --superseded-by` and `ledger migrate` stated: `{kind, value}`. Kind is `pull`, `fork-only`, `superseded` or `depends` (the comma-joined list that the writer knew). |

Statement rules:

- Per branch and kind, the entry with the greatest `ts` is live, in any arrival order. A statement with no value is a forget.
- A `depends` list is every requirement stated since the newest `depends` forget. Thus two machines that each add one keep both.
- A statement that replaces a live one has a stamp after it, whatever the writer's clock says.
- An entry with no `statement` field states nothing, whatever its prose says.
- A statement that this knives cannot fully read (unknown kind, extra field, bad value) states nothing. Every read names its file as a problem.

Other rules:

- There are two kinds only. A disposition selects a class of note. `finish --superseded-by` and `start --why` write events. A hand-written assertion is always a note.
- `notch --verify` tests commit-shaped evidence and anchors against visible commits and local tips. It flags missing evidence, lost anchors, and anchors that no longer match the subject tip. It never edits an entry or cancels its judgment.
- An entry that `--verify` cannot read (an unknown kind, a misnamed file) is a `problem`, exit `3`, which wins over the `1` of a flag.
- Do not copy state that a detector computes into the ledger. The ledger holds events and decisions.

## Automatic entries

A command records an event that it sees. A failed ledger write fails the command.

| Command | Entry |
|---|---|
| `start` | `claimed: <why>`. On a held claim, `resumed` or `resumed via workspace possession`. |
| `finish` | `claim released`, with the supersession if given, also when unheld. |
| `track --pr/--fork-only/--forget` | The changed statement. |
| `depends --on` | `requires <owner>/<name>#<number>`, by upstream name, however `--on` spelled it. |
| `release cut` | The full parent set, the cut change id beside the commit id, and the carried-parent delta from the prior cut. |
| `release include/drop/advance/rebase` | `edited <release> as <commit> (change <id>) with <n> parent(s): <delta>; parents: …`. `audit` holds the release against the commits its cut and edit events name. |
| `sync` | Tracked PRs that merged, closed, reopened or advanced. First sightings are silent. So is a transition that the ledger already holds as the newest for that PR ([sharing](#sharing-between-machines)). |

- Cut and edit events carry `parents` frontmatter: the full commit of each parent and **every** local bookmark on it. This lets a later `advance` or `include` find a member after a rebuild outside jj or an upstream merge. All names are kept, so the bookmark of another agent cannot hide the member name.
- The cut change id survives conflict-resolution rewrites before push.
- Unchanged PRs write nothing. knives never injects ledger contents into sessions. You read them on purpose.

## Status rendering

- Each branch shows its newest human note, else its newest event: `notch: {ts, kind, text, disposition?, anchor?, count}`. It is absent with no entries.
- Text shows a short token, with a disposition prefix and `+N` hidden siblings. The anchor dates the note to a tip, not only a time.
- Repository entries are `repo_notches: {count, last}`. In text, `notches <N> repo-level, newest: "<text>" (<age>)` follows the findings. This is a local read, with no forge call.

## Storage and exits

- The path is `~/.config/knives/ledger/<owner>/<name>/`. This is the lowercase `upstream_name` that [`knives repos`](reports.md#knives-repos) lists, so every machine files a fork in the same place (`RepoEntry::upstream_name`, `src/config.rs`). When `upstream` is a filesystem path, the registry key is used.
- An entry is a Markdown file with TOML frontmatter between `+++` fences and a prose body. A write finishes a temp file, then persists it atomically with no replacement and no lock. Two writers never share a file.
- The file name is `YYYYMMDDTHHMMSS.nnnnnnnnnZ-xxxx.md`, with the suffix in lowercase hex. Readers scan in lexical order, which is time order.
- Only a regular file with exactly that name is an entry (`is_entry_file_name`, `src/ledger.rs`). knives never reads or shares another `*.md` in the fork directory. Every read names it as a problem, exits `3`, and still answers from the entries. Readers skip other files, and symlinks with entry names.
- knives never rewrites an entry, and has no rotation. Only two things remove an entry file: `ledger migrate` (it moves the file), and a sweep that discards an uncommitted repeat transition of this machine. Readers ignore unknown frontmatter keys. There is no version field.
- Exits: `0` done, `2` usage error, `3` unreadable directory or entry. A repository with no ledger yet gives `0` and `no notches yet`.
- Commands that read branch statements read the ledger of each fork they work on, at start. These are `status`, `audit`, `pushed`, `sync`, `track`, `depends`, `finish`, `start`, and a `notch` note about a branch without `--pr`.
- An entry that does not parse stops the command with exit `3` and names the file. A multi-fork `status`, `audit` or `sync` marks that fork `ledger unavailable: …` instead, exits `3`, and reports the other forks.
- Content from a newer knives never stops a command, because machines and sessions run different versions. An entry of an unknown `kind` is left out of reads.
- A statement that this knives cannot fully read states nothing. Such a statement has an unknown kind, a field other than `kind` and `value`, or a bad value. A bad value is a `pull` that is not decimal, or a `depends` part that is not `<owner>/<name>#<number>`. Each is a problem that names the file in `status`, `notch` (read or `--verify`), `audit`, `pushed`, `sync` and every release command. They exit `3` and answer from the rest. A release write refuses. Read in part, it could make an older statement or a shorter list look live.
- A peer tree can hold only regular `<owner>/<repository>/<file>` entries. Any other path, such as a nested entry or a control character in a name, refuses the whole pull before it writes. A well-shaped peer entry that does not parse is left out. It is a problem that names the machine ref and path, and each pull checks it again.
- A pull writes nothing at a path whose contents disagree: two refs with different blobs, or a ref blob unlike the local file. The problem names the path and every `refs/knives/<machine>` that carries it, and the local file stays. The comparison uses the git blob id, so equal contents are not a problem.

## Upgrading from an older knives

An older knives kept branch statements in `state.json` (`tracked_pulls`, `fork_only`, `superseded`, `dependencies`). It kept each fork under its registry key: `ledger/<registry key>/`, `state.json` keys and `seen.json` sightings. This knives reads none of them there. After you upgrade, run `knives ledger migrate` once on each machine.

- Until it runs, the state file refuses to open in two cases. It holds those maps, or a `state.json` key or ledger directory still names a fork by its registry key. Each command that opens it exits `3` and names `knives ledger migrate`: `status`, `sync`, `audit`, `pushed`, `preflight`, `start`, `finish`, `track`, `depends` and a `notch` write.
- A fork ledger with an entry file in `ledger/<registry key>/` refuses each read and write (`notch`, release commands that read the ledger) and the sweep. Otherwise these would report all claims and statements as absent, or find no recorded cut for the drop guard.
- The hook (Claude Code and OpenCode) puts the refusal in context in place of the claims notice, once per session per repository.
- These still work: `repos`, `consumers`, `pr`, `register`, `gh`, and release commands that read no ledger.

`knives ledger migrate` does these steps in order:

1. It moves each entry file in `ledger/<registry key>/` to `ledger/<owner>/<name>/`, with contents untouched, and holds `ledger.lock`. A file there with the same bytes (from a machine that migrated first) is the same entry, and the old copy goes. Different bytes are a problem, and both stay. A registry key that is now an owner directory (`acme` beside `acme/demo`) keeps the directories in it.
2. It renames the `state.json` keys that name a fork by registry key: `claims` (and the `repo` of each claim), `comment_marks`, `pull_states`, `foreign_parents`, `conventions`, `pull_heads`. A name that is taken is a problem, and both stay.
3. It pulls each fork's ledger and writes one statement entry per `state.json` statement (an event `migrated from state.json: …`, with dependencies under upstream names). Then it drops the four maps. It reads no prose, and skips a statement that an entry already makes with the same kind and value.
   A fork whose pull fails has no statements written. A different statement already in the ledger for that branch and kind is a problem that names both values. It is not written. A key that names no fork in `repos.toml` is a problem that names both remedies: add the fork back, or remove the key from `state.json`. It is written nowhere. Any other problem in this step or an earlier one keeps every statement in `state.json`.
4. It renames `seen.json` sightings from `<registry key>/<workspace>` to `<upstream name>/<workspace>`, and keeps the later stamp. An unreadable `seen.json` is a problem, but it does not stop the statements.

- Any problem exits `3`. Run it again after the remedy. With nothing left, it changes nothing.
- `--dry-run` takes no lock, pulls nothing, writes nothing, and counts from the local ledger. It holds back what the real run would hold back with no network read. That is a fork whose `ledger` is the `origin` of no local git directory, and every fork when the sweep would refuse the setup. It cannot see failures that occur only during a network read or a write.

```text
{dry_run?, moved: [{from, to}], renamed: [{map, from, to}], sightings: [{from, to}],
 wrote, already, problems}
```

`moved` gives full old and new directory paths. `renamed` gives each key with its map, and `sightings` each `seen.json` key. `wrote` counts statements written, and `already` statements that were present. Every field is present, empty or `0` when nothing happened.

**Unclaimed directories.** When a fork's `upstream` changes, or two registries spell it differently, its entries stay in a directory that no entry keeps. That fork's ledger reads without them. Each such directory is a problem that names two remedies: set the right `upstream` in `repos.toml`, or move the entries to the right fork. Every pull-first command carries it, so reports exit `3` and replacing writes refuse. `notch` reads and sweeps carry it too. It blocks writes on every fork. Retire a fork only after you move or remove its directory.

## Sharing between machines

A fork's ledger travels when two things are true. Its `repos.toml` entry sets `ledger = "<owner>/<name>"` ([configuration](configuration.md#registry-fields)). A git directory over the ledger root `~/.config/knives/ledger/` has that repository as `origin`. The machine needs read and write access. Without it, the pull fails, and every release write, `depends` and `track --forget` refuse.

knives reads these git directories:

- `~/.config/knives/ledger/.git`, or the git directory it names as a gitfile (a linked worktree, or `git init --separate-git-dir`),
- each directory, or symlink to one, in `~/.config/knives/ledger-repositories/`, one for each further repository. Anything else there is refused.

Each sets `knives.machine`, unique among the machines that share that repository:

```bash
git init ~/.config/knives/ledger
git -C ~/.config/knives/ledger remote add origin <one ledger repository's URL>
git -C ~/.config/knives/ledger config knives.machine <name>

git init --bare ~/.config/knives/ledger-repositories/<dir>
git --git-dir ~/.config/knives/ledger-repositories/<dir> config core.bare false
git --git-dir ~/.config/knives/ledger-repositories/<dir> remote add origin <another ledger repository's URL>
git --git-dir ~/.config/knives/ledger-repositories/<dir> config knives.machine <name>
```

- A machine that joins only through `ledger-repositories` needs no `ledger/` yet. The first pull or sweep makes it.
- Join through `ledger-repositories/`: make one git directory there, as above, for each repository that a `ledger` names.
- A plain `git clone` of a ledger repository into `ledger/` is not a ledger. Its `main` is an old snapshot in the registry-key layout, and knives reads only `refs/knives/*`. The sweep and every ledger read refuse such a clone, naming a registry-key directory, until `knives ledger migrate` runs ([upgrading](#upgrading-from-an-older-knives)).

Routing:

- Each git directory carries the forks whose `ledger` names its `origin`, with case and `.git` aside. A fork goes through that one only.
- A fork with no `ledger` is not shared. A `ledger` that no `origin` names travels nowhere, and every sweep and every pull that asks about it reports that as a problem. When no fork sets `ledger`, nothing is shared, and that is not a problem.
- knives refuses these, and names the directory and the remedy:
  - two git directories with one `origin`,
  - a directory that carries forks but names no machine,
  - a `knives.machine` that is not one ref name component (no `/`, no leading `-` or `.`, no `..`), because it spells `refs/knives/<machine>`.
- Another `knives.*` key, for a newer knives, is not refused. `status` notes it and `ledger sweep` lists it under `notes`, each with the command that removes it.

### The sweep

- Every command that writes an entry starts `knives ledger sweep` in the background when it exits. That sweep takes `ledger.lock` with no wait. When the lock is held, it exits `0`, because the holder looks again before it stops.
- A sweep run by hand waits up to a minute for the holder, then runs its own pass and reports its own result. A failure of the sweep it waited for goes only to `ledger-sweep.log`.
- For each git directory, under its `knives-transport.lock`, the sweep does this:
  1. Fetch every machine ref into `refs/knives-remotes/origin/<machine>`.
  2. Write in only entries that the root lacks, as regular entry files in `<owner>/<repository>/`. A peer tree with any other path is refused before any write.
  3. Discard repeat transitions of this machine (below).
  4. Commit new entry files to `refs/knives/<machine>`, with git plumbing, never the index.
  5. Push that ref, which no other machine writes.
- It repeats until a pass finds nothing new.
- A failed fetch does not stop the commit and push. An entry that does not parse is a problem that names the file. It is left out of the repeat check. An uncommitted entry of this machine that does not parse is also left out of the commit. Otherwise every machine that took it could not read that fork. The other entries of the fork go.
- A machine ref only moves forward. The fetch refuses a peer ref that moved backward (rewritten, or removed and pushed again). It is a problem until someone restores it. The refs of other machines still come in.
- A fetch or push that runs over a minute is stopped. A stalled HTTP transfer fails sooner.
- `ledger-sweep.log` beside the state file holds the errors, and exists only while the last sweep failed. Run `knives ledger sweep` to see what one carries:

```text
{outcome, waited?, destinations?, problems?, notes?}
waited: {pid?, seconds}
destination: {git_dir, remote, forks, commits, pulled, pushes, discarded, skipped?,
              unsent?}
```

| Field | Meaning |
|---|---|
| `outcome` | `swept`. `busy`: a handed-off sweep found the lock held, and the holder carries its entries. `not-shared`: no git directory names a machine and no fork sets `ledger`. `failed`: the routing was unreadable, or the lock was still held after a minute (see `problems`). |
| `waited` | Present when it waited: the pid in `ledger.lock`, and the seconds. |
| destination | One git directory. `remote` is always `origin`. `forks` lists the upstream names it carries. The counts cover the whole sweep: commits, entries pulled, pushes, repeats discarded. |
| `skipped` | Per `<owner>/<name>`: entries that other machines sent through it for a fork that it does not carry here. This machine never writes them in. Two registries that route one fork differently cause this. |
| `unsent` | Entries of this machine, in forks it carries, that no fetched ref holds at the end. `0` when all arrived, or when nothing needed sending. Absent only when it could not be counted, and a problem says why. |

- Any problem exits `3`.
- A handed-off sweep prints nowhere. Thus each sweep also keeps discards and skipped counts in `ledger-sweep.json` beside the state file, and `status` reads it. Discards add up per fork, with the time of the newest. The skipped count per repository is what the last pull found. The file goes away when it holds nothing.

### Repeated transitions

- Each machine that syncs compares the forge with its own record, so two machines can each see `#N merged`.
- `sync` pulls first. It writes no transition that the ledger already holds as that PR's newest, with the same text and `pr` stamp. Closed, reopened and closed again is still two closes.
- When two machines each wrote it before either pulled, the second to sweep removes its uncommitted copy after its fetch and before its commit. This counts in `discarded`.
- knives never removes a committed entry, so a repeat that both machines committed stays (`repeated_transitions`, `src/ledger/transitions.rs`).

### Which commands pull first

- Pull first: `status`, `sync`, `audit`, `pushed`, `depends`, `track --forget`, and release commands that read the ledger. Those are the plan, `cut`, `rebase`, `include`, `drop`, `advance`, and `members` without `REF` (it reads the ledger to name the release in hand).
- Read no ledger and pull nothing: `members <REF>`, `--carries`, `--census`, `reap`.
- No pull: `notch`, `start`, `finish`, `track --pr`, `track --fork-only`. They build nothing from what they read. They read a statement only to stamp their entry with a PR number, and to stamp theirs after the live one.
- A failed pull, a `ledger` that no local git directory reaches, and a refused setup are each a problem that names the cause. Reports still answer from local entries, and exit `3`.
- Release writes refuse on these problems, because the drop guard checks the newest recorded cut, and a stale ledger gives the wrong one.
- `depends` and `track --forget` also refuse with exit `3` and write nothing. A forget built from a stale ledger would remove the newer statement of another machine on every machine. A `depends` removes nothing, but it prints the full list as `now requires …`, and a stale list can lack requirements from other machines. A forget with nothing stated after the pull writes nothing.
- `status` also notes how many fork entries the remote lacks, and names `ledger-sweep.log` after a failed sweep. From `ledger-sweep.json`, it reports the discarded repeats. It also reports entries that other machines sent through a repository that this machine does not use for that fork.
