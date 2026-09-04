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
use crate::remote_url::same_remote;

/// A repository root on this machine and the remotes it declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    /// The checkout root: the directory holding the `.git` directory. A
    /// workspace resolves to its checkout, never to itself.
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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindError {
    #[error("reading remotes of {}: {detail}", root.display())]
    RemotesUnreadable { root: PathBuf, detail: String },
}

/// Why `here` did not bind.
#[derive(Debug, PartialEq, Eq)]
pub enum Unbound {
    /// No `.git` at or above the directory.
    NotInsideARepository,
    /// A git clone with no `.jj`: the hook binds those, fork verbs need jj.
    GitOnly { root: PathBuf },
    /// A `.jj` with no `.git` beside it: a non-colocated checkout, which knives
    /// cannot read through git, or a `.jj` some tree carries as content.
    NotColocated { root: PathBuf },
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
            Self::NotColocated { root } => {
                return format!(
                    "{} has a .jj but no .git; knives reads a checkout through git, so it must \
                     be colocated",
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

/// The first ancestor of `path` (canonicalised) holding `.git`, as a directory
/// or a worktree's pointer file.
///
/// `.git` is the one marker a clone cannot deliver: git refuses to check out a
/// path component of that name whatever its type, so it is always the
/// repository's own. A `.jj` marks nothing here — a store, a pointer, a symlink
/// named `.jj` are all content a tree can carry. Nearest wins, so a clone
/// nested inside a checkout is its own root and never inherits the enclosing
/// identity; a jj workspace of a colocated checkout carries a `.git` file and
/// is its own root too.
pub fn nearest_root(path: &Path) -> Option<PathBuf> {
    let start = path.canonicalize().ok()?;
    start
        .ancestors()
        .find(|directory| directory.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Whether `root` holds a real `.jj` directory: what makes a git repository a
/// jj checkout knives manages. A symlink named `.jj` is content, not a marker.
pub fn has_jj_directory(root: &Path) -> bool {
    std::fs::symlink_metadata(root.join(".jj")).is_ok_and(|metadata| metadata.file_type().is_dir())
}

/// The checkout `path` belongs to: [`nearest_root`], then [`checkout_of_root`].
pub fn checkout_root(path: &Path) -> Option<PathBuf> {
    nearest_root(path).map(|root| checkout_of_root(&root))
}

/// The checkout a repository root belongs to.
///
/// The root itself when `.git` is its own directory; else — `.git` a file, a
/// linked worktree such as a jj workspace of a colocated checkout — the
/// directory holding the common git directory `git rev-parse --git-common-dir`
/// reports. This is where a fork verb operates and beside what workspaces are
/// placed; git resolves the worktree pointer itself, and nothing under `.jj`
/// is read. A worktree git cannot answer for stays its own root, so
/// [`remotes`] reports git's error.
pub fn checkout_of_root(root: &Path) -> PathBuf {
    if root.join(".git").is_dir() {
        return root.to_owned();
    }
    git(root)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
        // A linked worktree's common dir is the checkout's `.git`; a submodule's
        // is `<super>/.git/modules/<name>`, and the submodule is its own root.
        .filter(|common| common.file_name().is_some_and(|name| name == ".git"))
        .and_then(|common| common.parent().map(Path::to_path_buf))
        .and_then(|checkout| checkout.canonicalize().ok())
        .unwrap_or_else(|| root.to_owned())
}

/// A `git` invocation that reads only the repository it is pointed at.
///
/// git hooks, `rebase -x`, `bisect run`, and some editors export `GIT_DIR` and
/// its companions, and `git -c` exports `GIT_CONFIG_PARAMETERS` to every
/// subprocess; inherited, they would make a root report another repository's
/// configuration, or configuration that lives in no repository at all.
fn git_command() -> std::process::Command {
    let mut command = std::process::Command::new("git");
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
    ] {
        command.env_remove(variable);
    }
    for (name, _) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"GIT_CONFIG_") {
            command.env_remove(name);
        }
    }
    command
}

/// [`git_command`] run in `directory` (`git -C`), forbidden from discovering a
/// repository above it: a `.git` git cannot open (empty, half-initialised) would
/// otherwise let discovery continue to a parent repository and answer for it.
pub(crate) fn git(directory: &Path) -> std::process::Command {
    let mut command = git_command();
    if let Some(parent) = directory.parent() {
        command.env("GIT_CEILING_DIRECTORIES", parent);
    }
    command.arg("-C").arg(directory);
    command
}

/// Remotes of the repository rooted at `root`, from its own git configuration.
///
/// `git -C root config --local --get-regexp '^remote\..*\.url$'`: the
/// repository's own configuration file and nothing else — not the user's, not
/// the system's, not the environment's. For a linked worktree that is the
/// common repository's file, so a jj workspace of a colocated checkout reports
/// the checkout's remotes. A root with no `.git` is not a repository knives
/// reads.
pub fn remotes(root: &Path) -> Result<BTreeMap<String, String>, BindError> {
    let failure = |detail: String| BindError::RemotesUnreadable {
        root: root.to_owned(),
        detail,
    };
    let output = git(root)
        .args([
            "config",
            "--local",
            "-z",
            "--get-regexp",
            "^remote\\..*\\.url$",
        ])
        .output()
        .map_err(|error| failure(error.to_string()))?;
    // git exits 1 with empty output when nothing matches: no remotes, not an error.
    let no_matches =
        output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty();
    if !output.status.success() && !no_matches {
        return Err(failure(error_line(&output.stderr)));
    }
    String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|record| !record.is_empty())
        .map(|record| {
            // With `-z`, git prints `remote.<name>.url\n<url>` per NUL-terminated record.
            record
                .split_once('\n')
                .and_then(|(key, url)| {
                    let name = key.strip_prefix("remote.")?.strip_suffix(".url")?;
                    Some((name.to_owned(), url.trim().to_owned()))
                })
                .ok_or_else(|| failure(format!("unparseable remote record {record:?}")))
        })
        .collect()
}

/// The line of git's stderr that explains a failure: the first that is not a
/// `warning:`, which git prints ahead of some errors, else the first line.
fn error_line(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let lines = || {
        stderr
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
    };
    lines()
        .find(|line| !line.to_ascii_lowercase().starts_with("warning:"))
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

/// The colocated jj checkout a fork verb run in `cwd` operates on.
///
/// Or why there is none: a `.jj` nearer than any `.git` (a non-colocated
/// checkout, or a `.jj` some tree carries), no `.git` above at all, or a
/// `.git` with no `.jj` beside it (a plain git clone).
pub fn verb_checkout(cwd: &Path) -> Result<PathBuf, Unbound> {
    if let Some(root) = jj_only_ancestor(cwd) {
        return Err(Unbound::NotColocated { root });
    }
    let root = checkout_root(cwd).ok_or(Unbound::NotInsideARepository)?;
    if !has_jj_directory(&root) {
        return Err(Unbound::GitOnly { root });
    }
    Ok(root)
}

/// The nearest ancestor of `path` (canonicalised) holding a real `.jj`
/// directory but no `.git`, stopping at the nearest `.git`: a non-colocated
/// checkout the directory is inside, or a `.jj` some tree carries as content.
fn jj_only_ancestor(path: &Path) -> Option<PathBuf> {
    let start = path.canonicalize().ok()?;
    start
        .ancestors()
        .take_while(|directory| !directory.join(".git").exists())
        .find(|directory| has_jj_directory(directory))
        .map(Path::to_path_buf)
}

/// The fork the current directory is inside.
///
/// The checkout its nearest root belongs to ([`verb_checkout`]), bound by the
/// remotes read there. A verb ran in that directory, so its checkout's remotes
/// are what the verb is about — no trust decision rests on them.
pub fn here<'a>(registry: &'a Registry, cwd: &Path) -> Result<Fork<'a>, Unbound> {
    let root = verb_checkout(cwd)?;
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
    /// Why `name` is not in `found`: two checkouts, or none — carrying
    /// `problems`, the scan complaints the caller wants on this refusal. A
    /// named verb passes the scan's, since one of them may be the checkout it
    /// wanted; a sweep passes none and reports them once itself.
    pub fn unplaced(&self, name: &RepoName, problems: Vec<String>) -> Unresolved {
        match self.duplicates.get(name) {
            Some(paths) => Unresolved::Duplicate {
                home: self.home.clone(),
                paths: paths.clone(),
                problems,
            },
            None => Unresolved::Missing {
                home: self.home.clone(),
                problems,
            },
        }
    }
}

/// `home` is depth 0; `~/a/b/c` is read and its children are not queued.
const SCAN_DEPTH: usize = 3;

/// Scan `home` for colocated jj checkouts ([`checkouts_under`]) and bind each
/// to its entry.
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

/// The colocated jj checkouts under `home`, to depth three: directories holding
/// a `.git` directory and a real `.jj` directory ([`has_jj_directory`]).
///
/// Directories named with a leading `.` are skipped, symlinks are not followed,
/// and nothing below a real `.jj` is visited — except `home` itself, whose
/// children are always visited: a home that is a repository (a dotfiles
/// checkout, say) still holds the forks under it. A jj workspace carries a
/// `.git` *file* and is found through its checkout, not as a candidate. A
/// `.jj` with no `.git` is passed over in silence: content some tree carries,
/// or a checkout knives does not read. A git-only clone is not a fork checkout,
/// and a git-tracked parent (`~/work/.git`) does not hide the forks beneath
/// it. A directory that could not be listed is pushed to `problems`.
fn checkouts_under(home: &Path, problems: &mut Vec<String>) -> Vec<PathBuf> {
    let mut checkouts = Vec::new();
    let mut pending = vec![(home.to_owned(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        let is_jj = has_jj_directory(&directory);
        if is_jj && directory.join(".git").is_dir() {
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
    here: Option<&Fork<'a>>,
    home: &Path,
) -> Result<Fork<'a>, Unresolved> {
    if registry.get(name).is_none() {
        return Err(Unresolved::Unknown);
    }
    if let Some(fork) = here
        && fork.name == *name
    {
        return Ok(fork.clone());
    }
    let mut scan = scan(registry, home);
    if let Some(fork) = scan.found.remove(name) {
        return Ok(fork);
    }
    let problems = std::mem::take(&mut scan.problems);
    Err(scan.unplaced(name, problems))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use crate::config::{Registry, RepoEntry};
    use crate::ids::RepoName;

    use super::{
        BindError, Checkout, Fork, Unbound, Unresolved, error_line, has_jj_directory,
        jj_only_ancestor, nearest_root,
    };

    #[test]
    fn the_nearest_root_is_the_nearest_git_and_a_jj_alone_marks_nothing() {
        // `outer/.git` encloses `outer/store/.jj` (a `.jj`-only directory) and
        // `outer/store/deep`; the root of both is `outer`.
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("outer");
        let store = outer.join("store");
        let deep = store.join("deep");
        std::fs::create_dir_all(outer.join(".git")).unwrap();
        std::fs::create_dir_all(store.join(".jj")).unwrap();
        std::fs::create_dir_all(&deep).unwrap();
        let canonical_outer = outer.canonicalize().unwrap();
        assert_eq!(nearest_root(&deep), Some(canonical_outer.clone()));
        assert_eq!(nearest_root(&store), Some(canonical_outer));
        // The `.jj`-only directory is what a fork verb run inside it is refused as.
        assert_eq!(jj_only_ancestor(&deep), Some(store.canonicalize().unwrap()));
        assert_eq!(jj_only_ancestor(&outer), None);
        // No `.git` anywhere: no root at all.
        let alone = dir.path().join("alone");
        std::fs::create_dir_all(alone.join(".jj")).unwrap();
        assert_eq!(nearest_root(&alone), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_named_jj_is_not_a_checkout_marker() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real-store");
        std::fs::create_dir_all(real.join("repo")).unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::os::unix::fs::symlink(&real, root.join(".jj")).unwrap();
        assert!(
            root.join(".jj").is_dir(),
            "the symlink resolves to a directory"
        );
        assert!(!has_jj_directory(&root));
        assert_eq!(jj_only_ancestor(&root), None);
        let genuine = dir.path().join("genuine");
        std::fs::create_dir_all(genuine.join(".jj")).unwrap();
        assert!(has_jj_directory(&genuine));
    }

    fn entry(upstream: &str, origin: &str, release: Option<&str>) -> RepoEntry {
        RepoEntry {
            release: release.map(str::to_owned),
            ..RepoEntry::new(upstream, origin)
        }
    }

    #[test]
    fn the_error_line_skips_a_leading_warning_and_falls_back_to_it_alone() {
        let warned = b"warning: unable to access '/x/.gitconfig': Permission denied\nfatal: not a git repository: /x/.git\n";
        assert_eq!(error_line(warned), "fatal: not a git repository: /x/.git");
        let plain = b"fatal: not a git repository: /x/.git\n";
        assert_eq!(error_line(plain), "fatal: not a git repository: /x/.git");
        assert_eq!(error_line(b"warning: only this\n"), "warning: only this");
        assert_eq!(error_line(b"\n  \n"), "");
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
            Unbound::NotColocated {
                root: PathBuf::from("/r")
            }
            .message(&registry),
            "/r has a .jj but no .git; knives reads a checkout through git, so it must be \
             colocated"
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
