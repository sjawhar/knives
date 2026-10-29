//! `knives release` plan: stacked-history detection (field report #1).
//!
//! A member whose own history past the upstream trunk carries a merge — most
//! often a prior release cut — carries everything that merge carried, however
//! flat its own direct parent count looks. These exercise the real defect: a
//! branch built on top of an old release renders "flat" until this fires, and
//! a plain feature branch does not fire at all.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use lab::{Lab, commit_at, knives_release, release_parents, release_test_home};

#[test]
fn a_member_built_on_a_prior_release_is_reported_stacked() {
    // Given: an old dated release built from two branches, then a new branch
    // built directly on top of that old release — carrying its merge into
    // its own history — paired with a plain feature branch and cut as the
    // next release.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work(["new", "release/2026-08-04", "-m", "feat/gamma content"]);
    lab.jj_work(["bookmark", "create", "feat/gamma", "-r", "@"]);
    lab.jj_work(["new"]);
    lab.branch("feat/delta", "delta.txt", "delta\n");
    lab.octopus("release/2026-08-05", "feat/gamma", "feat/delta");
    let (home, _consumer) = release_test_home(&lab);

    // When: the next release is planned.
    let output = knives_release(&lab, &home, &[]);
    let text = String::from_utf8_lossy(&output.stdout).to_string();

    // Then: the stacked member is named, the release cut it carries is named,
    // and the headline says how many of the parents are stacked.
    assert!(
        text.contains("feat/gamma") && text.contains("carries 1 merge commit(s)"),
        "stacked history not reported: {text}"
    );
    assert!(
        text.contains("release/2026-08-04"),
        "the carried release merge is not named: {text}"
    );
    assert!(
        text.contains("3 parent(s), 1 stacked on a prior merge"),
        "stacked summary not reported: {text}"
    );
}

#[test]
fn linear_members_render_flat_with_no_stacked_history_finding() {
    // Given: a release built from two branches, each forked directly off
    // the trunk with no merge in their own history.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is planned.
    let output = knives_release(&lab, &home, &[]);
    let text = String::from_utf8_lossy(&output.stdout).to_string();

    // Then: no member carries a merge, so the release renders flat.
    assert!(
        text.contains("3 parent(s), flat"),
        "flat summary not reported: {text}"
    );
    assert!(
        !text.contains("stacked on a prior merge"),
        "an unstacked release should not report stacked history: {text}"
    );
}

#[test]
fn a_branch_forked_past_a_stale_upstream_view_is_not_stacked() {
    // Given: a release, then upstream advancing and merging a pull by merge
    // commit, neither fetched from upstream by the work checkout - its
    // `main@upstream` view is behind both - while the fork's trunk mirrors
    // upstream. A new branch forked from the fork's trunk has upstream's own
    // merge in its history past the stale upstream view, both of that merge's
    // parents beyond it, and nothing of its own but linear work.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        cut.status.success(),
        "{}",
        String::from_utf8_lossy(&cut.stdout)
    );
    lab.publish_pull("feat/alpha", 7);
    lab.advance_upstream_unfetched("upstream advance\n");
    lab.merge_pull_with_merge_commit_unfetched(7);
    lab.mirror_upstream_trunk_to_origin();
    lab.branch("feat/beta", "beta.txt", "beta\n");

    // When: the plan is read and the branch is included.
    let plan = knives_release(&lab, &home, &[]);
    let plan_text = String::from_utf8_lossy(&plan.stdout).to_string();
    let include = knives_release(&lab, &home, &["include", "feat/beta"]);
    let include_text = String::from_utf8_lossy(&include.stdout).to_string();

    // Then: upstream's own merge is not charged to the branch - the fork's
    // trunk reaches it - so the plan points at include and include takes it.
    assert!(
        !plan_text.contains("feat/beta's history"),
        "a flat branch read as stacked against a stale trunk view: {plan_text}"
    );
    assert!(
        plan_text.contains("feat/beta is not in release/2026-08-04"),
        "{plan_text}"
    );
    assert!(
        include.status.success() && include_text.contains("included feat/beta"),
        "include refused a flat branch: {include_text}\n{}",
        String::from_utf8_lossy(&include.stderr)
    );
}

#[test]
fn the_first_cut_refuses_a_branch_whose_history_carries_a_merge() {
    // Given: no release yet, two plain branches, and a third built by merging
    // both - the shape that would make the first cut carry everything that
    // merge carried while reading as a flat three-parent cut.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("feat/consolidated", "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);

    // When: the first cut is attempted.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    let text = String::from_utf8_lossy(&output.stdout).to_string();

    // Then: it refuses, names the stacked branch, and cuts nothing.
    assert_eq!(output.status.code(), Some(3), "{text}");
    assert!(
        text.contains("feat/consolidated's history past the trunk carries 1 merge commit(s)")
            && text.contains("rebase it off the trunk before cutting"),
        "the stacked branch must be named and refused: {text}"
    );
    let repo = knives::jj::Repo::open(lab.work_path()).expect("open");
    assert!(
        repo.resolve_commit("release/2026-08-04").is_err(),
        "the cut must not have been published"
    );
}

/// A first cut of `feat/alpha` and `feat/beta`, then `feat/gamma`: a merge of
/// `feat/alpha` and `other`, then gamma's own commit. Returns the lab, its
/// home, the merge and gamma's tip.
fn a_member_merging(
    other: &str,
) -> (
    Lab,
    tempfile::TempDir,
    knives::ids::CommitId,
    knives::ids::CommitId,
) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    if other == "feat/delta" {
        lab.branch("feat/delta", "delta.txt", "delta\n");
    }
    lab.jj_work([
        "new",
        "feat/alpha",
        other,
        "-m",
        "merge alpha and another line",
    ]);
    let merge = knives::ids::CommitId::new(lab.revision(&lab.work, "@", "commit_id"));
    lab.jj_work(["new", "-m", "feat/gamma"]);
    std::fs::write(lab.work.join("gamma.txt"), "gamma\n").expect("write gamma");
    lab.jj_work(["bookmark", "create", "feat/gamma", "-r", "@"]);
    lab.jj_work(["new", "main@origin"]);
    let gamma = commit_at(&lab, "feat/gamma");
    (lab, home, merge, gamma)
}

#[test]
fn a_member_whose_merge_joins_lines_the_release_carries_is_included() {
    // Given: feat/gamma, whose history starts with a merge of two members the
    // release already carries: everything it brings past the release is its own.
    let (lab, home, merge, gamma) = a_member_merging("feat/beta");
    let alpha = commit_at(&lab, "feat/alpha");
    let beta = commit_at(&lab, "feat/beta");

    // When: it is included.
    let output = knives_release(&lab, &home, &["include", "feat/gamma"]);

    // Then: it becomes a parent beside the members it builds on, and the
    // output names the merge.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "demo: feat/gamma's history carries merge(s) {} whose other lines \
             release/2026-08-04 already carries; it brings only its own 2 commit(s)\n",
            merge.short()
        )),
        "{stdout}"
    );
    let parents = release_parents(&lab, "release/2026-08-04");
    assert_eq!(parents.len(), 3, "{parents:?}");
    for member in [&alpha, &beta, &gamma] {
        assert!(parents.contains(member), "{parents:?}");
    }

    // And: the plan reports no stacked member.
    let plan = knives_release(&lab, &home, &[]);
    let text = String::from_utf8_lossy(&plan.stdout);
    assert!(text.contains("3 parent(s), flat"), "{text}");
    assert!(!text.contains("carries 1 merge commit(s)"), "{text}");
}

#[test]
fn a_member_whose_merge_brings_a_line_the_release_lacks_is_refused_with_its_commits() {
    // Given: feat/gamma, whose merge joins alpha with feat/delta, a branch the
    // release does not carry.
    let (lab, home, merge, gamma) = a_member_merging("feat/delta");
    let delta = commit_at(&lab, "feat/delta");
    let before = release_parents(&lab, "release/2026-08-04");

    // When: it is included.
    let output = knives_release(&lab, &home, &["include", "feat/gamma"]);

    // Then: refused, listing every commit it would bring that the release
    // lacks, and nothing changes.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "it brings 3 commit(s) the release does not carry: {}, {}, {}",
            gamma.short(),
            merge.short(),
            delta.short()
        )),
        "{stdout}"
    );
    assert_eq!(release_parents(&lab, "release/2026-08-04"), before);
}

#[test]
fn a_named_advance_takes_a_member_whose_merge_brings_an_older_commit_of_another_member() {
    // Given: a release of alpha and beta (two commits, b1 then b2), then
    // feat/alpha grown by a merge with b1 and one commit of its own: the merge
    // brings nothing the release lacks, and alpha still continues one member.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let b1 = commit_at(&lab, "feat/beta");
    lab::extend_branch(&lab, "feat/beta", "beta.txt", "beta\nmore\n");
    let b2 = commit_at(&lab, "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.jj_work([
        "new",
        "feat/alpha",
        b1.as_str(),
        "-m",
        "merge beta's first commit",
    ]);
    let merge = knives::ids::CommitId::new(lab.revision(&lab.work, "@", "commit_id"));
    lab.jj_work(["new", "-m", "alpha more"]);
    std::fs::write(lab.work.join("alpha.txt"), "alpha\nmore\n").expect("write alpha");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new", "main@origin"]);
    let alpha = commit_at(&lab, "feat/alpha");

    // When: it is advanced by name.
    let output = knives_release(&lab, &home, &["advance", "feat/alpha"]);

    // Then: the member moves, the merge is named, and the plan notes it
    // without calling the member stacked.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "demo: feat/alpha's history carries merge(s) {} whose other lines \
             release/2026-08-04 already carries; it brings only its own 3 commit(s)\n",
            merge.short()
        )),
        "{stdout}"
    );
    let parents = release_parents(&lab, "release/2026-08-04");
    assert_eq!(parents.len(), 2, "{parents:?}");
    assert!(
        parents.contains(&alpha) && parents.contains(&b2),
        "{parents:?}"
    );
    let plan = knives_release(&lab, &home, &[]);
    let text = String::from_utf8_lossy(&plan.stdout);
    assert!(text.contains("2 parent(s), flat"), "{text}");
    assert!(
        text.contains(&format!(
            "member feat/alpha carries merge(s) {} whose side lines the other members already \
             carry",
            merge.short()
        )),
        "{text}"
    );
}

#[test]
fn a_branch_built_on_an_earlier_write_of_the_release_is_refused() {
    // Given: fix/y built on the first cut, then the release advanced to a new
    // merge, so no ref names the commit fix/y rests on; the ledger still
    // records knives writing it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let first_write = commit_at(&lab, "release/2026-08-04");
    lab.jj_work(["new", "release/2026-08-04", "-m", "fix on the release"]);
    std::fs::write(lab.work.join("fix.txt"), "fix\n").expect("write fix");
    lab.jj_work(["bookmark", "create", "fix/y", "-r", "@"]);
    lab.jj_work(["new", "main@origin"]);
    lab::extend_branch(&lab, "feat/alpha", "alpha.txt", "alpha\nmore\n");
    let advanced = knives_release(&lab, &home, &["advance", "feat/alpha"]);
    assert!(advanced.status.success(), "{advanced:?}");
    assert_ne!(commit_at(&lab, "release/2026-08-04"), first_write);
    let before = release_parents(&lab, "release/2026-08-04");

    // When: fix/y is included.
    let output = knives_release(&lab, &home, &["include", "fix/y"]);

    // Then: refused as carrying a release merge, which it names.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "an earlier release write the ledger records ({})",
            first_write.short()
        )),
        "{stdout}"
    );
    assert_eq!(release_parents(&lab, "release/2026-08-04"), before);
}
