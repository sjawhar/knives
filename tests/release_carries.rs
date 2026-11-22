//! `knives release members`, its `--carries` question and the `--census`.
//!
//! Members counts the repository's parents and names who holds each; `--verify`
//! reports a dropped member's content. Carries compares a revision's net content
//! with each target, stops before superseded releases when a live one carries,
//! and answers against the trunk when no release exists. A release that names a
//! member as a parent carries it only when its tree holds every line of the
//! member's own change as the member left it, its own change starting where a
//! record the graph bears out says it left its base; the trunk carries any
//! ancestor. The census finds orphan branches, respects
//! an open pull, excludes anonymous heads, and says unanswered when the forge
//! withholds a fact or is unavailable.

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

use forge_shim::{install_failing_gh, path_with_gh_shim, pull_record};
use knives::jj::Repo;
use lab::{
    Lab, ReleaseOutput, commit_at, extend_branch, home_after_first_cut, knives_release,
    release_command, release_parents, release_test_home,
};
use release_forge::{ReleaseWithSnapshotForgeInput, release_with_snapshot_forge};
use std::process::Command;

#[test]
fn members_counts_parents_and_names_their_holders() {
    // Given: a flat two-member cut, then feat/alpha advances without moving
    // the release parent it originally held.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let release = commit_at(&lab, "release/2026-08-04");
    let released_alpha = commit_at(&lab, "feat/alpha");
    let released_beta = commit_at(&lab, "feat/beta");
    extend_branch(&lab, "feat/alpha", "alpha.txt", "alpha\nmore\n");
    let advanced_alpha = commit_at(&lab, "feat/alpha");

    // When: the release's members are inspected through the real CLI.
    let output = knives_release(&lab, &home, &["members"]);

    // Then: its own two direct parents are counted, and each is represented
    // once by its current holder or its branch's advanced successor.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "members failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "release/2026-08-04 @ {} — 2 parents",
            release.as_str().chars().take(12).collect::<String>()
        )),
        "the count must come from the release's parent list: {stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "- {} feat/beta",
            released_beta.as_str().chars().take(12).collect::<String>()
        )),
        "the held parent is missing its holder: {stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "- {} feat/alpha advanced to {}",
            released_alpha.as_str().chars().take(12).collect::<String>(),
            advanced_alpha.as_str().chars().take(12).collect::<String>()
        )),
        "the advanced member must name the current tip: {stdout}"
    );
    assert_eq!(
        stdout.lines().filter(|line| line.starts_with("- ")).count(),
        2,
        "each release parent must render exactly one row: {stdout}"
    );
}

#[test]
fn members_escapes_a_remote_bookmark_a_remote_gave_a_c1_character() {
    // Given: a cut whose member another clone also pushed under a name with
    // U+009B (CSI) in it. git takes that ref name (it refuses only C0), and
    // while `jj bookmark create` refuses it, `jj git fetch` imports it as the
    // remote bookmark `feat<U+009B>m@origin`, which then holds the member.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let released = commit_at(&lab, "feat/alpha");
    lab.push_branch("feat/alpha");
    lab::git_output(
        &lab.temp_origin(),
        [
            "update-ref",
            "refs/heads/feat\u{9b}m",
            "refs/heads/feat/alpha",
        ],
    );
    lab.jj_work(["git", "fetch", "--remote", "origin"]);

    // When: the release's members are listed through the real CLI.
    let output = knives_release(&lab, &home, &["members"]);

    // Then: the member row names that holder with U+FFFD in place of CSI.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "members failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let short: String = released.as_str().chars().take(12).collect();
    let row = stdout
        .lines()
        .find(|line| line.starts_with(&format!("- {short} ")))
        .unwrap_or_else(|| panic!("no row for the member: {stdout}"));
    assert!(row.contains("feat\u{fffd}m@origin"), "was: {row:?}");
    assert!(
        !stdout.chars().any(|c| c.is_control() && c != '\n'),
        "a control character reached the terminal: {stdout:?}"
    );
}

#[test]
fn members_verify_reports_a_dropped_members_content() {
    // Given: the same hand-resolved conflicting cut as the recut scenario,
    // where the release tree contains neither member's original content.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        String::from_utf8_lossy(&first.stdout).contains("cut release/2026-08-04 as"),
        "first cut was refused: {first:?}"
    );
    let dropped_beta = commit_at(&lab, "feat/beta");
    lab.jj_work(["edit", "release/2026-08-04"]);
    std::fs::write(lab.work.join("shared.txt"), "merged\n").expect("resolve by hand");
    lab.jj_work(["new"]);

    // When: each member's content is replayed against the resolved release.
    let output = knives_release(&lab, &home, &["members", "--verify"]);

    // Then: the lost member is a finding, rather than a successful inspection.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "members verify must fail closed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "!! feat/beta@{}: the cut tree is missing or diverges from the member's content",
            dropped_beta.as_str().chars().take(12).collect::<String>()
        )),
        "the dropped member must be named under missing: {stdout}"
    );
}

#[test]
fn prose_parent_lines_do_not_inflate_the_count() {
    // Given: a flat two-member release whose description contains a line that
    // looks like an old text parser's parent record.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.jj_work([
        "describe",
        "-r",
        "release/2026-08-04",
        "-m",
        "release notes\nparent deadbeef from feat/x",
    ]);

    // When: the release is inspected by name.
    let output = knives_release(&lab, &home, &["members", "release/2026-08-04"]);

    // Then: prose never changes the structural parent count.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "members failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("release/2026-08-04 @") && stdout.contains("— 2 parents"),
        "the prose line inflated the count: {stdout}"
    );
    assert_eq!(
        stdout.lines().filter(|line| line.starts_with("- ")).count(),
        2,
        "the report must have exactly one row per actual parent: {stdout}"
    );
}

/// Publish an origin bookmark from the second clone, then fetch it into `work`.
fn publish_remote_bookmark(lab: &Lab, source: &str, destination: &str) {
    let run_in_second = |args: &[&str]| {
        let command = Command::new("jj")
            .args(args)
            .current_dir(&lab.second)
            .output()
            .expect("run jj in second clone");
        assert!(
            command.status.success(),
            "jj {args:?} failed: {}",
            String::from_utf8_lossy(&command.stderr)
        );
    };
    run_in_second(&["git", "fetch", "--remote", "origin"]);
    run_in_second(&["bookmark", "create", destination, "-r", source]);
    run_in_second(&[
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        destination,
    ]);
    lab.jj_work(["git", "fetch", "--remote", "origin"]);
}

#[test]
fn release_carries_answers_carried_for_a_member() {
    // Given: a release cut carrying alpha.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(cut.status.success(), "{cut:?}");

    // When: carries checks every release target and the upstream trunk.
    let output = knives_release(&lab, &home, &["members", "--carries", "feat/alpha"]);

    // Then: the live release says it carries alpha exactly, so the answer is safe.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("carried-exact      release/2026-08-04"),
        "{stdout}"
    );
}

/// Alpha and beta on files of their own, cut as `release/2026-08-04`, then
/// the release edited by hand to drop beta's file: a resolution that lost a
/// member's change while the member stays a parent.
fn release_that_dropped_beta() -> (Lab, tempfile::TempDir) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.jj_work(["edit", "release/2026-08-04"]);
    std::fs::remove_file(lab.work.join("beta.txt")).expect("drop beta's file by hand");
    lab.jj_work(["new"]);
    assert!(
        release_parents(&lab, "release/2026-08-04").contains(&commit_at(&lab, "feat/beta")),
        "beta must stay a parent of the release"
    );
    (lab, home)
}

#[test]
fn release_carries_answers_not_carried_for_a_parent_whose_change_the_release_dropped() {
    // Given: a release that names beta as a parent but whose tree, edited by
    // hand, no longer holds beta's file; alpha's file it still holds.
    let (lab, home) = release_that_dropped_beta();
    let release = commit_at(&lab, "release/2026-08-04");
    let alpha = commit_at(&lab, "feat/alpha");

    // When: carries asks of each member.
    let beta_output = knives_release(&lab, &home, &["members", "--carries", "feat/beta"]);
    let alpha_output = knives_release(&lab, &home, &["members", "--carries", "feat/alpha"]);

    // Then: beta's ancestry does not make it carried; the release is the
    // evidence its replay was judged against, and nothing makes it safe.
    let beta = String::from_utf8_lossy(&beta_output.stdout);
    assert_eq!(beta_output.status.code(), Some(1), "{beta}");
    assert!(
        beta.contains(&format!(
            "NOT carried        release/2026-08-04 @ {}  (evidence {}: replay leaves real diffs)",
            release.short(),
            release.short()
        )),
        "{beta}"
    );
    // And: alpha, whose change the release's tree holds, is carried exactly,
    // and the reason says the release holds its own change, not only that it
    // is an ancestor.
    let alpha_stdout = String::from_utf8_lossy(&alpha_output.stdout);
    assert!(alpha_output.status.success(), "{alpha_stdout}");
    assert!(
        alpha_stdout.contains(&format!(
            "carried-exact      release/2026-08-04 @ {}  (evidence {}: tip is an ancestor and \
             the release holds every line of its own change)",
            release.short(),
            alpha.short()
        )),
        "{alpha_stdout}"
    );
}

#[test]
fn census_answers_not_carried_for_a_parent_whose_change_the_release_dropped() {
    // Given: the same release, beta a parent whose file its tree dropped.
    let (lab, home) = release_that_dropped_beta();

    // When: the census checks every maintained branch.
    let output = knives_release(&lab, &home, &["members", "--census", "--no-github"]);

    // Then: beta is not carried by the release that names it, so it is
    // checked against the trunk too, and alpha is carried exactly.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let beta = census_block(&stdout, "feat/beta");
    assert!(
        beta.contains("NOT carried        release/2026-08-04"),
        "{beta}"
    );
    assert!(beta.contains("NOT carried        main@upstream"), "{beta}");
    let alpha = census_block(&stdout, "feat/alpha");
    assert!(
        alpha.contains("carried-exact      release/2026-08-04"),
        "{alpha}"
    );
}

/// A branch `name` with one commit on `base` that writes `content` to
/// `file`, its bookmark on that commit: a pull request into `base`'s branch.
fn stack_branch(lab: &Lab, (name, base): (&str, &str), (file, content): (&str, &str)) {
    lab.jj_work(["new", "-r", base, "-m", name]);
    std::fs::write(lab.work.join(file), content).expect("write the stacked branch");
    lab.jj_work(["bookmark", "create", name, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// `knives depends <branch> --on <on>` in `lab`'s work checkout with the
/// config `home`: the record that says which branch `branch` is stacked on.
fn depends(lab: &Lab, home: &tempfile::TempDir, branch: &str, on: &str) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "depends", branch, "--on", on])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .output()
        .expect("run knives depends");
    assert!(
        output.status.success(),
        "depends {branch} --on {on}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A release whose members stack as pull requests into another pull
/// request's branch do. `feat/base` writes `base.txt`; `feat/pr`, stacked
/// on it, writes `pr.txt`; `feat/other`, stacked on `feat/base` beside
/// `feat/pr`, rewrites the line `feat/base` wrote; `feat/later`, stacked on
/// `feat/pr`, rewrites the line `feat/pr` wrote. The release is a merge of
/// `feat/other` and `feat/later` made by hand, as a fork's hand-cut release
/// is, with no conflict to resolve: it holds both lines as the later
/// members left them, and every other branch is its ancestor. Each stacked
/// branch records the branch it is stacked on.
fn release_with_stacked_members() -> (Lab, tempfile::TempDir) {
    let (lab, home) = release_with_unrecorded_stacked_members();
    for (branch, on) in [
        ("feat/pr", "feat/base"),
        ("feat/other", "feat/base"),
        ("feat/later", "feat/pr"),
    ] {
        depends(&lab, &home, branch, on);
    }
    (lab, home)
}

/// [`release_with_stacked_members`], with no branch recording its base.
fn release_with_unrecorded_stacked_members() -> (Lab, tempfile::TempDir) {
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
    lab.jj_work(["new", "feat/other", "feat/later", "-m", "release"]);
    lab.jj_work(["bookmark", "create", "release/2026-08-04", "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);
    (lab, home)
}

#[test]
fn release_carries_names_the_nearest_branch_a_stacked_member_is_built_on() {
    // Given: `feat/later`, stacked on `feat/pr`, which is stacked on
    // `feat/base`; nothing records a base.
    let (lab, home) = release_with_unrecorded_stacked_members();

    // When, then: its replay from the trunk conflicts, and the answer names
    // the branch it was built on, `feat/pr`, not one further down.
    assert_unanswered(
        &lab,
        &home,
        "feat/later",
        "record it with `knives depends feat/later --on feat/pr`",
    );
}

#[test]
fn release_carries_reads_each_stacked_member_by_its_own_lines() {
    // Given: a release whose later members rewrote what `feat/base` and the
    // pull request into its branch, `feat/pr`, wrote, and which merged its
    // parents with no resolution of its own; each stacked branch records
    // the branch it was built on.
    let (lab, home) = release_with_stacked_members();
    let release = commit_at(&lab, "release/2026-08-04");

    // When, then: `feat/later` and `feat/other`, whose own lines the release
    // holds as they left them, are carried exactly, with each tip as the
    // evidence: their own changes start at the branches they were built on.
    for name in ["feat/later", "feat/other"] {
        let output = knives_release(
            &lab,
            &home,
            &["members", "release/2026-08-04", "--carries", name],
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{name}: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains(&format!(
                "carried-exact      release/2026-08-04 @ {}  (evidence {}: tip is an ancestor \
                 and the release holds every line of its own change)",
                release.short(),
                commit_at(&lab, name).short()
            )),
            "{name}: {stdout}"
        );
    }

    // And: `feat/base` and `feat/pr`, whose lines later members rewrote, are
    // not carried exactly: the release does not hold them as they left them.
    for name in ["feat/base", "feat/pr"] {
        let output = knives_release(
            &lab,
            &home,
            &["members", "release/2026-08-04", "--carries", name],
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(1), "{name}: {stdout}");
        assert!(stdout.contains("conflicted "), "{name}: {stdout}");
    }
}

#[test]
fn release_carries_answers_conflicted_for_outside_work_whose_replay_conflicts() {
    // Given: the same release, and a branch it does not reach that writes
    // another line where `feat/base` wrote its own.
    let (lab, home) = release_with_stacked_members();
    lab.branch("feat/stranger", "base.txt", "stranger\n");

    // When: carries asks the release about it.
    let output = knives_release(
        &lab,
        &home,
        &[
            "members",
            "release/2026-08-04",
            "--carries",
            "feat/stranger",
        ],
    );

    // Then: its replay conflicts and nothing in the release stands for it,
    // so the answer is conflicted, a finding, as before.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "conflicted         release/2026-08-04 @ {}",
            commit_at(&lab, "release/2026-08-04").short()
        )),
        "{stdout}"
    );
}

#[test]
fn release_carries_answers_not_carried_for_an_ancestor_a_hand_resolution_dropped() {
    // Given: the same release, edited by hand to drop the file `feat/pr`
    // wrote, which `feat/later` rewrote; both stay its ancestors.
    let (lab, home) = release_with_stacked_members();
    lab.jj_work(["edit", "release/2026-08-04"]);
    std::fs::remove_file(lab.work.join("pr.txt")).expect("drop pr.txt by hand");
    lab.jj_work(["new"]);
    let release = commit_at(&lab, "release/2026-08-04");
    for name in ["feat/pr", "feat/later"] {
        assert!(
            Repo::open(&lab.work)
                .expect("open the lab")
                .is_ancestor(&commit_at(&lab, name), &release)
                .expect("ancestry answerable"),
            "{name} must stay an ancestor of the release"
        );

        // When: carries asks the release about it.
        let output = knives_release(
            &lab,
            &home,
            &["members", "release/2026-08-04", "--carries", name],
        );

        // Then: ancestry does not make it carried: the release's own
        // resolution dropped its change.
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(1), "{name}: {stdout}");
        assert!(!stdout.contains("carried-"), "{name}: {stdout}");
    }
}

/// The branch the release-resolution tests ask about.
const MEMBER: &str = "feat/member";

/// Write each of `files` in the work checkout, or remove it for `None`.
fn write_by_hand(lab: &Lab, files: &[(&str, Option<&str>)]) {
    for (file, content) in files {
        let path = lab.work.join(file);
        match content {
            Some(content) => std::fs::write(path, content).expect("write by hand"),
            None => std::fs::remove_file(path).expect("remove by hand"),
        }
    }
}

/// `name`'s bookmark on a merge of `left` and `right` made by hand, with
/// each of `files` then written, or removed, in the merge itself.
fn merge_by_hand(
    lab: &Lab,
    name: &str,
    (left, right): (&str, &str),
    files: &[(&str, Option<&str>)],
) {
    lab.jj_work(["new", "-r", left, "-r", right, "-m", name]);
    write_by_hand(lab, files);
    lab.jj_work(["bookmark", "create", name, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// `name`'s bookmark on one commit on `base` that writes, or removes,
/// each of `files`.
fn child_by_hand(lab: &Lab, name: &str, base: &str, files: &[(&str, Option<&str>)]) {
    lab.jj_work(["new", "-r", base, "-m", name]);
    write_by_hand(lab, files);
    lab.jj_work(["bookmark", "create", name, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// [`MEMBER`], which writes `shared.txt`, and `feat/other`, which writes a
/// file of its own.
fn member_and_other() -> Lab {
    let lab = Lab::new();
    lab.branch(MEMBER, "shared.txt", "member-line\n");
    lab.branch("feat/other", "other.txt", "other\n");
    lab
}

/// [`assert_carried`] for [`MEMBER`].
fn assert_member_carried(lab: &Lab, carried: bool) {
    assert_carried(lab, MEMBER, carried);
}

/// [`assert_carried_in`] with a config home of its own, which records no
/// branch's base.
fn assert_carried(lab: &Lab, name: &str, carried: bool) {
    let (home, _consumer) = release_test_home(lab);
    assert_carried_in(lab, &home, name, carried);
}

/// Ask whether the release carries `name`, by the release's ref and by its
/// commit, with the config `home`, and check both answers: exit `0` and
/// `carried-exact` when `carried`, else exit `1` and no carried verdict.
fn assert_carried_in(lab: &Lab, home: &tempfile::TempDir, name: &str, carried: bool) {
    let release = commit_at(lab, "release/2026-08-04");
    assert!(
        Repo::open(&lab.work)
            .expect("open the lab")
            .is_ancestor(&commit_at(lab, name), &release)
            .expect("ancestry answerable"),
        "{name} must be an ancestor of the release"
    );
    for target in ["release/2026-08-04", release.as_str()] {
        let output = knives_release(lab, home, &["members", target, "--carries", name]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        if carried {
            assert_eq!(output.status.code(), Some(0), "{name} {target}: {stdout}");
            assert!(
                stdout.contains("carried-exact "),
                "{name} {target}: {stdout}"
            );
        } else {
            assert_eq!(output.status.code(), Some(1), "{name} {target}: {stdout}");
            assert!(!stdout.contains("carried-"), "{name} {target}: {stdout}");
        }
    }
}

#[test]
fn release_carries_answers_carried_exact_for_a_member_a_release_merged_as_it_is() {
    // Given: the member and another branch merged by hand, the member's
    // file as it wrote it.
    let lab = member_and_other();
    merge_by_hand(&lab, "release/2026-08-04", (MEMBER, "feat/other"), &[]);

    // When, then: the member is carried exactly.
    assert_member_carried(&lab, true);
}

#[test]
fn release_carries_answers_not_carried_for_a_member_the_release_merge_dropped() {
    // Given: the member and another branch merged by hand, the member's
    // file removed in the merge.
    let lab = member_and_other();
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        (MEMBER, "feat/other"),
        &[("shared.txt", None)],
    );

    // When, then: the member is not carried.
    assert_member_carried(&lab, false);
}

#[test]
fn release_carries_answers_not_carried_for_a_member_a_merge_below_the_release_dropped() {
    // Given: the member merged by hand with another branch, its file
    // removed in that merge; and the release, a merge of that one with a
    // third branch, which changes nothing by hand.
    let lab = member_and_other();
    merge_by_hand(
        &lab,
        "feat/resolved",
        (MEMBER, "feat/other"),
        &[("shared.txt", None)],
    );
    lab.branch("feat/third", "third.txt", "third\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/resolved", "feat/third"),
        &[],
    );

    // When, then: the member is not carried.
    assert_member_carried(&lab, false);
}

#[test]
fn release_carries_answers_not_carried_for_a_member_whose_line_a_later_branch_rewrote() {
    // Given: a branch built on the member that rewrites its line, merged by
    // hand with another branch as the release.
    let lab = member_and_other();
    child_by_hand(
        &lab,
        "feat/rewrite",
        MEMBER,
        &[("shared.txt", Some("the member's line, rewritten\n"))],
    );
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/rewrite", "feat/other"),
        &[],
    );

    // When, then: the member is not carried exactly: the release does not
    // hold its line as it left it, though a branch built on it rewrote it.
    assert_member_carried(&lab, false);
}

#[test]
fn release_carries_answers_not_carried_for_a_member_whose_line_a_merge_moved_to_another_file() {
    // Given: the member and another branch merged by hand, the member's line
    // moved in the merge from its file to another one.
    let lab = member_and_other();
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        (MEMBER, "feat/other"),
        &[("shared.txt", None), ("moved.txt", Some("member-line\n"))],
    );

    // When, then: the member is not carried.
    assert_member_carried(&lab, false);
}

#[test]
fn release_carries_answers_not_carried_for_a_member_a_resolution_discarded_for_the_other_side() {
    // Given: the member and another branch that write the same file their
    // own ways, merged by hand with the conflict resolved to the other
    // branch's side alone.
    let lab = Lab::new();
    lab.branch(MEMBER, "shared.txt", "member-line\n");
    lab.branch("feat/other", "shared.txt", "other-side\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        (MEMBER, "feat/other"),
        &[("shared.txt", Some("other-side\n"))],
    );

    // When, then: the member is not carried: the resolution dropped it,
    // and the other side never held its line.
    assert_member_carried(&lab, false);
}

#[test]
fn release_carries_answers_not_carried_for_a_member_a_branch_built_on_it_reverted() {
    // Given: a branch built on the member that removes the member's file,
    // merged by hand with another branch as the release.
    let lab = member_and_other();
    child_by_hand(&lab, "feat/revert", MEMBER, &[("shared.txt", None)]);
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/revert", "feat/other"),
        &[],
    );

    // When, then: the member is not carried: nothing of its change is left.
    assert_member_carried(&lab, false);
}

#[test]
fn release_carries_answers_not_carried_for_a_member_whose_deletion_a_merge_restored() {
    // Given: the member, which empties `README.md`, and another branch,
    // merged by hand with the line the member deleted written back.
    let lab = Lab::new();
    lab.branch(MEMBER, "README.md", "");
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        (MEMBER, "feat/other"),
        &[("README.md", Some("seed\n"))],
    );

    // When, then: the member is not carried.
    assert_member_carried(&lab, false);
}

#[test]
fn release_carries_answers_not_carried_for_a_member_whose_lines_a_merge_reordered() {
    // Given: the member, which writes two lines, and another branch, merged
    // by hand with the member's two lines swapped.
    let lab = Lab::new();
    lab.branch(MEMBER, "shared.txt", "first\nsecond\n");
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        (MEMBER, "feat/other"),
        &[("shared.txt", Some("second\nfirst\n"))],
    );

    // When, then: the member is not carried exactly.
    assert_member_carried(&lab, false);
}

#[test]
fn release_carries_answers_conflicted_for_a_sibling_whose_rewrite_the_release_discarded() {
    // Given: `feat/alpha` and `feat/beta`, stacked on `feat/base`, rewrite
    // the line `b` it wrote each their own way; the release merges them by
    // hand keeping `feat/beta`'s line.
    let lab = Lab::new();
    lab.branch("feat/base", "f.txt", "a\nb\nc\n");
    child_by_hand(
        &lab,
        "feat/alpha",
        "feat/base",
        &[("f.txt", Some("a\nb-alpha\nc\n"))],
    );
    child_by_hand(
        &lab,
        "feat/beta",
        "feat/base",
        &[("f.txt", Some("a\nb-beta\nc\n"))],
    );
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/alpha", "feat/beta"),
        &[("f.txt", Some("a\nb-beta\nc\n"))],
    );
    let (home, _consumer) = release_test_home(&lab);
    depends(&lab, &home, "feat/alpha", "feat/base");
    depends(&lab, &home, "feat/beta", "feat/base");

    // When, then: `feat/alpha`, whose line the resolution discarded for a
    // sibling's, is not carried; `feat/beta` is; and `feat/base`, whose line
    // a branch built on it rewrote, is not carried exactly either.
    assert_carried_in(&lab, &home, "feat/alpha", false);
    assert_carried_in(&lab, &home, "feat/beta", true);
    assert_carried_in(&lab, &home, "feat/base", false);
}

#[test]
fn release_carries_answers_not_carried_for_a_member_a_resolution_discarded_before_a_later_edit() {
    // Given: the member and another branch write `shared.txt` their own
    // ways; a merge of them resolves to the other side alone; a commit on
    // that merge edits the file again; the release merges that commit with a
    // third branch.
    let lab = Lab::new();
    lab.branch(MEMBER, "shared.txt", "member-line\n");
    lab.branch("feat/other", "shared.txt", "other-side\n");
    merge_by_hand(
        &lab,
        "feat/resolved",
        (MEMBER, "feat/other"),
        &[("shared.txt", Some("other-side\n"))],
    );
    child_by_hand(
        &lab,
        "feat/later",
        "feat/resolved",
        &[("shared.txt", Some("other-side edited later\n"))],
    );
    lab.branch("feat/third", "third.txt", "third\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/later", "feat/third"),
        &[],
    );

    // When, then: the member is not carried: the later edit replaced what
    // the resolution left, not the member's line, which the release lacks.
    assert_member_carried(&lab, false);
}

/// `feat/foundation` writes `shared.txt` as `before`; the member, stacked on
/// it and recording it, writes `member`; a branch built on the member
/// writes `later`; the release merges that branch with another. Asks the
/// release about the member and checks it is carried exactly or not.
fn adjacent_edit((before, member, later): (&str, &str, &str), carried: bool) {
    let lab = Lab::new();
    lab.branch("feat/foundation", "shared.txt", before);
    child_by_hand(
        &lab,
        MEMBER,
        "feat/foundation",
        &[("shared.txt", Some(member))],
    );
    child_by_hand(&lab, "feat/rewrite", MEMBER, &[("shared.txt", Some(later))]);
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/rewrite", "feat/other"),
        &[],
    );
    let (home, _consumer) = release_test_home(&lab);
    depends(&lab, &home, MEMBER, "feat/foundation");
    assert_carried_in(&lab, &home, MEMBER, carried);
}

#[test]
fn release_carries_answers_not_carried_for_an_added_line_a_later_branch_rewrote() {
    // The member adds a line before `keep`; a later branch rewrites the
    // added line and keeps `keep`: the release lacks the line as it was
    // added.
    adjacent_edit(
        ("keep\n", "added\nkeep\n", "rewritten-added\nkeep\n"),
        false,
    );
}

#[test]
fn release_carries_answers_carried_exact_for_an_added_line_beside_a_rewrite_of_base_text() {
    // The member adds a line before `keep`; a later branch keeps the added
    // line and rewrites `keep`: every line the member wrote is there.
    adjacent_edit(("keep\n", "added\nkeep\n", "added\nrewritten-keep\n"), true);
}

#[test]
fn release_carries_answers_not_carried_for_an_added_line_a_later_branch_reverted() {
    // The member adds a line; a later branch removes it again: not carried.
    adjacent_edit(("keep\n", "added\nkeep\n", "keep\n"), false);
}

#[test]
fn release_carries_answers_not_carried_for_an_added_line_dropped_beside_a_rewrite_of_base_text() {
    // The member adds a line before `keep`; a later branch drops the added
    // line and rewrites `keep`: the rewrite beside it hides nothing.
    adjacent_edit(("keep\n", "added\nkeep\n", "rewritten-keep\n"), false);
}

#[test]
fn release_carries_answers_not_carried_for_a_replacement_reverted_beside_a_rewrite_of_base_text() {
    // The member replaces `old` with `new`; a later branch puts `old` back
    // and rewrites `keep`.
    adjacent_edit(
        ("old\nkeep\n", "new\nkeep\n", "old\nrewritten-keep\n"),
        false,
    );
}

#[test]
fn release_carries_answers_not_carried_for_a_deletion_restored_beside_a_rewrite_of_base_text() {
    // The member deletes `deleted`; a later branch puts it back and rewrites
    // `keep`.
    adjacent_edit(
        ("deleted\nkeep\n", "keep\n", "deleted\nrewritten-keep\n"),
        false,
    );
}

#[test]
fn release_carries_answers_not_carried_for_a_member_deleted_then_recreated_by_later_work() {
    // Given: a branch built on the member removes its file; another built
    // on that one writes a new file at the same path; the release merges
    // that with another branch.
    let lab = member_and_other();
    child_by_hand(&lab, "feat/drop", MEMBER, &[("shared.txt", None)]);
    child_by_hand(
        &lab,
        "feat/recreate",
        "feat/drop",
        &[(
            "shared.txt",
            Some("new file after the member was removed\n"),
        )],
    );
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/recreate", "feat/other"),
        &[],
    );

    // When, then: the member is not carried: the new file replaced nothing
    // of the member's, which was gone before it.
    assert_member_carried(&lab, false);
}

#[test]
fn release_carries_answers_not_carried_for_a_member_whose_deleted_line_the_release_put_back() {
    // Given: `feat/del`, stacked on `feat/base`, deletes the line `b`; the
    // release merges it with another branch and puts `b` back by hand.
    let lab = Lab::new();
    lab.branch("feat/base", "f.txt", "a\nb\nc\n");
    child_by_hand(&lab, "feat/del", "feat/base", &[("f.txt", Some("a\nc\n"))]);
    lab.branch("feat/x", "x.txt", "x\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/del", "feat/x"),
        &[("f.txt", Some("a\nb\nc\n"))],
    );
    let (home, _consumer) = release_test_home(&lab);
    depends(&lab, &home, "feat/del", "feat/base");

    // When, then: `feat/del` is not carried.
    assert_carried_in(&lab, &home, "feat/del", false);
}

#[test]
fn release_carries_cannot_answer_an_unrecorded_stacked_member_whose_deleted_line_was_put_back() {
    // Given: the same shape, but `feat/del` records no base.
    let lab = Lab::new();
    lab.branch("feat/base", "f.txt", "a\nb\nc\n");
    child_by_hand(&lab, "feat/del", "feat/base", &[("f.txt", Some("a\nc\n"))]);
    lab.branch("feat/x", "x.txt", "x\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/del", "feat/x"),
        &[("f.txt", Some("a\nb\nc\n"))],
    );
    let (home, _consumer) = release_test_home(&lab);

    // When, then: from the trunk, `f.txt` is one file `feat/del` added, and
    // the line it deleted cannot be seen; so it is not answered, and the
    // answer names the record to write.
    assert_unanswered(
        &lab,
        &home,
        "feat/del",
        "record it with `knives depends feat/del --on feat/base`",
    );
}

#[test]
fn release_carries_answers_not_carried_for_a_member_that_deleted_a_line_it_merged_in() {
    // Given: `feat/del`, built on `feat/base` and recording it, merges in
    // `feat/u`, which adds `u.txt`, then deletes a line of `u.txt`; the
    // release merges `feat/del` with another branch and puts the line back.
    let lab = Lab::new();
    lab.branch("feat/base", "base.txt", "base\n");
    child_by_hand(&lab, "feat/del", "feat/base", &[("del.txt", Some("del\n"))]);
    lab.branch("feat/u", "u.txt", "x\ny\n");
    merge_by_hand(&lab, "feat/del-merged", ("feat/del", "feat/u"), &[]);
    child_by_hand(
        &lab,
        "feat/del-final",
        "feat/del-merged",
        &[("u.txt", Some("x\n"))],
    );
    lab.jj_work(["bookmark", "set", "feat/del", "-r", "feat/del-final"]);
    lab.jj_work(["bookmark", "delete", "feat/del-merged", "feat/del-final"]);
    lab.branch("feat/x", "x.txt", "x\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/del", "feat/x"),
        &[("u.txt", Some("x\ny\n"))],
    );
    let (home, _consumer) = release_test_home(&lab);
    depends(&lab, &home, "feat/del", "feat/base");

    // When, then: `feat/del` is not carried exactly: its own change holds a
    // merge, so a line it deleted from what it merged in cannot be told
    // from a line the release added.
    assert_carried_in(&lab, &home, "feat/del", false);
}

/// `feat/line` changes `README.md`; `feat/member`, stacked on it, writes a
/// file of its own; the release merges `feat/member` with another branch
/// and rewrites by hand the line `feat/line` wrote, as a release rewrites a
/// dependency in a build file a branch below a member wrote.
fn member_on_a_line_whose_file_the_release_changed() -> Lab {
    let lab = Lab::new();
    lab.branch("feat/line", "README.md", "seed, as feat/line left it\n");
    child_by_hand(
        &lab,
        "feat/member",
        "feat/line",
        &[("member.txt", Some("member\n"))],
    );
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/member", "feat/other"),
        &[("README.md", Some("seed, as the release rewrote it\n"))],
    );
    lab
}

#[test]
fn release_carries_answers_carried_exact_for_a_stacked_member_its_base_file_changed_under() {
    // Given: a member stacked on `feat/line`, which records so, and a
    // release that changed by hand a file `feat/line` wrote and the member
    // never touched.
    let lab = member_on_a_line_whose_file_the_release_changed();
    let (home, _consumer) = release_test_home(&lab);
    depends(&lab, &home, "feat/member", "feat/line");

    // When, then: the member is carried exactly: its own change, from the
    // branch it is stacked on, is in the release as it left it.
    assert_carried_in(&lab, &home, "feat/member", true);
}

/// Ask whether the release carries `name` with the config `home`, and check
/// that it cannot be answered (exit 3) for the reason `reason`.
fn assert_unanswered(lab: &Lab, home: &tempfile::TempDir, name: &str, reason: &str) {
    let output = knives_release(
        lab,
        home,
        &["members", "release/2026-08-04", "--carries", name],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{name}: {stdout}");
    assert!(!stdout.contains("carried-"), "{name}: {stdout}");
    assert!(stdout.contains(reason), "{name}: {stdout}");
}

#[test]
fn release_carries_cannot_answer_a_stacked_member_that_records_no_base() {
    // Given: the same member and release, with no record of what the
    // member is stacked on.
    let lab = member_on_a_line_whose_file_the_release_changed();
    let (home, _consumer) = release_test_home(&lab);

    // When, then: its replay from the trunk conflicts, and nothing says
    // which branch its own change starts from, so it is not answered, and
    // the answer names the record that would answer it.
    assert_unanswered(
        &lab,
        &home,
        "feat/member",
        "feat/member is stacked on feat/line, and no record says which branch its own change \
         starts from: record it with `knives depends feat/member --on feat/line`",
    );
}

#[test]
fn release_carries_cannot_answer_a_stacked_member_that_records_several_bases() {
    // Given: the same member and release; the member records two branches.
    let lab = member_on_a_line_whose_file_the_release_changed();
    let (home, _consumer) = release_test_home(&lab);
    depends(&lab, &home, "feat/member", "feat/line");
    depends(&lab, &home, "feat/member", "feat/other");

    // When, then: which one its own change starts from cannot be told.
    assert_unanswered(
        &lab,
        &home,
        "feat/member",
        "feat/member is stacked on feat/line and records 2 branches or review pull requests",
    );
}

#[test]
fn release_carries_does_not_see_a_branch_stacked_on_a_review_branch_merged_into_it() {
    // Given: `feat/line` took `feat/review`, a pull request into it, by a
    // merge, and `feat/review` keeps its bookmark; `feat/x`, stacked on
    // `feat/line`, rewrites the line `feat/line` wrote; the release merges
    // `feat/x` with another branch. `feat/line` records no base.
    let lab = Lab::new();
    lab.branch("feat/line", "line.txt", "line\n");
    child_by_hand(
        &lab,
        "feat/review",
        "feat/line",
        &[("review.txt", Some("review\n"))],
    );
    child_by_hand(
        &lab,
        "feat/line-next",
        "feat/line",
        &[("next.txt", Some("next\n"))],
    );
    merge_by_hand(&lab, "landed", ("feat/line-next", "feat/review"), &[]);
    lab.jj_work(["bookmark", "set", "feat/line", "-r", "landed"]);
    lab.jj_work(["bookmark", "delete", "landed", "feat/line-next"]);
    child_by_hand(
        &lab,
        "feat/x",
        "feat/line",
        &[("line.txt", Some("line, as feat/x rewrote it\n"))],
    );
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(&lab, "release/2026-08-04", ("feat/x", "feat/other"), &[]);

    // When: carries asks the release about `feat/line`.
    let (home, _consumer) = release_test_home(&lab);
    let output = knives_release(
        &lab,
        &home,
        &["members", "release/2026-08-04", "--carries", "feat/line"],
    );

    // Then: `feat/line` is no branch stacked on `feat/review`, which it only
    // merged, so it starts from the trunk with no record asked for, and it
    // is conflicted, as the rewrite of its line makes it from the trunk.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("conflicted "), "{stdout}");
    assert!(!stdout.contains("record it with"), "{stdout}");
}

#[test]
fn release_carries_reads_a_member_landed_as_a_merges_first_parent_under_a_later_merge() {
    // Given: the member landed into `feat/line` by a merge whose first parent
    // is the member; `feat/line` then merged an unrelated branch; the release
    // merges `feat/line` with another branch and changes by hand the file
    // `feat/line` wrote first.
    let lab = Lab::new();
    lab.branch("feat/line", "README.md", "seed, as feat/line left it\n");
    child_by_hand(
        &lab,
        "feat/member",
        "feat/line",
        &[("member.txt", Some("member\n"))],
    );
    child_by_hand(
        &lab,
        "feat/line-next",
        "feat/line",
        &[("next.txt", Some("next\n"))],
    );
    merge_by_hand(&lab, "landed", ("feat/member", "feat/line-next"), &[]);
    lab.branch("feat/unrelated", "unrelated.txt", "unrelated\n");
    merge_by_hand(&lab, "after", ("landed", "feat/unrelated"), &[]);
    lab.jj_work(["bookmark", "set", "feat/line", "-r", "after"]);
    lab.jj_work(["bookmark", "delete", "landed", "after", "feat/line-next"]);
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/line", "feat/other"),
        &[(
            "README.md",
            Some("seed, as feat/line left it\na dependency the release added\n"),
        )],
    );
    let (home, _consumer) = release_test_home(&lab);
    depends(&lab, &home, "feat/member", "feat/line");

    // When, then: the merge nearest the member that has a parent apart from
    // it is the one that landed it, and the release holds its own change.
    assert_carried_in(&lab, &home, "feat/member", true);
}

#[test]
fn release_carries_reads_a_member_landed_into_its_base_from_before_it_landed() {
    // Given: the member landed into `feat/line` by a merge, then `feat/line`
    // went on; the release merges `feat/line` with another branch and
    // changes by hand the file `feat/line` wrote first.
    let lab = Lab::new();
    lab.branch("feat/line", "README.md", "seed, as feat/line left it\n");
    child_by_hand(
        &lab,
        "feat/member",
        "feat/line",
        &[("member.txt", Some("member\n"))],
    );
    child_by_hand(
        &lab,
        "feat/line-next",
        "feat/line",
        &[("next.txt", Some("next\n"))],
    );
    merge_by_hand(&lab, "landed", ("feat/line-next", "feat/member"), &[]);
    lab.jj_work(["bookmark", "set", "feat/line", "-r", "landed"]);
    lab.jj_work(["bookmark", "delete", "landed", "feat/line-next"]);
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/line", "feat/other"),
        &[(
            "README.md",
            Some("seed, as feat/line left it\na dependency the release added\n"),
        )],
    );
    let (home, _consumer) = release_test_home(&lab);
    depends(&lab, &home, "feat/member", "feat/line");

    // When, then: its own change is read from where it left `feat/line`
    // before it landed, and the release holds it.
    assert_carried_in(&lab, &home, "feat/member", true);

    // And: had `feat/line` taken the member by moving onto it, with no
    // merge, its own change cannot be told from the base's, so it is read
    // from the trunk: carried exactly only where that reading replays
    // empty, the release holding every line since the trunk as it was left.
    lab.jj_work([
        "bookmark",
        "set",
        "feat/line",
        "-r",
        "feat/member",
        "--allow-backwards",
    ]);
    child_by_hand(
        &lab,
        "feat/line-after",
        "feat/line",
        &[("after.txt", Some("after\n"))],
    );
    lab.jj_work(["bookmark", "set", "feat/line", "-r", "feat/line-after"]);
    lab.jj_work(["bookmark", "delete", "feat/line-after"]);
    merge_by_hand(&lab, "release/2026-08-05", ("feat/line", "feat/other"), &[]);
    let output = knives_release(
        &lab,
        &home,
        &["members", "release/2026-08-05", "--carries", "feat/member"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("carried-exact "), "{stdout}");

    // And: where the release changed a file `feat/line` wrote, the reading
    // from the trunk conflicts, and the member is not answered: from the
    // trunk, a line its base wrote and it deleted cannot be seen.
    for (release, readme) in [
        (
            "release/2026-08-06",
            "seed, as feat/line left it\na dependency the release added\n",
        ),
        ("release/2026-08-07", "seed, rewritten by the release\n"),
    ] {
        merge_by_hand(
            &lab,
            release,
            ("feat/line", "feat/other"),
            &[("README.md", Some(readme))],
        );
        let output = knives_release(
            &lab,
            &home,
            &["members", release, "--carries", "feat/member"],
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(3), "{release}: {stdout}");
        assert!(
            stdout.contains("which took it with no merge"),
            "{release}: {stdout}"
        );
    }
}

#[test]
fn release_carries_answers_not_carried_for_both_members_of_a_cut_conflict_resolved_to_neither() {
    // Given: `feat/alpha` and `feat/beta` write the same file their own
    // ways; knives cuts the release with the conflict stored, and the
    // release is resolved by hand to a third text.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        String::from_utf8_lossy(&cut.stdout).contains("cut release/2026-08-04 as"),
        "{cut:?}"
    );
    lab.jj_work(["edit", "release/2026-08-04"]);
    std::fs::write(lab.work.join("shared.txt"), "merged\n").expect("resolve by hand");
    lab.jj_work(["new"]);

    // When, then: neither member is carried, as `members --verify` says.
    assert_carried_in(&lab, &home, "feat/alpha", false);
    assert_carried_in(&lab, &home, "feat/beta", false);
    let verify = knives_release(&lab, &home, &["members", "--verify"]);
    assert_eq!(verify.status.code(), Some(1), "{verify:?}");
}

#[test]
fn release_carries_answers_not_carried_for_members_a_resolution_discarded_under_a_later_commit() {
    // Given: the same cut, resolved by hand to a third text; then one
    // one-parent commit on the release that edits that text again, and the
    // release bookmark moved onto it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        String::from_utf8_lossy(&cut.stdout).contains("cut release/2026-08-04 as"),
        "{cut:?}"
    );
    lab.jj_work(["edit", "release/2026-08-04"]);
    std::fs::write(lab.work.join("shared.txt"), "merged\n").expect("resolve by hand");
    lab.jj_work(["new", "-r", "release/2026-08-04", "-m", "a later edit"]);
    std::fs::write(lab.work.join("shared.txt"), "merged, fixed\n").expect("edit again");
    lab.jj_work(["bookmark", "set", "release/2026-08-04", "-r", "@"]);
    lab.jj_work(["new"]);

    // When, then: neither member is carried: the release holds neither
    // member's line, whatever commit last edited the text in its place.
    assert_carried_in(&lab, &home, "feat/alpha", false);
    assert_carried_in(&lab, &home, "feat/beta", false);
}

#[test]
fn release_carries_answers_not_carried_where_a_reversed_record_would_hide_a_dropped_commit() {
    // Given: `feat/x` is two commits, X1 writing `a.txt` and X2 writing
    // `b.txt`; `feat/y` is built on X1; the release merges both and drops
    // `a.txt` by hand.
    let lab = Lab::new();
    lab.branch("feat/x", "a.txt", "a\n");
    extend_branch(&lab, "feat/x", "b.txt", "b\n");
    let x1 = lab.revision(lab.work_path(), "feat/x-", "commit_id");
    lab.jj_work(["new", "-r", x1.trim(), "-m", "feat/y"]);
    std::fs::write(lab.work.join("y.txt"), "y\n").expect("write feat/y");
    lab.jj_work(["bookmark", "create", "feat/y", "-r", "@"]);
    lab.jj_work(["new"]);
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/x", "feat/y"),
        &[("a.txt", None)],
    );
    let (home, _consumer) = release_test_home(&lab);

    // When, then: `feat/x` is not carried with no record.
    assert_carried_in(&lab, &home, "feat/x", false);

    // And: a reversed record, `feat/x` built on `feat/y`, does not make it
    // carried: its fork point with `feat/y` is X1, one of its own commits.
    depends(&lab, &home, "feat/x", "feat/y");
    assert_carried_in(&lab, &home, "feat/x", false);
}

#[test]
fn release_carries_answers_not_carried_for_a_member_a_commit_below_a_later_merge_dropped() {
    // Given: a merge of `feat/alpha` and `feat/beta`; a commit on it that
    // removes `feat/alpha`'s file; and the release, a merge of that commit
    // with `feat/gamma`.
    let lab = Lab::new();
    lab.branch("feat/alpha", "a.txt", "alpha\n");
    lab.branch("feat/beta", "b.txt", "beta\n");
    merge_by_hand(&lab, "m", ("feat/alpha", "feat/beta"), &[]);
    child_by_hand(&lab, "f", "m", &[("a.txt", None)]);
    lab.branch("feat/gamma", "c.txt", "gamma\n");
    merge_by_hand(&lab, "release/2026-08-04", ("f", "feat/gamma"), &[]);

    // When, then: `feat/alpha` is not carried.
    assert_carried(&lab, "feat/alpha", false);
}

/// [`MEMBER`] on the trunk, one commit that writes each of `files`, with
/// `mode` set on the file `shared.sh` when given.
fn member_with(lab: &Lab, files: &[(&str, Option<&str>)], mode: Option<u32>) {
    use std::os::unix::fs::PermissionsExt as _;
    lab.jj_work(["new", "-r", "main@origin", "-m", MEMBER]);
    write_by_hand(lab, files);
    if let Some(mode) = mode {
        std::fs::set_permissions(
            lab.work.join("shared.sh"),
            std::fs::Permissions::from_mode(mode),
        )
        .expect("set the mode");
    }
    lab.jj_work(["bookmark", "create", MEMBER, "-r", "@"]);
    lab.jj_work(["new"]);
}

#[test]
fn release_carries_answers_not_carried_for_a_member_whose_mode_the_release_merge_dropped() {
    // Given: the member makes `shared.sh` executable; the release merges it
    // with another branch and clears the mode by hand.
    use std::os::unix::fs::PermissionsExt as _;
    let lab = Lab::new();
    member_with(&lab, &[("shared.sh", Some("run\n"))], Some(0o755));
    lab.branch("feat/other", "other.txt", "other\n");
    lab.jj_work(["new", "-r", MEMBER, "-r", "feat/other", "-m", "release"]);
    std::fs::set_permissions(
        lab.work.join("shared.sh"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("clear the mode");
    lab.jj_work(["bookmark", "create", "release/2026-08-04", "-r", "@"]);
    lab.jj_work(["new"]);

    // When, then: the member is not carried.
    assert_member_carried(&lab, false);
}

#[test]
fn release_carries_answers_not_carried_for_a_mode_dropped_where_a_later_branch_rewrote_the_line() {
    // Given: the member writes an executable `shared.sh`; a branch built on
    // it rewrites its line; the release merges that branch with another and
    // clears the mode by hand.
    use std::os::unix::fs::PermissionsExt as _;
    let lab = Lab::new();
    member_with(&lab, &[("shared.sh", Some("run\n"))], Some(0o755));
    child_by_hand(
        &lab,
        "feat/rewrite",
        MEMBER,
        &[("shared.sh", Some("run, rewritten\n"))],
    );
    lab.branch("feat/other", "other.txt", "other\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/rewrite",
        "-r",
        "feat/other",
        "-m",
        "release",
    ]);
    std::fs::set_permissions(
        lab.work.join("shared.sh"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("clear the mode");
    lab.jj_work(["bookmark", "create", "release/2026-08-04", "-r", "@"]);
    lab.jj_work(["new"]);

    // When, then: the member is not carried: its line was rewritten by a
    // branch built on it, but its mode is gone.
    assert_member_carried(&lab, false);
}

#[test]
fn release_carries_reads_a_link_a_member_wrote() {
    // Given: the member writes a link; a branch built on it points the link
    // elsewhere; and two releases, one dropping the link by hand, one
    // holding it as the later branch left it.
    let lab = Lab::new();
    lab.jj_work(["new", "-r", "main@origin", "-m", MEMBER]);
    std::os::unix::fs::symlink("README.md", lab.work.join("link")).expect("write the link");
    lab.jj_work(["bookmark", "create", MEMBER, "-r", "@"]);
    lab.jj_work(["new", "-r", MEMBER, "-m", "feat/repoint"]);
    std::fs::remove_file(lab.work.join("link")).expect("remove the link");
    std::os::unix::fs::symlink("elsewhere", lab.work.join("link")).expect("repoint the link");
    lab.jj_work(["bookmark", "create", "feat/repoint", "-r", "@"]);
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        (MEMBER, "feat/other"),
        &[("link", None)],
    );

    // When, then: a release that dropped the link does not carry the member.
    let (home, _consumer) = release_test_home(&lab);
    assert_carried_in(&lab, &home, MEMBER, false);

    // And: a release holding the link as the later branch left it does not
    // carry it exactly either: a link is not text a rewrite is read in.
    lab.jj_work([
        "new",
        "-r",
        "feat/repoint",
        "-r",
        "feat/other",
        "-m",
        "release",
    ]);
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        "@",
        "--allow-backwards",
    ]);
    lab.jj_work(["new"]);
    assert_carried_in(&lab, &home, MEMBER, false);
}

#[test]
fn release_carries_answers_not_carried_for_a_member_whose_blank_lines_the_release_dropped() {
    // Given: the member writes lines with blank lines between them; the
    // release merges it with another branch and drops the blank lines.
    let lab = Lab::new();
    lab.branch(MEMBER, "shared.txt", "x\n\n\ny\n");
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        (MEMBER, "feat/other"),
        &[("shared.txt", Some("x\ny\n"))],
    );

    // When, then: the member is not carried.
    assert_member_carried(&lab, false);
}

#[test]
fn release_carries_answers_not_carried_for_a_member_whose_file_later_work_moved() {
    // Given: the member changes `README.md`, which the trunk holds; a branch
    // built on it moves the file to `MOVED.md`; the release merges that
    // branch with another and changes the moved file by hand.
    let lab = Lab::new();
    lab.branch(MEMBER, "README.md", "seed, as the member left it\n");
    child_by_hand(
        &lab,
        "feat/move",
        MEMBER,
        &[
            ("README.md", None),
            ("MOVED.md", Some("seed, as the member left it\n")),
        ],
    );
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/move", "feat/other"),
        &[("MOVED.md", Some("dropped\n"))],
    );

    // When, then: the member is not carried exactly: the file it changed is
    // gone where it changed it, and a rewrite is not read across a move.
    assert_member_carried(&lab, false);
}

/// `git` on the lab's work repository's own git store, with a scratch index
/// at `index`, returning trimmed stdout: jj writes no submodule, so a
/// fixture that holds one is written with git's plumbing.
fn git_store(lab: &Lab, index: &std::path::Path, args: &[&str]) -> String {
    let git_dir = lab.jj_work_output(["git", "root"]);
    let output = std::process::Command::new("git")
        .env("GIT_DIR", git_dir.trim())
        .env("GIT_INDEX_FILE", index)
        .env("GIT_AUTHOR_NAME", "Knives Lab")
        .env("GIT_AUTHOR_EMAIL", "knives-lab@example.test")
        .env("GIT_COMMITTER_NAME", "Knives Lab")
        .env("GIT_COMMITTER_EMAIL", "knives-lab@example.test")
        .args(args)
        .output()
        .expect("run git");
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A commit on `parents` whose tree is `from`'s with `sub` set to the
/// gitlink `link` (or removed for `None`), under bookmark `name`.
fn commit_with_link(lab: &Lab, name: &str, (from, link): (&str, Option<&str>), parents: &[&str]) {
    let scratch = tempfile::tempdir().expect("scratch index directory");
    let index = scratch.path().join("index");
    git_store(lab, &index, &["read-tree", from]);
    let cacheinfo = link.map(|link| format!("160000,{link},sub"));
    git_store(
        lab,
        &index,
        &cacheinfo.as_deref().map_or_else(
            || vec!["update-index", "--force-remove", "sub"],
            |cacheinfo| vec!["update-index", "--add", "--cacheinfo", cacheinfo],
        ),
    );
    let tree = git_store(lab, &index, &["write-tree"]);
    let mut args = vec!["commit-tree", tree.as_str(), "-m", name];
    for parent in parents {
        args.extend(["-p", parent]);
    }
    let commit = git_store(lab, &index, &args);
    git_store(
        lab,
        &index,
        &["update-ref", &format!("refs/heads/{name}"), &commit],
    );
    lab.jj_work(["git", "import"]);
}

#[test]
fn release_carries_reads_a_submodule_a_member_wrote() {
    // Given: the member adds a submodule; a branch built on it moves the
    // submodule to another commit; and two releases, one that dropped the
    // submodule, one that holds it as the later branch left it.
    let lab = Lab::new();
    let trunk = commit_at(&lab, "main@origin");
    lab.branch("feat/other", "other.txt", "other\n");
    let other = commit_at(&lab, "feat/other");
    let first = "1111111111111111111111111111111111111111";
    let second = "2222222222222222222222222222222222222222";
    commit_with_link(
        &lab,
        MEMBER,
        (trunk.as_str(), Some(first)),
        &[trunk.as_str()],
    );
    let member = commit_at(&lab, MEMBER);
    commit_with_link(
        &lab,
        "feat/repoint",
        (member.as_str(), Some(second)),
        &[member.as_str()],
    );
    let repoint = commit_at(&lab, "feat/repoint");
    commit_with_link(
        &lab,
        "release/2026-08-04",
        (other.as_str(), None),
        &[member.as_str(), other.as_str()],
    );

    // When, then: a release that dropped the submodule does not carry the
    // member.
    let (home, _consumer) = release_test_home(&lab);
    assert_carried_in(&lab, &home, MEMBER, false);

    // And: a release holding the submodule as the later branch left it does
    // not carry it exactly either: a submodule is not text a rewrite is
    // read in.
    commit_with_link(
        &lab,
        "release/2026-08-04",
        (other.as_str(), Some(second)),
        &[repoint.as_str(), other.as_str()],
    );
    assert_carried_in(&lab, &home, MEMBER, false);
}

#[test]
fn release_carries_answers_carried_exact_against_the_trunk_for_a_merged_branch() {
    // Given: alpha merged upstream with a merge commit, so its tip is an
    // ancestor of the trunk, and no release exists.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let alpha = commit_at(&lab, "feat/alpha");
    let trunk = commit_at(&lab, "main@upstream");
    let (home, _consumer) = release_test_home(&lab);

    // When: alpha is checked.
    let output = knives_release(&lab, &home, &["members", "--carries", "feat/alpha"]);

    // Then: the trunk, which knives does not compose, carries it by ancestry
    // alone, with the tip as the evidence.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "carried-exact      main@upstream @ {}  (evidence {}: tip is an ancestor)",
            trunk.short(),
            alpha.short()
        )),
        "{stdout}"
    );
}

#[test]
fn release_carries_stops_before_superseded_targets_when_live_release_carries() {
    // Given: alpha is carried in both the previous cut and its live successor,
    // with the previous cut restored as a historical remote target afterward.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let historical = commit_at(&lab, "release/2026-08-04");
    lab.jj_work([
        "bookmark",
        "create",
        "history/alpha-release",
        "-r",
        historical.as_str(),
    ]);
    lab.push_branch("history/alpha-release");
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(second.status.success(), "{second:?}");
    publish_remote_bookmark(&lab, "history/alpha-release@origin", "release/2026-08-04");

    // When: carries finds alpha in the live release or trunk census.
    let output = knives_release(&lab, &home, &["members", "--carries", "feat/alpha"]);

    // Then: that safe answer does not probe or print stale release targets.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("carried-exact      release/2026-08-05"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("release/2026-08-04"),
        "safe results must not include superseded probes: {stdout}"
    );
}

#[test]
fn release_carries_answers_carried_rebased_for_a_rewritten_member_the_release_holds() {
    // Given: a member rebased onto the advanced trunk and adapted there, then
    // advanced onto the release; the recorded commit (the pre-rebase one) is
    // asked about. Its tree no longer replays cleanly, but the release holds a
    // rewrite of the same change.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let recorded = commit_at(&lab, "feat/alpha");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work(["rebase", "-b", "feat/alpha", "-d", "main@upstream"]);
    lab.jj_work(["new", "feat/alpha"]);
    std::fs::write(
        lab.work.join("alpha.txt"),
        "alpha, adapted to the new trunk\n",
    )
    .expect("adapt the member on its new base");
    lab.jj_work(["squash", "--into", "feat/alpha"]);
    let rewritten = commit_at(&lab, "feat/alpha");
    let advanced = knives_release(&lab, &home, &["advance", "feat/alpha"]);
    assert!(advanced.status.success(), "{advanced:?}");

    // When: the pre-rebase commit is checked against the release.
    let output = knives_release(&lab, &home, &["members", "--carries", recorded.as_str()]);

    // Then: carried, in rebased form, with the rewrite as the evidence — and
    // the exit says carried, so a deletion-safety reader is not sent to judge
    // by eye.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("carried-rebased    release/2026-08-04"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("evidence {}", rewritten.short())),
        "{stdout}"
    );
}

#[test]
fn release_carries_answers_not_carried_for_a_member_the_release_holds_only_an_older_version_of() {
    // Given: a cut of alpha; alpha then changed in place (`jj squash` into
    // it, which keeps its change id), so jj rebases the cut onto it; then the
    // release's bookmark back at the cut, whose parent is the older alpha.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let cut = commit_at(&lab, "release/2026-08-04");
    let older = commit_at(&lab, "feat/alpha");
    lab.jj_work(["new", "-r", "feat/alpha"]);
    std::fs::write(lab.work.join("more.txt"), "more of alpha\n").expect("write more.txt");
    lab.jj_work(["squash", "--into", "feat/alpha"]);
    lab.jj_work(["new", "-r", "main@origin"]);
    assert_ne!(
        commit_at(&lab, "feat/alpha"),
        older,
        "the squash rewrote alpha"
    );
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        cut.as_str(),
        "--allow-backwards",
    ]);

    // When: alpha is checked against the release and the trunk.
    let output = knives_release(&lab, &home, &["members", "--carries", "feat/alpha"]);

    // Then: not carried: the older alpha shares its change id, but it is
    // what the newer one replaced, and it lacks `more.txt`.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success(), "{stdout}");
    assert!(stdout.contains("NOT carried"), "{stdout}");
    assert!(!stdout.contains("carried-rebased"), "{stdout}");
}

#[test]
fn release_carries_answers_not_carried_for_outside_work() {
    // Given: a release cut carrying alpha and an independent beta branch.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(cut.status.success(), "{cut:?}");
    lab.branch("feat/beta", "beta.txt", "beta\n");

    // When: beta is checked against every release target and the trunk.
    let output = knives_release(&lab, &home, &["members", "--carries", "feat/beta"]);

    // Then: no safe target carries it, so the answer names the real remaining diff.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("NOT carried"), "{stdout}");
}

#[test]
fn members_with_a_target_checks_only_the_requested_target() {
    // Given: alpha is in the release but absent from the upstream trunk.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(cut.status.success(), "{cut:?}");

    // When: the explicit target is the upstream trunk.
    let output = knives_release(
        &lab,
        &home,
        &["members", "main@upstream", "--carries", "feat/alpha"],
    );

    // Then: the live release cannot make a single-target trunk query safe.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.contains("NOT carried        main@upstream"),
        "{stdout}"
    );
    assert!(!stdout.contains("release/2026-08-04"), "{stdout}");
}

#[test]
fn members_with_a_superseded_target_exits_successfully_when_it_carries() {
    // Given: alpha was carried by a release that later became superseded.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let historical = commit_at(&lab, "release/2026-08-04");
    lab.jj_work([
        "bookmark",
        "create",
        "history/alpha-release",
        "-r",
        historical.as_str(),
    ]);
    lab.push_branch("history/alpha-release");
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(second.status.success(), "{second:?}");
    publish_remote_bookmark(&lab, "history/alpha-release@origin", "release/2026-08-04");

    // When: the explicit target is the known, historical release.
    let output = knives_release(
        &lab,
        &home,
        &[
            "members",
            "release/2026-08-04@origin",
            "--carries",
            "feat/alpha",
        ],
    );

    // Then: an explicit REF reports the direct target verdict, not its safe-delete role.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("carried-exact      release/2026-08-04@origin"),
        "{stdout}"
    );
}

#[test]
fn release_carries_answers_against_the_trunk_when_no_release_exists() {
    // Given: a branch but no release in hand.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: carries has no explicit target.
    let output = knives_release(&lab, &home, &["members", "--carries", "feat/alpha"]);

    // Then: the orphan question is answered against the upstream trunk.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("main@upstream"), "{stdout}");
    assert!(stdout.contains("NOT carried"), "{stdout}");
}

#[test]
fn release_carries_reports_carried_rewritten_for_a_squash_landed_branch() {
    // Given: alpha is squash-landed, so the trunk has its content but not its tip.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);
    let alpha = commit_at(&lab, "feat/alpha");
    let trunk = commit_at(&lab, "main@upstream");
    assert!(
        !Repo::open(&lab.work)
            .expect("open after squash merge")
            .is_ancestor(&alpha, &trunk)
            .expect("ancestry answerable"),
        "fixture must use a rewritten trunk commit"
    );
    let (home, _consumer) = release_test_home(&lab);

    // When: alpha is checked without any release.
    let output = knives_release(&lab, &home, &["members", "--carries", "feat/alpha"]);

    // Then: the trunk's tree-content evidence proves its rewritten carriage.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("carried-rewritten  main@upstream"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&trunk.as_str().chars().take(12).collect::<String>()),
        "{stdout}"
    );
}

#[test]
fn carries_superseded_only_carriage_is_findings() {
    // Given: a published cut carrying alpha survives at origin after the local
    // release drops it and the next cut becomes live.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let original = commit_at(&lab, "release/2026-08-04");
    lab.jj_work([
        "bookmark",
        "create",
        "history/alpha-release",
        "-r",
        original.as_str(),
    ]);
    lab.push_branch("history/alpha-release");
    let dropped = knives_release(
        &lab,
        &home,
        &[
            "drop",
            "feat/alpha",
            "--why",
            "superseded release preserves it",
        ],
    );
    assert!(dropped.status.success(), "{dropped:?}");
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05", "--allow-drop"]);
    assert!(second.status.success(), "{second:?}");

    // Publish the preserved historical commit under its release name only after
    // the successor cut has passed the duplicate-release gate.
    let run_in_second = |args: &[&str]| {
        let command = Command::new("jj")
            .args(args)
            .current_dir(&lab.second)
            .output()
            .expect("run jj in second clone");
        assert!(
            command.status.success(),
            "jj {args:?} failed: {}",
            String::from_utf8_lossy(&command.stderr)
        );
    };
    run_in_second(&["git", "fetch", "--remote", "origin"]);
    run_in_second(&[
        "bookmark",
        "create",
        "release/2026-08-04",
        "-r",
        "history/alpha-release@origin",
    ]);
    run_in_second(&[
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        "release/2026-08-04",
    ]);
    lab.jj_work(["git", "fetch", "--remote", "origin"]);

    // When: bare carries finds alpha only in the historical remote cut.
    let output = knives_release(&lab, &home, &["members", "--carries", "feat/alpha"]);

    // Then: the historical row is visible, but it cannot make the result safe.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.contains("carried-exact      release/2026-08-04@origin"),
        "{stdout}"
    );
    assert!(
        stdout.contains("NOT carried        release/2026-08-05"),
        "{stdout}"
    );
}

/// A release history with one live and one superseded cut, plus one branch
/// deliberately absent from both. The historical cut remains only as an origin
/// ref, so it is a census target without becoming a maintained branch itself.
fn census_lab() -> (Lab, tempfile::TempDir) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let historical = commit_at(&lab, "release/2026-08-04");
    lab.jj_work([
        "bookmark",
        "create",
        "history/alpha-release",
        "-r",
        historical.as_str(),
    ]);
    lab.push_branch("history/alpha-release");
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(second.status.success(), "{second:?}");
    publish_remote_bookmark(&lab, "history/alpha-release@origin", "release/2026-08-04");
    lab.jj_work(["bookmark", "forget", "history/alpha-release"]);
    lab.branch("feat/beta", "beta.txt", "beta\n");
    (lab, home)
}

fn census_block<'a>(stdout: &'a str, branch: &str) -> &'a str {
    let wanted = format!("  {branch} @");
    stdout
        .split("\n\n")
        .find(|block| block.starts_with(&wanted))
        .unwrap_or_else(|| panic!("no census block for {branch}: {stdout}"))
}

#[test]
fn census_finds_the_orphan_branch() {
    // Given: alpha is in the live cut and beta is independent; the preceding
    // dated cut survives only as a superseded origin target.
    let (lab, home) = census_lab();

    // When: census asks only local carriage questions, so PR state is explicitly unknown.
    let output = knives_release(&lab, &home, &["members", "--census", "--no-github"]);

    // Then: a carried member names its live carrier but does not spend a
    // superseded probe, while a non-carried member proves the negative against
    // every target, including the historical release.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        !stdout
            .split("\n\n")
            .any(|block| block.starts_with("  main @")),
        "the upstream trunk is a target, never a maintained-branch row: {stdout}"
    );
    let alpha = census_block(&stdout, "feat/alpha");
    assert!(
        alpha.contains("carried-exact      release/2026-08-05"),
        "{alpha}"
    );
    assert!(
        !alpha.contains("release/2026-08-04"),
        "a live-carried row must not probe superseded targets: {alpha}"
    );
    let beta = census_block(&stdout, "feat/beta");
    for target in [
        "release/2026-08-05",
        "main@upstream",
        "release/2026-08-04@origin",
    ] {
        assert!(
            beta.contains(&format!("NOT carried        {target}")),
            "beta must be checked against {target}: {beta}"
        );
    }
    assert!(
        stdout.contains("orphans: not carried anywhere (pull request state unknown)\n  feat/beta"),
        "{stdout}"
    );
}

#[test]
fn census_marks_unknown_pull_orphans_as_unanswered_in_json() {
    // Given: beta is locally uncarried and pull-request lookup is deliberately skipped.
    let (lab, home) = census_lab();

    // When: the census is emitted as its machine report.
    let output = knives_release_json(&lab, &home, &["members", "--census", "--no-github"]);

    // Then: the qualified text listing remains actionable, but JSON cannot
    // represent beta as a pull-safe, proven orphan.
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse census JSON");
    let beta = report["rows"]
        .as_array()
        .expect("branch rows")
        .iter()
        .find(|row| row["branch"] == "feat/beta")
        .expect("beta row");
    assert!(beta["orphan"].is_null(), "{report}");
    assert!(
        report["orphans"]
            .as_array()
            .expect("qualified orphan listing")
            .iter()
            .any(|orphan| orphan == "feat/beta"),
        "{report}"
    );
    assert_eq!(output.status.code(), Some(3), "{report}");
}

#[test]
fn census_respects_an_open_pull() {
    // Given: beta's content remains outside every release but the forge says its
    // branch has an open pull request.
    let (lab, home) = census_lab();
    let pulls = format!("[{}]", pull_record(17, "OPEN", "feat/beta", None));

    // When: the real CLI completes one forge snapshot for the census.
    let output = release_with_snapshot_forge(ReleaseWithSnapshotForgeInput {
        lab: &lab,
        home: &home,
        pulls: &pulls,
        withheld_facts: &[],
        args: &["members", "--census"],
        output: ReleaseOutput::Json,
    });

    // Then: the branch association is retained in the report and forbids an
    // orphan result despite every target being non-carried.
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse census JSON");
    let beta = report["rows"]
        .as_array()
        .expect("branch rows")
        .iter()
        .find(|row| row["branch"] == "feat/beta")
        .expect("beta row");
    assert_eq!(beta["in_open_pull"], true, "{report}");
    assert_eq!(beta["orphan"], false, "{report}");
    let alpha = report["rows"]
        .as_array()
        .expect("branch rows")
        .iter()
        .find(|row| row["branch"] == "feat/alpha")
        .expect("alpha row");
    assert_eq!(
        alpha["in_open_pull"], false,
        "a completed snapshot answers that an absent branch has no open pull: {report}"
    );
    assert_eq!(output.status.code(), Some(0), "{report}");
}

#[test]
fn census_withholds_a_selected_pull_fact_as_unanswered() {
    // Given: beta is locally uncarried and discovery names its open pull request.
    let (lab, home) = census_lab();
    let pulls = format!("[{}]", pull_record(17, "OPEN", "feat/beta", None));

    // When: the live batch withholds that selected pull request's fact.
    let output = release_with_snapshot_forge(ReleaseWithSnapshotForgeInput {
        lab: &lab,
        home: &home,
        pulls: &pulls,
        withheld_facts: &[17],
        args: &["members", "--census"],
        output: ReleaseOutput::Json,
    });

    // Then: discovery cannot make the pull state a deletion-safe answer.
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse census JSON");
    let beta = report["rows"]
        .as_array()
        .expect("branch rows")
        .iter()
        .find(|row| row["branch"] == "feat/beta")
        .expect("beta row");
    assert!(beta["in_open_pull"].is_null(), "{report}");
    assert!(beta["orphan"].is_null(), "{report}");
    assert_eq!(output.status.code(), Some(3), "{report}");
}

#[test]
fn census_keeps_local_orphans_when_the_forge_is_unavailable() {
    // Given: beta is locally uncarried and the forge refuses every request.
    let (lab, home) = census_lab();

    // When: census attempts the normal forge snapshot.
    let output = knives_release_with_failing_forge(&lab, &home, &["members", "--census"]);

    // Then: failure changes pull-request knowledge to unknown without hiding
    // the local orphan finding, and the unanswered deletion-safety check wins.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("pull request state unavailable:"),
        "{stdout}"
    );
    assert!(
        stdout.contains("orphans: not carried anywhere (pull request state unknown)\n  feat/beta"),
        "{stdout}"
    );
}

#[test]
fn census_excludes_anonymous_heads() {
    // Given: an unbookmarked commit with unique content, disconnected from the
    // working copy before the census runs.
    let (lab, home) = census_lab();
    lab.jj_work(["new", "main@upstream", "-m", "stranded"]);
    std::fs::write(lab.work.join("stranded.txt"), "stranded\n").expect("write stranded content");
    lab.jj_work(["new", "main@upstream"]);

    // When: the maintained-branch census runs without a pull-request lookup.
    let output = knives_release_json(&lab, &home, &["members", "--census", "--no-github"]);

    // Then: the unnamed head is audit population, not a census row or schema field.
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse census JSON");
    let branches = report["rows"]
        .as_array()
        .expect("branch rows")
        .iter()
        .map(|row| row["branch"].as_str().expect("branch name"))
        .collect::<Vec<_>>();
    assert_eq!(branches, ["feat/alpha", "feat/beta"], "{report}");
    assert!(
        report.get("anonymous").is_none(),
        "anonymous heads belong exclusively to audit: {report}"
    );
}

/// Run census without a forge so its locally discovered anonymous id can be
/// supplied as a pull request's exact head oid on a subsequent run.
fn knives_release_json(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    release_command(lab, home, ReleaseOutput::Json, args)
        .output()
        .expect("run knives release census")
}

/// Run census with a forge that fails before returning any data.
fn knives_release_with_failing_forge(
    lab: &Lab,
    home: &tempfile::TempDir,
    args: &[&str],
) -> std::process::Output {
    let shim = tempfile::tempdir().expect("create forge shim directory");
    install_failing_gh(shim.path(), &shim.path().join("calls.log"));
    release_command(lab, home, ReleaseOutput::Text, args)
        .env("XDG_CACHE_HOME", shim.path().join("cache"))
        .env("PATH", path_with_gh_shim(shim.path()))
        .output()
        .expect("run knives release census with a failing forge")
}
