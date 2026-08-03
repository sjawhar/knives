# Configurable fork shape: trunk, fixed release branch, remote-role reporting, trust rules, status table

One PR. Five pieces, one theme: knives currently hardcodes the shape of a fork
(trunk named `main`, dated `release/` cuts, per-repo trust registration) and
reports what it finds in a form that is hard to read. This design makes the
shape configuration and the report a table.

Issue #4 (release cut must never lose content) is deliberately excluded: it
needs empirical work against the repositories where content actually vanished.
This design keeps the seams it will need (see "Previous release position"
below) but implements none of its checks.

## 1. Trunk is configuration, not the string "main"

**Bug:** `src/commands/status.rs` defines `TRUNK: &str = "main"` and
`UPSTREAM_TRUNK: &str = "main@upstream"`. `knives start` cannot open a
workspace in a fork of a repo whose trunk is `dev` (opencode), and every
probe measures against a branch that may not exist.

**Decision — one field, not two.** `RepoEntry.base` already exists ("the
branch upstream expects pull requests against", default `"main"`). The branch
we fork from, measure landed against, and target PRs at are the same branch in
every repo we know of. `base` keeps its name and default; its documented
meaning widens to "upstream's trunk". No new config field for this.

**Changes:**

- Delete both constants. Add accessors on `RepoEntry`:
  - `trunk()` → `&str` (the `base` value, default `"main"`)
  - `upstream_trunk()` → `String` (`"{base}@upstream"`)
- Thread the entry's values through every current use:
  - `status.rs`: `maintained_branches` and `divergent_rows` trunk exclusion,
    `carried_findings` exclusion, the fork-point revset in
    `add_branch_overlap_findings`, the landed probe in `landed_verdict`.
  - `start.rs`: workspace base and its output message.
  - `release.rs`: trunk resolution and member collection (the hardcoded
    `"main"` at line ~97).
  - `preflight.rs`: the hardcoded `"main"` at line ~291.
  - `main.rs`: the two sites defaulting a reference to `UPSTREAM_TRUNK`.
- Pure helpers gain a trunk parameter instead of reading a constant. Call
  sites already hold the `RepoEntry`.

**Result:** with `base = "dev"` in the opencode entry, `knives start` bases
workspaces on `dev@upstream` and no code path knows the name "main".

## 2. Fixed release branch (issue #2)

Some forks keep one integration branch (e.g. `sami`) that gets rebuilt and
pushed, instead of dated `release/YYYY-MM-DD` cuts.

**Config:**

- New optional field `release_branch: Option<String>` on `RepoEntry`.
  Absent = dated scheme, exactly today's behavior.
- New accessor `release_scheme()` → `ReleaseScheme::Dated |
  Fixed(BranchName)`. All release-aware code matches on this enum, never on
  the raw option, so the compiler forces every site — present and future — to
  answer "what does this mean under the fixed scheme?".
- Parse-time validation: `release_branch` equal to `base` or starting with
  `release/` is rejected with a message. A release branch shadowing the trunk
  or the dated namespace would corrupt every downstream check.

**Fixed-scheme semantics, site by site (the sites issue #2 lists):**

- **Cut:** rebuild the flat octopus (trunk + member tips; merge rules
  identical to dated) and point the fixed bookmark at it, then push to the
  release remote. `include`/`drop` = same rebuild, bump in place.
- **Previous release position:** the release remote's view of the fixed
  branch (`sami@origin` or `sami@release`) *before the push* is the previous
  release. Captured from the remote-tracking ref at cut time; no new state.
  This is the seam issue #4's pre/post-cut checks will attach to.
- **`is_our_release`:** under Fixed, the branch with the fixed name, local or
  on origin/release remotes. Upstream's refs never count (unchanged rule).
- **Release scanning (`releases_to_scan` / stale parents):** the candidates
  are local `sami` and its remote counterpart. No date ordering, no
  superseded-count note.
- **Carried-elsewhere exclusion, `maintained_branches`, `divergent_rows`:**
  exclude the fixed branch exactly as `release/*` is excluded today — it is a
  cut, not a branch of ours.
- **Consumer pin scan:** a consumer pins the fixed branch by name, so the
  check is "is the consumer's pinned commit the tip of the fixed branch, or
  behind it" — commit-on-branch. This sidesteps the cross-repo dated-name
  collision bug for fixed-scheme repos.
- **Future-dated-name check:** not applicable under Fixed. Skipped silently.
- **Landed probe:** untouched; it measures upstream trunk (part 1).

**Implementation shape (chosen over alternatives):** a `ReleaseScheme` enum
derived from config, matched exhaustively at each site. A trait was rejected
(two schemes don't earn dynamic dispatch and it hides per-site decisions);
inline `if release_branch.is_some()` checks were rejected (scatters the
decision, nothing stops future call sites from silently assuming dated).

## 3. Remote-role reporting gaps (issue #5)

Three contained fixes:

1. **Miswired-origin heuristic in `init`:** warn when origin's URL owner
   matches a consumer or org owner while an untracked remote points at a
   personal fork of upstream. Init output always states the convention:
   origin = the personal fork branches push to, upstream = the maintainer
   repo.
2. **PR inference from personal-fork heads:** match open upstream PRs whose
   heads live under the origin remote's owner, not only same-repo heads.
   Removes the manual `track --pr` workaround.
3. **Consumer pin scan source:** read the pin from the consumer's trunk at
   its origin, not the working copy. When the checkout lags its own origin,
   annotate the finding ("checkout is N commits behind its main") instead of
   producing a false BEHIND.

## 4. Trust rules replace per-repo registration (issue #3, reframed)

**Lifecycle fact that reshaped this:** there is no long-lived process. The
OpenCode plugin spawns the `knives` binary per hook event; each invocation
loads `repos.toml` fresh (`src/commands/hook.rs`). Claude Code hooks work the
same way. An edit to `repos.toml` takes effect on the next tool call. So
"approval" *is* a human editing the file — a pending/approve state machine
would be theater, since any agent-writable pending store proves nothing and
the file itself is the trust root.

**The real problem** is churn: workspaces under `~/agent-c/*/*` are created
and destroyed constantly, and none of them get AGENTS.md guidance because
per-repo registration cannot keep up.

**Design — declarative trust rules, written once by the human:**

```toml
[trust]
roots = ["~/agent-c"]                          # any repo under here is mine
owners = ["some-user", "some-org"]              # any repo whose upstream/origin
                                                 # is owned by these
```

- At hook time, when a touched path is inside no registered repo root, walk
  up to the containing repo root (a directory holding `.jj` or `.git`).
- The repo is a trusted guidance root if (a) it sits under a `roots` subtree,
  or (b) its remotes' URL owners match an `owners` entry.
- Owner checks read git remote configuration (no network). Result is cached
  in per-session state (`src/hook/state.rs`) so remote parsing does not run
  on every tool call; `roots` containment is a path check and needs no cache.
- Matched repos behave like `[trusted.*]` entries: guidance injection only,
  invisible to fork commands. Existing `[trusted]` entries remain for
  one-offs.
- Paths in `roots` expand like all registry paths (`expand_registry_path`).

**`knives register` (the rump of issue #3):** a command that inspects the
current repo's remotes and prints a ready-to-paste `[repos.<name>]` snippet
with detected upstream/origin. It writes nothing. An agent that wants a fork
registered mid-session shows the human the snippet; the human pastes it; the
next tool call picks it up. No pending state, no approve command, no MCP.

## 5. `knives status` renders branches as a table

The non-JSON branch listing has vertical alignment but no horizontal
alignment; rows are unreadable past a few branches.

- Branch rows become a real table: fixed column order (branch, tip,
  push-state, PR, review, checks, landed, flags), widths computed from
  content, one row per branch, header row.
- Empty cells render as `-` or blank, not omitted, so columns stay aligned.
- Findings, claims, notes, problems keep their current grouped-line format.
- JSON output is untouched.

## Testing

- **Unit:** scheme derivation from config (absent/present/invalid);
  fixed-branch exclusion in `maintained_branches`, `divergent_rows`,
  carried-elsewhere; `is_our_release` under Fixed; pin scan against a fixed
  branch; config validation rejections; trust-rule matching (roots
  containment incl. the sibling-prefix trap, owner matching from remote
  URLs); PR head matching from the origin owner; table rendering (alignment
  with mixed-width content, empty cells).
- **Lab/integration (existing lab-repo harness in `tests/`):**
  - a repo whose trunk is `dev`: `start`, landed probe, fork-point all use it;
  - cutting a fixed-scheme release twice, verifying the second cut sees the
    first as the previous release position;
  - a consumer pinning a fixed branch, reporting current and behind states.
- **Docs:** `using-knives` and `fork-maintenance` skills gain `base` (trunk)
  semantics, the `release_branch` knob with the fixed-scheme workflow, trust
  rules, and `knives register`.

## Sequencing inside the PR

1. Trunk threading (substrate: probes take a trunk parameter).
2. `ReleaseScheme` enum and fixed-scheme semantics on top.
3. Issue #5 fixes (independent of 1–2 except pin-scan touchpoints).
4. Trust rules + `register`.
5. Status table.
6. Skill-doc updates.
