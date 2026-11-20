# Claims on a fork that shares its ledger

A fork whose `repos.toml` entry sets `ledger` keeps each branch's claim on that ledger's
repository, so the claim holds on every machine that shares the ledger. How the ledger itself
travels: the README, [What it remembers](../README.md#what-it-remembers). The claim commit, the
push that changes it, its locks, the state file's copy of it and the refusal of a claim whose
activity cannot be read here: [design.md, `knives start`](design.md#knives-start). Who a claim's
owner is: [design.md, Harness adapters](design.md#harness-adapters). This file holds the rest.

## What decides

- A branch's claim is the ref `refs/knives-claims/<owner>/<name>/<branch>` on the ledger's
  repository, `<owner>/<name>` being the fork's upstream name in lowercase. The branch is one ref
  component: `%` is written `%25` and `/` `%2F`, so `feat/alpha` is `feat%2Falpha`. knives writes
  no local ref under `refs/knives-claims/`.
- A claim outlives the machine that took it, until a `finish` releases it or a
  `start --force --why` seizes it.
- A knives older than claim refs reads none: it takes claims in its own `state.json`, a claim on
  the ledger does not refuse it, and its pull and sweep work as before, since no claim ref is under
  `refs/knives/`.
- `state.json` keeps a copy of each ledger claim this machine took or resumed, with its `machine`
  and `ledger_commit`; a local claim is one with no `ledger_commit`. What the copy and a local claim
  decide, and what reads the copy: [design.md](design.md#knives-start). Each `start` that takes,
  seizes or resumes the claim writes the copy afresh, and the `finish` that releases it removes it.
- `start` takes the branch only when neither the ledger's claim nor a local claim is present, and
  `start` and `finish` go on only when every claim present lets the caller.
- `start` refuses (exit `2`), before it reads the ledger, a branch whose whole claim ref would be
  over 255 bytes, which GitHub refuses as a ref, or that `git check-ref-format` refuses. For the
  fork `example/fork`, 223 bytes are left for the encoded branch.

## The machine of a claim

- A ledger claim's machine is the `knives.machine` of the ledger git directory that took it; a
  local claim's is this machine. Two config homes with one `knives.machine` are one machine.
- `knives.machine` is one ref name component of at most 243 bytes: it spells
  `refs/knives/<machine>`, and GitHub refuses a ref over 255 bytes. A git directory naming a longer
  one is refused, with the command that names the machine again.
- A ledger claim records the place it was taken in, so its own holder's `finish` on another host
  refuses as the README's [`finish` row](../README.md#commands) says, and
  `finish --force --why "<reason>"` releases it.

## Refusals

- Another owner's claim refuses `start` (exit `2`), naming its machine. Where this machine cannot
  read the claim's activity, the refusal is:

  ```text
  <repo>/<branch> is claimed by <owner> (<kind>) on machine <machine>, taken <started> (<age>): <why>
  its activity cannot be read from this machine (<this machine>): its sightings and its processes are on machine <machine>
  use `knives start <branch> --force --why "…"` to seize the claim
  ```

  `<age>` reads like `12m ago`, and `just now` under a minute. In another config home of the
  claim's machine, the second line is
  `its activity cannot be read here: this config home did not record that claim, which was taken in <place>`.
  Where this machine can read the activity (a local claim, or one taken on this machine that this
  config home recorded), the refusal gives the last sighting and the workspace, process and place
  lines instead. In each of these refusals, where the claim and the caller are both anonymous
  (`os-user`, as [design.md](design.md#harness-adapters) says), the first line begins
  `both sides are anonymous identities, so they can never match; `.
- `start --force --why "…"` seizes the claim, whoever holds it, on whatever machine. The new claim
  is the caller's, taken now, in this place, on this machine.
- `finish` names another owner's claim as `start` does, with its machine.
- A claim commit whose `claim.toml` does not read, or is over 64 KiB and so is not read, is still a
  claim. `start` and `finish` refuse it (exit `2`), naming the ref, the commit and why.
  `start --force --why` replaces it; `finish --force --why` deletes it.

## The record and the push

- `claim.toml` has the keys `repo`, `branch`, `owner`, `kind`, `why`, `started` (RFC 3339, UTC),
  `place` (absent when not recorded), `machine` and `entry`, the file name of the ledger entry that
  recorded the change. Readers ignore unknown keys.
- Where another config home changed the claim after the command read it, the leased push changes
  nothing:
  - `start` prints `<owner> took <branch> on machine <machine> while this start ran`, then the
    refusal, and exits `2`. Where the claim that won is the caller's own, taken under its owner
    name in another config home, it prints
    ``nothing was claimed: <owner>, the owner of this start, took <branch> in another config home, on machine <machine>, while this start ran; `knives start <branch>` resumes that claim``,
    and where the claim was released meanwhile, it says that `knives start <branch>` takes it;
    each of these two exits `3`.
  - `finish` prints
    `nothing was released: the claim of <branch> changed on the ledger while this finish ran`,
    then the claim the ledger holds now, or that it holds none, and exits `1`.
- A `start` that took the claim and then could not open the workspace or save `state.json`
  releases it again, with the reason `knives start could not open the workspace: <cause>`, and
  exits `3`. Where that release cannot be sent, it says that the claim is held on the ledger with no
  workspace and that `knives finish <branch>` releases it.

## When the ledger cannot be read or written

- `start` prints `nothing was claimed: …` and `finish` prints `nothing was released: …`, each with
  the ledger, its git directory and git's account of why, and exits `3`. Where its push failed,
  `finish` also says to run `knives finish <branch>` again.
- A claim push the remote refused or never received is undone: this machine's ref moves back, and
  the entry is deleted. Where it may have landed and asking the remote does not settle it, knives
  puts nothing back, says that whether it landed is unknown, and exits `3`; the next `start` or
  `finish` of the branch reads the claim again, and a send pushes the entry. So an entry can record
  a claim change that did not land: only the claim ref decides.
- Where the ledger holds no claim of the branch now, `finish` of this machine's copy prints
  ``the ledger holds no claim of <branch> now; it was released or taken on another machine; run `knives ledger sweep` then `knives notch <branch>` to read by whom``,
  removes the copy, disposes of the workspace, and exits `1` at least. Where the ledger holds
  another owner's claim in place of the caller's copy, `finish` names that holder, removes only the
  copy, offers `knives start <branch> --force --why "…"`, and exits `1`.

## Joining a ledger

- A command that joins a ledger ([design.md](design.md#knives-start) says which) prints
  `joined the ledger <ledger>: made <git dir> as machine <machine>, carrying <forks>` on stderr.
  `knives ledger migrate --dry-run` joins none.
- The git directory it makes is `~/.config/knives/ledger-repositories/<owner>--<name>.git`,
  `<owner>/<name>` being the ledger's. Its `origin` is the fork's `origin` URL in `repos.toml`, else
  its `upstream`'s, with the repository path replaced by `<owner>/<name>`.
- Its `knives.machine` is `<short host name>-<8 hex digits>`. In a harness session (one of the
  [harness session variables](design.md#harness-adapters) is set and not blank), the digits are
  the first 8 of the SHA-256 of `/etc/machine-id` and the session's owner name, so two sessions on
  one host, each with its own config home, are two machines. In no harness session, the digits are
  random, drawn once per config home and drawn again where the ledger's repository already holds
  `refs/knives/<machine>`, which is another config home's ([design.md](design.md#knives-start)).
  A git directory made by hand or by an installer keeps the `knives.machine` it was given; a join
  in another config home picks random digits, so the two cannot share a name.
- A git directory set up by hand before the first join chooses the URL and the machine name: knives
  joins only a ledger no git directory has as its `origin`, and never changes a git directory it did
  not make. One already at the join's path with another `origin` is a problem naming both fixes:
  give it the ledger as its `origin`, or move it away.
- When the ledger's repository cannot be asked, nothing is made, and the command names the cause.
