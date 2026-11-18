//! A stacked release series: `stacked = true` on a series table.
//!
//! The next release of such a series is one merge of its previous release
//! and its new members. The previous release is the first parent, and it is
//! never a member. A member may stack on it, and `members --verify` measures
//! each member from its fork point with it. A release made as a chain of
//! merges, one member each, reads as its previous release and the side
//! parents on its first-parent line. A cut starts the next release as one
//! commit on the previous release, `include` adds each member, the ledger
//! labels the previous release, and the reap keeps every release the live
//! one descends from. A series without the key keeps today's rules: a branch
//! built on the previous release is refused.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::ledger::{Entry, Kind, RecordedParent};
use lab::{Lab, commit_at, knives_release, release_parents, release_test_home};

/// The series' first cut: the previous release of the release in hand.
const PREVIOUS: &str = "release/old/2026-08-04";

/// The series' next release, made by hand on [`PREVIOUS`], or cut.
const NEXT: &str = "release/old/2026-08-05";

/// The release cut on [`NEXT`] once that one is published.
const THIRD: &str = "release/old/2026-08-06";

/// Whether `revision` resolves in the lab's work checkout.
fn resolves(lab: &Lab, revision: &str) -> bool {
    knives::jj::Repo::open(lab.work_path())
        .expect("open work checkout")
        .resolve_commit(revision)
        .is_ok()
}

/// A lab whose upstream carries a second base, `old`, split from `main`: the
/// seed is their common ancestor, and each has moved on since.
fn two_base_lab() -> Lab {
    let lab = Lab::new();
    lab.upstream_branch("old", "old.txt", "old base\n");
    lab.advance_upstream("new base\n");
    lab
}

/// A branch forked from `base`, with its bookmark on its one commit.
fn branch_on(lab: &Lab, base: &str, name: &str, file: &str) {
    lab.jj_work(["new", "-r", base, "-m", name]);
    std::fs::write(lab.work_path().join(file), format!("{name}\n")).expect("write branch");
    lab.jj_work(["bookmark", "create", name, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// The release-test home with the registry entry given a series `old` on the
/// upstream branch `old`, plus whatever `extra` the test adds to that table.
fn series_home(lab: &Lab, extra: &str) -> tempfile::TempDir {
    let (home, _consumer) = release_test_home(lab);
    let registry = home.path().join("repos.toml");
    let mut text = std::fs::read_to_string(&registry).expect("read registry");
    text.push_str("\n[repos.demo.series.old]\nbase = \"old\"\n");
    text.push_str(extra);
    std::fs::write(&registry, text).expect("write registry");
    home
}

fn text(output: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Series `old`, with `stacked = true` when `stacked`: its first cut
/// [`PREVIOUS`] of `fix/a` and `fix/b`, pushed to origin and marked published
/// by a `record: published` note, and three branches on that cut: `del`
/// deletes `a.txt`, which the cut carries, and `m6` and `m5` each add a file.
fn previous_release_lab(stacked: bool) -> (Lab, tempfile::TempDir) {
    series_lab(stacked, true)
}

/// [`previous_release_lab`], the first cut marked published only when
/// `published`: otherwise no tag, note or pin marks it.
fn series_lab(stacked: bool, published: bool) -> (Lab, tempfile::TempDir) {
    let lab = two_base_lab();
    branch_on(&lab, "old@upstream", "fix/a", "a.txt");
    branch_on(&lab, "old@upstream", "fix/b", "b.txt");
    let home = series_home(&lab, if stacked { "stacked = true\n" } else { "" });
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", PREVIOUS]);
    assert!(cut.status.success(), "{}", text(&cut));
    lab.push_branch(PREVIOUS);
    lab.fetch_work();
    if published {
        mark_published(&lab, &home, PREVIOUS);
    }
    lab.jj_work(["new", "-r", PREVIOUS, "-m", "del"]);
    std::fs::remove_file(lab.work_path().join("a.txt")).expect("delete a file the cut carries");
    lab.jj_work(["bookmark", "create", "del", "-r", "@"]);
    lab.jj_work(["new"]);
    branch_on(&lab, PREVIOUS, "m6", "m6.txt");
    branch_on(&lab, PREVIOUS, "m5", "m5.txt");
    (lab, home)
}

/// A `record: published` note on release `name`, naming its commit, as the
/// release owner writes one.
fn mark_published(lab: &Lab, home: &tempfile::TempDir, name: &str) {
    let commit = commit_at(lab, name);
    let noted = lab::knives(
        lab,
        home,
        &[
            "notch",
            name,
            "-m",
            "record: published as v1.0.0",
            "--evidence",
            commit.as_str(),
            "--repo",
            "demo",
        ],
    );
    assert!(noted.status.success(), "{}", text(&noted));
}

/// `knives release --series old <args>`, which must exit 0.
fn release_ok(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    let mut full = vec!["--series", "old"];
    full.extend_from_slice(args);
    let output = knives_release(lab, home, &full);
    assert_eq!(output.status.code(), Some(0), "{args:?}: {}", text(&output));
    output
}

/// The fork's ledger entries.
fn ledger_entries(home: &tempfile::TempDir) -> Vec<Entry> {
    knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
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

/// Whether `from` and `to` hold the same tree in the work checkout.
fn same_tree(lab: &Lab, from: &str, to: &str) -> bool {
    knives::jj::changed_files_between(lab.work_path(), from, to)
        .expect("diff two revisions")
        .is_empty()
}

/// Whether `commit` is visible in the work checkout: not abandoned.
fn is_visible(lab: &Lab, commit: &knives::ids::CommitId) -> bool {
    knives::jj::commits_matching(lab.work_path(), "all()")
        .expect("list visible commits")
        .contains(commit)
}

/// Whether any ref of `name`, local or on a remote, is left in the work
/// checkout.
fn has_refs(lab: &Lab, name: &str) -> bool {
    knives::jj::Repo::open(lab.work_path())
        .expect("open work checkout")
        .bookmark_tips()
        .expect("read bookmark tips")
        .keys()
        .any(|reference| reference.branch().as_str() == name)
}

/// [`NEXT`] made by hand as one merge of [`PREVIOUS`] and the three
/// branches on it: the shape a stacked cut of them has.
fn flat_next(lab: &Lab) {
    lab.jj_work([
        "new", "-r", PREVIOUS, "-r", "del", "-r", "m6", "-r", "m5", "-m", NEXT,
    ]);
    lab.jj_work(["bookmark", "create", NEXT, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// [`NEXT`] made by hand as the compose script makes a release: the chain
/// `merge(merge(merge(PREVIOUS, del), m6), m5)`.
fn chain_next(lab: &Lab) {
    lab.jj_work(["new", "-r", PREVIOUS, "-r", "del", "-m", "compose del"]);
    lab.jj_work(["new", "-r", "@", "-r", "m6", "-m", "compose m6"]);
    lab.jj_work(["new", "-r", "@", "-r", "m5", "-m", NEXT]);
    lab.jj_work(["bookmark", "create", NEXT, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// The row `members` prints for `branch`'s commit, held by its bookmark.
fn member_row(lab: &Lab, branch: &str) -> String {
    format!("- {} {branch}", commit_at(lab, branch).short())
}

/// The row `members` prints for the previous release.
fn previous_row(lab: &Lab) -> String {
    format!(
        "- {} {PREVIOUS} previous release (not a member)",
        commit_at(lab, PREVIOUS).short()
    )
}

#[test]
fn the_plan_of_a_stacked_release_reports_no_stacked_member_and_exits_0() {
    // Given: a stacked series whose release in hand is one merge of its
    // previous release and three branches built on that release.
    let (lab, home) = previous_release_lab(true);
    flat_next(&lab);

    // When: the series plans.
    let plan = knives_release(&lab, &home, &["--series", "old"]);

    // Then: the previous release has a row of its own, no member reads as
    // stacked on a prior merge or is told to rebase off the trunk, and the
    // plan exits 0.
    let out = text(&plan);
    assert_eq!(plan.status.code(), Some(0), "{out}");
    assert!(out.contains(&format!("demo: {NEXT}")), "{out}");
    assert!(out.contains("4 parent(s), flat"), "{out}");
    assert!(
        out.contains(&format!(
            "    {}  {PREVIOUS} previous release (not a member)",
            commit_at(&lab, PREVIOUS).short()
        )),
        "{out}"
    );
    assert!(!out.contains("stacked on a prior merge"), "{out}");
    assert!(!out.contains("rebase it off the trunk"), "{out}");
    assert!(
        out.contains("every parent is still its branch tip"),
        "{out}"
    );
}

#[test]
fn the_plan_before_a_stacked_cut_reads_branches_on_the_previous_release_as_fit() {
    // Given: a stacked series whose release in hand is its published first
    // cut, the previous release of its next cut, with `del`, `m6` and `m5`
    // built on it.
    let (lab, home) = previous_release_lab(true);

    // When: the series plans.
    let plan = knives_release(&lab, &home, &["--series", "old"]);

    // Then: the plan exits 0, each branch reads as one the next cut
    // includes, and none is told to rebase off the trunk.
    let out = text(&plan);
    assert_eq!(plan.status.code(), Some(0), "{out}");
    assert!(!out.contains("rebase it off the trunk"), "{out}");
    for branch in ["del", "m6", "m5"] {
        assert!(
            out.contains(&format!(
                "{branch} is built on {PREVIOUS}, which the next cut of this stacked series"
            )),
            "{out}"
        );
    }

    // And: a branch that merges two of them carries a merge past the release
    // in hand, so it is still told so, the merge named.
    lab.jj_work(["new", "-r", "m6", "-r", "m5", "-m", "merged"]);
    lab.jj_work(["bookmark", "create", "merged", "-r", "@"]);
    lab.jj_work(["new"]);
    let merge = commit_at(&lab, "merged");
    let plan = knives_release(&lab, &home, &["--series", "old"]);
    let out = text(&plan);
    assert!(
        out.contains("branch merged's history past the trunk carries 1 merge commit(s)"),
        "{out}"
    );
    assert!(out.contains(merge.short()), "{out}");
    assert!(
        out.contains("rebase it off the trunk before including it"),
        "{out}"
    );
}

#[test]
fn members_verify_reads_a_stacked_release_with_no_missing_member() {
    // Given: the same stacked release, whose member `del` deletes a file the
    // previous release carries.
    let (lab, home) = previous_release_lab(true);
    flat_next(&lab);

    // When: its members' content is replayed onto it.
    let members = knives_release(&lab, &home, &["--series", "old", "members", "--verify"]);

    // Then: the previous release is no member, each member replays empty
    // from its fork point with that release, and nothing is missing or
    // unexplained.
    let out = text(&members);
    assert_eq!(members.status.code(), Some(0), "{out}");
    assert!(out.contains(&previous_row(&lab)), "{out}");
    for branch in ["del", "m6", "m5"] {
        assert!(out.contains(&member_row(&lab, branch)), "{out}");
        assert!(
            out.contains(&format!(
                "  {branch}@{}: carried (replay empty)",
                commit_at(&lab, branch).short()
            )),
            "{out}"
        );
    }
    assert!(!out.contains("!!"), "{out}");
}

#[test]
fn members_verify_reads_a_chain_shaped_release_as_its_previous_release_and_side_parents() {
    // Given: the next release made as a chain of merges, one member each,
    // as the compose script makes it.
    let (lab, home) = previous_release_lab(true);
    chain_next(&lab);

    // When: its members' content is replayed onto it.
    let members = knives_release(&lab, &home, &["--series", "old", "members", "--verify"]);

    // Then: it has two parents, and reads as its previous release, on a row
    // of its own, and the side parents on its first-parent line, none of
    // them missing.
    let out = text(&members);
    assert_eq!(members.status.code(), Some(0), "{out}");
    assert!(out.contains(&format!("{NEXT} @ ")), "{out}");
    assert!(out.contains(" — 2 parents"), "{out}");
    assert!(out.contains(&previous_row(&lab)), "{out}");
    for branch in ["m5", "m6", "del"] {
        assert!(out.contains(&member_row(&lab, branch)), "{out}");
    }
    assert!(!out.contains("!!"), "{out}");
}

#[test]
fn without_stacked_an_include_stacked_on_the_previous_release_is_refused() {
    // Given: the same series and branches without the stacked key, and the
    // next release cut: it starts identical to the published first cut,
    // which is now its previous release.
    let (lab, home) = previous_release_lab(false);
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", NEXT]);
    assert!(resolves(&lab, NEXT), "{}", text(&cut));

    // When: a branch built on the previous release is included.
    let include = knives_release(&lab, &home, &["--series", "old", "include", "del"]);

    // Then: today's rule refuses it: its history past the trunk carries the
    // previous release's merge.
    let out = text(&include);
    assert_eq!(include.status.code(), Some(3), "{out}");
    assert!(out.contains("rebase it off the trunk"), "{out}");
}

#[test]
fn a_stacked_cut_starts_on_the_previous_release() {
    // Given: a stacked series whose published first cut is the previous
    // release.
    let (lab, home) = previous_release_lab(true);
    let previous = commit_at(&lab, PREVIOUS);

    // When: the next release is cut.
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", NEXT]);

    // Then: it exits 0 and starts as one commit on the previous release
    // with that release's tree, the composition to edit.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(0), "{out}");
    assert_eq!(release_parents(&lab, NEXT), vec![previous.clone()], "{out}");
    assert!(same_tree(&lab, previous.as_str(), NEXT), "{out}");
    assert!(
        out.contains(&format!("{NEXT} starts identical to {PREVIOUS}")),
        "{out}"
    );
    assert!(out.contains("is the composition to edit"), "{out}");
}

#[test]
fn a_stacked_cut_on_an_unpublished_previous_release_is_refused_as_identical() {
    // Given: a stacked series whose first cut is on the publish remote with
    // no tag, no `record: published` note and no pin.
    let (lab, home) = series_lab(true, false);
    let recorded = ledger_entries(&home).len();

    // When: the next release is cut.
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", NEXT]);

    // Then: it would start identical to the first cut, which can still be
    // edited in place, so the cut refuses, says to include into it, and
    // writes nothing.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(out.contains("nothing to cut"), "{out}");
    assert!(out.contains("`knives release include <branch>`"), "{out}");
    assert!(!resolves(&lab, NEXT), "{out}");
    assert!(resolves(&lab, PREVIOUS), "{out}");
    assert_eq!(ledger_entries(&home).len(), recorded, "{out}");
}

#[test]
fn includes_on_a_stacked_cut_make_one_merge_of_the_previous_release_and_its_members() {
    // Given: the next release cut on the published previous release.
    let (lab, home) = previous_release_lab(true);
    let previous = commit_at(&lab, PREVIOUS);
    release_ok(&lab, &home, &["cut", NEXT]);

    // When: each branch built on the previous release is included.
    for branch in ["del", "m6", "m5"] {
        release_ok(&lab, &home, &["include", branch]);
    }

    // Then: the release is one merge of the previous release and the three,
    // in that order, every member replays from its fork point with the
    // previous release, and its tree is the one jj's own merge of them has.
    let mut expected = vec![previous.clone()];
    expected.extend(["del", "m6", "m5"].map(|branch| commit_at(&lab, branch)));
    assert_eq!(release_parents(&lab, NEXT), expected);
    let members = release_ok(&lab, &home, &["members", "--verify"]);
    assert!(!text(&members).contains("!!"), "{}", text(&members));
    lab.jj_work([
        "new",
        "-r",
        previous.as_str(),
        "-r",
        "del",
        "-r",
        "m6",
        "-r",
        "m5",
        "-m",
        "merged by jj",
    ]);
    lab.jj_work(["bookmark", "create", "merged-by-jj", "-r", "@"]);
    assert!(same_tree(&lab, "merged-by-jj", NEXT));
}

#[test]
fn the_previous_release_is_never_dropped_or_advanced_and_rebase_refuses() {
    // Given: the next release with `del` included, and a branch `later`
    // built on the previous release, which ancestry alone would read as
    // that release's successor.
    let (lab, home) = previous_release_lab(true);
    let previous = commit_at(&lab, PREVIOUS);
    release_ok(&lab, &home, &["cut", NEXT]);
    release_ok(&lab, &home, &["include", "del"]);
    branch_on(&lab, previous.as_str(), "later", "later.txt");
    let held = commit_at(&lab, NEXT);
    let series = |args: &[&str]| {
        let mut full = vec!["--series", "old"];
        full.extend_from_slice(args);
        knives_release(&lab, &home, &full)
    };

    // When: the previous release is dropped, every member is advanced,
    // `later` is advanced by name and from the previous release, and the
    // release is rebased.
    let dropped = series(&["drop", previous.as_str(), "--why", "not a member"]);
    let advance = series(&["advance"]);
    let named = series(&["advance", "later"]);
    let from = series(&["advance", "later", "--from", previous.as_str()]);
    let rebase = series(&["rebase"]);

    // Then: the drop and the rebase refuse, no advance moves the previous
    // release, and the release is as it was.
    assert_eq!(dropped.status.code(), Some(3), "{}", text(&dropped));
    assert!(
        text(&dropped).contains("the previous release"),
        "{}",
        text(&dropped)
    );
    assert_eq!(advance.status.code(), Some(0), "{}", text(&advance));
    assert!(
        text(&advance).contains("is at its branch tip"),
        "{}",
        text(&advance)
    );
    assert_eq!(named.status.code(), Some(3), "{}", text(&named));
    assert_eq!(from.status.code(), Some(3), "{}", text(&from));
    assert!(
        text(&from).contains("no advance moves it"),
        "{}",
        text(&from)
    );
    assert_eq!(rebase.status.code(), Some(3), "{}", text(&rebase));
    assert!(text(&rebase).contains("is stacked"), "{}", text(&rebase));
    assert_eq!(commit_at(&lab, NEXT), held);
    assert_eq!(
        release_parents(&lab, NEXT),
        vec![previous, commit_at(&lab, "del")]
    );
}

#[test]
fn a_member_with_a_merge_past_the_previous_release_is_refused() {
    // Given: the next release cut, and a branch that merges two branches
    // built on the previous release.
    let (lab, home) = previous_release_lab(true);
    release_ok(&lab, &home, &["cut", NEXT]);
    lab.jj_work(["new", "-r", "m6", "-r", "m5", "-m", "merged"]);
    lab.jj_work(["bookmark", "create", "merged", "-r", "@"]);
    lab.jj_work(["new"]);
    let merge = commit_at(&lab, "merged");
    let held = commit_at(&lab, NEXT);

    // When: that branch is included.
    let include = knives_release(&lab, &home, &["--series", "old", "include", "merged"]);

    // Then: its merge past the previous release is refused and named, and
    // the release is as it was.
    let out = text(&include);
    assert_eq!(include.status.code(), Some(3), "{out}");
    assert!(out.contains("carries 1 merge commit(s)"), "{out}");
    assert!(out.contains(merge.short()), "{out}");
    assert_eq!(commit_at(&lab, NEXT), held, "{out}");
}

#[test]
fn the_reap_after_a_stacked_cut_keeps_every_release_the_live_one_descends_from() {
    // Given: the first stacked cut, with `del` included, pushed and
    // published.
    let (lab, home) = previous_release_lab(true);
    let previous = commit_at(&lab, PREVIOUS);
    release_ok(&lab, &home, &["cut", NEXT]);
    release_ok(&lab, &home, &["include", "del"]);
    lab.push_branch(NEXT);
    lab.fetch_work();
    mark_published(&lab, &home, NEXT);
    let first = commit_at(&lab, NEXT);

    // When: a second stacked cut is made on it.
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", THIRD]);

    // Then: the cut and its reap exit 0. The first stacked cut's name is
    // forgotten here and on origin's tracking ref, its commit and the
    // previous release's stay visible under the new cut, and nothing is
    // abandoned.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&format!(
            "reaped {NEXT} (refs forgotten everywhere; commit kept, {THIRD} descends from it)"
        )),
        "{out}"
    );
    assert!(!out.contains("abandoned"), "{out}");
    assert!(!has_refs(&lab, NEXT), "{out}");
    assert!(!has_refs(&lab, PREVIOUS), "{out}");
    assert!(is_visible(&lab, &first), "{out}");
    assert!(is_visible(&lab, &previous), "{out}");
    assert_eq!(release_parents(&lab, THIRD), vec![first], "{out}");
}

#[test]
fn the_ledger_event_of_a_stacked_cut_labels_the_previous_release() {
    // Given: a stacked series whose published first cut is the previous
    // release.
    let (lab, home) = previous_release_lab(true);
    let previous = commit_at(&lab, PREVIOUS);
    let labelled = RecordedParent {
        commit: previous.as_str().to_owned(),
        branches: vec![format!("{PREVIOUS} previous release (not a member)")],
    };

    // When: the next release is cut, and then a member is included.
    release_ok(&lab, &home, &["cut", NEXT]);
    let cut = newest_event(&home, NEXT);
    let start = commit_at(&lab, NEXT);
    release_ok(&lab, &home, &["include", "del"]);
    let edit = newest_event(&home, NEXT);

    // Then: each event records the previous release after the commit in
    // its evidence and first in its parents, under the label, and records
    // as members only what was included.
    assert!(cut.text.starts_with(&format!("cut {NEXT} as ")), "{cut:?}");
    assert!(
        cut.text.contains("previous release (not a member)"),
        "{cut:?}"
    );
    assert_eq!(
        cut.evidence,
        vec![start.as_str().to_owned(), previous.as_str().to_owned()],
        "{cut:?}"
    );
    assert_eq!(cut.parents, vec![labelled.clone()], "{cut:?}");
    let del = commit_at(&lab, "del");
    assert!(
        edit.text.starts_with(&format!("edited {NEXT} as ")),
        "{edit:?}"
    );
    assert_eq!(
        edit.evidence,
        vec![
            commit_at(&lab, NEXT).as_str().to_owned(),
            previous.as_str().to_owned(),
            del.as_str().to_owned()
        ],
        "{edit:?}"
    );
    assert_eq!(
        edit.parents,
        vec![
            labelled,
            RecordedParent {
                commit: del.as_str().to_owned(),
                branches: vec!["del".to_owned()],
            }
        ],
        "{edit:?}"
    );
}
