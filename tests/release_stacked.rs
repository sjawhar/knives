//! `knives release` plan: stacked-history detection (field report #1).
//!
//! A member whose own history past the upstream trunk carries a merge — most
//! often a prior release cut — carries everything that merge carried, however
//! flat its own direct parent count looks. These exercise the real defect: a
//! branch built on top of an old release renders "flat" until this fires, and
//! a plain feature branch does not fire at all. A fork that lands each branch
//! on its release, one merge of the release and the branch, builds the next
//! release of those landings: each holds the release cut the others hold too,
//! so only a landing that carries another line the release lacks is stacked.

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
             release/2026-08-04 already carries; it brings only its own 2 commit(s)\n",
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

#[test]
fn a_named_advance_refuses_a_branch_on_an_older_release_before_reading_its_parents() {
    // Given: two releases of alpha and beta, and fix/x built on the older one.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    lab.jj_work([
        "new",
        "release/2026-08-04",
        "-m",
        "fix on the older release",
    ]);
    std::fs::write(lab.work.join("fix.txt"), "fix\n").expect("write fix");
    lab.jj_work(["bookmark", "create", "fix/x", "-r", "@"]);
    lab.jj_work(["new", "main@origin"]);
    let (home, _consumer) = release_test_home(&lab);
    let before = release_parents(&lab, "release/2026-08-05");

    // When: it is advanced by name.
    let output = knives_release(&lab, &home, &["advance", "fix/x"]);

    // Then: refused as stacked on the older release, not sent to drop and
    // include, which would refuse it too.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("releases in that history: release/2026-08-04")
            && stdout.contains("rebase it off the trunk before advancing onto it"),
        "{stdout}"
    );
    assert!(!stdout.contains("drop and include"), "{stdout}");
    assert_eq!(release_parents(&lab, "release/2026-08-05"), before);
}

#[test]
fn a_branch_merging_one_member_is_sent_to_include_which_adds_it_beside_that_member() {
    // Given: feat/c, which merges member feat/beta into its own line.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.branch("feat/c", "c.txt", "c\n");
    lab.jj_work(["new", "feat/c", "feat/beta", "-m", "merge beta into c"]);
    lab.jj_work(["new", "-m", "c more"]);
    std::fs::write(lab.work.join("c.txt"), "c\nmore\n").expect("write c");
    lab.jj_work(["bookmark", "set", "feat/c", "-r", "@"]);
    lab.jj_work(["new", "main@origin"]);
    let tip = commit_at(&lab, "feat/c");
    let beta = commit_at(&lab, "feat/beta");

    // When: a bare advance, a named advance, then an include.
    let bare = knives_release(&lab, &home, &["advance"]);
    let named = knives_release(&lab, &home, &["advance", "feat/c"]);
    let included = knives_release(&lab, &home, &["include", "feat/c"]);

    // Then: both advances point at include, which adds it beside beta.
    let bare_stdout = String::from_utf8_lossy(&bare.stdout);
    assert!(
        bare_stdout.contains("a bare advance leaves it alone: include it by name"),
        "{bare_stdout}"
    );
    let named_stdout = String::from_utf8_lossy(&named.stdout);
    assert_eq!(named.status.code(), Some(3), "{named_stdout}");
    assert!(
        named_stdout.contains(&format!(
            "demo: feat/c merges 1 member(s) of release/2026-08-04 ({}) and builds on them; \
             `knives release include feat/c` adds it beside them\n",
            beta.short()
        )),
        "{named_stdout}"
    );
    let included_stdout = String::from_utf8_lossy(&included.stdout);
    assert!(included.status.success(), "{included_stdout}");
    let parents = release_parents(&lab, "release/2026-08-04");
    assert_eq!(parents.len(), 3, "{parents:?}");
    assert!(
        parents.contains(&tip) && parents.contains(&beta),
        "{parents:?}"
    );
}

#[test]
fn a_bare_advance_sends_a_merge_branch_that_continues_no_member_to_include() {
    // Given: beta with b1 then b2, a release carrying b2, and feat/n: a new
    // line merged with b1, then one more commit; it continues no member.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let b1 = commit_at(&lab, "feat/beta");
    lab::extend_branch(&lab, "feat/beta", "beta.txt", "beta\nmore\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.branch("feat/n", "n.txt", "n\n");
    lab.jj_work([
        "new",
        "feat/n",
        b1.as_str(),
        "-m",
        "merge beta's first commit",
    ]);
    lab.jj_work(["new", "-m", "n more"]);
    std::fs::write(lab.work.join("n.txt"), "n\nmore\n").expect("write n");
    lab.jj_work(["bookmark", "set", "feat/n", "-r", "@"]);
    lab.jj_work(["new", "main@origin"]);

    // When: a bare advance runs.
    let output = knives_release(&lab, &home, &["advance"]);

    // Then: it names include, the verb that takes the branch.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "demo: feat/n's history carries merges of lines release/2026-08-04 already carries; \
             a bare advance leaves it alone: include it by name\n"
        ),
        "{stdout}"
    );
}

#[test]
fn a_member_grown_by_merging_another_member_advances_in_place() {
    // Given: a release of alpha (a1) and beta (b2), then feat/alpha grown by a
    // merge with feat/beta and one commit of its own.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let beta = commit_at(&lab, "feat/beta");
    lab.jj_work([
        "new",
        "feat/alpha",
        "feat/beta",
        "-m",
        "merge beta into alpha",
    ]);
    lab.jj_work(["new", "-m", "alpha more"]);
    std::fs::write(lab.work.join("alpha.txt"), "alpha\nmore\n").expect("write alpha");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new", "main@origin"]);
    let alpha = commit_at(&lab, "feat/alpha");

    // When: a bare advance, an include, then a named advance.
    let bare = knives_release(&lab, &home, &["advance"]);
    let included = knives_release(&lab, &home, &["include", "feat/alpha"]);
    let advanced = knives_release(&lab, &home, &["advance", "feat/alpha"]);

    // Then: the first two point at advance, which replaces alpha's member and
    // keeps beta: exactly [alpha's tip, beta].
    let bare_stdout = String::from_utf8_lossy(&bare.stdout);
    assert!(
        bare_stdout.contains("a bare advance leaves it alone: advance it by name"),
        "{bare_stdout}"
    );
    let included_stdout = String::from_utf8_lossy(&included.stdout);
    assert_eq!(included.status.code(), Some(3), "{included_stdout}");
    assert!(
        included_stdout.contains("`knives release advance feat/alpha`"),
        "{included_stdout}"
    );
    let advanced_stdout = String::from_utf8_lossy(&advanced.stdout);
    assert!(advanced.status.success(), "{advanced_stdout}");
    let mut parents = release_parents(&lab, "release/2026-08-04");
    parents.sort();
    let mut expected = vec![alpha, beta];
    expected.sort();
    assert_eq!(parents, expected, "{advanced_stdout}");
}

#[test]
fn a_member_that_merged_its_own_old_tip_back_in_is_refused_as_stacked_everywhere() {
    // Given: a release of alpha (a1) and beta, then feat/alpha rebuilt as a new
    // commit on the trunk, merged with a1, plus one more commit.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let old = commit_at(&lab, "feat/alpha");
    lab.jj_work(["new", "main@origin", "-m", "alpha rebuilt"]);
    std::fs::write(lab.work.join("alpha2.txt"), "alpha2\n").expect("write alpha2");
    lab.jj_work(["new", "@", old.as_str(), "-m", "merge old alpha back in"]);
    lab.jj_work(["new", "-m", "alpha more"]);
    std::fs::write(lab.work.join("alpha.txt"), "alpha\nmore\n").expect("write alpha");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new", "main@origin"]);
    let before = release_parents(&lab, "release/2026-08-04");

    // When: a bare advance, an include, then a named advance.
    let runs = [
        knives_release(&lab, &home, &["advance"]),
        knives_release(&lab, &home, &["include", "feat/alpha"]),
        knives_release(&lab, &home, &["advance", "feat/alpha"]),
    ];

    // Then: each says it carries its old tip's line and to rebase it off the
    // trunk; none points at a verb that would refuse it; nothing moves.
    for run in &runs {
        let stdout = String::from_utf8_lossy(&run.stdout);
        assert!(
            stdout.contains("branch feat/alpha's history past the trunk carries")
                && stdout.contains("rebase it off the trunk before advancing onto it"),
            "{stdout}"
        );
        assert!(
            !stdout.contains("it by name") && !stdout.contains("`knives release advance"),
            "{stdout}"
        );
        // The old tip is a release parent, so it is not among what the branch
        // would bring.
        assert!(!stdout.contains(old.short()), "{stdout}");
    }
    assert_eq!(release_parents(&lab, "release/2026-08-04"), before);
}

/// `feat/<name>`, a branch off the trunk that writes `<name>.txt`, landed on
/// `on` as a fork lands a branch: one merge of `on` and the branch, under the
/// bookmark `land/<name>`.
fn land(lab: &Lab, name: &str, on: &str) {
    let branch = format!("feat/{name}");
    let landing = format!("land/{name}");
    let message = format!("land {branch} on {on}");
    lab.branch(&branch, &format!("{name}.txt"), &format!("{name}\n"));
    lab.jj_work(["new", on, branch.as_str(), "-m", message.as_str()]);
    lab.jj_work(["bookmark", "create", landing.as_str(), "-r", "@"]);
    lab.jj_work(["new", "main@origin"]);
}

/// `release/2026-08-04`, a release of `feat/alpha` and `feat/beta`, with each
/// of `names` landed on it ([`land`]).
fn landings_on_a_release(names: &[&str]) -> Lab {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    for name in names {
        land(&lab, name, "release/2026-08-04");
    }
    lab
}

/// The plan's standard output for the newest release of `lab`.
fn plan_text(lab: &Lab, home: &tempfile::TempDir) -> String {
    String::from_utf8_lossy(&knives_release(lab, home, &[]).stdout).into_owned()
}

/// The plan's verdict on `land/gamma` and `land/delta` as members of
/// `text`'s release: flat, and neither reported or noted.
fn assert_landings_flat(text: &str) {
    assert!(text.contains("3 parent(s), flat"), "{text}");
    for landing in ["land/gamma", "land/delta"] {
        assert!(!text.contains(&format!("{landing}'s history")), "{text}");
        assert!(!text.contains(&format!("member {landing}")), "{text}");
    }
}

#[test]
fn landings_on_a_release_the_other_members_carry_are_not_stacked() {
    // Given: gamma and delta each landed on release/2026-08-04, and the next
    // release cut of the two landings. Each landing holds that release's merge
    // by construction, and the other landing holds it too.
    let lab = landings_on_a_release(&["gamma", "delta"]);
    lab.octopus("release/2026-08-05", "land/gamma", "land/delta");
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is planned.
    let text = plan_text(&lab, &home);

    // Then: neither landing is reported, as stacked or as a note, and the
    // release reads flat.
    assert_landings_flat(&text);
}

#[test]
fn a_landing_built_on_another_landing_is_still_reported_stacked() {
    // Given: gamma and epsilon landed on release/2026-08-04, then delta landed
    // on gamma's landing instead of on the release, and the next release cut
    // of delta's and epsilon's landings: gamma's landing is no member of it.
    let lab = landings_on_a_release(&["gamma", "epsilon"]);
    land(&lab, "delta", "land/gamma");
    lab.octopus("release/2026-08-05", "land/delta", "land/epsilon");
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is planned.
    let text = plan_text(&lab, &home);

    // Then: delta's landing is reported stacked, and the release does not
    // read flat.
    assert!(text.contains("branch land/delta's history"), "{text}");
    assert!(text.contains("stacked on a prior merge"), "{text}");
    assert!(!text.contains("parent(s), flat"), "{text}");
}

#[test]
fn a_landing_built_on_another_landing_is_the_one_stacked_member_and_names_the_merge() {
    // Given: the same release: epsilon's landing on release/2026-08-04, and
    // delta's landing on gamma's landing.
    let lab = landings_on_a_release(&["gamma", "epsilon"]);
    land(&lab, "delta", "land/gamma");
    lab.octopus("release/2026-08-05", "land/delta", "land/epsilon");
    let (home, _consumer) = release_test_home(&lab);
    let delta_landing = commit_at(&lab, "land/delta");
    let mut brought = [
        delta_landing.short().to_owned(),
        commit_at(&lab, "land/gamma").short().to_owned(),
        commit_at(&lab, "feat/delta").short().to_owned(),
        commit_at(&lab, "feat/gamma").short().to_owned(),
    ];
    brought.sort();

    // When: the release is planned.
    let text = plan_text(&lab, &home);

    // Then: only delta's landing is stacked. Its finding names the merge that
    // joins gamma's landing to delta's line, and lists the four commits it
    // brings: that merge, gamma's landing, and the two lines' own commits.
    assert!(
        text.contains("3 parent(s), 1 stacked on a prior merge"),
        "{text}"
    );
    assert!(!text.contains("land/epsilon's history"), "{text}");
    assert!(
        text.contains(&format!(
            "branch land/delta's history holds 1 release cut(s) the other members already \
             carry, and past them 1 merge commit(s) joining lines none of them has ({})",
            delta_landing.short()
        )),
        "{text}"
    );
    let listed = text
        .lines()
        .find(|line| line.contains("branch land/delta's history"))
        .and_then(|line| line.split_once("it brings 4 commit(s) the release does not carry: "))
        .map_or_else(
            || panic!("no list of the commits land/delta brings: {text}"),
            |(_, listed)| listed,
        );
    let mut listed: Vec<&str> = listed.split(", ").collect();
    listed.sort_unstable();
    assert_eq!(listed, brought, "{text}");
}

#[test]
fn include_takes_a_landing_on_a_release_the_members_carry() {
    // Given: the release of gamma's and delta's landings on
    // release/2026-08-04, then epsilon landed on that release too.
    let lab = landings_on_a_release(&["gamma", "delta", "epsilon"]);
    lab.octopus("release/2026-08-05", "land/gamma", "land/delta");
    let (home, _consumer) = release_test_home(&lab);
    let epsilon = commit_at(&lab, "land/epsilon");

    // When: epsilon's landing is included.
    let output = knives_release(&lab, &home, &["include", "land/epsilon"]);

    // Then: it is taken as a landing, not refused as stacked, and becomes a
    // parent of the release.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(
            "demo: land/epsilon lands on 1 release cut(s) whose lines release/2026-08-05 \
             already carries; it brings only its own 2 commit(s)\n"
        ),
        "{stdout}"
    );
    assert!(
        release_parents(&lab, "release/2026-08-05").contains(&epsilon),
        "{stdout}"
    );
}

#[test]
fn a_release_cut_that_is_a_member_beside_a_landing_on_it_is_reported_stacked() {
    // Given: gamma landed on release/2026-08-04, then the next release cut of
    // release/2026-08-04 itself and that landing: the shape a release read as
    // a member takes. The landing carries the cut, and the cut carries itself.
    let lab = landings_on_a_release(&["gamma"]);
    lab.octopus("release/2026-08-05", "release/2026-08-04", "land/gamma");
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is planned.
    let text = plan_text(&lab, &home);

    // Then: the release cut is reported stacked and named, as before the
    // landing rule; the landing on it reads as a landing.
    assert!(
        text.contains(
            "branch release/2026-08-04's history past the trunk carries 1 merge commit(s)"
        ) && text.contains("releases in that history: release/2026-08-04"),
        "{text}"
    );
    assert!(
        text.contains("3 parent(s), 1 stacked on a prior merge"),
        "{text}"
    );
    assert!(!text.contains("land/gamma's history"), "{text}");
}

#[test]
fn landings_on_an_earlier_write_no_ref_names_are_not_stacked() {
    // Given: release/2026-08-04 cut by knives, gamma and delta landed on that
    // write, then feat/alpha advanced, so the name holds a new write and only
    // the ledger records the one the landings rest on; then the next release
    // cut of the two landings.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let first_write = commit_at(&lab, "release/2026-08-04");
    land(&lab, "gamma", "release/2026-08-04");
    land(&lab, "delta", "release/2026-08-04");
    lab::extend_branch(&lab, "feat/alpha", "alpha.txt", "alpha\nmore\n");
    let advanced = knives_release(&lab, &home, &["advance", "feat/alpha"]);
    assert!(advanced.status.success(), "{advanced:?}");
    assert_ne!(commit_at(&lab, "release/2026-08-04"), first_write);
    lab.octopus("release/2026-08-05", "land/gamma", "land/delta");

    // When: the release is planned.
    let text = plan_text(&lab, &home);

    // Then: each landing's earlier write, which the ledger alone names, is
    // carried by the other landing, so neither is stacked.
    assert_landings_flat(&text);
}

#[test]
fn landings_on_a_release_whose_member_upstream_merged_are_not_stacked() {
    // Given: gamma and delta landed on release/2026-08-04, the next release
    // cut of the two landings, then upstream merging feat/alpha by merge
    // commit: release/2026-08-04 now has one parent past the trunk,
    // feat/beta, and is still the release each landing holds.
    let lab = landings_on_a_release(&["gamma", "delta"]);
    lab.octopus("release/2026-08-05", "land/gamma", "land/delta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);

    // When: the release is planned.
    let text = plan_text(&lab, &home);

    // Then: neither landing is stacked.
    assert_landings_flat(&text);
}
