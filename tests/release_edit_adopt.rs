//! What `knives release include`, `drop` and `advance` write in a file whose
//! conflict the release resolved (spec R11).
//!
//! An edit keeps the release's resolution of a conflict only where the plain
//! merge of the new parents leaves the same conflict there. Everywhere else
//! it writes that plain merge, names the file, and says whether the file
//! holds a conflict. The conflict is read from the plain merge of the release
//! in hand's own parents, so the rule holds whether or not an adopt recorded
//! the resolution. A release log whose conflict the release-log union joins
//! is left to the union. The shape is INFRA-4895's drop-after-adopt probe: M
//! adds a file of its own, D sets lines 5 and 6 of `f`, and A, made after the
//! first cut, sets line 5 as D does. These tests drive only the knives
//! binary.

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
    Lab, ReleaseOutput, commit_at, extend_branch, knives_release, release_command, release_parents,
    release_test_home,
};

/// The dated release most tests cut and edit.
const RELEASE: &str = "release/2026-08-04";
/// The fixed release of the test with a carrier parent.
const FIXED: &str = "integration";
/// A member that changes no line of `f`.
const M: &str = "feat/m";
/// A member that sets lines 5 and 6 of `f`, as `d809` did in the probe.
const D: &str = "feat/d";
/// A member, made after the first cut, that sets line 5 of `f` as D does.
const A: &str = "feat/a";
/// A member that changes only a file of its own.
const Q: &str = "feat/q";
/// A member that sets line 30 of `f`, far from the conflict, or, in the
/// tests of a side's count, a cherry-pick of A.
const P: &str = "feat/p";
/// A member that sets line 5 of a second file `g` as D does.
const B: &str = "feat/b";
/// Two members that each add a section on top of `CHANGELOG.md`.
const X: &str = "feat/x";
const Y: &str = "feat/y";
/// The line a person adds to `f` by hand, after its line 20: unchanged lines
/// lie between it and the conflict at lines 5 and 6.
const HAND_LINE: &str = "let e = hand ()\n";

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

/// `f` with [`HAND_LINE`] after its line 20.
fn with_hand_line(f: &str) -> String {
    f.replacen("let l20 = 20\n", &format!("let l20 = 20\n{HAND_LINE}"), 1)
}

/// A lab whose trunk, upstream and on the fork, holds `files`.
fn trunk_with(files: &[(&str, &str)]) -> Lab {
    let lab = Lab::new();
    for (file, content) in files {
        lab.upstream_trunk_file(file, content);
    }
    lab.mirror_upstream_trunk_to_origin();
    lab
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

/// A release verb run in the work checkout that must exit 0; its stdout.
fn release_ok(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> String {
    let output = knives_release(lab, home, args);
    assert_eq!(output.status.code(), Some(0), "{args:?}: {}", text(&output));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A release verb run in the work checkout that must go through, though the
/// plan around it may exit with findings of its own; its stdout.
fn release_ran(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> String {
    let output = knives_release(lab, home, args);
    assert!(
        matches!(output.status.code(), Some(0 | 1)),
        "{args:?}: {}",
        text(&output)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// `knives release adopt <release> --why <why>`, which must record the
/// release.
fn adopt(lab: &Lab, home: &tempfile::TempDir, release: &str, why: &str) {
    let adopted = release_ran(lab, home, &["adopt", release, "--why", why]);
    assert!(
        adopted.contains(&format!("as the newest write of {release}")),
        "{adopted}"
    );
}

/// The files `revision` leaves conflicted.
fn conflicts_of(lab: &Lab, revision: &str) -> Vec<String> {
    knives::jj::conflicted_files(&lab.work, revision).expect("list the conflicted files")
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
    let output = Command::new("jj")
        .args(&args)
        .current_dir(&lab.work)
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .output()
        .expect("make the plain merge");
    assert!(output.status.success(), "{}", text(&output));
    let merged = show_in(&lab.work, "@", file);
    lab.jj_work(["new", "main@origin"]);
    merged
}

/// The marker a line of a materialized conflict starts with: `<`, `>`, `+`
/// or `-` seven times or more, else nothing.
fn marker(line: &str) -> Option<char> {
    let first = line.chars().next()?;
    ("<>+-".contains(first) && line.starts_with(&first.to_string().repeat(7))).then_some(first)
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

/// Edit `release` by hand, as a person does: `jj edit`, the files written,
/// the working copy moved off it.
fn edit_by_hand(lab: &Lab, release: &str, files: &[(&str, &str)]) {
    lab.jj_work(["edit", release]);
    for (file, content) in files {
        std::fs::write(lab.work.join(file), content).expect("write by hand");
    }
    lab.jj_work(["new", "main@origin"]);
}

/// How `f` is resolved by hand after `include A` conflicts there.
#[derive(Clone, Copy)]
enum Resolution {
    /// `f` as D has it, which holds A's line too.
    DVersion,
    /// A text that keeps neither side.
    Neither,
}

/// The probe's shape up to the include: a trunk holding `f`; M adds
/// `m.txt`; D writes `d` as its `f`; a first cut of M and D; A, made after
/// the cut, sets line 5 of `f`; `include A`. Returns the lab, its home and
/// the include's stdout.
fn included(d: &str) -> (Lab, tempfile::TempDir, String) {
    let lab = trunk_with(&[("f", &f_with(&[]))]);
    member(&lab, M, &[("m.txt", "m\n")]);
    member(&lab, D, &[("f", d)]);
    let (home, _consumer) = release_test_home(&lab);
    let cut = release_ran(&lab, &home, &["cut", RELEASE]);
    assert_eq!(release_parents(&lab, RELEASE).len(), 2, "{cut}");
    member(&lab, A, &[("f", &a_f())]);
    let include = release_ok(&lab, &home, &["include", A]);
    (lab, home, include)
}

/// [`included`] with D's `f`: the include conflicts in `f`, which is
/// resolved by hand and adopted. Returns the lab and its home.
fn adopted(resolution: Resolution) -> (Lab, tempfile::TempDir) {
    let (lab, home, include) = included(&d_f());
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"], "{include}");
    match resolution {
        Resolution::DVersion => lab.jj_work(["restore", "--from", D, "--into", RELEASE, "f"]),
        Resolution::Neither => edit_by_hand(&lab, RELEASE, &[("f", "merged\n")]),
    }
    adopt(&lab, &home, RELEASE, "resolve f");
    assert!(
        conflicts_of(&lab, RELEASE).is_empty(),
        "the adopt left a conflict"
    );
    (lab, home)
}

/// [`adopted`] to D's version, then [`HAND_LINE`] added to `f` by hand and
/// recorded with `adopt --why`. Returns the lab and its home.
fn adopted_with_hand_line() -> (Lab, tempfile::TempDir) {
    let (lab, home) = adopted(Resolution::DVersion);
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
    let lab = trunk_with(&[("f", &f_with(&[]))]);
    member(&lab, M, &[("m.txt", "m\n")]);
    member(&lab, A, &[("f", a)]);
    member(&lab, D, &[("f", d)]);
    let (home, _consumer) = release_test_home(&lab);
    let config = format!("ui.conflict-marker-style={style}");
    lab.jj_work([
        "--config",
        config.as_str(),
        "new",
        "-r",
        M,
        "-r",
        A,
        "-r",
        D,
        "-m",
        RELEASE,
    ]);
    let path = lab.work.join("f");
    let conflicted = std::fs::read_to_string(&path).expect("read the conflicted f");
    std::fs::write(&path, edit(&conflicted)).expect("write f by hand");
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
    let mut resolved = String::new();
    let mut inside = None;
    for line in conflicted.split_inclusive('\n') {
        match (inside, marker(line)) {
            (None, Some('<')) => inside = Some(true),
            (Some(true), Some('>')) => {
                resolved.push_str("let l5 = compare_fix ()\nlet l6 = d809_only ()\n");
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
    let (lab, home) = adopted(Resolution::DVersion);

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
    let (lab, home) = adopted(Resolution::DVersion);
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
    let (lab, home) = adopted(Resolution::DVersion);

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
    let (lab, home) = adopted(Resolution::DVersion);
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
    let (lab, home, include) = included(&d_f());
    assert_eq!(conflicts_of(&lab, RELEASE), ["f"], "{include}");

    // When: D is dropped.
    let dropped = release_ok(&lab, &home, &["drop", D, "--why", "not ready"]);

    // Then: `f` is A's version.
    assert_eq!(show(&lab, "f"), a_f(), "{dropped}");

    // Given: D's change apart from A's, so the include has no conflict.
    let (lab, home, include) = included(&f_with(&[(20, "d809_only ()")]));
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
    let (lab, home) = adopted(Resolution::DVersion);
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
    let (lab, home) = adopted(Resolution::DVersion);

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
    let (lab, home) = adopted(Resolution::DVersion);
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
    let (lab, home) = adopted(Resolution::Neither);
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
    let (lab, home) = adopted(Resolution::DVersion);

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
    let (lab, home) = adopted(Resolution::DVersion);
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
    let (lab, home) = adopted(Resolution::DVersion);
    let adopted_parents = release_parents(&lab, RELEASE);
    lab.advance_upstream("advance\n");
    release_ok(&lab, &home, &["rebase", "main@upstream"]);
    for branch in [M, D, A, RELEASE] {
        lab.push_branch(branch);
    }
    let second = lab.second.clone();
    let upstream = lab.upstream.display().to_string();
    lab.jj_at(
        &second,
        ["git", "remote", "add", "upstream", upstream.as_str()],
    );
    lab.jj_at(&second, ["git", "fetch", "--all-remotes"]);
    for branch in [M, D, A, RELEASE] {
        let remote = format!("{branch}@origin");
        lab.jj_at(&second, ["bookmark", "track", remote.as_str()]);
    }
    let opened = knives::jj::Repo::open(&second).expect("open the second checkout");
    for parent in &adopted_parents {
        assert!(
            opened.resolve_commit(parent.as_str()).is_err(),
            "the second checkout holds {parent}"
        );
    }

    // When: D is dropped in the second checkout.
    let output = release_in(&lab, &home, &second, &["drop", D, "--why", "not ready"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));

    // Then: `f` is A's version, as K14 (1) writes it.
    assert_eq!(show_in(&second, RELEASE, "f"), a_f(), "{}", text(&output));
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
    let (lab, home) = adopted(Resolution::DVersion);
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

/// A registry home whose entry releases on the fixed branch [`FIXED`], with
/// an empty local consumer.
fn fixed_home(lab: &Lab) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\n\
             release_branch = \"{FIXED}\"\n",
            lab.upstream.display(),
        ),
    )
    .expect("write fixed-scheme registry");
    let consumer = home.path().join("consumer");
    std::fs::create_dir_all(&consumer).expect("create local consumer");
    std::fs::write(
        home.path().join("local-consumer"),
        consumer.display().to_string(),
    )
    .expect("write local consumer fixture path");
    home
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
    let home = fixed_home(&lab);
    release_ran(&lab, &home, &["cut"]);
    let carrier = commit_at(&lab, FIXED);
    lab.push_branch(FIXED);
    member(&lab, A, &[("f", &a_f())]);
    release_ran(&lab, &home, &["include", A]);
    assert_eq!(conflicts_of(&lab, FIXED), ["f"]);
    lab.jj_work(["restore", "--from", D, "--into", FIXED, "f"]);
    adopt(&lab, &home, FIXED, "resolve f");
    let before = commit_at(&lab, FIXED);
    let destination = format!("parents({FIXED}) | {FIXED}@origin");
    lab.jj_work(["rebase", "-r", FIXED, "-d", destination.as_str()]);
    lab.jj_work(["restore", "--from", before.as_str(), "--into", FIXED]);
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
    let (lab, home) = adopted(Resolution::Neither);
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
    let (lab, home) = adopted(Resolution::DVersion);
    let dropped = release_ok(&lab, &home, &["drop", D, "--why", "not ready"]);
    assert!(dropped.contains(&written_line("f", false)), "{dropped}");
    member(&lab, Q, &[("q.txt", "q\n")]);

    // When: Q is included.
    let included = release_ok(&lab, &home, &["include", Q]);

    // Then: `f` stays as the drop wrote it, and nothing names it.
    assert_eq!(show(&lab, "f"), a_f(), "{included}");
    assert!(!included.contains(&written_prefix("f")), "{included}");
}
