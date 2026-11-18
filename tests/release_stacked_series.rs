//! A stacked release series: `stacked = true` on a series table.
//!
//! The next release of such a series is one merge of its previous release
//! and its new members. The previous release is the first parent, and it is
//! never a member. A member may stack on it, and `members --verify` measures
//! each member from its fork point with it. A release made as a chain of
//! merges, one member each, reads as its previous release and the side
//! parents on its first-parent line. A series without the key keeps today's
//! rules: a branch built on the previous release is refused.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use lab::{Lab, commit_at, knives_release, release_test_home};

/// The series' first cut: the previous release of the release in hand.
const PREVIOUS: &str = "release/old/2026-08-04";

/// The series' next release, made by hand on [`PREVIOUS`], or cut.
const NEXT: &str = "release/old/2026-08-05";

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

/// Series `old`, with `stacked = true` when `stacked`: its first cut
/// [`PREVIOUS`] of `fix/a` and `fix/b`, pushed to origin and marked published
/// by a `record: published` note, and three branches on that cut: `del`
/// deletes `a.txt`, which the cut carries, and `m6` and `m5` each add a file.
fn previous_release_lab(stacked: bool) -> (Lab, tempfile::TempDir) {
    let lab = two_base_lab();
    branch_on(&lab, "old@upstream", "fix/a", "a.txt");
    branch_on(&lab, "old@upstream", "fix/b", "b.txt");
    let home = series_home(&lab, if stacked { "stacked = true\n" } else { "" });
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", PREVIOUS]);
    assert!(cut.status.success(), "{}", text(&cut));
    lab.push_branch(PREVIOUS);
    lab.fetch_work();
    let commit = commit_at(&lab, PREVIOUS);
    let noted = lab::knives(
        &lab,
        &home,
        &[
            "notch",
            PREVIOUS,
            "-m",
            "record: published as v1.0.0",
            "--evidence",
            commit.as_str(),
            "--repo",
            "demo",
        ],
    );
    assert!(noted.status.success(), "{}", text(&noted));
    lab.jj_work(["new", "-r", PREVIOUS, "-m", "del"]);
    std::fs::remove_file(lab.work_path().join("a.txt")).expect("delete a file the cut carries");
    lab.jj_work(["bookmark", "create", "del", "-r", "@"]);
    lab.jj_work(["new"]);
    branch_on(&lab, PREVIOUS, "m6", "m6.txt");
    branch_on(&lab, PREVIOUS, "m5", "m5.txt");
    (lab, home)
}

/// [`NEXT`] made by hand as one merge of [`PREVIOUS`] and the three
/// branches on it: the shape a stacked cut of them has.
fn flat_next(lab: &Lab) {
    lab.jj_work([
        "new", "-r", PREVIOUS, "-r", "del", "-r", "m6", "-r", "m5", "-m", NEXT,
    ]);
    lab.jj_work(["bookmark", "create", NEXT, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// [`NEXT`] made by hand as the compose script makes a release: the chain
/// `merge(merge(merge(PREVIOUS, del), m6), m5)`.
fn chain_next(lab: &Lab) {
    lab.jj_work(["new", "-r", PREVIOUS, "-r", "del", "-m", "compose del"]);
    lab.jj_work(["new", "-r", "@", "-r", "m6", "-m", "compose m6"]);
    lab.jj_work(["new", "-r", "@", "-r", "m5", "-m", NEXT]);
    lab.jj_work(["bookmark", "create", NEXT, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// The row `members` prints for `branch`'s commit, held by its bookmark.
fn member_row(lab: &Lab, branch: &str) -> String {
    format!("- {} {branch}", commit_at(lab, branch).short())
}

/// The row `members` prints for the previous release.
fn previous_row(lab: &Lab) -> String {
    format!(
        "- {} {PREVIOUS} previous release (not a member)",
        commit_at(lab, PREVIOUS).short()
    )
}

#[test]
fn the_plan_of_a_stacked_release_reports_no_stacked_member_and_exits_0() {
    // Given: a stacked series whose release in hand is one merge of its
    // previous release and three branches built on that release.
    let (lab, home) = previous_release_lab(true);
    flat_next(&lab);

    // When: the series plans.
    let plan = knives_release(&lab, &home, &["--series", "old"]);

    // Then: the previous release has a row of its own, no member reads as
    // stacked on a prior merge or is told to rebase off the trunk, and the
    // plan exits 0.
    let out = text(&plan);
    assert_eq!(plan.status.code(), Some(0), "{out}");
    assert!(out.contains(&format!("demo: {NEXT}")), "{out}");
    assert!(out.contains("4 parent(s), flat"), "{out}");
    assert!(
        out.contains(&format!(
            "    {}  {PREVIOUS} previous release (not a member)",
            commit_at(&lab, PREVIOUS).short()
        )),
        "{out}"
    );
    assert!(!out.contains("stacked on a prior merge"), "{out}");
    assert!(!out.contains("rebase it off the trunk"), "{out}");
    assert!(
        out.contains("every parent is still its branch tip"),
        "{out}"
    );
}

#[test]
fn members_verify_reads_a_stacked_release_with_no_missing_member() {
    // Given: the same stacked release, whose member `del` deletes a file the
    // previous release carries.
    let (lab, home) = previous_release_lab(true);
    flat_next(&lab);

    // When: its members' content is replayed onto it.
    let members = knives_release(&lab, &home, &["--series", "old", "members", "--verify"]);

    // Then: the previous release is no member, each member replays empty
    // from its fork point with that release, and nothing is missing or
    // unexplained.
    let out = text(&members);
    assert_eq!(members.status.code(), Some(0), "{out}");
    assert!(out.contains(&previous_row(&lab)), "{out}");
    for branch in ["del", "m6", "m5"] {
        assert!(out.contains(&member_row(&lab, branch)), "{out}");
        assert!(
            out.contains(&format!(
                "  {branch}@{}: carried (replay empty)",
                commit_at(&lab, branch).short()
            )),
            "{out}"
        );
    }
    assert!(!out.contains("!!"), "{out}");
}

#[test]
fn members_verify_reads_a_chain_shaped_release_as_its_previous_release_and_side_parents() {
    // Given: the next release made as a chain of merges, one member each,
    // as the compose script makes it.
    let (lab, home) = previous_release_lab(true);
    chain_next(&lab);

    // When: its members' content is replayed onto it.
    let members = knives_release(&lab, &home, &["--series", "old", "members", "--verify"]);

    // Then: it has two parents, and reads as its previous release, on a row
    // of its own, and the side parents on its first-parent line, none of
    // them missing.
    let out = text(&members);
    assert_eq!(members.status.code(), Some(0), "{out}");
    assert!(out.contains(&format!("{NEXT} @ ")), "{out}");
    assert!(out.contains(" — 2 parents"), "{out}");
    assert!(out.contains(&previous_row(&lab)), "{out}");
    for branch in ["m5", "m6", "del"] {
        assert!(out.contains(&member_row(&lab, branch)), "{out}");
    }
    assert!(!out.contains("!!"), "{out}");
}

#[test]
fn without_stacked_an_include_stacked_on_the_previous_release_is_refused() {
    // Given: the same series and branches without the stacked key, and the
    // next release cut: it starts identical to the published first cut,
    // which is now its previous release.
    let (lab, home) = previous_release_lab(false);
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", NEXT]);
    assert!(resolves(&lab, NEXT), "{}", text(&cut));

    // When: a branch built on the previous release is included.
    let include = knives_release(&lab, &home, &["--series", "old", "include", "del"]);

    // Then: today's rule refuses it: its history past the trunk carries the
    // previous release's merge.
    let out = text(&include);
    assert_eq!(include.status.code(), Some(3), "{out}");
    assert!(out.contains("rebase it off the trunk"), "{out}");
}
