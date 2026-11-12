//! The branch verbs after `start`: `finish`, `track` and `depends`.
//!
//! `finish` hands a claimed branch back and removes its workspace; `track`
//! states which pull request a branch belongs to, overriding inference;
//! `depends` records that a branch cannot land before something else does.
//! Each records one ledger event, and a statement rides on that event;
//! `finish` also writes the released claim to the state file.

use std::path::{Path, PathBuf};

use knives::bind::Fork;
use knives::cli::Exit;
use knives::commands::claim::{
    Bound, ClaimContext, ClaimDecision, Identity, current_identity, decide, last_seen_provenance,
    possession_permits_release, render_claim_context,
};
use knives::commands::start::{collides_with_checkout, possesses, workspace_path};
use knives::ids::{BranchName, BranchTarget, Requirement};
use knives::jj::{
    Repo, StaleRegistration, WorkspaceIdentity, path_occupant, remove_finished_worktree,
};
use knives::ledger::{ClaimChange, Displaced, Draft, Entry, Stated};
use knives::statement::{Statement, StatementKind};
use knives::store::{Store, default_state_path};

use super::scribe_for;

/// What a `finish` did, or nothing when it did nothing.
///
/// Releasing a claim and recording a supersession are two acts and either can
/// happen alone: a `finish` on an unheld branch releases no claim, and one with
/// `--superseded-by` still records where the work went.
fn release_event(had: bool, superseded_by: Option<&str>) -> Option<String> {
    match (had, superseded_by) {
        (true, Some(replacement)) => Some(format!("claim released; superseded by {replacement}")),
        (true, None) => Some("claim released".to_owned()),
        (false, Some(replacement)) => Some(format!("superseded by {replacement}")),
        (false, None) => None,
    }
}

/// The durable provenance required when a claim is released by force.
fn forced_release_event(
    claim: &knives::store::Claim,
    last_seen: knives::seen::LastSeen,
    why: &str,
) -> String {
    format!(
        "released {}'s claim by force ({}, claimed {}, last seen {}): {why}",
        claim.owner,
        knives::commands::claim::owner_kind_label(claim.kind),
        claim.started,
        last_seen_provenance(last_seen),
    )
}

pub(crate) struct FinishOptions<'a> {
    pub(crate) superseded_by: Option<&'a str>,
    pub(crate) cleanup: bool,
    pub(crate) force: bool,
    pub(crate) why: Option<&'a str>,
}

enum FinishClaimGate {
    Continue(Released),
    /// Another owner took or released the caller's own claim and nobody
    /// holds the branch now. The caller has been told so; its own workspace
    /// is still disposed of, and the finish reports a finding whatever that
    /// disposal gives.
    Lost,
    /// The gate has said everything there is to say; exit with this.
    Refuse(Exit),
}

/// What the claim gate leaves for the entry `finish` writes.
///
/// The gate is the one place that reads the claim before `release_claim`
/// removes it, and the one place that looks up its last sighting, so what
/// the entry records about the displaced holder travels from here rather
/// than being read a second time.
#[derive(Default)]
struct Released {
    /// The prose a forced release carries; absent when nothing forced it.
    text: Option<String>,
    /// The `--why` that forced the release, which the entry records as the
    /// reason the claim changed hands.
    why: Option<String>,
    /// The holder this release displaced, where another owner name held the
    /// claim.
    displaced: Option<Displaced>,
}

/// Hand a branch back and remove its workspace. The inverse of `start`.
///
/// Removing the directory loses no work: the working copy is snapshotted into its commit
/// first, and a snapshot that fails or leaves a file out keeps the directory exactly as
/// it is, for the next `start` of this branch to adopt, while the claim still comes back.
/// What does not survive a removal is anything jj never tracks, which is what
/// `--no-cleanup` is for. Another process still using the workspace, or a claim taken in
/// a place whose processes this one cannot see, refuses it unless `--force`.
/// A release that displaced another owner removes nothing and forgets
/// nothing: that directory is the displaced owner's, and so is the work in
/// it jj never tracked.
/// `bound` is the registry and the fork the current directory is inside, by
/// the name its claims are kept under, which names a terminal user acting
/// from a fork's workspace.
pub(crate) fn run_finish(
    fork: &Fork<'_>,
    branch: &BranchName,
    options: &FinishOptions<'_>,
    bound: Bound<'_>,
) -> anyhow::Result<Exit> {
    let target = &BranchTarget::new(fork.upstream.clone(), branch.clone());
    // Without this, `finish` would forget the primary workspace and delete the
    // checkout itself.
    if let Some(line) = collides_with_checkout(fork, branch) {
        eprintln!("{}: {line}", fork.name);
        return Ok(Exit::Usage);
    }
    let workspace = knives::commands::wip::workspace_for(branch.as_str());
    let directory = workspace_path(fork, branch);
    let place = Place {
        checkout: &fork.checkout.path,
        workspace: &workspace,
        directory: &directory,
    };
    // The fork's lock is taken first and held to the end: between the snapshot
    // and the removal, a `start` of this branch would otherwise take the lock,
    // adopt the workspace, and write in it, and this removal would take that
    // work with it. A claim writer on this fork waits for the removal, which a
    // workspace with a build tree in it can make seconds long.
    let mut store = Store::open_for_update(
        default_state_path(),
        &bound.registry.former_names(),
        &[&fork.upstream],
    )?;
    // Who may hand the claim back is settled before anything is read from the
    // workspace: a caller the gate refuses reads no directory and scans no
    // process table, and a workspace whose state jj cannot read no longer
    // hides the claim question from a caller that does not hold the claim.
    let (released, floor) = match finish_claim_gate(fork, target, &store, options, bound)? {
        FinishClaimGate::Continue(released) => (released, Exit::Ok),
        FinishClaimGate::Lost => (Released::default(), Exit::Findings),
        FinishClaimGate::Refuse(exit) => return Ok(exit),
    };
    // Read-only checks before the claim is touched: a refusal leaves everything
    // as it was. A release over another owner's claim reads nothing from their
    // workspace at all, because it will neither snapshot nor dispose of it.
    let cleared = if released.displaced.is_some() {
        Cleared::default()
    } else {
        match clear_workspace(fork, branch, &directory, options) {
            Ok(cleared) => cleared,
            Err(exit) => return Ok(exit),
        }
    };
    let kept = keep_working_copy(fork, branch, cleared.working_copy.as_deref());
    let forced = joined(released.text, cleared.over_live_users);
    let had = store.release_claim(target);
    let pr = store.tracked_pull(target);
    // Persist the immutable explanation before the mutable claim state. A failed
    // append then leaves the old claim in place instead of silently releasing or
    // seizing work without the provenance that explains why.
    let provenance = finish_provenance(forced, had, options.superseded_by);
    if let Some(text) = provenance {
        // `--superseded-by` always leaves provenance to carry it: the replacement
        // rides on the one entry that already names it.
        scribe_for(fork, bound)?.record(&Draft {
            statement: options.superseded_by.map(|replacement| Stated {
                statement: Statement {
                    kind: StatementKind::Superseded,
                    value: Some(replacement.to_owned()),
                },
                replaces: store.statement_stamp(target, StatementKind::Superseded),
            }),
            ..Draft::event(Some(branch.as_str()), text, pr).about_claim(
                had.then_some(ClaimChange::Released),
                released.why.as_deref(),
                released.displaced.as_ref(),
            )
        })?;
    }
    store.save()?;

    let claim = if had { "released" } else { "was not held" };
    // The release does not wait on the working copy. A snapshot jj refuses
    // would otherwise keep the claim, and the branch with it, with a session
    // that has already walked away; the directory is instead left exactly as
    // it is, and the next `start` of this branch adopts it untouched.
    if let Err(refused) = kept {
        eprintln!(
            "{}/{branch}: claim {claim}; jj could not snapshot all of the working copy at {}, \
             so the workspace is kept exactly as it is and the next `knives start {branch}` \
             adopts that directory without touching what is in it\n{}",
            fork.name,
            refused.at.display(),
            refused.refused,
        );
        return Ok(Exit::Incomplete);
    }
    let disposal = if let Some(displaced) = released.displaced.as_ref() {
        left_to_its_owner(&place, &displaced.owner)
    } else {
        ordinary_disposal(
            &place,
            cleared.identity.as_ref(),
            cleared.present,
            options.cleanup,
        )?
    };
    println!("{}/{branch}: claim {claim}; {disposal}", fork.name);
    Ok(Exit::worst(floor, Exit::Ok))
}

/// Forgets this branch's workspace and removes its directory, and says what
/// became of each.
fn ordinary_disposal(
    place: &Place<'_>,
    identity: Option<&WorkspaceIdentity>,
    present: bool,
    cleanup: bool,
) -> anyhow::Result<String> {
    let registration =
        release_registration(place.checkout, place.workspace, place.directory, present);
    let removal = removal_note(place, identity, &registration, cleanup)?;
    Ok(format!(
        "workspace {} {registration}; {removal}",
        place.workspace
    ))
}

/// Why a release that displaced another owner leaves their workspace exactly
/// as it is.
///
/// What jj never tracked in that directory is the displaced owner's work, and
/// the session that forces a release is not the one to delete it. Its own
/// `knives finish` disposes of both the directory and the registration, which
/// is why the registration has to survive this release too.
fn left_to_its_owner(place: &Place<'_>, owner: &str) -> String {
    format!(
        "{} and its `{}` registration are left as they are: the directory is {owner}'s, and \
         what jj never tracked in it is not this session's to delete",
        place.directory.display(),
        place.workspace,
    )
}

/// Where this branch's workspace lives, by the three names the disposal needs.
struct Place<'a> {
    checkout: &'a Path,
    workspace: &'a str,
    directory: &'a Path,
}

/// Removes the directory, when this branch's own workspace is what sits there
/// and its registration was cleared, and says what became of it either way.
fn removal_note(
    place: &Place<'_>,
    identity: Option<&WorkspaceIdentity>,
    registration: &Registration,
    cleanup: bool,
) -> anyhow::Result<String> {
    let shown = place.directory.display();
    let checkout = place.checkout.display();
    let workspace = place.workspace;
    Ok(match identity {
        None => format!(
            "no directory at {shown}{}",
            stale_worktree_note(place.checkout, place.directory)
        ),
        Some(WorkspaceIdentity::Ours(name)) if name.as_str() == workspace => {
            // A registration that could not be forgotten keeps its directory: with
            // the directory gone, every later `start` dies on jj's "already exists",
            // `--force` included, where a retried `finish` would have cleaned up.
            let removable = cleanup
                && !matches!(
                    registration,
                    Registration::Failed(_) | Registration::Elsewhere(_)
                );
            if removable {
                // Safe because jj already snapshotted the working copy into a commit:
                // the work is in the repository and reachable by change id. Untracked
                // files are the exception, which is what --no-cleanup is for.
                std::fs::remove_dir_all(place.directory)?;
                format!(
                    "{shown} removed; its commits remain in the repository{}",
                    stale_worktree_note(place.checkout, place.directory)
                )
            } else {
                format!("{shown} left on disk")
            }
        }
        // What sits at the path is not this branch's workspace, and the forget above
        // proved nothing about it: `jj workspace forget` exits 0 for a name it does
        // not know.
        Some(WorkspaceIdentity::Ours(name)) => {
            format!("{shown} is workspace {name} of {checkout}, not {workspace}, left alone")
        }
        Some(WorkspaceIdentity::Unreadable(detail)) => format!(
            "{shown} is a workspace of {checkout} whose working-copy state could not be read \
             ({detail}), left alone"
        ),
        Some(WorkspaceIdentity::Foreign(store)) => format!(
            "{shown} belongs to repository {}, not {checkout}, left alone",
            store.display()
        ),
        Some(WorkspaceIdentity::Repository) => {
            format!("{shown} is a repository in its own right, not a workspace, left alone")
        }
        Some(WorkspaceIdentity::SymbolicLink(target_path)) => format!(
            "{shown} is a symbolic link to {}, left alone",
            target_path.display()
        ),
        Some(WorkspaceIdentity::NotAWorkspace) => {
            format!("{shown} is not a workspace of {checkout}, left alone")
        }
    })
}

/// Two optional provenance texts as one.
fn joined(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(format!("{first}; {second}")),
        (first, second) => first.or(second),
    }
}

/// The prose the one entry a `finish` writes carries, or nothing when it has
/// nothing to record: what forced the finish, what the branch was superseded
/// by, and whether a claim was released at all.
fn finish_provenance(
    forced: Option<String>,
    had: bool,
    superseded_by: Option<&str>,
) -> Option<String> {
    forced
        .map(|text| match superseded_by {
            Some(replacement) => format!("{text}; superseded by {replacement}"),
            None => text,
        })
        .or_else(|| release_event(had, superseded_by))
}

/// What is at the branch's workspace path, and what the read-only checks
/// before the claim is touched allow `finish` to go on with.
///
/// The default is what a `finish` that will not look at the workspace reads
/// of it: nothing there, nothing to snapshot, nothing forced.
#[derive(Default)]
struct Cleared {
    /// What sits at the path, when anything does.
    identity: Option<WorkspaceIdentity>,
    /// Whether that is this branch's own workspace.
    present: bool,
    /// Where this branch's working copy is, when `finish` will forget it: the
    /// path itself, or the directory a symbolic link there points at.
    working_copy: Option<PathBuf>,
    /// The forced finish's provenance, when `--force` went over a live process.
    over_live_users: Option<String>,
}

/// Reads what is at `directory` and finds this branch's working copy there,
/// then checks that no other process still works in it. A workspace of this
/// repository whose state cannot be read is refused: it cannot be snapshotted,
/// so it must not be forgotten. `Err` carries the exit after the refusal is
/// printed.
fn clear_workspace(
    fork: &Fork<'_>,
    branch: &BranchName,
    directory: &Path,
    options: &FinishOptions<'_>,
) -> Result<Cleared, Exit> {
    let checkout = &fork.checkout.path;
    let workspace =
        knives::ids::WorkspaceName::new(knives::commands::wip::workspace_for(branch.as_str()));
    // Read what is at the path before writing anything to the repository: a jj
    // that refused to load a forgotten workspace's state would otherwise leave
    // the directory forgotten and unremovable. A path this code cannot even
    // look at is not an empty one: something may be there with work in it.
    let identity = match std::fs::metadata(directory) {
        Ok(_) => Some(knives::jj::workspace_identity(directory, checkout)),
        // Nothing there, or a link to nothing: neither holds work, and the
        // link is reported where the registration it leaves behind is.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(WorkspaceIdentity::Unreadable(error.to_string())),
    };
    // When the workspace at the path is this branch's own, the directory is the
    // evidence: jj's record of where it lives can be missing (made before jj
    // recorded paths) or stale (moved by hand).
    let present = identity.as_ref() == Some(&WorkspaceIdentity::Ours(workspace.clone()));
    // A symbolic link is judged by where the whole chain of links lands: this
    // branch's workspace there is snapshotted there before its name is
    // forgotten, and one whose state cannot be read is refused as if it sat at
    // the path.
    let landed = match &identity {
        Some(WorkspaceIdentity::SymbolicLink(_)) => match directory.canonicalize() {
            Ok(target) => Some((knives::jj::workspace_identity(&target, checkout), target)),
            // A link to nothing has nothing to snapshot. A link this code
            // cannot follow for any other reason may lead to work.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => Some((
                WorkspaceIdentity::Unreadable(error.to_string()),
                directory.to_owned(),
            )),
        },
        _ => None,
    };
    let (resolved, at) = match &landed {
        Some((resolved, target)) => (Some(resolved), target.as_path()),
        None => (identity.as_ref(), directory),
    };
    let working_copy = match resolved {
        Some(WorkspaceIdentity::Ours(name)) if *name == workspace => Some(at.to_owned()),
        Some(WorkspaceIdentity::Unreadable(detail)) => {
            eprintln!(
                "{}/{branch}: {} is a workspace of {} whose working-copy state could not be read \
                 ({detail}); it cannot be snapshotted, so it is not forgotten; nothing was changed",
                fork.name,
                at.display(),
                checkout.display()
            );
            return Err(Exit::Incomplete);
        }
        _ => None,
    };
    let over_live_users = match &working_copy {
        Some(working_copy) => live_users_gate(fork, branch, working_copy, options)?,
        None => None,
    };
    Ok(Cleared {
        identity,
        present,
        working_copy,
        over_live_users,
    })
}

/// A working copy jj would not snapshot whole, and where it sits.
struct NotSnapshotted {
    at: PathBuf,
    refused: knives::jj::JjError,
}

/// Snapshots this branch's working copy into jj before it is forgotten, and
/// names the commit that keeps it at once, before anything can fail on the
/// way to removing it. A snapshot that fails or leaves something out leaves
/// the workspace as it is and the disposal undone; what jj did snapshot stays
/// in its commit, and the claim is handed back either way.
fn keep_working_copy(
    fork: &Fork<'_>,
    branch: &BranchName,
    working_copy: Option<&Path>,
) -> Result<(), NotSnapshotted> {
    let Some(working_copy) = working_copy else {
        return Ok(());
    };
    match knives::jj::snapshot_workspace(working_copy) {
        Ok(kept) => {
            println!("{}/{branch}: its working copy is kept as {kept}", fork.name);
            Ok(())
        }
        Err(refused) => Err(NotSnapshotted {
            at: working_copy.to_owned(),
            refused,
        }),
    }
}

/// Whether anything else on this machine still works in `directory`: a
/// process (not this one, its ancestors, or its own harness's) with its working
/// directory or an open file there. `Ok` to go on, carrying the forced finish's
/// provenance when `--force` went over one.
fn live_users_gate(
    fork: &Fork<'_>,
    branch: &BranchName,
    directory: &Path,
    options: &FinishOptions<'_>,
) -> Result<Option<String>, Exit> {
    let session = knives::commands::claim::harness_session();
    let users = knives::liveness::live_users(
        directory,
        session
            .as_ref()
            .map(|(name, value)| (*name, value.as_str())),
    );
    if users.is_empty() {
        return Ok(None);
    }
    let list = users
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    if let Some(why) = options.why.filter(|_| options.force) {
        return Ok(Some(format!(
            "finished by force while in use by {list}: {why}"
        )));
    }
    eprintln!(
        "{}/{branch}: {} is in use by {list}; finishing would take the directory from under \
         it\nuse `knives finish {branch} --force --why \"…\"` once it is done",
        fork.name,
        directory.display()
    );
    Err(Exit::Usage)
}

/// The Git side of a workspace whose directory is now gone: `jj workspace
/// forget` unlinks the worktree it registered, but a directory that vanished
/// before the forget leaves git's `.git/worktrees/<name>` entry behind, locked
/// or prunable, and the next `start` at that path would fail on it. One that
/// stays because something else now sits at the path is named, with what is
/// there, so the failing `start` is not a surprise.
fn stale_worktree_note(checkout: &Path, directory: &Path) -> String {
    match remove_finished_worktree(checkout, directory) {
        Ok(StaleRegistration::Removed) => "; stale Git worktree registration removed".to_owned(),
        Ok(StaleRegistration::None | StaleRegistration::Locked) => String::new(),
        Ok(StaleRegistration::Occupied) => format!(
            "; {} is {}, so its stale Git worktree registration was not cleared",
            directory.display(),
            path_occupant(directory)
        ),
        Err(error) => format!("; Git worktree registration not cleared ({error})"),
    }
}

/// What became of the branch's workspace registration.
enum Registration {
    Forgotten,
    /// jj never knew the name; `jj workspace forget` would have exited 0 anyway,
    /// and saying "forgotten" would let a reader believe a registration was cleared.
    NotRegistered,
    /// jj recorded the name at another path, where a same-named workspace is
    /// live; forgetting the name would take that one over, so it is kept.
    Elsewhere(PathBuf),
    /// The forget failed, or the checkout could not be opened to ask. Reported,
    /// not an error: the claim is already released by the time this runs, and the
    /// ownership decision never needed the checkout.
    Failed(String),
}

impl std::fmt::Display for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forgotten => f.write_str("forgotten"),
            Self::NotRegistered => f.write_str("was not registered"),
            Self::Elsewhere(path) => write!(
                f,
                "not forgotten (it is registered at {}; finish or forget it there)",
                path.display()
            ),
            Self::Failed(detail) => write!(f, "not forgotten ({detail})"),
        }
    }
}

/// Forgets `workspace` in jj. Unless `present` — this branch's workspace sits
/// at `directory` — the name is forgotten only where jj recorded it at
/// `directory`: elsewhere it is a live same-named workspace, and a record jj
/// cannot give is not proof either way.
fn release_registration(
    checkout: &Path,
    workspace: &str,
    directory: &Path,
    present: bool,
) -> Registration {
    match Repo::open(checkout).and_then(|repo| repo.workspaces()) {
        Err(error) => Registration::Failed(error.to_string()),
        Ok(names) if !names.iter().any(|(name, _)| name.as_str() == workspace) => {
            Registration::NotRegistered
        }
        Ok(_) => {
            let elsewhere = if present {
                Ok(None)
            } else {
                knives::jj::workspace_recorded_elsewhere(checkout, workspace, directory)
            };
            match elsewhere {
                Err(error) => Registration::Failed(error.to_string()),
                Ok(Some(elsewhere)) => Registration::Elsewhere(elsewhere),
                Ok(None) => match knives::jj::forget_workspace(checkout, workspace) {
                    Ok(()) => Registration::Forgotten,
                    Err(error) => Registration::Failed(error.to_string()),
                },
            }
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the fork, the branch, the store, the finish options and the cwd binding are independent inputs"
)]
fn finish_claim_gate(
    fork: &Fork<'_>,
    target: &BranchTarget,
    store: &Store,
    options: &FinishOptions<'_>,
    bound: Bound<'_>,
) -> anyhow::Result<FinishClaimGate> {
    let Some(claim) = store
        .claims(Some(&target.repo))
        .into_iter()
        .find(|claim| claim.branch == target.branch.as_str())
        .cloned()
    else {
        return unheld_gate(target, store, options, bound);
    };
    let cwd = std::env::current_dir()?;
    let identity = current_identity(bound)?;
    let decision = match decide(&ClaimContext {
        held: Some(&claim),
        identity: &identity,
        in_claimed_workspace: possesses(&cwd, fork, &target.branch),
    }) {
        // Standing in the directory is evidence enough to take work up again,
        // never to hand back a claim taken under another name: after a takeover
        // the directory belongs to the new holder, and the finish an agent runs
        // from inside it would otherwise release that holder's claim and remove
        // the directory under it.
        ClaimDecision::Resume { possession: true }
            if !possession_permits_release(&claim, &identity) =>
        {
            ClaimDecision::RefuseHeld
        }
        decision => decision,
    };
    match decision {
        ClaimDecision::Resume { .. } | ClaimDecision::Take => {
            Ok(place_gate(&claim, target, options))
        }
        ClaimDecision::RefuseAnonymous | ClaimDecision::RefuseHeld => {
            let last_seen =
                knives::seen::last_seen_in(&claim, &fork.checkout.path, &knives::seen::load());
            if !options.force {
                return Ok(refuse_held(&Refusal {
                    target,
                    entries: store.ledger_entries(&target.repo),
                    identity: &identity,
                    claim: &claim,
                    last_seen,
                    anonymous: decision == ClaimDecision::RefuseAnonymous,
                }));
            }
            let why = options
                .why
                .ok_or_else(|| anyhow::anyhow!("--force requires --why"))?;
            Ok(FinishClaimGate::Continue(Released {
                text: Some(forced_release_event(&claim, last_seen, why)),
                why: Some(why.to_owned()),
                displaced: (claim.owner != identity.owner)
                    .then(|| Displaced::of(&claim, last_seen)),
            }))
        }
    }
}

/// A branch nobody holds: an ordinary finish of nothing, unless this fork's
/// ledger says another owner ended the caller's own claim on it. The caller
/// is told that first, because the claim it believed it still held is why it
/// is finishing at all; its own workspace is disposed of all the same.
fn unheld_gate(
    target: &BranchTarget,
    store: &Store,
    options: &FinishOptions<'_>,
    bound: Bound<'_>,
) -> anyhow::Result<FinishClaimGate> {
    if options.force {
        return Ok(FinishClaimGate::Continue(Released::default()));
    }
    let identity = current_identity(bound)?;
    let entries = store.ledger_entries(&target.repo);
    let Some(lost) = claim_ended(entries, &target.branch, &identity.owner) else {
        return Ok(FinishClaimGate::Continue(Released::default()));
    };
    eprintln!(
        "{}\nnobody holds {} now\nuse `knives start {}` to claim it again",
        displacement(target, &identity.owner, lost),
        target.key(),
        target.branch,
    );
    Ok(FinishClaimGate::Lost)
}

/// What a refusal of a claim this caller does not hold is decided from.
struct Refusal<'a> {
    target: &'a BranchTarget,
    /// This fork's ledger as the store read it, oldest first: what became of
    /// the caller's own claim is in it ([`claim_ended`]).
    entries: &'a [Entry],
    identity: &'a Identity,
    claim: &'a knives::store::Claim,
    last_seen: knives::seen::LastSeen,
    /// Both sides resolved to an OS user name, which can never match.
    anonymous: bool,
}

/// Refuse a finish of a claim the caller does not hold, and say what to do
/// instead.
///
/// An owner whose own claim the ledger says was taken or released is told
/// that, and is never offered the `finish --force` line: that command would
/// release the live claim of whoever holds the branch now and leave it
/// claimed by nobody. Taking the branch back is `start --force`, which
/// records the change; releasing a gone agent's claim is what the other
/// refusal is for.
fn refuse_held(refusal: &Refusal<'_>) -> FinishClaimGate {
    let context = render_claim_context(refusal.claim, refusal.last_seen, jiff::Timestamp::now());
    let branch = &refusal.target.branch;
    if let Some(lost) = claim_ended(refusal.entries, branch, &refusal.identity.owner) {
        eprintln!(
            "{}\n{context}\nnothing was changed here: the workspace and its registration are \
             the holder's\nuse `knives start {branch} --force --why \"…\"` to take the branch back",
            displacement(refusal.target, &refusal.identity.owner, lost),
        );
        return FinishClaimGate::Refuse(Exit::Findings);
    }
    let anonymous_note = if refusal.anonymous {
        "both sides are anonymous identities, so they can never match; "
    } else {
        ""
    };
    eprintln!(
        "{anonymous_note}{context}\nuse `knives finish {branch} --force --why \"…\"` to release \
         the claim"
    );
    FinishClaimGate::Refuse(Exit::Usage)
}

/// What this fork's ledger says became of `owner`'s claim: who ended it,
/// when, and the reason that owner gave for ending it.
fn displacement(target: &BranchTarget, owner: &str, lost: &Entry) -> String {
    // `claim_ended` answers with a taken or a released entry and nothing else.
    let verb = match lost.claim_change() {
        Some(ClaimChange::Taken) => "took",
        _ => "released",
    };
    let why = lost
        .claim_change_why
        .as_deref()
        .map_or_else(String::new, |why| format!(": {why}"));
    format!(
        "{} {verb} {owner}'s claim on {} at {}{why}",
        lost.owner,
        target.key(),
        lost.ts,
    )
}

/// The entry that ended `owner`'s claim on `branch`, read newest first, or
/// nothing where this fork's ledger records no such end.
///
/// The search stops at the newest claim or release `owner` itself wrote,
/// which closed the chapter before it: an owner that claimed the branch
/// again, or handed it back itself, is not told about a takeover it has
/// already answered. An entry that records no claim change — every entry an
/// older knives wrote — neither stops the search nor answers it, and neither
/// does a resume, so an owner that took its work up again in the directory it
/// had lost is still told who took the branch.
fn claim_ended<'a>(entries: &'a [Entry], branch: &BranchName, owner: &str) -> Option<&'a Entry> {
    for entry in entries.iter().rev() {
        if entry.subject.as_deref() != Some(branch.as_str()) {
            continue;
        }
        match entry.claim_change() {
            Some(ClaimChange::Claimed | ClaimChange::Released) if entry.owner == owner => {
                return None;
            }
            Some(ClaimChange::Taken | ClaimChange::Released)
                if entry.previous_owner.as_deref() == Some(owner) =>
            {
                return Some(entry);
            }
            _ => {}
        }
    }
    None
}

/// The claim's own holder finishing from another place: a host or agent box
/// whose processes this one cannot see, so whether the workspace is still in
/// use there cannot be checked. A duplicate harness of the same session in
/// another box is the case: the identity matches and the claim cannot tell
/// them apart. Refused without `--force`; a forced finish records the reason.
fn place_gate(
    claim: &knives::store::Claim,
    target: &BranchTarget,
    options: &FinishOptions<'_>,
) -> FinishClaimGate {
    let here = knives::liveness::this_place();
    let Some(taken) = claim.place.as_deref().filter(|taken| *taken != here) else {
        return FinishClaimGate::Continue(Released::default());
    };
    if let Some(why) = options.why.filter(|_| options.force) {
        return FinishClaimGate::Continue(Released {
            text: Some(format!(
                "finished by force from {here}, not {taken} where the claim was taken: {why}"
            )),
            why: Some(why.to_owned()),
            displaced: None,
        });
    }
    eprintln!(
        "{} was claimed by {} in {taken}; this finish runs in {here}, which cannot see whether a \
         process there still works in the workspace\nuse `knives finish {} --force --why \"…\"` \
         once nothing there uses it",
        claim.key(),
        claim.owner,
        target.branch,
    );
    FinishClaimGate::Refuse(Exit::Usage)
}

/// Pull `fork`'s ledger before a write built from what it states, and say
/// why it will not write when the pull failed.
///
/// A forget erases the statement before it, and the newest statement wins on
/// every machine, so built from a ledger missing another machine's newer
/// statement it would erase that statement everywhere, as a release write
/// would check its drop guard against the wrong cut. A `depends` statement
/// erases nothing, since the live list is every requirement stated since the
/// last forget, but it records that the branch must wait from what this
/// machine last pulled, and says "now requires …" from it: from a ledger it
/// knows is stale, that would tell the caller a list other machines may
/// already have changed. So each refuses, like a release write, rather than
/// write from what this machine happens to have.
fn pulled_before_replacing(fork: &Fork<'_>, bound: Bound<'_>, what: &str) -> bool {
    let problems =
        knives::ledger_sweep::pull(bound.registry, &[&fork.upstream]).problems_for(&fork.upstream);
    for problem in &problems {
        eprintln!("!! {}", knives::ledger::problem_human_text(problem));
    }
    if !problems.is_empty() {
        eprintln!(
            "{what} not recorded: it is built from what the ledger states, and another \
             machine's newer statement may be among what could not be pulled"
        );
    }
    problems.is_empty()
}

/// State or forget which pull request a branch belongs to.
///
/// A forget pulls first ([`pulled_before_replacing`]) and writes a statement
/// only when one is stated: a forget of nothing would erase, everywhere, a
/// statement another machine made that this one has not seen.
#[allow(
    clippy::too_many_arguments,
    reason = "the fork, the branch, the three ways to state its pull request and the cwd binding are independent inputs"
)]
pub(crate) fn run_track(
    fork: &Fork<'_>,
    branch: &BranchName,
    pr: Option<u64>,
    fork_only: bool,
    forget: bool,
    bound: Bound<'_>,
) -> anyhow::Result<Exit> {
    let target = &BranchTarget::new(fork.upstream.clone(), branch.clone());
    if forget && !pulled_before_replacing(fork, bound, "the forget") {
        return Ok(Exit::Incomplete);
    }
    // Opened for update to hold the fork's lock while the statement is read and
    // the next one appended: a concurrent `track` then reads this one's
    // statement rather than the one before it. Nothing here writes the state
    // file.
    let store = Store::open_for_update(
        default_state_path(),
        &bound.registry.former_names(),
        &[&fork.upstream],
    )?;
    // Read before the change, so a withdrawal is still filed under the number it
    // withdrew.
    let stated = store.tracked_pull(target);
    // Each branch stamps the number its entry is ABOUT, not whatever happened to
    // be stated a moment earlier. The event that creates an association is the
    // one `knives notch --pr <n>` most needs to find, and stamping the prior
    // value there — usually nothing — would hide it from the only filter the
    // field exists for.
    let (text, stamped, statement) = if fork_only {
        (
            "stated as having no upstream pull request".to_owned(),
            stated,
            Some(Statement {
                kind: StatementKind::ForkOnly,
                value: Some("stated with `knives track --fork-only`".to_owned()),
            }),
        )
    } else if forget {
        match stated {
            Some(_) => (
                "pull request statement forgotten".to_owned(),
                stated,
                Some(Statement {
                    kind: StatementKind::Pull,
                    value: None,
                }),
            ),
            None => ("no pull request statement to forget".to_owned(), None, None),
        }
    } else {
        let Some(number) = pr else {
            eprintln!("give --pr <number>, or --forget");
            return Ok(Exit::Usage);
        };
        (
            format!("stated as #{number}"),
            Some(number),
            Some(Statement {
                kind: StatementKind::Pull,
                value: Some(number.to_string()),
            }),
        )
    };
    let statement = statement.map(|statement| Stated {
        replaces: store.statement_stamp(target, statement.kind),
        statement,
    });
    scribe_for(fork, bound)?.record(&Draft {
        statement,
        ..Draft::event(Some(branch.as_str()), text.clone(), stamped)
    })?;
    drop(store);
    println!("{}/{branch} {}", fork.name, spoken(&text));
    Ok(Exit::Ok)
}

/// The prose form of a `track` outcome, which reads about the branch rather than
/// about the statement.
fn spoken(text: &str) -> String {
    match text {
        "stated as having no upstream pull request" => {
            "deliberately has no upstream pull request".to_owned()
        }
        "pull request statement forgotten" => "is back to inferring its pull request".to_owned(),
        "no pull request statement to forget" => "had no stated pull request".to_owned(),
        stated => stated.replacen("stated as ", "is ", 1),
    }
}

/// Record what a branch cannot land before.
///
/// Requirements are validated against the registry, because a dependency on a repo
/// knives does not manage is a typo, and a typo that records silently is worse than
/// no dependency at all: it reads as satisfied forever.
pub(crate) fn run_depends(
    fork: &Fork<'_>,
    branch: &BranchName,
    on: &[String],
    bound: Bound<'_>,
) -> anyhow::Result<Exit> {
    let registry = bound.registry;
    let target = &BranchTarget::new(fork.upstream.clone(), branch.clone());
    let mut requirements = Vec::new();
    for text in on {
        let Some(written) = Requirement::parse(text) else {
            eprintln!("cannot read {text} as a requirement; write it as `<repo>#<number>`");
            return Ok(Exit::Usage);
        };
        // Recorded under the required fork's repository name, whichever way it
        // was typed, so the statement means the same fork on every machine.
        let Some(upstream) = registry.resolve(written.repo.as_str()) else {
            let known: Vec<String> = registry.names().map(|n| n.to_string()).collect();
            eprintln!(
                "unknown repo {} in {text}; known: {}",
                written.repo,
                known.join(", ")
            );
            return Ok(Exit::Usage);
        };
        requirements.push(Requirement {
            repo: upstream,
            number: written.number,
        });
    }
    if !pulled_before_replacing(fork, bound, "the dependency") {
        return Ok(Exit::Incomplete);
    }
    // The fork's lock is held while the list is read and the next statement
    // appended, so two concurrent `depends` on this machine each add to the
    // other's list rather than racing; the pull above brought in what other
    // machines stated. Nothing here writes the state file.
    let store = Store::open_for_update(
        default_state_path(),
        &registry.former_names(),
        &[&fork.upstream],
    )?;
    let required = with_requirements(&store.dependencies(target), &requirements);
    let pr = store.tracked_pull(target);
    let listed: Vec<String> = requirements.iter().map(ToString::to_string).collect();
    scribe_for(fork, bound)?.record(&Draft {
        statement: Some(Stated {
            statement: Statement {
                kind: StatementKind::Depends,
                value: Some(required.join(",")),
            },
            replaces: store.statement_stamp(target, StatementKind::Depends),
        }),
        ..Draft::event(
            Some(branch.as_str()),
            format!("requires {}", listed.join(", ")),
            pr,
        )
    })?;
    drop(store);
    println!("{}/{branch} now requires {}", fork.name, listed.join(", "));
    Ok(Exit::Ok)
}

/// Everything a branch requires once `added` joins what it already `required`.
///
/// Additive and deduplicated, so declaring the same requirement twice is not an
/// error and re-running a script does not accumulate duplicates. The result is
/// the whole list, so each entry reads on its own as everything the branch
/// needed when it was written.
fn with_requirements(required: &[Requirement], added: &[Requirement]) -> Vec<String> {
    let mut texts: Vec<String> = required.iter().map(ToString::to_string).collect();
    for requirement in added {
        let text = requirement.to_string();
        if !texts.contains(&text) {
            texts.push(text);
        }
    }
    texts.sort();
    texts
}

#[cfg(test)]
mod tests {
    use super::{spoken, with_requirements};
    use knives::ids::Requirement;

    #[test]
    fn track_prose_preserves_the_established_human_output() {
        for (event, expected) in [
            ("stated as #4545", "is #4545"),
            (
                "stated as having no upstream pull request",
                "deliberately has no upstream pull request",
            ),
            (
                "pull request statement forgotten",
                "is back to inferring its pull request",
            ),
            (
                "no pull request statement to forget",
                "had no stated pull request",
            ),
        ] {
            assert_eq!(spoken(event), expected);
        }
    }

    #[test]
    fn requirements_join_what_was_already_required_without_duplicates() {
        let parse = |text: &str| Requirement::parse(text).expect("a requirement");
        assert_eq!(
            with_requirements(
                &[parse("sibling#49"), parse("other#7")],
                &[parse("sibling#49"), parse("another#3")],
            ),
            ["another#3", "other#7", "sibling#49"]
        );
    }
}
