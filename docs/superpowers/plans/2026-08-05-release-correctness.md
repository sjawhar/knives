# Release Correctness Implementation Plan (PR 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A release cut can never silently lose content, superseded cuts are reaped instead of accumulating, the shared-base invariant is maintained and checked, `release rebase` replaces the base instead of accumulating parents, and the coverage gaps from #9 are closed. Closes #4, #7, #9, #10, #11.

**Spec:** `docs/superpowers/specs/2026-08-05-release-correctness-and-knives-gh-design.md` — read it first.

**Architecture:** Everything hangs off two facts the codebase already has: `Repo::is_ancestor` (src/jj.rs:254) answers reachability through jj-lib's index, and `bookmark_tips()` enumerates every ref. New porcelain helpers (`commits_matching`, `forget_bookmark_include_remotes`, `abandon_commits`) do the mutating and revset work. A strict dated-name parser separates OUR dated cuts (`release/2026-08-05.1`) from upstream's semver branches (`release/0.3.190`). On top of those: a pre-cut orphan gate, a post-cut content audit (net-diff replay via `probe_net_diff`; amendment 2026-08-06 (2)), a reap engine shared by cut-time and `knives release reap`, a head filter for the divergence detector, shared-base classification of release parents, and a rewritten `run_rebase`.

**Tech Stack:** Rust edition 2024 (rust 1.90), clap, serde/toml, jj-lib =0.43.0 pinned. The TypeScript plugin under `plugin/` is untouched.

## Global Constraints

- **jj, not git.** All VCS through `jj` (`jj describe`, `jj new`, `jj git push`). Never `git commit`/`git push` in this repo.
- **One commit per PR.** Work accumulates in `@`. No per-task commits, no `jj split`. Describe once at the end (Task 14). Where a step below says "commit", skip it.
- **Gates that must stay green:** `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features --workspace -- -D warnings`, `cargo nextest run --all-targets --all-features --workspace` (fall back to `cargo test`), `bun run lint`, `bun run typecheck`, `bun run test:knives-plugin`.
- **Identity guard:** `tests/no_hardcoded_identity.rs` forbids the forge host written with a trailing slash, and project-family literals, under `src/`, `docs/`, `plugin/`, `skills/`, `hooks/`. Tests use `forge.invalid` / `example.test` URLs. This plan file itself lives in `docs/` — keep those literals out of it too.
- **House style:** doc comments state current behavior and the failure that motivated it. Test names are sentences. Given/When/Then comments in tests. `#![allow(clippy::indexing_slicing, reason = ...)]` at test-module top, matching existing files.
- **Exit discipline:** problems → `Exit::Incomplete`; findings → `Exit::Findings`; a command that cannot answer must not exit zero.
- **Never-pushes invariant:** `knives release cut` moves a bookmark and never pushes. Reaping likewise never touches a remote: `bookmark forget --include-remotes` erases local knowledge of remote refs, it deletes nothing on the wire. The spec's "bookmark moved and pushed" phrasing describes the operator's workflow, not the command.
- **Integration tests** live in `tests/jj_integration.rs` and use `lab::Lab` (`tests/common/lab.rs`). `lab.branch(name, file, content)` makes a branch off `main@origin`; `lab.octopus(name, first, second)` builds a release-shaped merge with parents `(main@origin, first, second)`; `lab.advance_upstream(content)` moves `main@upstream`; `lab.jj_work([...])` runs raw jj in the work clone. Binary invocations use `Command::new(env!("CARGO_BIN_EXE_knives"))` with `.env("KNIVES_CONFIG_HOME", home.path())` and a `repos.toml` written into that home (copy the shape from `release_rebase_repairs_a_followed_dated_release_with_a_sideways_merge`, tests/jj_integration.rs:1359).

---

### Task 1: jj porcelain plumbing — `commits_matching`, `forget_bookmark_include_remotes`, `abandon_commits`

**Files:**
- Modify: `src/jj.rs` (add three functions near `set_bookmark_anywhere`, ~line 922)
- Test: `tests/jj_integration.rs`

**Interfaces:**
- Produces:
  - `pub fn commits_matching(repo: &Path, revset: &str) -> Result<Vec<CommitId>, JjError>` — full commit ids matching a revset, empty vec for no matches.
  - `pub fn forget_bookmark_include_remotes(repo: &Path, name: &str) -> Result<(), JjError>`
  - `pub fn abandon_commits(repo: &Path, commits: &[CommitId]) -> Result<(), JjError>` — one invocation for all ids (abandoning one at a time rewrites later ids; see `ProbeCleanup`'s comment, src/jj.rs:776).

- [ ] **Step 1: Write the failing integration test** in `tests/jj_integration.rs`:

```rust
#[test]
fn a_forgotten_and_abandoned_release_disappears_and_the_remote_keeps_it() {
    // Given: a pushed release-shaped merge. Forget alone leaves the remote-tracking
    // ref pinning the commit (abandon then refuses "immutable"); forget
    // --include-remotes releases the pin. Ordering verified by experiment in the
    // spec (evidence item 1).
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work([
        "git", "push", "--remote", "origin", "--bookmark", "release/2026-08-04",
    ]);
    let repo = Repo::open(&lab.work).expect("open");
    let release = repo
        .resolve_commit("release/2026-08-04")
        .expect("resolve release");

    // When: the release is reaped in the load-bearing order.
    knives::jj::forget_bookmark_include_remotes(&lab.work, "release/2026-08-04")
        .expect("forget");
    knives::jj::abandon_commits(&lab.work, std::slice::from_ref(&release)).expect("abandon");

    // Then: no ref of any kind remains and the commit is invisible.
    let tips = Repo::open(&lab.work).expect("reopen").bookmark_tips().expect("tips");
    assert!(
        !tips.keys().any(|r| r.branch().as_str() == "release/2026-08-04"),
        "release refs survived: {tips:?}"
    );
    // Visibility check, verified empirically: naming a hidden commit id in a
    // revset RESURRECTS it into the resolution (`all() & <id>` still returns
    // it after abandon), so the only honest assertion is listing all() and
    // checking absence.
    let visible = knives::jj::commits_matching(&lab.work, "all()").expect("query");
    assert!(
        !visible.contains(&release),
        "abandoned commit still visible: {release}"
    );
    // And: the remote still has the branch — reaping never touches the wire.
    let on_remote = std::process::Command::new("git")
        .args(["ls-remote", "--heads", lab.temp_origin().to_str().expect("utf-8"), "release/2026-08-04"])
        .output()
        .expect("ls-remote");
    assert!(
        !String::from_utf8_lossy(&on_remote.stdout).trim().is_empty(),
        "remote branch was deleted"
    );
}
```

The lab does not expose the origin bare repo path; add this accessor to `tests/common/lab.rs` next to `work_path()` (~line 360):

```rust
pub(crate) fn temp_origin(&self) -> PathBuf {
    self.temp.path().join("origin.git")
}
```

- [ ] **Step 2: Run to verify failure:** `cargo test --test jj_integration a_forgotten_and_abandoned_release_disappears -- --nocapture` — FAIL: `forget_bookmark_include_remotes` not found.

- [ ] **Step 3: Implement** in `src/jj.rs` after `set_bookmark_anywhere` (~line 922):

```rust
/// Commits matching a revset, resolved through jj porcelain.
///
/// Exists for queries jj-lib makes hard (glob descriptions, `empty()`,
/// ancestry set arithmetic) and for callers that need "no matches" as an
/// empty answer rather than an error: `jj log` on an EMPTY REVSET prints
/// nothing and exits zero (verified with `none()`). Note that naming a hidden
/// commit id in a revset resurrects it into the resolution (even through
/// `all() & <id>`, verified); callers asking about visibility must list a
/// visibility-scoped revset and test membership themselves.
pub fn commits_matching(repo: &Path, revset: &str) -> Result<Vec<CommitId>, JjError> {
    let repo_path = path(repo);
    let output = command(
        "jj",
        [
            "--repository",
            &repo_path,
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            revset,
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    )?;
    Ok(output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| CommitId::new(line.trim()))
        .collect())
}

/// Forget a bookmark AND its remote-tracking refs, releasing the pin they hold.
///
/// `bookmark forget` alone leaves the `@remote` ref, which keeps the commit
/// immutable so a following abandon refuses. `--include-remotes` is the whole
/// point; verified by experiment (spec, evidence item 1). Erases local
/// knowledge only: nothing is deleted on any remote.
pub fn forget_bookmark_include_remotes(repo: &Path, name: &str) -> Result<(), JjError> {
    let repo_path = path(repo);
    command(
        "jj",
        [
            "--repository",
            &repo_path,
            "--ignore-working-copy",
            "bookmark",
            "forget",
            "--include-remotes",
            name,
        ],
    )?;
    Ok(())
}

/// Abandon commits by explicit id, in ONE invocation.
///
/// One at a time does not work: abandoning a commit rebases its descendants,
/// which rewrites the ids of the later ones (same lesson as `ProbeCleanup`).
pub fn abandon_commits(repo: &Path, commits: &[CommitId]) -> Result<(), JjError> {
    if commits.is_empty() {
        return Ok(());
    }
    let revset = commits
        .iter()
        .map(|commit| commit.as_str().to_owned())
        .collect::<Vec<_>>()
        .join("|");
    let repo_path = path(repo);
    command(
        "jj",
        [
            "--repository",
            &repo_path,
            "--ignore-working-copy",
            "abandon",
            "-r",
            &revset,
        ],
    )?;
    Ok(())
}
```

- [ ] **Step 4: Run:** `cargo test --test jj_integration a_forgotten_and_abandoned_release_disappears` — PASS.

---

### Task 2: strict dated-name parser in `src/ids.rs`

**Files:**
- Modify: `src/ids.rs` (after `is_release_name`, ~line 133)

**Interfaces:**
- Produces: `pub fn strict_dated_release(name: &str) -> Option<(String, u32)>` — `Some((date, suffix))` only for `release/YYYY-MM-DD` or `release/YYYY-MM-DD.N`. Returns the same ordering key shape as `status::release_order` so callers can `max_by_key`.

Why it exists: reaping enumerates dated refs on **any** remote, and `is_release_name(Dated)` is a bare prefix test that also matches upstream's own semver branches (`release/0.3.190@upstream` on a real fork). Reaping must never touch those, so the reaper needs a parser that only accepts our dated shape.

- [ ] **Step 1: Write the failing tests** in `src/ids.rs` `mod tests`:

```rust
#[test]
fn only_our_dated_shape_parses_as_a_dated_release() {
    use super::strict_dated_release;
    // Ours, with and without a same-day suffix.
    assert_eq!(
        strict_dated_release("release/2026-08-05"),
        Some(("2026-08-05".to_owned(), 0))
    );
    assert_eq!(
        strict_dated_release("release/2026-08-05.2"),
        Some(("2026-08-05".to_owned(), 2))
    );
    // Upstream's semver release branches are NOT ours to reap.
    assert_eq!(strict_dated_release("release/0.3.190"), None);
    // Shape violations.
    assert_eq!(strict_dated_release("release/"), None);
    assert_eq!(strict_dated_release("release/2026-8-5"), None);
    assert_eq!(strict_dated_release("release/2026-08-05."), None);
    assert_eq!(strict_dated_release("release/2026-08-05.x"), None);
    assert_eq!(strict_dated_release("feat/2026-08-05"), None);
}
```

- [ ] **Step 2: Run:** `cargo test -p knives only_our_dated_shape` — FAIL: function not found.

- [ ] **Step 3: Implement** in `src/ids.rs`:

```rust
/// Parse `release/YYYY-MM-DD[.N]`, the one shape our dated cuts take.
///
/// Stricter than [`is_release_name`] on purpose: the reaper enumerates release
/// refs on any remote, where upstream's own `release/0.3.190` style branches
/// also live, and a prefix test would hand those to `bookmark forget`. Returns
/// the `(date, suffix)` ordering key so "newest" is one `max_by_key` away.
pub fn strict_dated_release(name: &str) -> Option<(String, u32)> {
    let bare = name.strip_prefix(RELEASE_PREFIX)?;
    let (date, suffix) = match bare.split_once('.') {
        Some((date, suffix)) => (date, suffix.parse::<u32>().ok()?),
        None => (bare, 0),
    };
    let bytes = date.as_bytes();
    let dated = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && date
            .bytes()
            .enumerate()
            .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit());
    dated.then(|| (date.to_owned(), suffix))
}
```

- [ ] **Step 4: Run:** `cargo test -p knives only_our_dated_shape` — PASS. Also `cargo clippy --all-targets -- -D warnings` on the touched crate.

---

### Task 3: superseded-release enumeration and `newest_release` extraction

**Files:**
- Modify: `src/commands/release.rs` (extract from `plan()`, ~line 267; new functions + unit tests)

**Interfaces:**
- Consumes: `strict_dated_release` (Task 2), `BookmarkTips`, `release_order` (`crate::commands::status::release_order`).
- Produces:
  - `pub fn newest_release(tips: &BookmarkTips, scheme: &ReleaseScheme, publish_remote: &str) -> Option<(BookmarkRef, CommitId)>` — exactly the `newest` selection currently inlined in `plan()` (src/commands/release.rs:267-290), moved verbatim into a named function. `plan()` calls it.
  - `pub fn superseded_dated_releases(tips: &BookmarkTips) -> Vec<(BookmarkRef, CommitId)>` — every ref (local or remote) whose branch name parses via `strict_dated_release`, EXCEPT (a) all refs of the newest dated name, (b) refs on the `upstream` or `git` remotes. Sorted by `(name, ref)` for deterministic output.

Note the deliberate asymmetry with `is_our_release` (src/ids.rs:102): that function only trusts `origin`/`release` remotes, because "newest release" must never be somebody else's cut. Reaping instead covers OUR dated names on ANY publishing remote (historical refs live on other remotes on real forks — spec evidence item 9), and excludes `upstream`/`git` because those are not ours to forget. `strict_dated_release` is what keeps upstream's own `release/*` branches out even so.

- [ ] **Step 1: Write the failing unit tests** in `src/commands/release.rs` (new `mod reap_enumeration_tests` beside `mod tests`):

```rust
#[cfg(test)]
mod reap_enumeration_tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;
    use crate::ids::{BookmarkRef, BranchName, CommitId, RemoteName};

    fn local(name: &str, commit: &str) -> (BookmarkRef, CommitId) {
        (
            BookmarkRef::Local(BranchName::new(name)),
            CommitId::new(commit),
        )
    }

    fn remote(name: &str, remote: &str, commit: &str) -> (BookmarkRef, CommitId) {
        (
            BookmarkRef::Remote {
                branch: BranchName::new(name),
                remote: RemoteName::new(remote),
            },
            CommitId::new(commit),
        )
    }

    #[test]
    fn every_ref_of_a_superseded_dated_name_is_enumerated_on_any_remote_but_upstream() {
        // Given: two dated cuts with refs scattered across remotes (the shape a
        // pre-knives fork accumulates), upstream's own semver branch, and a work branch.
        let tips: BookmarkTips = [
            local("release/2026-08-04", "aaa"),
            remote("release/2026-08-04", "release", "aaa"),
            remote("release/2026-08-04", "publish2", "aaa"),
            remote("release/2026-08-04", "git", "aaa"),
            local("release/2026-08-05", "bbb"),
            remote("release/2026-08-05", "release", "bbb"),
            remote("release/0.3.190", "upstream", "ccc"),
            remote("release/2026-07-01", "upstream", "ddd"),
            local("feat/x", "eee"),
        ]
        .into_iter()
        .collect();

        let superseded = superseded_dated_releases(&tips);
        let names: Vec<String> = superseded.iter().map(|(r, _)| r.to_string()).collect();

        // Then: only the older dated name, on every remote except upstream and git.
        assert_eq!(
            names,
            vec![
                "release/2026-08-04".to_owned(),
                "release/2026-08-04@publish2".to_owned(),
                "release/2026-08-04@release".to_owned(),
            ]
        );
    }

    #[test]
    fn the_newest_dated_name_is_never_superseded_even_when_only_remote() {
        // The newest cut may exist only as a remote ref in a fresh clone.
        let tips: BookmarkTips = [
            local("release/2026-08-04", "aaa"),
            remote("release/2026-08-05.2", "release", "bbb"),
        ]
        .into_iter()
        .collect();
        let names: Vec<String> = superseded_dated_releases(&tips)
            .iter()
            .map(|(r, _)| r.to_string())
            .collect();
        assert_eq!(names, vec!["release/2026-08-04".to_owned()]);
    }
}
```

- [ ] **Step 2: Run:** `cargo test -p knives reap_enumeration` — FAIL: function not found.

- [ ] **Step 3: Implement.** First extract `newest_release` from `plan()` verbatim (the whole `let newest = match &scheme { ... }` block becomes the function body; `plan()` becomes `let newest = newest_release(&tips, &scheme, &publish_remote);`). Then add:

```rust
/// Every ref of every superseded dated cut, on any remote that is ours.
///
/// Superseded means "not the newest dated name". `upstream` is somebody
/// else's repository and `git` is jj's internal tracking view, so refs there
/// are never enumerated; every other remote is a place we have published to
/// (real forks accumulate historical release refs on more than one).
/// [`crate::ids::strict_dated_release`] keeps upstream-style semver names out
/// even on our remotes.
/// Which refs are ours to reap: everything except `upstream` (somebody
/// else's repository) and `git` (jj's internal tracking view). Applied to
/// BOTH the newest computation and the output: letting an excluded ref vote
/// on "newest" while being barred from the output let an upstream dated ref
/// outrank ours — classifying the LIVE release as superseded and handing it
/// to the reaper (caught in review with a reproduced fixture).
fn ours_to_reap(reference: &BookmarkRef) -> bool {
    !matches!(
        reference,
        BookmarkRef::Remote { remote, .. } if matches!(remote.as_str(), "upstream" | "git")
    )
}

pub fn superseded_dated_releases(tips: &BookmarkTips) -> Vec<(BookmarkRef, CommitId)> {
    // The VOTE is an allowlist (is_our_release: local | origin | release):
    // "newest release" must never be somebody else's cut, and bookmark_tips
    // reports EVERY remote jj knows — a mirror or a colleague's fork added as
    // a remote would otherwise outrank the live cut and hand it to the reaper.
    // The OUTPUT below keeps the broader denylist (ours_to_reap): historical
    // refs of superseded names on odd remotes are still ours to clean up.
    let newest = tips
        .keys()
        .filter(|reference| is_our_release(reference, &ReleaseScheme::Dated))
        .filter_map(|reference| strict_dated_release(reference.branch().as_str()))
        .max();
    let Some(newest) = newest else {
        return Vec::new();
    };
    let mut found: Vec<(BookmarkRef, CommitId)> = tips
        .iter()
        .filter(|(reference, _)| {
            ours_to_reap(reference)
                && strict_dated_release(reference.branch().as_str())
                    .is_some_and(|parsed| parsed != newest)
        })
        .map(|(reference, commit)| (reference.clone(), commit.clone()))
        .collect();
    // Name-major order (the doc contract): refs of one name adjacent, local
    // before remotes — BookmarkRef's derived Ord is variant-major and would
    // interleave names.
    found.sort_by(|(a, _), (b, _)| {
        (a.branch(), a).cmp(&(b.branch(), b))
    });
    found
}
```

Add `strict_dated_release` to the existing `crate::ids::{...}` import list at the top of the file.

- [ ] **Step 4: Run:** `cargo test -p knives reap_enumeration && cargo test -p knives release` — PASS, and the existing `plan()` tests still pass after the extraction.

---

### Task 4: divergence detector ignores superseded release refs (#7, decision C)

**Files:**
- Modify: `src/jj.rs` (`Repo::divergent_changes`, line 216)
- Modify: `src/commands/status.rs:630`, `src/commands/preflight.rs:251` (call sites)
- Test: `tests/jj_integration.rs`

**Interfaces:**
- Consumes: `superseded_dated_releases` (Task 3), `Repo::is_ancestor` (src/jj.rs:254).
- Produces: changed signature `pub fn divergent_changes(&self, ignored: &BTreeSet<BookmarkRef>) -> Result<Vec<(ChangeId, CommitId)>, JjError>`. Callers pass `&superseded_dated_releases(&tips).into_iter().map(|(r, _)| r).collect()`; an empty set reproduces today's behavior exactly.

Two-level filter, both levels required (spec §1.5):

1. **Head level:** a view head is skipped when at least one ref points at it and every ref pointing at it is in `ignored`. Heads with no refs (working copies) are kept.
2. **Commit level:** when a change resolves to several visible commits, a commit is kept only if it is reachable from some non-ignored head (`is_ancestor(commit, head)`). Without this, an old copy pinned *as an ancestor* of a superseded release merge still counts, and the finding survives the head filter — that is exactly the shape of the real-fork case in the spec (a divergent commit pinned by a superseded cut).

- [ ] **Step 1: Write the failing integration test** in `tests/jj_integration.rs`:

```rust
#[test]
fn divergence_pinned_only_by_a_superseded_release_ref_is_not_reported() {
    // Given: one change as two commits, where the old copy's only visibility is a
    // remote-tracking ref we are told to ignore. This is the re-materialized
    // superseded-cut shape: bare `jj git fetch` brings such refs back forever,
    // so the reader must ignore them rather than the graph staying clean.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "feature.txt", "one\n");
    lab.rewrite_in_both_clones("feat/alpha");
    let repo = Repo::open(&lab.work).expect("open");

    // Sanity: unfiltered, the divergence is visible (two commits, one change).
    let unfiltered = repo
        .divergent_changes(&std::collections::BTreeSet::new())
        .expect("divergent unfiltered");
    assert!(
        !unfiltered.is_empty(),
        "fixture failed to create divergence"
    );

    // When: the ref holding the stale copy is ignored.
    let ignored: std::collections::BTreeSet<BookmarkRef> =
        [BookmarkRef::Remote {
            branch: BranchName::new("feat/alpha"),
            remote: RemoteName::new("origin"),
        }]
        .into_iter()
        .collect();
    let filtered = repo.divergent_changes(&ignored).expect("divergent filtered");

    // Then: the finding is gone — the stale copy was visible only through the
    // ignored ref, so nothing else vouches for it.
    assert!(filtered.is_empty(), "still reported: {filtered:?}");
}
```

Add `RemoteName` to the test file's `knives::ids::{...}` import if absent.

- [ ] **Step 2: Run:** `cargo test --test jj_integration divergence_pinned_only_by` — FAIL: `divergent_changes` takes no arguments.

- [ ] **Step 3: Implement.** Replace the body of `divergent_changes` (src/jj.rs:216) with:

```rust
/// One change existing as several visible commits, ignoring nominated refs.
///
/// `ignored` names refs whose testimony does not count — in practice the
/// superseded dated releases, which any `jj git fetch` re-materializes as
/// untracked refs forever (they exist on the remote and jj keeps no memory of
/// forgetting them). A head every one of whose refs is ignored is skipped, and
/// a divergent copy is only reported while some non-ignored head can reach it.
/// Filtering the reader instead of re-cleaning the graph is deliberate: the
/// repo must stay correct under bare fetches by any tool.
pub fn divergent_changes(
    &self,
    ignored: &BTreeSet<BookmarkRef>,
) -> Result<Vec<(ChangeId, CommitId)>, JjError> {
    let tips = self.bookmark_tips()?;
    // Refs per commit, so "every ref on this head is ignored" is answerable.
    let mut refs_at: BTreeMap<&CommitId, Vec<&BookmarkRef>> = BTreeMap::new();
    for (reference, commit) in &tips {
        refs_at.entry(commit).or_default().push(reference);
    }
    // Kept heads are the VOUCHING authorities; enumeration walks ALL heads.
    // Skipping ignored heads from enumeration too silently erased a real
    // divergence whose only head-copy sat under a superseded release ref while
    // two live-branch copies were non-head ancestors of kept heads (caught in
    // review with a reproduced fixture: 0 reported where 2 was correct).
    let mut kept_heads = Vec::new();
    let mut all_heads = Vec::new();
    for head in self.repo.view().heads() {
        let commit = commit_id(head);
        let all_ignored = refs_at
            .get(&commit)
            .is_some_and(|refs| refs.iter().all(|reference| ignored.contains(reference)));
        if !all_ignored {
            kept_heads.push(commit.clone());
        }
        all_heads.push(commit);
    }

    let mut changes = BTreeMap::<ChangeId, BTreeSet<CommitId>>::new();
    for head in &all_heads {
        let commit = self
            .repo
            .store()
            .get_commit(&JjCommitId::try_from_hex(head.as_str()).ok_or_else(|| {
                JjError::Revision {
                    revision: head.as_str().to_owned(),
                    detail: "view head is not a hex commit id".to_owned(),
                }
            })?)
            .map_err(|error| JjError::Open {
                path: "commit store".to_owned(),
                detail: error.to_string(),
            })?;
        let change = ChangeId::new(commit.change_id().to_string());
        if let Some(targets) =
            self.repo
                .resolve_change_id(commit.change_id())
                .map_err(|error| JjError::Open {
                    path: "change index".to_owned(),
                    detail: error.to_string(),
                })?
        {
            let commits = changes.entry(change).or_default();
            for (_, id) in targets.visible_with_offsets() {
                let candidate = commit_id(id);
                // A copy only an ignored ref can reach does not count. Errors
                // PROPAGATE (oracle amendment): unwrap_or(false) would turn an
                // index failure into silent suppression of a correctness finding.
                let mut vouched = false;
                for kept in &kept_heads {
                    if self.is_ancestor(&candidate, kept)? {
                        vouched = true;
                        break;
                    }
                }
                if vouched {
                    commits.insert(candidate);
                }
            }
        }
    }
    Ok(changes
        .into_iter()
        .filter(|(_, commits)| commits.len() > 1)
        .flat_map(|(change, commits)| {
            commits
                .into_iter()
                .map(move |commit| (change.clone(), commit))
        })
        .collect())
}
```

Note the shape change from the current code: the current version iterates heads and resolves per-head; keep that structure, only adding the two filters. `BTreeSet` is already imported in src/jj.rs.

- [ ] **Step 4: Update the two callers.** In `src/commands/status.rs` (line 630) and `src/commands/preflight.rs` (line 251), compute the ignored set before the call:

```rust
let ignored: std::collections::BTreeSet<crate::ids::BookmarkRef> =
    crate::commands::release::superseded_dated_releases(&tips)
        .into_iter()
        .map(|(reference, _)| reference)
        .collect();
// ...
.extend(divergent_changes(&repo.divergent_changes(&ignored)?));
```

Both call sites already have `tips` (or can call `repo.bookmark_tips()?` once more if not in scope — check locally; status gathers tips earlier in `gather`). Use whichever binding exists; do not re-derive the scheme, `superseded_dated_releases` is scheme-independent by design (strict dated names are ours under either scheme, and under `Fixed` there are simply none... unless the fork migrated from dated to fixed, in which case reaping its dated leftovers is still right).

- [ ] **Step 5: Run:** `cargo test --test jj_integration divergen` — the new test AND `divergent_changes_reports_both_rewrites_after_fetch` (line 650, updated to pass `&BTreeSet::new()`) both PASS. `cargo clippy --all-targets -- -D warnings`.

---

### Task 5: the reap engine (#7)

**Files:**
- Modify: `src/commands/release.rs` (new section after `superseded_dated_releases`)
- Test: `tests/jj_integration.rs`

**Interfaces:**
- Consumes: Task 1 plumbing, Task 3 enumeration, `Repo::resolve_commit`.
- Produces:

```rust
pub struct ReapReport {
    /// Bookmark names whose refs were forgotten AND commits abandoned. A name
    /// whose abandon refused lands in `forgotten_only`, never here (oracle
    /// amendment: reaped must not overstate).
    pub reaped: Vec<String>,
    /// Refs forgotten but the commit abandon refused (still pinned by a ref
    /// outside the enumeration, e.g. a tag); details in `notes`.
    pub forgotten_only: Vec<String>,
    /// (name, reason) pairs that were deliberately left alone.
    pub kept: Vec<(String, String)>,
    /// Non-fatal notes (an abandon that refused, a resolve that failed).
    pub notes: Vec<String>,
}

pub fn reap_superseded(repo_path: &Path, repo: &Repo) -> anyhow::Result<ReapReport>
```

Behavior (spec §1.4): group `superseded_dated_releases` output by bookmark name. For each name, in order:

1. **Local-descendants gate:** for each distinct target commit of that name's refs, `commits_matching(repo_path, &format!("(descendants({id}) ~ {id}) ~ (empty() & description(exact:\"\"))"))`. Non-empty → push `(name, "has local descendants: <first 3 ids, comma-joined>")` to `kept` and skip the name entirely. Empty undescribed commits are excluded because they are parked workspace working-copies, which sit on release merges routinely; jj rebases them harmlessly when the parent is abandoned, and blocking on them would mean never reaping anything.
2. **Forget:** `forget_bookmark_include_remotes(repo_path, name)` — one call per name covers local + all remotes.
3. **Abandon:** collect the distinct target commits recorded in step 1, `abandon_commits(repo_path, &targets)`. An `Err` here (still-immutable via a ref outside our enumeration, e.g. a tag) goes to `notes`, not a failure: the forget already achieved the graph cleanup the log needs.

The newest cut and `previous_position` never appear in the input: `superseded_dated_releases` excludes the newest name, and `previous_position` (src/commands/release.rs:149) is `Fixed`-scheme-only while this enumeration is dated-only — the two cannot intersect. Say so in the doc comment rather than adding dead gates.

- [ ] **Step 1: Write the failing integration tests:**

```rust
#[test]
fn reap_removes_superseded_cuts_and_keeps_the_newest() {
    // Given: two dated cuts, both pushed.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    for name in ["release/2026-08-04", "release/2026-08-05"] {
        lab.jj_work(["git", "push", "--remote", "origin", "--bookmark", name]);
    }

    // When: the workspace is reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report =
        knives::commands::release::reap_superseded(&lab.work, &repo).expect("reap");

    // Then: the older cut is gone in every form, the newest survives.
    assert_eq!(report.reaped, vec!["release/2026-08-04".to_owned()]);
    let tips = Repo::open(&lab.work).expect("reopen").bookmark_tips().expect("tips");
    assert!(!tips.keys().any(|r| r.branch().as_str() == "release/2026-08-04"));
    assert!(tips.keys().any(|r| r.branch().as_str() == "release/2026-08-05"));
}

#[test]
fn reap_refuses_a_cut_that_has_local_descendants() {
    // Given: work stacked directly on a superseded cut — #4's third loss mode.
    // Reaping must never be the thing that drops it.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work(["new", "release/2026-08-04", "-m", "stacked work"]);
    lab.jj_work(["new"]); // park the working copy elsewhere
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");

    let repo = Repo::open(&lab.work).expect("open");
    let report =
        knives::commands::release::reap_superseded(&lab.work, &repo).expect("reap");

    // Then: nothing reaped; the reason names the descendant.
    assert!(report.reaped.is_empty(), "reaped: {:?}", report.reaped);
    assert_eq!(report.kept.len(), 1);
    assert!(report.kept[0].1.contains("descendant"), "{:?}", report.kept);
    let tips = Repo::open(&lab.work).expect("reopen").bookmark_tips().expect("tips");
    assert!(tips.keys().any(|r| r.branch().as_str() == "release/2026-08-04"));
}

#[test]
fn reap_clears_a_ref_the_next_fetch_rematerialized() {
    // Given: a reaped workspace whose next fetch resurrected the superseded ref
    // as untracked (jj keeps no memory of forgotten refs; spec evidence item 2).
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    for name in ["release/2026-08-04", "release/2026-08-05"] {
        lab.jj_work(["git", "push", "--remote", "origin", "--bookmark", name]);
    }
    let repo = Repo::open(&lab.work).expect("open");
    knives::commands::release::reap_superseded(&lab.work, &repo).expect("first reap");
    lab.fetch_work();
    let tips = Repo::open(&lab.work).expect("reopen").bookmark_tips().expect("tips");
    assert!(
        tips.keys().any(|r| r.branch().as_str() == "release/2026-08-04"),
        "fixture expects the fetch to re-materialize the ref; it did not"
    );

    // When: reaped again (idempotence is the contract).
    let repo = Repo::open(&lab.work).expect("reopen for second reap");
    let report =
        knives::commands::release::reap_superseded(&lab.work, &repo).expect("second reap");

    // Then: gone again.
    assert_eq!(report.reaped, vec!["release/2026-08-04".to_owned()]);
    let tips = Repo::open(&lab.work).expect("final open").bookmark_tips().expect("tips");
    assert!(!tips.keys().any(|r| r.branch().as_str() == "release/2026-08-04"));
}
```

- [ ] **Step 2: Run:** `cargo test --test jj_integration reap_` — FAIL: `reap_superseded` not found.

- [ ] **Step 3: Implement** in `src/commands/release.rs`:

```rust
/// Reap every superseded dated cut: forget its refs everywhere, abandon its commits.
///
/// The newest dated name never appears in the enumeration, and the
/// `previous_position` seam is `Fixed`-scheme-only while dated names are the
/// only thing enumerated, so neither needs a runtime gate here. What does:
/// a cut with local descendants is someone's stacked work (#4's third loss
/// mode) and is refused with the descendants named. Parked workspace working
/// copies — empty, undescribed — do not block: they sit on release merges as a
/// matter of course and jj rebases them harmlessly.
///
/// Never touches a remote. A later fetch re-materializes forgotten refs as
/// untracked (jj keeps no memory of forgetting); that is expected, harmless to
/// the default log, and cleared by the next reap. Correctness never depends on
/// reaping having run: the divergence detector ignores these refs regardless.
pub fn reap_superseded(repo_path: &Path, repo: &Repo) -> anyhow::Result<ReapReport> {
    let tips = repo.bookmark_tips()?;
    let mut by_name: std::collections::BTreeMap<String, Vec<CommitId>> = Default::default();
    for (reference, commit) in superseded_dated_releases(&tips) {
        let targets = by_name.entry(reference.branch().to_string()).or_default();
        if !targets.contains(&commit) {
            targets.push(commit);
        }
    }

    let mut report = ReapReport {
        reaped: Vec::new(),
        forgotten_only: Vec::new(),
        kept: Vec::new(),
        notes: Vec::new(),
    };
    'names: for (name, targets) in by_name {
        for target in &targets {
            let descendants = crate::jj::commits_matching(
                repo_path,
                &format!(
                    "(descendants({id}) ~ {id}) ~ (empty() & description(exact:\"\"))",
                    id = target.as_str()
                ),
            )?;
            if !descendants.is_empty() {
                let sample: Vec<String> = descendants
                    .iter()
                    .take(3)
                    .map(|c| c.as_str().chars().take(12).collect())
                    .collect();
                report.kept.push((
                    name.clone(),
                    format!("has local descendant(s): {}", sample.join(", ")),
                ));
                continue 'names;
            }
        }
        crate::jj::forget_bookmark_include_remotes(repo_path, &name)?;
        match crate::jj::abandon_commits(repo_path, &targets) {
            Ok(()) => report.reaped.push(name),
            Err(error) => {
                report
                    .notes
                    .push(format!("{name}: refs forgotten, abandon refused: {error}"));
                report.forgotten_only.push(name);
            }
        }
    }
    Ok(report)
}
```

Define `ReapReport` (fields as in Interfaces above, with the doc comments shown there) immediately before the function. Note `Repo` must be reopened by CALLERS after mutations elsewhere — `reap_superseded` reads tips from the `Repo` handle it was given, so pass a freshly opened one (the tests above model this).

- [ ] **Step 4: Run:** `cargo test --test jj_integration reap_` — all three PASS.

---

### Task 6: `knives release reap` subcommand + reap at cut time

**Files:**
- Modify: `src/cli.rs` (add to `ReleaseAction`, ~line 283)
- Modify: `src/main.rs` (`dispatch_release` ~line 159, `run_release` after the cut block ~line 611, new `run_reap`)
- Test: `tests/jj_integration.rs`

**Interfaces:**
- Consumes: `reap_superseded` (Task 5).
- Produces: `knives release reap` CLI; automatic reap after a successful dated cut inside `run_release`.

- [ ] **Step 1: Add the CLI variant** to `ReleaseAction` in `src/cli.rs`:

```rust
    /// Reap superseded dated cuts: forget their bookmarks everywhere, abandon
    /// their commits. The remote is never touched.
    ///
    /// Runs automatically after every cut; exists standalone for pre-knives
    /// repos carrying years of historical refs, and as the unlock when a rebase
    /// needs old-lineage commits mutable (superseded release refs are immutable
    /// heads, and they freeze every member commit in their ancestry). A later
    /// fetch re-materializes forgotten refs as untracked; re-run to clear them.
    Reap,
```

Add `vec!["knives", "release", "reap"]` to `every_designed_command_is_reachable` (src/cli.rs:321).

- [ ] **Step 2: Wire dispatch** in `src/main.rs` `dispatch_release`:

```rust
        Some(ReleaseAction::Reap) => run_reap(chosen.as_str()),
```

and add:

```rust
/// Reap superseded dated cuts on demand.
fn run_reap(name: &str) -> anyhow::Result<Exit> {
    let chosen = match selected(Some(name), false)? {
        Ok(list) => list,
        Err(exit) => return Ok(exit),
    };
    let mut worst = Exit::Ok;
    for (repo, entry) in chosen {
        let opened = knives::jj::Repo::open(&entry.path)?;
        let report = release::reap_superseded(&entry.path, &opened)?;
        print_reap(&repo.to_string(), &report);
        if !report.notes.is_empty() {
            worst = worst.worst(Exit::Findings);
        }
    }
    Ok(worst)
}

fn print_reap(repo: &str, report: &knives::commands::release::ReapReport) {
    if report.reaped.is_empty() && report.forgotten_only.is_empty() && report.kept.is_empty() {
        println!("{repo}: nothing to reap");
    }
    for name in &report.reaped {
        println!("{repo}: reaped {name} (refs forgotten everywhere, commit abandoned; remote untouched)");
    }
    for name in &report.forgotten_only {
        println!("{repo}: {name}: refs forgotten; commit abandon refused (see note)");
    }
    for (name, reason) in &report.kept {
        println!("{repo}: kept {name}: {reason}");
    }
    for note in &report.notes {
        println!("{repo}: ! {note}");
    }
}
```

- [ ] **Step 3: Reap after a successful cut.** In `run_release`, at the END of the `if let Some(name) = cut_name` block (after the workspaces-to-clean report, ~line 611), add (a small `reap_after_cut` helper is fine):

```rust
            // Reap superseded cuts now that a newer one exists. Under Fixed
            // the enumeration is empty by construction (no dated names), so
            // this is a no-op there.
            let reopened = knives::jj::Repo::open(&entry.path)?;
            let report = release::reap_superseded(&entry.path, &reopened)?;
            print_reap(&repo.to_string(), &report);
            if !report.notes.is_empty() {
                worst = worst.worst(Exit::Findings);
            }
```

Reopen deliberately: the `opened` handle predates the cut and reads stale tips. The notes fold is not optional: a refused abandon is a finding, and the Global Constraints' exit discipline (findings → `Exit::Findings`) applies to the cut path exactly as it does to standalone `reap`. (Amended 2026-08-06: the original snippet omitted the fold; review of Task 6 caught the asymmetry.)

- [ ] **Step 4: Write the failing integration test:**

```rust
#[test]
fn cutting_a_release_reaps_the_superseded_one() {
    // Given: an existing cut and a registry, then a newer cut through the binary.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nconsumers = [\"{}\"]\n",
            lab.work.display(),
            lab.upstream.display(),
            lab.work.display(),
        ),
    )
    .expect("write registry");

    // When: a newer release is cut.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "release", "--repo", "demo", "cut", "release/2026-08-05"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run release cut");

    // Then: the cut succeeds and the superseded name is gone.
    assert!(
        output.status.success(),
        "cut failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("reaped release/2026-08-04"), "{stdout}");
    let tips = Repo::open(&lab.work).expect("reopen").bookmark_tips().expect("tips");
    assert!(!tips.keys().any(|r| r.branch().as_str() == "release/2026-08-04"));
    assert!(tips.keys().any(|r| r.branch().as_str() == "release/2026-08-05"));
}
```

Note the registry needs a `consumers` entry (the plan's central question is pinned-ness; without one the command exits `Incomplete` — see src/commands/release.rs:315). Pointing it at the work dir itself is the established dodge in this suite when pins are irrelevant... it is NOT: check `release_rebase_repairs_a_followed_dated_release_with_a_sideways_merge` — it builds a real consumer. For this test the exit code matters, so build the consumer the same way that test does (`lab.consumer_with_pin_history(...)` with a `branch = "release/2026-08-04"` pin) and put its path in `consumers`. Copy those four lines verbatim from that test.

- [ ] **Step 5: Run:** `cargo test --test jj_integration cutting_a_release_reaps` — PASS. Then the whole file: `cargo test --test jj_integration` — no regressions (in particular `cutting_a_release_does_not_move_another_agents_working_copy` still passes; the reap excludes parked working copies by the empty-undescribed rule).

---

### Task 7: pre-cut orphan gate with `--allow-drop` (#4)

**Files:**
- Modify: `src/commands/release.rs` (new function), `src/cli.rs` (`Cut` gains a flag), `src/main.rs` (`run_release` wiring)
- Test: `tests/jj_integration.rs`

**Interfaces:**
- Consumes: `commits_matching` (Task 1), `newest_release` (Task 3).
- Produces: `pub fn orphaned_commits(repo_path: &Path, previous: &CommitId, keep: &[CommitId]) -> Result<Vec<CommitId>, JjError>` in release.rs; `--allow-drop` on `knives release cut`.

Semantics (spec §1.2): the protected lineage is the previous release *and its local descendants* (`::(P::)` — ancestors of descendants of P). Lost means not reachable from any keeper. Keepers are **every non-release local bookmark tip plus the upstream trunk tip** — not just release members, because a branch deliberately dropped from the release still holds its content through its own bookmark and must not trip the gate. Excluded from the report:

- parked working copies: `empty() & description(exact:"")`
- the release lineage's own machinery: commits whose description starts `release:` (a cut's own message, see `Cut::message`) or `chore(release):` (a rebase's message) — superseded merges are what reaping abandons, not content.

- [ ] **Step 1: Write the failing integration tests:**

```rust
#[test]
fn a_cut_refuses_when_work_lives_only_in_the_release_lineage() {
    // Given: a commit stacked on the release merge — #4's third loss mode. The
    // next flat cut would not include it, and nothing else reaches it.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work(["new", "release/2026-08-04", "-m", "hotfix applied on the release"]);
    std::fs::write(lab.work_path().join("hotfix.txt"), "fix\n").expect("write hotfix");
    lab.jj_work(["new"]); // park @ off the hotfix so it snapshots as its own commit
    let stacked = lab.revision(lab.work_path(), "description(glob:\"hotfix*\")", "commit_id");
    let (home, _consumer) = release_test_home(&lab); // helper defined in Step 2

    // When: a newer cut is attempted without acknowledgement.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "release", "--repo", "demo", "cut", "release/2026-08-05"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run release cut");

    // Then: refused, naming the exact commit, and no new bookmark exists.
    assert!(!output.status.success(), "cut should have refused");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains(&stacked.chars().take(12).collect::<String>()),
        "refusal must name the commit: {text}"
    );
    assert!(text.contains("--allow-drop"), "refusal must name the override: {text}");
    let tips = Repo::open(&lab.work).expect("open").bookmark_tips().expect("tips");
    assert!(!tips.keys().any(|r| r.branch().as_str() == "release/2026-08-05"));

    // And when: the operator overrides.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text", "release", "--repo", "demo", "cut", "release/2026-08-05", "--allow-drop",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run overridden cut");
    assert!(
        output.status.success(),
        "override failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_dropped_branch_does_not_trip_the_orphan_gate() {
    // Given: a branch stated out of the release. Its bookmark still holds its
    // content, so nothing is lost and the gate must stay quiet.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    let drop = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "release", "--repo", "demo", "drop", "feat/beta", "--why", "not this time"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("state the drop");
    assert!(drop.status.success());

    // When: the next cut is made without --allow-drop.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "release", "--repo", "demo", "cut", "release/2026-08-05"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run release cut");

    // Then: it cuts, because feat/beta's bookmark still reaches its commits.
    assert!(
        output.status.success(),
        "gate tripped on a stated drop: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
```

- [ ] **Step 2: Add the shared test fixture helper** near the top of `tests/jj_integration.rs` (module scope, after `relation_to_origin`):

```rust
/// Registry home + consumer for release-cut tests: one repo named `demo`,
/// one consumer following the current release by branch.
fn release_test_home(lab: &lab::Lab) -> (tempfile::TempDir, std::path::PathBuf) {
    let consumer = lab.consumer_with_pin_history(
        "pyproject.toml",
        "work = { git = \"https://forge.invalid/acme/work.git\", branch = \"release/2026-08-03\" }\n",
        "work = { git = \"https://forge.invalid/acme/work.git\", branch = \"release/2026-08-04\" }\n",
    );
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nconsumers = [\"{}\"]\n",
            lab.work.display(),
            lab.upstream.display(),
            consumer.display(),
        ),
    )
    .expect("write registry");
    (home, consumer)
}
```

Refactor the Task 6 test (`cutting_a_release_reaps_the_superseded_one`) to use this helper instead of its inline registry.

- [ ] **Step 3: Run:** `cargo test --test jj_integration orphan_gate a_cut_refuses_when_work_lives` — FAIL (the flag does not exist yet; clap rejects `--allow-drop`).

- [ ] **Step 4: Implement.** In `src/cli.rs`, extend `ReleaseAction::Cut`:

```rust
    Cut {
        /// The dated release name. Omit it for a configured fixed release branch.
        name: Option<String>,
        /// Proceed even when commits reachable only from the previous release
        /// lineage would be dropped. The refusal lists exactly what.
        #[arg(long)]
        allow_drop: bool,
    },
```

Thread the flag: `ReleaseInvocation::Cut(Option<String>)` in src/main.rs becomes `ReleaseInvocation::Cut { name: Option<String>, allow_drop: bool }`; `dispatch_release` passes it through. In `src/commands/release.rs` add:

```rust
/// Commits the recut would strand: reachable from the previous release or its
/// local descendants, and from no keeper.
///
/// Keepers are every non-release local bookmark tip plus the upstream trunk —
/// not just release members, because a branch stated out of the release still
/// holds its content through its own bookmark. Parked working copies (empty,
/// undescribed) and the release lineage's own merges (`release:` /
/// `chore(release):` messages) are not content and are excluded.
pub fn orphaned_commits(
    repo_path: &Path,
    previous: &CommitId,
    keep: &[CommitId],
) -> Result<Vec<CommitId>, crate::jj::JjError> {
    let keepers = keep
        .iter()
        .map(|commit| commit.as_str().to_owned())
        .collect::<Vec<_>>()
        .join("|");
    let revset = format!(
        "::( {previous}:: ) ~ ::({keepers}) ~ (empty() & description(exact:\"\")) \
         ~ description(glob:\"release:*\") ~ description(glob:\"chore(release):*\")",
        previous = previous.as_str(),
    );
    crate::jj::commits_matching(repo_path, &revset)
}
```

First add the scheme-aware "previous release" helper to `src/commands/release.rs` (oracle amendment: under `Fixed`, `newest_release` prefers the LOCAL bookmark, but the seam the fixed scheme depends on is the publish remote's ref — `previous_position`, src/commands/release.rs:149):

```rust
/// The release the next cut supersedes: the newest dated cut, or under the
/// fixed scheme the publish remote's current position (the seam
/// `previous_position` reads — the LOCAL fixed bookmark may already have
/// moved and is not what consumers see).
pub fn previous_release_for_cut(
    repo: &Repo,
    entry: &RepoEntry,
    tips: &BookmarkTips,
) -> Option<(String, CommitId)> {
    match entry.release_scheme() {
        ReleaseScheme::Dated => {
            newest_release(tips, &ReleaseScheme::Dated, &entry.publish_remote())
                .map(|(reference, commit)| (reference.to_string(), commit))
        }
        ReleaseScheme::Fixed(_) => previous_position(repo, entry),
    }
}
```

In `run_release` (src/main.rs), before the `release::Cut` request is built (after `carried.insert(0, ...)`, ~line 558), insert:

```rust
            // #4: refuse a cut that strands commits only the old lineage reaches.
            if let Some(previous) = release::previous_release_for_cut(
                &opened,
                &entry,
                &opened.bookmark_tips()?,
            ) {
                let mut keep: Vec<knives::ids::CommitId> = opened
                    .bookmark_tips()?
                    .iter()
                    .filter_map(|(reference, commit)| match reference {
                        knives::ids::BookmarkRef::Local(branch)
                            if !knives::ids::is_release_name(branch, &scheme) =>
                        {
                            Some(commit.clone())
                        }
                        _ => None,
                    })
                    .collect();
                keep.push(trunk.clone());
                let orphans = release::orphaned_commits(&entry.path, &previous.1, &keep)?;
                if !orphans.is_empty() && !allow_drop {
                    println!(
                        "{repo}: refusing to cut: {} commit(s) are reachable only from \
                         {} or its descendants and would be dropped:",
                        orphans.len(),
                        previous.0
                    );
                    for commit in &orphans {
                        println!("    {}", commit.as_str().chars().take(12).collect::<String>());
                    }
                    println!("  re-run with --allow-drop to state this is intended");
                    return Ok(Exit::Incomplete);
                }
                if !orphans.is_empty() {
                    println!(
                        "{repo}: --allow-drop: dropping {} commit(s) from the old lineage",
                        orphans.len()
                    );
                }
            }
```

(`trunk` is the already-resolved upstream trunk commit binding from the surrounding code, src/main.rs:557.)

- [ ] **Step 5: Run:** `cargo test --test jj_integration a_cut_refuses_when_work_lives a_dropped_branch_does_not_trip` — PASS, plus the earlier cut tests still pass (a plain two-branch cut has no orphans: everything in the old lineage is reachable from the branch bookmarks and trunk).

---

### Task 8: post-cut content audit (#4)

**Files:**
- Modify: `src/commands/release.rs` (`cut` splits into `build_cut` + `name_cut`; new `audit_cut`) and `src/jj.rs` (new `duplicate_onto` + `describe_commit` porcelain, #12)
- Modify: `src/main.rs` (`run_release` wiring)
- Test: `tests/jj_integration.rs`

**Interfaces:**
- Consumes: `probe_revision` (new two-line generalization of `probe_landed`, src/jj.rs:396 — see Step 3), `changed_files_between` (src/jj.rs:632), `abandon_commits` (Task 1), `RebaseOutcome` (src/detect/landed.rs), `previous_release_for_cut` (Task 7 amendment).
- Produces:

```rust
pub struct CutAudit {
    /// Branches whose replay onto the cut still carries real work — their
    /// content is NOT in the cut. Each entry fails the cut.
    pub missing: Vec<String>,
    /// Files that differ between the previous release and the cut with no
    /// member or trunk explaining the change. Each entry fails the cut.
    pub unexplained: Vec<String>,
    /// Branches whose replay conflicted: not judged either way (the cut
    /// carries expected conflicts; content presence is checked after resolution).
    pub inconclusive: Vec<String>,
}

impl CutAudit {
    pub fn passed(&self) -> bool {
        self.missing.is_empty() && self.unexplained.is_empty()
    }
}

pub fn build_cut(repo: &Path, request: &Cut) -> anyhow::Result<CommitId>      // create + parent-count check
pub fn name_cut(repo: &Path, name: &str, commit: &CommitId, scheme: &ReleaseScheme) -> anyhow::Result<()>
pub fn audit_cut(
    repo: &Path,
    members: &[(String, CommitId)],   // carried branches: (bookmark name, tip) — NOT the trunk entry
    cut: &CommitId,
    previous: Option<&CommitId>,
    trunk: &CommitId,
) -> anyhow::Result<CutAudit>
```

Semantics (spec §1.3), two independent checks:

1. **Hunk presence** (primary; catches the moved-file and uv.lock cases): for each member branch, replay the member's NET diff onto the cut — squash the range `{trunk}..{member_tip}` (trunk = the upstream trunk commit carried as parent 0) into one synthetic commit and probe THAT against `cut.as_str()` (`probe_net_diff`). `Empty` → every hunk already present in the cut's TREE. `CleanNonEmpty` → the cut's tree silently lacks this branch's hunks → `missing`. `Conflicted` → depends on the cut itself: if the cut has its own conflicts (`conflicted_files` non-empty), the answer is genuinely ambiguous → `inconclusive`, reported but not fatal; if the cut is conflict-free, the cut holds a DIFFERENT version of files this member touched — divergence, the uv.lock case — and it FAILS the audit like `missing`. Per-commit replay is forbidden: an intermediate commit replayed against a tree already holding the final content manufactures an add/add conflict, mislabeling faithful multi-commit members. For stacked members the net range also carries the prefix branch's hunks; that is correct (those hunks must be present too). (Amended 2026-08-06: the original prescription replayed `cut..member_tip`, which is an ANCESTRY range — in `run_release` every member tip is a parent of the cut, so the range was always empty and the check could never fire, even on a cut whose tree physically lost a member's file. Anchoring at trunk makes the probe compare TREES, which is what spec §1.3 demanded. Review of Task 8 caught this; the defect was the plan's, not the implementation's.)
2. **Unexplained drift** (second net, coarse): `changed(previous → cut)` minus the union of `changed(previous → member_i)` for every member and `changed(previous → trunk)`. A file the merge changed that no input changed is the merge inventing or losing content on its own. Skipped when there is no previous release.

Ordering in `run_release`: `build_cut` → `audit_cut` → on pass `name_cut` (+ everything downstream unchanged); on fail: `abandon_commits(&entry.path, &[created])`, print the findings, `Exit::Incomplete`. The audit runs BEFORE the bookmark exists, so a failed cut leaves no trace but the printed report.

- [ ] **Step 1: Write the failing integration tests:**

```rust
#[test]
fn the_audit_catches_a_cut_missing_a_members_content() {
    // Given: a real cut built while quietly leaving one member out of the
    // parents — the silent-loss shape (everything compiles, nothing conflicts).
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let trunk = repo.resolve_commit("main@upstream").expect("trunk");
    let alpha = repo.resolve_commit("feat/alpha").expect("alpha");
    let beta = repo.resolve_commit("feat/beta").expect("beta");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![trunk.clone(), alpha.clone()], // beta dropped
        provenance: vec![
            (trunk.clone(), "main@upstream".to_owned()),
            (alpha.clone(), "feat/alpha".to_owned()),
        ],
    };
    let created = knives::commands::release::build_cut(&lab.work, &request, None).expect("build");

    // When: the cut is audited against what SHOULD have been carried.
    let members = vec![
        ("feat/alpha".to_owned(), alpha),
        ("feat/beta".to_owned(), beta),
    ];
    let audit = knives::commands::release::audit_cut(&lab.work, &members, &created, None, &trunk)
        .expect("audit");

    // Then: the missing member is named; the present one is not.
    assert_eq!(audit.missing, vec!["feat/beta".to_owned()]);
    assert!(audit.unexplained.is_empty());
    assert!(!audit.passed());
}

#[test]
fn the_audit_passes_a_faithful_cut() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let trunk = repo.resolve_commit("main@upstream").expect("trunk");
    let alpha = repo.resolve_commit("feat/alpha").expect("alpha");
    let beta = repo.resolve_commit("feat/beta").expect("beta");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![trunk.clone(), alpha.clone(), beta.clone()],
        provenance: vec![
            (trunk.clone(), "main@upstream".to_owned()),
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let created = knives::commands::release::build_cut(&lab.work, &request, None).expect("build");
    let members = vec![
        ("feat/alpha".to_owned(), alpha),
        ("feat/beta".to_owned(), beta),
    ];
    let audit = knives::commands::release::audit_cut(&lab.work, &members, &created, None, &trunk)
        .expect("audit");
    assert!(audit.passed(), "{audit:?}");
}
```

Derive `Debug` on `CutAudit` for the assertion message.

- [ ] **Step 2: Run:** `cargo test --test jj_integration the_audit_` — FAIL: `build_cut` not found.

- [ ] **Step 3: Implement** in `src/commands/release.rs`. Split the existing `cut` (line 543):

```rust
/// Build the candidate cut and verify it has exactly the parents asked for.
/// Public seam so the audit can run between creation and naming.
///
/// Incremental by default (#12): a jj merge records its tree as a
/// resolution-diff against the auto-merge of its parents, so DUPLICATING the
/// previous release onto the new parent set preserves every prior conflict
/// resolution — adding a branch or advancing the base surfaces zero old
/// conflicts, and dropping a branch surfaces one focused conflict exactly
/// where its content was baked into a resolution (verified empirically;
/// silently shipping the dropped branch's lines is #4's inverse). A fresh
/// flat merge is built only when no previous release exists. Both paths
/// produce a flat octopus of exactly `request.parents`.
pub fn build_cut(
    repo: &Path,
    request: &Cut,
    previous: Option<&CommitId>,
) -> anyhow::Result<CommitId> {
    let created = match previous {
        Some(previous) => {
            let duplicated =
                crate::jj::duplicate_onto(repo, previous, &request.parents)?;
            // describe rewrites the commit id; use the id that carries the
            // message.
            crate::jj::describe_commit(repo, &duplicated, &request.message())?
        }
        None => crate::jj::create_merge(repo, &request.parents, &request.message())?,
    };
    let actual = Repo::open(repo)?.parents_of(created.as_str())?;
    anyhow::ensure!(
        actual.len() == request.parents.len(),
        "cut {} came out with {} parents, expected {}; refusing to name it",
        request.name,
        actual.len(),
        request.parents.len()
    );
    Ok(created)
}

// New jj porcelain in src/jj.rs (beside create_merge), same command/style
// conventions as its neighbors:
//
// /// Duplicate `source` onto a new parent set, carrying its resolution-diff.
// ///
// /// The primitive behind incremental cuts and rebase repairs (#12): the
// /// duplicate's tree is automerge(new parents) + source's recorded
// /// resolutions, so operators never re-resolve what a prior cut resolved.
// pub fn duplicate_onto(repo: &Path, source: &CommitId, parents: &[CommitId])
//     -> Result<CommitId, JjError>
// {
//     // jj --repository <repo> --ignore-working-copy duplicate -r <source>
//     //    -d <p1> -d <p2> ... ; parse "Duplicated <old> as <change> <commit>"
//     //    from stderr (mirror parse_duplicated, which handles the same shape),
//     //    then widen the short id via the same `jj log -T commit_id` trick
//     //    create_merge uses.
// }
//
// /// Rewrite a commit's description and return the REWRITTEN commit id.
// ///
// /// `jj describe -r <commit>` rewrites the commit; returning the old id
// /// would hand callers a stale handle (a bookmark set on it would pin the
// /// pre-describe copy). Parse the new id from jj's stderr ("Rewrote ..."), or
// /// re-resolve via the change id; widen short ids the way create_merge does.
// pub fn describe_commit(repo: &Path, commit: &CommitId, message: &str)
//     -> Result<CommitId, JjError>
// The parent-count read-back in build_cut re-opens the repo and so verifies
// the returned id is the live one.

/// Point the release name at an already-checked merge.
pub fn name_cut(
    repo: &Path,
    name: &str,
    commit: &CommitId,
    scheme: &ReleaseScheme,
) -> anyhow::Result<()> {
    match scheme {
        ReleaseScheme::Dated => crate::jj::set_bookmark(repo, name, commit.as_str())?,
        ReleaseScheme::Fixed(_) => {
            crate::jj::set_bookmark_anywhere(repo, name, commit.as_str())?;
        }
    }
    Ok(())
}

/// Make the cut, after checking it. Kept as the one-call form for callers
/// that do not audit; `run_release` uses the split seam. Passing `None` for
/// previous keeps existing direct callers (tests) on the from-scratch path.
pub fn cut(repo: &Path, request: &Cut, scheme: &ReleaseScheme) -> anyhow::Result<CommitId> {
    let created = build_cut(repo, request, None)?;
    name_cut(repo, &request.name, &created, scheme)?;
    Ok(created)
}
```

(Doc comments from the current `cut` move to `build_cut`/`cut` as appropriate; behavior is byte-identical for `cut` callers.) Then the audit:

```rust
/// Verify the cut actually contains what it merged (spec 1.3).
///
/// Hunk presence per member via the landed probe pointed at the fresh cut:
/// an Empty replay means the branch's changes are byte-present; a clean
/// non-empty replay means the merge silently lacks them — the auto-merge
/// picking the wrong side of a moved file or a lockfile raises no conflict,
/// and this is the check that catches it. A conflicted replay is reported,
/// not judged: the cut's own expected conflicts make the answer ambiguous.
///
/// Drift: a file that differs between the previous release and the cut with
/// no member and no trunk changing it was changed by the merge itself.
pub fn audit_cut(
    repo: &Path,
    members: &[(String, CommitId)],
    cut: &CommitId,
    previous: Option<&CommitId>,
    trunk: &CommitId,
) -> anyhow::Result<CutAudit> {
    use crate::detect::landed::RebaseOutcome;
    let mut audit = CutAudit {
        missing: Vec::new(),
        unexplained: Vec::new(),
        inconclusive: Vec::new(),
    };
    // Oracle amendment: probe the CAPTURED tip, never the bookmark name — a
    // branch moving between carried_branches and the audit would otherwise be
    // judged at the wrong commit. `name` is for reporting only.
    // Amendment 2026-08-06 (2): replay each member's NET diff — squash the range
    // {trunk}..{member_tip} into ONE synthetic commit and replay that onto the cut.
    // Replaying individual commits manufactures conflicts on ordinary multi-commit
    // members (an intermediate commit replayed against a tree already holding the
    // final content is an add/add conflict), which mislabels faithful cuts.
    // Classification (empirically calibrated, see review of Task 8 fix wave):
    //   net replay Empty            -> present
    //   net replay CleanNonEmpty    -> missing (content absent from the cut's tree)
    //   net replay Conflicted, cut itself conflict-free -> DIVERGED: the cut holds a
    //     different version of files this member touched (the uv.lock case) -> fails
    //     the audit exactly like missing
    //   cut itself conflicted (conflicted_files non-empty) -> inconclusive for members
    //     whose replay conflicted — the only case where the answer is genuinely ambiguous
    // Never classify by the head duplicate alone: on a faithful multi-commit member the
    // head duplicate is CleanNonEmpty (false missing).
    for (name, tip) in members {
        match crate::jj::probe_net_diff(repo, trunk.as_str(), tip.as_str(), cut.as_str())? {
            RebaseOutcome::Empty => {}
            RebaseOutcome::CleanNonEmpty => audit.missing.push(name.clone()),
            RebaseOutcome::Conflicted if cut_is_conflicted => audit.inconclusive.push(name.clone()),
            RebaseOutcome::Conflicted => audit.missing.push(name.clone()), // diverged; report the reason in the message
        }
    }
    if let Some(previous) = previous {
        let drifted =
            crate::jj::changed_files_between(repo, previous.as_str(), cut.as_str())?;
        let mut explained = std::collections::BTreeSet::new();
        for (_, tip) in members {
            explained.extend(crate::jj::changed_files_between(
                repo,
                previous.as_str(),
                tip.as_str(),
            )?);
        }
        explained.extend(crate::jj::changed_files_between(
            repo,
            previous.as_str(),
            trunk.as_str(),
        )?);
        audit.unexplained = drifted
            .into_iter()
            .filter(|file| !explained.contains(file))
            .collect();
    }
    Ok(audit)
}
```

`probe_net_diff` in `src/jj.rs`: build the synthetic net commit DIRECTLY — `jj new --no-edit -r {base}`, then `jj restore --from {revision} --into <synthetic>` (parent = base, tree = revision's tree: the net diff by construction; track the synthetic with the probe bookmark because restore rewrites its id) — then duplicate/replay that single commit onto the target, classify (empty / clean-non-empty / conflicted), and abandon all scratch commits. Squash-of-duplicated-range is FORBIDDEN as the net mechanism (amendment 2026-08-06 (3)): `jj squash --from <older> --into <newest>` re-applies earlier diffs over later ones, resurrecting paths a later commit deleted (add-then-delete, add-then-rename), which reports a faithful cut as missing and abandons it. `probe_landed` keeps its existing per-range semantics (its callers probe `onto..branch`, which is what landed-detection means); the audit uses the net-diff probe. (Amendment 2026-08-06 (2): the earlier sketch here — two-arg `probe_revision`, `{onto}..{revision}` — was the inert-check design; the architecture line at the top of this plan saying the audit "reuses `probe_landed`" is superseded by this.)

Two additional lock tests (amendment 2026-08-06 (2); shapes strengthened by amendment (3)):
- A faithful cut carrying a multi-commit member (two commits touching the SAME file) audits with NO `inconclusive` and passes. PLUS the net-mechanism shapes: a member that adds a file then DELETES it (net zero → audit passes) and a member that adds a file then RENAMES it (net = the new path only → audit passes) — these are the shapes squash-of-duplicates gets wrong.
- A regenerated-lockfile partial loss (member adds `uv.lock` with two entries; the cut's tree holds a different regenerated version) FAILS the audit (`!passed()`), with the member named.

Add a third integration test pinning the amendment: build the cut, MOVE `feat/beta`'s bookmark, then audit with the ORIGINAL captured tips — the audit judges the captured commits, not wherever the bookmark went:

```rust
#[test]
fn the_audit_judges_the_captured_tip_not_the_moved_bookmark() {
    // Given: a faithful cut, after which a member's bookmark moves on.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let trunk = repo.resolve_commit("main@upstream").expect("trunk");
    let alpha = repo.resolve_commit("feat/alpha").expect("alpha");
    let beta = repo.resolve_commit("feat/beta").expect("beta");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![trunk.clone(), alpha.clone(), beta.clone()],
        provenance: vec![
            (trunk.clone(), "main@upstream".to_owned()),
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let created = knives::commands::release::build_cut(&lab.work, &request, None).expect("build");
    // Move the bookmark FORWARD (new child), not an in-place rewrite: an amend
    // would auto-rebase the cut merge itself and change `created`'s id. A
    // forward move also discriminates: a name-based probe would see the new
    // commit as missing content and fail; the captured-tip probe must not.
    lab.jj_work(["new", "feat/beta", "-m", "beta moves on"]);
    std::fs::write(lab.work_path().join("beta2.txt"), "more\n").expect("write");
    lab.jj_work(["bookmark", "set", "feat/beta", "-r", "@"]);
    lab.jj_work(["new"]);

    // When: audited with the tips captured at plan time.
    let members = vec![
        ("feat/alpha".to_owned(), alpha),
        ("feat/beta".to_owned(), beta),
    ];
    let audit = knives::commands::release::audit_cut(&lab.work, &members, &created, None, &trunk)
        .expect("audit");

    // Then: still passing — the moved bookmark is invisible to the audit.
    assert!(audit.passed(), "{audit:?}");
}
```

And two tests for the incremental construction itself (#12) — the resolution-preservation property and the dropped-branch focused conflict:

```rust
#[test]
fn an_incremental_recut_preserves_the_previous_cuts_conflict_resolutions() {
    // Given: two branches editing the SAME region, merged into a cut whose
    // conflict the operator resolved in place, plus a third branch to add.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha side\n");
    lab.branch("feat/beta", "shared.txt", "beta side\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    // Resolve the cut's conflict the way an operator does: edit the merge.
    lab.jj_work(["edit", "release/2026-08-04"]);
    std::fs::write(lab.work_path().join("shared.txt"), "resolved union\n").expect("resolve");
    lab.jj_work(["new"]); // park @ off the merge; the resolution is snapshotted
    let repo = Repo::open(&lab.work).expect("open");
    let previous = repo.resolve_commit("release/2026-08-04").expect("previous");
    let trunk = repo.resolve_commit("main@upstream").expect("trunk");
    let alpha = repo.resolve_commit("feat/alpha").expect("alpha");
    let beta = repo.resolve_commit("feat/beta").expect("beta");
    let gamma = repo.resolve_commit("feat/gamma").expect("gamma");

    // When: the next cut adds gamma, built incrementally from the previous.
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![trunk.clone(), alpha.clone(), beta.clone(), gamma.clone()],
        provenance: vec![
            (trunk.clone(), "main@upstream".to_owned()),
            (alpha, "feat/alpha".to_owned()),
            (beta, "feat/beta".to_owned()),
            (gamma, "feat/gamma".to_owned()),
        ],
    };
    let created =
        knives::commands::release::build_cut(&lab.work, &request, Some(&previous))
            .expect("incremental build");

    // Then: the resolution survived verbatim, gamma's content arrived, and no
    // conflict resurfaced — the whole point of #12.
    let resolved = knives::jj::output_at_revision(&lab.work, created.as_str(), "cat shared.txt")
        .expect("read shared.txt");
    assert!(resolved.contains("resolved union"), "resolution lost: {resolved}");
    let conflicts =
        knives::jj::conflicted_files(&lab.work, created.as_str()).expect("conflicts");
    assert!(conflicts.is_empty(), "old conflicts resurfaced: {conflicts:?}");
    // And: the message is the NEW cut's, not the duplicated one's.
    // (read description via lab.revision)
    let description = lab.revision(lab.work_path(), created.as_str(), "description");
    assert!(description.contains("release/2026-08-05"), "{description}");
}

#[test]
fn dropping_a_resolved_branch_surfaces_a_focused_conflict_not_silence() {
    // Given: the same resolved cut; the next cut DROPS beta, whose content is
    // baked into the resolution. Silence would ship beta's lines in a
    // beta-less release (#4's inverse); a conflict demands the human call.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha side\n");
    lab.branch("feat/beta", "shared.txt", "beta side\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work(["edit", "release/2026-08-04"]);
    std::fs::write(lab.work_path().join("shared.txt"), "resolved union\n").expect("resolve");
    lab.jj_work(["new"]);
    let repo = Repo::open(&lab.work).expect("open");
    let previous = repo.resolve_commit("release/2026-08-04").expect("previous");
    let trunk = repo.resolve_commit("main@upstream").expect("trunk");
    let alpha = repo.resolve_commit("feat/alpha").expect("alpha");

    // When: beta is dropped from the parent set.
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![trunk.clone(), alpha.clone()],
        provenance: vec![
            (trunk, "main@upstream".to_owned()),
            (alpha, "feat/alpha".to_owned()),
        ],
    };
    let created =
        knives::commands::release::build_cut(&lab.work, &request, Some(&previous))
            .expect("incremental build");

    // Then: exactly the entangled file conflicts — reported, never silent.
    let conflicts =
        knives::jj::conflicted_files(&lab.work, created.as_str()).expect("conflicts");
    assert_eq!(conflicts, vec!["shared.txt".to_owned()], "{conflicts:?}");
}
```

NOTE for the implementer: `lab.jj_work(["edit", "release/2026-08-04"])` moves @ ONTO the merge and the file write + next jj command snapshots the resolution into it — that is the operator's real workflow (edit in place, no throwaway commits). The bookmark stays on the rewritten merge. If `octopus` leaves @ elsewhere, adjust with the fixture's actual shape rather than fighting it.

Check `crate::detect::landed` visibility: `RebaseOutcome` is imported in src/jj.rs via `crate::detect::landed::RebaseOutcome`, so the module is reachable; if `landed` is not `pub` in src/detect.rs, re-export `RebaseOutcome` there (`pub use landed::RebaseOutcome;` may already exist — check `src/detect.rs`; status.rs imports `LandedVerdict` from `crate::detect`, so add `RebaseOutcome` to that existing re-export list if needed).

- [ ] **Step 4: Wire into `run_release`** (src/main.rs). Replace `let created = release::cut(&entry.path, &request, &scheme)?;` (~line 567) with:

```rust
            // Oracle amendments: scheme-aware previous (the fixed seam is the
            // publish remote's ref, not the local bookmark), and an audit ERROR
            // must abandon the candidate too — nothing may survive unnamed.
            // #12: previous is computed BEFORE the build — it now also selects
            // the incremental (duplicate-based) construction path.
            let previous_commit = release::previous_release_for_cut(
                &opened,
                &entry,
                &opened.bookmark_tips()?,
            )
            .map(|(_, commit)| commit);
            let created =
                release::build_cut(&entry.path, &request, previous_commit.as_ref())?;
            // #4: verify the merge kept every member's content BEFORE naming it.
            let member_tips: Vec<(String, knives::ids::CommitId)> = carried
                .iter()
                .skip(1) // the trunk entry inserted above is not a member
                .cloned()
                .collect();
            let audit = match release::audit_cut(
                &entry.path,
                &member_tips,
                &created,
                previous_commit.as_ref(),
                &trunk,
            ) {
                Ok(audit) => audit,
                Err(error) => {
                    let _ = knives::jj::abandon_commits(
                        &entry.path,
                        std::slice::from_ref(&created),
                    );
                    return Err(error);
                }
            };
            for name in &audit.inconclusive {
                println!(
                    "  {name}: content check inconclusive (replay conflicted; \
                     re-check after resolving the cut's conflicts)"
                );
            }
            if !audit.passed() {
                for name in &audit.missing {
                    println!("  !! {name}: its changes are NOT in the cut tree");
                }
                for file in &audit.unexplained {
                    println!(
                        "  !! {file}: changed between the previous release and this cut \
                         with no member or trunk explaining it"
                    );
                }
                knives::jj::abandon_commits(&entry.path, std::slice::from_ref(&created))?;
                println!(
                    "{repo}: cut abandoned; nothing was named or pushed. Fix the inputs and re-cut."
                );
                return Ok(Exit::Incomplete);
            }
            release::name_cut(&entry.path, &request.name, &created, &scheme)?;
```

The pre-cut gate (Task 7) already captured `newest_release` — reuse that binding (`previous`) instead of recomputing if it is in scope; otherwise compute as shown.

- [ ] **Step 5: Run:** `cargo test --test jj_integration the_audit_ && cargo test --test jj_integration a_cut_ && cargo test --test jj_integration cutting_a_release` — PASS. The `a_cut_is_flat_and_carries_its_provenance` and `a_cut_refuses_when_the_merge_did_not_get_the_parents_it_asked_for` tests call `release::cut` directly and must keep passing unchanged.

---

### Task 9: shared base — `knives start` bases on it (#10)

**Files:**
- Modify: `src/commands/release.rs` (new `shared_base`), `src/commands/start.rs` (base selection)
- Test: `tests/jj_integration.rs`

**Interfaces:**
- Consumes: `Repo::is_ancestor`, `Repo::parents_of`, `newest_release` (Task 3).
- Produces: `pub fn shared_base(repo: &Repo, release: &CommitId, trunk_tip: &CommitId) -> anyhow::Result<Option<CommitId>>` — the newest trunk-reachable parent of the release: among parents P with `is_ancestor(P, trunk_tip)`, the one every other such parent can reach (`is_ancestor(other, candidate)`). `None` when no parent is trunk-reachable.

- [ ] **Step 1: Write the failing integration test:**

```rust
#[test]
fn start_bases_a_new_branch_on_the_shared_base_not_the_advanced_upstream() {
    // Given: a release whose members fork from today's trunk, then upstream advances.
    // Basing new work on the advanced tip would drag that advance into the next
    // cut through one member — the mixed-base conflict storm (#10).
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let repo = Repo::open(&lab.work).expect("open");
    let base_before_advance = repo.resolve_commit("main@origin").expect("resolve base");
    lab.advance_upstream("upstream advance\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: a branch is started through the binary.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "start", "feat/gamma", "--repo", "demo", "--why", "test"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run start");
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: the workspace's @ sits on the shared base, not the advanced tip.
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    let parent = lab.revision(&workspace, "@-", "commit_id");
    assert_eq!(
        parent,
        base_before_advance.as_str(),
        "based on {parent}, expected the shared base"
    );
}
```

The existing `a_new_workspace_is_based_on_the_upstream_trunk_not_the_current_change` (tests/jj_integration.rs:763) covers the no-release fallback; verify it still passes untouched after this task.

- [ ] **Step 2: Run:** — FAIL: parent equals the advanced upstream tip.

- [ ] **Step 3: Implement.** In `src/commands/release.rs`:

```rust
/// The shared base: the newest trunk-reachable parent of the newest release.
///
/// Every member branch forks from one base commit; that is the invariant (#10).
/// A release merge carries that base as a parent (the octopus's trunk input),
/// so it is recoverable from the release itself. When the release also carries
/// older accumulated bases (#11's damage), the newest one — the one every other
/// trunk-reachable parent can reach — is the base in force.
pub fn shared_base(
    repo: &Repo,
    release: &CommitId,
    trunk_tip: &CommitId,
) -> anyhow::Result<Option<CommitId>> {
    let parents = repo.parents_of(release.as_str())?;
    let mut bases = Vec::new();
    for parent in &parents {
        if repo.is_ancestor(&parent.commit, trunk_tip)? {
            bases.push(parent.commit.clone());
        }
    }
    let mut newest: Option<CommitId> = None;
    'candidates: for candidate in &bases {
        for other in &bases {
            if other != candidate && !repo.is_ancestor(other, candidate)? {
                continue 'candidates;
            }
        }
        newest = Some(candidate.clone());
        break;
    }
    Ok(newest)
}
```

In `src/commands/start.rs`, replace the base selection (`let upstream_trunk = entry.upstream_trunk();` at line 28 stays; the change is what gets passed to `add_workspace`):

```rust
    fetch_all(&entry.path)?;
    // The shared base every member forks from, when a release exists to name
    // one; the fetched upstream trunk only when nothing does. Basing on the
    // trunk tip while siblings sit on an older base drags newer upstream into
    // the next cut through this one branch (#10) — the fix for the accident
    // this comment used to describe now lives one level up.
    let opened = crate::jj::Repo::open(&entry.path)?;
    let tips = opened.bookmark_tips()?;
    let scheme = entry.release_scheme();
    let base = crate::commands::release::newest_release(&tips, &scheme, &entry.publish_remote())
        .and_then(|(_, release)| {
            let trunk_tip = opened.resolve_commit(&upstream_trunk).ok()?;
            crate::commands::release::shared_base(&opened, &release, &trunk_tip).ok()?
        });
    let (base_revision, base_label) = match &base {
        Some(commit) => (commit.as_str().to_owned(), "the release's shared base"),
        None => (upstream_trunk.clone(), "the fetched upstream trunk"),
    };
    add_workspace(
        &entry.path,
        &branch.as_str().replace('/', "-"),
        &destination,
        &base_revision,
    )?;
```

and update the final `println!` to `"workspace {} based on {base_revision} ({base_label})\nclaimed ..."`. Keep the original comment's warning about `jj new` inheriting a release merge — it is still the reason the base is never the current `@`.

- [ ] **Step 4: Run:** both start-basing tests PASS; `cargo clippy --all-targets -- -D warnings`.

---

### Task 10: mixed-base finding + release-parent classification (#10)

**Files:**
- Modify: `src/detect.rs` (two `FindingKind` variants), `src/commands/release.rs` (classification in `plan()`, new `mixed_base_findings`), `src/commands/preflight.rs` (report them), `src/main.rs` (render path already prints `plan.stale`; extend)
- Test: `tests/jj_integration.rs` + unit tests

**Interfaces:**
- Consumes: `shared_base` (Task 9), `commits_matching` (Task 1), `Finding`/`Subject` (src/detect.rs).
- Produces:
  - `FindingKind::MixedBase` (display: `"mixed-base"`) and `FindingKind::SupersededBase` (display: `"superseded-base"`), added to the enum (src/detect.rs:24) and its `Display` impl.
  - `pub fn mixed_base_findings(repo_path: &Path, members: &[(String, CommitId)], base: &CommitId, trunk_tip: &CommitId) -> Result<Vec<Finding>, crate::jj::JjError>` — for each member, `commits_matching(repo_path, &format!("(::{tip} & ::{trunk}) ~ ::{base}"))`; non-empty → `Finding::new(FindingKind::MixedBase, Subject::Branch(name), "branch {name} carries {n} trunk commit(s) beyond the shared base {base12}; it is based on a different upstream than its siblings")`.
  - `Plan` gains `pub base: Option<CommitId>` and `pub base_findings: Vec<Finding>`; `plan()` classifies parents: trunk-reachable parents that are not the newest one get a `SupersededBase` finding (`"parent {commit12} is an older upstream base superseded by {base12}; `knives release rebase` self-heals this"`); ONLY non-trunk-reachable parents are passed to `stale_parents` (the base is legitimately bookmarkless — the field report's false positive); member mixed-base findings appended. `render()` prints `base_findings` under the parents block; `exit_for` returns `Findings` when `base_findings` is non-empty (same arm as `stale`).

`stale_parents` (src/detect/stale_parents.rs) itself stays pure and unchanged — the classification happens where the repo is available, in `plan()`.

- [ ] **Step 1: Write the failing integration test:**

```rust
#[test]
fn the_base_parent_is_not_stale_and_a_drifted_member_is_a_mixed_base_finding() {
    // Given: a release whose first parent is the (bookmarkless) shared base, and
    // one member re-based past it onto the advanced upstream.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.advance_upstream("upstream advance\n");
    lab.rebase_and_force_push("feat/beta"); // now based past the shared base
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is planned.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "release", "--repo", "demo"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run release plan");
    let text = String::from_utf8_lossy(&output.stdout).to_string();

    // Then: the bookmarkless base is NOT reported as a stale parent...
    assert!(
        !text.contains("carries no bookmark"),
        "base parent misread as stale: {text}"
    );
    // ...the drifted member is named as a mixed base...
    assert!(
        text.contains("feat/beta") && text.contains("beyond the shared base"),
        "mixed base not reported: {text}"
    );
    // ...and feat/alpha (still on the base) is not.
    assert!(
        !text.contains("feat/alpha carries"),
        "well-based member misreported: {text}"
    );
}
```

Note `lab.rebase_and_force_push` (tests/common/lab.rs:196) rebases the branch onto `main@upstream` and pushes. (Corrected 2026-08-06 by Task 10 review: `jj rebase -b` auto-rebases the descendant release merge too, so this fixture does NOT produce a stale parent — the real output is `every parent is still its branch tip`. The fixture is weaker than originally believed; the mixed-base and base-not-flagged assertions are what it locks, and Task 10's fix wave adds the bookmarkless-base and superseded-base fixtures that lock the parent-classification half.)

- [ ] **Step 2: Run:** — FAIL (the plan reports the base as a parent nothing points at).

- [ ] **Step 3: Implement** exactly the Interfaces list above. In `plan()` (src/commands/release.rs:256), after `let parents = repo.parents_of(commit.as_str())?;` replace the `plan.stale = stale_parents(&parents, &tips);` line with:

```rust
    let trunk_tip = repo.resolve_commit(&entry.upstream_trunk()).ok();
    let base = match &trunk_tip {
        Some(trunk) => shared_base(&repo, &commit, trunk)?,
        None => None,
    };
    plan.base = base.clone();
    let mut member_parents = Vec::new();
    for parent in &parents {
        let trunk_reachable = match &trunk_tip {
            Some(trunk) => repo.is_ancestor(&parent.commit, trunk)?,
            None => false,
        };
        if !trunk_reachable {
            member_parents.push(parent.clone());
        } else if base.as_ref().is_some_and(|b| b != &parent.commit) {
            plan.base_findings.push(Finding::new(
                FindingKind::SupersededBase,
                Subject::Commit(parent.commit.clone()),
                format!(
                    "parent {} is an older upstream base superseded by {}; \
                     `knives release rebase` self-heals this",
                    short(&parent.commit),
                    base.as_ref().map_or_else(String::new, short),
                ),
            ));
        }
    }
    plan.stale = stale_parents(&member_parents, &tips);
    if let (Some(base), Some(trunk)) = (&base, &trunk_tip) {
        let members: Vec<(String, CommitId)> = carried_from_tips(&tips, entry.trunk(), &scheme)
            .into_iter()
            .collect();
        plan.base_findings
            .extend(mixed_base_findings(&entry.path, &members, base, trunk)?);
    }
```

with a local `fn short(commit: &CommitId) -> String { commit.as_str().chars().take(12).collect() }` helper, `FindingKind`/`Subject` imported from `crate::detect` (extend the existing import line 12). NOTE: `plan()` currently takes `entry: &RepoEntry` — it does (`entry.release_scheme()` etc.) and has `entry.path` for the repo path. `mixed_base_findings` goes right below `shared_base`:

```rust
/// Members whose trunk ancestry exceeds the shared base (#10).
///
/// A member based past the base drags newer upstream into the next cut through
/// itself alone, which surfaces as a conflict storm blamed on everything else.
/// The finding names the branch so the fix (rebase it onto the base, or move
/// the base deliberately) happens before the cut.
pub fn mixed_base_findings(
    repo_path: &Path,
    members: &[(String, CommitId)],
    base: &CommitId,
    trunk_tip: &CommitId,
) -> Result<Vec<Finding>, crate::jj::JjError> {
    let mut findings = Vec::new();
    for (name, tip) in members {
        let beyond = crate::jj::commits_matching(
            repo_path,
            &format!(
                "(::{tip} & ::{trunk}) ~ ::{base}",
                tip = tip.as_str(),
                trunk = trunk_tip.as_str(),
                base = base.as_str()
            ),
        )?;
        if !beyond.is_empty() {
            findings.push(Finding::new(
                FindingKind::MixedBase,
                Subject::Branch(crate::ids::BranchName::new(name)),
                format!(
                    "branch {name} carries {} trunk commit(s) beyond the shared base {}; \
                     it is based on a different upstream than its siblings",
                    beyond.len(),
                    base.as_str().chars().take(12).collect::<String>()
                ),
            ));
        }
    }
    Ok(findings)
}
```

`render()` (release.rs:334): after the stale block, add:

```rust
    for finding in &plan.base_findings {
        lines.push(format!("  !! {}", finding.detail));
    }
```

`exit_for` (release.rs:390): `if plan.stale.is_empty() && plan.base_findings.is_empty() { Exit::Ok } else { Exit::Findings }`.

**Preflight:** in `src/commands/preflight.rs`'s `gather`, alongside the divergent-changes call (line 251), compute the same base + `mixed_base_findings` and extend the report's findings (mirror how `divergent_changes` results are folded in; the entry and repo handle are both in scope there). Follow the local pattern for where findings accumulate.

- [ ] **Step 4: Run:** the new test PASSES; `a_stranded_release_parent_reports_where_the_branch_went` (line 1613) still passes — its stranded parent is a member parent, not the base. Fix any test that asserted the base's "carries no bookmark" line (search the test file for that string).

---

### Task 11: rebase replaces the base and refuses stale parents (#11)

**Files:**
- Modify: `src/main.rs` (`run_rebase`, line 186)
- Modify: `tests/jj_integration.rs` (existing test's expectations + two new tests)

**Interfaces:**
- Consumes: `Repo::is_ancestor`, `Repo::resolve_commit`, `branches_past` (src/jj.rs:931), `create_merge`, `set_bookmark_anywhere`.
- Produces: `run_rebase` with replace-not-accumulate semantics:
  1. **Already-contains by ancestry:** `opened.is_ancestor(&onto, &release_commit)?` where `release_commit = opened.resolve_commit(&release_name)?` — replaces the direct-parent identity scan (src/main.rs:201).
  2. **Partition parents (oracle-amended order):** FIRST, a parent held by a live branch bookmark — any bookmark still pointing at it whose branch is neither a release name nor the trunk — is KEPT, even when `onto` already reaches it: a landed branch remains a member with its parent and provenance intact (dropping members is `release drop`'s job, never the rebase's). SECOND, an unheld parent with `is_ancestor(parent, onto)` is a superseded base → dropped (replaced by `onto`); several can match when the merge carries accumulated bases, and replacing them all is the self-heal.
  3. **Stale-parent refusal:** an unheld parent not reachable from `onto` cannot be mapped to anything current: refuse with the parent id, where its branches went (`branches_past`), and the instruction to fix the branch or drop it first. Exit `Incomplete`.
  4. Carried = kept parents + `onto`; merge and `set_bookmark_anywhere` as today.

- [ ] **Step 1: Update the existing test's expectations.** `release_rebase_repairs_a_followed_dated_release_with_a_sideways_merge` (tests/jj_integration.rs:1359) currently asserts the accumulation bug: `parents.len() == previous_parents.len() + 1` and every original parent kept — including the old base. Rewrite the Then-block:

```rust
    // Then: the command succeeds; the old base is REPLACED by the new upstream
    // commit, and the branch parents are kept. Same count, not one more: a
    // rebase that only ever adds parents grows the octopus forever (#11).
    assert!(
        output.status.success(),
        "release rebase failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let repo = Repo::open(&lab.work).expect("reopen repaired release repository");
    let parents = repo.parents_of(release).expect("read repaired release parents");
    assert_eq!(parents.len(), previous_parents.len(), "was: {parents:?}");
    assert!(
        parents.iter().any(|parent| parent.commit == upstream),
        "upstream parent missing: {parents:?}"
    );
    let old_base = &previous_parents[0]; // lab.octopus puts main@origin first
    assert!(
        !parents.iter().any(|actual| actual.commit == old_base.commit),
        "superseded base still a parent: {parents:?}"
    );
    for parent in previous_parents.iter().skip(1) {
        assert!(
            parents.iter().any(|actual| actual.commit == parent.commit),
            "branch parent {} missing from {parents:?}",
            parent.commit
        );
    }
```

- [ ] **Step 2: Add the two new failing tests:**

```rust
#[test]
fn a_second_rebase_does_not_grow_the_release() {
    // Given: a release rebased once already, and upstream advancing again.
    let lab = lab::Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    for advance in ["first advance\n", "second advance\n"] {
        lab.advance_upstream(advance);
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(["--text", "release", "--repo", "demo", "rebase"])
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
            .output()
            .expect("run release rebase");
        assert!(
            output.status.success(),
            "rebase failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Then: three parents (base + two branches), whatever the rebase count.
    let parents = Repo::open(&lab.work)
        .expect("open")
        .parents_of(release)
        .expect("parents");
    assert_eq!(parents.len(), 3, "parents accumulated: {parents:?}");
}

#[test]
fn a_release_already_containing_the_reference_by_ancestry_is_left_alone() {
    // Given: a release rebased onto the current upstream tip, then asked again
    // for a commit it already reaches THROUGH that parent's history. The old
    // direct-parent check could not see this (#11 / #4 third bullet).
    let lab = lab::Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    let repo = Repo::open(&lab.work).expect("open");
    let seed = repo.resolve_commit("main@upstream").expect("seed tip");
    lab.advance_upstream("advance\n");
    let rebase = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "release", "--repo", "demo", "rebase"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("first rebase");
    assert!(rebase.status.success());
    let before = Repo::open(&lab.work).expect("open").resolve_commit(release).expect("release");

    // When: asked to include the SEED commit, an ancestor of the new base.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "release", "--repo", "demo", "rebase", seed.as_str()])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("second rebase");

    // Then: recognized as already contained; the release did not move.
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("already contains"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let after = Repo::open(&lab.work).expect("open").resolve_commit(release).expect("release");
    assert_eq!(before, after, "release moved for an already-contained commit");
}
```

- [ ] **Step 3: Run:** all three FAIL against current `run_rebase` (accumulation + identity check).

- [ ] **Step 4: Implement.** Replace the body of the per-repo loop in `run_rebase` (src/main.rs:191-224) from `let onto = ...` down:

```rust
        let onto = opened.resolve_commit(&reference)?;
        let release_commit = opened.resolve_commit(&release_name)?;
        // Ancestry, not parent identity: a commit already reachable through a
        // parent's history is contained, and adding it again grows the octopus.
        if opened.is_ancestor(&onto, &release_commit)? {
            println!("{repo}: {release_name} already contains {reference}");
            continue;
        }
        if release::repair_effect(&plan.pins) == release::RepairEffect::NewDatedName {
            println!(
                "{repo}: every pin of {release_name} is frozen, so moving it would reach \
                 nobody; cut a new dated release instead"
            );
            continue;
        }
        let parents = opened.parents_of(&release_name)?;
        let tips = opened.bookmark_tips()?;
        let scheme = entry.release_scheme();
        let mut carried: Vec<knives::ids::CommitId> = Vec::new();
        let mut replaced = 0usize;
        for parent in &parents {
            // Oracle amendment: a parent HELD by a live branch bookmark is kept
            // even when onto already reaches it — a landed branch remains a
            // member with its parent and provenance intact (spec 1.7 "keeping
            // branch parents"; dropping members is `release drop`'s job, never
            // the rebase's). Held = any bookmark still pointing at the parent
            // whose branch is neither a release name nor the trunk.
            let held = parent.bookmarks.iter().any(|reference| {
                tips.get(reference) == Some(&parent.commit)
                    && !knives::ids::is_release_name(reference.branch(), &scheme)
                    && reference.branch().as_str() != entry.trunk()
            });
            if held {
                carried.push(parent.commit.clone());
                continue;
            }
            // Unheld and reachable from onto: a superseded base (or an already-
            // replaced ancestor). Several match when earlier rebases accumulated
            // bases; replacing them all is the self-heal.
            if opened.is_ancestor(&parent.commit, &onto)? {
                replaced += 1;
                continue;
            }
            {
                let moved = knives::jj::branches_past(&entry.path, &parent.commit)?;
                let went: Vec<String> = moved
                    .iter()
                    .map(|(branch, tip)| {
                        format!("{branch} (now {})", tip.as_str().chars().take(12).collect::<String>())
                    })
                    .collect();
                eprintln!(
                    "{repo}: parent {} of {release_name} is stale — no bookmark points at it{}. \
                     Fix the branch (or drop it from the release) and re-run; carrying a stale \
                     parent silently ships pre-rewrite code.",
                    parent.commit.as_str().chars().take(12).collect::<String>(),
                    if went.is_empty() {
                        String::new()
                    } else {
                        format!("; its branch moved on: {}", went.join(", "))
                    }
                );
                return Ok(Exit::Incomplete);
            }
        }
        carried.push(onto.clone());
        let message = format!("chore(release): {release_name} rebased onto {reference}");
        // #12: the repair is the OLD release duplicated onto the new parent set,
        // never a from-scratch merge — prior conflict resolutions carry over, so
        // a rebase surfaces only conflicts the new base itself introduces.
        let duplicated = knives::jj::duplicate_onto(&entry.path, &release_commit, &carried)?;
        // describe rewrites the commit id; bookmark the id carrying the message.
        let created = knives::jj::describe_commit(&entry.path, &duplicated, &message)?;
        knives::jj::set_bookmark_anywhere(&entry.path, &release_name, created.as_str())?;
        println!(
            "{repo}: {release_name} now contains {reference} ({}), {} base parent(s) replaced, \
             {} branch parent(s) kept",
            &onto.as_str()[..12.min(onto.as_str().len())],
            replaced,
            carried.len() - 1
        );
```

Check: `parents_of` bookmark lists include remote refs too, so "held" here accepts a remote ref still pointing at the parent — deliberately the same rule as `stale_parents::is_held` (src/detect/stale_parents.rs:44). A parent held only by a remote ref is not stale (origin still points there).

- [ ] **Step 5: Run:** `cargo test --test jj_integration rebase` — the rewritten test and both new tests PASS. Note for the stale-refusal path: it is exercised by Task 12's neighbor — no separate binary test here because manufacturing a stale, non-replaced parent needs `advance_origin_branch` (lab.rs:142) plus a fetch, which the frozen-pin test's fixture already half-builds; if a dedicated test proves cheap while there, add `a_rebase_refuses_a_stale_parent_it_cannot_map` using that fixture shape.

- [ ] **Step 6: Fix `trunk_lag` the same way** (the trunk-containment probe from #4's third bullet — the finding that misled the field report's agent). `trunk_lag` (src/commands/release.rs:243) answers "does the release contain the upstream trunk" by scanning DIRECT parents for identity, so a release containing the trunk through a parent's history reads as lagging. Replace the parent scan with ancestry:

```rust
pub fn trunk_lag(repo: &Repo, release: Option<&str>, upstream_trunk: &str) -> Option<String> {
    let trunk = repo.resolve_commit(upstream_trunk).ok()?;
    let release = release?;
    let commit = repo.resolve_commit(release).ok()?;
    // Ancestry, not parent identity: the trunk is contained whenever it is
    // reachable, including through a parent's own history (#4, third bullet).
    if repo.is_ancestor(&trunk, &commit).unwrap_or(false) {
        return None;
    }
    Some(format!(
        "{release} does not contain the upstream trunk ({})",
        &trunk.as_str()[..12.min(trunk.as_str().len())]
    ))
}
```

Add an integration test right beside the rebase ones, shaped so the trunk is reachable ONLY through a parent's history — the false-negative shape from the field — never as a direct parent:

```rust
#[test]
fn a_release_contains_the_trunk_through_a_parents_history_not_as_a_direct_parent() {
    // Given: a member branch that merged the advanced upstream, and a release
    // whose direct parents are (seed trunk, that branch, another branch). The
    // advanced trunk is reachable only through the member's own merge.
    let lab = lab::Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.advance_upstream("advance\n");
    lab.jj_work(["new", "feat/alpha", "main@upstream", "-m", "merge upstream into alpha"]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    lab.octopus(release, "feat/alpha", "feat/beta");

    // When/Then: the probe sees containment through ancestry. The direct-parent
    // version reported lag here — no parent IS the advanced trunk commit.
    let repo = Repo::open(&lab.work).expect("open");
    assert_eq!(
        knives::commands::release::trunk_lag(&repo, Some(release), "main@upstream"),
        None,
        "trunk is contained through feat/alpha's merge; the probe must not report lag"
    );
}
```

Run: `cargo test --test jj_integration a_release_contains_the_trunk_through_a_parents_history` — FAILS before the `trunk_lag` fix (reports lag), PASSES after. This is the discriminating pin on ancestry semantics.

---

### Task 12: frozen-pin refusal integration test (#9 item 1)

**Files:**
- Test: `tests/jj_integration.rs`

**Interfaces:**
- Consumes: the untouched `repair_effect` gate in `run_rebase` (src/main.rs:207) — `PinKind::Frozen` comes from any pin line not containing `branch` (src/pins.rs:110), so a `rev = "release/..."` pin is frozen.

This is the other half of the gate the sideways-merge test covers: `repair_effect(..) == NewDatedName` must refuse and direct to a new dated cut. The gate licenses the permissive `set_bookmark_anywhere` move, so it deserves the same coverage as the path it guards.

- [ ] **Step 1: Write the failing-or-passing test** (it should pass immediately if the gate works; write it, watch it pass, and if it fails the gate has a real bug — fix in `run_rebase`):

```rust
#[test]
fn release_rebase_refuses_when_every_pin_is_frozen() {
    // Given: a dated release whose only consumer pins it by rev — frozen, so an
    // in-place repair would reach nobody (spec #9 item 1).
    let lab = lab::Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let consumer = lab.consumer_with_pin_history(
        "pyproject.toml",
        "work = { git = \"https://forge.invalid/acme/work.git\", rev = \"release/2026-08-03\" }\n",
        "work = { git = \"https://forge.invalid/acme/work.git\", rev = \"release/2026-08-04\" }\n",
    );
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nconsumers = [\"{}\"]\n",
            lab.work.display(),
            lab.upstream.display(),
            consumer.display(),
        ),
    )
    .expect("write registry");
    lab.advance_upstream("upstream advance\n");
    let before = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit(release)
        .expect("release before");

    // When: a rebase is requested.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "release", "--repo", "demo", "rebase"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run release rebase");

    // Then: the command refuses in prose, directs to a new dated name, and the
    // release bookmark did not move.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("frozen") && stdout.contains("cut a new dated release"),
        "refusal text missing: {stdout}"
    );
    let after = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit(release)
        .expect("release after");
    assert_eq!(before, after, "a frozen release was moved in place");
}
```

- [ ] **Step 2: Run:** `cargo test --test jj_integration release_rebase_refuses_when_every_pin_is_frozen` — expect PASS. If it fails, the failure IS the finding: debug `repair_effect`'s inputs (most likely the pin scan not seeing the consumer file), fix minimally, re-run.

---

### Task 13: hook state back-compat test (#9 item 2)

**Files:**
- Test: `src/hook/state_regression_tests.rs`

**Interfaces:**
- Consumes: `SessionState::load` / `SessionState::update` (src/hook/state.rs:36, :53), `owner_remotes` accessor (src/hook/state.rs:49).

- [ ] **Step 1: Add the test** (modeled on `missing_flag_fields_default_independently`, line 26):

```rust
#[test]
fn a_document_predating_owner_remotes_still_loads() -> anyhow::Result<()> {
    // Given: state written before `owner_remotes` existed. `#[serde(default)]`
    // is what keeps this loading; this pins it so the next reshaping of the
    // file cannot silently drop old sessions (#9 item 2).
    let home = tempfile::tempdir()?;
    let directory = home.path().join("hook-sessions");
    std::fs::create_dir_all(&directory)?;
    std::fs::write(
        directory.join("claude-code-s1.json"),
        r#"{"repos":{"/r":{"noticed":true,"guided":true}}}"#,
    )?;

    // When: the record is read.
    let state = SessionState::load(home.path(), "claude-code", "s1");

    // Then: the flags survive and the absent map reads as absent, not an error.
    assert!(state.repo(Path::new("/r")).noticed);
    assert!(state.repo(Path::new("/r")).guided);
    assert!(state.owner_remotes(Path::new("/r")).is_none());
    Ok(())
}
```

- [ ] **Step 2: Run:** `cargo test -p knives a_document_predating_owner_remotes` — PASS (it pins existing behavior; a failure means the `#[serde(default)]` contract broke and must be fixed in `src/hook/state.rs`).

---

### Task 14: gates, docs, commit, PR

**Files:**
- Modify: `README.md` (release section: reap + `--allow-drop` + audit, a paragraph each, matching the README's register)
- Modify: `skills/` — if a skill file documents `knives release` (check `grep -rn "release" skills/`), add `reap` and `--allow-drop` where commands are listed.

- [ ] **Step 1: Full gates:**

```bash
cargo fmt --all
cargo clippy --all-targets --all-features --workspace -- -D warnings
cargo nextest run --all-targets --all-features --workspace   # or cargo test
bun run lint && bun run typecheck && bun run test:knives-plugin
```

Expected: all green. The integration suite is the slow part (~minutes); run it once here in full even though tasks ran slices.

- [ ] **Step 2: Docs.** Update README's release workflow section: one paragraph on the pre-cut gate and `--allow-drop`, one on the post-cut audit, one on reaping (`knives release reap`, automatic at cut, never touches remotes, refetch re-materializes untracked refs and that is fine). Follow the `updating-docs` skill if loaded; keep the README's existing voice.

- [ ] **Step 3: Describe and push** (single commit for the whole PR):

```bash
jj describe -m "feat: release cuts that cannot lose content

Pre-cut orphan gate (--allow-drop to override), post-cut content audit,
superseded-cut reaping (automatic at cut + knives release reap), divergence
detector ignores superseded release refs, shared-base invariant (start bases
on it; mixed-base and superseded-base findings), release rebase replaces the
base instead of accumulating parents, and the #9 coverage gaps.

Closes #4. Closes #7. Closes #9. Closes #10. Closes #11."
jj new
jj bookmark set feat/release-correctness -r @-
jj git push --bookmark feat/release-correctness
```

- [ ] **Step 4: Open the PR** with `gh pr create` (title: `feat: release cuts that cannot lose content`), body summarizing the five issues and the field evidence, including the `Closes #N` lines. Watch CI (`gh pr checks --watch`), fix failures.

- [ ] **Step 5: Close out #9 item 3** with a comment on #9: items 1 and 2 are covered by tests in this PR; item 3 (live-forge pr-column collapse) is observation-only and stays open in comment form — quote the spec's "closed by comment when observed" line and close the issue as completed by the PR (the PR's `Closes #9` does it; the comment records the item-3 caveat).

- [ ] **Step 6: Run the post-pr sweep** (`post-pr` skill) before reporting merge-ready.

## Task Dependency Notes

Tasks 1→3 are strictly ordered (each consumes the previous). Task 4 needs 3. Task 5 needs 1+3; Task 6 needs 5. Task 7 needs 1+3; Task 8 needs 1+7 (shares `run_release` wiring — implement 7 before 8 to avoid merge friction in the same function). Task 9 needs 3; Task 10 needs 9; Task 11 is independent of 5-10 (only needs `is_ancestor`). Tasks 12-13 are independent tests. Task 14 last.
