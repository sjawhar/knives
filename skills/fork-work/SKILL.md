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

If it is not in that list, this skill does not apply — carry on normally. If a repository should be managed but is unregistered, run `knives register` inside it and hand the snippet to the human to paste into `repos.toml`. Do not edit `repos.toml` yourself.

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

## Get your own working copy the managed way

```
knives start <branch> --why "what you are doing"
```

This claims the branch and creates a jj workspace for it, based on the fetched upstream
trunk. When you are done:

```
knives finish <branch>
```

Which hands the claim back and removes the workspace. Nothing is lost — jj snapshots a
working copy into a commit, so the work is in the repository and reachable by change id.

## What not to do instead

These are the improvisations that cost work in a shared repository, and every one of them
has a knives command that does the same job safely:

- **Do not clone the repository again**, into `/tmp` or anywhere else. There is already a
  checkout, and a second one has its own bookmarks that will diverge from the first.
- **Do not create a scratch or temporary checkout** to "just try something". Use
  `knives start` and get a real workspace that other agents can see you are using.
- **Do not start a branch in the default workspace of a managed fork.** Another agent may
  be working there. `knives start` gives you your own workspace and bases it on the
  fetched upstream trunk, which also avoids silently inheriting a release merge as a
  parent. This is about these shared forks specifically — branching normally in your own
  projects is fine.
- **Do not `jj op restore`.** It discards other agents' operations along with your own
  mistake.
- **Do not push to `upstream`.** Contributions go through a pull request.

## Where the detail lives

This is the on-ramp. For the rest of the CLI — what the three remotes mean, stating a
pull request that inference cannot find, recording that one branch cannot land before
another, planning and cutting releases, JSON output — read the `using-knives` skill.
