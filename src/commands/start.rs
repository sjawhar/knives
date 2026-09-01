//! `knives start`: claim a branch and open a workspace on the shared base.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::cli::Exit;
use crate::commands::claim::{
    ClaimContext, ClaimDecision, current_identity, decide, last_seen_provenance,
    render_claim_context,
};
use crate::commands::release::shared_base;
use crate::commands::wip::workspace_for;
use crate::config::{RepoEntry, default_config_path, load};
use crate::ids::{BranchName, BranchTarget, RepoName, WorkspaceName};
use crate::jj::{Repo, add_workspace, fetch_all};
use crate::ledger::{Ledger, Scribe};
use crate::release_model::newest_release;
use crate::seen;
use crate::store::{Store, default_state_path};

/// Where a new workspace goes: a sibling of the repo, named for the branch.
///
/// Workspaces are cheap to create, well under a second, because tracked content
/// is small even in a large checkout. The real cost is rebuilding language
/// environments, not the checkout.
pub fn workspace_path(repo: &Path, branch: &BranchName) -> PathBuf {
    let safe = branch.as_str().replace('/', "-");
    repo.parent().unwrap_or(repo).join(safe)
}

pub fn run(
    repo_name: &RepoName,
    branch: &BranchName,
    why: Option<&str>,
    force: bool,
) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let Some(entry) = registry.get(repo_name) else {
        eprintln!("unknown repo {repo_name}");
        return Ok(Exit::Usage);
    };
    let upstream_trunk = entry.upstream_trunk();

    let mut store = Store::open_for_update(default_state_path())?;
    let cwd = std::env::current_dir()?;
    let identity = current_identity(&cwd)?;
    let destination = workspace_path(&entry.path, branch);
    let in_claimed_workspace = cwd.starts_with(&destination);
    let held = store
        .claims(Some(repo_name))
        .into_iter()
        .find(|claim| claim.branch == branch.as_str())
        .cloned();
    let workspace = WorkspaceName::new(workspace_for(branch.as_str()));
    let opened = Repo::open(&entry.path)?;
    let activity = opened.workspace_activity(&BTreeSet::from([workspace.clone()]), 200)?;
    let observations = seen::load();
    let claim_seen = held
        .as_ref()
        .map(|claim| seen::last_seen(claim, &activity, &observations));
    let decision = decide(&ClaimContext {
        held: held.as_ref(),
        identity: &identity,
        in_claimed_workspace,
    });
    let target = BranchTarget::new(repo_name.clone(), branch.clone());
    let pr = store.tracked_pull(&target);

    if force && let Some(previous) = held.as_ref() {
        let last_seen = claim_seen.expect("held claims have observation context");
        let workspace_notice = if destination.exists() {
            format!(
                "workspace {} at {}; left as-is",
                destination.display(),
                workspace_change(&opened, &workspace)?,
            )
        } else {
            let (base_revision, base_label) =
                create_workspace(entry, &upstream_trunk, &workspace, &destination)?;
            format!(
                "created missing workspace {} at {} based on {base_revision} ({base_label})",
                destination.display(),
                workspace_change(&Repo::open(&entry.path)?, &workspace)?,
            )
        };
        let reason = why.expect("clap requires --why with --force");
        let _ = store.claim(&target, &identity, reason);
        store.save()?;
        Scribe::new(
            Ledger::for_repo(repo_name),
            repo_name.clone(),
            entry.path.clone(),
            identity.owner.clone(),
        )
        .event(
            Some(branch.as_str()),
            format!(
                "seized from {} ({}, claimed {}, last seen {}): {reason}",
                previous.owner,
                crate::commands::claim::owner_kind_label(previous.kind),
                previous.started,
                last_seen_provenance(last_seen),
            ),
            pr,
        )?;
        println!(
            "seized {}\n{workspace_notice}",
            render_claim_context(previous, last_seen, jiff::Timestamp::now()),
        );
        return Ok(Exit::Ok);
    }

    match decision {
        ClaimDecision::RefuseAnonymous => {
            let claim = held.as_ref().expect("refusal has a held claim");
            eprintln!(
                "both sides anonymous; {}\nuse `knives start {branch} --force --why \"…\"` to seize the claim",
                render_claim_context(
                    claim,
                    claim_seen.expect("held claims have observation context"),
                    jiff::Timestamp::now(),
                ),
            );
            Ok(Exit::Usage)
        }
        ClaimDecision::RefuseHeld => {
            let claim = held.as_ref().expect("refusal has a held claim");
            eprintln!(
                "{}\nuse `knives start {branch} --force --why \"…\"` to seize the claim",
                render_claim_context(
                    claim,
                    claim_seen.expect("held claims have observation context"),
                    jiff::Timestamp::now(),
                ),
            );
            Ok(Exit::Usage)
        }
        ClaimDecision::Resume { possession } => {
            let claim = held.as_ref().expect("resume has a held claim");
            let last_seen = claim_seen.expect("held claims have observation context");
            let workspace_notice = if destination.exists() {
                format!(
                    "workspace {} at {}",
                    destination.display(),
                    workspace_change(&opened, &workspace)?,
                )
            } else {
                let (base_revision, base_label) =
                    create_workspace(entry, &upstream_trunk, &workspace, &destination)?;
                format!(
                    "created missing workspace {} at {} based on {base_revision} ({base_label})",
                    destination.display(),
                    workspace_change(&Repo::open(&entry.path)?, &workspace)?,
                )
            };
            let event = if possession {
                "resumed via workspace possession"
            } else {
                "resumed"
            };
            Scribe::new(
                Ledger::for_repo(repo_name),
                repo_name.clone(),
                entry.path.clone(),
                identity.owner.clone(),
            )
            .event(Some(branch.as_str()), event.to_owned(), pr)?;
            println!(
                "{event}\n{}\n{workspace_notice}",
                render_claim_context(claim, last_seen, jiff::Timestamp::now()),
            );
            Ok(Exit::Ok)
        }
        ClaimDecision::Take => {
            let reason = why.unwrap_or("started work");
            if destination.exists() {
                let change = workspace_change(&opened, &workspace)?;
                let _ = store.claim(&target, &identity, reason);
                store.save()?;
                Scribe::new(
                    Ledger::for_repo(repo_name),
                    repo_name.clone(),
                    entry.path.clone(),
                    identity.owner.clone(),
                )
                .event(
                    Some(branch.as_str()),
                    format!("claimed: {reason} (adopted existing workspace)"),
                    pr,
                )?;
                println!(
                    "adopted existing workspace {} at {change}; left as-is\nclaimed {repo_name}/{branch} for {}",
                    destination.display(),
                    identity.owner,
                );
                return Ok(Exit::Ok);
            }

            let (base_revision, base_label) =
                create_workspace(entry, &upstream_trunk, &workspace, &destination)?;
            let change = workspace_change(&Repo::open(&entry.path)?, &workspace)?;
            let _ = store.claim(&target, &identity, reason);
            store.save()?;
            Scribe::new(
                Ledger::for_repo(repo_name),
                repo_name.clone(),
                entry.path.clone(),
                identity.owner.clone(),
            )
            .event(Some(branch.as_str()), format!("claimed: {reason}"), pr)?;
            println!(
                "workspace {} at {change} based on {base_revision} ({base_label})\nclaimed {repo_name}/{branch} for {}",
                destination.display(),
                identity.owner,
            );
            Ok(Exit::Ok)
        }
    }
}

fn create_workspace(
    entry: &RepoEntry,
    upstream_trunk: &str,
    workspace: &WorkspaceName,
    destination: &Path,
) -> anyhow::Result<(String, String)> {
    fetch_all(&entry.path)?;
    // A release names the shared base every member branch forks from; without
    // one, the fetched upstream trunk starts the first branch. Never use the
    // current `@`: an agent in a release workspace could otherwise run `jj new`
    // and silently inherit the release merge as a parent.
    let opened = Repo::open(&entry.path)?;
    let tips = opened.bookmark_tips()?;
    let scheme = entry.release_scheme();
    let base = match newest_release(&tips, &scheme, entry.publish_remote()) {
        Some((_, release)) => {
            let trunk_tip = opened.resolve_commit(upstream_trunk)?;
            shared_base(&opened, &release, &trunk_tip)?
        }
        None => None,
    };
    let (base_revision, base_label) = base.map_or_else(
        || (upstream_trunk.to_owned(), "the fetched upstream trunk".to_owned()),
        |commit| {
            (
                commit.as_str().to_owned(),
                "the release's shared base".to_owned(),
            )
        },
    );
    add_workspace(&entry.path, workspace.as_str(), destination, &base_revision)?;
    Ok((base_revision, base_label))
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

    #[test]
    fn a_workspace_is_a_sibling_named_for_the_branch() {
        let path = workspace_path(
            Path::new("/home/u/forks/work/default"),
            &BranchName::new("feat/alpha"),
        );
        assert_eq!(path, PathBuf::from("/home/u/forks/work/feat-alpha"));
    }

    #[test]
    fn slashes_in_a_branch_name_do_not_create_nested_directories() {
        // `feat/a/b` must not become three directories deep, or the workspace
        // lands somewhere nobody looks.
        let path = workspace_path(Path::new("/repos/x/default"), &BranchName::new("feat/a/b"));
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("feat-a-b"));
    }
}
