//! The release log union every release write writes itself.
//!
//! Every member of a fork release inserts its own block at the top of
//! `CHANGELOG.md` and `RELEASE_NOTES.md`, so any composition of two members
//! conflicts in both files although no side touched another's text. A cut, an
//! `include`, an `advance` and a rebase write that conflict's union — the
//! block the write brings in above what the release already held, each
//! block whole, except that in a file with one `## [Unreleased]` heading,
//! inside that section, where every block is plain `### ` lists, the entries
//! under one `### ` heading join that heading's one list, the heading written
//! once — and the content checks read a member whose block the union holds
//! as carried. A conflict that is not every side inserting its own block —
//! a member that rewrote its block, sides on different bases, a code file, a
//! drop that takes a block out — stays conflicted and is named as a
//! person's.
//! The fixtures are the real ygo and oh-my-pi files, cut down; their README
//! names each source.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use lab::{Lab, commit_at, file_at_revision, knives_release, release_test_home};

const BASE_LOG: &str = include_str!("fixtures/changelog_union/ygo/base/CHANGELOG.md");
const BASE_NOTES: &str = include_str!("fixtures/changelog_union/ygo/base/RELEASE_NOTES.md");
const SAMI11_LOG: &str = include_str!("fixtures/changelog_union/ygo/sami11/CHANGELOG.md");
const SAMI11_NOTES: &str = include_str!("fixtures/changelog_union/ygo/sami11/RELEASE_NOTES.md");
const SPLIT_LOG: &str = include_str!("fixtures/changelog_union/ygo/split-from-store/CHANGELOG.md");
const SPLIT_NOTES: &str =
    include_str!("fixtures/changelog_union/ygo/split-from-store/RELEASE_NOTES.md");
const CLIENT_LOG: &str = include_str!("fixtures/changelog_union/ygo/client-count/CHANGELOG.md");
const CLIENT_NOTES: &str =
    include_str!("fixtures/changelog_union/ygo/client-count/RELEASE_NOTES.md");
const SPLIT_ONE_PATH_LOG: &str =
    include_str!("fixtures/changelog_union/ygo/split-one-path/CHANGELOG.md");
const SPLIT_ONE_PATH_NOTES: &str =
    include_str!("fixtures/changelog_union/ygo/split-one-path/RELEASE_NOTES.md");
const UNION_TWO_LOG: &str =
    include_str!("fixtures/changelog_union/ygo/union-sami11-split-from-store/CHANGELOG.md");
const UNION_TWO_NOTES: &str =
    include_str!("fixtures/changelog_union/ygo/union-sami11-split-from-store/RELEASE_NOTES.md");
const UNION_THREE_LOG: &str = include_str!(
    "fixtures/changelog_union/ygo/union-client-count-split-from-store-sami11/CHANGELOG.md"
);
const UNION_THREE_NOTES: &str = include_str!(
    "fixtures/changelog_union/ygo/union-client-count-split-from-store-sami11/RELEASE_NOTES.md"
);
const OMP_FORK_POINT: &str =
    include_str!("fixtures/changelog_union/oh-my-pi/fork-point/CHANGELOG.md");
const OMP_MEMBER: &str = include_str!("fixtures/changelog_union/oh-my-pi/member/CHANGELOG.md");
const OMP_RELEASE: &str = include_str!("fixtures/changelog_union/oh-my-pi/release/CHANGELOG.md");

/// Where oh-my-pi keeps the changelog that conflicted: a release log in a
/// directory, which is a release log all the same.
const OMP_PATH: &str = "packages/coding-agent/CHANGELOG.md";

const RELEASE: &str = "release/2026-08-04";
const SPLIT: &str = "rel/split-from-store";
const SAMI11: &str = "sami11";
const CLIENT: &str = "rel/client-count";

/// Both ygo release logs, as one fixture directory holds them.
const fn logs<'a>(log: &'a str, notes: &'a str) -> [(&'static str, &'a str); 2] {
    [("CHANGELOG.md", log), ("RELEASE_NOTES.md", notes)]
}

/// A lab whose trunk, upstream and on the fork, holds `files`: the base every
/// member forks from.
fn trunk_with(files: &[(&str, &str)]) -> Lab {
    let lab = Lab::new();
    for (file, content) in files {
        lab.upstream_trunk_file(file, content);
    }
    lab.mirror_upstream_trunk_to_origin();
    lab
}

/// The ygo series base, with both release logs on the trunk.
fn ygo_lab() -> Lab {
    trunk_with(&logs(BASE_LOG, BASE_NOTES))
}

/// A member branch forked from the fork's trunk that writes `files`.
fn member(lab: &Lab, name: &str, files: &[(&str, &str)]) {
    lab.jj_work(["new", "-r", "main@origin", "-m", name]);
    write_files(lab, files);
    lab.jj_work(["bookmark", "create", name, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// A commit on `branch` that writes `files`, the bookmark moved onto it.
fn grow(lab: &Lab, branch: &str, files: &[(&str, &str)]) {
    lab.jj_work(["new", branch, "-m", "follow-up"]);
    write_files(lab, files);
    lab.jj_work(["bookmark", "set", branch, "-r", "@"]);
    lab.jj_work(["new"]);
}

fn write_files(lab: &Lab, files: &[(&str, &str)]) {
    for (file, content) in files {
        std::fs::write(lab.work.join(file), content).expect("write a member's file");
    }
}

/// `file` with `block` inserted above its first `## ` heading: a new section
/// on top of every section the file holds.
fn above_first_section(file: &str, block: &str) -> String {
    let at = if file.starts_with("## ") {
        0
    } else {
        file.find("\n## ").map_or(file.len(), |newline| newline + 1)
    };
    let (prefix, sections) = file.split_at(at);
    format!("{prefix}{block}{sections}")
}

/// Run a release verb that must succeed; its stdout.
fn release_ok(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> String {
    let output = knives_release(lab, home, args);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{args:?} failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// The files the release still leaves conflicted.
fn conflicts(lab: &Lab) -> Vec<String> {
    knives::jj::conflicted_files(&lab.work, RELEASE).expect("list the release's conflicts")
}

/// The release's `file` is exactly `expected`, byte for byte.
fn assert_release_file(lab: &Lab, file: &str, expected: &str) {
    let actual = file_at_revision(lab, RELEASE, file);
    assert!(
        actual == expected,
        "{file} is not the expected content:\n{actual}"
    );
}

/// `knives --json audit demo --no-github`, parsed. The release homes name an
/// unreachable origin so consumer pins match it; the audit reads live remote
/// refs first and reports nothing else when they fail, so it runs against the
/// lab's own origin, with the same ledger.
fn audit_report(lab: &Lab, home: &tempfile::TempDir) -> serde_json::Value {
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"{}\"\n",
            lab.upstream.display(),
            lab.temp_origin().display(),
        ),
    )
    .expect("point the registry at the lab's origin");
    let output = lab::knives(lab, home, &["--json", "audit", "demo", "--no-github"]);
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "audit emits JSON ({error}): {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
    assert_eq!(
        report["problems"],
        serde_json::json!(["open pull-head reconciliation was skipped (--no-github)"]),
        "the audit must have read everything but pull heads: {report}"
    );
    report
}

/// The audit findings of `kind`.
fn findings<'a>(report: &'a serde_json::Value, kind: &str) -> Vec<&'a serde_json::Value> {
    report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .filter(|finding| finding["kind"] == kind)
        .collect()
}

/// The fork's ledger entries.
fn ledger_entries(home: &tempfile::TempDir) -> Vec<knives::ledger::Entry> {
    knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .read()
        .expect("read ledger")
        .entries
}

/// How many visible commits carry the release's change id: one, unless a
/// write left the commit it rewrote visible beside it.
fn commits_of_the_release_change(lab: &Lab) -> usize {
    let change = lab.revision(&lab.work, RELEASE, "change_id");
    lab.revision(&lab.work, "all()", "change_id ++ \"\\n\"")
        .lines()
        .filter(|line| *line == change)
        .count()
}

/// A first cut of split-from-store and sami11, unioned: the start of every
/// edit below.
fn unioned_first_cut() -> (Lab, tempfile::TempDir) {
    let lab = ygo_lab();
    member(&lab, SPLIT, &logs(SPLIT_LOG, SPLIT_NOTES));
    member(&lab, SAMI11, &logs(SAMI11_LOG, SAMI11_NOTES));
    let (home, _consumer) = release_test_home(&lab);
    release_ok(&lab, &home, &["cut", RELEASE]);
    assert_eq!(
        conflicts(&lab),
        Vec::<String>::new(),
        "the cut left conflicts"
    );
    (lab, home)
}

#[test]
fn a_first_cut_of_two_members_writes_both_release_logs_as_their_union() {
    // Given: two ygo members forked from the series base, each inserting its
    // own block at the top of both release logs.
    let lab = ygo_lab();
    member(&lab, SPLIT, &logs(SPLIT_LOG, SPLIT_NOTES));
    member(&lab, SAMI11, &logs(SAMI11_LOG, SAMI11_NOTES));
    let (home, _consumer) = release_test_home(&lab);

    // When: the first cut composes them.
    let stdout = release_ok(&lab, &home, &["cut", RELEASE]);

    // Then: the cut is written with no conflict, both files are the union in
    // the parent order (split-from-store's bookmark sorts first), and the
    // content audit reads neither member as missing or carried forward.
    assert!(stdout.contains(&format!("cut {RELEASE} as")), "{stdout}");
    assert_eq!(conflicts(&lab), Vec::<String>::new(), "{stdout}");
    assert_release_file(&lab, "CHANGELOG.md", UNION_TWO_LOG);
    assert_release_file(&lab, "RELEASE_NOTES.md", UNION_TWO_NOTES);
    assert!(!stdout.contains("missing or diverges"), "{stdout}");
    assert!(!stdout.contains("carried forward"), "{stdout}");

    // And: the output names each unioned file with its blocks' first lines,
    // the two blocks that claim one version as two lines, and says nothing
    // else is conflicted.
    assert!(
        stdout.contains(
            "    CHANGELOG.md\n      ## [1.55.1-sami.11] — 2026-11-13\n      \
             ## [1.55.1-sami.11] — 2026-11-12\n"
        ),
        "{stdout}"
    );
    assert!(
        stdout
            .contains("    RELEASE_NOTES.md\n      ## v1.55.1-sami.11\n      ## v1.55.1-sami.11\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  no other conflicts in this release"),
        "{stdout}"
    );

    // And: the cut event names the union after what its readers parse, and
    // the audit holds the release to that write: no drift.
    let entries = ledger_entries(&home);
    let event = entries
        .iter()
        .rev()
        .find(|entry| entry.text.starts_with(&format!("cut {RELEASE} as ")))
        .unwrap_or_else(|| panic!("no cut event: {entries:?}"));
    assert!(
        event
            .text
            .ends_with("; union: CHANGELOG.md, RELEASE_NOTES.md"),
        "{}",
        event.text
    );
    let report = audit_report(&lab, &home);
    assert!(findings(&report, "release-drift").is_empty(), "{report}");
}

#[test]
fn each_included_member_lands_its_block_on_top_of_the_union() {
    // Given: a release cut from sami11 alone.
    let lab = ygo_lab();
    member(&lab, SAMI11, &logs(SAMI11_LOG, SAMI11_NOTES));
    let (home, _consumer) = release_test_home(&lab);
    release_ok(&lab, &home, &["cut", RELEASE]);

    // When: split-from-store is included.
    member(&lab, SPLIT, &logs(SPLIT_LOG, SPLIT_NOTES));
    let included = release_ok(&lab, &home, &["include", SPLIT]);

    // Then: no conflict, and its block sits above the one the release held.
    assert_eq!(conflicts(&lab), Vec::<String>::new(), "{included}");
    assert_release_file(&lab, "CHANGELOG.md", UNION_TWO_LOG);
    assert_release_file(&lab, "RELEASE_NOTES.md", UNION_TWO_NOTES);
    assert!(
        included.contains("  2 release log(s) written as the union of their sides"),
        "{included}"
    );

    // And: the edit event ends with the union, and it is still the write
    // the ledger records for the release, at the commit the release holds.
    let entries = ledger_entries(&home);
    let event = entries
        .iter()
        .rev()
        .find(|entry| entry.text.starts_with(&format!("edited {RELEASE} as ")))
        .unwrap_or_else(|| panic!("no edit event: {entries:?}"));
    assert!(
        event
            .text
            .ends_with("; union: CHANGELOG.md, RELEASE_NOTES.md"),
        "{}",
        event.text
    );
    let newest = knives::release_model::recorded_writes(&entries, RELEASE)
        .next()
        .expect("a recorded write");
    assert_eq!(newest.commit, commit_at(&lab, RELEASE));

    // And: writing the union into the duplicate left one commit of the change.
    assert_eq!(commits_of_the_release_change(&lab), 1);

    // When: client-count is included too.
    member(&lab, CLIENT, &logs(CLIENT_LOG, CLIENT_NOTES));
    let included = release_ok(&lab, &home, &["include", CLIENT]);

    // Then: no conflict, and the newest member is on top of all three.
    assert_eq!(conflicts(&lab), Vec::<String>::new(), "{included}");
    assert_release_file(&lab, "CHANGELOG.md", UNION_THREE_LOG);
    assert_release_file(&lab, "RELEASE_NOTES.md", UNION_THREE_NOTES);
    assert_eq!(commits_of_the_release_change(&lab), 1);

    // And: the audit reads each write as knives' own and finds no stray head.
    let report = audit_report(&lab, &home);
    assert!(findings(&report, "release-drift").is_empty(), "{report}");
    assert!(findings(&report, "orphan-commit").is_empty(), "{report}");
}

#[test]
fn an_advanced_member_with_a_new_section_above_its_own_is_unioned_on_top() {
    // Given: the unioned cut of split-from-store and sami11, then sami11
    // grown by a commit that inserts a new section above its own block.
    let (lab, home) = unioned_first_cut();
    let section_log = "## [1.55.1-sami.12] — 2026-11-15\n\nAdds a section above sami11's own.\n\n";
    let section_notes = "## v1.55.1-sami.12\n\nThis release adds a section above sami11's own.\n\n";
    let grown_log = above_first_section(SAMI11_LOG, section_log);
    let grown_notes = above_first_section(SAMI11_NOTES, section_notes);
    grow(&lab, SAMI11, &logs(&grown_log, &grown_notes));

    // When: the release advances sami11.
    let advanced = release_ok(&lab, &home, &["advance", SAMI11]);

    // Then: no conflict, and the advanced side's new section is on top of
    // the union the release held.
    assert_eq!(conflicts(&lab), Vec::<String>::new(), "{advanced}");
    assert_release_file(
        &lab,
        "CHANGELOG.md",
        &above_first_section(UNION_TWO_LOG, section_log),
    );
    assert_release_file(
        &lab,
        "RELEASE_NOTES.md",
        &above_first_section(UNION_TWO_NOTES, section_notes),
    );
    assert!(
        advanced.contains("      ## [1.55.1-sami.12] — 2026-11-15\n"),
        "{advanced}"
    );
}

#[test]
fn an_advance_to_a_member_that_rewrote_its_block_stays_conflicted() {
    // Given: the ygo history's shape: the unioned cut of split-from-store and
    // sami11, client-count included on top of it, so its block sits right
    // above split-from-store's heading; then split-from-store grown into
    // split-one-path, which rewrote the block it had published, heading
    // included.
    let (lab, home) = unioned_first_cut();
    member(&lab, CLIENT, &logs(CLIENT_LOG, CLIENT_NOTES));
    release_ok(&lab, &home, &["include", CLIENT]);
    grow(&lab, SPLIT, &logs(SPLIT_ONE_PATH_LOG, SPLIT_ONE_PATH_NOTES));

    // When: the release advances it.
    let advanced = release_ok(&lab, &home, &["advance", SPLIT]);

    // Then: the changelog stays conflicted and is named as a person's, and
    // nothing is reported as unioned. The notes are jj's own merge here, not
    // a union: every member's notes open with the same `## v1.55.1-sami.11`
    // line, so jj matches the rewritten heading against client-count's and
    // resolves the file itself.
    assert_eq!(conflicts(&lab), ["CHANGELOG.md"], "{advanced}");
    assert!(
        advanced.contains("  1 conflicted file(s) left for a person:\n    CHANGELOG.md\n"),
        "{advanced}"
    );
    assert!(!advanced.contains("written as the union"), "{advanced}");
}

#[test]
fn sides_on_different_bases_stay_conflicted() {
    // Given: oh-my-pi's shape: the trunk at the member's fork point, one
    // member inserting its block there, the other holding the `sami`
    // release's file, an insertion over a later base upstream moved lines in.
    let lab = trunk_with(&[(OMP_PATH, OMP_FORK_POINT)]);
    member(&lab, "feat/member", &[(OMP_PATH, OMP_MEMBER)]);
    member(&lab, "feat/sami", &[(OMP_PATH, OMP_RELEASE)]);
    let (home, _consumer) = release_test_home(&lab);

    // When: the first cut composes them.
    let output = knives_release(&lab, &home, &["cut", RELEASE]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Then: the cut is written with the changelog conflicted and named as a
    // person's; nothing is reported as unioned.
    assert!(stdout.contains(&format!("cut {RELEASE} as")), "{stdout}");
    assert_eq!(conflicts(&lab), [OMP_PATH], "{stdout}");
    assert!(
        stdout.contains(&format!(
            "  1 conflicted file(s) left for a person:\n    {OMP_PATH}\n"
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("written as the union"), "{stdout}");
}

#[test]
fn a_code_file_both_members_change_stays_conflicted_beside_the_union() {
    // Given: two ygo members that each also write shared.txt their own way.
    let lab = ygo_lab();
    let mut split = logs(SPLIT_LOG, SPLIT_NOTES).to_vec();
    split.push(("shared.txt", "split\n"));
    let mut sami11 = logs(SAMI11_LOG, SAMI11_NOTES).to_vec();
    sami11.push(("shared.txt", "sami11\n"));
    member(&lab, SPLIT, &split);
    member(&lab, SAMI11, &sami11);
    let (home, _consumer) = release_test_home(&lab);

    // When: the first cut composes them.
    let output = knives_release(&lab, &home, &["cut", RELEASE]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Then: both logs are unioned and named so, and the code file alone
    // stays conflicted, named as a person's.
    assert!(stdout.contains(&format!("cut {RELEASE} as")), "{stdout}");
    assert_eq!(conflicts(&lab), ["shared.txt"], "{stdout}");
    assert_release_file(&lab, "CHANGELOG.md", UNION_TWO_LOG);
    assert_release_file(&lab, "RELEASE_NOTES.md", UNION_TWO_NOTES);
    assert!(
        stdout.contains("  2 release log(s) written as the union of their sides"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  1 conflicted file(s) left for a person:\n    shared.txt\n"),
        "{stdout}"
    );
}

/// Each of `lines` is a line of its own in the release's `held` changelog,
/// not part of a longer line.
fn assert_whole_lines(held: &str, lines: &[&str], output: &str) {
    for line in lines {
        assert!(
            held.lines().any(|held_line| held_line == *line),
            "{line:?} is not a line of its own in CHANGELOG.md:\n{held:?}\n{output}"
        );
    }
}

// A trunk changelog, and two members that each append a block at its end
// with no newline after the block's last line.
const NO_FINAL_NEWLINE_TRUNK: &str = "# Changelog\n";
const NO_FINAL_NEWLINE_A: &str = "# Changelog\n## a\n- a";
const NO_FINAL_NEWLINE_B: &str = "# Changelog\n## b\n- b";
/// Every line the trunk and the two members wrote.
const NO_FINAL_NEWLINE_LINES: [&str; 5] = ["# Changelog", "## a", "- a", "## b", "- b"];

#[test]
fn an_include_never_runs_a_block_with_no_final_newline_into_the_next() {
    // Given: a release of one member that appended its block at the end of
    // the changelog with no final newline, and a second member that appended
    // its own block the same way.
    let lab = trunk_with(&[("CHANGELOG.md", NO_FINAL_NEWLINE_TRUNK)]);
    member(&lab, "feat/a", &[("CHANGELOG.md", NO_FINAL_NEWLINE_A)]);
    let (home, _consumer) = release_test_home(&lab);
    release_ok(&lab, &home, &["cut", RELEASE]);
    member(&lab, "feat/b", &[("CHANGELOG.md", NO_FINAL_NEWLINE_B)]);

    // When: the second member is included.
    let included = release_ok(&lab, &home, &["include", "feat/b"]);

    // Then: the changelog is left to a person, or every line each member
    // wrote is a line of its own in the release: the last line of one block
    // and the first line of the next are never written as one line.
    if conflicts(&lab).is_empty() {
        let held = file_at_revision(&lab, RELEASE, "CHANGELOG.md");
        assert_whole_lines(&held, &NO_FINAL_NEWLINE_LINES, &included);
    } else {
        assert_eq!(conflicts(&lab), ["CHANGELOG.md"], "{included}");
    }
}

#[test]
fn a_first_cut_of_blocks_with_no_final_newline_is_written() {
    // Given: two members that each appended a block at the end of the
    // changelog with no final newline.
    let lab = trunk_with(&[("CHANGELOG.md", NO_FINAL_NEWLINE_TRUNK)]);
    member(&lab, "feat/a", &[("CHANGELOG.md", NO_FINAL_NEWLINE_A)]);
    member(&lab, "feat/b", &[("CHANGELOG.md", NO_FINAL_NEWLINE_B)]);
    let (home, _consumer) = release_test_home(&lab);

    // When: the first cut composes them.
    let output = knives_release(&lab, &home, &["cut", RELEASE]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Then: the cut is written, as knives 3.2.6 writes it, and the changelog
    // is left to a person or holds every line each member wrote as a line of
    // its own.
    assert!(
        stdout.contains(&format!("cut {RELEASE} as")),
        "the cut was not written:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    if conflicts(&lab).is_empty() {
        let held = file_at_revision(&lab, RELEASE, "CHANGELOG.md");
        assert_whole_lines(&held, &NO_FINAL_NEWLINE_LINES, &stdout);
    } else {
        assert_eq!(conflicts(&lab), ["CHANGELOG.md"], "{stdout}");
    }
}

#[test]
fn dropping_a_member_whose_block_a_union_holds_unions_nothing_away() {
    // Given: the unioned cut of split-from-store and sami11.
    let (lab, home) = unioned_first_cut();

    // When: split-from-store is dropped.
    let dropped = release_ok(&lab, &home, &["drop", SPLIT, "--why", "not ready"]);

    // Then: taking its block out of the union is no insertion, so knives
    // writes no union over it: both logs are left conflicted and named as a
    // person's, who decides what leaves with the member.
    assert!(
        !dropped.contains("written as the union"),
        "a drop must not union a member's block away: {dropped}"
    );
    assert_eq!(
        conflicts(&lab),
        ["CHANGELOG.md", "RELEASE_NOTES.md"],
        "{dropped}"
    );
    assert!(
        dropped.contains(
            "  2 conflicted file(s) left for a person:\n    CHANGELOG.md\n    RELEASE_NOTES.md\n"
        ),
        "{dropped}"
    );
}

#[test]
fn a_recut_of_a_unioned_release_carries_nothing_forward() {
    // Given: the unioned cut of split-from-store and sami11.
    let (lab, home) = unioned_first_cut();

    // When: the same composition is cut under a new name.
    let stdout = release_ok(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it is written, and the content audit reads both members as held:
    // no row calls either a recorded resolution carried forward.
    assert!(stdout.contains("cut release/2026-08-05 as"), "{stdout}");
    assert!(!stdout.contains("carried forward"), "{stdout}");
    assert!(!stdout.contains("missing or diverges"), "{stdout}");
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-05", "CHANGELOG.md"),
        UNION_TWO_LOG
    );
}

#[test]
fn members_verify_reads_a_unioned_release_as_carrying_every_member() {
    // Given: the unioned cut of split-from-store and sami11.
    let (lab, home) = unioned_first_cut();

    // When: each member's content is replayed against the release.
    let stdout = release_ok(&lab, &home, &["members", "--verify"]);

    // Then: neither member is missing; both replay empty.
    assert!(!stdout.contains("!!"), "{stdout}");
    assert_eq!(
        stdout.matches(": carried (replay empty)").count(),
        2,
        "{stdout}"
    );
}

/// The edit event the rebase recorded for the release.
fn rebase_event(home: &tempfile::TempDir) -> knives::ledger::Entry {
    let entries = ledger_entries(home);
    entries
        .iter()
        .rev()
        .find(|entry| {
            entry.text.starts_with(&format!("edited {RELEASE} as "))
                && entry.text.contains("rebased onto main@upstream")
        })
        .cloned()
        .unwrap_or_else(|| panic!("no rebase event: {entries:?}"))
}

#[test]
fn a_rebase_unions_the_logs_it_leaves_conflicted_as_it_records_the_release() {
    // Given: a release merged by hand from split-from-store and sami11, its
    // logs left conflicted as an older knives left them, and the upstream
    // trunk advanced past the members' base.
    let lab = ygo_lab();
    member(&lab, SPLIT, &logs(SPLIT_LOG, SPLIT_NOTES));
    member(&lab, SAMI11, &logs(SAMI11_LOG, SAMI11_NOTES));
    lab.jj_work(["new", "-r", SPLIT, "-r", SAMI11, "-m", RELEASE]);
    lab.jj_work(["bookmark", "create", RELEASE, "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);
    assert_eq!(conflicts(&lab), ["CHANGELOG.md", "RELEASE_NOTES.md"]);
    lab.advance_upstream("upstream advance\n");

    // When: the release is rebased onto the advanced trunk.
    let stdout = release_ok(&lab, &home, &["rebase", "main@upstream"]);

    // Then: the commit that records the rebased provenance carries the union
    // of both logs, said once, and its event names the union, then the mark
    // of a write over a release the ledger records no write of.
    assert_eq!(conflicts(&lab), Vec::<String>::new(), "{stdout}");
    assert_release_file(&lab, "CHANGELOG.md", UNION_TWO_LOG);
    assert_release_file(&lab, "RELEASE_NOTES.md", UNION_TWO_NOTES);
    assert_eq!(
        stdout
            .matches("  2 release log(s) written as the union of their sides")
            .count(),
        1,
        "{stdout}"
    );
    let event = rebase_event(&home);
    assert_eq!(
        event.evidence.first().map(String::as_str),
        Some(commit_at(&lab, RELEASE).as_str()),
        "{}",
        event.text
    );
    assert!(
        event
            .text
            .ends_with("; union: CHANGELOG.md, RELEASE_NOTES.md; unchecked: no record"),
        "{}",
        event.text
    );
    assert_eq!(commits_of_the_release_change(&lab), 1);
}

#[test]
fn a_rebase_that_sheds_a_stale_base_says_once_that_the_shed_write_unioned() {
    // Given: a release merged by hand over the trunk and both members — a
    // legacy base parent a rebase sheds — its logs left conflicted, and the
    // upstream trunk advanced.
    let lab = ygo_lab();
    member(&lab, SPLIT, &logs(SPLIT_LOG, SPLIT_NOTES));
    member(&lab, SAMI11, &logs(SAMI11_LOG, SAMI11_NOTES));
    lab.octopus(RELEASE, SPLIT, SAMI11);
    let (home, _consumer) = release_test_home(&lab);
    lab.advance_upstream("upstream advance\n");

    // When: the release is rebased onto the advanced trunk.
    let stdout = release_ok(&lab, &home, &["rebase", "main@upstream"]);

    // Then: the write that shed the base unioned both logs, the rebase moved
    // that union along, and the rebase's report and event say so once; the
    // event is marked, as no write of the release was recorded.
    assert!(stdout.contains("1 stale base parent(s) shed"), "{stdout}");
    assert_eq!(conflicts(&lab), Vec::<String>::new(), "{stdout}");
    assert_release_file(&lab, "CHANGELOG.md", UNION_TWO_LOG);
    assert_release_file(&lab, "RELEASE_NOTES.md", UNION_TWO_NOTES);
    assert_eq!(
        stdout
            .matches("  2 release log(s) written as the union of their sides")
            .count(),
        1,
        "{stdout}"
    );
    let event = rebase_event(&home);
    assert!(
        event
            .text
            .ends_with("; union: CHANGELOG.md, RELEASE_NOTES.md; unchecked: no record"),
        "{stdout}\n{}",
        event.text
    );
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
fn an_include_puts_a_members_entries_under_a_heading_the_release_already_has_in_that_one_list() {
    // Given: a trunk whose changelog's `[Unreleased]` section is empty; a
    // release cut from a member that adds an entry under `### Fixed` there;
    // and a second member that adds two entries under `### Fixed` there.
    let (trunk, alpha, beta) = (
        with_unreleased(""),
        with_unreleased("### Fixed\n\n- Alpha's fix.\n\n"),
        with_unreleased("### Fixed\n\n- Beta's first fix.\n- Beta's second fix.\n\n"),
    );
    let lab = trunk_with(&[("CHANGELOG.md", trunk.as_str())]);
    member(&lab, "feat/alpha", &[("CHANGELOG.md", alpha.as_str())]);
    let (home, _consumer) = release_test_home(&lab);
    release_ok(&lab, &home, &["cut", RELEASE]);
    member(&lab, "feat/beta", &[("CHANGELOG.md", beta.as_str())]);

    // When: the second member is included.
    let included = release_ok(&lab, &home, &["include", "feat/beta"]);

    // Then: no conflict is left, and `[Unreleased]` holds `### Fixed` once,
    // with all three entries in its one list, the member's two above the
    // entry the release held, each line as its member wrote it.
    assert_eq!(conflicts(&lab), Vec::<String>::new(), "{included}");
    assert_release_file(
        &lab,
        "CHANGELOG.md",
        &with_unreleased(
            "### Fixed\n\n- Beta's first fix.\n- Beta's second fix.\n- Alpha's fix.\n\n",
        ),
    );

    // And: the content check reads both members as carried.
    let verified = release_ok(&lab, &home, &["members", "--verify"]);
    assert!(!verified.contains("!!"), "{verified}");
    assert_eq!(
        verified.matches(": carried (replay empty)").count(),
        2,
        "{verified}"
    );
}

#[test]
fn a_first_cut_of_two_members_puts_their_entries_under_one_heading() {
    // Given: a trunk whose changelog's `[Unreleased]` section is empty, and
    // two members that each add one entry under `### Fixed` there.
    let (trunk, alpha, beta) = (
        with_unreleased(""),
        with_unreleased("### Fixed\n\n- Alpha's fix.\n\n"),
        with_unreleased("### Fixed\n\n- Beta's fix.\n\n"),
    );
    let lab = trunk_with(&[("CHANGELOG.md", trunk.as_str())]);
    member(&lab, "feat/alpha", &[("CHANGELOG.md", alpha.as_str())]);
    member(&lab, "feat/beta", &[("CHANGELOG.md", beta.as_str())]);
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is cut of both at once.
    let cut = release_ok(&lab, &home, &["cut", RELEASE]);

    // Then: no conflict is left, and `[Unreleased]` holds `### Fixed` once,
    // with both members' entries in its one list, each line unchanged. The
    // order of the two entries is not part of the rule.
    assert_eq!(conflicts(&lab), Vec::<String>::new(), "{cut}");
    let log = file_at_revision(&lab, RELEASE, "CHANGELOG.md");
    let unreleased = log
        .split_once("## [Unreleased]\n")
        .and_then(|(_, rest)| rest.split_once("## [1.0.0]"))
        .map(|(section, _)| section)
        .expect("an [Unreleased] section before [1.0.0]");
    assert_eq!(unreleased.matches("### Fixed\n").count(), 1, "{log}");
    let list = unreleased
        .split_once("### Fixed\n\n")
        .map(|(_, rest)| rest.split("\n\n").next().unwrap_or(""))
        .expect("a ### Fixed list");
    let mut entries: Vec<&str> = list.lines().collect();
    entries.sort_unstable();
    assert_eq!(entries, ["- Alpha's fix.", "- Beta's fix."], "{log}");
}

#[test]
fn a_first_cut_of_three_members_puts_two_members_entries_under_one_heading() {
    // Given: a trunk whose changelog's `[Unreleased]` section is empty;
    // `feat/alpha` and `feat/beta` each add one entry under `### Fixed`
    // there, and `feat/gamma` one under `### Added`.
    let (trunk, alpha, beta, gamma) = (
        with_unreleased(""),
        with_unreleased("### Fixed\n\n- Alpha's fix.\n\n"),
        with_unreleased("### Fixed\n\n- Beta's fix.\n\n"),
        with_unreleased("### Added\n\n- Gamma's addition.\n\n"),
    );
    let lab = trunk_with(&[("CHANGELOG.md", trunk.as_str())]);
    member(&lab, "feat/alpha", &[("CHANGELOG.md", alpha.as_str())]);
    member(&lab, "feat/beta", &[("CHANGELOG.md", beta.as_str())]);
    member(&lab, "feat/gamma", &[("CHANGELOG.md", gamma.as_str())]);
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is cut of all three at once.
    let cut = release_ok(&lab, &home, &["cut", RELEASE]);

    // Then: no conflict is left, and `[Unreleased]` holds `### Fixed` once,
    // with alpha's and beta's entries in its one list, and `### Added` once,
    // with gamma's, each line unchanged. The order is not part of the rule.
    assert_eq!(conflicts(&lab), Vec::<String>::new(), "{cut}");
    let log = file_at_revision(&lab, RELEASE, "CHANGELOG.md");
    let unreleased = log
        .split_once("## [Unreleased]\n")
        .and_then(|(_, rest)| rest.split_once("## [1.0.0]"))
        .map(|(section, _)| section)
        .expect("an [Unreleased] section before [1.0.0]");
    assert_eq!(unreleased.matches("### Fixed\n").count(), 1, "{log}");
    assert_eq!(unreleased.matches("### Added\n").count(), 1, "{log}");
    let list = |heading: &str| {
        let mut entries: Vec<&str> = unreleased
            .split_once(&format!("{heading}\n\n"))
            .map_or("", |(_, rest)| rest.split("\n\n").next().unwrap_or(""))
            .lines()
            .collect();
        entries.sort_unstable();
        entries
    };
    assert_eq!(
        list("### Fixed"),
        ["- Alpha's fix.", "- Beta's fix."],
        "{log}"
    );
    assert_eq!(list("### Added"), ["- Gamma's addition."], "{log}");
}

#[test]
fn a_first_cut_writes_lists_beside_a_code_fence_whole_and_names_the_heading_left_twice() {
    // Given: a trunk whose changelog's `[Unreleased]` section is empty; a
    // member that adds a `### Fixed` entry with a fenced TOML sample whose
    // first line is a `#` comment; and a member that adds a `### Fixed`
    // entry of its own.
    let fenced = "### Fixed\n\n- Alpha's fix, with a new key:\n\n```toml\n# the new key\nkey = 1\n\
                  ```\n\n";
    let plain = "### Fixed\n\n- Beta's fix.\n\n";
    let (trunk, alpha, beta) = (
        with_unreleased(""),
        with_unreleased(fenced),
        with_unreleased(plain),
    );
    let lab = trunk_with(&[("CHANGELOG.md", trunk.as_str())]);
    member(&lab, "feat/alpha", &[("CHANGELOG.md", alpha.as_str())]);
    member(&lab, "feat/beta", &[("CHANGELOG.md", beta.as_str())]);
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is cut of both at once.
    let cut = release_ok(&lab, &home, &["cut", RELEASE]);

    // Then: no conflict is left, and the changelog is the union of the two
    // blocks, each whole and byte for byte, in the parent order
    // (`feat/alpha` sorts first): a block there holds a code fence, so
    // knives joins nothing.
    assert_eq!(conflicts(&lab), Vec::<String>::new(), "{cut}");
    assert_release_file(
        &lab,
        "CHANGELOG.md",
        &with_unreleased(&format!("{fenced}{plain}")),
    );

    // And: the output names `### Fixed` as a heading left twice there.
    assert!(cut.contains(LEFT_FIXED_TWICE), "{cut}");
}

/// The line a write prints for a `CHANGELOG.md` whose union it wrote whole
/// and which it left with `### Fixed` twice in one section.
const LEFT_FIXED_TWICE: &str = "\n  CHANGELOG.md: left `### Fixed` twice: knives joins only plain \
                                `### ` lists in a file's one [Unreleased] section; join them by \
                                hand\n";

/// A changelog whose `[Unreleased]` section holds `unreleased`, above a
/// published release that lists `- Bump dependencies.` under `### Changed`.
fn with_bumped_release(unreleased: &str) -> String {
    format!(
        "# Changelog\n\n## [Unreleased]\n\n{unreleased}## [1.0.0] - 2026-08-01\n\n### Changed\n\n\
         - Bump dependencies.\n- Other.\n"
    )
}

#[test]
fn members_verify_reads_an_entry_unreleased_lost_as_missing_where_an_older_section_lists_its_line()
{
    // Given: a trunk whose changelog's `[1.0.0]` section lists
    // `- Bump dependencies.` under `### Changed`; `feat/alpha` adds a
    // `### Fixed` entry to `[Unreleased]`, and `feat/beta` adds that same
    // `- Bump dependencies.` line under `### Changed` there; a first cut of
    // both; then a hand edit of the release that drops beta's list from
    // `[Unreleased]`.
    let (trunk, alpha, beta) = (
        with_bumped_release(""),
        with_bumped_release("### Fixed\n\n- Alpha's fix.\n\n"),
        with_bumped_release("### Changed\n\n- Bump dependencies.\n\n"),
    );
    let lab = trunk_with(&[("CHANGELOG.md", trunk.as_str())]);
    member(&lab, "feat/alpha", &[("CHANGELOG.md", alpha.as_str())]);
    member(&lab, "feat/beta", &[("CHANGELOG.md", beta.as_str())]);
    let (home, _consumer) = release_test_home(&lab);
    let cut = release_ok(&lab, &home, &["cut", RELEASE]);
    assert_eq!(conflicts(&lab), Vec::<String>::new(), "{cut}");
    let dropped = commit_at(&lab, "feat/beta");
    lab.jj_work(["edit", RELEASE]);
    write_files(&lab, &[("CHANGELOG.md", alpha.as_str())]);
    lab.jj_work(["new"]);

    // When: each member's content is replayed against the release.
    let output = knives_release(&lab, &home, &["members", "--verify"]);

    // Then: beta is a finding, named as missing, although `[1.0.0]` lists
    // its line under the same heading; alpha, whose entry the release holds,
    // is carried.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "  !! feat/beta@{}: the cut tree is missing or diverges from the member's content",
            dropped.as_str().chars().take(12).collect::<String>()
        )),
        "{stdout}"
    );
    assert_eq!(
        stdout.matches(": carried (replay empty)").count(),
        1,
        "{stdout}"
    );
}

#[test]
fn a_first_cut_writes_lists_whole_where_the_changelog_holds_a_second_unreleased_heading() {
    // Given: a trunk whose changelog holds a stray second `## [Unreleased]`
    // heading below its `[1.0.0]` section, and two members that each add one
    // entry under `### Fixed` in the first `[Unreleased]` section.
    let stray = |unreleased: &str| with_unreleased(unreleased) + "\n## [Unreleased]\n\n- Stale.\n";
    let (alpha_list, beta_list) = (
        "### Fixed\n\n- Alpha's fix.\n\n",
        "### Fixed\n\n- Beta's fix.\n\n",
    );
    let (trunk, alpha, beta) = (stray(""), stray(alpha_list), stray(beta_list));
    let lab = trunk_with(&[("CHANGELOG.md", trunk.as_str())]);
    member(&lab, "feat/alpha", &[("CHANGELOG.md", alpha.as_str())]);
    member(&lab, "feat/beta", &[("CHANGELOG.md", beta.as_str())]);
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is cut of both at once.
    let cut = release_ok(&lab, &home, &["cut", RELEASE]);

    // Then: no conflict is left, and the changelog is the union of the two
    // blocks, each whole and byte for byte, in the parent order: a file with
    // two `## [Unreleased]` headings gets no join, and the output names
    // `### Fixed` as a heading left twice.
    assert_eq!(conflicts(&lab), Vec::<String>::new(), "{cut}");
    assert_release_file(
        &lab,
        "CHANGELOG.md",
        &stray(&format!("{alpha_list}{beta_list}")),
    );
    assert!(cut.contains(LEFT_FIXED_TWICE), "{cut}");

    // And: the cut's audit and `members --verify` read both members as
    // carried.
    assert!(!cut.contains("missing or diverges"), "{cut}");
    let verified = release_ok(&lab, &home, &["members", "--verify"]);
    assert!(!verified.contains("!!"), "{verified}");
    assert_eq!(
        verified.matches(": carried (replay empty)").count(),
        2,
        "{verified}"
    );
}

/// A changelog whose `[Unreleased]` section holds `unreleased`, then the
/// trunk's `### Added` list and a `### Fixed` list with `fix`, above one
/// published release.
fn with_trunk_lists(unreleased: &str, fix: &str) -> String {
    format!(
        "# Changelog\n\n## [Unreleased]\n\n{unreleased}### Added\n\n- The trunk's addition.\n\n\
         ### Fixed\n\n- {fix}\n\n## [1.0.0] - 2026-08-01\n\n### Added\n\n- The first release.\n"
    )
}

/// A member's own new `### Added` block at the top of `[Unreleased]`, above
/// the `### Added` list the release already has.
const NEW_ADDED: &str = "### Added\n\n- The member's addition.\n\n";

/// The line a release command prints for a `CHANGELOG.md` whose one
/// `[Unreleased]` section holds `### Added` twice.
const ADDED_TWICE: &str = "\n  CHANGELOG.md: [Unreleased] holds `### Added` twice; knives changed no \
                           line for it; join them by hand before you push\n";

#[test]
fn an_include_whose_clean_merge_leaves_a_heading_twice_in_unreleased_names_it_and_changes_nothing()
{
    // Given: a trunk whose changelog's `[Unreleased]` section holds a
    // `### Added` list; a release cut of `feat/alpha`, which changes no
    // release log; and `feat/beta`, whose own changelog adds a new
    // `### Added` block above that list, as at the Oh My Pi release commit
    // b40fd3c3.
    let (trunk, beta) = (
        with_trunk_lists("", "The trunk's fix."),
        with_trunk_lists(NEW_ADDED, "The trunk's fix."),
    );
    let lab = trunk_with(&[("CHANGELOG.md", trunk.as_str())]);
    member(&lab, "feat/alpha", &[("alpha.txt", "alpha\n")]);
    let (home, _consumer) = release_test_home(&lab);
    release_ok(&lab, &home, &["cut", RELEASE]);
    member(&lab, "feat/beta", &[("CHANGELOG.md", beta.as_str())]);

    // When: `feat/beta` is included.
    let included = release_ok(&lab, &home, &["include", "feat/beta"]);

    // Then: the merge is clean and the release's changelog is the bytes it
    // wrote, the member's file, so `[Unreleased]` holds `### Added` twice.
    assert_eq!(conflicts(&lab), Vec::<String>::new(), "{included}");
    assert!(!included.contains("written as the union"), "{included}");
    assert_release_file(&lab, "CHANGELOG.md", &beta);

    // And: one line names the file and that heading.
    assert!(included.contains(ADDED_TWICE), "{included}");
    assert_eq!(
        included.matches("[Unreleased] holds").count(),
        1,
        "{included}"
    );
}

#[test]
fn an_advance_that_leaves_the_changelog_conflicted_names_no_heading_and_the_adopt_names_it() {
    // Given: a trunk whose changelog's `[Unreleased]` section holds a
    // `### Added` and a `### Fixed` list; a release cut of `feat/alpha`,
    // which changes no release log, and `feat/beta`, which rewords the
    // trunk's fix; then `feat/alpha` grown by a commit whose changelog adds a
    // new `### Added` block at the top of `[Unreleased]` and rewords that fix
    // its own way, as at the Oh My Pi release commit b40fd3c3.
    let (trunk, beta, grown) = (
        with_trunk_lists("", "The trunk's fix."),
        with_trunk_lists("", "The trunk's fix, as beta words it."),
        with_trunk_lists(NEW_ADDED, "The trunk's fix, as alpha words it."),
    );
    let lab = trunk_with(&[("CHANGELOG.md", trunk.as_str())]);
    member(&lab, "feat/alpha", &[("alpha.txt", "alpha\n")]);
    member(&lab, "feat/beta", &[("CHANGELOG.md", beta.as_str())]);
    let (home, _consumer) = release_test_home(&lab);
    release_ok(&lab, &home, &["cut", RELEASE]);
    grow(&lab, "feat/alpha", &[("CHANGELOG.md", grown.as_str())]);

    // When: the release advances `feat/alpha`.
    let advanced = release_ok(&lab, &home, &["advance", "feat/alpha"]);

    // Then: the new block merges cleanly, but the two rewordings conflict,
    // so the changelog is left conflicted, named as a person's, and no line
    // names a heading in it.
    assert_eq!(conflicts(&lab), ["CHANGELOG.md"], "{advanced}");
    assert!(
        advanced.contains("  1 conflicted file(s) left for a person:\n    CHANGELOG.md\n"),
        "{advanced}"
    );
    assert!(!advanced.contains("[Unreleased] holds"), "{advanced}");

    // When: a person resolves the conflict in place, keeping both
    // `### Added` headings and both rewordings, and adopts the release.
    let resolved = with_trunk_lists(
        NEW_ADDED,
        "The trunk's fix, as alpha words it.\n- The trunk's fix, as beta words it.",
    );
    lab.jj_work(["edit", RELEASE]);
    write_files(&lab, &[("CHANGELOG.md", resolved.as_str())]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let adopted = release_ok(&lab, &home, &["adopt", RELEASE]);

    // Then: one line names the file and `### Added`, and the changelog is
    // the bytes the person wrote.
    assert!(adopted.contains(ADDED_TWICE), "{adopted}");
    assert_eq!(
        adopted.matches("[Unreleased] holds").count(),
        1,
        "{adopted}"
    );
    assert_eq!(conflicts(&lab), Vec::<String>::new(), "{adopted}");
    assert_release_file(&lab, "CHANGELOG.md", &resolved);
}
