//! Named release series: one fork carrying dated releases on more than one base.
//!
//! The default series is the entry's own `base` and cuts `release/<date>`; a
//! series declared under `[repos.<name>.series.<series>]` has its own base and
//! cuts `release/<series>/<date>`. A branch belongs to the series whose base it
//! forks from latest; a cut composes only its own series' branches, and the
//! edit verbs refuse a branch of another series or of none.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use lab::{Lab, commit_at, git_output, knives_release, release_parents, release_test_home};

/// Whether `revision` resolves in the lab's work checkout.
fn resolves(lab: &Lab, revision: &str) -> bool {
    knives::jj::Repo::open(lab.work_path())
        .expect("open work checkout")
        .resolve_commit(revision)
        .is_ok()
}

/// A lab whose upstream carries a second base, `old`, split from `main`: the
/// seed is their common ancestor, and each has moved on since.
fn two_base_lab() -> Lab {
    let lab = Lab::new();
    lab.upstream_branch("old", "old.txt", "old base\n");
    lab.advance_upstream("new base\n");
    lab
}

/// A branch forked from `base`, with its bookmark on its one commit.
fn branch_on(lab: &Lab, base: &str, name: &str, file: &str) {
    lab.jj_work(["new", "-r", base, "-m", name]);
    std::fs::write(lab.work_path().join(file), format!("{name}\n")).expect("write branch");
    lab.jj_work(["bookmark", "create", name, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// The release-test home with the registry entry given a series `old` on the
/// upstream branch `old`, plus whatever `extra` the test adds to that table.
fn series_home(lab: &Lab, extra: &str) -> tempfile::TempDir {
    let (home, _consumer) = release_test_home(lab);
    let registry = home.path().join("repos.toml");
    let mut text = std::fs::read_to_string(&registry).expect("read registry");
    text.push_str("\n[repos.demo.series.old]\nbase = \"old\"\n");
    text.push_str(extra);
    std::fs::write(&registry, text).expect("write registry");
    home
}

fn text(output: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn a_series_first_cut_composes_only_its_own_branches() {
    // Given: one branch on each base.
    let lab = two_base_lab();
    branch_on(&lab, "main@upstream", "feat/new", "new.txt");
    branch_on(&lab, "old@upstream", "fix/old", "old-fix.txt");
    let home = series_home(&lab, "");

    // When: series `old` cuts its first release.
    let cut = knives_release(
        &lab,
        &home,
        &["--series", "old", "cut", "release/old/2026-08-04"],
    );

    // Then: it carries the `old` branch alone.
    assert!(cut.status.success(), "{}", text(&cut));
    assert_eq!(
        release_parents(&lab, "release/old/2026-08-04"),
        vec![commit_at(&lab, "fix/old")]
    );
}

#[test]
fn a_default_series_cut_is_unaffected_by_a_series_cut() {
    // Given: series `old` has cut, and newer-dated than any default cut.
    let lab = two_base_lab();
    branch_on(&lab, "main@upstream", "feat/new", "new.txt");
    branch_on(&lab, "old@upstream", "fix/old", "old-fix.txt");
    let home = series_home(&lab, "");
    let series_cut = knives_release(
        &lab,
        &home,
        &["--series", "old", "cut", "release/old/2026-08-09"],
    );
    assert!(series_cut.status.success(), "{}", text(&series_cut));

    // When: the default series cuts.
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);

    // Then: it carries only the default series' branch, and the series cut
    // is still where it was.
    assert!(cut.status.success(), "{}", text(&cut));
    assert_eq!(
        release_parents(&lab, "release/2026-08-04"),
        vec![commit_at(&lab, "feat/new")]
    );
    assert_eq!(
        release_parents(&lab, "release/old/2026-08-09"),
        vec![commit_at(&lab, "fix/old")]
    );
}

#[test]
fn a_branch_that_fits_no_series_is_refused_with_the_reason() {
    // Given: a branch forked from the seed, where the two bases split: it
    // forks from both at the same commit.
    let lab = two_base_lab();
    branch_on(&lab, "old@upstream", "fix/old", "old-fix.txt");
    lab.branch("feat/stray", "stray.txt", "stray\n");
    let home = series_home(&lab, "");

    // When: series `old` cuts.
    let cut = knives_release(
        &lab,
        &home,
        &["--series", "old", "cut", "release/old/2026-08-04"],
    );

    // Then: the cut refuses, naming the branch and why.
    let out = text(&cut);
    assert!(!cut.status.success(), "{out}");
    assert!(
        out.contains("feat/stray belongs to no release series"),
        "{out}"
    );
}

#[test]
fn including_another_series_branch_is_refused() {
    // Given: series `old` has cut, and a default-series branch exists.
    let lab = two_base_lab();
    branch_on(&lab, "old@upstream", "fix/old", "old-fix.txt");
    branch_on(&lab, "main@upstream", "feat/new", "new.txt");
    let home = series_home(&lab, "");
    let cut = knives_release(
        &lab,
        &home,
        &["--series", "old", "cut", "release/old/2026-08-04"],
    );
    assert!(cut.status.success(), "{}", text(&cut));

    // When: the default-series branch is included in the `old` release.
    let include = knives_release(
        &lab,
        &home,
        &["--series", "old", "include", "feat/new", "--why", "test"],
    );

    // Then: it is refused, and the release keeps its one member.
    let out = text(&include);
    assert!(!include.status.success(), "{out}");
    assert!(
        out.contains("feat/new forks from the base of the default series"),
        "{out}"
    );
    assert_eq!(
        release_parents(&lab, "release/old/2026-08-04"),
        vec![commit_at(&lab, "fix/old")]
    );
}

#[test]
fn advancing_onto_another_series_branch_is_refused() {
    // Given: series `old` has cut, and a default-series branch exists.
    let lab = two_base_lab();
    branch_on(&lab, "old@upstream", "fix/old", "old-fix.txt");
    branch_on(&lab, "main@upstream", "feat/new", "new.txt");
    let home = series_home(&lab, "");
    let cut = knives_release(
        &lab,
        &home,
        &["--series", "old", "cut", "release/old/2026-08-04"],
    );
    assert!(cut.status.success(), "{}", text(&cut));

    // When: the `old` release is asked to advance onto the default-series branch.
    let advance = knives_release(&lab, &home, &["--series", "old", "advance", "feat/new"]);

    // Then: it is refused, naming the series the branch fits.
    let out = text(&advance);
    assert!(!advance.status.success(), "{out}");
    assert!(
        out.contains("feat/new forks from the base of the default series"),
        "{out}"
    );
    assert_eq!(
        release_parents(&lab, "release/old/2026-08-04"),
        vec![commit_at(&lab, "fix/old")]
    );
}

#[test]
fn audit_names_a_branch_that_fits_no_series() {
    // Given: a branch forked where the two bases split.
    let lab = two_base_lab();
    lab.branch("feat/stray", "stray.txt", "stray\n");
    // Audit reads live refs, so the registry names the lab's real origin.
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"{}\"\n\n\
             [repos.demo.series.old]\nbase = \"old\"\n",
            lab.upstream.display(),
            lab.temp_origin().display(),
        ),
    )
    .expect("write registry");

    // When: the fork is audited.
    let audit = lab::knives(&lab, &home, &["--text", "audit", "demo", "--no-github"]);

    // Then: the report names it and why.
    let out = text(&audit);
    assert!(
        out.contains("feat/stray belongs to no release series"),
        "{out}"
    );
}

#[test]
fn an_unknown_series_is_a_usage_error_naming_the_known_ones() {
    let lab = two_base_lab();
    let home = series_home(&lab, "");

    let output = knives_release(&lab, &home, &["--series", "9.9"]);

    let out = text(&output);
    assert_eq!(output.status.code(), Some(2), "{out}");
    assert!(
        out.contains("no release series \"9.9\": this entry has the default series and old"),
        "{out}"
    );
}

#[test]
fn a_series_base_shared_with_another_series_is_rejected() {
    let lab = two_base_lab();
    let home = series_home(&lab, "\n[repos.demo.series.again]\nbase = \"old\"\n");

    let output = knives_release(&lab, &home, &[]);

    let out = text(&output);
    assert!(!output.status.success(), "{out}");
    assert!(out.contains("is also the base of series"), "{out}");
}

#[test]
fn a_series_named_like_a_date_is_rejected() {
    let lab = two_base_lab();
    let (home, _consumer) = release_test_home(&lab);
    let registry = home.path().join("repos.toml");
    let mut config = std::fs::read_to_string(&registry).expect("read registry");
    config.push_str("\n[repos.demo.series.\"2026-08-04\"]\nbase = \"old\"\n");
    std::fs::write(&registry, config).expect("write registry");

    let output = knives_release(&lab, &home, &[]);

    let out = text(&output);
    assert!(!output.status.success(), "{out}");
    assert!(out.contains("is shaped like a dated cut"), "{out}");
}

#[test]
fn a_series_base_in_the_release_namespace_is_rejected() {
    let lab = two_base_lab();
    let (home, _consumer) = release_test_home(&lab);
    let registry = home.path().join("repos.toml");
    let mut config = std::fs::read_to_string(&registry).expect("read registry");
    config.push_str("\n[repos.demo.series.old]\nbase = \"release/old\"\n");
    std::fs::write(&registry, config).expect("write registry");

    let output = knives_release(&lab, &home, &[]);

    let out = text(&output);
    assert!(!output.status.success(), "{out}");
    assert!(
        out.contains("sits in the dated release/ namespace"),
        "{out}"
    );
}

/// The release-test home with `series_table` appended verbatim to the registry.
fn home_with(lab: &Lab, series_table: &str) -> tempfile::TempDir {
    let (home, _consumer) = release_test_home(lab);
    let registry = home.path().join("repos.toml");
    let mut config = std::fs::read_to_string(&registry).expect("read registry");
    config.push_str(series_table);
    std::fs::write(&registry, config).expect("write registry");
    home
}

/// Push upstream's `old` to origin as `old` and fetch it: the rocq shape, a
/// series base kept on the fork.
fn old_base_on_origin(lab: &Lab) {
    let origin = lab.temp_origin().display().to_string();
    git_output(&lab.maintainer, ["push", origin.as_str(), "old:old"]);
    lab.fetch_work();
}

#[test]
fn an_unresolved_series_base_refuses_every_release_verb() {
    // Given: series `old` says its base is on origin, but `old` exists only on
    // upstream, so `old@origin` does not resolve. One branch on each base.
    let lab = two_base_lab();
    branch_on(&lab, "main@upstream", "feat/new", "new.txt");
    branch_on(&lab, "old@upstream", "fix/old", "old-fix.txt");
    let home = home_with(
        &lab,
        "\n[repos.demo.series.old]\nbase = \"old\"\nbase_remote = \"origin\"\n",
    );

    // When: the default series plans and cuts.
    let plan = knives_release(&lab, &home, &[]);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);

    // Then: both refuse, naming the base, and no cut exists: the `old` branch
    // never reaches the default release by elimination.
    for output in [&plan, &cut] {
        let out = text(output);
        assert_eq!(output.status.code(), Some(3), "{out}");
        assert!(
            out.contains("release series base old@origin does not resolve"),
            "{out}"
        );
    }
    assert!(!resolves(&lab, "release/2026-08-04"), "no cut should exist");
}

#[test]
fn a_series_base_kept_on_origin_keeps_each_series_to_its_own_branches() {
    // Given: the rocq shape: series `old`'s base lives on origin.
    let lab = two_base_lab();
    old_base_on_origin(&lab);
    branch_on(&lab, "main@upstream", "feat/new", "new.txt");
    branch_on(&lab, "old@origin", "fix/old", "old-fix.txt");
    let home = home_with(
        &lab,
        "\n[repos.demo.series.old]\nbase = \"old\"\nbase_remote = \"origin\"\n",
    );

    // When: each series cuts.
    let series_cut = knives_release(
        &lab,
        &home,
        &["--series", "old", "cut", "release/old/2026-08-04"],
    );
    let default_cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);

    // Then: each carries only its own branch.
    assert!(series_cut.status.success(), "{}", text(&series_cut));
    assert!(default_cut.status.success(), "{}", text(&default_cut));
    assert_eq!(
        release_parents(&lab, "release/old/2026-08-04"),
        vec![commit_at(&lab, "fix/old")]
    );
    assert_eq!(
        release_parents(&lab, "release/2026-08-04"),
        vec![commit_at(&lab, "feat/new")]
    );
}

#[test]
fn a_fixed_release_branch_entry_still_reaps_its_leftover_dated_cuts() {
    // Given: a fixed-scheme entry with no series, holding two dated cuts from
    // before it moved to a fixed branch.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    let home = home_with(&lab, "");
    let registry = home.path().join("repos.toml");
    let config = std::fs::read_to_string(&registry)
        .expect("read registry")
        .replacen(
            "[repos.demo]\n",
            "[repos.demo]\nrelease_branch = \"integration\"\n",
            1,
        );
    std::fs::write(&registry, config).expect("write registry");

    // When: it reaps.
    let reap = knives_release(&lab, &home, &["reap"]);

    // Then: the superseded dated cut goes, as it always has.
    let out = text(&reap);
    assert!(out.contains("reaped release/2026-08-04"), "{out}");
    assert!(
        !resolves(&lab, "release/2026-08-04"),
        "release/2026-08-04 survived"
    );
}

#[test]
fn series_cannot_combine_with_a_fixed_release_branch() {
    let lab = two_base_lab();
    let home = home_with(&lab, "\n[repos.demo.series.old]\nbase = \"old\"\n");
    let registry = home.path().join("repos.toml");
    let config = std::fs::read_to_string(&registry)
        .expect("read registry")
        .replacen(
            "[repos.demo]\n",
            "[repos.demo]\nrelease_branch = \"integration\"\n",
            1,
        );
    std::fs::write(&registry, config).expect("write registry");

    let output = knives_release(&lab, &home, &[]);

    let out = text(&output);
    assert!(!output.status.success(), "{out}");
    assert!(out.contains("and [series] do not combine"), "{out}");
}

#[test]
fn an_unknown_base_remote_is_rejected() {
    let lab = two_base_lab();
    let home = home_with(
        &lab,
        "\n[repos.demo.series.old]\nbase = \"old\"\nbase_remote = \"release\"\n",
    );

    let output = knives_release(&lab, &home, &[]);

    let out = text(&output);
    assert!(!output.status.success(), "{out}");
    assert!(out.contains("base_remote"), "{out}");
}

#[test]
fn a_series_name_git_cannot_hold_as_a_ref_is_rejected() {
    for name in [
        "a b", "a~1", "a:b", "a..b", "a^", "a?", "a*", "a[b", "a\\\\b", "a@{1",
    ] {
        let lab = Lab::new();
        let home = home_with(
            &lab,
            &format!("\n[repos.demo.series.\"{name}\"]\nbase = \"old\"\n"),
        );

        let output = knives_release(&lab, &home, &[]);

        let out = text(&output);
        assert!(!output.status.success(), "{name}: {out}");
        assert!(out.contains("cannot name a git ref"), "{name}: {out}");
    }
}

#[test]
fn a_default_cut_named_like_a_series_prefix_is_refused() {
    // Given: series `old`, whose cuts live under release/old/.
    let lab = two_base_lab();
    branch_on(&lab, "main@upstream", "feat/new", "new.txt");
    let home = series_home(&lab, "");

    // When: the default series is asked to cut `release/old`.
    let cut = knives_release(&lab, &home, &["cut", "release/old"]);

    // Then: refused as a usage error: git cannot hold release/old beside
    // release/old/<date>.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(2), "{out}");
    assert!(out.contains("is the prefix of series old's cuts"), "{out}");
}

#[test]
fn a_criss_cross_branch_fits_no_series() {
    // Given: two commits `a` and `b` off the seed, a series base kept on
    // origin that merges them, and a branch that merges them separately: the
    // branch and that base have two fork points, `a` and `b`, not one.
    let lab = Lab::new();
    let seed = commit_at(&lab, "main@origin");
    lab.jj_work(["new", "-r", seed.as_str(), "-m", "a"]);
    std::fs::write(lab.work_path().join("a.txt"), "a\n").expect("write a");
    lab.jj_work(["bookmark", "create", "cross-a", "-r", "@"]);
    lab.jj_work(["new", "-r", seed.as_str(), "-m", "b"]);
    std::fs::write(lab.work_path().join("b.txt"), "b\n").expect("write b");
    lab.jj_work(["bookmark", "create", "cross-b", "-r", "@"]);
    lab.jj_work(["new", "-r", "cross-a", "-r", "cross-b", "-m", "cross base"]);
    lab.jj_work(["bookmark", "create", "cross", "-r", "@"]);
    lab.jj_work(["new", "-r", "cross-a", "-r", "cross-b", "-m", "feat/x"]);
    std::fs::write(lab.work_path().join("x.txt"), "x\n").expect("write x");
    lab.jj_work(["bookmark", "create", "feat/x", "-r", "@"]);
    lab.jj_work(["new"]);
    lab.jj_work(["bookmark", "delete", "cross-a", "cross-b"]);
    git_output(
        &lab.work,
        [
            "push",
            lab.temp_origin().to_str().expect("utf-8 path"),
            "cross:cross",
        ],
    );
    lab.fetch_work();
    let home = home_with(
        &lab,
        "\n[repos.demo.series.cross]\nbase = \"cross\"\nbase_remote = \"origin\"\n",
    );

    // When: the default series cuts.
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);

    // Then: it refuses, naming the branch and the missing single fork point.
    let out = text(&cut);
    assert!(!cut.status.success(), "{out}");
    assert!(out.contains("feat/x belongs to no release series"), "{out}");
    assert!(
        out.contains("no single fork point with cross@origin"),
        "{out}"
    );
}

#[test]
fn a_default_branch_whose_history_holds_the_other_base_still_fits_the_default() {
    // Given: upstream merged the `old` base into main and moved on, so `old`'s
    // tip is an ancestor of main's, and a branch off main carries it.
    let lab = two_base_lab();
    git_output(
        &lab.maintainer,
        ["merge", "--no-ff", "old", "-m", "merge old"],
    );
    lab.advance_upstream("after the merge\n");
    branch_on(&lab, "main@upstream", "feat/new", "new.txt");
    let home = series_home(&lab, "");

    // When: the default series cuts.
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);

    // Then: the branch fits the default series, whose fork point strictly
    // descends from the `old` one.
    assert!(cut.status.success(), "{}", text(&cut));
    assert_eq!(
        release_parents(&lab, "release/2026-08-04"),
        vec![commit_at(&lab, "feat/new")]
    );

    // And: with one base an ancestor of the other, a branch forked at the older
    // base's tip ties and fits no series.
    branch_on(&lab, "old@upstream", "fix/old", "old-fix.txt");
    let plan = knives_release(&lab, &home, &[]);
    let out = text(&plan);
    assert!(
        out.contains("fix/old belongs to no release series"),
        "{out}"
    );
}

#[test]
fn a_plain_entry_does_not_count_a_multi_segment_release_ref_as_a_cut() {
    // Given: an entry with no series, a dated cut, and a `release/x/y` ref,
    // which on main sorted after every date and became the newest release.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/x/y", "feat/alpha", "feat/beta");
    let home = home_with(&lab, "");

    // When: it plans.
    let plan = knives_release(&lab, &home, &[]);

    // Then: the release in hand is the dated cut.
    let out = text(&plan);
    assert!(out.contains("demo: release/2026-08-04"), "{out}");
    assert!(!out.contains("demo: release/x/y"), "{out}");
}
