//! The cut gate over members stacked on one another, as a pull request into
//! another pull request's branch is.
//!
//! Each member is held to its own change: replayed from the members it is
//! stacked on, never from the trunk, so it is not charged with what they
//! wrote; and where members stacked on it rewrote its lines, the cut may
//! hold them as those members left them. A member's own change missing from
//! the cut tree, or changed there, is refused, naming that member alone. A
//! branch stacked on a member that the cut does not take is never read. A
//! stacked member must record the branch it is stacked on (`knives
//! depends`), and the cut must carry that branch.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use std::process::Output;

use knives::ids::CommitId;
use lab::{Lab, extend_branch, knives_release, release_parents, release_test_home, stack_branch};

/// A release of the default series.
const RELEASE: &str = "release/2026-08-04";

/// The release after [`RELEASE`].
const NEXT: &str = "release/2026-08-05";

fn text(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The commit of the work checkout's `revision`, a revset that names one.
fn commit_of(lab: &Lab, revision: &str) -> CommitId {
    CommitId::new(lab.revision(lab.work_path(), revision, "commit_id").trim())
}

/// Four branches stacked as pull requests into one another's branches:
/// `feat/base` writes `base.txt`; `feat/pr`, stacked on it, writes `pr.txt`;
/// `feat/other`, stacked on `feat/base` too, rewrites the line `feat/base`
/// wrote; `feat/later`, stacked on `feat/pr`, rewrites the line `feat/pr`
/// wrote and then writes `later.txt`, a file of its own.
fn stacked_lab() -> Lab {
    let lab = Lab::new();
    lab.branch("feat/base", "base.txt", "base\n");
    stack_branch(&lab, ("feat/pr", "feat/base"), ("pr.txt", "pr\n"));
    stack_branch(
        &lab,
        ("feat/other", "feat/base"),
        ("base.txt", "base, as feat/other rewrote it\n"),
    );
    stack_branch(
        &lab,
        ("feat/later", "feat/pr"),
        ("pr.txt", "pr, as feat/later rewrote it\n"),
    );
    extend_branch(&lab, "feat/later", "later.txt", "later\n");
    lab
}

/// `knives depends <branch> --on <on>` in `lab`'s work checkout with the
/// config `home`: the record that says which branch `branch` is stacked on.
fn depends(lab: &Lab, home: &tempfile::TempDir, branch: &str, on: &str) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "depends", branch, "--on", on])
        .current_dir(lab.work_path())
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .output()
        .expect("run knives depends");
    assert!(
        output.status.success(),
        "depends {branch} --on {on}: {}",
        text(&output)
    );
}

/// A config home for [`stacked_lab`] that records the branch each stacked
/// branch is stacked on.
fn stacked_home(lab: &Lab) -> tempfile::TempDir {
    let (home, _consumer) = release_test_home(lab);
    for (branch, on) in [
        ("feat/pr", "feat/base"),
        ("feat/other", "feat/base"),
        ("feat/later", "feat/pr"),
    ] {
        depends(lab, &home, branch, on);
    }
    home
}

/// The four branches of [`stacked_lab`] merged by hand, as a fork's
/// hand-cut release is, with each of `files` then written (or, for `None`,
/// removed) by hand in the merge itself. Returns the merge.
fn hand_merge(lab: &Lab, files: &[(&str, Option<&str>)]) -> CommitId {
    lab.jj_work([
        "new",
        "-r",
        "feat/base",
        "-r",
        "feat/pr",
        "-r",
        "feat/other",
        "-r",
        "feat/later",
        "-m",
        RELEASE,
    ]);
    for (file, content) in files {
        let path = lab.work_path().join(file);
        match content {
            Some(content) => std::fs::write(path, content).expect("write by hand"),
            None => std::fs::remove_file(path).expect("remove by hand"),
        }
    }
    lab.jj_work(["new"]);
    commit_of(lab, "@-")
}

/// `knives release cut <name> --at <commit>`, whatever its exit.
fn record(lab: &Lab, home: &tempfile::TempDir, name: &str, commit: &CommitId) -> Output {
    knives_release(lab, home, &["cut", name, "--at", commit.as_str()])
}

/// Whether `out` refuses `member` as missing from or diverging in the cut:
/// `!! <member>: ...` or, where the commit is named, `!! <member>@<commit>: ...`.
fn names_missing(out: &str, member: &str) -> bool {
    out.lines().any(|line| {
        line.trim_start()
            .strip_prefix("!! ")
            .and_then(|rest| rest.strip_prefix(member))
            .is_some_and(|rest| rest.starts_with(':') || rest.starts_with('@'))
            && line.contains("the cut tree is missing or diverges from the member's content")
    })
}

#[test]
fn a_cut_of_members_stacked_on_one_another_goes_ahead() {
    // Given: four branches stacked on one another, whose later members
    // rewrote lines the members below them wrote.
    let lab = stacked_lab();
    let home = stacked_home(&lab);

    // When: the release is cut.
    let output = knives_release(&lab, &home, &["cut", RELEASE]);

    // Then: it is cut, of all four: no member is charged with a member's
    // change below it, nor with a rewrite of its lines above it.
    let out = text(&output);
    assert!(output.status.success(), "{out}");
    assert_eq!(release_parents(&lab, RELEASE).len(), 4, "{out}");
}

#[test]
fn a_hand_cut_of_members_stacked_on_one_another_is_recorded() {
    // Given: the four branches merged by hand, nothing changed in the merge.
    let lab = stacked_lab();
    let commit = hand_merge(&lab, &[]);
    let home = stacked_home(&lab);

    // When: it is recorded.
    let recorded = record(&lab, &home, RELEASE, &commit);

    // Then: it is recorded.
    let out = text(&recorded);
    assert!(recorded.status.success(), "{out}");
    assert!(
        out.contains(&format!(
            "demo: recorded {RELEASE} at {} as its cut",
            commit.short()
        )),
        "{out}"
    );
}

#[test]
fn a_cut_tree_without_a_stacked_members_change_is_refused_naming_that_member_alone() {
    // Given: the four merged by hand, with `later.txt`, which `feat/later`
    // alone wrote, removed by hand in the merge.
    let lab = stacked_lab();
    let commit = hand_merge(&lab, &[("later.txt", None)]);
    let home = stacked_home(&lab);

    // When: it is recorded.
    let recorded = record(&lab, &home, RELEASE, &commit);

    // Then: it is refused, naming `feat/later`; `feat/pr`, which it is
    // stacked on, is not named: its own change, `pr.txt`, is there as
    // `feat/later` left it.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(names_missing(&out, "feat/later"), "{out}");
    for member in ["feat/base", "feat/pr", "feat/other"] {
        assert!(!names_missing(&out, member), "{member}: {out}");
    }
}

#[test]
fn a_cut_tree_that_changes_a_stacked_members_line_is_refused_naming_it() {
    // Given: the four merged by hand, with the line of `later.txt` that
    // `feat/later` wrote changed by hand in the merge.
    let lab = stacked_lab();
    let commit = hand_merge(&lab, &[("later.txt", Some("later, changed by hand\n"))]);
    let home = stacked_home(&lab);

    // When: it is recorded.
    let recorded = record(&lab, &home, RELEASE, &commit);

    // Then: it is refused, naming `feat/later`.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(names_missing(&out, "feat/later"), "{out}");
}

#[test]
fn a_cut_tree_that_changes_a_line_a_stacked_member_rewrote_is_refused_naming_it() {
    // Given: the four merged by hand, with `pr.txt`, whose line `feat/pr`
    // wrote and `feat/later` rewrote, changed by hand in the merge.
    let lab = stacked_lab();
    let commit = hand_merge(&lab, &[("pr.txt", Some("pr, changed by hand\n"))]);
    let home = stacked_home(&lab);

    // When: it is recorded.
    let recorded = record(&lab, &home, RELEASE, &commit);

    // Then: it is refused, naming `feat/later`, whose rewrite is gone, and
    // `feat/pr`, whose line is there neither as it wrote it nor as
    // `feat/later` left it.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(names_missing(&out, "feat/later"), "{out}");
    assert!(names_missing(&out, "feat/pr"), "{out}");
}

#[test]
fn a_branch_stacked_on_a_member_but_not_in_the_release_is_never_read() {
    // Given: `feat/a` writes `a.txt`; `feat/b`, stacked on it, rewrites
    // that line; and a release merged by hand of the trunk and `feat/a`
    // alone, so `feat/b` is no member.
    let lab = Lab::new();
    lab.branch("feat/a", "a.txt", "a\n");
    stack_branch(
        &lab,
        ("feat/b", "feat/a"),
        ("a.txt", "a, as feat/b rewrote it\n"),
    );
    lab.jj_work(["new", "-r", "main@origin", "-r", "feat/a", "-m", RELEASE]);
    lab.jj_work(["new"]);
    let own = commit_of(&lab, "@-");
    let (home, _consumer) = release_test_home(&lab);

    // When: it is recorded.
    let recorded = record(&lab, &home, RELEASE, &own);

    // Then: it is recorded: it holds `feat/a`'s own line.
    let out = text(&recorded);
    assert!(recorded.status.success(), "{out}");

    // Given: the next release, merged by hand the same way, with `a.txt` as
    // `feat/b` rewrote it, though `feat/b` is still no member.
    lab.jj_work(["new", "-r", "main@origin", "-r", "feat/a", "-m", NEXT]);
    std::fs::write(lab.work_path().join("a.txt"), "a, as feat/b rewrote it\n")
        .expect("write feat/b's rewrite by hand");
    lab.jj_work(["new"]);
    let rewritten = commit_of(&lab, "@-");

    // When: it is recorded.
    let refused = record(&lab, &home, NEXT, &rewritten);

    // Then: it is refused, naming `feat/a`: a branch the release does not
    // take is no member above `feat/a`, and its rewrite excuses nothing.
    let out = text(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(names_missing(&out, "feat/a"), "{out}");
}

/// Whether `out` refuses a cut because `member` is stacked on `parent`,
/// which the cut leaves out. The base check names the member and its
/// record (`!! <member> is stacked on <record>, ...`); the composition gate
/// names each with its commit (`!! <member>@<commit> is stacked on ...`).
fn names_missing_parent(out: &str, member: &str, parent: &str) -> bool {
    out.lines().any(|line| {
        let names_member = line.contains(&format!("!! {member} is stacked on "))
            || line.contains(&format!("!! {member}@"));
        names_member
            && line.contains("is stacked on ")
            && line.contains(parent)
            && line.contains("which this release leaves out")
    })
}

#[test]
fn a_cut_that_leaves_out_the_branch_a_member_is_stacked_on_is_refused_naming_both() {
    // Given: `feat/pr` stacked on `feat/base`, both cut; then `feat/base`
    // dropped from the release, while `feat/pr`, which reaches it, stays.
    let lab = Lab::new();
    lab.branch("feat/base", "base.txt", "base\n");
    stack_branch(&lab, ("feat/pr", "feat/base"), ("pr.txt", "pr\n"));
    let (home, _consumer) = release_test_home(&lab);
    depends(&lab, &home, "feat/pr", "feat/base");
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert!(cut.status.success(), "{}", text(&cut));
    let dropped = knives_release(
        &lab,
        &home,
        &["drop", "feat/base", "--why", "the base leaves the release"],
    );
    assert!(dropped.status.success(), "{}", text(&dropped));

    for extra in [&[][..], &["--allow-drop"][..]] {
        // When: the next release is cut, with and without `--allow-drop`.
        let args: Vec<&str> = ["cut", NEXT]
            .into_iter()
            .chain(extra.iter().copied())
            .collect();
        let output = knives_release(&lab, &home, &args);

        // Then: it is refused, naming `feat/pr` and the branch it is
        // stacked on: `feat/base`'s commits would ride in unreviewed through
        // `feat/pr`. `--allow-drop` states a drop; it is no way past this.
        let out = text(&output);
        assert_eq!(output.status.code(), Some(3), "{extra:?}: {out}");
        assert!(
            names_missing_parent(&out, "feat/pr", "feat/base"),
            "{extra:?}: {out}"
        );
    }
}

#[test]
fn a_branch_landed_into_a_members_line_is_no_missing_parent() {
    // Given: `feat/line` cut; then `feat/review`, a pull request into its
    // branch, landed there (its branch fast-forwarded onto it, then built on
    // past it), so `feat/review`'s tip is on `feat/line`'s first-parent line;
    // its bookmark stays, and the release's record never held it.
    let lab = Lab::new();
    lab.branch("feat/line", "line.txt", "line\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert!(cut.status.success(), "{}", text(&cut));
    stack_branch(
        &lab,
        ("feat/review", "feat/line"),
        ("review.txt", "review\n"),
    );
    lab.jj_work(["bookmark", "set", "feat/line", "-r", "feat/review"]);
    extend_branch(
        &lab,
        "feat/line",
        "line.txt",
        "line, after the review landed\n",
    );
    let advanced = knives_release(&lab, &home, &["advance", "feat/line"]);
    assert!(advanced.status.success(), "{}", text(&advanced));

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: it is cut: `feat/review` was built on the commit the release
    // recorded for `feat/line`, so it is no base of `feat/line`, and it was
    // never in the release's record, so it is no missing parent.
    let out = text(&output);
    assert!(output.status.success(), "{out}");
    assert!(!out.contains("is stacked on"), "{out}");
}

#[test]
fn a_cut_of_stacked_members_whose_bases_are_in_it_goes_ahead_with_no_record() {
    // Given: four branches stacked on one another, none of which records
    // the branch it is stacked on; each is built on another of them.
    let lab = stacked_lab();
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is cut.
    let output = knives_release(&lab, &home, &["cut", RELEASE]);

    // Then: it is cut, of all four, with no record asked for: each base is
    // in the cut, and the audit holds each member to its own change.
    let out = text(&output);
    assert!(output.status.success(), "{out}");
    assert!(!out.contains("record it with"), "{out}");
    assert_eq!(release_parents(&lab, RELEASE).len(), 4, "{out}");
}

#[test]
fn a_hand_cut_of_a_stacked_member_whose_base_is_out_and_unrecorded_is_refused_naming_the_record() {
    // Given: `feat/pr`, stacked on `feat/base`, with no record; a release
    // merged by hand of the trunk and `feat/pr` alone.
    let lab = Lab::new();
    lab.branch("feat/base", "base.txt", "base\n");
    stack_branch(&lab, ("feat/pr", "feat/base"), ("pr.txt", "pr\n"));
    lab.jj_work(["new", "-r", "main@origin", "-r", "feat/pr", "-m", RELEASE]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let (home, _consumer) = release_test_home(&lab);

    // When: it is recorded.
    let recorded = record(&lab, &home, RELEASE, &commit);

    // Then: it is refused (exit 3), naming `feat/pr` and the record that
    // would let its own change be told from its base's; no release is
    // recorded.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(
        out.contains(
            "feat/pr is stacked on feat/base, and no record says which branch its own change \
             starts from: record it with `knives depends feat/pr --on feat/base`"
        ),
        "{out}"
    );
    assert!(!out.contains(&format!("recorded {RELEASE}")), "{out}");
}

#[test]
fn a_hand_cut_that_leaves_out_a_stacked_members_base_is_refused_naming_it() {
    // Given: `feat/pr`, stacked on `feat/base`, records it; a release merged
    // by hand of the trunk and `feat/pr` alone.
    let lab = Lab::new();
    lab.branch("feat/base", "base.txt", "base\n");
    stack_branch(&lab, ("feat/pr", "feat/base"), ("pr.txt", "pr\n"));
    lab.jj_work(["new", "-r", "main@origin", "-r", "feat/pr", "-m", RELEASE]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let (home, _consumer) = release_test_home(&lab);
    depends(&lab, &home, "feat/pr", "feat/base");

    // When: it is recorded.
    let recorded = record(&lab, &home, RELEASE, &commit);

    // Then: it is refused, naming `feat/pr` and its base: `feat/base`'s
    // commits would ride in through `feat/pr`, unreviewed as a member.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(names_missing_parent(&out, "feat/pr", "feat/base"), "{out}");
}
