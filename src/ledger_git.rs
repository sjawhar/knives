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
use std::time::Duration;

use crate::ids::{CommitId, UpstreamName};

/// Each machine's own ref: `refs/knives/<machine>`.
const OWN: &str = "refs/knives/";
/// Where a fetch leaves every machine's ref: `refs/knives-remotes/<remote>/<machine>`.
const FETCHED: &str = "refs/knives-remotes/";

/// How long one fetch or push may run, start to finish, before it is ended.
///
/// git waits with no limit of its own on a remote that takes the connection
/// and sends nothing, a connect that never completes, or an ssh session gone
/// quiet, and whatever waits on it waits too: a report, or a sweep holding
/// the ledger's lock. Each machine's ref is a small tree of small files, so
/// a minute is many times what a healthy fetch or push takes.
const TRANSPORT_DEADLINE: Duration = Duration::from_secs(60);
/// git abandons an HTTP transfer that runs below [`LOW_SPEED_BYTES`] bytes a
/// second for [`LOW_SPEED_SECONDS`] seconds, so a remote that stops answering
/// mid-transfer fails well before [`TRANSPORT_DEADLINE`].
const LOW_SPEED_BYTES: &str = "1000";
const LOW_SPEED_SECONDS: &str = "15";

/// One machine's ref as a fetch found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineRef {
    pub machine: String,
    pub commit: CommitId,
}

/// What [`materialise`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Materialised {
    /// Entries written into the root.
    pub written: usize,
    /// Entries the refs carry that the root lacks, in a fork this repository
    /// does not carry, by that fork's `<owner>/<name>` directory: what other
    /// machines sent that nothing here reads.
    pub skipped: BTreeMap<String, usize>,
    /// Entries the refs carry that the root lacks, in a fork this repository
    /// carries, that do not parse as an entry, and so were not written.
    pub unreadable: Vec<Unreadable>,
    /// Paths in a fork this repository carries whose contents the refs, or
    /// the refs and the file already here, do not agree on, and so where
    /// nothing was written.
    pub conflicts: Vec<Conflict>,
}

impl Materialised {
    /// Each entry the refs carry in a fork this repository carries that
    /// this pull did not write, with its path under the root, as a problem
    /// to report: one that does not parse, and one whose contents disagree.
    pub fn unwritten(&self) -> impl Iterator<Item = (&Path, String)> {
        self.unreadable
            .iter()
            .map(|unreadable| (unreadable.path.as_path(), unreadable.to_string()))
            .chain(
                self.conflicts
                    .iter()
                    .map(|conflict| (conflict.path.as_path(), conflict.to_string())),
            )
    }
}

/// A path [`materialise`] wrote nothing at because what is there does not
/// agree: two machines' refs carry different blobs at it, or a ref carries a
/// blob the file already here does not hash to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// Where under the ledger root it is.
    pub path: PathBuf,
    /// Every machine whose ref carries the path, whatever it holds there.
    pub machines: Vec<String>,
    /// Whether this machine has a file at the path, which is left as it is.
    pub present: bool,
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut sources: Vec<String> = Vec::new();
        if self.present {
            sources.push("the file here".to_owned());
        }
        sources.extend(
            self.machines
                .iter()
                .map(|machine| format!("{OWN}{machine}")),
        );
        let sources = match sources.split_last() {
            Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
            _ => sources.concat(),
        };
        write!(
            f,
            "{} differs between {sources}, so a pull writes nothing there{}; every pull \
             checks it again",
            crate::ledger::inline_human_text(&self.path.display().to_string()),
            if self.present {
                " and leaves the file here as it is"
            } else {
                ""
            }
        )
    }
}

/// A peer's entry [`materialise`] did not write because it does not parse
/// ([`crate::ledger::parse_bytes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    /// The machine whose ref carries it.
    pub machine: String,
    /// Where under the ledger root it would go.
    pub path: PathBuf,
    /// Why it does not parse.
    pub detail: String,
}

/// Named with its detail escaped ([`crate::ledger::inline_human_text`]): the
/// detail is what the parser said, which quotes the peer's own bytes, and
/// every report that pulls carries it, whoever renders it.
impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "machine {}'s ref carries {}, which this knives cannot read: {}. It is not written \
             here, since every read of that fork would fail on it; every pull checks it again",
            self.machine,
            crate::ledger::inline_human_text(&self.path.display().to_string()),
            crate::ledger::inline_human_text(&self.detail)
        )
    }
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
    ///
    /// `work_tree` is created when it does not exist yet: every git call
    /// runs in it ([`Repository::git`]), and a new machine joined only
    /// through a git directory in `ledger-repositories` has no ledger root
    /// until a pull writes one in, which no git call could otherwise reach.
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
        let forks = forks
            .into_iter()
            .map(fork_directory)
            .collect::<Result<_, _>>()?;
        let work_tree = absolute(work_tree)?;
        std::fs::create_dir_all(&work_tree).map_err(|source| GitError::Write {
            path: work_tree.clone(),
            source,
        })?;
        Ok(Self {
            git_dir: absolute(git_dir)?,
            work_tree,
            forks,
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

    /// Whether the entry at `path`, relative to the root and already checked
    /// by [`check_entry`], lies below the `<owner>/<name>` directory of a fork
    /// this repository carries. `strip_prefix` matches whole components, so
    /// `acme/tool` never matches `acme/toolkit/…`.
    fn carries_entry(&self, path: &Path) -> bool {
        self.forks.iter().any(|fork| {
            path.strip_prefix(fork.as_str())
                .is_ok_and(|rest| rest.components().next().is_some())
        })
    }

    /// `git` on this repository, run in the ledger root.
    ///
    /// `--git-dir` and `--work-tree` leave nothing to discovery, and
    /// [`crate::bind::git_command`] scrubs an inherited `GIT_DIR` and its
    /// companions. The configuration the environment carries as
    /// `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_<n>` and `GIT_CONFIG_VALUE_<n>` is
    /// kept: it is how a session routes git to the identity it acts as (an
    /// `include.path` of credential and URL rules), and without it a fetch or
    /// push falls back to whatever credential the user's own configuration
    /// names, acting on the forge as that user. No terminal prompt: a ledger
    /// command has nobody to answer one, whether it is a sweep running
    /// detached or a pull a report is waiting on, and a credential prompt
    /// would hold the ledger's lock until someone noticed.
    fn git(&self) -> Command {
        let mut command = crate::bind::git_command();
        for (name, value) in std::env::vars_os() {
            if carries_session_config(name.as_encoded_bytes()) {
                command.env(name, value);
            }
        }
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

/// Whether the environment variable `name` is part of the configuration a
/// session hands git as `GIT_CONFIG_COUNT` and its numbered pairs.
fn carries_session_config(name: &[u8]) -> bool {
    let numbered = |prefix: &[u8]| {
        name.strip_prefix(prefix)
            .is_some_and(|index| !index.is_empty() && index.iter().all(u8::is_ascii_digit))
    };
    name == b"GIT_CONFIG_COUNT" || numbered(b"GIT_CONFIG_KEY_") || numbered(b"GIT_CONFIG_VALUE_")
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
    #[error(
        "{invocation} was ended after {}s: the remote stopped answering",
        deadline.as_secs()
    )]
    Deadline {
        invocation: String,
        deadline: Duration,
    },
    #[error("{invocation} failed: {stderr}")]
    Failed { invocation: String, stderr: String },
    #[error(
        "git rejected this machine's ref {machine} on {remote} as non-fast-forward: another \
         machine may be committing under the same name; give each machine a unique \
         knives.machine with `git --git-dir={} config knives.machine <unique-name>` \
         ({invocation} failed: {stderr})",
        git_dir.display()
    )]
    MachineNameCollision {
        machine: String,
        remote: String,
        git_dir: PathBuf,
        invocation: String,
        stderr: String,
    },
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

/// Commit what `pending` found as `machine` to `refs/knives/<machine>`, as
/// a child of the commit that ref holds now.
///
/// That is every entry in the forks `repository` carries that no knives ref
/// carries yet, less any the caller [`Pending::forget`]s in between.
///
/// An entry is a regular file named as one
/// ([`crate::ledger::is_entry_file_name`]) directly inside a carried fork's
/// `<owner>/<repository>/` directory, the files
/// [`crate::ledger::Ledger::read`] reads. No other fork's directory is
/// read, however the root is shared.
/// "Carried" counts every ref under `refs/knives/` and `refs/knives-remotes/`,
/// so an entry materialised from a peer is not committed again as this
/// machine's own. `Ok(None)` when nothing is new, and the ref is then
/// untouched.
///
/// The ref moves by compare-and-swap against the value [`pending`] read, so
/// a second writer for the same machine racing this one fails loudly rather
/// than lose the other's commit.
pub fn commit_pending(
    repository: &Repository,
    machine: &str,
    pending: Pending,
) -> Result<Option<CommitId>, GitError> {
    let Pending {
        reference,
        previous,
        base,
        added,
    } = pending;
    if added.is_empty() {
        return Ok(None);
    }
    let blobs = hash_objects(repository, added.iter().map(PathBuf::as_path), true)?;
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

/// Whether [`commit_pending`] would commit anything now. It only reads, so
/// it can run while another process holds the ledger's lock and commits.
pub fn has_new_entries(repository: &Repository, machine: &str) -> Result<bool, GitError> {
    Ok(!pending(repository, machine)?.added.is_empty())
}

/// What a commit as one machine would start from and add.
#[derive(Debug)]
pub struct Pending {
    reference: String,
    previous: Option<CommitId>,
    base: Vec<TreeItem>,
    added: BTreeSet<PathBuf>,
}

impl Pending {
    /// Every entry, relative to the root, that no knives ref carries yet:
    /// what this machine wrote and has not committed.
    pub const fn added(&self) -> &BTreeSet<PathBuf> {
        &self.added
    }

    /// Leave out an entry that is no longer on disk.
    pub fn forget(&mut self, path: &Path) {
        let _ = self.added.remove(path);
    }
}

/// What a commit as `machine` would start from and add ([`commit_pending`]).
///
/// A remote that has `machine`'s ref when this checkout has none means the
/// name is another machine's, or this checkout was recreated; either way a
/// commit here would start a second history that the push then rejects, so
/// the call refuses with [`GitError::NotOwn`] instead, as [`push`] does.
pub fn pending(repository: &Repository, machine: &str) -> Result<Pending, GitError> {
    let reference = own_ref(machine)?;
    let refs = references(repository, &[OWN, FETCHED])?;
    let previous = refs
        .iter()
        .find(|(name, _)| *name == reference)
        .map(|(_, commit)| commit.clone());
    let base = previous
        .as_ref()
        .map(|commit| tree_items(repository, commit))
        .transpose()?
        .unwrap_or_default();
    let mut carried: BTreeSet<PathBuf> = base
        .iter()
        .filter(|item| item.mode.is_blob())
        .map(|item| item.path.clone())
        .collect();
    let mut fetched_copy = None;
    for (name, commit) in refs {
        if let Some((remote, fetched)) = name
            .strip_prefix(FETCHED)
            .and_then(|rest| rest.split_once('/'))
            && fetched == machine
        {
            fetched_copy.get_or_insert_with(|| remote.to_owned());
        }
        // This machine's own tree is listed once, above, whichever ref names
        // its commit: in steady state the fetched copy of its ref does too.
        if name == reference || previous.as_ref() == Some(&commit) {
            continue;
        }
        carried.extend(
            tree_items(repository, &commit)?
                .into_iter()
                .filter(|item| item.mode.is_blob())
                .map(|item| item.path),
        );
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
/// history, and the fetch fails rather than follow it. git still takes every
/// other machine's ref, so after a failed fetch [`fetched`] holds each copy
/// it could update, and the refused one as an earlier fetch left it. Not
/// `--quiet`: what git prints on its way to failing is the one place it names
/// the ref it refused. `--prune` drops the copy of a ref the remote no longer
/// has, so the report is the remote's refs now.
pub fn fetch(repository: &Repository, remote: &str) -> Result<Vec<MachineRef>, GitError> {
    let remote = ref_component("remote", remote)?;
    transport(repository.git().args([
        "fetch",
        "--no-tags",
        "--no-write-fetch-head",
        "--prune",
        remote,
        &format!("{OWN}*:{FETCHED}{remote}/*"),
    ]))?;
    fetched(repository, remote)
}

/// Every machine's ref as this repository's copy from `remote` holds it now,
/// by machine name: what the last [`fetch`] took, or this machine's own
/// [`push`] sent.
pub fn fetched(repository: &Repository, remote: &str) -> Result<Vec<MachineRef>, GitError> {
    let remote = ref_component("remote", remote)?;
    let namespace = format!("{FETCHED}{remote}/");
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

/// Push `refs/knives/<machine>` to `remote` when it is new there; whether it was.
///
/// The ref goes to the same name on `remote`, and nothing else, when it
/// holds a commit `remote` was not last seen holding, by a [`fetch`] or by
/// this machine's own push.
///
/// This is the one place a ref leaves the machine, so it is where "a machine
/// writes only its own ref" holds. The refspec is spelled from the machine name
/// alone and never forced, and a name that could spell any other ref (a glob,
/// a second refspec, a nested path) is refused before git runs. Only
/// [`commit_pending`] writes under `refs/knives/`, so
/// `refs/knives/<machine>` exists here only if this checkout committed as that
/// machine. A machine `remote` has but this checkout never committed as is a
/// peer, and pushing it is refused; a machine neither side has has nothing to
/// send, and neither does one `remote` was last seen holding at this commit,
/// which is what a sweep with nothing new to commit finds. git rejecting the
/// push means some other writer moved this machine's ref on `remote`.
///
/// Once `remote` has taken the commit, it is recorded as `remote`'s copy,
/// where [`fetch`] leaves copies, so the next call knows it was sent without
/// asking the remote again.
pub fn push(repository: &Repository, remote: &str, machine: &str) -> Result<bool, GitError> {
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
    let Some(commit) = at(&reference) else {
        return if at(&fetched).is_some() {
            Err(GitError::NotOwn {
                machine: machine.to_owned(),
                remote: remote.to_owned(),
            })
        } else {
            Ok(false)
        };
    };
    if at(&fetched) == Some(commit) {
        return Ok(false);
    }
    match transport(repository.git().args([
        "push",
        "--quiet",
        "--no-follow-tags",
        remote,
        &format!("{reference}:{reference}"),
    ])) {
        Ok(_) => {}
        Err(GitError::Failed { invocation, stderr })
            if push_was_rejected_as_non_fast_forward(&stderr) =>
        {
            return Err(GitError::MachineNameCollision {
                machine: machine.to_owned(),
                remote: remote.to_owned(),
                git_dir: repository.git_dir().to_owned(),
                invocation,
                stderr,
            });
        }
        Err(error) => return Err(error),
    }
    run(repository
        .git()
        .args(["update-ref", &fetched, commit.as_str()]))?;
    Ok(true)
}

/// Whether `git push` rejected its only ref because the remote had already
/// moved it since this machine last fetched.
fn push_was_rejected_as_non_fast_forward(stderr: &str) -> bool {
    stderr.contains("non-fast-forward")
        || stderr.contains("(fetch first)")
        || (stderr.contains("failed to update ref") && stderr.contains("reference already exists"))
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
                .filter(|item| item.mode.is_blob())
                .map(|item| item.path),
        );
    }
    let mut local = local_entries(repository)?;
    local.retain(|path| !sent.contains(path));
    Ok(local)
}

/// The git directory `git_dir` is, as an absolute path, with a gitfile followed.
///
/// A ledger root's `.git` is a file naming the real directory when the root
/// is a linked worktree or was made with `git init --separate-git-dir`. git
/// reads through a gitfile wherever `--git-dir` names one; a file of knives'
/// own belongs in the directory it names.
pub fn absolute_git_dir(git_dir: &Path) -> Result<PathBuf, GitError> {
    use std::os::unix::ffi::OsStringExt as _;
    let mut command = crate::bind::git_command();
    command
        .arg("--git-dir")
        .arg(git_dir)
        .args(["rev-parse", "--absolute-git-dir"]);
    let mut answer = run(&mut command)?;
    if answer.last() == Some(&b'\n') {
        answer.pop();
    }
    if answer.is_empty() {
        return Err(output_error(&command, "a git directory"));
    }
    Ok(PathBuf::from(OsString::from_vec(answer)))
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
/// ledger root lacks, that every ref carrying it agrees on, and that parses.
///
/// Each entry is written whole to a temporary name and renamed into place,
/// never over a file already there; see [`write_entry`]. A tree item that is
/// not an entry file, or whose path would leave the root or enter a `.git`,
/// fails the whole call before anything is written, whichever fork it names:
/// `mktree` accepts names like `..` and `.git`, so a peer's ref must be
/// checked before it reaches the filesystem. An entry in a fork this
/// repository does not carry is left out, and counted in
/// [`Materialised::skipped`]: it came through a remote that fork does not
/// belong to here, and written into the shared root it would be committed
/// again by the repository that does carry the fork. That is what two
/// machines whose registries send one fork to different repositories do,
/// and the count is how this machine says so.
///
/// Each entry is parsed as a local read parses it
/// ([`crate::ledger::parse_bytes`]) before it is written, and one that does
/// not parse is left out and named in [`Materialised::unreadable`]: written,
/// it would fail every read of its fork here, and a committed entry is never
/// rewritten, so deleting it would only have the next pull write it back.
/// Left out, it is checked again at every pull, so a knives that can read it
/// writes it then.
///
/// Two refs carrying different blobs at one path, or a ref whose blob at a
/// path differs from what is already there, write nothing at that path, and
/// it is named with every machine whose ref carries it in
/// [`Materialised::conflicts`]: otherwise whichever blob came first would
/// win by the order the refs are listed in, and a file already there is
/// never replaced. A regular file there is compared by the blob id git gives
/// its exact bytes; anything else there differs from every blob. Two refs
/// carrying one blob at a path carry one entry.
pub fn materialise(repository: &Repository, refs: &[MachineRef]) -> Result<Materialised, GitError> {
    let mut carried: BTreeMap<PathBuf, Vec<(ObjectId, &str)>> = BTreeMap::new();
    let mut uncarried = BTreeSet::new();
    for machine_ref in refs {
        for item in tree_items(repository, &machine_ref.commit)? {
            if item.mode == Mode::Tree {
                continue;
            }
            check_entry(&machine_ref.commit, &item)?;
            if repository.carries_entry(&item.path) {
                carried
                    .entry(item.path)
                    .or_default()
                    .push((item.oid, machine_ref.machine.as_str()));
            } else {
                uncarried.insert(item.path);
            }
        }
    }
    let mut done = Materialised::default();
    for path in uncarried {
        if occupant(repository, &path)? != Occupant::Nothing {
            continue;
        }
        // Rebuilt from its components, so a peer's tree that spells a `.`
        // inside the path counts it under the fork directory it names.
        let fork: PathBuf = path.parent().into_iter().flatten().collect();
        *done.skipped.entry(fork.display().to_string()).or_default() += 1;
    }
    let mut missing: BTreeMap<PathBuf, (ObjectId, &str)> = BTreeMap::new();
    let mut here: Vec<(PathBuf, ObjectId, Vec<String>)> = Vec::new();
    for (path, carriers) in carried {
        let Some((oid, machine)) = carriers.first() else {
            continue;
        };
        let agreed = carriers.iter().all(|(other, _)| other == oid);
        match (agreed, occupant(repository, &path)?) {
            (true, Occupant::Nothing) => {
                missing.insert(path, (oid.clone(), *machine));
            }
            (true, Occupant::File) => here.push((path, oid.clone(), machines(&carriers))),
            (_, occupant) => done.conflicts.push(Conflict {
                path,
                machines: machines(&carriers),
                present: occupant != Occupant::Nothing,
            }),
        }
    }
    if !here.is_empty() {
        let ids = hash_objects(
            repository,
            here.iter().map(|(path, _, _)| path.as_path()),
            false,
        )?;
        for ((path, oid, machines), id) in here.into_iter().zip(ids) {
            if id != oid {
                done.conflicts.push(Conflict {
                    path,
                    machines,
                    present: true,
                });
            }
        }
    }
    if missing.is_empty() {
        return Ok(done);
    }
    let mut input = Vec::new();
    for (oid, _) in missing.values() {
        input.extend_from_slice(oid.as_str().as_bytes());
        input.push(b'\n');
    }
    let mut command = repository.git();
    command.args(["cat-file", "--batch"]);
    let answer = run_with_input(&mut command, &input)?;
    let blobs = batch_blobs(&answer)
        .filter(|blobs| blobs.len() == missing.len())
        .ok_or_else(|| output_error(&command, "each requested blob, in order"))?;
    for ((relative, (_, machine)), content) in missing.iter().zip(blobs) {
        let path = repository.work_tree.join(relative);
        if let Err(error) = crate::ledger::parse_bytes(&path, content) {
            done.unreadable.push(Unreadable {
                machine: (*machine).to_owned(),
                path: relative.clone(),
                detail: error.to_string(),
            });
            continue;
        }
        if write_entry(&path, content)? {
            done.written += 1;
        }
    }
    Ok(done)
}

/// The machines whose refs carry a path, in the order the refs are listed.
fn machines(carriers: &[(ObjectId, &str)]) -> Vec<String> {
    carriers
        .iter()
        .map(|(_, machine)| (*machine).to_owned())
        .collect()
}

/// What is at an entry's path in the ledger root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Occupant {
    Nothing,
    /// A regular file, which may hold the very entry a ref carries.
    File,
    /// A directory, a symlink or anything else: never an entry.
    Other,
}

/// What is at `relative` under `repository`'s root, a symlink not followed.
fn occupant(repository: &Repository, relative: &Path) -> Result<Occupant, GitError> {
    let path = repository.work_tree.join(relative);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(Occupant::File),
        Ok(_) => Ok(Occupant::Other),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Occupant::Nothing),
        Err(source) => Err(GitError::Read { path, source }),
    }
}

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
enum Mode {
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
    const fn spelled(self) -> (&'static str, &'static str) {
        match self {
            Self::File => ("100644", "blob"),
            Self::Executable => ("100755", "blob"),
            Self::Symlink => ("120000", "blob"),
            Self::Tree => ("040000", "tree"),
            Self::Submodule => ("160000", "commit"),
        }
    }

    const fn is_blob(self) -> bool {
        matches!(self, Self::File | Self::Executable | Self::Symlink)
    }
}

/// A git object id in full, SHA-1 or SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ObjectId(String);

impl ObjectId {
    /// `text` when it is a full object id: 40 or 64 hex digits.
    fn parse(text: &str) -> Option<Self> {
        (matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| Self(text.to_owned()))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

/// One record of a recursive `ls-tree`, parsed where it is read
/// ([`tree_item`]): a path from the root, its mode and its object.
#[derive(Debug)]
struct TreeItem {
    path: PathBuf,
    mode: Mode,
    oid: ObjectId,
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
pub(crate) fn ref_component<'a>(role: &'static str, name: &'a str) -> Result<&'a str, GitError> {
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
/// fork's repository ([`crate::ledger::is_fork_directory`]), neither of them
/// `.`, `..` or a `.git`.
///
/// A fork whose upstream is a filesystem path is kept under its one-component
/// registry key, and names no repository a destination could share, so a
/// name of one component is refused here as surely as one of three.
fn fork_directory(fork: UpstreamName) -> Result<UpstreamName, GitError> {
    if crate::ledger::is_fork_directory(fork.as_str()) {
        Ok(fork)
    } else {
        Err(GitError::Fork {
            name: fork.to_string(),
        })
    }
}

/// Every regular file named as an entry
/// ([`crate::ledger::is_entry_file_name`]) directly inside each fork
/// `repository` carries, relative to the root: what
/// [`crate::ledger::Ledger::read`] reads, and nothing it reports as
/// misnamed. A fork with no directory yet has no entries.
fn local_entries(repository: &Repository) -> Result<BTreeSet<PathBuf>, GitError> {
    let mut found = BTreeSet::new();
    for fork in repository.forks() {
        let relative = Path::new(fork.as_str());
        let directory = repository.work_tree.join(relative);
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
            if name.to_str().is_some_and(crate::ledger::is_entry_file_name)
                && dirent.file_type().map_err(unreadable)?.is_file()
            {
                found.insert(relative.join(name));
            }
        }
    }
    Ok(found)
}

/// The blob id of each file at `paths` (relative to the root), in the same
/// order, written into the object store when `write` is set.
///
/// `--no-filters`, so a blob is the file's exact bytes whatever
/// `.gitattributes` says, and a peer materialises it byte for byte; a file
/// compared against a peer's blob is hashed the same way, so the same bytes
/// give the same id.
fn hash_objects<'a>(
    repository: &Repository,
    paths: impl IntoIterator<Item = &'a Path>,
    write: bool,
) -> Result<Vec<ObjectId>, GitError> {
    let mut input = Vec::new();
    let mut count = 0;
    for path in paths {
        push_quoted(&mut input, path);
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
fn commit_tree(
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
                    let id = ObjectId::parse(id)?;
                    Some((name.to_owned(), CommitId::new(id.0)))
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

/// One `ls-tree -z` record, `<mode> <type> <id>\t<path>`, parsed: `None`
/// for a mode git does not write, a type that mode does not have, or an id
/// that is not one.
fn tree_item(record: &[u8]) -> Option<TreeItem> {
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
fn check_entry(commit: &CommitId, item: &TreeItem) -> Result<(), GitError> {
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

/// [`run`] for a command that talks to a remote: one whose HTTP transfer
/// stalls fails, and one still running at [`TRANSPORT_DEADLINE`] is ended.
fn transport(command: &mut Command) -> Result<Vec<u8>, GitError> {
    command
        .env("GIT_HTTP_LOW_SPEED_LIMIT", LOW_SPEED_BYTES)
        .env("GIT_HTTP_LOW_SPEED_TIME", LOW_SPEED_SECONDS);
    run_until(command, TRANSPORT_DEADLINE)
}

/// [`run`], ending the command and every process it started once it has run
/// for `deadline`.
///
/// The command runs in a process group of its own, so ending it reaches the
/// helpers git starts for a transport (`git-remote-https`, `ssh`): they hold
/// its output pipes, and ending git alone would leave the read of its output
/// waiting on them. The same group keeps a Ctrl-C meant for knives from
/// reaching git; a fetch or push left running that way still ends at its own
/// low-speed limit or writes only the remote's copy of a ref.
fn run_until(command: &mut Command, deadline: Duration) -> Result<Vec<u8>, GitError> {
    use std::os::unix::process::CommandExt as _;
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|source| GitError::Run {
            invocation: invocation(command),
            source,
        })?;
    let group = rustix::process::Pid::from_child(&child);
    let (finished, waiting) = std::sync::mpsc::channel::<()>();
    let (output, ended) = std::thread::scope(|scope| {
        let watchdog = scope.spawn(move || {
            let expired = matches!(
                waiting.recv_timeout(deadline),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            );
            if expired {
                // Already gone is as good as ended.
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            }
            expired
        });
        let output = child.wait_with_output();
        drop(finished);
        let ended = watchdog
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (output, ended)
    });
    if ended {
        return Err(GitError::Deadline {
            invocation: invocation(command),
            deadline,
        });
    }
    let output = output.map_err(|source| GitError::Run {
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
    fn an_unwritten_peer_entry_is_named_with_no_control_character() {
        // An unreadable entry's detail is what the parser said, which
        // quotes the peer's own bytes; every report that pulls carries it.
        let path = PathBuf::from("acme/a-repo/20261011T120000.000000000Z-0000.md");
        let detail = crate::ledger::parse_bytes(&path, b"+++\nts = \"\x1b]0;pwned\x07\"\n+++\nx\n")
            .unwrap_err()
            .to_string();
        assert!(
            detail.contains('\u{1b}'),
            "the parser no longer quotes: {detail:?}"
        );
        let lines = [
            Unreadable {
                machine: "mallory".to_owned(),
                path,
                detail,
            }
            .to_string(),
            Conflict {
                path: PathBuf::from("acme/a-repo/x-\u{1b}[2J.md"),
                machines: vec!["mallory".to_owned()],
                present: true,
            }
            .to_string(),
        ];
        for line in lines {
            assert!(!line.chars().any(|c| c.is_ascii_control()), "{line:?}");
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

    #[test]
    fn a_tree_record_parses_only_as_a_mode_git_writes_with_its_own_type() {
        let id = "a".repeat(40);
        let record = |text: String| tree_item(text.as_bytes());
        for (mode, kind, parsed) in [
            ("100644", "blob", Mode::File),
            ("100755", "blob", Mode::Executable),
            ("120000", "blob", Mode::Symlink),
            ("040000", "tree", Mode::Tree),
            ("160000", "commit", Mode::Submodule),
        ] {
            let item = record(format!("{mode} {kind} {id}\tacme/x.md")).unwrap();
            assert_eq!(item.mode, parsed);
            assert_eq!(item.oid.as_str(), id);
            assert_eq!(item.path, Path::new("acme/x.md"));
            assert_eq!(parsed.spelled(), (mode, kind));
        }
        for text in [
            format!("100644 tree {id}\tx"),
            format!("040000 blob {id}\tx"),
            format!("100664 blob {id}\tx"),
            format!("100644 blob {}\tx", "a".repeat(39)),
            format!("100644 blob {id} extra\tx"),
            format!("100644 blob {id}\t"),
            format!("100644 blob {id}"),
        ] {
            assert!(record(text.clone()).is_none(), "{text:?} parsed");
        }
    }

    #[test]
    fn only_the_numbered_config_a_session_hands_git_is_carried() {
        for name in [
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_12",
        ] {
            assert!(carries_session_config(name.as_bytes()), "{name}");
        }
        for name in [
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_CONFIG",
            "GIT_CONFIG_KEY_",
            "GIT_CONFIG_VALUE_x",
            "GIT_DIR",
        ] {
            assert!(!carries_session_config(name.as_bytes()), "{name}");
        }
    }
}
