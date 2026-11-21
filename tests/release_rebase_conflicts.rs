//! What `knives release rebase` reports when the move leaves a member
//! conflicted.
//!
//! The rebase moves each member that the target's history does not hold, and
//! a member's own commits can conflict with the target. The report that ends
//! the command names each moved member whose rebased tip holds a conflict, by
//! its branch or by its short commit id, with its conflicted files, and the
//! command exits 1. That report is the rebase's own, or the landed drop's
//! when the drop writes. It says `no conflicts in this release`, or its union
//! form, only when neither the release nor a moved member holds a conflict. A
//! conflicted release with no conflicted member keeps exit 0. These tests
//! drive only the knives binary.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/forge_shim.rs"]
mod forge_shim;
#[path = "common/lab.rs"]
mod lab;
#[path = "common/release_forge.rs"]
mod release_forge;

use std::process::Output;

use forge_shim::pull_record;
use lab::{Lab, commit_at, file_at_revision, knives_release, release_parents, release_test_home};
use release_forge::knives_release_with_forge;

/// The release every test here cuts and rebases.
const RELEASE: &str = "release/2026-08-04";
/// A member that changes no line of `f`.
const M: &str = "feat/m";
/// A member that sets lines 5 and 6 of `f`, as `d809` did in the probe.
const D: &str = "feat/d";
/// A member, made after the cut, that sets line 5 of `f` as D does.
const A: &str = "feat/a";

/// Both streams, for an assertion's message.
fn text(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// `f`: thirty lines `let lN = N`, with each line of `changed` set to its
/// value.
fn f_with(changed: &[(usize, &str)]) -> String {
    use std::fmt::Write as _;
    let mut f = String::new();
    for line in 1..=30 {
        match changed.iter().find(|(at, _)| *at == line) {
            Some((_, value)) => writeln!(f, "let l{line} = {value}"),
            None => writeln!(f, "let l{line} = {line}"),
        }
        .expect("write to a String");
    }
    f
}

/// D's `f`: A's change to line 5, and its own line 6 next to it.
fn d_f() -> String {
    f_with(&[(5, "compare_fix ()"), (6, "d809_only ()")])
}

/// A's `f`: line 5 only.
fn a_f() -> String {
    f_with(&[(5, "compare_fix ()")])
}

/// The files `revision` leaves conflicted.
fn conflicts_of(lab: &Lab, revision: &str) -> Vec<String> {
    knives::jj::conflicted_files(&lab.work, revision).expect("list the conflicted files")
}

/// The line that names a member the rebase left conflicted.
fn member_line(name: &str, files: &str, count: usize) -> String {
    format!(
        "demo: member {name} holds {count} conflicted file(s) after the rebase: {files}; resolve \
         them on its branch"
    )
}

/// How the conflict of `include A` in `f` is resolved by hand before the
/// adopt.
#[derive(Clone, Copy)]
enum Resolution {
    /// `f` as D has it, which holds A's line too.
    DVersion,
    /// A text that keeps neither side.
    Neither,
}

/// The shape of case 3b of INFRA-4895's drop-after-adopt probe (version 2):
/// a trunk holding `f`; M adds `m.txt`; D sets lines 5 and 6 of `f`; a first
/// cut of M and D; A, made after the cut, sets line 5 as D does; `include A`
/// conflicts in `f`; `f` resolved by hand and adopted. Returns the lab and
/// its home.
fn adopted_release(resolution: Resolution) -> (Lab, tempfile::TempDir) {
    let lab = Lab::new();
    lab.upstream_trunk_file("f", &f_with(&[]));
    lab.mirror_upstream_trunk_to_origin();
    lab.branch(M, "m.txt", "m\n");
    lab.branch(D, "f", &d_f());
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert!(matches!(cut.status.code(), Some(0 | 1)), "{}", text(&cut));
    assert_eq!(release_parents(&lab, RELEASE).len(), 2, "{}", text(&cut));
    lab.branch(A, "f", &a_f());
    let included = knives_release(&lab, &home, &["include", A]);
    assert_eq!(included.status.code(), Some(0), "{}", text(&included));
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"], "{}", text(&included));
    match resolution {
        Resolution::DVersion => lab.jj_work(["restore", "--from", D, "--into", RELEASE, "f"]),
        Resolution::Neither => {
            // No message, so the squash keeps the release's own and opens
            // no editor.
            lab.jj_work(["new", RELEASE]);
            std::fs::write(lab.work.join("f"), "merged\n").expect("resolve f");
            lab.jj_work(["squash"]);
            lab.jj_work(["new", "main@origin"]);
        }
    }
    let adopted = knives_release(&lab, &home, &["adopt", RELEASE, "--why", "resolve f"]);
    assert!(
        matches!(adopted.status.code(), Some(0 | 1))
            && String::from_utf8_lossy(&adopted.stdout)
                .contains(&format!("as the newest write of {RELEASE}")),
        "{}",
        text(&adopted)
    );
    assert!(conflicts_of(&lab, RELEASE).is_empty(), "{}", text(&adopted));
    (lab, home)
}

#[test]
fn a_rebase_that_leaves_a_member_conflicted_names_it_and_exits_1() {
    // Given (K15 1): the adopted release, and a trunk commit T that makes
    // A's exact change to line 5 of `f`.
    let (lab, home) = adopted_release(Resolution::DVersion);
    lab.upstream_trunk_file("f", &a_f());

    // When: the release is rebased onto T.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: D's rebased tip holds a conflict in `f` and stays a parent of
    // the release.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(conflicts_of(&lab, D), ["f"], "{}", text(&output));
    assert!(
        release_parents(&lab, RELEASE).contains(&commit_at(&lab, D)),
        "{}",
        text(&output)
    );
    // And: the report names D's branch with `f`, never says the release has
    // no conflicts, and the command exits 1.
    assert!(
        stdout.contains(&member_line(D, "f", 1)),
        "{}",
        text(&output)
    );
    assert!(
        !stdout.contains("no conflicts in this release"),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
}

#[test]
fn a_no_drop_rebase_that_leaves_a_member_conflicted_exits_1() {
    // Given: K15 1's release and trunk.
    let (lab, home) = adopted_release(Resolution::DVersion);
    lab.upstream_trunk_file("f", &a_f());

    // When: the release is rebased onto T with `--no-drop`.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream", "--no-drop"]);

    // Then: the report names D's branch with `f`, and the command exits 1.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(conflicts_of(&lab, D), ["f"], "{}", text(&output));
    assert!(
        stdout.contains(&member_line(D, "f", 1)),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
}

#[test]
fn a_conflicted_release_alone_keeps_exit_0() {
    // Given (K15 2, a control): `f` resolved by hand to a text that keeps
    // neither side, and a trunk commit that changes line 30 of `f`, which no
    // member changes.
    let (lab, home) = adopted_release(Resolution::Neither);
    lab.upstream_trunk_file("f", &f_with(&[(30, "upstream30 ()")]));

    // When: the release is rebased onto it.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: the release holds a conflict in `f`, and no member does.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"], "{}", text(&output));
    for member in [M, D, A] {
        assert!(
            conflicts_of(&lab, member).is_empty(),
            "{member}: {}",
            text(&output)
        );
    }
    // And: the report names the release's conflicted file and no member,
    // and the command exits 0.
    assert!(
        stdout.contains("  1 conflicted file(s) left for a person:\n    f\n"),
        "{}",
        text(&output)
    );
    assert!(!stdout.contains("after the rebase:"), "{}", text(&output));
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
}

#[test]
fn a_conflicted_release_and_member_are_both_named() {
    // Given (K15 2): the adopted release, and a trunk commit that changes
    // line 6 of `f`, D's own line, as the probe's case 3e did.
    let (lab, home) = adopted_release(Resolution::DVersion);
    lab.upstream_trunk_file("f", &f_with(&[(6, "upstream6 ()")]));

    // When: the release is rebased onto it.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: the release and the members D and A hold a conflict in `f`.
    let stdout = String::from_utf8_lossy(&output.stdout);
    for conflicted in [RELEASE, D, A] {
        assert_eq!(
            conflicts_of(&lab, conflicted),
            ["f"],
            "{conflicted}: {}",
            text(&output)
        );
    }
    // And: the report names the release's file and each member, and the
    // command exits 1.
    assert!(
        stdout.contains("  1 conflicted file(s) left for a person:\n    f\n"),
        "{}",
        text(&output)
    );
    assert!(
        stdout.contains(&member_line(D, "f", 1)),
        "{}",
        text(&output)
    );
    assert!(
        stdout.contains(&member_line(A, "f", 1)),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
}

#[test]
fn a_clean_rebase_says_no_conflicts_and_exits_0() {
    // Given (K15 3, a control): the adopted release, and a trunk commit that
    // changes a file no member changes.
    let (lab, home) = adopted_release(Resolution::DVersion);
    lab.upstream_trunk_file("trunk-only.txt", "trunk only\n");

    // When: the release is rebased onto it.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: neither the release nor a member holds a conflict, the report
    // says so, and the command exits 0.
    let stdout = String::from_utf8_lossy(&output.stdout);
    for clean in [RELEASE, M, D, A] {
        assert!(
            conflicts_of(&lab, clean).is_empty(),
            "{clean}: {}",
            text(&output)
        );
    }
    assert!(
        stdout.contains("  no conflicts in this release"),
        "{}",
        text(&output)
    );
    assert!(!stdout.contains("after the rebase:"), "{}", text(&output));
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
}

#[test]
fn a_bare_rebase_whose_landed_drop_leaves_a_member_conflicted_names_it_and_exits_1() {
    // Given (K15 4, the probe's case 3c): the adopted release, and A's pull
    // request #7 squash-merged upstream, so that the trunk commit T holds
    // all of A's change and the forge reports #7 merged as T.
    let (lab, home) = adopted_release(Resolution::DVersion);
    lab.publish_pull(A, 7);
    lab.squash_merge_pull(7, None);
    let merged_at = commit_at(&lab, "main@upstream");
    let pulls = format!(
        "[{}]",
        pull_record(7, "MERGED", A, Some(merged_at.as_str()))
    );

    // When: the bare rebase moves onto T and drops what landed.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: A is dropped, and D's rebased tip holds a conflict in `f` and
    // stays a parent of the release.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (_, drop_report) = stdout
        .split_once(&format!("dropped {A}: landed upstream as #7"))
        .unwrap_or_else(|| panic!("A was not dropped: {}", text(&output)));
    assert_eq!(conflicts_of(&lab, D), ["f"], "{}", text(&output));
    let mut parents = release_parents(&lab, RELEASE);
    parents.sort();
    let mut kept = vec![commit_at(&lab, M), commit_at(&lab, D)];
    kept.sort();
    assert_eq!(parents, kept, "{}", text(&output));
    // And: the drop's report, which ends the command, names D's branch with
    // `f`, and the command exits 1.
    assert!(
        drop_report.contains(&member_line(D, "f", 1)),
        "{}",
        text(&output)
    );
    assert!(
        !stdout.contains("no conflicts in this release"),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    // And: `f` in the release still holds D's version.
    assert_eq!(file_at_revision(&lab, RELEASE, "f"), d_f());
}

#[test]
fn a_conflicted_member_no_bookmark_holds_is_named_by_its_commit() {
    // Given (K15 5): the adopted release, D held only by its branch on
    // `origin` (tracked, so the rebase moves it; its local bookmark is
    // deleted), and K15 1's trunk commit.
    let (lab, home) = adopted_release(Resolution::DVersion);
    lab.push_branch(D);
    lab.jj_work(["bookmark", "delete", D]);
    lab.upstream_trunk_file("f", &a_f());

    // When: the release is rebased onto it.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: D's rebased tip, which no bookmark holds, holds a conflict in
    // `f`, and the report names it by its short commit id.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (m, a) = (commit_at(&lab, M), commit_at(&lab, A));
    let parents = release_parents(&lab, RELEASE);
    let rebased_d = parents
        .iter()
        .find(|parent| **parent != m && **parent != a)
        .unwrap_or_else(|| panic!("no third parent: {parents:?}\n{}", text(&output)));
    assert_eq!(
        conflicts_of(&lab, rebased_d.as_str()),
        ["f"],
        "{}",
        text(&output)
    );
    assert!(
        stdout.contains(&member_line(rebased_d.short(), "f", 1)),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
}

/// A changelog whose `[Unreleased]` section holds `unreleased`, above one
/// published release.
fn with_unreleased(unreleased: &str) -> String {
    format!(
        "# Changelog\n\n## [Unreleased]\n\n{unreleased}## [1.0.0] - 2026-08-01\n\n### Added\n\n\
         - The first release.\n"
    )
}

#[test]
fn a_rebase_with_a_union_and_a_conflicted_member_does_not_say_no_other_conflicts() {
    // Given: a release of alpha, which adds an entry to the changelog's
    // `[Unreleased]` section, and beta; and a trunk commit that adds its own
    // entry at the same place.
    let lab = Lab::new();
    lab.upstream_trunk_file("CHANGELOG.md", &with_unreleased(""));
    lab.mirror_upstream_trunk_to_origin();
    lab.branch(
        "feat/alpha",
        "CHANGELOG.md",
        &with_unreleased("### Fixed\n\n- Alpha's fix.\n\n"),
    );
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert!(matches!(cut.status.code(), Some(0 | 1)), "{}", text(&cut));
    lab.upstream_trunk_file(
        "CHANGELOG.md",
        &with_unreleased("### Fixed\n\n- Upstream's fix.\n\n"),
    );

    // When: the release is rebased onto that trunk commit.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: alpha's rebased tip holds the conflict, and the release holds
    // the union of the changelog and no conflict.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        conflicts_of(&lab, "feat/alpha"),
        ["CHANGELOG.md"],
        "{}",
        text(&output)
    );
    assert!(conflicts_of(&lab, RELEASE).is_empty(), "{}", text(&output));
    assert!(
        stdout.contains("  1 release log(s) written as the union of their sides"),
        "{}",
        text(&output)
    );
    // And: the report says the release commit holds no conflicted file,
    // names alpha, never says there are no other conflicts, and the command
    // exits 1.
    assert!(
        stdout.contains(&format!(
            "demo: no conflicted file in the release commit\n{}",
            member_line("feat/alpha", "CHANGELOG.md", 1)
        )),
        "{}",
        text(&output)
    );
    assert!(
        !stdout.contains("no other conflicts in this release"),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
}
