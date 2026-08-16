//! `knives start`: claim a branch and open a workspace on the shared base.

use std::path::{Path, PathBuf};

use crate::cli::Exit;
use crate::commands::claim::current_owner;
use crate::commands::release::{newest_release, shared_base};
use crate::config::{default_config_path, load};
use crate::ids::{BranchName, BranchTarget, RepoName};
use crate::jj::{Repo, add_workspace, fetch_all};
use crate::ledger::{Ledger, Scribe};
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

pub fn run(repo_name: &RepoName, branch: &BranchName, why: Option<&str>) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let Some(entry) = registry.get(repo_name) else {
        eprintln!("unknown repo {repo_name}");
        return Ok(Exit::Usage);
    };
    let upstream_trunk = entry.upstream_trunk();

    let mut store = Store::open_for_update(default_state_path())?;
    let owner = current_owner(&std::env::current_dir()?)?;
    if let Some(held) = store
        .claims(Some(repo_name))
        .into_iter()
        .find(|c| c.branch == branch.as_str())
        && held.owner != owner
    {
        eprintln!(
            "{repo_name}/{branch} is already claimed by {}: {}",
            held.owner, held.why
        );
        return Ok(Exit::Usage);
    }

    let destination = workspace_path(&entry.path, branch);
    if destination.exists() {
        eprintln!("{} already exists", destination.display());
        return Ok(Exit::Usage);
    }

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
            let trunk_tip = opened.resolve_commit(&upstream_trunk)?;
            shared_base(&opened, &release, &trunk_tip)?
        }
        None => None,
    };
    let (base_revision, base_label) = base.map_or_else(
        || (upstream_trunk, "the fetched upstream trunk"),
        |commit| (commit.as_str().to_owned(), "the release's shared base"),
    );
    add_workspace(
        &entry.path,
        &branch.as_str().replace('/', "-"),
        &destination,
        &base_revision,
    )?;

    let reason = why.unwrap_or("started work");
    let target = BranchTarget::new(repo_name.clone(), branch.clone());
    let _ = store.claim(&target, &owner, reason);
    let pr = store.tracked_pull(&target);
    store.save()?;
    Scribe::new(
        Ledger::for_repo(repo_name),
        repo_name.clone(),
        entry.path.clone(),
        owner.clone(),
    )
    .event(Some(branch.as_str()), format!("claimed: {reason}"), pr)?;

    println!(
        "workspace {} based on {base_revision} ({base_label})\nclaimed {repo_name}/{branch} for {owner}",
        destination.display()
    );
    Ok(Exit::Ok)
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
