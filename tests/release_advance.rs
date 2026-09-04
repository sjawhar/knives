//! `knives release advance`: a member's parent follows its branch.
//!
//! Named parents move first, then all, and a named advance claims nothing about
//! members it never read. A bare advance skips an ambiguous parent, a branch
//! that merges several members and a branch stacked on the release, and under
//! the fixed scheme ignores the release bookmark; `--from` recovers a branch
//! rebuilt with `jj duplicate`, requires exactly one branch and a commit that is
//! a parent. The trunk and the release are refused by name.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::jj::Repo;
use lab::{
    Lab, commit_at, extend_branch, file_at_revision, home_after_first_cut, knives_release,
    release_parents, release_test_home,
};

#[test]
fn advance_moves_only_the_named_parents_then_all() {
    // Given: both members have advanced. Advancing is a content change beyond
    // any single include, so it moves exactly what was named, and everything
    // only when nothing is.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let repo = Repo::open(&lab.work).expect("open");
    let old_alpha = repo.resolve_commit("feat/alpha").expect("old alpha");
    let old_beta = repo.resolve_commit("feat/beta").expect("old beta");
    extend_branch(&lab, "feat/alpha", "alpha.txt", "alpha\nmore\n");
    extend_branch(&lab, "feat/beta", "beta.txt", "beta\nmore\n");
    let repo = Repo::open(&lab.work).expect("reopen");
    let new_alpha = repo.resolve_commit("feat/alpha").expect("new alpha");
    let new_beta = repo.resolve_commit("feat/beta").expect("new beta");

    // When: only alpha is advanced by name.
    let output = knives_release(&lab, &home, &["advance", "feat/alpha"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");

    // Then: alpha moved, beta did not.
    let parents = release_parents(&lab, "release/2026-08-04");
    assert!(
        parents.contains(&new_alpha),
        "the named member did not reach its tip: {parents:?}"
    );
    assert!(
        !parents.contains(&old_alpha),
        "the named member's old parent stayed: {parents:?}"
    );
    assert!(
        parents.contains(&old_beta),
        "advance moved an unnamed member: {parents:?}"
    );
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-04", "alpha.txt"),
        "alpha\nmore\n"
    );
    assert!(
        stdout.contains("advanced feat/alpha"),
        "the delta must name what moved: {stdout}"
    );
    let after_named = parents.len();

    // And: a bare advance moves every member that has advanced.
    let output = knives_release(&lab, &home, &["advance"]);
    assert!(output.status.success(), "{output:?}");
    let parents = release_parents(&lab, "release/2026-08-04");
    assert!(
        parents.contains(&new_beta),
        "the bare advance left the advanced member behind: {parents:?}"
    );
    assert!(
        !parents.contains(&old_beta),
        "the bare advance kept the stale parent: {parents:?}"
    );
    assert_eq!(
        parents.len(),
        after_named,
        "a bare advance added or lost a parent: {parents:?}"
    );
    assert!(
        parents.contains(&new_alpha),
        "the bare advance moved the already-advanced member back: {parents:?}"
    );
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-04", "beta.txt"),
        "beta\nmore\n"
    );
}

#[test]
fn a_named_advance_claims_nothing_about_the_members_it_never_read() {
    // Given: one member already at its tip and one that has advanced. A named
    // advance looked only at what it was given, so "every member is at its
    // branch tip" is a fact it is not in a position to state.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    extend_branch(&lab, "feat/beta", "beta.txt", "beta\nmore\n");
    let before = release_parents(&lab, "release/2026-08-04");

    // When: only the member that has not moved is named.
    let output = knives_release(&lab, &home, &["advance", "feat/alpha"]);

    // Then: it reports alpha, stays quiet about beta, and moves nothing.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("feat/alpha is already at its tip"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("every member of"),
        "a named advance claimed the sweep only a bare advance performs: {stdout}"
    );
    assert_eq!(release_parents(&lab, "release/2026-08-04"), before);
}

#[test]
fn a_bare_advance_with_an_ambiguous_parent_changes_nothing() {
    // Given: one released parent with two advanced branches descending from it.
    // A bare advance promises every advanced member; advancing some while
    // skipping the ambiguous one would report success on a composition nobody
    // asked for.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let released = commit_at(&lab, "feat/alpha");
    lab.jj_work(["new", "feat/alpha", "-m", "alpha follow-up"]);
    std::fs::write(lab.work.join("alpha.txt"), "alpha\nmore\n").expect("extend alpha");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new", released.as_str(), "-m", "rival follow-up"]);
    std::fs::write(lab.work.join("rival.txt"), "rival\n").expect("write rival");
    lab.jj_work(["bookmark", "create", "feat/rival", "-r", "@"]);
    lab.jj_work(["new"]);
    let before = release_parents(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["advance"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(stdout.contains("nothing advanced"), "{stdout}");
    assert!(
        stdout.contains("several advanced branches")
            && stdout.contains("feat/alpha")
            && stdout.contains("feat/rival"),
        "the refusal must name the branches it could not choose between: {stdout}"
    );
    assert_eq!(
        release_parents(&lab, "release/2026-08-04"),
        before,
        "an ambiguous bare advance mutated the release"
    );
}

#[test]
fn a_bare_advance_skips_a_branch_that_merges_several_members() {
    // Given: two released members, and a third branch built by merging both of
    // their released tips directly -- the shape of an integration branch built
    // across several former members, or of a member rebuilt with `jj
    // duplicate`, whose new tip has no ancestry back to its own stale parent
    // but happens to leave some *other* branch still reachable from it. Its
    // current tip descends from both stale parents, so each parent's ancestry
    // search finds it as the sole successor; deduping that result would
    // silently fold two distinct members into one.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let old_alpha = commit_at(&lab, "feat/alpha");
    let old_beta = commit_at(&lab, "feat/beta");
    let before = release_parents(&lab, "release/2026-08-04");
    assert!(
        before.contains(&old_alpha) && before.contains(&old_beta),
        "{before:?}"
    );
    lab.jj_work([
        "new",
        "feat/alpha",
        "feat/beta",
        "-m",
        "consolidated across both",
    ]);
    std::fs::write(lab.work.join("consolidated.txt"), "consolidated\n")
        .expect("write consolidated content");
    lab.jj_work(["bookmark", "create", "feat/consolidated", "-r", "@"]);
    lab.jj_work(["new"]);

    let output = knives_release(&lab, &home, &["advance"]);

    // Then: a branch whose history carries a merge of two members is not a
    // candidate at all — it is stacked history, named as such — and nothing
    // moves. The release is left exactly as it was.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("feat/consolidated's history past the trunk carries 1 merge")
            && stdout.contains("rebase it off the trunk before advancing onto it"),
        "the stacked branch must be named and skipped: {stdout}"
    );
    assert_eq!(
        release_parents(&lab, "release/2026-08-04"),
        before,
        "an overreaching bare advance mutated the release"
    );
}

#[test]
fn advance_from_recovers_a_branch_rebuilt_with_jj_duplicate() {
    // Given: two released members, then feat/alpha rebuilt the way `jj
    // duplicate` rebuilds a branch onto a new base -- same content, a fresh
    // change id sharing no ancestry with the commit the release still carries.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let old_alpha = commit_at(&lab, "feat/alpha");
    let old_beta = commit_at(&lab, "feat/beta");
    lab.jj_work(["new", "main", "-m", "alpha rebuilt onto a new base"]);
    std::fs::write(lab.work.join("alpha.txt"), "alpha\n").expect("rebuild alpha content");
    lab.jj_work([
        "bookmark",
        "set",
        "feat/alpha",
        "-r",
        "@",
        "--allow-backwards",
    ]);
    lab.jj_work(["new"]);
    let rebuilt_alpha = commit_at(&lab, "feat/alpha");
    assert_ne!(
        rebuilt_alpha, old_alpha,
        "the rebuild must be a fresh commit"
    );

    // When: nothing can pair the rebuilt branch with its parent — no ancestry, no
    // shared change id, and (the cut's own record removed) no name on file — a
    // plain named advance is refused, not guessed at.
    std::fs::remove_dir_all(home.path().join("ledger")).expect("forget the cut record");
    let plain = knives_release(&lab, &home, &["advance", "feat/alpha"]);
    let plain_stdout = String::from_utf8_lossy(&plain.stdout);
    assert_eq!(plain.status.code(), Some(3), "{plain_stdout}");
    assert!(
        plain_stdout.contains("carries no parent of feat/alpha"),
        "{plain_stdout}"
    );

    // But naming the exact old parent it replaces succeeds.
    let output = knives_release(
        &lab,
        &home,
        &["advance", "feat/alpha", "--from", old_alpha.as_str()],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("advanced feat/alpha"), "{stdout}");
    let parents = release_parents(&lab, "release/2026-08-04");
    assert!(
        parents.contains(&rebuilt_alpha),
        "the rebuilt branch did not land: {parents:?}"
    );
    assert!(
        !parents.contains(&old_alpha),
        "the old parent stayed: {parents:?}"
    );
    assert!(
        parents.contains(&old_beta),
        "an unrelated member moved too: {parents:?}"
    );
    assert_eq!(parents.len(), 2, "{parents:?}");
}

#[test]
fn advance_from_requires_exactly_one_branch() {
    // Given: --from asserts one specific mapping. More than one named branch
    // makes that assertion ambiguous, so it is refused before touching
    // anything, not applied to the first branch and silently ignored for
    // the rest.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let old_alpha = commit_at(&lab, "feat/alpha");
    let before = release_parents(&lab, "release/2026-08-04");

    let output = knives_release(
        &lab,
        &home,
        &[
            "advance",
            "feat/alpha",
            "feat/beta",
            "--from",
            old_alpha.as_str(),
        ],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(2), "{stdout}");
    assert!(stdout.contains("give exactly one branch"), "{stdout}");
    assert_eq!(
        release_parents(&lab, "release/2026-08-04"),
        before,
        "a rejected --from still mutated the release"
    );
}

#[test]
fn advance_from_refuses_when_the_named_commit_is_not_a_parent() {
    // Given: feat/alpha rebuilt onto a new base (so it is not already at its
    // released tip), and --from naming a commit that never was a member --
    // not even its own true old parent.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let stray = commit_at(&lab, "feat/gamma");
    lab.jj_work(["new", "main", "-m", "alpha rebuilt onto a new base"]);
    std::fs::write(lab.work.join("alpha.txt"), "alpha\n").expect("rebuild alpha content");
    lab.jj_work([
        "bookmark",
        "set",
        "feat/alpha",
        "-r",
        "@",
        "--allow-backwards",
    ]);
    lab.jj_work(["new"]);
    let before = release_parents(&lab, "release/2026-08-04");

    let output = knives_release(
        &lab,
        &home,
        &["advance", "feat/alpha", "--from", stray.as_str()],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("is not a parent of release/2026-08-04"),
        "{stdout}"
    );
    assert!(
        stdout.contains("knives release include feat/alpha"),
        "{stdout}"
    );
    assert_eq!(
        release_parents(&lab, "release/2026-08-04"),
        before,
        "a refused --from still mutated the release"
    );
}

#[test]
fn a_bare_advance_under_the_fixed_scheme_ignores_the_release_bookmark() {
    // Given: a fixed release branch, whose own bookmark descends from every
    // member parent. A bare advance looks for bookmarks that moved past a
    // parent, so the release is the one bookmark it must never count as one:
    // "advancing" a member onto the release commit would make the release its
    // own member, and under this scheme the name never changes to reveal it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nrelease_branch = \"integration\"\n",
            lab.upstream.display(),
        ),
    )
    .expect("write fixed-scheme registry");
    let consumer = tempfile::tempdir().expect("create local consumer");
    std::fs::write(
        home.path().join("local-consumer"),
        consumer.path().display().to_string(),
    )
    .expect("write local consumer fixture path");
    let first = knives_release(&lab, &home, &["cut"]);
    let released = Repo::open(&lab.work)
        .expect("open first fixed cut")
        .resolve_commit("integration")
        .unwrap_or_else(|error| panic!("first fixed cut was not named: {error}\n{first:?}"));
    extend_branch(&lab, "feat/alpha", "alpha.txt", "alpha\nmore\n");
    let advanced_alpha = commit_at(&lab, "feat/alpha");

    let output = knives_release(&lab, &home, &["advance"]);

    // Then: the member moved to its branch tip, and the release bookmark was
    // not mistaken for a branch that carries it.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("advanced feat/alpha"), "{stdout}");
    let parents = release_parents(&lab, "integration");
    assert!(parents.contains(&advanced_alpha), "{parents:?}");
    assert!(
        !parents.contains(&released),
        "the release bookmark was treated as a member's successor: {parents:?}"
    );
    assert_eq!(
        file_at_revision(&lab, "integration", "alpha.txt"),
        "alpha\nmore\n"
    );
}

#[test]
fn a_bare_advance_ignores_a_branch_stacked_on_the_release() {
    // Given: a cut, and a branch somebody built on top of it — #4's third loss
    // mode's shape, and the reason `reap` refuses cuts with local descendants.
    // Such a branch descends from every member, so ancestry alone calls it their
    // advanced tip; advancing onto it would fold work nobody included into the
    // release and put the old cut in the new cut's own ancestry.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let before = release_parents(&lab, "release/2026-08-04");
    lab.jj_work(["new", "release/2026-08-04", "-m", "stacked on the cut"]);
    std::fs::write(lab.work.join("stacked.txt"), "stacked\n").expect("write stacked content");
    lab.jj_work(["bookmark", "create", "feat/stacked", "-r", "@"]);
    lab.jj_work(["new"]);

    let output = knives_release(&lab, &home, &["advance"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("every member of release/2026-08-04 is at its branch tip"),
        "a branch stacked on the release is not an advanced member: {stdout}"
    );
    assert_eq!(
        release_parents(&lab, "release/2026-08-04"),
        before,
        "the bare advance folded a stacked branch into the release"
    );

    // And: the plan says what the branch actually is, rather than advising the
    // verb that refuses it.
    let plan = knives_release(&lab, &home, &[]);
    let planned = String::from_utf8_lossy(&plan.stdout);
    assert!(
        planned.contains("feat/stacked is stacked on release/2026-08-04"),
        "the plan called a stacked branch an advanced member: {planned}"
    );
}

#[test]
fn advancing_a_named_stacked_branch_is_refused() {
    // Given: the same stacked branch, named outright. Honouring it would make
    // the release contain its own predecessor, so it is answered, not improvised.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let before = release_parents(&lab, "release/2026-08-04");
    lab.jj_work(["new", "release/2026-08-04", "-m", "stacked on the cut"]);
    std::fs::write(lab.work.join("stacked.txt"), "stacked\n").expect("write stacked content");
    lab.jj_work(["bookmark", "create", "feat/stacked", "-r", "@"]);
    lab.jj_work(["new"]);

    let output = knives_release(&lab, &home, &["advance", "feat/stacked"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("feat/stacked is stacked on release/2026-08-04"),
        "the refusal must say what is wrong with the branch: {stdout}"
    );
    assert_eq!(
        release_parents(&lab, "release/2026-08-04"),
        before,
        "the refusal still moved a parent"
    );
}

#[test]
fn advance_refuses_the_trunk_and_the_release_by_name() {
    // Given: a cut. A bare advance never treats the trunk or a release bookmark
    // as a branch that carries a member, and naming one must not reach the mover
    // it is kept away from: a member advanced onto the trunk gives the release a
    // second base, and one advanced onto a release makes it carry a whole cut.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let before = release_parents(&lab, "release/2026-08-04");

    for named in ["main", "release/2026-08-04"] {
        let output = knives_release(&lab, &home, &["advance", named]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(3), "{named}: {stdout}");
        assert!(
            stdout.contains("is the trunk or a release name"),
            "{named} must be refused as unadvanceable: {stdout}"
        );
        assert_eq!(
            release_parents(&lab, "release/2026-08-04"),
            before,
            "advancing {named} moved a parent"
        );
    }
}
