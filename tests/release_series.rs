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

use lab::{Lab, commit_at, knives_release, release_parents, release_test_home};

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
