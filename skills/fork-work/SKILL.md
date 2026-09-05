---
name: fork-work
description: Check knives before working in a repository we maintain a fork of. Make sure to use this skill whenever you are about to change, fix, implement, refactor or test anything in such a repository, and equally when you are only reading or investigating one — tracing how it works, finding where something is implemented, reviewing its history. Also use it before cloning or re-cloning one of these projects or making any scratch or temporary checkout of it, and when asked which branch to use, whether another agent is working somewhere, or how to get a working copy. These repositories are shared with other agents and coordinated by the knives CLI, so improvising a checkout costs real work; consult this first even when the request sounds like ordinary coding or plain code reading.
---

# About to work in a fork

## Stop and find out where you are

Reading counts, not just writing. Investigating how one of these projects works is the
moment you most want to know that a fork exists, that it carries changes upstream does not
have, and that another agent may be mid-change in it — because conclusions drawn from the
wrong copy are wrong quietly.

```
knives repos
```

If the repository you are in, or the one you were about to clone, is in that list, it is
a managed fork. It is shared with other agents, it has an upstream you may be
contributing to, and there is already a right way to get a working copy of it.

A row reading `not on this machine` is a scan miss, not proof of absence: the scan of `~`
found no checkout here (three levels deep, jj checkouts only, dot-directories and symlinks
skipped). Check deeper or elsewhere before cloning — every command except `knives repos`
binds a checkout as soon as you stand inside it, wherever it is, so `cd` into the suspected
directory and run `knives status`. A row naming two checkouts is a refusal: knives will not
choose between them, and neither should you without asking.

If it is not in that list, this skill does not apply — carry on normally. If a repository should be managed but is unregistered, run `knives register` inside it and hand the snippet to the human to paste into `repos.toml`; `already registered as <name>` means it is in the list under that name. Do not edit `repos.toml` yourself.

## Then find out what is going on in it

```
knives status
```

Run it from inside the repository; it needs no argument. It reports every branch, its
pull request and that pull request's state, whether the branch is in the upstream trunk,
and which branches other agents are holding right now. It reports facts and does not tell
you what to do; the judgment is yours.

Read the claims before you touch anything. If another agent holds the branch you were
about to work on, that is a collision, and the cost of discovering it later is their work
or yours.

## Then read the notches

```
knives notch
```

What agents did and decided here lately: claims taken and handed back, pull requests
stated, dependencies recorded, releases cut, and whatever anyone thought worth writing
down. Before you touch a branch you do not understand, ask about that branch:

```
knives notch <branch>
```

Every entry carries the branch's tip at the time it was written. That is the part to read
carefully: an entry saying "superseded by #1157" at a commit the branch has since moved
past is a reason to re-check, not a conclusion to inherit. A weird branch nobody can
explain is exactly what this answers, and the reason it exists is usually one line long.

When you make a call worth remembering — this is superseded, the owner parked it, you
promised a reviewer something, you re-homed a pull request onto another branch — record it
before you move on:

```
knives notch <branch> -m "what you decided and why" --evidence <commit-or-ref>
```

Cite something. Every audit claim that survived review cited a commit or a `file:line`;
every false one did not.

A branch's notes may carry a workflow's own prefixes; the workflow that wrote them defines
them, and a note that says it is open is open.

## Get your own working copy the managed way

```
knives start <branch> --why "what you are doing"
```

This claims the branch and creates a jj workspace for it: on the branch's own tip when the
branch already exists (your `@` is an empty child of it), or on the release's shared base
(the fetched upstream trunk when no release exists) for a new branch, so it composes into
the release without forcing a rebase. A `start` that pauses is waiting for another agent's
knives command to release the claim lock; let it. A refusal names the holder — the
`using-knives` skill has the messages. As soon as your active work there stops —
including when it now waits on something external, such as a pull request in review:

```
knives finish <branch>
```

Which hands the claim back and removes the workspace so another agent can pick the branch
up. Nothing is lost — jj snapshots a working copy into a commit, so the work is in the
repository and reachable by change id, and the branch, its bookmark, and any open pull
request all survive the release.

## What not to do instead

These are the improvisations that cost work in a shared repository, and every one of them
has a knives command that does the same job safely:

- **Do not clone the repository again**, into `/tmp` or anywhere else. There is already a
  checkout, and a second one has its own bookmarks that will diverge from the first.
- **Do not create a scratch or temporary checkout** to "just try something". Use
  `knives start` and get a real workspace that other agents can see you are using.
- **Do not start a branch in the default workspace of a managed fork.** Another agent may
  be working there. `knives start` gives you your own workspace, based where the section
  above says, so you never inherit a release merge as a parent by accident. This is about
  these shared forks specifically; branching normally in your own projects is fine.
- **The two sanctioned moves.** A branch that needs a newer base moves one of two ways, and
  which one depends on whether the branch is a release member (`knives release members` lists
  the parents of the release in hand; a branch named there is a member). A member moves with
  `knives release rebase`: every member and the release together, so the composition stays
  whole. A lone branch moves with `jj rebase -b <branch> -d <trunk>@upstream`, which keeps its
  change ids so `knives release advance <branch>` follows it — and, because `-b` rebases
  descendants, also carries any release merge built on it, which is expected: the release
  follows its member, and superseded cuts are `knives release reap`ed. Never `jj duplicate` a
  branch: it mints new commit ids the release cannot match to the branch (`knives release
  advance --from` is the repair, not the plan). Never keep two copies of a branch — a
  "release-lineage" or "sibling" branch carrying a pull request's content on an older base so
  the release can carry it while the pull request branch is rebased for the maintainer. One
  branch is both the release member and the pull request head; if it does not compose into the
  release, the release is behind: move the release, do not fork the branch.
- **Do not build a branch on top of a release merge.** A branch whose history carries a
  release cut carries every member of that cut, and a pull request from it asks the
  maintainer to review the whole fork. `knives status` and `knives preflight` report it as
  `stacked-history`; `jj rebase -b <branch> -d <trunk>@upstream` fixes it and keeps the change ids.
- **Do not `jj op restore`.** It discards other agents' operations along with your own
  mistake.
- **Do not push to `upstream`.** Contributions go through a pull request.

## Where the detail lives

This is the on-ramp. For the rest of the CLI — what the three remotes mean, stating a
pull request that inference cannot find, recording that one branch cannot land before
another, planning and cutting releases, JSON output — read the `using-knives` skill.
