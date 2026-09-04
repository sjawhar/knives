//! `knives start`: claim a branch and open a workspace on its tip, or on the
//! release's shared base for a new one.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::bind::Fork;
use crate::cli::Exit;
use crate::commands::claim::{
    ClaimContext, ClaimDecision, Identity, current_identity, decide, last_seen_provenance,
    render_claim_context,
};
use crate::commands::release::shared_base;
use crate::commands::wip::workspace_for;
use crate::config::Role;
use crate::detect::BookmarkTips;
use crate::ids::{
    BookmarkRef, BranchName, BranchTarget, CommitId, RemoteName, RepoName, WorkspaceName,
};
use crate::jj::{
    Repo, WorkspaceIdentity, add_workspace, fetch_all, is_workspace_named, repo_immutable_heads,
    set_repo_immutable_heads, user_immutable_heads, workspace_identity,
};
use crate::ledger::{Ledger, Scribe};
use crate::release_model::newest_release;
use crate::seen;
use crate::store::{Store, default_state_path};

/// Where a branch's workspace goes: under the entry's workspace root, named for
/// the branch with slashes flattened.
///
/// Workspaces are cheap to create, well under a second, because tracked content
/// is small even in a large checkout. The real cost is rebuilding language
/// environments, not the checkout.
pub fn workspace_path(fork: &Fork<'_>, branch: &BranchName) -> PathBuf {
    fork.workspace_root().join(workspace_for(branch.as_str()))
}

/// Whether the current directory is inside this branch's workspace.
///
/// Possession is the one claim check that needs no identity: standing in the
/// workspace is being the one working there. `cwd` is a physical path, so the
/// configured directory is canonicalised before the comparison — a `workspaces`
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
/// A `workspaces` directory inside the checkout is refused first: it puts every
/// branch workspace in the working copy it belongs to, which is never what
/// anyone meant. The registry cannot check it — the checkout is found, not
/// stated — so the two verbs that would use the directory do. The directory is
/// canonicalised when it exists, as the checkout already is, so a spelling
/// through a symlink is judged by where it lands; one that does not exist yet
/// has only its spelling.
pub fn collides_with_checkout(fork: &Fork<'_>, branch: &BranchName) -> Option<String> {
    let checkout = &fork.checkout.path;
    let spelled = fork.workspace_root();
    let root = spelled
        .canonicalize()
        .unwrap_or_else(|_| spelled.to_owned());
    if root.starts_with(checkout) {
        return Some(format!(
            "[repos.{}] workspaces {} is inside the checkout {}; branch workspaces cannot \
             live in the working copy they belong to",
            fork.name,
            root.display(),
            checkout.display()
        ));
    }
    let workspace = workspace_for(branch.as_str());
    let directory = root.join(&workspace);
    (workspace == "default" || checkout.starts_with(&directory)).then(|| {
        format!(
            "branch {branch} maps to workspace {workspace} at {}, which is the registered \
             checkout itself or contains it; refusing to touch {}",
            directory.display(),
            checkout.display()
        )
    })
}

struct StartContext<'a> {
    store: &'a mut Store,
    fork: &'a Fork<'a>,
    branch: &'a BranchName,
    identity: Identity,
    upstream_trunk: String,
    destination: PathBuf,
    workspace: WorkspaceName,
    opened: Repo,
}

/// Claim `branch` and open its workspace. `bound` is the entry the current
/// directory is inside, from which a terminal user's identity is derived.
#[allow(
    clippy::too_many_arguments,
    reason = "the fork, the branch, the reason, the override and the cwd binding are independent inputs"
)]
pub fn run(
    fork: &Fork<'_>,
    branch: &BranchName,
    why: Option<&str>,
    force: bool,
    bound: Option<&RepoName>,
) -> anyhow::Result<Exit> {
    let repo_name = &fork.name;
    let entry = fork.entry;
    let checkout = &fork.checkout.path;
    if let Some(line) = collides_with_checkout(fork, branch) {
        eprintln!("{repo_name}: {line}");
        return Ok(Exit::Usage);
    }
    let mut store = Store::open_for_update(default_state_path())?;
    let cwd = std::env::current_dir()?;
    let destination = workspace_path(fork, branch);
    let in_claimed_workspace = possesses(&cwd, fork, branch);
    let mut context = StartContext {
        store: &mut store,
        fork,
        branch,
        identity: current_identity(bound)?,
        upstream_trunk: entry.upstream_trunk(),
        workspace: WorkspaceName::new(workspace_for(branch.as_str())),
        opened: Repo::open(checkout)?,
        destination,
    };
    // A rule a human stated is their decision: `status` reports one that
    // differs, and nothing overwrites it. knives' own earlier write (marked by
    // its `doc`) is refreshed when the entry's rule has moved on. A user-level
    // rule is shadowed by the write — jj resolves the repo layer above it — and
    // the line says so. Done on every `start`, refused or not: the rule belongs
    // to the repository, and the agent about to run jj here is the one jj's
    // default would have walled.
    let rule = entry.immutable_heads();
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
        let shadow = shadowed.map_or_else(String::new, |user_rule| {
            format!(" (shadows the user-level rule {user_rule} here)")
        });
        println!("jj immutable_heads() {verb} {repo_name}'s repository config: {rule}{shadow}");
    }
    let held = context
        .store
        .claims(Some(repo_name))
        .into_iter()
        .find(|claim| claim.branch == branch.as_str())
        .cloned();
    let activity = context
        .opened
        .workspace_activity(&BTreeSet::from([context.workspace.clone()]), 200)?;
    let observations = seen::load();
    let claim_seen = held
        .as_ref()
        .map(|claim| seen::last_seen(claim, &activity, &observations));
    let decision = decide(&ClaimContext {
        held: held.as_ref(),
        identity: &context.identity,
        in_claimed_workspace,
    });

    if force && held.is_some() {
        let (previous, last_seen) = claim_context(held.as_ref(), claim_seen)?;
        let reason = why.ok_or_else(|| anyhow::anyhow!("--force requires --why"))?;
        return force_claim(&mut context, previous, last_seen, reason);
    }

    match decision {
        ClaimDecision::RefuseAnonymous => {
            let (claim, last_seen) = claim_context(held.as_ref(), claim_seen)?;
            Ok(refuse_claim(branch, claim, last_seen, true))
        }
        ClaimDecision::RefuseHeld => {
            let (claim, last_seen) = claim_context(held.as_ref(), claim_seen)?;
            Ok(refuse_claim(branch, claim, last_seen, false))
        }
        ClaimDecision::Resume { possession } => {
            resume_claim(&context, held.as_ref(), claim_seen, possession)
        }
        ClaimDecision::Take => take_claim(&mut context, why.unwrap_or("started work")),
    }
}

fn claim_context(
    held: Option<&crate::store::Claim>,
    claim_seen: Option<crate::seen::LastSeen>,
) -> anyhow::Result<(&crate::store::Claim, crate::seen::LastSeen)> {
    held.zip(claim_seen)
        .ok_or_else(|| anyhow::anyhow!("claim decision lacked held claim context"))
}

fn refuse_claim(
    branch: &BranchName,
    claim: &crate::store::Claim,
    last_seen: crate::seen::LastSeen,
    anonymous: bool,
) -> Exit {
    let anonymous_note = if anonymous {
        "both sides are anonymous identities, so they can never match; "
    } else {
        ""
    };
    eprintln!(
        "{anonymous_note}{}\nuse `knives start {branch} --force --why \"…\"` to seize the claim",
        render_claim_context(claim, last_seen, jiff::Timestamp::now()),
    );
    Exit::Usage
}

fn force_claim(
    context: &mut StartContext<'_>,
    previous: &crate::store::Claim,
    last_seen: crate::seen::LastSeen,
    reason: &str,
) -> anyhow::Result<Exit> {
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
    )?;
    println!(
        "seized {}\n{workspace_notice}",
        render_claim_context(previous, last_seen, jiff::Timestamp::now()),
    );
    Ok(Exit::Ok)
}

fn resume_claim(
    context: &StartContext<'_>,
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
    Scribe::new(
        Ledger::for_repo(&context.fork.name),
        context.fork.name.clone(),
        context.fork.checkout.path.clone(),
        context.identity.owner.clone(),
    )
    .event(
        Some(context.branch.as_str()),
        event.to_owned(),
        context.store.tracked_pull(&BranchTarget::new(
            context.fork.name.clone(),
            context.branch.clone(),
        )),
    )?;
    println!(
        "{event}\n{}\n{workspace_notice}",
        render_claim_context(claim, last_seen, jiff::Timestamp::now()),
    );
    Ok(Exit::Ok)
}

fn take_claim(context: &mut StartContext<'_>, reason: &str) -> anyhow::Result<Exit> {
    if context.destination.exists() {
        if let Some(line) = adoption_refusal(
            &context.destination,
            &context.fork.checkout.path,
            &context.workspace,
        ) {
            eprintln!("{line}");
            return Ok(Exit::Usage);
        }
        // The identity gate above already established this is our workspace of
        // this name; `reattach_workspace` re-checks on its own behalf as the
        // mutation's guard, and a mismatch it finds is a race, not a user error.
        let change = if let Ok(change) = workspace_change(&context.opened, &context.workspace) {
            change
        } else {
            context
                .opened
                .reattach_workspace(&context.destination, &context.workspace)?;
            workspace_change(
                &Repo::open(&context.fork.checkout.path)?,
                &context.workspace,
            )?
        };
        record_claim(
            context,
            reason,
            format!("claimed: {reason} (adopted existing workspace)"),
        )?;
        println!(
            "adopted existing workspace {} at {change}; left as-is\nclaimed {}/{} for {}",
            context.destination.display(),
            context.fork.name,
            context.branch,
            context.identity.owner,
        );
        return Ok(Exit::Ok);
    }

    let (base_revision, base_label) = match create_workspace(context)? {
        WorkspaceBase::Created { revision, label } => (revision, label),
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
    record_claim(context, reason, format!("claimed: {reason}"))?;
    println!(
        "workspace {} at {change} based on {base_revision} ({base_label})\nclaimed {}/{} for {}",
        context.destination.display(),
        context.fork.name,
        context.branch,
        context.identity.owner,
    );
    Ok(Exit::Ok)
}

fn record_claim(context: &mut StartContext<'_>, reason: &str, event: String) -> anyhow::Result<()> {
    let target = BranchTarget::new(context.fork.name.clone(), context.branch.clone());
    let pull = context.store.tracked_pull(&target);
    let _ = context.store.claim(&target, &context.identity, reason);
    Scribe::new(
        Ledger::for_repo(&context.fork.name),
        context.fork.name.clone(),
        context.fork.checkout.path.clone(),
        context.identity.owner.clone(),
    )
    .event(Some(context.branch.as_str()), event, pull)?;
    context.store.save()?;
    Ok(())
}

fn resume_workspace_notice(context: &StartContext<'_>) -> String {
    if !context.destination.exists() {
        return format!(
            "workspace missing at {}; `knives start {} --force --why \"…\"` rebuilds it",
            context.destination.display(),
            context.branch
        );
    }
    match workspace_change(&context.opened, &context.workspace) {
        Ok(change) => format!("workspace {} at {change}", context.destination.display()),
        Err(error) => format!(
            "workspace {} is present but not registered as {} ({error}); \
             `knives start {} --force --why \"…\"` rebuilds it",
            context.destination.display(),
            context.workspace,
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
fn adoption_refusal(destination: &Path, checkout: &Path, name: &WorkspaceName) -> Option<String> {
    let shown = destination.display();
    let line = match workspace_identity(destination, checkout) {
        WorkspaceIdentity::Ours(actual) if actual == *name => return None,
        WorkspaceIdentity::Ours(actual) => format!(
            "cannot adopt {shown}: it is workspace {actual} of {}, not {name}; forget or finish \
             workspace {actual}, or choose a different branch",
            checkout.display()
        ),
        WorkspaceIdentity::Foreign(store) => format!(
            "cannot adopt {shown}: it belongs to repository {}, not {}; move the foreign \
             workspace or choose a different branch",
            store.display(),
            checkout.display()
        ),
        WorkspaceIdentity::Unreadable(detail) => format!(
            "cannot adopt {shown}: it is a workspace of {} whose working-copy state could not \
             be read ({detail})",
            checkout.display()
        ),
        WorkspaceIdentity::Repository => format!(
            "cannot adopt {shown}: it is a repository in its own right, not a workspace of {}; \
             choose a different branch",
            checkout.display()
        ),
        WorkspaceIdentity::SymbolicLink(target) => format!(
            "cannot adopt {shown}: it is a symbolic link to {}; start from the real directory \
             or choose a different branch",
            target.display()
        ),
        WorkspaceIdentity::NotAWorkspace => format!(
            "cannot adopt {shown}: not a workspace of {}; move it or choose a different branch",
            checkout.display()
        ),
    };
    Some(line)
}

/// The workspace line for a claim being seized: the existing workspace, or the
/// one just created. `Err` is the refusal line; nothing is claimed in that case,
/// so a plain `start` afterwards is the way back.
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
            context.destination.display(),
            workspace_change(&context.opened, &context.workspace)?,
        )));
    }
    Ok(match create_workspace(context)? {
        WorkspaceBase::Created { revision, label } => Ok(format!(
            "created missing workspace {} at {} based on {revision} ({label})",
            context.destination.display(),
            workspace_change(
                &Repo::open(&context.fork.checkout.path)?,
                &context.workspace
            )?,
        )),
        WorkspaceBase::Divergent(tips) => Err(divergent_refusal_line(context.branch, &tips)),
    })
}

/// Where a new workspace's working copy was put, or why it was not.
enum WorkspaceBase {
    Created {
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

/// Create the branch's workspace with its working copy on the right commit.
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
fn create_workspace(context: &StartContext<'_>) -> anyhow::Result<WorkspaceBase> {
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
    add_workspace(
        checkout,
        context.workspace.as_str(),
        &context.destination,
        &revision,
    )?;
    Ok(WorkspaceBase::Created { revision, label })
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
        let path = workspace_path(
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
        let path = workspace_path(
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
        let path = workspace_path(
            &Fork::at("x", &sibling, Path::new("/repos/x/default")),
            &BranchName::new("feat/a/b"),
        );
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("feat-a-b"));
    }
}
