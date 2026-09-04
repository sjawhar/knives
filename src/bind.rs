//! Which registry entry a directory is, decided by its remotes.
//!
//! Identity is the remote named `upstream`: a checkout is entry X when its
//! `upstream` URL names the same repository as X's, whatever the spelling.
//! `origin` and `release` are compared and reported as notes, never used to
//! bind. Verbs that run outside a checkout find one by scanning `$HOME`.

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
    /// Whether this is a jj checkout (`.jj/repo` is a directory). Fork verbs need
    /// one; the hook binds git-only clones too.
    pub fn is_jj(&self) -> bool {
        self.path.join(".jj").join("repo").is_dir()
    }
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
    pub fn workspace_root(&self) -> &Path {
        self.entry
            .workspaces
            .as_deref()
            .unwrap_or_else(|| self.checkout.path.parent().unwrap_or(&self.checkout.path))
    }

    /// How the checkout's `origin` and `release` differ from the registry's, in
    /// that order; empty when both match. `release` is compared only when the
    /// entry has one.
    pub fn remote_notes(&self) -> Vec<String> {
        [
            ("origin", Some(self.entry.remote(Role::Origin))),
            ("release", self.entry.release.as_deref()),
        ]
        .into_iter()
        .filter_map(|(role, expected)| {
            let expected = expected?;
            match self.checkout.remotes.get(role) {
                None => Some(format!("{role} remote absent; registry says {expected}")),
                Some(actual) if !same_remote(actual, expected) => Some(format!(
                    "{role} remote is {actual}; registry says {expected}"
                )),
                Some(_) => None,
            }
        })
        .collect()
    }
}

#[cfg(test)]
impl<'a> Fork<'a> {
    /// A fork whose checkout is at `path` and declares no remotes, for unit
    /// tests that need a checkout location without a repository.
    pub(crate) fn at(name: &str, entry: &'a RepoEntry, path: &Path) -> Self {
        Self {
            name: RepoName::new(name),
            entry,
            checkout: Checkout {
                path: path.to_owned(),
                remotes: BTreeMap::new(),
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("{} is neither a jj nor a git repository", root.display())]
    NotARepository { root: PathBuf },
    #[error("reading remotes of {}: {detail}", root.display())]
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
    pub fn message(&self) -> String {
        match self {
            Self::NotInsideARepository => {
                "not inside a repository; name a repo, or run this from inside one".to_owned()
            }
            Self::NoUpstream { root } => format!(
                "{} has no `upstream` remote, so it is not a managed fork; name a repo",
                root.display()
            ),
            Self::Unregistered { root, upstream } => format!(
                "{} forks {upstream}, which is not in the registry; `knives register` prints the entry",
                root.display()
            ),
        }
    }
}

/// Why `resolve` did not produce a fork.
#[derive(Debug, PartialEq, Eq)]
pub enum Unresolved {
    Unknown,
    Missing { home: PathBuf },
    Duplicate { home: PathBuf, paths: Vec<PathBuf> },
}

impl Unresolved {
    /// `Unknown` renders `unknown repo <name>`, `Missing` renders
    /// `no checkout of <name> under <home>`, and `Duplicate` renders
    /// `<name> has <n> checkouts under <home>: <paths>; knives will not choose`.
    /// The caller appends `; known: a, b` to `Unknown` — the registry's names are
    /// its to list.
    pub fn message(&self, name: &RepoName) -> String {
        match self {
            Self::Unknown => format!("unknown repo {name}"),
            Self::Missing { home } => format!("no checkout of {name} under {}", home.display()),
            Self::Duplicate { home, paths } => {
                let listed = paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "{name} has {} checkouts under {}: {listed}; knives will not choose",
                    paths.len(),
                    home.display()
                )
            }
        }
    }
}

/// Whether two remote spellings name one repository.
///
/// A value that parses as a remote URL with a host compares as (host without
/// user, path without trailing `/` or `.git`), case-insensitively. A value that
/// does not (a filesystem path, or a `file://` URL, whose authority is empty)
/// compares as its trimmed text, so two directories that differ by `.git` stay
/// two directories.
pub fn same_remote(a: &str, b: &str) -> bool {
    remote_key(a) == remote_key(b)
}

fn remote_key(remote: &str) -> String {
    let trimmed = remote.trim().trim_end_matches('/');
    match remote_authority_and_path(trimmed) {
        Some((authority, path)) if !authority.is_empty() => {
            let host = authority.rsplit('@').next().unwrap_or(authority);
            let path = path.trim_matches('/');
            let path = path.strip_suffix(".git").unwrap_or(path);
            format!(
                "{}/{}",
                host.to_ascii_lowercase(),
                path.to_ascii_lowercase()
            )
        }
        _ => trimmed.to_owned(),
    }
}

/// `(authority, path)` of `scheme://authority/path` or `user@authority:path`;
/// `None` otherwise.
pub fn remote_authority_and_path(url: &str) -> Option<(&str, &str)> {
    let url = url.trim_end_matches('/');
    if let Some((_, authority_and_path)) = url.split_once("://") {
        return authority_and_path.split_once('/');
    }
    let (authority, path) = url.split_once(':')?;
    authority.contains('@').then_some((authority, path))
}

/// The owner segment of an authority-delimited `<owner>/<repository>` remote path.
pub fn url_owner(url: &str) -> Option<&str> {
    let (_, path) = remote_authority_and_path(url)?;
    let (owner, repository) = path.split_once('/')?;
    (!owner.is_empty() && !repository.is_empty()).then_some(owner)
}

/// The `owner/repo` path of a forge remote with trailing `/` and `.git` removed;
/// `None` for a non-URL.
pub fn remote_slug(url: &str) -> Option<&str> {
    let (_, path) = remote_authority_and_path(url.trim().trim_end_matches('/'))?;
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repository) = path.split_once('/')?;
    (!owner.is_empty() && !repository.is_empty() && !repository.contains('/')).then_some(path)
}

/// The first ancestor of `path` (canonicalised) holding `.jj` or `.git`.
///
/// Nearest marker of either kind wins, so a clone nested inside a checkout is
/// its own root and never inherits the enclosing identity. A jj workspace is
/// its own root here.
pub fn nearest_root(path: &Path) -> Option<PathBuf> {
    let start = path.canonicalize().ok()?;
    start
        .ancestors()
        .find(|directory| directory.join(".jj").is_dir() || directory.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The checkout `path` belongs to.
///
/// [`nearest_root`], then — when that root's `.jj/repo` is a file — the checkout
/// the pointer names. An unreadable pointer returns the workspace root itself so
/// the remote reader surfaces jj's own error.
pub fn checkout_root(path: &Path) -> Option<PathBuf> {
    let root = nearest_root(path)?;
    let jj_dir = root.join(".jj");
    let pointer = jj_dir.join("repo");
    if !pointer.is_file() {
        return Some(root);
    }
    // A workspace's pointer holds the checkout's `.jj/repo` store, relative to
    // the workspace's `.jj` when it can be; the checkout is two levels above it.
    let Ok(text) = std::fs::read_to_string(&pointer) else {
        return Some(root);
    };
    let store = jj_dir.join(text.trim());
    let checkout = store
        .parent()
        .and_then(Path::parent)
        .map_or(root, |checkout| {
            checkout
                .canonicalize()
                .unwrap_or_else(|_| checkout.to_owned())
        });
    Some(checkout)
}

/// Remotes of the repository rooted at `root`, read from jj when `.jj` is
/// present (colocated or not, checkout or workspace), from git when only `.git`
/// is present.
pub fn remotes(root: &Path) -> Result<BTreeMap<String, String>, BindError> {
    let failure = |detail: String| BindError::Remotes {
        root: root.to_owned(),
        detail,
    };
    let listing = if root.join(".jj").is_dir() {
        std::process::Command::new("jj")
            .arg("-R")
            .arg(root)
            .args(["--ignore-working-copy", "git", "remote", "list"])
            .output()
    } else if root.join(".git").exists() {
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", "--get-regexp", "^remote\\..*\\.url$"])
            .output()
    } else {
        return Err(BindError::NotARepository {
            root: root.to_owned(),
        });
    };
    let output = listing.map_err(|error| failure(error.to_string()))?;
    // git exits 1 with empty output when nothing matches: no remotes, not an error.
    let no_matches =
        output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty();
    if !output.status.success() && !no_matches {
        return Err(failure(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (key, url) = line
                .split_once(' ')
                .ok_or_else(|| failure(format!("unparseable remote line {line:?}")))?;
            // jj prints `name url`; git prints `remote.name.url url`.
            let name = key
                .strip_prefix("remote.")
                .and_then(|rest| rest.strip_suffix(".url"))
                .unwrap_or(key);
            Ok((name.to_owned(), url.trim().to_owned()))
        })
        .collect()
}

/// The entry whose `upstream` matches; `None` when none does.
pub fn entry_for<'a>(registry: &'a Registry, upstream: &str) -> Option<(RepoName, &'a RepoEntry)> {
    registry
        .repos
        .iter()
        .find(|(_, entry)| same_remote(&entry.upstream, upstream))
        .map(|(name, entry)| (RepoName::new(name.as_str()), entry))
}

/// The fork the current directory is inside.
pub fn here<'a>(
    registry: &'a Registry,
    cwd: &Path,
) -> Result<Result<Fork<'a>, Unbound>, BindError> {
    let Some(root) = checkout_root(cwd) else {
        return Ok(Err(Unbound::NotInsideARepository));
    };
    let remotes = remotes(&root)?;
    let Some(upstream) = remotes.get("upstream") else {
        return Ok(Err(Unbound::NoUpstream { root }));
    };
    let Some((name, entry)) = entry_for(registry, upstream) else {
        return Ok(Err(Unbound::Unregistered {
            root,
            upstream: upstream.clone(),
        }));
    };
    Ok(Ok(Fork {
        name,
        entry,
        checkout: Checkout {
            path: root,
            remotes,
        },
    }))
}

/// Every entry's checkout under `home`, and what could not be decided.
#[derive(Debug, Default)]
pub struct Scan<'a> {
    pub found: BTreeMap<RepoName, Fork<'a>>,
    /// Entries with more than one checkout: every path, sorted.
    pub duplicates: BTreeMap<RepoName, Vec<PathBuf>>,
    /// Directories that looked like checkouts but whose remotes could not be read.
    pub problems: Vec<String>,
}

/// `home` is depth 0; `~/a/b/c` is read and its children are not queued.
const SCAN_DEPTH: usize = 3;

/// Scan `home` to depth three for jj checkouts and bind each to its entry.
///
/// Directories named with a leading `.` are skipped, symlinks are not followed,
/// and nothing below a `.jj` or `.git` is visited — except `home` itself, whose
/// children are always visited: a home that is a repository (a dotfiles
/// checkout, say) still holds the forks under it. A `.jj/repo` directory is a
/// checkout; a `.jj/repo` file is a workspace, found through its checkout; a
/// git-only clone is not a fork checkout.
pub fn scan<'a>(registry: &'a Registry, home: &Path) -> Scan<'a> {
    let mut scan = Scan::default();
    // Keyed by name and carrying the entry, so the split below needs no lookup.
    let mut candidates: BTreeMap<RepoName, (&'a RepoEntry, Vec<Checkout>)> = BTreeMap::new();
    let mut pending = vec![(home.to_owned(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        let jj = directory.join(".jj");
        let is_jj = jj.is_dir();
        if is_jj && jj.join("repo").is_dir() {
            match remotes(&directory) {
                Ok(remotes) => {
                    if let Some(upstream) = remotes.get("upstream")
                        && let Some((name, entry)) = entry_for(registry, upstream)
                    {
                        let path = directory
                            .canonicalize()
                            .unwrap_or_else(|_| directory.clone());
                        candidates
                            .entry(name)
                            .or_insert((entry, Vec::new()))
                            .1
                            .push(Checkout { path, remotes });
                    }
                }
                Err(error) => scan.problems.push(error.to_string()),
            }
        }
        let is_repository = is_jj || directory.join(".git").exists();
        if (is_repository && depth > 0) || depth == SCAN_DEPTH {
            continue;
        }
        let Ok(children) = std::fs::read_dir(&directory) else {
            continue;
        };
        for child in children.flatten() {
            // `file_type` does not follow symlinks, so a linked directory is not one.
            let is_directory = child.file_type().is_ok_and(|kind| kind.is_dir());
            let hidden = child.file_name().as_encoded_bytes().starts_with(b".");
            if is_directory && !hidden {
                pending.push((child.path(), depth + 1));
            }
        }
    }
    for (name, (entry, mut checkouts)) in candidates {
        checkouts.sort_by(|a, b| a.path.cmp(&b.path));
        match <[Checkout; 1]>::try_from(checkouts) {
            Ok([checkout]) => {
                scan.found.insert(
                    name.clone(),
                    Fork {
                        name,
                        entry,
                        checkout,
                    },
                );
            }
            Err(many) => {
                scan.duplicates.insert(
                    name,
                    many.into_iter().map(|checkout| checkout.path).collect(),
                );
            }
        }
    }
    scan
}

/// One named entry's fork: the cwd's when it binds to `name`, else the scan's.
///
/// A `BindError` from the cwd's own repository propagates: a checkout whose
/// remotes cannot be read is an error to show, not a reason to scan elsewhere.
pub fn resolve<'a>(
    registry: &'a Registry,
    name: &RepoName,
    cwd: &Path,
    home: &Path,
) -> Result<Result<Fork<'a>, Unresolved>, BindError> {
    if registry.get(name).is_none() {
        return Ok(Err(Unresolved::Unknown));
    }
    if let Ok(fork) = here(registry, cwd)?
        && fork.name == *name
    {
        return Ok(Ok(fork));
    }
    let mut scan = scan(registry, home);
    if let Some(fork) = scan.found.remove(name) {
        return Ok(Ok(fork));
    }
    if let Some(paths) = scan.duplicates.remove(name) {
        return Ok(Err(Unresolved::Duplicate {
            home: home.to_owned(),
            paths,
        }));
    }
    Ok(Err(Unresolved::Missing {
        home: home.to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use crate::config::RepoEntry;
    use crate::ids::RepoName;

    use super::{Checkout, Fork, Unbound, Unresolved, remote_slug, same_remote, url_owner};

    fn entry(upstream: &str, origin: &str, release: Option<&str>) -> RepoEntry {
        RepoEntry {
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
        assert!(same_remote(
            "https://forge.example/org/tool",
            "git@forge.example:org/tool.git"
        ));
        assert!(same_remote(
            "https://forge.example/org/tool.git/",
            "HTTPS://Forge.Example/Org/Tool"
        ));
        assert!(same_remote(
            "ssh://git@forge.example/org/tool",
            "https://forge.example/org/tool"
        ));
    }

    #[test]
    fn different_repositories_are_not_the_same_remote() {
        assert!(!same_remote(
            "https://forge.example/org/tool",
            "https://forge.example/org/tool-2"
        ));
        assert!(!same_remote(
            "https://forge.example/org/tool",
            "https://forge.example/other/tool"
        ));
        assert!(!same_remote(
            "https://forge.example/org/tool",
            "https://elsewhere.example/org/tool"
        ));
    }

    #[test]
    fn a_filesystem_path_compares_as_its_trimmed_text() {
        assert!(same_remote("/tmp/lab/upstream", " /tmp/lab/upstream/ "));
        assert!(!same_remote("/tmp/lab/upstream", "/tmp/lab/other"));
        // Two directories that differ by `.git` are two directories, spelled
        // as paths or as `file://` URLs.
        assert!(!same_remote("/tmp/lab/origin.git", "/tmp/lab/origin"));
        assert!(!same_remote("file:///tmp/x.git", "file:///tmp/x"));
        assert!(same_remote("file:///tmp/x.git", "file:///tmp/x.git/"));
    }

    #[test]
    fn a_remote_slug_is_the_owner_and_repository_of_a_forge_url() {
        assert_eq!(
            remote_slug("https://forge.example/Org/Tool.git/"),
            Some("Org/Tool")
        );
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
                    (
                        "upstream".to_owned(),
                        "https://forge.example/org/tool".to_owned(),
                    ),
                    (
                        "origin".to_owned(),
                        "git@forge.example:ours/tool".to_owned(),
                    ),
                    (
                        "release".to_owned(),
                        "https://forge.example/company/tool.git".to_owned(),
                    ),
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
                    (
                        "upstream".to_owned(),
                        "https://forge.example/org/tool".to_owned(),
                    ),
                    (
                        "origin".to_owned(),
                        "https://forge.example/stranger/tool".to_owned(),
                    ),
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
        let registry_entry = entry(
            "https://forge.example/org/tool",
            "https://forge.example/ours/tool",
            None,
        );
        let fork = Fork {
            name: RepoName::new("tool"),
            entry: &registry_entry,
            checkout: Checkout {
                path: PathBuf::from("/checkout"),
                remotes: BTreeMap::from([(
                    "upstream".to_owned(),
                    "https://forge.example/org/tool".to_owned(),
                )]),
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
            checkout: Checkout {
                path: PathBuf::from("/forks/tool/default"),
                remotes: BTreeMap::new(),
            },
        };
        assert_eq!(fork.workspace_root(), Path::new("/forks/tool"));
        let mut with_workspaces = entry("u", "o", None);
        with_workspaces.workspaces = Some(PathBuf::from("/worktrees/tool"));
        let fork = Fork {
            entry: &with_workspaces,
            ..fork
        };
        assert_eq!(fork.workspace_root(), Path::new("/worktrees/tool"));
    }

    #[test]
    fn every_refusal_renders_its_exact_text() {
        assert_eq!(
            Unbound::NotInsideARepository.message(),
            "not inside a repository; name a repo, or run this from inside one"
        );
        assert_eq!(
            Unbound::NoUpstream {
                root: PathBuf::from("/r")
            }
            .message(),
            "/r has no `upstream` remote, so it is not a managed fork; name a repo"
        );
        assert_eq!(
            Unbound::Unregistered {
                root: PathBuf::from("/r"),
                upstream: "https://forge.example/o/t".to_owned()
            }
            .message(),
            "/r forks https://forge.example/o/t, which is not in the registry; `knives register` prints the entry"
        );
        let name = RepoName::new("tool");
        assert_eq!(Unresolved::Unknown.message(&name), "unknown repo tool");
        assert_eq!(
            Unresolved::Missing {
                home: PathBuf::from("/home/x")
            }
            .message(&name),
            "no checkout of tool under /home/x"
        );
        assert_eq!(
            Unresolved::Duplicate {
                home: PathBuf::from("/home/x"),
                paths: vec![PathBuf::from("/home/x/a"), PathBuf::from("/home/x/b")]
            }
            .message(&name),
            "tool has 2 checkouts under /home/x: /home/x/a, /home/x/b; knives will not choose"
        );
    }
}
