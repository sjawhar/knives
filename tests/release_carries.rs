//! `knives release members`, its `--carries` question and the `--census`.
//!
//! Members counts the repository's parents and names who holds each; `--verify`
//! reports a dropped member's content. Carries compares a revision's net content
//! with each target, stops before superseded releases when a live one carries,
//! and answers against the trunk when no release exists. A release that names a
//! member as a parent carries it exactly only when it holds every line of the
//! member's own change from its fork point with the trunk, as `--verify` reads
//! the member's own lines: none lost, none held only by its words, and no
//! unresolved conflict in the change's paths. Any other ancestor of the
//! release is merged-then-edited, whatever the member records, and the
//! trunk carries any ancestor. The census finds orphan branches, respects
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
    Lab, ReleaseOutput, adopt, carried_row, census, census_block, commit_at, conflicts_of,
    edit_by_hand, extend_branch, home_after_first_cut, knives_release, label, missing_row,
    release_command, release_parents, release_test_home, stdout, text, trunk_with, verify,
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

    // When: each member is read by its own lines against the resolved release.
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
fn release_carries_answers_merged_then_edited_for_a_parent_whose_change_the_release_dropped() {
    // Given: a release that names beta as a parent but whose tree, edited by
    // hand, no longer holds beta's file; alpha's file it still holds.
    let (lab, home) = release_that_dropped_beta();
    let release = commit_at(&lab, "release/2026-08-04");
    let alpha = commit_at(&lab, "feat/alpha");

    // When: carries asks of each member.
    let beta_output = knives_release(&lab, &home, &["members", "--carries", "feat/beta"]);
    let alpha_output = knives_release(&lab, &home, &["members", "--carries", "feat/alpha"]);

    // Then: beta's ancestry does not make it carried: its commits are in the
    // release but its change is not, so it is merged-then-edited, a person's
    // to judge (exit 3), with the release as the evidence and the release's
    // own write named as what changed its file since.
    let beta = String::from_utf8_lossy(&beta_output.stdout);
    assert_eq!(beta_output.status.code(), Some(3), "{beta}");
    assert!(
        beta.contains(&format!(
            "merged-then-edited release/2026-08-04 @ {}  (evidence {}: tip is an ancestor, but \
             work since changed what it brought; judge by eye)",
            release.short(),
            release.short()
        )),
        "{beta}"
    );
    assert!(
        beta.contains(&format!("changed its files since (1): {}", release.short())),
        "{beta}"
    );
    // And: alpha, whose change the release's tree holds, is carried exactly,
    // and the reason says the release holds every line of its change, not
    // only that it is an ancestor.
    let alpha_stdout = String::from_utf8_lossy(&alpha_output.stdout);
    assert!(alpha_output.status.success(), "{alpha_stdout}");
    assert!(
        alpha_stdout.contains(&format!(
            "carried-exact      release/2026-08-04 @ {}  (evidence {}: tip is an ancestor and \
             the release holds every line of its change)",
            release.short(),
            alpha.short()
        )),
        "{alpha_stdout}"
    );
}

#[test]
fn census_answers_merged_then_edited_for_a_parent_whose_change_the_release_dropped() {
    // Given: the same release, beta a parent whose file its tree dropped.
    let (lab, home) = release_that_dropped_beta();

    // When: the census checks every maintained branch.
    let output = knives_release(&lab, &home, &["members", "--census", "--no-github"]);

    // Then: beta is merged-then-edited in the release that names it, so it
    // is no orphan and nothing escalates; alpha is carried exactly.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let beta = census_block(&stdout, "feat/beta");
    assert!(
        beta.contains("merged-then-edited release/2026-08-04"),
        "{beta}"
    );
    assert!(!beta.contains("carried-"), "{beta}");
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
    for (branch, on) in [
        ("feat/pr", "feat/base"),
        ("feat/other", "feat/base"),
        ("feat/later", "feat/pr"),
    ] {
        depends(&lab, &home, branch, on);
    }
    (lab, home)
}

#[test]
fn release_carries_answers_merged_then_edited_for_stacked_members_whose_lines_others_rewrote() {
    // Given: a release whose later members rewrote what `feat/base` and the
    // pull request into its branch, `feat/pr`, wrote, and which merged its
    // parents with no resolution of its own; each stacked branch records
    // the branch it was built on.
    let (lab, home) = release_with_stacked_members();

    // When, then: `feat/base`, `feat/pr` and `feat/later` read
    // merged-then-edited, exit 3: their commits are in the release, and
    // each one's change from the trunk holds a line another member
    // rewrote. A record of the branch each was built on decides nothing;
    // the answer names it.
    for name in ["feat/base", "feat/pr", "feat/later"] {
        assert_carried_in(&lab, &home, name, false);
    }
    // And: `feat/other`, whose change from the trunk is `feat/base`'s file
    // as `feat/other` rewrote it, which the release holds, is carried
    // exactly.
    assert_carried_in(&lab, &home, "feat/other", true);
    let later = knives_release(
        &lab,
        &home,
        &["members", "release/2026-08-04", "--carries", "feat/later"],
    );
    let stdout = String::from_utf8_lossy(&later.stdout);
    assert!(
        stdout.contains(
            "note: feat/later records it needs origin:acme/work:feat/pr; work there may account \
             for some of what changed, but a record is not read as carriage"
        ),
        "{stdout}"
    );
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
fn release_carries_answers_merged_then_edited_for_an_ancestor_a_hand_resolution_dropped() {
    // Given: the same release, edited by hand to drop the file `feat/pr`
    // wrote, which `feat/later` rewrote; both stay its ancestors.
    let (lab, home) = release_with_stacked_members();
    lab.jj_work(["edit", "release/2026-08-04"]);
    std::fs::remove_file(lab.work.join("pr.txt")).expect("drop pr.txt by hand");
    lab.jj_work(["new"]);

    // When, then: ancestry does not make either carried: the release's own
    // resolution dropped their change.
    for name in ["feat/pr", "feat/later"] {
        assert_carried_in(&lab, &home, name, false);
    }
}

/// The branch the release-resolution tests ask about.
const MEMBER: &str = "feat/member";
/// The release that the tests of the own-lines reading cut and ask about.
const CUT: &str = "release/2026-08-04";

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

/// [`assert_carried_in`] with a config home of its own.
fn assert_carried(lab: &Lab, name: &str, carried: bool) {
    let (home, _consumer) = release_test_home(lab);
    assert_carried_in(lab, &home, name, carried);
}

/// Ask whether the release carries `name`, an ancestor of it, by the
/// release's ref and by its commit, with the config `home`, and check both
/// answers: exit `0` and `carried-exact` when `carried`, else exit `3` and
/// `merged-then-edited` (its commits are in the release; what it brought was
/// changed since), never a carried verdict.
fn assert_carried_in(lab: &Lab, home: &tempfile::TempDir, name: &str, carried: bool) {
    let release = commit_at(lab, CUT);
    assert!(
        Repo::open(&lab.work)
            .expect("open the lab")
            .is_ancestor(&commit_at(lab, name), &release)
            .expect("ancestry answerable"),
        "{name} must be an ancestor of the release"
    );
    for target in [CUT, release.as_str()] {
        let output = knives_release(lab, home, &["members", target, "--carries", name]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        if carried {
            assert_eq!(output.status.code(), Some(0), "{name} {target}: {stdout}");
            assert!(
                stdout.contains("carried-exact "),
                "{name} {target}: {stdout}"
            );
        } else {
            assert_eq!(output.status.code(), Some(3), "{name} {target}: {stdout}");
            assert!(
                stdout.contains("merged-then-edited "),
                "{name} {target}: {stdout}"
            );
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
fn release_carries_answers_merged_then_edited_for_a_member_the_release_merge_dropped() {
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
fn release_carries_answers_merged_then_edited_for_a_member_a_merge_below_the_release_dropped() {
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
fn release_carries_answers_merged_then_edited_for_a_member_whose_line_a_later_branch_rewrote() {
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
fn release_carries_answers_merged_then_edited_for_a_member_whose_line_a_merge_moved_to_another_file()
 {
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
fn release_carries_answers_merged_then_edited_for_a_member_a_resolution_discarded_for_the_other_side()
 {
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
fn release_carries_answers_merged_then_edited_for_a_member_a_branch_built_on_it_reverted() {
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
fn release_carries_answers_merged_then_edited_for_a_member_whose_deletion_a_merge_restored() {
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
fn release_carries_answers_merged_then_edited_for_a_member_whose_lines_a_merge_reordered() {
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
fn release_carries_answers_merged_then_edited_for_a_sibling_whose_rewrite_the_release_discarded() {
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

    // When, then: `feat/alpha`, whose line the resolution discarded for a
    // sibling's, is not carried; `feat/beta` is; and `feat/base`, whose line
    // a branch built on it rewrote, is not carried exactly either.
    assert_carried_in(&lab, &home, "feat/alpha", false);
    assert_carried_in(&lab, &home, "feat/beta", true);
    assert_carried_in(&lab, &home, "feat/base", false);
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_member_a_resolution_discarded_before_a_later_edit()
 {
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
/// it, writes `member`; a branch built on the member writes `later`; the
/// release merges that branch with another. Asks the release about the
/// member and checks it is carried exactly or not.
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
    assert_member_carried(&lab, carried);
}

#[test]
fn release_carries_answers_merged_then_edited_for_an_added_line_a_later_branch_rewrote() {
    // The member adds a line before `keep`; a later branch rewrites the
    // added line and keeps `keep`: the release lacks the line as it was
    // added.
    adjacent_edit(
        ("keep\n", "added\nkeep\n", "rewritten-added\nkeep\n"),
        false,
    );
}

#[test]
fn release_carries_answers_merged_then_edited_for_an_added_line_beside_a_rewrite_of_base_text() {
    // The member adds a line before `keep`; a later branch keeps the added
    // line and rewrites `keep`: from the trunk, the member's change is the
    // whole file as it left it, and the release holds that file edited.
    adjacent_edit(
        ("keep\n", "added\nkeep\n", "added\nrewritten-keep\n"),
        false,
    );
}

#[test]
fn release_carries_answers_merged_then_edited_for_an_added_line_a_later_branch_reverted() {
    // The member adds a line; a later branch removes it again.
    adjacent_edit(("keep\n", "added\nkeep\n", "keep\n"), false);
}

#[test]
fn release_carries_answers_merged_then_edited_for_an_added_line_dropped_beside_a_rewrite_of_base_text()
 {
    // The member adds a line before `keep`; a later branch drops the added
    // line and rewrites `keep`: the rewrite beside it hides nothing.
    adjacent_edit(("keep\n", "added\nkeep\n", "rewritten-keep\n"), false);
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_replacement_reverted_beside_a_rewrite_of_base_text()
 {
    // The member replaces `old` with `new`; a later branch puts `old` back
    // and rewrites `keep`.
    adjacent_edit(
        ("old\nkeep\n", "new\nkeep\n", "old\nrewritten-keep\n"),
        false,
    );
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_deletion_restored_beside_a_rewrite_of_base_text()
 {
    // The member deletes `deleted`; a later branch puts it back and rewrites
    // `keep`.
    adjacent_edit(
        ("deleted\nkeep\n", "keep\n", "deleted\nrewritten-keep\n"),
        false,
    );
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_member_deleted_then_recreated_by_later_work() {
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
fn release_carries_answers_carried_exact_for_a_put_back_line_of_a_file_the_change_adds_whole() {
    // Given: `feat/del`, stacked on `feat/base`, deletes the line `b`; the
    // release merges it with another branch and puts `b` back by hand.
    let lab = Lab::new();
    lab.branch("feat/base", "f.txt", "a\nb\nc\n");
    child_by_hand(&lab, "feat/del", "feat/base", &[("f.txt", Some("a\nc\n"))]);
    lab.branch("feat/x", "x.txt", "x\n");
    merge_by_hand(
        &lab,
        CUT,
        ("feat/del", "feat/x"),
        &[("f.txt", Some("a\nb\nc\n"))],
    );
    let (home, _consumer) = release_test_home(&lab);

    // When, then: from the trunk, `f.txt` is one file `feat/del` added as
    // `a`, `c`, and the release holds both lines with `b` between them, so
    // every command reads `feat/del` carried. A member that deletes a line
    // of a file that its fork point holds is read missing where the release
    // puts the line back (the deleter of the guard tests).
    assert_every_command_reads_carried(&lab, &home, &["feat/del"]);
}

#[test]
fn release_carries_answers_carried_exact_for_a_member_that_deleted_a_line_it_merged_in() {
    // Given: `feat/del`, built on `feat/base`, merges in `feat/u`, which adds
    // `u.txt`, then deletes a line of `u.txt`; the release merges `feat/del`
    // with another branch and puts the line back.
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
        CUT,
        ("feat/del", "feat/x"),
        &[("u.txt", Some("x\ny\n"))],
    );
    let (home, _consumer) = release_test_home(&lab);

    // When, then: every command reads `feat/del` carried. From the trunk,
    // `u.txt` is one file that `feat/del` added as `x`, so the line it
    // deleted after merging it in is no hunk of its own change, and the
    // release holds `x`.
    assert_every_command_reads_carried(&lab, &home, &["feat/del"]);
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
fn release_carries_answers_merged_then_edited_for_a_stacked_member_whatever_it_records() {
    // Given: a member stacked on `feat/line`, and a release that changed by
    // hand a file `feat/line` wrote and the member never touched.
    let lab = member_on_a_line_whose_file_the_release_changed();
    let (home, _consumer) = release_test_home(&lab);
    let release = commit_at(&lab, "release/2026-08-04");
    let ask = |home: &tempfile::TempDir| {
        let output = knives_release(
            &lab,
            home,
            &["members", "release/2026-08-04", "--carries", "feat/member"],
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert_eq!(output.status.code(), Some(3), "{stdout}");
        assert!(
            stdout.contains(&format!(
                "merged-then-edited release/2026-08-04 @ {}",
                release.short()
            )),
            "{stdout}"
        );
        assert!(!stdout.contains("carried-"), "{stdout}");
        stdout
    };

    // When, then: with no record, it is merged-then-edited: from the trunk
    // its change holds `feat/line`'s line, which the release rewrote; the
    // answer lists the release as the commit that changed its files since,
    // and names no record.
    let stdout = ask(&home);
    assert!(
        stdout.contains(&format!("changed its files since (1): {}", release.short())),
        "{stdout}"
    );
    assert!(!stdout.contains("records it needs"), "{stdout}");

    // And: a record of the branch it was built on, or of two, gives the same
    // verdict; the answer only names what it records.
    depends(&lab, &home, "feat/member", "feat/line");
    let stdout = ask(&home);
    assert!(
        stdout.contains(
            "note: feat/member records it needs origin:acme/work:feat/line; work there may \
             account for some of what changed, but a record is not read as carriage"
        ),
        "{stdout}"
    );
    depends(&lab, &home, "feat/member", "feat/other");
    let stdout = ask(&home);
    assert!(
        stdout.contains(
            "note: feat/member records it needs origin:acme/work:feat/line, \
             origin:acme/work:feat/other;"
        ),
        "{stdout}"
    );
}

#[test]
fn release_carries_answers_carried_exact_for_a_stacked_member_whose_whole_line_the_release_holds() {
    // Given: the same member, merged with another branch by a release that
    // changes nothing by hand.
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
        &[],
    );

    // When, then: its change from the trunk, `feat/line`'s commit with its
    // own, is held whole by the release, so it is carried exactly with no
    // record.
    assert_carried(&lab, "feat/member", true);
}

#[test]
fn release_carries_answers_merged_then_edited_for_both_members_of_a_cut_conflict_resolved_to_neither()
 {
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
fn release_carries_answers_merged_then_edited_for_members_a_resolution_discarded_under_a_later_commit()
 {
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

/// A knives cut, [`CUT`], of `members`, each `(branch, file, content)` a
/// branch on a trunk that holds `trunk`, which leaves `file` conflicted. The
/// conflict is resolved by hand to `resolution`, and `knives release adopt`
/// records it.
fn resolved_cut(
    trunk: &[(&str, &str)],
    members: &[(&str, &str, &str)],
    (file, resolution): (&str, &str),
) -> (Lab, tempfile::TempDir) {
    let lab = trunk_with(trunk);
    for (branch, path, content) in members {
        lab.branch(branch, path, content);
    }
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", CUT]);
    assert!(
        stdout(&cut).contains(&format!("cut {CUT} as")),
        "{}",
        text(&cut)
    );
    assert_eq!(conflicts_of(&lab, CUT), [file], "{}", text(&cut));
    edit_by_hand(&lab, CUT, &[(file, resolution)]);
    adopt(&lab, &home, CUT, &format!("resolve {file} by hand"));
    assert_eq!(
        conflicts_of(&lab, CUT),
        Vec::<String>::new(),
        "the adopt left a conflict"
    );
    (lab, home)
}

/// Each of `names` reads carried in [`CUT`] by every command that reads it:
/// `members --verify` by the member's own lines (exit 0), `--carries` and the
/// census (carried-exact).
fn assert_every_command_reads_carried(lab: &Lab, home: &tempfile::TempDir, names: &[&str]) {
    let verified = verify(lab, home, CUT);
    let out = stdout(&verified);
    assert_eq!(verified.status.code(), Some(0), "{}", text(&verified));
    for name in names {
        assert!(out.contains(&carried_row(&label(lab, name))), "{out}");
        assert_carried_in(lab, home, name, true);
    }
    let census_out = census(lab, home);
    for name in names {
        assert!(
            census_block(&census_out, name).contains(&format!("carried-exact      {CUT} @ ")),
            "{census_out}"
        );
    }
}

/// `name` reads missing in [`CUT`] by `members --verify` (exit 1), with
/// `lost`, the start of a line it names under the member's row, and
/// merged-then-edited by `--carries` (exit 3) and the census.
fn assert_missing_and_merged_then_edited(
    lab: &Lab,
    home: &tempfile::TempDir,
    name: &str,
    lost: &str,
) {
    let verified = verify(lab, home, CUT);
    let out = stdout(&verified);
    assert_eq!(verified.status.code(), Some(1), "{}", text(&verified));
    assert!(out.contains(&missing_row(&label(lab, name))), "{out}");
    assert!(
        out.lines()
            .any(|line| line.starts_with("      ") && line.contains(lost)),
        "no line under the row names {lost}: {out}"
    );
    assert_carried_in(lab, home, name, false);
    let census_out = census(lab, home);
    let block = census_block(&census_out, name);
    assert!(
        block.contains(&format!("merged-then-edited {CUT} @ ")),
        "{census_out}"
    );
    assert!(!block.contains("carried-"), "{census_out}");
}

/// The file of [`bugs_cut`], which its members add entries to.
const BUGS: &str = "doc/bugs.md";
/// The entry that `feat/alpha` adds to [`BUGS`].
const ALPHA_ENTRY: &str = "- [alpha bug](#alpha-bug)";
/// The two entries that `feat/beta` adds to [`BUGS`].
const BETA_ENTRIES: [&str; 2] = [
    "- [beta bug](#beta-bug)",
    "- [beta second bug](#beta-second-bug)",
];

/// [`BUGS`], a list of known bugs, with `entries` after its first entry:
/// the place where each member of [`bugs_cut`] adds its own.
fn bugs_with(entries: &[&str]) -> String {
    let mut bugs = String::from("# Known bugs\n\n- [first bug](#first-bug)\n");
    for entry in entries {
        bugs.push_str(entry);
        bugs.push('\n');
    }
    bugs.push_str("- [last bug](#last-bug)\n");
    bugs
}

/// [`resolved_cut`] of `feat/alpha`, which adds [`ALPHA_ENTRY`] to the
/// trunk's [`BUGS`], `feat/beta`, which adds [`BETA_ENTRIES`] at the same
/// place, and `feat/gamma`, which writes a file of its own: two members'
/// entries at one place conflict. `resolution` is the new text of [`BUGS`].
fn bugs_cut(resolution: &str) -> (Lab, tempfile::TempDir) {
    resolved_cut(
        &[(BUGS, bugs_with(&[]).as_str())],
        &[
            ("feat/alpha", BUGS, bugs_with(&[ALPHA_ENTRY]).as_str()),
            ("feat/beta", BUGS, bugs_with(&BETA_ENTRIES).as_str()),
            ("feat/gamma", "gamma.txt", "gamma\n"),
        ],
        (BUGS, resolution),
    )
}

#[test]
fn release_carries_answers_carried_exact_for_members_a_resolution_put_side_by_side() {
    // Given: a cut whose conflict in `doc/bugs.md`, two members' entries at
    // one place, was resolved by hand to every entry of both and adopted.
    let union = bugs_with(&[ALPHA_ENTRY, BETA_ENTRIES[0], BETA_ENTRIES[1]]);
    let (lab, home) = bugs_cut(&union);

    // When/Then: `members --verify` reads each member carried by its own
    // lines, and `--carries` and the census give the same answer: the
    // release holds every line of each member's change, and the other
    // member's entries beside them change none of its lines.
    assert_every_command_reads_carried(&lab, &home, &["feat/alpha", "feat/beta", "feat/gamma"]);
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_line_a_side_by_side_resolution_dropped() {
    // Given: the same cut, resolved by hand to alpha's entry and beta's first
    // entry beside it, without beta's second entry, and adopted.
    let (lab, home) = bugs_cut(&bugs_with(&[ALPHA_ENTRY, BETA_ENTRIES[0]]));

    // When/Then: `members --verify` reads beta missing and names the line it
    // lacks; `--carries` and the census read it merged-then-edited, never
    // carried. Alpha, whose entry the release holds, is carried by all three.
    assert_missing_and_merged_then_edited(
        &lab,
        &home,
        "feat/beta",
        "doc/bugs.md:5: added line lost ‹- [beta second bug](#beta-second-bug)›",
    );
    assert_carried_in(&lab, &home, "feat/alpha", true);
}

/// The file of [`guard_cut`].
const INDUCTIVE: &str = "kernel/inductive.ml";
/// The trunk's [`INDUCTIVE`]: a function whose `raise` line is followed by two
/// `let` lines.
const GUARD: &str = "let check_guard env t =\n  let fail () =\n    \
                     raise (CoFixGuardError (env,NotGuardedForm t)) in\n  \
                     let first = Array.length env in\n  let second = first + 1 in\n  fail ()\n";
/// `feat/deleter`'s [`INDUCTIVE`]: [`GUARD`] with the two `let` lines
/// deleted.
const GUARD_DELETED: &str = "let check_guard env t =\n  let fail () =\n    \
                             raise (CoFixGuardError (env,NotGuardedForm t)) in\n  fail ()\n";
/// `feat/inserter`'s [`INDUCTIVE`]: [`GUARD`] with the `raise` line rewritten,
/// its ` in` removed, and two definitions inserted after it.
const GUARD_INSERTED: &str = "let check_guard env t =\n  let fail () =\n    \
                              raise (CoFixGuardError (env,NotGuardedForm t))\n  \
                              and note () = Feedback.msg_debug (str \"guard failed\")\n  \
                              and trace () = Feedback.msg_debug (str \"guard traced\") in\n  \
                              let first = Array.length env in\n  let second = first + 1 in\n  \
                              fail ()\n";
/// [`INDUCTIVE`] with both changes: the inserter's lines and the deleter's
/// deletion.
const GUARD_BOTH: &str = "let check_guard env t =\n  let fail () =\n    \
                          raise (CoFixGuardError (env,NotGuardedForm t))\n  \
                          and note () = Feedback.msg_debug (str \"guard failed\")\n  \
                          and trace () = Feedback.msg_debug (str \"guard traced\") in\n  \
                          fail ()\n";

/// [`resolved_cut`] of `feat/deleter`, which deletes the two `let` lines that
/// follow the `raise` line of [`GUARD`], and `feat/inserter`, which rewrites
/// the `raise` line and inserts lines right after it: the member's deletion
/// stands next to another member's edit, so the two conflict. `resolution`
/// is the new text of [`INDUCTIVE`].
fn guard_cut(resolution: &str) -> (Lab, tempfile::TempDir) {
    resolved_cut(
        &[(INDUCTIVE, GUARD)],
        &[
            ("feat/deleter", INDUCTIVE, GUARD_DELETED),
            ("feat/inserter", INDUCTIVE, GUARD_INSERTED),
        ],
        (INDUCTIVE, resolution),
    )
}

#[test]
fn release_carries_answers_carried_exact_for_a_deletion_beside_another_members_rewrite() {
    // Given: a cut whose conflict, the deleter's deletion of two lines
    // right after the line that the inserter rewrote, was resolved by hand
    // to the inserter's lines and the deletion, and adopted.
    let (lab, home) = guard_cut(GUARD_BOTH);

    // When/Then: `members --verify` reads both members carried, and
    // `--carries` and the census give the same answer: the release holds
    // every line that each change adds and removes, and the other member's
    // edit beside the deletion is no line of the deleter's.
    assert_every_command_reads_carried(&lab, &home, &["feat/deleter", "feat/inserter"]);
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_hunk_the_release_put_back_in_a_trunk_file() {
    // Given: the same cut, resolved by hand to the inserter's text alone,
    // which holds the two lines that the deleter removed, and adopted.
    let (lab, home) = guard_cut(GUARD_INSERTED);

    // When/Then: `members --verify` reads the deleter missing and names each
    // line that was put back; `--carries` and the census read it
    // merged-then-edited. The inserter's lines are all held.
    assert_missing_and_merged_then_edited(
        &lab,
        &home,
        "feat/deleter",
        "kernel/inductive.ml:4: removed line put back ‹",
    );
    assert_carried_in(&lab, &home, "feat/inserter", true);
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_line_the_release_put_back_from_a_hunk() {
    // Given: the member removes `b` and `c` from a file that the trunk holds;
    // the release merges it with another branch and puts `b` back by hand,
    // not `c`.
    let lab = trunk_with(&[("f.txt", "a\nb\nc\nd\n")]);
    lab.branch(MEMBER, "f.txt", "a\nd\n");
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        CUT,
        (MEMBER, "feat/other"),
        &[("f.txt", Some("a\nb\nd\n"))],
    );
    let (home, _consumer) = release_test_home(&lab);

    // When/Then: `b` holds a letter, so putting it back undoes the member's
    // removal of it, whatever happened to `c`: `members --verify` reads the
    // member missing and names `b`, and `--carries` and the census read it
    // merged-then-edited.
    assert_missing_and_merged_then_edited(
        &lab,
        &home,
        MEMBER,
        "f.txt:2: removed line put back ‹b›",
    );
}

#[test]
fn release_carries_answers_carried_exact_for_a_lone_brace_put_back_from_a_hunk() {
    // Given: the member removes a call and the `}` after it; the release
    // merges it with another branch and puts the `}` back by hand, not the
    // call (K11 (9)).
    let lab = trunk_with(&[("f.txt", "a\n  y();\n}\nd\n")]);
    lab.branch(MEMBER, "f.txt", "a\nd\n");
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        CUT,
        (MEMBER, "feat/other"),
        &[("f.txt", Some("a\n}\nd\n"))],
    );
    let (home, _consumer) = release_test_home(&lab);

    // When, then: every command reads the member carried: a line of
    // punctuation alone counts as put back only with its whole hunk.
    assert_every_command_reads_carried(&lab, &home, &[MEMBER]);
}

/// Rocq's `dev/doc/critical-bugs.md`, as `tests/fixtures/side_by_side` holds
/// it.
const CRITICAL_BUGS: &str = "dev/doc/critical-bugs.md";
const CRITICAL_BUGS_BASE: &str = include_str!("fixtures/side_by_side/critical-bugs/base.md");
const CRITICAL_BUGS_MEMBER: &str = include_str!("fixtures/side_by_side/critical-bugs/member.md");
const CRITICAL_BUGS_RELEASE: &str = include_str!("fixtures/side_by_side/critical-bugs/release.md");

/// [`MEMBER`], which writes [`CRITICAL_BUGS_MEMBER`] on a trunk that holds
/// [`CRITICAL_BUGS_BASE`], and `feat/other`, merged by hand as [`CUT`]
/// with [`CRITICAL_BUGS`] as `release`.
fn critical_bugs_release(release: &str) -> Lab {
    let lab = trunk_with(&[(CRITICAL_BUGS, CRITICAL_BUGS_BASE)]);
    lab.branch(MEMBER, CRITICAL_BUGS, CRITICAL_BUGS_MEMBER);
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        CUT,
        (MEMBER, "feat/other"),
        &[(CRITICAL_BUGS, Some(release))],
    );
    lab
}

#[test]
fn release_carries_answers_carried_exact_for_real_entries_a_resolution_kept_beside_others() {
    // Given: the member adds one entry to the table of contents of Rocq's
    // `critical-bugs.md` and three below it; a release holds the member's
    // entries first at each place, with other members' entries after them,
    // as a hand resolution of the conflicts there wrote them.
    let lab = critical_bugs_release(CRITICAL_BUGS_RELEASE);
    let (home, _consumer) = release_test_home(&lab);

    // When/Then: every command reads the member carried.
    assert_every_command_reads_carried(&lab, &home, &[MEMBER]);
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_real_entry_the_release_lacks() {
    // Given: the same release without the member's entry in the table of
    // contents.
    let entry = "      - [incorrect variance inference for a constructor";
    let lacking: String = CRITICAL_BUGS_RELEASE
        .lines()
        .filter(|line| !line.starts_with(entry))
        .flat_map(|line| [line, "\n"])
        .collect();
    assert_eq!(
        lacking.lines().count() + 1,
        CRITICAL_BUGS_RELEASE.lines().count(),
        "the fixture lost one line"
    );
    let lab = critical_bugs_release(&lacking);
    let (home, _consumer) = release_test_home(&lab);

    // When/Then: `members --verify` reads the member missing and names the
    // entry; `--carries` and the census read it merged-then-edited.
    assert_missing_and_merged_then_edited(
        &lab,
        &home,
        MEMBER,
        "dev/doc/critical-bugs.md:4: added line lost ‹",
    );
}

/// [`MEMBER`] and `feat/other`, merged by hand as [`CUT`] with `file`
/// written as `content` and its mode set to `mode`. The release moves there
/// from wherever it was.
fn release_of_member_and_other(lab: &Lab, (file, content): (&str, &str), mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    lab.jj_work(["new", "-r", MEMBER, "-r", "feat/other", "-m", "release"]);
    let path = lab.work.join(file);
    std::fs::write(&path, content).expect("resolve by hand");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("set the mode");
    lab.jj_work(["bookmark", "set", CUT, "-r", "@", "--allow-backwards"]);
    lab.jj_work(["new"]);
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_mode_a_side_by_side_release_dropped() {
    // Given: the trunk holds `shared.sh`, not executable; the member adds a
    // line to it and makes it executable; `feat/other` adds a line at the
    // same place; a release made by hand holds both lines side by side and
    // clears the mode.
    let lab = trunk_with(&[("shared.sh", "first\nlast\n")]);
    member_with(
        &lab,
        &[("shared.sh", Some("first\nmember\nlast\n"))],
        Some(0o755),
    );
    lab.branch("feat/other", "shared.sh", "first\nother\nlast\n");
    let both = ("shared.sh", "first\nmember\nother\nlast\n");
    release_of_member_and_other(&lab, both, 0o644);
    let (home, _consumer) = release_test_home(&lab);

    // When, then: the member is not carried exactly: the release holds its
    // line, but not its mode, and `members --verify` names the mode.
    assert_carried_in(&lab, &home, MEMBER, false);
    let verified = verify(&lab, &home, CUT);
    assert_eq!(verified.status.code(), Some(1), "{}", text(&verified));
    assert!(
        stdout(&verified).contains("shared.sh: mode differs from the member's (executable yes)"),
        "{}",
        text(&verified)
    );

    // And: a release that holds the mode too carries it exactly.
    release_of_member_and_other(&lab, both, 0o755);
    assert_carried_in(&lab, &home, MEMBER, true);
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_binary_file_a_release_holds_side_by_side() {
    // Given: the trunk holds `shared.bin`, whose NUL byte makes it no text;
    // the member and `feat/other` each add a line at one place of it; a
    // release made by hand holds both lines side by side.
    let lab = trunk_with(&[("shared.bin", "first\n\0\nlast\n")]);
    member_with(
        &lab,
        &[("shared.bin", Some("first\n\0\nmember\nlast\n"))],
        None,
    );
    lab.branch("feat/other", "shared.bin", "first\n\0\nother\nlast\n");
    release_of_member_and_other(
        &lab,
        ("shared.bin", "first\n\0\nmember\nother\nlast\n"),
        0o644,
    );

    // When, then: the member is not carried exactly: a file that is no text
    // is not read by its lines.
    assert_member_carried(&lab, false);
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_member_whose_first_commit_the_release_dropped()
{
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

    // When, then: `feat/x` is not carried, and neither is `feat/y`, whose
    // change from the trunk holds X1's file too.
    let (home, _consumer) = release_test_home(&lab);
    assert_carried_in(&lab, &home, "feat/x", false);
    assert_carried_in(&lab, &home, "feat/y", false);
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_member_whose_base_merged_a_branch_off_its_first_commit()
 {
    // Given: `feat/member`, on `feat/line`, is M1 writing `a.txt` then M2
    // writing `b.txt`; `feat/follow` is built on M1; `feat/line` took the
    // member by moving onto it, then merged `feat/follow`; the release merges
    // `feat/line` with another branch and drops `a.txt` by hand.
    let lab = Lab::new();
    lab.branch("feat/line", "line.txt", "line\n");
    child_by_hand(&lab, "feat/member", "feat/line", &[("a.txt", Some("a\n"))]);
    child_by_hand(
        &lab,
        "feat/follow",
        "feat/member",
        &[("f.txt", Some("f\n"))],
    );
    extend_branch(&lab, "feat/member", "b.txt", "b\n");
    lab.jj_work(["bookmark", "set", "feat/line", "-r", "feat/member"]);
    merge_by_hand(&lab, "landed", ("feat/line", "feat/follow"), &[]);
    lab.jj_work(["bookmark", "set", "feat/line", "-r", "landed"]);
    lab.jj_work(["bookmark", "delete", "landed", "feat/follow"]);
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/line", "feat/other"),
        &[("a.txt", None)],
    );

    // When, then: neither the member nor `feat/line` is carried: M1's file
    // is gone.
    let (home, _consumer) = release_test_home(&lab);
    assert_carried_in(&lab, &home, "feat/member", false);
    assert_carried_in(&lab, &home, "feat/line", false);
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_member_whose_tip_its_built_on_branch_merged() {
    // Given: `feat/x` is X1 writing `a.txt` then X2 writing `b.txt`; `feat/y`
    // is built on X1 and then merges `feat/x`; the release merges `feat/y`
    // with another branch and drops `a.txt` by hand.
    let lab = Lab::new();
    lab.branch("feat/x", "a.txt", "a\n");
    extend_branch(&lab, "feat/x", "b.txt", "b\n");
    child_by_hand(&lab, "feat/y", "feat/x-", &[("y.txt", Some("y\n"))]);
    merge_by_hand(&lab, "ym", ("feat/y", "feat/x"), &[]);
    lab.jj_work(["bookmark", "set", "feat/y", "-r", "ym"]);
    lab.jj_work(["bookmark", "delete", "ym"]);
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/y", "feat/other"),
        &[("a.txt", None)],
    );
    let (home, _consumer) = release_test_home(&lab);

    // When, then: `feat/x` is not carried, with no record and with a record
    // that it was built on `feat/y`, which is false.
    assert_carried_in(&lab, &home, "feat/x", false);
    depends(&lab, &home, "feat/x", "feat/y");
    assert_carried_in(&lab, &home, "feat/x", false);
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_member_beside_a_backup_bookmark_on_its_first_commit()
 {
    // Given: `feat/x` is X1 writing `a.txt`, with a backup bookmark
    // `feat/x-wip` on X1, then X2 writing `b.txt`; `feat/y` is built on X1;
    // the release merges both and drops `a.txt` by hand.
    let lab = Lab::new();
    lab.branch("feat/x", "a.txt", "a\n");
    lab.jj_work(["bookmark", "create", "feat/x-wip", "-r", "feat/x"]);
    extend_branch(&lab, "feat/x", "b.txt", "b\n");
    child_by_hand(&lab, "feat/y", "feat/x-", &[("y.txt", Some("y\n"))]);
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/x", "feat/y"),
        &[("a.txt", None)],
    );
    let (home, _consumer) = release_test_home(&lab);

    // When, then: `feat/x` is not carried, with no record and with a record
    // that it was built on `feat/y`, which is false.
    assert_carried_in(&lab, &home, "feat/x", false);
    depends(&lab, &home, "feat/x", "feat/y");
    assert_carried_in(&lab, &home, "feat/x", false);
}

#[test]
fn release_carries_answers_carried_exact_for_a_replaced_line_of_a_file_the_change_adds_whole() {
    // Given: `feat/foundation` writes `a`, `timeout = 30`, `b`; the member,
    // stacked on it, deletes the timeout line; `feat/sibling`, also on it,
    // sets `timeout = 60`; the release merges both and keeps the sibling's
    // line.
    let lab = Lab::new();
    lab.branch("feat/foundation", "conf.txt", "a\ntimeout = 30\nb\n");
    child_by_hand(
        &lab,
        MEMBER,
        "feat/foundation",
        &[("conf.txt", Some("a\nb\n"))],
    );
    child_by_hand(
        &lab,
        "feat/sibling",
        "feat/foundation",
        &[("conf.txt", Some("a\ntimeout = 60\nb\n"))],
    );
    merge_by_hand(
        &lab,
        CUT,
        (MEMBER, "feat/sibling"),
        &[("conf.txt", Some("a\ntimeout = 60\nb\n"))],
    );
    let (home, _consumer) = release_test_home(&lab);

    // When, then: every command reads both carried. From the trunk, the
    // member's change adds `conf.txt` whole, as `a` and `b`, so the deletion
    // of the timeout line is no hunk of its own change, and the release
    // holds both of its lines, with the sibling's line between them.
    assert_every_command_reads_carried(&lab, &home, &[MEMBER, "feat/sibling"]);
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_member_that_removed_one_of_two_equal_lines() {
    // Given: `feat/base` writes `p D D X q`; the member, stacked on it,
    // writes `p D X q`; `feat/other`, also on it, writes `p X D D q`; the
    // release merges both and keeps `feat/other`'s text.
    let lab = Lab::new();
    lab.branch("feat/base", "f.txt", "p\nD\nD\nX\nq\n");
    child_by_hand(
        &lab,
        MEMBER,
        "feat/base",
        &[("f.txt", Some("p\nD\nX\nq\n"))],
    );
    child_by_hand(
        &lab,
        "feat/other",
        "feat/base",
        &[("f.txt", Some("p\nX\nD\nD\nq\n"))],
    );
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        (MEMBER, "feat/other"),
        &[("f.txt", Some("p\nX\nD\nD\nq\n"))],
    );

    // When, then: the member is not carried.
    assert_member_carried(&lab, false);
}

#[test]
fn release_carries_answers_carried_exact_for_a_deletion_the_release_wrote_back_edited() {
    // Given: the member empties `README.md`, which the trunk holds as
    // `seed`; the release merges it with another branch and writes
    // `seed, edited` by hand.
    let lab = Lab::new();
    lab.branch(MEMBER, "README.md", "");
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        CUT,
        (MEMBER, "feat/other"),
        &[("README.md", Some("seed, edited\n"))],
    );
    let (home, _consumer) = release_test_home(&lab);

    // When, then: every command reads the member carried: a hunk that the
    // member removed is put back only where the release inserts its lines as
    // they were, and `seed, edited` is another line.
    assert_every_command_reads_carried(&lab, &home, &[MEMBER]);
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_member_a_commit_below_a_later_merge_dropped() {
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

#[test]
fn release_carries_answers_merged_then_edited_for_a_member_a_later_release_commit_reverted() {
    // Given: the member and another branch merged as the release; then a
    // commit on the release that reverts the member's change, and the
    // release bookmark moved onto it.
    let lab = member_and_other();
    merge_by_hand(&lab, "release/2026-08-04", (MEMBER, "feat/other"), &[]);
    child_by_hand(
        &lab,
        "revert",
        "release/2026-08-04",
        &[("shared.txt", None)],
    );
    lab.jj_work(["bookmark", "set", "release/2026-08-04", "-r", "revert"]);
    lab.jj_work(["bookmark", "delete", "revert"]);

    // When, then: the member is not carried: the release reaches it, but
    // its change is gone.
    assert_member_carried(&lab, false);
}

#[test]
fn release_carries_answers_conflicted_for_a_conflicted_tip_whose_clean_resolution_the_release_holds()
 {
    // Given: a merge of two branches that conflict on `shared.txt`, kept
    // unresolved under `feat/conflicted`; its resolution, a jj rewrite of the
    // same change, under `feat/resolved`; and a release that merges the
    // resolution with another branch.
    let lab = Lab::new();
    lab.branch("feat/left", "shared.txt", "left\n");
    lab.branch("feat/right", "shared.txt", "right\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/left",
        "-r",
        "feat/right",
        "-m",
        "unresolved member",
    ]);
    lab.jj_work(["bookmark", "create", "feat/conflicted", "-r", "@"]);
    lab.jj_work(["new"]);
    let conflicted = commit_at(&lab, "feat/conflicted");
    lab.jj_work(["edit", "feat/conflicted"]);
    std::fs::write(lab.work.join("shared.txt"), "resolved\n").expect("resolve the conflict");
    lab.jj_work(["new"]);
    let clean = commit_at(&lab, "feat/conflicted");
    lab.jj_work(["bookmark", "create", "feat/resolved", "-r", clean.as_str()]);
    lab.jj_work([
        "bookmark",
        "set",
        "feat/conflicted",
        "-r",
        conflicted.as_str(),
        "--allow-backwards",
    ]);
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        "release/2026-08-04",
        ("feat/resolved", "feat/other"),
        &[],
    );
    let release = commit_at(&lab, "release/2026-08-04");
    let repo = Repo::open(&lab.work).expect("open the lab");
    assert_eq!(
        repo.conflicted_files(conflicted.as_str())
            .expect("read the tip's conflicts"),
        ["shared.txt"],
        "the tip holds the conflict"
    );
    assert!(
        !repo
            .is_ancestor(&conflicted, &release)
            .expect("read ancestry")
    );
    assert!(repo.is_ancestor(&clean, &release).expect("read ancestry"));
    assert!(
        repo.standing_rewrites_of(&conflicted)
            .expect("read the rewrites")
            .contains(&clean)
    );
    let (home, _consumer) = release_test_home(&lab);

    // When, then: the resolution is carried exactly.
    let control = knives_release(
        &lab,
        &home,
        &[
            "members",
            "release/2026-08-04",
            "--carries",
            "feat/resolved",
        ],
    );
    assert_eq!(control.status.code(), Some(0), "{}", text(&control));

    // And: the conflicted tip is not carried, though the release holds a
    // rewrite of it: a tip with an unresolved conflict is never carried.
    let query = knives_release(
        &lab,
        &home,
        &[
            "members",
            "release/2026-08-04",
            "--carries",
            "feat/conflicted",
        ],
    );
    let out = String::from_utf8_lossy(&query.stdout);
    assert_eq!(query.status.code(), Some(1), "{}", text(&query));
    assert!(
        out.contains(&format!(
            "conflicted         release/2026-08-04 @ {}",
            release.short()
        )),
        "{out}"
    );
    assert!(!out.contains("carried-"), "{out}");
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
fn release_carries_answers_carried_exact_for_an_added_file_whose_mode_the_release_cleared() {
    // Given: the member adds an executable `shared.sh`; the release merges it
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
    lab.jj_work(["bookmark", "create", CUT, "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);

    // When, then: every command reads the member carried. A change of the
    // executable bit is read where the member changes the bit of a file that
    // its fork point holds (the side-by-side mode test above); a file that
    // the member adds has no bit of its fork point's to change, and
    // `--verify` has always read its lines alone.
    assert_every_command_reads_carried(&lab, &home, &[MEMBER]);
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_mode_dropped_where_a_later_branch_rewrote_the_line()
 {
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
fn release_carries_answers_carried_exact_for_a_member_whose_blank_lines_the_release_dropped() {
    // Given: the member writes lines with blank lines between them; the
    // release merges it with another branch and drops the blank lines.
    let lab = Lab::new();
    lab.branch(MEMBER, "shared.txt", "x\n\n\ny\n");
    lab.branch("feat/other", "other.txt", "other\n");
    merge_by_hand(
        &lab,
        CUT,
        (MEMBER, "feat/other"),
        &[("shared.txt", Some("x\ny\n"))],
    );
    let (home, _consumer) = release_test_home(&lab);

    // When, then: every command reads the member carried: its own lines are
    // its non-blank lines, and the release holds them all.
    assert_every_command_reads_carried(&lab, &home, &[MEMBER]);
}

#[test]
fn release_carries_answers_merged_then_edited_for_a_member_whose_file_later_work_moved() {
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
    // standing rewrite of it.
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
