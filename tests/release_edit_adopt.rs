//! What `knives release include`, `drop` and `advance` write in a file whose
//! conflict the release resolved (spec R11).
//!
//! An edit keeps the release's resolution of a conflict only where the plain
//! merge of the new parents leaves the same conflict there. Two other kinds
//! of file keep what the duplicate writes in them: a `CHANGELOG.md` or
//! `RELEASE_NOTES.md` whose conflict the release-log union joins, which is
//! left to the union, and a file whose every conflict hunk the release still
//! leaves open, with the sides it had. Everywhere else the edit writes that
//! plain merge, names the file where that changed it, and says whether the
//! file holds a conflict. The conflict is read from the plain merge of the
//! release in hand's own parents, so the rule holds whether or not an adopt
//! recorded the resolution. The shape is INFRA-4895's drop-after-adopt
//! probe: M adds a file of its own, D sets lines 5 and 6 of `f`, and A,
//! made after the first cut, sets line 5 as D does. Before anything is
//! written, the edit checks what it would write: where it would lose a
//! remaining member's line away from any conflict it leaves, it writes
//! nothing and exits 3, unless `--allow-loss` gives a reason. These tests
//! drive only the knives binary.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]
// allow: SIZE_OK: real-binary scenarios of one rule (K14) share one fixture and its helpers.

#[path = "common/lab.rs"]
mod lab;

use std::path::Path;
use std::process::{Command, Output};

use lab::{
    Lab, ReleaseOutput, Resolution, a_f, adopt, adopted_release, commit_at, conflicts_of, d_f,
    edit_by_hand, extend_branch, f_with, fast_forward, fixed_home, included_release, jj_in,
    knives_release, operation_ids, release_command, release_ok, release_parents, release_ran,
    release_test_home, text, trunk_with,
};

/// The dated release most tests cut and edit.
const RELEASE: &str = "release/2026-08-04";
/// The fixed release of the test with a carrier parent.
const FIXED: &str = "integration";
/// A member that changes no line of `f`.
const M: &str = "feat/m";
/// A member that sets lines 5 and 6 of `f`, as `d809` did in the probe.
const D: &str = "feat/d";
/// A member, made after the first cut, that sets line 5 of `f` as D does;
/// or, in the tests of a side's count, one that writes `f` as `a`; in the
/// tests of a release log, one that adds [`A_ENTRY`] to `CHANGELOG.md`; and
/// in the test of an adopted `--why` change, one in the first cut that adds
/// a line to a file `g`.
const A: &str = "feat/a";
/// A member that changes only a file of its own; or, in a test of a release
/// log, one that inserts its own block in `CHANGELOG.md`.
const Q: &str = "feat/q";
/// A member that sets line 30 of `f`, far from the conflict, or, in the
/// tests of a side's count, a cherry-pick of A.
const P: &str = "feat/p";
/// A member that sets line 5 of a second file `g` as A does; or, in the
/// test of an advance, one that writes a file of its own and then grows by
/// a commit that sets line 5 of `f`.
const B: &str = "feat/b";
/// A member that adds a section on top of `CHANGELOG.md`.
const X: &str = "feat/x";
/// Another member that adds a section on top of `CHANGELOG.md`.
const Y: &str = "feat/y";
/// A member that inserts a block at the end of `CHANGELOG.md`; or, in the
/// test of a merge branch, one that sets line 10 of `f`, which the branch
/// [`C`] merges and rewrites.
const E: &str = "feat/e";
/// A member that inserts another block at the end of `CHANGELOG.md`; or,
/// in the test of a merge branch, one that changes a file of its own, which
/// [`C`] merges too.
const F: &str = "feat/f";
/// A member, included after an adopt, that changes another line of a file.
const G: &str = "feat/g";
/// A branch that merges E and F and rewrites a line of E.
const C: &str = "feat/c";
/// A member that forks inside A, at A's first commit, once A is a member of
/// two commits (K14 35).
const N: &str = "feat/n";
/// A's entry in `CHANGELOG.md`, which D's block holds too.
const A_ENTRY: &str = "- a entry\n";
/// The line a person adds to `f` by hand, after its line 20: unchanged lines
/// lie between it and the conflict at lines 5 and 6.
const HAND_LINE: &str = "let e = hand ()\n";

/// `f` with [`HAND_LINE`] after its line 20.
fn with_hand_line(f: &str) -> String {
    f.replacen("let l20 = 20\n", &format!("let l20 = 20\n{HAND_LINE}"), 1)
}

/// A member branch forked from the fork's trunk that writes `files`.
fn member(lab: &Lab, name: &str, files: &[(&str, &str)]) {
    lab.jj_work(["new", "-r", "main@origin", "-m", name]);
    for (file, content) in files {
        std::fs::write(lab.work.join(file), content).expect("write a member's file");
    }
    lab.jj_work(["bookmark", "create", name, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// A release verb run in checkout `dir`, whatever its exit.
fn release_in(lab: &Lab, home: &tempfile::TempDir, dir: &Path, args: &[&str]) -> Output {
    release_command(lab, home, ReleaseOutput::Text, args)
        .current_dir(dir)
        .output()
        .expect("run knives release")
}

/// `file` at `revision` in checkout `dir`, a conflict written with jj's
/// snapshot markers.
fn show_in(dir: &Path, revision: &str, file: &str) -> String {
    let output = Command::new("jj")
        .args([
            "--ignore-working-copy",
            "--config",
            "ui.conflict-marker-style=snapshot",
            "file",
            "show",
            "-r",
            revision,
            &format!("root:{file}"),
        ])
        .current_dir(dir)
        .output()
        .expect("show a file");
    assert!(output.status.success(), "{}", text(&output));
    String::from_utf8(output.stdout).expect("utf-8 file content")
}

/// The release's `file` in the work checkout ([`show_in`]).
fn show(lab: &Lab, file: &str) -> String {
    show_in(&lab.work, RELEASE, file)
}

/// `file` in the plain merge of `parents`, as `jj new` of them holds it
/// ([`show_in`]). The merge is made in the work checkout, read, and left: it
/// is empty and has no description, so jj abandons it once the working copy
/// moves off it.
fn plain(lab: &Lab, parents: &[&str], file: &str) -> String {
    let mut args = vec!["new"];
    for parent in parents {
        args.extend(["-r", *parent]);
    }
    jj_in(&lab.work, &args);
    let merged = show_in(&lab.work, "@", file);
    lab.jj_work(["new", "main@origin"]);
    merged
}

/// The marker a line of a materialized conflict starts with: `<`, `>`, `+`
/// or `-` seven times or more, else nothing.
fn marker(line: &str) -> Option<char> {
    let first = line.chars().next()?;
    ("<>+-".contains(first) && line.chars().take(7).eq(std::iter::repeat_n(first, 7)))
        .then_some(first)
}

/// Each conflict hunk of `text`, read from jj's snapshot markers: its bases
/// and its sides, each sorted, after each side that equals a base cancels
/// with it, as jj-lib's `Merge::simplify` cancels them.
fn hunks(text: &str) -> Vec<(Vec<String>, Vec<String>)> {
    let mut found = Vec::new();
    let mut open: Option<(Vec<String>, Vec<String>)> = None;
    let mut term: Option<(char, String)> = None;
    for line in text.split_inclusive('\n') {
        let kind = marker(line);
        if open.is_none() {
            if kind == Some('<') {
                open = Some((Vec::new(), Vec::new()));
            }
            continue;
        }
        let Some(kind) = kind else {
            if let Some((_, content)) = term.as_mut() {
                content.push_str(line);
            }
            continue;
        };
        if let (Some((was, content)), Some((bases, sides))) = (term.take(), open.as_mut()) {
            if was == '+' {
                sides.push(content);
            } else {
                bases.push(content);
            }
        }
        if kind != '>' {
            term = Some((kind, String::new()));
            continue;
        }
        if let Some((mut bases, mut sides)) = open.take() {
            sides.retain(|side| {
                let cancelled = bases.iter().position(|base| base == side);
                if let Some(at) = cancelled {
                    bases.remove(at);
                }
                cancelled.is_none()
            });
            if !bases.is_empty() {
                bases.sort();
                sides.sort();
                found.push((bases, sides));
            }
        }
    }
    found
}

/// The line an edit prints for a file it wrote as the plain merge of its new
/// parents.
fn written_line(file: &str, conflicted: bool) -> String {
    format!(
        "demo: {file}: written as the plain merge of the new parents (the resolution answers \
         another conflict), {}",
        if conflicted {
            "which leaves a conflict"
        } else {
            "with no conflict"
        }
    )
}

/// The start of every [`written_line`] for `file`.
fn written_prefix(file: &str) -> String {
    format!("demo: {file}: written as the plain merge")
}

/// The line an edit prints when the plain merge it wrote leaves out a hand
/// change outside the resolved conflict.
fn not_carried_line(file: &str) -> String {
    format!("demo: {file}: the hand change outside the resolved conflict was not carried")
}

/// The conflict report's lines for `files`, left for a person.
fn left_for_a_person(files: &[&str]) -> String {
    let mut report = format!("  {} conflicted file(s) left for a person:\n", files.len());
    for file in files {
        report.push_str("    ");
        report.push_str(file);
        report.push('\n');
    }
    report
}

/// [`adopted_release`] to D's version, then [`HAND_LINE`] added to `f` by
/// hand and recorded with `adopt --why`. Returns the lab and its home.
fn adopted_with_hand_line() -> (Lab, tempfile::TempDir) {
    let (lab, home) = adopted_release(Resolution::DVersion);
    edit_by_hand(&lab, RELEASE, &[("f", &with_hand_line(&d_f()))]);
    adopt(&lab, &home, RELEASE, "a line by hand");
    (lab, home)
}

/// A release of M, A (writing `a` as its `f`) and D (writing `d`) merged by
/// hand, which no knives write recorded: a release knives could not check.
/// `f` is left as jj writes its conflict into the working copy, with jj's
/// markers in `style`, changed by `edit`, and then snapshotted. Returns the
/// lab and its home.
fn merged_by_hand(
    (a, d): (&str, &str),
    style: &str,
    edit: impl FnOnce(&str) -> String,
) -> (Lab, tempfile::TempDir) {
    octopus_by_hand(
        &[("f", &f_with(&[]))],
        &[(M, &[("m.txt", "m\n")]), (A, &[("f", a)]), (D, &[("f", d)])],
        ("f", style),
        edit,
    )
}

/// A release of `members`, each a branch from a trunk that holds `trunk`
/// and writing its own files, merged by hand with `jj new`, which no knives
/// write recorded: a release knives could not check. `file` is left as jj
/// writes its conflict into the working copy, with jj's markers in `style`,
/// changed by `edit`, and then snapshotted. Returns the lab and its home.
fn octopus_by_hand(
    trunk: &[(&str, &str)],
    members: &[(&str, &[(&str, &str)])],
    (file, style): (&str, &str),
    edit: impl FnOnce(&str) -> String,
) -> (Lab, tempfile::TempDir) {
    let lab = trunk_with(trunk);
    for (name, files) in members {
        member(&lab, name, files);
    }
    let (home, _consumer) = release_test_home(&lab);
    let config = format!("ui.conflict-marker-style={style}");
    let mut args = vec!["--config", config.as_str(), "new"];
    for (name, _) in members {
        args.extend(["-r", *name]);
    }
    args.extend(["-m", RELEASE]);
    jj_in(&lab.work, &args);
    let path = lab.work.join(file);
    let conflicted = std::fs::read_to_string(&path).expect("read the conflicted file");
    std::fs::write(&path, edit(&conflicted)).expect("write the file by hand");
    lab.jj_work([
        "--config",
        config.as_str(),
        "bookmark",
        "create",
        RELEASE,
        "-r",
        "@",
    ]);
    lab.jj_work(["new", "main@origin"]);
    (lab, home)
}

/// A's `f` with a conflict of its own at line 20 too.
fn a_f_two_hunks() -> String {
    f_with(&[(5, "compare_fix ()"), (20, "a_twenty ()")])
}

/// D's `f` with a conflict of its own at line 20 too.
fn d_f_two_hunks() -> String {
    f_with(&[
        (5, "compare_fix ()"),
        (6, "d809_only ()"),
        (20, "d_twenty ()"),
    ])
}

/// `conflicted` with its first conflict, at lines 5 and 6, resolved by hand
/// to D's text, which holds A's line; a later conflict left open.
fn first_hunk_to_d(conflicted: &str) -> String {
    first_conflict_resolved(
        conflicted,
        "let l5 = compare_fix ()\nlet l6 = d809_only ()\n",
    )
}

/// `conflicted` with its first conflict, its markers and the text between
/// them, replaced by `resolution`; a later conflict left open.
fn first_conflict_resolved(conflicted: &str, resolution: &str) -> String {
    let mut resolved = String::new();
    let mut inside = None;
    for line in conflicted.split_inclusive('\n') {
        match (inside, marker(line)) {
            (None, Some('<')) => inside = Some(true),
            (Some(true), Some('>')) => {
                resolved.push_str(resolution);
                inside = Some(false);
            }
            (Some(true), _) => {}
            _ => resolved.push_str(line),
        }
    }
    resolved
}

/// [`merged_by_hand`] with two conflicts in `f`, at lines 5 and 6 (h1) and
/// at line 20 (h2): h1 resolved by hand to D's text, h2 left open.
fn resolved_in_part() -> (Lab, tempfile::TempDir) {
    merged_by_hand(
        (&a_f_two_hunks(), &d_f_two_hunks()),
        "diff",
        first_hunk_to_d,
    )
}

#[test]
fn a_drop_after_an_adopt_writes_the_plain_merge_where_the_conflict_is_gone() {
    // Given (K14 1): `f` resolved by hand to D's version and adopted.
    let (lab, home) = adopted_release(Resolution::DVersion);

    // When: D is dropped.
    let dropped = release_ok(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: `f` is the plain merge of M and A: A's line is there, D's own
    // line is not, and the output names `f` as written so.
    let expected = plain(&lab, &[M, A], "f");
    assert_eq!(expected, a_f());
    assert_eq!(show(&lab, "f"), expected, "{dropped}");
    assert!(dropped.contains(&written_line("f", false)), "{dropped}");
}

#[test]
fn an_advance_to_the_base_after_an_adopt_writes_the_plain_merge() {
    // Given (K14 2): `f` resolved to D's version and adopted, then a commit
    // D2 on D's branch that puts `f` back to the base.
    let (lab, home) = adopted_release(Resolution::DVersion);
    extend_branch(&lab, D, "f", &f_with(&[]));

    // When: D is advanced to D2.
    let advanced = release_ok(&lab, &home, &["advance", D]);

    // Then: `f` is the plain merge of M, D2 and A, which holds A's line.
    let expected = plain(&lab, &[M, D, A], "f");
    assert_eq!(expected, a_f());
    assert_eq!(show(&lab, "f"), expected, "{advanced}");
    assert!(advanced.contains(&written_line("f", false)), "{advanced}");
}

#[test]
fn a_drop_of_the_other_side_after_an_adopt_writes_the_clean_plain_merge() {
    // Given (K14 3): `f` resolved to D's version and adopted.
    let (lab, home) = adopted_release(Resolution::DVersion);

    // When: A, whose line D also holds, is dropped.
    let dropped = release_ok(&lab, &home, &["drop", A, "--why", "not ready"]);

    // Then: `f` is the plain merge of M and D, with no conflict.
    let expected = plain(&lab, &[M, D], "f");
    assert_eq!(expected, d_f());
    assert!(conflicts_of(&lab, RELEASE).is_empty(), "{dropped}");
    assert_eq!(show(&lab, "f"), expected, "{dropped}");
    assert!(dropped.contains(&written_line("f", false)), "{dropped}");
}

#[test]
fn an_advance_that_changes_the_conflict_after_an_adopt_leaves_its_conflict() {
    // Given (K14 4): `f` resolved to D's version and adopted, then a commit
    // D3 on D's branch that sets A's line to another text.
    let (lab, home) = adopted_release(Resolution::DVersion);
    extend_branch(
        &lab,
        D,
        "f",
        &f_with(&[(5, "other_fix ()"), (6, "d809_only ()")]),
    );

    // When: D is advanced to D3.
    let advanced = release_ok(&lab, &home, &["advance", D]);

    // Then: `f` holds the conflict that the plain merge of M, D3 and A
    // leaves, and the output reports it.
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"], "{advanced}");
    let expected = hunks(&plain(&lab, &[M, D, A], "f"));
    assert!(!expected.is_empty(), "the plain merge leaves no conflict");
    assert_eq!(hunks(&show(&lab, "f")), expected, "{advanced}");
    assert!(advanced.contains(&left_for_a_person(&["f"])), "{advanced}");
    assert!(advanced.contains(&written_line("f", true)), "{advanced}");
}

#[test]
fn a_drop_with_no_adopt_writes_the_remaining_side() {
    // Given (K14 5, a control): the include left `f` conflicted, with no hand
    // resolution and no adopt.
    let (lab, home, include) = included_release(&d_f());
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"], "{include}");

    // When: D is dropped.
    let dropped = release_ok(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: `f` is A's version.
    assert_eq!(show(&lab, "f"), a_f(), "{dropped}");

    // Given: D's change apart from A's, so the include has no conflict.
    let (lab, home, include) = included_release(&f_with(&[(20, "d809_only ()")]));
    assert!(conflicts_of(&lab, RELEASE).is_empty(), "{include}");

    // When: D is dropped.
    let dropped = release_ok(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: `f` is A's version too.
    assert_eq!(show(&lab, "f"), a_f(), "{dropped}");
}

#[test]
fn an_include_beside_the_conflict_keeps_the_resolution() {
    // Given (K14 6, a control): `f` resolved to D's version and adopted, and
    // P, which changes line 30 of `f`, with unchanged lines between it and
    // the conflict.
    let (lab, home) = adopted_release(Resolution::DVersion);
    member(&lab, P, &[("f", &f_with(&[(30, "extra ()")]))]);

    // When: P is included.
    let included = release_ok(&lab, &home, &["include", P]);

    // Then: `f` keeps the resolution with P's line.
    assert!(conflicts_of(&lab, RELEASE).is_empty(), "{included}");
    assert_eq!(
        show(&lab, "f"),
        f_with(&[(5, "compare_fix ()"), (6, "d809_only ()"), (30, "extra ()")]),
        "{included}"
    );

    // When: A is advanced to a commit that also changes line 10.
    extend_branch(
        &lab,
        A,
        "f",
        &f_with(&[(5, "compare_fix ()"), (10, "compare_more ()")]),
    );
    let advanced = release_ok(&lab, &home, &["advance", A]);

    // Then: `f` keeps the resolution with that line too.
    assert!(conflicts_of(&lab, RELEASE).is_empty(), "{advanced}");
    assert_eq!(
        show(&lab, "f"),
        f_with(&[
            (5, "compare_fix ()"),
            (6, "d809_only ()"),
            (10, "compare_more ()"),
            (30, "extra ()"),
        ]),
        "{advanced}"
    );
}

#[test]
fn a_drop_of_an_unrelated_member_keeps_the_resolution() {
    // Given (K14 7, a control): `f` resolved to D's version and adopted.
    let (lab, home) = adopted_release(Resolution::DVersion);

    // When: M is dropped.
    let dropped = release_ok(&lab, &home, &["drop", M, "--why", "not ready"]);

    // Then: `f` keeps D's version, with A's line.
    assert!(conflicts_of(&lab, RELEASE).is_empty(), "{dropped}");
    assert_eq!(show(&lab, "f"), d_f(), "{dropped}");

    // When: D is advanced to a commit that only adds a file.
    extend_branch(&lab, D, "d.txt", "d\n");
    let advanced = release_ok(&lab, &home, &["advance", D]);

    // Then: `f` keeps D's version.
    assert!(conflicts_of(&lab, RELEASE).is_empty(), "{advanced}");
    assert_eq!(show(&lab, "f"), d_f(), "{advanced}");
}

#[test]
fn a_rebase_onto_an_unrelated_trunk_keeps_the_resolution() {
    // Given (K14 8, a control): `f` resolved to D's version and adopted, and
    // a trunk commit that changes a file no member changes.
    let (lab, home) = adopted_release(Resolution::DVersion);
    lab.advance_upstream("advance\n");

    // When: the release is rebased onto it.
    let rebased = release_ok(&lab, &home, &["rebase", "main@upstream"]);

    // Then: `f` keeps the resolution.
    assert!(conflicts_of(&lab, RELEASE).is_empty(), "{rebased}");
    assert_eq!(show(&lab, "f"), d_f(), "{rebased}");
}

#[test]
fn a_resolution_that_keeps_neither_side_is_kept() {
    // Given (K14 9, a control): `f` resolved to a text that keeps neither
    // side, adopted, and Q, which changes another file.
    let (lab, home) = adopted_release(Resolution::Neither);
    member(&lab, Q, &[("q.txt", "q\n")]);

    // When: Q is included.
    let included = release_ok(&lab, &home, &["include", Q]);

    // Then: `f` keeps that text, with no conflict.
    assert!(conflicts_of(&lab, RELEASE).is_empty(), "{included}");
    assert_eq!(show(&lab, "f"), "merged\n", "{included}");
}

#[test]
fn a_drop_and_an_include_of_the_same_member_leave_the_returning_conflict() {
    // Given (K14 10): `f` resolved to D's version and adopted.
    let (lab, home) = adopted_release(Resolution::DVersion);

    // When: D is dropped.
    let dropped = release_ok(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: `f` is A's version, as K14 (1) writes it.
    assert_eq!(show(&lab, "f"), a_f(), "{dropped}");

    // When: D is included again.
    let included = release_ok(&lab, &home, &["include", D]);

    // Then: `f` holds the conflict that the plain merge of M, A and D leaves,
    // with A's and D's sides, and the output reports it: no resolution the
    // release no longer holds is written.
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"], "{included}");
    let expected = hunks(&plain(&lab, &[M, A, D], "f"));
    assert!(!expected.is_empty(), "the plain merge leaves no conflict");
    assert_eq!(hunks(&show(&lab, "f")), expected, "{included}");
    assert!(included.contains(&left_for_a_person(&["f"])), "{included}");
}

#[test]
fn a_hand_change_adopted_after_a_drop_is_carried_and_kept_in_the_returning_conflict() {
    // Given (K14 11): `f` resolved to D's version and adopted; D dropped;
    // then a line added to `f` by hand and adopted with `--why`.
    let (lab, home) = adopted_release(Resolution::DVersion);
    release_ok(&lab, &home, &["drop", D, "--why", "not ready"]);
    let after_drop = show(&lab, "f");
    edit_by_hand(&lab, RELEASE, &[("f", &with_hand_line(&after_drop))]);
    adopt(&lab, &home, RELEASE, "a line by hand");
    member(&lab, Q, &[("q.txt", "q\n")]);

    // When: Q, which changes another file, is included.
    let included = release_ok(&lab, &home, &["include", Q]);

    // Then: `f` is A's version with the hand line.
    assert_eq!(show(&lab, "f"), with_hand_line(&a_f()), "{included}");

    // When: D is included again.
    let returned = release_ok(&lab, &home, &["include", D]);

    // Then: `f` holds the returning conflict with the hand line in it, and
    // the output reports it.
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"], "{returned}");
    assert!(show(&lab, "f").contains(HAND_LINE), "{returned}");
    assert!(returned.contains(&left_for_a_person(&["f"])), "{returned}");
}

#[test]
fn a_second_checkout_after_a_pushed_rebase_reads_the_rule_from_its_release_in_hand() {
    // Given (K14 12): `f` resolved to D's version and adopted; the release
    // rebased onto an unrelated trunk commit, and only then the members and
    // the release pushed; a second checkout on the same ledger that fetched
    // them, so it holds none of the adopted write's parents.
    let (lab, home) = adopted_release(Resolution::DVersion);
    let adopted_parents = release_parents(&lab, RELEASE);
    lab.advance_upstream("advance\n");
    release_ok(&lab, &home, &["rebase", "main@upstream"]);
    for branch in [M, D, A, RELEASE] {
        lab.push_branch(branch);
    }
    let second = &lab.second;
    let upstream = lab.upstream.display().to_string();
    lab.jj_at(
        second,
        ["git", "remote", "add", "upstream", upstream.as_str()],
    );
    lab.jj_at(second, ["git", "fetch", "--all-remotes"]);
    for branch in [M, D, A, RELEASE] {
        let remote = format!("{branch}@origin");
        lab.jj_at(second, ["bookmark", "track", remote.as_str()]);
    }
    let opened = knives::jj::Repo::open(second).expect("open the second checkout");
    for parent in &adopted_parents {
        assert!(
            opened.resolve_commit(parent.as_str()).is_err(),
            "the second checkout holds {parent}"
        );
    }

    // When: D is dropped in the second checkout.
    let output = release_in(&lab, &home, second, &["drop", D, "--why", "not ready"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));

    // Then: `f` is A's version, as K14 (1) writes it.
    assert_eq!(show_in(second, RELEASE, "f"), a_f(), "{}", text(&output));
}

#[test]
fn a_hand_change_beside_a_resolution_is_named_as_not_carried_when_d_is_dropped() {
    // Given (K14 13): `f` resolved to D's version and adopted, then a line
    // added by hand outside the conflict and adopted with `--why`.
    let (lab, home) = adopted_with_hand_line();

    // When: D is dropped.
    let dropped = release_ok(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: `f` is the plain merge of M and A, never the base with the hand
    // line, and the output names `f` and says the hand change was not
    // carried.
    assert_eq!(show(&lab, "f"), plain(&lab, &[M, A], "f"), "{dropped}");
    assert!(dropped.contains(&written_line("f", false)), "{dropped}");
    assert!(dropped.contains(&not_carried_line("f")), "{dropped}");

    // Given: the same adopts, then a commit D2 on D's branch that puts `f`
    // back to the base.
    let (lab, home) = adopted_with_hand_line();
    extend_branch(&lab, D, "f", &f_with(&[]));

    // When: D is advanced to D2.
    let advanced = release_ok(&lab, &home, &["advance", D]);

    // Then: the same: the plain merge of M, D2 and A, and both lines.
    assert_eq!(show(&lab, "f"), plain(&lab, &[M, D, A], "f"), "{advanced}");
    assert!(advanced.contains(&written_line("f", false)), "{advanced}");
    assert!(advanced.contains(&not_carried_line("f")), "{advanced}");
}

#[test]
fn a_hand_resolution_no_adopt_recorded_is_not_dropped_in_silence() {
    // Given (K14 14): a release of M, A and D merged by hand, which the
    // ledger records no write of, `f` resolved by hand to D's version.
    let (lab, home) = merged_by_hand((&a_f(), &d_f()), "diff", |_| d_f());

    // When: D is dropped.
    let dropped = release_ok(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: `f` is the plain merge of M and A, and the output names it.
    assert_eq!(show(&lab, "f"), plain(&lab, &[M, A], "f"), "{dropped}");
    assert!(dropped.contains(&written_line("f", false)), "{dropped}");
}

#[test]
fn an_unresolved_conflict_in_the_release_is_written_as_before_and_names_nothing() {
    // Given (K14 15, a control): a release of M, A and D merged by hand, `f`
    // left conflicted with a line added outside the conflict.
    let (lab, home) = merged_by_hand((&a_f(), &d_f()), "diff", with_hand_line);
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"]);

    // When: A is dropped.
    let dropped = release_ok(&lab, &home, &["drop", A, "--why", "not ready"]);

    // Then: `f` is D's version with the hand line, and nothing names `f`.
    assert_eq!(show(&lab, "f"), with_hand_line(&d_f()), "{dropped}");
    assert!(!dropped.contains(&written_prefix("f")), "{dropped}");
    assert!(!dropped.contains(&not_carried_line("f")), "{dropped}");
}

#[test]
fn a_partly_resolved_file_follows_the_rule() {
    // Given (K14 18): a release of M, A and D merged by hand, `f` with two
    // conflicts: h1 resolved by hand to D's text, h2 left open.
    let (lab, home) = resolved_in_part();
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"]);

    // When: D is dropped.
    let dropped = release_ok(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: `f` is the plain merge of M and A, and the output names it.
    let expected = plain(&lab, &[M, A], "f");
    assert_eq!(expected, a_f_two_hunks());
    assert_eq!(show(&lab, "f"), expected, "{dropped}");
    assert!(dropped.contains(&written_line("f", false)), "{dropped}");
}

#[test]
fn a_file_resolved_in_part_keeps_its_resolution_where_its_hunks_stay() {
    // Given (K14 22, a control): `f` with h1 resolved by hand to D's text and
    // h2 open, and Q, which changes another file.
    let (lab, home) = resolved_in_part();
    let before = hunks(&show(&lab, "f"));
    assert_eq!(before.len(), 1, "{before:?}");
    member(&lab, Q, &[("q.txt", "q\n")]);

    // When: Q is included.
    let included = release_ok(&lab, &home, &["include", Q]);

    // Then: h1 keeps D's text and h2 stays open, and nothing names `f`.
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"], "{included}");
    let after = show(&lab, "f");
    assert!(
        after.contains("let l4 = 4\nlet l5 = compare_fix ()\nlet l6 = d809_only ()\nlet l7 = 7\n"),
        "{after}"
    );
    assert_eq!(hunks(&after), before, "{included}");
    assert!(!included.contains(&written_prefix("f")), "{included}");
}

#[test]
fn a_conflicted_hunk_whose_sides_the_release_changed_counts_as_answered() {
    // Given: a release of M, A and D merged by hand, its conflict in `f`
    // left open but A's side changed by hand inside the markers.
    let (lab, home) = merged_by_hand((&a_f(), &d_f()), "snapshot", |conflicted| {
        conflicted.replacen(
            "let l5 = compare_fix ()\nlet l6 = 6\n",
            "let l5 = compare_fix (2)\nlet l6 = 6\n",
            1,
        )
    });
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"]);
    assert!(show(&lab, "f").contains("compare_fix (2)"));

    // When: D is dropped.
    let dropped = release_ok(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: the changed hunk is a resolution the plain merge of M and A no
    // longer answers to: `f` is that merge, and the output names it.
    assert_eq!(show(&lab, "f"), plain(&lab, &[M, A], "f"), "{dropped}");
    assert!(dropped.contains(&written_line("f", false)), "{dropped}");
}

/// A release of M, A, P and D cut together, with `f` x on the trunk, a for
/// A, a again for P (a cherry-pick of A) and d for D: `f` conflicts, and is
/// resolved by hand to a and adopted. Returns the lab and its home.
fn cherry_pick_resolved() -> (Lab, tempfile::TempDir) {
    let lab = trunk_with(&[("f", "x\n")]);
    member(&lab, M, &[("m.txt", "m\n")]);
    member(&lab, A, &[("f", "a\n")]);
    member(&lab, P, &[("f", "a\n")]);
    member(&lab, D, &[("f", "d\n")]);
    let (home, _consumer) = release_test_home(&lab);
    let cut = release_ran(&lab, &home, &["cut", RELEASE]);
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"], "{cut}");
    edit_by_hand(&lab, RELEASE, &[("f", "a\n")]);
    adopt(&lab, &home, RELEASE, "resolve f to a");
    (lab, home)
}

#[test]
fn a_dropped_cherry_pick_changes_a_sides_count_and_the_plain_merge_is_written() {
    // Given (K14 21): `f` resolved by hand to a, which A and its
    // cherry-pick P both write.
    let (lab, home) = cherry_pick_resolved();

    // When: the cherry-pick is dropped.
    let dropped = release_ok(&lab, &home, &["drop", P, "--why", "a duplicate"]);

    // Then: a's side counts once now, so `f` holds the conflict of the plain
    // merge of M, A and D, never x, and the output reports it.
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"], "{dropped}");
    let expected = hunks(&plain(&lab, &[M, A, D], "f"));
    assert!(!expected.is_empty(), "the plain merge leaves no conflict");
    assert_eq!(hunks(&show(&lab, "f")), expected, "{dropped}");
    assert!(dropped.contains(&left_for_a_person(&["f"])), "{dropped}");
    assert!(dropped.contains(&written_line("f", true)), "{dropped}");
}

#[test]
fn an_equal_repeat_of_a_side_keeps_the_resolution() {
    // Given: `f` resolved by hand to a, which A and its cherry-pick P both
    // write, and Q, which changes another file.
    let (lab, home) = cherry_pick_resolved();
    member(&lab, Q, &[("q.txt", "q\n")]);

    // When: Q is included, which keeps A and P.
    let included = release_ok(&lab, &home, &["include", Q]);

    // Then: `f` keeps the resolution, and nothing names it.
    assert!(conflicts_of(&lab, RELEASE).is_empty(), "{included}");
    assert_eq!(show(&lab, "f"), "a\n", "{included}");
    assert!(!included.contains(&written_prefix("f")), "{included}");
}

#[test]
fn files_two_adopts_resolved_both_follow_the_rule() {
    // Given: D changes lines 5 and 6 of `f` and of `g`; A changes line 5 of
    // `f`, and B line 5 of `g`; an adopt resolved `f` to D's version after
    // `include A`, and a later one `g` after `include B`.
    let lab = trunk_with(&[("f", &f_with(&[])), ("g", &f_with(&[]))]);
    member(&lab, M, &[("m.txt", "m\n")]);
    member(&lab, D, &[("f", &d_f()), ("g", &d_f())]);
    let (home, _consumer) = release_test_home(&lab);
    release_ran(&lab, &home, &["cut", RELEASE]);
    member(&lab, A, &[("f", &a_f())]);
    release_ok(&lab, &home, &["include", A]);
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"]);
    lab.jj_work(["restore", "--from", D, "--into", RELEASE, "f"]);
    adopt(&lab, &home, RELEASE, "resolve f");
    member(&lab, B, &[("g", &a_f())]);
    release_ok(&lab, &home, &["include", B]);
    assert_eq!(conflicts_of(&lab, RELEASE), ["g"]);
    lab.jj_work(["restore", "--from", D, "--into", RELEASE, "g"]);
    adopt(&lab, &home, RELEASE, "resolve g");

    // When: D, whose version both resolutions took, is dropped.
    let dropped = release_ok(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: each file is its plain merge of M, A and B, and both are named.
    for file in ["f", "g"] {
        assert_eq!(show(&lab, file), plain(&lab, &[M, A, B], file), "{dropped}");
        assert!(dropped.contains(&written_line(file, false)), "{dropped}");
    }
    assert_eq!(show(&lab, "f"), a_f());
}

#[test]
fn a_file_changed_beyond_the_conflicts_is_written_as_before() {
    // Given: `f` resolved to D's version and adopted, then M's own file,
    // which the release's parents merge cleanly, changed by hand and adopted
    // with `--why`.
    let (lab, home) = adopted_release(Resolution::DVersion);
    edit_by_hand(&lab, RELEASE, &[("m.txt", "m, repaired\n")]);
    adopt(&lab, &home, RELEASE, "repair m.txt");

    // When: D is dropped.
    let dropped = release_ok(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: the repair is carried, as the duplicate writes it, and nothing
    // names that file; `f` follows the rule.
    assert_eq!(show(&lab, "m.txt"), "m, repaired\n", "{dropped}");
    assert!(!dropped.contains(&written_prefix("m.txt")), "{dropped}");
    assert_eq!(show(&lab, "f"), a_f(), "{dropped}");
}

#[test]
fn a_three_parent_conflict_matches_after_its_equal_sides_cancel() {
    // Given: a first cut of M, D and P, which changes line 30 of `f`; then
    // `include A` conflicts in `f`, a merge of three members that change
    // `f`, where P's side equals the base inside the conflict; `f` resolved
    // by hand to D's lines with P's, and adopted.
    let lab = trunk_with(&[("f", &f_with(&[]))]);
    member(&lab, M, &[("m.txt", "m\n")]);
    member(&lab, D, &[("f", &d_f())]);
    member(&lab, P, &[("f", &f_with(&[(30, "extra ()")]))]);
    let (home, _consumer) = release_test_home(&lab);
    release_ran(&lab, &home, &["cut", RELEASE]);
    member(&lab, A, &[("f", &a_f())]);
    release_ok(&lab, &home, &["include", A]);
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"]);
    let resolved = f_with(&[(5, "compare_fix ()"), (6, "d809_only ()"), (30, "extra ()")]);
    edit_by_hand(&lab, RELEASE, &[("f", &resolved)]);
    adopt(&lab, &home, RELEASE, "resolve f");
    member(&lab, Q, &[("q.txt", "q\n")]);

    // When: Q, which changes another file, is included.
    let included = release_ok(&lab, &home, &["include", Q]);

    // Then: the same conflict comes back, so `f` keeps the resolution, and
    // nothing names it.
    assert!(conflicts_of(&lab, RELEASE).is_empty(), "{included}");
    assert_eq!(show(&lab, "f"), resolved, "{included}");
    assert!(!included.contains(&written_prefix("f")), "{included}");
}

#[test]
fn a_release_with_a_carrier_parent_compares_without_it() {
    // Given: a fixed release of M and D cut and pushed; A included, `f`
    // resolved to D's version and adopted; then the release made a
    // fast-forward of its pushed cut, as the fork recipe does (the cut
    // becomes a parent, a carrier), and pushed; and Q, which changes another
    // file.
    let lab = trunk_with(&[("f", &f_with(&[]))]);
    member(&lab, M, &[("m.txt", "m\n")]);
    member(&lab, D, &[("f", &d_f())]);
    let home = fixed_home(&lab, FIXED);
    release_ran(&lab, &home, &["cut"]);
    let carrier = commit_at(&lab, FIXED);
    lab.push_branch(FIXED);
    member(&lab, A, &[("f", &a_f())]);
    release_ran(&lab, &home, &["include", A]);
    assert_eq!(conflicts_of(&lab, FIXED), ["f"]);
    lab.jj_work(["restore", "--from", D, "--into", FIXED, "f"]);
    adopt(&lab, &home, FIXED, "resolve f");
    fast_forward(&lab, FIXED);
    lab.push_branch(FIXED);
    assert!(release_parents(&lab, FIXED).contains(&carrier));
    member(&lab, Q, &[("q.txt", "q\n")]);

    // When: Q is included.
    let included = release_ran(&lab, &home, &["include", Q]);

    // Then: the conflict is read from the members, the carrier left out, so
    // it comes back and `f` keeps the resolution; nothing names `f`.
    assert!(
        !release_parents(&lab, FIXED).contains(&carrier),
        "{included}"
    );
    assert!(conflicts_of(&lab, FIXED).is_empty(), "{included}");
    assert_eq!(show_in(&lab.work, FIXED, "f"), d_f(), "{included}");
    assert!(!included.contains(&written_prefix("f")), "{included}");
}

#[test]
fn a_hand_change_beside_a_resolution_is_kept_by_an_include_of_another_member() {
    // Given: `f` resolved to D's version and adopted, a line added by hand
    // outside the conflict and adopted, and Q, which changes another file.
    let (lab, home) = adopted_with_hand_line();
    member(&lab, Q, &[("q.txt", "q\n")]);

    // When: Q is included.
    let included = release_ok(&lab, &home, &["include", Q]);

    // Then: the same conflict comes back, so `f` keeps the resolution and
    // the hand line, and nothing names `f`.
    assert_eq!(show(&lab, "f"), with_hand_line(&d_f()), "{included}");
    assert!(!included.contains(&written_prefix("f")), "{included}");
    assert!(!included.contains(&not_carried_line("f")), "{included}");
}

#[test]
fn a_whole_file_resolution_conflicts_with_another_change_to_the_file() {
    // Given (the probe's case 2b): `f` resolved to a text that keeps neither
    // side, adopted, and P, which changes line 30 of `f`.
    let (lab, home) = adopted_release(Resolution::Neither);
    member(&lab, P, &[("f", &f_with(&[(30, "extra ()")]))]);

    // When: P is included.
    let included = release_ok(&lab, &home, &["include", P]);

    // Then: the resolution rewrote the whole file, so P's change conflicts
    // with it, and the output reports the conflict; the rule names nothing.
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"], "{included}");
    assert!(included.contains(&left_for_a_person(&["f"])), "{included}");
    assert!(!included.contains(&written_prefix("f")), "{included}");
}

/// A changelog with `top` above its one published release. It has no blank
/// line, so each line a diff matches is unique and the merges below do not
/// depend on how a diff pairs blank lines.
fn log_with(top: &str) -> String {
    format!("# Changelog\n{top}## [1.0.0]\n- old 1\n- old 2\n")
}

#[test]
fn a_hand_resolved_changelog_the_union_declined_follows_the_rule() {
    // Given (K14 17): A's CHANGELOG.md adds an entry on top; D's holds A's
    // entry and its own, and changes the heading below them, so the union
    // declines the conflict `include A` leaves; a person resolves it to D's
    // version and adopts it.
    let lab = trunk_with(&[("CHANGELOG.md", &log_with(""))]);
    member(&lab, M, &[("m.txt", "m\n")]);
    let forked_log =
        "# Changelog\n## [Unreleased]\n- a entry\n- d entry\n## [1.0.0] - fork\n- old 1\n- old 2\n";
    member(&lab, D, &[("CHANGELOG.md", forked_log)]);
    let (home, _consumer) = release_test_home(&lab);
    release_ran(&lab, &home, &["cut", RELEASE]);
    let entry_log = log_with("## [Unreleased]\n- a entry\n");
    member(&lab, A, &[("CHANGELOG.md", &entry_log)]);
    let include = release_ok(&lab, &home, &["include", A]);
    assert_eq!(conflicts_of(&lab, RELEASE), ["CHANGELOG.md"], "{include}");
    lab.jj_work(["restore", "--from", D, "--into", RELEASE, "CHANGELOG.md"]);
    adopt(&lab, &home, RELEASE, "resolve the changelog");

    // When: D is dropped.
    let dropped = release_ok(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: CHANGELOG.md is the plain merge of M and A, with A's entry, and
    // the output names it.
    let expected = plain(&lab, &[M, A], "CHANGELOG.md");
    assert_eq!(expected, entry_log);
    assert_eq!(show(&lab, "CHANGELOG.md"), expected, "{dropped}");
    assert!(
        dropped.contains(&written_line("CHANGELOG.md", false)),
        "{dropped}"
    );
}

#[test]
fn a_release_log_edited_by_hand_beyond_the_union_is_written_as_before() {
    // Given: X and Y each add a section on top of CHANGELOG.md, which the
    // first cut writes as their union; then a line added by hand below every
    // section a member added, and adopted with `--why`.
    let lab = trunk_with(&[("CHANGELOG.md", &log_with(""))]);
    member(
        &lab,
        X,
        &[("CHANGELOG.md", &log_with("## [1.1.0-x]\n- x entry\n"))],
    );
    member(
        &lab,
        Y,
        &[("CHANGELOG.md", &log_with("## [1.1.0-y]\n- y entry\n"))],
    );
    let (home, _consumer) = release_test_home(&lab);
    let cut = release_ran(&lab, &home, &["cut", RELEASE]);
    assert!(conflicts_of(&lab, RELEASE).is_empty(), "{cut}");
    let unioned = show(&lab, "CHANGELOG.md");
    edit_by_hand(
        &lab,
        RELEASE,
        &[("CHANGELOG.md", &format!("{unioned}- hand note\n"))],
    );
    adopt(&lab, &home, RELEASE, "a note by hand");

    // When: X, whose section the union holds, is dropped.
    let dropped = release_ok(&lab, &home, &["drop", X, "--why", "not ready"]);

    // Then: the log is written as before: taking X's section out of the
    // union is no insertion, so it is left conflicted for a person, still
    // holding the hand line; the rule names nothing.
    assert_eq!(conflicts_of(&lab, RELEASE), ["CHANGELOG.md"], "{dropped}");
    assert!(
        show(&lab, "CHANGELOG.md").contains("- hand note\n"),
        "{dropped}"
    );
    assert!(
        dropped.contains(&left_for_a_person(&["CHANGELOG.md"])),
        "{dropped}"
    );
    assert!(
        !dropped.contains(&written_prefix("CHANGELOG.md")),
        "{dropped}"
    );
}

#[test]
fn a_second_edit_after_a_plain_merge_write_names_the_file_no_more() {
    // Given: `f` resolved to D's version and adopted, then D dropped, which
    // writes `f` as the plain merge of M and A (K14 1); and Q, which changes
    // another file.
    let (lab, home) = adopted_release(Resolution::DVersion);
    let dropped = release_ok(&lab, &home, &["drop", D, "--why", "not ready"]);
    assert!(dropped.contains(&written_line("f", false)), "{dropped}");
    member(&lab, Q, &[("q.txt", "q\n")]);

    // When: Q is included.
    let included = release_ok(&lab, &home, &["include", Q]);

    // Then: `f` stays as the drop wrote it, and nothing names it.
    assert_eq!(show(&lab, "f"), a_f(), "{included}");
    assert!(!included.contains(&written_prefix("f")), "{included}");
}

/// A's changelog: its entry on top.
fn a_log() -> String {
    log_with(A_ENTRY)
}

/// D's changelog: its own entry, then A's, inserted where A inserts its own.
fn d_log() -> String {
    log_with(&format!("- d entry\n{A_ENTRY}"))
}

/// The line that opens an edit's refusal for `member`'s lines in `file`.
fn refusal_line(file: &str, member: &str) -> String {
    format!(
        "demo: refusing to write {RELEASE}: {file} would lose {member}'s lines; nothing written\n"
    )
}

/// The report's line for an added line at `line` of a member's tip that an
/// edit would lose in `file`.
fn added_lost(file: &str, line: u32, text: &str) -> String {
    format!("      {file}:{line}: added line lost ‹{text}›\n")
}

/// The remedy that ends an edit's refusal.
fn remedy_line() -> String {
    format!(
        "demo: run the edit again with `--allow-loss <reason>`, put the named lines back by \
         hand, and record that with `knives release adopt {RELEASE} --why <reason>`\n"
    )
}

/// The fork's ledger entries.
fn ledger_entries(home: &tempfile::TempDir) -> Vec<knives::ledger::Entry> {
    knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .read()
        .expect("read the ledger")
        .entries
}

/// The newest event the ledger records about the release.
fn newest_event(home: &tempfile::TempDir) -> knives::ledger::Entry {
    ledger_entries(home)
        .into_iter()
        .rev()
        .find(|entry| {
            entry.kind == knives::ledger::Kind::Event && entry.subject.as_deref() == Some(RELEASE)
        })
        .expect("an event about the release")
}

/// What a refused edit must leave as it was: the operation log, the
/// release's commit and the ledger.
struct Untouched {
    operations: Vec<String>,
    commit: knives::ids::CommitId,
    entries: Vec<knives::ledger::Entry>,
}

impl Untouched {
    fn read(lab: &Lab, home: &tempfile::TempDir) -> Self {
        Self {
            operations: operation_ids(&lab.work),
            commit: commit_at(lab, RELEASE),
            entries: ledger_entries(home),
        }
    }

    /// Each is as it was when read; `output` is the refused edit's, for the
    /// message.
    fn assert_kept(&self, lab: &Lab, home: &tempfile::TempDir, output: &str) {
        assert_eq!(operation_ids(&lab.work), self.operations, "{output}");
        assert_eq!(commit_at(lab, RELEASE), self.commit, "{output}");
        assert_eq!(ledger_entries(home), self.entries, "{output}");
    }
}

/// A knives cut of M, A and D, where A and D insert blocks at one place of
/// CHANGELOG.md and D's (in `d`) holds A's entry: the cut writes their
/// union, with A's entry twice. Then CHANGELOG.md is resolved by hand to
/// `resolved` and adopted with `--why`. Returns the lab and its home.
fn union_resolved_by_hand(d: &str, resolved: &str) -> (Lab, tempfile::TempDir) {
    let lab = trunk_with(&[("CHANGELOG.md", &log_with(""))]);
    member(&lab, M, &[("m.txt", "m\n")]);
    member(&lab, A, &[("CHANGELOG.md", &a_log())]);
    member(&lab, D, &[("CHANGELOG.md", d)]);
    let (home, _consumer) = release_test_home(&lab);
    let cut = release_ran(&lab, &home, &["cut", RELEASE]);
    assert!(conflicts_of(&lab, RELEASE).is_empty(), "{cut}");
    let unioned = show(&lab, "CHANGELOG.md");
    assert_eq!(unioned.matches(A_ENTRY).count(), 2, "{cut}\n{unioned}");
    edit_by_hand(&lab, RELEASE, &[("CHANGELOG.md", resolved)]);
    adopt(
        &lab,
        &home,
        RELEASE,
        "keep A's entry once, inside D's block",
    );
    (lab, home)
}

#[test]
fn a_union_joined_changelog_resolved_by_hand_is_not_dropped_in_silence() {
    // Given (K14 19): a release of M, A and D merged by hand, which no knives
    // write recorded. A and D insert their blocks at one place of
    // CHANGELOG.md, and D's holds A's entry, so the release-log union joins
    // that conflict; CHANGELOG.md is resolved by hand to D's version.
    let (lab, home) = octopus_by_hand(
        &[("CHANGELOG.md", &log_with(""))],
        &[
            (M, &[("m.txt", "m\n")]),
            (A, &[("CHANGELOG.md", &a_log())]),
            (D, &[("CHANGELOG.md", &d_log())]),
        ],
        ("CHANGELOG.md", "diff"),
        |_| d_log(),
    );
    let before = Untouched::read(&lab, &home);

    // When: D is dropped.
    let output = knives_release(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: the drop writes nothing, exits 3, and names CHANGELOG.md, A and
    // A's entry, and the remedy.
    let out = text(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    before.assert_kept(&lab, &home, &out);
    assert!(out.contains(&refusal_line("CHANGELOG.md", A)), "{out}");
    assert!(
        out.contains(&added_lost("CHANGELOG.md", 2, "- a entry")),
        "{out}"
    );
    assert!(out.contains(&remedy_line()), "{out}");
}

#[test]
fn a_recorded_union_with_a_hand_removed_duplicate_is_not_dropped_in_silence() {
    // Given (K14 20): a knives cut whose union holds A's entry twice; the
    // copy outside D's block removed by hand, so the text there is D's
    // version, and adopted with `--why`.
    let (lab, home) = union_resolved_by_hand(&d_log(), &d_log());
    let before = Untouched::read(&lab, &home);

    // When: D is dropped.
    let output = knives_release(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: the drop writes nothing, exits 3, and names CHANGELOG.md, A and
    // A's entry.
    let out = text(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    before.assert_kept(&lab, &home, &out);
    assert!(out.contains(&refusal_line("CHANGELOG.md", A)), "{out}");
    assert!(
        out.contains(&added_lost("CHANGELOG.md", 2, "- a entry")),
        "{out}"
    );
}

#[test]
fn three_members_at_one_changelog_place_resolved_to_d_refuse_the_drop_of_d() {
    // Given (K14 24): a knives cut of M, A, Q and D, where A, Q and D each
    // insert a block at one place of CHANGELOG.md and D's holds A's and Q's
    // entries: the union holds both entries twice. CHANGELOG.md is resolved
    // by hand to D's version and adopted with `--why`.
    let lab = trunk_with(&[("CHANGELOG.md", &log_with(""))]);
    let d = log_with(&format!("- d entry\n{A_ENTRY}- q entry\n"));
    member(&lab, M, &[("m.txt", "m\n")]);
    member(&lab, A, &[("CHANGELOG.md", &a_log())]);
    member(&lab, Q, &[("CHANGELOG.md", &log_with("- q entry\n"))]);
    member(&lab, D, &[("CHANGELOG.md", &d)]);
    let (home, _consumer) = release_test_home(&lab);
    let cut = release_ran(&lab, &home, &["cut", RELEASE]);
    let unioned = show(&lab, "CHANGELOG.md");
    assert_eq!(unioned.matches(A_ENTRY).count(), 2, "{cut}\n{unioned}");
    assert_eq!(
        unioned.matches("- q entry\n").count(),
        2,
        "{cut}\n{unioned}"
    );
    edit_by_hand(&lab, RELEASE, &[("CHANGELOG.md", &d)]);
    adopt(
        &lab,
        &home,
        RELEASE,
        "keep each entry once, inside D's block",
    );
    let before = Untouched::read(&lab, &home);

    // When: D is dropped.
    let output = knives_release(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: the drop writes nothing, exits 3, and names CHANGELOG.md with
    // A's and Q's entries; the operation log, the bookmark and the ledger
    // are as they were.
    let out = text(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    before.assert_kept(&lab, &home, &out);
    assert!(out.contains(&refusal_line("CHANGELOG.md", A)), "{out}");
    assert!(out.contains(&refusal_line("CHANGELOG.md", Q)), "{out}");
    assert!(
        out.contains(&added_lost("CHANGELOG.md", 2, "- a entry")),
        "{out}"
    );
    assert!(
        out.contains(&added_lost("CHANGELOG.md", 2, "- q entry")),
        "{out}"
    );
}

#[test]
fn a_reworded_d_entry_elsewhere_does_not_hide_a_lost_a_entry() {
    // Given (K14 27): (20)'s shape, where D also adds an entry at the end of
    // CHANGELOG.md, and the person rewords it there, with unchanged lines
    // between that place and the union's: a drop of D leaves that place a
    // conflict the union declines.
    let d = format!("{}- d other\n", d_log());
    let resolved = format!("{}- d other, reworded\n", d_log());
    let (lab, home) = union_resolved_by_hand(&d, &resolved);
    let before = Untouched::read(&lab, &home);

    // When: D is dropped.
    let output = knives_release(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: the drop writes nothing, exits 3, and names CHANGELOG.md, A and
    // A's entry.
    let out = text(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    before.assert_kept(&lab, &home, &out);
    assert!(out.contains(&refusal_line("CHANGELOG.md", A)), "{out}");
    assert!(
        out.contains(&added_lost("CHANGELOG.md", 2, "- a entry")),
        "{out}"
    );
}

#[test]
fn a_part_resolved_changelog_does_not_hide_a_lost_entry() {
    // Given (K14 28): (19)'s hand-made release with two more members, E and
    // F, that each insert a block at the end of CHANGELOG.md, with unchanged
    // lines between that place and A's; A's place is resolved by hand to
    // D's version, and the end is left in conflict.
    let (lab, home) = octopus_by_hand(
        &[("CHANGELOG.md", &log_with(""))],
        &[
            (M, &[("m.txt", "m\n")]),
            (A, &[("CHANGELOG.md", &a_log())]),
            (D, &[("CHANGELOG.md", &d_log())]),
            (
                E,
                &[("CHANGELOG.md", &format!("{}- e entry\n", log_with("")))],
            ),
            (
                F,
                &[("CHANGELOG.md", &format!("{}- f entry\n", log_with("")))],
            ),
        ],
        ("CHANGELOG.md", "diff"),
        |conflicted| first_conflict_resolved(conflicted, &format!("- d entry\n{A_ENTRY}")),
    );
    assert_eq!(conflicts_of(&lab, RELEASE), ["CHANGELOG.md"]);
    let before = Untouched::read(&lab, &home);

    // When: D is dropped.
    let output = knives_release(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: the drop writes nothing, exits 3, and names CHANGELOG.md, A,
    // A's entry and the remedy.
    let out = text(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    before.assert_kept(&lab, &home, &out);
    assert!(out.contains(&refusal_line("CHANGELOG.md", A)), "{out}");
    assert!(
        out.contains(&added_lost("CHANGELOG.md", 2, "- a entry")),
        "{out}"
    );
    assert!(out.contains(&remedy_line()), "{out}");
}

#[test]
fn an_advance_over_another_members_line_goes_ahead() {
    // Given (K14 23, a control): a cut of M, D and B, where B changes a file
    // of its own; A included, `f` resolved to D's version and adopted; then
    // B grown by a commit that also sets line 5 of `f`, A's line, to another
    // text.
    let lab = trunk_with(&[("f", &f_with(&[]))]);
    member(&lab, M, &[("m.txt", "m\n")]);
    member(&lab, D, &[("f", &d_f())]);
    member(&lab, B, &[("b.txt", "b\n")]);
    let (home, _consumer) = release_test_home(&lab);
    release_ran(&lab, &home, &["cut", RELEASE]);
    member(&lab, A, &[("f", &a_f())]);
    let include = release_ok(&lab, &home, &["include", A]);
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"], "{include}");
    lab.jj_work(["restore", "--from", D, "--into", RELEASE, "f"]);
    adopt(&lab, &home, RELEASE, "resolve f");
    extend_branch(&lab, B, "f", &f_with(&[(5, "b_fix ()")]));

    // When: B is advanced.
    let advanced = release_ok(&lab, &home, &["advance", B]);

    // Then: the edit is written, and the conflict at `f` is left for a
    // person: A's line is at that conflict, so it is not counted.
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"], "{advanced}");
    assert!(advanced.contains(&left_for_a_person(&["f"])), "{advanced}");
}

#[test]
fn a_line_an_adopted_why_change_removed_is_not_counted() {
    // Given (K14 25, a control): a file `g` on the trunk; A adds a line to
    // it; a cut of M and A; then A's line removed from `g` by hand and
    // adopted with `--why`, so the release lacks it, though the plain merge
    // of its parents holds it with no conflict; and G, which changes another
    // line of `g`, far from A's.
    let lab = trunk_with(&[("g", &f_with(&[]))]);
    member(&lab, M, &[("m.txt", "m\n")]);
    let with_a = f_with(&[]).replacen("let l10 = 10\n", "let l10 = 10\nlet a_line = 1\n", 1);
    member(&lab, A, &[("g", &with_a)]);
    let (home, _consumer) = release_test_home(&lab);
    release_ran(&lab, &home, &["cut", RELEASE]);
    edit_by_hand(&lab, RELEASE, &[("g", &f_with(&[]))]);
    adopt(&lab, &home, RELEASE, "take A's line out of g");
    member(&lab, G, &[("g", &f_with(&[(25, "g_fix ()")]))]);

    // When: G is included.
    let included = release_ok(&lab, &home, &["include", G]);

    // Then: written, with G's line and without A's: the release in hand
    // already lacked A's line, so the include does not count it.
    assert_eq!(show(&lab, "g"), f_with(&[(25, "g_fix ()")]), "{included}");
}

#[test]
fn a_merge_branch_include_rewriting_a_members_line_beside_the_resolved_hunk_goes_ahead() {
    // Given (K14 26, a control): a cut of M, D, E and F, where E sets line 10
    // of `f` to ε and F changes a file of its own; A included, `f` conflicts
    // at lines 5 and 6 and is resolved to D's text with ε, and adopted; then
    // C, a merge of E and F, rewrites ε to ε', which lacks a word of ε, with
    // unchanged lines between the resolved hunk and ε.
    let lab = trunk_with(&[("f", &f_with(&[]))]);
    member(&lab, M, &[("m.txt", "m\n")]);
    member(&lab, D, &[("f", &d_f())]);
    member(&lab, E, &[("f", &f_with(&[(10, "e_ten value")]))]);
    member(&lab, F, &[("f.txt", "f\n")]);
    let (home, _consumer) = release_test_home(&lab);
    release_ran(&lab, &home, &["cut", RELEASE]);
    member(&lab, A, &[("f", &a_f())]);
    let include = release_ok(&lab, &home, &["include", A]);
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"], "{include}");
    let resolved = [
        (5, "compare_fix ()"),
        (6, "d809_only ()"),
        (10, "e_ten value"),
    ];
    edit_by_hand(&lab, RELEASE, &[("f", &f_with(&resolved))]);
    adopt(&lab, &home, RELEASE, "resolve f");
    lab.jj_work(["new", "-r", E, "-r", F, "-m", C]);
    std::fs::write(lab.work.join("f"), f_with(&[(10, "e_ten")])).expect("rewrite E's line");
    lab.jj_work(["bookmark", "create", C, "-r", "@"]);
    lab.jj_work(["new", "main@origin"]);

    // When: C is included.
    let included = release_ok(&lab, &home, &["include", C]);

    // Then: written; E and F stay members, and `f` keeps the resolution and
    // holds ε'. ε is not counted: the plain merge lacks it with no conflict
    // at its place.
    let parents = release_parents(&lab, RELEASE);
    for member in [E, F, C] {
        assert!(
            parents.contains(&commit_at(&lab, member)),
            "{member}: {included}"
        );
    }
    assert_eq!(
        show(&lab, "f"),
        f_with(&[(5, "compare_fix ()"), (6, "d809_only ()"), (10, "e_ten")]),
        "{included}"
    );
}

#[test]
fn a_drop_allowed_to_lose_records_the_loss_and_adopt_records_its_repair() {
    // Given (K14 30): (20)'s shape.
    let (lab, home) = union_resolved_by_hand(&d_log(), &d_log());
    let before = Untouched::read(&lab, &home);

    // When: D is dropped with an `--allow-loss` that holds no reason.
    let blank = knives_release(
        &lab,
        &home,
        &["drop", D, "--why", "not ready", "--allow-loss", ""],
    );

    // Then: a usage error, as `drop --why ''` is, that names `--allow-loss`;
    // nothing is written.
    let stderr = String::from_utf8_lossy(&blank.stderr);
    assert_eq!(blank.status.code(), Some(2), "{}", text(&blank));
    assert!(
        stderr.contains("--allow-loss")
            && stderr.contains("a why must hold a reason, not be empty or only whitespace"),
        "{stderr}"
    );
    before.assert_kept(&lab, &home, &stderr);

    // When: D is dropped with a reason.
    let allowed = release_ok(
        &lab,
        &home,
        &[
            "drop",
            D,
            "--why",
            "not ready",
            "--allow-loss",
            "D is reworked",
        ],
    );

    // Then: written; A's entry is named as lost, with how to put it back,
    // and the event records the loss and its reason after the delta.
    assert_ne!(commit_at(&lab, RELEASE), before.commit, "{allowed}");
    assert!(
        allowed.contains(&format!(
            "demo: CHANGELOG.md: {A}'s lines lost by this write (--allow-loss)\n{}",
            added_lost("CHANGELOG.md", 2, "- a entry")
        )),
        "{allowed}"
    );
    assert!(
        allowed.contains(&format!(
            "demo: put the named lines back by hand and record that with `knives release adopt \
             {RELEASE} --why <reason>`\n"
        )),
        "{allowed}"
    );
    let event = newest_event(&home);
    assert!(
        event.text.contains(&format!(
            "dropped {D}: not ready; lost: CHANGELOG.md ({A}: 1 line(s)); why lost: D is \
             reworked; parents: "
        )),
        "{}",
        event.text
    );

    // When: A's entry is put back by hand, and the release adopted with
    // `--why`.
    edit_by_hand(&lab, RELEASE, &[("CHANGELOG.md", &a_log())]);
    let repaired = commit_at(&lab, RELEASE);
    let adopted = release_ok(
        &lab,
        &home,
        &["adopt", RELEASE, "--why", "put back A's entry"],
    );

    // Then: it records that commit.
    assert!(
        adopted.contains(&format!("as the newest write of {RELEASE}")),
        "{adopted}"
    );
    assert_eq!(
        newest_event(&home).evidence.first().map(String::as_str),
        Some(repaired.as_str()),
        "{adopted}"
    );
}

#[test]
fn allow_loss_with_nothing_counted_changes_nothing() {
    // Given: a cut of M and D, and Q, which changes a file of its own.
    let lab = trunk_with(&[("f", &f_with(&[]))]);
    member(&lab, M, &[("m.txt", "m\n")]);
    member(&lab, D, &[("f", &d_f())]);
    let (home, _consumer) = release_test_home(&lab);
    release_ran(&lab, &home, &["cut", RELEASE]);
    member(&lab, Q, &[("q.txt", "q\n")]);

    // When: Q is included with `--allow-loss`.
    let included = release_ok(&lab, &home, &["include", Q, "--allow-loss", "no loss here"]);

    // Then: it writes and reports as it does without the flag: no line is
    // named as lost, and its event holds no loss and no reason for one.
    assert!(
        included.contains(&format!(
            "demo: {RELEASE} now has 3 parent(s): included {Q}\n"
        )),
        "{included}"
    );
    assert!(!included.contains("--allow-loss"), "{included}");
    let event = newest_event(&home);
    assert!(
        event.text.contains(&format!("included {Q}; parents: ")),
        "{}",
        event.text
    );
    assert!(!event.text.contains("; lost: "), "{}", event.text);
    assert!(!event.text.contains("; why lost: "), "{}", event.text);
}

// ---------------------------------------------------------------------------
// R6 in R11's check and adopt's measure: a remaining member that the trunk
// contains is read from the base the trunk took it in on.
// ---------------------------------------------------------------------------

/// D24's line for a member that the trunk holds on its first-parent line,
/// read from `base`, which another parent of the release holds.
fn base_line(member: &str, base: &knives::ids::CommitId) -> String {
    format!(
        "  {member}: read from {}, the newest trunk commit below it that another parent of \
         {RELEASE} holds",
        base.short()
    )
}

/// D24's line for a member that the trunk holds on its first-parent line,
/// and that no other parent of the release gives a base.
fn no_base_line(member: &str) -> String {
    format!(
        "  {member}: its lines are not read: the trunk holds it on its first-parent line, and \
         every other parent of {RELEASE} holds it or no trunk commit below it"
    )
}

/// Whether `out` holds `line` as one whole line.
fn has_line(out: &str, line: &str) -> bool {
    out.lines().any(|candidate| candidate == line)
}

/// `knives release adopt <release> --why <why>`, whatever its exit.
fn adopt_why(lab: &Lab, home: &tempfile::TempDir, why: &str) -> Output {
    knives_release(lab, home, &["adopt", RELEASE, "--why", why])
}

/// `CHANGELOG.md` as D writes it, with A's entry taken out by hand.
fn without_a_entry() -> String {
    log_with("- d entry\n")
}

#[test]
fn a_remaining_member_the_trunk_merged_is_refused_when_the_edit_drops_its_lines() {
    // Given (K14 31): (20)'s shape; then upstream merges A's pull request by
    // a merge commit, so the trunk contains A.
    let (lab, home) = union_resolved_by_hand(&d_log(), &d_log());
    lab.publish_pull(A, 1);
    lab.merge_pull_with_merge_commit(1);
    let before = Untouched::read(&lab, &home);

    // When: D is dropped.
    let output = knives_release(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: A is read from its fork point with the merge's first parent, so
    // the drop writes nothing, exits 3, and names CHANGELOG.md, A and A's
    // entry.
    let out = text(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    before.assert_kept(&lab, &home, &out);
    assert!(out.contains(&refusal_line("CHANGELOG.md", A)), "{out}");
    assert!(
        out.contains(&added_lost("CHANGELOG.md", 2, "- a entry")),
        "{out}"
    );
}

/// (20)'s shape, where A is two commits, the first writing a file of its
/// own and the second adding A's entry; then upstream fast-forwards its
/// trunk to A and moves on by one commit, and the release is rebased onto
/// that trunk with `knives release rebase main@upstream`: M and D move onto
/// the trunk past A's tip, and A stays a parent, the usual shape after a
/// rebase. Returns the lab and its home.
fn rebased_onto_a_fast_forwarded_member() -> (Lab, tempfile::TempDir) {
    let lab = trunk_with(&[("CHANGELOG.md", &log_with(""))]);
    member(&lab, M, &[("m.txt", "m\n")]);
    member(&lab, A, &[("a.txt", "a\n")]);
    extend_branch(&lab, A, "CHANGELOG.md", &a_log());
    member(&lab, D, &[("CHANGELOG.md", &d_log())]);
    let (home, _consumer) = release_test_home(&lab);
    let cut = release_ran(&lab, &home, &["cut", RELEASE]);
    let unioned = show(&lab, "CHANGELOG.md");
    assert_eq!(unioned.matches(A_ENTRY).count(), 2, "{cut}\n{unioned}");
    edit_by_hand(&lab, RELEASE, &[("CHANGELOG.md", &d_log())]);
    adopt(
        &lab,
        &home,
        RELEASE,
        "keep A's entry once, inside D's block",
    );
    lab.publish_pull(A, 1);
    lab.fast_forward_pull(1);
    lab.advance_upstream("past A\n");
    let target = commit_at(&lab, "main@upstream");
    let rebased = release_ran(&lab, &home, &["rebase", "main@upstream"]);
    assert!(
        release_parents(&lab, RELEASE).contains(&commit_at(&lab, A)),
        "A stays a parent: {rebased}"
    );
    for member in [M, D] {
        assert_eq!(
            lab.revision(&lab.work, &format!("{member}-"), "commit_id"),
            target.as_str(),
            "{member} forks past A's tip: {rebased}"
        );
    }
    (lab, home)
}

#[test]
fn an_edit_and_an_adopt_read_no_line_of_a_fast_forwarded_member_every_parent_holds() {
    // Given (K14 33): A, fast-forwarded onto the trunk, and M and D, which
    // fork past A's tip after the rebase; D's block in CHANGELOG.md holds
    // A's entry, as the hand resolution left it; then A's entry taken out of
    // CHANGELOG.md by hand.
    let (lab, home) = rebased_onto_a_fast_forwarded_member();
    edit_by_hand(&lab, RELEASE, &[("CHANGELOG.md", &without_a_entry())]);

    // When: the release is adopted with a reason.
    let adopted = adopt_why(&lab, &home, "take out A's entry");

    // Then: it is recorded; every other parent holds A's tip, so no base
    // says where A began: the measure reads no line of A and says so,
    // naming A, and counts nothing of A as absent.
    let out = text(&adopted);
    assert!(
        out.contains(&format!("as the newest write of {RELEASE}")),
        "{out}"
    );
    assert!(has_line(&out, &no_base_line(A)), "{out}");
    assert!(
        !out.lines()
            .any(|line| line.contains(&format!("{A}: "))
                && line.contains("absent from the resolution")),
        "{out}"
    );

    // Given: the same release again, as the rebase left it.
    let (lab, home) = rebased_onto_a_fast_forwarded_member();

    // When: D is dropped.
    let output = knives_release(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: the check reads no line of A and says so, naming A, and the
    // drop goes ahead, though its write loses A's entry.
    let out = text(&output);
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert!(!show(&lab, "CHANGELOG.md").contains(A_ENTRY), "{out}");
    assert!(has_line(&out, &no_base_line(A)), "{out}");
}

#[test]
fn an_edit_and_an_adopt_read_a_fast_forwarded_member_from_a_parent_below_it() {
    // Given (K14 34): (20)'s shape, where M and D fork at T0, the trunk
    // commit below A's first commit; then upstream fast-forwards its trunk
    // to A.
    let (lab, home) = union_resolved_by_hand(&d_log(), &d_log());
    let below = commit_at(&lab, "main@upstream");
    lab.publish_pull(A, 1);
    lab.fast_forward_pull(1);
    let before = Untouched::read(&lab, &home);

    // When: D is dropped.
    let output = knives_release(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: A is read from T0, which the other parents hold: the drop writes
    // nothing, exits 3, names CHANGELOG.md, A and A's entry, and names T0
    // as A's base.
    let out = text(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    before.assert_kept(&lab, &home, &out);
    assert!(out.contains(&refusal_line("CHANGELOG.md", A)), "{out}");
    assert!(
        out.contains(&added_lost("CHANGELOG.md", 2, "- a entry")),
        "{out}"
    );
    assert!(has_line(&out, &base_line(A, &below)), "{out}");

    // Given: A's entry taken out of CHANGELOG.md by hand.
    edit_by_hand(&lab, RELEASE, &[("CHANGELOG.md", &without_a_entry())]);

    // When: the release is adopted with a reason.
    let adopted = adopt_why(&lab, &home, "take out A's entry");

    // Then: A's entry is counted absent and flagged, with the findings
    // exit, and T0 is named as A's base.
    let out = text(&adopted);
    assert_eq!(adopted.status.code(), Some(1), "{out}");
    assert!(
        out.contains(&format!(
            "  !! {A}: 1 of 1 added line(s) of CHANGELOG.md absent from the resolution"
        )),
        "{out}"
    );
    assert!(has_line(&out, &base_line(A, &below)), "{out}");
}

#[test]
fn an_advance_past_a_fast_forwarded_tip_reads_the_member_from_that_tip_and_goes_ahead() {
    // Given: (20)'s shape, where M and D fork at T0, the trunk commit below
    // A's first commit; upstream fast-forwards its trunk to A; then A's
    // branch moves on by a commit that writes a file of its own.
    let (lab, home) = union_resolved_by_hand(&d_log(), &d_log());
    lab.publish_pull(A, 1);
    lab.fast_forward_pull(1);
    let landed = commit_at(&lab, A);
    extend_branch(&lab, A, "a2.txt", "a2 line\n");
    let moved_on = commit_at(&lab, A);

    // When: A is advanced.
    let output = knives_release(&lab, &home, &["advance", A]);

    // Then: the advance matches A's new tip to the landed tip by the
    // record. The trunk does not hold the new tip, so the check reads A
    // from its fork point with the trunk, the landed tip: no line names a
    // base for A, the check counts nothing, and the advance writes. The
    // release holds A's new tip in place of the landed one, A's entry once,
    // inside D's block, and A's new file.
    let out = text(&output);
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&format!(
            "demo: {A} matched {} by the last cut or edit record; that parent has landed \
             upstream, so the branch's tip continues it only by name",
            landed.short()
        )),
        "{out}"
    );
    assert!(!out.contains(&format!("  {A}: read from ")), "{out}");
    assert!(!has_line(&out, &no_base_line(A)), "{out}");
    assert!(!out.contains("refusing to write"), "{out}");
    let parents = release_parents(&lab, RELEASE);
    assert!(parents.contains(&moved_on), "{out}");
    assert!(!parents.contains(&landed), "{out}");
    assert_eq!(show(&lab, "CHANGELOG.md"), d_log(), "{out}");
    assert_eq!(show(&lab, "a2.txt"), "a2 line\n", "{out}");
}

#[test]
fn an_edit_reads_a_fast_forwarded_member_from_a_base_inside_it_and_names_it() {
    // Given (K14 35): (20)'s shape, where A is two commits: c1 adds an entry
    // on top of CHANGELOG.md and c2 another above it; D's block holds both;
    // N forks at c1 and writes a file of its own. The cut's union holds
    // each of A's entries twice; CHANGELOG.md is resolved by hand to D's
    // version and adopted; then upstream fast-forwards its trunk to A.
    let (first_entry, second_entry) = ("- a1 entry\n", "- a2 entry\n");
    let lab = trunk_with(&[("CHANGELOG.md", &log_with(""))]);
    member(&lab, M, &[("m.txt", "m\n")]);
    member(&lab, A, &[("CHANGELOG.md", &log_with(first_entry))]);
    let first = commit_at(&lab, A);
    let both = format!("{second_entry}{first_entry}");
    extend_branch(&lab, A, "CHANGELOG.md", &log_with(&both));
    lab.jj_work(["new", "-r", first.as_str(), "-m", N]);
    std::fs::write(lab.work.join("n.txt"), "n\n").expect("write N's file");
    lab.jj_work(["bookmark", "create", N, "-r", "@"]);
    lab.jj_work(["new", "main@origin"]);
    let d = log_with(&format!("- d entry\n{both}"));
    member(&lab, D, &[("CHANGELOG.md", &d)]);
    let (home, _consumer) = release_test_home(&lab);
    let cut = release_ran(&lab, &home, &["cut", RELEASE]);
    let unioned = show(&lab, "CHANGELOG.md");
    for entry in [first_entry, second_entry] {
        assert_eq!(unioned.matches(entry).count(), 2, "{cut}\n{unioned}");
    }
    edit_by_hand(&lab, RELEASE, &[("CHANGELOG.md", &d)]);
    adopt(
        &lab,
        &home,
        RELEASE,
        "keep A's entries once, inside D's block",
    );
    lab.publish_pull(A, 1);
    lab.fast_forward_pull(1);
    let before = Untouched::read(&lab, &home);

    // When: D is dropped.
    let output = knives_release(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: A is read from c1, the newest trunk commit below its tip that N
    // holds, and the output names c1; of A's two entries, which the write
    // would lose, only c2's is counted: the drop writes nothing, exits 3,
    // and names CHANGELOG.md, A and c2's entry, not c1's.
    let out = text(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    before.assert_kept(&lab, &home, &out);
    assert!(out.contains(&refusal_line("CHANGELOG.md", A)), "{out}");
    assert!(
        out.contains(&added_lost("CHANGELOG.md", 2, "- a2 entry")),
        "{out}"
    );
    assert!(!out.contains("‹- a1 entry›"), "{out}");
    assert!(has_line(&out, &base_line(A, &first)), "{out}");
}

#[test]
fn an_edit_reads_no_line_of_a_fast_forwarded_member_whose_other_parents_share_no_trunk_commit() {
    // Given: (20)'s shape, where M and D fork from a commit of an unrelated
    // history that holds the trunk's CHANGELOG.md, and A from the trunk; the
    // cut's CHANGELOG.md resolved by hand to D's version, whose block holds
    // A's entry, and adopted; then upstream fast-forwards its trunk to A.
    let lab = trunk_with(&[("CHANGELOG.md", &log_with(""))]);
    lab.jj_work(["new", "root()", "-m", "an unrelated history"]);
    std::fs::write(lab.work.join("CHANGELOG.md"), log_with("")).expect("write the log");
    lab.jj_work(["new"]);
    let unrelated = lab.revision(&lab.work, "@-", "commit_id");
    for (name, file, content) in [(M, "m.txt", "m\n".to_owned()), (D, "CHANGELOG.md", d_log())] {
        lab.jj_work(["new", "-r", unrelated.as_str(), "-m", name]);
        std::fs::write(lab.work.join(file), content).expect("write a member's file");
        lab.jj_work(["bookmark", "create", name, "-r", "@"]);
    }
    member(&lab, A, &[("CHANGELOG.md", &a_log())]);
    let (home, _consumer) = release_test_home(&lab);
    let cut = release_ran(&lab, &home, &["cut", RELEASE]);
    edit_by_hand(&lab, RELEASE, &[("CHANGELOG.md", &d_log())]);
    adopt(&lab, &home, RELEASE, "resolve the changelog to D's version");
    lab.publish_pull(A, 1);
    lab.fast_forward_pull(1);

    // When: D is dropped.
    let output = knives_release(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: the other parents share no trunk commit with A, so no base says
    // where A began: the check reads no line of A and says so, naming A,
    // and the drop goes ahead.
    let out = text(&output);
    assert_eq!(output.status.code(), Some(0), "{cut}\n{out}");
    assert!(has_line(&out, &no_base_line(A)), "{out}");
}
