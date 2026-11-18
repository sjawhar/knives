//! `knives release cut NAME --at REV`: a release cut outside knives recorded
//! as NAME's cut by one command, with no new commit, no temporary name and no
//! throwaway cut.
//!
//! The record is the cut event a knives cut writes, so `adopt`, the edit
//! verbs and another machine's `audit` read the release as recorded. Refused,
//! writing nothing: a conflicted commit, a name that sorts before a release
//! the series already holds or records, and a release made as merges on its
//! previous release outside a stacked series. The same commit recorded again
//! writes nothing. In a stacked series a chain of merges, one member each,
//! records its previous release and the side parents on its first-parent
//! line, and an edit of it, or a member rewritten in place under it, keeps
//! what its inner merges resolved.

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
use std::process::Output;
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
/// wrote, which the ledger does not record.
fn hand_cut_lab() -> (Lab, tempfile::TempDir) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(RELEASE, "feat/alpha", "feat/beta");
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
