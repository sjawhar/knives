//! Claims that hold on every machine sharing a fork's ledger.
//!
//! A claim on a shared fork is one ref on the ledger's remote,
//! `refs/knives-claims/<owner>/<name>/<branch>` ([`ClaimRef`]), holding a
//! commit with no parent whose tree is one file, `claim.toml`
//! ([`ClaimRecord`]). The remote is the one place every machine sharing the
//! ledger sees, so whoever's commit that ref holds holds the branch, on
//! whatever machine they are; and a push that names the commit it replaces
//! (the lease of [`push_claim`]) is the one write git refuses when another
//! machine got there first.
//!
//! The prefix is outside everything the exchange of entries touches: every
//! release fetches `refs/knives/*` and lists only refs under `refs/knives/`
//! and `refs/knives-remotes/`, so a claim ref is never read as entries and an
//! older knives sees none. Nothing here writes a local ref under it either: a
//! read takes a claim commit's objects and no name for them.
//!
//! A claim moves in the same push as this machine's ref, with `--atomic`, so
//! the entry that records the change and the change itself land together or
//! not at all. What came of that push is read from git's porcelain lines and
//! its stderr, and where they do not settle it, from the remote itself.

use std::collections::BTreeMap;
use std::process::Stdio;

use serde::{Deserialize, Serialize};

use super::run::{invocation, output_error, run, run_with_input, succeeded, transport_transcript};
use super::tree::{Mode, ObjectId, tree_item};
use super::{
    GitError, MAX_REF, Repository, fork_directory, machine_component, own_ref, record_sent,
    ref_component,
};
use crate::ids::{BranchName, CommitId, UpstreamName};
use crate::store::OwnerKind;

/// Where every claim ref lives, on the remote only.
const CLAIMS: &str = "refs/knives-claims/";
/// The one file a claim commit's tree holds.
const RECORD: &str = "claim.toml";

#[derive(Debug, thiserror::Error)]
pub enum ClaimError {
    #[error(
        "refusing branch {branch:?} on a shared ledger: its claim ref would be {bytes} bytes, \
         more than the {} a ref may be on the ledger's remote",
        MAX_REF
    )]
    TooLong { branch: String, bytes: usize },
    #[error(
        "refusing branch {branch:?} on a shared ledger: git refuses its claim ref {reference:?} \
         as a ref name"
    )]
    Refused { branch: String, reference: String },
    #[error("writing the claim record of {repo} {branch:?}: {source}")]
    Record {
        repo: String,
        branch: String,
        source: toml::ser::Error,
    },
    #[error(transparent)]
    Git(#[from] GitError),
}

/// The ref that holds one branch's claim on a shared ledger's remote:
/// `refs/knives-claims/<owner>/<name>/<branch>`.
///
/// The branch is one ref component, `%` written `%25` and `/` written `%2F`
/// and every other byte kept, so each branch has a ref of its own and no
/// claim ref is a directory of another: `a` and `a/b` spelled as two levels
/// would be a loose ref file and a directory at one path, which no
/// repository holds both of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRef(String);

impl ClaimRef {
    /// The claim ref of `branch` in the fork `repo`, the `<owner>/<name>`
    /// [`crate::config::RepoEntry::upstream_name`] gives.
    ///
    /// Refused before anything is written, with [`ClaimError::TooLong`] or
    /// [`ClaimError::Refused`], when the whole claim ref is longer than 255
    /// bytes, or `git check-ref-format` refuses it (a `..`, a space, a `~` or
    /// a `.lock` ending among its reasons): the ledger's remote could not hold
    /// such a ref, so no claim of that branch could ever be taken or
    /// released.
    pub fn new(repo: &UpstreamName, branch: &BranchName) -> Result<Self, ClaimError> {
        let repo = fork_directory(repo.clone())?;
        let reference = format!("{CLAIMS}{repo}/{}", encode(branch.as_str()));
        if reference.len() > MAX_REF {
            return Err(ClaimError::TooLong {
                branch: branch.to_string(),
                bytes: reference.len(),
            });
        }
        let mut command = crate::bind::git_command();
        command.args(["check-ref-format", &reference]);
        let output = command
            .stdin(Stdio::null())
            .output()
            .map_err(|source| GitError::Run {
                invocation: invocation(&command),
                source,
            })?;
        // A name it refuses is answered with exit 1 and nothing said.
        if output.status.code() == Some(1) {
            return Err(ClaimError::Refused {
                branch: branch.to_string(),
                reference,
            });
        }
        succeeded(&command, output)?;
        Ok(Self(reference))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ClaimRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `branch` as one ref component: `%` as `%25`, `/` as `%2F`, nothing else
/// changed, so two branches never share an encoding.
fn encode(branch: &str) -> String {
    let mut encoded = String::with_capacity(branch.len());
    for character in branch.chars() {
        match character {
            '%' => encoded.push_str("%25"),
            '/' => encoded.push_str("%2F"),
            _ => encoded.push(character),
        }
    }
    encoded
}

/// Who holds a claim, as its claim commit records it in `claim.toml`.
///
/// Read with unknown keys ignored, so a later knives may add one and every
/// knives before it still reads the claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRecord {
    /// The fork's upstream, `<owner>/<name>`.
    pub repo: String,
    pub branch: String,
    pub owner: String,
    pub kind: OwnerKind,
    pub why: String,
    /// When the claim was taken, RFC 3339 in UTC, as
    /// [`crate::store::Claim::started`] is.
    pub started: String,
    /// Where it was taken ([`crate::liveness::this_place`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
    /// The `knives.machine` of the ledger git directory that took it.
    pub machine: String,
    /// The file name of the ledger entry that recorded the change.
    pub entry: String,
}

/// A claim ref's commit, as a read found it on the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldClaim {
    pub commit: CommitId,
    /// Its record, or why its `claim.toml` does not read as one. A record
    /// that does not read is still a claim: the ref is held all the same,
    /// and only a push that names this commit replaces or deletes it.
    pub record: Result<ClaimRecord, String>,
}

/// Write the claim commit of `record`, a commit with no parent whose tree
/// holds exactly `claim.toml`; its id.
///
/// Plumbing only, as [`super::commit_pending`] commits: a blob, a tree and a
/// commit, never an index. The record's machine is author and committer,
/// with an empty email, and the commit is never signed, as a machine's own
/// commits are. It has no parent because a claim is replaced, never
/// extended: the lease that replaces it names this commit alone. The
/// record's entry name is unique to the nanosecond, so no two claim commits
/// are one, and a lease meant for one claim never matches an older one.
pub fn write_claim(repository: &Repository, record: &ClaimRecord) -> Result<CommitId, ClaimError> {
    let machine = machine_component(&record.machine)?;
    let text = toml::to_string(record).map_err(|source| ClaimError::Record {
        repo: record.repo.clone(),
        branch: record.branch.clone(),
        source,
    })?;
    let mut command = repository.git();
    command.args(["hash-object", "-w", "--stdin"]);
    let answer = run_with_input(&mut command, text.as_bytes())?;
    let blob = one_id(&command, &answer)?;
    let (mode, kind) = Mode::File.spelled();
    let listing = format!("{mode} {kind} {}\t{RECORD}\0", blob.as_str());
    let mut command = repository.git();
    command.args(["mktree", "-z"]);
    let answer = run_with_input(&mut command, listing.as_bytes())?;
    let tree = one_id(&command, &answer)?;
    let mut command = repository.git();
    command.args([
        "commit-tree",
        "--no-gpg-sign",
        "-m",
        &format!("{machine}: claim {} {}", record.repo, record.branch),
        tree.as_str(),
    ]);
    for (variable, value) in [
        ("GIT_AUTHOR_NAME", machine),
        ("GIT_AUTHOR_EMAIL", ""),
        ("GIT_COMMITTER_NAME", machine),
        ("GIT_COMMITTER_EMAIL", ""),
    ] {
        command.env(variable, value);
    }
    let answer = run(&mut command)?;
    let commit = one_id(&command, &answer)?;
    Ok(CommitId::new(commit.0))
}

/// The one object id `command` printed.
fn one_id(command: &std::process::Command, answer: &[u8]) -> Result<ObjectId, GitError> {
    std::str::from_utf8(answer)
        .ok()
        .and_then(|text| ObjectId::parse(text.trim_end()))
        .ok_or_else(|| output_error(command, "exactly one object id"))
}

/// The claim `remote` holds at `claim`, if it holds one.
///
/// `git ls-remote` names the ref's commit, and a fetch of that one ref then
/// takes its objects with no name for them: no `FETCH_HEAD`, and, since
/// `--refmap=` sets aside the refspecs the remote is configured with, no
/// remote-tracking ref either, so no local ref is ever written under
/// `refs/knives-claims*`. The objects matter beyond the record: a push that
/// leases on this commit hands pre-push hooks `<new> ^<old>` to walk, which
/// fails when the old commit is not here. Both run with the transport's
/// deadline and low-speed limits, and a read that fails is a [`GitError`]
/// with git's stderr.
pub fn read_claim(
    repository: &Repository,
    remote: &str,
    claim: &ClaimRef,
) -> Result<Option<HeldClaim>, GitError> {
    let remote = ref_component("remote", remote)?;
    let Some(commit) = remote_refs(repository, remote, &[claim.as_str()])?.remove(claim.as_str())
    else {
        return Ok(None);
    };
    let mut command = repository.git();
    command.args([
        "fetch",
        "--no-tags",
        "--no-write-fetch-head",
        "--refmap=",
        remote,
        claim.as_str(),
    ]);
    transport_transcript(&mut command)?.answer(&command)?;
    let record = record_of(repository, &commit)?;
    Ok(Some(HeldClaim { commit, record }))
}

/// Each of `names` that `remote` has, with its commit: exactly those refs,
/// since `ls-remote` also lists any ref whose name merely ends in one.
pub(super) fn remote_refs(
    repository: &Repository,
    remote: &str,
    names: &[&str],
) -> Result<BTreeMap<String, CommitId>, GitError> {
    let mut command = repository.git();
    command.args(["ls-remote", remote]).args(names);
    let answer = transport_transcript(&mut command)?.answer(&command)?;
    std::str::from_utf8(&answer)
        .ok()
        .and_then(|text| {
            text.lines()
                .map(|line| {
                    let (id, name) = line.split_once('\t')?;
                    Some((name, ObjectId::parse(id)?))
                })
                .collect::<Option<Vec<_>>>()
        })
        .map(|listed| {
            listed
                .into_iter()
                .filter(|(name, _)| names.contains(name))
                .map(|(name, id)| (name.to_owned(), CommitId::new(id.0)))
                .collect()
        })
        .ok_or_else(|| output_error(&command, "an object id and a ref name per line"))
}

/// The record `commit` carries, or why it carries none that reads.
fn record_of(
    repository: &Repository,
    commit: &CommitId,
) -> Result<Result<ClaimRecord, String>, GitError> {
    let mut command = repository.git();
    command.args(["ls-tree", "-z", commit.as_str(), "--", RECORD]);
    let answer = run(&mut command)?;
    let Some(listed) = answer
        .split(|&byte| byte == 0)
        .find(|item| !item.is_empty())
    else {
        return Ok(Err(format!("its tree holds no {RECORD}")));
    };
    let item = tree_item(listed)
        .ok_or_else(|| output_error(&command, "`<mode> <type> <id>\\t<path>` records"))?;
    if item.mode != Mode::File {
        return Ok(Err(format!("its {RECORD} is not a regular file")));
    }
    let mut command = repository.git();
    command.args(["cat-file", "blob", item.oid.as_str()]);
    Ok(parse_record(&run(&mut command)?))
}

/// `claim.toml`'s bytes as a record, or why they are not one.
fn parse_record(bytes: &[u8]) -> Result<ClaimRecord, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| format!("its {RECORD} is not UTF-8: {error}"))?;
    toml::from_str(text).map_err(|error: toml::de::Error| {
        format!(
            "its {RECORD} does not read as a claim: {}",
            error.message().trim_end()
        )
    })
}

/// One claim push: the claim ref's change and this machine's ref, together.
#[derive(Debug, Clone, Copy)]
pub struct ClaimPush<'a> {
    /// The configured remote that carries the ledger.
    pub remote: &'a str,
    /// This machine's `knives.machine`.
    pub machine: &'a str,
    pub claim: &'a ClaimRef,
    /// The claim commit the remote must hold for the push to land; `None`:
    /// the claim ref must be absent.
    pub expected: Option<&'a CommitId>,
    /// What the claim ref becomes; `None` deletes it.
    pub new: Option<&'a CommitId>,
    /// This machine's ref before the caller committed the entry that records
    /// this change (P); `None` when it had no ref yet.
    pub before: Option<&'a CommitId>,
    /// This machine's ref after that commit (N), which the push sends.
    pub after: &'a CommitId,
}

/// The remote's claim ref, as read just after a claim push that did not
/// land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimSeen {
    /// The read failed, with this text: what the ref holds is not known.
    Unread(String),
    Absent,
    At(CommitId),
}

/// Why git refused a claim push that applied nothing, as git's words say.
///
/// Whether the claim was lost is the caller's to judge from [`ClaimSeen`],
/// what the remote's claim ref holds now: git words a lost lease
/// differently by version and by server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The claim ref was not what the lease expected: stale, already
    /// there, at another commit, gone, or the ref of an atomic transaction
    /// that failed.
    Lease,
    /// This machine's ref was refused as not a fast-forward, or created
    /// first by another push: another machine may be committing under the
    /// same name, as [`GitError::MachineNameCollision`] says.
    MachineRef,
    /// Anything else that applied nothing: a hook that declined, a remote
    /// that could not be reached or refused access, a pre-push hook here
    /// that refused.
    Other,
}

/// What came of a [`push_claim`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimPushed {
    /// Both refs moved: the claim ref holds the new commit, or is gone, and
    /// the remote's machine ref is N, recorded as the remote's copy.
    Landed,
    /// Nothing was applied, and this machine's ref is back at P.
    Refused {
        refusal: Refusal,
        claim: ClaimSeen,
        /// git's own account, to show escaped: it quotes the remote.
        account: String,
    },
    /// The remote may have applied the push or not. This machine's ref stays
    /// at N.
    Unknown {
        claim: ClaimSeen,
        /// git's own account, to show escaped: it quotes the remote.
        account: String,
    },
}

/// Push a claim's change and this machine's ref in one atomic update.
///
/// `git push --atomic --porcelain --no-follow-tags <remote>
/// --force-with-lease=<claim ref>:<expected> refs/knives/<m>:refs/knives/<m>
/// <new>:<claim ref>`: the claim ref moves only from the commit the caller
/// read, or from absent, and the machine ref is never forced, so its push
/// is a fast-forward as every other push of it is. Under `--atomic` the two
/// refs move together or not at all.
///
/// What came of it, in this order:
///
/// 1. git exited zero and its porcelain lines accept both refs: landed.
/// 2. Otherwise the remote is asked for both refs. When its machine ref is
///    N, the push landed: the two refs move together, and N is the commit
///    the caller just made, so no other writer put it there. That covers a
///    remote that applied the push and then hung up or reported a failure.
///    The test needs N to be new: when `before` is N, it is skipped.
/// 3. A porcelain line marked `!` is a refusal, since under `--atomic` one
///    refused ref means none was applied, unless it is `[remote failure]`:
///    the remote took the commands and did not say what it did with them,
///    which step 4 covers. So is git stopping before the remote's
///    receive-pack took any command, as its stderr says with nothing on
///    stdout. The machine ref is then moved back to P
///    ([`restore_machine_ref`]), and the claim ref as step 2 read it goes
///    with the answer.
/// 4. Anything else, the deadline included, is an unknown outcome, and the
///    machine ref stays at N: the remote may still apply the push, and only
///    N keeps this machine's ref a fast-forward of whatever it applied.
///
/// Once landed, N is recorded as the remote's copy of this machine's ref,
/// as [`super::push`] records it.
pub fn push_claim(repository: &Repository, push: &ClaimPush<'_>) -> Result<ClaimPushed, GitError> {
    let remote = ref_component("remote", push.remote)?;
    let machine = own_ref(push.machine)?;
    let claim = push.claim.as_str();
    let mut command = repository.git();
    command.args([
        "push",
        "--atomic",
        "--porcelain",
        "--no-follow-tags",
        remote,
        &format!(
            "--force-with-lease={claim}:{}",
            push.expected.map_or("", CommitId::as_str)
        ),
        &format!("{machine}:{machine}"),
        &format!("{}:{claim}", push.new.map_or("", CommitId::as_str)),
    ]);
    let transcript = transport_transcript(&mut command)?;
    let stdout = String::from_utf8_lossy(&transcript.stdout);
    let stderr = String::from_utf8_lossy(&transcript.stderr);
    let said = Said {
        code: transcript.status.and_then(|status| status.code()),
        stdout: &stdout,
        stderr: &stderr,
    };
    let verdict = verdict(said, claim, &machine);
    let sent =
        || record_sent(repository, remote, push.machine, push.after).map(|()| ClaimPushed::Landed);
    if verdict == Verdict::Accepted {
        return sent();
    }
    let remote_now = remote_refs(repository, remote, &[claim, &machine]);
    if push.before != Some(push.after)
        && remote_now
            .as_ref()
            .is_ok_and(|refs| refs.get(&machine) == Some(push.after))
    {
        return sent();
    }
    let seen = remote_now.map_or_else(
        |error| ClaimSeen::Unread(error.to_string()),
        |mut refs| refs.remove(claim).map_or(ClaimSeen::Absent, ClaimSeen::At),
    );
    let account = transcript.account(&command);
    if let Verdict::Refused(refusal) = verdict {
        restore_machine_ref(repository, push.machine, push.before, push.after)?;
        Ok(ClaimPushed::Refused {
            refusal,
            claim: seen,
            account,
        })
    } else {
        Ok(ClaimPushed::Unknown {
            claim: seen,
            account,
        })
    }
}

/// Move this machine's ref back from `after` (N) to `before` (P), or delete
/// it when there was no P: what a claim push that applied nothing leaves.
///
/// A compare-and-swap against N, so a ref something else moved meanwhile is
/// left as it is and the call fails loudly. Never for a push whose outcome
/// is unknown: the remote may still apply it, and only N keeps this
/// machine's ref a fast-forward of whatever it applied.
pub fn restore_machine_ref(
    repository: &Repository,
    machine: &str,
    before: Option<&CommitId>,
    after: &CommitId,
) -> Result<(), GitError> {
    let reference = own_ref(machine)?;
    let mut command = repository.git();
    // `update-ref [-d] <ref> [<P>] <N>`: set to P, or delete, only from N.
    command.arg("update-ref");
    if before.is_none() {
        command.arg("-d");
    }
    command
        .arg(&reference)
        .args(before.map(CommitId::as_str))
        .arg(after.as_str());
    run(&mut command)?;
    Ok(())
}

/// What a claim push printed, and how it exited: `None` when it did not
/// exit by itself (the deadline, or a signal, ended it).
#[derive(Debug, Clone, Copy)]
struct Said<'a> {
    code: Option<i32>,
    stdout: &'a str,
    stderr: &'a str,
}

/// What git's own output says of a claim push, before the remote is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Accepted,
    Refused(Refusal),
    Unknown,
}

/// One porcelain status line, `<flag>\t<from>:<to>\t<summary>`.
#[derive(Debug)]
struct Status<'a> {
    flag: char,
    to: &'a str,
    summary: &'a str,
}

/// Reasons a refused machine ref gives when another push moved or made it:
/// git's own, and a server's lock failure on it.
const COLLISION: &[&str] = &[
    "(non-fast-forward)",
    "(fetch first)",
    "reference already exists",
    " but expected ",
];
/// Reasons a refused claim ref gives when it was not what the lease expected,
/// as git and GitHub word them. Under `--atomic` git marks every ref of a
/// failed transaction `(atomic transaction failed)`, naming the cause only on
/// a `remote:` line.
const LEASE: &[&str] = &[
    "(stale info)",
    "reference already exists",
    " but expected ",
    "unable to resolve reference",
    "(atomic transaction failed)",
    "(failed)",
];
/// What git says when the connection broke after the remote may have taken
/// the push.
const HUNG_UP: &[&str] = &[
    "the remote end hung up unexpectedly",
    "RPC failed",
    "unexpected disconnect",
];
/// What git says when it stopped before the remote's receive-pack took any
/// command.
const UNREACHED: &[&str] = &[
    // curl's `Could not resolve host` and ssh's `Could not resolve hostname`.
    "Could not resolve host",
    "Failed to connect to",
    "Connection refused",
    "Connection timed out",
    "does not appear to be a git repository",
    "Repository not found",
    "Could not read from remote repository",
    "Permission denied (publickey",
    "Authentication failed",
    // With nothing on stdout: a pre-push hook here refused, after the ref
    // advertisement and before any command went to the remote.
    "failed to push some refs",
];

/// What a claim push's own output says of it.
fn verdict(said: Said<'_>, claim: &str, machine: &str) -> Verdict {
    let statuses = statuses(said.stdout);
    if statuses
        .iter()
        .any(|status| status.flag == '!' && status.summary.starts_with("[remote failure]"))
    {
        return Verdict::Unknown;
    }
    if statuses.iter().any(|status| status.flag == '!') {
        return Verdict::Refused(refusal(&statuses, said.stderr, claim, machine));
    }
    let reported = |name: &str| statuses.iter().any(|status| status.to == name);
    if said.code == Some(0) && reported(claim) && reported(machine) {
        Verdict::Accepted
    } else if said.code.is_some_and(|code| code != 0) && stopped_before_receive_pack(said) {
        Verdict::Refused(Refusal::Other)
    } else {
        Verdict::Unknown
    }
}

/// Every porcelain status line of `stdout`: not the `To <url>` line before
/// them, nor the `Done` after.
fn statuses(stdout: &str) -> Vec<Status<'_>> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let mut flag = fields.next()?.chars();
            let (Some(flag), None) = (flag.next(), flag.next()) else {
                return None;
            };
            let (_, to) = fields.next()?.split_once(':')?;
            let summary = fields.next()?;
            " +-*=!"
                .contains(flag)
                .then_some(Status { flag, to, summary })
        })
        .collect()
}

/// Which ref a refused claim push was refused on, as git's words say.
fn refusal(statuses: &[Status<'_>], stderr: &str, claim: &str, machine: &str) -> Refusal {
    let refused = |name: &str, reasons: &[&str]| {
        statuses.iter().any(|status| {
            status.flag == '!'
                && status.to == name
                && reasons.iter().any(|reason| status.summary.contains(reason))
        })
    };
    let lock = format!("cannot lock ref '{machine}': ");
    let machine_lock_lost = stderr.lines().any(|line| {
        line.split_once(&lock)
            .is_some_and(|(_, reason)| COLLISION.iter().any(|words| reason.contains(words)))
    });
    if machine_lock_lost || refused(machine, COLLISION) {
        Refusal::MachineRef
    } else if refused(claim, LEASE) {
        Refusal::Lease
    } else {
        Refusal::Other
    }
}

/// Whether git says it stopped before the remote's receive-pack took any
/// command: it could not reach the remote, the remote refused access when
/// asked for its refs, or a pre-push hook here refused.
///
/// Only with nothing on stdout: git prints its porcelain report, `Done`
/// included, once it got further than that. A hang-up said anywhere leaves
/// the outcome open.
fn stopped_before_receive_pack(said: Said<'_>) -> bool {
    said.stdout.trim().is_empty()
        && !HUNG_UP.iter().any(|words| said.stderr.contains(words))
        && said
            .stderr
            .lines()
            .any(|line| UNREACHED.iter().any(|words| line.contains(words)) || refused_access(line))
}

/// Whether `line` is curl's answer to an HTTP 401, 403 or 5xx.
fn refused_access(line: &str) -> bool {
    line.split_once("The requested URL returned error: ")
        .and_then(|(_, code)| code.get(..3))
        .is_some_and(|code| {
            matches!(code, "401" | "403")
                || (code.starts_with('5') && code.bytes().all(|byte| byte.is_ascii_digit()))
        })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    #![allow(
        clippy::significant_drop_tightening,
        reason = "a lab holds the environment lock to the end of its test, past its last use"
    )]
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use super::*;
    // `ClaimRef::new` runs `git check-ref-format`, so naming a claim ref spawns git.
    use crate::config::test_support::environment_lock;

    const FORK: &str = "example/fork";
    const CLAIM: &str = "refs/knives-claims/example/fork/feat%2Fa";
    const MACHINE: &str = "refs/knives/machine-one";
    const COPY: &str = "refs/knives-remotes/origin/machine-one";

    /// `git --git-dir <git_dir> <args>` with `input` on its stdin, which must
    /// succeed; its stdout, trimmed.
    fn git_in(git_dir: &Path, args: &[&str], input: &[u8]) -> String {
        let mut child = crate::bind::git_command()
            .arg("--git-dir")
            .arg(git_dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "lab")
            .env("GIT_AUTHOR_EMAIL", "lab@example.invalid")
            .env("GIT_COMMITTER_NAME", "lab")
            .env("GIT_COMMITTER_EMAIL", "lab@example.invalid")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn git(git_dir: &Path, args: &[&str]) -> String {
        git_in(git_dir, args, b"")
    }

    /// `git init --bare` at `git_dir`, its parents made as needed.
    fn init_bare(git_dir: &Path) {
        let status = crate::bind::git_command()
            .args(["init", "--quiet", "--bare"])
            .arg(git_dir)
            .status()
            .unwrap();
        assert!(status.success(), "git init --bare {}", git_dir.display());
    }

    /// The commit `reference` holds in `git_dir`, if it exists.
    fn at(git_dir: &Path, reference: &str) -> Option<CommitId> {
        let id = git(
            git_dir,
            &["for-each-ref", "--format=%(objectname)", reference],
        );
        (!id.is_empty()).then(|| CommitId::new(id))
    }

    /// Every ref in `git_dir`, by name.
    fn refs(git_dir: &Path) -> Vec<String> {
        git(git_dir, &["for-each-ref", "--format=%(refname)"])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// A new commit of the empty tree in `git_dir`.
    fn commit(git_dir: &Path, message: &str, parent: Option<&CommitId>) -> CommitId {
        let tree = git(git_dir, &["mktree"]);
        let mut args = vec!["commit-tree", "-m", message, tree.as_str()];
        if let Some(parent) = parent {
            args.extend(["-p", parent.as_str()]);
        }
        CommitId::new(git(git_dir, &args))
    }

    /// A scratch directory holding the ledger's remote, `remote.git`, and
    /// each machine's git directory. It holds the environment lock until it
    /// drops.
    struct Lab {
        dir: tempfile::TempDir,
        remote: PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Lab {
        fn new() -> Self {
            let lock = environment_lock();
            let dir = tempfile::tempdir().unwrap();
            let remote = dir.path().join("remote.git");
            init_bare(&remote);
            Self {
                dir,
                remote,
                _lock: lock,
            }
        }

        /// A machine's ledger git directory, whose `origin` is the remote.
        fn machine(&self, name: &str) -> Repository {
            let git_dir = self.dir.path().join(name).join("ledger.git");
            init_bare(&git_dir);
            git(
                &git_dir,
                &["remote", "add", "origin", self.remote.to_str().unwrap()],
            );
            Repository::new(
                &git_dir,
                &self.dir.path().join(name).join("ledger"),
                [UpstreamName::new(FORK)],
            )
            .unwrap()
        }

        /// `record`'s claim commit at the claim ref on the remote, as
        /// another machine's landed push leaves it.
        fn held(&self, record: &ClaimRecord) -> CommitId {
            let remote = Repository::new(
                &self.remote,
                &self.dir.path().join("unused"),
                [UpstreamName::new(FORK)],
            )
            .unwrap();
            let commit = write_claim(&remote, record).unwrap();
            git(&self.remote, &["update-ref", CLAIM, commit.as_str()]);
            commit
        }

        /// The remote's hook `name`, running `script`.
        fn hook(&self, name: &str, script: &str) {
            let hooks = self.dir.path().join("remote-hooks");
            std::fs::create_dir_all(&hooks).unwrap();
            let path = hooks.join(name);
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            git(
                &self.remote,
                &["config", "core.hooksPath", hooks.to_str().unwrap()],
            );
        }
    }

    fn record(machine: &str, owner: &str, entry: &str) -> ClaimRecord {
        ClaimRecord {
            repo: FORK.to_owned(),
            branch: "feat/a".to_owned(),
            owner: owner.to_owned(),
            kind: OwnerKind::HarnessSession,
            why: "port the parser".to_owned(),
            started: "2026-11-16T08:00:00Z".to_owned(),
            place: Some("box-one/4026531836".to_owned()),
            machine: machine.to_owned(),
            entry: entry.to_owned(),
        }
    }

    fn claim_ref() -> ClaimRef {
        ClaimRef::new(&UpstreamName::new(FORK), &BranchName::new("feat/a")).unwrap()
    }

    /// Move machine one's ref to a new commit N, a child of what it held (P),
    /// as the caller's commit of pending entries does before the push.
    fn commit_entries(repository: &Repository, message: &str) -> (Option<CommitId>, CommitId) {
        let git_dir = repository.git_dir();
        let before = at(git_dir, MACHINE);
        let after = commit(git_dir, message, before.as_ref());
        git(
            git_dir,
            &[
                "update-ref",
                MACHINE,
                after.as_str(),
                before.as_ref().map_or("", CommitId::as_str),
            ],
        );
        (before, after)
    }

    fn push(
        repository: &Repository,
        expected: Option<&CommitId>,
        new: Option<&CommitId>,
        (before, after): &(Option<CommitId>, CommitId),
    ) -> ClaimPushed {
        push_claim(
            repository,
            &ClaimPush {
                remote: "origin",
                machine: "machine-one",
                claim: &claim_ref(),
                expected,
                new,
                before: before.as_ref(),
                after,
            },
        )
        .unwrap()
    }

    /// The refusal and the claim seen of a push that must have been refused.
    fn refused(pushed: ClaimPushed) -> (Refusal, ClaimSeen) {
        match pushed {
            ClaimPushed::Refused { refusal, claim, .. } => (refusal, claim),
            other => panic!("not refused: {other:?}"),
        }
    }

    const ENTRY_ONE: &str = "20261116T080000.000000001Z-a1b2.md";
    const ENTRY_TWO: &str = "20261116T080000.000000002Z-c3d4.md";

    #[test]
    fn a_branch_is_one_ref_component_with_percent_and_slash_written_out() {
        let _lock = environment_lock();
        for (branch, reference) in [
            ("main", "refs/knives-claims/example/fork/main"),
            ("feat/a", "refs/knives-claims/example/fork/feat%2Fa"),
            ("100%/done", "refs/knives-claims/example/fork/100%25%2Fdone"),
            ("a%2Fb", "refs/knives-claims/example/fork/a%252Fb"),
            ("é-ü", "refs/knives-claims/example/fork/é-ü"),
        ] {
            let claim = ClaimRef::new(&UpstreamName::new(FORK), &BranchName::new(branch)).unwrap();
            assert_eq!(claim.as_str(), reference, "{branch}");
        }
    }

    #[test]
    fn branches_a_and_a_slash_b_get_two_refs_neither_a_directory_of_the_other() {
        let _lock = environment_lock();
        // Given: the claim refs of `a` and `a/b`.
        let fork = UpstreamName::new(FORK);
        let a = ClaimRef::new(&fork, &BranchName::new("a")).unwrap();
        let nested = ClaimRef::new(&fork, &BranchName::new("a/b")).unwrap();
        assert_ne!(a, nested);

        // When: a repository holds both.
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join("scratch.git");
        init_bare(&git_dir);
        let held = commit(&git_dir, "held", None);
        for claim in [&a, &nested] {
            git(&git_dir, &["update-ref", claim.as_str(), held.as_str()]);
        }

        // Then: both are there, each its own ref.
        assert_eq!(refs(&git_dir), [a.to_string(), nested.to_string()]);
    }

    #[test]
    fn a_branch_whose_whole_claim_ref_passes_255_bytes_is_refused() {
        let _lock = environment_lock();
        // Given: the fork's claim refs, whose `refs/knives-claims/example/fork/`
        // is 32 bytes, so 223 are left for the branch.
        let fork = UpstreamName::new(FORK);
        let claim = |branch: String| ClaimRef::new(&fork, &BranchName::new(branch));

        // Then: a branch whose claim ref is 255 bytes has it, whether a plain
        // byte or a `/`, three bytes once written out, ends the branch.
        for longest in ["a".repeat(223), format!("{}/", "a".repeat(220))] {
            let reference = claim(longest.clone()).unwrap();
            assert_eq!(reference.as_str().len(), 255, "{longest}");
        }
        // And: one byte more is refused, naming the 256 its ref would be.
        for branch in ["a".repeat(224), format!("{}/", "a".repeat(221))] {
            assert!(
                matches!(
                    claim(branch.clone()),
                    Err(ClaimError::TooLong { bytes: 256, .. })
                ),
                "{branch} was accepted"
            );
        }
    }

    #[test]
    fn a_branch_git_refuses_as_a_ref_name_is_refused() {
        let _lock = environment_lock();
        for branch in [
            "feat.lock",
            "a..b",
            "a b",
            "a~1",
            "a^b",
            "a:b",
            ".hidden",
            "ends.",
            "",
        ] {
            let refused = ClaimRef::new(&UpstreamName::new(FORK), &BranchName::new(branch));
            assert!(
                matches!(&refused, Err(ClaimError::Refused { reference, .. }) if *reference == format!("{CLAIMS}{FORK}/{}", encode(branch))),
                "{branch:?}: {refused:?}"
            );
        }
    }

    #[test]
    fn a_record_reads_back_as_it_was_written() {
        for record in [
            record("machine-one", "owner-one", ENTRY_ONE),
            ClaimRecord {
                place: None,
                kind: OwnerKind::OsUser,
                ..record("machine-one", "owner-one", ENTRY_ONE)
            },
        ] {
            let text = toml::to_string(&record).unwrap();
            assert_eq!(parse_record(text.as_bytes()), Ok(record));
        }
    }

    #[test]
    fn a_record_with_a_key_this_knives_does_not_know_still_reads() {
        let text = concat!(
            "repo = \"example/fork\"\n",
            "branch = \"feat/a\"\n",
            "owner = \"owner-one\"\n",
            "kind = \"workspace-derived\"\n",
            "why = \"port the parser\"\n",
            "started = \"2026-11-16T08:00:00Z\"\n",
            "machine = \"machine-one\"\n",
            "entry = \"20261116T080000.000000001Z-a1b2.md\"\n",
            "added_later = \"a key a later knives writes\"\n",
        );
        assert_eq!(
            parse_record(text.as_bytes()),
            Ok(ClaimRecord {
                kind: OwnerKind::WorkspaceDerived,
                place: None,
                ..record("machine-one", "owner-one", ENTRY_ONE)
            })
        );
    }

    #[test]
    fn a_claim_commit_has_no_parent_and_holds_only_its_record_by_its_machine() {
        // Given: machine one's claim record.
        let lab = Lab::new();
        let one = lab.machine("one");
        let record = record("machine-one", "owner-one", ENTRY_ONE);

        // When: its claim commit is written.
        let commit = write_claim(&one, &record).unwrap();

        // Then: the commit has no parent, and the machine with an empty email
        // wrote it.
        let raw = git(one.git_dir(), &["cat-file", "commit", commit.as_str()]);
        assert!(
            !raw.lines().any(|line| line.starts_with("parent ")),
            "{raw}"
        );
        assert!(
            raw.lines()
                .any(|line| line.starts_with("author machine-one <> ")),
            "{raw}"
        );
        assert!(
            raw.lines()
                .any(|line| line.starts_with("committer machine-one <> ")),
            "{raw}"
        );
        assert!(
            raw.ends_with("\n\nmachine-one: claim example/fork feat/a"),
            "{raw}"
        );
        // And: its tree is `claim.toml` alone, a regular file holding the record.
        let tree = git(one.git_dir(), &["ls-tree", commit.as_str()]);
        let items: Vec<&str> = tree.lines().collect();
        assert_eq!(items.len(), 1, "{tree}");
        assert!(
            items[0].starts_with("100644 blob ") && items[0].ends_with("\tclaim.toml"),
            "{tree}"
        );
        let text = git(
            one.git_dir(),
            &["cat-file", "blob", &format!("{commit}:claim.toml")],
        );
        assert_eq!(parse_record(text.as_bytes()), Ok(record));
    }

    #[test]
    fn a_claim_ref_the_remote_lacks_reads_as_no_claim() {
        let lab = Lab::new();
        let one = lab.machine("one");
        assert_eq!(read_claim(&one, "origin", &claim_ref()).unwrap(), None);
    }

    #[test]
    fn a_claim_read_brings_its_record_and_objects_and_writes_no_local_ref() {
        // Given: machine two's claim on the remote, and machine one's git
        // directory configured to copy every remote ref it fetches.
        let lab = Lab::new();
        let theirs = record("machine-two", "owner-two", ENTRY_TWO);
        let held = lab.held(&theirs);
        let one = lab.machine("one");
        git(
            one.git_dir(),
            &["config", "remote.origin.fetch", "+refs/*:refs/*"],
        );

        // When: machine one reads the claim.
        let read = read_claim(&one, "origin", &claim_ref()).unwrap();

        // Then: it is machine two's commit and record.
        assert_eq!(
            read,
            Some(HeldClaim {
                commit: held.clone(),
                record: Ok(theirs)
            })
        );
        // And: no ref under `refs/knives-claims` exists here, nor FETCH_HEAD.
        let local = refs(one.git_dir());
        assert!(
            !local
                .iter()
                .any(|name| name.starts_with("refs/knives-claims")),
            "{local:?}"
        );
        assert!(!one.git_dir().join("FETCH_HEAD").exists());
        // And: the commit's objects are here, for a push that leases on it.
        git(
            one.git_dir(),
            &["cat-file", "-e", &format!("{held}^{{tree}}")],
        );
    }

    #[test]
    fn a_claim_whose_record_is_not_toml_is_still_a_claim_with_the_reason() {
        // Given: a claim commit on the remote whose `claim.toml` is not TOML.
        let lab = Lab::new();
        let blob = git_in(
            &lab.remote,
            &["hash-object", "-w", "--stdin"],
            b"owner = = ?\n",
        );
        let tree = git_in(
            &lab.remote,
            &["mktree"],
            format!("100644 blob {blob}\tclaim.toml\n").as_bytes(),
        );
        let held = CommitId::new(git(&lab.remote, &["commit-tree", "-m", "claim", &tree]));
        git(&lab.remote, &["update-ref", CLAIM, held.as_str()]);
        let one = lab.machine("one");

        // When: it is read.
        let read = read_claim(&one, "origin", &claim_ref()).unwrap().unwrap();

        // Then: the read names the commit and why its record does not read.
        assert_eq!(read.commit, held);
        let reason = read.record.unwrap_err();
        assert!(
            reason.starts_with("its claim.toml does not read as a claim: "),
            "{reason}"
        );
    }

    #[test]
    fn a_take_lands_the_claim_and_the_machine_ref_together() {
        // Given: no claim on the remote, and machine one's ref moved to N.
        let lab = Lab::new();
        let one = lab.machine("one");
        let moved = commit_entries(&one, "entry one");
        let mine = write_claim(&one, &record("machine-one", "owner-one", ENTRY_ONE)).unwrap();

        // When: it pushes the take.
        let pushed = push(&one, None, Some(&mine), &moved);

        // Then: both refs are on the remote, and N is recorded as its copy.
        assert_eq!(pushed, ClaimPushed::Landed);
        assert_eq!(at(&lab.remote, CLAIM), Some(mine));
        assert_eq!(at(&lab.remote, MACHINE).as_ref(), Some(&moved.1));
        assert_eq!(at(one.git_dir(), COPY), Some(moved.1));
    }

    #[test]
    fn a_take_of_a_claim_another_machine_holds_is_refused_and_changes_nothing() {
        // Given: machine two's claim on the remote.
        let lab = Lab::new();
        let theirs = lab.held(&record("machine-two", "owner-two", ENTRY_TWO));
        let one = lab.machine("one");
        let moved = commit_entries(&one, "entry one");
        let mine = write_claim(&one, &record("machine-one", "owner-one", ENTRY_ONE)).unwrap();

        // When: machine one pushes a take, leasing on no claim.
        let pushed = push(&one, None, Some(&mine), &moved);

        // Then: it is refused on the lease, naming the claim the remote holds.
        assert_eq!(
            refused(pushed),
            (Refusal::Lease, ClaimSeen::At(theirs.clone()))
        );
        // And: the remote holds what it held, and nothing else.
        assert_eq!(refs(&lab.remote), [CLAIM]);
        assert_eq!(at(&lab.remote, CLAIM), Some(theirs));
        // And: machine one's ref is gone again, as before its commit.
        assert_eq!(at(one.git_dir(), MACHINE), None);
        assert_eq!(at(one.git_dir(), COPY), None);
    }

    #[test]
    fn a_seize_leasing_on_the_claim_it_read_lands() {
        // Given: machine two's claim, as machine one read it.
        let lab = Lab::new();
        let theirs = lab.held(&record("machine-two", "owner-two", ENTRY_TWO));
        let one = lab.machine("one");
        let read = read_claim(&one, "origin", &claim_ref()).unwrap().unwrap();
        assert_eq!(read.commit, theirs);
        let moved = commit_entries(&one, "entry one");
        let mine = write_claim(&one, &record("machine-one", "owner-one", ENTRY_ONE)).unwrap();

        // When: machine one pushes its seize, leasing on that commit.
        let pushed = push(&one, Some(&read.commit), Some(&mine), &moved);

        // Then: the claim is machine one's.
        assert_eq!(pushed, ClaimPushed::Landed);
        assert_eq!(at(&lab.remote, CLAIM), Some(mine));
        assert_eq!(at(&lab.remote, MACHINE), Some(moved.1));
    }

    #[test]
    fn a_seize_whose_lease_is_stale_is_refused() {
        // Given: machine two's claim, and a lease on another commit.
        let lab = Lab::new();
        let theirs = lab.held(&record("machine-two", "owner-two", ENTRY_TWO));
        let one = lab.machine("one");
        let stale = write_claim(&one, &record("machine-three", "owner-three", ENTRY_TWO)).unwrap();
        let moved = commit_entries(&one, "entry one");
        let mine = write_claim(&one, &record("machine-one", "owner-one", ENTRY_ONE)).unwrap();

        // When: machine one seizes, leasing on the stale commit.
        let pushed = push(&one, Some(&stale), Some(&mine), &moved);

        // Then: it is refused, the remote unchanged.
        assert_eq!(
            refused(pushed),
            (Refusal::Lease, ClaimSeen::At(theirs.clone()))
        );
        assert_eq!(refs(&lab.remote), [CLAIM]);
        assert_eq!(at(&lab.remote, CLAIM), Some(theirs));
        assert_eq!(at(one.git_dir(), MACHINE), None);
    }

    #[test]
    fn a_release_leasing_on_the_claim_deletes_it() {
        // Given: machine one's landed claim.
        let lab = Lab::new();
        let one = lab.machine("one");
        let taken = commit_entries(&one, "entry one");
        let mine = write_claim(&one, &record("machine-one", "owner-one", ENTRY_ONE)).unwrap();
        assert_eq!(push(&one, None, Some(&mine), &taken), ClaimPushed::Landed);

        // When: it releases, leasing on its claim commit.
        let released = commit_entries(&one, "entry two");
        let pushed = push(&one, Some(&mine), None, &released);

        // Then: the claim ref is gone, and the machine ref moved on.
        assert_eq!(pushed, ClaimPushed::Landed);
        assert_eq!(at(&lab.remote, CLAIM), None);
        assert_eq!(at(&lab.remote, MACHINE).as_ref(), Some(&released.1));
        assert_eq!(at(one.git_dir(), COPY), Some(released.1));
    }

    #[test]
    fn a_machine_ref_the_remote_holds_at_another_commit_refuses_the_push() {
        // Given: the remote's machine-one ref at a commit another machine
        // made under the same name.
        let lab = Lab::new();
        let elsewhere = commit(&lab.remote, "another machine's entries", None);
        git(&lab.remote, &["update-ref", MACHINE, elsewhere.as_str()]);
        let one = lab.machine("one");
        let moved = commit_entries(&one, "entry one");
        let mine = write_claim(&one, &record("machine-one", "owner-one", ENTRY_ONE)).unwrap();

        // When: machine one pushes a take.
        let pushed = push(&one, None, Some(&mine), &moved);

        // Then: it is refused on the machine ref, and nothing changed.
        assert_eq!(refused(pushed), (Refusal::MachineRef, ClaimSeen::Absent));
        assert_eq!(refs(&lab.remote), [MACHINE]);
        assert_eq!(at(&lab.remote, MACHINE), Some(elsewhere));
        assert_eq!(at(one.git_dir(), MACHINE), None);
    }

    #[test]
    fn a_push_the_remote_applied_before_it_hung_up_is_landed() {
        // Given: a remote that ends receive-pack after it updated the refs, so
        // a push there fails on the client although it landed.
        let lab = Lab::new();
        lab.hook("post-receive", "#!/bin/sh\nkill -9 \"$PPID\"\n");
        let probe = lab.machine("probe");
        let sent = commit(probe.git_dir(), "probe", None);
        let probed = crate::bind::git_command()
            .arg("--git-dir")
            .arg(probe.git_dir())
            .args(["push", "origin", &format!("{sent}:refs/heads/probe")])
            .output()
            .unwrap();
        assert!(!probed.status.success(), "the probe push succeeded");
        assert_eq!(at(&lab.remote, "refs/heads/probe"), Some(sent));
        let one = lab.machine("one");
        let moved = commit_entries(&one, "entry one");
        let mine = write_claim(&one, &record("machine-one", "owner-one", ENTRY_ONE)).unwrap();

        // When: machine one pushes a take.
        let pushed = push(&one, None, Some(&mine), &moved);

        // Then: the remote's machine ref is N, so the push landed.
        assert_eq!(pushed, ClaimPushed::Landed);
        assert_eq!(at(&lab.remote, CLAIM), Some(mine));
        assert_eq!(at(&lab.remote, MACHINE).as_ref(), Some(&moved.1));
        assert_eq!(at(one.git_dir(), COPY), Some(moved.1));
    }

    #[test]
    fn a_push_the_remote_dropped_before_any_update_is_never_landed() {
        // Given: a remote that ends receive-pack before it updates anything.
        let lab = Lab::new();
        lab.hook("pre-receive", "#!/bin/sh\nkill -9 \"$PPID\"\n");
        let one = lab.machine("one");
        let moved = commit_entries(&one, "entry one");
        let mine = write_claim(&one, &record("machine-one", "owner-one", ENTRY_ONE)).unwrap();

        // When: machine one pushes a take.
        let pushed = push(&one, None, Some(&mine), &moved);

        // Then: the outcome is unknown or refused, never landed, and the
        // remote holds nothing.
        assert_eq!(refs(&lab.remote), Vec::<String>::new());
        match pushed {
            // Unknown: machine one keeps N, a fast-forward of anything the
            // remote may still apply.
            ClaimPushed::Unknown { claim, .. } => {
                assert_eq!(claim, ClaimSeen::Absent);
                assert_eq!(at(one.git_dir(), MACHINE), Some(moved.1));
            }
            ClaimPushed::Refused { claim, .. } => {
                assert_eq!(claim, ClaimSeen::Absent);
                assert_eq!(at(one.git_dir(), MACHINE), None);
            }
            ClaimPushed::Landed => panic!("a push the remote never applied was landed"),
        }
        assert_eq!(at(one.git_dir(), COPY), None);
    }

    #[test]
    fn a_refused_push_moves_the_machine_ref_back_to_what_it_held() {
        // Given: machine one's ref moved from P to N.
        let lab = Lab::new();
        let one = lab.machine("one");
        let (_, before) = commit_entries(&one, "entry one");
        let (_, after) = commit_entries(&one, "entry two");

        // When: it is moved back.
        restore_machine_ref(&one, "machine-one", Some(&before), &after).unwrap();

        // Then: it holds P again.
        assert_eq!(at(one.git_dir(), MACHINE), Some(before));
    }

    #[test]
    fn a_machine_ref_that_did_not_exist_before_is_deleted_again() {
        let lab = Lab::new();
        let one = lab.machine("one");
        let (before, after) = commit_entries(&one, "entry one");
        assert_eq!(before, None);

        restore_machine_ref(&one, "machine-one", None, &after).unwrap();

        assert_eq!(at(one.git_dir(), MACHINE), None);
    }

    #[test]
    fn a_machine_ref_that_moved_meanwhile_is_left_alone_and_the_move_back_fails() {
        // Given: machine one's ref moved from P to N, then on to another commit.
        let lab = Lab::new();
        let one = lab.machine("one");
        let (_, before) = commit_entries(&one, "entry one");
        let (_, after) = commit_entries(&one, "entry two");
        let (_, moved) = commit_entries(&one, "entry three");

        // When: N is moved back, to P or to nothing.
        for restored in [
            restore_machine_ref(&one, "machine-one", Some(&before), &after),
            restore_machine_ref(&one, "machine-one", None, &after),
        ] {
            // Then: it fails, and the ref is where the other writer left it.
            assert!(
                matches!(restored, Err(GitError::Failed { .. })),
                "{restored:?}"
            );
            assert_eq!(at(one.git_dir(), MACHINE).as_ref(), Some(&moved));
        }
    }

    /// One claim push's whole output, and what it says of the push.
    struct Case {
        shape: &'static str,
        code: Option<i32>,
        stdout: &'static str,
        stderr: &'static str,
        verdict: Verdict,
    }

    /// GitHub's answers, from two machines racing the same refs; each copied
    /// whole, with its ref spelled as this push's claim ref or machine ref and
    /// its host as example.invalid.
    const GITHUB: &[Case] = &[
        Case {
            shape: "GitHub, a lost create race",
            code: Some(1),
            stdout: concat!(
                "To https://example.invalid/example/ledger.git\n",
                "!\tf3dc2cc7d41950a0a8b4f10fe15145add1b96573:refs/knives-claims/example/fork/feat%2Fa\t[remote rejected] (cannot lock ref 'refs/knives-claims/example/fork/feat%2Fa': reference already exists)\n",
                "Done\n",
            ),
            stderr: "error: failed to push some refs to 'https://example.invalid/example/ledger.git'\n",
            verdict: Verdict::Refused(Refusal::Lease),
        },
        Case {
            shape: "GitHub, a seize whose claim was deleted first",
            code: Some(1),
            stdout: concat!(
                "To https://example.invalid/example/ledger.git\n",
                "!\t45935ab848a6b0ba41dafa4fbd8bbbd7a77ceff1:refs/knives-claims/example/fork/feat%2Fa\t[remote rejected] (cannot lock ref 'refs/knives-claims/example/fork/feat%2Fa': unable to resolve reference 'refs/knives-claims/example/fork/feat%2Fa')\n",
                "Done\n",
            ),
            stderr: "error: failed to push some refs to 'https://example.invalid/example/ledger.git'\n",
            verdict: Verdict::Refused(Refusal::Lease),
        },
        Case {
            shape: "GitHub, a release whose claim was seized first",
            code: Some(1),
            stdout: concat!(
                "To https://example.invalid/example/ledger.git\n",
                "!\t:refs/knives-claims/example/fork/feat%2Fa\t[remote rejected] (cannot lock ref 'refs/knives-claims/example/fork/feat%2Fa': is at 1656ff1bca03f7498fa93540d45330fc5e4d83ca but expected 22169ab26b42bb763cbf6a3da6de9e4460e6c796)\n",
                "Done\n",
            ),
            stderr: "error: failed to push some refs to 'https://example.invalid/example/ledger.git'\n",
            verdict: Verdict::Refused(Refusal::Lease),
        },
        Case {
            shape: "GitHub, a lease git found stale before sending",
            code: Some(1),
            stdout: concat!(
                "To https://example.invalid/example/ledger.git\n",
                "!\tcd433827dd6e98ee9193056dd7730fe5d9954777:refs/knives-claims/example/fork/feat%2Fa\t[rejected] (stale info)\n",
                "Done\n",
            ),
            stderr: "error: failed to push some refs to 'https://example.invalid/example/ledger.git'\n",
            verdict: Verdict::Refused(Refusal::Lease),
        },
        Case {
            shape: "GitHub, the loser of an atomic race, its other ref marked new though nothing was applied",
            code: Some(1),
            stdout: concat!(
                "To https://example.invalid/example/ledger.git\n",
                "*\t419e781fa67710e4f6e3a827da149caefdc4b2b2:refs/knives/machine-one\t[new reference]\n",
                "!\t419e781fa67710e4f6e3a827da149caefdc4b2b2:refs/knives-claims/example/fork/feat%2Fa\t[remote rejected] (cannot lock ref 'refs/knives-claims/example/fork/feat%2Fa': reference already exists)\n",
                "Done\n",
            ),
            stderr: "error: failed to push some refs to 'https://example.invalid/example/ledger.git'\n",
            verdict: Verdict::Refused(Refusal::Lease),
        },
        Case {
            shape: "GitHub, the loser of an atomic race, with the remote's own errors",
            code: Some(1),
            stdout: concat!(
                "To https://example.invalid/example/ledger.git\n",
                "*\t4266ae66e82e06a62a7bc8928e3ccf469afc6d1c:refs/knives/machine-one\t[new reference]\n",
                "!\t4266ae66e82e06a62a7bc8928e3ccf469afc6d1c:refs/knives-claims/example/fork/feat%2Fa\t[remote rejected] (cannot lock ref 'refs/knives-claims/example/fork/feat%2Fa': reference already exists)\n",
                "Done\n",
            ),
            stderr: concat!(
                "remote: error: unable to write file /data/repositories/5/nw/58/16/7e/1180229016/1318902388.git/objects/ghq_okafifNViSh/pack/pack-daf6b323ce72245687e45f7d4ccc1ec9025719fe.pack: No such file or directory        \n",
                "remote: fatal: unable to rename temporary '*.pack' file to '/data/repositories/5/nw/58/16/7e/1180229016/1318902388.git/objects/ghq_okafifNViSh/pack/pack-daf6b323ce72245687e45f7d4ccc1ec9025719fe.pack'        \n",
                "error: failed to push some refs to 'https://example.invalid/example/ledger.git'\n",
            ),
            verdict: Verdict::Refused(Refusal::Lease),
        },
        Case {
            shape: "GitHub, the winner of an atomic race",
            code: Some(0),
            stdout: concat!(
                "To https://example.invalid/example/ledger.git\n",
                "*\t8a9ad30d31256c6c7b56b1bf7671d1206a353fd6:refs/knives-claims/example/fork/feat%2Fa\t[new reference]\n",
                "*\t8a9ad30d31256c6c7b56b1bf7671d1206a353fd6:refs/knives/machine-one\t[new reference]\n",
                "Done\n",
            ),
            stderr: "",
            verdict: Verdict::Accepted,
        },
        Case {
            shape: "GitHub, an HTTP 403 on the ref advertisement",
            code: Some(128),
            stdout: "",
            stderr: concat!(
                "remote: Write access to repository not granted.\n",
                "fatal: unable to access 'https://example.invalid/example/ledger.git/': The requested URL returned error: 403\n",
            ),
            verdict: Verdict::Refused(Refusal::Other),
        },
    ];
    /// Three reasons a receive-pack gives that the races did not meet: each
    /// line as git prints it.
    const REASONS: &[Case] = &[
        Case {
            shape: "a remote that did not report on the refs `[remote failure]`",
            code: Some(1),
            stdout: concat!(
                "To https://example.invalid/example/ledger.git\n",
                "!\trefs/knives/machine-one:refs/knives/machine-one\t[remote failure] (remote failed to report status)\n",
                "!\t8a9ad30d31256c6c7b56b1bf7671d1206a353fd6:refs/knives-claims/example/fork/feat%2Fa\t[remote failure] (remote failed to report status)\n",
                "Done\n",
            ),
            stderr: "error: failed to push some refs to 'https://example.invalid/example/ledger.git'\n",
            verdict: Verdict::Unknown,
        },
        Case {
            shape: "a remote rejection `(failed)`",
            code: Some(1),
            stdout: concat!(
                "To https://example.invalid/example/ledger.git\n",
                "!\trefs/knives/machine-one:refs/knives/machine-one\t[remote rejected] (failed)\n",
                "!\t8a9ad30d31256c6c7b56b1bf7671d1206a353fd6:refs/knives-claims/example/fork/feat%2Fa\t[remote rejected] (failed)\n",
                "Done\n",
            ),
            stderr: "error: failed to push some refs to 'https://example.invalid/example/ledger.git'\n",
            verdict: Verdict::Refused(Refusal::Lease),
        },
        Case {
            shape: "a remote rejection `(missing necessary objects)`",
            code: Some(1),
            stdout: concat!(
                "To https://example.invalid/example/ledger.git\n",
                "!\trefs/knives/machine-one:refs/knives/machine-one\t[remote rejected] (missing necessary objects)\n",
                "!\t8a9ad30d31256c6c7b56b1bf7671d1206a353fd6:refs/knives-claims/example/fork/feat%2Fa\t[remote rejected] (missing necessary objects)\n",
                "Done\n",
            ),
            stderr: "error: failed to push some refs to 'https://example.invalid/example/ledger.git'\n",
            verdict: Verdict::Refused(Refusal::Other),
        },
    ];
    /// Local git 2.47's answers to this very push, each copied whole with its
    /// remote's path as a neutral one.
    const LOCAL: &[Case] = &[
        Case {
            shape: "a take that lands",
            code: Some(0),
            stdout: concat!(
                "To /tmp/.tmpXYvBbh/ledger.git\n",
                "*\trefs/knives/machine-one:refs/knives/machine-one\t[new reference]\n",
                "*\td25175424efc8e335d2dd2092d608d39c41b1608:refs/knives-claims/example/fork/feat%2Fa\t[new reference]\n",
                "Done\n",
            ),
            stderr: "",
            verdict: Verdict::Accepted,
        },
        Case {
            shape: "a seize that lands",
            code: Some(0),
            stdout: concat!(
                "To /tmp/.tmpXYvBbh/ledger.git\n",
                "+\ta3f3b5b149b20aac43d51e2a85f2524cde9ca5c6:refs/knives-claims/example/fork/feat%2Fa\t3914180...a3f3b5b (forced update)\n",
                " \trefs/knives/machine-one:refs/knives/machine-one\tbfe2ea8..f373717\n",
                "Done\n",
            ),
            stderr: "",
            verdict: Verdict::Accepted,
        },
        Case {
            shape: "a release that lands",
            code: Some(0),
            stdout: concat!(
                "To /tmp/.tmpXYvBbh/ledger.git\n",
                "-\t:refs/knives-claims/example/fork/feat%2Fa\t[deleted]\n",
                " \trefs/knives/machine-one:refs/knives/machine-one\t13c49b2..b673d45\n",
                "Done\n",
            ),
            stderr: "",
            verdict: Verdict::Accepted,
        },
        Case {
            shape: "a take of a claim the remote advertised, though git says it hung up",
            code: Some(1),
            stdout: concat!(
                "To /tmp/.tmpXYvBbh/ledger.git\n",
                "!\t968b683080ce99d833c71dcc9a4181d9fc6e231f:refs/knives-claims/example/fork/feat%2Fa\t[rejected] (stale info)\n",
                "!\trefs/knives/machine-one:refs/knives/machine-one\t[rejected] (atomic push failed)\n",
                "Done\n",
            ),
            stderr: concat!(
                "error: atomic push failed for ref refs/knives-claims/example/fork/feat%2Fa. status: 7\n",
                "fatal: the remote end hung up unexpectedly\n",
                "error: failed to push some refs to '/tmp/.tmpXYvBbh/ledger.git'\n",
            ),
            verdict: Verdict::Refused(Refusal::Lease),
        },
        Case {
            shape: "a release of a claim already gone",
            code: Some(1),
            stdout: concat!(
                "To /tmp/.tmpXYvBbh/ledger.git\n",
                "!\trefs/knives/machine-one:refs/knives/machine-one\t[rejected] (atomic push failed)\n",
                "!\t(delete):refs/knives-claims/example/fork/feat%2Fa\t[rejected] (stale info)\n",
                "Done\n",
            ),
            stderr: concat!(
                "error: atomic push failed for ref refs/knives-claims/example/fork/feat%2Fa. status: 7\n",
                "fatal: the remote end hung up unexpectedly\n",
                "error: failed to push some refs to '/tmp/.tmpXYvBbh/ledger.git'\n",
            ),
            verdict: Verdict::Refused(Refusal::Lease),
        },
        Case {
            shape: "a lost create race on the claim ref, named only on a remote line",
            code: Some(1),
            stdout: concat!(
                "To /tmp/.tmpXYvBbh/ledger.git\n",
                "!\trefs/knives/machine-one:refs/knives/machine-one\t[remote rejected] (atomic transaction failed)\n",
                "!\t7769a2e7233ea1de6617492fee87c6f591164374:refs/knives-claims/example/fork/feat%2Fa\t[remote rejected] (atomic transaction failed)\n",
                "Done\n",
            ),
            stderr: concat!(
                "remote: error: cannot lock ref 'refs/knives-claims/example/fork/feat%2Fa': reference already exists        \n",
                "error: failed to push some refs to '/tmp/.tmpXYvBbh/ledger.git'\n",
            ),
            verdict: Verdict::Refused(Refusal::Lease),
        },
        Case {
            shape: "a seize whose claim moved after git read it",
            code: Some(1),
            stdout: concat!(
                "To /tmp/.tmpXYvBbh/ledger.git\n",
                "!\taee3129f6a8ff6da174369054a65c9602aad1a1e:refs/knives-claims/example/fork/feat%2Fa\t[remote rejected] (atomic transaction failed)\n",
                "!\trefs/knives/machine-one:refs/knives/machine-one\t[remote rejected] (atomic transaction failed)\n",
                "Done\n",
            ),
            stderr: concat!(
                "remote: error: cannot lock ref 'refs/knives-claims/example/fork/feat%2Fa': is at 314374c7a2b93018c0c8b6d3fdf692d5762ce325 but expected 55db826db2327be9af9bfbbf9afc9480431a6f94        \n",
                "error: failed to push some refs to '/tmp/.tmpXYvBbh/ledger.git'\n",
            ),
            verdict: Verdict::Refused(Refusal::Lease),
        },
        Case {
            shape: "a lost create race on the machine ref, named only on a remote line",
            code: Some(1),
            stdout: concat!(
                "To /tmp/.tmpXYvBbh/ledger.git\n",
                "!\trefs/knives/machine-one:refs/knives/machine-one\t[remote rejected] (atomic transaction failed)\n",
                "!\tb4df0893b8a7b28d2363ae3dd91b71a6dc94cf08:refs/knives-claims/example/fork/feat%2Fa\t[remote rejected] (atomic transaction failed)\n",
                "Done\n",
            ),
            stderr: concat!(
                "remote: error: cannot lock ref 'refs/knives/machine-one': reference already exists        \n",
                "error: failed to push some refs to '/tmp/.tmpXYvBbh/ledger.git'\n",
            ),
            verdict: Verdict::Refused(Refusal::MachineRef),
        },
        Case {
            shape: "a machine ref behind the remote's",
            code: Some(1),
            stdout: concat!(
                "To /tmp/.tmpXYvBbh/ledger.git\n",
                "!\trefs/knives/machine-one:refs/knives/machine-one\t[rejected] (non-fast-forward)\n",
                "!\t3a7ccf5b5e8418dd847dbf7985514d8d66d0e3c3:refs/knives-claims/example/fork/feat%2Fa\t[rejected] (atomic push failed)\n",
                "Done\n",
            ),
            stderr: concat!(
                "error: atomic push failed for ref refs/knives/machine-one. status: 2\n",
                "fatal: the remote end hung up unexpectedly\n",
                "error: failed to push some refs to '/tmp/.tmpXYvBbh/ledger.git'\n",
                "hint: Updates were rejected because a pushed branch tip is behind its remote\n",
                "hint: counterpart. If you want to integrate the remote changes, use 'git pull'\n",
                "hint: before pushing again.\n",
                "hint: See the 'Note about fast-forwards' in 'git push --help' for details.\n",
            ),
            verdict: Verdict::Refused(Refusal::MachineRef),
        },
        Case {
            shape: "a machine ref the remote holds at a commit unknown here",
            code: Some(1),
            stdout: concat!(
                "To /tmp/.tmpXYvBbh/ledger.git\n",
                "!\trefs/knives/machine-one:refs/knives/machine-one\t[rejected] (fetch first)\n",
                "!\t5f186f7f8a4bf2f01909cb556b55440cdee7bb3b:refs/knives-claims/example/fork/feat%2Fa\t[rejected] (atomic push failed)\n",
                "Done\n",
            ),
            stderr: concat!(
                "error: atomic push failed for ref refs/knives/machine-one. status: 5\n",
                "fatal: the remote end hung up unexpectedly\n",
                "error: failed to push some refs to '/tmp/.tmpXYvBbh/ledger.git'\n",
                "hint: Updates were rejected because the remote contains work that you do not\n",
                "hint: have locally. This is usually caused by another repository pushing to\n",
                "hint: the same ref. If you want to integrate the remote changes, use\n",
                "hint: 'git pull' before pushing again.\n",
                "hint: See the 'Note about fast-forwards' in 'git push --help' for details.\n",
            ),
            verdict: Verdict::Refused(Refusal::MachineRef),
        },
        Case {
            shape: "a pre-receive hook that declined",
            code: Some(1),
            stdout: concat!(
                "To /tmp/.tmpXYvBbh/ledger.git\n",
                "!\trefs/knives/machine-one:refs/knives/machine-one\t[remote rejected] (pre-receive hook declined)\n",
                "!\tbb019b0b7691cfcd2771a97efd9cdf7cf00f3e87:refs/knives-claims/example/fork/feat%2Fa\t[remote rejected] (pre-receive hook declined)\n",
                "Done\n",
            ),
            stderr: concat!(
                "remote: ledger pushes are frozen        \n",
                "error: failed to push some refs to '/tmp/.tmpXYvBbh/ledger.git'\n",
            ),
            verdict: Verdict::Refused(Refusal::Other),
        },
        Case {
            shape: "a pre-push hook here that refused",
            code: Some(1),
            stdout: "",
            stderr: concat!(
                "refusing this push\n",
                "error: failed to push some refs to '/tmp/.tmpXYvBbh/ledger.git'\n",
            ),
            verdict: Verdict::Refused(Refusal::Other),
        },
        Case {
            shape: "a remote path that is no repository",
            code: Some(128),
            stdout: "",
            stderr: concat!(
                "fatal: '/tmp/.tmpXYvBbh/nowhere.git' does not appear to be a git repository\n",
                "fatal: Could not read from remote repository.\n",
                "\n",
                "Please make sure you have the correct access rights\n",
                "and the repository exists.\n",
            ),
            verdict: Verdict::Refused(Refusal::Other),
        },
        Case {
            shape: "a host that does not resolve",
            code: Some(128),
            stdout: "",
            stderr: "fatal: unable to access 'https://example.invalid/example/ledger.git/': Could not resolve host: example.invalid\n",
            verdict: Verdict::Refused(Refusal::Other),
        },
        Case {
            shape: "a connection refused",
            code: Some(128),
            stdout: "",
            stderr: "fatal: unable to access 'http://127.0.0.1:9/example/ledger.git/': Failed to connect to 127.0.0.1 port 9 after 0 ms: Could not connect to server\n",
            verdict: Verdict::Refused(Refusal::Other),
        },
        Case {
            shape: "a remote that hung up after it applied the push",
            code: Some(1),
            stdout: concat!(
                "To /tmp/.tmpXYvBbh/ledger.git\n",
                "*\trefs/knives/machine-one:refs/knives/machine-one\t[new reference]\n",
                "*\t2c2af94051e5c78305fa06c2493446a421c37ca4:refs/knives-claims/example/fork/feat%2Fa\t[new reference]\n",
            ),
            stderr: concat!(
                "Killed\n",
                "send-pack: unexpected disconnect while reading sideband packet\n",
                "error: error in sideband demultiplexer\n",
                "error: failed to push some refs to '/tmp/.tmpXYvBbh/ledger.git'\n",
            ),
            verdict: Verdict::Unknown,
        },
        Case {
            shape: "a remote that hung up in the middle of the push",
            code: Some(128),
            stdout: "",
            stderr: concat!(
                "Killed\n",
                "send-pack: unexpected disconnect while reading sideband packet\n",
                "fatal: the remote end hung up unexpectedly\n",
            ),
            verdict: Verdict::Unknown,
        },
    ];
    /// A push ended where git had said nothing of its refs.
    const ENDED: &[Case] = &[
        Case {
            shape: "an HTTPS remote that hung up after receive-pack: `Done` alone on stdout",
            code: Some(1),
            stdout: "Done\n",
            stderr: "error: failed to push some refs to 'https://example.invalid/example/ledger.git'\n",
            verdict: Verdict::Unknown,
        },
        Case {
            shape: "git ended at the deadline",
            code: None,
            stdout: "",
            stderr: "",
            verdict: Verdict::Unknown,
        },
    ];

    #[test]
    fn a_claim_push_is_judged_from_its_whole_output() {
        for case in GITHUB.iter().chain(REASONS).chain(LOCAL).chain(ENDED) {
            let said = Said {
                code: case.code,
                stdout: case.stdout,
                stderr: case.stderr,
            };
            assert_eq!(
                verdict(said, CLAIM, MACHINE),
                case.verdict,
                "{}",
                case.shape
            );
        }
    }
}
