//! What a release carries of each member, read from the member's own lines
//! (spec R6, R8 and R9).
//!
//! A member's own change is the diff from its fork point with the upstream
//! trunk to its tip. The release carries the member when it still holds each
//! line that the member added, and does not hold again a hunk that the member
//! removed. Where another change edited a line of the member, the line is
//! held by its words. A release-log entry is held wherever the release's one
//! `## [Unreleased]` section holds it. `members --verify`, both audits of
//! `release cut` and the composition gate read a member this way, and each
//! names the lines that a member lacks. `--carries` and `--census` read a
//! release that reaches a member's tip the same way, from the same base:
//! carried-exact where the release holds every line, none of them only by its
//! words, and merged-then-edited otherwise. Their tests here pin what they
//! answer for these shapes. These tests drive only the knives binary.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]
// allow: SIZE_OK: real-binary scenarios of one rule (K6, K9, K11) share one fixture and its helpers.

#[path = "common/lab.rs"]
mod lab;

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use knives::ids::CommitId;
use lab::{
    Lab, ReleaseOutput, carried_row, carries, census, census_block, commit_at, extend_branch,
    home_after_first_cut, jj_in, knives_release, label, missing_row, release_command,
    release_test_home, stdout, text, trunk_with, verify,
};
use serde_json::{Value, json};
use tempfile::TempDir;

/// The release most tests read.
const RELEASE: &str = "release/2026-08-04";
/// The release that a next cut or record takes.
const NEXT: &str = "release/2026-08-05";
/// The member whose lines each test follows.
const M: &str = "feat/m";
/// Another member of the same release.
const N: &str = "feat/n";
/// A third member, on a file of its own.
const O: &str = "feat/o";
/// The exit of a command that reports findings.
const FINDINGS: i32 = 1;
/// The exit of a command that refuses and writes nothing.
const REFUSED: i32 = 3;

/// Write `content` to `file` under `root`.
fn write_file(root: &Path, file: &str, content: &str) {
    std::fs::write(root.join(file), content).expect("write a file");
}

/// Set the mode of the file at `path`.
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("set a mode");
}

/// A symlink at `link` to `target`.
fn symlink(target: &str, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("make a symlink");
}

/// Each of `items` as a line, ended by a newline.
fn lines(items: &[&str]) -> String {
    let mut joined = String::new();
    for line in items {
        joined.push_str(line);
        joined.push('\n');
    }
    joined
}

/// A commit on `base`, a revset that names one commit, whose change `edit`
/// makes in the work checkout, with the bookmark `name` on it. The working
/// copy steps off it.
fn branch_on(lab: &Lab, base: &str, name: &str, edit: impl FnOnce(&Path)) {
    jj_in(&lab.work, &["new", "-r", base, "-m", name]);
    edit(&lab.work);
    jj_in(&lab.work, &["bookmark", "create", name, "-r", "@"]);
    jj_in(&lab.work, &["new"]);
}

/// A release made outside knives: a commit that merges `parents`, with `edit`
/// run on its working copy and the bookmark `name` on it. Its commit.
fn hand_release(lab: &Lab, name: &str, parents: &[&str], edit: impl FnOnce(&Path)) -> CommitId {
    let mut args = vec!["new"];
    for parent in parents {
        args.extend(["-r", *parent]);
    }
    args.extend(["-m", name]);
    jj_in(&lab.work, &args);
    edit(&lab.work);
    jj_in(&lab.work, &["bookmark", "create", name, "-r", "@"]);
    jj_in(&lab.work, &["new"]);
    commit_at(lab, name)
}

/// The lines of a cut's report that name `branch` with a divergence: each
/// row that reads it missing or carries its divergence forward says so.
fn divergence_rows<'a>(stdout: &'a str, branch: &str) -> Vec<&'a str> {
    stdout
        .lines()
        .filter(|line| line.contains(branch) && line.contains("diverge"))
        .collect()
}

/// The JSON report of the release verb `args`, parsed.
fn json_report(lab: &Lab, home: &TempDir, args: &[&str]) -> Value {
    let output = release_command(lab, home, ReleaseOutput::Json, args)
        .output()
        .expect("run knives release");
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("not JSON ({error}): {}", text(&output)))
}

/// [`verify`]'s JSON report, parsed.
fn verify_json(lab: &Lab, home: &TempDir, release: &str) -> Value {
    json_report(lab, home, &["members", release, "--verify"])
}

/// The entry of `audit.members` for the member at `commit`, in a report of
/// `members --verify`.
fn audit_member<'a>(report: &'a Value, commit: &CommitId) -> &'a Value {
    report["audit"]["members"]
        .as_array()
        .and_then(|members| {
            members
                .iter()
                .find(|member| member["commit"] == commit.as_str())
        })
        .unwrap_or_else(|| panic!("no audit.members entry for {}: {report}", commit.as_str()))
}

/// Whether a `members --verify` report lists `label` in `audit.missing`.
fn lists_missing(report: &Value, label: &str) -> bool {
    report["audit"]["missing"]
        .as_array()
        .is_some_and(|missing| missing.iter().any(|name| name == label))
}

/// `knives release cut <name> --at <commit>`, whatever its exit.
fn record(lab: &Lab, home: &TempDir, name: &str, commit: &CommitId) -> Output {
    knives_release(lab, home, &["cut", name, "--at", commit.as_str()])
}

/// The line `cut --at` prints once it recorded `name` at `commit`.
fn recorded_line(name: &str, commit: &CommitId) -> String {
    format!(
        "demo: recorded {name} at {} as its cut; nothing was committed",
        commit.short()
    )
}

// ---------------------------------------------------------------------------
// R6: each member's change is measured from its own fork point.
// ---------------------------------------------------------------------------

/// `l.txt` with its line L set to `value`: the file that each trunk commit
/// of K6 changes. It is no release log.
fn l_txt(value: &str) -> String {
    format!("first\nL = {value}\nlast\n")
}

/// The three members of K6 that fork at T0, T1 and T2.
const THREE_FORKS: [&str; 3] = ["feat/m0", "feat/m1", "feat/m2"];

/// A trunk T0, T1, T2, each a child of the one before and each changing line
/// L of `l.txt`; a member forked at each, writing a file of its own
/// ([`THREE_FORKS`]); the trunk at T2; and the three merged by hand as
/// [`RELEASE`], a release cut outside knives. Its commit.
fn three_forks_merged_by_hand() -> (Lab, TempDir, CommitId) {
    let base = l_txt("0");
    let lab = trunk_with(&[("l.txt", base.as_str())]);
    branch_on(&lab, "main@upstream", "feat/m0", |work| {
        write_file(work, "m0.txt", "m0\n");
    });
    lab.upstream_trunk_file("l.txt", &l_txt("1"));
    branch_on(&lab, "main@upstream", "feat/m1", |work| {
        write_file(work, "m1.txt", "m1\n");
    });
    lab.upstream_trunk_file("l.txt", &l_txt("2"));
    branch_on(&lab, "main@upstream", "feat/m2", |work| {
        write_file(work, "m2.txt", "m2\n");
    });
    let release = hand_release(&lab, RELEASE, &THREE_FORKS, |work| {
        write_file(work, "l.txt", &l_txt("2"));
    });
    let (home, _consumer) = release_test_home(&lab);
    (lab, home, release)
}

#[test]
fn verify_measures_each_member_from_its_own_fork_point_after_the_trunk_moved() {
    // Given: a trunk T0 whose `l.txt` is no release log; `feat/a` changes its
    // line L and `feat/b` writes a file of its own, both forked at T0; a
    // knives cut of both; then the trunk moves to a child of T0 that changes
    // L, which the release does not contain.
    let base = l_txt("0");
    let lab = trunk_with(&[("l.txt", base.as_str())]);
    lab.branch("feat/a", "l.txt", &l_txt("a"));
    lab.branch("feat/b", "b.txt", "b\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.upstream_trunk_file("l.txt", &l_txt("1"));

    // When: the release's members are verified.
    let output = verify(&lab, &home, RELEASE);

    // Then: each member is measured from its own fork point, T0, so the move
    // of the trunk is charged to neither: exit 0, no `!!` line, and both read
    // carried.
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(!out.contains("!!"), "{out}");
    for branch in ["feat/a", "feat/b"] {
        assert!(out.contains(&carried_row(&label(&lab, branch))), "{out}");
    }
}

#[test]
fn verify_reads_members_forking_at_three_trunk_commits_as_carried() {
    // Given: a release made outside knives of three members that fork at T0,
    // T1 and T2, each changing a file of its own; the trunk is at T2.
    let (lab, home, _release) = three_forks_merged_by_hand();

    // When: the release's members are verified.
    let output = verify(&lab, &home, RELEASE);

    // Then: each is measured from its own fork point: exit 0, no `!!` line,
    // and every member reads carried.
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(!out.contains("!!"), "{out}");
    for branch in THREE_FORKS {
        assert!(out.contains(&carried_row(&label(&lab, branch))), "{out}");
    }
}

#[test]
fn a_record_of_members_forking_at_three_trunk_commits_is_recorded() {
    // Given: the release of three members that fork at T0, T1 and T2.
    let (lab, home, release) = three_forks_merged_by_hand();

    // When: it is recorded as a release cut outside knives.
    let recorded = record(&lab, &home, RELEASE, &release);

    // Then: its audit reads no member missing, so it is recorded: exit 0.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(out.contains(&recorded_line(RELEASE, &release)), "{out}");
}

#[test]
fn a_knives_cut_of_members_forking_at_three_trunk_commits_reads_each_carried() {
    // Given: a knives cut of `feat/m0`, forked at T0; then the trunk moves to
    // T1, where `feat/m1` forks and is included; then to T2, where `feat/m2`
    // forks and is included.
    let base = l_txt("0");
    let lab = trunk_with(&[("l.txt", base.as_str())]);
    lab.branch("feat/m0", "m0.txt", "m0\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    for (value, member, file) in [("1", "feat/m1", "m1.txt"), ("2", "feat/m2", "m2.txt")] {
        lab.upstream_trunk_file("l.txt", &l_txt(value));
        branch_on(&lab, "main@upstream", member, |work| {
            write_file(work, file, "own\n");
        });
        let included = knives_release(&lab, &home, &["include", member]);
        assert_eq!(included.status.code(), Some(0), "{}", text(&included));
    }

    // When: a next cut of a new name.
    let cut = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: its audit reads no member missing and carries none forward: no
    // row names a member with a divergence, and the cut is made.
    let out = stdout(&cut);
    for member in THREE_FORKS {
        assert_eq!(
            divergence_rows(&out, member),
            Vec::<&str>::new(),
            "{}",
            text(&cut)
        );
    }
    assert!(out.contains(&format!("  cut {NEXT} as ")), "{}", text(&cut));
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
}

/// The file each member of [`next_release_and_control`] adds a line to, in a
/// region of its own, as the members of a fork add lines to one shared file.
const SHARED: &str = "shared.txt";

/// [`SHARED`] holding the line of each member of `notes`, of `m0`, `m1` and
/// `m2`, in that member's region.
fn shared_txt(notes: &[&str]) -> String {
    let mut text = String::new();
    for (region, member, rest) in [
        ("alpha", "m0", "one\ntwo\n"),
        ("beta", "m1", "three\nfour\n"),
        ("gamma", "m2", "five\nsix\n"),
    ] {
        text.push_str(region);
        text.push('\n');
        if notes.contains(&member) {
            text.push_str(member);
            text.push_str(" note\n");
        }
        text.push_str(rest);
    }
    text
}

/// A trunk T0 to T3, each changing line L of `l.txt`. `feat/m0` forks at T0
/// and `feat/m1` at T1, each writing a file of its own and its line of
/// [`SHARED`]; [`RELEASE`], cut outside knives, merges the two. Then
/// `feat/m0` advances by a line of its file, `feat/m2` forks at T2 the same
/// way, and the trunk moves to T3, past every fork point. [`NEXT`] merges
/// the three ([`THREE_FORKS`]) and holds each one's lines. The control
/// merges the same parents in the same order and holds [`RELEASE`]'s tree,
/// which lacks `feat/m2` and `feat/m0`'s advance; no bookmark names it. The
/// commits of [`NEXT`] and of the control.
fn next_release_and_control() -> (Lab, TempDir, CommitId, CommitId) {
    let lab = trunk_with(&[
        ("l.txt", l_txt("0").as_str()),
        (SHARED, shared_txt(&[]).as_str()),
    ]);
    branch_on(&lab, "main@upstream", "feat/m0", |work| {
        write_file(work, "m0.txt", "m0\n");
        write_file(work, SHARED, &shared_txt(&["m0"]));
    });
    lab.upstream_trunk_file("l.txt", &l_txt("1"));
    branch_on(&lab, "main@upstream", "feat/m1", |work| {
        write_file(work, "m1.txt", "m1\n");
        write_file(work, SHARED, &shared_txt(&["m1"]));
    });
    hand_release(&lab, RELEASE, &["feat/m0", "feat/m1"], |work| {
        write_file(work, "l.txt", &l_txt("1"));
        write_file(work, SHARED, &shared_txt(&["m0", "m1"]));
    });
    extend_branch(&lab, "feat/m0", "m0.txt", "m0\nmore\n");
    lab.upstream_trunk_file("l.txt", &l_txt("2"));
    branch_on(&lab, "main@upstream", "feat/m2", |work| {
        write_file(work, "m2.txt", "m2\n");
        write_file(work, SHARED, &shared_txt(&["m2"]));
    });
    lab.upstream_trunk_file("l.txt", &l_txt("3"));
    let next = hand_release(&lab, NEXT, &THREE_FORKS, |work| {
        write_file(work, "l.txt", &l_txt("2"));
        write_file(work, SHARED, &shared_txt(&["m0", "m1", "m2"]));
    });
    let control = hand_release(&lab, "control", &THREE_FORKS, |work| {
        jj_in(work, &["restore", "--from", RELEASE]);
    });
    jj_in(&lab.work, &["bookmark", "delete", "control"]);
    let repo = knives::jj::Repo::open(&lab.work).expect("open the work checkout");
    assert_eq!(
        repo.parent_commits(control.as_str())
            .expect("the control's parents"),
        repo.parent_commits(next.as_str())
            .expect("the release's parents"),
        "the control has the release's parents, in order"
    );
    let tree_diff = Command::new("jj")
        .args(["--ignore-working-copy", "diff", "--summary"])
        .args(["--from", RELEASE, "--to", control.as_str()])
        .current_dir(&lab.work)
        .output()
        .expect("run jj diff");
    assert!(tree_diff.status.success(), "{}", text(&tree_diff));
    assert_eq!(
        stdout(&tree_diff),
        "",
        "the control holds the older release's tree"
    );
    let (home, _consumer) = release_test_home(&lab);
    (lab, home, next, control)
}

#[test]
fn verify_passes_a_release_that_holds_its_members_and_fails_its_control() {
    // Given: a release of three members that fork at T0, T1 and T2 and each
    // add a line to one shared file, with the trunk at T3; and its control,
    // with the same parents and the tree of the release before it.
    let (lab, home, next, control) = next_release_and_control();

    // When: each is verified by its commit.
    let held = verify(&lab, &home, next.as_str());
    let lacking = verify(&lab, &home, control.as_str());

    // Then: the release reads every member carried, with exit 0.
    let out = stdout(&held);
    assert_eq!(held.status.code(), Some(0), "{}", text(&held));
    assert!(!out.contains("!!"), "{out}");
    for member in THREE_FORKS {
        assert!(out.contains(&carried_row(&label(&lab, member))), "{out}");
    }

    // And: the control reads `feat/m0` and `feat/m2` missing, with the
    // findings exit, and names each line it lacks under its row; `feat/m1`,
    // whose lines the older tree holds, reads carried.
    let out = stdout(&lacking);
    assert_eq!(lacking.status.code(), Some(FINDINGS), "{}", text(&lacking));
    let advanced = label(&lab, "feat/m0");
    let added = label(&lab, "feat/m2");
    for (member, lost) in [
        (&advanced, vec!["      m0.txt:2: added line lost ‹more›"]),
        (
            &added,
            vec![
                "      m2.txt:1: added line lost ‹m2›",
                "      shared.txt:8: added line lost ‹m2 note›",
            ],
        ),
    ] {
        let row = missing_row(member);
        let below: Vec<&str> = out
            .lines()
            .skip_while(|line| *line != row)
            .skip(1)
            .take_while(|line| line.starts_with("      "))
            .collect();
        assert_eq!(below, lost, "{out}");
    }
    assert!(out.contains(&carried_row(&label(&lab, "feat/m1"))), "{out}");
}

#[test]
fn verify_reports_differ_between_a_release_and_its_control() {
    // Given: the release of three members and its control, which has the
    // release's parents and the tree of the release before it.
    let (lab, home, next, control) = next_release_and_control();

    // When: each is verified by its commit, as JSON.
    let held = verify_json(&lab, &home, next.as_str());
    let lacking = verify_json(&lab, &home, control.as_str());

    // Then: the release lists no member missing, and the control lists
    // exactly the two members whose lines its tree lacks.
    assert_eq!(held["audit"]["missing"], json!([]), "{held}");
    assert_eq!(
        lacking["audit"]["missing"],
        json!([label(&lab, "feat/m0"), label(&lab, "feat/m2")]),
        "{lacking}"
    );
    for report in [&held, &lacking] {
        assert_eq!(report["audit"]["unexplained"], json!([]), "{report}");
        assert_eq!(report["audit"]["inconclusive"], json!([]), "{report}");
    }
}

/// [`M`], forked at T2 and writing `m.txt`, and [`RELEASE`], a commit on T0
/// that holds `M`'s change as a cherry-pick and does not reach `M`'s tip. The
/// trunk is at T2. The release's commit.
fn cherry_pick_on_an_older_trunk() -> (Lab, TempDir, CommitId) {
    let base = l_txt("0");
    let lab = trunk_with(&[("l.txt", base.as_str())]);
    let first = commit_at(&lab, "main@upstream");
    lab.upstream_trunk_file("l.txt", &l_txt("1"));
    lab.upstream_trunk_file("l.txt", &l_txt("2"));
    branch_on(&lab, "main@upstream", M, |work| {
        write_file(work, "m.txt", "the member's line\n");
    });
    let release = hand_release(&lab, RELEASE, &[first.as_str()], |work| {
        write_file(work, "m.txt", "the member's line\n");
    });
    let (home, _consumer) = release_test_home(&lab);
    (lab, home, release)
}

#[test]
fn carries_reads_carried_rewritten_for_a_cherry_pick_on_an_older_trunk() {
    // Given: a release on T0 that holds a cherry-pick of M, which forks at T2.
    let (lab, home, release) = cherry_pick_on_an_older_trunk();

    // When: M's carriage is asked.
    let output = carries(&lab, &home, M);

    // Then: M's own change is measured from its fork point, T2, so the
    // trunk's changes T1 and T2 are not charged to it: carried-rewritten.
    let out = stdout(&output);
    assert!(
        out.contains(&format!(
            "carried-rewritten  {RELEASE} @ {}  (evidence {}: no net content remains)",
            release.short(),
            release.short()
        )),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
}

#[test]
fn census_reads_a_cherry_pick_on_an_older_trunk_as_carried() {
    // Given: a release on T0 that holds a cherry-pick of M, which forks at T2.
    let (lab, home, release) = cherry_pick_on_an_older_trunk();

    // When: the census checks every maintained branch.
    let out = census(&lab, &home);

    // Then: M is measured from its own fork point and read as carried.
    let block = census_block(&out, M);
    assert!(
        block.contains(&format!(
            "carried-rewritten  {RELEASE} @ {}",
            release.short()
        )),
        "{out}"
    );
}

#[test]
fn a_record_with_a_trunk_parent_that_lacks_an_earlier_trunk_change_reads_it_missing() {
    // Given: T0, T1 and T2 change line L in turn; `feat/m0` forks at T0 and
    // writes a file of its own; a hand merge of T2, which the trunk holds,
    // and `feat/m0`, whose `l.txt` holds L as T0 has it, without the change
    // of T1 or of T2.
    let base = l_txt("0");
    let lab = trunk_with(&[("l.txt", base.as_str())]);
    branch_on(&lab, "main@upstream", "feat/m0", |work| {
        write_file(work, "m0.txt", "m0\n");
    });
    lab.upstream_trunk_file("l.txt", &l_txt("1"));
    lab.upstream_trunk_file("l.txt", &l_txt("2"));
    let trunk = commit_at(&lab, "main@upstream");
    let release = hand_release(&lab, RELEASE, &["main@upstream", "feat/m0"], |work| {
        write_file(work, "l.txt", &base);
    });
    let (home, _consumer) = release_test_home(&lab);

    // When: it is recorded.
    let recorded = record(&lab, &home, RELEASE, &release);

    // Then: the trunk parent is measured from the fork point of the members
    // and the trunk, T0, as at f6a1ef95, so it reads missing and the record
    // is refused; the member reads carried.
    let out = stdout(&recorded);
    assert_eq!(recorded.status.code(), Some(REFUSED), "{}", text(&recorded));
    assert!(
        out.lines().any(|line| {
            line.starts_with("  !! ")
                && (line.contains("main@upstream") || line.contains(trunk.short()))
                && line.ends_with("the cut tree is missing or diverges from the member's content")
        }),
        "{out}"
    );
    assert_eq!(
        divergence_rows(&out, "feat/m0"),
        Vec::<&str>::new(),
        "{out}"
    );
}

// ---------------------------------------------------------------------------
// R6: a revision that the upstream trunk contains is read from the base the
// trunk took it in on: the first parent of the merge that took it in, or,
// on the trunk's first-parent line, the newest commit of that line that the
// release's parents hold.
// ---------------------------------------------------------------------------

/// The revision that upstream takes into its trunk, whose carriage each test
/// of K11 (19) to (20c) asks.
const X: &str = "feat/x";
/// In K11 (20b), the branch that merges X and that upstream merges by a merge
/// commit; in (20c), a member that upstream merges by a merge commit before
/// it fast-forwards its trunk to X.
const Y: &str = "feat/y";
/// X's two lines, in `x.txt`.
const X_LINES: &str = "x one\nx two\n";

/// The start of a check that reads the revision as not carried by
/// `release` at `commit`, up to its evidence; f6a1ef95 prints it too.
fn not_carried(release: &str, commit: &CommitId) -> String {
    format!(
        "NOT carried        {release} @ {}  (evidence ",
        commit.short()
    )
}

/// The start of a check that reads the revision as carried by `release` at
/// `commit`, which does not reach its tip, up to its evidence.
fn carried_rewritten(release: &str, commit: &CommitId) -> String {
    format!(
        "carried-rewritten  {release} @ {}  (evidence ",
        commit.short()
    )
}

/// `knives release members <release> --carries <revision>`, whatever its
/// exit.
fn carries_against(lab: &Lab, home: &TempDir, release: &str, revision: &str) -> Output {
    knives_release(lab, home, &["members", release, "--carries", revision])
}

/// A member `name` forked at `base` that writes `files`: a cherry-pick of a
/// revision's change onto a release's trunk commit, say.
fn member_on(lab: &Lab, base: &str, name: &str, files: &[(&str, &str)]) {
    branch_on(lab, base, name, |work| {
        for (file, content) in files {
            write_file(work, file, content);
        }
    });
}

#[test]
fn carries_reads_a_trunk_merged_revision_from_before_its_merge() {
    // Given (K11 19): X forks at T0 and writes two lines of `x.txt`; the
    // trunk moves to T1, where N forks; a release made by hand on T1 of N,
    // which does not hold X's change; then upstream merges X's pull request
    // by a merge commit, so the trunk contains X's tip.
    let lab = trunk_with(&[("l.txt", l_txt("0").as_str())]);
    member_on(&lab, "main@upstream", X, &[("x.txt", X_LINES)]);
    lab.upstream_trunk_file("l.txt", &l_txt("1"));
    let before_merge = commit_at(&lab, "main@upstream");
    member_on(&lab, "main@upstream", N, &[("n.txt", "n\n")]);
    let lacking = hand_release(&lab, RELEASE, &[N], |_| {});
    lab.publish_pull(X, 1);
    lab.merge_pull_with_merge_commit(1);
    let (home, _consumer) = release_test_home(&lab);

    // When: X's carriage is asked against that release.
    let output = carries_against(&lab, &home, RELEASE, X);

    // Then: X's change is read from its fork point with the merge's first
    // parent, T0: the release does not carry it, with the findings exit.
    // `--carries` names no line: `members --verify` does.
    let out = stdout(&output);
    assert!(
        out.contains(&not_carried(RELEASE, &lacking)),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(FINDINGS), "{}", text(&output));

    // And: the census, whose live release that is, reads it the same way.
    let census_out = census(&lab, &home);
    let block = census_block(&census_out, X);
    assert!(
        block.contains(&not_carried(RELEASE, &lacking)),
        "{census_out}"
    );

    // Given: a later release on T1 of N and a cherry-pick of X's change.
    member_on(
        &lab,
        before_merge.as_str(),
        "feat/x-pick",
        &[("x.txt", X_LINES)],
    );
    let holding = hand_release(&lab, NEXT, &[N, "feat/x-pick"], |_| {});

    // When/Then: `--carries` against it, and the census, whose live release
    // it is now, read X as carried.
    let carried = carries_against(&lab, &home, NEXT, X);
    assert!(
        stdout(&carried).contains(&carried_rewritten(NEXT, &holding)),
        "{}",
        text(&carried)
    );
    assert_eq!(carried.status.code(), Some(0), "{}", text(&carried));
    let census_out = census(&lab, &home);
    assert!(
        census_block(&census_out, X).contains(&carried_rewritten(NEXT, &holding)),
        "{census_out}"
    );
}

#[test]
fn carries_reads_a_fast_forwarded_revision_from_the_trunk_commit_its_release_stands_on() {
    // Given (K11 20): X of two commits on c0: c1 adds `a.txt` and c2 adds
    // `b.txt`; upstream fast-forwards its trunk to X, so X's tip is on the
    // trunk's first-parent line; a release on c0 of a cherry-pick of c2.
    let lab = trunk_with(&[("l.txt", l_txt("0").as_str())]);
    let c0 = commit_at(&lab, "main@upstream");
    member_on(&lab, "main@upstream", X, &[("a.txt", "c1 line\n")]);
    extend_branch(&lab, X, "b.txt", "c2 line\n");
    lab.publish_pull(X, 1);
    lab.fast_forward_pull(1);
    member_on(&lab, c0.as_str(), "feat/c2-pick", &[("b.txt", "c2 line\n")]);
    let lacking = hand_release(&lab, RELEASE, &["feat/c2-pick"], |_| {});
    let (home, _consumer) = release_test_home(&lab);

    // When: X's carriage is asked against that release.
    let output = carries_against(&lab, &home, RELEASE, X);

    // Then: X's change is read from c0, the newest commit of the trunk's
    // line that the release holds: c1's line is not in it, so X reads
    // missing.
    let out = stdout(&output);
    assert!(
        out.contains(&not_carried(RELEASE, &lacking)),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(FINDINGS), "{}", text(&output));

    // Given: a release on c0 of a member that holds both commits' changes.
    member_on(
        &lab,
        c0.as_str(),
        "feat/x-picks",
        &[("a.txt", "c1 line\n"), ("b.txt", "c2 line\n")],
    );
    let holding = hand_release(&lab, NEXT, &["feat/x-picks"], |_| {});

    // When/Then: X reads carried against it.
    let carried = carries_against(&lab, &home, NEXT, X);
    assert!(
        stdout(&carried).contains(&carried_rewritten(NEXT, &holding)),
        "{}",
        text(&carried)
    );
    assert_eq!(carried.status.code(), Some(0), "{}", text(&carried));
}

#[test]
fn carries_reads_a_fast_forwarded_revision_whole_against_a_release_of_unrelated_history() {
    // Given: X forks at the trunk, which holds `README.md` and `l.txt`, and
    // writes two lines of `x.txt`; upstream fast-forwards its trunk to X, so
    // X's tip is on the trunk's first-parent line; a release made by hand of
    // a member of an unrelated history, which holds X's `x.txt` and no other
    // file.
    let lab = trunk_with(&[("l.txt", l_txt("0").as_str())]);
    member_on(&lab, "main@upstream", X, &[("x.txt", X_LINES)]);
    lab.publish_pull(X, 1);
    lab.fast_forward_pull(1);
    member_on(&lab, "root()", "feat/x-alone", &[("x.txt", X_LINES)]);
    let lacking = hand_release(&lab, RELEASE, &["feat/x-alone"], |_| {});
    let (home, _consumer) = release_test_home(&lab);

    // When: X's carriage is asked against that release.
    let output = carries_against(&lab, &home, RELEASE, X);

    // Then: the release's parents hold no commit of X's first-parent line
    // but the root commit, so X's base is the root commit and X's whole tree
    // is its change: the release, which lacks the trunk's files, does not
    // carry X, with the findings exit.
    let out = stdout(&output);
    assert!(
        out.contains(&not_carried(RELEASE, &lacking)),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(FINDINGS), "{}", text(&output));

    // Given: a release of a member of an unrelated history that holds X's
    // whole tree.
    member_on(
        &lab,
        "root()",
        "feat/x-whole",
        &[
            ("README.md", "seed\n"),
            ("l.txt", l_txt("0").as_str()),
            ("x.txt", X_LINES),
        ],
    );
    let holding = hand_release(&lab, NEXT, &["feat/x-whole"], |_| {});

    // When/Then: X reads carried against it.
    let carried = carries_against(&lab, &home, NEXT, X);
    assert!(
        stdout(&carried).contains(&carried_rewritten(NEXT, &holding)),
        "{}",
        text(&carried)
    );
    assert_eq!(carried.status.code(), Some(0), "{}", text(&carried));
}

/// K11 (20b): the trunk at an older commit, returned; then it moves to T1,
/// where X forks and writes `x.txt` and Y forks and writes `y.txt`; Y
/// merges X; the trunk moves to T2; and upstream merges Y's pull request by
/// a merge commit, so the trunk contains X through Y.
fn merged_through_another_branch() -> (Lab, CommitId) {
    let lab = trunk_with(&[("l.txt", l_txt("0").as_str())]);
    let older = commit_at(&lab, "main@upstream");
    lab.upstream_trunk_file("l.txt", &l_txt("1"));
    member_on(&lab, "main@upstream", X, &[("x.txt", X_LINES)]);
    member_on(&lab, "main@upstream", Y, &[("y.txt", "y\n")]);
    lab.jj_work(["new", "-r", Y, "-r", X, "-m", "merge feat/x into feat/y"]);
    lab.jj_work(["bookmark", "set", Y, "-r", "@"]);
    lab.jj_work(["new"]);
    lab.upstream_trunk_file("l.txt", &l_txt("2"));
    lab.publish_pull(Y, 1);
    lab.merge_pull_with_merge_commit(1);
    (lab, older)
}

#[test]
fn carries_reads_a_revision_merged_through_another_branch_carried_where_the_release_holds_it() {
    // Given (K11 20b, a control): X merged upstream through Y; a release on
    // a trunk commit older than X's fork point, of a cherry-pick of X.
    let (lab, older) = merged_through_another_branch();
    member_on(&lab, older.as_str(), "feat/x-pick", &[("x.txt", X_LINES)]);
    let release = hand_release(&lab, RELEASE, &["feat/x-pick"], |_| {});
    let (home, _consumer) = release_test_home(&lab);

    // When: X's carriage is asked against that release.
    let output = carries_against(&lab, &home, RELEASE, X);

    // Then: X's change is read from its own fork point, T1, so the trunk's
    // change of `l.txt` is not charged to it: carried.
    assert!(
        stdout(&output).contains(&carried_rewritten(RELEASE, &release)),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
}

#[test]
fn carries_reads_a_revision_merged_through_another_branch_missing_where_the_release_lacks_it() {
    // Given (K11 20b): X merged upstream through Y; a release on a trunk
    // commit older than X's fork point, of a member that writes a file of
    // its own.
    let (lab, older) = merged_through_another_branch();
    member_on(&lab, older.as_str(), O, &[("o.txt", "o\n")]);
    let release = hand_release(&lab, RELEASE, &[O], |_| {});
    let (home, _consumer) = release_test_home(&lab);

    // When: X's carriage is asked against that release.
    let output = carries_against(&lab, &home, RELEASE, X);

    // Then: X reads missing.
    let out = stdout(&output);
    assert!(
        out.contains(&not_carried(RELEASE, &release)),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(FINDINGS), "{}", text(&output));
}

/// K11 (20c): Y forks at T0's parent and writes `y.txt`; the trunk moves
/// to T0, returned, where N forks and writes `n.txt`; upstream merges Y's
/// pull request by a merge commit, then fast-forwards its trunk to X, two
/// commits on that merge: c1 adds `a.txt` and c2 adds `b.txt`.
fn fast_forward_after_a_merged_member() -> (Lab, CommitId) {
    let lab = trunk_with(&[("l.txt", l_txt("0").as_str())]);
    member_on(&lab, "main@upstream", Y, &[("y.txt", "y\n")]);
    lab.upstream_trunk_file("l.txt", &l_txt("1"));
    let t0 = commit_at(&lab, "main@upstream");
    member_on(&lab, "main@upstream", N, &[("n.txt", "n\n")]);
    lab.publish_pull(Y, 1);
    lab.merge_pull_with_merge_commit(1);
    member_on(&lab, "main@upstream", X, &[("a.txt", "c1 line\n")]);
    extend_branch(&lab, X, "b.txt", "c2 line\n");
    lab.publish_pull(X, 2);
    lab.fast_forward_pull(2);
    (lab, t0)
}

#[test]
fn carries_reads_a_fast_forward_after_a_merged_member_carried_where_the_release_holds_it() {
    // Given (K11 20c, a control): a release on T0 of Y, N, whose lines X
    // lacks, and a member that holds c1's and c2's changes.
    let (lab, t0) = fast_forward_after_a_merged_member();
    member_on(
        &lab,
        t0.as_str(),
        "feat/x-picks",
        &[("a.txt", "c1 line\n"), ("b.txt", "c2 line\n")],
    );
    let release = hand_release(&lab, RELEASE, &[Y, N, "feat/x-picks"], |_| {});
    let (home, _consumer) = release_test_home(&lab);

    // When: X's carriage is asked against that release.
    let output = carries_against(&lab, &home, RELEASE, X);

    // Then: X's change is read from T0, the newest commit of the trunk's
    // line that the release holds: Y's change and X's own, all of which the
    // release holds, and not the absence of N's file: carried.
    assert!(
        stdout(&output).contains(&carried_rewritten(RELEASE, &release)),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
}

#[test]
fn carries_reads_a_fast_forward_after_a_merged_member_missing_where_the_release_lacks_c1() {
    // Given (K11 20c): a release on T0 of Y, N and a member that holds c2's
    // change alone.
    let (lab, t0) = fast_forward_after_a_merged_member();
    member_on(&lab, t0.as_str(), "feat/c2-pick", &[("b.txt", "c2 line\n")]);
    let release = hand_release(&lab, RELEASE, &[Y, N, "feat/c2-pick"], |_| {});
    let (home, _consumer) = release_test_home(&lab);

    // When: X's carriage is asked against that release.
    let output = carries_against(&lab, &home, RELEASE, X);

    // Then: X reads missing: c1's line is not in the release.
    let out = stdout(&output);
    assert!(
        out.contains(&not_carried(RELEASE, &release)),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(FINDINGS), "{}", text(&output));
}

#[test]
fn a_member_the_trunk_contains_and_the_release_reaches_is_carried_by_ancestry() {
    // Given (a control): X and N write `f.txt` each its own way; a release
    // made by hand merges both and resolves `f.txt` to N's line, dropping
    // X's; then upstream merges X's pull request by a merge commit, so the
    // trunk contains X, which the release reaches.
    let lab = trunk_with(&[("f.txt", "base line\n")]);
    lab.branch(X, "f.txt", "x line\n");
    lab.branch(N, "f.txt", "n line\n");
    let release = hand_release(&lab, RELEASE, &[X, N], |work| {
        write_file(work, "f.txt", "n line\n");
    });
    lab.publish_pull(X, 1);
    lab.merge_pull_with_merge_commit(1);
    let (home, _consumer) = release_test_home(&lab);

    // When/Then: `--verify` reads nothing missing, X included, which the
    // trunk holds and the release reaches: exit 0, and no `!!` row.
    let verified = verify(&lab, &home, RELEASE);
    let out = stdout(&verified);
    assert_eq!(verified.status.code(), Some(0), "{}", text(&verified));
    assert!(!out.contains("!!"), "{out}");

    // And: `--carries` of X against the release reads carried-exact.
    let carried = carries_against(&lab, &home, RELEASE, X);
    assert!(
        stdout(&carried).contains(&format!(
            "carried-exact      {RELEASE} @ {}",
            release.short()
        )),
        "{}",
        text(&carried)
    );
    assert_eq!(carried.status.code(), Some(0), "{}", text(&carried));
}

// ---------------------------------------------------------------------------
// R8: a release-log entry is held where the release's `[Unreleased]` holds it.
// ---------------------------------------------------------------------------

/// A release log whose `[Unreleased]` section holds `unreleased`, and whose
/// one published section, `[1.0.0]`, ends with `released`.
fn release_log(unreleased: &str, released: &str) -> String {
    format!(
        "# Changelog\n\n## [Unreleased]\n\n{unreleased}## [1.0.0] - 2026-08-01\n\n### Added\n\n\
         - The first release.\n{released}"
    )
}

/// The trunk's `[Unreleased]` lists.
const TRUNK_LISTS: &str = "### Added\n\n- Trunk addition one.\n- Trunk addition two.\n\
                           - Trunk addition three.\n\n### Fixed\n\n- Trunk fix.\n\n";

/// The member's own `### Fixed` block, which it adds at the top of
/// `[Unreleased]`.
const MEMBER_BLOCK: &str = "### Fixed\n\n- Member fix one.\n- Member fix two.\n\n";

/// The trunk's lists with both of the member's entries joined into the one
/// `### Fixed` list, as the release-log union writes them.
const JOINED_LISTS: &str = "### Added\n\n- Trunk addition one.\n- Trunk addition two.\n\
                            - Trunk addition three.\n\n### Fixed\n\n- Member fix one.\n\
                            - Member fix two.\n- Trunk fix.\n\n";

/// The trunk's lists with only the member's first entry joined.
const ONE_JOINED_LISTS: &str = "### Added\n\n- Trunk addition one.\n- Trunk addition two.\n\
                                - Trunk addition three.\n\n### Fixed\n\n- Member fix one.\n\
                                - Trunk fix.\n\n";

/// The line `members --verify` prints under the member's `!!` row for its
/// second entry, line 8 of its release log, where the release does not hold
/// it.
fn second_entry_lost(file: &str) -> String {
    format!("      {file}:8: added line lost ‹- Member fix two.›")
}

/// A trunk whose `file` is `trunk`; a member [`M`] that writes it as
/// `member`; a knives cut of it; then the release edited by hand so that
/// `file` is `release`.
fn moved_entries_lab(file: &str, (trunk, member, release): (&str, &str, &str)) -> (Lab, TempDir) {
    let lab = trunk_with(&[(file, trunk)]);
    lab.branch(M, file, member);
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.jj_work(["edit", RELEASE]);
    write_file(&lab.work, file, release);
    lab.jj_work(["new"]);
    (lab, home)
}

/// [`moved_entries_lab`] of `file`, where the member adds [`MEMBER_BLOCK`] at
/// the top of the trunk's `[Unreleased]` section and the release holds
/// `release_lists` there instead.
fn member_block_lab(file: &str, release_lists: &str) -> (Lab, TempDir) {
    let trunk = release_log(TRUNK_LISTS, "");
    let member = release_log(&format!("{MEMBER_BLOCK}{TRUNK_LISTS}"), "");
    let release = release_log(release_lists, "");
    moved_entries_lab(file, (&trunk, &member, &release))
}

/// `members --verify` reads [`M`] as carried, with exit 0.
fn assert_member_carried(lab: &Lab, home: &TempDir) {
    let output = verify(lab, home, RELEASE);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(out.contains(&carried_row(&label(lab, M))), "{out}");
}

/// `members --verify` reads [`M`] as missing, with the findings exit and its
/// `!!` row, as f6a1ef95 does too. Its standard output.
fn assert_member_missing(lab: &Lab, home: &TempDir) -> String {
    let output = verify(lab, home, RELEASE);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(FINDINGS), "{}", text(&output));
    assert!(out.contains(&missing_row(&label(lab, M))), "{out}");
    out
}

#[test]
fn a_changelog_entry_joined_into_the_unreleased_list_reads_held() {
    // Given: a member adds two entries under its own `### Fixed` heading at
    // the top of `[Unreleased]` in `CHANGELOG.md`; the release holds both
    // joined into its one `### Fixed` list further down the same section.
    let (lab, home) = member_block_lab("CHANGELOG.md", JOINED_LISTS);

    // When/Then: the member reads carried.
    assert_member_carried(&lab, &home);
}

#[test]
fn a_release_notes_entry_joined_into_the_unreleased_list_reads_held() {
    // Given: the same, in `RELEASE_NOTES.md`.
    let (lab, home) = member_block_lab("RELEASE_NOTES.md", JOINED_LISTS);

    // When/Then: the member reads carried.
    assert_member_carried(&lab, &home);
}

/// [`moved_entries_lab`] of `CHANGELOG.md`, where the release joins the
/// member's first entry and lists its second only under `[1.0.0]`, a
/// released section.
fn second_entry_only_released() -> (Lab, TempDir) {
    let trunk = release_log(TRUNK_LISTS, "");
    let member = release_log(&format!("{MEMBER_BLOCK}{TRUNK_LISTS}"), "");
    let release = release_log(ONE_JOINED_LISTS, "- Member fix two.\n");
    moved_entries_lab("CHANGELOG.md", (&trunk, &member, &release))
}

#[test]
fn a_changelog_entry_only_a_released_section_holds_reads_lost() {
    // Given: the release holds the member's second entry only in a released
    // section of its log.
    let (lab, home) = second_entry_only_released();

    // When/Then: the member reads missing.
    assert_member_missing(&lab, &home);
}

#[test]
fn a_changelog_entry_only_a_released_section_holds_reads_lost_names_its_lines() {
    // Given: the release holds the member's second entry only in a released
    // section of its log.
    let (lab, home) = second_entry_only_released();

    // When: the release's members are verified.
    let out = assert_member_missing(&lab, &home);

    // Then: the output names that entry under the member's row, and not the
    // first entry, which the `[Unreleased]` section holds.
    assert!(
        out.contains(&format!(
            "{}\n{}",
            missing_row(&label(&lab, M)),
            second_entry_lost("CHANGELOG.md")
        )),
        "{out}"
    );
    assert!(!out.contains("‹- Member fix one.›"), "{out}");
}

#[test]
fn a_moved_entry_in_notes_md_reads_lost() {
    // Given: the same move as in a release log, in `NOTES.md`, which is none.
    let (lab, home) = member_block_lab("NOTES.md", JOINED_LISTS);

    // When/Then: the member reads missing.
    assert_member_missing(&lab, &home);
}

#[test]
fn a_changelog_with_two_unreleased_sections_holds_no_moved_entry() {
    // Given: the same move as in `a_changelog_entry_joined_…`, in a log whose
    // every version holds a second `## [Unreleased]` heading line.
    let twice = |unreleased: &str| release_log(&format!("{unreleased}## [Unreleased]\n\n"), "");
    let trunk = twice(TRUNK_LISTS);
    let member = twice(&format!("{MEMBER_BLOCK}{TRUNK_LISTS}"));
    let release = twice(JOINED_LISTS);
    let (lab, home) = moved_entries_lab("CHANGELOG.md", (&trunk, &member, &release));

    // When: the release's members are verified.
    let out = assert_member_missing(&lab, &home);

    // Then: the log holds no one `[Unreleased]` section, so neither moved
    // entry is held, and both are named.
    for (line, entry) in [(7, "- Member fix one."), (8, "- Member fix two.")] {
        assert!(
            out.contains(&format!(
                "      CHANGELOG.md:{line}: added line lost ‹{entry}›"
            )),
            "{out}"
        );
    }
}

#[test]
fn an_unreleased_entry_with_trailing_spaces_reads_held() {
    // Given: the member writes its first entry with trailing spaces; the
    // release's `[Unreleased]` section holds it without them.
    let trunk = release_log(TRUNK_LISTS, "");
    let member = release_log(
        &format!("### Fixed\n\n- Member fix one.   \n- Member fix two.\n\n{TRUNK_LISTS}"),
        "",
    );
    let release = release_log(JOINED_LISTS, "");
    let (lab, home) = moved_entries_lab("CHANGELOG.md", (&trunk, &member, &release));

    // When/Then: trailing whitespace aside, the section holds it: carried.
    assert_member_carried(&lab, &home);
}

#[test]
fn a_line_after_the_unreleased_section_s_setext_heading_reads_lost() {
    // Given: the release joins the member's first entry, and lists its second
    // after a setext heading that follows the `[Unreleased]` lists, outside
    // the section as knives reads it.
    let trunk = release_log(TRUNK_LISTS, "");
    let member = release_log(&format!("{MEMBER_BLOCK}{TRUNK_LISTS}"), "");
    let release = release_log(
        &format!("{ONE_JOINED_LISTS}Older notes\n-----------\n\n- Member fix two.\n\n"),
        "",
    );
    let (lab, home) = moved_entries_lab("CHANGELOG.md", (&trunk, &member, &release));

    // When: the release's members are verified.
    let out = assert_member_missing(&lab, &home);

    // Then: the second entry is named as lost, and the first is held.
    assert!(out.contains(&second_entry_lost("CHANGELOG.md")), "{out}");
    assert!(!out.contains("‹- Member fix one.›"), "{out}");
}

#[test]
fn a_release_log_entry_the_trunk_moved_past_reads_held() {
    // Given: a trunk whose `[Unreleased]` lists `- Base entry.` under
    // `### Fixed`; M inserts two entries before it; the trunk then moves that
    // entry into a released section, `[1.1.0]`; N, on that trunk, adds
    // another entry under `### Fixed` in `[Unreleased]`; a release made
    // outside knives holds M's entries in that list after N's.
    let moved = "## [1.1.0] - 2026-08-02\n\n### Fixed\n\n- Base entry.\n\n";
    let trunk = release_log("### Fixed\n\n- Base entry.\n\n", "");
    let lab = trunk_with(&[("CHANGELOG.md", trunk.as_str())]);
    lab.branch(
        M,
        "CHANGELOG.md",
        &release_log(
            "### Fixed\n\n- Member entry one.\n- Member entry two.\n- Base entry.\n\n",
            "",
        ),
    );
    lab.upstream_trunk_file("CHANGELOG.md", &release_log(moved, ""));
    branch_on(&lab, "main@upstream", N, |work| {
        let unreleased = format!("### Fixed\n\n- Other entry.\n\n{moved}");
        write_file(work, "CHANGELOG.md", &release_log(&unreleased, ""));
    });
    hand_release(&lab, RELEASE, &[M, N], |work| {
        let unreleased = format!(
            "### Fixed\n\n- Other entry.\n- Member entry one.\n- Member entry two.\n\n{moved}"
        );
        write_file(work, "CHANGELOG.md", &release_log(&unreleased, ""));
    });
    let (home, _consumer) = release_test_home(&lab);

    // When: the release's members are verified.
    let output = verify(&lab, &home, RELEASE);

    // Then: both read carried.
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    for branch in [M, N] {
        assert!(out.contains(&carried_row(&label(&lab, branch))), "{out}");
    }
}

// ---------------------------------------------------------------------------
// R9: a member is carried when the release holds its own lines.
// ---------------------------------------------------------------------------

/// The file most R9 tests change: no release log, and no indentation.
const JS: &str = "lib.js";
const IMPORT: &str = "import os from \"os\";";
const VALUE_1: &str = "const value = 1;";
const VALUE_2: &str = "const value = 2;";
const LOG: &str = "console.log(value);";
const HELPER_OPEN: &str = "function helper() {";
const HELPER_RETURN: &str = "return os.homedir();";
const CLOSE: &str = "}";

/// A trunk whose `lib.js` is `trunk`, with [`M`] and [`N`] each writing it,
/// and a release made outside knives that merges both, with `lib.js` as
/// `release`. The release's commit.
fn js_release(
    trunk: &[&str],
    (member, neighbour): (&[&str], &[&str]),
    release: &[&str],
) -> (Lab, TempDir, CommitId) {
    let base = lines(trunk);
    let lab = trunk_with(&[(JS, base.as_str())]);
    lab.branch(M, JS, &lines(member));
    lab.branch(N, JS, &lines(neighbour));
    let content = lines(release);
    let commit = hand_release(&lab, RELEASE, &[M, N], |work| {
        write_file(work, JS, &content);
    });
    let (home, _consumer) = release_test_home(&lab);
    (lab, home, commit)
}

/// M adds a function after the first line of `lib.js`; N changes the line
/// right after that place; the release, cut outside knives, merges both and
/// holds both changes.
fn neighbour_edit_release() -> (Lab, TempDir, CommitId) {
    js_release(
        &[IMPORT, VALUE_1, LOG],
        (
            &[IMPORT, HELPER_OPEN, HELPER_RETURN, CLOSE, VALUE_1, LOG],
            &[IMPORT, VALUE_2, LOG],
        ),
        &[IMPORT, HELPER_OPEN, HELPER_RETURN, CLOSE, VALUE_2, LOG],
    )
}

#[test]
fn a_neighbour_edit_leaves_a_member_carried_in_every_gate() {
    // Given: M adds a function; N changes the line right after it; the
    // release merges both and holds both changes.
    let (lab, home, release) = neighbour_edit_release();

    // When/Then: `members --verify` exits 0 and reads M and N as carried.
    let verified = verify(&lab, &home, RELEASE);
    let out = stdout(&verified);
    assert_eq!(verified.status.code(), Some(0), "{}", text(&verified));
    for branch in [M, N] {
        assert!(out.contains(&carried_row(&label(&lab, branch))), "{out}");
    }

    // And: `--carries` and `--census` read M carried-exact, exit 0, as
    // `--verify` does: the release holds every line of M's change, and N's
    // edit next to M's lines changes none of them.
    let tip = commit_at(&lab, M);
    let carried = carries(&lab, &home, M);
    let carried_out = stdout(&carried);
    assert!(
        carried_out.contains(&format!(
            "carried-exact      {RELEASE} @ {}  (evidence {}: tip is an ancestor and the \
             release holds every line of its change)",
            release.short(),
            tip.short()
        )),
        "{}",
        text(&carried)
    );
    assert!(!carried_out.contains("merged-then-edited"), "{carried_out}");
    assert_eq!(carried.status.code(), Some(0), "{}", text(&carried));
    let census_out = census(&lab, &home);
    for branch in [M, N] {
        assert!(
            census_block(&census_out, branch).contains(&format!(
                "carried-exact      {RELEASE} @ {}",
                release.short()
            )),
            "{census_out}"
        );
    }

    // And: `release cut NAME --at` records the release.
    let recorded = record(&lab, &home, RELEASE, &release);
    assert_eq!(recorded.status.code(), Some(0), "{}", text(&recorded));
    assert!(
        text(&recorded).contains(&recorded_line(RELEASE, &release)),
        "{}",
        text(&recorded)
    );
}

#[test]
fn a_next_cut_over_a_neighbour_edit_reads_the_member_carried() {
    // Given: the release of the neighbour edit, recorded; then `feat/o`, on
    // a file of its own, included.
    let (lab, home, release) = neighbour_edit_release();
    let recorded = record(&lab, &home, RELEASE, &release);
    lab.branch(O, "o.txt", "o\n");
    let included = knives_release(&lab, &home, &["include", O]);

    // When: a next knives cut over it.
    let cut = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: its audit names M in neither its missing rows nor its carried
    // forward rows, and the cut is made.
    let out = stdout(&cut);
    assert_eq!(
        divergence_rows(&out, M),
        Vec::<&str>::new(),
        "{}",
        text(&cut)
    );
    assert!(out.contains(&format!("  cut {NEXT} as ")), "{}", text(&cut));
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));

    // And: the record and the include before it went through.
    assert_eq!(recorded.status.code(), Some(0), "{}", text(&recorded));
    assert_eq!(included.status.code(), Some(0), "{}", text(&included));
}

#[test]
fn an_edit_between_two_added_lines_leaves_the_member_carried() {
    // Given: M adds a line before `const value` and one after it; N changes
    // that line, between M's two; the release holds all three.
    let (first, second) = ("const first = 0;", "const second = 3;");
    let (lab, home, _release) = js_release(
        &[IMPORT, VALUE_1, LOG],
        (
            &[IMPORT, first, VALUE_1, second, LOG],
            &[IMPORT, VALUE_2, LOG],
        ),
        &[IMPORT, first, VALUE_2, second, LOG],
    );

    // When: the release's members are verified.
    let output = verify(&lab, &home, RELEASE);

    // Then: exit 0, and M and N read carried.
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    for branch in [M, N] {
        assert!(out.contains(&carried_row(&label(&lab, branch))), "{out}");
    }
}

/// The call M adds on line 3 of `lib.js`.
const CALL: &str = "run(value, 2);";
/// N's line at the same place: M's call, wrapped in another.
const WRAPPED: &str = "held(run(value, 2));";
/// How each command names [`CALL`], which the release holds only by its
/// words, after the member's label.
const WORD_HELD: &str =
    "lib.js:3 held only by its words ‹run(value, 2);›; release line 3 ‹held(run(value, 2));›";

/// M adds [`CALL`] after `lib.js`'s second line; N adds [`WRAPPED`] at the
/// same place; a release made outside knives holds the wrapped call.
fn wrapped_call_release() -> (Lab, TempDir, CommitId) {
    js_release(
        &[IMPORT, VALUE_1, LOG],
        (
            &[IMPORT, VALUE_1, CALL, LOG],
            &[IMPORT, VALUE_1, WRAPPED, LOG],
        ),
        &[IMPORT, VALUE_1, WRAPPED, LOG],
    )
}

/// `--carries` and `--census` of [`M`], which read the release's own lines as
/// `--verify` does, read merged-then-edited: M's line is held only by its
/// words, since N's wrap of [`CALL`] changed what M brought.
fn assert_carries_and_census_read_merged_then_edited(lab: &Lab, home: &TempDir) {
    let carried = carries(lab, home, M);
    let out = stdout(&carried);
    assert!(
        out.contains(&format!("merged-then-edited {RELEASE} @ ")),
        "{}",
        text(&carried)
    );
    assert_eq!(carried.status.code(), Some(3), "{}", text(&carried));
    let census_out = census(lab, home);
    assert!(
        census_block(&census_out, M).contains(&format!("merged-then-edited {RELEASE} @ ")),
        "{census_out}"
    );
}

#[test]
fn a_wrapped_call_is_held_by_its_words_and_named() {
    // Given: N wraps the call that M adds on one line, and the release holds
    // the wrapped line.
    let (lab, home, release) = wrapped_call_release();
    let member = label(&lab, M);

    // When/Then: `--verify` reads M carried and exits 0, and its text names
    // M's line as held only by its words, with the release's line that holds
    // them.
    let verified = verify(&lab, &home, RELEASE);
    let out = stdout(&verified);
    assert_eq!(verified.status.code(), Some(0), "{}", text(&verified));
    assert!(out.contains(&carried_row(&member)), "{out}");
    assert!(out.contains(&format!("  {member}: {WORD_HELD}")), "{out}");

    // And: its JSON report names the line the same way.
    let report = verify_json(&lab, &home, RELEASE);
    let entry = audit_member(&report, &commit_at(&lab, M));
    assert_eq!(entry["reading"], "carried", "{report}");
    assert_eq!(
        entry["held_by_words"],
        json!([{
            "path": JS,
            "line": 3,
            "text": CALL,
            "release_lines": [{ "line": 3, "text": WRAPPED }],
        }]),
        "{report}"
    );

    // And: `--carries` and `--census`, which hold only a line the release
    // keeps as it was, leave it to a person.
    assert_carries_and_census_read_merged_then_edited(&lab, &home);

    // And: the audit of a next knives cut over the recorded release names
    // the line as `--verify` does.
    let recorded = record(&lab, &home, RELEASE, &release);
    assert_eq!(recorded.status.code(), Some(0), "{}", text(&recorded));
    // The working copy steps off the release, or the reap keeps it for that.
    lab.jj_work(["new", "main@origin"]);
    let cut = knives_release(&lab, &home, &["cut", NEXT]);
    assert!(stdout(&cut).contains(WORD_HELD), "{}", text(&cut));
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
}

/// The files of one K11 (5) scenario, each `lib.js`: the trunk's; M's and
/// N's; the trunk's after N landed and a later commit; and `M2`'s, M's
/// change made again on that later trunk.
struct Rewrite<'a> {
    trunk: &'a str,
    member: &'a str,
    neighbour: &'a str,
    later_trunk: &'a str,
    rewritten: &'a str,
}

/// K11 (5): M's function and N's edit apart at first, by `const keep`. The
/// later trunk drops that line, so `M2` holds M's lines beside N's edit.
const NEIGHBOUR_REWRITE: Rewrite<'static> = Rewrite {
    trunk: "import os from \"os\";\nconst keep = 0;\nconst value = 1;\nconsole.log(value);\n",
    member: "import os from \"os\";\nfunction helper() {\nreturn os.homedir();\n}\n\
             const keep = 0;\nconst value = 1;\nconsole.log(value);\n",
    neighbour: "import os from \"os\";\nconst keep = 0;\nconst value = 2;\nconsole.log(value);\n",
    later_trunk: "import os from \"os\";\nconst value = 2;\nconsole.log(value);\n",
    rewritten: "import os from \"os\";\nfunction helper() {\nreturn os.homedir();\n}\n\
                const value = 2;\nconsole.log(value);\n",
};

/// K11 (5)'s shape with N's edit on M's line: N adds `strict` to a call of
/// `make`, and `M2` adds it to M's call too.
const WORD_REWRITE: Rewrite<'static> = Rewrite {
    trunk: "import os from \"os\";\nconst keep = 0;\nconst base = make(1);\nconsole.log(base);\n",
    member: "import os from \"os\";\nconst helper = make(2);\nconst keep = 0;\n\
             const base = make(1);\nconsole.log(base);\n",
    neighbour: "import os from \"os\";\nconst keep = 0;\nconst base = make(1, strict);\n\
                console.log(base);\n",
    later_trunk: "import os from \"os\";\nconst keep = 0;\nconst base = make(1, strict);\n\
                  console.info(base);\n",
    rewritten: "import os from \"os\";\nconst helper = make(2, strict);\nconst keep = 0;\n\
                const base = make(1, strict);\nconsole.info(base);\n",
};

/// Make `M2` in the second clone: M's change made again by hand on the trunk
/// that clone fetches, with `lib.js` as `content` and a change id of its
/// own; move M's bookmark there and push it. No record here says that `M2`
/// stands for M.
fn rewrite_in_second_clone_on_trunk(lab: &Lab, content: &str) {
    lab.jj_at(&lab.second, ["git", "fetch", "--remote", "origin"]);
    lab.jj_at(&lab.second, ["new", "-r", "main@origin", "-m", M]);
    write_file(&lab.second, JS, content);
    lab.jj_at(
        &lab.second,
        ["bookmark", "set", M, "-r", "@", "--allow-backwards"],
    );
    lab.jj_at(&lab.second, ["new"]);
    lab.jj_at(
        &lab.second,
        ["git", "push", "--remote", "origin", "--bookmark", M],
    );
}

/// After a knives cut of [`M`] and [`N`], N lands upstream by a merge commit,
/// and a later trunk commit follows. `M2`, M made again in the second clone
/// on that trunk, is pushed, and M's bookmark here moves to it on the fetch.
/// [`O`] forks there. M's first commit, which the cut recorded, is returned.
fn rewrite_lab(files: &Rewrite<'_>) -> (Lab, TempDir, CommitId) {
    let lab = trunk_with(&[(JS, files.trunk)]);
    lab.branch(M, JS, files.member);
    lab.branch(N, JS, files.neighbour);
    lab.track_in_second_clone(M);
    let (home, _consumer) = home_after_first_cut(&lab);
    let recorded = commit_at(&lab, M);
    lab.publish_pull(N, 1);
    lab.merge_pull_with_merge_commit(1);
    lab.upstream_trunk_file(JS, files.later_trunk);
    lab.mirror_upstream_trunk_to_origin();
    rewrite_in_second_clone_on_trunk(&lab, files.rewritten);
    lab.jj_work(["git", "fetch", "--remote", "origin"]);
    assert_ne!(commit_at(&lab, M), recorded, "M's bookmark moved to M2");
    lab.branch(O, "o.txt", "o\n");
    (lab, home, recorded)
}

#[test]
fn the_gate_does_not_refuse_a_rewrite_whose_lines_the_cut_holds() {
    // Given: a recorded cut of M and N; then a release made outside knives
    // whose members are M2, a rewrite of M with the same change on a later
    // trunk commit, made in another clone with a change id of its own, and
    // O. It holds M's lines beside N's edit and does not reach M.
    let (lab, home, _recorded) = rewrite_lab(&NEIGHBOUR_REWRITE);
    let subject = hand_release(&lab, NEXT, &[M, O], |_| {});

    // When: it is recorded as the next release.
    let output = record(&lab, &home, NEXT, &subject);

    // Then: the composition gate reads M's lines in it and does not refuse
    // it for M: it is recorded, and no lacking lines are listed.
    let out = text(&output);
    assert!(!out.contains("does not carry"), "{out}");
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert!(out.contains(&recorded_line(NEXT, &subject)), "{out}");
    assert!(!out.contains("lines each member lacks:"), "{out}");
}

#[test]
fn the_gate_names_a_recorded_members_line_held_only_by_its_words() {
    // Given: the shape of the test above, with N's edit on M's line: the
    // release outside knives holds M's call with N's argument added.
    let (lab, home, _recorded) = rewrite_lab(&WORD_REWRITE);
    let subject = hand_release(&lab, NEXT, &[M, O], |_| {});

    // When: it is recorded as the next release.
    let output = record(&lab, &home, NEXT, &subject);

    // Then: it is recorded, and the gate names M's line as held only by its
    // words, with the release's line that holds them.
    let out = text(&output);
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert!(
        out.contains(
            "lib.js:2 held only by its words ‹const helper = make(2);›; release line 2 \
             ‹const helper = make(2, strict);›"
        ),
        "{out}"
    );
}

#[test]
fn the_gate_names_the_lines_a_refused_member_lacks() {
    // Given: a knives cut of M, which writes two lines of `m.txt`, and N;
    // then a release made outside knives of N and O alone.
    let lab = Lab::new();
    lab.branch(M, "m.txt", "m one\nm two\n");
    lab.branch(N, "n.txt", "n\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch(O, "o.txt", "o\n");
    let subject = hand_release(&lab, NEXT, &[N, O], |_| {});

    // When: it is recorded as the next release.
    let output = record(&lab, &home, NEXT, &subject);

    // Then: the gate refuses it for M, and after its refusal names each line
    // of M that it lacks.
    let out = text(&output);
    assert_eq!(output.status.code(), Some(REFUSED), "{out}");
    assert!(out.contains("does not carry 1 member(s)"), "{out}");
    let (_, lacks) = out
        .split_once("  lines each member lacks:\n")
        .unwrap_or_else(|| panic!("no list of the lines each member lacks: {out}"));
    for (line, content) in [(1, "m one"), (2, "m two")] {
        assert!(
            lacks.contains(&format!("m.txt:{line}: added line lost ‹{content}›")),
            "{out}"
        );
    }
}

#[test]
fn a_cut_with_allow_drop_names_the_lines_a_dropped_member_lacks() {
    // Given: a knives cut of M, which writes two lines of `m.txt`, and N;
    // then M dropped from the release.
    let lab = Lab::new();
    lab.branch(M, "m.txt", "m one\nm two\n");
    lab.branch(N, "n.txt", "n\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let dropped = knives_release(&lab, &home, &["drop", M, "--why", "not this time"]);
    assert_eq!(dropped.status.code(), Some(0), "{}", text(&dropped));

    // When: the next cut is taken with M's drop stated.
    let m = commit_at(&lab, M);
    let output = knives_release(&lab, &home, &["cut", NEXT, "--allow-drop", m.short()]);

    // Then: the cut goes ahead, its `--allow-drop` line names M, and after
    // it the gate names each line of M that the cut lacks.
    let out = text(&output);
    assert_eq!(output.status.code(), Some(0), "{out}");
    let (allowed, lacks) = out
        .split_once("  lines each member lacks:\n")
        .unwrap_or_else(|| panic!("no list of the lines each member lacks: {out}"));
    assert!(
        allowed
            .lines()
            .any(|line| line.contains("--allow-drop: ") && line.contains(M)),
        "{out}"
    );
    for (line, content) in [(1, "m one"), (2, "m two")] {
        assert!(
            lacks.contains(&format!("m.txt:{line}: added line lost ‹{content}›")),
            "{out}"
        );
    }
}

/// M adds a function to `lib.js`, N writes a file of its own, and a release
/// made outside knives holds another text of M's function.
fn another_text_release() -> (Lab, TempDir) {
    let base = lines(&[IMPORT, VALUE_1, LOG]);
    let lab = trunk_with(&[(JS, base.as_str())]);
    lab.branch(
        M,
        JS,
        &lines(&[IMPORT, VALUE_1, LOG, HELPER_OPEN, HELPER_RETURN, CLOSE]),
    );
    lab.branch(N, "n.txt", "n\n");
    let other = lines(&[
        IMPORT,
        VALUE_1,
        LOG,
        HELPER_OPEN,
        "return os.tmpdir();",
        CLOSE,
    ]);
    hand_release(&lab, RELEASE, &[M, N], |work| write_file(work, JS, &other));
    let (home, _consumer) = release_test_home(&lab);
    (lab, home)
}

#[test]
fn the_release_holds_another_text_of_a_members_function() {
    // Given: the release holds another text of M's function.
    let (lab, home) = another_text_release();

    // When/Then: M reads missing, with the findings exit, in the text and in
    // the JSON report.
    assert_member_missing(&lab, &home);
    let report = verify_json(&lab, &home, RELEASE);
    assert!(lists_missing(&report, &label(&lab, M)), "{report}");
}

#[test]
fn the_release_holds_another_text_of_a_members_function_names_its_lines() {
    // Given: the release holds another text of M's function.
    let (lab, home) = another_text_release();

    // When: the release's members are verified.
    let out = assert_member_missing(&lab, &home);

    // Then: the text names the line of M that the release lacks, with its
    // path, under M's row.
    assert!(
        out.contains(&format!(
            "{}\n      lib.js:5: added line lost ‹{HELPER_RETURN}›",
            missing_row(&label(&lab, M))
        )),
        "{out}"
    );

    // And: so does the JSON report.
    let report = verify_json(&lab, &home, RELEASE);
    let entry = audit_member(&report, &commit_at(&lab, M));
    assert_eq!(entry["reading"], "missing", "{report}");
    assert_eq!(
        entry["lost"],
        json!([{
            "path": JS,
            "kind": "added",
            "line": 5,
            "text": HELPER_RETURN,
            "carried_forward": false,
        }]),
        "{report}"
    );
}

/// A trunk whose `file` is `trunk`; M writes it as `member` and N as
/// `neighbour`; a release made outside knives holds N's version.
fn neighbour_version_release(
    file: &str,
    (trunk, member, neighbour): (&str, &str, &str),
) -> (Lab, TempDir) {
    let lab = trunk_with(&[(file, trunk)]);
    lab.branch(M, file, member);
    lab.branch(N, file, neighbour);
    hand_release(&lab, RELEASE, &[M, N], |work| {
        write_file(work, file, neighbour);
    });
    let (home, _consumer) = release_test_home(&lab);
    (lab, home)
}

#[test]
fn a_release_that_puts_back_a_two_line_removed_hunk_reads_missing() {
    // Given: M removes two lines of `lib.js`; N changes another line; the
    // release holds both removed lines again, at their place.
    let (first, removed_one, removed_two, last) = (
        "const a = 1;",
        "const b = 2;",
        "const c = 3;",
        "const d = 4;",
    );
    let (lab, home) = neighbour_version_release(
        JS,
        (
            &lines(&[first, removed_one, removed_two, last, "const e = 5;"]),
            &lines(&[first, last, "const e = 5;"]),
            &lines(&[first, removed_one, removed_two, last, "const e = 50;"]),
        ),
    );

    // When/Then: M reads missing.
    assert_member_missing(&lab, &home);
}

#[test]
fn a_release_that_puts_back_one_line_of_a_removed_hunk_reads_carried() {
    // Given: M removes the two lines `go();` and `}`; N removes `go();`
    // alone; the release holds N's version, so of M's hunk it puts back only
    // a lone `}`.
    let (start, open, finish) = ("start();", "if (ready) {", "finish();");
    let (lab, home) = neighbour_version_release(
        JS,
        (
            &lines(&[start, open, "go();", CLOSE, finish]),
            &lines(&[start, open, finish]),
            &lines(&[start, open, CLOSE, finish]),
        ),
    );

    // When: the release's members are verified.
    let output = verify(&lab, &home, RELEASE);

    // Then: M and N read carried.
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    for branch in [M, N] {
        assert!(out.contains(&carried_row(&label(&lab, branch))), "{out}");
    }
}

#[test]
fn a_release_that_puts_back_a_removed_run_with_one_line_edited_reads_missing() {
    // Given: M removes `a`, `b` and `c`; N changes `b` to `X`; the release
    // holds N's version, as a modify/delete conflict resolved toward the
    // edit leaves it: `a` and `c` are back.
    let (lab, home) =
        neighbour_version_release("f.txt", ("p\na\nb\nc\nq\n", "p\nq\n", "p\na\nX\nc\nq\n"));

    // When/Then: M reads missing, named by `a` and `c`, not `b`; N reads
    // carried.
    let out = assert_member_missing(&lab, &home);
    for line in [
        "f.txt:2: removed line put back ‹a›",
        "f.txt:4: removed line put back ‹c›",
    ] {
        assert!(out.contains(&format!("      {line}")), "{out}");
    }
    assert!(!out.contains("put back ‹b›"), "{out}");
    assert!(out.contains(&carried_row(&label(&lab, N))), "{out}");
}

#[test]
fn a_release_that_holds_an_emptied_file_again_with_one_line_edited_reads_missing() {
    // Given: M empties `f.txt`; N changes its line `b` to `B`; the release
    // holds N's version.
    let (lab, home) = neighbour_version_release("f.txt", ("a\nb\nc\n", "", "a\nB\nc\n"));

    // When/Then: M reads missing, named by `a` and `c`; N reads carried.
    let out = assert_member_missing(&lab, &home);
    for line in [
        "f.txt:1: removed line put back ‹a›",
        "f.txt:3: removed line put back ‹c›",
    ] {
        assert!(out.contains(&format!("      {line}")), "{out}");
    }
    assert!(out.contains(&carried_row(&label(&lab, N))), "{out}");
}

#[test]
fn a_release_that_writes_every_removed_line_back_edited_reads_carried() {
    // Given (a control): M removes `a`, `b` and `c`; N changes each of
    // them; the release holds N's version, which holds none of M's removed
    // lines again.
    let (lab, home) =
        neighbour_version_release("f.txt", ("p\na\nb\nc\nq\n", "p\nq\n", "p\nA\nB\nC\nq\n"));

    // When/Then: M and N read carried.
    assert_member_carried(&lab, &home);
    let out = stdout(&verify(&lab, &home, RELEASE));
    assert!(out.contains(&carried_row(&label(&lab, N))), "{out}");
}

#[test]
fn a_release_where_another_members_method_ends_in_the_removed_lines_brace_reads_carried() {
    // Given: M removes a method of an object; N adds another method at the
    // same place; the release holds N's version, whose new method ends in
    // the `},` that M removed.
    let (lab, home) = neighbour_version_release(
        "f.ts",
        (
            "const o = {\n  a: 1,\n  b: () => {\n    old();\n  },\n};\n",
            "const o = {\n  a: 1,\n};\n",
            "const o = {\n  a: 1,\n  c: () => {\n    go();\n  },\n};\n",
        ),
    );

    // When/Then: M and N read carried: the `},` is N's.
    assert_member_carried(&lab, &home);
    let out = stdout(&verify(&lab, &home, RELEASE));
    assert!(out.contains(&carried_row(&label(&lab, N))), "{out}");
    assert!(!out.contains("put back"), "{out}");
}

#[test]
fn a_release_that_puts_back_a_stray_brace_the_member_removed_reads_missing() {
    // Given: M removes a stray `}`; N adds a line at the end; the release
    // holds N's version, which still has the `}`.
    let (lab, home) = neighbour_version_release(
        JS,
        (
            "run() {\n  go();\n}\n}\nfinish();\n",
            "run() {\n  go();\n}\nfinish();\n",
            "run() {\n  go();\n}\n}\nfinish();\nmore();\n",
        ),
    );

    // When/Then: M reads missing: its whole hunk is back, as R9 reads it,
    // so the `}` is named although it holds no letter or digit; N reads
    // carried.
    let out = assert_member_missing(&lab, &home);
    assert!(out.contains("removed line put back ‹}›"), "{out}");
    assert!(out.contains(&carried_row(&label(&lab, N))), "{out}");
}

/// The Python file of K11 (12) to (14), whose lines the check words.
const PY: &str = "lib.py";

/// K11 (12): M removes an argument of `call`; N adds another; the release
/// holds N's line, which keeps the argument M removed.
fn removed_argument_release() -> (Lab, TempDir) {
    neighbour_version_release(
        PY,
        (
            "import os\ncall(a, insecure=True)\nprint(a)\n",
            "import os\ncall(a)\nprint(a)\n",
            "import os\ncall(a, insecure=True, timeout=5)\nprint(a)\n",
        ),
    )
}

#[test]
fn a_removed_argument_put_back_beside_another_reads_missing() {
    // Given: M changes `call(a, insecure=True)` to `call(a)`; N adds
    // `timeout=5`; the release holds `call(a, insecure=True, timeout=5)`.
    let (lab, home) = removed_argument_release();

    // When/Then: M reads missing.
    assert_member_missing(&lab, &home);
}

#[test]
fn a_removed_argument_put_back_beside_another_reads_missing_names_its_lines() {
    // Given: the same release.
    let (lab, home) = removed_argument_release();

    // When: the release's members are verified.
    let out = assert_member_missing(&lab, &home);

    // Then: the output names M's line.
    assert!(
        out.contains(&format!(
            "{}\n      lib.py:2: added line lost ‹call(a)›",
            missing_row(&label(&lab, M))
        )),
        "{out}"
    );
}

/// K11 (13): M re-indents a block of four lines; N edits one of them and
/// keeps the old indentation, which the release holds.
fn reindented_block_release() -> (Lab, TempDir) {
    neighbour_version_release(
        PY,
        (
            "if ok:\n  one()\n  two()\n  three()\n  four()\n",
            "if ok:\n    one()\n    two()\n    three()\n    four()\n",
            "if ok:\n  one()\n  two(x)\n  three()\n  four()\n",
        ),
    )
}

#[test]
fn a_reindented_block_the_release_keeps_unindented_reads_missing() {
    // Given: M re-indents four lines; the release keeps their old
    // indentation, and N's edit of one of them.
    let (lab, home) = reindented_block_release();

    // When/Then: M reads missing.
    assert_member_missing(&lab, &home);
}

#[test]
fn a_reindented_block_the_release_keeps_unindented_reads_missing_names_its_lines() {
    // Given: the same release.
    let (lab, home) = reindented_block_release();

    // When: the release's members are verified.
    let out = assert_member_missing(&lab, &home);

    // Then: the output names each of M's four lines.
    for line in 2..=5 {
        assert!(
            out.contains(&format!("      lib.py:{line}: added line lost ‹")),
            "{out}"
        );
    }
}

/// K11 (14): M changes `call(a, b)` to `call(a)`; N replaces that line with
/// `pass`, which the release holds.
fn replaced_line_release() -> (Lab, TempDir) {
    neighbour_version_release(
        PY,
        (
            "import os\ncall(a, b)\nprint(a)\n",
            "import os\ncall(a)\nprint(a)\n",
            "import os\npass\nprint(a)\n",
        ),
    )
}

#[test]
fn a_line_the_release_replaced_whole_reads_missing() {
    // Given: the release holds N's `pass` where M wrote `call(a)`.
    let (lab, home) = replaced_line_release();

    // When/Then: M reads missing.
    assert_member_missing(&lab, &home);
}

#[test]
fn a_line_the_release_replaced_whole_reads_missing_names_its_lines() {
    // Given: the same release.
    let (lab, home) = replaced_line_release();

    // When: the release's members are verified.
    let out = assert_member_missing(&lab, &home);

    // Then: the output names M's line.
    assert!(
        out.contains(&format!(
            "{}\n      lib.py:2: added line lost ‹call(a)›",
            missing_row(&label(&lab, M))
        )),
        "{out}"
    );
}

/// The members of K11 (10), in the release's parent order.
const NON_TEXT: [&str; 5] = [
    "feat/kept-bin",
    "feat/other-bin",
    "feat/deleter",
    "feat/empty",
    "feat/exec",
];

/// A trunk that holds `gone.txt` and `run.sh`; five members: one changes the
/// binary `kept.bin`, which the release holds as the member wrote it; one
/// changes `other.bin`, which the release holds in another version; one
/// deletes `gone.txt`, which the release still holds; one adds an empty
/// `empty.txt`, whose path the release holds; and one sets the executable
/// bit of `run.sh`, which the release's file lacks.
fn non_text_release() -> (Lab, TempDir) {
    let lab = trunk_with(&[("gone.txt", "gone\n"), ("run.sh", "echo run\n")]);
    lab.branch_with_bytes("feat/kept-bin", "kept.bin", b"\0kept by the member\n");
    lab.branch_with_bytes("feat/other-bin", "other.bin", b"\0the member's version\n");
    branch_on(&lab, "main@origin", "feat/deleter", |work| {
        std::fs::remove_file(work.join("gone.txt")).expect("delete gone.txt");
    });
    lab.branch("feat/empty", "empty.txt", "");
    branch_on(&lab, "main@origin", "feat/exec", |work| {
        set_mode(&work.join("run.sh"), 0o755);
    });
    hand_release(&lab, RELEASE, &NON_TEXT, |work| {
        std::fs::write(work.join("other.bin"), b"\0the release's version\n")
            .expect("write the release's other.bin");
        write_file(work, "gone.txt", "gone\n");
        set_mode(&work.join("run.sh"), 0o644);
    });
    let (home, _consumer) = release_test_home(&lab);
    (lab, home)
}

#[test]
fn a_binary_file_a_deletion_and_a_mode_the_release_replaced_read_missing() {
    // Given: the five members of K11 (10) and their release.
    let (lab, home) = non_text_release();

    // When: the release's members are verified.
    let output = verify(&lab, &home, RELEASE);

    // Then: the members whose binary file, deletion or executable bit the
    // release does not hold read missing, in the text and in the JSON.
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(FINDINGS), "{}", text(&output));
    let report = verify_json(&lab, &home, RELEASE);
    for branch in ["feat/other-bin", "feat/deleter", "feat/exec"] {
        let member = label(&lab, branch);
        assert!(out.contains(&missing_row(&member)), "{out}");
        assert!(lists_missing(&report, &member), "{report}");
    }
}

#[test]
fn non_text_deleted_empty_and_mode_changes_read_by_their_rule() {
    // Given: the five members of K11 (10) and their release.
    let (lab, home) = non_text_release();

    // When: the release's members are verified.
    let output = verify(&lab, &home, RELEASE);

    // Then: the binary file the release holds as the member wrote it, and
    // the empty file whose path it holds, read carried.
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(FINDINGS), "{}", text(&output));
    for branch in ["feat/kept-bin", "feat/empty"] {
        assert!(out.contains(&carried_row(&label(&lab, branch))), "{out}");
    }

    // And: each other member reads missing, and its lost path is named under
    // its row: the other binary version, the file the release still holds,
    // and the executable bit.
    for (branch, named) in [
        (
            "feat/other-bin",
            "      other.bin: file differs from the member's (",
        ),
        (
            "feat/deleter",
            "      gone.txt: file differs from the member's (",
        ),
        (
            "feat/exec",
            "      run.sh: mode differs from the member's (executable yes)",
        ),
    ] {
        assert!(
            out.contains(&format!("{}\n{named}", missing_row(&label(&lab, branch)))),
            "{out}"
        );
    }
}

/// A branch `name` on the fork's trunk whose one commit adds a submodule at
/// `sub`, made with git, as jj makes none. The gitlink names the trunk's own
/// commit. jj imports the branch on its next command.
fn submodule_branch(lab: &Lab, name: &str) {
    let base = commit_at(lab, "main@origin");
    let index = lab.temp_path().join("submodule.index");
    let git = |args: &[&str]| -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&lab.work)
            .env("GIT_INDEX_FILE", &index)
            .env("GIT_AUTHOR_NAME", "Knives Lab")
            .env("GIT_AUTHOR_EMAIL", "knives-lab@example.test")
            .env("GIT_COMMITTER_NAME", "Knives Lab")
            .env("GIT_COMMITTER_EMAIL", "knives-lab@example.test")
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?}: {}", text(&output));
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    git(&["read-tree", base.as_str()]);
    let gitlink = format!("160000,{},sub", base.as_str());
    git(&["update-index", "--add", "--cacheinfo", &gitlink]);
    let tree = git(&["write-tree"]);
    let commit = git(&["commit-tree", &tree, "-p", base.as_str(), "-m", name]);
    git(&["update-ref", &format!("refs/heads/{name}"), &commit]);
}

#[test]
fn a_symlink_and_a_submodule_are_carried_when_the_release_holds_the_tips_value() {
    // Given: `feat/link` adds a symlink to `a.txt`; `feat/other-link` adds
    // another; `feat/sub` adds a submodule; the release holds the first link
    // and the submodule as the tips hold them, and points the other link at
    // `b.txt`.
    let lab = trunk_with(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
    branch_on(&lab, "main@origin", "feat/link", |work| {
        symlink("a.txt", &work.join("link"));
    });
    branch_on(&lab, "main@origin", "feat/other-link", |work| {
        symlink("a.txt", &work.join("other-link"));
    });
    submodule_branch(&lab, "feat/sub");
    hand_release(
        &lab,
        RELEASE,
        &["feat/link", "feat/other-link", "feat/sub"],
        |work| {
            std::fs::remove_file(work.join("other-link")).expect("remove other-link");
            symlink("b.txt", &work.join("other-link"));
        },
    );
    let (home, _consumer) = release_test_home(&lab);

    // When: the release's members are verified.
    let output = verify(&lab, &home, RELEASE);

    // Then: the link and the submodule the release holds as the tips hold
    // them read carried; the other link reads missing, named.
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(FINDINGS), "{}", text(&output));
    for branch in ["feat/link", "feat/sub"] {
        assert!(out.contains(&carried_row(&label(&lab, branch))), "{out}");
    }
    assert!(
        out.contains(&format!(
            "{}\n      other-link: file differs from the member's (",
            missing_row(&label(&lab, "feat/other-link"))
        )),
        "{out}"
    );
}

#[test]
fn an_executable_bit_on_a_binary_file_is_read_by_its_mode() {
    // Given: two members each set the executable bit of a binary file of the
    // trunk; the release keeps the first bit and not the second.
    let lab = trunk_with(&[("a.bin", "\0tool a\n"), ("b.bin", "\0tool b\n")]);
    for (branch, file) in [("feat/exec-a", "a.bin"), ("feat/exec-b", "b.bin")] {
        branch_on(&lab, "main@origin", branch, |work| {
            set_mode(&work.join(file), 0o755);
        });
    }
    hand_release(&lab, RELEASE, &["feat/exec-a", "feat/exec-b"], |work| {
        set_mode(&work.join("b.bin"), 0o644);
    });
    let (home, _consumer) = release_test_home(&lab);

    // When: the release's members are verified.
    let output = verify(&lab, &home, RELEASE);

    // Then: the member whose bit the release keeps reads carried; the other
    // reads missing, and its bit is named.
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(FINDINGS), "{}", text(&output));
    assert!(
        out.contains(&carried_row(&label(&lab, "feat/exec-a"))),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "{}\n      b.bin: mode differs from the member's (executable yes)",
            missing_row(&label(&lab, "feat/exec-b"))
        )),
        "{out}"
    );
}

#[test]
fn a_rename_reads_as_a_deletion_and_an_addition() {
    // Given: M renames `a.txt` to `b.txt` with no change of content; N writes
    // a file of its own; one release holds `b.txt` and no `a.txt`, and
    // another holds `a.txt` only.
    let lab = trunk_with(&[("a.txt", "moved line\n")]);
    branch_on(&lab, "main@origin", M, |work| {
        std::fs::rename(work.join("a.txt"), work.join("b.txt")).expect("rename a.txt");
    });
    lab.branch(N, "n.txt", "n\n");
    hand_release(&lab, RELEASE, &[M, N], |_| {});
    hand_release(&lab, NEXT, &[M, N], |work| {
        std::fs::rename(work.join("b.txt"), work.join("a.txt")).expect("rename b.txt back");
    });
    let (home, _consumer) = release_test_home(&lab);
    let member = label(&lab, M);

    // When/Then: against the release that holds `b.txt` only, M reads
    // carried.
    let renamed = verify(&lab, &home, RELEASE);
    assert_eq!(renamed.status.code(), Some(0), "{}", text(&renamed));
    assert!(
        stdout(&renamed).contains(&carried_row(&member)),
        "{}",
        text(&renamed)
    );

    // And: against the release that holds `a.txt` only, M reads missing, and
    // both paths are named.
    let kept = verify(&lab, &home, NEXT);
    let out = stdout(&kept);
    assert_eq!(kept.status.code(), Some(FINDINGS), "{}", text(&kept));
    assert!(out.contains(&missing_row(&member)), "{out}");
    assert!(
        out.contains("      b.txt:1: added line lost ‹moved line›"),
        "{out}"
    );
    assert!(
        out.lines().any(|line| line.starts_with("      a.txt:")),
        "{out}"
    );
}

#[test]
fn a_range_over_30000_words_is_not_followed_by_words() {
    // Given: M changes one line of `big.txt`; the release replaces it with a
    // line that holds M's words and more than 30000 others.
    let lab = trunk_with(&[("big.txt", "keep\nkey = 1\nend\n")]);
    lab.branch(M, "big.txt", "keep\nkey = 2\nend\n");
    lab.branch(N, "n.txt", "n\n");
    let huge = format!("keep\nkey = 2{}\nend\n", " w".repeat(30_001));
    hand_release(&lab, RELEASE, &[M, N], |work| {
        write_file(work, "big.txt", &huge);
    });
    let (home, _consumer) = release_test_home(&lab);

    // When: the release's members are verified.
    let out = assert_member_missing(&lab, &home);

    // Then: the line reads lost, and the output says that the word step did
    // not run in that range.
    assert!(
        out.contains(
            "      big.txt:2-2: the word step did not run (more than 30000 words); its lines \
             count as lost"
        ),
        "{out}"
    );
}

/// How long `members --verify` may take over K11 (21)'s hunk before the
/// test kills it.
const VERIFY_LIMIT: Duration = Duration::from_secs(60);

/// K11 (21): `count` words drawn from a vocabulary of 50, `w0` to `w49`, in
/// a fixed sequence.
fn drawn_words(count: usize) -> Vec<String> {
    (0..count).map(|at| format!("w{}", at * 7 % 50)).collect()
}

/// `words` with the word at `at` changed.
fn word_changed(words: &[String], at: usize) -> Vec<String> {
    let mut changed = words.to_vec();
    changed[at] = "changed".to_owned();
    changed
}

/// `words` as one line, with its line feed.
fn word_line(words: &[String]) -> String {
    format!("{}\n", words.join(" "))
}

/// [`verify`] run as a child process, killed when it has not finished
/// within [`VERIFY_LIMIT`], so that a reading with no bound fails the test
/// instead of hanging it. Its streams go to files, which no full pipe can
/// stall. It prints how long the command took.
fn verify_within_the_limit(lab: &Lab, home: &TempDir, release: &str) -> Output {
    let (out_path, err_path) = (
        lab.temp_path().join("verify.stdout"),
        lab.temp_path().join("verify.stderr"),
    );
    let mut child = release_command(
        lab,
        home,
        ReleaseOutput::Text,
        &["members", release, "--verify"],
    )
    .stdout(std::fs::File::create(&out_path).expect("create the stdout file"))
    .stderr(std::fs::File::create(&err_path).expect("create the stderr file"))
    .spawn()
    .expect("start knives release members --verify");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll knives") {
            break status;
        }
        if started.elapsed() >= VERIFY_LIMIT {
            child.kill().expect("kill knives");
            let killed = child.wait().expect("reap knives");
            panic!(
                "`knives release members {release} --verify` did not finish in {} s ({killed})",
                VERIFY_LIMIT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    eprintln!(
        "`knives release members {release} --verify` took {:?}",
        started.elapsed()
    );
    Output {
        status,
        stdout: std::fs::read(&out_path).expect("read the stdout file"),
        stderr: std::fs::read(&err_path).expect("read the stderr file"),
    }
}

#[test]
fn verify_reads_a_line_over_30000_words_by_the_line_rule_in_bounded_time() {
    // Given (K11 21, a control): M's hunk replaces L, a line of 40000 words
    // drawn from 50, by L', L with one word changed, and a short line; N
    // writes a file of its own; the release holds another text of L', one
    // more word changed, and the short line.
    let removed = drawn_words(40_000);
    let copy = word_changed(&removed, 10_000);
    let other = word_changed(&copy, 30_000);
    let short = "a short line\n";
    let fork = format!("keep\n{}end\n", word_line(&removed));
    let lab = trunk_with(&[("big.txt", fork.as_str())]);
    lab.branch(
        M,
        "big.txt",
        &format!("keep\n{}{short}end\n", word_line(&copy)),
    );
    lab.branch(N, "n.txt", "n\n");
    let release = format!("keep\n{}{short}end\n", word_line(&other));
    hand_release(&lab, RELEASE, &[M, N], |work| {
        write_file(work, "big.txt", &release);
    });
    let (home, _consumer) = release_test_home(&lab);

    // When: the release's members are verified, within 60 s.
    let output = verify_within_the_limit(&lab, &home, RELEASE);

    // Then: M reads missing: L' is read by the line rule, lost, and the
    // output says that the word step did not run in its range.
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(FINDINGS), "{}", text(&output));
    assert!(out.contains(&missing_row(&label(&lab, M))), "{out}");
    assert!(
        out.contains(
            "      big.txt:2-2: the word step did not run (more than 30000 words); its lines \
             count as lost"
        ),
        "{out}"
    );
}

#[test]
fn a_conflict_in_the_members_path_is_inconclusive_and_one_elsewhere_is_not() {
    // Given: M and N write `f.txt` each its own way; O writes a file of its
    // own; the release merges all three and leaves `f.txt` conflicted.
    let lab = trunk_with(&[("f.txt", "base line\n")]);
    lab.branch(M, "f.txt", "m line\n");
    lab.branch(N, "f.txt", "n line\n");
    lab.branch(O, "o.txt", "o\n");
    hand_release(&lab, RELEASE, &[M, N, O], |_| {});
    let (home, _consumer) = release_test_home(&lab);

    // When: the release's members are verified.
    let output = verify(&lab, &home, RELEASE);

    // Then: M reads inconclusive, with its conflicted path named; O, whose
    // path holds no conflict, reads carried; nothing fails.
    let out = stdout(&output);
    assert!(
        out.contains(&format!(
            "  {}: content check inconclusive (an unresolved conflict; re-check after resolving \
             it)\n      f.txt: unresolved conflict in the release",
            label(&lab, M)
        )),
        "{out}"
    );
    assert!(out.contains(&carried_row(&label(&lab, O))), "{out}");
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));

    // And: `--carries` reads M, which the release reaches and holds an
    // unresolved conflict in a path of, merged-then-edited, and O
    // carried-exact.
    let conflicted = carries(&lab, &home, M);
    assert!(
        stdout(&conflicted).contains(&format!("merged-then-edited {RELEASE} @ ")),
        "{}",
        text(&conflicted)
    );
    assert_eq!(conflicted.status.code(), Some(3), "{}", text(&conflicted));
    let other = stdout(&carries(&lab, &home, O));
    assert!(
        other.contains(&format!("carried-exact      {RELEASE} @ ")),
        "{other}"
    );
}

#[test]
fn a_member_whose_tip_holds_a_conflict_reads_inconclusive_and_not_carried() {
    // Given: M forks at T0 and changes the line of `f.txt`; the trunk moves
    // to T1, which changes that line too, and N forks there; M is rebased
    // onto T1, which leaves its tip conflicted in `f.txt`; a release made
    // outside knives merges M and N and holds no conflict in `f.txt`.
    let lab = trunk_with(&[("f.txt", "l = 0\n")]);
    lab.branch(M, "f.txt", "l = m\n");
    lab.upstream_trunk_file("f.txt", "l = 1\n");
    branch_on(&lab, "main@upstream", N, |work| {
        write_file(work, "n.txt", "n\n");
    });
    lab.jj_work(["rebase", "-s", M, "-d", "main@upstream"]);
    hand_release(&lab, RELEASE, &[M, N], |work| {
        write_file(work, "f.txt", "l = m\n");
    });
    let (home, _consumer) = release_test_home(&lab);
    let member = label(&lab, M);

    // When: the release's members are verified.
    let output = verify(&lab, &home, RELEASE);

    // Then: M's own change cannot be read in `f.txt`, so it reads
    // inconclusive, not missing, and nothing fails.
    let out = stdout(&output);
    assert!(
        out.contains(&format!("  {member}: content check inconclusive")),
        "{out}"
    );
    assert!(!out.contains(&missing_row(&member)), "{out}");
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));

    // And: `--carries` does not read it carried: the release reaches it, and
    // a tip that holds an unresolved conflict is merged-then-edited, exit 3.
    let carried = carries(&lab, &home, M);
    let out = stdout(&carried);
    assert!(
        out.contains(&format!("merged-then-edited {RELEASE} @ ")),
        "{}",
        text(&carried)
    );
    assert!(!out.contains("carried-"), "{out}");
    assert_eq!(carried.status.code(), Some(3), "{}", text(&carried));
}

/// A knives cut of [`M`], which writes `m.txt` as `line`; the trunk then
/// advances, and M is rebased onto it with jj, which moves the release onto
/// that rewrite of M. M's first commit.
fn rebased_member_lab(line: &str) -> (Lab, TempDir, CommitId) {
    let lab = Lab::new();
    lab.branch(M, "m.txt", line);
    let (home, _consumer) = home_after_first_cut(&lab);
    let first = commit_at(&lab, M);
    lab.advance_upstream("upstream advance\n");
    lab.jj_work(["rebase", "-s", M, "-d", "main@upstream"]);
    (lab, home, first)
}

#[test]
fn carries_reads_carried_rebased_where_the_release_holds_the_rewrites_lines() {
    // Given: M's rewrite changes M's line, and the release holds the
    // rewrite's line and not M's.
    let (lab, home, first) = rebased_member_lab("alpha = 1\n");
    lab.jj_work(["new", M]);
    write_file(&lab.work, "m.txt", "alpha = 2\n");
    lab.jj_work(["squash", "--into", M]);
    let rewrite = commit_at(&lab, M);
    let release = commit_at(&lab, RELEASE);

    // When: M's first commit is checked against the release.
    let output = carries(&lab, &home, first.as_str());

    // Then: carried-rebased, with the rewrite as the evidence.
    let out = stdout(&output);
    assert!(
        out.contains(&format!(
            "carried-rebased    {RELEASE} @ {}  (evidence {}: a rewrite of the change is an \
             ancestor)",
            release.short(),
            rewrite.short()
        )),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
}

#[test]
fn carries_reads_carried_rebased_where_a_neighbours_edit_sits_beside_the_rewrites_lines() {
    // Given: M adds a function after the first line of `lib.js`; N changes
    // the line right after that place; M is reworded, which makes a rewrite
    // with M's change id and tree; the release, cut outside knives, merges
    // the rewrite and N and holds both changes, and does not reach M's first
    // commit.
    let trunk = lines(&[IMPORT, VALUE_1, LOG]);
    let lab = trunk_with(&[(JS, trunk.as_str())]);
    lab.branch(
        M,
        JS,
        &lines(&[IMPORT, HELPER_OPEN, HELPER_RETURN, CLOSE, VALUE_1, LOG]),
    );
    lab.branch(N, JS, &lines(&[IMPORT, VALUE_2, LOG]));
    let first = commit_at(&lab, M);
    lab.jj_work(["describe", "-r", M, "-m", "feat/m, reworded"]);
    let rewrite = commit_at(&lab, M);
    assert_ne!(rewrite, first, "the reword rewrote M");
    let release = hand_release(&lab, RELEASE, &[M, N], |work| {
        let both = lines(&[IMPORT, HELPER_OPEN, HELPER_RETURN, CLOSE, VALUE_2, LOG]);
        write_file(work, JS, &both);
    });
    let (home, _consumer) = release_test_home(&lab);

    // When: M's first commit is checked against the release.
    let output = carries(&lab, &home, first.as_str());

    // Then: carried-rebased, with the rewrite as the evidence: the release
    // holds every line of the rewrite's change, as `--verify` reads it, and
    // N's edit beside them changes none of them.
    let out = stdout(&output);
    assert!(
        out.contains(&format!(
            "carried-rebased    {RELEASE} @ {}  (evidence {}: a rewrite of the change is an \
             ancestor)",
            release.short(),
            rewrite.short()
        )),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
}

#[test]
fn carries_reads_not_carried_where_the_release_reverts_a_rewrites_lines() {
    // Given: a rewrite of M, with M's change id, is an ancestor of the
    // release, and a later commit of the release reverts its line.
    let (lab, home, first) = rebased_member_lab("the member's line\n");
    lab.jj_work(["new", RELEASE, "-m", "revert the rewrite's line"]);
    write_file(&lab.work, "m.txt", "");
    lab.jj_work(["bookmark", "set", RELEASE, "-r", "@"]);
    lab.jj_work(["new"]);
    let release = commit_at(&lab, RELEASE);

    // When: M's first commit is checked against the release.
    let output = carries(&lab, &home, first.as_str());

    // Then: not carried, as its ancestry alone does not carry it: the
    // release's later commit emptied the rewrite's line, which a reading of
    // the rewrite's own lines finds lost, a finding.
    let out = stdout(&output);
    assert!(!out.contains("carried-"), "{}", text(&output));
    assert!(
        out.contains(&format!(
            "conflicted         {RELEASE} @ {}  (evidence {}: judge by eye)",
            release.short(),
            release.short()
        )),
        "{out}"
    );
    assert_eq!(output.status.code(), Some(FINDINGS), "{out}");
}
