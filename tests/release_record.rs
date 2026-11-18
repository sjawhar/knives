//! `knives release cut NAME --at REV`: a release cut outside knives recorded
//! as NAME's cut by one command, with no new commit, no temporary name and no
//! throwaway cut.
//!
//! The record is the cut event a knives cut writes, so `adopt`, the edit
//! verbs and another machine's `audit` read the release as recorded. Refused,
//! writing nothing: a conflicted commit, a commit that shares no history with
//! the trunk, one that merges no member, a name that sorts before a release
//! the series already holds or records, a change no member explains, and a
//! release made as merges on its previous release outside a stacked series.
//! A parent the trunk holds is never a member. The same commit recorded
//! again writes nothing, and records of one name run at once take turns. In
//! a stacked series a chain of merges, one member each, records its previous
//! release and the side parents on its first-parent line, and an edit of it,
//! or a member rewritten in place under it, keeps what its inner merges
//! resolved. A record is held against the newest write of the newest release
//! the ledger records, so a member that `include` added since that release's
//! cut is held too, and in a stacked series that write must be on the
//! record's first-parent line, so a commit built below it is refused, or
//! with `--allow-drop` recorded with the drop stated. A clone that does not
//! hold that write is refused, naming it and the fetch. A fix by hand is
//! held file by file against that write, and a file's hold ends where that
//! write no longer holds the adopt's content there; a clone that cannot
//! measure the fix, as where it does not hold the adopt's commit, holds each
//! named file while that write holds something at it. The adopt event lists
//! the files of its fix whole, so a path that holds a separator reads back
//! whole; an older adopt event names them in its text alone, and a clone
//! holds each path that the text may name and that write holds. A
//! published name that the publish remote lacks is not recorded until it is
//! pushed, and the refusal gives the commands. A printed command quotes the
//! name for the shell.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use std::fs::{File, TryLockError};
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use knives::ids::CommitId;
use knives::ledger::{Entry, Kind, Ledger, RecordedParent};
use lab::{Lab, commit_at, file_at_revision, knives_release, release_parents, release_test_home};

/// A release of the default series.
const RELEASE: &str = "release/2026-08-04";

/// The first cut of series `old`: the previous release of [`NEXT`].
const PREVIOUS: &str = "release/old/2026-08-04";

/// Series `old`'s next release, made by hand on [`PREVIOUS`].
const NEXT: &str = "release/old/2026-08-05";

fn text(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The fork's ledger entries, as the release homes keep them.
fn ledger_entries(home: &tempfile::TempDir) -> Vec<Entry> {
    Ledger::at(home.path().join("ledger").join("demo"))
        .read()
        .expect("read ledger")
        .entries
}

/// The newest event the ledger records about `name`.
fn newest_event(home: &tempfile::TempDir, name: &str) -> Entry {
    let entries = ledger_entries(home);
    entries
        .iter()
        .rev()
        .find(|entry| entry.kind == Kind::Event && entry.subject.as_deref() == Some(name))
        .cloned()
        .unwrap_or_else(|| panic!("no event about {name}: {entries:?}"))
}

/// Every bookmark in the work checkout, local and remote, with its commit.
fn bookmarks(lab: &Lab) -> knives::detect::BookmarkTips {
    knives::jj::Repo::open(lab.work_path())
        .expect("open the work checkout")
        .bookmark_tips()
        .expect("read bookmark tips")
}

/// Every visible commit in the work checkout.
fn visible_commits(lab: &Lab) -> Vec<CommitId> {
    let mut commits =
        knives::jj::commits_matching(lab.work_path(), "all()").expect("list visible commits");
    commits.sort();
    commits
}

/// The commit of the work checkout's `revision`, a revset that names one.
fn commit_of(lab: &Lab, revision: &str) -> CommitId {
    CommitId::new(lab.revision(lab.work_path(), revision, "commit_id").trim())
}

/// `feat/alpha` and `feat/beta` on the trunk, merged by hand as
/// [`RELEASE`] and pushed to the publish remote: a release no knives cut
/// wrote, which the ledger does not record. It is what a release is, a flat
/// merge of its members, with no trunk parent.
fn hand_cut_lab() -> (Lab, tempfile::TempDir) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work(["new", "-r", "feat/alpha", "-r", "feat/beta", "-m", RELEASE]);
    lab.jj_work(["bookmark", "create", RELEASE, "-r", "@"]);
    lab.jj_work(["new"]);
    lab.push_branch(RELEASE);
    let (home, _consumer) = release_test_home(&lab);
    (lab, home)
}

/// `knives release cut <name> --at <commit>`, whatever its exit.
fn record(lab: &Lab, home: &tempfile::TempDir, name: &str, commit: &CommitId) -> Output {
    knives_release(lab, home, &["cut", name, "--at", commit.as_str()])
}

/// [`record`] in series `old`.
fn record_in_series(lab: &Lab, home: &tempfile::TempDir, name: &str, revision: &str) -> Output {
    knives_release(
        lab,
        home,
        &["--series", "old", "cut", name, "--at", revision],
    )
}

/// The line `cut --at` prints once it recorded `name` at `commit`.
fn recorded_line(name: &str, commit: &CommitId) -> String {
    format!(
        "demo: recorded {name} at {} as its cut; nothing was committed",
        commit.short()
    )
}

#[test]
fn a_hand_cut_release_is_recorded_by_one_command() {
    // Given: a release merged by hand and pushed to the publish remote,
    // which no ledger entry records, so adopt has no write to replace.
    let (lab, home) = hand_cut_lab();
    let commit = commit_at(&lab, RELEASE);
    let refused = knives_release(&lab, &home, &["adopt", RELEASE]);
    assert_eq!(refused.status.code(), Some(3), "{}", text(&refused));
    assert!(
        text(&refused).contains("cut the release first"),
        "{}",
        text(&refused)
    );
    let (entries, refs, commits) = (
        ledger_entries(&home),
        bookmarks(&lab),
        visible_commits(&lab),
    );

    // When: the release is recorded at its commit.
    let recorded = record(&lab, &home, RELEASE, &commit);

    // Then: it exits 0 and says so, with no push to make: the publish remote
    // holds the name.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(out.contains(&recorded_line(RELEASE, &commit)), "{out}");
    assert!(!out.contains("push release/"), "{out}");

    // And: the ledger gains exactly one entry, the cut event, whose evidence
    // starts with the commit; no bookmark moved and no commit was made.
    let after = ledger_entries(&home);
    assert_eq!(after.len(), entries.len() + 1, "{after:?}");
    let event = newest_event(&home, RELEASE);
    assert!(
        event
            .text
            .starts_with(&format!("cut {RELEASE} as {} ", commit.short())),
        "{event:?}"
    );
    assert!(
        event.text.contains("; recorded: composed outside knives"),
        "{event:?}"
    );
    assert_eq!(
        event.evidence.first().map(String::as_str),
        Some(commit.as_str()),
        "{event:?}"
    );
    assert_eq!(bookmarks(&lab), refs, "{out}");
    assert_eq!(visible_commits(&lab), commits, "{out}");

    // And: adopt now reads the release as its recorded cut.
    let adopted = knives_release(&lab, &home, &["adopt", RELEASE]);
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    assert!(
        text(&adopted).contains("nothing to adopt"),
        "{}",
        text(&adopted)
    );
}

#[test]
fn recording_an_absent_name_makes_its_bookmark_at_the_commit() {
    // Given: a release merged by hand that no ref names.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work([
        "new",
        "-r",
        "main@origin",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "merged by hand",
    ]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let (home, _consumer) = release_test_home(&lab);
    let refs = bookmarks(&lab);

    // When: it is recorded as a name no ref holds.
    let recorded = record(&lab, &home, RELEASE, &commit);

    // Then: it exits 0, the local bookmark names the commit, no other
    // bookmark moved, and the line says how to push the name.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert_eq!(commit_at(&lab, RELEASE), commit, "{out}");
    let mut after = bookmarks(&lab);
    after.retain(|reference, _| reference.branch().as_str() != RELEASE);
    assert_eq!(after, refs, "{out}");
    assert!(
        out.contains(&format!(
            "{}; push {RELEASE} with jj git push --remote origin --bookmark {RELEASE}",
            recorded_line(RELEASE, &commit)
        )),
        "{out}"
    );
    assert_eq!(
        newest_event(&home, RELEASE)
            .evidence
            .first()
            .map(String::as_str),
        Some(commit.as_str())
    );
}

#[test]
fn the_push_a_record_prints_quotes_a_name_the_shell_would_read_apart() {
    // Given: a release merged by hand that no ref names, and a name whose
    // label holds a quote and a dollar sign, as the series' grammar allows.
    const NAME: &str = "release/2026-08-04-it's-$HOME";
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "merged by hand",
    ]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let (home, _consumer) = release_test_home(&lab);

    // When: it is recorded under that name.
    let recorded = record(&lab, &home, NAME, &commit);

    // Then: it exits 0, and the push it prints holds the name as one shell
    // word: in single quotes, its own quote written '\''.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&format!(
            "{}; push {NAME} with jj git push --remote origin --bookmark \
             'release/2026-08-04-it'\\''s-$HOME'\n",
            recorded_line(NAME, &commit)
        )),
        "{out}"
    );
}

#[test]
fn recording_again_at_the_same_commit_writes_nothing() {
    // Given: a release cut by hand and recorded once.
    let (lab, home) = hand_cut_lab();
    let commit = commit_at(&lab, RELEASE);
    let first = record(&lab, &home, RELEASE, &commit);
    assert_eq!(first.status.code(), Some(0), "{}", text(&first));
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: it is recorded again at the same commit.
    let again = record(&lab, &home, RELEASE, &commit);

    // Then: it exits 0, says it is already recorded, and writes nothing.
    let out = text(&again);
    assert_eq!(again.status.code(), Some(0), "{out}");
    assert!(out.contains("is already recorded"), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");
}

#[test]
fn a_name_older_than_the_newest_recorded_cut_is_refused() {
    // Given: a knives cut of `release/2026-08-05`, which the ledger records,
    // whose local bookmark is then forgotten, so only the ledger names it,
    // and a release merged by hand that no ref names.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(cut.status.success(), "{}", text(&cut));
    lab.jj_work(["bookmark", "forget", "release/2026-08-05"]);
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "merged by hand",
    ]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: the hand merge is recorded under the older name.
    let recorded = record(&lab, &home, RELEASE, &commit);

    // Then: it is refused, the newer name named with the ledger as what
    // holds it, and nothing is written.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(
        out.contains(
            "refusing to record release/2026-08-04: release/2026-08-05 sorts after it, and the \
             ledger records a cut or edit of it"
        ),
        "{out}"
    );
    assert!(out.contains("the older name first"), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");
}

#[test]
fn a_conflicted_commit_is_refused() {
    // Given: a merge made by hand of two branches that write one file each
    // their own way, left conflicted, which no ref names.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "merged by hand",
    ]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let (home, _consumer) = release_test_home(&lab);
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: it is recorded.
    let recorded = record(&lab, &home, RELEASE, &commit);

    // Then: it is refused, the conflicted file named, and nothing is
    // written.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: {RELEASE} at {} still has 1 conflicted file(s), so it is not resolved yet; \
             resolve them, then record it:\n    shared.txt\n",
            commit.short()
        )),
        "{out}"
    );
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");
}

#[test]
fn a_commit_that_shares_no_history_with_the_trunk_is_refused() {
    // Given: a commit made on a history of its own, which no ref names: it
    // holds none of the trunk, so it is no release of the series.
    let lab = Lab::new();
    lab.jj_work(["new", "-r", "root()", "-m", "an unrelated history"]);
    std::fs::write(lab.work_path().join("other.txt"), "other\n").expect("write other.txt");
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let (home, _consumer) = release_test_home(&lab);
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: it is recorded as the series' release.
    let recorded = record(&lab, &home, RELEASE, &commit);

    // Then: it is refused, and nothing is written.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");
}

#[test]
fn the_root_commit_is_refused() {
    // Given: the repository's root commit, which has no parent and no file.
    let lab = Lab::new();
    let root = commit_of(&lab, "root()");
    let (home, _consumer) = release_test_home(&lab);
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: it is recorded as a release.
    let recorded = record(&lab, &home, RELEASE, &root);

    // Then: it is refused, and nothing is written.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");
}

#[test]
fn a_change_no_member_explains_is_refused_outside_a_stacked_series() {
    // Given: RELEASE recorded, then the next release merged by hand of three
    // branches, with a file added in the merge itself that no member has.
    let (lab, home) = hand_cut_lab();
    let first = record(&lab, &home, RELEASE, &commit_at(&lab, RELEASE));
    assert_eq!(first.status.code(), Some(0), "{}", text(&first));
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-r",
        "feat/gamma",
        "-m",
        "the next release, merged by hand",
    ]);
    std::fs::write(lab.work_path().join("unexplained.txt"), "no member\n")
        .expect("write unexplained.txt");
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: it is recorded.
    let recorded = record(&lab, &home, "release/2026-08-05", &commit);

    // Then: it is refused as a stacked series refuses it, naming the file no
    // member or trunk explains, and nothing is written.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(out.contains("unexplained.txt"), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");
}

#[test]
fn a_first_record_with_a_change_no_member_explains_is_refused() {
    // Given: the series' first release, merged by hand of two branches, with
    // a file added in the merge itself that no member has. No release of the
    // series is recorded or named before it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "the first release, merged by hand",
    ]);
    std::fs::write(lab.work_path().join("unexplained.txt"), "no member\n")
        .expect("write unexplained.txt");
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let (home, _consumer) = release_test_home(&lab);
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: it is recorded.
    let recorded = record(&lab, &home, RELEASE, &commit);

    // Then: it is refused as a later record is, naming the file no member or
    // trunk explains, and nothing is written: the series' first record is
    // the release every later record's drift is measured from.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(out.contains("unexplained.txt"), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");
}

#[test]
fn a_first_record_refusal_names_the_members_fork_point_with_the_trunk() {
    // Given: the series' first release, merged by hand of two branches, with
    // a file added in the merge itself that no member has.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work(["new", "-r", "feat/alpha", "-r", "feat/beta", "-m", RELEASE]);
    std::fs::write(lab.work_path().join("unexplained.txt"), "no member\n")
        .expect("write unexplained.txt");
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let (home, _consumer) = release_test_home(&lab);

    // When: it is recorded.
    let recorded = record(&lab, &home, RELEASE, &commit);

    // Then: the refusal names what the drift was measured from: no release
    // precedes the first record, so it is the members' fork point with the
    // trunk, never a previous release.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(
        out.contains(
            "  !! unexplained.txt: changed between the members' fork point with the trunk and \
             this cut with no member or trunk explaining it"
        ),
        "{out}"
    );
    assert!(!out.contains("between the previous release"), "{out}");
}

#[test]
fn a_hand_merge_from_the_trunk_records_only_its_branches() {
    // Given: a release merged by hand from the trunk, as `git merge` makes
    // one there: its parents are the trunk, then two branches.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work([
        "new",
        "-r",
        "main@upstream",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "merged by hand from the trunk",
    ]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let (home, _consumer) = release_test_home(&lab);

    // When: it is recorded.
    let recorded = record(&lab, &home, RELEASE, &commit);

    // Then: it exits 0, and the cut event records exactly the two branches:
    // a parent the trunk holds is never a member.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    let event = newest_event(&home, RELEASE);
    let members = [commit_at(&lab, "feat/alpha"), commit_at(&lab, "feat/beta")];
    let mut evidence = vec![commit.as_str().to_owned()];
    evidence.extend(members.iter().map(|member| member.as_str().to_owned()));
    assert_eq!(event.evidence, evidence, "{event:?}");
    let parents: Vec<&str> = event
        .parents
        .iter()
        .map(|parent| parent.commit.as_str())
        .collect();
    assert_eq!(
        parents,
        members.each_ref().map(CommitId::as_str),
        "{event:?}"
    );
    assert!(event.text.contains(" with 2 parent(s): "), "{event:?}");
}

#[test]
fn a_commit_that_merges_no_member_is_refused() {
    // Given: a one-commit branch on the trunk, given as the release: its one
    // parent is the trunk, which is never a member.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let commit = commit_at(&lab, "feat/alpha");
    let (home, _consumer) = release_test_home(&lab);
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: it is recorded as the series' release.
    let recorded = record(&lab, &home, RELEASE, &commit);

    // Then: it is refused, saying it merges no member, and nothing is
    // written.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "refusing to record {RELEASE} at {}: it merges no member",
            commit.short()
        )),
        "{out}"
    );
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");
}

#[test]
fn a_hand_merge_on_the_previous_release_is_refused_outside_a_stacked_series() {
    // Given: RELEASE recorded, then the next release merged by hand of
    // RELEASE itself and one more branch.
    let (lab, home) = hand_cut_lab();
    let previous = commit_at(&lab, RELEASE);
    let first = record(&lab, &home, RELEASE, &previous);
    assert_eq!(first.status.code(), Some(0), "{}", text(&first));
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.jj_work([
        "new",
        "-r",
        RELEASE,
        "-r",
        "feat/gamma",
        "-m",
        "the next release, made on the last",
    ]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: it is recorded.
    let recorded = record(&lab, &home, "release/2026-08-05", &commit);

    // Then: it is refused, naming the previous release as never a member
    // and the setting that reads a release made on it, and nothing is
    // written.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "its parent {} is {RELEASE}, the series' previous release, which is never a member",
            previous.short()
        )),
        "{out}"
    );
    assert!(out.contains("the default series has no table"), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");
}

#[test]
fn a_record_that_lacks_a_member_included_after_the_last_cut_is_refused_naming_it() {
    // Given: RELEASE recorded at a hand merge of feat/alpha and feat/beta,
    // then feat/gamma included into it, which the ledger records as an edit
    // after the cut, and the next release merged by hand of feat/alpha,
    // feat/beta and feat/delta, without feat/gamma.
    let (lab, home) = hand_cut_lab();
    let first = record(&lab, &home, RELEASE, &commit_at(&lab, RELEASE));
    assert_eq!(first.status.code(), Some(0), "{}", text(&first));
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let include = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert_eq!(include.status.code(), Some(0), "{}", text(&include));
    lab.branch("feat/delta", "delta.txt", "delta\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-r",
        "feat/delta",
        "-m",
        "the next release, merged by hand",
    ]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: it is recorded as the next release.
    let recorded = record(&lab, &home, "release/2026-08-05", &commit);

    // Then: it is refused, naming RELEASE, the newest release the ledger
    // records, and feat/gamma, the member its newest write holds that the
    // commit does not carry, and nothing is written.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to record release/2026-08-05 at {}: it does not carry 1 member(s) of \
             {RELEASE}, the newest release that the ledger records in the series:\n    \
             feat/gamma@{}\n",
            commit.short(),
            commit_at(&lab, "feat/gamma").short()
        )),
        "{out}"
    );
    assert!(out.contains("nothing was recorded"), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");
}

#[test]
fn a_published_name_the_publish_remote_lacks_is_refused_with_the_push_that_records_it() {
    // Given: a release merged by hand that no ref names, its commit tagged:
    // the tag marks it published, and the publish remote holds no ref of
    // the name yet.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "merged by hand",
    ]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    lab.jj_work(["tag", "set", "v1.0.0", "-r", commit.as_str()]);
    let (home, _consumer) = release_test_home(&lab);
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: it is recorded.
    let refused = record(&lab, &home, RELEASE, &commit);

    // Then: it is refused and nothing is written. The refusal gives the
    // commands that record it: make the bookmark, push it to the publish
    // remote, record again. It never gives the remedy of a hand edit.
    let out = text(&refused);
    let at = commit.short();
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to record {RELEASE} at {at}: it is published (tag v1.0.0 names \
             {at}), and origin does not hold {RELEASE}; a published release is recorded once \
             the publish remote holds it: `jj bookmark create {RELEASE} -r {at}`, then `jj git \
             push --remote origin --bookmark {RELEASE}`, then `knives release --repo demo cut \
             {RELEASE} --at {at}`"
        )),
        "{out}"
    );
    assert!(!out.contains("a hand edit cannot be recorded"), "{out}");
    assert!(!out.contains("cut a new dated name"), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");

    // When: the bookmark is made and pushed, as the refusal says, and the
    // record runs again.
    lab.jj_work(["bookmark", "create", RELEASE, "-r", at]);
    lab.push_branch(RELEASE);
    let recorded = record(&lab, &home, RELEASE, &commit);

    // Then: it is recorded, with no push left to make.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(out.contains(&recorded_line(RELEASE, &commit)), "{out}");
    assert!(!out.contains("; push "), "{out}");
    assert_eq!(
        newest_event(&home, RELEASE)
            .evidence
            .first()
            .map(String::as_str),
        Some(commit.as_str()),
        "{out}"
    );
}

/// The events the ledger records about `name`.
fn events_about(home: &tempfile::TempDir, name: &str) -> Vec<Entry> {
    ledger_entries(home)
        .into_iter()
        .filter(|entry| entry.kind == Kind::Event && entry.subject.as_deref() == Some(name))
        .collect()
}

/// `knives release cut <name> --at <commit>` for each of `commits`, every
/// one started before any is waited for, each run to its end.
fn records_at_once(
    lab: &Lab,
    home: &tempfile::TempDir,
    name: &str,
    commits: &[&CommitId],
) -> Vec<Output> {
    // Every process starts before the first is waited for.
    let mut running = Vec::with_capacity(commits.len());
    for commit in commits {
        running.push(
            lab::release_command(
                lab,
                home,
                lab::ReleaseOutput::Text,
                &["cut", name, "--at", commit.as_str()],
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start knives release cut --at"),
        );
    }
    running
        .into_iter()
        .map(|child| child.wait_with_output().expect("wait for knives release"))
        .collect()
}

#[test]
fn three_records_of_one_name_at_once_write_one_event() {
    // Given: a release merged by hand and pushed, which the ledger does not
    // record.
    let (lab, home) = hand_cut_lab();
    let commit = commit_at(&lab, RELEASE);
    let entries = ledger_entries(&home);

    // When: three processes record it under its name at once.
    let outputs = records_at_once(&lab, &home, RELEASE, &[&commit, &commit, &commit]);

    // Then: each exits 0, the ledger gains exactly one entry, the one cut
    // event, and the other two say the name is already recorded: each
    // record reads the ledger only once the one before it is done.
    let out: String = outputs.iter().map(text).collect();
    for output in &outputs {
        assert_eq!(output.status.code(), Some(0), "{out}");
    }
    assert_eq!(ledger_entries(&home).len(), entries.len() + 1, "{out}");
    let events = events_about(&home, RELEASE);
    assert_eq!(events.len(), 1, "{out}");
    assert_eq!(
        events[0].evidence.first().map(String::as_str),
        Some(commit.as_str()),
        "{out}"
    );
    let repeats = outputs
        .iter()
        .filter(|output| text(output).contains("is already recorded"))
        .count();
    assert_eq!(repeats, 2, "{out}");
}

#[test]
fn a_record_of_a_recorded_name_at_another_commit_is_refused_naming_the_recorded_one() {
    // Given: two releases merged by hand, which no ref names: one of two
    // branches, and one of those and a third.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.jj_work(["new", "-r", "feat/alpha", "-r", "feat/beta", "-m", "two"]);
    lab.jj_work(["new"]);
    let two = commit_of(&lab, "@-");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-r",
        "feat/gamma",
        "-m",
        "three",
    ]);
    lab.jj_work(["new"]);
    let three = commit_of(&lab, "@-");
    let (home, _consumer) = release_test_home(&lab);

    // When: each is recorded under one name, both at once.
    let outputs = records_at_once(&lab, &home, RELEASE, &[&two, &three]);

    // Then: the ledger records the name once, at the commit of the record
    // that ran first, which exits 0.
    let out: String = outputs.iter().map(text).collect();
    let events = events_about(&home, RELEASE);
    assert_eq!(events.len(), 1, "{out}");
    let recorded = events[0]
        .evidence
        .first()
        .cloned()
        .expect("a recorded commit");
    let (first, second) = if recorded == two.as_str() {
        (&outputs[0], &outputs[1])
    } else {
        assert_eq!(recorded, three.as_str(), "{out}");
        (&outputs[1], &outputs[0])
    };
    assert_eq!(first.status.code(), Some(0), "{out}");
    assert_eq!(commit_at(&lab, RELEASE).as_str(), recorded, "{out}");

    // And: the other is refused, exit 3, naming the recorded commit.
    assert_eq!(second.status.code(), Some(3), "{out}");
    assert!(
        text(second).contains(&format!(
            "the ledger already records {RELEASE}, newest as the recorded cut {}",
            CommitId::new(recorded.as_str()).short()
        )),
        "{out}"
    );
}

/// A lab whose upstream carries a second base, `old`, split from `main`.
fn two_base_lab() -> Lab {
    let lab = Lab::new();
    lab.upstream_branch("old", "old.txt", "old base\n");
    lab.advance_upstream("new base\n");
    lab
}

/// A branch on `base` whose one commit writes `content` to `file`, with its
/// bookmark on that commit.
fn branch_on(lab: &Lab, base: &str, name: &str, (file, content): (&str, &str)) {
    lab.jj_work(["new", "-r", base, "-m", name]);
    std::fs::write(lab.work_path().join(file), content).expect("write branch");
    lab.jj_work(["bookmark", "create", name, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// The release-test home with a series `old` on the upstream branch `old`,
/// plus whatever `extra` the test adds to its table.
fn series_home(lab: &Lab, extra: &str) -> tempfile::TempDir {
    let (home, _consumer) = release_test_home(lab);
    let registry = home.path().join("repos.toml");
    let mut registered = std::fs::read_to_string(&registry).expect("read registry");
    registered.push_str("\n[repos.demo.series.old]\nbase = \"old\"\n");
    registered.push_str(extra);
    std::fs::write(&registry, registered).expect("write registry");
    home
}

#[test]
fn a_series_hand_cut_is_recorded() {
    // Given: series `old`, and two branches on its base merged by hand as a
    // release of it with a version label, pushed to the publish remote.
    const SPEED: &str = "release/old/2026-08-04-speed-8.0.9";
    let lab = two_base_lab();
    branch_on(&lab, "old@upstream", "fix/a", ("a.txt", "fix/a\n"));
    branch_on(&lab, "old@upstream", "fix/b", ("b.txt", "fix/b\n"));
    lab.jj_work(["new", "-r", "fix/a", "-r", "fix/b", "-m", SPEED]);
    lab.jj_work(["bookmark", "create", SPEED, "-r", "@"]);
    lab.jj_work(["new"]);
    lab.push_branch(SPEED);
    let home = series_home(&lab, "");
    let commit = commit_at(&lab, SPEED);

    // When: the release is recorded in its series.
    let recorded = record_in_series(&lab, &home, SPEED, commit.as_str());

    // Then: it exits 0, and the cut event records the commit and its two
    // branches.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(out.contains(&recorded_line(SPEED, &commit)), "{out}");
    let event = newest_event(&home, SPEED);
    assert!(
        event.text.starts_with(&format!("cut {SPEED} as ")),
        "{event:?}"
    );
    let mut evidence = vec![commit.as_str().to_owned()];
    evidence.extend(["fix/a", "fix/b"].map(|branch| commit_at(&lab, branch).as_str().to_owned()));
    assert_eq!(event.evidence, evidence, "{event:?}");

    // And: adopt reads it as its recorded cut.
    let adopted = knives_release(&lab, &home, &["--series", "old", "adopt", SPEED]);
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    assert!(
        text(&adopted).contains("nothing to adopt"),
        "{}",
        text(&adopted)
    );
}

/// Series `old`, with `stacked = true` when `stacked`: its first cut
/// [`PREVIOUS`] of `fix/a` (which writes `lines.txt`) and `fix/b`, pushed
/// and marked published by a `record: published` note.
fn previous_release_lab(stacked: bool) -> (Lab, tempfile::TempDir) {
    let lab = two_base_lab();
    branch_on(
        &lab,
        "old@upstream",
        "fix/a",
        ("lines.txt", "1\n2\n3\n4\n5\n"),
    );
    branch_on(&lab, "old@upstream", "fix/b", ("b.txt", "fix/b\n"));
    let home = series_home(&lab, if stacked { "stacked = true\n" } else { "" });
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", PREVIOUS]);
    assert!(cut.status.success(), "{}", text(&cut));
    lab.push_branch(PREVIOUS);
    lab.fetch_work();
    let noted = lab::knives(
        &lab,
        &home,
        &[
            "notch",
            PREVIOUS,
            "-m",
            "record: published as v1.0.0",
            "--evidence",
            commit_at(&lab, PREVIOUS).as_str(),
            "--repo",
            "demo",
        ],
    );
    assert!(noted.status.success(), "{}", text(&noted));
    (lab, home)
}

/// Three branches on [`PREVIOUS`]: `m6` changes the first line of
/// `lines.txt`, `m5` its last line, and `m4` adds a file.
fn members_on_previous(lab: &Lab) {
    branch_on(lab, PREVIOUS, "m6", ("lines.txt", "1 m6\n2\n3\n4\n5\n"));
    branch_on(lab, PREVIOUS, "m5", ("lines.txt", "1\n2\n3\n4\n5 m5\n"));
    branch_on(lab, PREVIOUS, "m4", ("m4.txt", "m4\n"));
}

/// What `lines.txt` holds in [`chain_next`]: each member's line, and the
/// middle line its inner merge of `m5` changed by hand.
const RESOLVED: &str = "1 m6\n2\n3 by hand\n4\n5 m5\n";

/// [`NEXT`] made by hand as a compose script makes a release: the chain
/// `merge(merge(merge(PREVIOUS, m6), m5), m4)`, whose inner merge of `m5`
/// also changes the middle line of `lines.txt` by hand.
fn chain_next(lab: &Lab) {
    lab.jj_work(["new", "-r", PREVIOUS, "-r", "m6", "-m", "compose m6"]);
    lab.jj_work(["new", "-r", "@", "-r", "m5", "-m", "compose m5"]);
    std::fs::write(lab.work_path().join("lines.txt"), RESOLVED).expect("change a line by hand");
    lab.jj_work(["new", "-r", "@", "-r", "m4", "-m", NEXT]);
    lab.jj_work(["bookmark", "create", NEXT, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// [`previous_release_lab`] with the stacked key, its members, and the chain
/// [`NEXT`] recorded with `cut --at`, which must exit 0.
fn recorded_chain_lab() -> (Lab, tempfile::TempDir) {
    let (lab, home) = previous_release_lab(true);
    members_on_previous(&lab);
    chain_next(&lab);
    let recorded = record_in_series(&lab, &home, NEXT, NEXT);
    assert_eq!(recorded.status.code(), Some(0), "{}", text(&recorded));
    (lab, home)
}

#[test]
fn a_chain_shaped_release_is_recorded_in_a_stacked_series() {
    // Given: a stacked series whose next release is a chain of merges, one
    // member each, on its published first cut, held only here.
    let (lab, home) = previous_release_lab(true);
    members_on_previous(&lab);
    chain_next(&lab);
    let (commit, previous) = (commit_at(&lab, NEXT), commit_at(&lab, PREVIOUS));

    // When: the chain is recorded.
    let recorded = record_in_series(&lab, &home, NEXT, NEXT);

    // Then: it exits 0, so the audit found nothing missing, and the line
    // says how to push it.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&format!(
            "{}; push {NEXT} with jj git push --remote origin --bookmark {NEXT}",
            recorded_line(NEXT, &commit)
        )),
        "{out}"
    );

    // And: the event records the previous release under its label, then the
    // side parents on the chain's first-parent line, never its two literal
    // parents.
    let event = newest_event(&home, NEXT);
    let members = ["m6", "m5", "m4"].map(|branch| (branch, commit_at(&lab, branch)));
    let mut evidence = vec![commit.as_str().to_owned(), previous.as_str().to_owned()];
    evidence.extend(members.iter().map(|(_, member)| member.as_str().to_owned()));
    assert_eq!(event.evidence, evidence, "{event:?}");
    let mut parents = vec![RecordedParent {
        commit: previous.as_str().to_owned(),
        branches: vec![format!("{PREVIOUS} previous release (not a member)")],
    }];
    parents.extend(members.iter().map(|(branch, member)| RecordedParent {
        commit: member.as_str().to_owned(),
        branches: vec![(*branch).to_owned()],
    }));
    assert_eq!(event.parents, parents, "{event:?}");
    assert!(
        event.text.contains("; recorded: composed outside knives"),
        "{event:?}"
    );

    // And: `members --verify` reads those members back, each replayed empty.
    let verify = knives_release(&lab, &home, &["--series", "old", "members", "--verify"]);
    let out = text(&verify);
    assert_eq!(verify.status.code(), Some(0), "{out}");
    for (branch, member) in &members {
        assert!(
            out.contains(&format!("- {} {branch}", member.short())),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "  {branch}@{}: carried (replay empty)",
                member.short()
            )),
            "{out}"
        );
    }
}

#[test]
fn a_chain_shaped_release_outside_a_stacked_series_is_refused_naming_the_setting() {
    // Given: the same chain in a series without the stacked key.
    let (lab, home) = previous_release_lab(false);
    members_on_previous(&lab);
    chain_next(&lab);
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: the chain is recorded.
    let recorded = record_in_series(&lab, &home, NEXT, NEXT);

    // Then: it is refused, the series table and its setting named, and
    // nothing is written.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(out.contains("stacks on an earlier merge"), "{out}");
    assert!(
        out.contains("set `stacked = true` on [repos.demo.series.\"old\"]"),
        "{out}"
    );
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");
}

#[test]
fn an_edit_of_a_recorded_chain_keeps_what_its_inner_merge_changed() {
    // Given: the recorded chain, whose inner merge of `m5` changed a line by
    // hand, and one more branch on the previous release.
    let (lab, home) = recorded_chain_lab();
    branch_on(&lab, PREVIOUS, "m3", ("m3.txt", "m3\n"));

    // When: that branch is included.
    let include = knives_release(&lab, &home, &["--series", "old", "include", "m3"]);

    // Then: the release is one merge of the previous release, the members
    // and the branch, and it keeps the line the inner merge changed.
    assert_eq!(include.status.code(), Some(0), "{}", text(&include));
    let mut expected = vec![commit_at(&lab, PREVIOUS)];
    expected.extend(["m6", "m5", "m4", "m3"].map(|branch| commit_at(&lab, branch)));
    assert_eq!(release_parents(&lab, NEXT), expected);
    assert_eq!(file_at_revision(&lab, NEXT, "lines.txt"), RESOLVED);
    assert_eq!(file_at_revision(&lab, NEXT, "m3.txt"), "m3\n");
}

#[test]
fn a_recorded_chain_whose_member_is_rewritten_in_place_reads_as_recorded() {
    // Given: the recorded chain, then a member reworded in place, so jj
    // rebases the chain onto it, and one more branch on the previous
    // release.
    let (lab, home) = recorded_chain_lab();
    let recorded = commit_at(&lab, NEXT);
    lab.jj_work(["describe", "m6", "-m", "m6, reworded"]);
    assert_ne!(commit_at(&lab, NEXT), recorded);
    branch_on(&lab, PREVIOUS, "m3", ("m3.txt", "m3\n"));

    // When: that branch is included.
    let include = knives_release(&lab, &home, &["--series", "old", "include", "m3"]);

    // Then: the release reads as its recorded cut rebased by jj, so the
    // include is not refused, and the release keeps the line the inner
    // merge changed.
    let out = text(&include);
    assert_eq!(include.status.code(), Some(0), "{out}");
    assert_eq!(file_at_revision(&lab, NEXT, "lines.txt"), RESOLVED);
}

#[test]
fn a_start_commit_on_the_previous_release_is_recorded_with_no_member() {
    // Given: a stacked series' published first cut, and a commit made on it
    // by hand with its tree, the start of the next release, which merges no
    // member yet.
    let (lab, home) = previous_release_lab(true);
    lab.jj_work(["new", "-r", PREVIOUS, "-m", NEXT]);
    lab.jj_work(["new"]);
    let (commit, previous) = (commit_of(&lab, "@-"), commit_at(&lab, PREVIOUS));

    // When: it is recorded as the next release.
    let recorded = record_in_series(&lab, &home, NEXT, commit.as_str());

    // Then: it exits 0, and the event records the previous release under
    // its label and no member: a start commit is a release with none.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    let event = newest_event(&home, NEXT);
    assert_eq!(
        event.evidence,
        vec![commit.as_str().to_owned(), previous.as_str().to_owned()],
        "{event:?}"
    );
    assert_eq!(
        event.parents,
        vec![RecordedParent {
            commit: previous.as_str().to_owned(),
            branches: vec![format!("{PREVIOUS} previous release (not a member)")],
        }],
        "{event:?}"
    );
}

/// Series `old`'s third release, recorded after [`NEXT`].
const THIRD: &str = "release/old/2026-08-06";

#[test]
fn a_record_built_on_an_older_release_than_the_newest_recorded_one_is_refused_naming_its_members() {
    // Given: a stacked series whose published first cut is PREVIOUS; a
    // knives cut of NEXT on it, with `m6` and `m5` included; and a commit
    // merged by hand of PREVIOUS and `m4` alone, so it is built on PREVIOUS
    // and holds neither member of NEXT.
    let (lab, home) = previous_release_lab(true);
    members_on_previous(&lab);
    let previous = commit_at(&lab, PREVIOUS);
    let steps: [&[&str]; 3] = [&["cut", NEXT], &["include", "m6"], &["include", "m5"]];
    for args in steps {
        let mut series = vec!["--series", "old"];
        series.extend_from_slice(args);
        let ran = knives_release(&lab, &home, &series);
        assert_eq!(ran.status.code(), Some(0), "{args:?}: {}", text(&ran));
    }
    lab.jj_work(["new", "-r", previous.as_str(), "-r", "m4", "-m", THIRD]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: it is recorded as the series' third release.
    let refused = record_in_series(&lab, &home, THIRD, commit.as_str());

    // Then: it is refused, naming NEXT, the newest release the ledger
    // records, and both members of NEXT's newest write, which the commit
    // does not carry; nothing is written.
    let out = text(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to record {THIRD} at {}: it does not carry 2 member(s) of {NEXT}, \
             the newest release that the ledger records in the series:\n",
            commit.short()
        )),
        "{out}"
    );
    for branch in ["m6", "m5"] {
        assert!(
            out.contains(&format!(
                "    {branch}@{}\n",
                commit_at(&lab, branch).short()
            )),
            "{out}"
        );
    }
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");

    // When: it is recorded again, the drop stated.
    let allowed = knives_release(
        &lab,
        &home,
        &[
            "--series",
            "old",
            "cut",
            THIRD,
            "--at",
            commit.as_str(),
            "--allow-drop",
        ],
    );

    // Then: it is recorded, and its event states the drop as a cut's does.
    let out = text(&allowed);
    assert_eq!(allowed.status.code(), Some(0), "{out}");
    assert!(out.contains(&recorded_line(THIRD, &commit)), "{out}");
    let event = newest_event(&home, THIRD);
    assert_eq!(
        event.evidence.first().map(String::as_str),
        Some(commit.as_str()),
        "{event:?}"
    );
    assert!(
        event.text.contains(&format!(
            "; {NEXT}'s newest recorded write (an edit) holds 2 member(s); dropped: m6@{}, m5@{}",
            commit_at(&lab, "m6").short(),
            commit_at(&lab, "m5").short()
        )),
        "{event:?}"
    );
}

/// Series `old`'s fourth release, recorded after [`THIRD`].
const FOURTH: &str = "release/old/2026-08-07";

/// A `record: published` note on release `name`, naming its commit, as the
/// release owner writes one: a stacked cut starts on a published release.
fn mark_published(lab: &Lab, home: &tempfile::TempDir, name: &str) {
    let noted = lab::knives(
        lab,
        home,
        &[
            "notch",
            name,
            "-m",
            "record: published as v1.0.0",
            "--evidence",
            commit_at(lab, name).as_str(),
            "--repo",
            "demo",
        ],
    );
    assert!(noted.status.success(), "{}", text(&noted));
}

/// [`previous_release_lab`] with the stacked key and its members, then a
/// knives cut of [`NEXT`] on [`PREVIOUS`] with `m6` and `m5` included, each
/// step exiting 0; and the commit of `PREVIOUS`, whose refs the reap after
/// that cut forgets.
fn next_with_members_lab() -> (Lab, tempfile::TempDir, CommitId) {
    let (lab, home) = previous_release_lab(true);
    members_on_previous(&lab);
    let previous = commit_at(&lab, PREVIOUS);
    let steps: [&[&str]; 3] = [&["cut", NEXT], &["include", "m6"], &["include", "m5"]];
    for args in steps {
        let mut series = vec!["--series", "old"];
        series.extend_from_slice(args);
        let ran = knives_release(&lab, &home, &series);
        assert_eq!(ran.status.code(), Some(0), "{args:?}: {}", text(&ran));
    }
    (lab, home, previous)
}

/// A commit merged by hand of `previous` and `m4` alone, which no ref names:
/// built on [`PREVIOUS`], so below every later release of the series.
fn merged_on_previous(lab: &Lab, previous: &CommitId) -> CommitId {
    lab.jj_work(["new", "-r", previous.as_str(), "-r", "m4", "-m", FOURTH]);
    lab.jj_work(["new"]);
    commit_of(lab, "@-")
}

/// The refusal of a record of [`FOURTH`] at `commit` that is not built on
/// `write`, the newest write of [`THIRD`], a cut.
fn not_built_on_third(commit: &CommitId, write: &CommitId) -> String {
    format!(
        "demo: refusing to record {FOURTH} at {}: it does not have {THIRD}'s newest recorded \
         write (a cut) {} on its first-parent line, so it does not carry {THIRD}\n",
        commit.short(),
        write.short()
    )
}

/// [`recorded_chain_lab`], then `m6` reworded in place, so jj rebases the
/// chain and [`NEXT`]'s bookmark onto it: NEXT holds a rewrite of its
/// recorded write, which knives reads as that write. Returns NEXT's
/// rewritten commit.
fn rewritten_chain_lab() -> (Lab, tempfile::TempDir, CommitId) {
    let (lab, home) = recorded_chain_lab();
    let recorded = commit_at(&lab, NEXT);
    lab.jj_work(["describe", "m6", "-m", "m6, reworded"]);
    let rewritten = commit_at(&lab, NEXT);
    assert_ne!(rewritten, recorded, "jj rebased NEXT over the rewritten m6");
    (lab, home, rewritten)
}

#[test]
fn a_stacked_cut_on_a_release_jj_rewrote_in_place_goes_ahead() {
    // Given: NEXT recorded as a chain, then rebased by jj over `m6`
    // reworded in place, and published by a note.
    let (lab, home, rewritten) = rewritten_chain_lab();
    mark_published(&lab, &home, NEXT);

    // When: the next release is cut.
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", THIRD]);

    // Then: it goes ahead on the rewritten NEXT: a rewrite of the newest
    // recorded write stands for that write on the start's first-parent
    // line.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(0), "{out}");
    assert_eq!(release_parents(&lab, THIRD), vec![rewritten], "{out}");
}

#[test]
fn a_record_built_on_a_release_jj_rewrote_in_place_is_recorded() {
    // Given: NEXT recorded as a chain, then rebased by jj over `m6`
    // reworded in place, and a start commit made by hand on the rewritten
    // NEXT.
    let (lab, home, rewritten) = rewritten_chain_lab();
    lab.jj_work(["new", "-r", rewritten.as_str(), "-m", THIRD]);
    lab.jj_work(["new"]);
    let start = commit_of(&lab, "@-");

    // When: it is recorded as the series' third release.
    let recorded = record_in_series(&lab, &home, THIRD, start.as_str());

    // Then: it is recorded: the rewritten NEXT, on its first-parent line,
    // stands for NEXT's recorded write.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(out.contains(&recorded_line(THIRD, &start)), "{out}");
}

#[test]
fn a_record_built_below_a_stacked_start_of_the_newest_release_is_refused_naming_its_write() {
    // Given: a stacked series whose published first cut is PREVIOUS; a
    // knives cut of NEXT on it, with `m6` and `m5` included and marked
    // published; a knives cut of THIRD on NEXT, a stacked start whose event
    // records NEXT under its label and no member; and a commit merged by
    // hand of PREVIOUS and `m4` alone.
    let (lab, home, previous) = next_with_members_lab();
    mark_published(&lab, &home, NEXT);
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", THIRD]);
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    let start = commit_at(&lab, THIRD);
    let commit = merged_on_previous(&lab, &previous);
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: it is recorded as the series' fourth release.
    let refused = record_in_series(&lab, &home, FOURTH, commit.as_str());

    // Then: it is refused, naming THIRD's newest recorded write, a cut, its
    // commit and THIRD, and saying to build the commit on that write;
    // nothing is written. Its members alone hold none of NEXT's, which THIRD
    // carries through NEXT.
    let out = text(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(out.contains(&not_built_on_third(&commit, &start)), "{out}");
    assert!(
        out.contains(&format!(
            "  nothing was recorded; build the commit on {} and record that one, or re-run with \
             --allow-drop to state the drop is intended\n",
            start.short()
        )),
        "{out}"
    );
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");

    // When: it is recorded again, the drop stated.
    let allowed = knives_release(
        &lab,
        &home,
        &[
            "--series",
            "old",
            "cut",
            FOURTH,
            "--at",
            commit.as_str(),
            "--allow-drop",
        ],
    );

    // Then: it is recorded, and both its line and its event name THIRD's
    // newest recorded write, its release and its commit.
    let out = text(&allowed);
    assert_eq!(allowed.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&format!(
            "demo: --allow-drop: recording {FOURTH} at {} without {THIRD}'s newest recorded write \
             (a cut) {} on its first-parent line\n",
            commit.short(),
            start.short()
        )),
        "{out}"
    );
    let event = newest_event(&home, FOURTH);
    assert_eq!(
        event.evidence.first().map(String::as_str),
        Some(commit.as_str()),
        "{event:?}"
    );
    assert!(
        event.text.contains(&format!(
            "; {THIRD}'s newest recorded write (a cut) holds 0 member(s); all carried; not built \
             on {THIRD}'s newest recorded write (a cut) {}",
            start.short()
        )),
        "{event:?}"
    );
}

#[test]
fn a_record_built_below_a_recorded_start_of_the_newest_release_is_refused_naming_its_write() {
    // Given: the same series with NEXT cut by knives and `m6` and `m5`
    // included; THIRD recorded with `cut --at` at a start commit made by
    // hand on NEXT, a release with no member; and a commit merged by hand of
    // PREVIOUS and `m4` alone.
    let (lab, home, previous) = next_with_members_lab();
    lab.jj_work(["new", "-r", NEXT, "-m", THIRD]);
    lab.jj_work(["new"]);
    let start = commit_of(&lab, "@-");
    let recorded = record_in_series(&lab, &home, THIRD, start.as_str());
    assert_eq!(recorded.status.code(), Some(0), "{}", text(&recorded));
    let commit = merged_on_previous(&lab, &previous);
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: it is recorded as the series' fourth release.
    let refused = record_in_series(&lab, &home, FOURTH, commit.as_str());

    // Then: it is refused, naming THIRD's recorded start, a cut, its commit
    // and THIRD; nothing is written.
    let out = text(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(out.contains(&not_built_on_third(&commit, &start)), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");
}

#[test]
fn a_record_that_lacks_a_member_only_the_previous_cut_recorded_names_that_cut() {
    // Given: RELEASE recorded at a hand merge of `feat/alpha` and
    // `feat/beta`; `feat/beta` dropped from it, an edit, pushed; and the
    // next release merged by hand of `feat/alpha` and `feat/gamma`.
    const NEWER: &str = "release/2026-08-05";
    let (lab, home) = hand_cut_lab();
    let recorded = record(&lab, &home, RELEASE, &commit_at(&lab, RELEASE));
    assert_eq!(recorded.status.code(), Some(0), "{}", text(&recorded));
    let dropped = knives_release(
        &lab,
        &home,
        &["drop", "feat/beta", "--why", "not this time"],
    );
    assert_eq!(dropped.status.code(), Some(0), "{}", text(&dropped));
    lab.push_branch(RELEASE);
    lab.fetch_work();
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/gamma",
        "-m",
        "merged by hand",
    ]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let beta = commit_at(&lab, "feat/beta");

    // When: it is recorded as the next release.
    let refused = record(&lab, &home, NEWER, &commit);

    // Then: it is refused, naming `feat/beta` with the previous cut, which
    // only that cut recorded; nothing is recorded.
    let out = text(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to record {NEWER} at {}: it does not carry 1 member(s) the previous \
             cut {RELEASE} recorded:\n    feat/beta@{}\n  nothing was recorded; record a commit \
             that carries each of them, or re-run with --allow-drop to state the drop is \
             intended\n",
            commit.short(),
            beta.short()
        )),
        "{out}"
    );

    // When: it is recorded again, the drop stated.
    let allowed = knives_release(
        &lab,
        &home,
        &["cut", NEWER, "--at", commit.as_str(), "--allow-drop"],
    );

    // Then: its event names each record as what it is: RELEASE's newest
    // write, the drop, first, then the member only the previous cut held.
    assert_eq!(allowed.status.code(), Some(0), "{}", text(&allowed));
    let event = newest_event(&home, NEWER);
    assert!(
        event.text.contains(&format!(
            "; {RELEASE}'s newest recorded write (an edit) holds 1 member(s); dropped from \
             previous cut {RELEASE}: feat/beta@{}",
            beta.short()
        )),
        "{event:?}"
    );
}

#[test]
fn a_stacked_cut_after_a_drop_from_a_recorded_chain_names_include_for_that_member() {
    // Given: NEXT recorded as the chain of `m6`, `m5` and `m4` on PREVIOUS;
    // `m4` dropped from it, an edit; and NEXT published by a note.
    let (lab, home) = recorded_chain_lab();
    let dropped = knives_release(
        &lab,
        &home,
        &["--series", "old", "drop", "m4", "--why", "not this time"],
    );
    assert_eq!(dropped.status.code(), Some(0), "{}", text(&dropped));
    mark_published(&lab, &home, NEXT);
    let entries = ledger_entries(&home);

    // When: the next release is cut on NEXT.
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", THIRD]);

    // Then: it is refused, naming `m4` with the previous cut, NEXT's record,
    // and `include` as its remedy. NEXT's newest write is the commit the cut
    // starts on, so no other remedy is given; nothing is written.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut: the previous cut {NEXT} recorded 1 member(s) this cut does \
             not carry:\n    m4@{}\n  the candidate was discarded; `knives release include \
             <branch>` restores a member the previous cut recorded, or re-run with --allow-drop \
             to state the drop is intended\n",
            commit_at(&lab, "m4").short()
        )),
        "{out}"
    );
    assert!(!out.contains("first-parent line"), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");
}

#[test]
fn a_stacked_cut_below_a_recorded_chain_names_its_members_and_its_write() {
    // Given: NEXT recorded as the chain of `m6`, `m5` and `m4` on PREVIOUS,
    // whose record is both the series' previous cut and its newest write;
    // and NEXT's bookmark forgotten here, so PREVIOUS is this machine's
    // newest release.
    let (lab, home) = recorded_chain_lab();
    let next = commit_at(&lab, NEXT);
    lab.jj_work(["bookmark", "forget", NEXT]);
    let entries = ledger_entries(&home);

    // When: the next release is cut, on PREVIOUS.
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", THIRD]);

    // Then: it is refused, naming the three members with the previous cut,
    // NEXT's record off the start's first-parent line, and the remedies of
    // both; nothing is written.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut: the previous cut {NEXT} recorded 3 member(s) this cut does \
             not carry:\n"
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "demo: refusing to cut: this cut does not have {NEXT}'s newest recorded write (a \
             cut) {} on its first-parent line, so it does not carry {NEXT}\n",
            next.short()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "  the candidate was discarded; `knives release include <branch>` restores a member \
             the previous cut recorded; fetch, then move {NEXT} to {}, its newest recorded \
             write: `jj bookmark set {NEXT} -r {} --allow-backwards`, or re-run with \
             --allow-drop to state the drop is intended\n",
            next.short(),
            next.short()
        )),
        "{out}"
    );
    assert_eq!(ledger_entries(&home), entries, "{out}");
}

#[test]
fn a_record_that_lacks_a_member_both_records_hold_names_it_with_the_newest_write() {
    // Given: RELEASE recorded at a hand merge of `feat/alpha` and
    // `feat/beta`, whose record is both the previous cut and the newest
    // write; and the next release merged by hand of `feat/alpha` and
    // `feat/gamma`.
    const NEWER: &str = "release/2026-08-05";
    let (lab, home) = hand_cut_lab();
    let recorded = record(&lab, &home, RELEASE, &commit_at(&lab, RELEASE));
    assert_eq!(recorded.status.code(), Some(0), "{}", text(&recorded));
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/gamma",
        "-m",
        "merged by hand",
    ]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let beta = commit_at(&lab, "feat/beta");

    // When: it is recorded as the next release.
    let refused = record(&lab, &home, NEWER, &commit);

    // Then: it is refused, naming `feat/beta` with RELEASE, the newest
    // release that the ledger records.
    let out = text(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to record {NEWER} at {}: it does not carry 1 member(s) of {RELEASE}, \
             the newest release that the ledger records in the series:\n    feat/beta@{}\n",
            commit.short(),
            beta.short()
        )),
        "{out}"
    );

    // When: it is recorded again, the drop stated.
    let allowed = knives_release(
        &lab,
        &home,
        &["cut", NEWER, "--at", commit.as_str(), "--allow-drop"],
    );

    // Then: its event names RELEASE's newest write as what it is, a cut,
    // and the member dropped.
    assert_eq!(allowed.status.code(), Some(0), "{}", text(&allowed));
    let event = newest_event(&home, NEWER);
    assert!(
        event.text.contains(&format!(
            "; {RELEASE}'s newest recorded write (a cut) holds 2 member(s); dropped: feat/beta@{}",
            beta.short()
        )),
        "{event:?}"
    );
}

#[test]
fn a_stacked_record_that_lacks_a_member_dropped_from_a_recorded_chain_names_the_previous_cut() {
    // Given: NEXT recorded as the chain of `m6`, `m5` and `m4` on PREVIOUS;
    // `m4` dropped from it, an edit; and a start commit made by hand on
    // NEXT.
    let (lab, home) = recorded_chain_lab();
    let dropped = knives_release(
        &lab,
        &home,
        &["--series", "old", "drop", "m4", "--why", "not this time"],
    );
    assert_eq!(dropped.status.code(), Some(0), "{}", text(&dropped));
    lab.jj_work(["new", "-r", NEXT, "-m", THIRD]);
    lab.jj_work(["new"]);
    let start = commit_of(&lab, "@-");
    let m4 = commit_at(&lab, "m4");
    let entries = ledger_entries(&home);

    // When: it is recorded as the series' third release.
    let refused = record_in_series(&lab, &home, THIRD, start.as_str());

    // Then: it is refused, naming `m4` with the previous cut, NEXT's
    // record; NEXT's newest write is the start's parent, so nothing else is
    // named, and nothing is written.
    let out = text(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to record {THIRD} at {}: it does not carry 1 member(s) the previous \
             cut {NEXT} recorded:\n    m4@{}\n  nothing was recorded; record a commit that \
             carries each of them, or re-run with --allow-drop to state the drop is intended\n",
            start.short(),
            m4.short()
        )),
        "{out}"
    );
    assert!(!out.contains("first-parent line"), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");

    // When: it is recorded again, the drop stated.
    let allowed = knives_release(
        &lab,
        &home,
        &[
            "--series",
            "old",
            "cut",
            THIRD,
            "--at",
            start.as_str(),
            "--allow-drop",
        ],
    );

    // Then: its event names NEXT's newest write, the drop, first, then the
    // member that only the previous cut held.
    assert_eq!(allowed.status.code(), Some(0), "{}", text(&allowed));
    let event = newest_event(&home, THIRD);
    assert!(
        event.text.contains(&format!(
            "; {NEXT}'s newest recorded write (an edit) holds 2 member(s); dropped from previous \
             cut {NEXT}: m4@{}",
            m4.short()
        )),
        "{event:?}"
    );
}

#[test]
fn a_stacked_record_below_a_recorded_chain_names_its_members_and_its_write() {
    // Given: NEXT recorded as the chain of `m6`, `m5` and `m4` on PREVIOUS,
    // whose record is both the previous cut and the newest write; and a
    // commit merged by hand of PREVIOUS and `m3`, a new branch on it.
    let (lab, home) = recorded_chain_lab();
    let next = commit_at(&lab, NEXT);
    branch_on(&lab, PREVIOUS, "m3", ("m3.txt", "m3\n"));
    lab.jj_work(["new", "-r", PREVIOUS, "-r", "m3", "-m", THIRD]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let entries = ledger_entries(&home);

    // When: it is recorded as the series' third release.
    let refused = record_in_series(&lab, &home, THIRD, commit.as_str());

    // Then: it is refused, naming NEXT's three members, NEXT's record off
    // its first-parent line, and a commit built on that record that carries
    // them as the remedy; nothing is written.
    let out = text(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    let at = commit.short();
    assert!(
        out.contains(&format!(
            "demo: refusing to record {THIRD} at {at}: it does not carry 3 member(s) of {NEXT}, \
             the newest release that the ledger records in the series:\n"
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "demo: refusing to record {THIRD} at {at}: it does not have {NEXT}'s newest recorded \
             write (a cut) {} on its first-parent line, so it does not carry {NEXT}\n",
            next.short()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "  nothing was recorded; record a commit built on {} that carries each of them, or \
             re-run with --allow-drop to state the drop is intended\n",
            next.short()
        )),
        "{out}"
    );
    assert_eq!(ledger_entries(&home), entries, "{out}");
}

/// The forge repository the lab fork's upstream is reached by in the
/// two-machine test, so the ledger keeps the fork under [`FORK`].
const UPSTREAM: &str = "https://forge.invalid/acme/demo";
/// [`UPSTREAM`]'s `<owner>/<name>`: what the ledger keeps the lab fork under.
const FORK: &str = "acme/demo";
/// The repository each fork's registry entry says its ledger belongs to.
const LEDGER: &str = "acme/ledger";
/// [`LEDGER`] as each ledger root's `origin` spells it; git reaches it at a
/// bare remote through `insteadOf`.
const LEDGER_URL: &str = "https://forge.invalid/acme/ledger";

// From here to `settle`, every helper but `with_lab_origin` is
// tests/ledger_sweep.rs's, copied: they are private to that file.

/// A config home whose registry calls the lab fork `demo`, with [`UPSTREAM`]
/// as its upstream and its ledger shared with [`LEDGER`], and the lab's work
/// checkout pointing there too.
fn shared_home(lab: &Lab) -> tempfile::TempDir {
    let (home, _consumer) = release_test_home(lab);
    lab.upstream_at_forge_url(UPSTREAM);
    let registry = home.path().join("repos.toml");
    let registered = std::fs::read_to_string(&registry).expect("read the registry");
    std::fs::write(
        &registry,
        format!(
            "{}ledger = \"{LEDGER}\"\n",
            registered.replace(&lab.upstream.display().to_string(), UPSTREAM)
        ),
    )
    .expect("write the registry");
    home
}

/// Point `home`'s registry origin at the lab's own origin: the audit reads
/// the publish remote's live refs from it.
fn with_lab_origin(lab: &Lab, home: &Path) {
    let registry = home.join("repos.toml");
    let registered = std::fs::read_to_string(&registry).expect("read the registry");
    std::fs::write(
        &registry,
        registered.replace(
            "origin = \"https://forge.invalid/acme/work.git\"",
            &format!("origin = \"{}\"", lab.temp_origin().display()),
        ),
    )
    .expect("point the registry at the lab's origin");
}

/// Make `home`'s ledger root a repository whose `origin` is [`LEDGER_URL`],
/// reached at `remote`, committing as `machine`.
fn share(home: &Path, machine: &str, remote: &Path) {
    let root = home.join("ledger");
    lab::git_repository(&root, &[("origin", LEDGER_URL)]);
    lab::git_output(
        &root,
        [
            "config",
            &format!("url.{}.insteadOf", remote.display()),
            LEDGER_URL,
        ],
    );
    lab::git_output(&root, ["config", "knives.machine", machine]);
}

/// A second machine's config home: the same registry, nothing else yet.
fn second_machine(first: &Path) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("second config home");
    std::fs::copy(first.join("repos.toml"), home.path().join("repos.toml"))
        .expect("copy the registry");
    home
}

/// `knives <args>` from `lab`'s work checkout, as the machine whose config
/// home is `home`.
fn knives_on(lab: &Lab, home: &Path, args: &[&str]) -> Output {
    lab::knives_command(lab.work_path(), home, lab.temp_path(), args)
        .env("KNIVES_OWNER", "ses_fff688")
        .output()
        .expect("run knives")
}

/// `knives ledger sweep` as the machine whose config home is `home`.
fn sweep(home: &Path) -> Output {
    lab::knives_command(home, home, home, &["--json", "ledger", "sweep"])
        .output()
        .expect("run knives ledger sweep")
}

/// Wait for `ready`, up to a minute.
fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether some process holds the lock file at `path`.
fn held(path: &Path) -> bool {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .expect("open a lock file");
    match file.try_lock() {
        Ok(()) => false,
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(error)) => panic!("locking {}: {error}", path.display()),
    }
}

/// Wait until no sweep holds `home`'s ledger lock, so none outlives the test.
fn settle(home: &Path) {
    wait_until("the last sweep to finish", || {
        !held(&home.join("ledger.lock"))
    });
}

/// `knives --json audit demo --no-github` as the machine whose config home
/// is `home`, parsed.
fn audit_on(lab: &Lab, home: &Path) -> serde_json::Value {
    let output = knives_on(lab, home, &["--json", "audit", "demo", "--no-github"]);
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("audit emits JSON ({error}): {}", text(&output)))
}

/// The audit's notes that mention `name`.
fn notes_naming(report: &serde_json::Value, name: &str) -> Vec<String> {
    report["notes"]
        .as_array()
        .map(|notes| {
            notes
                .iter()
                .filter_map(serde_json::Value::as_str)
                .filter(|note| note.contains(name))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn another_machine_reads_the_record_after_a_sweep() {
    // Given: two machines sharing one ledger remote, and a release merged by
    // hand and pushed to the publish remote, which no ledger records: the
    // second machine's audit says the publish remote's name has no recorded
    // cut.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(RELEASE, "feat/alpha", "feat/beta");
    lab.push_branch(RELEASE);
    let commit = commit_at(&lab, RELEASE);
    let alpha = shared_home(&lab);
    with_lab_origin(&lab, alpha.path());
    let beta = second_machine(alpha.path());
    let (_remote_dir, remote) = lab::bare_remote();
    share(alpha.path(), "alpha", &remote);
    share(beta.path(), "beta", &remote);
    let unrecorded = format!("publish remote {RELEASE} has no recorded cut or edit event");
    let before = audit_on(&lab, beta.path());
    assert!(
        notes_naming(&before, RELEASE).contains(&unrecorded),
        "{before}"
    );

    // When: the first machine records the release and sweeps its ledger.
    let recorded = knives_on(
        &lab,
        alpha.path(),
        &[
            "release",
            "--repo",
            "demo",
            "cut",
            RELEASE,
            "--at",
            commit.as_str(),
        ],
    );
    assert_eq!(recorded.status.code(), Some(0), "{}", text(&recorded));
    settle(alpha.path());
    let swept = sweep(alpha.path());
    assert!(swept.status.success(), "{}", text(&swept));
    settle(alpha.path());

    // Then: the second machine's audit read everything but pull heads, and
    // reads the name as recorded: no note that it has no record or was
    // written unchecked, and no release drift.
    let after = audit_on(&lab, beta.path());
    assert_eq!(
        after["problems"],
        serde_json::json!(["open pull-head reconciliation was skipped (--no-github)"]),
        "the audit must have read everything but pull heads: {after}"
    );
    let notes = notes_naming(&after, RELEASE);
    assert!(
        !notes
            .iter()
            .any(|note| note.contains("has no recorded cut or edit event")),
        "{after}"
    );
    assert!(
        !notes.iter().any(|note| note.contains("could not check")),
        "{after}"
    );
    let drift = after["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .filter(|finding| finding["kind"] == "release-drift")
        .any(|finding| finding.to_string().contains(RELEASE));
    assert!(!drift, "{after}");

    // And: its ledger names the cut event.
    let entries = Ledger::at(beta.path().join("ledger").join(FORK))
        .read()
        .expect("read the second machine's ledger")
        .entries;
    assert!(
        entries.iter().any(|entry| {
            entry.kind == Kind::Event
                && entry.subject.as_deref() == Some(RELEASE)
                && entry
                    .text
                    .starts_with(&format!("cut {RELEASE} as {} ", commit.short()))
        }),
        "{entries:?}"
    );
}

#[test]
fn a_record_that_lacks_a_fix_the_newest_release_adopted_is_refused() {
    // Given: RELEASE, a knives cut of `feat/alpha` and `feat/beta`, fixed by
    // hand in place and adopted, which the ledger records as its newest
    // write; and the next release merged by hand of the same two branches,
    // which lacks the fix.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    lab.jj_work(["new", "-r", RELEASE]);
    std::fs::write(lab.work_path().join("fix.txt"), "fixed by hand\n").expect("write fix.txt");
    lab.jj_work(["squash", "--into", RELEASE]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let adopted = knives_release(&lab, &home, &["adopt", RELEASE, "--why", "a fix by hand"]);
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    let adopt = commit_at(&lab, RELEASE);
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "the next release, merged by hand",
    ]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: it is recorded.
    let recorded = record(&lab, &home, "release/2026-08-05", &commit);

    // Then: it is refused, naming the adopt's commit, and nothing is
    // written: no recorded change leaves a release in silence.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(out.contains(adopt.short()), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");
}

/// [`RELEASE`], a knives cut of `feat/alpha` and `feat/beta`, then `fix.txt`
/// added by hand in place and adopted, which the ledger records as its
/// newest write; and the next release merged by hand of the same two
/// branches, which lacks the fix. Returns the adopt's commit and the hand
/// merge's.
fn adopted_fix_and_hand_merge_lab() -> (Lab, tempfile::TempDir, CommitId, CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    lab.jj_work(["new", "-r", RELEASE]);
    std::fs::write(lab.work_path().join("fix.txt"), "fixed by hand\n").expect("write fix.txt");
    lab.jj_work(["squash", "--into", RELEASE]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let adopted = knives_release(&lab, &home, &["adopt", RELEASE, "--why", "a fix by hand"]);
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    let adopt = commit_at(&lab, RELEASE);
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "the next release, merged by hand",
    ]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    (lab, home, adopt, commit)
}

#[test]
fn a_record_without_a_fix_the_newest_release_adopted_is_recorded_under_allow_drop_and_says_so() {
    // Given: RELEASE's newest recorded write is an adopt of a fix by hand,
    // and the next release merged by hand lacks the fix.
    let (lab, home, adopt, commit) = adopted_fix_and_hand_merge_lab();

    // When: it is recorded, the drop stated.
    let allowed = knives_release(
        &lab,
        &home,
        &[
            "cut",
            "release/2026-08-05",
            "--at",
            commit.as_str(),
            "--allow-drop",
        ],
    );

    // Then: it is recorded, and both its line and its event name the adopt,
    // its release and its commit.
    let out = text(&allowed);
    assert_eq!(allowed.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&format!(
            "demo: --allow-drop: recording release/2026-08-05 at {} without the fix by hand of \
             {RELEASE}'s adopt {}, in 1 file(s): fix.txt\n",
            commit.short(),
            adopt.short()
        )),
        "{out}"
    );
    let event = newest_event(&home, "release/2026-08-05");
    assert_eq!(
        event.evidence.first().map(String::as_str),
        Some(commit.as_str()),
        "{event:?}"
    );
    assert!(
        event.text.contains(&format!(
            "; {RELEASE}'s newest recorded write (an edit) holds 2 member(s); all carried; \
             without the fix by hand of {RELEASE}'s adopt {}",
            adopt.short()
        )),
        "{event:?}"
    );
}

#[test]
fn a_record_that_carries_the_fix_the_newest_release_adopted_is_recorded() {
    // Given: RELEASE's newest recorded write is an adopt of a fix by hand,
    // and the next release merged by hand lacks the fix.
    let (lab, home, adopt, commit) = adopted_fix_and_hand_merge_lab();

    // When: it is recorded.
    let refused = record(&lab, &home, "release/2026-08-05", &commit);

    // Then: it is refused, naming the adopt and its file, and the refusal
    // says to record a commit that carries the fix, or to keep the new
    // content by an adopt of it.
    let out = text(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to record release/2026-08-05 at {}: it does not carry the fix by hand \
             of {RELEASE}'s adopt {}, in 1 file(s): fix.txt\n  nothing \
             was recorded; record a commit that carries that fix; to keep the new content of \
             those files, edit {RELEASE} by hand to hold it and run `knives release adopt \
             {RELEASE}`, or re-run with --allow-drop to state the drop is intended\n",
            commit.short(),
            adopt.short()
        )),
        "{out}"
    );

    // When: the fix is added to the hand merge in place, and that merge is
    // recorded.
    lab.jj_work(["new", "-r", commit.as_str()]);
    std::fs::write(lab.work_path().join("fix.txt"), "fixed by hand\n").expect("write fix.txt");
    lab.jj_work(["squash"]);
    let fixed = commit_of(&lab, "@-");
    assert_ne!(fixed, commit, "the squash rewrote the hand merge");
    let recorded = record(&lab, &home, "release/2026-08-05", &fixed);

    // Then: it is recorded, carrying every record it is held against.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&recorded_line("release/2026-08-05", &fixed)),
        "{out}"
    );
    let event = newest_event(&home, "release/2026-08-05");
    assert!(
        event.text.contains(&format!(
            "; {RELEASE}'s newest recorded write (an edit) holds 2 member(s); all carried; \
             recorded: composed outside knives"
        )),
        "{event:?}"
    );
}

#[test]
fn a_record_that_lacks_a_fix_adopted_before_an_include_is_refused_and_one_with_it_is_recorded() {
    // Given: RELEASE, a knives cut of `feat/alpha` and `feat/beta`, fixed by
    // hand in place and adopted; then `feat/gamma` included, an edit that is
    // RELEASE's newest recorded write; and the next release merged by hand
    // of the three branches, which lacks the fix.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    lab.jj_work(["new", "-r", RELEASE]);
    std::fs::write(lab.work_path().join("fix.txt"), "fixed by hand\n").expect("write fix.txt");
    lab.jj_work(["squash", "--into", RELEASE]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let adopted = knives_release(&lab, &home, &["adopt", RELEASE, "--why", "a fix by hand"]);
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    let adopt = commit_at(&lab, RELEASE);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert_eq!(included.status.code(), Some(0), "{}", text(&included));
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-r",
        "feat/gamma",
        "-m",
        "the next release, merged by hand",
    ]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let entries = ledger_entries(&home);

    // When: it is recorded.
    let refused = record(&lab, &home, "release/2026-08-05", &commit);

    // Then: it is refused, naming the adopt, though the newest recorded
    // write is the include: that write keeps the fix, and so must the next
    // release. Nothing is written.
    let out = text(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "it does not carry the fix by hand of {RELEASE}'s adopt {}, in 1 file(s): fix.txt\n",
            adopt.short()
        )),
        "{out}"
    );
    assert_eq!(ledger_entries(&home), entries, "{out}");

    // When: the fix is added to the hand merge in place, and that merge is
    // recorded.
    lab.jj_work(["new", "-r", commit.as_str()]);
    std::fs::write(lab.work_path().join("fix.txt"), "fixed by hand\n").expect("write fix.txt");
    lab.jj_work(["squash"]);
    let fixed = commit_of(&lab, "@-");
    let recorded = record(&lab, &home, "release/2026-08-05", &fixed);

    // Then: it is recorded.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&recorded_line("release/2026-08-05", &fixed)),
        "{out}"
    );
}

/// [`RELEASE`], a knives cut of `feat/alpha` and `feat/beta`; then `fix.txt`
/// written by hand in place and adopted; then the same lines of `fix.txt`
/// changed by hand again and adopted again; and the next release merged by
/// hand of the two branches, with `fix.txt` as `fixed`. Returns the second
/// adopt's commit and the merge's.
fn twice_adopted_lab(fixed: &str) -> (Lab, tempfile::TempDir, CommitId, CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    for (content, why) in [
        ("first fix\n", "a fix by hand"),
        ("second fix\n", "the fix redone"),
    ] {
        lab.jj_work(["new", "-r", RELEASE]);
        std::fs::write(lab.work_path().join("fix.txt"), content).expect("write fix.txt");
        lab.jj_work(["squash", "--into", RELEASE]);
        lab.jj_work(["new", "-r", "main@origin"]);
        let adopted = knives_release(&lab, &home, &["adopt", RELEASE, "--why", why]);
        assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    }
    let adopt = commit_at(&lab, RELEASE);
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "the next release, merged by hand",
    ]);
    std::fs::write(lab.work_path().join("fix.txt"), fixed).expect("write fix.txt");
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    (lab, home, adopt, commit)
}

#[test]
fn a_record_that_holds_the_newest_adopts_fix_carries_an_earlier_adopt_of_the_same_file() {
    // Given: RELEASE adopted twice, each a fix by hand of the same lines of
    // `fix.txt`; and the next release merged by hand, with `fix.txt` as the
    // second adopt left it. The first adopt's change does not replay empty
    // onto it: the second one changed those lines again.
    let (lab, home, _adopt, commit) = twice_adopted_lab("second fix\n");

    // When: it is recorded.
    let recorded = record(&lab, &home, "release/2026-08-05", &commit);

    // Then: it is recorded: at every file the first fix changed, it holds
    // what RELEASE's newest recorded write holds.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&recorded_line("release/2026-08-05", &commit)),
        "{out}"
    );
}

#[test]
fn a_record_that_holds_only_the_first_adopts_fix_is_refused_naming_the_second_adopt() {
    // Given: RELEASE adopted twice, each a fix by hand of the same lines of
    // `fix.txt`; and the next release merged by hand, with `fix.txt` as the
    // first adopt left it.
    let (lab, home, adopt, commit) = twice_adopted_lab("first fix\n");
    let entries = ledger_entries(&home);

    // When: it is recorded.
    let refused = record(&lab, &home, "release/2026-08-05", &commit);

    // Then: it is refused, naming the second adopt, and nothing is written.
    let out = text(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "it does not carry the fix by hand of {RELEASE}'s adopt {}, in 1 file(s): fix.txt\n",
            adopt.short()
        )),
        "{out}"
    );
    assert_eq!(ledger_entries(&home), entries, "{out}");
}

#[test]
fn a_record_that_lacks_a_fix_an_earlier_release_adopted_is_refused() {
    // Given: RELEASE, a knives cut of `feat/alpha` and `feat/beta`, fixed by
    // hand in place and adopted; then the next release, a knives cut that
    // carries the fix and records no adopt of its own; and the third release
    // merged by hand of the same two branches, which lacks the fix.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    lab.jj_work(["new", "-r", RELEASE]);
    std::fs::write(lab.work_path().join("fix.txt"), "fixed by hand\n").expect("write fix.txt");
    lab.jj_work(["squash", "--into", RELEASE]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let adopted = knives_release(&lab, &home, &["adopt", RELEASE, "--why", "a fix by hand"]);
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    let next = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(matches!(next.status.code(), Some(0 | 1)), "{}", text(&next));
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-05", "fix.txt"),
        "fixed by hand\n",
        "the next cut carries the fix"
    );
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "the third release, merged by hand",
    ]);
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");
    let (entries, refs) = (ledger_entries(&home), bookmarks(&lab));

    // When: it is recorded.
    let recorded = record(&lab, &home, "release/2026-08-06", &commit);

    // Then: it is refused, naming the fix it does not carry, and nothing is
    // written: a fix an earlier release adopted, which the newest release
    // carries, leaves no release in silence either.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!("the fix by hand of {RELEASE}'s adopt")),
        "{out}"
    );
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert_eq!(bookmarks(&lab), refs, "{out}");
}

/// [`RELEASE`], a knives cut of `feat/alpha` and `feat/beta`, then `fix.txt`
/// added by hand in place and adopted; and the next release, a knives cut
/// that carries the fix and records no adopt of its own. Returns the
/// adopt's commit.
fn adopted_then_cut_lab() -> (Lab, tempfile::TempDir, CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    lab.jj_work(["new", "-r", RELEASE]);
    std::fs::write(lab.work_path().join("fix.txt"), "fixed by hand\n").expect("write fix.txt");
    lab.jj_work(["squash", "--into", RELEASE]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let adopted = knives_release(&lab, &home, &["adopt", RELEASE, "--why", "a fix by hand"]);
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    let adopt = commit_at(&lab, RELEASE);
    let next = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(matches!(next.status.code(), Some(0 | 1)), "{}", text(&next));
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-05", "fix.txt"),
        "fixed by hand\n",
        "the next cut carries the fix"
    );
    (lab, home, adopt)
}

/// The third release, merged by hand of `feat/alpha` and `feat/beta`, with
/// `fix.txt` holding `fix` when there is one, and no ref on it. Returns its
/// commit.
fn third_hand_merge(lab: &Lab, fix: Option<&str>) -> CommitId {
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "the third release, merged by hand",
    ]);
    if let Some(fix) = fix {
        std::fs::write(lab.work_path().join("fix.txt"), fix).expect("write fix.txt");
    }
    lab.jj_work(["new"]);
    commit_of(lab, "@-")
}

#[test]
fn a_record_that_carries_a_fix_an_earlier_release_adopted_is_recorded() {
    // Given: RELEASE fixed by hand and adopted; the next release, a knives
    // cut that carries the fix; and the third release merged by hand of the
    // same two branches, with the fix.
    let (lab, home, _adopt) = adopted_then_cut_lab();
    let commit = third_hand_merge(&lab, Some("fixed by hand\n"));

    // When: it is recorded.
    let recorded = record(&lab, &home, "release/2026-08-06", &commit);

    // Then: it is recorded: it carries the fix that the series holds.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&recorded_line("release/2026-08-06", &commit)),
        "{out}"
    );
}

#[test]
fn a_record_without_a_fix_that_a_later_adopt_removed_is_recorded() {
    // Given: RELEASE fixed by hand and adopted; the next release, a knives
    // cut that carries the fix, then the fix removed from it by hand in
    // place and adopted; and the third release merged by hand of the same
    // two branches, without the fix.
    let (lab, home, _adopt) = adopted_then_cut_lab();
    lab.jj_work(["new", "-r", "release/2026-08-05"]);
    std::fs::remove_file(lab.work_path().join("fix.txt")).expect("remove fix.txt");
    lab.jj_work(["squash", "--into", "release/2026-08-05"]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let removed = knives_release(
        &lab,
        &home,
        &[
            "adopt",
            "release/2026-08-05",
            "--why",
            "the fix is not needed",
        ],
    );
    assert_eq!(removed.status.code(), Some(0), "{}", text(&removed));
    let commit = third_hand_merge(&lab, None);

    // When: it is recorded.
    let recorded = record(&lab, &home, "release/2026-08-06", &commit);

    // Then: it is recorded, and its event goes without no fix: the newest
    // recorded write no longer carries RELEASE's fix, so the series no
    // longer holds it.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&recorded_line("release/2026-08-06", &commit)),
        "{out}"
    );
    let event = newest_event(&home, "release/2026-08-06");
    assert!(!event.text.contains("without the fix by hand"), "{event:?}");
}

#[test]
fn a_record_without_a_fix_an_earlier_release_adopted_is_recorded_under_allow_drop_naming_it() {
    // Given: RELEASE fixed by hand and adopted; the next release, a knives
    // cut that carries the fix; and the third release merged by hand of the
    // same two branches, without the fix.
    let (lab, home, adopt) = adopted_then_cut_lab();
    let commit = third_hand_merge(&lab, None);

    // When: it is recorded, the drop stated.
    let allowed = knives_release(
        &lab,
        &home,
        &[
            "cut",
            "release/2026-08-06",
            "--at",
            commit.as_str(),
            "--allow-drop",
        ],
    );

    // Then: it is recorded, and both its line and its event name RELEASE's
    // adopt, its release, its commit and the file it went without; the
    // drift check names no file of the fix.
    let out = text(&allowed);
    assert_eq!(allowed.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&format!(
            "demo: --allow-drop: recording release/2026-08-06 at {} without the fix by hand of \
             {RELEASE}'s adopt {}, in 1 file(s): fix.txt\n",
            commit.short(),
            adopt.short()
        )),
        "{out}"
    );
    assert!(!out.contains("!! fix.txt"), "{out}");
    let event = newest_event(&home, "release/2026-08-06");
    assert_eq!(
        event.evidence.first().map(String::as_str),
        Some(commit.as_str()),
        "{event:?}"
    );
    assert!(
        event.text.contains(&format!(
            "; all carried; without the fix by hand of {RELEASE}'s adopt {}, in 1 file(s): \
             fix.txt",
            adopt.short()
        )),
        "{event:?}"
    );
}

#[test]
fn a_record_that_carries_a_fix_the_release_it_follows_lacks_is_recorded() {
    // Given: RELEASE, a knives cut of `feat/alpha` and `feat/beta`, fixed by
    // hand in place and adopted; RELEASE's bookmark back at its cut, as a
    // machine that never fetched the adopt has it; and the next release
    // merged by hand of the same two branches, with the fix. The release
    // the record follows here lacks the fix, and no member holds it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    let first = commit_at(&lab, RELEASE);
    lab.jj_work(["new", "-r", RELEASE]);
    std::fs::write(lab.work_path().join("fix.txt"), "fixed by hand\n").expect("write fix.txt");
    lab.jj_work(["squash", "--into", RELEASE]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let adopted = knives_release(&lab, &home, &["adopt", RELEASE, "--why", "a fix by hand"]);
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    lab.jj_work([
        "bookmark",
        "set",
        RELEASE,
        "-r",
        first.as_str(),
        "--allow-backwards",
    ]);
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "the next release, merged by hand",
    ]);
    std::fs::write(lab.work_path().join("fix.txt"), "fixed by hand\n").expect("write fix.txt");
    lab.jj_work(["new"]);
    let commit = commit_of(&lab, "@-");

    // When: it is recorded.
    let recorded = record(&lab, &home, "release/2026-08-05", &commit);

    // Then: it is recorded: the file of a fix that the gate holds is the
    // gate's to judge, not the drift check's, and the gate finds the fix
    // carried.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&recorded_line("release/2026-08-05", &commit)),
        "{out}"
    );
    assert!(!out.contains("!! fix.txt"), "{out}");
}

#[test]
fn a_record_in_a_clone_without_the_adopts_commit_is_refused_naming_it() {
    // Given: in the work checkout, RELEASE cut of `feat/alpha` and
    // `feat/beta`, fixed by hand in place and adopted, then the next release
    // cut on it, which carries the fix; the two branches and the next
    // release pushed to origin, the adopt's commit never; and a second clone
    // of origin that shares the ledger, which does not hold the adopt's
    // commit, with the third release merged by hand there of the two
    // branches, without the fix.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    lab.jj_work(["new", "-r", RELEASE]);
    std::fs::write(lab.work_path().join("fix.txt"), "fixed by hand\n").expect("write fix.txt");
    lab.jj_work(["squash", "--into", RELEASE]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let adopted = knives_release(&lab, &home, &["adopt", RELEASE, "--why", "a fix by hand"]);
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    let adopt = commit_at(&lab, RELEASE);
    let next = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(matches!(next.status.code(), Some(0 | 1)), "{}", text(&next));
    for branch in ["feat/alpha", "feat/beta", "release/2026-08-05"] {
        lab.push_branch(branch);
    }
    let upstream = lab.upstream.to_str().expect("utf-8 path").to_owned();
    lab.jj_at(&lab.second, ["git", "remote", "add", "upstream", &upstream]);
    lab.jj_at(&lab.second, ["git", "fetch", "--all-remotes"]);
    assert!(
        knives::jj::Repo::open(&lab.second)
            .expect("open the second clone")
            .resolve_commit(adopt.as_str())
            .is_err(),
        "the second clone does not hold the adopt's commit"
    );
    lab.jj_at(
        &lab.second,
        [
            "new",
            "-r",
            "feat/alpha@origin",
            "-r",
            "feat/beta@origin",
            "-m",
            "the third release, merged by hand",
        ],
    );
    lab.jj_at(&lab.second, ["new"]);
    let commit = CommitId::new(lab.revision(&lab.second, "@-", "commit_id").trim());
    let entries = ledger_entries(&home);

    // When: it is recorded in the second clone.
    let recorded = lab::knives_command(
        &lab.second,
        home.path(),
        lab.temp_path(),
        &[
            "--text",
            "release",
            "--repo",
            "demo",
            "cut",
            "release/2026-08-06",
            "--at",
            commit.as_str(),
        ],
    )
    .output()
    .expect("run knives in the second clone");

    // Then: it is refused, naming the adopt's commit it cannot read, and
    // nothing is written: a fix this clone cannot measure is no fix it may
    // drop in silence.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(out.contains(adopt.short()), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");
}

#[test]
fn a_record_in_a_clone_without_the_newest_write_is_refused() {
    // Given: in the work checkout, RELEASE cut of `feat/alpha` and
    // `feat/beta`, fixed by hand in place and adopted, then the next release
    // cut on it, which carries the fix; only the two branches pushed to
    // origin; and a second clone of origin that shares the ledger, which
    // holds neither the adopt's commit nor the next release's, with the third
    // release merged by hand there of the two branches, without the fix.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    lab.jj_work(["new", "-r", RELEASE]);
    std::fs::write(lab.work_path().join("fix.txt"), "fixed by hand\n").expect("write fix.txt");
    lab.jj_work(["squash", "--into", RELEASE]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let adopted = knives_release(&lab, &home, &["adopt", RELEASE, "--why", "a fix by hand"]);
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    let adopt = commit_at(&lab, RELEASE);
    let cut_next = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(
        matches!(cut_next.status.code(), Some(0 | 1)),
        "{}",
        text(&cut_next)
    );
    let next = commit_at(&lab, "release/2026-08-05");
    for branch in ["feat/alpha", "feat/beta"] {
        lab.push_branch(branch);
    }
    let upstream = lab.upstream.to_str().expect("utf-8 path").to_owned();
    lab.jj_at(&lab.second, ["git", "remote", "add", "upstream", &upstream]);
    lab.jj_at(&lab.second, ["git", "fetch", "--all-remotes"]);
    let second = knives::jj::Repo::open(&lab.second).expect("open the second clone");
    assert!(
        second.resolve_commit(adopt.as_str()).is_err()
            && second.resolve_commit(next.as_str()).is_err(),
        "the second clone holds neither the adopt's commit nor the next release's"
    );
    lab.jj_at(
        &lab.second,
        [
            "new",
            "-r",
            "feat/alpha@origin",
            "-r",
            "feat/beta@origin",
            "-m",
            "the third release, merged by hand",
        ],
    );
    lab.jj_at(&lab.second, ["new"]);
    let commit = CommitId::new(lab.revision(&lab.second, "@-", "commit_id").trim());
    let entries = ledger_entries(&home);

    // When: it is recorded in the second clone.
    let recorded = lab::knives_command(
        &lab.second,
        home.path(),
        lab.temp_path(),
        &[
            "--text",
            "release",
            "--repo",
            "demo",
            "cut",
            "release/2026-08-06",
            "--at",
            commit.as_str(),
        ],
    )
    .output()
    .expect("run knives in the second clone");

    // Then: it is refused, naming a commit it cannot read, and nothing is
    // written: a clone that cannot read the newest write cannot know what
    // that write carries.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(
        out.contains(next.short()) || out.contains(adopt.short()),
        "{out}"
    );
    assert_eq!(ledger_entries(&home), entries, "{out}");
}

/// Give the second clone of origin, which shares the ledger, the upstream
/// remote, then fetch everything both remotes hold now.
fn fetch_second_clone(lab: &Lab) {
    let upstream = lab.upstream.to_str().expect("utf-8 path").to_owned();
    lab.jj_at(&lab.second, ["git", "remote", "add", "upstream", &upstream]);
    lab.jj_at(&lab.second, ["git", "fetch", "--all-remotes"]);
}

/// Whether the second clone holds `commit`.
fn second_clone_holds(lab: &Lab, commit: &CommitId) -> bool {
    knives::jj::Repo::open(&lab.second)
        .expect("open the second clone")
        .resolve_commit(commit.as_str())
        .is_ok()
}

/// The third release merged by hand in the second clone of `feat/alpha`
/// and `feat/beta` as origin holds them, with `fix.txt` holding `fix` when
/// there is one, and no ref on it. Returns its commit.
fn hand_merge_in_second_clone(lab: &Lab, fix: Option<&str>) -> CommitId {
    lab.jj_at(
        &lab.second,
        [
            "new",
            "-r",
            "feat/alpha@origin",
            "-r",
            "feat/beta@origin",
            "-m",
            "the third release, merged by hand",
        ],
    );
    if let Some(fix) = fix {
        std::fs::write(lab.second.join("fix.txt"), fix).expect("write fix.txt");
    }
    lab.jj_at(&lab.second, ["new"]);
    CommitId::new(lab.revision(&lab.second, "@-", "commit_id").trim())
}

/// `knives release cut` in the second clone, then `args`, whatever its
/// exit.
fn cut_in_second_clone(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> Output {
    let args: Vec<&str> = ["--text", "release", "--repo", "demo", "cut"]
        .into_iter()
        .chain(args.iter().copied())
        .collect();
    lab::knives_command(&lab.second, home.path(), lab.temp_path(), &args)
        .output()
        .expect("run knives in the second clone")
}

/// [`adopted_then_cut_lab`] with the two branches and the next release
/// pushed, never the adopt's commit, and the second clone fetched: it holds
/// the newest recorded write, the next release's cut, which carries the
/// fix, and not the adopt's commit. Returns the adopt's commit and the next
/// release's.
fn adopt_unfetched_lab() -> (Lab, tempfile::TempDir, CommitId, CommitId) {
    let (lab, home, adopt) = adopted_then_cut_lab();
    let next = commit_at(&lab, "release/2026-08-05");
    for branch in ["feat/alpha", "feat/beta", "release/2026-08-05"] {
        lab.push_branch(branch);
    }
    fetch_second_clone(&lab);
    assert!(
        !second_clone_holds(&lab, &adopt) && second_clone_holds(&lab, &next),
        "the second clone holds the next release and not the adopt's commit"
    );
    (lab, home, adopt, next)
}

/// The line that names RELEASE's fix by hand, of the adopt `adopt`, as one
/// that the second clone held by its content against `next`, the next
/// release's cut, the newest recorded write.
fn fix_held_by_content(adopt: &CommitId, next: &CommitId) -> String {
    format!(
        "the fix by hand of {RELEASE}'s adopt {adopt}, in 1 file(s): fix.txt (this repository \
         does not hold {adopt}, so the fix is held by its content: what release/2026-08-05's \
         newest recorded write (a cut) {next} holds in those files)\n",
        adopt = adopt.short(),
        next = next.short()
    )
}

#[test]
fn a_record_in_a_clone_without_the_adopts_commit_that_holds_its_fix_is_recorded() {
    // Given: RELEASE fixed by hand and adopted, the next release cut on it
    // with the fix; a second clone that holds the next release and not the
    // adopt's commit; and the third release merged by hand there, with
    // `fix.txt` as the next release holds it.
    let (lab, home, _adopt, _next) = adopt_unfetched_lab();
    let commit = hand_merge_in_second_clone(&lab, Some("fixed by hand\n"));

    // When: it is recorded in the second clone.
    let recorded = cut_in_second_clone(
        &lab,
        &home,
        &["release/2026-08-06", "--at", commit.as_str()],
    );

    // Then: it is recorded: the clone cannot measure the fix, so it holds it
    // by its content, and the commit holds at `fix.txt` what the newest
    // recorded write holds there.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&recorded_line("release/2026-08-06", &commit)),
        "{out}"
    );
}

#[test]
fn a_record_in_a_clone_without_the_adopts_commit_that_changes_its_file_is_refused() {
    // Given: the same second clone, and the third release merged by hand
    // there with `fix.txt` changed from what the next release holds.
    let (lab, home, adopt, next) = adopt_unfetched_lab();
    let commit = hand_merge_in_second_clone(&lab, Some("fixed another way\n"));
    let entries = ledger_entries(&home);

    // When: it is recorded in the second clone.
    let recorded = cut_in_second_clone(
        &lab,
        &home,
        &["release/2026-08-06", "--at", commit.as_str()],
    );

    // Then: it is refused, naming the adopt's commit as one this clone does
    // not hold and the write whose content held the fix, and nothing is
    // written; the drift check leaves `fix.txt` to the gate.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to record release/2026-08-06 at {}: it does not carry {}",
            commit.short(),
            fix_held_by_content(&adopt, &next)
        )),
        "{out}"
    );
    assert!(!out.contains("!! fix.txt"), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");
}

#[test]
fn a_record_in_a_clone_without_the_adopts_commit_goes_ahead_under_allow_drop_naming_it() {
    // Given: the same second clone, and the third release merged by hand
    // there with `fix.txt` changed from what the next release holds.
    let (lab, home, adopt, next) = adopt_unfetched_lab();
    let commit = hand_merge_in_second_clone(&lab, Some("fixed another way\n"));

    // When: it is recorded in the second clone, the drop stated.
    let allowed = cut_in_second_clone(
        &lab,
        &home,
        &[
            "release/2026-08-06",
            "--at",
            commit.as_str(),
            "--allow-drop",
        ],
    );

    // Then: it is recorded, and both its line and its event name RELEASE's
    // adopt and its commit.
    let out = text(&allowed);
    assert_eq!(allowed.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&format!(
            "demo: --allow-drop: recording release/2026-08-06 at {} without {}",
            commit.short(),
            fix_held_by_content(&adopt, &next)
        )),
        "{out}"
    );
    assert!(
        out.contains(&recorded_line("release/2026-08-06", &commit)),
        "{out}"
    );
    let event = newest_event(&home, "release/2026-08-06");
    assert!(
        event.text.contains(&format!(
            "; all carried; without the fix by hand of {RELEASE}'s adopt {}",
            adopt.short()
        )),
        "{event:?}"
    );
}

/// [`RELEASE`], a knives cut of `feat/alpha` and `feat/beta`, pushed with
/// both branches and published by a note; then the next release, a knives
/// cut in the work checkout, never pushed: the newest recorded write; and
/// the second clone fetched, which does not hold that write. Returns the
/// next release's commit.
fn newest_write_unfetched_lab() -> (Lab, tempfile::TempDir, CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    for branch in ["feat/alpha", "feat/beta", RELEASE] {
        lab.push_branch(branch);
    }
    mark_published(&lab, &home, RELEASE);
    let next = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(matches!(next.status.code(), Some(0 | 1)), "{}", text(&next));
    let newest = commit_at(&lab, "release/2026-08-05");
    fetch_second_clone(&lab);
    assert!(
        !second_clone_holds(&lab, &newest),
        "the second clone does not hold the newest write"
    );
    (lab, home, newest)
}

/// What the composition gate says of a subject, `subject` (`this cut` or
/// `it`), that it cannot hold against `newest`, the next release's cut,
/// which this clone does not hold.
fn not_held_against(subject: &str, newest: &CommitId) -> String {
    format!(
        "{subject} cannot be held against release/2026-08-05's newest recorded write (a cut) {}: \
         this repository does not hold that commit, so it cannot tell what that write carries\n",
        newest.short()
    )
}

#[test]
fn a_record_in_a_clone_without_the_newest_write_is_refused_naming_its_fetch() {
    // Given: a second clone that does not hold the newest recorded write,
    // the next release's cut, and a release merged by hand there of the
    // same two branches.
    let (lab, home, newest) = newest_write_unfetched_lab();
    let commit = hand_merge_in_second_clone(&lab, None);
    let entries = ledger_entries(&home);

    // When: it is recorded in the second clone.
    let recorded = cut_in_second_clone(
        &lab,
        &home,
        &["release/2026-08-06", "--at", commit.as_str()],
    );

    // Then: it is refused, naming that write, its release and its commit,
    // with the fetch and, where the fetch does not bring it, the adopt of
    // that release, and nothing is written: the clone cannot tell what that
    // write carries, so it holds the commit to nothing less.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to record release/2026-08-06 at {}: {}",
            commit.short(),
            not_held_against("it", &newest)
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "  nothing was recorded; fetch, then record a commit that carries what {next} carries; \
             if the fetch does not bring {next}, `knives release adopt release/2026-08-05` \
             records the commit this repository holds as release/2026-08-05's newest write, or \
             re-run with --allow-drop to state the drop is intended\n",
            next = newest.short()
        )),
        "{out}"
    );
    assert_eq!(ledger_entries(&home), entries, "{out}");
}

#[test]
fn a_record_in_a_clone_without_the_newest_write_goes_ahead_under_allow_drop_naming_it() {
    // Given: the same second clone and hand merge.
    let (lab, home, newest) = newest_write_unfetched_lab();
    let commit = hand_merge_in_second_clone(&lab, None);

    // When: it is recorded in the second clone, the drop stated.
    let allowed = cut_in_second_clone(
        &lab,
        &home,
        &[
            "release/2026-08-06",
            "--at",
            commit.as_str(),
            "--allow-drop",
        ],
    );

    // Then: it is recorded, and both its line and its event name the
    // write it was not held against, its release and its commit.
    let out = text(&allowed);
    assert_eq!(allowed.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&format!(
            "demo: --allow-drop: recording release/2026-08-06 at {} without holding it against \
             release/2026-08-05's newest recorded write (a cut) {}, which this repository does \
             not hold\n",
            commit.short(),
            newest.short()
        )),
        "{out}"
    );
    assert!(
        out.contains(&recorded_line("release/2026-08-06", &commit)),
        "{out}"
    );
    let event = newest_event(&home, "release/2026-08-06");
    assert!(
        event.text.contains(&format!(
            "; release/2026-08-05's newest recorded write (a cut) holds 2 member(s); all carried; \
             not held against release/2026-08-05's newest recorded write (a cut) {}, which this \
             repository does not hold",
            newest.short()
        )),
        "{event:?}"
    );
}

#[test]
fn a_cut_in_a_clone_without_the_newest_write_is_refused_naming_its_fetch() {
    // Given: a second clone that does not hold the newest recorded write,
    // the next release's cut, and whose newest release is the published
    // RELEASE.
    let (lab, home, newest) = newest_write_unfetched_lab();
    let entries = ledger_entries(&home);

    // When: the third release is cut in the second clone.
    let cut = cut_in_second_clone(&lab, &home, &["release/2026-08-06"]);

    // Then: it is refused, naming that write, its release and its commit,
    // saying to fetch, then move that release to it, and where the fetch
    // does not bring it, to adopt the release; nothing is cut.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut: {}",
            not_held_against("this cut", &newest)
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "  the candidate was discarded; fetch, then move release/2026-08-05 to {next}, its \
             newest recorded write: `jj bookmark set release/2026-08-05 -r {next} \
             --allow-backwards`; if the fetch does not bring {next}, `knives release adopt \
             release/2026-08-05` records the commit this repository holds as \
             release/2026-08-05's newest write, or re-run with --allow-drop to state the drop is \
             intended\n",
            next = newest.short()
        )),
        "{out}"
    );
    assert_eq!(ledger_entries(&home), entries, "{out}");
    assert!(
        knives::jj::Repo::open(&lab.second)
            .expect("open the second clone")
            .resolve_commit("release/2026-08-06")
            .is_err(),
        "{out}"
    );
}

#[test]
fn a_record_in_a_clone_without_the_write_an_adopt_replaced_holds_the_file_it_names() {
    // Given: RELEASE fixed by hand in place and adopted, pushed, and fetched
    // by the second clone; then in the work checkout `feat/gamma` included
    // into it, an edit that carries the fix and is never pushed, the fix
    // removed from it by hand in place and adopted, and the release and the
    // branches pushed and fetched by the second clone. That clone holds both
    // adopts and not the edit the second one replaced, so it measures the
    // second adopt against an automerge, which no file of it changes.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    lab.jj_work(["new", "-r", RELEASE]);
    std::fs::write(lab.work_path().join("fix.txt"), "fixed by hand\n").expect("write fix.txt");
    lab.jj_work(["squash", "--into", RELEASE]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let adopted = knives_release(&lab, &home, &["adopt", RELEASE, "--why", "a fix by hand"]);
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    let first = commit_at(&lab, RELEASE);
    for branch in ["feat/alpha", "feat/beta", RELEASE] {
        lab.push_branch(branch);
    }
    fetch_second_clone(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert_eq!(included.status.code(), Some(0), "{}", text(&included));
    let edit = commit_at(&lab, RELEASE);
    lab.jj_work(["new", "-r", RELEASE]);
    std::fs::remove_file(lab.work_path().join("fix.txt")).expect("remove fix.txt");
    lab.jj_work(["squash", "--into", RELEASE]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let removed = knives_release(
        &lab,
        &home,
        &["adopt", RELEASE, "--why", "the fix is not needed"],
    );
    assert_eq!(removed.status.code(), Some(0), "{}", text(&removed));
    let second = commit_at(&lab, RELEASE);
    for branch in ["feat/gamma", RELEASE] {
        lab.push_branch(branch);
    }
    lab.jj_at(&lab.second, ["git", "fetch", "--all-remotes"]);
    assert!(
        second_clone_holds(&lab, &first)
            && second_clone_holds(&lab, &second)
            && !second_clone_holds(&lab, &edit),
        "the second clone holds both adopts and not the edit"
    );
    lab.jj_at(
        &lab.second,
        [
            "new",
            "-r",
            "feat/alpha@origin",
            "-r",
            "feat/beta@origin",
            "-r",
            "feat/gamma@origin",
            "-m",
            "the next release, merged by hand",
        ],
    );
    std::fs::write(lab.second.join("fix.txt"), "fixed by hand\n").expect("write fix.txt");
    lab.jj_at(&lab.second, ["new"]);
    let commit = CommitId::new(lab.revision(&lab.second, "@-", "commit_id").trim());
    let entries = ledger_entries(&home);

    // When: the next release, merged by hand with the fix the second adopt
    // removed, is recorded in the second clone.
    let recorded = cut_in_second_clone(
        &lab,
        &home,
        &["release/2026-08-05", "--at", commit.as_str()],
    );

    // Then: it is refused by the gate, naming the second adopt: the file
    // that adopt's event names and nothing here measures is held by its
    // content, and the commit does not hold there what that adopt holds.
    // Nothing is written, and the drift check leaves `fix.txt` to the gate.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to record release/2026-08-05 at {}: it does not carry the fix by hand \
             of {RELEASE}'s adopt {}, in 1 file(s): fix.txt\n",
            commit.short(),
            second.short()
        )),
        "{out}"
    );
    assert!(!out.contains("!! fix.txt"), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");
}

#[test]
fn a_record_in_a_clone_without_the_adopts_commit_holds_a_fix_whose_path_holds_a_separator() {
    // The adopt event names its files in one line of text, joined by `, `
    // and followed by ` (why: `, so a clone that cannot read the adopt's
    // commit reads such a path back in pieces. Each piece is a file nothing
    // holds, so it may not stand for the fix.
    for file in ["x, y.txt", "x (why: y.txt"] {
        // Given: in the work checkout, RELEASE cut of `feat/alpha` and
        // `feat/beta`, fixed by hand in place with the new file `file` and
        // adopted, then the next release cut on it, which carries the fix;
        // the two branches and the next release pushed to origin, the
        // adopt's commit never; and a second clone of origin that shares the
        // ledger, with the third release merged by hand there of the two
        // branches, without the fix.
        let lab = Lab::new();
        lab.branch("feat/alpha", "alpha.txt", "alpha\n");
        lab.branch("feat/beta", "beta.txt", "beta\n");
        let (home, _consumer) = release_test_home(&lab);
        let cut = knives_release(&lab, &home, &["cut", RELEASE]);
        assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
        lab.jj_work(["new", "-r", RELEASE]);
        std::fs::write(lab.work_path().join(file), "fixed by hand\n").expect("write the fix");
        lab.jj_work(["squash", "--into", RELEASE]);
        lab.jj_work(["new", "-r", "main@origin"]);
        let adopted = knives_release(&lab, &home, &["adopt", RELEASE, "--why", "a fix by hand"]);
        assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
        let adopt = commit_at(&lab, RELEASE);
        let next = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
        assert!(matches!(next.status.code(), Some(0 | 1)), "{}", text(&next));
        for branch in ["feat/alpha", "feat/beta", "release/2026-08-05"] {
            lab.push_branch(branch);
        }
        let upstream = lab.upstream.to_str().expect("utf-8 path").to_owned();
        lab.jj_at(&lab.second, ["git", "remote", "add", "upstream", &upstream]);
        lab.jj_at(&lab.second, ["git", "fetch", "--all-remotes"]);
        assert!(
            knives::jj::Repo::open(&lab.second)
                .expect("open the second clone")
                .resolve_commit(adopt.as_str())
                .is_err(),
            "the second clone does not hold the adopt's commit"
        );
        lab.jj_at(
            &lab.second,
            [
                "new",
                "-r",
                "feat/alpha@origin",
                "-r",
                "feat/beta@origin",
                "-m",
                "the third release, merged by hand",
            ],
        );
        lab.jj_at(&lab.second, ["new"]);
        let commit = CommitId::new(lab.revision(&lab.second, "@-", "commit_id").trim());
        let entries = ledger_entries(&home);

        // When: it is recorded in the second clone.
        let recorded = lab::knives_command(
            &lab.second,
            home.path(),
            lab.temp_path(),
            &[
                "--text",
                "release",
                "--repo",
                "demo",
                "cut",
                "release/2026-08-06",
                "--at",
                commit.as_str(),
            ],
        )
        .output()
        .expect("run knives in the second clone");

        // Then: it is refused, naming the adopt's commit it cannot read, and
        // nothing is written, as for a path that holds no separator.
        let out = text(&recorded);
        assert_eq!(recorded.status.code(), Some(3), "{file}: {out}");
        assert!(out.contains(adopt.short()), "{file}: {out}");
        assert_eq!(ledger_entries(&home), entries, "{file}: {out}");
    }
}

/// Each file of the fork's ledger that holds an `adopt` event of `name`:
/// its path, its frontmatter and its text.
fn adopt_event_files(
    home: &tempfile::TempDir,
    name: &str,
) -> Vec<(std::path::PathBuf, toml::Table, String)> {
    let mut files = Vec::new();
    let ledger = home.path().join("ledger").join("demo");
    for dirent in std::fs::read_dir(&ledger).expect("read the ledger directory") {
        let path = dirent.expect("read a ledger file").path();
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some((frontmatter, text)) = contents
            .strip_prefix("+++\n")
            .and_then(|rest| rest.split_once("+++\n"))
        else {
            continue;
        };
        if text.starts_with(&format!("edited {name} as ")) && text.contains(" parent(s): adopted: ")
        {
            let frontmatter: toml::Table = frontmatter
                .parse()
                .expect("parse an adopt event's frontmatter");
            files.push((path, frontmatter, text.to_owned()));
        }
    }
    files
}

/// Remove the `fix_files` list from each `adopt` event of `name`, so the
/// event reads as a knives from before that list writes it: its text alone
/// names the files of its fix.
fn without_fix_files(home: &tempfile::TempDir, name: &str) {
    let events = adopt_event_files(home, name);
    assert!(!events.is_empty(), "no adopt event of {name}");
    for (path, mut frontmatter, text) in events {
        frontmatter.remove("fix_files");
        let frontmatter = toml::to_string(&frontmatter).expect("write the frontmatter");
        std::fs::write(
            &path,
            format!("+++\n{}\n+++\n{text}", frontmatter.trim_end()),
        )
        .expect("rewrite the adopt event");
    }
}

/// [`RELEASE`] cut of `feat/alpha` and `feat/beta`, fixed by hand in place
/// with the new file `file` and adopted, then the next release cut on it,
/// which carries the fix; the two branches and the next release pushed, the
/// adopt's commit never; and the second clone fetched, which holds the next
/// release and not the adopt's commit. With `older`, the adopt event loses
/// its `fix_files` list before the next cut ([`without_fix_files`]).
/// Returns the adopt's commit and the next release's.
fn separator_fix_lab(file: &str, older: bool) -> (Lab, tempfile::TempDir, CommitId, CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert_eq!(cut.status.code(), Some(0), "{file}: {}", text(&cut));
    lab.jj_work(["new", "-r", RELEASE]);
    std::fs::write(lab.work_path().join(file), "fixed by hand\n").expect("write the fix");
    lab.jj_work(["squash", "--into", RELEASE]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let adopted = knives_release(&lab, &home, &["adopt", RELEASE, "--why", "a fix by hand"]);
    assert_eq!(adopted.status.code(), Some(0), "{file}: {}", text(&adopted));
    let adopt = commit_at(&lab, RELEASE);
    if older {
        without_fix_files(&home, RELEASE);
    }
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(
        matches!(cut.status.code(), Some(0 | 1)),
        "{file}: {}",
        text(&cut)
    );
    let next = commit_at(&lab, "release/2026-08-05");
    for branch in ["feat/alpha", "feat/beta", "release/2026-08-05"] {
        lab.push_branch(branch);
    }
    fetch_second_clone(&lab);
    assert!(
        !second_clone_holds(&lab, &adopt) && second_clone_holds(&lab, &next),
        "the second clone holds the next release and not the adopt's commit"
    );
    (lab, home, adopt, next)
}

/// The third release merged by hand in the second clone of `feat/alpha`
/// and `feat/beta` as origin holds them, with the fix at `file`, and no ref
/// on it. Returns its commit.
fn fixed_merge_in_second_clone(lab: &Lab, file: &str) -> CommitId {
    lab.jj_at(
        &lab.second,
        [
            "new",
            "-r",
            "feat/alpha@origin",
            "-r",
            "feat/beta@origin",
            "-m",
            "the third release, merged by hand",
        ],
    );
    std::fs::write(lab.second.join(file), "fixed by hand\n").expect("write the fix");
    lab.jj_at(&lab.second, ["new"]);
    CommitId::new(lab.revision(&lab.second, "@-", "commit_id").trim())
}

#[test]
fn a_record_in_a_clone_without_the_adopts_commit_reads_a_listed_path_with_a_separator_whole() {
    for file in ["x, y.txt", "x (why: y.txt"] {
        // Given: RELEASE fixed by hand with the new file `file` and adopted,
        // whose event lists that file; the next release cut on it; a second
        // clone that holds the next release and not the adopt's commit; and
        // the third release merged by hand there, with the fix.
        let (lab, home, _adopt, _next) = separator_fix_lab(file, false);
        let listed: Vec<String> = adopt_event_files(&home, RELEASE)
            .iter()
            .filter_map(|(_, frontmatter, _)| frontmatter.get("fix_files")?.as_array().cloned())
            .flatten()
            .filter_map(|listed| Some(listed.get("path")?.as_str()?.to_owned()))
            .collect();
        assert_eq!(listed, [file], "the adopt event lists the file whole");
        let commit = fixed_merge_in_second_clone(&lab, file);

        // When: it is recorded in the second clone.
        let recorded = cut_in_second_clone(
            &lab,
            &home,
            &["release/2026-08-06", "--at", commit.as_str()],
        );

        // Then: it is recorded: the clone reads the path whole from the
        // adopt event's list, and the commit holds there what the newest
        // recorded write holds.
        let out = text(&recorded);
        assert_eq!(recorded.status.code(), Some(0), "{file}: {out}");
        assert!(
            out.contains(&recorded_line("release/2026-08-06", &commit)),
            "{file}: {out}"
        );
    }
}

#[test]
fn an_older_adopt_of_a_path_with_a_separator_holds_that_path_whole_in_a_clone_without_its_commit() {
    for file in ["x, y.txt", "x (why: y.txt"] {
        // Given: RELEASE fixed by hand with the new file `file` and adopted,
        // its event as a knives from before the `fix_files` list writes it,
        // so its text alone names the file, in pieces; the next release cut
        // on it in the work checkout; a second clone that holds the next
        // release and not the adopt's commit; and the third release merged
        // by hand there, without the fix.
        let (lab, home, adopt, next) = separator_fix_lab(file, true);
        let commit = hand_merge_in_second_clone(&lab, None);
        let entries = ledger_entries(&home);

        // When: it is recorded in the second clone.
        let refused = cut_in_second_clone(
            &lab,
            &home,
            &["release/2026-08-06", "--at", commit.as_str()],
        );

        // Then: it is refused, naming the adopt and the path whole, and
        // nothing is written: the clone joins the pieces of the text back
        // into each path that they may name, and the newest recorded write
        // holds that one.
        let out = text(&refused);
        assert_eq!(refused.status.code(), Some(3), "{file}: {out}");
        assert!(
            out.contains(&format!(
                "demo: refusing to record release/2026-08-06 at {}: it does not carry the fix by \
                 hand of {RELEASE}'s adopt {adopt}, in 1 file(s): {file} (this repository does \
                 not hold {adopt}, so the fix is held by its content: what release/2026-08-05's \
                 newest recorded write (a cut) {} holds in those files)\n",
                commit.short(),
                next.short(),
                adopt = adopt.short()
            )),
            "{file}: {out}"
        );
        assert_eq!(ledger_entries(&home), entries, "{file}: {out}");

        // When: the third release, merged by hand there with the fix, is
        // recorded.
        let fixed = fixed_merge_in_second_clone(&lab, file);
        let recorded =
            cut_in_second_clone(&lab, &home, &["release/2026-08-06", "--at", fixed.as_str()]);

        // Then: it is recorded: it holds at that path what the newest
        // recorded write holds.
        let out = text(&recorded);
        assert_eq!(recorded.status.code(), Some(0), "{file}: {out}");
        assert!(
            out.contains(&recorded_line("release/2026-08-06", &fixed)),
            "{file}: {out}"
        );
    }
}

#[test]
fn an_adopt_event_that_lists_its_files_reads_in_a_knives_that_does_not_know_the_list() {
    // Given: a ledger whose one file is an adopt event as this knives writes
    // it, with its `fix_files` list, and one more list a later knives may
    // add. Built against an older knives's library, this test is that
    // knives reading the event.
    let home = tempfile::tempdir().expect("a config home");
    let ledger = home.path().join("ledger").join("demo");
    std::fs::create_dir_all(&ledger).expect("create the ledger directory");
    let said = "edited release/2026-08-04 as 0123456789ab (change kkkkkkkkkkkk) with 2 \
                parent(s): adopted: replaces the recorded cut 1123456789ab; measured against \
                the recorded cut 1123456789ab; changed beyond the baseline's conflicts: x, \
                y.txt (why: a fix by hand); parents: feat/alpha@2123456789ab, \
                feat/beta@3123456789ab";
    std::fs::write(
        ledger.join("20260815T221403.000000000Z-0000.md"),
        format!(
            "+++\nts = \"2026-08-15T22:14:03Z\"\nowner = \"ses_fff688\"\n\
             subject = \"release/2026-08-04\"\nkind = \"event\"\n\
             evidence = [\"0123456789ab\", \"2123456789ab\", \"3123456789ab\"]\n\
             [[parents]]\ncommit = \"2123456789ab\"\nbranches = [\"feat/alpha\"]\n\
             [[parents]]\ncommit = \"3123456789ab\"\nbranches = [\"feat/beta\"]\n\
             [[fix_files]]\npath = \"x, y.txt\"\nchange = \"beyond\"\n\
             [[later_list]]\npath = \"w.txt\"\n+++\n{said}\n"
        ),
    )
    .expect("write the adopt event");

    // When: the ledger is read.
    let read = Ledger::at(ledger).read().expect("read the ledger");

    // Then: the event reads, with its subject, its text and its parents,
    // and no problem is named.
    assert!(read.problems.is_empty(), "{:?}", read.problems);
    assert_eq!(read.entries.len(), 1, "{:?}", read.entries);
    let event = &read.entries[0];
    assert_eq!(event.subject.as_deref(), Some("release/2026-08-04"));
    assert_eq!(event.text, said);
    assert_eq!(
        event.parents,
        [
            RecordedParent {
                commit: "2123456789ab".to_owned(),
                branches: vec!["feat/alpha".to_owned()],
            },
            RecordedParent {
                commit: "3123456789ab".to_owned(),
                branches: vec!["feat/beta".to_owned()],
            },
        ]
    );
}

/// Change `files` by hand on `release` in place (`jj squash` into it, which
/// keeps its change id): each written with its content, or removed where it
/// has none; then adopt it with `why`. Returns the adopt's commit.
fn fix_by_hand(
    (lab, home): (&Lab, &tempfile::TempDir),
    release: &str,
    files: &[(&str, Option<&str>)],
    why: &str,
) -> CommitId {
    lab.jj_work(["new", "-r", release]);
    for (file, content) in files {
        let path = lab.work_path().join(file);
        match content {
            Some(content) => std::fs::write(&path, content).expect("write a file by hand"),
            None => std::fs::remove_file(&path).expect("remove a file by hand"),
        }
    }
    lab.jj_work(["squash", "--into", release]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let adopted = knives_release(lab, home, &["adopt", release, "--why", why]);
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    commit_at(lab, release)
}

/// The next release merged by hand of `feat/alpha` and `feat/beta` in the
/// work checkout, with each of `files` written with its content, and no ref
/// on it. Returns its commit.
fn merged_with(lab: &Lab, files: &[(&str, &str)]) -> CommitId {
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "the next release, merged by hand",
    ]);
    for (file, content) in files {
        std::fs::write(lab.work_path().join(file), content).expect("write a file of the merge");
    }
    lab.jj_work(["new"]);
    commit_of(lab, "@-")
}

/// The third release merged by hand in the second clone of `feat/alpha` and
/// `feat/beta` as origin holds them, with each of `files` holding `fixed by
/// hand`, and no ref on it. Returns its commit.
fn merged_in_second_clone_with(lab: &Lab, files: &[&str]) -> CommitId {
    lab.jj_at(
        &lab.second,
        [
            "new",
            "-r",
            "feat/alpha@origin",
            "-r",
            "feat/beta@origin",
            "-m",
            "the third release, merged by hand",
        ],
    );
    for file in files {
        std::fs::write(lab.second.join(file), "fixed by hand\n").expect("write a fixed file");
    }
    lab.jj_at(&lab.second, ["new"]);
    CommitId::new(lab.revision(&lab.second, "@-", "commit_id").trim())
}

#[test]
fn a_record_whose_own_merge_changes_or_deletes_a_fixed_file_is_refused_naming_it() {
    for fixed in [None, Some("fixed another way\n")] {
        // Given: RELEASE fixed by hand at `fix.txt` and adopted; the next
        // release, a knives cut that carries the fix; and the third release
        // merged by hand of the same two branches, whose resolution leaves
        // out `fix.txt` or changes it.
        let (lab, home, adopt) = adopted_then_cut_lab();
        let commit = third_hand_merge(&lab, fixed);
        let entries = ledger_entries(&home);

        // When: it is recorded.
        let refused = record(&lab, &home, "release/2026-08-06", &commit);

        // Then: it is refused, naming the adopt and the file, and nothing
        // is written.
        let out = text(&refused);
        assert_eq!(refused.status.code(), Some(3), "{fixed:?}: {out}");
        assert!(
            out.contains(&format!(
                "demo: refusing to record release/2026-08-06 at {}: it does not carry the fix by \
                 hand of {RELEASE}'s adopt {}, in 1 file(s): fix.txt\n",
                commit.short(),
                adopt.short()
            )),
            "{fixed:?}: {out}"
        );
        assert!(!out.contains("!! fix.txt"), "{fixed:?}: {out}");
        assert_eq!(ledger_entries(&home), entries, "{fixed:?}: {out}");
    }
}

#[test]
fn a_record_without_one_file_of_a_fix_is_refused_for_that_file_alone() {
    // Given: RELEASE, a knives cut of `feat/alpha` and `feat/beta`, fixed by
    // hand in place at `x.txt` and `y.txt` and adopted; then `x.txt` changed
    // by hand again and adopted again, which ends the first adopt's hold on
    // `x.txt` alone; and the next release merged by hand of the two
    // branches, with `x.txt` as the second adopt left it and without
    // `y.txt`.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    let first = fix_by_hand(
        (&lab, &home),
        RELEASE,
        &[("x.txt", Some("x fixed\n")), ("y.txt", Some("y fixed\n"))],
        "a fix by hand",
    );
    let second = fix_by_hand(
        (&lab, &home),
        RELEASE,
        &[("x.txt", Some("x refined\n"))],
        "the fix refined",
    );
    let without = merged_with(&lab, &[("x.txt", "x refined\n")]);
    let entries = ledger_entries(&home);

    // When: it is recorded.
    let refused = record(&lab, &home, "release/2026-08-05", &without);

    // Then: it is refused, naming the first adopt and `y.txt` alone, and
    // nothing is written: it carries the second adopt's fix, and the drift
    // check leaves `y.txt` to the gate.
    let out = text(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to record release/2026-08-05 at {}: it does not carry the fix by hand \
             of {RELEASE}'s adopt {}, in 1 file(s): y.txt\n",
            without.short(),
            first.short()
        )),
        "{out}"
    );
    assert!(!out.contains(&format!("adopt {}", second.short())), "{out}");
    assert!(!out.contains("!! y.txt"), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");

    // When: the merge with `y.txt` too is recorded.
    let with = merged_with(&lab, &[("x.txt", "x refined\n"), ("y.txt", "y fixed\n")]);
    let recorded = record(&lab, &home, "release/2026-08-05", &with);

    // Then: it is recorded.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&recorded_line("release/2026-08-05", &with)),
        "{out}"
    );
}

/// RELEASE, a knives cut of `feat/alpha` and `feat/beta`, fixed by hand in
/// place at `fix.txt` and `keep.txt` and adopted, its event as a knives from
/// before the `fix_files` list writes it when `older`; the next release cut
/// on it, then `keep.txt` removed from it by hand and adopted; the two
/// branches and the next release pushed, the first adopt's commit never;
/// the next release published by a note; and the second clone fetched.
/// Returns the first adopt's commit.
fn deleted_named_file_lab(older: bool) -> (Lab, tempfile::TempDir, CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    let adopt = fix_by_hand(
        (&lab, &home),
        RELEASE,
        &[
            ("fix.txt", Some("fixed by hand\n")),
            ("keep.txt", Some("kept by hand\n")),
        ],
        "a fix by hand",
    );
    if older {
        without_fix_files(&home, RELEASE);
    }
    let next = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(matches!(next.status.code(), Some(0 | 1)), "{}", text(&next));
    fix_by_hand(
        (&lab, &home),
        "release/2026-08-05",
        &[("keep.txt", None)],
        "keep.txt is not needed",
    );
    for branch in ["feat/alpha", "feat/beta", "release/2026-08-05"] {
        lab.push_branch(branch);
    }
    mark_published(&lab, &home, "release/2026-08-05");
    fetch_second_clone(&lab);
    assert!(
        !second_clone_holds(&lab, &adopt),
        "the second clone does not hold the first adopt's commit"
    );
    (lab, home, adopt)
}

#[test]
fn a_clone_without_the_adopts_commit_ends_the_hold_on_a_file_a_later_release_removed() {
    for older in [true, false] {
        // Given: a second clone that does not hold RELEASE's adopt, whose
        // event names `fix.txt` and `keep.txt`, in its text alone when
        // `older`; the newest recorded write, which removed `keep.txt` and
        // holds `fix.txt`; and the third release merged by hand there,
        // without `fix.txt`.
        let (lab, home, adopt) = deleted_named_file_lab(older);
        let without = merged_in_second_clone_with(&lab, &[]);
        let entries = ledger_entries(&home);

        // When: it is recorded in the second clone.
        let refused = cut_in_second_clone(
            &lab,
            &home,
            &["release/2026-08-06", "--at", without.as_str()],
        );

        // Then: it is refused for `fix.txt` alone, which the newest write
        // holds, and nothing is written.
        let out = text(&refused);
        assert_eq!(refused.status.code(), Some(3), "{older}: {out}");
        assert!(
            out.contains(&format!(
                "it does not carry the fix by hand of {RELEASE}'s adopt {}, in 1 file(s): \
                 fix.txt (this repository does not hold",
                adopt.short()
            )),
            "{older}: {out}"
        );
        assert_eq!(ledger_entries(&home), entries, "{older}: {out}");

        // When: the third release is cut in the second clone.
        let cut = cut_in_second_clone(&lab, &home, &["release/2026-08-06"]);

        // Then: it is cut: the newest write holds nothing at `keep.txt`, so
        // the hold on it has ended, and the cut holds `fix.txt` as that
        // write does.
        let out = text(&cut);
        assert!(matches!(cut.status.code(), Some(0 | 1)), "{older}: {out}");
        assert!(!out.contains("the fix by hand of"), "{older}: {out}");

        // When: a release merged by hand there with `fix.txt` is recorded
        // next.
        let with = merged_in_second_clone_with(&lab, &["fix.txt"]);
        let recorded =
            cut_in_second_clone(&lab, &home, &["release/2026-08-07", "--at", with.as_str()]);

        // Then: it is recorded.
        let out = text(&recorded);
        assert_eq!(recorded.status.code(), Some(0), "{older}: {out}");
        assert!(
            out.contains(&recorded_line("release/2026-08-07", &with)),
            "{older}: {out}"
        );
    }
}

#[test]
fn an_older_adopt_whose_text_reads_two_ways_holds_each_path_that_the_newest_write_holds() {
    // Given: RELEASE fixed by hand with the new files `x`, `y.txt` and
    // `x, y.txt` and adopted, its event as a knives from before the
    // `fix_files` list writes it, so its text `x, x, y.txt, y.txt` reads as
    // more than one list of paths; the next release cut on it, which holds
    // all three; a second clone that does not hold the adopt's commit; and
    // the third release merged by hand there, with `x` and `x, y.txt` and
    // without `y.txt`.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    let fixed = Some("fixed by hand\n");
    let adopt = fix_by_hand(
        (&lab, &home),
        RELEASE,
        &[("x", fixed), ("y.txt", fixed), ("x, y.txt", fixed)],
        "a fix by hand",
    );
    without_fix_files(&home, RELEASE);
    let next = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(matches!(next.status.code(), Some(0 | 1)), "{}", text(&next));
    for branch in ["feat/alpha", "feat/beta", "release/2026-08-05"] {
        lab.push_branch(branch);
    }
    fetch_second_clone(&lab);
    assert!(
        !second_clone_holds(&lab, &adopt),
        "the second clone does not hold the adopt's commit"
    );
    let without = merged_in_second_clone_with(&lab, &["x", "x, y.txt"]);
    let entries = ledger_entries(&home);

    // When: it is recorded in the second clone.
    let refused = cut_in_second_clone(
        &lab,
        &home,
        &["release/2026-08-06", "--at", without.as_str()],
    );

    // Then: it is refused, naming the adopt and `y.txt` alone, and nothing
    // is written: each path that the text may name and the newest recorded
    // write holds is held on its own.
    let out = text(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "it does not carry the fix by hand of {RELEASE}'s adopt {}, in 1 file(s): y.txt (this \
             repository does not hold",
            adopt.short()
        )),
        "{out}"
    );
    assert_eq!(ledger_entries(&home), entries, "{out}");

    // When: the merge with all three is recorded there.
    let with = merged_in_second_clone_with(&lab, &["x", "y.txt", "x, y.txt"]);
    let recorded = cut_in_second_clone(&lab, &home, &["release/2026-08-06", "--at", with.as_str()]);

    // Then: it is recorded.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
}

#[test]
fn a_listed_fix_file_that_names_no_repository_path_ends_its_hold() {
    // Given: RELEASE, a knives cut of `feat/alpha` and `feat/beta`, fixed by
    // hand in place at `fix.txt` and adopted; then its adopt event's
    // `fix_files` list given two strings that are no repository path, as a
    // peer's ledger may carry.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    fix_by_hand(
        (&lab, &home),
        RELEASE,
        &[("fix.txt", Some("fixed by hand\n"))],
        "a fix by hand",
    );
    let events = adopt_event_files(&home, RELEASE);
    assert!(!events.is_empty(), "no adopt event of {RELEASE}");
    for (path, mut frontmatter, text) in events {
        let listed = frontmatter
            .get_mut("fix_files")
            .and_then(toml::Value::as_array_mut)
            .expect("the adopt event lists its files");
        for named in ["a//b", "/x"] {
            let mut file = toml::Table::new();
            file.insert("path".to_owned(), toml::Value::String(named.to_owned()));
            file.insert(
                "change".to_owned(),
                toml::Value::String("beyond".to_owned()),
            );
            listed.push(toml::Value::Table(file));
        }
        let frontmatter = toml::to_string(&frontmatter).expect("write the frontmatter");
        std::fs::write(
            &path,
            format!("+++\n{}\n+++\n{text}", frontmatter.trim_end()),
        )
        .expect("rewrite the adopt event");
    }

    // When: the next release is cut.
    let next = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it is cut with the fix: nothing holds a string that is no
    // repository path, so its hold ends, and it is no error.
    let out = text(&next);
    assert!(matches!(next.status.code(), Some(0 | 1)), "{out}");
    assert!(out.contains("cut release/2026-08-05 as"), "{out}");
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-05", "fix.txt"),
        "fixed by hand\n",
        "{out}"
    );
}
