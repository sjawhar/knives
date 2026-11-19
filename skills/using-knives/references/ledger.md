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
| `claim_change` | What the entry did to the claim on its subject: `claimed`, `taken`, `released` or `resumed`. Absent on an entry that changed no claim, and on every entry an older knives wrote. |
| `claim_change_why` | The reason that goes with the change: the reason the claim holds after it for `claimed`, `taken` and `resumed`. For `released`, the `--why` of the `finish`, with `--force` or without, or `knives start could not open the workspace: <cause>` where `start` gave back the claim it had just taken. Absent when the change has none. |
| `previous_owner` | The owner whose claim the entry took or released. Absent where it displaced nobody. |
| `previous_kind` | How that owner's name was resolved: `harness-session`, `workspace-derived` or `os-user`. |
| `previous_started` | When that owner's claim was taken. |
| `previous_last_seen` | The durable token of that owner's last sighting: a timestamp, `none-since-claim`, `none-within-window` or `unreadable-operation-log`. Absent where this machine cannot read that claim's activity: a claim taken on another machine, or one this config home did not record. |
| `previous_why` | That owner's own reason for the claim. |

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

A command records an event that it sees. A failed ledger write fails the command. On a fork that shares its ledger, an entry that does not reach the ledger's repository before the command exits also fails it, with exit `3` ([the sweep](#the-sweep)).

| Command | Entry |
|---|---|
| `start` | `claimed: <why>`, carrying `claim_change` `claimed`. A forced start writes `seized from <owner> (…): <why>`, carrying `taken` where another owner name held the claim and `claimed` where the caller held it itself. Over a claim commit whose holder cannot be read, it writes `seized the claim <ref> at <commit>, whose holder could not be read: <why>`. On a held claim, `resumed` or `resumed via workspace possession`, carrying `resumed`. A start that took a claim on the ledger and then could not open the workspace writes `claim released`, carrying `released`. |
| `finish` | `claim released`, with the supersession if given, carrying `claim_change` `released`. A forced release over another owner name writes `released <owner>'s claim by force (…): <why>`, and over a claim commit whose holder cannot be read, `released the claim <ref> at <commit>, whose holder could not be read, by force: <why>`. Where no claim was held, it writes `superseded by <branch>` for `--superseded-by`, ending with `: <why>` when `--why` is given, and `finished with no claim held: <why>` for `--why` alone. These carry no claim change. With neither flag and no claim, it writes nothing. |
| `track --pr/--fork-only/--forget` | The changed statement. |
| `depends --on` | `requires <owner>/<name>#<number>`, by upstream name, however `--on` spelled it. |
| `release cut` | The full parent set, the cut change id beside the commit id, and the carried-parent delta from the prior cut. It ends with `; union: <files>` when the cut wrote release logs as a union, then with `; unchecked: <reason>` when knives could not check the release in hand. |
| `release include/drop/advance/rebase` | `edited <release> as <commit> (change <id>) with <n> parent(s): <delta>; parents: …`, ending with `; union: <files>` when the write wrote release logs as a union, then with `; unchecked: <reason>` when knives could not check the release in hand. `audit` holds the release against the commits its cut and edit events name. |
| `release adopt` | An `edited` event too: `edited <release> as <commit> (change <id>) with <n> parent(s): adopted: <what changed>; parents: …`. `<what changed>` names the write it replaces and the baseline, the files resolved by hand, the files changed beyond the baseline's conflicts with the `--why`, the carriers, and each member's count of absent added lines. The evidence is the commit, then its parents. It never carries `; unchecked:`. `audit` and every reader of edit events, an older knives too, read it as the name's newest write. |
| `sync` | Tracked PRs that merged, closed, reopened or advanced. First sightings are silent. So is a transition that the ledger already holds as the newest for that PR ([sharing](#sharing-between-machines)). |

- Cut, edit and adopt events carry `parents` frontmatter: the full commit of each parent and **every** local bookmark on it. This lets a later `advance` or `include` find a member after a rebuild outside jj or an upstream merge. All names are kept, so the bookmark of another agent cannot hide the member name.
- An adopt event copies the `parents` of the write it replaces, so each member keeps the branch names it was recorded under. It adds one entry per carrier that the replaced write does not list, whose branch is the label `<release> earlier write (carrier, not a member)`, so no reader takes it for a member ([carriers](releases.md#plan-and-keep-the-composition)). Where the replaced event has no `parents` (an older knives wrote it), adopt builds the list as a write does. A cut or edit leaves carriers out of the parent set, so its `parents` name none.
- `; unchecked: W absent` says that this repository did not hold the recorded write that the release's members matched. `; unchecked: no match` says that the members matched no recorded write. `; unchecked: no record` says that the ledger recorded no write of the name. The mark is sticky: a write over a release that equals or matches a marked write repeats that write's reason. An `adopt` event ends it. While the newest write of a name carries the mark, `audit` shows a note ([reports](reports.md#knives-audit-repo---all---no-github)). What the writes print: [releases](releases.md#when-knives-cannot-check-the-release-in-hand).
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
- knives never rewrites an entry, and has no rotation. Only three things remove an entry file: `ledger migrate` (it moves the file), a commit that discards an uncommitted repeat transition of this machine (a sweep's, a send's or a claim change's), and a claim change that the ledger's repository did not take, which removes the entry it wrote ([claim refs](#claim-refs)). Readers ignore unknown frontmatter keys. There is no version field.
- Exits: `0` done, `2` usage error, `3` unreadable directory or entry, or a note that did not reach a shared ledger. A repository with no ledger yet gives `0` and `no notches yet`.
- Commands that read branch statements read the ledger of each fork they work on, at start. These are `status`, `audit`, `pushed`, `sync`, `track`, `depends`, `finish`, `start`, and a `notch` note about a branch without `--pr`.
- An entry that does not parse stops the command with exit `3` and names the file. A multi-fork `status`, `audit` or `sync` marks that fork `ledger unavailable: …` instead, exits `3`, and reports the other forks. An unforced `finish` of a claim that another identity holds still gives its refusal with exit `2`, and adds one line that says the ledger could not be read and why.
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
- `--dry-run` takes no lock, joins no ledger, pulls nothing, writes nothing, and counts from the local ledger. It holds back what the real run would hold back with no network read: every fork when the sweep would refuse the setup. It also holds back a fork whose `ledger` is the `origin` of no local git directory yet, and says that a real run joins that ledger first, then pulls. It cannot see failures that occur only during a network read or a write.

```text
{dry_run?, moved: [{from, to}], renamed: [{map, from, to}], sightings: [{from, to}],
 wrote, already, problems}
```

`moved` gives full old and new directory paths. `renamed` gives each key with its map, and `sightings` each `seen.json` key. `wrote` counts statements written, and `already` statements that were present. Every field is present, empty or `0` when nothing happened.

**Unclaimed directories.** When a fork's `upstream` changes, or two registries spell it differently, its entries stay in a directory that no entry keeps. That fork's ledger reads without them. Each such directory is a problem that names two remedies: set the right `upstream` in `repos.toml`, or move the entries to the right fork. Every pull-first command carries it, so reports exit `3` and replacing writes refuse. `notch` reads and sweeps carry it too. It blocks writes on every fork. Retire a fork only after you move or remove its directory.

## Sharing between machines

A fork's ledger travels when two things are true. Its `repos.toml` entry sets `ledger = "<owner>/<name>"` ([configuration](configuration.md#registry-fields)). A git directory over the ledger root `~/.config/knives/ledger/` has that repository as `origin`. The fork's claims then travel through that repository too ([claim refs](#claim-refs)). The machine needs read and write access. Without it, the pull fails, and every release write, `depends` and `track --forget` refuse. `start` then claims nothing and `finish` releases nothing, each with exit `3`. Every other write exits `3`, and its entries stay on this machine.

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

- knives makes such a git directory itself when a fork's `ledger` names a repository that no git directory here has as `origin`. It does this the first time a pull, a send, a claim or a sweep routes that fork, and never under `ledger migrate --dry-run`:
  - The path is `~/.config/knives/ledger-repositories/<owner>--<name>.git`: a bare git directory with `core.bare false` and `core.worktree` set to the ledger root.
  - Its `origin` is the URL of the fork's `origin` in `repos.toml`, else of its `upstream`, with the repository path replaced by `<owner>/<name>`. So the ledger is on the fork's own forge, reached with the protocol and the credentials the fork uses.
  - Its `knives.machine` is `<short host name>-<8 hex digits>`. A character that a ref name component cannot hold becomes `-`. knives chooses the name once, when it makes the directory. A directory that exists keeps its name.
    - In a harness session (an identity variable of [claim identity](coordination.md#claim-identity-and-locking) is set), the digits are the first 8 hex digits of the SHA-256 of `/etc/machine-id` (trimmed, or empty when the file is missing), a newline and the session id. So two sessions on one host that each join with a config home of their own are two machines. The same session on the same host gets the same name again.
    - In no harness session, the digits are the first 8 characters of `/etc/machine-id` when they are hex digits, else random.
  - Before knives puts the directory in place, it asks the ledger's repository for `refs/knives/<machine>`. When the repository has that ref, knives copies it into the new directory. So a config home that was deleted and made again, by the same session on the same host, is the same machine: it writes to that ref again, and `start` and `finish` of its own claims work as they did before.
  - When knives cannot ask the ledger's repository (for example, offline), it makes nothing. The command says that joining the ledger failed, with git's cause, and the next command that routes the fork tries again.
  - knives prints `joined the ledger <ledger>: made <git dir> as machine <machine>, carrying <forks>` on stderr, and the command goes on.
- knives never changes a git directory it did not make. When a directory is already at that path and its `origin` is another repository, the fork's entries travel nowhere. Each command that routes the fork says why, and names the two fixes: give that directory the ledger as its `origin`, or move it away so that knives joins the ledger itself.
- To choose the URL or the machine name yourself, make the git directory by hand, as above, before a command routes the fork. Make one for each repository that a `ledger` names.
- A machine that joins only through `ledger-repositories` needs no `ledger/` yet. The first pull or sweep makes it.
- A plain `git clone` of a ledger repository into `ledger/` is not a ledger. Its `main` is an old snapshot in the registry-key layout, and knives reads only `refs/knives/*`. The sweep and every ledger read refuse such a clone, naming a registry-key directory, until `knives ledger migrate` runs ([upgrading](#upgrading-from-an-older-knives)).

Routing:

- Each git directory carries the forks whose `ledger` names its `origin`, with case and `.git` aside. A fork goes through that one only.
- A fork with no `ledger` is not shared. A `ledger` that no `origin` names is joined, as above. When the join fails, the fork's entries travel nowhere. Every sweep and every pull that asks about the fork reports why as a problem. Every write of it exits `3`, and `start` and `finish` of its branches exit `3` and change nothing. When no fork sets `ledger`, nothing is shared, and that is not a problem.
- knives refuses these, and names the directory and the remedy:
  - two git directories with one `origin`,
  - a directory that carries forks but names no machine,
  - a `knives.machine` that is not one ref name component (no `/`, no leading `-` or `.`, no `..`), because it spells `refs/knives/<machine>`.
- Another `knives.*` key, for a newer knives, is not refused. `status` notes it and `ledger sweep` lists it under `notes`, each with the command that removes it.

### The sweep

- Every command that writes an entry sends it before it exits. An entry of a fork with no `ledger` is not sent.
- The send goes only through each git directory whose copy of the remote, as the last fetch or push left it, lacks an entry that the command wrote. When that copy holds them all, as after a claim's push, the send makes no network call. Otherwise it waits up to a minute for `ledger.lock`, then makes one pass (below) through each such git directory.
- An entry that did not reach the ledger stays on this machine. The command prints this line for each fork, with `entry` for one, and exits `3`:

  ```text
  <fork>: <n> ledger entries this command wrote did not reach the ledger <ledger> (<git dir>): <cause>; `knives ledger sweep` sends them
  ```

- A send prints the entries it could not send. Its other problems, such as a peer entry that does not parse, go to `ledger-sweep.log` and do not change the exit. When `ledger-sweep.log` itself cannot be written, the send says so in one line that starts with `!!`, and the exit does not change.
- A sweep run by hand waits up to a minute for the holder of `ledger.lock`: another sweep, or a write's send. Then it runs its own pass and reports its own result.
- For each git directory, under its `knives-transport.lock`, a pass does this:
  1. Fetch every machine ref into `refs/knives-remotes/origin/<machine>`.
  2. Write in only entries that the root lacks, as regular entry files in `<owner>/<repository>/`. A peer tree with any other path is refused before any write.
  3. Discard repeat transitions of this machine (below).
  4. Commit new entry files to `refs/knives/<machine>`, with git plumbing, never the index.
  5. Push that ref, which no other machine writes.
- A sweep repeats its pass until a pass finds nothing new.
- A failed fetch does not stop the commit and push. An entry that does not parse is a problem that names the file. It is left out of the repeat check. An uncommitted entry of this machine that does not parse is also left out of the commit. Otherwise every machine that took it could not read that fork. The other entries of the fork go.
- A machine ref only moves forward. The fetch refuses a peer ref that moved backward (rewritten, or removed and pushed again). It is a problem until someone restores it. The refs of other machines still come in.
- A fetch or push that runs over a minute is stopped. A stalled HTTP transfer fails sooner. A ledger fetch that hits a ref lock is not retried, because the transport lock already serialises knives ledger fetches and a retry would hold that lock past the minute promised to a waiting command.
- `ledger-sweep.log` beside the state file holds the errors, and exists only while the last sweep or send failed. Run `knives ledger sweep` to see what one carries:

```text
{outcome, waited?, destinations?, problems?, notes?}
waited: {pid?, seconds}
destination: {git_dir, remote, forks, commits, pulled, pushes, discarded, skipped?,
              unsent?}
```

| Field | Meaning |
|---|---|
| `outcome` | `swept`. `not-shared`: no git directory names a machine and no fork sets `ledger`. `failed`: the routing was unreadable, or the lock was still held after a minute (see `problems`). |
| `waited` | Present when it waited: the pid in `ledger.lock`, and the seconds. |
| destination | One git directory. `remote` is always `origin`. `forks` lists the upstream names it carries. The counts cover the whole sweep: commits, entries pulled, pushes, repeats discarded. |
| `skipped` | Per `<owner>/<name>`: entries that other machines sent through it for a fork that it does not carry here. This machine never writes them in. Two registries that route one fork differently cause this. |
| `unsent` | Entries of this machine, in forks it carries, that no fetched ref holds at the end. `0` when all arrived, or when nothing needed sending. Absent only when it could not be counted, and a problem says why. |

- Any problem exits `3`.
- A write's send prints none of this. So each sweep and each send also keeps discards and skipped counts in `ledger-sweep.json` beside the state file, and `status` reads it. Discards add up per fork, with the time of the newest. The skipped count per repository is what the last pull found. The file goes away when it holds nothing.

### Repeated transitions

- Each machine that syncs compares the forge with its own record, so two machines can each see `#N merged`.
- `sync` pulls first. It writes no transition that the ledger already holds as that PR's newest, with the same text and `pr` stamp. Closed, reopened and closed again is still two closes.
- When two machines each wrote it before either pulled, the second to send it removes its own uncommitted copy before its commit. This counts in `discarded`.
- knives never removes a committed entry, so a repeat that both machines committed stays (`repeated_transitions`, `src/ledger/transitions.rs`).

### Which commands pull first

- Pull first: `status`, `sync`, `audit`, `pushed`, `depends`, `track --forget`, and release commands that read the ledger. Those are the plan, `cut`, `rebase`, `include`, `drop`, `advance`, `adopt`, and `members` without `REF` (it reads the ledger to name the release in hand).
- `members <REF>`, with or without `--verify`, pulls nothing. It reads only the local ledger, to label carriers. `--carries` and `--census` read no ledger and pull nothing.
- `reap` pulls nothing and reads the local ledger: the commits it records knives writing under each superseded name, and their rewrites, count for the keep rule and are cleaned up after it. A `cut` reads the ledger, too, to name the workspaces left from branches it records.
- No pull before they decide: `notch`, `start`, `finish`, `track --pr`, `track --fork-only`. They build nothing from the entries they read. They read a statement only to stamp their entry with a PR number, and to stamp theirs after the live one.
- On a fork that shares its ledger, `start` and `finish` read the branch's claim ref from the ledger's repository before they decide ([claim refs](#claim-refs)). That read takes no entries.
- The send of every write pulls during its pass, after the command has decided ([the sweep](#the-sweep)).
- A failed pull, a `ledger` that could not be joined, and a refused setup are each a problem that names the cause. Reports still answer from local entries, and exit `3`.
- Release writes refuse on these problems, because the drop guard checks the newest recorded cut, and a stale ledger gives the wrong one.
- `depends` and `track --forget` also refuse with exit `3` and write nothing. A forget built from a stale ledger would remove the newer statement of another machine on every machine. A `depends` removes nothing, but it prints the full list as `now requires …`, and a stale list can lack requirements from other machines. A forget with nothing stated after the pull writes nothing.
- `status` also notes how many fork entries the remote lacks, and names `ledger-sweep.log` after a failed sweep or send. From `ledger-sweep.json`, it reports the discarded repeats. It also reports entries that other machines sent through a repository that this machine does not use for that fork.

### Claim refs

On a fork whose `ledger` is set, the claim of a branch is a ref on the ledger's repository (`src/ledger_git/claims.rs`). Every machine that shares the ledger reads the same ref, so the claim holds on each of them. It stays after the machine that took it is gone, until a `finish` or a `start --force --why` changes it. Who may take, resume or release it: [coordination](coordination.md#claim-identity-and-locking).

The ref:

- It is `refs/knives-claims/<owner>/<name>/<branch>`. `<owner>/<name>` is the fork's lowercase upstream name.
- The branch is one ref component: `%` is written `%25`, `/` is written `%2F`, and every other byte stays. So `feat/alpha` is `feat%2Falpha`, and the refs of `a` and `a/b` never collide.
- `start` refuses a branch with exit `2`, before it reads the claim, when the whole claim ref is longer than 255 bytes or `git check-ref-format` refuses the ref. GitHub refuses every ref longer than 255 bytes, so the ledger's repository could never hold that claim. The prefix and `<owner>/<name>/` count too: for the fork `acme/demo`, 226 bytes are left for the encoded branch. No claim of such a branch can exist, so its `finish` is that of a fork with no `ledger`.
- The ref is on the remote only. knives writes no local ref under `refs/knives-claims/`. A read runs `git ls-remote origin <ref>`, then fetches the objects of that one commit, under the git directory's `knives-transport.lock`.

The record:

- The ref holds a commit with no parent. Its tree holds one regular file, `claim.toml`.
- Its keys are `repo` (the upstream name), `branch`, `owner`, `kind` (`harness-session`, `workspace-derived` or `os-user`), `why`, `started` (RFC 3339 UTC), `place` (absent when not recorded), `machine` (the `knives.machine` that took it) and `entry` (the file name of the entry that recorded the change).
- Author and committer are the machine, with an empty email. The message is `<machine>: claim <repo> <branch>`.
- A reader ignores a key that it does not know. A commit whose `claim.toml` does not read is still a claim. `start` refuses it with exit `2`, and names the ref, the commit and why the holder cannot be read. `start --force --why` replaces it, and `finish --force --why` deletes it.
- The record is in the claim commit, so entries gain no kind and no key, and `ledger-sweep.json` keeps its schema.
- `state.json` keeps a copy of each ledger claim that this machine took or resumed, with two more keys: `machine` and `ledger_commit` (the claim commit). `status`, the session hook and preflight read the copy, so they list this machine's claims only. The copy decides nothing. Each `start` and `finish` of the branch replaces it with the ledger's claim, or removes it.

The push:

- A take, a seize, a restate (the holder's `start --why`) and a release are each one `git push --atomic`. It moves the claim ref and this machine's `refs/knives/<machine>` together, and that ref carries the entry that records the change. The two reach the remote together or not at all.
- The push is leased (`--force-with-lease=<ref>:<commit>`) on the claim commit that the command read, or on no ref for a take. When another machine changed the claim after the read, the remote refuses the push and nothing changes.
- A `start` that lost that race reads the claim again. It prints `<owner> took <branch> on machine <machine> while this start ran`, then the refusal, and exits `2`. A `finish` that lost it names the claim that the ledger holds now, releases and removes nothing, and exits `1`.
- The command holds the fork lock, then takes `ledger.lock`, then the git directory's `knives-transport.lock`, and waits up to a minute for each. A sweep takes the last two in the same order.

When the push fails:

- The remote cannot have applied the push when it refused it, or when git stopped before the remote received it. knives then moves this machine's ref back, deletes the entry and exits `3`. `start` prints `nothing was claimed: …` and `finish` prints `nothing was released: …`, each with the ledger, the git directory and git's cause.
- The remote may have applied the push when the one-minute deadline stopped git, or when the connection broke after the remote received it. knives then asks the remote for both refs. When the remote holds this machine's new ref, the push landed.
- Otherwise knives puts nothing back. This machine's ref keeps the entry, and `state.json` stays as it was before the command. The command says that whether the push landed is unknown, and exits `3`. The next `knives start <branch>` or `knives finish <branch>` reads the claim ref again, and the next send pushes the entry either way. A command that is killed during the push leaves the same.
- So the ledger can hold a `claimed` or `released` entry for a push that did not land. An entry decides nothing. Only the claim ref decides.

What older releases see:

- Nothing. Every release fetches only `refs/knives/*`, and lists refs only under `refs/knives/` and `refs/knives-remotes/`. A claim ref is outside both, so the pull and the sweep of an older knives work on this ledger as before.
- An older knives does not read claim refs. It takes its claims in its own `state.json`, and a claim on the ledger does not refuse it. So a claim holds on a machine only while that machine runs a knives that reads claim refs.
- A claim in `state.json` with no `ledger_commit` is a local claim: an older knives took it, or it was taken before the registry named a ledger. It refuses another owner on its own machine only.
- An older knives that saves `state.json` drops `machine` and `ledger_commit` from a copy. This knives then reads the copy as a local claim of the same owner, and the ledger's claim still decides.
- knives 3.2.6 and every later release keep their pull and their sweep working on a ledger that holds claim refs. Releases before 3.2.6 are not covered.
