//! Ledger entries between machines, through git, without git's index or a
//! checkout.
//!
//! The ledger root, one `<owner>/<name>` directory per fork, is the working
//! tree of one or more git repositories that git never stages into, checks
//! out, or rewrites. Each [`Repository`] carries the forks it names and no
//! others: an entry is about one fork, and the fork decides which remote may
//! see it. Each machine commits the entries it wrote to its own ref,
//! `refs/knives/<machine>`; a fetch copies every machine's ref to
//! `refs/knives-remotes/<remote>/<machine>`; materialising writes the entries
//! the directory lacks.
//!
//! Only plumbing runs here. A repository has one `.git/index` and one lock on
//! it, so staging entries with `git add` makes concurrent writers contend for
//! that lock and fail on it, when the ledger is one file per entry precisely so
//! that no two writers share anything. `hash-object`, `mktree`, `commit-tree`
//! and `update-ref` write objects and one ref, never an index, and so never
//! consult an exclude file either: which entries a repository carries is its
//! fork list and nothing else. A machine moves only its own ref, and only
//! forward, so its push is a fast-forward nothing else contends for, and
//! nothing is ever rebased.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::ids::{CommitId, UpstreamName};

/// Each machine's own ref: `refs/knives/<machine>`.
const OWN: &str = "refs/knives/";
/// Where a fetch leaves every machine's ref: `refs/knives-remotes/<remote>/<machine>`.
const FETCHED: &str = "refs/knives-remotes/";

/// One machine's ref as a fetch found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineRef {
    pub machine: String,
    pub commit: CommitId,
}

/// One git repository over the ledger root, and the forks it carries.
///
/// Its git directory is named outright rather than discovered from the
/// working tree, so a second repository over the same root (a git directory
/// elsewhere whose working tree is that root) is reached exactly as the first
/// is. Entries are committed from, and materialised into, only the
/// directories of the forks it names; a repository naming no fork carries
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    git_dir: PathBuf,
    work_tree: PathBuf,
    forks: BTreeSet<UpstreamName>,
}

impl Repository {
    /// The repository at `git_dir` over the ledger root `work_tree`, carrying
    /// `forks`. A fork name that is not exactly two directory names in the
    /// root, `<owner>/<name>`, is refused: it would reach entries outside
    /// that fork's directory, or inside another fork's.
    pub fn new(
        git_dir: &Path,
        work_tree: &Path,
        forks: impl IntoIterator<Item = UpstreamName>,
    ) -> Result<Self, GitError> {
        let absolute = |path: &Path| {
            std::path::absolute(path).map_err(|source| GitError::Read {
                path: path.to_owned(),
                source,
            })
        };
        Ok(Self {
            git_dir: absolute(git_dir)?,
            work_tree: absolute(work_tree)?,
            forks: forks
                .into_iter()
                .map(fork_directory)
                .collect::<Result<_, _>>()?,
        })
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    pub fn work_tree(&self) -> &Path {
        &self.work_tree
    }

    pub const fn forks(&self) -> &BTreeSet<UpstreamName> {
        &self.forks
    }

    /// Whether the entry at `path`, relative to the root, lies in the
    /// `<owner>/<name>` directory of a fork this repository carries.
    fn carries_entry(&self, path: &Path) -> bool {
        let mut components = path.components();
        match (components.next(), components.next(), components.next()) {
            (
                Some(Component::Normal(owner)),
                Some(Component::Normal(name)),
                Some(Component::Normal(_)),
            ) => self
                .forks
                .iter()
                .any(|carried| Path::new(carried.as_str()) == Path::new(owner).join(name)),
            _ => false,
        }
    }

    /// `git` on this repository, run in the ledger root.
    ///
    /// `--git-dir` and `--work-tree` leave nothing to discovery. No terminal
    /// prompt: a ledger command has nobody to answer one, whether it is a
    /// sweep running detached or a pull a report is waiting on, and a
    /// credential prompt would hold the ledger's lock until someone noticed.
    fn git(&self) -> Command {
        let mut command = crate::bind::git_command();
        command
            .arg("-C")
            .arg(&self.work_tree)
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.work_tree)
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error(
        "refusing {role} name {name:?}: it must be one git ref name component, with no `/` and no leading `-`"
    )]
    Name { role: &'static str, name: String },
    #[error(
        "refusing fork name {name:?}: it must be two directory names in the ledger root, \
         <owner>/<name>, neither of them .git"
    )]
    Fork { name: String },
    #[error("running {invocation}: {source}")]
    Run {
        invocation: String,
        source: std::io::Error,
    },
    #[error("{invocation} failed: {stderr}")]
    Failed { invocation: String, stderr: String },
    #[error("{invocation} did not answer with {expected}")]
    Output {
        invocation: String,
        expected: &'static str,
    },
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("commit {commit} carries {path:?}, which is not a ledger entry: {detail}")]
    NotAnEntry {
        commit: CommitId,
        path: PathBuf,
        detail: &'static str,
    },
    #[error("{path:?} is a file on disk and a directory in this machine's ref, or the reverse")]
    Shape { path: PathBuf },
    #[error(
        "refusing to write as {machine}: {remote} has its ref but this checkout never committed as it, so the name is another machine's"
    )]
    NotOwn { machine: String, remote: String },
}

/// Commit every entry in the forks `repository` carries that no knives ref
/// carries yet to `refs/knives/<machine>`, as a child of the commit that ref
/// holds now.
///
/// An entry is a `*.md` regular file anywhere under a carried fork's
/// directory outside `.git`, the files [`crate::ledger::Ledger::entries`]
/// reads; no other fork's directory is read, however the root is shared.
/// "Carried" counts every ref under `refs/knives/` and `refs/knives-remotes/`,
/// so an entry materialised from a peer is not committed again as this
/// machine's own. `Ok(None)` when nothing is new, and the ref is then
/// untouched.
///
/// A remote that has `machine`'s ref when this checkout has none means the
/// name is another machine's, or this checkout was recreated; either way a
/// commit here would start a second history that the push then rejects, so
/// the call refuses with [`GitError::NotOwn`] instead, as [`push`] does.
///
/// The ref moves by compare-and-swap against the value read at the start, so
/// a second writer for the same machine racing this one fails loudly rather
/// than lose the other's commit.
pub fn commit_new_entries(
    repository: &Repository,
    machine: &str,
) -> Result<Option<CommitId>, GitError> {
    let Pending {
        reference,
        previous,
        base,
        added,
    } = pending(repository, machine)?;
    if added.is_empty() {
        return Ok(None);
    }
    let blobs = hash_objects(repository, &added)?;
    let tree = write_tree(repository, base, added.into_iter().zip(blobs))?;
    let commit = commit_tree(repository, machine, &tree, previous.as_ref())?;
    run(repository.git().args([
        "update-ref",
        &reference,
        commit.as_str(),
        previous.as_ref().map_or("", CommitId::as_str),
    ]))?;
    Ok(Some(commit))
}

/// Whether [`commit_new_entries`] would commit anything now. It only reads,
/// so it can run while another process holds the ledger's lock and commits.
pub fn has_new_entries(repository: &Repository, machine: &str) -> Result<bool, GitError> {
    Ok(!pending(repository, machine)?.added.is_empty())
}

/// What a commit as one machine would start from and add.
struct Pending {
    reference: String,
    previous: Option<CommitId>,
    base: Vec<TreeItem>,
    added: BTreeSet<PathBuf>,
}

fn pending(repository: &Repository, machine: &str) -> Result<Pending, GitError> {
    let reference = own_ref(machine)?;
    let mut previous = None;
    let mut base = Vec::new();
    let mut carried = BTreeSet::new();
    let mut fetched_copy = None;
    for (name, commit) in references(repository, &[OWN, FETCHED])? {
        if let Some((remote, fetched)) = name
            .strip_prefix(FETCHED)
            .and_then(|rest| rest.split_once('/'))
            && fetched == machine
        {
            fetched_copy.get_or_insert_with(|| remote.to_owned());
        }
        let items = tree_items(repository, &commit)?;
        if name == reference {
            carried.extend(
                items
                    .iter()
                    .filter(|item| item.node.is_blob())
                    .map(|item| item.path.clone()),
            );
            base = items;
            previous = Some(commit);
        } else {
            carried.extend(
                items
                    .into_iter()
                    .filter(|item| item.node.is_blob())
                    .map(|item| item.path),
            );
        }
    }
    if previous.is_none()
        && let Some(remote) = fetched_copy
    {
        return Err(GitError::NotOwn {
            machine: machine.to_owned(),
            remote,
        });
    }
    let mut added = local_entries(repository)?;
    added.retain(|path| !carried.contains(path));
    Ok(Pending {
        reference,
        previous,
        base,
        added,
    })
}

/// Fetch every machine's ref from `remote` into `refs/knives-remotes/<remote>/`
/// and report each by machine name.
///
/// `remote` is a configured remote's name, because it names the namespace the
/// copies live in. The refspec is not forced: a machine only ever moves its own
/// ref forward, so a copy that would move backward means some writer rewrote
/// history, and the fetch fails rather than follow it. `--prune` drops the copy
/// of a ref the remote no longer has, so the report is the remote's refs now.
pub fn fetch(repository: &Repository, remote: &str) -> Result<Vec<MachineRef>, GitError> {
    let remote = ref_component("remote", remote)?;
    let namespace = format!("{FETCHED}{remote}/");
    run(repository.git().args([
        "fetch",
        "--quiet",
        "--no-tags",
        "--no-write-fetch-head",
        "--prune",
        remote,
        &format!("{OWN}*:{namespace}*"),
    ]))?;
    Ok(references(repository, &[&namespace])?
        .into_iter()
        .filter_map(|(name, commit)| {
            Some(MachineRef {
                machine: name.strip_prefix(&namespace)?.to_owned(),
                commit,
            })
        })
        .collect())
}

/// Push `refs/knives/<machine>` to the same name on `remote`, and nothing else.
///
/// This is the one place a ref leaves the machine, so it is where "a machine
/// writes only its own ref" holds. The refspec is spelled from the machine name
/// alone and never forced, and a name that could spell any other ref (a glob,
/// a second refspec, a nested path) is refused before git runs. Only
/// [`commit_new_entries`] writes under `refs/knives/`, so
/// `refs/knives/<machine>` exists here only if this checkout committed as that
/// machine. A machine `remote` has but this checkout never committed as is a
/// peer, and pushing it is refused; a machine neither side has has nothing to
/// send, and the call succeeds having sent nothing. git rejecting the push
/// means some other writer moved this machine's ref on `remote`.
///
/// Once `remote` has taken the commit, it is recorded as `remote`'s copy,
/// where [`fetch`] leaves copies, so [`unpushed`] knows it was sent without
/// asking the remote again.
pub fn push(repository: &Repository, remote: &str, machine: &str) -> Result<(), GitError> {
    let reference = own_ref(machine)?;
    let remote = ref_component("remote", remote)?;
    let fetched = format!("{FETCHED}{remote}/{machine}");
    let found = references(repository, &[&reference, &fetched])?;
    let Some((_, commit)) = found.iter().find(|(name, _)| *name == reference) else {
        return if found.iter().any(|(name, _)| *name == fetched) {
            Err(GitError::NotOwn {
                machine: machine.to_owned(),
                remote: remote.to_owned(),
            })
        } else {
            Ok(())
        };
    };
    run(repository.git().args([
        "push",
        "--quiet",
        "--no-follow-tags",
        remote,
        &format!("{reference}:{reference}"),
    ]))?;
    run(repository
        .git()
        .args(["update-ref", &fetched, commit.as_str()]))?;
    Ok(())
}

/// Whether `refs/knives/<machine>` holds a commit `remote` was not last seen
/// holding, by a [`fetch`] or by this machine's own [`push`]: what a sweep
/// with nothing new to commit still has to send.
pub fn unpushed(repository: &Repository, remote: &str, machine: &str) -> Result<bool, GitError> {
    let reference = own_ref(machine)?;
    let remote = ref_component("remote", remote)?;
    let fetched = format!("{FETCHED}{remote}/{machine}");
    let found = references(repository, &[&reference, &fetched])?;
    let at = |wanted: &str| {
        found
            .iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, commit)| commit)
    };
    Ok(at(&reference).is_some_and(|own| at(&fetched) != Some(own)))
}

/// Every entry in the forks `repository` carries that `remote` lacks.
///
/// That is every entry written here that no copy of `remote`'s refs carries,
/// committed or not. The copies are what the last [`fetch`] or this machine's
/// own [`push`] saw, so the answer is as current as the last pull.
pub fn unsent(repository: &Repository, remote: &str) -> Result<BTreeSet<PathBuf>, GitError> {
    let remote = ref_component("remote", remote)?;
    let namespace = format!("{FETCHED}{remote}/");
    let mut sent = BTreeSet::new();
    for (_, commit) in references(repository, &[&namespace])? {
        sent.extend(
            tree_items(repository, &commit)?
                .into_iter()
                .filter(|item| item.node.is_blob())
                .map(|item| item.path),
        );
    }
    let mut local = local_entries(repository)?;
    local.retain(|path| !sent.contains(path));
    Ok(local)
}

/// Every key of the repository at `git_dir`'s own config that `pattern`, a
/// `git config --get-regexp` pattern, matches.
///
/// Lowercased as git prints it (a remote's name excepted), with its value, in
/// file order: a key set twice appears twice. A key with no value at all
/// reads as empty. Values are as written: a `url.<base>.insteadOf` rewrites
/// where git fetches, never what a remote's `url` says it is.
pub fn local_config(git_dir: &Path, pattern: &str) -> Result<Vec<(String, String)>, GitError> {
    let mut command = crate::bind::git_command();
    command.arg("--git-dir").arg(git_dir).args([
        "config",
        "--local",
        "-z",
        "--get-regexp",
        pattern,
    ]);
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|source| GitError::Run {
            invocation: invocation(&command),
            source,
        })?;
    // `--get-regexp` exits 1, printing nothing, when no key matches.
    if output.status.code() == Some(1) && output.stdout.is_empty() {
        return Ok(Vec::new());
    }
    let answer = succeeded(&command, output)?;
    answer
        .split(|&byte| byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            let text = std::str::from_utf8(record)
                .map_err(|_| output_error(&command, "UTF-8 `<key>\\n<value>` records"))?;
            let (key, value) = text.split_once('\n').unwrap_or((text, ""));
            Ok((key.to_owned(), value.to_owned()))
        })
        .collect()
}

/// Write every entry `refs` carry, in a fork `repository` carries, that the
/// ledger root lacks, returning how many this call wrote.
///
/// Each entry is written whole to a temporary name and renamed into place,
/// never over a file already there; see [`write_entry`]. A tree item that is
/// not an entry file, or whose path would leave the root or enter a `.git`,
/// fails the whole call before anything is written, whichever fork it names:
/// `mktree` accepts names like `..` and `.git`, so a peer's ref must be
/// checked before it reaches the filesystem. An entry in a fork this
/// repository does not carry is left out: it came through a remote that fork
/// does not belong to, and written into the shared root it would be committed
/// again by the repository that does carry the fork.
pub fn materialise(repository: &Repository, refs: &[MachineRef]) -> Result<usize, GitError> {
    let mut missing = BTreeMap::new();
    for machine_ref in refs {
        for item in tree_items(repository, &machine_ref.commit)? {
            if item.node.is_tree() {
                continue;
            }
            check_entry(&machine_ref.commit, &item)?;
            if !repository.carries_entry(&item.path) || missing.contains_key(&item.path) {
                continue;
            }
            let path = repository.work_tree.join(&item.path);
            let present = path.try_exists().map_err(|source| GitError::Read {
                path: path.clone(),
                source,
            })?;
            if !present {
                missing.insert(item.path, item.node.oid);
            }
        }
    }
    if missing.is_empty() {
        return Ok(0);
    }
    let mut input = Vec::new();
    for oid in missing.values() {
        input.extend_from_slice(oid.as_bytes());
        input.push(b'\n');
    }
    let mut command = repository.git();
    command.args(["cat-file", "--batch"]);
    let answer = run_with_input(&mut command, &input)?;
    let blobs = batch_blobs(&answer)
        .filter(|blobs| blobs.len() == missing.len())
        .ok_or_else(|| output_error(&command, "each requested blob, in order"))?;
    let mut written = 0;
    for (relative, content) in missing.keys().zip(blobs) {
        if write_entry(&repository.work_tree.join(relative), content)? {
            written += 1;
        }
    }
    Ok(written)
}

/// A name in a tree, as `ls-tree` prints it and `mktree` reads it.
#[derive(Debug)]
struct Node {
    mode: String,
    kind: String,
    oid: String,
}

impl Node {
    fn is_blob(&self) -> bool {
        self.kind == "blob"
    }

    fn is_tree(&self) -> bool {
        self.kind == "tree"
    }
}

/// One record of a recursive `ls-tree`: a node and its path from the root.
#[derive(Debug)]
struct TreeItem {
    path: PathBuf,
    node: Node,
}

/// Every directory of a tree being written, by path from the root (`""`
/// is the root), with the nodes it holds by name.
type Directories = BTreeMap<PathBuf, BTreeMap<OsString, Node>>;

/// `refs/knives/<machine>`, for a machine name that spells exactly that ref.
fn own_ref(machine: &str) -> Result<String, GitError> {
    Ok(format!("{OWN}{}", ref_component("machine", machine)?))
}

/// `name` when it can stand as exactly one component of a ref name.
///
/// A machine's ref and a remote's fetched namespace are spelled from these, so
/// a name that is not one component addresses some other ref: `*` or a `:` in
/// a refspec reaches a peer's ref, and a `/` nests under one. A leading `-` is
/// refused so a remote cannot read as an option to `git`. A `.lock` suffix is
/// refused in any case, because a loose ref on a case-insensitive filesystem
/// would collide with git's lock file for it.
fn ref_component<'a>(role: &'static str, name: &'a str) -> Result<&'a str, GitError> {
    let malformed = name.is_empty()
        || name == "@"
        || name.starts_with(['.', '-'])
        || name.ends_with('.')
        || Path::new(name)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("lock"))
        || name.contains("..")
        || name.contains("@{")
        || name
            .chars()
            .any(|character| character.is_ascii_control() || " ~^:?*[\\/".contains(character));
    if malformed {
        Err(GitError::Name {
            role,
            name: name.to_owned(),
        })
    } else {
        Ok(name)
    }
}

/// `fork` when it names exactly two directories in the ledger root,
/// `<owner>/<name>` as [`crate::config::RepoEntry::upstream_name`] spells a
/// fork's repository, neither of them `.`, `..` or a `.git`.
///
/// A fork whose upstream is a filesystem path is kept under its one-component
/// registry key, and names no repository a destination could share, so a
/// name of one component is refused here as surely as one of three.
fn fork_directory(fork: UpstreamName) -> Result<UpstreamName, GitError> {
    let plain = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && !part.contains('\\')
            && !part.eq_ignore_ascii_case(".git")
    };
    let two = fork
        .as_str()
        .split_once('/')
        .is_some_and(|(owner, name)| plain(owner) && plain(name) && !name.contains('/'));
    if two {
        Ok(fork)
    } else {
        Err(GitError::Fork {
            name: fork.to_string(),
        })
    }
}

/// Every `*.md` regular file under the directories of the forks `repository`
/// carries, relative to the root, with every `.git` skipped. A fork with no
/// directory yet has no entries.
fn local_entries(repository: &Repository) -> Result<BTreeSet<PathBuf>, GitError> {
    let mut found = BTreeSet::new();
    let mut pending: Vec<PathBuf> = repository
        .forks
        .iter()
        .map(|fork| PathBuf::from(fork.as_str()))
        .collect();
    while let Some(relative) = pending.pop() {
        let directory = repository.work_tree.join(&relative);
        let unreadable = |source| GitError::Read {
            path: directory.clone(),
            source,
        };
        let listing = match std::fs::read_dir(&directory) {
            Ok(listing) => listing,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(unreadable(source)),
        };
        for dirent in listing {
            let dirent = dirent.map_err(unreadable)?;
            let name = dirent.file_name();
            let kind = dirent.file_type().map_err(unreadable)?;
            if kind.is_dir() && name != ".git" {
                pending.push(relative.join(name));
            } else if kind.is_file() && Path::new(&name).extension() == Some(OsStr::new("md")) {
                found.insert(relative.join(name));
            }
        }
    }
    Ok(found)
}

/// Write each file at `paths` (relative to `repo_dir`) into the object store,
/// returning the blob ids in the same order.
///
/// `--no-filters`, so a blob is the file's exact bytes whatever
/// `.gitattributes` says, and a peer materialises it byte for byte.
fn hash_objects(
    repository: &Repository,
    paths: &BTreeSet<PathBuf>,
) -> Result<Vec<String>, GitError> {
    let mut input = Vec::new();
    for path in paths {
        push_quoted(&mut input, path);
        input.push(b'\n');
    }
    let mut command = repository.git();
    command.args(["hash-object", "-w", "--no-filters", "--stdin-paths"]);
    let answer = run_with_input(&mut command, &input)?;
    let ids = object_ids(&command, &answer)?;
    if ids.len() == paths.len() {
        Ok(ids)
    } else {
        Err(output_error(&command, "one blob id per path"))
    }
}

/// `path` C-quoted, the form `--stdin-paths` reads back exactly: otherwise a
/// name holding a newline would end its line early, and one opening with `"`
/// would be unquoted as if it were quoted.
fn push_quoted(out: &mut Vec<u8>, path: &Path) {
    out.push(b'"');
    for &byte in path.as_os_str().as_bytes() {
        match byte {
            b'"' | b'\\' => out.extend_from_slice(&[b'\\', byte]),
            0..0x20 | 0x7f => out.extend_from_slice(&[
                b'\\',
                b'0' + (byte >> 6),
                b'0' + ((byte >> 3) & 7),
                b'0' + (byte & 7),
            ]),
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}

/// The id of the tree that is `base` plus a blob at each `added` path.
///
/// Only the directories that gain a blob, and their ancestors, are written
/// afresh; every other subtree keeps the id `base` lists, so a commit costs one
/// `mktree` per directory it touches rather than one per directory the ledger
/// has.
fn write_tree(
    repository: &Repository,
    base: Vec<TreeItem>,
    added: impl Iterator<Item = (PathBuf, String)>,
) -> Result<String, GitError> {
    let mut directories = Directories::new();
    for item in base {
        if let Some((parent, name)) = split(&item.path) {
            directories
                .entry(parent)
                .or_default()
                .insert(name, item.node);
        }
    }
    let mut dirty = BTreeSet::from([PathBuf::new()]);
    for (path, oid) in added {
        let Some((mut directory, name)) = split(&path) else {
            continue;
        };
        let blob = Node {
            mode: "100644".to_owned(),
            kind: "blob".to_owned(),
            oid,
        };
        let replaced = directories
            .entry(directory.clone())
            .or_default()
            .insert(name, blob);
        if replaced.is_some_and(|node| node.is_tree()) {
            return Err(GitError::Shape { path });
        }
        while let Some((parent, name)) = split(&directory) {
            if !dirty.insert(directory.clone()) {
                break;
            }
            let node = directories
                .entry(parent.clone())
                .or_default()
                .entry(name)
                .or_insert_with(|| Node {
                    mode: "040000".to_owned(),
                    kind: "tree".to_owned(),
                    oid: String::new(),
                });
            if !node.is_tree() {
                return Err(GitError::Shape { path: directory });
            }
            directory = parent;
        }
    }
    write_directory(repository, &directories, &dirty, Path::new(""))
}

/// Write `directory`'s tree, writing each subtree in `dirty` first.
fn write_directory(
    repository: &Repository,
    directories: &Directories,
    dirty: &BTreeSet<PathBuf>,
    directory: &Path,
) -> Result<String, GitError> {
    let mut input = Vec::new();
    for (name, node) in directories.get(directory).into_iter().flatten() {
        let path = directory.join(name);
        let rewritten;
        let oid = if dirty.contains(&path) {
            rewritten = write_directory(repository, directories, dirty, &path)?;
            &rewritten
        } else {
            &node.oid
        };
        for field in [node.mode.as_bytes(), b" ", node.kind.as_bytes(), b" "] {
            input.extend_from_slice(field);
        }
        input.extend_from_slice(oid.as_bytes());
        input.push(b'\t');
        input.extend_from_slice(name.as_bytes());
        input.push(0);
    }
    let mut command = repository.git();
    command.args(["mktree", "-z"]);
    let answer = run_with_input(&mut command, &input)?;
    single_id(&command, &answer)
}

/// `path`'s directory and final name; nothing for the root.
fn split(path: &Path) -> Option<(PathBuf, OsString)> {
    Some((
        path.parent()?.to_path_buf(),
        path.file_name()?.to_os_string(),
    ))
}

/// Commit `tree` as `machine`, a child of `parent` when there is one.
///
/// The machine is author and committer, with an empty email, so a ledger
/// commit neither depends on nor carries the git identity of whoever set up
/// the box: a machine with no `user.name` still commits. It is never signed,
/// so a `commit.gpgSign` meant for the box's own commits cannot stall a sweep
/// on a passphrase or a signing agent.
fn commit_tree(
    repository: &Repository,
    machine: &str,
    tree: &str,
    parent: Option<&CommitId>,
) -> Result<CommitId, GitError> {
    let mut command = repository.git();
    command.args([
        "commit-tree",
        "--no-gpg-sign",
        "-m",
        &format!("{machine}: new ledger entries"),
    ]);
    if let Some(parent) = parent {
        command.args(["-p", parent.as_str()]);
    }
    command.arg(tree);
    for (variable, value) in [
        ("GIT_AUTHOR_NAME", machine),
        ("GIT_AUTHOR_EMAIL", ""),
        ("GIT_COMMITTER_NAME", machine),
        ("GIT_COMMITTER_EMAIL", ""),
    ] {
        command.env(variable, value);
    }
    let answer = run(&mut command)?;
    single_id(&command, &answer).map(CommitId::new)
}

/// Every ref `for-each-ref` matches against `patterns`, by full name, with the
/// commit it holds.
///
/// A pattern matches whole components, so `refs/knives/a` never matches
/// `refs/knives/ab`; unlike `rev-parse`, a missing `refs/knives/a` never
/// resolves to a branch that happens to be spelled `refs/knives/a`.
fn references(
    repository: &Repository,
    patterns: &[&str],
) -> Result<Vec<(String, CommitId)>, GitError> {
    let mut command = repository.git();
    command
        .args(["for-each-ref", "--format=%(objectname) %(refname)"])
        .args(patterns);
    let answer = run(&mut command)?;
    std::str::from_utf8(&answer)
        .ok()
        .and_then(|text| {
            text.lines()
                .map(|line| {
                    let (id, name) = line.split_once(' ')?;
                    is_object_id(id).then(|| (name.to_owned(), CommitId::new(id)))
                })
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| output_error(&command, "an object id and a ref name per line"))
}

/// Every blob and subtree under `commit`, by path from its root.
fn tree_items(repository: &Repository, commit: &CommitId) -> Result<Vec<TreeItem>, GitError> {
    let mut command = repository.git();
    command.args(["ls-tree", "-r", "-t", "-z", commit.as_str()]);
    let answer = run(&mut command)?;
    answer
        .split(|&byte| byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            tree_item(record)
                .ok_or_else(|| output_error(&command, "`<mode> <type> <id>\\t<path>` records"))
        })
        .collect()
}

/// One `ls-tree -z` record: `<mode> <type> <id>\t<path>`.
fn tree_item(record: &[u8]) -> Option<TreeItem> {
    let tab = record.iter().position(|&byte| byte == b'\t')?;
    let (header, path) = record.split_at_checked(tab)?;
    let path = path.get(1..).filter(|path| !path.is_empty())?;
    let mut fields = std::str::from_utf8(header).ok()?.split(' ');
    let node = Node {
        mode: fields.next()?.to_owned(),
        kind: fields.next()?.to_owned(),
        oid: fields.next()?.to_owned(),
    };
    if fields.next().is_some() || !is_object_id(&node.oid) {
        return None;
    }
    Some(TreeItem {
        path: PathBuf::from(OsStr::from_bytes(path)),
        node,
    })
}

/// Refuse a tree item that is not an entry file as [`commit_new_entries`]
/// writes one (a `100644` blob named `*.md`), or whose path leaves the ledger
/// directory or enters a `.git`.
fn check_entry(commit: &CommitId, item: &TreeItem) -> Result<(), GitError> {
    let detail = if !item.node.is_blob() || item.node.mode != "100644" {
        Some("not a regular, non-executable file")
    } else if item.path.extension() != Some(OsStr::new("md")) {
        Some("not named *.md")
    } else if !item.path.components().all(
        |component| matches!(component, Component::Normal(name) if !name.eq_ignore_ascii_case(".git")),
    ) {
        Some("its path leaves the ledger directory or enters a .git")
    } else {
        None
    };
    detail.map_or(Ok(()), |detail| {
        Err(GitError::NotAnEntry {
            commit: commit.clone(),
            path: item.path.clone(),
            detail,
        })
    })
}

/// The bodies of a `cat-file --batch` answer, in order; nothing when an
/// object is missing or is not a blob, or the answer is cut short.
fn batch_blobs(mut rest: &[u8]) -> Option<Vec<&[u8]>> {
    let mut blobs = Vec::new();
    while !rest.is_empty() {
        let newline = rest.iter().position(|&byte| byte == b'\n')?;
        let (header, body) = rest.split_at_checked(newline)?;
        let mut fields = std::str::from_utf8(header).ok()?.split(' ');
        let (_, kind, size) = (fields.next()?, fields.next()?, fields.next()?);
        if kind != "blob" || fields.next().is_some() {
            return None;
        }
        let (blob, after) = body.get(1..)?.split_at_checked(size.parse().ok()?)?;
        rest = after.strip_prefix(b"\n")?;
        blobs.push(blob);
    }
    Some(blobs)
}

/// Make `content` visible at `path` whole or not at all, never replacing a
/// file already there; whether this call wrote it.
///
/// The bytes go to a temporary file in the same directory, and only
/// `persist_noclobber` makes the entry's name appear, as
/// [`crate::ledger::Ledger::append`] does: a reader can run at any moment and
/// treats a file it cannot parse as a broken ledger, so a half-written entry
/// would fail an unrelated command. An entry is written once and never
/// rewritten, so a file that appeared at the name in the meantime is that same
/// entry, and is left alone.
fn write_entry(path: &Path, content: &[u8]) -> Result<bool, GitError> {
    let directory = path.parent().unwrap_or(path);
    let unwritable = |path: &Path| {
        let path = path.to_owned();
        move |source| GitError::Write { path, source }
    };
    std::fs::create_dir_all(directory).map_err(unwritable(directory))?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(directory).map_err(unwritable(directory))?;
    temporary.write_all(content).map_err(unwritable(path))?;
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(true),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(unwritable(path)(error.error)),
    }
}

/// Object ids, one per line.
fn object_ids(command: &Command, answer: &[u8]) -> Result<Vec<String>, GitError> {
    std::str::from_utf8(answer)
        .ok()
        .and_then(|text| {
            text.lines()
                .map(|line| is_object_id(line).then(|| line.to_owned()))
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| output_error(command, "one object id per line"))
}

/// The one object id `command` printed.
fn single_id(command: &Command, answer: &[u8]) -> Result<String, GitError> {
    let mut ids = object_ids(command, answer)?;
    match (ids.pop(), ids.is_empty()) {
        (Some(id), true) => Ok(id),
        _ => Err(output_error(command, "exactly one object id")),
    }
}

/// A SHA-1 or SHA-256 object id in full.
fn is_object_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Run `command` to completion, returning its stdout when it exits zero.
fn run(command: &mut Command) -> Result<Vec<u8>, GitError> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|source| GitError::Run {
            invocation: invocation(command),
            source,
        })?;
    succeeded(command, output)
}

/// [`run`], with `input` written to the command's stdin.
///
/// The input is written from its own thread: git answers as it reads, and a
/// command whose stdout pipe fills while this side is still writing would wait
/// on a reader that is itself waiting on it.
fn run_with_input(command: &mut Command, input: &[u8]) -> Result<Vec<u8>, GitError> {
    let failed_to_run = |command: &Command| {
        let invocation = invocation(command);
        move |source| GitError::Run { invocation, source }
    };
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(failed_to_run(command))?;
    let stdin = child.stdin.take();
    let (written, output) = std::thread::scope(|scope| {
        let writer = scope.spawn(move || stdin.map_or(Ok(()), |mut stdin| stdin.write_all(input)));
        let output = child.wait_with_output();
        let written = writer
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (written, output)
    });
    // A command that failed has usually closed its stdin too: its own error,
    // not the broken pipe, is the one worth reporting.
    let answer = succeeded(command, output.map_err(failed_to_run(command))?)?;
    written.map_err(failed_to_run(command))?;
    Ok(answer)
}

fn succeeded(command: &Command, output: Output) -> Result<Vec<u8>, GitError> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(GitError::Failed {
            invocation: invocation(command),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn output_error(command: &Command, expected: &'static str) -> GitError {
    GitError::Output {
        invocation: invocation(command),
        expected,
    }
}

/// `command` as one line, for an error message.
fn invocation(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(OsStr::to_string_lossy)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ref_component_is_one_plain_name() {
        for name in ["alpha", "box-1", "dev.box", "under_score", "A1"] {
            assert_eq!(ref_component("machine", name).unwrap(), name);
        }
    }

    #[test]
    fn a_name_that_could_spell_another_ref_or_an_option_is_refused() {
        for name in [
            "", "@", "*", "a*", "a:b", "a/b", "../a", "a..b", ".a", "a.", "a.lock", "a@{1}", "a b",
            "a~1", "a^", "a?", "a[b", "a\\b", "a\nb", "-f", "--mirror",
        ] {
            assert!(
                matches!(ref_component("machine", name), Err(GitError::Name { .. })),
                "{name:?} was accepted"
            );
        }
    }

    #[test]
    fn a_fork_is_exactly_an_owner_and_a_name() {
        for name in ["acme/tool", "acme-labs/tool_kit", "a.b/c-d"] {
            assert_eq!(
                fork_directory(UpstreamName::new(name)).unwrap().as_str(),
                name
            );
        }
        for name in [
            "",
            "tool",
            "acme/",
            "/tool",
            "acme/tool/x",
            "../tool",
            "acme/..",
            "./tool",
            ".git/tool",
            "acme/.GIT",
            "acme\\tool/x",
            "acme/to\\ol",
        ] {
            assert!(
                matches!(
                    fork_directory(UpstreamName::new(name)),
                    Err(GitError::Fork { .. })
                ),
                "{name:?} was accepted"
            );
        }
    }

    #[test]
    fn a_path_is_quoted_the_way_stdin_paths_unquotes_it() {
        let mut out = Vec::new();
        push_quoted(&mut out, Path::new("\"a\\b\nc\x7f/d.md"));
        assert_eq!(out, b"\"\\\"a\\\\b\\012c\\177/d.md\"");
    }

    #[test]
    fn a_batch_answer_splits_into_its_blobs_and_a_missing_object_spoils_it() {
        let id = "0".repeat(40);
        let answer = format!("{id} blob 3\nabc\n{id} blob 0\n\n");
        assert_eq!(
            batch_blobs(answer.as_bytes()).unwrap(),
            [b"abc".as_slice(), b""]
        );
        assert!(batch_blobs(format!("{id} missing\n").as_bytes()).is_none());
        assert!(batch_blobs(format!("{id} blob 9\nabc\n").as_bytes()).is_none());
    }
}
