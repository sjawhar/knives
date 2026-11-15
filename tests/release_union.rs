//! The release log union every release write writes itself.
//!
//! Every member of a fork release inserts its own block at the top of
//! `CHANGELOG.md` and `RELEASE_NOTES.md`, so any composition of two members
//! conflicts in both files although no side touched another's text. A cut, an
//! `include`, an `advance` and a rebase write that conflict's union — every
//! block verbatim, the block the write brings in above what the release
//! already held — and the content checks read a member whose block the union
//! holds as carried. A conflict that is not every side inserting its own
//! block — a member that rewrote its block, sides on different bases, a code
//! file, a drop that takes a block out — stays conflicted and is named as a
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
    // of both logs, said once, and its event names the union.
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
            .ends_with("; union: CHANGELOG.md, RELEASE_NOTES.md"),
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
    // that union along, and the rebase's report and event say so once.
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
    assert!(
        rebase_event(&home)
            .text
            .ends_with("; union: CHANGELOG.md, RELEASE_NOTES.md"),
        "{stdout}"
    );
}
