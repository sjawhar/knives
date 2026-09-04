//! Which registry entry a directory is, decided by its remotes.
//!
//! Identity is the remote named `upstream`: a checkout is entry X when its
//! `upstream` URL names the same repository as X's, whatever the spelling.
//! `origin` and `release` are compared and reported as notes, never used to
//! bind. Verbs that run outside a checkout find one by scanning `$HOME`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::config::{Registry, RepoEntry, Role};
use crate::ids::RepoName;
use crate::jj::{Unvouched, vouched_workspace};

/// A repository root on this machine and the remotes it declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    /// The checkout root: the directory whose `.jj/repo` is a directory (or the
    /// `.git`-only root). A workspace resolves to its checkout, never to itself.
    pub path: PathBuf,
    pub remotes: BTreeMap<String, String>,
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

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum BindError {
    #[error("{} is neither a jj nor a git repository", root.display())]
    NotARepository { root: PathBuf },
    #[error("reading remotes of {}: {detail}", root.display())]
    RemotesUnreadable { root: PathBuf, detail: String },
}

/// Why `here` did not bind.
#[derive(Debug, PartialEq, Eq)]
pub enum Unbound {
    /// No `.jj` or `.git` at or above the directory.
    NotInsideARepository,
    /// A git clone with no `.jj`: the hook binds those, fork verbs need jj.
    GitOnly { root: PathBuf },
    /// A repository, but it declares no `upstream` remote.
    NoUpstream { root: PathBuf },
    /// A fork of something the registry does not list.
    Unregistered { root: PathBuf, upstream: String },
    /// A repository whose remotes could not be read.
    Unreadable(BindError),
}

impl Unbound {
    /// The refusal. One that a repo name would fix ends with `; known: a, b`,
    /// the registry's names; a git clone or an unreadable repository is refused
    /// in its own words, since typing a name is not the fix.
    pub fn message(&self, registry: &Registry) -> String {
        let text = match self {
            Self::NotInsideARepository => {
                "not inside a repository; name a repo, or run this from inside one".to_owned()
            }
            Self::GitOnly { root } => {
                return format!(
                    "{} is a git clone, not a jj checkout; fork commands need jj",
                    root.display()
                );
            }
            Self::NoUpstream { root } => format!(
                "{} has no `upstream` remote, so it is not a managed fork; name a repo",
                root.display()
            ),
            Self::Unregistered { root, upstream } => format!(
                "{} forks {upstream}, which is not in the registry; `knives register` prints the entry",
                root.display()
            ),
            Self::Unreadable(error) => return error.to_string(),
        };
        format!("{text}; known: {}", known(registry))
    }
}

/// Why `resolve` did not produce a fork.
///
/// `Missing` and `Duplicate` carry the scan's own complaints, so a checkout
/// whose remotes could not be read is named beside the entry it may have been.
#[derive(Debug, PartialEq, Eq)]
pub enum Unresolved {
    Unknown,
    Missing {
        home: PathBuf,
        problems: Vec<String>,
    },
    Duplicate {
        home: PathBuf,
        paths: Vec<PathBuf>,
        problems: Vec<String>,
    },
}

impl Unresolved {
    /// `Unknown` renders `unknown repo <name>; known: a, b`, `Missing` renders
    /// `no checkout of <name> under <home>`, and `Duplicate` renders
    /// `<name> has <n> checkouts under <home>: <paths>; knives will not choose`.
    /// Each scan problem follows as `; could not read: <problem>`. A name that
    /// is known but has no checkout, or two, does not list the registry, since
    /// listing entries would not help find a directory.
    pub fn message(&self, name: &RepoName, registry: &Registry) -> String {
        match self {
            Self::Unknown => format!("unknown repo {name}; known: {}", known(registry)),
            Self::Missing { home, problems } => with_problems(
                format!("no checkout of {name} under {}", home.display()),
                problems,
            ),
            Self::Duplicate {
                home,
                paths,
                problems,
            } => {
                let listed = paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                with_problems(
                    format!(
                        "{name} has {} checkouts under {}: {listed}; knives will not choose",
                        paths.len(),
                        home.display()
                    ),
                    problems,
                )
            }
        }
    }
}

fn with_problems(mut text: String, problems: &[String]) -> String {
    for problem in problems {
        let _ = write!(text, "; could not read: {problem}");
    }
    text
}

/// The registry's names, for a refusal that has to say what it does know.
fn known(registry: &Registry) -> String {
    registry
        .names()
        .map(|name| name.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether two remote spellings name one repository.
///
/// A value that parses as a remote URL with a host compares as its
/// [`host_and_path`], case-insensitively. A value that does not (a filesystem
/// path, or a `file://` URL, whose authority is empty) compares as its trimmed
/// text, so two directories that differ by `.git` stay two directories.
pub fn same_remote(a: &str, b: &str) -> bool {
    match (host_and_path(a), host_and_path(b)) {
        (Some((host_a, path_a)), Some((host_b, path_b))) => {
            host_a.eq_ignore_ascii_case(host_b) && path_a.eq_ignore_ascii_case(path_b)
        }
        (None, None) => a.trim().trim_end_matches('/') == b.trim().trim_end_matches('/'),
        _ => false,
    }
}

/// `(host, path)` of a remote URL as spelled: the authority without its user
/// or port, and the path without surrounding `/` or a `.git` suffix. `None`
/// for a non-URL: a filesystem path, or a `file://` URL, whose authority is
/// empty. scp form without a user, `host:path`, is a URL too when the part
/// before the colon holds no `/`; a filesystem path with a colon in a later
/// component stays a path.
fn host_and_path(remote: &str) -> Option<(&str, &str)> {
    let trimmed = remote.trim().trim_end_matches('/');
    let (authority, path) = remote_authority_and_path(trimmed).or_else(|| {
        let (host, path) = trimmed.split_once(':')?;
        (!host.is_empty() && !host.contains('/')).then_some((host, path))
    })?;
    if authority.is_empty() {
        return None;
    }
    let host = authority.rsplit('@').next().unwrap_or(authority);
    // `host:2222` is `host`: the port is how to reach it, not what it is.
    let host = match host.rsplit_once(':') {
        Some((name, port))
            if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            name
        }
        _ => host,
    };
    Some((host, without_git_suffix(path.trim_matches('/'))))
}

/// `path` without a trailing `.git` in any case.
const fn without_git_suffix(path: &str) -> &str {
    match path.split_at_checked(path.len().saturating_sub(4)) {
        Some((stem, suffix)) if suffix.eq_ignore_ascii_case(".git") => stem,
        _ => path,
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
/// The host of a remote URL, without its user; `None` for a non-URL.
pub fn remote_host(url: &str) -> Option<&str> {
    let (authority, _) = remote_authority_and_path(url)?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    (!host.is_empty()).then_some(host)
}

/// The owner segment of an authority-delimited `<owner>/<repository>` remote path.
///
/// Unlike [`remote_slug`], an empty authority (`https:///owner/repo`) still
/// yields its owner: this feeds heuristics that should stay conservative when a
/// URL is odd, not identity, which needs a host.
pub fn url_owner(url: &str) -> Option<&str> {
    let (_, path) = remote_authority_and_path(url)?;
    let (owner, repository) = path.split_once('/')?;
    (!owner.is_empty() && !repository.is_empty()).then_some(owner)
}

/// The `owner/repo` path of a forge remote with trailing `/` and `.git` removed;
/// `None` for a non-URL.
pub fn remote_slug(url: &str) -> Option<&str> {
    let (_, path) = host_and_path(url)?;
    let (owner, repository) = path.split_once('/')?;
    (!owner.is_empty() && !repository.is_empty() && !repository.contains('/')).then_some(path)
}

/// The last path segment of a remote URL without `.git`: the repository's own
/// name, whichever owner or forge holds it.
pub fn repository_name(url: &str) -> Option<&str> {
    let (_, repository) = url.trim_end_matches('/').rsplit_once('/')?;
    let name = without_git_suffix(repository);
    (!name.is_empty()).then_some(name)
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

/// The checkout `path` belongs to: [`nearest_root`], then [`checkout_of_root`].
pub fn checkout_root(path: &Path) -> Option<PathBuf> {
    nearest_root(path).map(|root| checkout_of_root(&root))
}

/// The checkout a repository root belongs to: the root itself, or — when its
/// `.jj/repo` is a file and the checkout it names vouches for the root as its
/// workspace ([`vouched_pointer`]) — that checkout.
///
/// This decides where a fork verb operates ([`here`] reads that checkout's
/// remotes) and folds a workspace into its checkout. A pointer the checkout
/// does not vouch for, one that cannot be read, or one that names a store that
/// is no longer a directory (the checkout was deleted under the workspace)
/// leaves the root as its own, so [`remotes`] refuses it in the pointer's
/// terms, or jj's, rather than reporting a directory that is not a repository.
pub fn checkout_of_root(root: &Path) -> PathBuf {
    match vouched_pointer(root) {
        Some(Ok(checkout)) => checkout,
        Some(Err(_)) | None => root.to_owned(),
    }
}

/// What the `.jj/repo` pointer file at `root` says. `Ok(checkout)` when the
/// checkout it names vouches for `root` as its workspace: the operation
/// `root/.jj/working_copy` recorded is in that checkout's operation store, and
/// the checkout's view at head has a working-copy commit under the workspace's
/// name. `Err(detail)` when it does not — a pointer file is ordinary content
/// any tree can carry, and only the checkout's records say whose workspace the
/// tree is. `None` when `root` has no pointer file, or the pointer names no
/// store that exists: jj's own error is the report then.
fn vouched_pointer(root: &Path) -> Option<Result<PathBuf, String>> {
    let jj_dir = root.join(".jj");
    let pointer = jj_dir.join("repo");
    if !pointer.is_file() {
        return None;
    }
    // A workspace's pointer holds the checkout's `.jj/repo` store, relative to
    // the workspace's `.jj` when it can be; the checkout is two levels above it.
    let text = std::fs::read_to_string(&pointer).ok()?;
    let store = jj_dir.join(text.trim());
    if !store.is_dir() {
        return None;
    }
    let checkout = store.parent()?.parent()?;
    let checkout = checkout
        .canonicalize()
        .unwrap_or_else(|_| checkout.to_owned());
    Some(match vouched_workspace(&checkout, root) {
        Ok(_) => Ok(checkout),
        Err(Unvouched::NotARepository { detail }) => Err(format!(
            ".jj/repo names {}, which is not a repository: {detail}",
            checkout.display()
        )),
        Err(Unvouched::StateUnreadable) => Err(".jj/working_copy cannot be read".to_owned()),
        Err(Unvouched::Unknown {
            workspace,
            operation,
        }) => Err(format!(
            ".jj/repo names {}, which has no workspace {workspace} at operation {operation}",
            checkout.display()
        )),
    })
}

/// Remotes of the repository rooted at `root`, from the VCS that owns the root.
///
/// `.git` present (a directory, or a worktree's pointer file, which git
/// validates itself) → `git -C root config --get-regexp '^remote\..*\.url$'`,
/// whether or not `.jj` is beside it: a colocated checkout keeps its remotes
/// there anyway. Otherwise `.jj` a directory → `jj -R root git remote list`;
/// when `.jj/repo` is a pointer file, only once the checkout it names vouches
/// for `root` as its workspace ([`vouched_pointer`]), since `jj -R` follows any
/// pointer whose tree carries a plausible `.jj/working_copy`. Git wins because
/// a `.jj/repo` pointer *file* is ordinary content a clone can carry, while the
/// `.git` that arrives with it holds the remotes it was actually cloned from.
pub fn remotes(root: &Path) -> Result<BTreeMap<String, String>, BindError> {
    let failure = |detail: String| BindError::RemotesUnreadable {
        root: root.to_owned(),
        detail,
    };
    let listing = if root.join(".git").exists() {
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", "--get-regexp", "^remote\\..*\\.url$"])
            .output()
    } else if root.join(".jj").is_dir() {
        if let Some(Err(detail)) = vouched_pointer(root) {
            return Err(failure(detail));
        }
        std::process::Command::new("jj")
            .arg("-R")
            .arg(root)
            .args(["--ignore-working-copy", "git", "remote", "list"])
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
        return Err(failure(error_line(&output.stderr)));
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
/// The line of a VCS's stderr that explains a failure: jj may print a
/// `Warning:` (per-repo config, say) before its error and follows the error
/// with hints; git prints the error first.
fn error_line(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let lines = || {
        stderr
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
    };
    lines()
        .find(|line| !line.starts_with("Warning:"))
        .or_else(|| lines().next())
        .unwrap_or_default()
        .to_owned()
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
///
/// The checkout its nearest root belongs to, bound by the remotes read there.
/// A verb ran in that directory, so its checkout's remotes are what the verb
/// is about — no trust decision rests on them.
pub fn here<'a>(registry: &'a Registry, cwd: &Path) -> Result<Fork<'a>, Unbound> {
    let Some(root) = checkout_root(cwd) else {
        return Err(Unbound::NotInsideARepository);
    };
    if !root.join(".jj").is_dir() {
        return Err(Unbound::GitOnly { root });
    }
    let remotes = remotes(&root).map_err(Unbound::Unreadable)?;
    let Some(upstream) = remotes.get("upstream") else {
        return Err(Unbound::NoUpstream { root });
    };
    let Some((name, entry)) = entry_for(registry, upstream) else {
        return Err(Unbound::Unregistered {
            root,
            upstream: upstream.clone(),
        });
    };
    Ok(Fork {
        name,
        entry,
        checkout: Checkout {
            path: root,
            remotes,
        },
    })
}

/// Every entry's checkout under `home`, and what could not be decided.
#[derive(Debug, Default)]
pub struct Scan<'a> {
    /// The scan root, named in every refusal about an entry it did not place.
    pub home: PathBuf,
    pub found: BTreeMap<RepoName, Fork<'a>>,
    /// Entries with more than one checkout: every path, sorted.
    pub duplicates: BTreeMap<RepoName, Vec<PathBuf>>,
    /// What could not be read: a checkout whose remotes failed, a directory
    /// whose listing failed. Never dropped; every report of the scan names them.
    pub problems: Vec<String>,
}

impl Scan<'_> {
    /// Why `name` is not in `found`: two checkouts, or none. The scan's own
    /// problems are reported by the caller, once, not on every entry.
    pub fn unplaced(&self, name: &RepoName) -> Unresolved {
        self.duplicates.get(name).map_or_else(
            || Unresolved::Missing {
                home: self.home.clone(),
                problems: Vec::new(),
            },
            |paths| Unresolved::Duplicate {
                home: self.home.clone(),
                paths: paths.clone(),
                problems: Vec::new(),
            },
        )
    }
}

/// `home` is depth 0; `~/a/b/c` is read and its children are not queued.
const SCAN_DEPTH: usize = 3;

/// Scan `home` for jj checkouts ([`checkouts_under`]) and bind each to its entry.
///
/// Every checkout's remotes are read at once, one thread each: each read is a
/// process spawn, and a home holds tens of checkouts, not thousands.
///
/// A checkout whose remotes cannot be read is a problem only while some entry
/// is still unplaced — neither found nor found twice — since it may be where
/// that entry lives. Once every entry is placed, unreadable strangers are
/// dropped: the scan locates entries, it does not audit the health of every
/// repository under `home`.
pub fn scan<'a>(registry: &'a Registry, home: &Path) -> Scan<'a> {
    let mut scan = Scan {
        home: home.to_owned(),
        ..Scan::default()
    };
    let checkouts = checkouts_under(home, &mut scan.problems);
    #[allow(
        clippy::needless_collect,
        reason = "every reader is spawned before any is joined; the suggested form joins each before spawning the next"
    )]
    let read = std::thread::scope(|threads| {
        let handles: Vec<_> = checkouts
            .iter()
            .map(|directory| threads.spawn(move || remotes(directory)))
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect::<Vec<_>>()
    });
    // Keyed by name and carrying the entry, so the split below needs no lookup.
    let mut candidates: BTreeMap<RepoName, (&'a RepoEntry, Vec<Checkout>)> = BTreeMap::new();
    let mut unreadable = Vec::new();
    for (directory, remotes) in checkouts.into_iter().zip(read) {
        match remotes {
            Ok(remotes) => {
                if let Some(upstream) = remotes.get("upstream")
                    && let Some((name, entry)) = entry_for(registry, upstream)
                {
                    let path = directory.canonicalize().unwrap_or(directory);
                    candidates
                        .entry(name)
                        .or_insert((entry, Vec::new()))
                        .1
                        .push(Checkout { path, remotes });
                }
            }
            Err(error) => unreadable.push(error.to_string()),
        }
    }
    let every_entry_placed = registry.names().all(|name| candidates.contains_key(&name));
    if !every_entry_placed {
        scan.problems.extend(unreadable);
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

/// The jj checkouts (`.jj/repo` a directory) under `home`, to depth three.
///
/// Directories named with a leading `.` are skipped, symlinks are not followed,
/// and nothing below a `.jj` is visited — except `home` itself, whose children
/// are always visited: a home that is a repository (a dotfiles checkout, say)
/// still holds the forks under it. A `.jj/repo` file is a workspace, found
/// through its checkout; a git-only clone is not a fork checkout, and a
/// git-tracked parent (`~/work/.git`) does not hide the forks beneath it. A
/// directory that could not be listed is pushed to `problems`.
fn checkouts_under(home: &Path, problems: &mut Vec<String>) -> Vec<PathBuf> {
    let mut checkouts = Vec::new();
    let mut pending = vec![(home.to_owned(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        let jj = directory.join(".jj");
        let is_jj = jj.is_dir();
        if is_jj && jj.join("repo").is_dir() {
            checkouts.push(directory.clone());
        }
        if (is_jj && depth > 0) || depth == SCAN_DEPTH {
            continue;
        }
        let children = match std::fs::read_dir(&directory) {
            Ok(children) => children,
            Err(error) => {
                problems.push(format!("cannot read {}: {error}", directory.display()));
                continue;
            }
        };
        for child in children {
            let child = match child {
                Ok(child) => child,
                Err(error) => {
                    problems.push(format!("cannot read {}: {error}", directory.display()));
                    continue;
                }
            };
            // `file_type` does not follow symlinks, so a linked directory is not one.
            let is_directory = child.file_type().is_ok_and(|kind| kind.is_dir());
            let hidden = child.file_name().as_encoded_bytes().starts_with(b".");
            if is_directory && !hidden {
                pending.push((child.path(), depth + 1));
            }
        }
    }
    checkouts
}

/// One named entry's fork: `here` when it is that entry, else the scan's.
///
/// `here` is the fork the current directory is inside, bound once by the
/// caller; a directory that did not bind is `None`, and how it failed is the
/// caller's to report. The scan's problems ride on the refusal: one of them
/// may be the checkout `name` wanted.
pub fn resolve<'a>(
    registry: &'a Registry,
    name: &RepoName,
    here: Option<Fork<'a>>,
    home: &Path,
) -> Result<Fork<'a>, Unresolved> {
    if registry.get(name).is_none() {
        return Err(Unresolved::Unknown);
    }
    if let Some(fork) = here
        && fork.name == *name
    {
        return Ok(fork);
    }
    let mut scan = scan(registry, home);
    if let Some(fork) = scan.found.remove(name) {
        return Ok(fork);
    }
    let mut why = scan.unplaced(name);
    if let Unresolved::Missing { problems, .. } | Unresolved::Duplicate { problems, .. } = &mut why
    {
        *problems = scan.problems;
    }
    Err(why)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use crate::config::{Registry, RepoEntry};
    use crate::ids::RepoName;

    use super::{
        BindError, Checkout, Fork, Unbound, Unresolved, error_line, remote_host, remote_slug,
        repository_name, same_remote, url_owner,
    };

    fn entry(upstream: &str, origin: &str, release: Option<&str>) -> RepoEntry {
        RepoEntry {
            release: release.map(str::to_owned),
            ..RepoEntry::new(upstream, origin)
        }
    }

    #[test]
    fn the_error_line_skips_a_leading_jj_warning_and_falls_back_to_it_alone() {
        let jj = b"Warning: Per-repo config not found. Generating an empty one.\nInternal error: The repository appears broken or inaccessible\nHint: try again\n";
        assert_eq!(
            error_line(jj),
            "Internal error: The repository appears broken or inaccessible"
        );
        let git = b"fatal: not a git repository: /x/.git\n";
        assert_eq!(error_line(git), "fatal: not a git repository: /x/.git");
        assert_eq!(error_line(b"Warning: only this\n"), "Warning: only this");
        assert_eq!(error_line(b"\n  \n"), "");
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
    fn an_uppercase_git_suffix_is_stripped_like_a_lowercase_one() {
        assert!(same_remote(
            "https://forge.example/Org/Tool.GIT",
            "https://forge.example/org/tool"
        ));
    }

    #[test]
    fn a_port_on_the_host_does_not_make_another_repository() {
        assert!(same_remote(
            "ssh://git@forge.example:2222/org/tool",
            "https://forge.example/org/tool"
        ));
        assert!(same_remote(
            "https://forge.example:443/org/tool.git",
            "git@forge.example:org/tool"
        ));
    }

    #[test]
    fn scp_form_without_a_user_is_a_url_when_the_host_holds_no_slash() {
        assert!(same_remote(
            "forge.example:org/tool",
            "git@forge.example:org/tool.git"
        ));
        assert!(same_remote(
            "forge.example:org/tool",
            "https://forge.example/org/tool"
        ));
        // A colon in a later path component does not turn a directory into a host.
        assert!(!same_remote(
            "/tmp/lab/a:b/tool",
            "https://tmp/lab/a:b/tool"
        ));
        assert!(same_remote("/tmp/lab/a:b/tool", " /tmp/lab/a:b/tool/ "));
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
        assert_eq!(
            remote_host("git@forge.example:org/tool.git"),
            Some("forge.example")
        );
        assert_eq!(remote_host("https:///ours/work.git"), None);
        assert_eq!(remote_host("/tmp/lab/upstream"), None);
        assert_eq!(
            repository_name("https://forge.invalid/someone/Tool.GIT/"),
            Some("Tool")
        );
        assert_eq!(repository_name("https://forge.invalid/someone/.git"), None);
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
        let alpha = entry("u", "o", None);
        let registry = Registry {
            repos: BTreeMap::from([
                ("alpha".to_owned(), alpha.clone()),
                ("beta".to_owned(), alpha),
            ]),
            ..Registry::default()
        };
        assert_eq!(
            Unbound::NotInsideARepository.message(&registry),
            "not inside a repository; name a repo, or run this from inside one; known: alpha, beta"
        );
        assert_eq!(
            Unbound::NoUpstream {
                root: PathBuf::from("/r")
            }
            .message(&registry),
            "/r has no `upstream` remote, so it is not a managed fork; name a repo; known: alpha, beta"
        );
        assert_eq!(
            Unbound::Unregistered {
                root: PathBuf::from("/r"),
                upstream: "https://forge.example/o/t".to_owned()
            }
            .message(&registry),
            "/r forks https://forge.example/o/t, which is not in the registry; `knives register` prints the entry; known: alpha, beta"
        );
        assert_eq!(
            Unbound::GitOnly {
                root: PathBuf::from("/r")
            }
            .message(&registry),
            "/r is a git clone, not a jj checkout; fork commands need jj"
        );
        assert_eq!(
            Unbound::Unreadable(BindError::RemotesUnreadable {
                root: PathBuf::from("/r"),
                detail: "boom".to_owned()
            })
            .message(&registry),
            "reading remotes of /r: boom"
        );
        let name = RepoName::new("tool");
        assert_eq!(
            Unresolved::Unknown.message(&name, &registry),
            "unknown repo tool; known: alpha, beta"
        );
        assert_eq!(
            Unresolved::Missing {
                home: PathBuf::from("/home/x"),
                problems: Vec::new(),
            }
            .message(&name, &registry),
            "no checkout of tool under /home/x"
        );
        assert_eq!(
            Unresolved::Duplicate {
                home: PathBuf::from("/home/x"),
                paths: vec![PathBuf::from("/home/x/a"), PathBuf::from("/home/x/b")],
                problems: Vec::new(),
            }
            .message(&name, &registry),
            "tool has 2 checkouts under /home/x: /home/x/a, /home/x/b; knives will not choose"
        );
    }

    #[test]
    fn a_refusal_carries_the_scan_problems_after_its_text() {
        // A checkout whose remotes could not be read may be the one the entry
        // wanted, so `resolve` names it rather than dropping it.
        let why = Unresolved::Missing {
            home: PathBuf::from("/home/x"),
            problems: vec![
                "reading remotes of /home/x/broken: boom".to_owned(),
                "cannot read /home/x/locked: denied".to_owned(),
            ],
        };
        assert_eq!(
            why.message(&RepoName::new("ghost"), &Registry::default()),
            "no checkout of ghost under /home/x; could not read: reading remotes of \
             /home/x/broken: boom; could not read: cannot read /home/x/locked: denied"
        );
        let twice = Unresolved::Duplicate {
            home: PathBuf::from("/home/x"),
            paths: vec![PathBuf::from("/home/x/a"), PathBuf::from("/home/x/b")],
            problems: vec!["reading remotes of /home/x/broken: boom".to_owned()],
        };
        assert_eq!(
            twice.message(&RepoName::new("twice"), &Registry::default()),
            "twice has 2 checkouts under /home/x: /home/x/a, /home/x/b; knives will not \
             choose; could not read: reading remotes of /home/x/broken: boom"
        );
    }
}
