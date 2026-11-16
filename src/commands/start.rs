//! `knives start`: claim a branch and open a workspace on its tip, or on the
//! release's shared base for a new one.
//!
//! On a fork that shares its ledger, the ledger's claim ref decides who
//! holds the branch, on every machine that shares the ledger, and the claim
//! is on the ledger before the workspace is touched ([`shared_start`]). On
//! any other fork the claim is this machine's state file's alone.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::bind::Fork;
use crate::cli::Exit;
use crate::commands::claim::{
    Bound, ClaimContext, ClaimDecision, Identity, SharedClaims, change_on_ledger, claimed_on,
    copy_of, current_identity, decide, git_dir_of, last_seen_provenance, owner_kind_label,
    refusing, render_claim_context, render_shared_claim_context, sighting_provenance,
    unapplied_cause, unread_claim, unreadable_activity, unreadable_holder, unwritten,
};
use crate::commands::release::shared_base;
use crate::commands::wip::workspace_for;
use crate::config::Role;
use crate::detect::BookmarkTips;
use crate::ids::{BookmarkRef, BranchName, BranchTarget, CommitId, RemoteName, WorkspaceName};
use crate::jj::{
    MAX_ACTIVITY_OPS, Repo, StaleRegistration, WorkspaceIdentity, add_workspace, fetch_all,
    forget_workspace, is_workspace_named, path_occupant, registered_workspace_path,
    remove_finished_worktree, remove_prunable_worktree, repo_immutable_heads,
    set_repo_immutable_heads, user_immutable_heads, workspace_identity,
    workspace_recorded_elsewhere,
};
use crate::ledger::{
    ClaimChange, Displaced, Draft, Ledger, Scribe, default_ledger_root, inline_human_text,
    problem_human_text,
};
use crate::ledger_git::{ClaimError, ClaimRecord, ClaimRef, ClaimSeen, HeldClaim};
use crate::ledger_sweep::{self, ClaimOutcome, LedgerFailure, NotApplied, SharedFork};
use crate::release_model::newest_release;
use crate::seen;
use crate::store::{Claim, OwnerKind, Store, default_state_path};

/// Where `start` creates a new workspace for a branch: under the entry's
/// workspace root, named for the branch with slashes flattened.
///
/// Workspaces are cheap to create, well under a second, because tracked content
/// is small even in a large checkout. The real cost is rebuilding language
/// environments, not the checkout.
pub fn new_workspace_path(fork: &Fork<'_>, branch: &BranchName) -> PathBuf {
    fork.workspace_root().join(workspace_for(branch.as_str()))
}

/// Where a branch's workspace is: the path the repository recorded for its
/// registered workspace, else where `start` would create it.
///
/// The configured root says where new workspaces go, not where existing ones
/// are: changing `workspaces` moves only workspaces created afterwards, and
/// every verb that finds an existing workspace (`start` resuming or rebuilding
/// it, `finish`, the possession check) keeps finding one made under an earlier
/// root, its directory vanished or not. A record that cannot be read is no
/// answer, so the configured path stands in.
pub fn workspace_path(fork: &Fork<'_>, branch: &BranchName) -> PathBuf {
    locate_workspace(
        &fork.checkout.path,
        &workspace_for(branch.as_str()),
        new_workspace_path(fork, branch),
    )
}

/// The recorded path of `checkout`'s registered workspace `workspace`, else
/// `fallback`. One exception: with nothing at the recorded path and this very
/// workspace at `fallback`, it was moved there by hand, and `fallback` is where
/// it is. A symbolic link at the recorded path counts as something there: what
/// it is gets judged there.
fn locate_workspace(checkout: &Path, workspace: &str, fallback: PathBuf) -> PathBuf {
    let Some(recorded) = registered_workspace_path(checkout, workspace)
        .ok()
        .flatten()
    else {
        return fallback;
    };
    let vanished = std::fs::symlink_metadata(&recorded).is_err();
    if vanished && is_workspace_named(&fallback, checkout, &WorkspaceName::new(workspace)) {
        return fallback;
    }
    recorded
}

/// Whether the current directory is inside this branch's workspace.
///
/// Possession is the one claim check that needs no identity: standing in the
/// workspace is being the one working there. `cwd` is a physical path, so the
/// workspace's directory is canonicalised before the comparison — a path
/// spelled through a symlink (a directory on another disk) never matched
/// otherwise. And the directory must be this checkout's workspace of this name:
/// with `workspaces` free to point anywhere, standing in a directory at the path
/// is not standing in the workspace.
pub fn possesses(cwd: &Path, fork: &Fork<'_>, branch: &BranchName) -> bool {
    let directory = workspace_path(fork, branch);
    directory
        .canonicalize()
        .is_ok_and(|canonical| cwd.starts_with(canonical))
        && is_workspace_named(
            &directory,
            &fork.checkout.path,
            &WorkspaceName::new(workspace_for(branch.as_str())),
        )
}

/// Whether a branch's workspace would be the primary workspace or the registered
/// checkout — `start` must not create it and `finish` must not remove it.
///
/// The primary workspace is named "default" and the registered checkout is its
/// directory; `start` can never have created either for a branch, so a branch
/// whose flattened name lands on them is a collision, not a workspace to open or
/// clean up. With `workspaces` free to point anywhere, the directory can also be
/// an ancestor of the checkout — `workspaces = ~/forks` above
/// `~/forks/tool/default`, branch `tool` — and removing an ancestor removes the
/// checkout, so containment is the test, not equality. One rule for both verbs:
/// when it lived in `finish` alone, `start default` under a configured directory
/// reached `jj workspace add --name default` and died on jj's raw "already
/// exists".
///
/// A `workspaces` directory inside the checkout is refused when the branch's
/// workspace would be placed there: it puts every branch workspace in the
/// working copy it belongs to, which is never what anyone meant. The registry
/// cannot check it — the checkout is found, not stated — so the two verbs that
/// would use the directory do. The directory is
/// canonicalised when it exists, as the checkout already is, so a spelling
/// through a symlink is judged by where it lands; one that does not exist yet
/// has only its spelling.
///
/// Both lines are refusals a terminal reads. The registered checkout is found
/// on disk, and the workspace directory is the path jj recorded for the name
/// when it has one ([`locate_workspace`]), so both reach the terminal escaped
/// ([`crate::ledger::inline_human_text`]). The `workspaces` root is the
/// entry's own `repos.toml` value and stays raw.
pub fn collides_with_checkout(fork: &Fork<'_>, branch: &BranchName) -> Option<String> {
    let checkout = &fork.checkout.path;
    let spelled = fork.workspace_root();
    let root = spelled
        .canonicalize()
        .unwrap_or_else(|_| spelled.to_owned());
    let workspace = workspace_for(branch.as_str());
    let configured = root.join(&workspace);
    // An existing workspace is judged where it is; a new one where it would go.
    // The root is judged only when it is used: a root set for future
    // workspaces does not lock out one made under an earlier root.
    let directory = locate_workspace(checkout, &workspace, configured.clone());
    if directory == configured && root.starts_with(checkout) {
        return Some(format!(
            "[repos.{}] workspaces {} is inside the checkout {}; branch workspaces cannot \
             live in the working copy they belong to",
            fork.name,
            root.display(),
            crate::ledger::inline_human_text(&checkout.display().to_string())
        ));
    }
    (workspace == "default" || checkout.starts_with(&directory)).then(|| {
        format!(
            "branch {branch} maps to workspace {workspace} at {}, which is the registered \
             checkout itself or contains it; refusing to touch {}",
            crate::ledger::inline_human_text(&directory.display().to_string()),
            crate::ledger::inline_human_text(&checkout.display().to_string())
        )
    })
}

struct StartContext<'a> {
    store: &'a mut Store,
    fork: &'a Fork<'a>,
    branch: &'a BranchName,
    /// The reason the caller gave, which every path that writes a claim
    /// needs: a claim taken, seized, or held and now held for another
    /// reason.
    why: Option<&'a str>,
    identity: Identity,
    upstream_trunk: String,
    destination: PathBuf,
    workspace: WorkspaceName,
    opened: Repo,
}

/// Claim `branch` and open its workspace. `bound` is the registry and the
/// fork the current directory is inside, by the name its claims are kept
/// under, from which a terminal user's identity is derived.
#[allow(
    clippy::too_many_arguments,
    reason = "the fork, the branch, the reason, the override and the cwd binding are independent inputs"
)]
pub fn run(
    fork: &Fork<'_>,
    branch: &BranchName,
    why: Option<&str>,
    force: bool,
    bound: Bound<'_>,
) -> anyhow::Result<Exit> {
    let repo_name = &fork.name;
    let entry = fork.entry;
    let checkout = &fork.checkout.path;
    if let Some(line) = collides_with_checkout(fork, branch) {
        eprintln!("{repo_name}: {line}");
        return Ok(Exit::Usage);
    }
    // Holds this fork's lock, and only this fork's, until the command ends:
    // another `start` on the fork waits and then decides from this
    // one's claim, while a `start` on any other fork runs alongside. The state
    // file's lock is taken only by the save that records the claim.
    let mut store = Store::open_for_update(
        default_state_path(),
        &bound.registry.former_names(),
        &[&fork.upstream],
    )?;
    // A fork that shares its ledger is decided from the ledger's claim, on
    // every machine that shares it. A ledger no git directory here reaches
    // is joined first; one that cannot be joined is no ledger to decide
    // from, so nothing is claimed.
    match ledger_sweep::shared_fork(&default_ledger_root(), bound.registry, &fork.upstream) {
        Ok(Some(shared)) => {
            let request = Request {
                fork,
                branch,
                why,
                force,
                bound,
            };
            return shared_start(&request, &mut store, &shared);
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!(
                "nothing was claimed: {}",
                problem_human_text(&error.to_string())
            );
            return Ok(Exit::Incomplete);
        }
    }
    let cwd = std::env::current_dir()?;
    let destination = workspace_path(fork, branch);
    let in_claimed_workspace = possesses(&cwd, fork, branch);
    let identity = current_identity(bound)?;
    let held = store
        .claims(Some(&fork.upstream))
        .into_iter()
        .find(|claim| claim.branch == branch.as_str())
        .cloned();
    // On a fork that shares no ledger, who holds the branch is in the state
    // file alone, so the decision is made before the repository is opened: a
    // checkout whose operation log cannot be read must still say who holds
    // the claim it is refusing, rather than die on a read only the refusal's
    // sighting ever wanted.
    let decision = decide(&ClaimContext {
        held: held.iter().collect(),
        identity: &identity,
        in_claimed_workspace,
        machine: None,
    });
    let forced = force && held.is_some();
    write_immutable_heads(fork)?;
    if !forced
        && matches!(
            decision,
            ClaimDecision::RefuseAnonymous | ClaimDecision::RefuseHeld
        )
    {
        let claim = held
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("claim decision lacked held claim context"))?;
        let last_seen = seen::last_seen_in(claim, checkout, &seen::load());
        return Ok(refuse_claim(
            claim,
            &destination,
            last_seen,
            decision == ClaimDecision::RefuseAnonymous,
        ));
    }
    // Everything below acts on the repository, so this is where it is opened.
    let mut context = StartContext {
        store: &mut store,
        fork,
        branch,
        why,
        identity,
        upstream_trunk: entry.trunk_ref(),
        workspace: WorkspaceName::new(workspace_for(branch.as_str())),
        opened: Repo::open(checkout)?,
        destination,
    };
    let activity = context.opened.workspace_activity(
        &BTreeSet::from([context.workspace.clone()]),
        MAX_ACTIVITY_OPS,
    )?;
    let observations = seen::load();
    let claim_seen = held
        .as_ref()
        .map(|claim| seen::last_seen(claim, &activity, &observations));

    if forced {
        let (previous, last_seen) = claim_context(held.as_ref(), claim_seen)?;
        return force_claim(&mut context, previous, last_seen);
    }

    match decision {
        ClaimDecision::Resume { possession } => {
            resume_claim(&mut context, held.as_ref(), claim_seen, possession)
        }
        ClaimDecision::Take => take_claim(&mut context),
        // Both returned above, before the repository was opened.
        ClaimDecision::RefuseAnonymous | ClaimDecision::RefuseHeld => Err(anyhow::anyhow!(
            "a refused claim reached the paths that open the repository"
        )),
    }
}

/// Write the entry's `immutable_heads()` rule into the repository's jj
/// config, where no person stated one.
///
/// A rule a human stated is their decision: `status` reports one that
/// differs, and nothing overwrites it. knives' own earlier write (marked by
/// its `doc`) is refreshed when the entry's rule has moved on. A user-level
/// rule is shadowed by the write — jj resolves the repo layer above it — and
/// the line says so. Done on every `start`, refused or not: the rule belongs
/// to the repository, and the agent about to run jj here is the one jj's
/// default would have walled.
fn write_immutable_heads(fork: &Fork<'_>) -> anyhow::Result<()> {
    let checkout = &fork.checkout.path;
    let rule = fork.entry.immutable_heads();
    let stated = repo_immutable_heads(checkout)?;
    let verb = match &stated {
        None => Some("written to"),
        Some(stated) if stated.written_by_knives && stated.rule != rule => Some("refreshed in"),
        Some(_) => None,
    };
    if let Some(verb) = verb {
        let shadowed = if stated.is_none() {
            user_immutable_heads(checkout)?
        } else {
            None
        };
        set_repo_immutable_heads(checkout, &rule)?;
        // The repository-level rule is this entry's `repos.toml` value, but the
        // user-level one it shadows is read back from the jj config a person on
        // this machine wrote, so the line prints it escaped
        // ([`crate::ledger::inline_human_text`]).
        let shadow = shadowed.map_or_else(String::new, |user_rule| {
            format!(
                " (shadows the user-level rule {} here)",
                inline_human_text(&user_rule)
            )
        });
        println!(
            "jj immutable_heads() {verb} {}'s repository config: {rule}{shadow}",
            fork.name
        );
    }
    Ok(())
}

fn claim_context(
    held: Option<&crate::store::Claim>,
    claim_seen: Option<crate::seen::LastSeen>,
) -> anyhow::Result<(&crate::store::Claim, crate::seen::LastSeen)> {
    held.zip(claim_seen)
        .ok_or_else(|| anyhow::anyhow!("claim decision lacked held claim context"))
}

/// What a refusal says first when the claim and the caller are both
/// anonymous: equal names prove nothing there.
const fn anonymous_note(anonymous: bool) -> &'static str {
    if anonymous {
        "both sides are anonymous identities, so they can never match; "
    } else {
        ""
    }
}

fn refuse_claim(
    claim: &crate::store::Claim,
    workspace: &Path,
    last_seen: crate::seen::LastSeen,
    anonymous: bool,
) -> Exit {
    eprintln!(
        "{}{}\n{}\nuse `knives start {} --force --why \"…\"` to seize the claim",
        anonymous_note(anonymous),
        render_claim_context(claim, last_seen, jiff::Timestamp::now()),
        claim_evidence(claim, workspace),
        claim.branch,
    );
    Exit::Usage
}

/// What the refused caller needs to judge the claim: where its workspace is,
/// what runs in it, where it was taken, and what those facts are worth.
///
/// A holder's name sorts nothing when one machine holds forty claims of one
/// session. The question a refused agent has is whether the work is still
/// live, and the only evidence of that on this machine is a process inside
/// the workspace. [`crate::liveness::live_users`] sees what this process can
/// see, so its silence proves nothing about a claim taken in another PID
/// namespace, and the lines say which case this is rather than leaving the
/// reader to compare two places by eye.
///
/// A process line is another party's text: the command comes from the
/// `/proc/<pid>/cmdline` of a process this caller did not start, or its
/// `/proc/<pid>/comm` when that is empty, and the path is a path on disk, so
/// both go to the terminal escaped
/// ([`crate::ledger::inline_human_text`]) — whoever started that process
/// would otherwise choose what the refused caller's terminal receives. The
/// place is the same: whatever session took the claim wrote it into the state
/// file, so it goes out escaped too. The workspace path is the one jj
/// recorded for the name when it has one ([`workspace_path`]), chosen by
/// whichever session registered that workspace, so it goes out escaped as
/// well. The pid is a number this process read as one, and the branch is
/// this caller's own argument.
fn claim_evidence(claim: &crate::store::Claim, workspace: &Path) -> String {
    let users = crate::liveness::others_using(workspace);
    // A directory that vanished leaves the same empty list as a workspace
    // nobody is in, so the path is judged on its own. A dangling symbolic
    // link is something at the path, as it is everywhere else in this file.
    // Only a path that is not there at all is nothing there: an `lstat` that
    // failed for any other reason leaves a workspace that may hold live work,
    // and this evidence claims neither way.
    let nothing_there = std::fs::symlink_metadata(workspace)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
    let mut lines = vec![format!(
        "its workspace is {}{}",
        crate::ledger::inline_human_text(&workspace.display().to_string()),
        if nothing_there {
            ", and nothing is at that path"
        } else {
            ""
        },
    )];
    if users.is_empty() {
        lines.push("no process of this machine is in it".to_owned());
    } else {
        lines.extend(
            users
                .iter()
                .map(|user| format!("  {}", crate::ledger::inline_human_text(&user.to_string()))),
        );
    }
    let here = crate::liveness::this_place();
    let taken_here = claim.place.as_deref() == Some(here.as_str());
    lines.push(match claim.place.as_deref() {
        Some(_) if taken_here => "the claim was taken in this place".to_owned(),
        Some(place) => format!(
            "the claim was taken in another place, {}",
            crate::ledger::inline_human_text(place)
        ),
        None => "the claim records no place".to_owned(),
    });
    lines.push(
        match (users.is_empty(), taken_here) {
            (false, _) => {
                "a process named above is evidence that the work in the workspace is live"
            }
            (true, true) => {
                "the list was read in the place the claim was taken, so nothing this machine can \
                 see is working in the workspace"
            }
            (true, false) => {
                "an empty list is evidence of nothing here, because this machine cannot see the \
                 processes of another host or agent box"
            }
        }
        .to_owned(),
    );
    lines.push(
        "the list leaves out this run's own processes, the processes of its harness session, and \
         any process this user cannot inspect"
            .to_owned(),
    );
    lines.join("\n")
}

fn force_claim(
    context: &mut StartContext<'_>,
    previous: &crate::store::Claim,
    last_seen: crate::seen::LastSeen,
) -> anyhow::Result<Exit> {
    let reason = context
        .why
        .ok_or_else(|| anyhow::anyhow!("--force requires --why"))?;
    let workspace_notice = match workspace_notice(context, true)? {
        Ok(notice) => notice,
        // Refused before seizing, as `take_claim` refuses before claiming: a
        // seized claim with no workspace would leave the branch held and the
        // agent one `--force` further from the work.
        Err(refusal) => {
            eprintln!("{refusal}");
            return Ok(Exit::Usage);
        }
    };
    let displaced = Displaced::over(previous, &context.identity, last_seen);
    record_claim(
        context,
        reason,
        format!(
            "seized from {} ({}, claimed {}, last seen {}): {reason}",
            previous.owner,
            crate::commands::claim::owner_kind_label(previous.kind),
            previous.started,
            last_seen_provenance(last_seen),
        ),
        displaced.as_ref(),
    )?;
    println!(
        "seized {}\n{workspace_notice}",
        render_claim_context(previous, last_seen, jiff::Timestamp::now()),
    );
    Ok(Exit::Ok)
}

fn resume_claim(
    context: &mut StartContext<'_>,
    held: Option<&crate::store::Claim>,
    claim_seen: Option<crate::seen::LastSeen>,
    possession: bool,
) -> anyhow::Result<Exit> {
    let (claim, last_seen) = claim_context(held, claim_seen)?;
    let workspace_notice = resume_workspace_notice(context);
    let event = if possession {
        "resumed via workspace possession"
    } else {
        "resumed"
    };
    let target = BranchTarget::new(context.fork.upstream.clone(), context.branch.clone());
    // The reason a claim carries is why the work is held now, so its holder
    // saying why again replaces it: a reader of a refusal days later is told
    // what the branch is for today, not what the first `start` was for.
    // Nobody else speaks for the claim, and the possession arm resumes
    // under names that may differ: a session standing in the directory of
    // another owner's claim leaves that owner's reason alone.
    let restated = match context.why {
        Some(why) if claim.owner == context.identity.owner => {
            context.store.restate_claim(&target, why)
        }
        _ => None,
    };
    let claim = restated.as_ref().unwrap_or(claim);
    // The immutable explanation before the mutable claim state, as a release
    // records it: a failed append leaves the reason the claim already held.
    scribe_for(context).record(
        &Draft::event(
            Some(context.branch.as_str()),
            event.to_owned(),
            context.store.tracked_pull(&target),
        )
        .about_claim(Some(ClaimChange::Resumed), Some(claim.why.as_str()), None),
    )?;
    context.store.save()?;
    println!(
        "{event}\n{}\n{workspace_notice}",
        render_claim_context(claim, last_seen, jiff::Timestamp::now()),
    );
    Ok(Exit::Ok)
}

/// Claim the branch and say so, adopting the directory that is already there
/// or making a new workspace.
///
/// Every owner name printed here goes out escaped
/// ([`crate::ledger::inline_human_text`]), this run's own included: with no
/// harness variable set the identity is read back from a claim in the state
/// file that another session wrote
/// (`crate::commands::hook::owner_for`), and a printer that escaped only
/// that case would be a rule no reader of this code, and no reader of the
/// output, could tell apart from the other. Bound once, so both `claimed …
/// for` lines say the same thing and one test covers both.
fn take_claim(context: &mut StartContext<'_>) -> anyhow::Result<Exit> {
    let reason = context.why.unwrap_or("started work");
    let owner = crate::ledger::inline_human_text(&context.identity.owner);
    if context.destination.exists() {
        if let Some(line) = adoption_refusal(
            &context.destination,
            &context.fork.checkout.path,
            &context.workspace,
        ) {
            eprintln!("{line}");
            return Ok(Exit::Usage);
        }
        let change = adopted_change(context)?;
        record_claim(
            context,
            reason,
            format!("claimed: {reason} (adopted existing workspace)"),
            None,
        )?;
        println!(
            "adopted existing workspace {} at {change}; left as-is\nclaimed {}/{} for {owner}",
            crate::ledger::inline_human_text(&context.destination.display().to_string()),
            context.fork.name,
            context.branch,
        );
        return Ok(Exit::Ok);
    }

    let (base_revision, base_label) = match create_workspace(context, Leftovers::Keep)? {
        WorkspaceBase::At { revision, label } => (revision, label),
        WorkspaceBase::Divergent(tips) => {
            // Nothing was claimed: the agent picks a tip and starts again.
            eprintln!("{}", divergent_refusal_line(context.branch, &tips));
            return Ok(Exit::Usage);
        }
    };
    let change = workspace_change(
        &Repo::open(&context.fork.checkout.path)?,
        &context.workspace,
    )?;
    record_claim(context, reason, format!("claimed: {reason}"), None)?;
    println!(
        "workspace {} at {change} based on {base_revision} ({base_label})\nclaimed {}/{} for \
         {owner}",
        crate::ledger::inline_human_text(&context.destination.display().to_string()),
        context.fork.name,
        context.branch,
    );
    Ok(Exit::Ok)
}

/// Write the claim and the entry that explains it.
///
/// `displaced` is the holder this write took the claim from, where another
/// owner name held it. That, and only that, makes the entry a `taken` one:
/// a forced start over the caller's own claim displaced nobody, however its
/// prose reads.
fn record_claim(
    context: &mut StartContext<'_>,
    reason: &str,
    event: String,
    displaced: Option<&Displaced>,
) -> anyhow::Result<()> {
    let target = BranchTarget::new(context.fork.upstream.clone(), context.branch.clone());
    let pull = context.store.tracked_pull(&target);
    let _ = context.store.claim(&target, &context.identity, reason);
    let change = if displaced.is_some() {
        ClaimChange::Taken
    } else {
        ClaimChange::Claimed
    };
    scribe_for(context).record(
        &Draft::event(Some(context.branch.as_str()), event, pull).about_claim(
            Some(change),
            Some(reason),
            displaced,
        ),
    )?;
    context.store.save()?;
    Ok(())
}

/// The writer of this `start`'s entries: the fork's ledger, anchored in its
/// checkout, as the caller's owner name.
fn scribe_for(context: &StartContext<'_>) -> Scribe {
    Scribe::new(
        Ledger::for_fork(context.fork),
        context.fork.upstream.clone(),
        context.fork.checkout.path.clone(),
        context.identity.owner.clone(),
    )
}

/// The working copy of the directory a take adopts, which the repository
/// is told about again first when it lost track of it.
///
/// The identity gate before this ([`adoption_refusal`]) already
/// established this is our workspace of this name; `reattach_workspace`
/// re-checks on its own behalf as the mutation's guard, and a mismatch it
/// finds is a race, not a user error.
fn adopted_change(context: &StartContext<'_>) -> anyhow::Result<String> {
    if let Ok(change) = workspace_change(&context.opened, &context.workspace) {
        return Ok(change);
    }
    context
        .opened
        .reattach_workspace(&context.destination, &context.workspace)?;
    workspace_change(
        &Repo::open(&context.fork.checkout.path)?,
        &context.workspace,
    )
}

/// The workspace line for a resumed claim: where this branch's workspace is
/// and what its working copy is at, or why there is nothing to resume in.
///
/// The path is the one jj recorded for the workspace when it has one
/// ([`workspace_path`]), which whichever session registered it chose, and
/// the error is jj's own text, which prints more than one line, so both
/// reach the terminal escaped ([`crate::ledger::inline_human_text`] and
/// [`crate::ledger::problem_human_text`]). The branch and the workspace name
/// are this process's own.
fn resume_workspace_notice(context: &StartContext<'_>) -> String {
    let shown = crate::ledger::inline_human_text(&context.destination.display().to_string());
    if !context.destination.exists() {
        return format!(
            "workspace missing at {shown}; `knives start {} --force --why \"…\"` rebuilds it",
            context.branch
        );
    }
    match workspace_change(&context.opened, &context.workspace) {
        Ok(change) => format!("workspace {shown} at {change}"),
        Err(error) => format!(
            "workspace {shown} is present but not registered as {} ({}); \
             `knives start {} --force --why \"…\"` rebuilds it",
            context.workspace,
            crate::ledger::problem_human_text(&error.to_string()),
            context.branch
        ),
    }
}

/// Why the directory at `destination` cannot be adopted as `checkout`'s workspace
/// named `name`, or `None` when that is what it is.
///
/// A registered name with another repository's directory at the path was adopted
/// as ours, and the next `finish` removed it. With `workspaces` free to point
/// anywhere, what sits at the path is not knives' to assume, registered name or
/// not. The checkout itself and its ancestors never get this far:
/// [`collides_with_checkout`] refuses them before any claim work.
///
/// Every value here is read off disk: the workspace name and the store path
/// jj recorded, the path jj recorded for this name (the destination the
/// caller located), the link's target, and jj's own detail for a working copy
/// it cannot read. All of them reach the terminal escaped
/// ([`crate::ledger::inline_human_text`], and
/// [`crate::ledger::problem_human_text`] for jj's detail, which prints more
/// than one line); the branch's own workspace name is this process's.
fn adoption_refusal(destination: &Path, checkout: &Path, name: &WorkspaceName) -> Option<String> {
    let shown = crate::ledger::inline_human_text(&destination.display().to_string());
    let here = crate::ledger::inline_human_text(&checkout.display().to_string());
    let line = match workspace_identity(destination, checkout) {
        WorkspaceIdentity::Ours(actual) if actual == *name => return None,
        WorkspaceIdentity::Ours(actual) => {
            let actual = crate::ledger::inline_human_text(actual.as_str());
            format!(
                "cannot adopt {shown}: it is workspace {actual} of {here}, not {name}; forget or \
                 finish workspace {actual}, or choose a different branch"
            )
        }
        WorkspaceIdentity::Foreign(store) => format!(
            "cannot adopt {shown}: it belongs to repository {}, not {here}; move the foreign \
             workspace or choose a different branch",
            crate::ledger::inline_human_text(&store.display().to_string())
        ),
        WorkspaceIdentity::Unreadable(detail) => format!(
            "cannot adopt {shown}: it is a workspace of {here} whose working-copy state could \
             not be read ({})",
            crate::ledger::problem_human_text(&detail)
        ),
        WorkspaceIdentity::Repository => format!(
            "cannot adopt {shown}: it is a repository in its own right, not a workspace of \
             {here}; choose a different branch"
        ),
        WorkspaceIdentity::SymbolicLink(target) => format!(
            "cannot adopt {shown}: it is a symbolic link to {}; start from the real directory \
             or choose a different branch",
            crate::ledger::inline_human_text(&target.display().to_string())
        ),
        WorkspaceIdentity::NotAWorkspace => format!(
            "cannot adopt {shown}: not a workspace of {here}; move it or choose a different \
             branch"
        ),
    };
    Some(line)
}

/// The workspace line for a claim being seized: the existing workspace, or the
/// one just created. `Err` is the refusal line; nothing is claimed in that case,
/// so a plain `start` afterwards is the way back.
///
/// The path is the one jj recorded for the workspace when it has one
/// ([`workspace_path`]), so both lines print it escaped
/// ([`crate::ledger::inline_human_text`]); the refusal escapes its own values
/// ([`adoption_refusal`]).
fn workspace_notice(
    context: &StartContext<'_>,
    left_as_is: bool,
) -> anyhow::Result<Result<String, String>> {
    if context.destination.exists() {
        if let Some(line) = adoption_refusal(
            &context.destination,
            &context.fork.checkout.path,
            &context.workspace,
        ) {
            return Ok(Err(line));
        }
        let retained = if left_as_is { "; left as-is" } else { "" };
        return Ok(Ok(format!(
            "workspace {} at {}{retained}",
            crate::ledger::inline_human_text(&context.destination.display().to_string()),
            workspace_change(&context.opened, &context.workspace)?,
        )));
    }
    Ok(match create_workspace(context, Leftovers::Clear)? {
        WorkspaceBase::At { revision, label } => Ok(format!(
            "created missing workspace {} at {} based on {revision} ({label})",
            crate::ledger::inline_human_text(&context.destination.display().to_string()),
            workspace_change(
                &Repo::open(&context.fork.checkout.path)?,
                &context.workspace
            )?,
        )),
        WorkspaceBase::Divergent(tips) => Err(divergent_refusal_line(context.branch, &tips)),
    })
}

/// What [`create_workspace`] does with what a workspace whose directory vanished
/// left behind at the branch's name and path.
#[derive(Clone, Copy)]
enum Leftovers {
    /// A plain `start`: remove a prunable Git registration, and refuse on a
    /// locked one, which cannot be told apart from another machine's live
    /// workspace in a shared store.
    Keep,
    /// A seized or forced claim, whose `--why` takes the workspace over: forget
    /// jj's registration of the name, then remove git's of the path, locked or
    /// not, so the rebuild's `add` can reuse both.
    Clear,
}

fn clear_vanished_workspace(context: &StartContext<'_>) -> anyhow::Result<()> {
    let checkout = &context.fork.checkout.path;
    // The path is the one jj recorded for the workspace when it has one
    // ([`workspace_path`]), so the lines a terminal reads print it escaped
    // ([`crate::ledger::inline_human_text`]).
    let shown = crate::ledger::inline_human_text(&context.destination.display().to_string());
    if check_vanished_workspace(context)? {
        forget_workspace(checkout, context.workspace.as_str())?;
        println!(
            "forgot workspace {} (its directory at {shown} was gone)",
            context.workspace
        );
    }
    match remove_finished_worktree(checkout, &context.destination)? {
        StaleRegistration::Removed => println!(
            "removed a stale Git worktree registration at {shown} (its directory was gone)"
        ),
        StaleRegistration::Occupied => refuse_occupied(context)?,
        StaleRegistration::None | StaleRegistration::Locked => {}
    }
    Ok(())
}

/// The refusals of [`clear_vanished_workspace`], which change nothing, so a
/// `start` on a shared fork makes them before its claim is on the ledger;
/// whether jj has the branch's workspace name registered, for the clear to
/// forget.
fn check_vanished_workspace(context: &StartContext<'_>) -> anyhow::Result<bool> {
    let checkout = &context.fork.checkout.path;
    // The refusals below are errors, which `main::error_line` escapes once
    // for all of them.
    let shown = crate::ledger::inline_human_text(&context.destination.display().to_string());
    // Nothing is forgotten while something sits at the path: `add` would fail
    // on it, and the forget could not be taken back.
    if std::fs::symlink_metadata(&context.destination).is_ok() {
        refuse_occupied(context)?;
    }
    let registered = context
        .opened
        .workspaces()?
        .iter()
        .any(|(name, _)| *name == context.workspace);
    if registered {
        // The name is the repository's: a same-named workspace jj recorded at
        // another path is live there, and forgetting it would take it over.
        if let Some(elsewhere) = workspace_recorded_elsewhere(
            checkout,
            context.workspace.as_str(),
            &context.destination,
        )? {
            anyhow::bail!(
                "workspace {} is registered at {}, not {shown}; finish or forget it there",
                context.workspace,
                elsewhere.display()
            );
        }
    }
    Ok(registered)
}

/// The refusal for a workspace path that holds something other than a
/// directory: a stale Git registration there stays, and `add` would fail on it.
fn refuse_occupied(context: &StartContext<'_>) -> anyhow::Result<()> {
    anyhow::bail!(
        "{} is {}, not a workspace directory; move it aside and start again",
        context.destination.display(),
        path_occupant(&context.destination)
    )
}

/// The commit a new workspace's working copy goes on, or why there is none.
enum WorkspaceBase {
    At {
        revision: String,
        label: String,
    },
    /// The branch's local bookmark names several commits, so there is no one tip
    /// to continue from.
    Divergent(Vec<CommitId>),
}

/// The refusal for a divergent branch: which tips, and how to pick one. Nothing
/// was claimed, so a plain `start` afterwards is the way back.
fn divergent_refusal_line(branch: &BranchName, tips: &[CommitId]) -> String {
    let listed: Vec<&str> = tips.iter().map(CommitId::short).collect();
    format!(
        "{branch} is divergent ({} tips: {}); `jj bookmark set {branch} -r <commit> \
         --allow-backwards` on the one to continue, then start again",
        tips.len(),
        listed.join(", ")
    )
}

/// Create the branch's workspace with its working copy on the right commit
/// ([`workspace_base`]), clearing first what a vanished one left at its
/// name and path as `leftovers` says; the commit it was put on.
fn create_workspace(
    context: &StartContext<'_>,
    leftovers: Leftovers,
) -> anyhow::Result<WorkspaceBase> {
    let base = workspace_base(context)?;
    if let WorkspaceBase::At { revision, .. } = &base {
        clear_leftovers(context, leftovers)?;
        add_workspace(
            &context.fork.checkout.path,
            context.workspace.as_str(),
            &context.destination,
            revision,
        )?;
    }
    Ok(base)
}

/// The commit a new workspace of the branch goes on, after a fetch.
///
/// A branch that already exists — locally, or on one of our remotes, which the
/// fetch just brought in — is continued from its tip: the working copy is an
/// empty child of it, so the agent's next commit is the branch's next commit.
/// Basing an existing branch on the shared base put the agent one `jj new
/// <branch>` away from the work they claimed, with nothing saying so.
///
/// A new branch starts from the release's shared base, the point every member
/// forks from; without a release, the fetched upstream trunk. Never the current
/// `@`: an agent in a release workspace could otherwise run `jj new` and
/// silently inherit the release merge as a parent.
fn workspace_base(context: &StartContext<'_>) -> anyhow::Result<WorkspaceBase> {
    let entry = context.fork.entry;
    let checkout = &context.fork.checkout.path;
    fetch_all(checkout)?;
    let opened = Repo::open(checkout)?;
    let tips = opened.bookmark_tips()?;
    let ours = [
        RemoteName::new(Role::Origin.to_string()),
        RemoteName::new(entry.publish_remote()),
    ];
    let (revision, label) = match branch_tip(&opened, &tips, context.branch, &ours)? {
        BranchTip::Local(tip) => (tip.as_str().to_owned(), format!("{}'s tip", context.branch)),
        BranchTip::Remote(remote, tip) => (
            tip.as_str().to_owned(),
            format!("{}'s tip at {remote}", context.branch),
        ),
        BranchTip::Divergent(targets) => return Ok(WorkspaceBase::Divergent(targets)),
        BranchTip::Unknown => {
            let upstream_trunk = context.upstream_trunk.as_str();
            let scheme = entry.release_scheme();
            let base = match newest_release(&tips, &scheme, entry.publish_remote()) {
                Some((_, release)) => {
                    let trunk_tip = opened.resolve_commit(upstream_trunk)?;
                    shared_base(&opened, &release, &trunk_tip)?
                }
                None => None,
            };
            base.map_or_else(
                || {
                    (
                        upstream_trunk.to_owned(),
                        "the fetched upstream trunk".to_owned(),
                    )
                },
                |commit| {
                    (
                        commit.as_str().to_owned(),
                        "the release's shared base".to_owned(),
                    )
                },
            )
        }
    };
    Ok(WorkspaceBase::At { revision, label })
}

/// Clear what a workspace whose directory vanished left at the branch's
/// name and path, as `leftovers` says.
///
/// A registration whose directory is gone would make `add` fail on git's
/// "already registered" and leave an empty directory at the path.
fn clear_leftovers(context: &StartContext<'_>, leftovers: Leftovers) -> anyhow::Result<()> {
    match leftovers {
        Leftovers::Clear => clear_vanished_workspace(context)?,
        Leftovers::Keep => {
            match remove_prunable_worktree(&context.fork.checkout.path, &context.destination)? {
                StaleRegistration::None => {}
                StaleRegistration::Removed => println!(
                    "removed a stale Git worktree registration at {} (its directory was gone)",
                    crate::ledger::inline_human_text(&context.destination.display().to_string())
                ),
                StaleRegistration::Locked => anyhow::bail!(
                    "a locked Git worktree registration at {} has no directory there; if it is \
                     this branch's and no other machine has it open, `knives finish {}` clears it",
                    context.destination.display(),
                    context.branch
                ),
                StaleRegistration::Occupied => refuse_occupied(context)?,
            }
        }
    }
    Ok(())
}

/// Where a branch that may already exist has its tip.
enum BranchTip {
    Local(CommitId),
    /// Not tracked locally; pushed by someone else to one of our remotes.
    Remote(RemoteName, CommitId),
    /// The local bookmark names several commits.
    Divergent(Vec<CommitId>),
    /// Nothing of ours names it: a new branch.
    Unknown,
}

/// `ours` are the remotes a branch can already exist on and be continued from,
/// in preference order. Upstream is somebody else's repository: a name that
/// exists only there is one of their branches, and a fork branch that happens
/// to share the name is new here — started on the shared base, never on
/// upstream's tip, which sits on a newer trunk than the release's.
fn branch_tip(
    opened: &Repo,
    tips: &BookmarkTips,
    branch: &BranchName,
    ours: &[RemoteName],
) -> anyhow::Result<BranchTip> {
    if let Some(tip) = tips.get(&BookmarkRef::Local(branch.clone())) {
        return Ok(BranchTip::Local(tip.clone()));
    }
    if let Some((_, targets)) = opened
        .conflicted_bookmarks()?
        .into_iter()
        .find(|(reference, _)| matches!(reference, BookmarkRef::Local(named) if named == branch))
    {
        return Ok(BranchTip::Divergent(targets));
    }
    for remote in ours {
        if let Some(tip) = tips.get(&BookmarkRef::Remote {
            branch: branch.clone(),
            remote: remote.clone(),
        }) {
            return Ok(BranchTip::Remote(remote.clone(), tip.clone()));
        }
    }
    Ok(BranchTip::Unknown)
}

fn workspace_change(repo: &Repo, workspace: &WorkspaceName) -> anyhow::Result<String> {
    repo.workspaces()?
        .into_iter()
        .find_map(|(name, change)| (name == *workspace).then(|| change.as_str().to_owned()))
        .ok_or_else(|| anyhow::anyhow!("workspace {} has no working copy", workspace.as_str()))
}

/// What `knives start` was asked: the fork, the branch, the reason, the
/// override and the cwd binding.
#[derive(Clone, Copy)]
struct Request<'a> {
    fork: &'a Fork<'a>,
    branch: &'a BranchName,
    why: Option<&'a str>,
    force: bool,
    bound: Bound<'a>,
}

/// Where the branch's claim is on a fork that shares its ledger, and the
/// claim commit this `start` read there.
struct OnLedger<'a> {
    /// The ledger root.
    root: PathBuf,
    shared: &'a SharedFork,
    claim: ClaimRef,
    /// The ledger's claim commit as this `start` read it; `None` where it
    /// read none. A take, a seize and a restate lease on it, so none lands
    /// over a claim another machine took or changed since.
    read: Option<CommitId>,
}

impl OnLedger<'_> {
    /// This machine's name on the ledger.
    fn machine(&self) -> &str {
        &self.shared.destination.machine
    }

    /// A change of the claim from `expected` to `becomes`, `None` releasing
    /// it.
    fn change<'b>(
        &'b self,
        expected: Option<&'b CommitId>,
        becomes: Option<ClaimRecord>,
    ) -> ledger_sweep::ClaimChange<'b> {
        ledger_sweep::ClaimChange {
            root: &self.root,
            shared: self.shared,
            claim: &self.claim,
            expected,
            becomes,
        }
    }
}

/// The record of `claim` as its claim commit holds it: the same holder,
/// taken when and where and on the machine it was.
fn record_of(claim: &Claim, here: &str) -> ClaimRecord {
    ClaimRecord {
        repo: claim.repo.clone(),
        branch: claim.branch.clone(),
        owner: claim.owner.clone(),
        kind: claim.kind,
        why: claim.why.clone(),
        started: claim.started.clone(),
        place: claim.place.clone(),
        machine: claimed_on(claim, here).to_owned(),
        entry: String::new(),
    }
}

/// A claim of the branch for the caller, taken now, in this place and on
/// this machine, for `reason`.
fn new_record(context: &StartContext<'_>, on: &OnLedger<'_>, reason: &str) -> ClaimRecord {
    ClaimRecord {
        repo: context.fork.upstream.to_string(),
        branch: context.branch.to_string(),
        owner: context.identity.owner.clone(),
        kind: context.identity.kind,
        why: reason.to_owned(),
        started: jiff::Timestamp::now().to_string(),
        place: Some(crate::liveness::this_place()),
        machine: on.machine().to_owned(),
        entry: String::new(),
    }
}

/// What `start` says where the branch's claim could not be read from the
/// ledger: nothing was claimed, where it was read from, and git's account
/// of why, which quotes the remote.
fn unread(branch: &BranchName, shared: &SharedFork, failure: &LedgerFailure) -> String {
    format!(
        "nothing was claimed: {}",
        unread_claim(branch, shared, failure)
    )
}

/// What `start` says where the claim it read is gone from the ledger now:
/// it was released while this `start` ran, so a `start` again takes it.
fn released_meanwhile(branch: &BranchName, on: &OnLedger<'_>) -> String {
    format!(
        "nothing was claimed: the ledger {} holds no claim of {branch} now; it was released \
         while this start ran, and `knives start {branch}` takes it",
        on.shared.ledger
    )
}

/// `start` on a fork that shares its ledger.
///
/// The ledger's claim ref decides who holds the branch, on every machine
/// that shares the ledger. It is read under the fork's lock and weighed
/// with a local claim an older knives left in this machine's state file
/// ([`decide`]), and every refusal comes before anything is written to the
/// ledger. A take, a seize or a restate is then one push, leased on the
/// claim commit read, so a claim another machine took meanwhile refuses it;
/// the workspace is touched only once the ledger holds the claim, and the
/// state file keeps its copy only after that. A claim that cannot be read,
/// or pushed, is no claim: nothing is claimed here, and the exit is 3.
fn shared_start(
    request: &Request<'_>,
    store: &mut Store,
    shared: &SharedFork,
) -> anyhow::Result<Exit> {
    let Request {
        fork,
        branch,
        why,
        force,
        bound,
    } = *request;
    let claim = match ClaimRef::new(&fork.upstream, branch) {
        Ok(claim) => claim,
        Err(error @ (ClaimError::TooLong { .. } | ClaimError::Refused { .. })) => {
            eprintln!("{}: {}", fork.name, inline_human_text(&error.to_string()));
            return Ok(Exit::Usage);
        }
        Err(error) => return Err(error.into()),
    };
    let held = match ledger_sweep::read_claim(shared, &claim) {
        Ok(held) => held,
        Err(failure) => {
            eprintln!("{}", unread(branch, shared, &failure));
            return Ok(Exit::Incomplete);
        }
    };
    let target = BranchTarget::new(fork.upstream.clone(), branch.clone());
    let recorded = store
        .claims(Some(&fork.upstream))
        .into_iter()
        .find(|claim| claim.branch == branch.as_str())
        .cloned();
    let claims = SharedClaims::new(&target, held, recorded);
    let on = OnLedger {
        root: default_ledger_root(),
        shared,
        claim,
        read: claims.ledger.as_ref().map(|(commit, _)| commit.clone()),
    };
    let cwd = std::env::current_dir()?;
    let destination = workspace_path(fork, branch);
    let identity = current_identity(bound)?;
    let weighed = ClaimContext {
        held: claims.present(),
        identity: &identity,
        in_claimed_workspace: possesses(&cwd, fork, branch),
        machine: Some(on.machine()),
    };
    let decision = decide(&weighed);
    let forced = force && claims.any();
    write_immutable_heads(fork)?;
    if !forced {
        if let Some((commit, Err(cause))) = &claims.ledger {
            eprintln!(
                "{}",
                unreadable_record_refusal(&target, &on.claim, commit, cause)
            );
            return Ok(Exit::Usage);
        }
        if let Some(claim) = refusing(&weighed) {
            let activity = claims.readable(claim, on.machine()).then(|| {
                (
                    seen::last_seen_in(claim, &fork.checkout.path, &seen::load()),
                    claim_evidence(claim, &destination),
                )
            });
            let refused = Refused {
                claim,
                machine: claimed_on(claim, on.machine()),
                here: on.machine(),
                activity,
                anonymous: decision == ClaimDecision::RefuseAnonymous,
            };
            eprintln!("{}", shared_refusal(refused, jiff::Timestamp::now()));
            return Ok(Exit::Usage);
        }
    }
    // Everything below acts on the repository, so this is where it is opened.
    let mut context = StartContext {
        store,
        fork,
        branch,
        why,
        identity,
        upstream_trunk: fork.entry.trunk_ref(),
        workspace: WorkspaceName::new(workspace_for(branch.as_str())),
        opened: Repo::open(&fork.checkout.path)?,
        destination,
    };
    if forced {
        return seize_shared(&mut context, &on, &claims);
    }
    match decision {
        ClaimDecision::Resume { possession } => {
            resume_shared(&mut context, &on, &claims, possession)
        }
        ClaimDecision::Take => take_shared(&mut context, &on),
        // Both returned above, before the repository was opened.
        ClaimDecision::RefuseAnonymous | ClaimDecision::RefuseHeld => Err(anyhow::anyhow!(
            "a refused claim reached the paths that open the repository"
        )),
    }
}

/// A claim on a fork that shares its ledger that refuses this `start`, as
/// the refusal shows it.
struct Refused<'a> {
    claim: &'a Claim,
    /// The machine it was taken on.
    machine: &'a str,
    /// This machine.
    here: &'a str,
    /// Its last sighting and the evidence of its workspace
    /// ([`claim_evidence`]), where its activity can be read here.
    activity: Option<(seen::LastSeen, String)>,
    /// Whether the claim and the caller are both anonymous.
    anonymous: bool,
}

/// The refusal a claim on a fork that shares its ledger gives a `start` it
/// does not permit.
///
/// Where the claim's activity can be read here, it is the refusal of a fork
/// that shares no ledger, naming the machine the claim was taken on. Where
/// it cannot, it says when the claim was taken and why its activity cannot
/// be read, and prints no workspace, sighting, process or place line: each
/// would be this machine's empty records of work done elsewhere, and would
/// read as evidence that nothing is working on it.
fn shared_refusal(refused: Refused<'_>, now: jiff::Timestamp) -> String {
    let (last_seen, evidence) = match refused.activity {
        Some((last_seen, evidence)) => (Some(last_seen), evidence),
        None => (
            None,
            unreadable_activity(refused.claim, refused.machine, refused.here),
        ),
    };
    format!(
        "{}{}\n{evidence}\nuse `knives start {} --force --why \"…\"` to seize the claim",
        anonymous_note(refused.anonymous),
        render_shared_claim_context(refused.claim, refused.machine, last_seen, now),
        refused.claim.branch,
    )
}

/// The refusal of a claim commit whose record does not read: a claim all
/// the same, whose holder this `start` cannot name. Its record's fault is
/// the text of whoever wrote the commit, so it goes out escaped.
fn unreadable_record_refusal(
    target: &BranchTarget,
    claim: &ClaimRef,
    commit: &CommitId,
    cause: &str,
) -> String {
    format!(
        "{}\nuse `knives start {} --force --why \"…\"` to replace the claim",
        unreadable_holder(target, claim, commit, cause),
        target.branch,
    )
}

/// What a take or a seize does to the workspace once the ledger holds its
/// claim.
enum Prepared {
    /// Adopt the directory at the branch's workspace path.
    Adopt,
    /// Create the workspace on `revision`, which `label` names.
    Create { revision: String, label: String },
}

/// Every refusal of the workspace that changes nothing, made before the
/// claim is on the ledger, so that none of them leaves a claim with no
/// workspace. `Err` is a refusal line (exit 2); an error is a refusal too,
/// as wherever a workspace is made.
///
/// The directory at the path is adopted, or refused as [`adoption_refusal`]
/// refuses it. Where there is none, the fork is fetched and the commit to
/// create the workspace on is chosen, with its divergence refusal
/// ([`workspace_base`]), and what a vanished workspace left at the path is
/// judged: something there that is not a directory refuses. For a plain
/// `start` a locked Git registration refuses, and a prunable one, which git
/// would prune itself, is removed here ([`clear_leftovers`]); a seize
/// refuses a workspace of the name that jj recorded at another path, and
/// clears the rest after its push ([`check_vanished_workspace`]).
fn prepare_workspace(
    context: &StartContext<'_>,
    leftovers: Leftovers,
) -> anyhow::Result<Result<Prepared, String>> {
    if context.destination.exists() {
        return Ok(adoption_refusal(
            &context.destination,
            &context.fork.checkout.path,
            &context.workspace,
        )
        .map_or(Ok(Prepared::Adopt), Err));
    }
    let (revision, label) = match workspace_base(context)? {
        WorkspaceBase::At { revision, label } => (revision, label),
        WorkspaceBase::Divergent(tips) => {
            return Ok(Err(divergent_refusal_line(context.branch, &tips)));
        }
    };
    match leftovers {
        Leftovers::Keep => {
            if std::fs::symlink_metadata(&context.destination).is_ok() {
                refuse_occupied(context)?;
            }
            clear_leftovers(context, Leftovers::Keep)?;
        }
        Leftovers::Clear => {
            check_vanished_workspace(context)?;
        }
    }
    Ok(Ok(Prepared::Create { revision, label }))
}

/// The adopted directory, or the new workspace, of a take whose claim the
/// ledger holds, and the line that says which, as [`take_claim`] says it.
fn take_workspace(context: &StartContext<'_>, prepared: &Prepared) -> anyhow::Result<String> {
    let shown = inline_human_text(&context.destination.display().to_string());
    Ok(match prepared {
        Prepared::Adopt => format!(
            "adopted existing workspace {shown} at {}; left as-is",
            adopted_change(context)?
        ),
        Prepared::Create { revision, label } => {
            add_workspace(
                &context.fork.checkout.path,
                context.workspace.as_str(),
                &context.destination,
                revision,
            )?;
            format!(
                "workspace {shown} at {} based on {revision} ({label})",
                workspace_change(
                    &Repo::open(&context.fork.checkout.path)?,
                    &context.workspace
                )?
            )
        }
    })
}

/// The directory there, or the workspace made in place of one that
/// vanished, of a seize whose claim the ledger holds, and the line that
/// says which, as [`workspace_notice`] says it.
fn seize_workspace(context: &StartContext<'_>, prepared: &Prepared) -> anyhow::Result<String> {
    let shown = inline_human_text(&context.destination.display().to_string());
    Ok(match prepared {
        Prepared::Adopt => format!(
            "workspace {shown} at {}; left as-is",
            workspace_change(&context.opened, &context.workspace)?
        ),
        Prepared::Create { revision, label } => {
            clear_vanished_workspace(context)?;
            add_workspace(
                &context.fork.checkout.path,
                context.workspace.as_str(),
                &context.destination,
                revision,
            )?;
            format!(
                "created missing workspace {shown} at {} based on {revision} ({label})",
                workspace_change(
                    &Repo::open(&context.fork.checkout.path)?,
                    &context.workspace
                )?
            )
        }
    })
}

/// An event about the branch, `text`, with the pull request stated for it.
fn event_draft<'a>(context: &StartContext<'a>, text: String) -> Draft<'a> {
    let target = BranchTarget::new(context.fork.upstream.clone(), context.branch.clone());
    Draft::event(
        Some(context.branch.as_str()),
        text,
        context.store.tracked_pull(&target),
    )
}

/// `claim`'s last sighting, read here: its workspace's moves in the
/// operation log, and this machine's sightings of its owner and workspace.
fn sighting(context: &StartContext<'_>, claim: &Claim) -> anyhow::Result<seen::LastSeen> {
    let activity = context.opened.workspace_activity(
        &BTreeSet::from([context.workspace.clone()]),
        MAX_ACTIVITY_OPS,
    )?;
    Ok(seen::last_seen(claim, &activity, &seen::load()))
}

/// [`take_claim`] on a fork that shares its ledger: the workspace's
/// refusals, then the claim on the ledger, then the workspace, then the
/// claim's copy in the state file.
fn take_shared(context: &mut StartContext<'_>, on: &OnLedger<'_>) -> anyhow::Result<Exit> {
    let reason = context.why.unwrap_or("started work");
    let prepared = match prepare_workspace(context, Leftovers::Keep)? {
        Ok(prepared) => prepared,
        // Nothing was claimed: the agent fixes what is refused and starts again.
        Err(refusal) => {
            eprintln!("{refusal}");
            return Ok(Exit::Usage);
        }
    };
    let event = match prepared {
        Prepared::Adopt => format!("claimed: {reason} (adopted existing workspace)"),
        Prepared::Create { .. } => format!("claimed: {reason}"),
    };
    let draft =
        event_draft(context, event).about_claim(Some(ClaimChange::Claimed), Some(reason), None);
    let (commit, copy) = match land(context, on, new_record(context, on, reason), &draft)? {
        Ok(landed) => landed,
        Err(exit) => return Ok(exit),
    };
    let opened = take_workspace(context, &prepared).and_then(|line| {
        context.store.copy_ledger_claim(copy);
        context.store.save()?;
        Ok(line)
    });
    match opened {
        Ok(line) => {
            println!(
                "{line}\nclaimed {}/{} for {}",
                context.fork.name,
                context.branch,
                inline_human_text(&context.identity.owner)
            );
            Ok(Exit::Ok)
        }
        Err(error) => Ok(release_unopened(context, on, &commit, &error)),
    }
}

/// [`force_claim`] on a fork that shares its ledger: `--force --why` over
/// every claim present, whoever holds it.
///
/// The new claim is the caller's, taken now, in this place and on this
/// machine, leased on the ledger's claim commit as this `start` read it, or
/// on no claim where only a local claim is present; this machine's local
/// claim, if any, is replaced by the copy. Its `taken` entry names the
/// holder it replaced, another owner's claim where one is present, with the
/// sighting only where that claim's activity can be read here.
fn seize_shared(
    context: &mut StartContext<'_>,
    on: &OnLedger<'_>,
    claims: &SharedClaims,
) -> anyhow::Result<Exit> {
    let reason = context
        .why
        .ok_or_else(|| anyhow::anyhow!("--force requires --why"))?;
    let prepared = match prepare_workspace(context, Leftovers::Clear)? {
        Ok(prepared) => prepared,
        // Refused before seizing, as a take refuses before claiming.
        Err(refusal) => {
            eprintln!("{refusal}");
            return Ok(Exit::Usage);
        }
    };
    let present = claims.present();
    let previous = present
        .iter()
        .copied()
        .find(|claim| claim.owner != context.identity.owner)
        .or_else(|| present.first().copied());
    let last_seen = previous
        .filter(|claim| claims.readable(claim, on.machine()))
        .map(|claim| sighting(context, claim))
        .transpose()?;
    let displaced = previous.and_then(|claim| {
        Displaced::over(
            claim,
            &context.identity,
            last_seen.unwrap_or(seen::LastSeen::NoneWithinWindow),
        )
    });
    let change = if displaced.is_some() {
        ClaimChange::Taken
    } else {
        ClaimChange::Claimed
    };
    let mut draft = event_draft(context, seized_event(previous, last_seen, on, reason))
        .about_claim(Some(change), Some(reason), displaced.as_ref());
    if last_seen.is_none() {
        draft.previous_last_seen = None;
    }
    let (commit, copy) = match land(context, on, new_record(context, on, reason), &draft)? {
        Ok(landed) => landed,
        Err(exit) => return Ok(exit),
    };
    let opened = seize_workspace(context, &prepared).and_then(|line| {
        context.store.copy_ledger_claim(copy);
        context.store.save()?;
        Ok(line)
    });
    match opened {
        Ok(line) => {
            let seized = previous.map_or_else(
                || {
                    format!(
                        "the claim {} at {}, whose holder could not be read",
                        on.claim,
                        on.read.as_ref().map_or("", CommitId::as_str)
                    )
                },
                |claim| {
                    render_shared_claim_context(
                        claim,
                        claimed_on(claim, on.machine()),
                        last_seen,
                        jiff::Timestamp::now(),
                    )
                },
            );
            println!("seized {seized}\n{line}");
            Ok(Exit::Ok)
        }
        Err(error) => Ok(release_unopened(context, on, &commit, &error)),
    }
}

/// The text of a seize's entry: whose claim it replaced, when that was
/// taken, and its last sighting where it can be read here, else the
/// machine it was taken on; a claim whose holder could not be read is
/// named by its ref and commit.
fn seized_event(
    previous: Option<&Claim>,
    last_seen: Option<seen::LastSeen>,
    on: &OnLedger<'_>,
    reason: &str,
) -> String {
    let Some(claim) = previous else {
        return format!(
            "seized the claim {} at {}, whose holder could not be read: {reason}",
            on.claim,
            on.read.as_ref().map_or("", CommitId::as_str)
        );
    };
    let seen = sighting_provenance(last_seen, claimed_on(claim, on.machine()));
    format!(
        "seized from {} ({}, claimed {}, {seen}): {reason}",
        claim.owner,
        owner_kind_label(claim.kind),
        claim.started,
    )
}

/// [`resume_claim`] on a fork that shares its ledger.
///
/// A claim present only as a local claim stays this machine's own, and
/// resumes as on a fork that shares no ledger. The ledger's claim resumes
/// with a `resumed` entry, which the send before exit carries; its holder
/// saying why again restates it on the ledger instead: a new claim commit
/// with the new reason, taken when and where and on the machine it was, in
/// one push with that entry. Either way the state file's copy becomes the
/// ledger's claim, written where this config home holds none of it or
/// another (a new config home under this machine's name, or a claim an
/// older knives took).
fn resume_shared(
    context: &mut StartContext<'_>,
    on: &OnLedger<'_>,
    claims: &SharedClaims,
    possession: bool,
) -> anyhow::Result<Exit> {
    let Some(held) = claims.on_ledger() else {
        let local = claims.local();
        let claim_seen = local.map(|claim| sighting(context, claim)).transpose()?;
        return resume_claim(context, local, claim_seen, possession);
    };
    let last_seen = claims
        .readable(held, on.machine())
        .then(|| sighting(context, held))
        .transpose()?;
    let workspace_notice = resume_workspace_notice(context);
    let event = if possession {
        "resumed via workspace possession"
    } else {
        "resumed"
    };
    // As on a fork that shares no ledger, only the holder's own name speaks
    // for the reason its claim carries.
    let copy = if let Some(why) = context.why.filter(|_| held.owner == context.identity.owner) {
        let draft = event_draft(context, event.to_owned()).about_claim(
            Some(ClaimChange::Resumed),
            Some(why),
            None,
        );
        let record = ClaimRecord {
            why: why.to_owned(),
            ..record_of(held, on.machine())
        };
        match land(context, on, record, &draft)? {
            Ok((_, copy)) => copy,
            Err(exit) => return Ok(exit),
        }
    } else {
        scribe_for(context).record(&event_draft(context, event.to_owned()).about_claim(
            Some(ClaimChange::Resumed),
            Some(held.why.as_str()),
            None,
        ))?;
        held.clone()
    };
    let line = render_shared_claim_context(
        &copy,
        claimed_on(&copy, on.machine()),
        last_seen,
        jiff::Timestamp::now(),
    );
    if claims.recorded.as_ref() != Some(&copy) {
        context.store.copy_ledger_claim(copy);
    }
    context.store.save()?;
    println!("{event}\n{line}\n{workspace_notice}");
    Ok(Exit::Ok)
}

/// Push `record` as the branch's claim, leased on the claim commit this
/// `start` read, with the entry `draft` recording it, in one push.
///
/// The claim commit that landed and its copy for the state file; or the
/// exit after saying why nothing was claimed: a claim another machine took
/// or changed meanwhile refuses as any claim refuses (exit 2), and a ledger
/// that could not be written, or a push whose outcome is unknown, claims
/// nothing here (exit 3).
fn land(
    context: &StartContext<'_>,
    on: &OnLedger<'_>,
    record: ClaimRecord,
    draft: &Draft<'_>,
) -> anyhow::Result<Result<(CommitId, Claim), Exit>> {
    let branch = context.branch;
    let change = on.change(on.read.as_ref(), Some(record.clone()));
    Ok(
        match change_on_ledger(change, &scribe_for(context), draft) {
            ClaimOutcome::Landed {
                claim: Some(commit),
                ..
            } => {
                let target = BranchTarget::new(context.fork.upstream.clone(), branch.clone());
                let copy = copy_of(&target, &commit, record);
                Ok((commit, copy))
            }
            ClaimOutcome::Landed { claim: None, .. } => {
                anyhow::bail!(
                    "the ledger took the claim of {branch} and holds no claim commit for it"
                )
            }
            ClaimOutcome::NotApplied { why, failure } => Err(lost(context, on, &why, &failure)
                .unwrap_or_else(|cause| {
                    eprintln!(
                        "nothing was claimed: {}",
                        unwritten(on.shared, &failure, &cause)
                    );
                    Exit::Incomplete
                })),
            ClaimOutcome::Unknown { failure, .. } => {
                eprintln!(
                    "the claim of {branch} was pushed to the ledger {} ({}), and whether it landed \
                 is unknown: {}; nothing is recorded on this machine, and `knives start \
                 {branch}` reads the claim again",
                    failure.ledger,
                    git_dir_of(on.shared),
                    problem_human_text(&failure.cause)
                );
                Err(Exit::Incomplete)
            }
        },
    )
}

/// What a claim push that applied nothing comes to.
///
/// Where the ledger's claim ref now holds another commit than this `start`
/// read, another machine took or changed the claim meanwhile
/// ([`taken_meanwhile`]); where it holds none though one was read, the claim
/// was released meanwhile: the exit after saying so. Otherwise why the push
/// applied nothing, for the caller to say that nothing was claimed.
fn lost(
    context: &StartContext<'_>,
    on: &OnLedger<'_>,
    why: &NotApplied,
    failure: &LedgerFailure,
) -> Result<Exit, String> {
    let NotApplied::Refused { claim, .. } = why else {
        return Err(unapplied_cause(on.shared, why, failure));
    };
    match claim {
        ClaimSeen::At(now) if Some(now) != on.read.as_ref() => Ok(taken_meanwhile(context, on)),
        ClaimSeen::Absent if on.read.is_some() => {
            eprintln!("{}", released_meanwhile(context.branch, on));
            Ok(Exit::Incomplete)
        }
        _ => Err(unapplied_cause(on.shared, why, failure)),
    }
}

/// A lost lease: another machine took or changed the claim between this
/// `start`'s read and its push. The claim is read again, and refuses as any
/// claim there refuses, after a line that says who took it.
fn taken_meanwhile(context: &StartContext<'_>, on: &OnLedger<'_>) -> Exit {
    let branch = context.branch;
    let target = BranchTarget::new(context.fork.upstream.clone(), branch.clone());
    match ledger_sweep::read_claim(on.shared, &on.claim) {
        Err(failure) => {
            eprintln!("{}", unread(branch, on.shared, &failure));
            Exit::Incomplete
        }
        Ok(None) => {
            eprintln!("{}", released_meanwhile(branch, on));
            Exit::Incomplete
        }
        Ok(Some(HeldClaim {
            commit,
            record: Err(cause),
        })) => {
            eprintln!(
                "the claim of {branch} changed while this start ran\n{}",
                unreadable_record_refusal(&target, &on.claim, &commit, &cause)
            );
            Exit::Usage
        }
        Ok(Some(HeldClaim {
            commit,
            record: Ok(record),
        })) => {
            let claim = copy_of(&target, &commit, record);
            let machine = claimed_on(&claim, on.machine());
            // Taken after this config home read the claim, so it recorded
            // nothing of it: its activity cannot be read here.
            let refusal = shared_refusal(
                Refused {
                    claim: &claim,
                    machine,
                    here: on.machine(),
                    activity: None,
                    anonymous: claim.kind == OwnerKind::OsUser
                        && context.identity.kind == OwnerKind::OsUser,
                },
                jiff::Timestamp::now(),
            );
            eprintln!(
                "{} took {branch} on machine {} while this start ran\n{refusal}",
                inline_human_text(&claim.owner),
                inline_human_text(machine)
            );
            Exit::Usage
        }
    }
}

/// The workspace could not be opened, or the claim's copy not saved, after
/// the ledger took the claim at `commit`: release it, leased on that
/// commit, so that no claim stays on the ledger with no workspace. Where
/// that release cannot be sent either, the claim is held with no workspace,
/// and `knives finish` is what releases it. Exit 3 either way: the start
/// did not open the workspace.
fn release_unopened(
    context: &StartContext<'_>,
    on: &OnLedger<'_>,
    commit: &CommitId,
    error: &anyhow::Error,
) -> Exit {
    let branch = context.branch;
    let cause = format!("{error:#}");
    let why = format!("knives start could not open the workspace: {cause}");
    let draft = event_draft(context, "claim released".to_owned()).about_claim(
        Some(ClaimChange::Released),
        Some(&why),
        None,
    );
    let outcome = change_on_ledger(on.change(Some(commit), None), &scribe_for(context), &draft);
    eprintln!(
        "knives start could not open the workspace of {branch}: {}",
        problem_human_text(&cause)
    );
    match outcome {
        ClaimOutcome::Landed { .. } => eprintln!(
            "its claim is released on the ledger {}, so nothing is claimed",
            on.shared.ledger
        ),
        ClaimOutcome::NotApplied { failure, .. } | ClaimOutcome::Unknown { failure, .. } => {
            eprintln!(
                "the claim of {branch} is held on the ledger {} with no workspace, since its \
                 release could not be sent ({}): {}; `knives finish {branch}` releases it",
                failure.ledger,
                git_dir_of(on.shared),
                problem_human_text(&failure.cause)
            );
        }
    }
    Exit::Incomplete
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;
    use crate::config::RepoEntry;

    fn entry(workspaces: Option<&str>) -> RepoEntry {
        RepoEntry {
            workspaces: workspaces.map(PathBuf::from),
            ..RepoEntry::new("u", "o")
        }
    }

    #[test]
    fn a_branch_whose_workspace_is_the_checkout_or_above_it_collides() {
        // With `workspaces` free to point anywhere, a branch whose flattened name is
        // an intermediate component of the checkout path maps to an ancestor of the
        // checkout, and `default` maps to a directory that is not the checkout at
        // all — but is still the primary workspace's name.
        let checkout = Path::new("/home/u/forks/tool/default");
        let sibling_entry = entry(None);
        let configured_entry = entry(Some("/home/u/.worktrees/tool"));
        let above_entry = entry(Some("/home/u/forks"));
        let sibling = Fork::at("tool", &sibling_entry, checkout);
        let configured = Fork::at("tool", &configured_entry, checkout);
        let above = Fork::at("tool", &above_entry, checkout);
        for (fork, branch) in [
            (&sibling, "default"),
            (&configured, "default"),
            (&above, "tool"),
        ] {
            let line = collides_with_checkout(fork, &BranchName::new(branch)).expect("a collision");
            assert!(line.contains("registered checkout"), "was: {line}");
        }
        for (fork, branch) in [
            (&sibling, "tool-fix"),
            (&configured, "feat/alpha"),
            (&above, "tool-fix"),
        ] {
            assert!(
                collides_with_checkout(fork, &BranchName::new(branch)).is_none(),
                "{branch} was refused"
            );
        }
    }

    #[test]
    fn a_workspaces_directory_inside_the_checkout_is_refused_for_every_branch() {
        // The checkout path itself is the most likely slip. Containment is by
        // component: a sibling sharing the checkout's name as a string prefix is
        // outside it.
        let checkout = Path::new("/home/u/tool");
        for inside in ["/home/u/tool", "/home/u/tool/.worktrees"] {
            let inside_entry = entry(Some(inside));
            let fork = Fork::at("tool", &inside_entry, checkout);
            let line =
                collides_with_checkout(&fork, &BranchName::new("feat/alpha")).expect("a refusal");
            assert_eq!(
                line,
                format!(
                    "[repos.tool] workspaces {inside} is inside the checkout /home/u/tool; \
                     branch workspaces cannot live in the working copy they belong to"
                )
            );
        }
        let beside_entry = entry(Some("/home/u/tool-worktrees"));
        let beside = Fork::at("tool", &beside_entry, checkout);
        assert!(collides_with_checkout(&beside, &BranchName::new("feat/alpha")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_workspaces_directory_spelled_through_a_symlink_into_the_checkout_is_refused() {
        // `~/link` → the checkout: `workspaces = ~/link/.worktrees` names a
        // directory inside the working copy without sharing its spelling.
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("tool");
        std::fs::create_dir_all(checkout.join(".worktrees")).unwrap();
        let checkout = checkout.canonicalize().unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&checkout, &link).unwrap();
        let linked = link.join(".worktrees");
        let linked_entry = entry(linked.to_str());
        let fork = Fork::at("tool", &linked_entry, &checkout);
        let refusal =
            collides_with_checkout(&fork, &BranchName::new("feat/alpha")).expect("a refusal");
        assert!(
            refusal.contains("is inside the checkout"),
            "the symlinked spelling resolves into the checkout: {refusal}"
        );
        // A symlink that lands beside the checkout is beside it.
        let beside = dir.path().join("worktrees");
        std::fs::create_dir_all(&beside).unwrap();
        let beside_link = dir.path().join("beside-link");
        std::os::unix::fs::symlink(&beside, &beside_link).unwrap();
        let beside_entry = entry(beside_link.to_str());
        let fork = Fork::at("tool", &beside_entry, &checkout);
        assert!(collides_with_checkout(&fork, &BranchName::new("feat/alpha")).is_none());
    }

    #[test]
    fn a_workspace_is_a_sibling_named_for_the_branch() {
        let sibling = entry(None);
        let path = new_workspace_path(
            &Fork::at("work", &sibling, Path::new("/home/u/forks/work/default")),
            &BranchName::new("feat/alpha"),
        );
        assert_eq!(path, PathBuf::from("/home/u/forks/work/feat-alpha"));
    }

    #[test]
    fn a_configured_workspaces_directory_holds_the_workspace_instead() {
        // A checkout at `~/<name>` has no `default` leaf to sit beside; without
        // this, every branch of every such repository would land in `~`.
        let configured = entry(Some("/home/u/.worktrees/work"));
        let path = new_workspace_path(
            &Fork::at("work", &configured, Path::new("/home/u/work")),
            &BranchName::new("feat/alpha"),
        );
        assert_eq!(path, PathBuf::from("/home/u/.worktrees/work/feat-alpha"));
    }

    #[test]
    fn slashes_in_a_branch_name_do_not_create_nested_directories() {
        // `feat/a/b` must not become three directories deep, or the workspace
        // lands somewhere nobody looks.
        let sibling = entry(None);
        let path = new_workspace_path(
            &Fork::at("x", &sibling, Path::new("/repos/x/default")),
            &BranchName::new("feat/a/b"),
        );
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("feat-a-b"));
    }

    /// The collision refusal both `start` and `finish` print names the
    /// registered checkout and the directory jj recorded for the workspace,
    /// neither of which this process wrote.
    #[test]
    fn a_control_character_in_a_checkout_path_reaches_the_terminal_escaped() {
        let checkout = PathBuf::from("/home/u/for\u{7}ks/tool/default");
        let sibling_entry = entry(None);
        let fork = Fork::at("tool", &sibling_entry, &checkout);
        let line = collides_with_checkout(&fork, &BranchName::new("default")).expect("a collision");
        assert!(!line.contains('\u{7}'), "{line}");
        assert_eq!(
            line.matches("/home/u/for\u{fffd}ks/tool/").count(),
            2,
            "the workspace directory and the checkout: {line}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_control_character_in_a_symlink_target_reaches_the_adoption_refusal_escaped() {
        // The target is whatever the link holds, which is nothing this process
        // wrote: a refusal that printed it raw would hand the directory's owner
        // the refused caller's terminal.
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("feat-alpha");
        std::os::unix::fs::symlink("/elsewhere/wor\u{1b}[31mk", &destination).unwrap();
        let line = adoption_refusal(
            &destination,
            Path::new("/home/u/forks/tool/default"),
            &WorkspaceName::new("feat-alpha"),
        )
        .expect("a refusal");
        assert!(!line.contains('\u{1b}'), "{line}");
        assert!(line.contains("/elsewhere/wor\u{fffd}[31mk"), "{line}");
    }

    /// Owner one's claim on the ledger, taken on machine one three hours
    /// before [`at_eleven`].
    fn ledger_claim() -> Claim {
        Claim {
            repo: "acme/demo".into(),
            branch: "feat/alpha".into(),
            owner: "owner-one".into(),
            kind: OwnerKind::HarnessSession,
            why: "port the parser".into(),
            started: "2026-11-16T08:00:00Z".into(),
            files: Vec::new(),
            place: Some("box-one 4026531836".into()),
            machine: Some("machine-one".into()),
            ledger_commit: Some("0123456789abcdef0123456789abcdef01234567".into()),
        }
    }

    fn at_eleven() -> jiff::Timestamp {
        "2026-11-16T11:00:00Z".parse().unwrap()
    }

    /// A line a refusal of a claim whose activity cannot be read here must
    /// never hold: each reads as evidence that nothing works on the branch.
    const IDLE_LINES: [&str; 5] = [
        "no activity observed",
        "no process of this machine",
        "its workspace is",
        "the claim was taken in this place",
        "the claim was taken in another place",
    ];

    #[test]
    fn a_claim_taken_on_another_machine_is_refused_with_no_word_of_its_activity_here() {
        // Given: owner one's claim, taken on machine one.
        let claim = ledger_claim();

        // When: a start on machine two is refused by it.
        let refusal = shared_refusal(
            Refused {
                claim: &claim,
                machine: "machine-one",
                here: "machine-two",
                activity: None,
                anonymous: false,
            },
            at_eleven(),
        );

        // Then: it names the holder, the machine and when the claim was
        // taken, says why its activity cannot be read, and nothing else.
        assert_eq!(
            refusal,
            "acme/demo/feat/alpha is claimed by owner-one (harness-session) on machine \
             machine-one, taken 2026-11-16T08:00:00Z (3h ago): port the parser\n\
             its activity cannot be read from this machine (machine-two): its sightings and its \
             processes are on machine machine-one\n\
             use `knives start feat/alpha --force --why \"…\"` to seize the claim"
        );
        for idle in IDLE_LINES {
            assert!(!refusal.contains(idle), "{refusal}");
        }
    }

    #[test]
    fn a_claim_this_config_home_did_not_record_is_refused_naming_where_it_was_taken() {
        // Given: owner one's claim, taken on this machine's name by another
        // config home.
        let claim = ledger_claim();

        // When: a start here is refused by it, as an anonymous caller.
        let refusal = shared_refusal(
            Refused {
                claim: &claim,
                machine: "machine-one",
                here: "machine-one",
                activity: None,
                anonymous: true,
            },
            at_eleven(),
        );

        // Then: the second line names the place, and the anonymous note
        // still comes first.
        let lines: Vec<&str> = refusal.lines().collect();
        assert!(
            lines[0].starts_with("both sides are anonymous identities, so they can never match; "),
            "{refusal}"
        );
        assert_eq!(
            lines[1],
            "its activity cannot be read here: this config home did not record that claim, \
             which was taken in box-one 4026531836"
        );
        for idle in IDLE_LINES {
            assert!(!refusal.contains(idle), "{refusal}");
        }
    }

    #[test]
    fn a_claim_whose_activity_is_readable_here_is_refused_with_its_sighting_and_evidence() {
        let claim = ledger_claim();

        let refusal = shared_refusal(
            Refused {
                claim: &claim,
                machine: "machine-one",
                here: "machine-one",
                activity: Some((
                    seen::LastSeen::NoneSinceClaim,
                    "its workspace is /w/feat-alpha".to_owned(),
                )),
                anonymous: false,
            },
            at_eleven(),
        );

        assert_eq!(
            refusal,
            "acme/demo/feat/alpha is claimed by owner-one (harness-session) on machine \
             machine-one, claimed 3h ago, no activity observed since claimed: port the parser\n\
             its workspace is /w/feat-alpha\n\
             use `knives start feat/alpha --force --why \"…\"` to seize the claim"
        );
    }

    #[test]
    fn a_claim_whose_record_does_not_read_is_refused_naming_its_ref_and_commit() {
        let target = BranchTarget::new(
            crate::ids::UpstreamName::new("acme/demo"),
            BranchName::new("feat/alpha"),
        );
        let claim = ClaimRef::new(&target.repo, &target.branch).unwrap();

        let refusal = unreadable_record_refusal(
            &target,
            &claim,
            &CommitId::new("0123456789abcdef0123456789abcdef01234567"),
            "its claim.toml does not read as a claim: \u{1b}[2J",
        );

        assert_eq!(
            refusal,
            "acme/demo/feat/alpha is claimed on the ledger by \
             refs/knives-claims/acme/demo/feat%2Falpha at \
             0123456789abcdef0123456789abcdef01234567, but the holder cannot be read: its \
             claim.toml does not read as a claim: \u{fffd}[2J\n\
             use `knives start feat/alpha --force --why \"…\"` to replace the claim"
        );
    }
}
