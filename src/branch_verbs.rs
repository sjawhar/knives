//! The branch verbs after `start`: `finish`, `track` and `depends`.
//!
//! `finish` hands a claimed branch back and removes its workspace; `track`
//! states which pull request a branch belongs to, overriding inference;
//! `depends` records that a branch cannot land before something else does.
//! Each records one ledger event, and a statement rides on that event;
//! `finish` also writes the released claim to the state file.
//!
//! On a fork that shares its ledger, the ledger's claim ref says who holds
//! the branch, on every machine that shares it. `finish` reads it under the
//! fork's lock, decides from it as `start` does, and releases it with one
//! push, leased on the claim commit it read, that carries the entry
//! recording the release; the state file and the workspace are touched only
//! once the ledger took that push, so a release the ledger did not take
//! releases nothing here.

use std::path::{Path, PathBuf};

use knives::bind::Fork;
use knives::cli::Exit;
use knives::commands::claim::{
    Bound, ClaimContext, ClaimDecision, Identity, SharedClaims, change_on_ledger, claimed_on,
    copy_of, current_identity, decide, git_dir_of, possession_permits_release, refusing,
    render_claim_context, render_shared_claim_context, sighting_provenance, unapplied_cause,
    unread_claim, unreadable_activity, unreadable_holder, unwritten,
};
use knives::commands::start::{collides_with_checkout, possesses, workspace_path};
use knives::config::Registry;
use knives::ids::{BranchName, BranchTarget, CommitId, Requirement, UpstreamName};
use knives::jj::{
    Repo, StaleRegistration, WorkspaceIdentity, path_occupant, remove_finished_worktree,
};
use knives::ledger::{
    ClaimChange, Displaced, Draft, Entry, LedgerError, Scribe, Stated, default_ledger_root,
    inline_human_text, problem_human_text,
};
use knives::ledger_git::{ClaimError, ClaimRef, ClaimSeen, HeldClaim};
use knives::ledger_sweep::{self, ClaimOutcome, NotApplied, SharedFork};
use knives::seen::LastSeen;
use knives::statement::{Statement, StatementKind};
use knives::store::{Claim, Store, default_state_path};

use super::scribe_for;

/// What a `finish` did, or nothing when it did nothing.
///
/// Releasing a claim and recording a supersession are two acts and either can
/// happen alone: a `finish` on an unheld branch releases no claim, and one with
/// `--superseded-by` still records where the work went.
///
/// The reason `--why` gave is recorded whatever the finish did. Where a claim
/// was released it is the released claim's own field (`claim_change_why`),
/// so the text does not repeat it; where none was, the text carries it: a
/// supersession ends with it, and a finish that did neither records it on
/// an entry of its own, which changes no claim.
fn release_event(had: bool, superseded_by: Option<&str>, why: Option<&str>) -> Option<String> {
    match (had, superseded_by, why) {
        (true, Some(replacement), _) => {
            Some(format!("claim released; superseded by {replacement}"))
        }
        (true, None, _) => Some("claim released".to_owned()),
        (false, Some(replacement), None) => Some(format!("superseded by {replacement}")),
        (false, Some(replacement), Some(why)) => {
            Some(format!("superseded by {replacement}: {why}"))
        }
        (false, None, Some(why)) => Some(format!("finished with no claim held: {why}")),
        (false, None, None) => None,
    }
}

/// The durable provenance required when a claim is released by force: whose
/// claim it was, how its owner was named, when it was taken, and its
/// `sighting` ([`sighting_provenance`]).
fn forced_release_event(claim: &Claim, sighting: &str, why: &str) -> String {
    format!(
        "released {}'s claim by force ({}, claimed {}, {sighting}): {why}",
        claim.owner,
        knives::commands::claim::owner_kind_label(claim.kind),
        claim.started,
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
    /// Nobody holds the branch now, though the caller had a claim on it:
    /// another owner took or released the caller's own claim, or the ledger
    /// no longer holds the claim this machine's state file keeps a copy of
    /// ([`Released::stale_copy`]). The caller has been told so; its own
    /// workspace is still disposed of, and the finish exits with the worse
    /// of a finding and what that disposal gives: `2` where a live process
    /// refuses the removal, `3` where jj cannot read the workspace state or
    /// snapshot the working copy.
    Lost(Released),
    /// The ledger holds another owner's claim in place of the caller's own,
    /// which this machine's state file still keeps a copy of. The caller has
    /// been told who holds it; the copy is removed, nothing else changes,
    /// and the finish exits 1.
    StaleCopy,
    /// The gate has said everything there is to say; exit with this.
    Refuse(Exit),
}

/// What the claim gate leaves for the entry `finish` writes and the release
/// it makes.
///
/// The gate is the one place that reads the claim before `release_claim`
/// removes it, and the one place that looks up its last sighting, so what
/// the entry records about the displaced holder travels from here rather
/// than being read a second time.
#[derive(Default)]
struct Released {
    /// The prose a forced release carries; absent when nothing forced it.
    text: Option<String>,
    /// The holder this release displaced, where another owner name held the
    /// claim.
    displaced: Option<Displaced>,
    /// Whether the displaced holder's activity cannot be read here
    /// ([`SharedClaims::readable`]): this machine's sightings of work done
    /// elsewhere are empty for want of looking, so the entry records none.
    unread_activity: bool,
    /// On a fork that shares its ledger, the commit of the ledger's claim
    /// this finish releases, as the gate read it: the release is leased on
    /// it, so it lands only on the claim the gate decided from.
    on_ledger: Option<CommitId>,
    /// Whether what the state file holds for the branch is a copy of a
    /// claim the ledger no longer holds: the finish removes it, and releases
    /// nobody's claim by doing so.
    stale_copy: bool,
}

/// What a `finish` was asked, of which branch: the fork, the branch, the
/// options and the cwd binding.
#[derive(Clone, Copy)]
struct Finishing<'a> {
    fork: &'a Fork<'a>,
    target: &'a BranchTarget,
    options: &'a FinishOptions<'a>,
    bound: Bound<'a>,
}

/// A fork that shares its ledger, and the claim ref on it of the branch a
/// `finish` is for.
struct Ledgered<'a> {
    shared: SharedFork,
    claim: ClaimRef,
    target: &'a BranchTarget,
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
/// On a fork that shares its ledger the claim is the ledger's, and it is
/// released there first ([`release_on_ledger`]): a release the ledger did
/// not take leaves the state file and the workspace as they were.
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
    //
    // A fork ledger that cannot be read does not fail the open: a caller the
    // claim gate refuses needs the ledger only to search it for who ended
    // its own claim, and the refusal it gets must not turn into a failure
    // because that search cannot run ([`read_ledger`]).
    let store = Store::open_each_for_update(
        default_state_path(),
        &bound.registry.former_names(),
        &[&fork.upstream],
    )?;
    // A fork that shares its ledger hands back the ledger's claim, which is
    // read under the fork's lock by the gate below.
    let ledger = match ledgered(fork, target, bound.registry)? {
        Ok(ledger) => ledger,
        Err(exit) => return Ok(exit),
    };
    let finishing = Finishing {
        fork,
        target,
        options,
        bound,
    };
    // Who may hand the claim back is settled before anything is read from the
    // workspace: a caller the gate refuses reads no directory and scans no
    // process table, and a workspace whose state jj cannot read no longer
    // hides the claim question from a caller that does not hold the claim.
    let gate = match &ledger {
        Some(ledger) => shared_claim_gate(&finishing, &store, ledger)?,
        None => finish_claim_gate(fork, target, &store, options, bound)?,
    };
    let (released, floor) = match gate {
        FinishClaimGate::Continue(released) => (released, Exit::Ok),
        FinishClaimGate::Lost(released) => (released, Exit::Findings),
        FinishClaimGate::StaleCopy => return forget_stale_copy(store, target),
        FinishClaimGate::Refuse(exit) => return Ok(exit),
    };
    // Everything past the gate reads the ledger's statements and writes an
    // entry to it, so a ledger that could not be read fails the finish here,
    // with the one line naming it, before the workspace is read or the claim
    // touched.
    let mut store = store.all_read()?;
    // Read-only checks before the claim is touched: a refusal leaves everything
    // as it was. A release over another owner's claim reads their workspace
    // too: `knives start` refuses to adopt a workspace whose state jj cannot
    // read, so releasing the claim over one would leave the branch closed to
    // everybody.
    let cleared = match clear_workspace(fork, branch, &directory, options) {
        Ok(cleared) => cleared,
        Err(exit) => return Ok(exit),
    };
    // What that release does not do is write in the directory: the displaced
    // owner's working copy is theirs to snapshot, as the disposal below is
    // theirs to run.
    let kept = if released.displaced.is_some() {
        Ok(())
    } else {
        keep_working_copy(fork, branch, cleared.working_copy.as_deref())
    };
    // The live-process gate is the one force the claim gate does not see, so
    // its prose joins the gate's.
    let released = Released {
        text: joined(released.text, cleared.over_live_users),
        ..released
    };
    let had = match record_finish(&finishing, &mut store, &released, ledger.as_ref())? {
        Ok(had) => had,
        Err(exit) => return Ok(exit),
    };
    store.save()?;

    let claim = if had { "released" } else { "was not held" };
    // The release does not wait on the working copy. A snapshot jj refuses
    // would otherwise keep the claim, and the branch with it, with a session
    // that has already walked away; the directory is instead left exactly as
    // it is, and the next `start` of this branch adopts it untouched.
    //
    // The path is the one jj recorded for the workspace and the refusal is
    // jj's own text, which prints more than one line, so they reach the
    // terminal escaped ([`inline_human_text`] and
    // [`knives::ledger::problem_human_text`]).
    if let Err(refused) = kept {
        eprintln!(
            "{}/{branch}: claim {claim}; jj could not snapshot all of the working copy at {}, \
             so the workspace is kept exactly as it is and the next `knives start {branch}` \
             adopts that directory without touching what is in it\n{}",
            fork.name,
            inline_human_text(&refused.at.display().to_string()),
            knives::ledger::problem_human_text(&refused.refused.to_string()),
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
    Ok(floor)
}

/// Where a `finish` of `target` releases its claim on the ledger, when its
/// fork shares one: the destination that carries the fork, a ledger no git
/// directory here reaches joined first, and the branch's claim ref. `Err`
/// is the exit after saying why nothing was released: a ledger that cannot
/// be joined is no ledger to release a claim on.
///
/// A branch no claim ref can name ([`ClaimRef::new`]) has never been
/// claimed on any ledger, so its finish is that of a fork that shares none.
fn ledgered<'a>(
    fork: &Fork<'_>,
    target: &'a BranchTarget,
    registry: &Registry,
) -> anyhow::Result<Result<Option<Ledgered<'a>>, Exit>> {
    let shared = match ledger_sweep::shared_fork(&default_ledger_root(), registry, &fork.upstream) {
        Ok(Some(shared)) => shared,
        Ok(None) => return Ok(Ok(None)),
        Err(error) => {
            eprintln!(
                "nothing was released: {}",
                problem_human_text(&error.to_string())
            );
            return Ok(Err(Exit::Incomplete));
        }
    };
    match ClaimRef::new(&target.repo, &target.branch) {
        Ok(claim) => Ok(Ok(Some(Ledgered {
            shared,
            claim,
            target,
        }))),
        Err(ClaimError::TooLong { .. } | ClaimError::Refused { .. }) => Ok(Ok(None)),
        Err(error) => Err(error.into()),
    }
}

/// Remove this machine's copy of the caller's claim, which the ledger holds
/// another owner's claim in place of ([`FinishClaimGate::StaleCopy`]), and
/// nothing else: exit 1.
fn forget_stale_copy(mut store: Store, target: &BranchTarget) -> anyhow::Result<Exit> {
    store.release_claim(target);
    store.save()?;
    Ok(Exit::Findings)
}

/// Release the claim the gate let go of, and write the entry a `finish`
/// leaves where it has one to leave; whether a claim was released, or the
/// exit after saying why nothing was, with the state file left unsaved.
///
/// The immutable explanation goes before the mutable claim state: a failed
/// append leaves the old claim in place instead of silently releasing or
/// seizing work without the provenance that explains why. Where the gate
/// found the ledger's claim, the entry goes in the one push that deletes it
/// ([`release_on_ledger`]); anywhere else it is appended here, and the send
/// before the command exits carries it to a ledger the fork shares.
fn record_finish(
    finishing: &Finishing<'_>,
    store: &mut Store,
    released: &Released,
    ledger: Option<&Ledgered<'_>>,
) -> anyhow::Result<Result<bool, Exit>> {
    let Finishing {
        fork,
        target,
        options,
        bound,
    } = *finishing;
    let removed = store.release_claim(target);
    let had = released.on_ledger.is_some() || (removed && !released.stale_copy);
    let pr = store.tracked_pull(target);
    let Some(text) = finish_provenance(
        released.text.as_deref(),
        had,
        options.superseded_by,
        options.why,
    ) else {
        return Ok(Ok(had));
    };
    // `--superseded-by` always leaves provenance to carry it: the replacement
    // rides on the one entry that already names it.
    let mut draft = Draft {
        statement: options.superseded_by.map(|replacement| Stated {
            statement: Statement {
                kind: StatementKind::Superseded,
                value: Some(replacement.to_owned()),
            },
            replaces: store.statement_stamp(target, StatementKind::Superseded),
        }),
        ..Draft::event(Some(target.branch.as_str()), text, pr).about_claim(
            had.then_some(ClaimChange::Released),
            options.why,
            released.displaced.as_ref(),
        )
    };
    if released.unread_activity {
        draft.previous_last_seen = None;
    }
    let scribe = scribe_for(fork, bound)?;
    if let (Some(ledger), Some(commit)) = (ledger, released.on_ledger.as_ref()) {
        return Ok(release_on_ledger(ledger, commit, &scribe, &draft).map(|()| had));
    }
    scribe.record(&draft)?;
    Ok(Ok(had))
}

/// Delete the ledger's claim of the branch, leased on `commit`, the claim
/// commit the gate read, in one push with the entry `draft` that `scribe`
/// writes; `Err` is the exit after saying why nothing was released, with
/// nothing here changed.
///
/// A claim another machine changed or released meanwhile is read again and
/// named, as the gate names it (exit 1). A ledger that could not be written
/// releases nothing (exit 3). A push whose outcome is unknown may have
/// landed or not, so it leaves the state file and the workspace as they
/// were, and the next `finish` reads the claim again (exit 3).
fn release_on_ledger(
    ledger: &Ledgered<'_>,
    commit: &CommitId,
    scribe: &Scribe,
    draft: &Draft<'_>,
) -> Result<(), Exit> {
    let root = default_ledger_root();
    let change = ledger_sweep::ClaimChange {
        root: &root,
        shared: &ledger.shared,
        claim: &ledger.claim,
        expected: Some(commit),
        becomes: None,
    };
    let branch = &ledger.target.branch;
    match change_on_ledger(change, scribe, draft) {
        ClaimOutcome::Landed { .. } => Ok(()),
        ClaimOutcome::NotApplied {
            why:
                NotApplied::Refused {
                    claim: ClaimSeen::Absent,
                    ..
                },
            ..
        } => Err(changed_meanwhile(ledger)),
        ClaimOutcome::NotApplied {
            why:
                NotApplied::Refused {
                    claim: ClaimSeen::At(now),
                    ..
                },
            ..
        } if now != *commit => Err(changed_meanwhile(ledger)),
        ClaimOutcome::NotApplied { why, failure } => {
            eprintln!(
                "nothing was released: {}; run `knives finish {branch}` again",
                unwritten(
                    &ledger.shared,
                    &failure,
                    &unapplied_cause(&ledger.shared, &why, &failure)
                )
            );
            Err(Exit::Incomplete)
        }
        ClaimOutcome::Unknown { failure, .. } => {
            eprintln!(
                "the release of the claim of {branch} was pushed to the ledger {} ({}), and \
                 whether it landed is unknown: {}; this machine's state file and the workspace \
                 are left as they were, and `knives finish {branch}` reads the claim again",
                failure.ledger,
                git_dir_of(&ledger.shared),
                problem_human_text(&failure.cause)
            );
            Err(Exit::Incomplete)
        }
    }
}

/// A lost lease: the ledger's claim changed between this `finish`'s read
/// and its push, so nothing was released or removed. The claim is read
/// again and named, as the gate names it (exit 1).
fn changed_meanwhile(ledger: &Ledgered<'_>) -> Exit {
    let target = ledger.target;
    let here = ledger.shared.destination.machine.as_str();
    let now = match ledger_sweep::read_claim(&ledger.shared, &ledger.claim) {
        Err(failure) => {
            eprintln!(
                "nothing was released: {}",
                unread_claim(&target.branch, &ledger.shared, &failure)
            );
            return Exit::Incomplete;
        }
        Ok(None) => no_claim_now(&target.branch),
        Ok(Some(HeldClaim {
            commit,
            record: Err(cause),
        })) => unreadable_holder(target, &ledger.claim, &commit, &cause),
        // Changed after this config home read it, so it recorded nothing of
        // it: its activity cannot be read here.
        Ok(Some(HeldClaim {
            commit,
            record: Ok(record),
        })) => {
            let claim = copy_of(target, &commit, record);
            shared_holder(&claim, claimed_on(&claim, here), here, None)
        }
    };
    eprintln!(
        "nothing was released: the claim of {} changed on the ledger while this finish ran\n{now}",
        target.branch
    );
    Exit::Findings
}

/// What `finish` says where the ledger holds no claim of `branch` now,
/// though this machine recorded one or this finish read one: another
/// machine ended it, and a sweep brings that machine's entries here.
fn no_claim_now(branch: &BranchName) -> String {
    format!(
        "the ledger holds no claim of {branch} now; it was released or taken on another \
         machine; run `knives ledger sweep` then `knives notch {branch}` to read by whom"
    )
}

/// `claim`, taken on machine `machine`, as `finish` names its holder on a
/// fork that shares its ledger, `here` being this machine: in `start`'s
/// words ([`render_shared_claim_context`]), with its sighting where its
/// activity can be read here (`last_seen`), else with why it cannot
/// ([`unreadable_activity`]).
fn shared_holder(claim: &Claim, machine: &str, here: &str, last_seen: Option<LastSeen>) -> String {
    let context = render_shared_claim_context(claim, machine, last_seen, jiff::Timestamp::now());
    if last_seen.is_some() {
        context
    } else {
        format!("{context}\n{}", unreadable_activity(claim, machine, here))
    }
}

/// What `finish` says where the ledger holds another owner's claim,
/// `holder` ([`shared_holder`]), in place of the caller's own, which this
/// machine's state file keeps a copy of.
fn copy_superseded(branch: &BranchName, holder: &str) -> String {
    format!(
        "the ledger holds another owner's claim of {branch} in place of yours\n{holder}\nthis \
         machine's record of your claim is removed, and nothing else was changed: the \
         workspace and its registration stay as they are\nuse `knives start {branch} --force \
         --why \"…\"` to take the branch back"
    )
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
///
/// The owner name is the displaced session's own text, copied from the claim
/// it wrote into the state file every session of this machine reads, and the
/// directory is the path jj recorded for the workspace when it has one
/// (`workspace_path`, `src/commands/start.rs`), which whichever session
/// registered that workspace chose, so this line prints both escaped
/// ([`inline_human_text`]): raw, either would let another session choose
/// what the forcing caller's terminal receives.
fn left_to_its_owner(place: &Place<'_>, owner: &str) -> String {
    format!(
        "{} and its `{}` registration are left as they are: the directory is {}'s, and \
         what jj never tracked in it is not this session's to delete",
        inline_human_text(&place.directory.display().to_string()),
        place.workspace,
        inline_human_text(owner),
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
///
/// Every value here is read off disk: the path jj recorded for the workspace
/// when it has one (`workspace_path`, `src/commands/start.rs`), the checkout
/// found for this fork, the workspace name and the store path jj recorded
/// for what sits at the path, a link's target, and jj's own detail for a
/// working copy it cannot read. All of them reach the terminal escaped
/// ([`inline_human_text`], and [`knives::ledger::problem_human_text`] for
/// jj's detail, which prints more than one line); the branch's own workspace
/// name is this process's.
fn removal_note(
    place: &Place<'_>,
    identity: Option<&WorkspaceIdentity>,
    registration: &Registration,
    cleanup: bool,
) -> anyhow::Result<String> {
    let shown = inline_human_text(&place.directory.display().to_string());
    let checkout = inline_human_text(&place.checkout.display().to_string());
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
            format!(
                "{shown} is workspace {} of {checkout}, not {workspace}, left alone",
                inline_human_text(name.as_str())
            )
        }
        Some(WorkspaceIdentity::Unreadable(detail)) => format!(
            "{shown} is a workspace of {checkout} whose working-copy state could not be read \
             ({}), left alone",
            knives::ledger::problem_human_text(detail)
        ),
        Some(WorkspaceIdentity::Foreign(store)) => format!(
            "{shown} belongs to repository {}, not {checkout}, left alone",
            inline_human_text(&store.display().to_string())
        ),
        Some(WorkspaceIdentity::Repository) => {
            format!("{shown} is a repository in its own right, not a workspace, left alone")
        }
        Some(WorkspaceIdentity::SymbolicLink(target_path)) => format!(
            "{shown} is a symbolic link to {}, left alone",
            inline_human_text(&target_path.display().to_string())
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
/// by, whether a claim was released at all, and the reason `why` where no
/// claim was released ([`release_event`]). Forced prose already ends with
/// the reason that forced it.
fn finish_provenance(
    forced: Option<&str>,
    had: bool,
    superseded_by: Option<&str>,
    why: Option<&str>,
) -> Option<String> {
    forced
        .map(|text| {
            superseded_by.map_or_else(
                || text.to_owned(),
                |replacement| format!("{text}; superseded by {replacement}"),
            )
        })
        .or_else(|| release_event(had, superseded_by, why))
}

/// What is at the branch's workspace path, and what the read-only checks
/// before the claim is touched allow `finish` to go on with.
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
            // The path is where the workspace landed on disk, the checkout is
            // the one found for this fork, and the detail is jj's own, which
            // prints more than one line: all three go out escaped
            // ([`inline_human_text`] and
            // [`knives::ledger::problem_human_text`]).
            eprintln!(
                "{}/{branch}: {} is a workspace of {} whose working-copy state could not be read \
                 ({}); it cannot be snapshotted, so it is not forgotten; nothing was changed",
                fork.name,
                inline_human_text(&at.display().to_string()),
                inline_human_text(&checkout.display().to_string()),
                knives::ledger::problem_human_text(detail),
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
///
/// The same list of processes goes two ways, and only one of them is escaped:
/// a command line comes from `/proc/<pid>/cmdline` of a process this caller
/// did not start, so the refusal a terminal reads escapes it
/// ([`inline_human_text`]), as `knives start`'s refusal escapes these same
/// lines (`claim_evidence`, `src/commands/start.rs`), while the forced
/// finish's provenance keeps it raw as [`forced_release_event`] keeps the raw
/// owner, because the entry is the durable record and its reader escapes it
/// when it prints it.
///
/// The directory is where this branch's working copy landed on disk: the
/// path jj recorded for the workspace when it has one (`workspace_path`,
/// `src/commands/start.rs`), or what a symbolic link there points at. Either
/// way another session chose it, so the refusal prints it escaped too.
fn live_users_gate(
    fork: &Fork<'_>,
    branch: &BranchName,
    directory: &Path,
    options: &FinishOptions<'_>,
) -> Result<Option<String>, Exit> {
    let users = knives::liveness::others_using(directory);
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
    let shown = inline_human_text(&list);
    eprintln!(
        "{}/{branch}: {} is in use by {shown}; finishing would take the directory from under \
         it\nuse `knives finish {branch} --force --why \"…\"` once it is done",
        fork.name,
        inline_human_text(&directory.display().to_string())
    );
    Err(Exit::Usage)
}

/// The Git side of a workspace whose directory is now gone: `jj workspace
/// forget` unlinks the worktree it registered, but a directory that vanished
/// before the forget leaves git's `.git/worktrees/<name>` entry behind, locked
/// or prunable, and the next `start` at that path would fail on it. One that
/// stays because something else now sits at the path is named, with what is
/// there, so the failing `start` is not a surprise.
///
/// This note goes on the disposal line a terminal reads: the directory is
/// the path jj recorded for the workspace when it has one (`workspace_path`,
/// `src/commands/start.rs`), what sits there is read off disk, a link's
/// target among it, and the failure is git's own text, which prints more
/// than one line, so all three go out escaped ([`inline_human_text`] and
/// [`knives::ledger::problem_human_text`]).
fn stale_worktree_note(checkout: &Path, directory: &Path) -> String {
    match remove_finished_worktree(checkout, directory) {
        Ok(StaleRegistration::Removed) => "; stale Git worktree registration removed".to_owned(),
        Ok(StaleRegistration::None | StaleRegistration::Locked) => String::new(),
        Ok(StaleRegistration::Occupied) => format!(
            "; {} is {}, so its stale Git worktree registration was not cleared",
            inline_human_text(&directory.display().to_string()),
            inline_human_text(&path_occupant(directory))
        ),
        Err(error) => format!(
            "; Git worktree registration not cleared ({})",
            knives::ledger::problem_human_text(&error.to_string())
        ),
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

/// The disposal line a terminal reads. The path is the one jj recorded for
/// the name and the detail is jj's own text, which prints more than one line,
/// so both go out escaped ([`inline_human_text`] and
/// [`knives::ledger::problem_human_text`]).
impl std::fmt::Display for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forgotten => f.write_str("forgotten"),
            Self::NotRegistered => f.write_str("was not registered"),
            Self::Elsewhere(path) => write!(
                f,
                "not forgotten (it is registered at {}; finish or forget it there)",
                inline_human_text(&path.display().to_string())
            ),
            Self::Failed(detail) => write!(
                f,
                "not forgotten ({})",
                knives::ledger::problem_human_text(detail)
            ),
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

/// The claim gate of a fork that shares no ledger, where the claim is this
/// machine's state file's alone.
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
    let Some(claim) = recorded(store, target) else {
        return unheld_gate(target, store, options, bound);
    };
    let cwd = std::env::current_dir()?;
    let identity = current_identity(bound)?;
    let (decision, refused) = release_decision(&ClaimContext {
        held: vec![&claim],
        identity: &identity,
        in_claimed_workspace: possesses(&cwd, fork, &target.branch),
        machine: None,
    });
    let Some(refused) = refused else {
        // The holder's finish is a release, which writes the ledger: with
        // the ledger unreadable it goes on only to fail on that in
        // `run_finish`, naming it as it always has, rather than being
        // refused for the place first.
        if store.unreadable(&target.repo).is_some() {
            return Ok(FinishClaimGate::Continue(Released::default()));
        }
        return Ok(place_gate(&claim, target, options));
    };
    let last_seen = knives::seen::last_seen_in(refused, &fork.checkout.path, &knives::seen::load());
    if !options.force {
        return Ok(refuse_held(&Refusal {
            target,
            ledger: read_ledger(store, &target.repo),
            identity: &identity,
            claim: refused,
            context: render_claim_context(refused, last_seen, jiff::Timestamp::now()),
            anonymous: decision == ClaimDecision::RefuseAnonymous,
        }));
    }
    let why = options
        .why
        .ok_or_else(|| anyhow::anyhow!("--force requires --why"))?;
    Ok(FinishClaimGate::Continue(forced_release(
        refused,
        &identity,
        Some(last_seen),
        why,
    )))
}

/// What this machine's state file holds for the branch `target`, if
/// anything.
fn recorded(store: &Store, target: &BranchTarget) -> Option<Claim> {
    store
        .claims(Some(&target.repo))
        .into_iter()
        .find(|claim| claim.branch == target.branch.as_str())
        .cloned()
}

/// [`decide`] for a caller handing back the claims `weighed` holds, with
/// the claim that refuses it, where one does.
///
/// Standing in the directory is evidence enough to take work up again,
/// never to hand back a claim taken under another name: after a takeover
/// the directory belongs to the new holder, and the finish an agent runs
/// from inside it would otherwise release that holder's claim and remove
/// the directory under it ([`possession_permits_release`]).
fn release_decision<'a>(weighed: &ClaimContext<'a>) -> (ClaimDecision, Option<&'a Claim>) {
    match decide(weighed) {
        ClaimDecision::Resume { possession: true } => weighed
            .held
            .iter()
            .copied()
            .find(|claim| !possession_permits_release(claim, weighed.identity))
            .map_or(
                (ClaimDecision::Resume { possession: true }, None),
                |claim| (ClaimDecision::RefuseHeld, Some(claim)),
            ),
        decision @ (ClaimDecision::RefuseAnonymous | ClaimDecision::RefuseHeld) => {
            (decision, refusing(weighed))
        }
        decision @ (ClaimDecision::Resume { possession: false } | ClaimDecision::Take) => {
            (decision, None)
        }
    }
}

/// What a forced release of `claim`, another owner's, leaves for the
/// entry: whose claim it ended, with its sighting `last_seen` where its
/// activity can be read here, else the machine it was taken on, and the
/// holder it displaced.
fn forced_release(
    claim: &Claim,
    identity: &Identity,
    last_seen: Option<LastSeen>,
    why: &str,
) -> Released {
    // A claim whose activity cannot be read here is a ledger claim, which
    // names the machine it was taken on.
    let machine = claim.machine.as_deref().unwrap_or_default();
    Released {
        text: Some(forced_release_event(
            claim,
            &sighting_provenance(last_seen, machine),
            why,
        )),
        displaced: Displaced::over(
            claim,
            identity,
            last_seen.unwrap_or(LastSeen::NoneWithinWindow),
        ),
        unread_activity: last_seen.is_none(),
        ..Released::default()
    }
}

/// The claim gate of a fork that shares its ledger: the ledger's claim,
/// read under the fork's lock, and a local claim an older knives left in
/// this machine's state file, weighed as `start` weighs them
/// ([`SharedClaims`], [`release_decision`]).
///
/// A read that fails releases and removes nothing (exit 3). This machine's
/// copy of a ledger claim decides nothing: where the ledger holds no claim
/// now, the copy is removed and the caller's workspace disposed of, as for
/// a claim another owner ended; where the ledger holds another owner's
/// claim in place of the caller's own copy, the caller is told who holds
/// it and only the copy is removed. A holder is named as `start` names it
/// ([`shared_holder`]).
fn shared_claim_gate(
    finishing: &Finishing<'_>,
    store: &Store,
    ledger: &Ledgered<'_>,
) -> anyhow::Result<FinishClaimGate> {
    let Finishing {
        fork,
        target,
        options,
        bound,
    } = *finishing;
    let held = match ledger_sweep::read_claim(&ledger.shared, &ledger.claim) {
        Ok(held) => held,
        Err(failure) => {
            eprintln!(
                "nothing was released: {}",
                unread_claim(&target.branch, &ledger.shared, &failure)
            );
            return Ok(FinishClaimGate::Refuse(Exit::Incomplete));
        }
    };
    let claims = SharedClaims::new(target, held, recorded(store, target));
    let lease = claims.ledger.as_ref().map(|(commit, _)| commit.clone());
    let stale = claims.recorded.as_ref().filter(|copy| {
        copy.ledger_commit.is_some()
            && copy.ledger_commit.as_deref() != lease.as_ref().map(CommitId::as_str)
    });
    if stale.is_some() && lease.is_none() {
        eprintln!("{}", no_claim_now(&target.branch));
        return Ok(FinishClaimGate::Lost(Released {
            stale_copy: true,
            ..Released::default()
        }));
    }
    if let Some((commit, Err(cause))) = &claims.ledger
        && !options.force
    {
        eprintln!(
            "{}\nuse `knives finish {} --force --why \"…\"` to release the claim",
            unreadable_holder(target, &ledger.claim, commit, cause),
            target.branch
        );
        return Ok(FinishClaimGate::Refuse(Exit::Usage));
    }
    let here = ledger.shared.destination.machine.as_str();
    let identity = current_identity(bound)?;
    let cwd = std::env::current_dir()?;
    let (decision, refused) = release_decision(&ClaimContext {
        held: claims.present(),
        identity: &identity,
        in_claimed_workspace: possesses(&cwd, fork, &target.branch),
        machine: Some(here),
    });
    let Some(claim) = refused else {
        return holder_gate(finishing, store, &claims, ledger).map(|gate| with_lease(gate, lease));
    };
    let machine = claimed_on(claim, here);
    let last_seen = claims
        .readable(claim, here)
        .then(|| knives::seen::last_seen_in(claim, &fork.checkout.path, &knives::seen::load()));
    if options.force {
        let why = options
            .why
            .ok_or_else(|| anyhow::anyhow!("--force requires --why"))?;
        return Ok(FinishClaimGate::Continue(Released {
            on_ledger: lease,
            ..forced_release(claim, &identity, last_seen, why)
        }));
    }
    if claim.ledger_commit.is_some() && stale.is_some_and(|copy| copy.owner == identity.owner) {
        eprintln!(
            "{}",
            copy_superseded(
                &target.branch,
                &shared_holder(claim, machine, here, last_seen)
            )
        );
        return Ok(FinishClaimGate::StaleCopy);
    }
    Ok(refuse_held(&Refusal {
        target,
        ledger: read_ledger(store, &target.repo),
        identity: &identity,
        claim,
        context: shared_holder(claim, machine, here, last_seen),
        anonymous: decision == ClaimDecision::RefuseAnonymous,
    }))
}

/// The gate of a caller that every claim present on a fork that shares its
/// ledger permits to hand back: the holder's own finish, judged by where
/// the ledger's claim was taken, or the local claim where only that is
/// present ([`place_gate`]). With no claim whose holder can be read, a
/// ledger claim whose record does not read is released by force, which the
/// gate asked for, and with no claim at all this is a finish of nothing
/// ([`unheld_gate`]).
fn holder_gate(
    finishing: &Finishing<'_>,
    store: &Store,
    claims: &SharedClaims,
    ledger: &Ledgered<'_>,
) -> anyhow::Result<FinishClaimGate> {
    let Finishing {
        target,
        options,
        bound,
        ..
    } = *finishing;
    let Some(claim) = claims.on_ledger().or_else(|| claims.local()) else {
        let Some((commit, _)) = &claims.ledger else {
            return unheld_gate(target, store, options, bound);
        };
        let why = options
            .why
            .ok_or_else(|| anyhow::anyhow!("--force requires --why"))?;
        return Ok(FinishClaimGate::Continue(Released {
            text: Some(format!(
                "released the claim {} at {}, whose holder could not be read, by force: {why}",
                ledger.claim,
                commit.as_str()
            )),
            ..Released::default()
        }));
    };
    // The holder's finish is a release, which writes the ledger: with the
    // fork's ledger unreadable here it goes on only to fail on that in
    // `run_finish`, naming it, rather than being refused for the place first.
    if store.unreadable(&target.repo).is_some() {
        return Ok(FinishClaimGate::Continue(Released::default()));
    }
    Ok(place_gate(claim, target, options))
}

/// `gate`, releasing the ledger's claim at `lease` too where it lets the
/// finish go on.
fn with_lease(gate: FinishClaimGate, lease: Option<CommitId>) -> FinishClaimGate {
    match gate {
        FinishClaimGate::Continue(released) => FinishClaimGate::Continue(Released {
            on_ledger: lease,
            ..released
        }),
        gate => gate,
    }
}

/// A branch nobody holds: an ordinary finish of nothing, unless this fork's
/// ledger says another owner ended the caller's own claim on it. The caller
/// is told that first, because the claim it believed it still held is why it
/// is finishing at all; its own workspace is disposed of all the same.
///
/// A ledger that could not be read answers nothing here: the finish goes on
/// to fail on it in `run_finish`, naming it, before anything is touched. The
/// search is what would tell the caller that its claim was ended, and a
/// finish of nothing without it would report a claim that "was not held" and
/// remove the workspace of an owner that was never told why.
fn unheld_gate(
    target: &BranchTarget,
    store: &Store,
    options: &FinishOptions<'_>,
    bound: Bound<'_>,
) -> anyhow::Result<FinishClaimGate> {
    if options.force {
        return Ok(FinishClaimGate::Continue(Released::default()));
    }
    let Ok(entries) = read_ledger(store, &target.repo) else {
        return Ok(FinishClaimGate::Continue(Released::default()));
    };
    let identity = current_identity(bound)?;
    let Some((change, lost)) = claim_ended(entries, &target.branch, &identity.owner) else {
        return Ok(FinishClaimGate::Continue(Released::default()));
    };
    eprintln!(
        "{}\nnobody holds {} now\nuse `knives start {}` to claim it again",
        displacement(target, &identity.owner, change, lost),
        target.key(),
        target.branch,
    );
    Ok(FinishClaimGate::Lost(Released::default()))
}

/// `repo`'s ledger as the store read it, oldest first, or why it could not
/// be read. `run_finish` opens its store keeping a ledger it cannot read
/// ([`Store::open_each_for_update`]), on which every other ledger accessor
/// of the store panics, so the claim gate asks for the ledger only this way.
fn read_ledger<'a>(store: &'a Store, repo: &UpstreamName) -> Result<&'a [Entry], &'a LedgerError> {
    store
        .unreadable(repo)
        .map_or_else(|| Ok(store.ledger_entries(repo)), Err)
}

/// What a refusal of a claim this caller does not hold is decided from.
struct Refusal<'a> {
    target: &'a BranchTarget,
    /// This fork's ledger as the store read it, oldest first: what became of
    /// the caller's own claim is in it ([`claim_ended`]). Why it could not
    /// be read, where it could not: the refusal is given without the search.
    ledger: Result<&'a [Entry], &'a LedgerError>,
    identity: &'a Identity,
    claim: &'a Claim,
    /// The claim's holder as the refusal names it, with its last sighting:
    /// [`render_claim_context`] on a fork that shares no ledger, and on one
    /// that does, the words `start` names it in ([`shared_holder`]).
    context: String,
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
///
/// A ledger that could not be read is not searched: the caller gets the
/// refusal it would get with nothing found there, and one line saying the
/// ledger could not be read and why. The reason is text this process did not
/// write: a path, the operating system's error, or what a parser said of an
/// entry file the ledger pull may have brought from another machine, and it
/// can run over more than one line, so it goes out escaped
/// ([`knives::ledger::problem_human_text`]).
fn refuse_held(refusal: &Refusal<'_>) -> FinishClaimGate {
    let context = &refusal.context;
    let branch = &refusal.target.branch;
    // The notice belongs to a caller another owner name displaced. A caller
    // whose own name holds the claim lost it to nobody: it is refused because
    // two identities of that name cannot be told apart, and telling it that
    // its claim was taken would name the branch it holds as somebody else's.
    if refusal.claim.owner != refusal.identity.owner
        && let Ok(entries) = refusal.ledger
        && let Some((change, lost)) = claim_ended(entries, branch, &refusal.identity.owner)
    {
        eprintln!(
            "{}\n{context}\nnothing was changed here: the workspace and its registration are \
             the holder's\nuse `knives start {branch} --force --why \"…\"` to take the branch back",
            displacement(refusal.target, &refusal.identity.owner, change, lost),
        );
        return FinishClaimGate::Refuse(Exit::Findings);
    }
    let anonymous_note = if refusal.anonymous {
        "both sides are anonymous identities, so they can never match; "
    } else {
        ""
    };
    let unread = refusal.ledger.err().map_or_else(String::new, |error| {
        format!(
            "the fork's ledger could not be read, so it was not searched for who ended a claim \
             of yours on {branch} ({})\n",
            knives::ledger::problem_human_text(&error.to_string())
        )
    });
    eprintln!(
        "{anonymous_note}{context}\n{unread}use `knives finish {branch} --force --why \"…\"` to \
         release the claim"
    );
    FinishClaimGate::Refuse(Exit::Usage)
}

/// What this fork's ledger says became of `owner`'s claim: who ended it,
/// how, when, and the reason that owner gave for ending it.
///
/// `change` is the change the entry recorded, as [`claim_ended`] matched it,
/// so each word here is the one its own change carries and a later search
/// cannot make this print a verb the entry never recorded.
///
/// Both owner names and the reason are text this process did not write, so
/// all three go to the terminal escaped ([`inline_human_text`]). `lost.owner`
/// and the reason come off a ledger entry, which the ledger pull may have
/// brought from another machine: an entry naming this caller as the owner it
/// displaced would otherwise send terminal control sequences to the caller's
/// own terminal. `owner` is this run's identity, and with no harness variable
/// set that name is read back from a claim in the state file
/// (`crate::commands::hook::owner_for`), which another session wrote. The
/// timestamp is escaped too: a ledger read already rejects an entry whose
/// `ts` no timestamp can be parsed from, but the `ts` is text another machine
/// wrote, and the escape keeps this line safe without depending on that parser.
fn displacement(target: &BranchTarget, owner: &str, change: ClaimChange, lost: &Entry) -> String {
    let verb = match change {
        ClaimChange::Taken => "took",
        ClaimChange::Released => "released",
        ClaimChange::Claimed => "claimed",
        ClaimChange::Resumed => "resumed",
    };
    let why = lost
        .claim_change_why
        .as_deref()
        .map_or_else(String::new, |why| format!(": {}", inline_human_text(why)));
    format!(
        "{} {verb} {}'s claim on {} at {}{why}",
        inline_human_text(&lost.owner),
        inline_human_text(owner),
        target.key(),
        inline_human_text(&lost.ts),
    )
}

/// The entry that ended `owner`'s claim on `branch`, read newest first, with
/// the change it recorded, or nothing where this fork's ledger records no
/// such end.
///
/// The search stops at the newest claim or release `owner` itself wrote,
/// which closed the chapter before it: an owner that claimed the branch
/// again, or handed it back itself, is not told about a takeover it has
/// already answered. An entry that records no claim change — every entry an
/// older knives wrote — neither stops the search nor answers it, and neither
/// does a resume, so an owner that took its work up again in the directory it
/// had lost is still told who took the branch.
fn claim_ended<'a>(
    entries: &'a [Entry],
    branch: &BranchName,
    owner: &str,
) -> Option<(ClaimChange, &'a Entry)> {
    for entry in entries.iter().rev() {
        if entry.subject.as_deref() != Some(branch.as_str()) {
            continue;
        }
        match entry.claim_change() {
            Some(ClaimChange::Claimed | ClaimChange::Released) if entry.owner == owner => {
                return None;
            }
            Some(change @ (ClaimChange::Taken | ClaimChange::Released))
                if entry.previous_owner.as_deref() == Some(owner) =>
            {
                return Some((change, entry));
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
///
/// The owner and the place are the text of whatever session took the claim,
/// held in a state file every session of this machine reads, so the refusal
/// prints both escaped ([`inline_human_text`]): raw, that session would
/// choose what this caller's terminal receives.
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
            ..Released::default()
        });
    }
    eprintln!(
        "{} was claimed by {} in {}; this finish runs in {here}, which cannot see whether a \
         process there still works in the workspace\nuse `knives finish {} --force --why \"…\"` \
         once nothing there uses it",
        claim.key(),
        inline_human_text(&claim.owner),
        inline_human_text(taken),
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
    use std::path::PathBuf;

    use super::{
        Place, Registration, finish_provenance, left_to_its_owner, removal_note, shared_holder,
        spoken, with_requirements,
    };
    use knives::ids::Requirement;
    use knives::jj::WorkspaceIdentity;
    use knives::store::{Claim, OwnerKind};

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

    /// The disposal line `finish` prints names the path jj recorded for the
    /// workspace and quotes jj's own failure text, which runs to more than one
    /// line; neither may carry a terminal control sequence.
    #[test]
    fn a_control_character_in_a_registration_reaches_the_terminal_escaped() {
        assert_eq!(
            Registration::Elsewhere(PathBuf::from("/forks/work/feat\u{7}-a")).to_string(),
            "not forgotten (it is registered at /forks/work/feat\u{fffd}-a; finish or forget it \
             there)"
        );
        assert_eq!(
            Registration::Failed("Error: a\u{1b}[31mb\nand more".to_owned()).to_string(),
            "not forgotten (Error: a\u{fffd}[31mb and more)"
        );
    }

    /// The directory of the disposal line is the path jj recorded for the
    /// workspace, which whichever session registered it chose, and what sits
    /// there is read off disk; neither may drive the terminal of the session
    /// that runs `finish`.
    #[test]
    fn a_control_character_in_a_registered_workspace_path_reaches_the_disposal_line_escaped() {
        let directory = PathBuf::from("/forks/work/feat-a\u{1b}]0;held\u{9b}7m");
        let checkout = PathBuf::from("/forks/work/default");
        let place = Place {
            checkout: &checkout,
            workspace: "feat-a",
            directory: &directory,
        };
        let note = removal_note(
            &place,
            Some(&WorkspaceIdentity::Foreign(PathBuf::from(
                "/other/re\u{7}po",
            ))),
            &Registration::Forgotten,
            true,
        )
        .expect("a removal note");
        assert!(!note.contains('\u{1b}'), "{note}");
        assert!(
            note.contains("/forks/work/feat-a\u{fffd}]0;held\u{fffd}7m"),
            "{note}"
        );
        assert!(note.contains("/other/re\u{fffd}po"), "{note}");
    }

    /// The line a forced release prints for the displaced owner names the same
    /// registered path beside that owner's own name.
    #[test]
    fn a_control_character_in_a_displaced_owners_workspace_path_reaches_the_terminal_escaped() {
        let directory = PathBuf::from("/forks/work/feat-b\u{1b}[31m");
        let checkout = PathBuf::from("/forks/work/default");
        let place = Place {
            checkout: &checkout,
            workspace: "feat-b",
            directory: &directory,
        };
        let line = left_to_its_owner(&place, "agent\u{7}-two");
        assert!(!line.contains('\u{1b}'), "{line}");
        assert!(!line.contains('\u{7}'), "{line}");
        assert!(line.contains("/forks/work/feat-b\u{fffd}[31m"), "{line}");
        assert!(line.contains("agent\u{fffd}-two"), "{line}");
    }

    #[test]
    fn a_released_claim_carries_the_reason_in_its_field_and_the_text_stays_as_it_was() {
        // Given: a finish that released a claim, with a reason
        // When: its entry's text is written, with and without a supersession
        let plain = finish_provenance(None, true, None, Some("the port landed"));
        let superseded = finish_provenance(None, true, Some("feat/next"), Some("the port landed"));

        // Then: the text is what a finish with no reason writes, since the
        // reason is the released claim's own field
        assert_eq!(plain.as_deref(), Some("claim released"));
        assert_eq!(
            superseded.as_deref(),
            Some("claim released; superseded by feat/next")
        );
    }

    #[test]
    fn a_finish_that_released_no_claim_ends_its_text_with_the_reason() {
        // Given: a finish of a branch nobody held, with a reason
        // When: its entry's text is written, with and without a supersession
        let superseded = finish_provenance(None, false, Some("feat/next"), Some("moved on"));
        let plain = finish_provenance(None, false, None, Some("moved on"));

        // Then: the supersession ends with the reason, and a finish that did
        // nothing else records the reason on its own entry
        assert_eq!(
            superseded.as_deref(),
            Some("superseded by feat/next: moved on")
        );
        assert_eq!(
            plain.as_deref(),
            Some("finished with no claim held: moved on")
        );
    }

    #[test]
    fn a_finish_with_no_reason_writes_what_it_always_wrote() {
        assert_eq!(
            finish_provenance(None, true, None, None).as_deref(),
            Some("claim released")
        );
        assert_eq!(
            finish_provenance(None, false, Some("feat/next"), None).as_deref(),
            Some("superseded by feat/next")
        );
        assert_eq!(finish_provenance(None, false, None, None), None);
    }

    #[test]
    fn forced_prose_already_carries_the_reason_that_forced_it() {
        let forced = "finished by force while in use by pid 7 (sleep 120): the box is gone";
        assert_eq!(
            finish_provenance(Some(forced), false, None, Some("the box is gone")).as_deref(),
            Some(forced)
        );
        assert_eq!(
            finish_provenance(
                Some(forced),
                true,
                Some("feat/next"),
                Some("the box is gone")
            )
            .as_deref(),
            Some(
                "finished by force while in use by pid 7 (sleep 120): the box is gone; \
                 superseded by feat/next"
            )
        );
    }

    /// Owner one's claim on the ledger, taken on machine one, its owner and
    /// reason holding terminal control characters.
    fn ledger_claim() -> Claim {
        Claim {
            repo: "acme/demo".into(),
            branch: "feat/alpha".into(),
            owner: "owner\u{1b}[2J-one".into(),
            kind: OwnerKind::HarnessSession,
            why: "port\u{7} the parser".into(),
            started: "2026-11-16T08:00:00Z".into(),
            files: Vec::new(),
            place: Some("box-one 4026531836".into()),
            machine: Some("machine-one".into()),
            ledger_commit: Some("0123456789abcdef0123456789abcdef01234567".into()),
        }
    }

    #[test]
    fn a_holder_whose_activity_cannot_be_read_here_is_named_with_its_machine_and_when_taken() {
        // Given: owner one's claim, taken on machine one
        let claim = ledger_claim();

        // When: a finish on machine two names its holder
        let holder = shared_holder(&claim, "machine-one", "machine-two", None);

        // Then: owner, machine, when it was taken and its reason, escaped,
        // then why its activity cannot be read here, and no sighting
        let (context, unreadable) = holder.split_once('\n').expect("two lines");
        assert!(
            context.starts_with(
                "acme/demo/feat/alpha is claimed by owner\u{fffd}[2J-one (harness-session) on \
                 machine machine-one, taken 2026-11-16T08:00:00Z ("
            ),
            "{holder}"
        );
        assert!(
            context.ends_with("ago): port\u{fffd} the parser"),
            "{holder}"
        );
        assert_eq!(
            unreadable,
            "its activity cannot be read from this machine (machine-two): its sightings and its \
             processes are on machine machine-one"
        );
        assert!(!holder.contains("no activity observed"), "{holder}");
    }
}
