//! The git objects a ledger repository is made of: blobs hashed from entry
//! files, the tree written over them, the commit on a machine's ref, and the
//! answers `git` gives when asked to read them back.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use super::run::{output_error, run, run_with_input};
use super::{GitError, Repository};
use crate::ids::CommitId;

/// A name in a tree being written ([`write_tree`]).
#[derive(Debug)]
struct Node {
    mode: Mode,
    /// What the base tree lists for it; `None` for a directory this commit
    /// adds, which [`write_directory`] writes and so gives its id.
    oid: Option<ObjectId>,
}

/// A tree entry's mode, which fixes its object type: one of the five git
/// writes, the only ones `ls-tree` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// `100644 blob`: a regular, non-executable file, the one an entry is.
    File,
    /// `100755 blob`.
    Executable,
    /// `120000 blob`.
    Symlink,
    /// `040000 tree`.
    Tree,
    /// `160000 commit`: a submodule.
    Submodule,
}

impl Mode {
    /// The mode `mode` and `kind` spell together, as `ls-tree` prints them;
    /// `None` for any other mode, or a type that mode does not have.
    fn parse(mode: &str, kind: &str) -> Option<Self> {
        let parsed = match mode {
            "100644" => Self::File,
            "100755" => Self::Executable,
            "120000" => Self::Symlink,
            "040000" => Self::Tree,
            "160000" => Self::Submodule,
            _ => return None,
        };
        (parsed.spelled().1 == kind).then_some(parsed)
    }

    /// The mode and type as `mktree` reads them.
    pub(super) const fn spelled(self) -> (&'static str, &'static str) {
        match self {
            Self::File => ("100644", "blob"),
            Self::Executable => ("100755", "blob"),
            Self::Symlink => ("120000", "blob"),
            Self::Tree => ("040000", "tree"),
            Self::Submodule => ("160000", "commit"),
        }
    }

    pub(super) const fn is_blob(self) -> bool {
        matches!(self, Self::File | Self::Executable | Self::Symlink)
    }
}

/// A git object id in full, SHA-1 or SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ObjectId(pub(super) String);

impl ObjectId {
    /// `text` when it is a full object id: 40 or 64 hex digits.
    pub(super) fn parse(text: &str) -> Option<Self> {
        (matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| Self(text.to_owned()))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// One record of a recursive `ls-tree`, parsed where it is read
/// ([`tree_item`]): a path from the root, its mode and its object.
#[derive(Debug)]
pub(super) struct TreeItem {
    pub(super) path: PathBuf,
    pub(super) mode: Mode,
    pub(super) oid: ObjectId,
}

/// Every directory of a tree being written, by path from the root (`""`
/// is the root), with the nodes it holds by name.
type Directories = BTreeMap<PathBuf, BTreeMap<OsString, Node>>;

/// The blob id of each file at `paths` (relative to the root), in the same
/// order, written into the object store when `write` is set.
///
/// `--no-filters`, so a blob is the file's exact bytes whatever
/// `.gitattributes` says, and a peer materialises it byte for byte; a file
/// compared against a peer's blob is hashed the same way, so the same bytes
/// give the same id. Each path is handed to git under the root, so it reads
/// the same file whatever directory git runs in.
pub(super) fn hash_objects<'a>(
    repository: &Repository,
    paths: impl IntoIterator<Item = &'a Path>,
    write: bool,
) -> Result<Vec<ObjectId>, GitError> {
    let mut input = Vec::new();
    let mut count = 0;
    for path in paths {
        push_quoted(&mut input, &repository.work_tree.join(path));
        input.push(b'\n');
        count += 1;
    }
    let mut command = repository.git();
    command.arg("hash-object");
    if write {
        command.arg("-w");
    }
    command.args(["--no-filters", "--stdin-paths"]);
    let answer = run_with_input(&mut command, &input)?;
    let ids = object_ids(&command, &answer)?;
    if ids.len() == count {
        Ok(ids)
    } else {
        Err(output_error(&command, "one blob id per path"))
    }
}

/// `path` C-quoted, the form `--stdin-paths` reads back exactly: otherwise a
/// name holding a newline would end its line early, and one opening with `"`
/// would be unquoted as if it were quoted.
pub(super) fn push_quoted(out: &mut Vec<u8>, path: &Path) {
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
pub(super) fn write_tree(
    repository: &Repository,
    base: Vec<TreeItem>,
    added: impl Iterator<Item = (PathBuf, ObjectId)>,
) -> Result<ObjectId, GitError> {
    let mut directories = Directories::new();
    for item in base {
        if let Some((parent, name)) = split(&item.path) {
            directories.entry(parent).or_default().insert(
                name,
                Node {
                    mode: item.mode,
                    oid: Some(item.oid),
                },
            );
        }
    }
    let mut dirty = BTreeSet::from([PathBuf::new()]);
    for (path, oid) in added {
        let Some((mut directory, name)) = split(&path) else {
            continue;
        };
        let blob = Node {
            mode: Mode::File,
            oid: Some(oid),
        };
        let replaced = directories
            .entry(directory.clone())
            .or_default()
            .insert(name, blob);
        if replaced.is_some_and(|node| node.mode == Mode::Tree) {
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
                .or_insert(Node {
                    mode: Mode::Tree,
                    oid: None,
                });
            if node.mode != Mode::Tree {
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
) -> Result<ObjectId, GitError> {
    let mut input = Vec::new();
    for (name, node) in directories.get(directory).into_iter().flatten() {
        let path = directory.join(name);
        let rewritten;
        let oid = match &node.oid {
            Some(oid) if !dirty.contains(&path) => oid,
            _ => {
                rewritten = write_directory(repository, directories, dirty, &path)?;
                &rewritten
            }
        };
        let (mode, kind) = node.mode.spelled();
        for field in [mode, " ", kind, " ", oid.as_str(), "\t"] {
            input.extend_from_slice(field.as_bytes());
        }
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
pub(super) fn commit_tree(
    repository: &Repository,
    machine: &str,
    tree: &ObjectId,
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
    command.arg(tree.as_str());
    for (variable, value) in [
        ("GIT_AUTHOR_NAME", machine),
        ("GIT_AUTHOR_EMAIL", ""),
        ("GIT_COMMITTER_NAME", machine),
        ("GIT_COMMITTER_EMAIL", ""),
    ] {
        command.env(variable, value);
    }
    let answer = run(&mut command)?;
    single_id(&command, &answer).map(|id| CommitId::new(id.0))
}

/// Every ref `for-each-ref` matches against `patterns`, by full name, with the
/// commit it holds.
///
/// A pattern matches whole components, so `refs/knives/a` never matches
/// `refs/knives/ab`; unlike `rev-parse`, a missing `refs/knives/a` never
/// resolves to a branch that happens to be spelled `refs/knives/a`.
pub(super) fn references(
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
                    let id = ObjectId::parse(id)?;
                    Some((name.to_owned(), CommitId::new(id.0)))
                })
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| output_error(&command, "an object id and a ref name per line"))
}

/// Every blob and subtree under `commit`, by path from its root.
pub(super) fn tree_items(
    repository: &Repository,
    commit: &CommitId,
) -> Result<Vec<TreeItem>, GitError> {
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

/// One `ls-tree -z` record, `<mode> <type> <id>\t<path>`, parsed: `None`
/// for a mode git does not write, a type that mode does not have, or an id
/// that is not one.
pub(super) fn tree_item(record: &[u8]) -> Option<TreeItem> {
    let tab = record.iter().position(|&byte| byte == b'\t')?;
    let (header, path) = record.split_at_checked(tab)?;
    let path = path.get(1..).filter(|path| !path.is_empty())?;
    let mut fields = std::str::from_utf8(header).ok()?.split(' ');
    let mode = Mode::parse(fields.next()?, fields.next()?)?;
    let oid = ObjectId::parse(fields.next()?)?;
    if fields.next().is_some() {
        return None;
    }
    Some(TreeItem {
        path: PathBuf::from(OsStr::from_bytes(path)),
        mode,
        oid,
    })
}

/// Whether `path` has the one shape a ledger entry reader accepts:
/// `<owner>/<repository>/<file>`, the owner and the repository each a
/// directory a fork can be kept under ([`crate::ledger::is_ledger_component`]).
fn entry_path(path: &Path) -> bool {
    let mut components = path.components();
    let directory = |component: Option<Component<'_>>| match component {
        Some(Component::Normal(name)) => name
            .to_str()
            .is_some_and(crate::ledger::is_ledger_component),
        _ => false,
    };
    directory(components.next())
        && directory(components.next())
        && matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
}

/// Refuse a tree item that is not an entry file as [`commit_pending`]
/// writes one: a `100644` blob at `<owner>/<repository>/<file>`, its
/// directories ones a fork can be kept under and its file named as
/// [`crate::ledger::is_entry_file_name`] says. A name no knives writes,
/// one holding a terminal control sequence among them, never reaches this
/// machine's ledger.
pub(super) fn check_entry(commit: &CommitId, item: &TreeItem) -> Result<(), GitError> {
    let detail = if item.mode != Mode::File {
        Some("not a regular, non-executable file")
    } else if !entry_path(&item.path) {
        Some("not exactly <owner>/<repository>/<file>.md")
    } else if !item
        .path
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(crate::ledger::is_entry_file_name)
    {
        Some("not named as knives names an entry, YYYYMMDDTHHMMSS.nnnnnnnnnZ-xxxx.md")
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
pub(super) fn batch_blobs(mut rest: &[u8]) -> Option<Vec<&[u8]>> {
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

/// Object ids, one per line.
fn object_ids(command: &Command, answer: &[u8]) -> Result<Vec<ObjectId>, GitError> {
    std::str::from_utf8(answer)
        .ok()
        .and_then(|text| {
            text.lines()
                .map(ObjectId::parse)
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| output_error(command, "one object id per line"))
}

/// The one object id `command` printed.
fn single_id(command: &Command, answer: &[u8]) -> Result<ObjectId, GitError> {
    let mut ids = object_ids(command, answer)?;
    match (ids.pop(), ids.is_empty()) {
        (Some(id), true) => Ok(id),
        _ => Err(output_error(command, "exactly one object id")),
    }
}
