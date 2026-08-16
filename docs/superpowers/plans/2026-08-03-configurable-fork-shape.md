# Configurable Fork Shape Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make knives work on forks whose shape differs from the hardcoded default: configurable upstream trunk (opencode's is `dev`), a fixed release branch (`sami`) instead of dated `release/` cuts, three remote-role reporting fixes, declarative trust rules for guidance injection, a `knives register` command, and a readable status table.

**Spec:** `docs/superpowers/specs/2026-08-03-configurable-fork-shape-design.md` — read it first.

**Architecture:** Two config knobs on `RepoEntry` drive everything: `base` (existing field, meaning widened to "upstream's trunk") replaces the `TRUNK`/`UPSTREAM_TRUNK` constants via accessors threaded to every site, and a new `release_branch` field derives a `ReleaseScheme` enum (`Dated | Fixed(BranchName)`) matched exhaustively at every release-aware site. Trust rules are a new `[trust]` registry section consulted by the hook resolver with per-session caching of owner probes. The status table is a render-only change.

**Tech Stack:** Rust edition 2024 (rust 1.90), clap, serde/toml, jj-lib =0.43.0 pinned; TypeScript plugin under `plugin/` (Bun + Biome) — untouched by this plan except nothing breaks its contract.

## Global Constraints

- **jj, not git.** All VCS through `jj` (`jj describe`, `jj new`, `jj bookmark set`, `jj git push`). Never run `git commit`/`git push` in this repo.
- **One commit per PR.** Work accumulates in `@`. No per-task commits, no `jj split`. Describe once at the end (Task 19). This overrides the per-step commit ceremony below — where a step says "commit", skip it.
- **Gates that must stay green:** `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features --workspace -- -D warnings`, `cargo nextest run --all-targets --all-features --workspace` (fall back to `cargo test` if nextest is absent), `bun run lint`, `bun run typecheck`, `bun run test:knives-plugin`.
- **Identity guard:** `tests/no_hardcoded_identity.rs` forbids forge-host and user/org literals under `src/`. Tests spell the host as `concat!("github", ".com")` (see `src/forge.rs` tests) and use `example.invalid` URLs. No `sjawhar`, no real org names anywhere in `src/` or test literals.
- **House style:** doc comments state current behavior and the reason it exists (often citing the failure that motivated it). Test names are sentences (`fn a_branch_behind_origin_is_not_judged_against_the_trunk`). Given/When/Then comments in tests. `#![allow(clippy::indexing_slicing, reason = ...)]` at test-module top matches existing files.
- **Exit discipline:** problems → `Exit::Incomplete`; findings → `Exit::Findings`; a command that cannot answer must not exit zero.
- **Never pushes invariant:** `knives release cut` moves a bookmark and never pushes (doc comment on `release::cut`). The fixed scheme keeps this: publishing remains the operator's `jj git push --bookmark <name>`. The spec's "then push to the release remote" phrasing describes the overall workflow, not the command.

---

### Task 1: Trunk accessors on `RepoEntry`

**Files:**
- Modify: `src/config.rs` (RepoEntry impl, ~line 136–169; `base` field doc ~line 110)

**Interfaces:**
- Produces: `RepoEntry::trunk(&self) -> &str` (the `base` value, default `"main"`), `RepoEntry::upstream_trunk(&self) -> String` (`"{trunk}@upstream"`). `default_base()` stays (PR-base call sites keep compiling) and delegates to `trunk()`.

- [ ] **Step 1: Write the failing tests** in `src/config.rs` `mod tests`:

```rust
#[test]
fn the_trunk_is_the_base_field_and_defaults_to_main() {
    // The trunk we fork from, measure landed against, and target PRs at are the
    // same branch in every repo we know of, so one field serves both meanings.
    let dir = tempfile::tempdir().unwrap();
    let plain = "[repos.demo]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\n";
    let registry = load(&write(dir.path(), plain)).unwrap();
    assert_eq!(registry.repos["demo"].trunk(), "main");
    assert_eq!(registry.repos["demo"].upstream_trunk(), "main@upstream");

    let stated = "[repos.demo]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\nbase = \"dev\"\n";
    let registry = load(&write(dir.path(), stated)).unwrap();
    assert_eq!(registry.repos["demo"].trunk(), "dev");
    assert_eq!(registry.repos["demo"].upstream_trunk(), "dev@upstream");
    assert_eq!(registry.repos["demo"].default_base(), "dev");
}
```

- [ ] **Step 2: Run to verify failure:** `cargo test -p knives the_trunk_is_the_base` — FAIL: no method `trunk`.

- [ ] **Step 3: Implement** in `impl RepoEntry`:

```rust
/// The branch upstream treats as its trunk: what we fork from, measure
/// landed against, and target pull requests at. One field, not two, because
/// no repo we manage has ever split them; `base` keeps its name for
/// compatibility with existing registries.
pub fn trunk(&self) -> &str {
    self.base.as_deref().unwrap_or("main")
}

/// The upstream remote's view of the trunk, e.g. `dev@upstream`.
///
/// Every landed probe and fork point measures against this, never the local
/// trunk: our fork's trunk answers about the wrong repository.
pub fn upstream_trunk(&self) -> String {
    format!("{}@upstream", self.trunk())
}
```
and change `default_base()` body to `self.trunk()`. Widen the `base` field doc to: `/// Upstream's trunk: the branch we fork from, measure landed against, and target pull requests at. Defaults to "main".`

- [ ] **Step 4: Run:** `cargo test -p knives config::` — PASS.

---

### Task 2: Thread the trunk through `status.rs` (delete both constants)

**Files:**
- Modify: `src/commands/status.rs` (constants at lines 20–23; `landed_verdict` ~192; `maintained_branches` ~348; `divergent_rows` ~501; `carried_findings` ~619; `add_branch_overlap_findings` ~644; `gather` ~673; tests)

**Interfaces:**
- Consumes: `entry.trunk()`, `entry.upstream_trunk()` from Task 1.
- Produces (signature changes later tasks build on):
  - `fn landed_verdict(path, branch, tips, options, upstream_trunk: &str) -> Result<Option<LandedVerdict>, JjError>`
  - `fn maintained_branches(tips: &BookmarkTips, trunk: &str) -> (Vec<(BranchName, CommitId)>, usize)`
  - `fn carried_findings(report: &Report, repo: &Repo, trunk: &str) -> anyhow::Result<Vec<Finding>>`
  - `DivergentInput` unchanged (it already carries `entry`).
  - The `pub const TRUNK` / `pub const UPSTREAM_TRUNK` items are **deleted**. The re-export line `pub use crate::ids::{RELEASE_PREFIX, is_our_release};` stays.

- [ ] **Step 1: Write the failing test** in `src/commands/status.rs` tests:

```rust
#[test]
fn the_trunk_exclusion_follows_the_repo_entry_not_the_name_main() {
    // Given: a repo whose upstream trunk is dev, carrying a branch named main
    let map = tips(&[
        (local("dev"), "aaa"),
        (local("main"), "bbb"),
        (local("feat/alpha"), "ccc"),
    ]);
    // When: maintained branches are collected with dev as the trunk
    let (branches, _) = maintained_branches(&map, "dev");
    let names: Vec<String> = branches.iter().map(|(b, _)| b.to_string()).collect();
    // Then: dev is excluded as the trunk, and a branch that merely shares the
    // name main is ours to report
    assert!(!names.contains(&"dev".to_owned()), "was: {names:?}");
    assert!(names.contains(&"main".to_owned()), "was: {names:?}");
    assert!(names.contains(&"feat/alpha".to_owned()));
}
```

- [ ] **Step 2: Run:** `cargo test -p knives the_trunk_exclusion_follows` — FAIL (wrong arity).

- [ ] **Step 3: Implement.** Delete both constants. Mechanical threading, every site:
  - `maintained_branches(tips, trunk)`: replace `branch.as_str() != TRUNK` with `branch.as_str() != trunk`.
  - `divergent_rows`: replace `branch.as_str() == TRUNK` with `branch.as_str() == input.entry.trunk()`.
  - `carried_findings(report, repo, trunk)`: replace `reference.branch().as_str() != TRUNK` with `!= trunk`.
  - `add_branch_overlap_findings`: `let from = format!("fork_point({} | {})", entry.upstream_trunk(), row.name);`
  - `landed_verdict(..., upstream_trunk: &str)`: pass to `probe_landed(path, branch, upstream_trunk)`.
  - `gather`: call sites pass `entry.trunk()` / `entry.upstream_trunk()`.
  - Fix the two existing tests that construct `RepoEntry` literals in this file — no change needed (they use `base: None`), but any test calling changed helpers gets the new argument (`"main"`).

- [ ] **Step 4: Run:** `cargo test -p knives status::` then `cargo clippy --all-targets -- -D warnings` on the touched crate — PASS. (`start.rs`, `release.rs`, `main.rs` now fail to compile; that is Task 3.)

---

### Task 3: Thread the trunk through `start`, `release`, `preflight`, `main`

**Files:**
- Modify: `src/commands/start.rs:7,59,71` — import gone; use entry.
- Modify: `src/commands/release.rs:11,91–104,113–124` — `carried_branches`, `trunk_lag`.
- Modify: `src/commands/preflight.rs:273,291` — hardcoded `"release/"` stays for now (Task 7 makes it scheme-aware); hardcoded `"main"` becomes the entry's trunk.
- Modify: `src/main.rs:188,533,536` — per-entry defaults.

**Interfaces:**
- Produces: `release::carried_branches(repo: &Repo, trunk: &str) -> anyhow::Result<Vec<(String, CommitId)>>`, `release::trunk_lag(repo: &Repo, release: Option<&str>, upstream_trunk: &str) -> Option<String>`, `preflight::branch_states(..)` reads `entry.trunk()` (it already receives `entry` via `gather`).

- [ ] **Step 1: Write the failing test** in `src/commands/release.rs` tests:

```rust
#[test]
fn carried_branches_excludes_the_configured_trunk_not_the_name_main() {
    // A fork of a dev-trunk upstream may carry a branch literally named main;
    // that branch is work, and dev is the one that is not.
    // (Constructed through the pure filter, mirroring maintained_branches.)
    let tips: crate::detect::BookmarkTips = [
        (BookmarkRef::Local(BranchName::new("dev")), CommitId::new("aaa")),
        (BookmarkRef::Local(BranchName::new("main")), CommitId::new("bbb")),
        (BookmarkRef::Local(BranchName::new("feat/x")), CommitId::new("ccc")),
    ]
    .into_iter()
    .collect();
    let names: Vec<String> = carried_from_tips(&tips, "dev").into_iter().map(|(b, _)| b).collect();
    assert!(!names.contains(&"dev".to_owned()));
    assert!(names.contains(&"main".to_owned()));
}
```

- [ ] **Step 2: Run:** FAIL (no `carried_from_tips`).

- [ ] **Step 3: Implement.** Extract the filter from `carried_branches` into a pure `fn carried_from_tips(tips: &BookmarkTips, trunk: &str) -> Vec<(String, CommitId)>` (same body, `branch.as_str() != trunk`); `carried_branches(repo, trunk)` calls it. `trunk_lag` gains `upstream_trunk: &str` and resolves that instead of the constant. In `main.rs`:
  - `run_rebase` (line ~188): move the default inside the per-repo loop: `let reference = reference.map_or_else(|| entry.upstream_trunk(), str::to_owned);`
  - `run_release` cut block (lines ~533–536): `let trunk_name = entry.upstream_trunk(); let trunk = opened.resolve_commit(&trunk_name)?; carried.insert(0, (trunk_name, trunk));`
  - `start.rs`: `let upstream_trunk = entry.upstream_trunk();` passed to `add_workspace` and the println.
  - `preflight.rs:291`: `branch.as_str() != entry.trunk()` (thread `entry` into `branch_states` if not already a parameter — it is reachable from `gather`'s `entry`).
  - All `release::carried_branches(&opened)` call sites in `main.rs` become `release::carried_branches(&opened, entry.trunk())`.

- [ ] **Step 4: Run the full gate:** `cargo test` — everything compiles and passes. `cargo clippy --all-targets --all-features --workspace -- -D warnings` — clean.

---

### Task 4: Lab support for a `dev` trunk + integration proof

**Files:**
- Modify: `tests/common/lab.rs` (constructor)
- Modify: `tests/jj_integration.rs` (new test)

**Interfaces:**
- Produces: `Lab::with_trunk(trunk: &str) -> Self` — same lab, initial branch named `trunk`. `Lab::new()` becomes `Self::with_trunk("main")`. A stored `trunk` field replaces EVERY `"main"` literal in `lab.rs` — enumerated: constructor seeding (`branch -M`, upstream push, fork push), `branch()`'s `main@origin` revset, `octopus()`'s `main@origin`, `rebase_and_force_push()`'s `main@upstream`, `squash_merge_pull()`'s `checkout main` and `push origin main`, `advance_upstream()`'s `push origin main`, and the `GIT_CONFIG_VALUE_0` env pin. After the change, a grep for `"main"` / `main@` in `tests/common/lab.rs` must match nothing except `with_trunk("main")`'s default.

- [ ] **Step 1: Write the failing test** in `tests/jj_integration.rs`:

```rust
#[test]
fn a_fork_whose_trunk_is_dev_probes_and_forks_against_dev() {
    // Given: an upstream whose only branch is dev, and a feature branch on it
    let lab = Lab::with_trunk("dev");
    lab.branch("feat/alpha", "feature.txt", "content\n");
    // When: the landed probe measures against dev@upstream
    let outcome = knives::jj::probe_landed(
        lab.work_path(),
        &knives::ids::BranchName::new("feat/alpha"),
        "dev@upstream",
    )
    .expect("probe runs");
    // Then: unmerged work replays clean and non-empty — the probe found the
    // trunk rather than erroring on a nonexistent main
    assert_eq!(outcome, knives::jj::RebaseOutcome::CleanNonEmpty);
}
```
(Add `pub(crate) fn work_path(&self) -> &Path { &self.work }` to `Lab`; check `RebaseOutcome` derives `PartialEq` — it does, or add it.)

- [ ] **Step 2: Run:** `cargo test --test jj_integration a_fork_whose_trunk_is_dev` — FAIL: no `with_trunk`.

- [ ] **Step 3: Implement** `with_trunk`: store `trunk: String` on `Lab`; replace every literal `"main"` inside `Lab` methods with `&self.trunk` (constructor seeding, `branch -M`, pushes, `main@origin` revsets, maintainer checkout, `GIT_CONFIG_VALUE_0`). `Lab::new()` delegates.

- [ ] **Step 4: Run:** the new test AND the whole existing suite (`cargo test --test jj_integration`) — both pass, proving the refactor did not disturb `main`-trunk labs.

---

### Task 5: `ReleaseScheme` enum, config field, parse-time validation

**Files:**
- Modify: `src/ids.rs` (enum next to `is_our_release`)
- Modify: `src/config.rs` (field, accessor, validation in `load`)

**Interfaces:**
- Produces:
  - `ids::ReleaseScheme` — `#[derive(Debug, Clone, PartialEq, Eq)] pub enum ReleaseScheme { Dated, Fixed(BranchName) }`
  - `RepoEntry.release_branch: Option<String>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`, after `release`)
  - `RepoEntry::release_scheme(&self) -> ReleaseScheme`
  - `load()` rejects `release_branch == base-or-default-trunk` and `release_branch` starting with `release/`, via a new `ConfigError::Invalid { path: PathBuf, detail: String }` variant.
- Every `RepoEntry { .. }` literal in tests across the crate gains `release_branch: None` (there are ~8: `status.rs` ×2, `main.rs` ×2, `repos.rs`, `sync.rs` ×1 helper, plus any in `tests/`). Compiler finds them all.

- [ ] **Step 1: Write the failing tests** in `src/config.rs`:

```rust
#[test]
fn the_release_scheme_is_dated_unless_a_fixed_branch_is_stated() {
    let dir = tempfile::tempdir().unwrap();
    let plain = "[repos.demo]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\n";
    let registry = load(&write(dir.path(), plain)).unwrap();
    assert_eq!(registry.repos["demo"].release_scheme(), crate::ids::ReleaseScheme::Dated);

    let fixed = "[repos.demo]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\nrelease_branch = \"integration\"\n";
    let registry = load(&write(dir.path(), fixed)).unwrap();
    assert_eq!(
        registry.repos["demo"].release_scheme(),
        crate::ids::ReleaseScheme::Fixed(crate::ids::BranchName::new("integration"))
    );
}

#[test]
fn a_release_branch_shadowing_the_trunk_or_the_dated_namespace_fails_to_parse() {
    // A release branch named for the trunk would make every trunk exclusion
    // also exclude the release, and one under release/ would collide with the
    // dated scheme's namespace. Both corrupt every downstream check, so the
    // registry refuses at parse time, the same place a missing role fails.
    let dir = tempfile::tempdir().unwrap();
    for (text, needle) in [
        ("[repos.d]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\nrelease_branch = \"main\"\n", "trunk"),
        ("[repos.d]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\nbase = \"dev\"\nrelease_branch = \"dev\"\n", "trunk"),
        ("[repos.d]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\nrelease_branch = \"release/2026-01-01\"\n", "release/"),
    ] {
        let message = load(&write(dir.path(), text)).unwrap_err().to_string();
        assert!(message.contains(needle), "for {text}: was {message}");
    }
}
```

- [ ] **Step 2: Run:** FAIL (unknown field / no variant).

- [ ] **Step 3: Implement.** In `ids.rs`, below `is_our_release`:

```rust
/// How this fork names its releases.
///
/// Derived from configuration and matched exhaustively at every release-aware
/// site, so the compiler forces each of them — including ones added later — to
/// answer "what does this mean when the release is one fixed branch?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseScheme {
    /// Dated `release/YYYY-MM-DD[.n]` cuts. The default, and the historical behavior.
    Dated,
    /// One integration branch that is rebuilt and advanced in place. The branch's
    /// previous position plays the role of the previous release.
    Fixed(BranchName),
}
```

In `config.rs`: add the field; accessor:

```rust
/// How releases are named, derived from `release_branch`.
pub fn release_scheme(&self) -> crate::ids::ReleaseScheme {
    self.release_branch.as_deref().map_or(
        crate::ids::ReleaseScheme::Dated,
        |name| crate::ids::ReleaseScheme::Fixed(crate::ids::BranchName::new(name)),
    )
}
```

In `load()`, after path resolution, per entry:

```rust
if let Some(name) = entry.release_branch.as_deref() {
    if name == entry.trunk() {
        return Err(ConfigError::Invalid { path: path.to_owned(), detail: format!(
            "release_branch {name:?} names the trunk; a release branch shadowing the trunk corrupts every trunk exclusion") });
    }
    if name.starts_with("release/") {
        return Err(ConfigError::Invalid { path: path.to_owned(), detail: format!(
            "release_branch {name:?} sits in the dated release/ namespace; the two schemes must not collide") });
    }
}
```
(`Invalid` variant: `#[error("{path} is not a valid registry: {detail}")]`.) Add `release_branch: None` to every struct literal the compiler flags.

- [ ] **Step 4: Run:** `cargo test` — PASS, whole workspace compiles.

---

### Task 6: Scheme-aware `is_our_release`

**Files:**
- Modify: `src/ids.rs` (`is_our_release`, ~line 100)
- Modify: callers: `src/commands/status.rs` (`releases_to_scan` filter — full rework in Task 7, here just arity), `src/commands/repos.rs:31`, `src/jj.rs` (`branches_containing`, ~line 287), `src/commands/release.rs` newest-release filter (~line 137).

**Interfaces:**
- Produces: `pub fn is_our_release(reference: &BookmarkRef, scheme: &ReleaseScheme) -> bool`.
- `jj::Repo::branches_containing(&self, commit: &CommitId, scheme: &ReleaseScheme)`.
- `repos::release_state` reads each entry's scheme (it iterates the registry, entries in hand).

- [ ] **Step 1: Write the failing test** in `src/ids.rs` tests:

```rust
#[test]
fn under_a_fixed_scheme_the_fixed_branch_is_the_release_and_dated_names_are_not() {
    use super::{BookmarkRef, BranchName, ReleaseScheme, RemoteName, is_our_release};
    let fixed = ReleaseScheme::Fixed(BranchName::new("integration"));
    let local = |name: &str| BookmarkRef::Local(BranchName::new(name));
    let remote = |name: &str, r: &str| BookmarkRef::Remote {
        branch: BranchName::new(name),
        remote: RemoteName::new(r),
    };
    // The fixed branch is a cut wherever we publish it, never on upstream.
    assert!(is_our_release(&local("integration"), &fixed));
    assert!(is_our_release(&remote("integration", "origin"), &fixed));
    assert!(is_our_release(&remote("integration", "release"), &fixed));
    assert!(!is_our_release(&remote("integration", "upstream"), &fixed));
    assert!(!is_our_release(&remote("integration", "git"), &fixed));
    // Under Fixed, a dated name is not one of this repo's releases.
    assert!(!is_our_release(&local("release/2026-07-29"), &fixed));
    // Dated behavior is unchanged.
    assert!(is_our_release(&local("release/2026-07-29"), &ReleaseScheme::Dated));
    assert!(!is_our_release(&local("integration"), &ReleaseScheme::Dated));
}
```

- [ ] **Step 2: Run:** FAIL (arity).

- [ ] **Step 3: Implement:**

```rust
pub fn is_our_release(reference: &BookmarkRef, scheme: &ReleaseScheme) -> bool {
    let named_as_release = match scheme {
        ReleaseScheme::Dated => reference.branch().as_str().starts_with(RELEASE_PREFIX),
        ReleaseScheme::Fixed(name) => reference.branch() == name,
    };
    if !named_as_release {
        return false;
    }
    match reference {
        BookmarkRef::Local(_) => true,
        BookmarkRef::Remote { remote, .. } => matches!(remote.as_str(), "origin" | "release"),
    }
}
```
Update every caller to pass a scheme (`&entry.release_scheme()`; the existing `is_our_release` tests in `ids.rs` and `status.rs` pass `&ReleaseScheme::Dated`). `branches_containing` threads it through to its internal filter; its caller `carried_findings` (status) passes the entry's scheme — extend `carried_findings(report, repo, trunk, scheme)`.

- [ ] **Step 4: Run:** `cargo test` — PASS.

---

### Task 7: Status and preflight under the fixed scheme

**Files:**
- Modify: `src/commands/status.rs` (`scan_releases`, `releases_to_scan`, `maintained_branches`, `divergent_rows`, `gather`)
- Modify: `src/commands/preflight.rs` (lines ~273, ~291: release exclusions)
- Modify: `src/commands/release.rs` (`carried_from_tips` — plan-gap fix, see below)

**Interfaces:**
- Produces:
  - `fn releases_to_scan(tips: &BookmarkTips, scheme: &ReleaseScheme, publish_remote: &str) -> (Vec<(BookmarkRef, CommitId)>, usize)` — `publish_remote` is `"release"` when `entry.has_split_release()` else `"origin"` (callers compute it from the entry; Dated ignores it, preserving today's behavior byte-for-byte). Under `Fixed`: candidates are exactly the local fixed branch plus its counterpart on the PUBLISH remote only — the other role remote is ignored; no `release_order`, `skipped = 0`.
  - `fn maintained_branches(tips, trunk, scheme) -> ...` — under `Fixed`, exclude the fixed branch the way `release/*` is excluded; under `Dated`, prefix exclusion as today.
  - `divergent_rows` gets the same exclusion via `input.entry`.
  - `preflight` exclusions: a branch is skipped when its name is a release under the scheme OR it is the trunk — implement with a shared predicate `pub fn is_release_name(branch: &BranchName, scheme: &ReleaseScheme) -> bool` in `src/ids.rs`, beside `ReleaseScheme`, `RELEASE_PREFIX`, and `is_our_release` (prefix match for Dated, equality for Fixed), so status/preflight/release all use one predicate without depending on a rendering module for domain classification.
  - **Plan-gap fix (from Task 6 review):** `release.rs`'s `carried_from_tips(tips, trunk)` still hardcodes `starts_with("release/")`. Under Fixed, the fixed branch would be returned as a CARRIED BRANCH and merged into its own new cut — contradicting "rebuilt and advanced in place". Extend it to `carried_from_tips(tips, trunk, scheme)` using `is_release_name`, thread `carried_branches(repo, trunk, scheme)`, update call sites (main.rs, jj_integration.rs), and add a test: under `Fixed("integration")`, tips {integration, feat/x, dev} with trunk dev yield only feat/x — the fixed branch is a cut, not cargo.

- [ ] **Step 1: Write the failing tests** in `src/commands/status.rs`:

```rust
#[test]
fn under_a_fixed_scheme_the_fixed_branch_is_scanned_and_is_not_a_maintained_branch() {
    let fixed = crate::ids::ReleaseScheme::Fixed(BranchName::new("integration"));
    let map = tips(&[
        (local("integration"), "aaa"),
        (remote("integration", "origin"), "bbb"),
        (remote("integration", "release"), "eee"), // non-publish role remote: ignored
        (local("feat/alpha"), "ccc"),
        (local("release/2026-07-28"), "ddd"), // stale leftover from a scheme migration
    ]);
    // When: releases are chosen and branches collected under the fixed scheme
    let (chosen, skipped) = releases_to_scan(&map, &fixed, "origin");
    let names: Vec<String> = chosen.iter().map(|(r, _)| r.to_string()).collect();
    let (branches, _) = maintained_branches(&map, "main", &fixed);
    let branch_names: Vec<String> = branches.iter().map(|(b, _)| b.to_string()).collect();
    // Then: local and remote positions of the fixed branch are the scan set
    assert!(names.contains(&"integration".to_owned()), "was: {names:?}");
    assert!(names.contains(&"integration@origin".to_owned()), "was: {names:?}");
    assert!(
        !names.contains(&"integration@release".to_owned()),
        "only the publish remote's counterpart is a release candidate: {names:?}"
    );
    assert!(!names.iter().any(|n| n.contains("release/")), "was: {names:?}");
    assert_eq!(skipped, 0);
    // And: the fixed branch is a cut, not a branch of ours — while a dated
    // leftover under Fixed is reported as an ordinary branch, not hidden
    assert!(!branch_names.contains(&"integration".to_owned()), "was: {branch_names:?}");
    assert!(branch_names.contains(&"release/2026-07-28".to_owned()), "was: {branch_names:?}");
}

#[test]
fn the_dated_scheme_still_scans_only_the_newest_release_on_each_side() {
    // Regression guard: Dated behavior is byte-identical after the enum threading.
    let map = tips(&[
        (local("release/2026-07-17.5"), "aaa"),
        (local("release/2026-07-28"), "bbb"),
        (remote("release/2026-07-29", "origin"), "ddd"),
    ]);
    let (chosen, skipped) = releases_to_scan(&map, &crate::ids::ReleaseScheme::Dated, "origin");
    assert_eq!(chosen.len(), 2);
    assert_eq!(skipped, 1);
}
```
(Adjust the three existing `releases_to_scan` tests to pass `&ReleaseScheme::Dated, "origin"`; `maintained_branches` tests pass `&ReleaseScheme::Dated` too.)

- [ ] **Step 2: Run:** FAIL.

- [ ] **Step 3: Implement.** Add the shared predicate to `src/ids.rs` and match in each function:

```rust
/// Whether a branch name is a release under this scheme: the dated prefix, or
/// the one fixed integration branch. One predicate, because status, preflight
/// and release each growing their own was how `repos` once claimed an
/// upstream release as ours.
pub fn is_release_name(branch: &BranchName, scheme: &ReleaseScheme) -> bool {
    match scheme {
        ReleaseScheme::Dated => branch.as_str().starts_with(RELEASE_PREFIX),
        ReleaseScheme::Fixed(name) => branch == name,
    }
}
```

`releases_to_scan` under `Fixed`: keep the local fixed branch and exactly its `publish_remote` counterpart (`BookmarkRef::Remote { branch: fixed, remote: publish_remote }`), `skipped = 0`; other role remotes are not candidates — a previous position on the non-publish remote is stale by definition. `maintained_branches`/`divergent_rows`/`preflight` exclusions: `is_release_name(branch, scheme) || branch.as_str() == trunk`. `gather` passes `&entry.release_scheme()` and the publish remote everywhere.

- [ ] **Step 4: Run:** `cargo test` — PASS. `cargo clippy` — clean.

---

### Task 8: `knives release` under the fixed scheme

**Files:**
- Modify: `src/cli.rs` (`ReleaseAction::Cut` name becomes `Option<String>`; the `every_designed_command_is_reachable` test gains `vec!["knives", "release", "cut"]`)
- Modify: `src/commands/release.rs` (`plan` newest-release selection ~line 134–151; new `cut_name` resolution; previous-position capture)
- Modify: `src/main.rs` (`dispatch_release`, `run_release`)

**Interfaces:**
- Produces:
  - `release::cut_name(scheme: &ReleaseScheme, requested: Option<&str>) -> Result<String, String>` — Dated+None → Err("a dated release cut needs a name, e.g. release/2026-08-03"); Dated+Some → Ok(name); Fixed+None → Ok(fixed); Fixed+Some(n) where n == fixed → Ok; Fixed+Some(other) → Err("this repo cuts the fixed release branch {fixed}; drop the name or use {fixed}").
  - `release::previous_position(repo: &Repo, entry: &RepoEntry) -> Option<(String, CommitId)>` — under Fixed, resolves the PUBLISH remote-tracking ref ONLY: `{fixed}@release` when `entry.has_split_release()` else `{fixed}@origin` — never the local fixed bookmark, and never the non-publish role remote. `None` under Dated or when the ref does not exist (first cut). Sound pre-push by construction: `release::cut` only creates the merge and moves the LOCAL bookmark via `set_bookmark` — it neither pushes nor fetches, so the remote-tracking ref still reflects the last published cut when `previous_position` runs after it. Doc comment must state both facts: remote-only resolution, and that this is the seam issue #4's pre/post-cut checks will attach to. Test must cover: local fixed bookmark moved, remote ref unchanged → previous position is the remote ref, not the new local tip.
  - `plan()` newest-release selection matches on the scheme: Dated as today; Fixed keeps the local fixed branch and its publish-remote counterpart with the same local-preference tiebreak, no `release_order`.

- [ ] **Step 1: Write the failing tests** in `src/commands/release.rs`:

```rust
#[cfg(test)]
mod scheme_tests {
    use super::*;
    use crate::ids::{BranchName, ReleaseScheme};

    #[test]
    fn a_dated_cut_requires_a_name_and_a_fixed_cut_supplies_its_own() {
        let dated = ReleaseScheme::Dated;
        let fixed = ReleaseScheme::Fixed(BranchName::new("integration"));
        assert!(cut_name(&dated, None).is_err());
        assert_eq!(cut_name(&dated, Some("release/2026-08-03")).unwrap(), "release/2026-08-03");
        assert_eq!(cut_name(&fixed, None).unwrap(), "integration");
        assert_eq!(cut_name(&fixed, Some("integration")).unwrap(), "integration");
        // A stray dated name under the fixed scheme would silently fork the
        // naming; refusing is the only answer that cannot lose a cut.
        assert!(cut_name(&fixed, Some("release/2026-08-03")).is_err());
    }
}
```

- [ ] **Step 2: Run:** FAIL.

- [ ] **Step 3: Implement** `cut_name` and `previous_position` exactly per the Interfaces block. In `plan()`:

```rust
let scheme = entry.release_scheme();
let publish_remote = if entry.has_split_release() { "release" } else { "origin" };
let newest = match &scheme {
    ReleaseScheme::Dated => /* existing max_by_key block, verbatim */,
    ReleaseScheme::Fixed(fixed) => tips
        .iter()
        .filter(|(reference, _)| match reference {
            BookmarkRef::Local(branch) => branch == fixed,
            BookmarkRef::Remote { branch, remote } => {
                branch == fixed && remote.as_str() == publish_remote
            }
        })
        .max_by_key(|(reference, _)| u8::from(reference.is_local()))
        .map(|(r, c)| (r, c)),
};
```
(`plan` gains `entry` access — it already has it.) In `main.rs::run_release`: resolve the name via `cut_name` before cutting (usage error path prints the message, returns `Exit::Usage`); after a successful fixed-scheme cut, print the previous position:

```rust
if let Some((reference, commit)) = release::previous_position(&opened, &entry) {
    println!("  previous release position: {reference} at {}", &commit.as_str()[..12.min(commit.as_str().len())]);
} else if matches!(entry.release_scheme(), ReleaseScheme::Fixed(_)) {
    println!("  no previous release position: this is the first cut of the fixed branch");
}
```
`dispatch_release` passes `Option<String>` through. There is no future-dated-name validation in the code today; nothing to skip — the enum match documents the non-applicability.

- [ ] **Step 4: Run:** `cargo test` — PASS. Manually sanity-check help text: `cargo run -- release cut --help`.

---

### Task 9: Pins under the fixed scheme

**Files:**
- Modify: `src/pins.rs` (`scan` gains the scheme; `Pin` gains `locked: Option<String>`)
- Modify: `src/commands/release.rs` (`scan_consumer_for` threads the scheme)
- Modify: `src/commands/repos.rs` (`pin_lag` commit-on-branch under Fixed)

**Interfaces:**
- Produces:
  - `pins::scan(file: &str, text: &str, scheme: &ReleaseScheme) -> Vec<Pin>` — Dated: needle `release/`, behavior identical to today. Fixed(name): a line pins when the decoded line contains `name` as a ref token (delimited by the same stop set used today plus `=`, `/`, `@` on the left — implement as: find `name`, require the char before (if any) to be one of `"'=/@ ` and the extracted token to equal `name` exactly). `kind` from `branch` presence as today.
  - `Pin.locked: Option<String>` — the hex fragment after `#` following the reference on the same line (uv.lock's resolved commit), `None` elsewhere. Populated for both schemes (harmless for Dated).
  - `repos::PinLag { pub lag: Option<String>, pub notes: Vec<String> }` and `repos::pin_lag(entry: &RepoEntry, newest: Option<&String>, repo: Option<&Repo>) -> PinLag` — Dated: today's logic produces `lag`, `notes` empty until Task 13 fills them. Fixed: a `Follows` pin of the fixed name with no locked commit is current by definition; a pin with `locked` is behind when `repo.is_ancestor(locked, tip)` and `locked != tip` (tip = the local fixed branch commit, from `bookmark_tips`); unknown ancestry → a `notes` entry ("could not compare ...") rather than silently current. `render_with_releases` prints `notes` lines under the repo's line so consumer-checkout annotations are actually visible in `knives repos` output (Task 13 depends on this channel existing).
  - `release_state` in `repos.rs` already opens each `Repo`; restructure so `run()` passes the opened repo to `pin_lag` (open once, reuse).

- [ ] **Step 1: Write the failing tests** in `src/pins.rs`:

```rust
#[test]
fn a_fixed_branch_pin_is_found_with_its_locked_commit() {
    use crate::ids::{BranchName, ReleaseScheme};
    let fixed = ReleaseScheme::Fixed(BranchName::new("integration"));
    let text = "url = \"https://forge.invalid/o/r.git?branch=integration#548aaafb99\"";
    let pins = scan("uv.lock", text, &fixed);
    assert_eq!(pins.len(), 1, "was: {pins:?}");
    assert_eq!(pins[0].reference, "integration");
    assert_eq!(pins[0].kind, PinKind::Follows);
    assert_eq!(pins[0].locked.as_deref(), Some("548aaafb99"));
}

#[test]
fn a_word_containing_the_fixed_name_is_not_a_pin() {
    use crate::ids::{BranchName, ReleaseScheme};
    let fixed = ReleaseScheme::Fixed(BranchName::new("integration"));
    // "reintegration" contains the name; a substring match would call this a pin.
    assert!(scan("pyproject.toml", "mode = \"reintegration\"", &fixed).is_empty());
}

#[test]
fn dated_scanning_is_unchanged_by_the_scheme_parameter() {
    let text = "url = \"https://x/y.git?rev=release%2F2026-07-28.2#548aaafb\"";
    let pins = scan("uv.lock", text, &crate::ids::ReleaseScheme::Dated);
    assert_eq!(pins[0].reference, "release/2026-07-28.2");
    assert_eq!(pins[0].locked.as_deref(), Some("548aaafb"));
}
```
(Existing pins tests: add the `&ReleaseScheme::Dated` argument.)

- [ ] **Step 2: Run:** FAIL.

- [ ] **Step 3: Implement.** In `scan`, per line after decoding: needle by scheme; for Fixed verify token boundaries (char before ∈ `"'=/@ ` or line start; token extraction with today's `take_while` stop set must yield exactly the name). `locked`: after the reference's end position, if the next char is `#`, take the following `is_ascii_hexdigit` run (≥6 chars, else `None`). Thread the scheme through `scan_consumer_for(consumer, slug, scheme)` and both its callers. Rework `pin_lag` per the Interfaces block, matching on the scheme at the top.

- [ ] **Step 4: Run:** `cargo test` — PASS.

---

### Task 10: Lab integration test — fixed-scheme cut, twice

**Files:**
- Modify: `tests/jj_integration.rs`

- [ ] **Step 1: Write the test** (this is the end-to-end proof for Tasks 5–8):

```rust
#[test]
fn a_fixed_release_branch_is_cut_in_place_and_its_previous_position_is_the_old_cut() {
    // Given: a fork with one feature branch and a fixed integration branch scheme
    let lab = Lab::new();
    lab.branch("feat/alpha", "feature.txt", "one\n");
    let entry = lab.repo_entry_with_release_branch("integration"); // helper below

    // When: the first cut is made and pushed, the branch advances, and a second cut is made
    let opened = knives::jj::Repo::open(lab.work_path()).expect("open");
    let carried = knives::commands::release::carried_branches(&opened, "main").expect("tips");
    let trunk = opened.resolve_commit(&entry.upstream_trunk()).expect("trunk");
    let mut parents = vec![trunk];
    parents.extend(carried.iter().map(|(_, c)| c.clone()));
    let first = knives::commands::release::cut(
        lab.work_path(),
        &knives::commands::release::Cut {
            name: "integration".to_owned(),
            parents: parents.clone(),
            provenance: vec![],
        },
    )
    .expect("first cut");
    lab.push_branch("integration");
    lab.fetch_work();

    // Then: before any second push, the remote-tracking ref is the previous release
    // MANDATORY reopen: `opened` predates the push/fetch and cannot see the new
    // remote-tracking ref.
    let opened = knives::jj::Repo::open(lab.work_path()).expect("reopen after fetch");
    let previous = knives::commands::release::previous_position(&opened, &entry)
        .expect("a pushed cut is a previous position");
    assert_eq!(previous.1, first, "the old cut plays the role of the previous release");
}
```
Add `Lab::repo_entry_with_release_branch(&self, name: &str) -> knives::config::RepoEntry` building an entry with `path: work`, `upstream`/`origin` from the lab, `release_branch: Some(name.into())`, everything else `None`/empty. The reopen after `fetch_work` is mandatory, not a fallback: `Repo::open` reads repository state at call time, and the original handle predates the push.

- [ ] **Step 2: Run:** `cargo test --test jj_integration a_fixed_release_branch` — this should PASS if Tasks 5–8 are correct; treat a failure as a real defect in those tasks, not in the test.

---

### Task 11: PR inference matches heads from every role remote's owner (#5.2)

**Files:**
- Modify: `src/forge.rs` (`ours_only`, ~line 219)
- Modify: callers `src/commands/status.rs:419`, `src/commands/sync.rs:155`

**Interfaces:**
- Produces: `pub fn ours_only(pull_requests: BTreeMap<BranchName, PullRequest>, remotes: &[&str]) -> BTreeMap<BranchName, PullRequest>` — a PR is ours when its head-repository owner matches the owner of ANY given remote URL. Callers pass `&[entry.remote(Role::Origin), entry.remote(Role::Release)]` (dedup not needed; Release falls back to Origin). Unparseable remotes contribute nothing; if NO remote parses, keep everything (today's fail-open stance, same reason).

- [ ] **Step 1: Write the failing test** in `src/forge.rs` tests:

```rust
#[test]
fn a_head_on_the_release_remotes_owner_is_ours_too() {
    // Six real forks had origin pointed at an org copy while PR heads lived on a
    // personal fork recorded under another role. Matching only origin's owner
    // reported those PRs as nobody's and their branches as unpushed for months.
    use super::{Account, PullRequest, ours_only};
    use crate::ids::BranchName;
    use std::collections::BTreeMap;
    let origin = format!("https://{HOST}/org-copy/some-repo.git");
    let release = format!("https://{HOST}/personal/some-repo.git");
    let mut prs = BTreeMap::new();
    let _ = prs.insert(BranchName::new("feat/a"), PullRequest {
        number: 7,
        head_repository_owner: Some(Account { login: "personal".to_owned() }),
        ..PullRequest::default()
    });
    assert_eq!(ours_only(prs.clone(), &[&origin, &release]).len(), 1);
    assert!(ours_only(prs, &[&origin]).is_empty(), "origin alone must not match");
}
```

- [ ] **Step 2: Run:** FAIL (arity).

- [ ] **Step 3: Implement:**

```rust
pub fn ours_only(
    pull_requests: BTreeMap<BranchName, PullRequest>,
    remotes: &[&str],
) -> BTreeMap<BranchName, PullRequest> {
    let owners: Vec<&str> = remotes.iter().filter_map(|remote| remote_owner(remote)).collect();
    if owners.is_empty() {
        // A set of remotes we cannot parse is not a licence to claim everyone's work,
        // and not a reason to claim nobody's either: keep today's fail-open answer.
        return pull_requests;
    }
    pull_requests
        .into_iter()
        .filter(|(_, pr)| owners.iter().any(|owner| pr.is_from(owner)))
        .collect()
}
```
Update the two callers and the existing `ours_only` tests (wrap the single remote in a slice).

- [ ] **Step 4: Run:** `cargo test` — PASS.

---

### Task 12: `init` warns about a miswired origin and states the convention (#5.1)

**Files:**
- Modify: `src/commands/init.rs` (`decide`, `render`, `InitOutcome::Adopted` gains warnings)

**Interfaces:**
- Produces: `InitOutcome::Adopted { name, entry, warnings: Vec<String> }`. Heuristic in a pure `fn miswiring_warnings(remotes: &BTreeMap<String, String>) -> Vec<String>`: for every remote that is not a role name (`upstream`/`origin`/`release`), if it points at the same repository slug as upstream (`repo_slug`-style last path segment, `.git` trimmed) with an owner that differs from both upstream's and origin's owners, warn: `"origin is {origin}; untracked remote {name} looks like another fork of upstream ({url}). knives treats origin as YOUR fork — the one your branches push to and your PR heads live on. If {name} is that fork, rename remotes so it is origin."` `render` for `Adopted` always appends the convention line: `"convention: origin = your fork (push target, PR heads); upstream = the maintainer's repo (fetch only)"` plus any warnings.

- [ ] **Step 1: Write the failing test** in `src/commands/init.rs` tests:

```rust
#[test]
fn an_untracked_second_fork_of_upstream_is_flagged_as_a_possible_miswiring() {
    // Given: origin pointing at an org copy while a personal fork of the same
    // repository sits as an ad hoc remote — the exact wiring that produced
    // months of misleading unpushed findings on six real forks
    let found = remotes(&[
        ("origin", "https://forge.invalid/org-copy/tool.git"),
        ("upstream", "https://forge.invalid/maintainer/tool.git"),
        ("mine", "https://forge.invalid/someone/tool.git"),
    ]);
    // When: init decides
    let outcome = decide(Path::new("/tmp/tool/default"), &found);
    // Then: adoption succeeds, carrying a warning that names the suspect remote
    let InitOutcome::Adopted { warnings, .. } = &outcome else { panic!("expected adoption") };
    assert_eq!(warnings.len(), 1, "was: {warnings:?}");
    assert!(warnings[0].contains("mine"), "was: {warnings:?}");
    let text = render(&outcome, Path::new("/tmp/repos.toml"));
    assert!(text.contains("origin = your fork"), "the convention is always stated: {text}");
}

#[test]
fn a_remote_for_a_different_repository_is_not_a_miswiring() {
    let found = remotes(&[
        ("origin", "https://forge.invalid/someone/tool.git"),
        ("upstream", "https://forge.invalid/maintainer/tool.git"),
        ("other", "https://forge.invalid/someone/unrelated.git"),
    ]);
    let InitOutcome::Adopted { warnings, .. } = decide(Path::new("/tmp/t"), &found) else {
        panic!("expected adoption")
    };
    assert!(warnings.is_empty(), "was: {warnings:?}");
}
```
Note: owner extraction must not use `forge::remote_owner` (it is host-specific); write a local `fn url_owner(url: &str) -> Option<&str>` that takes the second-to-last path segment of any URL (`https://host/OWNER/repo.git` and `git@host:OWNER/repo.git` forms), so `forge.invalid` fixtures work and the identity guard is respected.

- [ ] **Step 2: Run:** FAIL.

- [ ] **Step 3: Implement** per the Interfaces block. Every `InitOutcome::Adopted` construction and match site gains `warnings` (compiler-guided: `decide`, `decide_with_registry`, `render`, `run`, existing tests add `warnings: _` or assert empty).

- [ ] **Step 4: Run:** `cargo test` — PASS.

---

### Task 13: Consumer pin read from the consumer's origin trunk (#5.3)

**Files:**
- Modify: `src/jj.rs` — new helper `file_at_origin_trunk`
- Modify: `src/commands/release.rs` (`scan_consumer_for`)
- Modify: `src/commands/repos.rs` (lag annotation)

**Interfaces:**
- Produces:
  - `jj::file_at_origin_trunk(consumer: &Path, file: &str) -> Result<Option<(String, usize)>, JjError>` — resolves the consumer's origin default branch (`git -C <consumer> rev-parse --abbrev-ref origin/HEAD`, falling back to trying `origin/main` then `origin/master` when HEAD is unset), returns `(file content at that ref via git show <ref>:<file>, commits the checkout's ref is behind that ref via git rev-list --count <ref> ^HEAD)`. `Ok(None)` when the path is not a git/jj repo or the ref/file is absent.
  - `release::scan_consumer_for(consumer, slug, scheme) -> (Vec<Pin>, Vec<String>)` — pins now read from the origin-trunk content when available (falling back to the working copy with a note `"{consumer}: not a repository; pins read from the working copy"`); when the checkout is N>0 commits behind its origin trunk, append note `"{consumer} checkout is {N} commit(s) behind its {branch}"`. The `Vec<String>` notes surface through `Plan.notes` and `repos` output.
- Both callers (`release::plan`, `repos::pin_lag`) destructure the pair and forward notes.

- [ ] **Step 1: Write the failing test** in `tests/jj_integration.rs` (needs a real repo; the lab provides one):

```rust
#[test]
fn a_consumer_checkout_parked_behind_its_origin_does_not_produce_a_false_behind() {
    // Given: a consumer repo whose origin trunk pins the newest release while
    // the checkout's working copy still shows an older pin — the exact state
    // that produced false BEHIND findings twice
    let lab = Lab::new();
    let consumer = lab.consumer_with_pin_history(
        "uv.lock",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-20\"\n",   // old, checkout
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-28\"\n",   // new, origin trunk
    );
    // When: the consumer is scanned
    let (pins, notes) = knives::commands::release::scan_consumer_for(
        &consumer,
        Some("tool"),
        &knives::ids::ReleaseScheme::Dated,
    );
    // Then: the pin is the origin trunk's, and the checkout's lag is a note
    assert_eq!(pins.len(), 1, "was: {pins:?}");
    assert_eq!(pins[0].reference, "release/2026-07-28");
    assert!(
        notes.iter().any(|n| n.contains("behind")),
        "the stale checkout is annotated, not silently trusted: {notes:?}"
    );
}
```
Add `Lab::consumer_with_pin_history(&self, file, checkout_content, origin_content) -> PathBuf`: create a bare origin + clone, commit `checkout_content`, push, commit `origin_content`, push, then `git reset --hard HEAD~1` in the clone (checkout one commit behind origin), `git fetch`. (Plain `git` is fine inside the lab fixture — the jj-only rule governs the knives repo itself, and `lab.rs` already shells out to git.)

- [ ] **Step 2: Run:** FAIL.

- [ ] **Step 3: Implement** per the Interfaces block. `file_at_origin_trunk` uses the existing `command`/`command_output` helpers in `jj.rs`. Unit-test `scan_consumer_for`'s non-repo fallback with a `tempfile` dir (existing test `a_siblings_pin_does_not_answer_this_repos_question` becomes the fallback case — update it to destructure the pair and assert the fallback note).

- [ ] **Step 4: Run:** `cargo test` — PASS.

---

### Task 14: `[trust]` rules in the registry

**Files:**
- Modify: `src/config.rs` (`TrustRules` struct, `Registry.trust`, path expansion in `load`)

**Interfaces:**
- Produces:

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustRules {
    /// Directory subtrees whose repositories are all trusted for guidance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<PathBuf>,
    /// Forge owners whose repositories are trusted for guidance, matched
    /// against remote URLs case-insensitively.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owners: Vec<String>,
}
impl TrustRules {
    pub fn is_empty(&self) -> bool { self.roots.is_empty() && self.owners.is_empty() }
}
```
  - `Registry.trust: TrustRules` (`#[serde(default, skip_serializing_if = "TrustRules::is_empty")]`) — present on the type for the same reason `trusted` is: `save` rewrites the whole file.
  - `load()` expands `trust.roots` with `expand_registry_path` like every other path.

- [ ] **Step 1: Write the failing tests** in `src/config.rs`:

```rust
#[test]
fn trust_rules_parse_expand_and_survive_a_save() {
    let _lock = environment_lock();
    let environment = EnvironmentGuard::capture(&["HOME"]);
    environment.set("HOME", "/home/someone");
    let dir = tempfile::tempdir().unwrap();
    let text = "[trust]\nroots = [\"~/session-workspace\"]\nowners = [\"some-owner\", \"some-org\"]\n";
    let path = write(dir.path(), text);
    let registry = load(&path).unwrap();
    assert_eq!(registry.trust.roots, vec![PathBuf::from("/home/someone/session-workspace")]);
    assert_eq!(registry.trust.owners, vec!["some-owner".to_owned(), "some-org".to_owned()]);
    // `init` rewrites the whole file; a section serde does not know about
    // would be silently deleted the next time it runs.
    save(&registry, &path).unwrap();
    let reloaded = load(&path).unwrap();
    assert_eq!(reloaded.trust.owners.len(), 2);
}
```

- [ ] **Step 2: Run:** FAIL. **Step 3: Implement** per Interfaces. **Step 4:** `cargo test` — PASS.

---

### Task 15: Trust-rule resolution in the hooks, with session caching

**Files:**
- Modify: `src/hook/resolve.rs` (repo-root walk, rule matching)
- Modify: `src/hook/state.rs` (`SessionState` verdict cache)
- Modify: `src/commands/hook.rs` (`relevant_tool_match`, `opencode_chat_system`, `session_start` fall back to trust rules)

**Interfaces:**
- Produces, in `resolve.rs`:
  - `pub fn repo_root_above(path: &Path) -> Option<PathBuf>` — from the canonical existing parent, walk up to the first directory containing `.jj` or `.git`.
  - `pub fn trust_rule_match(paths: &[PathBuf], trust: &TrustRules, probe: &mut dyn FnMut(&Path) -> Option<bool>) -> Option<Match>` — for each path: canonicalize (reuse `canonical_path`), find `repo_root_above`; the root matches when (a) it sits under any `trust.roots` entry by path components (reuse the `strip_prefix` containment used by `managed_repo_for`), or (b) `probe(root)` returns `Some(true)` — the probe answers "do this repo's remote owners intersect trust.owners", letting the caller inject caching and keeping this function pure. A match yields `Match { repo: GuidanceRoot { name: root dir-name (parent-of-`default` rule reused from init via a small shared helper), root, kind: GuidanceRootKind::Trusted }, candidate }`.
  - **Security posture (must appear in `TrustRules::owners` doc comment and Task 18 docs):** `owners` matches SELF-DECLARED remote URLs read from the candidate checkout's own git config — it is not forge-authenticated, and any cloned repo can set a remote URL claiming any owner. This is an intentional convenience trust grant: what it grants is guidance injection as fenced data (the same grant as a `[trusted]` entry), never fork-command access. Use `owners` only when reading repo-owned AGENTS.md as data is acceptable for anything that can appear on the machine; prefer `roots` when in doubt.
- In `state.rs`: `SessionState` gains `#[serde(default)] owner_verdicts: HashMap<PathBuf, bool>` (in `DiskState` too), with `pub fn owner_verdict(&self, root: &Path) -> Option<bool>` and `pub fn record_owner_verdict(&mut self, root: &Path, verdict: bool)`; `clear()` clears it. Old state files deserialize fine (`serde(default)`).
- In `hook.rs`: a shared

```rust
fn match_with_trust(
    paths: &[PathBuf],
    registry: &Registry,
    cache: Option<(&Path, &str, &str)>, // (home, harness, session_id)
) -> anyhow::Result<Option<Match>>
```
  which tries `managed_repo_for` first, then `trust_rule_match` with a probe that: consults the cached verdict when `cache` is given; on miss runs `crate::jj::git_remotes(root)`, extracts owners with the URL-owner helper from Task 12 (move it to `resolve.rs` and have init use it from there), compares case-insensitively against `trust.owners`, and records the verdict via `SessionState::update`. `relevant_tool_match`, `opencode_chat_system`, and Claude `session_start`/`post_tool_use` route through it. Trusted matches inject guidance only — the existing `kind == Managed` checks already suppress notices and owner exports for trusted roots; verify each call site preserves that.

- [ ] **Step 1: Write the failing tests** in `src/hook/resolve.rs`:

```rust
#[test]
fn a_repo_under_a_trust_root_is_a_trusted_guidance_root() {
    // Given: a workspace-shaped checkout under a trusted subtree, never registered
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("session-workspace/platform/default");
    std::fs::create_dir_all(root.join(".jj")).unwrap();
    std::fs::write(root.join("AGENTS.md"), "rules\n").unwrap();
    let trust = crate::config::TrustRules {
        roots: vec![dir.path().join("session-workspace")],
        owners: vec![],
    };
    // When: a file inside it is resolved with no owner probe available
    let mut probe = |_: &std::path::Path| None;
    let hit = trust_rule_match(&[root.join("AGENTS.md")], &trust, &mut probe)
        .expect("a root rule needs no probe");
    // Then: it is trusted, named for its parent because the leaf is `default`
    assert_eq!(hit.repo.kind, crate::config::GuidanceRootKind::Trusted);
    assert_eq!(hit.repo.name, "platform");
    assert_eq!(hit.repo.root, root.canonicalize().unwrap());
}

#[test]
fn a_repo_matching_a_trusted_owner_is_found_through_the_probe() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("elsewhere/tool");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let trust = crate::config::TrustRules { roots: vec![], owners: vec!["someone".to_owned()] };
    let mut asked = Vec::new();
    let mut probe = |p: &std::path::Path| { asked.push(p.to_owned()); Some(true) };
    let hit = trust_rule_match(&[root.join("src/lib.rs")], &trust, &mut probe);
    assert!(hit.is_some());
    assert_eq!(asked.len(), 1, "the probe is asked once per root");

    let mut deny = |_: &std::path::Path| Some(false);
    assert!(trust_rule_match(&[root.join("src/lib.rs")], &trust, &mut deny).is_none());
}

#[test]
fn a_sibling_of_a_trust_root_sharing_its_name_prefix_is_outside() {
    // session-workspace-2 shares the string prefix; component containment must reject it,
    // the same trap managed_repo_for and the plugin's isInside avoid.
    let dir = tempfile::tempdir().unwrap();
    let inside = dir.path().join("session-workspace-2/repo");
    std::fs::create_dir_all(inside.join(".jj")).unwrap();
    let trust = crate::config::TrustRules {
        roots: vec![dir.path().join("session-workspace")],
        owners: vec![],
    };
    let mut probe = |_: &std::path::Path| None;
    assert!(trust_rule_match(&[inside.join("x")], &trust, &mut probe).is_none());
}
```
And in `src/hook/state.rs`: a round-trip test that `owner_verdicts` persists and `clear()` empties it (mirror the existing state tests' shape). Also an ADVERSARIAL test in `resolve.rs` documenting the spoofability boundary: a repo OUTSIDE every `roots` entry whose probe answers `Some(true)` (modeling a checkout that self-declares a trusted owner's remote URL) DOES match — assert it matches and that its kind is `Trusted`, with a comment stating this is the accepted, documented trade-off (guidance-as-data only), so a future reader finds the decision where the behavior lives.

- [ ] **Step 2: Run:** FAIL.

- [ ] **Step 3: Implement the resolver and state layers ONLY** (`resolve.rs` + `state.rs`) per Interfaces. Order of checks inside `trust_rule_match`: roots first (free), owner probe second (spawns git). The dir-name helper: `fn guidance_name(root: &Path) -> String` — `default`-leaf → parent name, else leaf name; move `init::repo_name`'s body there and re-export for init.

- [ ] **Step 4: Run:** `cargo test` — resolver/state tests PASS. Hook behavior is NOT yet wired; do not mark the task done here.

- [ ] **Step 4b: Wire `hook.rs`** through `match_with_trust` at all four call sites (`relevant_tool_match`, `opencode_chat_system`, Claude `session_start`/`post_tool_use`), then run `cargo test --test hook_opencode --test hook_claude_code` — the existing hook contract tests still pass (trust rules default to empty, so behavior without config is unchanged). The task is incomplete until the hook paths actually consult trust rules.

- [ ] **Step 5: End-to-end check by hand** (the hook binary is the real surface):

```bash
cargo build
printf '%s' '{"event":"tool.execute.after","session_id":"manual-test","tool":"read","args":{"filePath":"'"$HOME"'/session-workspace/platform/default/AGENTS.md"},"parts":{"notice":true,"guidance":true}}' \
  | KNIVES_CONFIG_HOME=/tmp/knives-manual ./target/debug/knives hook opencode
```
with `/tmp/knives-manual/repos.toml` containing `[trust]\nroots = ["~/session-workspace"]`. Expected: JSON whose `addition` contains the AGENTS.md body wrapped in a `knives-guidance` envelope. Then repeat with an empty `repos.toml` — expected `{"addition":""}`.

---

### Task 16: `knives register` prints a paste-ready snippet

**Files:**
- Modify: `src/cli.rs` (new `Command::Register`; reachability test gains `vec!["knives", "register"]`)
- Create: `src/commands/register.rs`
- Modify: `src/commands.rs` (module), `src/main.rs` (dispatch)

**Interfaces:**
- Produces: `register::run(target: Option<PathBuf>) -> anyhow::Result<Exit>` and a pure `register::snippet(name: &str, entry: &RepoEntry) -> String`. Reuses `init::decide` (public already) for remote-role detection. Prints the snippet plus one line of instruction; **writes nothing** — the human pastes it into `repos.toml`, and because every hook invocation reloads the file, it is live on the next tool call (say this in the output). Missing roles → same message as init, `Exit::Usage`.

- [ ] **Step 1: Write the failing test** in `src/commands/register.rs`:

```rust
#[test]
fn the_snippet_is_valid_toml_that_round_trips_into_a_registry_entry() {
    // The whole command is "print what a human would paste"; a snippet that
    // does not parse back into the same entry is worse than no command.
    let entry = crate::config::RepoEntry {
        path: std::path::PathBuf::from("/home/someone/forks/tool/default"),
        upstream: "https://forge.invalid/maintainer/tool.git".to_owned(),
        origin: "https://forge.invalid/someone/tool.git".to_owned(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let text = snippet("tool", &entry);
    assert!(text.starts_with("[repos.tool]"), "was: {text}");
    let parsed: crate::config::Registry = toml::from_str(&text).expect("snippet parses");
    assert_eq!(parsed.repos["tool"], entry);
}
```

- [ ] **Step 2: Run:** FAIL.

- [ ] **Step 3: Implement.** `snippet` serializes a one-entry `Registry` with `toml::to_string_pretty` (guarantees the round-trip by construction). `run` mirrors `init::run`'s shape: resolve target dir, require `.jj`, `jj::git_remotes`, `init::decide`; on `Adopted { name, entry, warnings }` print warnings, the snippet, and: `"paste this into {config_path} to register; hooks reload the registry on every event, so it takes effect on the next tool call"`. On anything else, render init's message, `Exit::Usage`. CLI doc comment: `/// Print a registry snippet for this repo. Writes nothing: registration is a trust grant, so a human pastes it.`

- [ ] **Step 4: Run:** `cargo test` and `cargo run -- register --help` — PASS.

---

### Task 17: Status branch table

**Files:**
- Modify: `src/commands/status.rs` (`branch_line` → table builder; `render`)

**Interfaces:**
- Produces: `fn branch_table(rows: &[BranchRow]) -> Vec<String>` replacing the per-row `branch_line` in `render` (delete `branch_line`). Columns, in order: `branch  tip  push  pr  review  checks  landed  flags`. Cell content mapping (all data already computed today, only layout changes):
  - `branch`: name. `tip`: `short(tip)` or `divergent`.
  - `push`: `unpushed` | `pushed` | `unpushed-commits` | `origin=<short> (behind|diverged|unresolved)`.
  - `pr`: `#N`, plus ` <state>` when not open, plus ` draft`; `no-pr` when neither inferred nor stated; stated ones render `#N <state> (stated)`.
  - `review`: the review decision, `no-review` when a PR exists without one, `-` otherwise.
  - `checks`: `failing` | `none-ran` | `ok` (open PR with consulted checks), `-` when not consulted or no open PR.
  - `landed`: the verdict's Display, `-` when absent.
  - `flags`: comma-joined subset of `CONFLICTING`, `behind-base`, `review-stale`, `fork-only`; `-` when empty.
  - Layout: header row + one row per branch; each column padded to its widest cell with two spaces between; rows produced by `format!("    {:<w0$}  {:<w1$}  ...")` with widths computed over header+cells; trailing whitespace trimmed per line (`String::trim_end`). JSON output untouched.

- [ ] **Step 1: Write the failing tests** in `src/commands/status.rs`:

```rust
#[test]
fn branch_rows_render_as_an_aligned_table_with_a_header() {
    // Vertical alignment without horizontal alignment made ten-branch reports
    // unreadable: every fact was present and nothing lined up.
    let with_pr = row("feat/alpha", Some(LandedVerdict::InTrunk), Some(pull_request(1128)));
    let bare = row("fix/a-much-longer-branch-name", None, None);
    let lines = branch_table(&[with_pr, bare]);
    assert_eq!(lines.len(), 3, "header plus one row per branch: {lines:?}");
    let header = &lines[0];
    assert!(header.contains("branch") && header.contains("pr") && header.contains("landed"));
    // Every row starts each column at the same offset as the header.
    let column_start = |line: &str, word: &str| line.find(word).unwrap_or(usize::MAX);
    let tip_at = column_start(header, "tip");
    for line in &lines[1..] {
        assert!(line.len() >= tip_at, "short row breaks alignment: {line:?}");
    }
    assert!(lines[1].contains("#1128"), "was: {}", lines[1]);
    assert!(lines[1].contains("APPROVED"));
    assert!(lines[2].contains("no-pr"));
    assert!(lines[2].contains('-'), "empty cells render as placeholders, not gaps");
}

#[test]
fn an_empty_cell_never_shifts_its_neighbours() {
    let with_flags = {
        let mut pr = pull_request(7);
        pr.mergeable = "CONFLICTING".to_owned();
        row("feat/conflicted", None, Some(pr))
    };
    let plain = row("feat/plain", None, None);
    let lines = branch_table(&[with_flags, plain]);
    let header_landed = lines[0].find("landed").expect("header names the column");
    // The landed column starts at the same offset in every row.
    for line in &lines[1..] {
        let cell: String = line.chars().skip(header_landed).take(1).collect();
        assert!(!cell.is_empty(), "was: {line:?}");
    }
    assert!(lines[1].contains("CONFLICTING"));
}
```
(Existing render tests that assert on old `branch_line` output — check them: `a_problem_is_printed...` and exit tests don't touch branch lines; any that do get updated to the table's content.)

- [ ] **Step 2: Run:** FAIL.

- [ ] **Step 3: Implement** `branch_table`: build `Vec<[String; 8]>` of cells from the mapping above (extract the existing `branch_line` match arms into per-column helpers so no logic is invented, only rearranged), compute per-column max width including the header, emit padded lines with four-space indent, `trim_end` each. Wire into `render` in place of the `.map(branch_line)` call.

- [ ] **Step 4: Run:** `cargo test`, then eyeball the real thing: `cargo run -- status --text` inside this very repo (it is registered on the machine this plan executes on; if not, skip the eyeball) — columns line up.

---

### Task 18: Documentation — skills, README, config reference

**Files:**
- Modify: `skills/using-knives/SKILL.md`
- Modify: `skills/fork-work/SKILL.md`
- Modify: `README.md` (config example)

- [ ] **Step 1:** `skills/using-knives/SKILL.md` — apply the `updating-docs` skill's rules (rewrite sections, don't append changelog prose):
  - Registry reference section (~line 131–143): document `base` as upstream's trunk (default `main`, opencode-style forks set `dev`), `release_branch` for the fixed scheme, `[trust]` roots/owners, and that edits take effect on the next hook event. The `[trust].owners` entry MUST carry the security caveat verbatim from Task 15: owner matching reads self-declared remote URLs from the checkout, is not forge-authenticated, and grants guidance-as-data injection only — prefer `roots` when in doubt.
  - `knives release` section: a paragraph on the fixed scheme — cut rebuilds the flat octopus and advances the one branch in place; the branch's previous position (its remote-tracking ref before the push) plays the role of the previous release; `cut` needs no name; publishing is still your `jj git push --bookmark <name>`.
  - `knives status`: note the branch table columns.
  - New `knives register` entry: prints a paste-ready snippet, writes nothing, human pastes it, live on next tool call.
  - `init`: origin/upstream convention line and the miswiring warning.
- [ ] **Step 2:** `skills/fork-work/SKILL.md` — one short addition: when a needed repo is unregistered, run `knives register` and hand the snippet to the human; do not edit `repos.toml` yourself.
- [ ] **Step 3:** `README.md` — extend the config example with `base`, `release_branch`, and `[trust]`, one comment line each.
- [ ] **Step 4:** Re-read both SKILL.md files end-to-end for statements the code changes made false (dated-only phrasing like "plans and cuts dated releases" in the using-knives description frontmatter — update it).

---

### Task 19: Gates, commit, PR

- [ ] **Step 1:** Full gate run:

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --workspace -- -D warnings
cargo nextest run --all-targets --all-features --workspace || cargo test --all-targets --all-features --workspace
bun run lint && bun run typecheck && bun run test:knives-plugin
```
All green. Fix anything red before proceeding — no `#[allow]` escapes without a `reason` that satisfies the house style.

- [ ] **Step 2:** Self-review the diff: `jj diff --git --stat` then `jj diff --git`. Check: no leftover `TRUNK` constant references, no `println!` debugging, every new public item has the house-style doc comment, no real user/org/repo names in `src/` or test literals.

- [ ] **Step 3:** Single commit and PR (jj, per Global Constraints):

```bash
jj describe -m "feat: configurable fork shape

Trunk from config (base widens to upstream's trunk), fixed release branch
scheme (release_branch -> ReleaseScheme matched exhaustively), remote-role
reporting fixes (miswired-origin warning in init, PR heads matched from every
role remote's owner, consumer pins read from the consumer's origin trunk),
[trust] rules for guidance injection with knives register, and a status
branch table."
jj bookmark set feat/configurable-fork-shape -r @
jj git push --named feat/configurable-fork-shape=@
gh pr create --title "feat: configurable fork shape" --body-file - <<'EOF'
Implements docs/superpowers/specs/2026-08-03-configurable-fork-shape-design.md

- trunk is configuration: `base` widens to "upstream's trunk"; TRUNK/UPSTREAM_TRUNK constants deleted (fixes `knives start` on dev-trunk forks)
- fixed release branch: `release_branch = "..."` derives a ReleaseScheme enum matched at every release-aware site; previous release = the branch's pre-push remote position (the seam #4 will use)
- closes #2, closes #3 (reframed to [trust] rules + a write-nothing `knives register`), closes #5
- status branches render as an aligned table
EOF
```

- [ ] **Step 4:** Watch CI; fix failures; then run the `post-pr` skill's sweep before reporting merge-ready.

---

## Self-Review Notes (already applied)

- **Spec coverage:** §1 trunk → Tasks 1–4; §2 fixed scheme → Tasks 5–10; §3 reporting gaps → Tasks 11–13; §4 trust rules + register → Tasks 14–16; §5 table → Task 17; testing/docs → woven through + Task 18. The spec's "future-dated-name check" has no counterpart in today's code — nothing to skip; the enum match documents non-applicability (noted in Task 8).
- **Deviation from spec, deliberate:** the spec's cut description says "then push to the release remote"; `release::cut`'s contract is that knives never pushes, and this plan keeps that invariant (Global Constraints). The previous-position capture works pre-push by construction.
- **Type consistency:** `ReleaseScheme` and `is_release_name` live in `ids.rs` (config depends on ids, never the reverse; status/preflight/release share the predicate without depending on a rendering module); the URL-owner helper lands in `resolve.rs` at Task 15 with init consuming it (Task 12 may host it temporarily in `init.rs` — Task 15 moves it and fixes imports).

## Oracle plan-review amendments (2026-08-03, applied before execution)

1. Fixed-scheme release scanning/selection uses ONLY the publish remote (`release` when split, else `origin`); the non-publish role remote is never a candidate (Tasks 7, 8).
2. `previous_position` is explicitly remote-only (never the local fixed bookmark) with a test pinning that; soundness argument recorded at its Interfaces block (Task 8).
3. Task 10's repo reopen after push/fetch is mandatory, not a fallback.
4. Task 8's `plan()` snippet computes the scheme once and filters by publish remote — no unused binding, no unstable origin/release tie.
5. `pin_lag` returns `PinLag { lag, notes }` so Task 13's consumer-origin annotations have a visible channel in `knives repos` (Tasks 9, 13).
6. Trust `owners` spoofability documented as an accepted trade-off + adversarial test mandated (Tasks 15, 18).
7. Task 4 enumerates EVERY `"main"` literal in lab.rs including `advance_upstream()` and the constructor, with a grep acceptance check.
8. `is_release_name` moved to `ids.rs` (Task 7).
9. Task 15 split: resolver/state implementation (Steps 3–4) and hook wiring (Step 4b) are separate gates — the task is not done until hook paths consult trust rules.
