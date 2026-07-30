#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::commands::{
    status::{self, OriginRelation},
    sync,
};
use knives::config::RepoEntry;
use knives::detect::landed::RebaseOutcome;
use knives::forge::{ChecksSummary, Forge, ForgeError, PullRequest};
use knives::ids::{BookmarkRef, BranchName, CommitId, RemoteName};
use knives::jj::{Repo, changed_files, changed_files_between, probe_landed, pull_heads};
use knives::store::Store;
use std::collections::BTreeMap;

struct StateUnavailableForge;

impl Forge for StateUnavailableForge {
    fn pull_requests(
        &self,
        _repo: &std::path::Path,
    ) -> Result<BTreeMap<BranchName, PullRequest>, ForgeError> {
        Ok(BTreeMap::new())
    }

    fn review_predates_head(
        &self,
        _repo: &std::path::Path,
        _number: u64,
    ) -> Result<Option<bool>, ForgeError> {
        Ok(None)
    }

    fn checks(
        &self,
        _repo: &std::path::Path,
        _number: u64,
    ) -> Result<Option<ChecksSummary>, ForgeError> {
        Ok(None)
    }

    fn pull_request_state(
        &self,
        _repo: &std::path::Path,
        _number: u64,
    ) -> Result<Option<String>, ForgeError> {
        Err(ForgeError::Command {
            command: "gh pr view".to_owned(),
            dir: "/repo".to_owned(),
            code: 1,
            stderr: "unavailable".to_owned(),
        })
    }

    fn newest_comment(
        &self,
        _repo: &std::path::Path,
        _number: u64,
    ) -> Result<Option<String>, ForgeError> {
        Ok(None)
    }
}

fn relation_to_origin(lab: &lab::Lab) -> Result<Option<OriginRelation>, knives::jj::JjError> {
    let repo = Repo::open(&lab.work).expect("open");
    let branch = BranchName::new("feat/alpha");
    let tip = repo.resolve_commit(branch.as_str()).expect("local tip");
    let origin_tip = repo
        .resolve_commit("feat/alpha@origin")
        .expect("origin tip");

    status::relation_to_origin(&repo, &tip, Some(&origin_tip))
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
    let error = status::relation_to_origin(&repo, &tip, Some(&unresolved))
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
    let carriers = repo.branches_containing(&tip).expect("carriers");
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
        .branches_containing(&tip)
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
        .branches_containing(&tip)
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
        .branches_containing(&tip)
        .expect("carriers");

    // Then: our fetched pull request is not mistaken for someone else's carrier.
    assert!(
        !carriers.contains(&BookmarkRef::Local(BranchName::new("pr-4545"))),
        "fetched pull head was reported: {carriers:?}"
    );
}

#[test]
fn unavailable_state_for_a_tracked_pull_request_is_incomplete() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.publish_pull("feat/alpha", 42);
    let name = knives::ids::RepoName::new("a-repo");
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let mut store = Store::open_for_update(lab.work.join("state.json")).expect("store");
    store.record_pull_head(&name, 42, "previous");

    let report = sync::sync_repo(&name, &entry, &mut store, Some(&StateUnavailableForge))
        .expect("sync report");

    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.contains("state of #42 unavailable")),
        "was: {report:?}"
    );
    assert!(report.notes.is_empty(), "was: {report:?}");
    assert_eq!(sync::exit_for(&report), knives::cli::Exit::Incomplete);
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
fn squash_merge_lands_content_that_ancestry_cannot_see() {
    // The reason this crate probes instead of asking about ancestry. Both halves
    // are asserted, and the second goes through the crate, so a regression in
    // `probe_landed` fails this test rather than only the environment fact.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);
    lab.fetch_work();

    // Ancestry says the branch is not in the trunk.
    let merged_in = lab.revision(
        &lab.work,
        "feat/alpha & ::main@upstream",
        "commit_id ++ \"\\n\"",
    );
    assert!(
        merged_in.trim().is_empty(),
        "the branch became an ancestor; the premise is gone"
    );

    // The crate says its content is there anyway.
    let verdict = knives::jj::probe_landed(
        &lab.work,
        &knives::ids::BranchName::new("feat/alpha"),
        "main@upstream",
    )
    .expect("probe");
    assert_eq!(verdict, knives::detect::RebaseOutcome::Empty);
}

#[test]
fn probe_landed_is_empty_after_plain_squash_merge() {
    // Given: a branch squash-merged unchanged upstream.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.push_branch("feature");
    lab.publish_pull("feature", 8);
    lab.squash_merge_pull(8, None);

    // When: the branch is replayed onto upstream main.
    let outcome = probe_landed(&lab.work, &BranchName::new("feature"), "main@upstream")
        .expect("probe landed");

    // Then: jj reports that replay as empty.
    assert_eq!(outcome, RebaseOutcome::Empty);
}

#[test]
fn probe_landed_is_conflicted_after_maintainer_changes_squash_content() {
    // Given: a branch squash-merged with maintainer edits.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.push_branch("feature");
    lab.publish_pull("feature", 9);
    lab.squash_merge_pull(9, Some("maintainer rewrite\n"));

    // When: the branch is replayed onto upstream main.
    let outcome = probe_landed(&lab.work, &BranchName::new("feature"), "main@upstream")
        .expect("probe landed");

    // Then: jj preserves the maintainer conflict.
    assert_eq!(outcome, RebaseOutcome::Conflicted);
}

#[test]
fn probe_landed_is_clean_nonempty_for_open_branch() {
    // Given: an open branch not present upstream.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.advance_upstream("advance\n");
    lab.rebase_and_force_push("feature");

    // When: it is replayed onto upstream main.
    let outcome = probe_landed(&lab.work, &BranchName::new("feature"), "main@upstream")
        .expect("probe landed");

    // Then: the replay is clean but still carries work.
    assert_eq!(outcome, RebaseOutcome::CleanNonEmpty);
}

#[test]
fn probe_landed_cleans_only_its_temporary_commits() {
    // Given: an open branch and a stable working copy.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    let children_before = lab.revision(&lab.work, "children(main@upstream)", "commit_id");
    let branch_before = lab.revision(&lab.work, "feature", "commit_id");
    let working_copy_before = lab.revision(&lab.work, "@", "change_id");

    // When: landing is probed.
    probe_landed(&lab.work, &BranchName::new("feature"), "main@upstream").expect("probe landed");

    // Then: only temporary probe commits have disappeared.
    assert_eq!(
        lab.revision(&lab.work, "children(main@upstream)", "commit_id"),
        children_before
    );
    assert_eq!(
        lab.revision(&lab.work, "feature", "commit_id"),
        branch_before
    );
    assert_eq!(
        lab.revision(&lab.work, "@", "change_id"),
        working_copy_before
    );
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
        .divergent_changes()
        .expect("read divergence");

    // Then: exactly one change has two visible commits.
    assert_eq!(divergent.len(), 2);
    assert_eq!(divergent[0].0, divergent[1].0);
    assert_ne!(divergent[0].1, divergent[1].1);
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
fn the_probe_cleans_up_every_commit_it_created_not_just_the_first() {
    // A branch of several commits duplicates as several. All of them must be
    // abandoned, and all of them must count toward the verdict. Identifying
    // them by what jj reported creating is what makes that possible; the old
    // set-difference approach could not tell them from another agent's commits.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("feat/pair", "feat/alpha", "feat/beta");

    let before = lab.revision(&lab.work, "all()", "commit_id ++ \"\\n\"");
    let outcome = knives::jj::probe_landed(
        &lab.work,
        &knives::ids::BranchName::new("feat/pair"),
        "main",
    );
    assert!(
        outcome.is_ok(),
        "a multi-commit branch must be probed, not refused: {outcome:?}"
    );
    let after = lab.revision(&lab.work, "all()", "commit_id ++ \"\\n\"");

    assert_eq!(before, after, "the probe left commits behind");
}

#[test]
fn a_new_workspace_is_based_on_the_upstream_trunk_not_the_current_change() {
    // The accident this default exists to prevent: an agent sitting in a release
    // workspace runs `jj new` and silently inherits the release merge as a
    // parent, so unrelated work rides into its pull request.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/dated", "feat/alpha", "feat/beta");
    // Move the upstream trunk so it differs from our fork's copy. Without this
    // the two are the same commit and the test passes whichever trunk the code
    // uses, proving nothing.
    lab.advance_upstream("moved on\n");

    // Given: the working copy is parked on the release merge, the dangerous spot
    let parked = lab.revision(&lab.work, "@", "change_id.short(8)");
    assert!(!parked.trim().is_empty());

    // When: a workspace is opened the way `knives start` opens one
    let destination = lab.work.parent().expect("parent").join("feat-gamma");
    knives::jj::add_workspace(&lab.work, "feat-gamma", &destination, "main@upstream")
        .expect("add workspace");

    // Then: its only parent is the upstream trunk, not the release merge
    let parents = lab.revision(&destination, "parents(@)", "commit_id.short(12) ++ \"\\n\"");
    let upstream = lab.revision(&lab.work, "main@upstream", "commit_id.short(12)");
    let listed: Vec<&str> = parents.split_whitespace().collect();
    assert_eq!(
        listed,
        vec![upstream.trim()],
        "new work must sit on the upstream trunk alone"
    );
}

#[test]
fn a_cut_is_flat_and_carries_its_provenance() {
    // A release must be a flat merge of exactly the parents intended. The
    // failure this guards is silent: a cut that dropped a parent looks exactly
    // like one that did not, until work goes missing downstream.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");

    let alpha =
        knives::ids::CommitId::new(lab.revision(&lab.work, "feat/alpha", "commit_id").trim());
    let beta = knives::ids::CommitId::new(lab.revision(&lab.work, "feat/beta", "commit_id").trim());

    let request = knives::commands::release::Cut {
        name: "release/2026-07-30".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha, "pull/10/head".to_owned()),
            (beta, "feat/beta".to_owned()),
        ],
    };
    let created = knives::commands::release::cut(&lab.work, &request).expect("cut");

    // Flat: exactly two parents, no nested integration node.
    let parents = knives::jj::Repo::open(&lab.work)
        .expect("open")
        .parents_of(created.as_str())
        .expect("parents");
    assert_eq!(parents.len(), 2, "a release must be flat");

    // The dated name points at it, and the provenance rode along.
    let named = lab.revision(&lab.work, "release/2026-07-30", "commit_id");
    assert_eq!(named.trim(), created.as_str());
    let message = lab.revision(&lab.work, created.as_str(), "description");
    assert!(
        message.contains("from pull/10/head"),
        "provenance was lost: {message}"
    );
}

#[test]
fn a_cut_refuses_when_the_merge_did_not_get_the_parents_it_asked_for() {
    // The refusal was untested: the whole ensure! block could be deleted and the
    // flatness test still passed. jj dedupes duplicate parents, so asking for
    // the same commit twice produces a merge with fewer parents than requested,
    // which is exactly the "a branch's work was dropped" shape.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let alpha =
        knives::ids::CommitId::new(lab.revision(&lab.work, "feat/alpha", "commit_id").trim());

    let request = knives::commands::release::Cut {
        name: "release/2026-07-30".to_owned(),
        parents: vec![alpha.clone(), alpha.clone()],
        provenance: vec![(alpha, "feat/alpha".to_owned())],
    };
    let outcome = knives::commands::release::cut(&lab.work, &request);
    assert!(
        outcome.is_err(),
        "a parent-count mismatch must refuse, got {outcome:?}"
    );

    // And the dated name was not set on a bad cut. Asked of the bookmark list,
    // because resolving a bookmark that rightly does not exist is an error.
    let names = knives::jj::Repo::open(&lab.work)
        .expect("open")
        .bookmark_tips()
        .expect("tips");
    assert!(
        !names
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-07-30"),
        "a refused cut still claimed the name"
    );
}

#[test]
fn status_with_the_landed_probe_reports_a_merged_branch_and_leaves_no_trace() {
    // The probe path through `knives status` end to end. It is exercised here and
    // deliberately never against a live shared repository, because it mutates.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);

    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let name = knives::ids::RepoName::new("a-repo");
    let temp = std::env::temp_dir().join(format!("knives-status-{}", std::process::id()));
    let store = knives::store::Store::open(temp.join("state.json")).expect("store");

    let before = lab.revision(&lab.work, "children(main@upstream)", "commit_id ++ \"\\n\"");
    let report = knives::commands::status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: true,
            forge: None,
            registry: None,
        },
    )
    .expect("gather");
    let after = lab.revision(&lab.work, "children(main@upstream)", "commit_id ++ \"\\n\"");

    // The merged branch reads as already in the trunk. Stated on the branch itself
    // rather than as a finding: it is a fact about the branch, not something wrong.
    let verdicts: Vec<_> = report
        .branches
        .iter()
        .filter_map(|row| row.landed)
        .collect();
    assert!(
        verdicts.contains(&knives::detect::landed::LandedVerdict::InTrunk),
        "expected the squash-merged branch to read as in-trunk: {verdicts:?}"
    );

    // And the probe left the repository as it found it.
    assert_eq!(before, after, "the probe left commits behind");
}

#[test]
fn status_reports_branch_overlap_after_upstream_advances_without_landed_probe() {
    // Given: two maintained branches from a trunk revision that upstream has since advanced past
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    lab.advance_upstream("upstream advanced past the branches\n");
    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let store_path = lab
        .work
        .parent()
        .expect("lab work directory has a parent")
        .join("state.json");
    let store = knives::store::Store::open(store_path).expect("store");
    let name = knives::ids::RepoName::new("a-repo");

    // When: status deliberately skips only the landed replay
    let report = knives::commands::status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            registry: None,
        },
    )
    .expect("gather");

    // Then: the independent path comparison still reports the shared file
    let overlap = report
        .findings
        .iter()
        .find(|finding| {
            finding.kind == knives::detect::FindingKind::BranchOverlap
                && finding.subject == knives::detect::Subject::File("shared.txt".to_owned())
        })
        .expect("the shared file is reported even without the landed probe");
    assert!(overlap.detail.contains("feat/alpha"), "was: {overlap:?}");
    assert!(overlap.detail.contains("feat/beta"), "was: {overlap:?}");
}

#[test]
fn status_reports_a_branch_carried_elsewhere() {
    // Given: an open branch whose tip is an ancestor of another local bookmark
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["bookmark", "create", "theirs/rework", "-r", "feat/alpha"]);
    lab.jj_work(["new", "theirs/rework", "-m", "extra work on top"]);
    lab.jj_work(["bookmark", "set", "theirs/rework", "-r", "@"]);
    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let name = knives::ids::RepoName::new("a-repo");
    let temp = std::env::temp_dir().join(format!("knives-status-{}", std::process::id()));
    let store = knives::store::Store::open(temp.join("state.json")).expect("store");

    // When: status gathers the branch report
    let report = knives::commands::status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            registry: None,
        },
    )
    .expect("gather");

    // Then: the branch fact names the reference that reaches its tip
    assert!(report.findings.iter().any(|finding| {
        finding.kind == knives::detect::FindingKind::CarriedElsewhere
            && finding.subject == knives::detect::Subject::Branch(BranchName::new("feat/alpha"))
            && finding.detail.contains("theirs/rework")
    }));
}

#[test]
fn status_reports_a_carrier_for_a_closed_pull_request() {
    // Given: a closed pull request whose branch is reachable from another bookmark
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["bookmark", "create", "theirs/rework", "-r", "feat/alpha"]);
    lab.jj_work(["new", "theirs/rework", "-m", "extra work on top"]);
    lab.jj_work(["bookmark", "set", "theirs/rework", "-r", "@"]);
    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let name = knives::ids::RepoName::new("a-repo");
    let temp = std::env::temp_dir().join(format!("knives-status-{}", std::process::id()));
    let store = knives::store::Store::open(temp.join("state.json")).expect("store");
    let forge = knives::forge::FakeForge {
        pull_requests: std::iter::once((
            BranchName::new("feat/alpha"),
            knives::forge::PullRequest {
                number: 7,
                state: "CLOSED".to_owned(),
                review_decision: String::new(),
                head_ref_name: "feat/alpha".to_owned(),
                head_ref_oid: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                updated_at: "2026-01-01T00:00:00Z".to_owned(),
                is_draft: false,
                url: String::new(),
                head_repository_owner: None,
                mergeable: String::new(),
                merge_state_status: String::new(),
                base_ref_name: "main".to_owned(),
            },
        ))
        .collect(),
        ..knives::forge::FakeForge::default()
    };

    // When: status gathers the branch report with the closed pull request
    let report = knives::commands::status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: Some(&forge),
            registry: None,
        },
    )
    .expect("gather");

    // Then: forge state does not suppress the local ancestry fact
    assert!(report.findings.iter().any(|finding| {
        finding.kind == knives::detect::FindingKind::CarriedElsewhere
            && finding.subject == knives::detect::Subject::Branch(BranchName::new("feat/alpha"))
            && finding.detail.contains("theirs/rework")
    }));
}

#[test]
fn status_does_not_report_trunk_as_a_carrier_without_landed_probe() {
    // Given: a branch whose tip is reachable from the local trunk bookmark
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work([
        "new",
        "-r",
        "main@origin",
        "-r",
        "feat/alpha",
        "-m",
        "trunk carries feature",
    ]);
    lab.jj_work(["bookmark", "set", "main", "-r", "@"]);
    lab.jj_work(["new"]);
    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let name = knives::ids::RepoName::new("a-repo");
    let temp = std::env::temp_dir().join(format!("knives-status-{}", std::process::id()));
    let store = knives::store::Store::open(temp.join("state.json")).expect("store");

    // When: status skips the landed probe
    let report = knives::commands::status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            registry: None,
        },
    )
    .expect("gather");

    // Then: trunk is never a carrier finding, even without an InTrunk verdict
    assert!(!report.findings.iter().any(|finding| {
        finding.kind == knives::detect::FindingKind::CarriedElsewhere
            && finding.subject == knives::detect::Subject::Branch(BranchName::new("feat/alpha"))
    }));
}

#[test]
fn a_fresh_cut_carries_every_branch_and_nothing_else() {
    // What a dated release is: a flat merge of the current tip of everything we
    // carry. Not the trunk, not other releases.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-07-29", "feat/alpha", "feat/beta");

    let repo = knives::jj::Repo::open(&lab.work).expect("open");
    let carried = knives::commands::release::carried_branches(&repo).expect("carried");
    let names: Vec<&str> = carried.iter().map(|(branch, _)| branch.as_str()).collect();

    assert!(names.contains(&"feat/alpha"));
    assert!(names.contains(&"feat/beta"));
    assert!(
        !names.iter().any(|n| n.starts_with("release/")),
        "a release is not a branch we carry"
    );
    assert!(
        !names.contains(&"main"),
        "the trunk is not a branch we carry"
    );
}

#[test]
fn preflight_flags_a_branch_whose_tip_is_divergent() {
    // Pins a bug that shipped silently: divergence findings carry a CHANGE id,
    // and comparing them against a branch tip's COMMIT id never matches, so
    // nothing was ever reported as divergent.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.rewrite_in_both_clones("feat/alpha");

    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let states = knives::commands::preflight::branch_states(&entry, &[]).expect("branch states");
    assert!(
        states.iter().any(|state| state.divergent),
        "a branch whose tip is divergent must be reported as divergent, got {states:#?}"
    );
}

#[test]
fn the_probe_never_abandons_a_commit_it_did_not_create() {
    // Reproduction of a data-loss defect found in review. The cleanup used to
    // identify its own commits by set difference over children(onto). A dirty
    // `@` that is a child of `onto` has its commit id rewritten by any
    // snapshotting command, so it appeared in that difference and was abandoned.
    // Three commits and two bookmarks of another agent's work were destroyed by
    // a single `knives status`. Cleanup now abandons only ids jj reported creating.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");

    // Another agent's work, sitting as a child of the trunk with a dirty tree.
    lab.jj_work(["new", "main", "-m", "SOMEONE ELSE MID-TASK"]);
    std::fs::write(lab.work.join("their-wip.txt"), "precious\n").expect("write");
    lab.jj_work(["bookmark", "create", "their-work", "-r", "@"]);
    let theirs = lab.revision(&lab.work, "their-work", "commit_id");

    let _ = knives::jj::probe_landed(
        &lab.work,
        &knives::ids::BranchName::new("feat/alpha"),
        "main",
    );

    // Their commit, their bookmark and their file all survive.
    let still_there = lab.revision(&lab.work, "their-work", "commit_id");
    assert_eq!(
        still_there.trim(),
        theirs.trim(),
        "the probe abandoned another agent's commit"
    );
    assert!(
        lab.work.join("their-wip.txt").exists(),
        "the probe destroyed another agent's uncommitted file"
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
    let _ = knives::jj::create_merge(&lab.work, &[alpha, beta], "release: test").expect("merge");

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
fn a_stranded_release_parent_reports_where_the_branch_went() {
    // The payload the design asks for. `parents_of` only ever reports bookmarks
    // pointing AT a parent, so the pure detector can only ever say "carries no
    // bookmark". Naming the branch and its new tip is the actionable half.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let stranded = lab.revision(&lab.work, "feat/alpha", "commit_id");

    // The branch moves on, leaving the old commit with nothing pointing at it.
    lab.jj_work(["new", "feat/alpha", "-m", "more work"]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    let moved_to = lab.revision(&lab.work, "feat/alpha", "commit_id");
    assert_ne!(stranded.trim(), moved_to.trim());

    let past = knives::jj::branches_past(&lab.work, &knives::ids::CommitId::new(stranded.trim()))
        .expect("branches past");
    assert!(
        past.iter().any(|(branch, tip)| branch.as_str() == "feat/alpha" && tip.as_str() == moved_to.trim()),
        "expected feat/alpha reported at its new tip, got {past:?}"
    );
}

#[test]
fn a_foreign_pull_request_can_be_fetched_and_carried_as_a_release_parent() {
    // The design allows a release parent to be any upstream pull request, not
    // only our own branches. Without the objects locally that commit cannot be a
    // merge parent at all, and none of the obvious fetch routes work: jj brings
    // branches only, and importing a raw pull ref leaves the commit invisible.
    let lab = lab::Lab::new();
    lab.branch("feat/theirs", "theirs.txt", "someone else's work\n");
    lab.push_branch("feat/theirs");
    lab.publish_pull("feat/theirs", 42);
    let sha = lab.revision(&lab.work, "feat/theirs", "commit_id");

    // A clone that has never seen the branch, only the pull ref.
    let fetched = knives::jj::fetch_pull_ref(&lab.second, &lab.upstream.display().to_string(), 42)
        .expect("fetch pull ref");
    assert_eq!(fetched.as_str(), sha.trim(), "fetched the wrong commit");

    // And it is usable as a parent, which is the whole point.
    let trunk = knives::ids::CommitId::new(lab.revision(&lab.second, "main", "commit_id").trim());
    let merge =
        knives::jj::create_merge(&lab.second, &[trunk, fetched], "release: with a foreign PR")
            .expect("merge");
    let parents = knives::jj::Repo::open(&lab.second)
        .expect("open")
        .parents_of(merge.as_str())
        .expect("parents");
    assert_eq!(parents.len(), 2);
}
