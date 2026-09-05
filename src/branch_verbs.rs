//! The branch verbs after `start`: `finish`, `track` and `depends`.
//!
//! `finish` hands a claimed branch back and removes its workspace; `track`
//! states which pull request a branch belongs to, overriding inference;
//! `depends` records that a branch cannot land before something else does.
//! Each is one state write with its ledger event.

use std::path::Path;

use knives::bind::Fork;
use knives::cli::Exit;
use knives::commands::claim::{
    ClaimContext, ClaimDecision, current_identity, decide, last_seen_provenance,
    render_claim_context,
};
use knives::commands::start::{collides_with_checkout, possesses, workspace_path};
use knives::config::Registry;
use knives::ids::{BranchName, BranchTarget, RepoName, Requirement};
use knives::jj::{Repo, WorkspaceIdentity};
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
    Continue(Option<String>),
    Refuse,
}

/// Hand a branch back and remove its workspace. The inverse of `start`.
///
/// Removing the directory loses no work: jj snapshots a working copy into a commit, so
/// every change made there is already in the repository and reachable by change id. What
/// does not survive is anything jj never tracked, which is what `--no-cleanup` is for.
/// `bound` is the entry the current directory is inside, which names a terminal
/// user acting from a fork's workspace.
pub(crate) fn run_finish(
    fork: &Fork<'_>,
    branch: &BranchName,
    options: &FinishOptions<'_>,
    bound: Option<&RepoName>,
) -> anyhow::Result<Exit> {
    let target = &BranchTarget::new(fork.name.clone(), branch.clone());
    let checkout_path = &fork.checkout.path;
    // Without this, `finish` would forget the primary workspace and delete the
    // checkout itself.
    if let Some(line) = collides_with_checkout(fork, branch) {
        eprintln!("{}: {line}", fork.name);
        return Ok(Exit::Usage);
    }
    let mut store = Store::open_for_update(default_state_path())?;
    let workspace = knives::commands::wip::workspace_for(branch.as_str());
    let directory = workspace_path(fork, branch);
    let forced_release = match finish_claim_gate(fork, target, &store, options, bound)? {
        FinishClaimGate::Continue(event) => event,
        FinishClaimGate::Refuse => return Ok(Exit::Usage),
    };
    let had = store.release_claim(target);
    if let Some(new) = options.superseded_by {
        store.supersede(target, new);
    }
    let pr = store.tracked_pull(target);
    // Persist the immutable explanation before the mutable claim state. A failed
    // append then leaves the old claim in place instead of silently releasing or
    // seizing work without the provenance that explains why.
    let provenance = forced_release
        .map(|text| match options.superseded_by {
            Some(replacement) => format!("{text}; superseded by {replacement}"),
            None => text,
        })
        .or_else(|| release_event(had, options.superseded_by));
    if let Some(text) = provenance {
        scribe_for(fork, bound)?.event(Some(branch.as_str()), text, pr)?;
    }
    store.save()?;
    // The claim is recorded; nothing below touches the store, and removing a
    // workspace with a build tree in it can take seconds another writer would
    // otherwise spend waiting for the lock.
    drop(store);

    let claim = if had { "released" } else { "was not held" };
    // Read what is at the path before writing anything to the repository: a jj
    // that refused to load a forgotten workspace's state would otherwise leave
    // the directory forgotten and unremovable.
    let identity = directory
        .exists()
        .then(|| knives::jj::workspace_identity(&directory, checkout_path));
    let registration = release_registration(checkout_path, &workspace);
    let shown = directory.display();
    let checkout = checkout_path.display();
    let removal = match identity {
        None => format!("no directory at {shown}"),
        Some(WorkspaceIdentity::Ours(name)) if name.as_str() == workspace => {
            // A registration that could not be forgotten keeps its directory: with
            // the directory gone, every later `start` dies on jj's "already exists",
            // `--force` included, where a retried `finish` would have cleaned up.
            let removable = options.cleanup && !matches!(registration, Registration::Failed(_));
            if removable {
                // Safe because jj already snapshotted the working copy into a commit:
                // the work is in the repository and reachable by change id. Untracked
                // files are the exception, which is what --no-cleanup is for.
                std::fs::remove_dir_all(&directory)?;
                format!("{shown} removed; its commits remain in the repository")
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
    };
    println!("{target}: claim {claim}; workspace {workspace} {registration}; {removal}");
    Ok(Exit::Ok)
}

/// What became of the branch's workspace registration.
enum Registration {
    Forgotten,
    /// jj never knew the name; `jj workspace forget` would have exited 0 anyway,
    /// and saying "forgotten" would let a reader believe a registration was cleared.
    NotRegistered,
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
            Self::Failed(detail) => write!(f, "not forgotten ({detail})"),
        }
    }
}

fn release_registration(checkout: &Path, workspace: &str) -> Registration {
    match Repo::open(checkout).and_then(|repo| repo.workspaces()) {
        Err(error) => Registration::Failed(error.to_string()),
        Ok(names) if !names.iter().any(|(name, _)| name.as_str() == workspace) => {
            Registration::NotRegistered
        }
        Ok(_) => match knives::jj::forget_workspace(checkout, workspace) {
            Ok(()) => Registration::Forgotten,
            Err(error) => Registration::Failed(error.to_string()),
        },
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
    bound: Option<&RepoName>,
) -> anyhow::Result<FinishClaimGate> {
    let Some(claim) = store
        .claims(Some(&target.repo))
        .into_iter()
        .find(|claim| claim.branch == target.branch.as_str())
        .cloned()
    else {
        return Ok(FinishClaimGate::Continue(None));
    };
    let workspace = knives::commands::wip::workspace_for(target.branch.as_str());
    let cwd = std::env::current_dir()?;
    let identity = current_identity(bound)?;
    let decision = decide(&ClaimContext {
        held: Some(&claim),
        identity: &identity,
        in_claimed_workspace: possesses(&cwd, fork, &target.branch),
    });
    match decision {
        ClaimDecision::Resume { .. } | ClaimDecision::Take => Ok(FinishClaimGate::Continue(None)),
        ClaimDecision::RefuseAnonymous | ClaimDecision::RefuseHeld => {
            let activity = Repo::open(&fork.checkout.path)?.workspace_activity(
                &std::collections::BTreeSet::from([knives::ids::WorkspaceName::new(workspace)]),
                knives::jj::MAX_ACTIVITY_OPS,
            )?;
            let last_seen = knives::seen::last_seen(&claim, &activity, &knives::seen::load());
            if !options.force {
                let anonymous_note = if decision == ClaimDecision::RefuseAnonymous {
                    "both sides are anonymous identities, so they can never match; "
                } else {
                    ""
                };
                eprintln!(
                    "{anonymous_note}{}\nuse `knives finish {} --force --why \"…\"` to release the claim",
                    render_claim_context(&claim, last_seen, jiff::Timestamp::now()),
                    target.branch,
                );
                return Ok(FinishClaimGate::Refuse);
            }
            let why = options
                .why
                .ok_or_else(|| anyhow::anyhow!("--force requires --why"))?;
            Ok(FinishClaimGate::Continue(Some(forced_release_event(
                &claim, last_seen, why,
            ))))
        }
    }
}

/// State or forget which pull request a branch belongs to.
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
    bound: Option<&RepoName>,
) -> anyhow::Result<Exit> {
    let target = &BranchTarget::new(fork.name.clone(), branch.clone());
    let mut store = Store::open_for_update(default_state_path())?;
    // Read before the change, so a withdrawal is still filed under the number it
    // withdrew.
    let stated = store.tracked_pull(target);
    // Each branch stamps the number its entry is ABOUT, not whatever happened to
    // be stated a moment earlier. The event that creates an association is the
    // one `knives notch --pr <n>` most needs to find, and stamping the prior
    // value there — usually nothing — would hide it from the only filter the
    // field exists for.
    let (text, stamped) = if fork_only {
        store.mark_fork_only(target, "stated with `knives track --fork-only`");
        (
            "stated as having no upstream pull request".to_owned(),
            stated,
        )
    } else if forget {
        let had = store.untrack_pull(target);
        (
            if had {
                "pull request statement forgotten".to_owned()
            } else {
                "no pull request statement to forget".to_owned()
            },
            stated,
        )
    } else {
        let Some(number) = pr else {
            eprintln!("give --pr <number>, or --forget");
            return Ok(Exit::Usage);
        };
        store.track_pull(target, number);
        (format!("stated as #{number}"), Some(number))
    };
    store.save()?;
    scribe_for(fork, bound)?.event(Some(branch.as_str()), text.clone(), stamped)?;
    println!("{target} {}", spoken(&text));
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
#[allow(
    clippy::too_many_arguments,
    reason = "the registry the requirements are checked against, the fork, the branch, the requirements and the cwd binding are independent inputs"
)]
pub(crate) fn run_depends(
    registry: &Registry,
    fork: &Fork<'_>,
    branch: &BranchName,
    on: &[String],
    bound: Option<&RepoName>,
) -> anyhow::Result<Exit> {
    let target = &BranchTarget::new(fork.name.clone(), branch.clone());
    let mut requirements = Vec::new();
    for text in on {
        let Some(requirement) = Requirement::parse(text) else {
            eprintln!("cannot read {text} as a requirement; write it as `<repo>#<number>`");
            return Ok(Exit::Usage);
        };
        if registry.get(&requirement.repo).is_none() {
            let known: Vec<String> = registry.names().map(|n| n.to_string()).collect();
            eprintln!(
                "unknown repo {} in {text}; known: {}",
                requirement.repo,
                known.join(", ")
            );
            return Ok(Exit::Usage);
        }
        requirements.push(requirement);
    }
    let mut store = Store::open_for_update(default_state_path())?;
    store.add_dependencies(target, &requirements);
    let pr = store.tracked_pull(target);
    store.save()?;
    let listed: Vec<String> = requirements.iter().map(ToString::to_string).collect();
    scribe_for(fork, bound)?.event(
        Some(branch.as_str()),
        format!("requires {}", listed.join(", ")),
        pr,
    )?;
    println!("{target} now requires {}", listed.join(", "));
    Ok(Exit::Ok)
}

#[cfg(test)]
mod tests {
    use super::spoken;

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
}
