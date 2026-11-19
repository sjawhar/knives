# Claims on a fork that shares its ledger

Read this before `start` or `finish` on a fork whose registry entry sets `ledger`, when a claim refusal names a machine, when `start` or `finish` exits `3` naming a ledger, and when a command prints `joined the ledger`. Who may take, resume or release a claim anywhere: [coordination.md](coordination.md#claims-start-and-finish).

## What decides

- A branch's claim is the ref `refs/knives-claims/<owner>/<name>/<branch>` on the ledger's repository, `<owner>/<name>` being the fork's lowercase upstream name. The branch is one ref component: `%` is written `%25` and `/` `%2F`, so `feat/alpha` is `feat%2Falpha`. knives writes no local ref under `refs/knives-claims/`.
- `start` and `finish` read it under the fork lock before they decide, so it binds every machine that shares the ledger and runs a knives that reads claim refs. It outlives the machine that took it, until a `finish` or a `start --force --why` changes it.
- An older knives reads no claim ref: it takes claims in its own `state.json`, a ledger claim does not refuse it, and its pull and sweep work as before.
- `state.json` keeps a copy of each ledger claim this machine took or resumed, with `machine` and `ledger_commit`. `status`, the session hook and preflight read only the copies, so they list this machine's claims, and a copy can be out of date. A copy decides nothing: each `start` and `finish` of the branch replaces or removes it.
- A claim in `state.json` with no `ledger_commit` is a local claim: an older knives took it, or it was taken before the registry named a ledger. It binds this machine only. `start` weighs both kinds: it takes the branch only when no claim is present, and resumes only when every claim present lets you.
- `start` refuses (exit `2`), before it reads anything, a branch whose whole claim ref is over 255 bytes or that `git check-ref-format` refuses: GitHub refuses every longer ref. For the fork `acme/demo`, 226 bytes are left for the encoded branch.

## The machine of a claim

- It is the `knives.machine` of the ledger git directory that took the claim; a local claim is this machine's. Two config homes with one `knives.machine` are one machine.
- Standing in the claimed workspace resumes the claim only on its machine: a workspace at the same path on another machine is another directory.
- A claim taken on another machine was taken in another place, so even your own is finished only with `finish --force --why "<reason>"`.

## Refusals

- Another owner's claim refuses `start` (exit `2`), naming its machine:

  ```text
  <repo>/<branch> is claimed by <owner> (<kind>) on machine <machine>, taken <started> (<age> ago): <why>
  its activity cannot be read from this machine (<this machine>): its sightings and its processes are on machine <machine>
  use `knives start <branch> --force --why "…"` to seize the claim
  ```

  Where this machine can read the claim's activity (a local claim, or a claim taken on this machine that this config home recorded), the refusal gives its last sighting and the workspace, process and place lines instead. In another config home of the claim's machine, the second line is `its activity cannot be read here: this config home did not record that claim, which was taken in <place>`. An age under a minute reads `just now`.
- `start --force --why "…"` seizes the claim, whoever holds it, on whatever machine. The new claim is yours, taken now, here.
- `finish` names another owner's claim as `start` does, with its machine.
- A claim commit whose `claim.toml` does not read, or is over 64 KiB and so is not read, is still a claim. `start` and an unforced `finish` refuse it (exit `2`), naming the ref, the commit and why. `start --force --why` replaces it; `finish --force --why` deletes it.

## The record and the push

- The claim ref holds a commit with no parent whose tree is one file, `claim.toml`, with the keys `repo`, `branch`, `owner`, `kind`, `why`, `started` (RFC 3339 UTC), `place` (absent when not recorded), `machine` and `entry` (the file name of the entry that recorded the change). Readers ignore unknown keys.
- A take, a seize, a restate (the holder's `start --why`) and a release are each one `git push --atomic` of the claim ref and `refs/knives/<machine>`, which carries the entry that records the change. It is leased on the claim commit the command read (on none for a take), so when another machine changed the claim since, nothing changes:
  - `start` prints `<owner> took <branch> on machine <machine> while this start ran`, then the refusal, and exits `2`. Where the winner is your own owner name in another config home, it says that `knives start <branch>` resumes it, and exits `3`.
  - `finish` names the claim the ledger holds now, releases and removes nothing, and exits `1`.
- A `start` that took the claim and then could not open the workspace or save `state.json` releases it again, with the reason `knives start could not open the workspace: <cause>`. When that release cannot be sent, it says that the claim is held with no workspace and that `knives finish <branch>` releases it.

## When the ledger cannot be read or written

- `start` prints `nothing was claimed: …` and `finish` prints `nothing was released: …`, each with the ledger, the git directory and git's cause, and exits `3`. `start` records no claim and opens no workspace. `finish` leaves `state.json` and the workspace as they were: run it again.
- A push the remote refused, or never received, is undone: this machine's ref moves back and the entry is deleted. When it may have landed and asking the remote does not settle it, knives puts nothing back, says that whether it landed is unknown, and exits `3`; the next `start` or `finish` of the branch reads the claim again, and the next send pushes the entry. So an entry can record a claim change that did not land: only the claim ref decides.
- Where the ledger holds no claim of the branch now, `finish` of your copy prints ``the ledger holds no claim of <branch> now; it was released or taken on another machine; run `knives ledger sweep` then `knives notch <branch>` to read by whom``, removes the copy and disposes of your workspace (exit `1` at least). Where it holds another owner's claim, `finish` names that holder, removes only the copy, offers `knives start <branch> --force --why "…"` and exits `1`.

## Joining a ledger

Where no git directory has a fork's `ledger` as `origin`, the first pull, send, `start` or `finish` that routes the fork makes one and prints `joined the ledger <ledger>: made <git dir> as machine <machine>, carrying <forks>` on stderr. A sweep joins every ledger `repos.toml` names; `ledger migrate --dry-run` joins none. To choose the URL or the machine name, set the git directory up by hand first ([ledger.md](ledger.md#sharing-between-machines)).

- It is `~/.config/knives/ledger-repositories/<owner>--<name>.git`. Its `origin` is the fork's `origin` URL in `repos.toml`, else its `upstream`'s, with the repository path replaced by `<owner>/<name>`.
- Its `knives.machine` is `<short host name>-<8 hex digits>`. In a harness session (an owner variable of [coordination](coordination.md#claims-start-and-finish) is set), the digits hash `/etc/machine-id` and the session id: two sessions on one host, each with its own config home, are two machines, and a session that joins again on the same host from a new config home is the same machine and keeps its claims. In no harness session, they are the first 8 characters of `/etc/machine-id` when those are hex, else random, and random when the ledger's repository already holds that ref.
- knives asks only the repository, so a config home set up by hand that has not pushed yet can share a joined name. The second of the two to push is refused as another writer of that ref, with `git --git-dir=<git dir> config knives.machine <unique-name>` to rename it.
- When the repository cannot be asked, nothing is made: the command names the cause, and the next one tries again. knives never changes a git directory it did not make: one already at that path with another `origin` is a problem naming both fixes.
