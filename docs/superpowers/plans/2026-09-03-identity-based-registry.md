# Identity-Based Registry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `repos.toml` names repositories, not directories: a checkout is bound to its entry by its `upstream` remote, checkouts are found by scanning `$HOME`, trust is its own identity-keyed list, and no knives command writes the registry.

**Architecture:** A new `bind` module owns "which entry is this checkout" (walk-up from cwd, scan of `$HOME`, remote normalisation) and produces a `Fork { name, entry, checkout }` that every verb works on instead of `entry.path`. The hook resolves a touched file to two independent facts — managed (its `upstream` matches a `[repos.*]` entry) and trusted (`[trust]` rules match any remote or the root) — and only trusted grants guidance. `init` and `config::save` are deleted; `register` prints an entry without `path`.

**Tech Stack:** Rust (edition per `Cargo.toml`, clippy pedantic-clean, `expect_used` denied), `jj` colocated with git, `serde`/`toml`, the lab harness in `tests/common/lab.rs`, bun for `plugin/knives.test.ts`.

**Spec:** `docs/superpowers/specs/2026-09-03-identity-based-registry-design.md` — the binding authority. Its "Settled while designing" list records decisions the human made; do not re-open them.

**History:** this plan was executed once; the checkout holding the result was destroyed before it was pushed. The review findings from that execution are folded into the tasks below (marked *review-ruled*), so the rebuild lands the reviewed state directly. Every task commit is pushed to the `wip/identity-registry` bookmark by the coordinator as soon as its review is clean.

## Global Constraints

Copied from the spec. Every task's requirements implicitly include these.

- **Identity is the remote named `upstream`.** A checkout is entry X when its `upstream` remote URL equals X's `upstream` after normalisation. `origin` and `release` never affect binding.
- **Normalisation:** a value that parses as a remote URL (`scheme://host/path` or `user@host:path`) compares as `(host without user, path)` with trailing `/` and `.git` stripped, case-insensitively. A value that does not parse compares as its trimmed string — `.git` is **not** stripped from a non-URL value (*review-ruled*: two filesystem paths differing by `.git` are two directories).
- **Remote reader dispatches on the repository marker** (planning ruling): `.jj` present → `jj -R <root> --ignore-working-copy git remote list`; only `.git` present → `git -C <root> config --get-regexp '^remote\..*\.url$'`; neither → error. No fallback chain. `jj::git_remotes` and `jj::git_toplevel` are deleted.
- **Two roots** (*review-ruled*): `bind::checkout_root(path)` = the checkout a path belongs to (a workspace's `.jj/repo` pointer is followed); `bind::nearest_root(path)` = the first ancestor holding `.jj` or `.git`, pointer not followed (a workspace is its own root). Both return at the first marker of either kind; nested repositories never inherit an enclosing checkout's identity.
- **Scan:** `$HOME`, depth 3 (`~/a/b/c` is depth 3 and is visited; its children are not), skip directory names starting with `.`, never follow symlinks, stop descending at a directory containing `.jj`. A directory whose `.jj/repo` is a directory is a checkout candidate; one whose `.jj/repo` is a file is a workspace and is skipped; a `.git`-only directory is not a candidate. No depth knob, no root list.
- **Scan refusals:** two checkouts for one entry → refused, both paths named, the tool never picks. No checkout → `knives repos` prints `not on this machine`; `status --all`/`sync --all` report it as a problem row; a named single-repo verb exits `Usage`. Scan problems (directories whose remotes could not be read) are surfaced beside the missing line, never dropped (*review-ruled*).
- **Remote notes:** once bound, the checkout's `origin` and `release` remotes are compared to the entry's; each absence or difference is a **note** (never a finding, never a fallback) on that repository in `status` and `repos`, with exactly these texts: `origin remote is <X>; registry says <Y>`, `origin remote absent; registry says <Y>`, `release remote is <X>; registry says <Y>`, `release remote absent; registry says <Y>`. `release` is compared only when the entry has `release`.
- **Registry shape:** `[repos.<name>]` = `upstream`, `origin` (required), `release`, `base`, `release_branch`, `test_count_command`, `consumers`, `workspaces`. `path` is gone. `[trusted.*]` is gone. `[trust]` = `repos` (new, `owner/repo` slugs; a trailing `.git` on a configured slug is stripped before matching — *review-ruled*), `owners`, `roots`.
- **Load rejects, with exactly these messages:** `[repos.<name>] path is no longer a registry field; delete it — knives finds checkouts by their remotes`; `[trusted.<name>] is no longer a registry table; move it to [trust] repos = ["<owner>/<repo>"]`; `[repos.<a>] and [repos.<b>] share upstream <url>; identity must be unique`; `[trust] repos takes forge slugs ("<owner>/<repo>"); found "<value>"`. Every other unknown field fails through `#[serde(deny_unknown_fields)]`. No compatibility read.
- **Fork entries no longer grant guidance.** Managed = notice, claim roster, `KNIVES_OWNER` derivation, `seen` observation. Trusted = guidance injection. A checkout can be both, either, or neither. The hook never fails a session: an unreadable remote set is printed to stderr as `knives hook: <error>` and contributes no remote facts (`roots` still applies) — *review-ruled*.
- **Nothing writes the registry.** `knives init` and `config::save` are deleted. `knives register` prints; on an already-registered checkout it prints `already registered as <name>` and exits 0. `register` binds through `bind::checkout_root`, so it works from any directory inside a checkout (*review-ruled*).
- **Which verbs bind how:** single-repo verbs (`start`, `finish`, `track`, `depends`, `notch`, `release …`, `preflight`, `pr`, `pushed`, `consumers`) take the named repo, else the one cwd binds to, else `Usage` with the reason; a bound checkout that is not a jj checkout is refused with `<root> is a git clone, not a jj checkout; fork commands need jj`. Many-repo verbs (`status`, `audit`) keep today's rule: a name selects one, a binding cwd selects that one, otherwise every entry via the scan. `sync` keeps its rule: name, `--all`, binding cwd, else `Usage`. `knives repos` takes no name and always scans.
- **`knives repos` JSON** (planning ruling): `path` is `Option<String>` — the found location or `null`; the `trusted` array is gone; `[trust]` is not rendered.
- **`workspaces` inside the checkout** (planning ruling): the refusal moves from registry load to `start`/`finish`, same message text as today (`config.rs:628-636`).
- **Cross-fork dependency checks** (*review-ruled*): a sibling's forge identity is `remote_slug(entry.upstream)` from the registry, and `gh` runs in the current fork's checkout; `pull_facts` needs only `name_with_owner`. A sibling whose `upstream` is not a forge URL yields the problem `<name>: upstream <url> is not a forge repository; cannot check dependencies against it` and no forge call.
- **No real identities in shipped text.** `tests/no_hardcoded_identity.rs` scans `src/`, `plugin/`, `docs/`, `skills/`, `hooks/`: use `forge.example` / `forge.invalid` hosts and `org/tool`-style names everywhere, including this plan and every test.
- **Version control is jj.** Commit = `jj describe -m "<conventional message>"` then `jj new` **before** the next piece of work (the coordinator does the `jj new`). Never `git add`/`git commit`. Run `unset JJ_USER JJ_EMAIL` before any `jj describe` (a lab identity leaks into shells on this machine). Conventional-commit prefixes; the breaking-change marker (`!` after the type, plus a `BREAKING CHANGE:` footer) goes on the one commit that breaks the registry format (Task 3) and on the PR title, nowhere else.
- **Test commands:** `env -u JJ_EMAIL -u JJ_USER cargo test` (whole suite), `cargo test --test <file>`, `cargo test <filter>` (unit), `cargo clippy --all-targets` (must stay clean), `cargo fmt --check`, and `cd plugin && KNIVES_BIN=$PWD/../target/debug/knives bun test` (the real-binary plugin tests run only with `KNIVES_BIN` set). The built binary is `target/debug/knives`; `KNIVES_CONFIG_HOME=<dir>` points it at a registry; `HOME` is what the scan reads.
- **Destructive commands.** No task runs `rm -rf` on a path containing a variable or `~`; hermetic environments are set per command (`env HOME=/tmp/x cmd`), never by `export` in a separate shell call. Temporary fixtures live under `mktemp -d` paths that are spelled literally when removed.

---

## File Structure

| File | Responsibility after this plan |
|---|---|
| `src/bind.rs` (new) | `Checkout`, `Fork`, remote reading, URL normalisation and the URL helpers, `checkout_root`, `nearest_root`, `here`, `scan`, `resolve`, remote notes. The only module that answers "which entry is this directory". |
| `src/config.rs` | Registry content only: `RepoEntry` (policy + remotes by role), `TrustRules` (`repos`, `owners`, `roots`, `grants`), `Registry`, `load` with the named rejections. No paths, no `save`. |
| `src/hook/resolve.rs` | Touched-path → `Match { root, candidate, managed, trusted }`. |
| `src/hook/state.rs` | Per-session cache of a checkout's full remote map (was owner list). |
| `src/commands/hook.rs` | Managed gates notice/claims/owner/seen; trusted gates guidance. |
| `src/commands/register.rs` | The one registry-adjacent command: reads a checkout's remotes, prints a `[repos.<name>]` snippet or `already registered as <name>`. Absorbs `decide` from `init.rs`. |
| `src/commands/init.rs` | **Deleted.** |
| `src/main.rs` | Resolves a `Fork` per verb through `bind`; `selected` builds `Selected` rows via one scan. |
| Every module reading `entry.path` | Reads `fork.checkout.path`; `entry` accessors for remotes/policy unchanged. |
| `tests/common/lab.rs` | Registries without `path`; every binary builder sets `HOME` to the lab's temp directory so any scan is hermetic. |
| `tests/registry_binding.rs` (new) | Integration: binding from checkout/workspace/nested/outside, scan hits and refusals, `resolve`, rejections, `register` output, `sync`/`status` selection rules. |
| `~/.dotfiles/knives/repos.toml`, `installers/knives.sh` | Already rewritten in the `registry` jj workspace at `~/.dotfiles-registry` (change `vvqklwsu`, unpushed). Task 5 is complete. |

## Task dependency graph

```
Task 1 (bind module) ──> Task 2 (trust + hook) ──> Task 3 (fork cutover) ──> Task 4 (docs)
Task 5 (dotfiles) — complete; pushed by the coordinator after release
```

Tasks 2 and 3 both edit `src/config.rs`, `src/commands/repos.rs`, `src/commands/hook.rs`; they serialise in that order.

---

### Task 1: The `bind` module

**Depends on:** none.

**Files:**
- Create: `src/bind.rs`
- Modify: `src/lib.rs` (add `pub mod bind;` beside the other modules, and a crate-doc bullet "[`bind`] decides which registry entry a directory is, from its remotes"); `src/config.rs:485` `home_dir` becomes `pub`
- Modify: `src/hook/resolve.rs` — `remote_authority_and_path` (154-161) and `url_owner` (148-152) **move** to `src/bind.rs` as `pub fn`; `resolve.rs` imports them from `bind` (*review-ruled*: `config` must not depend on `hook`)
- Modify: `tests/common/lab.rs` — make the free `jj` helper (line 688) `pub` (the lab convention; clippy rejects `pub(crate)` there), and add `pub fn temp_path(&self) -> &Path { self.temp.path() }` on `Lab` beside `work_path` (466)
- Test: unit tests inside `src/bind.rs`; integration `tests/registry_binding.rs` (create; `#[path = "common/lab.rs"] mod lab;` as every test does)

**Interfaces:**
- Consumes: `crate::config::{Registry, RepoEntry, Role}`, `crate::ids::RepoName`.
- Produces (exact signatures later tasks rely on):

```rust
// src/bind.rs
//! Which registry entry a directory is, decided by its remotes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::{Registry, RepoEntry, Role};
use crate::ids::RepoName;

/// A repository root on this machine and the remotes it declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    /// The checkout root: the directory whose `.jj/repo` is a directory (or the
    /// `.git`-only root). A workspace resolves to its checkout, never to itself.
    pub path: PathBuf,
    pub remotes: BTreeMap<String, String>,
}

impl Checkout {
    /// Whether this is a jj checkout (`.jj/repo` is a directory). Fork verbs need one;
    /// the hook binds git-only clones too.
    pub fn is_jj(&self) -> bool;
}

/// A registry entry bound to the checkout that is it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fork<'a> {
    pub name: RepoName,
    pub entry: &'a RepoEntry,
    pub checkout: Checkout,
}

impl Fork<'_> {
    /// `entry.workspaces`, else the checkout's parent directory.
    pub fn workspace_root(&self) -> &Path;
    /// The spec's four note texts for `origin`/`release`, in that order; empty when both match.
    pub fn remote_notes(&self) -> Vec<String>;
}

#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("{root} is neither a jj nor a git repository")]
    NotARepository { root: PathBuf },
    #[error("reading remotes of {root}: {detail}")]
    Remotes { root: PathBuf, detail: String },
}

/// Why `here` did not bind.
#[derive(Debug, PartialEq, Eq)]
pub enum Unbound {
    /// No `.jj` or `.git` at or above the directory.
    NotInsideARepository,
    /// A repository, but it declares no `upstream` remote.
    NoUpstream { root: PathBuf },
    /// A fork of something the registry does not list.
    Unregistered { root: PathBuf, upstream: String },
}

impl Unbound {
    /// `not inside a repository; name a repo, or run this from inside one`
    /// `<root> has no \`upstream\` remote, so it is not a managed fork; name a repo`
    /// `<root> forks <upstream>, which is not in the registry; \`knives register\` prints the entry`
    pub fn message(&self) -> String;
}

/// The first ancestor of `path` (canonicalised) that is a repository root: a
/// directory holding `.jj` or `.git`. Nearest marker of either kind wins. A jj
/// workspace is its own root here.
pub fn nearest_root(path: &Path) -> Option<PathBuf>;

/// The checkout `path` belongs to: `nearest_root`, then — when that root's
/// `.jj/repo` is a file — the checkout the pointer names. An unreadable pointer
/// returns the workspace root itself so the remote reader surfaces jj's own error.
pub fn checkout_root(path: &Path) -> Option<PathBuf>;

/// Remotes of the repository rooted at `root`, read from jj when `.jj` is present,
/// from git when only `.git` is present.
pub fn remotes(root: &Path) -> Result<BTreeMap<String, String>, BindError>;

/// Whether two remote spellings name one repository (see Global Constraints).
pub fn same_remote(a: &str, b: &str) -> bool;

/// `(authority, path)` of `scheme://authority/path` or `user@authority:path`; `None` otherwise.
pub fn remote_authority_and_path(url: &str) -> Option<(&str, &str)>;

/// The owner segment of a forge remote path, when the URL parses.
pub fn url_owner(url: &str) -> Option<&str>;

/// The `owner/repo` path of a forge remote with trailing `/` and `.git` removed; `None` for a non-URL.
pub fn remote_slug(url: &str) -> Option<&str>;

/// The entry whose `upstream` matches; `None` when none does.
pub fn entry_for<'a>(registry: &'a Registry, upstream: &str) -> Option<(RepoName, &'a RepoEntry)>;

/// The fork the current directory is inside.
pub fn here<'a>(registry: &'a Registry, cwd: &Path) -> Result<Result<Fork<'a>, Unbound>, BindError>;

/// Every entry's checkout under `home`, and what could not be decided.
#[derive(Debug, Default)]
pub struct Scan<'a> {
    pub found: BTreeMap<RepoName, Fork<'a>>,
    /// Entries with more than one checkout: every path, sorted.
    pub duplicates: BTreeMap<RepoName, Vec<PathBuf>>,
    /// Directories that looked like checkouts but whose remotes could not be read.
    pub problems: Vec<String>,
}

pub fn scan<'a>(registry: &'a Registry, home: &Path) -> Scan<'a>;

/// Why `resolve` did not produce a fork.
#[derive(Debug, PartialEq, Eq)]
pub enum Unresolved {
    Unknown,
    Missing { home: PathBuf },
    Duplicate { home: PathBuf, paths: Vec<PathBuf> },
}

impl Unresolved {
    /// `unknown repo <name>`  (the caller appends `; known: a, b` for this variant only)
    /// `no checkout of <name> under <home>`
    /// `<name> has <N> checkouts under <home>: <a>, <b>; knives will not choose`
    pub fn message(&self, name: &RepoName) -> String;
}

/// One named entry's fork: the cwd's when it binds to `name`, else the scan's.
/// A `BindError` from the cwd's own repository propagates: a checkout whose
/// remotes cannot be read is an error to show, not a reason to scan elsewhere.
pub fn resolve<'a>(
    registry: &'a Registry,
    name: &RepoName,
    cwd: &Path,
    home: &Path,
) -> Result<Result<Fork<'a>, Unresolved>, BindError>;
```

The scan root everywhere is `crate::config::home_dir()` (made `pub` in this task); `bind` has no `home()` of its own.

- [ ] **Step 1: Write the failing unit tests for normalisation, notes, and the message renderers**

Add to the bottom of the new `src/bind.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn entry(upstream: &str, origin: &str, release: Option<&str>) -> RepoEntry {
        RepoEntry {
            path: PathBuf::from("/unused"), // Task 3 deletes this field and this line
            upstream: upstream.to_owned(),
            origin: origin.to_owned(),
            base: None,
            release: release.map(str::to_owned),
            release_branch: None,
            test_count_command: None,
            consumers: vec![],
            workspaces: None,
        }
    }

    #[test]
    fn https_and_ssh_spellings_of_one_repository_are_the_same_remote() {
        assert!(same_remote("https://forge.example/org/tool", "git@forge.example:org/tool.git"));
        assert!(same_remote("https://forge.example/org/tool.git/", "HTTPS://Forge.Example/Org/Tool"));
        assert!(same_remote("ssh://git@forge.example/org/tool", "https://forge.example/org/tool"));
    }

    #[test]
    fn different_repositories_are_not_the_same_remote() {
        assert!(!same_remote("https://forge.example/org/tool", "https://forge.example/org/tool-2"));
        assert!(!same_remote("https://forge.example/org/tool", "https://forge.example/other/tool"));
        assert!(!same_remote("https://forge.example/org/tool", "https://elsewhere.example/org/tool"));
    }

    #[test]
    fn a_filesystem_path_compares_as_its_trimmed_text() {
        assert!(same_remote("/tmp/lab/upstream", " /tmp/lab/upstream/ "));
        assert!(!same_remote("/tmp/lab/upstream", "/tmp/lab/other"));
        // Two directories that differ by `.git` are two directories.
        assert!(!same_remote("/tmp/lab/origin.git", "/tmp/lab/origin"));
    }

    #[test]
    fn a_remote_slug_is_the_owner_and_repository_of_a_forge_url() {
        assert_eq!(remote_slug("https://forge.example/Org/Tool.git/"), Some("Org/Tool"));
        assert_eq!(remote_slug("git@forge.example:org/tool"), Some("org/tool"));
        assert_eq!(remote_slug("/tmp/lab/upstream"), None);
        assert_eq!(url_owner("git@forge.example:org/tool.git"), Some("org"));
    }

    #[test]
    fn matching_origin_and_release_produce_no_notes() {
        let registry_entry = entry(
            "https://forge.example/org/tool",
            "https://forge.example/ours/tool.git",
            Some("https://forge.example/company/tool"),
        );
        let fork = Fork {
            name: RepoName::new("tool"),
            entry: &registry_entry,
            checkout: Checkout {
                path: PathBuf::from("/checkout"),
                remotes: BTreeMap::from([
                    ("upstream".to_owned(), "https://forge.example/org/tool".to_owned()),
                    ("origin".to_owned(), "git@forge.example:ours/tool".to_owned()),
                    ("release".to_owned(), "https://forge.example/company/tool.git".to_owned()),
                ]),
            },
        };
        assert!(fork.remote_notes().is_empty(), "{:?}", fork.remote_notes());
    }

    #[test]
    fn a_different_origin_and_an_absent_release_are_each_one_note() {
        let registry_entry = entry(
            "https://forge.example/org/tool",
            "https://forge.example/ours/tool",
            Some("https://forge.example/company/tool"),
        );
        let fork = Fork {
            name: RepoName::new("tool"),
            entry: &registry_entry,
            checkout: Checkout {
                path: PathBuf::from("/checkout"),
                remotes: BTreeMap::from([
                    ("upstream".to_owned(), "https://forge.example/org/tool".to_owned()),
                    ("origin".to_owned(), "https://forge.example/stranger/tool".to_owned()),
                ]),
            },
        };
        assert_eq!(
            fork.remote_notes(),
            vec![
                "origin remote is https://forge.example/stranger/tool; registry says https://forge.example/ours/tool".to_owned(),
                "release remote absent; registry says https://forge.example/company/tool".to_owned(),
            ]
        );
    }

    #[test]
    fn release_is_not_compared_when_the_entry_has_none() {
        let registry_entry = entry("https://forge.example/org/tool", "https://forge.example/ours/tool", None);
        let fork = Fork {
            name: RepoName::new("tool"),
            entry: &registry_entry,
            checkout: Checkout {
                path: PathBuf::from("/checkout"),
                remotes: BTreeMap::from([("upstream".to_owned(), "https://forge.example/org/tool".to_owned())]),
            },
        };
        assert_eq!(
            fork.remote_notes(),
            vec!["origin remote absent; registry says https://forge.example/ours/tool".to_owned()]
        );
    }

    #[test]
    fn workspace_root_defaults_to_the_checkout_parent() {
        let registry_entry = entry("u", "o", None);
        let fork = Fork {
            name: RepoName::new("tool"),
            entry: &registry_entry,
            checkout: Checkout { path: PathBuf::from("/forks/tool/default"), remotes: BTreeMap::new() },
        };
        assert_eq!(fork.workspace_root(), Path::new("/forks/tool"));
        let mut with_workspaces = entry("u", "o", None);
        with_workspaces.workspaces = Some(PathBuf::from("/worktrees/tool"));
        let fork = Fork { entry: &with_workspaces, ..fork };
        assert_eq!(fork.workspace_root(), Path::new("/worktrees/tool"));
    }

    #[test]
    fn every_refusal_renders_its_exact_text() {
        assert_eq!(
            Unbound::NotInsideARepository.message(),
            "not inside a repository; name a repo, or run this from inside one"
        );
        assert_eq!(
            Unbound::NoUpstream { root: PathBuf::from("/r") }.message(),
            "/r has no `upstream` remote, so it is not a managed fork; name a repo"
        );
        assert_eq!(
            Unbound::Unregistered { root: PathBuf::from("/r"), upstream: "https://forge.example/o/t".to_owned() }.message(),
            "/r forks https://forge.example/o/t, which is not in the registry; `knives register` prints the entry"
        );
        let name = RepoName::new("tool");
        assert_eq!(Unresolved::Unknown.message(&name), "unknown repo tool");
        assert_eq!(
            Unresolved::Missing { home: PathBuf::from("/home/x") }.message(&name),
            "no checkout of tool under /home/x"
        );
        assert_eq!(
            Unresolved::Duplicate { home: PathBuf::from("/home/x"), paths: vec![PathBuf::from("/home/x/a"), PathBuf::from("/home/x/b")] }.message(&name),
            "tool has 2 checkouts under /home/x: /home/x/a, /home/x/b; knives will not choose"
        );
    }
}
```

- [ ] **Step 2: Run the unit tests to verify they fail**

Run: `cargo test bind::tests 2>&1 | tail -20`
Expected: compile error — `bind` module does not exist.

- [ ] **Step 3: Implement the pure parts: URL helpers, `same_remote`, `remote_slug`, notes, `workspace_root`, the renderers**

Move `remote_authority_and_path` and `url_owner` from `hook/resolve.rs` into `bind.rs` (make them `pub`; `resolve.rs` and `hook.rs` import `crate::bind::{remote_authority_and_path, url_owner}`). Then:

```rust
fn remote_key(remote: &str) -> String {
    let trimmed = remote.trim().trim_end_matches('/');
    match remote_authority_and_path(trimmed) {
        Some((authority, path)) => {
            let host = authority.rsplit('@').next().unwrap_or(authority);
            let path = path.trim_matches('/');
            let path = path.strip_suffix(".git").unwrap_or(path);
            format!("{}/{}", host.to_ascii_lowercase(), path.to_ascii_lowercase())
        }
        None => trimmed.to_owned(),
    }
}

pub fn same_remote(a: &str, b: &str) -> bool {
    remote_key(a) == remote_key(b)
}

pub fn remote_slug(url: &str) -> Option<&str> {
    let (_, path) = remote_authority_and_path(url.trim().trim_end_matches('/'))?;
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repository) = path.split_once('/')?;
    (!owner.is_empty() && !repository.is_empty() && !repository.contains('/')).then_some(path)
}

impl Fork<'_> {
    pub fn workspace_root(&self) -> &Path {
        self.entry
            .workspaces
            .as_deref()
            .unwrap_or_else(|| self.checkout.path.parent().unwrap_or(&self.checkout.path))
    }

    pub fn remote_notes(&self) -> Vec<String> {
        [("origin", Some(self.entry.remote(Role::Origin))), ("release", self.entry.release.as_deref())]
            .into_iter()
            .filter_map(|(role, expected)| {
                let expected = expected?;
                match self.checkout.remotes.get(role) {
                    None => Some(format!("{role} remote absent; registry says {expected}")),
                    Some(actual) if !same_remote(actual, expected) => {
                        Some(format!("{role} remote is {actual}; registry says {expected}"))
                    }
                    Some(_) => None,
                }
            })
            .collect()
    }
}
```

- [ ] **Step 4: Run the unit tests to verify they pass**

Run: `cargo test bind::tests 2>&1 | tail -20`
Expected: `test result: ok. 9 passed`.

- [ ] **Step 5: Write the failing integration tests for `nearest_root`, `checkout_root`, `remotes`, `here`, `scan`, `resolve`**

Create `tests/registry_binding.rs`:

```rust
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

//! A checkout is bound to its registry entry by its `upstream` remote, from the
//! directory you stand in or by scanning `$HOME`.

#[path = "common/lab.rs"]
mod lab;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use knives::bind::{self, Unbound, Unresolved};
use knives::config::{Registry, RepoEntry};
use knives::ids::RepoName;

fn entry(upstream: &str, origin: &str) -> RepoEntry {
    RepoEntry {
        path: PathBuf::from("/unused"), // Task 3 deletes this field and this line
        upstream: upstream.to_owned(),
        origin: origin.to_owned(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: vec![],
        workspaces: None,
    }
}

fn registry(entries: &[(&str, RepoEntry)]) -> Registry {
    Registry {
        repos: entries.iter().map(|(name, entry)| ((*name).to_owned(), entry.clone())).collect(),
        ..Registry::default()
    }
}

/// A jj checkout (colocated) with the given remotes: what the scan looks for.
fn jj_checkout(root: &Path, remotes: &[(&str, &str)]) {
    std::fs::create_dir_all(root).expect("create checkout");
    let jj = |args: &[&str]| {
        let status = std::process::Command::new("jj")
            .args(args)
            .current_dir(root)
            .env("JJ_CONFIG", "/dev/null")
            .env("JJ_USER", "Knives Lab")
            .env("JJ_EMAIL", "knives-lab@example.test")
            .status()
            .expect("run jj");
        assert!(status.success(), "jj {args:?} failed");
    };
    jj(&["git", "init", "--colocate"]);
    for (name, url) in remotes {
        jj(&["git", "remote", "add", name, url]);
    }
}

/// A git-only repository with the given remotes, the shape an agent's `/tmp` clone has.
fn git_repository(root: &Path, remotes: &[(&str, &str)]) {
    std::fs::create_dir_all(root).expect("create repository");
    let init = std::process::Command::new("git")
        .args(["-C", root.to_str().expect("utf-8"), "init", "--quiet"])
        .status()
        .expect("git init");
    assert!(init.success());
    for (name, url) in remotes {
        let added = std::process::Command::new("git")
            .args(["-C", root.to_str().expect("utf-8"), "remote", "add", name, url])
            .status()
            .expect("git remote add");
        assert!(added.success());
    }
}

#[test]
fn a_checkout_root_is_found_from_a_subdirectory_and_from_a_workspace() {
    let lab = lab::Lab::new();
    let nested = lab.work.join("src").join("deep");
    std::fs::create_dir_all(&nested).expect("nested directory");
    let workspace = lab.temp_path().join("ws");
    lab::jj(&lab.work, ["workspace", "add", "--name", "ws", workspace.to_str().expect("utf-8")]);

    let expected = lab.work.canonicalize().expect("canonical work");
    assert_eq!(bind::checkout_root(&nested), Some(expected.clone()));
    assert_eq!(bind::checkout_root(&workspace), Some(expected));
    // The workspace is its own nearest root; the checkout is the subdirectory's.
    assert_eq!(bind::nearest_root(&workspace), Some(workspace.canonicalize().expect("canonical ws")));
    assert_eq!(bind::nearest_root(&nested), Some(lab.work.canonicalize().expect("canonical")));
    assert_eq!(bind::checkout_root(lab.temp_path()), None);
}

#[test]
fn remotes_are_read_from_jj_checkouts_and_from_git_only_clones() {
    let lab = lab::Lab::new();
    let jj_remotes = bind::remotes(&lab.work).expect("jj remotes");
    assert_eq!(jj_remotes.get("upstream").map(String::as_str), Some(lab.upstream.to_str().expect("utf-8")));
    assert!(jj_remotes.contains_key("origin"));

    let clone = lab.temp_path().join("plain-clone");
    git_repository(&clone, &[("origin", "https://forge.invalid/someone/tool.git")]);
    let git_remotes = bind::remotes(&clone).expect("git remotes");
    assert_eq!(
        git_remotes,
        BTreeMap::from([("origin".to_owned(), "https://forge.invalid/someone/tool.git".to_owned())])
    );

    let plain = lab.temp_path().join("not-a-repo");
    std::fs::create_dir_all(&plain).expect("plain dir");
    assert!(bind::remotes(&plain).is_err());
}

#[test]
fn here_binds_the_checkout_and_its_workspaces_to_their_entry() {
    let lab = lab::Lab::new();
    let registry = registry(&[("demo", entry(lab.upstream.to_str().expect("utf-8"), "https://forge.invalid/acme/work.git"))]);
    let workspace = lab.temp_path().join("ws");
    lab::jj(&lab.work, ["workspace", "add", "--name", "ws", workspace.to_str().expect("utf-8")]);

    let from_checkout = bind::here(&registry, &lab.work).expect("read").expect("bound");
    assert_eq!(from_checkout.name, RepoName::new("demo"));
    assert_eq!(from_checkout.checkout.path, lab.work.canonicalize().expect("canonical"));
    assert!(from_checkout.checkout.is_jj());

    let from_workspace = bind::here(&registry, &workspace).expect("read").expect("bound");
    assert_eq!(from_workspace.checkout.path, from_checkout.checkout.path);
}

#[test]
fn the_nearest_repository_wins_when_one_is_nested_inside_another() {
    let lab = lab::Lab::new();
    let inner_git = lab.work.join("vendor").join("dep");
    git_repository(&inner_git, &[("upstream", "https://forge.invalid/org/dep")]);
    std::fs::create_dir_all(inner_git.join("src")).expect("nested source directory");
    let inner_jj = inner_git.join("nested").join("tool");
    jj_checkout(&inner_jj, &[("upstream", "https://forge.invalid/org/tool")]);
    let registry = registry(&[
        ("demo", entry(lab.upstream.to_str().expect("utf-8"), "https://forge.invalid/acme/work.git")),
        ("dep", entry("https://forge.invalid/org/dep", "https://forge.invalid/acme/dep")),
        ("tool", entry("https://forge.invalid/org/tool", "https://forge.invalid/acme/tool")),
    ]);

    let from_git = bind::here(&registry, &inner_git.join("src")).expect("read").expect("bound");
    assert_eq!(from_git.name, RepoName::new("dep"));
    assert_eq!(from_git.checkout.path, inner_git.canonicalize().expect("canonical"));
    assert!(!from_git.checkout.is_jj());
    let from_jj = bind::here(&registry, &inner_jj).expect("read").expect("bound");
    assert_eq!(from_jj.name, RepoName::new("tool"));
    let from_outer = bind::here(&registry, &lab.work.join("vendor")).expect("read").expect("bound");
    assert_eq!(from_outer.name, RepoName::new("demo"));
}

#[test]
fn here_refuses_outside_a_repository_without_upstream_and_when_unregistered() {
    let lab = lab::Lab::new();
    let registry = registry(&[("demo", entry("https://forge.invalid/org/elsewhere", "https://forge.invalid/acme/elsewhere"))]);

    let nowhere = lab.temp_path().join("nowhere");
    std::fs::create_dir_all(&nowhere).expect("plain dir");
    assert_eq!(bind::here(&registry, &nowhere).expect("read"), Err(Unbound::NotInsideARepository));

    let no_upstream = lab.temp_path().join("no-upstream");
    git_repository(&no_upstream, &[("origin", "https://forge.invalid/me/thing")]);
    let unbound = bind::here(&registry, &no_upstream).expect("read").expect_err("unbound");
    assert!(matches!(unbound, Unbound::NoUpstream { .. }), "{unbound:?}");

    let unbound = bind::here(&registry, &lab.work).expect("read").expect_err("unbound");
    assert!(
        matches!(&unbound, Unbound::Unregistered { upstream, .. } if upstream == lab.upstream.to_str().expect("utf-8")),
        "{unbound:?}"
    );
}

#[test]
fn scan_finds_each_entry_once_skips_workspaces_and_dot_directories_and_stops_at_depth_three() {
    // Given: a home with a checkout at depth 1, one at depth 3, a workspace, a
    // dot-directory hiding a checkout, a checkout at depth 4, and a git-only
    // clone whose upstream matches an entry.
    let lab = lab::Lab::new();
    let home = lab.temp_path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let shallow = home.join("tool");
    jj_checkout(&shallow, &[("upstream", "https://forge.invalid/org/tool")]);
    let git_only = home.join("plain");
    git_repository(&git_only, &[("upstream", "https://forge.invalid/org/plain")]);
    let deep_parent = home.join("forks").join("work");
    std::fs::create_dir_all(&deep_parent).expect("deep parent");
    let deep = deep_parent.join("default");
    std::fs::rename(&lab.work, &deep).expect("move checkout under home");
    let workspace = deep_parent.join("feature");
    lab::jj(&deep, ["workspace", "add", "--name", "feature", workspace.to_str().expect("utf-8")]);
    let hidden = home.join(".cache").join("tool");
    jj_checkout(&hidden, &[("upstream", "https://forge.invalid/org/hidden")]);
    let too_deep = home.join("a").join("b").join("c").join("d");
    jj_checkout(&too_deep, &[("upstream", "https://forge.invalid/org/too-deep")]);

    let registry = registry(&[
        ("tool", entry("https://forge.invalid/org/tool", "https://forge.invalid/acme/tool")),
        ("plain", entry("https://forge.invalid/org/plain", "https://forge.invalid/acme/plain")),
        ("work", entry(lab.upstream.to_str().expect("utf-8"), "https://forge.invalid/acme/work.git")),
        ("hidden", entry("https://forge.invalid/org/hidden", "https://forge.invalid/acme/hidden")),
        ("too-deep", entry("https://forge.invalid/org/too-deep", "https://forge.invalid/acme/too-deep")),
    ]);

    let scan = bind::scan(&registry, &home);

    assert_eq!(
        scan.found.keys().map(ToString::to_string).collect::<Vec<_>>(),
        vec!["tool".to_owned(), "work".to_owned()],
        "problems: {:?}",
        scan.problems
    );
    assert_eq!(scan.found[&RepoName::new("work")].checkout.path, deep.canonicalize().expect("canonical"));
    assert!(scan.duplicates.is_empty(), "{:?}", scan.duplicates);
    // `plain` (git-only), `hidden` (dot-directory) and `too-deep` (depth 4) are
    // not found; the jj checkout is found once although its workspace is under home too.
    std::fs::rename(&deep, &lab.work).expect("move checkout back for Lab's cleanup");
}

#[test]
fn scan_refuses_to_choose_between_two_checkouts_of_one_entry() {
    let lab = lab::Lab::new();
    let home = lab.temp_path().join("home");
    let first = home.join("one");
    let second = home.join("two");
    jj_checkout(&first, &[("upstream", "https://forge.invalid/org/tool")]);
    jj_checkout(&second, &[("upstream", "https://forge.invalid/org/tool")]);
    let registry = registry(&[("tool", entry("https://forge.invalid/org/tool", "https://forge.invalid/acme/tool"))]);

    let scan = bind::scan(&registry, &home);

    assert!(scan.found.is_empty());
    assert_eq!(scan.duplicates.get(&RepoName::new("tool")).map(Vec::len), Some(2));
}

#[test]
fn resolve_prefers_the_current_directory_then_the_scan_then_says_why_not() {
    let lab = lab::Lab::new();
    let home = lab.temp_path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let elsewhere = home.join("elsewhere");
    jj_checkout(&elsewhere, &[("upstream", "https://forge.invalid/org/elsewhere")]);
    let registry = registry(&[
        ("demo", entry(lab.upstream.to_str().expect("utf-8"), "https://forge.invalid/acme/work.git")),
        ("elsewhere", entry("https://forge.invalid/org/elsewhere", "https://forge.invalid/acme/elsewhere")),
        ("absent", entry("https://forge.invalid/org/absent", "https://forge.invalid/acme/absent")),
    ]);

    let demo = bind::resolve(&registry, &RepoName::new("demo"), &lab.work, &home).expect("read").expect("resolved");
    assert_eq!(demo.checkout.path, lab.work.canonicalize().expect("canonical"));

    let other = bind::resolve(&registry, &RepoName::new("elsewhere"), &lab.work, &home).expect("read").expect("resolved");
    assert_eq!(other.checkout.path, elsewhere.canonicalize().expect("canonical"));

    let missing = bind::resolve(&registry, &RepoName::new("absent"), &lab.work, &home).expect("read").expect_err("missing");
    assert_eq!(missing, Unresolved::Missing { home: home.clone() });

    let unknown = bind::resolve(&registry, &RepoName::new("nope"), &lab.work, &home).expect("read").expect_err("unknown");
    assert_eq!(unknown, Unresolved::Unknown);
}
```

`lab::jj` and `Lab::temp_path` are the two lab additions from this task's Files block. `Lab::new()` creates `work` under `temp`; the scan test moves it under `home` and moves it back so the `Lab` drop is clean. `tempfile` directories are named `.tmpXXXX`: a dot-prefixed name is skipped as a *child*, never as the scan root itself, so `HOME = lab.temp_path()` is scanned and finds `work` at depth 1 (`second` has no `upstream` remote and is ignored).

- [ ] **Step 6: Run the integration tests to verify they fail**

Run: `cargo test --test registry_binding 2>&1 | tail -20`
Expected: compile errors for the missing functions.

- [ ] **Step 7: Implement `nearest_root`, `checkout_root`, `remotes`, `entry_for`, `here`, `scan`, `resolve`, `Checkout::is_jj`**

One private ancestor walker, two public entry points:

```rust
fn first_repository_root(path: &Path) -> Option<PathBuf> {
    let start = path.canonicalize().ok()?;
    start
        .ancestors()
        .find(|directory| directory.join(".jj").is_dir() || directory.join(".git").exists())
        .map(Path::to_path_buf)
}

pub fn nearest_root(path: &Path) -> Option<PathBuf> {
    first_repository_root(path)
}

pub fn checkout_root(path: &Path) -> Option<PathBuf> {
    let root = first_repository_root(path)?;
    let pointer = root.join(".jj").join("repo");
    if !pointer.is_file() {
        return Some(root);
    }
    // `<checkout>/.jj/repo` → the checkout is two levels up from the store the
    // pointer names. Unreadable: keep the workspace so jj's error surfaces later.
    let Ok(text) = std::fs::read_to_string(&pointer) else { return Some(root) };
    let store = PathBuf::from(text.trim());
    let store = if store.is_absolute() { store } else { pointer.parent().map_or(store.clone(), |p| p.join(&store)) };
    store
        .parent()
        .and_then(Path::parent)
        .map(|checkout| checkout.canonicalize().unwrap_or_else(|_| checkout.to_owned()))
        .or(Some(root))
}

impl Checkout {
    pub fn is_jj(&self) -> bool {
        self.path.join(".jj").join("repo").is_dir()
    }
}
```

The contract: "the first ancestor that is a repository root, of either kind". A git clone nested inside a jj checkout is its own root; a colocated checkout has both markers at one directory. The hook's former `repo_root_above` is this rule.

```rust
pub fn remotes(root: &Path) -> Result<BTreeMap<String, String>, BindError> {
    let output = if root.join(".jj").is_dir() {
        std::process::Command::new("jj")
            .arg("-R").arg(root)
            .args(["--ignore-working-copy", "git", "remote", "list"])
            .output()
    } else if root.join(".git").exists() {
        std::process::Command::new("git")
            .arg("-C").arg(root)
            .args(["config", "--get-regexp", "^remote\\..*\\.url$"])
            .output()
    } else {
        return Err(BindError::NotARepository { root: root.to_owned() });
    }
    .map_err(|error| BindError::Remotes { root: root.to_owned(), detail: error.to_string() })?;
    // git exits 1 with empty output when nothing matches: an empty map, not an error.
    let no_matches = output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty();
    if !output.status.success() && !no_matches {
        return Err(BindError::Remotes {
            root: root.to_owned(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (key, url) = line.split_once(' ').ok_or_else(|| BindError::Remotes {
                root: root.to_owned(),
                detail: format!("unparseable remote line {line:?}"),
            })?;
            // jj prints `name url`; git prints `remote.name.url url`.
            let name = key.strip_prefix("remote.").and_then(|v| v.strip_suffix(".url")).unwrap_or(key);
            Ok((name.to_owned(), url.trim().to_owned()))
        })
        .collect()
}

pub fn entry_for<'a>(registry: &'a Registry, upstream: &str) -> Option<(RepoName, &'a RepoEntry)> {
    registry
        .repos
        .iter()
        .find(|(_, entry)| same_remote(&entry.upstream, upstream))
        .map(|(name, entry)| (RepoName::new(name.clone()), entry))
}

pub fn here<'a>(registry: &'a Registry, cwd: &Path) -> Result<Result<Fork<'a>, Unbound>, BindError> {
    let Some(root) = checkout_root(cwd) else {
        return Ok(Err(Unbound::NotInsideARepository));
    };
    let remotes = remotes(&root)?;
    let Some(upstream) = remotes.get("upstream") else {
        return Ok(Err(Unbound::NoUpstream { root }));
    };
    let Some((name, entry)) = entry_for(registry, upstream) else {
        return Ok(Err(Unbound::Unregistered { root, upstream: upstream.clone() }));
    };
    Ok(Ok(Fork { name, entry, checkout: Checkout { path: root, remotes } }))
}

const SCAN_DEPTH: usize = 3;

pub fn scan<'a>(registry: &'a Registry, home: &Path) -> Scan<'a> {
    let mut scan = Scan::default();
    // Keyed by name, carrying the entry, so no lookup can fail later (the crate denies `expect`).
    let mut candidates: BTreeMap<RepoName, (&'a RepoEntry, Vec<Checkout>)> = BTreeMap::new();
    let mut pending = vec![(home.to_owned(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        let jj = directory.join(".jj");
        if jj.is_dir() {
            if jj.join("repo").is_dir() {
                match remotes(&directory) {
                    Ok(remotes) => {
                        if let Some(upstream) = remotes.get("upstream")
                            && let Some((name, entry)) = entry_for(registry, upstream)
                        {
                            let path = directory.canonicalize().unwrap_or_else(|_| directory.clone());
                            candidates.entry(name).or_insert((entry, Vec::new())).1.push(Checkout { path, remotes });
                        }
                    }
                    Err(error) => scan.problems.push(error.to_string()),
                }
            }
            continue; // a checkout or a workspace: never descend
        }
        if directory.join(".git").exists() || depth == SCAN_DEPTH {
            continue; // git-only is not a fork checkout; depth 3 is the last level read
        }
        let Ok(entries) = std::fs::read_dir(&directory) else { continue };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else { continue };
            if !file_type.is_dir() { continue } // symlinks are not followed
            if entry.file_name().to_string_lossy().starts_with('.') { continue }
            pending.push((entry.path(), depth + 1));
        }
    }
    for (name, (entry, mut checkouts)) in candidates {
        checkouts.sort_by(|a, b| a.path.cmp(&b.path));
        match <[Checkout; 1]>::try_from(checkouts) {
            Ok([checkout]) => {
                scan.found.insert(name.clone(), Fork { name, entry, checkout });
            }
            Err(many) => {
                scan.duplicates.insert(name, many.into_iter().map(|checkout| checkout.path).collect());
            }
        }
    }
    scan
}

pub fn resolve<'a>(registry: &'a Registry, name: &RepoName, cwd: &Path, home: &Path)
    -> Result<Result<Fork<'a>, Unresolved>, BindError>
{
    if registry.get(name).is_none() {
        return Ok(Err(Unresolved::Unknown));
    }
    if let Ok(fork) = here(registry, cwd)? && fork.name == *name {
        return Ok(Ok(fork));
    }
    let mut scan = scan(registry, home);
    if let Some(fork) = scan.found.remove(name) {
        return Ok(Ok(fork));
    }
    if let Some(paths) = scan.duplicates.remove(name) {
        return Ok(Err(Unresolved::Duplicate { home: home.to_owned(), paths }));
    }
    Ok(Err(Unresolved::Missing { home: home.to_owned() }))
}
```

`home` itself is depth 0; `SCAN_DEPTH == 3` means `~/a/b/c` is read and its children are not queued. The test's `a/b/c/d` checkout (depth 4) must be missed. Add `pub mod bind;` to `src/lib.rs` with its crate-doc bullet; make `config::home_dir` `pub`; implement the two `message` renderers with the exact texts from the Interfaces block.

- [ ] **Step 8: Run all bind tests, clippy, fmt, identity scan**

Run: `cargo test --test registry_binding 2>&1 | tail -20 && cargo test bind::tests 2>&1 | tail -5 && cargo clippy --all-targets 2>&1 | tail -5 && cargo fmt --check && cargo test --test no_hardcoded_identity 2>&1 | tail -3`
Expected: 9 integration + 9 unit tests pass; clippy and fmt clean; identity scan 2 passed.

- [ ] **Step 9: Commit**

```bash
unset JJ_USER JJ_EMAIL
jj describe -m "feat(bind): bind a checkout to its registry entry by its upstream remote

A new module answers which entry a directory is: walk up to the checkout
(through a workspace's .jj/repo pointer), read its remotes from jj or git,
match upstream against the registry after normalising URL spellings. A scan
of \$HOME to depth three finds every entry's checkout for the verbs that run
outside one, refusing to choose when an entry has two. origin and release
are compared and reported as notes, never used for binding."
```

**Verification:** an implementer knows this unit is done when `tests/registry_binding.rs` shows a lab checkout, a workspace beside it, a nested git clone, a git-only clone, a dot-directory, a depth-4 directory, and a duplicate pair each classified the way the spec says; every refusal and note renders its exact text; and — driven outside the suite — a one-off `#[ignore]` test (deleted before commit) reports `bind::remotes` for this very checkout identical to `jj git remote list`.

---

### Task 2: Trust is its own list; the hook matches managed and trusted independently

**Depends on:** Task 1.

**Files:**
- Modify: `src/config.rs` — `TrustRules` gains `repos` and `grants`; delete `TrustedEntry` (281-283, 330-336), `Registry::trusted` (342-346), `Registry::guidance_roots` (362-382), `GuidanceRootKind` (310-320); `load` validates `trust.repos` with `is_forge_slug`, rejects `[trusted.*]` with the named message, and stops resolving `trusted` paths (596-598); `#[serde(deny_unknown_fields)]` on `TrustRules` and `Registry` (not yet on `RepoEntry` — `path` still exists until Task 3)
- Modify: `src/hook/resolve.rs` — `Match` reshaped; `managed_repo_for`, `trust_rule_match`, `repo_root_above` replaced by `match_checkout`; URL helpers now imported from `bind`
- Modify: `src/hook/state.rs` — `owner_remotes: HashMap<PathBuf, Vec<String>>` becomes `remotes: HashMap<PathBuf, BTreeMap<String, String>>`; `owner_remotes()`/`record_owner_remotes()` become `remotes()`/`record_remotes()`
- Modify: `src/commands/hook.rs` — `match_with_trust` (429-502), `owner_for` (235-262), every `matched.repo.kind == GuidanceRootKind::Managed` (159, 176, 246, 310, 349, 365), `contains_cwd` (511)
- Modify: `src/hook/guidance.rs` — `GuidanceRoot` loses `kind`; fix the test constructing it (~290)
- Modify: `src/commands/repos.rs` — delete `TrustedRow`, `Report.trusted`, `trusted_lines` (350-357, 410-414, 442), and the `TrustedEntry` test (996-1006)
- Modify: `src/main.rs` tests (826-841, 856-871) and `src/commands/claim.rs` test fixture (~300) — test-only: `Registry` literals lose `trusted`; the workspace-derived identity fixture becomes a git repository with a matching `upstream` since `owner_for` now binds by remote
- Modify: `src/commands/hook_regression_tests.rs`, `src/hook/state_regression_tests.rs`, `tests/hook_claude_code.rs`, `tests/hook_opencode.rs`, `tests/hook_guidance.rs`, `plugin/knives.test.ts` — fixtures become real repositories with remotes; `[trusted.*]` becomes `[trust]`

**Interfaces:**
- Consumes: `bind::{checkout_root, nearest_root, remotes, entry_for, same_remote, here, remote_slug, url_owner}`.
- Produces:

```rust
// src/config.rs
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustRules {
    /// Repositories trusted for guidance by identity: `owner/repo`, matched
    /// against any remote of a checkout, case-insensitively, `.git` stripped
    /// from both sides.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owners: Vec<String>,
}
impl TrustRules {
    pub const fn is_empty(&self) -> bool;  // all three empty
    /// Whether these rules trust a checkout at `root` declaring `remotes`.
    pub fn grants(&self, root: &Path, remotes: &BTreeMap<String, String>) -> bool;
}

// src/hook/resolve.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The nearest repository root containing the touched path (a jj workspace
    /// is its own root). Guidance walks this tree; session-state keys use it.
    pub root: PathBuf,
    /// The touched path, canonicalised.
    pub candidate: PathBuf,
    /// The registry name when the *checkout's* `upstream` matches an entry.
    pub managed: Option<RepoName>,
    /// Whether `[trust]` grants guidance for this checkout (any remote, or `roots`).
    pub trusted: bool,
}
impl Match {
    /// The registry name when managed, else `guidance_name(&self.root)`.
    pub fn name(&self) -> String;
    pub fn is_managed(&self) -> bool;
}
/// The first touched path inside a repository, with both facts decided from the
/// remotes of the checkout `bind::checkout_root` resolves to. `remotes_of` is the
/// (cached) reader, keyed by that checkout path; it returns `None` when remotes
/// cannot be read — the caller has already reported why.
pub fn match_checkout(
    paths: &[PathBuf],
    registry: &Registry,
    remotes_of: &mut dyn FnMut(&Path) -> Option<BTreeMap<String, String>>,
) -> Option<Match>;
```

`GuidanceRoot` shrinks to `{ name: String, root: PathBuf }`; `guidance_for`, `notice_if_requested`, `notice_digest`, `format_guidance` keep their signatures with that smaller type, built from a `Match` via `GuidanceRoot { name: matched.name(), root: matched.root.clone() }`.

- [ ] **Step 1: Write the failing config tests for `[trust] repos` and the deleted `[trusted]` table**

In `src/config.rs` tests module, add:

```rust
#[test]
fn trust_repos_are_forge_slugs_and_grant_by_any_remote() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repos.toml");
    std::fs::write(&path, "[trust]\nrepos = [\"Company/Tool\", \"company/other.git\"]\nowners = [\"someone\"]\n").unwrap();
    let registry = load(&path).unwrap();
    let by_repo = std::collections::BTreeMap::from([("origin".to_owned(), "git@forge.example:company/tool.git".to_owned())]);
    assert!(registry.trust.grants(Path::new("/anywhere"), &by_repo));
    let by_repo_with_git_suffix_configured = std::collections::BTreeMap::from([("origin".to_owned(), "https://forge.example/company/other".to_owned())]);
    assert!(registry.trust.grants(Path::new("/anywhere"), &by_repo_with_git_suffix_configured));
    let other = std::collections::BTreeMap::from([("origin".to_owned(), "https://forge.example/company/third".to_owned())]);
    assert!(!registry.trust.grants(Path::new("/anywhere"), &other));
    let by_owner = std::collections::BTreeMap::from([("upstream".to_owned(), "https://forge.example/someone/anything".to_owned())]);
    assert!(registry.trust.grants(Path::new("/anywhere"), &by_owner));
}

#[test]
fn a_trust_repo_that_is_not_a_slug_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repos.toml");
    std::fs::write(&path, "[trust]\nrepos = [\"~/somewhere\"]\n").unwrap();
    let error = load(&path).unwrap_err().to_string();
    assert!(error.contains("[trust] repos takes forge slugs (\"<owner>/<repo>\"); found \"~/somewhere\""), "{error}");
}

#[test]
fn a_trusted_table_names_its_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repos.toml");
    std::fs::write(&path, "[trusted.work]\npath = \"~/work\"\n").unwrap();
    let error = load(&path).unwrap_err().to_string();
    assert!(
        error.contains("[trusted.work] is no longer a registry table; move it to [trust] repos = [\"<owner>/<repo>\"]"),
        "{error}"
    );
}
```

Delete every existing test that constructs `TrustedEntry` or writes `[trusted.` (grep `config.rs` for both) — the table no longer exists.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test config::tests::trust_repos config::tests::a_trust_repo config::tests::a_trusted_table 2>&1 | tail -15`
Expected: compile errors (`grants` missing, `repos` field missing) or assertion failures.

- [ ] **Step 3: Implement the config side**

- Add `repos` to `TrustRules` as in Interfaces; `is_empty` covers all three.
- `grants`: `roots` containment exactly as `trust_rule_match` does today (`resolve.rs:95-113`, canonicalise each root, `strip_prefix`); `owners` via `bind::url_owner(url)` per remote, `eq_ignore_ascii_case`; `repos`: for each remote URL, `bind::remote_slug(url)` `eq_ignore_ascii_case` the configured slug with a trailing `.git` stripped (`slug.strip_suffix(".git").unwrap_or(slug)`). Any rule true → true.
- In `load`: before `toml::from_str`, parse as `toml::Table`; if it has a `trusted` table, return `ConfigError::Invalid` with the exact message for its first key (sorted). Validate `trust.repos` with `is_forge_slug` → the exact message.
- Delete `TrustedEntry`, `Registry::trusted`, `guidance_roots`, `GuidanceRootKind`, the `trusted` path resolution in `load`. `#[serde(deny_unknown_fields)]` on `TrustRules` and `Registry`. Rewrite the `TrustRules::owners` doc (293-299: "jj-only checkouts match only through roots" is no longer true) and the `Registry::trust` doc (347-350: no `save`, no `init`).

- [ ] **Step 4: Run the config tests**

Run: `cargo test config::tests 2>&1 | tail -10`
Expected: all pass (the three new ones included).

- [ ] **Step 5: Rewrite the hook fixtures as real repositories and write the failing hook tests**

In `tests/hook_claude_code.rs`, `Repositories::new` (77-98) makes bare directories; `configure` (100-125) writes a path registry. Replace both:

```rust
fn git_repository(root: &Path, remotes: &[(&str, &str)]) {
    std::fs::create_dir_all(root).expect("create repository");
    assert!(std::process::Command::new("git")
        .args(["-C", root.to_str().expect("utf-8"), "init", "--quiet"])
        .status().expect("git init").success());
    for (name, url) in remotes {
        assert!(std::process::Command::new("git")
            .args(["-C", root.to_str().expect("utf-8"), "remote", "add", name, url])
            .status().expect("git remote add").success());
    }
}

impl Repositories {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("config home");
        let alpha = home.path().join("alpha");   // managed AND trusted (origin under a trusted owner)
        let beta = home.path().join("beta");     // managed, NOT trusted
        let trusted = home.path().join("trusted"); // trusted only, by [trust] repos
        git_repository(&alpha, &[
            ("upstream", "https://forge.invalid/maintainer/alpha"),
            ("origin", "https://forge.invalid/ours/alpha"),
        ]);
        git_repository(&beta, &[
            ("upstream", "https://forge.invalid/maintainer/beta"),
            ("origin", "https://forge.invalid/stranger/beta"),
        ]);
        git_repository(&trusted, &[("origin", "https://forge.invalid/company/trusted.git")]);
        for (root, instructions) in [(&alpha, "alpha instructions"), (&beta, "beta instructions"), (&trusted, "trusted instructions")] {
            std::fs::write(root.join("AGENTS.md"), instructions).expect("write instructions");
            std::fs::write(root.join("file.txt"), "content").expect("write file");
        }
        Self { home, alpha, beta, trusted }
    }

    fn configure(&self, include_trusted: bool) {
        let trust = if include_trusted {
            "[trust]\nowners = [\"ours\"]\nrepos = [\"company/trusted\"]\n"
        } else {
            "[trust]\nowners = [\"ours\"]\n"
        };
        // `path` is still a required field until Task 3 deletes it; the hook
        // code written in this task never reads it.
        let config = format!(
            "[repos.alpha]\npath = \"{}\"\nupstream = \"https://forge.invalid/maintainer/alpha\"\norigin = \"https://forge.invalid/ours/alpha\"\n\n\
             [repos.beta]\npath = \"{}\"\nupstream = \"https://forge.invalid/maintainer/beta\"\norigin = \"https://forge.invalid/ours/beta\"\n\n{trust}",
            self.alpha.display(),
            self.beta.display(),
        );
        std::fs::write(self.home.path().join("repos.toml"), config).expect("write registry");
        let state = json!({"claims": {"beta/feat/claimed": {
            "repo": "beta",
            "branch": "feat/claimed",
            "owner": "agent-one",
            "why": "porting",
            "started": "2026-01-01T00:00:00Z",
            "files": []
        }}});
        std::fs::write(self.home.path().join("state.json"), state.to_string()).expect("write state");
    }
}
```

Add these tests to `tests/hook_claude_code.rs` (`post_tool_use_read`/`additional_context` are the file's existing helpers; read lines 133-200 for the constructor names and reuse them):

```rust
#[test]
fn a_managed_checkout_outside_trust_gets_the_notice_but_no_guidance() {
    let repositories = Repositories::new();
    repositories.configure(true);
    let event = post_tool_use_read(&repositories.beta.join("file.txt"), "session-beta");
    let output = run_hook(repositories.home.path(), &event);
    let context = additional_context(&output);
    assert!(context.contains("managed"), "{context}");
    assert!(!context.contains("beta instructions"), "{context}");
}

#[test]
fn a_trusted_checkout_that_is_not_a_fork_gets_guidance_but_no_notice() {
    let repositories = Repositories::new();
    repositories.configure(true);
    let event = post_tool_use_read(&repositories.trusted.join("file.txt"), "session-trusted");
    let output = run_hook(repositories.home.path(), &event);
    let context = additional_context(&output);
    assert!(context.contains("trusted instructions"), "{context}");
    assert!(!context.contains("managed"), "{context}");
}

#[test]
fn a_checkout_that_is_both_managed_and_trusted_gets_notice_and_guidance() {
    let repositories = Repositories::new();
    repositories.configure(true);
    let event = post_tool_use_read(&repositories.alpha.join("file.txt"), "session-alpha");
    let output = run_hook(repositories.home.path(), &event);
    let context = additional_context(&output);
    assert!(context.contains("managed"), "{context}");
    assert!(context.contains("alpha instructions"), "{context}");
}

#[test]
fn a_plain_git_clone_under_a_trusted_owner_gets_guidance_wherever_it_is() {
    let repositories = Repositories::new();
    repositories.configure(true);
    let clone = repositories.home.path().join("scratch").join("tmp-clone");
    git_repository(&clone, &[("origin", "https://forge.invalid/ours/anything")]);
    std::fs::write(clone.join("AGENTS.md"), "clone instructions").expect("write");
    std::fs::write(clone.join("file.txt"), "content").expect("write");
    let event = post_tool_use_read(&clone.join("file.txt"), "session-clone");
    let output = run_hook(repositories.home.path(), &event);
    assert!(additional_context(&output).contains("clone instructions"));
}

/// A jj workspace of a trusted repository: guidance comes from the workspace's own tree.
#[test]
fn a_workspace_of_a_trusted_repository_gets_its_own_guidance() {
    let repositories = Repositories::new();
    repositories.configure(true);
    let checkout = repositories.home.path().join("tool");
    jj_checkout(&checkout, &[("origin", "https://forge.invalid/company/trusted")]);
    std::fs::write(checkout.join("AGENTS.md"), "trusted instructions").expect("write");
    jj_in(&checkout, &["describe", "-m", "init"]);
    jj_in(&checkout, &["new"]);
    let workspace = repositories.home.path().join("tool-feat");
    jj_in(&checkout, &["workspace", "add", "--name", "feat", workspace.to_str().expect("utf-8")]);
    std::fs::write(workspace.join("file.txt"), "content").expect("write");
    let event = post_tool_use_read(&workspace.join("file.txt"), "session-ws");
    let output = run_hook(repositories.home.path(), &event);
    assert!(additional_context(&output).contains("trusted instructions"), "{output}");
}

/// Remotes that cannot be read are reported, and a `roots` rule still grants guidance.
#[test]
fn unreadable_remotes_are_reported_and_a_trust_root_still_grants_guidance() {
    let repositories = Repositories::new();
    let fake = repositories.home.path().join("under-root").join("fake");
    std::fs::create_dir_all(fake.join(".jj")).expect("bare .jj without a repo");
    std::fs::write(fake.join("AGENTS.md"), "root instructions").expect("write");
    std::fs::write(fake.join("file.txt"), "content").expect("write");
    std::fs::write(
        repositories.home.path().join("repos.toml"),
        format!("[trust]\nroots = [\"{}\"]\n", repositories.home.path().join("under-root").display()),
    ).expect("registry");
    let event = post_tool_use_read(&fake.join("file.txt"), "session-fake");
    let (success, output, errors) = run_hook_input(repositories.home.path(), &event.to_string());
    assert!(success);
    assert!(errors.contains("knives hook:"), "{errors}");
    assert!(additional_context(&output).contains("root instructions"), "{output}");
}
```

`jj_checkout`/`jj_in` are the same small helpers as `tests/registry_binding.rs` uses (colocated `jj git init`, `JJ_CONFIG=/dev/null`, lab user/email); copy them into this file. Then re-read every existing test in `hook_claude_code.rs` and `hook_opencode.rs`: any assertion that `beta` receives guidance moves to `alpha` or becomes the managed-not-trusted assertion. Apply the same fixture change to `tests/hook_opencode.rs:100-125`, `src/commands/hook_regression_tests.rs`, and `plugin/knives.test.ts` (`repository()` at 34-45: `git init` + `git remote add origin https://forge.invalid/ours/managed` via `Bun.spawnSync`; registry `[repos.managed]\npath = "${root}"\nupstream = "https://forge.invalid/maintainer/managed"\norigin = "https://forge.invalid/ours/managed"\n\n[trust]\nowners = ["ours"]\n`; the rewrite at 751-754: `[repos.gone]` keeps its `path` line until Task 3 but needs a distinct `upstream`; `[trusted.work]` → `[trust] repos = ["company/work"]` with the `trusted` dir made a git repository whose `origin` is `https://forge.invalid/company/work`).

- [ ] **Step 6: Run the hook tests to verify they fail**

Run: `cargo test --test hook_claude_code 2>&1 | tail -20`
Expected: the six new tests fail (beta still receives guidance; the clone receives none; …); others may fail on the fixture change.

- [ ] **Step 7: Implement the hook side**

- `src/hook/resolve.rs`: replace `managed_repo_for`, `trust_rule_match`, `repo_root_above` with `match_checkout` per Interfaces: for each path, `canonical_path`; `root = bind::nearest_root(&candidate)?`; `checkout = bind::checkout_root(&candidate)?`; `remotes = remotes_of(&checkout)` (None ⇒ no remote facts; `roots` still applies via `grants(root, &BTreeMap::new())`); `managed = remotes.get("upstream").and_then(|u| bind::entry_for(registry, u)).map(|(name, _)| name)`; `trusted = registry.trust.grants(&root, &remotes)`; return the first path whose root is managed or trusted. Keep `guidance_name`, `argument_paths`, `canonical_path`.
- `src/hook/state.rs`: the cache becomes `remotes: HashMap<PathBuf, BTreeMap<String, String>>` with `remotes(&self, root)`/`record_remotes(&mut self, root, remotes)`. Update `state_regression_tests.rs`.
- `src/commands/hook.rs`: `match_with_trust` builds the `remotes_of` closure: cache hit → clone; miss → `match bind::remotes(root) { Ok(r) => record + Some(r), Err(error) => { eprintln!("knives hook: {error}"); None } }`. The two 'requires a colocated .git checkout' stderr lines (452, 468) are deleted. Replace each `matched.repo.kind == GuidanceRootKind::Managed` with `matched.is_managed()`; gate `guidance_for` on `matched.trusted`; build `GuidanceRoot { name: matched.name(), root: matched.root.clone() }` where the old code passed `&matched.repo`. `owner_for`: `let Ok(Ok(fork)) = bind::here(&registry, cwd) else { return Ok(None) };` then `fork.name` for the claim filter — a `BindError` falls through to no owner, never fails the command. `contains_cwd` compares `bind::nearest_root(cwd) == Some(root)`.
- `src/hook/guidance.rs`: remove `kind` from `GuidanceRoot`; fix the test at ~290.
- `src/commands/repos.rs`: delete `TrustedRow`, `Report.trusted`, `trusted_lines`, the `trusted`-only render branch (410-414), the `TrustedEntry` test.
- `src/main.rs` and `src/commands/claim.rs` test fixtures per Files. Add a unit test in `claim.rs`: cwd inside a directory with a bare `.jj/` → identity resolves to the OS user without error.

- [ ] **Step 8: Run the hook, plugin, and whole suites**

Run: `cargo test --test hook_claude_code --test hook_opencode --test hook_guidance 2>&1 | tail -15 && env -u JJ_EMAIL -u JJ_USER cargo test 2>&1 | tail -5 && cargo build && cd plugin && KNIVES_BIN=$PWD/../target/debug/knives bun test 2>&1 | tail -5 && cd ..`
Expected: all green; the bun run reports the real-binary tests as passing, not skipped.

- [ ] **Step 9: Clippy, fmt, commit**

Run: `cargo clippy --all-targets 2>&1 | tail -5 && cargo fmt --check`

```bash
unset JJ_USER JJ_EMAIL
jj describe -m "feat(trust): guidance follows the repository, not the fork entry

[trust] gains repos = [\"owner/repo\"] beside owners and roots; [trusted.*]
by path is gone and names its replacement when found. The hook resolves a
touched file to its nearest repository root, reads the checkout's remotes
once per session, and decides two facts independently: managed (upstream
matches a fork entry: notice, claims, owner) and trusted ([trust] matches
any remote or the root: guidance). A fork entry no longer grants guidance;
an agent's /tmp clone of a trusted repository does, and so does a jj
workspace of one."
```

**Verification:** an implementer knows this unit is done when, with `target/debug/knives hook claude-code` fed a PostToolUse event, a file inside a plain `git clone` whose `origin` owner is in `[trust] owners` yields its `AGENTS.md` as guidance; a file inside a managed fork whose remotes match no trust rule yields the managed notice and no guidance; a file inside a jj workspace of a trusted repository yields the workspace's own `AGENTS.md`; and a `[trusted.x]` table in the registry produces the exact replacement message on stderr with exit 0.

---

### Task 3: `path` leaves the registry; every verb works on a `Fork`

**Depends on:** Tasks 1 and 2.

**Files:**
- Modify: `src/config.rs` — delete `RepoEntry.path` (115), `resolved_path` (164-166), `workspace_root` (170-174; now on `Fork`), `Registry::containing`/`containing_direct` (390-409), `workspace_checkout` (416-436), `save` (654-666), `ConfigError::Write`/`Serialise` if unused; `checked_workspaces` (611-639) keeps only the empty check; `load` adds the `path` rejection and the duplicate-`upstream` rejection; `#[serde(deny_unknown_fields)]` on `RepoEntry`; rewrite the `workspaces` doc (143-150, 'Resolved like `path`'); the `test_support` module and every test constructing `path:`
- Delete: `src/commands/init.rs`; remove `pub mod init;` from `src/commands.rs:13`; remove `Command::Init` from `src/cli.rs:185-189` and `init::run` from `src/main.rs:70`
- Modify: `src/commands/register.rs` — absorbs `decide`/`MissingRoles`/warnings from `init.rs`; binds through `bind::checkout_root` so it works from any directory inside a checkout; new `Outcome::AlreadyRegistered { name }`
- Modify: `src/main.rs` — `one_repo` → `one_fork`; `selected` → `Vec<Selected>` via `bind::scan`; `sync_targets` likewise; `scribe_for` takes `&Fork`; delete the two `sync_targets_*` unit tests (804-878; coverage moves to `tests/registry_binding.rs`)
- Modify: every `entry.path` reader → `fork.checkout.path`: `src/branch_verbs.rs` (73-76, 116-119, 226, 261-264), `src/carriage.rs` (146, 194, 247), `src/release_cut.rs` (594-595, 608-609, and its registry lookups), `src/release_edit.rs`, `src/release_rebase.rs`, `src/release_carries.rs`, `src/commands/{audit,consumers,gh,notch,pr,preflight,pushed,release,repos,start,status,sync,wip}.rs` (`audit.rs:148` calls `jj::git_remotes` → use `fork.checkout.remotes`; `wip.rs:72,84` read `entry.path`; `wip::gather` has no caller at `main` — delete it and its tests rather than migrate dead code), `src/commands/status/{dependencies,overlap,phases,releases}.rs`, `src/seen.rs` (143-146, and the `configured_workspace` fixture at ~232 that writes `path = …`), `src/jj.rs` (delete `git_toplevel` 1235-1239 and `git_remotes` 1244-1284)
- Modify: `src/commands/status/dependencies.rs` — sibling identity from `bind::remote_slug(&entry.upstream)`, `gh` run in the current fork's checkout, the non-forge problem string
- Modify: `src/commands/start.rs` — `workspace_path(fork, branch)`; add the `workspaces`-inside-checkout refusal to the collision check (66-83) with the message from `config.rs:631-635`; `finish` (`branch_verbs.rs`) calls the same check
- Modify: `src/commands/status.rs` — `gather` appends `fork.remote_notes()` to `report.notes`; `src/commands/repos.rs` — `RepoRow.path: Option<String>`, rows built from a `bind::scan`, `notes` extended with `remote_notes` and `scan.problems`, render `not on this machine` for `None`
- Modify: `tests/common/lab.rs` — registry strings drop `path` (530, and `lab_entry` 490-500 loses `path:`); `release_command` (601-620) and `start_command` (624-631) add `.env("HOME", lab.temp_path())`; every per-test binary builder in `tests/*.rs` that sets `KNIVES_CONFIG_HOME` (grep `CARGO_BIN_EXE_knives`: `branch_finish.rs`, `notch_command.rs`, `registry_readoption.rs`, `status_report.rs`, `gh_command.rs`, `hook_claude_code.rs`, `hook_opencode.rs`, `remote_audit.rs`, `forge_consumers.rs`, …) adds the same `HOME` line
- Modify: every test writing `path = ` (grep list: `branch_finish.rs` 446, 549-550; `claim_lifecycle.rs` 910; `forge_consumers.rs` 174, 228, 273, 314; `gh_command.rs` 485; `hook_claude_code.rs`, `hook_opencode.rs`, `plugin/knives.test.ts` (both `registry()` and `[repos.gone]`); `notch_command.rs` 20; `registry_readoption.rs`; `release_advance.rs` 377; `release_cut.rs` 544, 654; `release_edit.rs` 384; `release_rebase.rs` 95; `remote_audit.rs` 26; `status_report.rs` 52-53; `workspace_placement.rs` 28, 305, 424) — delete the `path` line; every checkout a test binds must be a jj checkout with an `upstream` remote matching its entry (`notch_command.rs` and `remote_audit.rs` fixtures need `jj git remote add upstream`; `gh_command.rs`'s checkout adds an `upstream` remote with the https spelling of the registry's `git@` URL)
- Rewrite: `tests/registry_readoption.rs` → `register` behaviour; delete `tests/branch_finish.rs::finish_by_owner_releases_when_checkout_activity_is_unavailable` (435-470; its premise — a registry path that does not exist — has no expression in the new model)
- Test: `tests/registry_binding.rs` gains the CLI-level tests below; `src/config.rs` gains the rejection tests

**Interfaces:**
- Consumes: everything Task 1 produces; `Match`/`TrustRules` from Task 2.
- Produces:

```rust
// src/commands/register.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    NotARepository { path: PathBuf },
    NotAJjCheckout { path: PathBuf },
    MissingRoles { path: PathBuf, found: Vec<String>, absent: Vec<String> },
    AlreadyRegistered { name: RepoName },
    Snippet { name: String, entry: RepoEntry, warnings: Vec<String> },
}
/// `path` may be any directory inside the checkout: `bind::checkout_root` finds the root.
pub fn outcome_for(path: &Path, registry: &Registry) -> anyhow::Result<Outcome>;
pub fn run(target: Option<PathBuf>) -> anyhow::Result<Exit>;

// src/commands/repos.rs
pub struct RepoRow { pub name: String, pub path: Option<String>, /* rest unchanged */ }

// src/main.rs (private)
/// One registry entry as a many-repo verb sees it after the scan.
enum Selected<'a> {
    Bound(Fork<'a>),
    /// Not found, or found twice: still a row, never opened. `scan_problems`
    /// carries the scan's own complaints so an unreadable checkout is named.
    Unplaced { name: RepoName, entry: &'a RepoEntry, why: knives::bind::Unresolved, scan_problems: Vec<String> },
}
/// The single fork a verb acts on; `None` after printing why not (exit `Usage`).
fn one_fork<'a>(registry: &'a Registry, requested: Option<&str>) -> anyhow::Result<Option<Fork<'a>>>;
/// Every entry a many-repo verb covers, bound where the scan could.
fn selected<'a>(registry: &'a Registry, requested: Option<&str>, all: bool) -> anyhow::Result<Result<Vec<Selected<'a>>, Exit>>;
```

Every verb entry point that today takes `entry: &RepoEntry` **and reads `entry.path`** takes `fork: &Fork<'_>` instead; it reads `fork.entry.<accessor>()` for remotes/policy and `fork.checkout.path` for the directory. Verbs that only read policy keep `&RepoEntry` (pass `fork.entry`).

- [ ] **Step 1: Write the failing config rejection tests**

In `src/config.rs` tests (hold `environment_lock()` and pin `HOME` in the third, as sibling tests do):

```rust
#[test]
fn a_path_field_names_its_removal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repos.toml");
    std::fs::write(
        &path,
        "[repos.tool]\npath = \"~/tool\"\nupstream = \"https://forge.example/org/tool\"\norigin = \"https://forge.example/ours/tool\"\n",
    ).unwrap();
    let error = load(&path).unwrap_err().to_string();
    assert!(
        error.contains("[repos.tool] path is no longer a registry field; delete it — knives finds checkouts by their remotes"),
        "{error}"
    );
}

#[test]
fn two_entries_sharing_an_upstream_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repos.toml");
    std::fs::write(
        &path,
        "[repos.a]\nupstream = \"https://forge.example/org/tool\"\norigin = \"https://forge.example/ours/tool\"\n\
         [repos.b]\nupstream = \"git@forge.example:org/tool.git\"\norigin = \"https://forge.example/theirs/tool\"\n",
    ).unwrap();
    let error = load(&path).unwrap_err().to_string();
    assert!(
        error.contains("[repos.a] and [repos.b] share upstream https://forge.example/org/tool; identity must be unique"),
        "{error}"
    );
}

#[test]
fn an_entry_without_path_loads_and_workspaces_is_a_preference() {
    let _lock = environment_lock();
    let _guard = EnvironmentGuard::capture(["HOME"]);
    let dir = tempfile::tempdir().unwrap();
    set("HOME", dir.path());
    let path = dir.path().join("repos.toml");
    std::fs::write(
        &path,
        "[repos.tool]\nupstream = \"https://forge.example/org/tool\"\norigin = \"https://forge.example/ours/tool\"\nworkspaces = \"~/.worktrees/tool\"\n",
    ).unwrap();
    let registry = load(&path).unwrap();
    let entry = &registry.repos["tool"];
    assert_eq!(entry.workspaces.as_deref(), Some(dir.path().join(".worktrees/tool").as_path()));
}
```

(`environment_lock`, `EnvironmentGuard`, `set` are the existing `test_support` helpers in `config.rs:14-82`; use their real names.)

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test config::tests::a_path_field config::tests::two_entries config::tests::an_entry_without_path 2>&1 | tail -15`
Expected: the first two fail (the file loads or fails with toml's generic message); the third fails to compile or errors on `path` missing.

- [ ] **Step 3: Implement the registry change and work the compiler's list**

In `src/config.rs`: delete `path`; add `#[serde(deny_unknown_fields)]`; delete the listed functions; `checked_workspaces(name, entry, home, path)` keeps the empty check and `expand_registry_path` only. In `load`, before deserialising: parse `toml::Table`; for each `repos.<name>` table with a `path` key → `Invalid` with the exact message (first offender, sorted by name). After deserialising: pairwise `bind::same_remote(a.upstream, b.upstream)` over sorted names → `Invalid` with the exact message, printing `a`'s `upstream` spelling. Update `test_support` and every test constructing `RepoEntry { path: … }`.

Then `cargo build 2>&1 | grep -c '^error'` and work the list module by module: `bind.rs` (drop the `path:` line from the Task 1 test helpers, unit and integration) → `main.rs` → `start.rs`/`branch_verbs.rs` → `release_*.rs` → `commands/*.rs` → `status/*.rs` → `seen.rs` → `gh.rs` (`registry.containing(cwd)` → `bind::here(&registry, cwd).ok().and_then(Result::ok).map(|f| f.entry)`; `crate::jj::git_remotes(cwd)` → `bind::checkout_root(cwd).and_then(|r| bind::remotes(&r).ok()).unwrap_or_default()`) → `jj.rs` deletions → `init.rs` deletion + `cli.rs` + `commands.rs` → `register.rs`.

`main.rs` shape:

```rust
fn known(registry: &Registry) -> String {
    registry.names().map(|n| n.to_string()).collect::<Vec<_>>().join(", ")
}

fn one_fork<'a>(registry: &'a Registry, requested: Option<&str>) -> anyhow::Result<Option<Fork<'a>>> {
    let cwd = std::env::current_dir()?;
    let fork = if let Some(name) = requested {
        let name = RepoName::new(name);
        match knives::bind::resolve(registry, &name, &cwd, &knives::config::home_dir())? {
            Ok(fork) => fork,
            Err(why @ knives::bind::Unresolved::Unknown) => {
                eprintln!("{}; known: {}", why.message(&name), known(registry));
                return Ok(None);
            }
            Err(why) => {
                eprintln!("{}", why.message(&name));
                return Ok(None);
            }
        }
    } else {
        match knives::bind::here(registry, &cwd)? {
            Ok(fork) => fork,
            Err(unbound) => {
                eprintln!("{}; known: {}", unbound.message(), known(registry));
                return Ok(None);
            }
        }
    };
    if !fork.checkout.is_jj() {
        eprintln!("{} is a git clone, not a jj checkout; fork commands need jj", fork.checkout.path.display());
        return Ok(None);
    }
    Ok(Some(fork))
}

fn selected<'a>(registry: &'a Registry, requested: Option<&str>, all: bool)
    -> anyhow::Result<Result<Vec<Selected<'a>>, Exit>>
{
    if registry.is_empty() {
        eprintln!("no repos configured; add entries to {}", default_config_path().display());
        return Ok(Err(Exit::Usage));
    }
    let cwd = std::env::current_dir()?;
    let home = knives::config::home_dir();
    if let Some(name) = requested {
        let name = RepoName::new(name);
        return Ok(match knives::bind::resolve(registry, &name, &cwd, &home)? {
            Ok(fork) => Ok(vec![Selected::Bound(fork)]),
            Err(why @ knives::bind::Unresolved::Unknown) => {
                eprintln!("{}; known: {}", why.message(&name), known(registry));
                Err(Exit::Usage)
            }
            Err(why) => {
                eprintln!("{}", why.message(&name));
                Err(Exit::Usage)
            }
        });
    }
    if !all && let Ok(fork) = knives::bind::here(registry, &cwd)? {
        return Ok(Ok(vec![Selected::Bound(fork)]));
    }
    let mut scan = knives::bind::scan(registry, &home);
    // Every entry is a row. One the scan did not find, or found twice, is still
    // a row — rendered as a problem, exactly as an unopenable path was before —
    // and nothing is opened for it.
    Ok(Ok(registry
        .repos
        .iter()
        .map(|(name, entry)| {
            let name = RepoName::new(name.clone());
            match scan.found.remove(&name) {
                Some(fork) => Selected::Bound(fork),
                None => Selected::Unplaced {
                    why: match scan.duplicates.remove(&name) {
                        Some(paths) => knives::bind::Unresolved::Duplicate { home: home.clone(), paths },
                        None => knives::bind::Unresolved::Missing { home: home.clone() },
                    },
                    scan_problems: scan.problems.clone(),
                    name,
                    entry,
                },
            }
        })
        .collect()))
}
```

`run_status`, `run_audit`, and `run_sync` turn `Selected::Unplaced` into today's problem row (`could not gather: <why.message(&name)>` followed by each scan problem; trunk from `entry.trunk()`) without opening anything; `sync_targets` keeps its own rule (name / `--all` / binding cwd / else `Usage`) on the same two `bind` calls. `knives repos` builds its rows from one `bind::scan` directly.

`status::gather` (and `gather_timed`) takes `fork: &Fork<'_>`; at the top: `report.notes.extend(fork.remote_notes())`. `repos::gather` builds rows from a `bind::scan` (found → `Some(path)`, notes extended; duplicates → `None` plus a problem naming the paths; missing → `None`; `scan.problems` appended to notes).

`register.rs`: `outcome_for(path, &registry)`: `bind::checkout_root(path)` none → `NotARepository`; root without `.jj/repo` dir → `NotAJjCheckout` (`<root> is a git clone, not a jj checkout; fork commands need jj`); `bind::remotes(&root)?`; upstream/origin missing → `MissingRoles`; `bind::entry_for(&registry, upstream)` some → `AlreadyRegistered { name }`; else `Snippet { name: guidance_name(&root), entry: RepoEntry { upstream, origin, release: remotes.get("release").cloned(), base: None, release_branch: None, test_count_command: None, consumers: vec![], workspaces: None }, warnings }` with the miswired-origin warning logic moved verbatim from `init.rs::decide`. `run` prints `already registered as <name>` (stdout, exit 0), the snippet (exit 0), or the refusal (exit `Usage`).

`dependencies.rs`: `upstream_identity(entry) -> Option<RepoIdentity>` from `bind::remote_slug(&entry.upstream)` with an empty `id`; `None` ⇒ push `<name>: upstream <url> is not a forge repository; cannot check dependencies against it` and skip the forge call; `pull_facts(&fork.checkout.path, &identity, &numbers)`.

`start.rs` collision check: after computing `directory = workspace_path(fork, branch)`, also refuse when `fork.workspace_root().starts_with(&fork.checkout.path)` with the exact text from `config.rs:631-635`; `finish` runs the same function before touching anything.

- [ ] **Step 4: Fix the fixtures and write the CLI-level binding tests**

Delete every `path = "…"` line from the test registries listed in Files. Add the `HOME` line to every binary builder named in Files. Both new test files define the same local helper (as `tests/registry_readoption.rs:42-48` does today):

```rust
fn knives(lab: &lab::Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(args)
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .output()
        .expect("run knives")
}
```

`tests/status_report.rs:40-110` builds two labs and runs `status --all` from a third directory; two temp roots cannot share one `HOME`, so that test renames `second.work` to `first.temp_path().join("zebra")` before writing its two-repo registry (the checkout's remotes are absolute paths to its own bare repositories and survive the move) and points `HOME` at `first.temp_path()`. Rewrite `tests/registry_readoption.rs` as:

```rust
#[test]
fn register_prints_a_snippet_without_path_for_an_unregistered_checkout() {
    let lab = lab::Lab::new();
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(home.path().join("repos.toml"), "").expect("empty registry");
    let output = knives(&lab, &home, &["--text", "register"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(stdout.contains("[repos.work]"), "{stdout}");
    assert!(stdout.contains(&format!("upstream = \"{}\"", lab.upstream.display())), "{stdout}");
    assert!(!stdout.contains("path ="), "{stdout}");
}

#[test]
fn register_names_the_entry_a_registered_checkout_already_is_from_any_subdirectory() {
    let lab = lab::Lab::new();
    let (home, _consumer) = lab::release_test_home(&lab); // writes [repos.demo] with the lab upstream
    let nested = lab.work.join("src");
    std::fs::create_dir_all(&nested).expect("nested");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "register"])
        .current_dir(&nested)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .output()
        .expect("run knives");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "already registered as demo");
}
```

Add to `tests/registry_binding.rs`:

```rust
#[test]
fn a_named_verb_whose_checkout_is_not_on_this_machine_exits_usage_and_says_so() {
    let lab = lab::Lab::new();
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        "[repos.ghost]\nupstream = \"https://forge.invalid/org/ghost\"\norigin = \"https://forge.invalid/acme/ghost\"\n",
    ).expect("registry");
    let output = knives(&lab, &home, &["--text", "notch", "--repo", "ghost"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no checkout of ghost under"), "{stderr}");
    assert!(!stderr.contains("known:"), "{stderr}");
}

#[test]
fn status_inside_a_bound_checkout_reports_only_it_and_carries_the_origin_note() {
    let lab = lab::Lab::new();
    let (home, _consumer) = lab::release_test_home(&lab);
    let output = knives(&lab, &home, &["--text", "status", "--no-landed", "--no-github"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("demo"), "{stdout}");
    // The lab's checkout origin is a local bare path; the registry says a forge URL.
    assert!(stdout.contains("origin remote is "), "{stdout}");
    assert!(stdout.contains("; registry says https://forge.invalid/acme/work.git"), "{stdout}");
}

#[test]
fn sync_outside_any_checkout_without_a_name_or_all_exits_usage() {
    let lab = lab::Lab::new();
    let (home, _consumer) = lab::release_test_home(&lab);
    let outside = lab.temp_path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "sync", "--no-github"])
        .current_dir(&outside)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .output()
        .expect("run knives");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("give a repo name, or --all"));
}

#[test]
fn status_outside_any_checkout_sweeps_every_entry_through_the_scan() {
    let lab = lab::Lab::new();
    let (home, _consumer) = lab::release_test_home(&lab);
    let outside = lab.temp_path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "status", "--no-landed", "--no-github"])
        .current_dir(&outside)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .output()
        .expect("run knives");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("demo"), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
}
```

- [ ] **Step 5: Run the whole suite until green**

Run: `cargo build 2>&1 | grep -E '^(error|warning)' | sort | uniq -c` until empty, then `env -u JJ_EMAIL -u JJ_USER cargo test 2>&1 | tail -30`, then `cd plugin && KNIVES_BIN=$PWD/../target/debug/knives bun test 2>&1 | tail -5`.
Expected: every cargo suite passes; bun reports the real-binary tests passing, not skipped. If the `HOME` override breaks an unrelated command in a test, fix that test's environment at the root; do not drop the override.

- [ ] **Step 6: Clippy, fmt, identity scan, leftover grep**

Run: `cargo clippy --all-targets 2>&1 | tail -5 && cargo fmt --check && cargo test --test no_hardcoded_identity 2>&1 | tail -3 && grep -rn 'entry\.path\|git_remotes\|git_toplevel\|containing(\|TrustedEntry\|mod init\|wip::gather' src/ tests/ plugin/ | grep -v 'DirEntry\|branches_containing'`
Expected: clean, clean, 2 passed, no output.

- [ ] **Step 7: Commit**

```bash
unset JJ_USER JJ_EMAIL
jj describe -m "feat(registry)!: path leaves the registry; checkouts are found by their remotes

[repos.<name>] holds upstream, origin, release and policy. A checkout is the
entry whose upstream its own upstream remote matches; every verb resolves a
Fork (entry plus found checkout) through the directory it runs in or a scan
of \$HOME. knives init and config::save are deleted: nothing writes the
registry. knives register prints an entry without path, or names the entry a
checkout already is. A path line, a [trusted.*] table, or two entries with
one upstream fail the load with the replacement named. origin and release
remotes that differ from the registry are reported as notes.

BREAKING CHANGE: repos.toml entries with path, and [trusted.*] tables, no
longer load."
```

**Verification:** an implementer knows this unit is done when `target/debug/knives status --no-landed --no-github` run inside a lab checkout reports that repository with the origin note, the same command run from a directory outside every checkout (with `HOME` pointing at a directory that contains the checkout) reports it through the scan, `knives notch --repo <absent>` exits 2 with `no checkout of <absent> under <home>`, `knives register` from a subdirectory of the checkout prints `already registered as <name>`, a registry carrying `path =` refuses to load with the replacement named — and `knives init` is an unrecognised subcommand.

---

### Task 4: Documentation and skills

**Depends on:** Task 3.

**Files:**
- Modify: `README.md` — the registration example (140-142), the Configuration block (144-167), the command table row for `knives init` (209), the allowlist paragraph (288-292)
- Modify: `docs/design.md` — the registry/trust section (~80-81), the command list (236), the allowlist bullets (438-440)
- Modify: `skills/using-knives/SKILL.md` — `knives init` section (388-391) deleted; `knives register`, `knives repos`, and "The registry" sections rewritten; a short "How a checkout is found" subsection
- Modify: `skills/fork-work/SKILL.md` — any `init`/`path` mention (grep); `not on this machine` is described as a scan miss ("the scan of `~` found no checkout here (three levels deep); check deeper or elsewhere before cloning"), not proof of absence
- No code.

- [ ] **Step 1: Grep for everything the change invalidated**

Run: `grep -rn -E 'knives init|path = |\[trusted|guidance_roots|containing\(' README.md docs/design.md skills/ omp/README.md 2>/dev/null`
Expected: a list; every hit is edited in this task (the `Repo::branches_containing(...)` mention in design.md is a false positive — leave it and say so).

- [ ] **Step 2: Rewrite the README configuration block**

Replace `README.md:144-167` with:

````markdown
## Configuration

`~/.config/knives/repos.toml` names repositories, not directories. knives finds
the checkout by its `upstream` remote — walking up from where you stand, or
scanning `~` to depth three when you are not inside one.

```toml
[repos.libcore]
upstream = "https://forge.example/org/libcore"
origin = "https://forge.example/ours/libcore"
base = "main"                         # optional: upstream's trunk (defaults to main)
release = "https://forge.example/company/libcore"   # optional: where releases publish
consumers = ["company/workbench"]     # optional: forge slugs that pin this repo's releases

[repos.tool]
upstream = "https://forge.example/org/tool"
origin = "https://forge.example/ours/tool"
release_branch = "integration"        # optional: fixed release branch instead of dated cuts
workspaces = "~/.worktrees/tool"      # optional: where `knives start` opens branch workspaces

[trust]
repos = ["company/workbench"]         # repositories whose AGENTS.md is injected, by identity
owners = ["ours", "company"]          # every repository under these owners
roots = ["~/projects/company"]        # every repository under these directories
```

A fork entry does not grant guidance; `[trust]` does, and it follows the
repository wherever it is cloned. From anywhere inside a checkout,
`knives register` prints the entry for it; nothing writes the file for you.
````

Delete the `knives init …` example at 140-142 (replace with `knives register` printing the snippet) and the `knives init` row at 209. Rewrite 288-292 so "the registry is the allowlist" reads "the `[trust]` rules are the allowlist".

- [ ] **Step 3: Update `docs/design.md`**

Replace the registry bullet list (~76-82) so it describes `[repos.*]` without `path`, the `upstream`-remote binding, the `$HOME` scan, and `[trust] repos/owners/roots`; state the normalisation rule as the code has it (a non-URL value compares as its trimmed text; URLs lose `.git`, trailing `/`, and case); update the security-posture line to say trust names repositories and no command writes the file. Remove `knives init` from the command list at 236 and rewrite the `knives register` line to include `already registered as <name>`. Rewrite the allowlist bullets at 438-440: `[trust]` provides guidance roots; `[repos.*]` provides fork commands and the managed notice only.

- [ ] **Step 4: Update the skills**

`skills/using-knives/SKILL.md`: delete the `knives init` section; in "The registry", replace the TOML example with the README's and add:

```markdown
### How a checkout is found

A checkout is the entry whose `upstream` its own `upstream` remote matches
(`.git`, trailing `/`, and case do not matter). Standing inside one — or inside
a `knives start` workspace of one — binds it. From anywhere else, `knives repos`,
`status --all`, and naming a repository scan `~` to depth three for checkouts;
an entry with no checkout found reads `not on this machine`, and an entry with
two is refused with both paths named. A checkout whose `origin` or `release`
remote differs from the registry still binds, and `status` and `repos` carry a
note saying so: `origin remote is <X>; registry says <Y>`.
```

Update `knives register` to mention `already registered as <name>`, and `knives repos` to mention the nullable path. In `skills/fork-work/SKILL.md`, fix any `init`/`path` mention the grep found and the `not on this machine` wording.

- [ ] **Step 5: Identity scan, then commit**

Run: `cargo test --test no_hardcoded_identity 2>&1 | tail -3 && grep -rn 'knives init' README.md docs skills --exclude-dir=superpowers | wc -l`
Expected: 2 passed; `0`.

```bash
unset JJ_USER JJ_EMAIL
jj describe -m "docs: registry names repositories; trust is its own list; init is gone"
```

**Verification:** an implementer knows this unit is done when the README's Configuration TOML block, pasted verbatim into a fresh `HOME`'s `.config/knives/repos.toml`, loads (`env HOME=<that dir> target/debug/knives repos` prints two rows, each `not on this machine`, exit 0), and `grep -rn 'knives init' README.md docs skills --exclude-dir=superpowers` prints nothing.

---

### Task 5: The shared registry in dotfiles — complete

Done in the first execution and preserved: jj workspace `registry` at `~/.dotfiles-registry`, change `vvqklwsu` on top of dotfiles `main`, described `knives: registry names repositories, not paths; trust by owner`. Sixteen `path` lines removed, three `workspaces` lines carried, `[trusted.*]` → `[trust] owners = [...]`, installer comment rewritten. **Not pushed.** The coordinator moves `main` to it and pushes after the knives release is installed (`mise up`), then forgets the workspace. Nothing for an implementer to do.

---

## End-to-end verification plan

Every scenario is driven through the surface a human uses: the built binary on a shell, or the hook binary fed a real event. Tooling: `jj`/`git` to build checkouts, `target/debug/knives`, the event fixtures in `tests/fixtures/claude_hook_*.json` as templates (rewrite `cwd`/`file_path` to the real paths). No new tooling is needed.

**Environment discipline for the acceptance run:** the hermetic home is created once with `mktemp -d` and its literal path is used in every command (`env HOME=/tmp/tmp.abc123 JJ_USER=lab JJ_EMAIL=lab@example.test knives …`). Nothing is exported across shell calls. Nothing is deleted with `rm -rf` on a variable; the coordinator removes the literal temp directory at the end.

**Hermetic scenarios** (fresh `HOME`, registry at `$HOME/.config/knives/repos.toml`):

1. **`knives repos` finds checkouts by scanning.** Build `$HOME/forks/tool/default` as a jj checkout (`jj git init --colocate`, `jj git remote add upstream https://forge.invalid/org/tool`, `… origin https://forge.invalid/ours/tool`) and `$HOME/other` with `upstream https://forge.invalid/org/other`; registry lists `tool`, `other`, and `ghost`. From `$HOME`: `knives repos` shows three rows — `tool` and `other` with their found paths, `ghost` with `not on this machine`. `--json`: `path` is a string, a string, `null`.
2. **`knives status` inside a checkout binds without a scan and carries the note.** Give `tool`'s checkout `origin https://forge.invalid/stranger/tool` while the registry says `ours`. Inside `$HOME/forks/tool/default/src`: `knives status --no-landed --no-github` reports `tool` only, with the note `origin remote is https://forge.invalid/stranger/tool; registry says https://forge.invalid/ours/tool`.
3. **`knives status tool` from outside** (`cd $HOME`): the same report, found through the scan. `knives status --no-landed --no-github` bare from `$HOME`: all three rows, `ghost` as a problem row.
4. **Two checkouts, one entry.** Clone `tool` again to `$HOME/tool-copy` with the same `upstream`. `knives status tool` exits 2 and names both paths; `knives repos` shows `tool` as `not on this machine` with a problem naming both. Remove the copy afterwards by its literal path.
5. **A workspace binds to its checkout.** Inside `tool`'s checkout: `knives start feat/x --why test` opens `$HOME/forks/tool/feat-x`; inside that directory `knives status --no-landed --no-github` reports `tool`; `knives finish feat/x` from there succeeds.
6. **`knives register`.** In an unregistered jj checkout with `upstream`/`origin` remotes: prints `[repos.<dirname>]` with `upstream`, `origin`, no `path`, exit 0. From a subdirectory of `tool`: prints `already registered as tool`, exit 0. In a directory with no repository: the not-a-repository refusal. In a plain git clone with an `upstream` remote: `is a git clone, not a jj checkout`.
7. **Refusals.** From `$HOME` (not a repo): `knives notch` → exit 2, `not inside a repository; name a repo…`. From a git repo with only `origin`: `has no \`upstream\` remote`. `knives notch --repo ghost` → `no checkout of ghost under $HOME` with no `known:` suffix.
8. **Old registry.** Put a `path = "~/x"` line in `tool`'s entry: every verb exits 3 with `[repos.tool] path is no longer a registry field; delete it — knives finds checkouts by their remotes`. Put back `[trusted.work]\npath = "~/w"`: the `[trusted.work] is no longer…` message. `knives init` → clap's unrecognised-subcommand error.
9. **Hook: trusted clone in /tmp.** Registry `[trust] owners = ["ours"]`. `git clone`-shaped repo at `/tmp/<random>/thing` with `origin https://forge.invalid/ours/thing` and an `AGENTS.md` reading `THING_GUIDANCE`. Feed `knives hook claude-code` a PostToolUse `Read` event whose `file_path` is inside it (`session_id` fresh): the response's `additionalContext` contains `THING_GUIDANCE` and no "managed" notice.
10. **Hook: managed fork outside trust.** `tool`'s remotes are `maintainer`/`stranger` and `[trust]` is `owners = ["ours"]`: the same event for a file in `tool` yields the managed notice (claims roster) and **no** `AGENTS.md` content. Add `repos = ["org/tool"]` to `[trust]` (matches `upstream`): now both appear. A file inside `tool`'s `feat-x` workspace (scenario 5) yields the workspace's own `AGENTS.md`.
11. **Docs are executable.** Paste the README Configuration block verbatim into `$HOME/.config/knives/repos.toml`; `knives repos` loads it and prints `libcore` and `tool` as `not on this machine`; `grep -rn 'knives init' README.md docs skills --exclude-dir=superpowers` prints nothing.
12. **Hook over a jj (non-colocated) checkout.** `jj git init` without `--colocate` for a trusted repo: guidance still arrives (remotes read through jj).

**This machine, real registry** (after Task 5's file is installed and the new binary is on `PATH` via `mise up`): `knives repos` lists sixteen rows, every one with a found path under `~` and none `not on this machine`; `knives status --all --no-landed --no-github` from `~` produces the same repository set as before the change; inside the one fork whose checkout `origin` lacks the `.git` suffix the registry spells (`knives repos` names it; compare `jj git remote list` to the registry line) `knives status --no-landed --no-github` carries **no** origin note — normalisation absorbed it; `knives hook claude-code` fed a Read event for a file in a `/tmp` clone of any repository under a `[trust] owners` entry returns that repository's `AGENTS.md`.

Each scenario is marked `RAN` with what was observed, `WAIVED-BY-SAMI`, or `BLOCKED` with the blocker. Test-suite results are not `RAN`.

## Hardening ledger

(empty)
