//! The jj layer's answers, read from a real repository.
//!
//! Workspaces resolve to their repo and attribute their moves; a local branch is
//! ahead of, behind or diverged from origin; ancestry answers both ways; carriers
//! exclude release cuts, git-tracking refs and fetched pull heads; bookmark tips
//! keep local and remote distinct; divergence is reported once per rewrite and
//! only where a live head vouches for it; refs and changed files read without
//! snapshotting; a release write moves no other agent's working copy and is
//! refused with no parents.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::commands::status::{self, OriginRelation};
use knives::config::Registry;
use knives::ids::{BookmarkRef, BranchName, CommitId, ReleaseScheme, RemoteName, WorkspaceName};
use knives::jj::{Repo, changed_files, changed_files_between, pull_heads, remote_refs};
use lab::{Lab, lab_entry, operation_ids};
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

fn relation_to_origin(lab: &lab::Lab) -> Result<Option<OriginRelation>, knives::jj::JjError> {
    let repo = Repo::open(&lab.work).expect("open");
    let branch = BranchName::new("feat/alpha");
    let tip = repo.resolve_commit(branch.as_str()).expect("local tip");
    let origin_tip = repo
        .resolve_commit("feat/alpha@origin")
        .expect("origin tip");

    status::phases::relation_to_origin(&repo, &tip, Some(&origin_tip))
}

#[test]
fn a_jj_workspace_beside_a_registered_repo_resolves_that_repo() {
    let lab = Lab::new();
    let workspace = lab
        .work
        .parent()
        .expect("workspace parent")
        .join("feature-alpha");
    knives::jj::add_workspace(&lab.work, "feature-alpha", &workspace, "main@upstream")
        .expect("add workspace");
    let registry = Registry {
        repos: BTreeMap::from([("demo".to_owned(), lab_entry(&lab))]),
        ..Registry::default()
    };

    assert_eq!(
        registry.containing(&workspace).map(|(name, _)| name),
        Some(knives::ids::RepoName::new("demo"))
    );
    let unrelated = tempfile::tempdir().expect("unrelated directory");
    assert!(registry.containing(unrelated.path()).is_none());
}

#[test]
fn workspace_activity_attributes_working_copy_moves_to_their_workspace() {
    let lab = Lab::new();
    lab.jj_work(["workspace", "add", "--name", "feat-x", "../feat-x-ws"]);
    let workspace_dir = lab
        .work
        .parent()
        .expect("workspace parent")
        .join("feat-x-ws");
    std::fs::write(workspace_dir.join("w.txt"), "work\n").expect("write workspace content");
    lab.jj_at(&workspace_dir, ["new", "-m", "wip"]);

    let repo = Repo::open(&lab.work).expect("open");
    let wanted = BTreeSet::from([WorkspaceName::new("feat-x")]);
    let activity = repo.workspace_activity(&wanted, 200).expect("walk");

    assert!(
        activity.moves.contains_key(&WorkspaceName::new("feat-x")),
        "was: {activity:?}"
    );
}

#[test]
fn workspace_activity_reports_nothing_for_a_workspace_that_never_moved() {
    let lab = Lab::new();
    let repo = Repo::open(&lab.work).expect("open");
    let wanted = BTreeSet::from([WorkspaceName::new("never-created")]);
    let activity = repo
        .workspace_activity(&wanted, operation_ids(&lab.work).len())
        .expect("walk");

    assert!(activity.moves.is_empty(), "was: {activity:?}");
    assert!(activity.horizon.is_none(), "was: {activity:?}");
}

#[test]
fn local_commit_after_push_is_ahead_of_origin() {
    // Given: a branch already pushed to origin and one additional local commit.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.jj_work(["new", "-r", "feat/alpha", "-m", "local advance"]);
    std::fs::write(lab.work.join("local.txt"), "local\n").expect("write local commit");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);

    // When: the origin relation is resolved from both tips.
    let relation = relation_to_origin(&lab);

    // Then: origin is the ancestor, so local is ahead.
    assert!(relation.is_ok());
    assert_eq!(
        relation.expect("resolved relation"),
        Some(OriginRelation::Ahead)
    );
}

#[test]
fn origin_commit_after_push_leaves_local_branch_behind() {
    // Given: a branch pushed to origin, then advanced from another clone.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.jj_work(["bookmark", "untrack", "feat/alpha", "--remote", "origin"]);
    lab.advance_origin_branch("feat/alpha", "origin advance\n");
    lab.fetch_work();

    // When: the local and fetched origin tips are compared.
    let relation = relation_to_origin(&lab);

    // Then: origin is the descendant, so local is behind.
    assert_eq!(
        relation.expect("resolved relation"),
        Some(OriginRelation::Behind)
    );
}

#[test]
fn rewritten_local_branch_is_diverged_from_origin() {
    // Given: a pushed branch whose local tip was rewritten without updating origin.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "feature.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.rewrite_local_branch("feat/alpha", "rewritten\n");

    // When: the origin relation is resolved from mutually unreachable tips.
    let relation = relation_to_origin(&lab);

    // Then: neither side is announced as behind the other.
    assert!(relation.is_ok());
    assert_eq!(
        relation.expect("resolved relation"),
        Some(OriginRelation::Diverged)
    );
}

#[test]
fn an_unresolvable_origin_tip_returns_an_error() {
    // Given: a real local branch and an origin id the repository cannot resolve.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let repo = Repo::open(&lab.work).expect("open");
    let branch = BranchName::new("feat/alpha");
    let tip = repo.resolve_commit(branch.as_str()).expect("local tip");
    let unresolved = CommitId::new("1111111111111111111111111111111111111111");

    // When: the resolver compares local history to that absent origin tip.
    let error = status::phases::relation_to_origin(&repo, &tip, Some(&unresolved))
        .expect_err("an unresolved origin tip must not become a relation");

    // Then: the caller receives an error to report rather than a history verdict.
    assert!(error.to_string().contains(unresolved.as_str()));
}

#[test]
fn ancestry_is_answered_in_both_directions() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let repo = knives::jj::Repo::open(&lab.work).expect("open");
    let tip = repo.resolve_commit("feat/alpha").expect("tip");
    let base = repo.resolve_commit("main").expect("main");

    assert!(repo.is_ancestor(&base, &tip).expect("base is behind tip"));
    assert!(
        !repo
            .is_ancestor(&tip, &base)
            .expect("tip is not behind base")
    );
}

#[test]
fn a_tip_carried_into_another_branch_is_found() {
    // Given: a maintainer branch built on our branch's tip
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let repo = Repo::open(&lab.work).expect("open");
    let tip = repo.resolve_commit("feat/alpha").expect("tip");
    lab.jj_work(["bookmark", "create", "theirs/rework", "-r", "feat/alpha"]);
    lab.jj_work(["new", "theirs/rework", "-m", "extra work on top"]);
    lab.jj_work(["bookmark", "set", "theirs/rework", "-r", "@"]);

    // When: bookmarks carrying the original tip are listed
    let repo = Repo::open(&lab.work).expect("reopen");
    let carriers = repo
        .branches_containing(&tip, &ReleaseScheme::Dated, "origin")
        .expect("carriers");
    let named: Vec<String> = carriers.iter().map(ToString::to_string).collect();

    // Then: the other branch is included and the branch itself is not
    assert!(
        named.iter().any(|name| name.contains("theirs/rework")),
        "was: {named:?}"
    );
    assert!(
        !named.iter().any(|name| name == "feat/alpha"),
        "a branch does not carry itself: {named:?}"
    );
}

#[test]
fn a_release_cut_is_not_a_carrier_locally_or_at_origin() {
    // Given: a flat release cut that carries our feature branch, then is pushed.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let tip = repo.resolve_commit("feat/alpha").expect("tip");
    lab.octopus("release/2026-07-30", "feat/alpha", "feat/beta");
    lab.push_branch("release/2026-07-30");

    // When: carriers of the feature tip are listed.
    let carriers = Repo::open(&lab.work)
        .expect("reopen")
        .branches_containing(&tip, &ReleaseScheme::Dated, "origin")
        .expect("carriers");

    // Then: the release is not reported through either representation we own.
    assert!(
        !carriers.contains(&BookmarkRef::Local(BranchName::new("release/2026-07-30"))),
        "local release was reported: {carriers:?}"
    );
    assert!(
        !carriers.contains(&BookmarkRef::Remote {
            branch: BranchName::new("release/2026-07-30"),
            remote: RemoteName::new("origin"),
        }),
        "origin release was reported: {carriers:?}"
    );
}

#[test]
fn git_tracking_refs_are_not_carriers_but_other_branches_are() {
    // Given: a maintainer branch carrying our tip and jj's matching git-tracking ref.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let repo = Repo::open(&lab.work).expect("open");
    let tip = repo.resolve_commit("feat/alpha").expect("tip");
    lab.jj_work(["bookmark", "create", "theirs/rework", "-r", "feat/alpha"]);
    lab.jj_work(["new", "theirs/rework", "-m", "extra work on top"]);
    lab.jj_work(["bookmark", "set", "theirs/rework", "-r", "@"]);

    // When: carriers of our tip are listed.
    let carriers = Repo::open(&lab.work)
        .expect("reopen")
        .branches_containing(&tip, &ReleaseScheme::Dated, "origin")
        .expect("carriers");

    // Then: the real branch remains useful evidence, but jj's duplicate does not.
    assert!(
        carriers.contains(&BookmarkRef::Local(BranchName::new("theirs/rework"))),
        "the ordinary carrier was lost: {carriers:?}"
    );
    assert!(
        !carriers.iter().any(|reference| {
            matches!(reference, BookmarkRef::Remote { remote, .. } if remote.as_str() == "git")
        }),
        "git-tracking refs were reported: {carriers:?}"
    );
}

#[test]
fn fetched_pull_request_heads_are_not_carriers() {
    // Given: a fetched pull-request head that descends from our branch tip.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let tip = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("tip");
    lab.jj_work(["bookmark", "create", "pr-4545", "-r", "feat/alpha"]);
    lab.jj_work(["new", "pr-4545", "-m", "fetched pull head advance"]);
    std::fs::write(lab.work.join("pull-head.txt"), "fetched\n").expect("write pull head");
    lab.jj_work(["bookmark", "set", "pr-4545", "-r", "@"]);
    lab.jj_work(["new"]);

    // When: carriers of the feature tip are listed.
    let carriers = Repo::open(&lab.work)
        .expect("reopen")
        .branches_containing(&tip, &ReleaseScheme::Dated, "origin")
        .expect("carriers");

    // Then: our fetched pull request is not mistaken for someone else's carrier.
    assert!(
        !carriers.contains(&BookmarkRef::Local(BranchName::new("pr-4545"))),
        "fetched pull head was reported: {carriers:?}"
    );
}

#[test]
fn bookmark_tips_keeps_local_and_remote_refs_distinct() {
    // Given: a fork checkout with the same branch name locally and on origin.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.push_branch("feature");
    lab.rewrite_local_branch("feature", "rewritten\n");

    // When: typed bookmark tips are read through jj-lib.
    let tips = Repo::open(&lab.work)
        .expect("open repository")
        .bookmark_tips()
        .expect("read tips");

    // Then: the local and remote references remain separate map keys.
    let local = BookmarkRef::Local(BranchName::new("feature"));
    let remote = BookmarkRef::Remote {
        branch: BranchName::new("feature"),
        remote: RemoteName::new("origin"),
    };
    assert_ne!(tips.get(&local), tips.get(&remote));
}

#[test]
fn parents_of_octopus_includes_bookmarks_for_every_parent() {
    // Given: an octopus merge over two labelled branches and main.
    let lab = lab::Lab::new();
    lab.branch("one", "one.txt", "one\n");
    lab.branch("two", "two.txt", "two\n");
    lab.octopus("release", "one", "two");

    // When: its parents are read through jj-lib.
    let parents = Repo::open(&lab.work)
        .expect("open repository")
        .parents_of("release")
        .expect("read parents");

    // Then: every octopus parent retains its bookmark reference.
    assert_eq!(parents.len(), 3);
    assert!(parents.iter().any(|parent| {
        parent
            .bookmarks
            .contains(&BookmarkRef::Local(BranchName::new("one")))
    }));
    assert!(parents.iter().any(|parent| {
        parent
            .bookmarks
            .contains(&BookmarkRef::Local(BranchName::new("two")))
    }));
}

#[test]
fn divergent_changes_reports_both_rewrites_after_fetch() {
    // Given: the same branch rewritten independently in two jj clones.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.rewrite_in_both_clones("feature");

    // When: divergence is read through jj-lib after fetching.
    let divergent = Repo::open(&lab.work)
        .expect("open repository")
        .divergent_changes(&std::collections::BTreeSet::new())
        .expect("read divergence");

    // Then: exactly one change has two visible commits.
    assert_eq!(divergent.len(), 2);
    assert_eq!(divergent[0].0, divergent[1].0);
    assert_ne!(divergent[0].1, divergent[1].1);
}

#[test]
fn divergent_changes_reports_copies_buried_under_descendants() {
    // Given: one change as two visible commits, EACH buried under a child, so
    // neither copy is a view head. This is the fleet's dominant shape — a
    // branch advanced past its rewritten ancestor while a remote-pinned chain
    // kept the old copy — and enumerating only head copies missed it (the jj
    // fork carried 74 divergent changes; 35 were reported).
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.rewrite_in_both_clones("feature");
    // Bury the local rewrite: make the parked child real so it survives, then
    // bury the fetched rewrite under a child of its own.
    std::fs::write(lab.work.join("local-child.txt"), "local\n").expect("write local child");
    lab.jj_work(["describe", "-m", "child of local rewrite"]);
    lab.jj_work(["new", "feature@origin", "-m", "child of fetched rewrite"]);
    std::fs::write(lab.work.join("fetched-child.txt"), "fetched\n").expect("write fetched child");
    lab.jj_work(["status"]);

    // When: divergence is read through jj-lib.
    let divergent = Repo::open(&lab.work)
        .expect("open repository")
        .divergent_changes(&std::collections::BTreeSet::new())
        .expect("read divergence");

    // Then: the buried pair is still reported.
    assert_eq!(divergent.len(), 2, "{divergent:?}");
    assert_eq!(divergent[0].0, divergent[1].0);
    assert_ne!(divergent[0].1, divergent[1].1);
}

#[test]
fn divergence_pinned_only_by_a_superseded_release_ref_is_not_reported() {
    // Given: one change as two commits, where the old copy's only visibility is a
    // remote-tracking ref we are told to ignore. This is the re-materialized
    // superseded-cut shape: bare `jj git fetch` brings such refs back forever,
    // so the reader must ignore them rather than the graph staying clean.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "feature.txt", "one\n");
    lab.rewrite_in_both_clones("feat/alpha");
    let repo = Repo::open(&lab.work).expect("open");

    // Sanity: unfiltered, the divergence is visible (two commits, one change).
    let unfiltered = repo
        .divergent_changes(&std::collections::BTreeSet::new())
        .expect("divergent unfiltered");
    assert!(
        !unfiltered.is_empty(),
        "fixture failed to create divergence"
    );

    // When: the ref holding the stale copy is ignored.
    let ignored = std::collections::BTreeSet::from([BookmarkRef::Remote {
        branch: BranchName::new("feat/alpha"),
        remote: RemoteName::new("origin"),
    }]);
    let filtered = repo
        .divergent_changes(&ignored)
        .expect("divergent filtered");

    // Then: the finding is gone — the stale copy was visible only through the
    // ignored ref, so nothing else vouches for it.
    assert!(filtered.is_empty(), "still reported: {filtered:?}");
}

#[test]
fn divergence_with_only_ignored_head_keeps_copies_vouched_by_live_heads() {
    // Given: two rewrites of one change, each kept as a non-head ancestor of a
    // live head, plus a third rewrite whose sole head ref is a dated release.
    let lab = Lab::new();
    lab.branch("feat/alpha", "feature.txt", "one\n");
    lab.rewrite_in_both_clones("feat/alpha");

    std::fs::write(lab.work.join("keep-one.txt"), "keep\n").expect("write first child");
    lab.jj_work(["describe", "-m", "keep first rewrite"]);
    lab.jj_work(["new", "feat/alpha@origin", "-m", "keep second rewrite"]);
    std::fs::write(lab.work.join("keep-two.txt"), "keep\n").expect("write second child");
    lab.jj_work(["status"]);

    let status = Command::new("jj")
        .args(["edit", "--ignore-immutable", "feat/alpha"])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("rewrite release head");
    assert!(status.success(), "rewrite release head");
    std::fs::write(lab.second.join("feature.txt"), "third rewrite\n").expect("write third rewrite");
    let status = Command::new("jj")
        .args(["bookmark", "create", "release/2024-01-01", "-r", "@"])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("create release bookmark");
    assert!(status.success(), "create release bookmark");
    let status = Command::new("jj")
        .args([
            "git",
            "push",
            "--remote",
            "origin",
            "--bookmark",
            "release/2024-01-01",
        ])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("push release bookmark");
    assert!(status.success(), "push release bookmark");
    lab.fetch_work();
    lab.jj_work(["bookmark", "forget", "--include-remotes", "feat/alpha"]);
    let repo = Repo::open(&lab.work).expect("open");

    // When: the release ref is ignored.
    let unfiltered = repo
        .divergent_changes(&std::collections::BTreeSet::new())
        .expect("read unfiltered divergence");
    let ignored = std::collections::BTreeSet::from([BookmarkRef::Remote {
        branch: BranchName::new("release/2024-01-01"),
        remote: RemoteName::new("origin"),
    }]);
    let filtered = repo
        .divergent_changes(&ignored)
        .expect("read filtered divergence");

    // Then: the ignored head is excluded but its two live-vouched sibling copies remain.
    assert_eq!(unfiltered.len(), 3, "fixture should expose three copies");
    assert_eq!(
        filtered.len(),
        2,
        "live-vouched copies disappeared: {filtered:?}"
    );
    assert_eq!(filtered[0].0, filtered[1].0);
}

#[test]
fn unrelated_divergence_survives_a_nonempty_ignored_ref_set() {
    // Given: independently rewritten alpha and beta branches.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "one\n");
    lab.rewrite_in_both_clones("feat/alpha");
    lab.branch("feat/beta", "beta.txt", "one\n");
    lab.rewrite_in_both_clones("feat/beta");
    let repo = Repo::open(&lab.work).expect("open");

    // When: only alpha's remote ref is ignored.
    let unfiltered = repo
        .divergent_changes(&std::collections::BTreeSet::new())
        .expect("read unfiltered divergence");
    let ignored = std::collections::BTreeSet::from([BookmarkRef::Remote {
        branch: BranchName::new("feat/alpha"),
        remote: RemoteName::new("origin"),
    }]);
    let filtered = repo
        .divergent_changes(&ignored)
        .expect("read filtered divergence");

    // Then: alpha is suppressed while beta's pair remains reported.
    assert_eq!(unfiltered.len(), 4, "fixture should expose two divergences");
    assert_eq!(
        filtered.len(),
        2,
        "beta divergence disappeared: {filtered:?}"
    );
    assert_eq!(filtered[0].0, filtered[1].0);
}

#[test]
fn pull_heads_reads_local_upstream_pull_refs() {
    // Given: a branch published at an upstream pull-ref namespace.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.push_branch("feature");
    lab.publish_pull("feature", 42);

    // When: pull heads are listed through the git transport.
    let heads = pull_heads(
        &lab.work,
        lab.upstream.to_str().expect("utf-8 upstream path"),
    )
    .expect("read pull heads");

    // Then: the pull number maps to its published object id.
    assert!(heads.contains_key(&42));
}

#[test]
fn remote_refs_reads_live_heads_by_pattern() {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    let url = lab.temp_origin().display().to_string();

    let refs = remote_refs(&url, &["refs/heads/*"]).expect("ls-remote");

    assert!(refs.contains_key("refs/heads/feat/alpha"));
    let none = remote_refs(&url, &["refs/pull/*/head"]).expect("ls-remote");
    assert!(none.is_empty(), "a path remote has no pull refs");
}

#[test]
fn changed_files_reports_sorted_paths_without_snapshotting_working_copy() {
    // Given: a branch with one changed file and an untouched working copy.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    let working_copy_before = lab.revision(&lab.work, "@", "change_id");

    // When: changed paths are requested for the branch revision.
    let files = changed_files(&lab.work, "feature").expect("read changed files");

    // Then: paths are normalized, sorted, and the working copy stays untouched.
    assert_eq!(files, vec!["feature.txt"]);
    assert_eq!(
        lab.revision(&lab.work, "@", "change_id"),
        working_copy_before
    );
}

#[test]
fn changed_files_between_handles_a_branch_behind_advanced_upstream() {
    // Given: a branch whose upstream trunk has advanced since the branch forked.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "main@upstream",
        "-m",
        "merge upstream into branch",
    ]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    let working_copy_before = lab.revision(&lab.work, "@", "change_id");
    let from = "fork_point(main@upstream | feat/alpha)";

    // When: the branch tree is compared directly with its fork point.
    let files = changed_files_between(&lab.work, from, "feat/alpha").expect("diff branch trees");

    // Then: the branch file is returned without changing the working copy.
    assert_eq!(files, vec!["alpha.txt"]);
    assert_eq!(
        lab.revision(&lab.work, "@", "change_id"),
        working_copy_before
    );
}

#[test]
fn cutting_a_release_does_not_move_another_agents_working_copy() {
    // Reproduction of a defect found in review. `create_merge` used `jj new`,
    // which moves `@`, so cutting a release parked whoever was working in the
    // repo's default workspace on top of the release octopus with their
    // uncommitted edits pending against it. That is verbatim the accident
    // `knives start` exists to prevent, caused by `knives release`.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");

    lab.jj_work(["new", "main", "-m", "SOMEONE ELSE MID-TASK"]);
    std::fs::write(lab.work.join("their-wip.txt"), "in progress\n").expect("write");
    let before = lab.revision(&lab.work, "@", "change_id");

    let alpha =
        knives::ids::CommitId::new(lab.revision(&lab.work, "feat/alpha", "commit_id").trim());
    let beta = knives::ids::CommitId::new(lab.revision(&lab.work, "feat/beta", "commit_id").trim());
    let _ = knives::jj::write_release(
        &lab.work,
        &knives::jj::ReleaseWrite {
            source: None,
            parents: &[alpha, beta],
            message: Some("release: test"),
            bookmark: None,
            operation: "knives: cut release: test",
        },
    )
    .expect("merge");

    let after = lab.revision(&lab.work, "@", "change_id");
    assert_eq!(
        before, after,
        "cutting a release moved someone else's working copy"
    );
    assert!(
        lab.work.join("their-wip.txt").exists(),
        "their uncommitted file vanished"
    );
}

#[test]
fn a_release_write_with_no_parents_is_refused_rather_than_done_in_place() {
    // A caller that computed an empty parent set would report the change it
    // meant to make while the composition stayed exactly as it was.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let source = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("resolve alpha");

    let error = knives::jj::write_release(
        &lab.work,
        &knives::jj::ReleaseWrite {
            source: Some(&source),
            parents: &[],
            message: None,
            bookmark: None,
            operation: "knives: an empty edit",
        },
    )
    .expect_err("must refuse");

    assert!(
        error.to_string().contains("destination parent"),
        "the error must say what was missing: {error}"
    );
}
